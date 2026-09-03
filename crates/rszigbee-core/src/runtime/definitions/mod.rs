//! Turning a resolved definition into behaviour.
//!
//! This is the seam the whole compatibility layer exists to cross. A definition
//! is data; a command is a typed intent; and this module is the only place that
//! decides which cluster and command an intent becomes for a given device.
//!
//! # Nothing here guesses
//!
//! Every mapping is derived from the definition. If the definition does not say
//! a device has on/off, `SetOn` is refused — it does not fall back to "well,
//! most things have `genOnOff`". A fallback would appear to work on the devices
//! where the guess is right, which is exactly what makes it dangerous: the
//! failures are silent and land on whichever device the guess is wrong for.
//!
//! Refusals are typed and name what was missing, so "this device does not do
//! that" is distinguishable from "rszigbee cannot express that yet".

use rszigbee_devices::DeviceMatch;
use rszigbee_spec::ids::ClusterId;

use crate::device::DeviceInfo;

mod commands;
mod configure;
mod sources;

pub use commands::{PlannedZcl, plan_command};
pub use configure::{ConfigureStep, configure_plan};
pub use sources::{command_to_action, custom_clusters, report_to_state};

/// `genOnOff`.
const ON_OFF: ClusterId = ClusterId(0x0006);

/// `genLevelCtrl`.
const LEVEL: ClusterId = ClusterId(0x0008);

/// `genIdentify`.
const IDENTIFY: ClusterId = ClusterId(0x0003);

/// `closuresWindowCovering`.
const WINDOW_COVERING: ClusterId = ClusterId(0x0102);

/// `closuresDoorLock`.
const LOCK: ClusterId = ClusterId(0x0101);

/// Builds the facts a definition is matched on from what the interview learned.
///
/// Everything is optional because an interview can be partial, and a device
/// that answered `genBasic` but refused its endpoints is still resolvable.
#[must_use]
pub fn device_match(info: &DeviceInfo) -> DeviceMatch {
    let mut m = DeviceMatch::default();
    m.model_id.clone_from(&info.basic.model_id);
    m.manufacturer_name
        .clone_from(&info.basic.manufacturer_name);
    m.application_version = info.basic.app_version;
    m.stack_version = info.basic.stack_version;
    m.zcl_version = info.basic.zcl_version;
    m.hardware_version = info.basic.hardware_version;
    m.date_code.clone_from(&info.basic.date_code);
    m.software_build_id
        .clone_from(&info.basic.software_build_id);
    m.ieee = Some(info.ieee);
    m.endpoints = info
        .endpoints
        .iter()
        .map(|e| {
            let mut em = rszigbee_devices::EndpointMatch::new(e.id);
            em.profile = Some(e.profile);
            em.device_id = Some(e.device_id);
            em.input_clusters.clone_from(&e.input_clusters);
            em.output_clusters.clone_from(&e.output_clusters);
            em
        })
        .collect();
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    // Named explicitly rather than leaning on `use super::*`: `Extend` is also
    // in the prelude as `std::iter::Extend`, so an implicit import silently
    // resolves to the trait and the error points at the wrong thing.
    use rszigbee_devices::{Definition, Extend, NumericSpec};
    use rszigbee_spec::ids::{AttrId, ClusterId, CommandId, EndpointId, Ieee, Nwk, ProfileId};
    use rszigbee_spec::zcl::types::{ZclType, ZclValue};

    use crate::command::{Brightness, CommandError, DeviceCommand};
    use crate::device::{DeviceKind, EndpointInfo};
    use crate::state::StateValue;

    fn light_definition() -> Definition {
        let mut d = Definition::new("TRADFRI bulb");
        d.match_rules.models = vec!["TRADFRI bulb E27 WS opal 980lm".into()];
        d.extend = vec![
            Extend::Light {
                brightness: true,
                color_temp: Some((250, 454)),
                color: false,
            },
            Extend::Identify,
        ];
        d
    }

    fn sensor_definition() -> Definition {
        let mut d = Definition::new("TS0601_soil");
        d.match_rules.models = vec!["TS0601".into()];
        d.extend = vec![Extend::Temperature(NumericSpec::default())];
        d
    }

    fn device(clusters: &[u16]) -> DeviceInfo {
        let mut info = DeviceInfo::new(
            Ieee::new(0x0012_4b00_2218_9abc),
            Nwk::new(0x1234),
            DeviceKind::Router,
        );
        info.endpoints = vec![EndpointInfo {
            id: EndpointId(1),
            profile: ProfileId::HA,
            device_id: 0x0100,
            input_clusters: clusters.iter().copied().map(ClusterId).collect(),
            output_clusters: Vec::new(),
        }];
        info
    }

    #[test]
    fn set_on_becomes_the_gen_on_off_on_command() {
        let planned = plan_command(
            &light_definition(),
            &device(&[0x0000, 0x0006, 0x0008]),
            &DeviceCommand::SetOn(true),
        )
        .expect("a light has on/off");
        assert_eq!(planned.cluster, ON_OFF);
        assert_eq!(planned.command, CommandId(0x01));
        assert!(planned.payload.is_empty());
        assert_eq!(planned.endpoint, EndpointId(1));
    }

    #[test]
    fn set_off_becomes_command_zero() {
        let planned = plan_command(
            &light_definition(),
            &device(&[0x0006]),
            &DeviceCommand::SetOn(false),
        )
        .expect("a light has on/off");
        assert_eq!(planned.command, CommandId(0x00));
    }

    #[test]
    fn a_sensor_refuses_set_on_rather_than_guessing_gen_on_off() {
        // The whole point. A temperature sensor has no on/off, and falling back
        // to "most things have genOnOff" would send a command that either does
        // nothing or does something unintended.
        let error = plan_command(
            &sensor_definition(),
            &device(&[0x0000, 0x0402]),
            &DeviceCommand::SetOn(true),
        )
        .expect_err("a sensor must refuse on/off");
        assert!(
            matches!(error, CommandError::UnsupportedCapability(ref c) if c.as_str() == "state"),
            "{error:?}"
        );
    }

    #[test]
    fn brightness_uses_move_to_level_with_on_off_so_a_dark_light_turns_on() {
        let planned = plan_command(
            &light_definition(),
            &device(&[0x0006, 0x0008]),
            &DeviceCommand::SetBrightness(Brightness::from_percent(50)),
        )
        .expect("a light has brightness");
        assert_eq!(planned.cluster, LEVEL);
        // 0x04 is moveToLevelWithOnOff; 0x00 would leave an off light off.
        assert_eq!(planned.command, CommandId(0x04));
        assert_eq!(planned.payload.len(), 3, "level plus a 16-bit transition");
    }

    #[test]
    fn a_sensor_refuses_brightness() {
        let error = plan_command(
            &sensor_definition(),
            &device(&[0x0402]),
            &DeviceCommand::SetBrightness(Brightness::from_percent(50)),
        )
        .expect_err("a sensor has no brightness");
        assert!(
            matches!(error, CommandError::UnsupportedCapability(ref c) if c.as_str() == "brightness"),
            "{error:?}"
        );
    }

    #[test]
    fn an_unmapped_command_is_an_explicit_error_not_an_approximation() {
        let error = plan_command(
            &light_definition(),
            &device(&[0x0006]),
            &DeviceCommand::SetTargetTemperature(21.0),
        )
        .expect_err("thermostats are not mapped yet");
        assert!(matches!(error, CommandError::NoDefinition), "{error:?}");
    }

    #[test]
    fn locking_a_light_is_refused_as_an_unsupported_capability() {
        // Distinct from the case above: `Lock` *is* mapped now, so the right
        // answer is "this device has no lock", not "rszigbee cannot do locks".
        let error = plan_command(
            &light_definition(),
            &device(&[0x0006]),
            &DeviceCommand::Lock,
        )
        .expect_err("a bulb has no lock");
        assert!(
            matches!(error, CommandError::UnsupportedCapability(ref c) if c.as_str() == "lock"),
            "{error:?}"
        );
    }

    fn cover_definition(inverted: bool) -> Definition {
        let mut d = Definition::new("cover");
        d.match_rules.models = vec!["COVER".into()];
        d.extend = vec![Extend::WindowCovering {
            lift: true,
            tilt: false,
            inverted,
        }];
        d
    }

    #[test]
    fn opening_a_cover_is_the_up_command() {
        let planned = plan_command(
            &cover_definition(false),
            &device(&[0x0102]),
            &DeviceCommand::Open,
        )
        .expect("a covering opens");
        assert_eq!(planned.cluster, WINDOW_COVERING);
        assert_eq!(planned.command, CommandId(0x00));
        assert!(planned.payload.is_empty());
    }

    #[test]
    fn a_position_is_sent_as_percentage_closed_not_percentage_open() {
        // ZCL's goToLiftPercentage takes percentage *closed*, and a caller
        // asking for a position means percentage open. Getting this backwards
        // closes a blind that was asked to open.
        let planned = plan_command(
            &cover_definition(false),
            &device(&[0x0102]),
            &DeviceCommand::SetPosition(crate::command::Percent::new(30)),
        )
        .expect("a covering positions");
        assert_eq!(planned.command, CommandId(0x05));
        assert_eq!(planned.payload, vec![70], "30% open is 70% closed");
    }

    #[test]
    fn an_inverted_cover_gets_the_value_the_other_way_round() {
        let planned = plan_command(
            &cover_definition(true),
            &device(&[0x0102]),
            &DeviceCommand::SetPosition(crate::command::Percent::new(30)),
        )
        .expect("a covering positions");
        assert_eq!(
            planned.payload,
            vec![30],
            "an inverted motor already counts the other way"
        );
    }

    #[test]
    fn tilting_a_cover_that_cannot_tilt_is_refused() {
        let error = plan_command(
            &cover_definition(false),
            &device(&[0x0102]),
            &DeviceCommand::SetTilt(crate::command::Percent::new(50)),
        )
        .expect_err("this covering declares lift only");
        assert!(
            matches!(error, CommandError::UnsupportedCapability(ref c) if c.as_str() == "tilt"),
            "{error:?}"
        );
    }

    #[test]
    fn a_lock_state_report_becomes_a_boolean() {
        let mut definition = Definition::new("lock");
        definition.match_rules.models = vec!["LOCK".into()];
        definition.extend = vec![Extend::Lock];

        // lockState 1 = locked, 2 = unlocked.
        let (capability, value) =
            report_to_state(&definition, LOCK, 0x0000, &ZclValue::Enum(1)).expect("modelled");
        assert_eq!(capability.as_str(), "lock");
        assert_eq!(value, StateValue::Bool(true));

        let (_, unlocked) =
            report_to_state(&definition, LOCK, 0x0000, &ZclValue::Enum(2)).expect("modelled");
        assert_eq!(unlocked, StateValue::Bool(false));
    }

    #[test]
    fn a_received_on_command_becomes_an_action_not_state() {
        let mut definition = Definition::new("remote");
        definition.match_rules.models = vec!["REMOTE".into()];
        definition.extend = vec![Extend::CommandsOnOff {
            commands: vec!["on".into(), "off".into()],
            endpoints: Vec::new(),
        }];

        let (capability, action) =
            command_to_action(&definition, ON_OFF, 0x01, &[]).expect("on is declared");
        assert_eq!(capability.as_str(), "action");
        assert_eq!(action, "on");

        // `offWithEffect` is still the off button from a user's point of view.
        assert_eq!(
            command_to_action(&definition, ON_OFF, 0x40, &[]).map(|(_, a)| a),
            Some("off".to_owned())
        );

        // Not declared, so not reported: upstream says this remote sends on
        // and off, and inventing a toggle would surface an action that never
        // happened.
        assert!(command_to_action(&definition, ON_OFF, 0x02, &[]).is_none());
    }

    #[test]
    fn a_command_with_composite_parameters_is_refused_not_sent_short() {
        // `genScenes.add` takes extension field sets, which have no `ZclType`.
        // Encoding it with an empty payload would send a frame that is
        // silently too short, and the device would either reject it or act on
        // whatever followed in its buffer.
        let registry = rszigbee_spec::zcl::registry::ClusterRegistry::with_builtins();
        let error = crate::runtime::encode::command(
            &registry,
            Ieee::new(0x1),
            0,
            &crate::command::ZclCommand {
                endpoint: Some(EndpointId(1)),
                cluster: ClusterId(0x0005),
                command: CommandId(0x00),
                params: Vec::new(),
                manufacturer: None,
                disable_default_response: false,
            },
        )
        .expect_err("a command with untypeable parameters must be refused");
        assert!(
            matches!(error, crate::runtime::EncodeError::UntypedParameters { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_bulb_does_not_turn_its_own_commands_into_actions() {
        // A light *has* on/off; it does not *emit* it. Without this a bulb
        // echoing a command back would look like a button press.
        assert!(command_to_action(&light_definition(), ON_OFF, 0x01, &[]).is_none());
    }

    #[test]
    fn a_named_endpoint_from_the_definition_beats_the_first_cluster_host() {
        // A two-gang switch: both endpoints host genOnOff, and only the
        // definition knows which is which. Picking the first host would switch
        // the wrong gang.
        let mut definition = Definition::new("two gang");
        definition.match_rules.models = vec!["TS0002".into()];
        definition.extend = vec![Extend::OnOff {
            endpoints: vec![EndpointId(2)],
            power_on_behavior: false,
        }];

        let mut info = device(&[0x0006]);
        info.endpoints.push(EndpointInfo {
            id: EndpointId(2),
            profile: ProfileId::HA,
            device_id: 0x0100,
            input_clusters: vec![ClusterId(0x0006)],
            output_clusters: Vec::new(),
        });

        let planned = plan_command(&definition, &info, &DeviceCommand::SetOn(true))
            .expect("declared endpoint");
        assert_eq!(planned.endpoint, EndpointId(2));
    }

    #[test]
    fn identify_needs_no_capability_because_every_device_has_it() {
        let planned = plan_command(
            &sensor_definition(),
            &device(&[0x0000]),
            &DeviceCommand::Identify {
                duration: core::time::Duration::from_secs(3),
            },
        )
        .expect("genIdentify is mandatory");
        assert_eq!(planned.cluster, IDENTIFY);
        assert_eq!(planned.payload, vec![0x03, 0x00]);
    }

    #[test]
    fn a_capability_implies_its_reporting_even_with_no_explicit_binding() {
        // The failure this exists to prevent, end to end: a device that joins,
        // interviews, resolves a definition, advertises a temperature -- and
        // then never reports one, because nothing configured reporting.
        //
        // Upstream's `m.temperature()` configures reporting as part of what it
        // means, and a definition transcoded from it has no `bindings` at all.
        let definition = sensor_definition();
        assert!(
            definition.bindings.is_empty(),
            "the premise: this definition has no explicit binding"
        );

        let plan = configure_plan(&definition, &device(&[0x0000, 0x0402]));
        let temperature = plan
            .iter()
            .find(|s| s.cluster == ClusterId(0x0402))
            .expect("temperature reporting must be planned from the capability alone");
        assert_eq!(temperature.attribute, Some(AttrId(0x0000)));
        assert!(temperature.max_interval > 0);
        assert_eq!(
            temperature.endpoint,
            EndpointId(1),
            "the endpoint that hosts the cluster"
        );
    }

    #[test]
    fn a_temperature_report_is_scaled_even_though_the_definition_carries_no_divisor() {
        // `m.temperature()` takes no arguments, so the transcoded definition
        // has `NumericSpec::default()` with a divisor of one. The divisor is
        // implied by the capability, and if it were taken from the definition
        // this would report 2137 degrees.
        let (capability, value) = report_to_state(
            &sensor_definition(),
            ClusterId(0x0402),
            0x0000,
            &ZclValue::Int(2137),
        )
        .expect("temperature is a modelled capability");
        assert_eq!(capability.as_str(), "temperature");
        match value {
            StateValue::Float(v) => assert!((v - 21.37).abs() < 1e-9, "got {v}"),
            other => panic!("expected a float, got {other:?}"),
        }
    }

    #[test]
    fn an_attribute_nothing_models_is_ignored_rather_than_invented() {
        // Devices report attributes nobody modelled. Making up a capability
        // name for them puts junk in a caller's state.
        assert!(
            report_to_state(
                &sensor_definition(),
                ClusterId(0x0402),
                0x0099,
                &ZclValue::Int(1)
            )
            .is_none()
        );
    }

    #[test]
    fn an_invalid_reading_becomes_null_not_zero() {
        // 0x8000 for an int16 means "no reading". Zero would read as a real
        // measurement; dropping it would look like the device went quiet.
        let (_, value) = report_to_state(
            &sensor_definition(),
            ClusterId(0x0402),
            0x0000,
            &ZclValue::Invalid(ZclType::Int(2)),
        )
        .expect("still a modelled capability");
        assert_eq!(value, StateValue::Null);
    }

    #[test]
    fn an_on_off_report_becomes_a_boolean_state() {
        let (capability, value) = report_to_state(
            &light_definition(),
            ClusterId(0x0006),
            0x0000,
            &ZclValue::Bool(true),
        )
        .expect("a light reports state");
        assert_eq!(capability.as_str(), "state");
        assert_eq!(value, StateValue::Bool(true));
    }

    #[test]
    fn a_battery_percentage_is_halved_because_zcl_doubles_it() {
        let mut definition = sensor_definition();
        definition.extend.push(Extend::Battery { voltage: false });
        let (capability, value) =
            report_to_state(&definition, ClusterId(0x0001), 0x0021, &ZclValue::Uint(200))
                .expect("battery is modelled");
        assert_eq!(capability.as_str(), "battery");
        // 200 raw is 100%, not 200%.
        assert_eq!(value, StateValue::Float(100.0));
    }

    #[test]
    fn the_configure_plan_lists_a_step_per_reported_attribute() {
        let mut definition = sensor_definition();
        let mut binding = rszigbee_devices::Binding::default();
        binding.endpoint = EndpointId(1);
        binding.cluster = ClusterId(0x0402);
        // Two distinct attributes; the same one twice would be a meaningless
        // duplicate and is collapsed.
        let mut second = rszigbee_devices::Reporting::default();
        second.attribute = AttrId(0x0001);
        binding.reporting = vec![rszigbee_devices::Reporting::default(), second];
        definition.bindings = vec![binding];

        let plan = configure_plan(&definition, &device(&[0x0402]));
        // One implied by the temperature capability, plus the explicit ones.
        assert!(plan.len() >= 2, "{plan:?}");
        assert!(plan.iter().all(|s| s.cluster == ClusterId(0x0402)));
        assert!(
            plan.iter().all(|s| s.max_interval > 0),
            "a max interval of zero makes a dead device indistinguishable from a quiet one"
        );
    }

    #[test]
    fn a_binding_for_an_endpoint_the_device_lacks_is_dropped() {
        let mut definition = sensor_definition();
        let mut binding = rszigbee_devices::Binding::default();
        binding.endpoint = EndpointId(7);
        binding.cluster = ClusterId(0x0402);
        definition.bindings = vec![binding];

        // Emitting it would be a guaranteed failure at join time. The
        // capability-implied step for endpoint 1 is still there, which is
        // correct -- only the impossible binding is dropped.
        let plan = configure_plan(&definition, &device(&[0x0402]));
        assert!(
            plan.iter().all(|s| s.endpoint != EndpointId(7)),
            "a binding for an endpoint the device lacks must be dropped: {plan:?}"
        );
        assert!(!plan.is_empty(), "the implied step should survive");
    }

    #[test]
    fn the_match_input_carries_what_gen_basic_learned() {
        let mut info = device(&[0x0000]);
        info.basic.model_id = Some("TS0601".into());
        info.basic.manufacturer_name = Some("_TZE200_myd45weu".into());

        let m = device_match(&info);
        assert_eq!(m.model_id.as_deref(), Some("TS0601"));
        assert_eq!(m.manufacturer_name.as_deref(), Some("_TZE200_myd45weu"));
        assert_eq!(m.endpoints.len(), 1);
        // Both addresses matter: fingerprints key on the manufacturer name, and
        // a few key on an address prefix.
        assert!(m.ieee.is_some());
    }
}
