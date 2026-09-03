//! What a device reports, and what it means.
//!
//! One table decides which attribute or datapoint feeds which capability, and
//! it is the same table the outbound side reads. Keeping the two apart is how a
//! value comes to be readable under one name and writable under another.
//!
//! The canonical cluster, attribute, wire type *and scaling* for a well-known
//! capability live here rather than in a definition, because upstream implies
//! them rather than stating them: `m.temperature()` takes no arguments and
//! still means hundredths of a degree on cluster 0x0402.

use rszigbee_devices::{Definition, Extend, NumericSpec};
use rszigbee_spec::ids::{AttrId, ClusterId, ManufacturerCode};
use rszigbee_spec::zcl::registry::ClusterDef;
use rszigbee_spec::zcl::types::{ZclType, ZclValue};

use crate::capability::CapabilityId;
use crate::state::StateValue;

use super::{LEVEL, LOCK, ON_OFF};

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
pub(super) fn numeric(
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
pub(super) fn boolean(capability: &'static str, cluster: u16, ty: ZclType) -> Source {
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
pub(super) fn well_known(extend: &Extend) -> Option<Vec<Source>> {
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
pub(super) fn leak(name: &str) -> &'static str {
    Box::leak(name.to_owned().into_boxed_str())
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
    params: &[(String, ZclValue)],
) -> Option<(CapabilityId, String)> {
    let (declared, action) = match cluster {
        ON_OFF => {
            let declared = definition.extend.iter().find_map(|e| match e {
                Extend::CommandsOnOff { commands, .. } => Some(commands),
                _ => None,
            })?;
            let action = match command {
                // 0x40 is `offWithEffect`, folded in with plain off: from a
                // user's point of view the off button was pressed, and the
                // effect is only how it faded on the way.
                0x00 | 0x40 => "off",
                0x01 => "on",
                0x02 => "toggle",
                _ => return None,
            };
            (declared, action.to_owned())
        }
        LEVEL => {
            let declared = definition.extend.iter().find_map(|e| match e {
                Extend::CommandsLevelCtrl { commands, .. } => Some(commands),
                _ => None,
            })?;
            (declared, level_action(command, params)?)
        }
        _ => return None,
    };

    // Only actions the definition declares. A device upstream says emits up
    // and down should not suddenly report a step.
    if !declared.iter().any(|n| n == &action) {
        return None;
    }
    Some((CapabilityId::from("action"), action))
}

/// Names a level-control command, including its direction.
///
/// The direction is in the payload, not the command id: `move` and `step` each
/// carry a mode byte where 0 is up and 1 is down. Reporting both as one action
/// would make a dimmer remote's two directions indistinguishable, which is the
/// only thing anyone wants from it.
pub(super) fn level_action(command: u8, params: &[(String, ZclValue)]) -> Option<String> {
    /// Reads a named mode byte, defaulting to up when absent.
    fn upward(params: &[(String, ZclValue)], name: &str) -> bool {
        params
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_uint())
            .is_none_or(|mode| mode == 0)
    }

    Some(
        match command {
            // The `WithOnOff` variants are the same gesture: a remote's button
            // does not become a different button because it also turns the
            // light on.
            0x00 | 0x04 => "brightness_move_to_level",
            0x01 | 0x05 => {
                if upward(params, "movemode") {
                    "brightness_move_up"
                } else {
                    "brightness_move_down"
                }
            }
            0x02 | 0x06 => {
                if upward(params, "stepmode") {
                    "brightness_step_up"
                } else {
                    "brightness_step_down"
                }
            }
            0x03 | 0x07 => "brightness_stop",
            _ => return None,
        }
        .to_owned(),
    )
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
