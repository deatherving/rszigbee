//! Decoding an incoming ZCL frame into something a caller can act on.
//!
//! [`Event::ZclMessage`] carries decoded content — attributes with typed
//! values, a named command with its parameters — rather than bytes, because
//! bytes are not what an application wants and decoding them in every consumer
//! is how two consumers come to disagree about what a frame meant.
//!
//! # Failure is a first-class outcome
//!
//! A frame that cannot be decoded produces [`Event::UnparsedFrame`] carrying
//! the original bytes and the reason. That is deliberately an event rather than
//! a log line: it is the answer to "why is my device not working", and it needs
//! to be countable and visible to whoever is trying to add support for the
//! device. Dropping such a frame silently is what makes an unsupported device
//! look like a broken one.

use rszigbee_spec::codec::Reader;
use rszigbee_spec::ids::Ieee;
use rszigbee_spec::zcl::frame::{Direction, FrameType, ZclFrame};
use rszigbee_spec::zcl::registry::ClusterRegistry;
use rszigbee_spec::zcl::types::{ZclType, ZclValue, decode_value};

use crate::adapter::ZclRx;
use crate::event::{ParseFailure, ZclMessage, ZclMessageKind};

/// Foundation commands this decoder understands.
const READ_ATTRIBUTES_RESPONSE: u8 = 0x01;
const REPORT_ATTRIBUTES: u8 = 0x0a;
const DEFAULT_RESPONSE: u8 = 0x0b;

/// Decodes one received frame.
///
/// # Errors
///
/// Returns the reason the frame could not be decoded, for the caller to attach
/// to [`Event::UnparsedFrame`] along with the original bytes.
pub fn zcl_message(
    registry: &ClusterRegistry,
    ieee: Ieee,
    rx: &ZclRx,
) -> Result<ZclMessage, ParseFailure> {
    let frame = ZclFrame::decode(&rx.frame).map_err(ParseFailure::Codec)?;
    let mut reader = Reader::new(frame.payload);

    let kind = match frame.header.frame_type {
        FrameType::Global => match frame.header.command.0 {
            REPORT_ATTRIBUTES | READ_ATTRIBUTES_RESPONSE => {
                let with_status = frame.header.command.0 == READ_ATTRIBUTES_RESPONSE;
                ZclMessageKind::Attributes(attributes(
                    registry,
                    ieee,
                    rx,
                    &mut reader,
                    with_status,
                )?)
            }
            DEFAULT_RESPONSE => {
                let command = reader.u8().map_err(ParseFailure::Codec)?;
                let status = reader.u8().map_err(ParseFailure::Codec)?;
                ZclMessageKind::DefaultResponse { command, status }
            }
            other => return Err(ParseFailure::UnknownCommand(other)),
        },
        FrameType::Specific => {
            let cluster = registry
                .get(Some(ieee), rx.cluster)
                .ok_or(ParseFailure::UnknownCluster(rx.cluster))?;
            // A server-to-client frame is a response, so it is looked up in the
            // response table. Using the command table for both is how a
            // response id gets reported as an unrelated command's name.
            let table = if frame.header.direction == Direction::ServerToClient {
                &cluster.responses
            } else {
                &cluster.commands
            };
            let definition = table.get(&frame.header.command.0);

            let mut params = Vec::new();
            let mut name = None;
            if let Some(definition) = definition {
                name = Some(definition.name.clone());
                for parameter in &definition.params {
                    // A truncated payload stops the walk rather than failing
                    // the frame: what was decoded before the end is still true,
                    // and some devices genuinely send short payloads.
                    match decode_value(parameter.ty, &mut reader) {
                        Ok(value) => params.push((parameter.name.clone(), value)),
                        Err(_) => break,
                    }
                }
            }

            ZclMessageKind::Command {
                id: frame.header.command.0,
                name,
                params,
            }
        }
        // A reserved frame type is not something to guess at.
        FrameType::Reserved(_) => {
            return Err(ParseFailure::UnknownCommand(frame.header.command.0));
        }
    };

    Ok(ZclMessage {
        ieee,
        endpoint: rx.endpoint,
        cluster: rx.cluster,
        kind,
        link_quality: rx.link_quality,
    })
}

/// Decodes a run of attribute records.
///
/// `with_status` distinguishes a read response, whose records carry a status
/// byte and omit the value when it is non-zero, from a report, whose records
/// never do. Conflating them shifts every field by one byte.
fn attributes(
    registry: &ClusterRegistry,
    ieee: Ieee,
    rx: &ZclRx,
    reader: &mut Reader<'_>,
    with_status: bool,
) -> Result<Vec<(u16, ZclValue)>, ParseFailure> {
    let mut out = Vec::new();
    while !reader.is_empty() {
        let id = reader.u16_le().map_err(ParseFailure::Codec)?;
        if with_status {
            let status = reader.u8().map_err(ParseFailure::Codec)?;
            if status != 0 {
                // An unreadable attribute is reported by status with no value
                // following. Trying to read one consumes the next record.
                continue;
            }
        }
        let tag = reader.u8().map_err(ParseFailure::Codec)?;
        let ty = ZclType::from_u8(tag);

        // The wire type is authoritative, not the registry's. A device that
        // reports a different type than the specification says is a device that
        // exists, and decoding by the registry's type would misread its frame.
        // The registry is consulted only to notice the disagreement.
        if let Some(known) = registry.attr(Some(ieee), rx.cluster, rszigbee_spec::ids::AttrId(id))
            && known.ty != ty
        {
            {
                tracing::debug!(
                    %ieee,
                    cluster = rx.cluster.0,
                    attribute = id,
                    expected = ?known.ty,
                    found = ?ty,
                    "attribute reported with a different type than the registry declares"
                );
            }
        }

        let value = decode_value(ty, reader).map_err(ParseFailure::Codec)?;
        out.push((id, value));
    }
    Ok(out)
}
