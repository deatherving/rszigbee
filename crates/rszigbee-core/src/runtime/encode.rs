//! Turning the ZCL escape hatches into frames.
//!
//! [`DeviceCommand::Zcl`] and [`DeviceCommand::ZclAttributeWrite`] are named in
//! ZCL's own terms — a cluster, a command, parameters by name — not in bytes.
//! Encoding them is the runtime's job because the adapter takes a finished
//! frame, so exactly one implementation of the ZCL codec exists and no adapter
//! can reinterpret a frame on the way past.
//!
//! Parameter *types* come from the registry rather than from the caller. A
//! caller that had to supply the wire type could supply the wrong one, and a
//! `uint8` written as a `uint16` is a frame the device either rejects or
//! misreads — which presents as "this device ignores commands".

use rszigbee_spec::codec::Writer;
use rszigbee_spec::ids::{AttrId, ClusterId, CommandId, Ieee, ManufacturerCode};
use rszigbee_spec::zcl::frame::{ZclFrame, ZclHeader};
use rszigbee_spec::zcl::registry::ClusterRegistry;
use rszigbee_spec::zcl::types::encode_value;

use crate::command::{ZclAttributeWrite, ZclCommand};

/// The ZCL foundation `write attributes` command.
const WRITE_ATTRIBUTES: CommandId = CommandId(0x02);

/// Why a frame could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// The registry has no such cluster, so nothing can be typed.
    #[error(
        "cluster 0x{cluster:04x} is not in the registry, so parameter types are unknown. \
         Register it with `ClusterRegistry::insert_global` or \
         `insert_for_device` first."
    )]
    UnknownCluster {
        /// The cluster asked for.
        cluster: u16,
    },

    /// The cluster has no such command.
    #[error("cluster 0x{cluster:04x} has no command 0x{command:02x}")]
    UnknownCommand {
        /// The cluster.
        cluster: u16,
        /// The command.
        command: u8,
    },

    /// The cluster has no such attribute.
    #[error("cluster 0x{cluster:04x} has no attribute 0x{attribute:04x}")]
    UnknownAttribute {
        /// The cluster.
        cluster: u16,
        /// The attribute.
        attribute: u16,
    },

    /// A parameter the command declares was not supplied.
    ///
    /// Refused rather than defaulted: a missing parameter shortens the payload,
    /// and a device reading a short payload gets whatever follows it.
    #[error("command '{command}' requires parameter '{parameter}', which was not supplied")]
    MissingParameter {
        /// The command's name.
        command: String,
        /// The parameter's name.
        parameter: String,
    },

    /// A parameter was supplied that the command does not declare.
    ///
    /// Also refused: silently dropping it means a caller believing it sent
    /// something it did not.
    #[error("command '{command}' has no parameter '{parameter}'")]
    UnknownParameter {
        /// The command's name.
        command: String,
        /// The parameter's name.
        parameter: String,
    },

    /// A value did not fit its declared wire type.
    #[error("parameter '{parameter}' does not encode as the declared type: {detail}")]
    BadValue {
        /// Which parameter.
        parameter: String,
        /// What the codec said.
        detail: String,
    },

    /// Nothing to write.
    #[error("an attribute write with no attributes would be a no-op frame")]
    NoAttributes,
}

/// Encodes a cluster-specific command.
pub fn command(
    registry: &ClusterRegistry,
    device: Ieee,
    tsn: u8,
    request: &ZclCommand,
) -> Result<Vec<u8>, EncodeError> {
    let cluster =
        registry
            .get(Some(device), request.cluster)
            .ok_or(EncodeError::UnknownCluster {
                cluster: request.cluster.0,
            })?;
    let definition =
        cluster
            .commands
            .get(&request.command.0)
            .ok_or(EncodeError::UnknownCommand {
                cluster: request.cluster.0,
                command: request.command.0,
            })?;

    // Reject unknown names before encoding anything, so a typo is an error
    // rather than a silently short frame.
    for (name, _) in &request.params {
        if !definition.params.iter().any(|p| &p.name == name) {
            return Err(EncodeError::UnknownParameter {
                command: definition.name.clone(),
                parameter: name.clone(),
            });
        }
    }

    // Walked in the order the *definition* declares, not the order the caller
    // supplied. ZCL payloads are positional: the caller's ordering is not part
    // of the contract, and honouring it would make a correct-looking call
    // produce a wrong frame.
    let mut writer = Writer::new();
    for parameter in &definition.params {
        let value = request
            .params
            .iter()
            .find(|(name, _)| name == &parameter.name)
            .map(|(_, value)| value)
            .ok_or_else(|| EncodeError::MissingParameter {
                command: definition.name.clone(),
                parameter: parameter.name.clone(),
            })?;
        encode_value(value, parameter.ty, &mut writer).map_err(|e| EncodeError::BadValue {
            parameter: parameter.name.clone(),
            detail: e.to_string(),
        })?;
    }

    let mut header = ZclHeader::command(tsn, request.command)
        .with_disable_default_response(request.disable_default_response);
    if let Some(code) = request.manufacturer {
        header = header.with_manufacturer(ManufacturerCode(code));
    }

    Ok(ZclFrame {
        header,
        payload: &writer.into_vec(),
    }
    .encode())
}

/// Encodes a foundation `write attributes` command.
pub fn attribute_write(
    registry: &ClusterRegistry,
    device: Ieee,
    tsn: u8,
    request: &ZclAttributeWrite,
) -> Result<Vec<u8>, EncodeError> {
    if request.attributes.is_empty() {
        return Err(EncodeError::NoAttributes);
    }

    let mut writer = Writer::new();
    for (attribute, value) in &request.attributes {
        let definition = registry
            .attr(Some(device), request.cluster, *attribute)
            .ok_or(EncodeError::UnknownAttribute {
                cluster: request.cluster.0,
                attribute: attribute.0,
            })?;
        // Each record is id, then the type tag, then the value. The tag has to
        // match what the value actually encodes as, which is why the type comes
        // from the registry and not from the caller.
        writer.u16_le(attribute.0);
        writer.u8(definition.ty.to_u8());
        encode_value(value, definition.ty, &mut writer).map_err(|e| EncodeError::BadValue {
            parameter: definition.name.clone(),
            detail: e.to_string(),
        })?;
    }

    let mut header = ZclHeader::global(tsn, WRITE_ATTRIBUTES);
    if let Some(code) = request.manufacturer {
        header = header.with_manufacturer(ManufacturerCode(code));
    }

    Ok(ZclFrame {
        header,
        payload: &writer.into_vec(),
    }
    .encode())
}

/// A `read attributes` frame, used by the availability probe.
///
/// `genBasic.zclVersion` is mandatory on every Zigbee device, so this needs no
/// registry lookup and cannot be refused for being unsupported. What is being
/// tested is whether the device answers at all.
pub fn probe(tsn: u8) -> Vec<u8> {
    const READ_ATTRIBUTES: CommandId = CommandId(0x00);
    const ZCL_VERSION: AttrId = AttrId(0x0000);

    let mut writer = Writer::new();
    writer.u16_le(ZCL_VERSION.0);
    ZclFrame {
        header: ZclHeader::global(tsn, READ_ATTRIBUTES),
        payload: &writer.into_vec(),
    }
    .encode()
}

/// The cluster the probe reads.
pub const PROBE_CLUSTER: ClusterId = ClusterId(0x0000);
