//! The Tuya datapoint engine.
//!
//! Tuya devices multiplex everything through one manufacturer cluster keyed by
//! a datapoint number whose meaning is device specific. The number alone says
//! nothing — datapoint 2 is a temperature on one device and a countdown timer
//! on another — so the definition's datapoint table is the only thing that
//! makes a frame interpretable.
//!
//! ```text
//!   Tuya frame -> DP codec -> datapoint table -> value converter -> capability
//!   DeviceCommand -> capability -> datapoint table -> converter -> dataRequest
//! ```
//!
//! Both directions go through the same table, so a value that can be read
//! cannot be written under a different name by accident.
//!
//! # Nothing is inferred from the datapoint number
//!
//! A datapoint the table does not mention produces no state. That is not
//! caution for its own sake: guessing would attach a countdown timer's value
//! to whatever capability happened to share its number, and the result reads
//! like a plausible measurement.

use rszigbee_devices::{Definition, TuyaDatapoint, TuyaKind};
use rszigbee_spec::tuya::{Datapoint, Value};

use crate::capability::CapabilityId;
use crate::command::DeviceCommand;
use crate::device::DeviceInfo;
use crate::runtime::behavior::{BehaviorRegistry, DecodeContext, EncodeContext};
use crate::state::{StateChanges, StateValue};

/// Converts reported datapoints into a capability state delta.
///
/// Datapoints the definition does not name are skipped, and their absence from
/// the result is the signal that the table is incomplete.
#[must_use]
pub fn datapoints_to_state(
    definition: &Definition,
    datapoints: &[Datapoint],
    device: &DeviceInfo,
    behaviors: &BehaviorRegistry,
) -> StateChanges {
    let mut changes = StateChanges::new();
    for point in datapoints {
        let Some(entry) = definition.tuya_datapoints.iter().find(|d| d.dp == point.dp) else {
            continue;
        };

        // The declarative path first, and only what it cannot express is
        // delegated. A datapoint whose table entry names a behaviour has no
        // declarative form to try.
        if let TuyaKind::Behavior { name } = &entry.kind {
            let Some(behavior) = behaviors.get(name) else {
                // Named but not implemented. Nothing happens, which is
                // visible in the coverage report rather than silent here.
                continue;
            };
            let outcome = behavior.decode_datapoint(&DecodeContext {
                device,
                definition,
                datapoint: point,
                capability: &entry.name,
            });
            // `NotHandled` does not fall through to a generic best effort:
            // there is nothing generic that could interpret this datapoint,
            // and guessing is how a plausible-looking value gets invented.
            if let Some(delegated) = outcome.handled() {
                changes.merge(&delegated);
            }
            continue;
        }

        if let Some(value) = convert(entry, &point.value) {
            changes.set(CapabilityId::from(entry.name.as_str()), value);
        }
    }
    changes
}

/// Lowers a command through a named behaviour, when one claims it.
///
/// Tried only after [`command_to_datapoint`] finds nothing: the declarative
/// table is the first path, and a behaviour exists for what it cannot say.
#[must_use]
pub fn command_via_behavior(
    definition: &Definition,
    command: &DeviceCommand,
    device: &DeviceInfo,
    behaviors: &BehaviorRegistry,
) -> Option<Vec<Datapoint>> {
    // Only behaviours this definition actually names. A behaviour the
    // definition did not ask for must not intercept its commands.
    for entry in &definition.tuya_datapoints {
        let TuyaKind::Behavior { name } = &entry.kind else {
            continue;
        };
        let Some(behavior) = behaviors.get(name) else {
            continue;
        };
        if let Some(points) = behavior
            .encode_command(&EncodeContext {
                device,
                definition,
                command,
            })
            .handled()
        {
            return Some(points);
        }
    }
    None
}

/// Applies one datapoint's converter.
///
/// Returns `None` when the reported type does not match what the table
/// declares. That is a real situation — firmware revisions change a datapoint's
/// type — and reporting nothing is better than reinterpreting the bytes, which
/// produces a number that looks like a reading.
fn convert(entry: &TuyaDatapoint, value: &Value) -> Option<StateValue> {
    Some(match (&entry.kind, value) {
        (TuyaKind::Bool { inverted }, Value::Bool(on)) => StateValue::Bool(on ^ inverted),
        // Some firmware reports a boolean datapoint as an enum or a number.
        // Accepted because refusing it would make a working switch look silent.
        (TuyaKind::Bool { inverted }, Value::Enum(v)) => StateValue::Bool((*v != 0) ^ inverted),
        (TuyaKind::Bool { inverted }, Value::Number(v)) => StateValue::Bool((*v != 0) ^ inverted),

        (TuyaKind::Value(spec), Value::Number(raw)) => {
            StateValue::Float(spec.apply(i64::from(*raw)))
        }
        (TuyaKind::Value(spec), Value::Bitmap(raw)) => {
            StateValue::Float(spec.apply(i64::from(*raw)))
        }

        // A lookup table whose values are booleans, which upstream writes as
        // `lookup({heat: true, off: false})`. Mapped to 1 and 0 by the
        // transcoder, so the same table serves both wire types.
        (TuyaKind::Enum(names), Value::Bool(on)) => {
            let want = i64::from(*on);
            names.iter().find(|(v, _)| *v == want).map_or_else(
                || StateValue::Bool(*on),
                |(_, name)| StateValue::Enum(name.clone()),
            )
        }
        (TuyaKind::Enum(names), Value::Enum(raw)) => names
            .iter()
            .find(|(v, _)| *v == i64::from(*raw))
            // An unnamed value is reported by number rather than dropped: the
            // device is telling us something, and a name can be added later.
            .map_or_else(
                || StateValue::Int(i64::from(*raw)),
                |(_, name)| StateValue::Enum(name.clone()),
            ),

        (TuyaKind::Bitmap(names), Value::Bitmap(bits)) => {
            let set: Vec<StateValue> = names
                .iter()
                .filter(|(bit, _)| bits & (1u32 << u32::from(*bit)) != 0)
                .map(|(_, name)| StateValue::Str(name.clone()))
                .collect();
            StateValue::List(set)
        }

        (TuyaKind::String, Value::Str(text)) => StateValue::Str(text.clone()),

        // Raw is the escape hatch: the bytes reach a caller as a list of
        // numbers so someone can work out what they mean.
        (TuyaKind::Raw, Value::Raw(bytes)) => StateValue::List(
            bytes
                .iter()
                .map(|b| StateValue::Int(i64::from(*b)))
                .collect(),
        ),

        // A declared type the device contradicted. Reported by returning
        // nothing rather than reinterpreting the bytes.
        _ => return None,
    })
}

/// Lowers a command to a datapoint, using the definition's table.
///
/// Returns `None` when the definition does not give the device that capability,
/// or when this build cannot express the command. Both are refusals, never a
/// guess: writing to a datapoint number that means something else on this
/// device is how a command turns a light into a factory reset.
#[must_use]
pub fn command_to_datapoint(definition: &Definition, command: &DeviceCommand) -> Option<Datapoint> {
    let (capability, intent) = intent_of(command)?;
    let entry = definition
        .tuya_datapoints
        .iter()
        .find(|d| d.name == capability)?;
    // A read-only datapoint is not writable. Sending to one is not harmless:
    // some devices treat an unexpected write as a configuration change.
    if matches!(entry.access, rszigbee_devices::Access::Report) {
        return None;
    }
    Some(Datapoint {
        dp: entry.dp,
        value: encode_intent(entry, &intent)?,
    })
}

/// What a command wants, as a capability name and a value.
enum Intent {
    Bool(bool),
    Number(i64),
    Named(String),
}

/// The capability a command addresses, and the value it asks for.
fn intent_of(command: &DeviceCommand) -> Option<(&'static str, Intent)> {
    Some(match command {
        DeviceCommand::SetOn(on) => ("state", Intent::Bool(*on)),
        DeviceCommand::SetBrightness(level) => {
            ("brightness", Intent::Number(i64::from(level.raw())))
        }
        DeviceCommand::SetPosition(percent) => {
            ("position", Intent::Number(i64::from(percent.raw())))
        }
        DeviceCommand::SetPreset(name) => ("preset", Intent::Named(name.clone())),
        DeviceCommand::SetTargetTemperature(degrees) => {
            // Scaled by the table's own divisor below, so this is the real
            // quantity rather than a raw value.
            ("current_heating_setpoint", Intent::Number(scale(*degrees)))
        }
        DeviceCommand::Open => ("state", Intent::Named("OPEN".into())),
        DeviceCommand::Close => ("state", Intent::Named("CLOSE".into())),
        DeviceCommand::Stop => ("state", Intent::Named("STOP".into())),
        DeviceCommand::Lock => ("lock", Intent::Bool(true)),
        DeviceCommand::Unlock => ("lock", Intent::Bool(false)),
        _ => return None,
    })
}

/// Rounds a real quantity to the nearest whole unit.
fn scale(value: f64) -> i64 {
    // `as` on a float is a saturating cast in Rust, so an absurd value clamps
    // rather than wrapping into a plausible one.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "saturating by definition, and a setpoint is far inside i64"
    )]
    let rounded = value.round() as i64;
    rounded
}

/// Encodes an intent for one datapoint, according to its declared kind.
fn encode_intent(entry: &TuyaDatapoint, intent: &Intent) -> Option<Value> {
    Some(match (&entry.kind, intent) {
        (TuyaKind::Bool { inverted }, Intent::Bool(on)) => Value::Bool(on ^ inverted),
        (TuyaKind::Value(spec), Intent::Number(n)) => {
            // Multiplied by the divisor on the way out, because the table's
            // divisor describes the wire and the caller speaks in real units.
            let divisor = if spec.divisor == 0 { 1 } else { spec.divisor };
            let raw = n.saturating_sub(spec.offset).saturating_mul(divisor);
            Value::Number(i32::try_from(raw).ok()?)
        }
        (TuyaKind::Enum(names), Intent::Named(name)) => {
            let raw = names.iter().find(|(_, n)| n == name).map(|(v, _)| *v)?;
            Value::Enum(u8::try_from(raw).ok()?)
        }
        (TuyaKind::Enum(names), Intent::Bool(on)) => {
            // A boolean written to an enum datapoint, which Tuya switches do:
            // the names are `ON` and `OFF`.
            let want = if *on { "ON" } else { "OFF" };
            let raw = names
                .iter()
                .find(|(_, n)| n.eq_ignore_ascii_case(want))
                .map(|(v, _)| *v)?;
            Value::Enum(u8::try_from(raw).ok()?)
        }
        (TuyaKind::String, Intent::Named(name)) => Value::Str(name.clone()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rszigbee_devices::{Access, NumericSpec};

    use crate::device::DeviceKind;

    /// A device to pass as decode context in tests.
    fn device() -> DeviceInfo {
        DeviceInfo::new(
            rszigbee_spec::ids::Ieee::new(0x1),
            rszigbee_spec::ids::Nwk::new(0x1),
            DeviceKind::EndDevice,
        )
    }

    /// The behaviours a test runs with: the shipped set.
    fn behaviors() -> BehaviorRegistry {
        BehaviorRegistry::with_builtins()
    }

    fn table(entries: Vec<TuyaDatapoint>) -> Definition {
        let mut d = Definition::new("TS0601_soil");
        d.match_rules.models = vec!["TS0601".into()];
        d.tuya_datapoints = entries;
        d
    }

    fn numeric(dp: u8, name: &str, divisor: i64, access: Access) -> TuyaDatapoint {
        let mut spec = NumericSpec::default();
        spec.divisor = divisor;
        let mut point = TuyaDatapoint::new(dp, name, TuyaKind::Value(spec));
        point.access = access;
        point
    }

    #[test]
    fn a_soil_moisture_datapoint_becomes_a_scaled_capability() {
        let definition = table(vec![
            numeric(3, "soil_moisture", 1, Access::Report),
            numeric(5, "temperature", 10, Access::Report),
        ]);
        let changes = datapoints_to_state(
            &definition,
            &[
                Datapoint {
                    dp: 3,
                    value: Value::Number(42),
                },
                Datapoint {
                    dp: 5,
                    value: Value::Number(213),
                },
            ],
            &device(),
            &behaviors(),
        );
        assert_eq!(
            changes
                .get(&CapabilityId::from("soil_moisture"))
                .and_then(StateValue::as_f64),
            Some(42.0)
        );
        assert_eq!(
            changes
                .get(&CapabilityId::from("temperature"))
                .and_then(StateValue::as_f64),
            Some(21.3),
            "the table's divisor is what makes 213 mean 21.3 degrees"
        );
    }

    #[test]
    fn a_datapoint_the_table_does_not_name_produces_nothing() {
        // Guessing would attach a countdown timer's value to whatever
        // capability happened to share its number, and the result reads like a
        // plausible measurement.
        let definition = table(vec![numeric(3, "soil_moisture", 1, Access::Report)]);
        let changes = datapoints_to_state(
            &definition,
            &[Datapoint {
                dp: 99,
                value: Value::Number(1),
            }],
            &device(),
            &behaviors(),
        );
        assert!(changes.is_empty());
    }

    #[test]
    fn an_inverted_boolean_reports_the_other_way_round() {
        let definition = table(vec![TuyaDatapoint::new(
            1,
            "contact",
            TuyaKind::Bool { inverted: true },
        )]);
        let changes = datapoints_to_state(
            &definition,
            &[Datapoint {
                dp: 1,
                value: Value::Bool(true),
            }],
            &device(),
            &behaviors(),
        );
        assert_eq!(
            changes.get(&CapabilityId::from("contact")),
            Some(&StateValue::Bool(false)),
            "an inverted contact sensor reports closed as open otherwise"
        );
    }

    #[test]
    fn a_boolean_reported_as_a_number_is_still_accepted() {
        // Firmware does this, and refusing it would make a working switch look
        // silent.
        let definition = table(vec![
            TuyaDatapoint::new(1, "state", TuyaKind::Bool { inverted: false }).writable(),
        ]);
        for value in [Value::Number(1), Value::Enum(1)] {
            let changes = datapoints_to_state(
                &definition,
                &[Datapoint { dp: 1, value }],
                &device(),
                &behaviors(),
            );
            assert_eq!(
                changes.get(&CapabilityId::from("state")),
                Some(&StateValue::Bool(true))
            );
        }
    }

    #[test]
    fn a_type_the_device_contradicted_reports_nothing_rather_than_reinterpreting() {
        // Firmware revisions change a datapoint's type. Reading a string as a
        // number would produce a value that looks like a reading.
        let definition = table(vec![numeric(5, "temperature", 10, Access::Report)]);
        let changes = datapoints_to_state(
            &definition,
            &[Datapoint {
                dp: 5,
                value: Value::Str("warm".into()),
            }],
            &device(),
            &behaviors(),
        );
        assert!(changes.is_empty());
    }

    #[test]
    fn an_unnamed_enum_value_is_reported_by_number_rather_than_dropped() {
        let definition = table(vec![
            TuyaDatapoint::new(
                4,
                "mode",
                TuyaKind::Enum(vec![(0, "auto".into()), (1, "manual".into())]),
            )
            .writable(),
        ]);
        let changes = datapoints_to_state(
            &definition,
            &[Datapoint {
                dp: 4,
                value: Value::Enum(7),
            }],
            &device(),
            &behaviors(),
        );
        assert_eq!(
            changes.get(&CapabilityId::from("mode")),
            Some(&StateValue::Int(7)),
            "the device is telling us something, and a name can be added later"
        );
    }

    #[test]
    fn a_bitmap_becomes_the_list_of_set_flag_names() {
        let definition = table(vec![TuyaDatapoint::new(
            9,
            "fault",
            TuyaKind::Bitmap(vec![(0, "low_battery".into()), (2, "tamper".into())]),
        )]);
        let changes = datapoints_to_state(
            &definition,
            &[Datapoint {
                dp: 9,
                value: Value::Bitmap(0b101),
            }],
            &device(),
            &behaviors(),
        );
        assert_eq!(
            changes.get(&CapabilityId::from("fault")),
            Some(&StateValue::List(vec![
                StateValue::Str("low_battery".into()),
                StateValue::Str("tamper".into()),
            ]))
        );
    }

    #[test]
    fn set_on_becomes_a_write_to_the_state_datapoint() {
        let definition = table(vec![
            TuyaDatapoint::new(1, "state", TuyaKind::Bool { inverted: false }).writable(),
        ]);
        let point = command_to_datapoint(&definition, &DeviceCommand::SetOn(true))
            .expect("a writable state datapoint");
        assert_eq!(point.dp, 1);
        assert_eq!(point.value, Value::Bool(true));
    }

    #[test]
    fn a_read_only_datapoint_is_not_written_to() {
        // Some devices treat an unexpected write as a configuration change, so
        // this is not merely pointless.
        let definition = table(vec![TuyaDatapoint::new(
            1,
            "state",
            TuyaKind::Bool { inverted: false },
        )]);
        assert!(command_to_datapoint(&definition, &DeviceCommand::SetOn(true)).is_none());
    }

    #[test]
    fn a_command_for_a_capability_the_table_lacks_is_refused() {
        // Writing to a datapoint number that means something else on this
        // device is how a command turns a light into a factory reset.
        let definition = table(vec![numeric(3, "soil_moisture", 1, Access::Report)]);
        assert!(command_to_datapoint(&definition, &DeviceCommand::SetOn(true)).is_none());
    }

    #[test]
    fn a_written_number_is_multiplied_by_the_tables_divisor() {
        // The divisor describes the wire; the caller speaks real units. Sending
        // 21 where the device expects 210 sets the thermostat to 2.1 degrees.
        let definition = table(vec![numeric(
            16,
            "current_heating_setpoint",
            10,
            Access::ReportAndSet,
        )]);
        let point = command_to_datapoint(&definition, &DeviceCommand::SetTargetTemperature(21.5))
            .expect("a writable setpoint");
        assert_eq!(
            point.value,
            Value::Number(220),
            "21.5 rounds to 22, then x10"
        );
    }

    #[test]
    fn a_boolean_written_to_an_enum_datapoint_finds_the_named_value() {
        // Tuya switches do this: the datapoint is an enum whose names are ON
        // and OFF.
        let definition = table(vec![
            TuyaDatapoint::new(
                1,
                "state",
                TuyaKind::Enum(vec![(0, "OFF".into()), (1, "ON".into())]),
            )
            .writable(),
        ]);
        let point = command_to_datapoint(&definition, &DeviceCommand::SetOn(true))
            .expect("an enum state datapoint");
        assert_eq!(point.value, Value::Enum(1));
    }

    #[test]
    fn a_cover_open_command_finds_the_named_enum_value() {
        let definition = table(vec![
            TuyaDatapoint::new(
                1,
                "state",
                TuyaKind::Enum(vec![
                    (0, "OPEN".into()),
                    (1, "STOP".into()),
                    (2, "CLOSE".into()),
                ]),
            )
            .writable(),
        ]);
        assert_eq!(
            command_to_datapoint(&definition, &DeviceCommand::Close).map(|p| p.value),
            Some(Value::Enum(2))
        );
    }
}
