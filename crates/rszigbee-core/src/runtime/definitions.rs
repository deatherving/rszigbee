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

use rszigbee_devices::{Definition, DeviceMatch, Extend, NumericSpec};
use rszigbee_spec::ids::{AttrId, ClusterId, CommandId, EndpointId, ManufacturerCode};
use rszigbee_spec::zcl::registry::ClusterDef;
use rszigbee_spec::zcl::types::{ZclType, ZclValue};

use crate::capability::CapabilityId;
use crate::command::{CommandError, DeviceCommand};
use crate::device::DeviceInfo;
use crate::state::StateValue;

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

/// Where one capability's value comes from on the wire.
///
/// The canonical cluster, attribute, type **and scaling** for a capability live
/// here rather than in the definition, because they are implied by the helper
/// rather than stated by it: upstream's `m.temperature()` takes no arguments
/// and still means "cluster 0x0402, attribute 0x0000, hundredths of a degree".
///
/// Getting the scaling from here is not a convenience. ZCL carries 21.37 °C as
/// the integer 2137, so a missing divisor reports 2137 °C — and a definition
/// transcoded from a zero-argument helper has no divisor to give.
#[derive(Debug, Clone)]
pub struct Source {
    /// The capability this feeds.
    pub capability: &'static str,
    /// Cluster it is read from.
    pub cluster: ClusterId,
    /// Attribute within the cluster.
    pub attribute: AttrId,
    /// Wire type, needed to configure reporting.
    pub ty: ZclType,
    /// How the raw value becomes a real quantity.
    pub value: ValueShape,
}

/// How a raw attribute value becomes a [`StateValue`].
#[derive(Debug, Clone)]
pub enum ValueShape {
    /// Scaled number.
    Numeric(NumericSpec),
    /// Boolean, with the raw value meaning true.
    Boolean {
        /// The raw value that means true.
        on: i64,
    },
    /// Named value.
    Named(Vec<(i64, String)>),
}

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
    /// The attribute's wire type.
    ///
    /// Carried on the step rather than looked up later, because the registry
    /// does not know every cluster a definition can name — soil moisture and
    /// CO2 are not in the built-in set — and configuring reporting with the
    /// wrong type produces a frame the device rejects.
    pub attribute_type: Option<ZclType>,
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

/// Every capability a definition's `extend` list implies, with its wiring.
///
/// This is the sensor path's counterpart to [`plan_command`]: one place that
/// decides which attribute feeds which capability, so an inbound report and an
/// outbound reporting configuration cannot disagree about it.
#[must_use]
pub fn sources(definition: &Definition) -> Vec<Source> {
    let mut out = Vec::new();
    for extend in &definition.extend {
        if let Some(mut implied) = well_known(extend) {
            out.append(&mut implied);
            continue;
        }
        match extend {
            // These carry their own wiring, so nothing is implied.
            Extend::Numeric {
                name,
                cluster,
                attribute,
                spec,
                ..
            } => out.push(Source {
                capability: leak(name),
                cluster: *cluster,
                attribute: *attribute,
                ty: ZclType::Int(2),
                value: ValueShape::Numeric(spec.clone()),
            }),
            Extend::Binary {
                name,
                cluster,
                attribute,
                value_on,
                ..
            } => out.push(Source {
                capability: leak(name),
                cluster: *cluster,
                attribute: *attribute,
                ty: ZclType::Bool,
                value: ValueShape::Boolean { on: *value_on },
            }),
            Extend::EnumLookup {
                name,
                cluster,
                attribute,
                values,
                ..
            } => out.push(Source {
                capability: leak(name),
                cluster: *cluster,
                attribute: *attribute,
                ty: ZclType::Enum8,
                value: ValueShape::Named(values.clone()),
            }),
            _ => {}
        }
    }
    out
}

/// A scaled numeric source.
fn numeric(
    capability: &'static str,
    cluster: u16,
    attribute: u16,
    ty: ZclType,
    divisor: i64,
) -> Source {
    let mut spec = NumericSpec::default();
    spec.divisor = divisor;
    Source {
        capability,
        cluster: ClusterId(cluster),
        attribute: AttrId(attribute),
        ty,
        value: ValueShape::Numeric(spec),
    }
}

/// A boolean source at attribute zero.
fn boolean(capability: &'static str, cluster: u16, ty: ZclType) -> Source {
    Source {
        capability,
        cluster: ClusterId(cluster),
        attribute: AttrId(0x0000),
        ty,
        value: ValueShape::Boolean { on: 1 },
    }
}

/// The capabilities whose cluster, attribute, type and scaling are implied by
/// the helper rather than stated by it.
///
/// Divisors come from the ZCL specification, which is where upstream's helpers
/// get them too: temperature and humidity are hundredths, and battery
/// percentage is doubled so a raw 200 means 100%.
fn well_known(extend: &Extend) -> Option<Vec<Source>> {
    Some(match extend {
        Extend::Temperature(_) => {
            vec![numeric("temperature", 0x0402, 0x0000, ZclType::Int(2), 100)]
        }
        Extend::Humidity(_) => vec![numeric("humidity", 0x0405, 0x0000, ZclType::Uint(2), 100)],
        Extend::SoilMoisture(_) => {
            vec![numeric(
                "soil_moisture",
                0x0408,
                0x0000,
                ZclType::Uint(2),
                100,
            )]
        }
        Extend::Co2(_) => vec![numeric("co2", 0x040d, 0x0000, ZclType::Single, 1)],
        Extend::Illuminance(_) => vec![numeric("illuminance", 0x0400, 0x0000, ZclType::Uint(2), 1)],
        Extend::Battery { voltage } => {
            let mut out = vec![numeric("battery", 0x0001, 0x0021, ZclType::Uint(1), 2)];
            if *voltage {
                out.push(numeric("voltage", 0x0001, 0x0020, ZclType::Uint(1), 10));
            }
            out
        }
        Extend::Occupancy => vec![boolean("occupancy", 0x0406, ZclType::Bitmap(1))],
        Extend::WindowCovering { lift, tilt, .. } => {
            let mut out = Vec::new();
            if *lift {
                out.push(numeric("position", 0x0102, 0x0008, ZclType::Uint(1), 1));
            }
            if *tilt {
                out.push(numeric("tilt", 0x0102, 0x0009, ZclType::Uint(1), 1));
            }
            out
        }
        // `lockState`: 1 is locked, 2 unlocked. Reported as the boolean a
        // caller wants rather than the raw enum.
        Extend::Lock => vec![Source {
            capability: "lock",
            cluster: LOCK,
            attribute: AttrId(0x0000),
            ty: ZclType::Enum8,
            value: ValueShape::Boolean { on: 1 },
        }],
        Extend::OnOff { .. } => vec![boolean("state", 0x0006, ZclType::Bool)],
        Extend::Light { brightness, .. } => {
            let mut out = vec![boolean("state", 0x0006, ZclType::Bool)];
            if *brightness {
                out.push(numeric("brightness", 0x0008, 0x0000, ZclType::Uint(1), 1));
            }
            out
        }
        _ => return None,
    })
}

/// Interns a definition-supplied capability name.
///
/// Definitions are loaded once and live for the process, so a leak here is
/// bounded by the catalogue rather than by traffic. The alternative is making
/// [`Source::capability`] an owned `String` and cloning it on every inbound
/// report, which is the hot path.
fn leak(name: &str) -> &'static str {
    Box::leak(name.to_owned().into_boxed_str())
}

/// The manufacturer-specific clusters a definition declares.
///
/// Registered against the device rather than globally: the same cluster id
/// means different things to different manufacturers, so a global registration
/// would make one vendor's device decode another vendor's frames with the
/// wrong attribute types.
#[must_use]
pub fn custom_clusters(definition: &Definition) -> Vec<ClusterDef> {
    definition
        .extend
        .iter()
        .filter_map(|e| match e {
            Extend::AddCustomCluster(custom) => Some(custom),
            _ => None,
        })
        .map(|custom| {
            let mut def = ClusterDef::new(custom.id.0, &custom.name);
            def.manufacturer = custom.manufacturer.map(ManufacturerCode);
            for (id, name, tag) in &custom.attributes {
                def = def.attr(*id, name, ZclType::from_u8(*tag));
            }
            for (id, name, params) in &custom.commands {
                let typed: Vec<(&str, ZclType)> = params
                    .iter()
                    .map(|(n, tag)| (n.as_str(), ZclType::from_u8(*tag)))
                    .collect();
                def = def.cmd(*id, name, &typed);
            }
            for (id, name, params) in &custom.responses {
                let typed: Vec<(&str, ZclType)> = params
                    .iter()
                    .map(|(n, tag)| (n.as_str(), ZclType::from_u8(*tag)))
                    .collect();
                def = def.rsp(*id, name, &typed);
            }
            def
        })
        .collect()
}

/// Names the action a received cluster command represents.
///
/// A remote or wall switch *sends* on/off rather than having on/off, so its
/// frames arrive as cluster-specific commands rather than attribute reports.
/// That is why this is separate from [`report_to_state`]: a button press is
/// momentary and is not state, and folding it into state is what forces
/// upstream to carry a hard-coded list of keys to exclude again afterwards.
///
/// Returns `None` when the definition does not say this device emits commands,
/// or the command is not one it names.
#[must_use]
pub fn command_to_action(
    definition: &Definition,
    cluster: ClusterId,
    command: u8,
) -> Option<(CapabilityId, String)> {
    let names = definition.extend.iter().find_map(|e| match e {
        Extend::CommandsOnOff { commands, .. } => Some(commands),
        _ => None,
    })?;
    if cluster != ON_OFF {
        return None;
    }
    // `offWithEffect` is reported as `off`: from a user's point of view the
    // button was the off button, and the effect is how it dimmed on the way.
    let action = match command {
        // 0x40 is `offWithEffect`, folded in with plain off: from a user's
        // point of view the off button was pressed, and the effect is only how
        // it faded on the way.
        0x00 | 0x40 => "off",
        0x01 => "on",
        0x02 => "toggle",
        _ => return None,
    };
    // Only actions the definition declares. A device that upstream says emits
    // on and off should not suddenly report a toggle.
    if !names.iter().any(|n| n == action) {
        return None;
    }
    Some((CapabilityId::from("action"), action.to_owned()))
}

/// Converts a reported attribute into a capability value.
///
/// Returns `None` when the definition does not say this attribute feeds
/// anything — which is normal: devices report attributes nobody modelled, and
/// inventing a capability name for them would put junk into a caller's state.
#[must_use]
pub fn report_to_state(
    definition: &Definition,
    cluster: ClusterId,
    attribute: u16,
    value: &ZclValue,
) -> Option<(CapabilityId, StateValue)> {
    let source = sources(definition)
        .into_iter()
        .find(|s| s.cluster == cluster && s.attribute.0 == attribute)?;

    // An "invalid" encoding means the device is saying "no reading". Reported
    // as null rather than as zero, which would read as a real measurement of
    // nothing, or dropped, which would look like the device went quiet.
    if value.is_invalid() {
        return Some((CapabilityId::from(source.capability), StateValue::Null));
    }

    let state = match &source.value {
        ValueShape::Numeric(spec) => {
            let raw = value
                .as_int()
                .or_else(|| value.as_uint().and_then(|v| i64::try_from(v).ok()))?;
            StateValue::Float(spec.apply(raw))
        }
        ValueShape::Boolean { on } => {
            let raw = value
                .as_uint()
                .and_then(|v| i64::try_from(v).ok())
                .or_else(|| value.as_int())
                .or_else(|| match value {
                    ZclValue::Bool(b) => Some(i64::from(*b)),
                    _ => None,
                })?;
            StateValue::Bool(raw == *on)
        }
        ValueShape::Named(values) => {
            let raw = value
                .as_uint()
                .and_then(|v| i64::try_from(v).ok())
                .or_else(|| value.as_int())?;
            let name = values
                .iter()
                .find(|(v, _)| *v == raw)
                .map(|(_, name)| name.clone())?;
            StateValue::Enum(name)
        }
    };
    Some((CapabilityId::from(source.capability), state))
}

/// What a window covering supports.
#[derive(Debug, Clone, Copy)]
struct Cover {
    lift: bool,
    tilt: bool,
    inverted: bool,
}

/// The covering controls a definition declares, if it is a covering at all.
fn cover_controls(definition: &Definition) -> Option<Cover> {
    definition.extend.iter().find_map(|e| match e {
        Extend::WindowCovering {
            lift,
            tilt,
            inverted,
        } => Some(Cover {
            lift: *lift,
            tilt: *tilt,
            inverted: *inverted,
        }),
        _ => None,
    })
}

/// Converts a requested percentage to the device's scale.
///
/// ZCL's `goToLiftPercentage` takes "percentage closed", and a caller asking
/// for a position means "percentage open" — so the value is flipped, and
/// flipped back for a device that already reports the other way round. Getting
/// this wrong closes a blind that was asked to open.
fn cover_percent(open: u8, inverted: bool) -> u8 {
    let closed = 100u8.saturating_sub(open.min(100));
    if inverted { open.min(100) } else { closed }
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
        DeviceCommand::Open | DeviceCommand::Close | DeviceCommand::Stop => {
            let cover = cover_controls(definition)
                .ok_or_else(|| CommandError::UnsupportedCapability("position".into()))?;
            let _ = cover;
            // up 0x00, down 0x01, stop 0x02. Deliberately not remapped when
            // the device's percentage scale is inverted: `inverted` describes
            // how it reports *positions*, and up is still up.
            let id = match command {
                DeviceCommand::Open => 0x00,
                DeviceCommand::Close => 0x01,
                _ => 0x02,
            };
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &[], WINDOW_COVERING),
                cluster: WINDOW_COVERING,
                command: CommandId(id),
                payload: Vec::new(),
            })
        }
        DeviceCommand::SetPosition(percent) => {
            let cover = cover_controls(definition)
                .ok_or_else(|| CommandError::UnsupportedCapability("position".into()))?;
            if !cover.lift {
                return Err(CommandError::UnsupportedCapability("position".into()));
            }
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &[], WINDOW_COVERING),
                cluster: WINDOW_COVERING,
                // goToLiftPercentage
                command: CommandId(0x05),
                payload: vec![cover_percent(percent.raw(), cover.inverted)],
            })
        }
        DeviceCommand::SetTilt(percent) => {
            let cover = cover_controls(definition)
                .ok_or_else(|| CommandError::UnsupportedCapability("tilt".into()))?;
            if !cover.tilt {
                return Err(CommandError::UnsupportedCapability("tilt".into()));
            }
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &[], WINDOW_COVERING),
                cluster: WINDOW_COVERING,
                // goToTiltPercentage
                command: CommandId(0x08),
                payload: vec![cover_percent(percent.raw(), cover.inverted)],
            })
        }
        DeviceCommand::Lock | DeviceCommand::Unlock => {
            if !definition.extend.iter().any(|e| matches!(e, Extend::Lock)) {
                return Err(CommandError::UnsupportedCapability("lock".into()));
            }
            // lockDoor 0x00, unlockDoor 0x01.
            let id = u8::from(matches!(command, DeviceCommand::Unlock));
            Ok(PlannedZcl {
                endpoint: endpoint_for(definition, info, &[], LOCK),
                cluster: LOCK,
                command: CommandId(id),
                payload: Vec::new(),
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
    let mut steps: Vec<ConfigureStep> = Vec::new();
    let mut seen: std::collections::HashSet<(EndpointId, ClusterId, Option<AttrId>)> =
        std::collections::HashSet::new();

    // Explicit bindings first, so that where a definition states an interval
    // its value wins over the default below. The definition knows more about
    // the device than a default does.
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
                attribute_type: None,
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
                // An explicit binding does not state a type, so it is resolved
                // from the capability sources when one names the same
                // attribute, and left to the caller otherwise.
                attribute_type: sources(definition)
                    .iter()
                    .find(|s| s.cluster == binding.cluster && s.attribute == reporting.attribute)
                    .map(|s| s.ty),
                min_interval: reporting.min_interval,
                max_interval: reporting.max_interval,
                min_change: reporting.min_change,
            });
        }
    }
    for step in &steps {
        seen.insert((step.endpoint, step.cluster, step.attribute));
    }

    // Then what the capabilities imply. This is the half that matters most:
    // upstream's `m.temperature()` configures reporting as part of what it
    // means, and a definition transcoded from it has an empty `bindings` list.
    // Without this a device joins, interviews, resolves, advertises a
    // temperature capability -- and never reports a temperature, which is
    // indistinguishable from a broken sensor.
    for source in sources(definition) {
        let endpoint = info
            .endpoint_with_input(source.cluster)
            .map_or(EndpointId(1), |e| e.id);
        let key = (endpoint, source.cluster, Some(source.attribute));
        if !seen.insert(key) {
            continue;
        }
        steps.push(ConfigureStep {
            endpoint,
            cluster: source.cluster,
            attribute: Some(source.attribute),
            attribute_type: Some(source.ty),
            min_interval: DEFAULT_MIN_INTERVAL,
            max_interval: DEFAULT_MAX_INTERVAL,
            // Report any change. A threshold suppresses small movements, and
            // choosing one is per-device tuning the definition does not do.
            min_change: 0,
        });
    }
    steps
}

/// Ten seconds. Short enough that a state change is prompt, long enough that a
/// chatty device cannot saturate the network.
const DEFAULT_MIN_INTERVAL: u16 = 10;

/// An hour. This is the number availability depends on: until it elapses, a
/// device that only reports on change is indistinguishable from a dead one.
const DEFAULT_MAX_INTERVAL: u16 = 3600;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Brightness;
    use crate::device::{DeviceKind, EndpointInfo};
    use crate::state::StateValue;
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
            command_to_action(&definition, ON_OFF, 0x01).expect("on is declared");
        assert_eq!(capability.as_str(), "action");
        assert_eq!(action, "on");

        // `offWithEffect` is still the off button from a user's point of view.
        assert_eq!(
            command_to_action(&definition, ON_OFF, 0x40).map(|(_, a)| a),
            Some("off".to_owned())
        );

        // Not declared, so not reported: upstream says this remote sends on
        // and off, and inventing a toggle would surface an action that never
        // happened.
        assert!(command_to_action(&definition, ON_OFF, 0x02).is_none());
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
        assert!(command_to_action(&light_definition(), ON_OFF, 0x01).is_none());
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
