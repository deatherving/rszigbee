//! Turning an intent into a cluster and a command.
//!
//! Every mapping is derived from the definition, and there is no fallback. If
//! the definition does not say a device has on/off, `SetOn` is refused rather
//! than sent to `genOnOff` on the assumption that most things have it — the
//! devices where that assumption holds would work, and the rest would fail
//! silently.

use rszigbee_devices::{Definition, Extend};
use rszigbee_spec::ids::{ClusterId, CommandId, EndpointId};

use crate::command::{CommandError, DeviceCommand};
use crate::device::DeviceInfo;

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
