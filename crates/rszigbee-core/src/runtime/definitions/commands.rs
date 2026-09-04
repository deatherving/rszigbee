//! Turning an intent into a cluster and a command.
//!
//! Every mapping is derived from the definition, and there is no fallback. If
//! the definition does not say a device has on/off, `SetOn` is refused rather
//! than sent to `genOnOff` on the assumption that most things have it — the
//! devices where that assumption holds would work, and the rest would fail
//! silently.

use rszigbee_devices::{Definition, Extend};
use rszigbee_spec::ids::{ClusterId, CommandId, EndpointId};

use crate::capability::CapabilityId;
use crate::command::{Brightness, CommandError, DeviceCommand, Mireds, Percent};
use crate::device::DeviceInfo;
use crate::state::{StateChanges, StateValue};

use super::{IDENTIFY, LEVEL, LOCK, ON_OFF, WINDOW_COVERING};

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

/// What a window covering supports.
#[derive(Debug, Clone, Copy)]
pub(super) struct Cover {
    lift: bool,
    tilt: bool,
    inverted: bool,
}

/// The covering controls a definition declares, if it is a covering at all.
pub(super) fn cover_controls(definition: &Definition) -> Option<Cover> {
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
pub(super) fn cover_percent(open: u8, inverted: bool) -> u8 {
    let closed = 100u8.saturating_sub(open.min(100));
    if inverted { open.min(100) } else { closed }
}

/// Whether the definition says this device has on/off, and on which endpoints.
pub(super) fn on_off_endpoints(definition: &Definition) -> Option<Vec<EndpointId>> {
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
pub(super) fn has_brightness(definition: &Definition) -> bool {
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
pub(super) fn endpoint_for(
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

/// One capability value as the command that writes it.
///
/// The table that makes `Set` and the ergonomic constructors the same thing.
/// Values are accepted in the forms that actually arrive: `"ON"`, `true` and
/// `1` all mean on, because an MQTT client sends whichever its author reached
/// for and `Zigbee2MQTT` accepts all of them.
fn lower(capability: &CapabilityId, value: &StateValue) -> Result<DeviceCommand, CommandError> {
    /// Anything that plausibly means "on".
    fn truthy(value: &StateValue) -> Option<bool> {
        match value {
            StateValue::Bool(b) => Some(*b),
            StateValue::Int(i) => Some(*i != 0),
            StateValue::Str(s) | StateValue::Enum(s) => match s.to_ascii_uppercase().as_str() {
                "ON" | "TRUE" | "1" | "OPEN" | "LOCK" => Some(true),
                "OFF" | "FALSE" | "0" | "CLOSE" | "UNLOCK" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// A numeric value as an integer, when it is exactly one.
    ///
    /// A float is rounded and range-checked *before* the conversion, which is
    /// what makes it exact rather than truncating; an out-of-range or
    /// non-finite value is refused rather than clamped, because clamping would
    /// silently command a level nobody asked for.
    fn integer(value: &StateValue) -> Option<i64> {
        match value {
            StateValue::Int(i) => Some(*i),
            StateValue::Float(f) => {
                let rounded = f.round();
                #[allow(clippy::cast_possible_truncation)]
                (rounded.is_finite() && rounded.abs() <= 9_007_199_254_740_992.0)
                    .then_some(rounded as i64)
            }
            _ => None,
        }
    }

    let invalid = || CommandError::InvalidValue {
        capability: capability.clone(),
        value: format!("{value:?}"),
    };

    match capability.as_str() {
        "state" => match value {
            StateValue::Str(s) | StateValue::Enum(s) if s.eq_ignore_ascii_case("toggle") => {
                Ok(DeviceCommand::Toggle)
            }
            _ => truthy(value).map(DeviceCommand::SetOn).ok_or_else(invalid),
        },
        "brightness" => integer(value)
            .and_then(|v| u8::try_from(v).ok())
            .map(|raw| DeviceCommand::SetBrightness(Brightness::new(raw)))
            .ok_or_else(invalid),
        "color_temp" => integer(value)
            .and_then(|v| u16::try_from(v).ok())
            .map(|mireds| DeviceCommand::SetColorTemp(Mireds(mireds)))
            .ok_or_else(invalid),
        "position" => integer(value)
            .and_then(|v| u8::try_from(v).ok())
            .map(|p| DeviceCommand::SetPosition(Percent::new(p)))
            .ok_or_else(invalid),
        // Not a guess: an unknown capability name is refused by name, so the
        // caller learns which one rather than that "something" failed.
        _ => Err(CommandError::UnsupportedCapability(capability.clone())),
    }
}

/// Plans the window-covering commands.
///
/// Grouped because they share the same capability lookup and the same cluster,
/// and because `plan_command` was over the line-count limit -- which is the
/// limit doing its job: the covering cases are a coherent unit and read better
/// on their own.
///
/// # Errors
///
/// [`CommandError::UnsupportedCapability`] when the definition does not give
/// the device lift or tilt, as the command requires.
fn plan_cover(
    definition: &Definition,
    info: &DeviceInfo,
    command: &DeviceCommand,
) -> Result<PlannedZcl, CommandError> {
    match command {
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
        other => Err(unimplemented_command(other)),
    }
}

/// The error for a command this build does not implement.
///
/// Named explicitly rather than approximated with a related command -- and
/// *not* reported as a missing definition, which is what it used to say. That
/// blamed definition resolution for an unimplemented command and sent a real
/// investigation to the wrong place while the definition was perfectly fine.
fn unimplemented_command(command: &DeviceCommand) -> CommandError {
    CommandError::InvalidValue {
        capability: CapabilityId::from("command"),
        value: format!("{command:?} is not implemented by this build"),
    }
}

/// Plans the general `Set` form by lowering it to a specific command.
///
/// The comment on [`DeviceCommand`] says each ergonomic constructor lowers to
/// `Set` so that there is exactly one execution path. The implementation had it
/// the other way round and `Set` was not handled at all, so the general form --
/// the one an MQTT `/set` naturally produces -- failed with an error blaming a
/// missing definition. Found by running the gateway against a broker.
///
/// # Errors
///
/// Refuses a `Set` naming anything other than exactly one capability, and one
/// whose value does not mean anything for that capability.
fn plan_set(
    definition: &Definition,
    info: &DeviceInfo,
    changes: &StateChanges,
) -> Result<PlannedZcl, CommandError> {
    let mut entries = changes.iter();
    let (Some((capability, value)), None) = (entries.next(), entries.next()) else {
        // One frame per plan, so a multi-capability write needs more than this
        // can return. Refused explicitly rather than applying the first entry
        // and dropping the rest, which would leave the device in a state nobody
        // asked for.
        return Err(CommandError::InvalidValue {
            capability: CapabilityId::from("set"),
            value: "a Set must name exactly one capability; send them separately".into(),
        });
    };
    let lowered = lower(capability, value)?;
    plan_command(definition, info, &lowered)
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
    // The general form is lowered to a specific one first, so both share this
    // planner rather than diverging.
    if let DeviceCommand::Set(changes) = command {
        return plan_set(definition, info, changes);
    }

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
        DeviceCommand::Open
        | DeviceCommand::Close
        | DeviceCommand::Stop
        | DeviceCommand::SetPosition(_)
        | DeviceCommand::SetTilt(_) => plan_cover(definition, info, command),
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
        other => Err(unimplemented_command(other)),
    }
}
