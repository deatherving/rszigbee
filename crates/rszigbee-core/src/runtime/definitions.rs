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

use rszigbee_devices::{Definition, DeviceMatch, Extend};
use rszigbee_spec::ids::{AttrId, ClusterId, CommandId, EndpointId};

use crate::command::{CommandError, DeviceCommand};
use crate::device::DeviceInfo;

/// `genOnOff`.
const ON_OFF: ClusterId = ClusterId(0x0006);
/// `genLevelCtrl`.
const LEVEL: ClusterId = ClusterId(0x0008);
/// `genIdentify`.
const IDENTIFY: ClusterId = ClusterId(0x0003);

/// A command lowered to a ZCL frame, ready for the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedZcl {
    /// Endpoint to send to.
    pub endpoint: EndpointId,
    /// Cluster.
    pub cluster: ClusterId,
    /// Command within the cluster.
    pub command: CommandId,
    /// Payload after the ZCL header.
    pub payload: Vec<u8>,
}

/// One binding-and-reporting step a definition asks for at join time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigureStep {
    /// Endpoint to bind.
    pub endpoint: EndpointId,
    /// Cluster to bind.
    pub cluster: ClusterId,
    /// Attribute to configure reporting for, when there is one.
    pub attribute: Option<AttrId>,
    /// Shortest reporting interval, seconds.
    pub min_interval: u16,
    /// Longest interval before the device reports anyway, seconds.
    pub max_interval: u16,
    /// Smallest change worth reporting.
    pub min_change: u64,
}

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

/// Whether the definition says this device has on/off, and on which endpoints.
fn on_off_endpoints(definition: &Definition) -> Option<Vec<EndpointId>> {
    for extend in &definition.extend {
        match extend {
            Extend::OnOff { endpoints, .. } => return Some(endpoints.clone()),
            // A light is on/off plus brightness, so it answers `SetOn` too.
            Extend::Light { .. } => return Some(Vec::new()),
            _ => {}
        }
    }
    None
}

/// Whether the definition says this device has brightness.
fn has_brightness(definition: &Definition) -> bool {
    definition.extend.iter().any(|e| {
        matches!(
            e,
            Extend::Light {
                brightness: true,
                ..
            }
        )
    })
}

/// Picks the endpoint to address for `cluster`.
///
/// Definition first, then what the device actually reported, then endpoint 1.
/// The order matters on a multi-gang switch: the definition names which
/// endpoint is which, and picking the first that merely hosts the cluster would
/// turn on the wrong gang.
fn endpoint_for(
    definition: &Definition,
    info: &DeviceInfo,
    declared: &[EndpointId],
    cluster: ClusterId,
) -> EndpointId {
    if let Some(first) = declared.first() {
        return *first;
    }
    if let Some((_, id)) = definition.endpoint_names.first() {
        return *id;
    }
    info.endpoint_with_input(cluster)
        .map_or(EndpointId(1), |e| e.id)
}

/// Lowers a command to a ZCL frame using the definition.
///
/// # Errors
///
/// [`CommandError::UnsupportedCapability`] when the definition does not give
/// the device that capability, and [`CommandError::NoDefinition`] for commands
/// this build cannot yet express. Both are explicit: nothing is guessed and
/// nothing silently does less than asked.
pub fn plan_command(
    definition: &Definition,
    info: &DeviceInfo,
    command: &DeviceCommand,
) -> Result<PlannedZcl, CommandError> {
    match command {
        DeviceCommand::SetOn(on) => {
            let declared = on_off_endpoints(definition)
                .ok_or_else(|| CommandError::UnsupportedCapability("state".into()))?;
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &declared, ON_OFF),
                cluster: ON_OFF,
                command: CommandId(u8::from(*on)),
                payload: Vec::new(),
            })
        }
        DeviceCommand::Toggle => {
            let declared = on_off_endpoints(definition)
                .ok_or_else(|| CommandError::UnsupportedCapability("state".into()))?;
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &declared, ON_OFF),
                cluster: ON_OFF,
                command: CommandId(0x02),
                payload: Vec::new(),
            })
        }
        DeviceCommand::SetBrightness(level) => {
            if !has_brightness(definition) {
                return Err(CommandError::UnsupportedCapability("brightness".into()));
            }
            // `moveToLevelWithOnOff`, not `moveToLevel`: setting a brightness
            // on a light that is off should turn it on, which is what every
            // user means and what upstream does.
            let mut payload = Vec::with_capacity(3);
            payload.push(level.raw());
            // Transition time in tenths of a second. Zero means "as fast as
            // the device can", which is the only honest default when the
            // definition does not say.
            payload.extend_from_slice(&0u16.to_le_bytes());
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &[], LEVEL),
                cluster: LEVEL,
                command: CommandId(0x04),
                payload,
            })
        }
        DeviceCommand::Identify { duration } => {
            // Every Zigbee device implements `genIdentify`, so this needs no
            // capability in the definition.
            let seconds = u16::try_from(duration.as_secs()).unwrap_or(u16::MAX);
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &[], IDENTIFY),
                cluster: IDENTIFY,
                command: CommandId(0x00),
                payload: seconds.to_le_bytes().to_vec(),
            })
        }
        // Not yet mapped. Returned as an explicit error naming the gap rather
        // than approximated with a related command.
        _ => Err(CommandError::NoDefinition),
    }
}

/// Materialises the bindings and reporting a definition asks for.
///
/// Producing the plan is separate from executing it on purpose: an operator
/// wants to see what joining a device will do to it before it happens, and a
/// plan that can be inspected is also a plan that can be tested without a
/// radio.
///
/// Without reporting configured a sensor pairs, interviews, and then appears
/// silent forever, which is the most common way a working device looks broken.
#[must_use]
pub fn configure_plan(definition: &Definition, info: &DeviceInfo) -> Vec<ConfigureStep> {
    let mut steps = Vec::new();
    for binding in &definition.bindings {
        // An endpoint the device does not have cannot be bound. Emitting the
        // step anyway would produce a guaranteed failure at join time.
        if !info.endpoints.is_empty() && info.endpoint(binding.endpoint).is_none() {
            continue;
        }
        if binding.reporting.is_empty() {
            steps.push(ConfigureStep {
                endpoint: binding.endpoint,
                cluster: binding.cluster,
                attribute: None,
                min_interval: 0,
                max_interval: 0,
                min_change: 0,
            });
            continue;
        }
        for reporting in &binding.reporting {
            steps.push(ConfigureStep {
                endpoint: binding.endpoint,
                cluster: binding.cluster,
                attribute: Some(reporting.attribute),
                min_interval: reporting.min_interval,
                max_interval: reporting.max_interval,
                min_change: reporting.min_change,
            });
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Brightness;
    use crate::device::{DeviceKind, EndpointInfo};
    use rszigbee_spec::ids::{Ieee, Nwk, ProfileId};

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
        d.extend = vec![Extend::Temperature(rszigbee_devices::NumericSpec::default())];
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
            &DeviceCommand::Lock,
        )
        .expect_err("Lock is not mapped yet");
        assert!(matches!(error, CommandError::NoDefinition), "{error:?}");
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
    fn the_configure_plan_lists_a_step_per_reported_attribute() {
        let mut definition = sensor_definition();
        let mut binding = rszigbee_devices::Binding::default();
        binding.endpoint = EndpointId(1);
        binding.cluster = ClusterId(0x0402);
        binding.reporting = vec![
            rszigbee_devices::Reporting::default(),
            rszigbee_devices::Reporting::default(),
        ];
        definition.bindings = vec![binding];

        let plan = configure_plan(&definition, &device(&[0x0402]));
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].cluster, ClusterId(0x0402));
        assert!(
            plan[0].max_interval > 0,
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

        // Emitting it would be a guaranteed failure at join time.
        assert!(configure_plan(&definition, &device(&[0x0402])).is_empty());
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
