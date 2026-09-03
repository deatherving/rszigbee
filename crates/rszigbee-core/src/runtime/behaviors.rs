//! The behaviours this build ships.
//!
//! Each one is here because a declarative table genuinely cannot express it,
//! and each is attached to one part of a definition rather than taking a device
//! over. Adding a field to the definition format to cover any of these would
//! make the schema worse for every other device.

use rszigbee_devices::TuyaKind;
use rszigbee_spec::tuya::{Datapoint, Value};

use crate::capability::CapabilityId;
use crate::command::DeviceCommand;
use crate::runtime::behavior::{DecodeContext, DeviceBehavior, EncodeContext, Outcome};
use crate::state::{StateChanges, StateValue};

/// A Tuya thermostat's weekly schedule, carried as one raw datapoint.
///
/// One datapoint holds a whole day's transitions: a day number followed by four
/// transitions of hour, minute and a two-byte temperature in tenths.
///
/// ```text
/// day | hh mm tt tt | hh mm tt tt | hh mm tt tt | hh mm tt tt
/// ```
///
/// That is a table cannot express: one value unpacks into a variable number of
/// structured entries. Adding `unpack_schedule` to the definition format would
/// be a field no other device could use.
///
/// Reported as the text form upstream uses — `"06:00/21.0 08:00/17.0 ..."` —
/// because that is what every existing tool, dashboard and automation for these
/// thermostats already reads and writes. Inventing a nicer shape would break
/// compatibility for no benefit anyone asked for.
#[derive(Debug, Clone, Copy)]
pub struct TuyaThermostatSchedule;

/// Bytes per transition: hour, minute, and a two-byte temperature.
const BYTES_PER_TRANSITION: usize = 4;

/// The lowest temperature these thermostats accept, in tenths of a degree.
const MIN_TENTHS: u16 = 50;
/// The highest, in tenths. Upstream rejects outside 5.0 to 35.0 °C, and a
/// device given something outside it stores nonsense.
const MAX_TENTHS: u16 = 350;

impl DeviceBehavior for TuyaThermostatSchedule {
    fn name(&self) -> &'static str {
        "tuya:thermostat-schedule"
    }

    fn decode_datapoint(&self, ctx: &DecodeContext<'_>) -> Outcome<StateChanges> {
        // Only this behaviour's own datapoints. A definition naming it for one
        // datapoint must not have its other datapoints intercepted.
        let Some(entry) = ctx
            .definition
            .tuya_datapoints
            .iter()
            .find(|d| d.dp == ctx.datapoint.dp)
        else {
            return Outcome::NotHandled;
        };
        if !matches!(&entry.kind, TuyaKind::Behavior { name } if name == self.name()) {
            return Outcome::NotHandled;
        }

        let Value::Raw(bytes) = &ctx.datapoint.value else {
            // The table delegated this datapoint here, so declining would
            // leave it unhandled — which is correct. The device sent something
            // other than the raw payload a schedule comes in, and inventing a
            // schedule from it would write a plausible-looking week into a
            // caller's state.
            return Outcome::NotHandled;
        };

        match decode_schedule(bytes) {
            Some(text) => {
                let mut changes = StateChanges::new();
                changes.set(CapabilityId::from(ctx.capability), StateValue::Str(text));
                Outcome::Handled(changes)
            }
            // Handled, with nothing to report: this *is* the schedule
            // datapoint, and a malformed one is not something another
            // behaviour should get a try at.
            None => Outcome::Handled(StateChanges::new()),
        }
    }

    fn encode_command(&self, ctx: &EncodeContext<'_>) -> Outcome<Vec<Datapoint>> {
        let DeviceCommand::SetPreset(text) = ctx.command else {
            return Outcome::NotHandled;
        };

        // The datapoint this behaviour owns, and the day number it carries.
        // Encoded per day, because that is how the device stores it.
        let Some(entry) = ctx
            .definition
            .tuya_datapoints
            .iter()
            .find(|d| matches!(&d.kind, TuyaKind::Behavior { name } if name == self.name()))
        else {
            return Outcome::NotHandled;
        };

        // The day number is byte zero, and upstream takes it from which
        // datapoint the schedule arrived on. Without a day the device would
        // apply the schedule to whichever day it last had selected.
        let Some(day) = schedule_day(&entry.name) else {
            return Outcome::NotHandled;
        };

        match encode_schedule(day, text) {
            Some(bytes) => Outcome::Handled(vec![Datapoint {
                dp: entry.dp,
                value: Value::Raw(bytes),
            }]),
            // Handled and refused: the text was not a valid schedule, and
            // sending a partial one would leave the thermostat following half a
            // day's transitions.
            None => Outcome::Handled(Vec::new()),
        }
    }
}

/// The day number a schedule capability names, e.g. `schedule_monday` is 1.
///
/// Upstream encodes the day in the datapoint's position; the capability name is
/// what survives transcoding, so it is read from there.
fn schedule_day(capability: &str) -> Option<u8> {
    let day = capability.rsplit('_').next()?;
    Some(match day {
        "monday" => 1,
        "tuesday" => 2,
        "wednesday" => 3,
        "thursday" => 4,
        "friday" => 5,
        "saturday" => 6,
        "sunday" => 7,
        _ => return None,
    })
}

/// Unpacks a schedule payload into the text form.
///
/// Returns `None` for a payload that is not a whole number of transitions
/// after the day byte, or that carries an impossible time. A malformed
/// schedule reported as text would be acted on by an automation.
fn decode_schedule(bytes: &[u8]) -> Option<String> {
    // Byte zero is the day number, which the device already knows; what a
    // caller wants is the transitions.
    let body = bytes.get(1..)?;
    if body.is_empty() || body.len() % BYTES_PER_TRANSITION != 0 {
        return None;
    }

    let mut parts = Vec::new();
    // Destructured rather than indexed. `as_chunks` guarantees the width, and
    // the pattern makes that visible to the compiler as well as the reader:
    // these are device-supplied bytes, and the parse-path invariant is that no
    // decoder may index into them.
    let (transitions, _) = body.as_chunks::<BYTES_PER_TRANSITION>();
    for &[hour, minute, high, low] in transitions {
        // A time outside the clock means the payload is not a schedule,
        // whatever the length said.
        if hour > 24 || minute > 59 {
            return None;
        }
        let tenths = u16::from_be_bytes([high, low]);
        parts.push(format!(
            "{hour:02}:{minute:02}/{}.{}",
            tenths / 10,
            tenths % 10
        ));
    }
    Some(parts.join(" "))
}

/// Packs the text form back into a payload.
///
/// Returns `None` on anything invalid rather than clamping. A thermostat given
/// a clamped schedule follows a week nobody asked for, and the caller has no
/// way to tell that happened.
fn encode_schedule(day: u8, text: &str) -> Option<Vec<u8>> {
    let mut out = vec![day];
    let mut transitions = 0usize;

    for transition in text.split_whitespace() {
        let (time, temperature) = transition.split_once('/')?;
        let (hour, minute) = time.split_once(':')?;
        let hour: u8 = hour.parse().ok()?;
        let minute: u8 = minute.parse().ok()?;
        let degrees: f64 = temperature.parse().ok()?;

        // The same bounds upstream enforces. Outside them the device stores
        // nonsense, so this is a refusal rather than a clamp.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "bounds-checked immediately below, and a saturating cast \
                      cannot produce a value inside the accepted range"
        )]
        let tenths = (degrees * 10.0).round() as u16;
        if hour > 24 || minute > 59 || !(MIN_TENTHS..=MAX_TENTHS).contains(&tenths) {
            return None;
        }

        out.push(hour);
        out.push(minute);
        out.extend_from_slice(&tenths.to_be_bytes());
        transitions = transitions.saturating_add(1);
    }

    (transitions > 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rszigbee_devices::{Definition, TuyaDatapoint};
    use rszigbee_spec::ids::{Ieee, Nwk};

    use crate::device::{DeviceInfo, DeviceKind};

    fn definition() -> Definition {
        let mut d = Definition::new("TS0601_thermostat");
        d.match_rules.models = vec!["TS0601".into()];
        d.tuya_datapoints = vec![
            TuyaDatapoint::new(
                28,
                "schedule_monday",
                TuyaKind::Behavior {
                    name: "tuya:thermostat-schedule".into(),
                },
            )
            .writable(),
            // A plain datapoint alongside it, which this behaviour must leave
            // alone: the escape hatch is local, not total.
            TuyaDatapoint::new(
                2,
                "current_heating_setpoint",
                TuyaKind::Value(rszigbee_devices::NumericSpec::default()),
            ),
        ];
        d
    }

    fn device() -> DeviceInfo {
        DeviceInfo::new(Ieee::new(0x1), Nwk::new(0x1), DeviceKind::EndDevice)
    }

    /// Monday, four transitions: 06:00/21.0, 08:00/17.0, 17:00/21.5, 22:00/16.0.
    fn payload() -> Vec<u8> {
        vec![1, 6, 0, 0, 210, 8, 0, 0, 170, 17, 0, 0, 215, 22, 0, 0, 160]
    }

    #[test]
    fn a_schedule_datapoint_unpacks_into_the_text_form_upstream_uses() {
        // The text form matters: every existing dashboard and automation for
        // these thermostats already reads and writes it.
        let definition = definition();
        let point = Datapoint {
            dp: 28,
            value: Value::Raw(payload()),
        };
        let outcome = TuyaThermostatSchedule.decode_datapoint(&DecodeContext {
            device: &device(),
            definition: &definition,
            datapoint: &point,
            capability: "schedule_monday",
        });
        let changes = outcome.handled().expect("this behaviour owns dp 28");
        assert_eq!(
            changes
                .get(&CapabilityId::from("schedule_monday"))
                .and_then(|v| match v {
                    StateValue::Str(s) => Some(s.as_str()),
                    _ => None,
                }),
            Some("06:00/21.0 08:00/17.0 17:00/21.5 22:00/16.0")
        );
    }

    #[test]
    fn a_datapoint_the_table_did_not_delegate_here_is_left_alone() {
        // The boundary that makes this an escape hatch rather than a takeover:
        // the thermostat's other datapoints stay declarative.
        let definition = definition();
        let point = Datapoint {
            dp: 2,
            value: Value::Number(210),
        };
        let outcome = TuyaThermostatSchedule.decode_datapoint(&DecodeContext {
            device: &device(),
            definition: &definition,
            datapoint: &point,
            capability: "current_heating_setpoint",
        });
        assert!(
            !outcome.is_handled(),
            "a plain datapoint must go through the declarative path"
        );
    }

    #[test]
    fn a_malformed_schedule_is_handled_and_reports_nothing() {
        // Handled, not declined: this *is* the schedule datapoint, so no other
        // behaviour should get a try at it. And nothing is reported, because a
        // half-decoded week would be acted on by an automation.
        let definition = definition();
        for bytes in [
            vec![1, 6, 0, 0],       // not a whole transition
            vec![1, 99, 0, 0, 210], // hour 99
            vec![1, 6, 61, 0, 210], // minute 61
            vec![1],                // day only
        ] {
            let point = Datapoint {
                dp: 28,
                value: Value::Raw(bytes.clone()),
            };
            let outcome = TuyaThermostatSchedule.decode_datapoint(&DecodeContext {
                device: &device(),
                definition: &definition,
                datapoint: &point,
                capability: "schedule_monday",
            });
            let changes = outcome.handled().unwrap_or_else(|| {
                panic!("the schedule datapoint must be claimed even when malformed: {bytes:?}")
            });
            assert!(changes.is_empty(), "{bytes:?}");
        }
    }

    #[test]
    fn writing_a_schedule_round_trips_through_the_payload() {
        let definition = definition();
        let outcome = TuyaThermostatSchedule.encode_command(&EncodeContext {
            device: &device(),
            definition: &definition,
            command: &DeviceCommand::SetPreset(
                "06:00/21.0 08:00/17.0 17:00/21.5 22:00/16.0".into(),
            ),
        });
        let points = outcome.handled().expect("a schedule command");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].dp, 28);
        assert_eq!(points[0].value, Value::Raw(payload()));
    }

    #[test]
    fn a_temperature_outside_the_device_range_is_refused_not_clamped() {
        // A thermostat given a clamped schedule follows a week nobody asked
        // for, and the caller cannot tell it happened.
        let definition = definition();
        for text in [
            "06:00/2.0",  // below 5.0
            "06:00/40.0", // above 35.0
            "25:00/21.0", // hour 25
            "06:61/21.0", // minute 61
            "06:00",      // no temperature
            "",           // nothing at all
        ] {
            let outcome = TuyaThermostatSchedule.encode_command(&EncodeContext {
                device: &device(),
                definition: &definition,
                command: &DeviceCommand::SetPreset(text.into()),
            });
            let points = outcome
                .handled()
                .unwrap_or_else(|| panic!("the command is this behaviour's: {text:?}"));
            assert!(points.is_empty(), "{text:?} should be refused");
        }
    }

    #[test]
    fn the_day_number_comes_from_the_capability_name() {
        // Byte zero is the day, and without it the device applies the schedule
        // to whichever day it last had selected.
        assert_eq!(schedule_day("schedule_monday"), Some(1));
        assert_eq!(schedule_day("schedule_sunday"), Some(7));
        assert_eq!(schedule_day("schedule_someday"), None);
    }

    #[test]
    fn a_command_this_behaviour_does_not_own_is_declined() {
        let definition = definition();
        let outcome = TuyaThermostatSchedule.encode_command(&EncodeContext {
            device: &device(),
            definition: &definition,
            command: &DeviceCommand::SetOn(true),
        });
        assert!(!outcome.is_handled());
    }
}
