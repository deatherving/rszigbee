//! ZDO request encoding and response decoding.
//!
//! Only the descriptors the interview needs. The full 116-cluster table is
//! generated from upstream data in a later phase (see the README, "What is
//! transcoded, not invented").
//!
//! # Sequence numbers
//!
//! Every ZDO request carries a transaction sequence number as its **first
//! payload byte**, and the matching response echoes it. Some adapters prepend
//! it themselves and some expect the caller to — that is what
//! `AdapterCapabilities::zdo_sequence_in_payload` distinguishes. These encoders
//! take the sequence explicitly so neither side has to guess.

use alloc::vec::Vec;

use crate::codec::{CodecError, Reader, Writer};
use crate::ids::{ClusterId, EndpointId, Ieee, Nwk, ProfileId};
use crate::zdo::{ZdoClusterId, ZdoStatus};

/// Encodes `Node_Desc_req`: sequence, then the target's short address.
#[must_use]
pub fn encode_node_desc_req(seq: u8, target: Nwk) -> Vec<u8> {
    let mut w = Writer::with_capacity(3);
    w.u8(seq).u16_le(target.raw());
    w.into_vec()
}

/// Encodes `Active_EP_req`.
#[must_use]
pub fn encode_active_ep_req(seq: u8, target: Nwk) -> Vec<u8> {
    let mut w = Writer::with_capacity(3);
    w.u8(seq).u16_le(target.raw());
    w.into_vec()
}

/// Encodes `Simple_Desc_req`: sequence, target, then the endpoint to describe.
#[must_use]
pub fn encode_simple_desc_req(seq: u8, target: Nwk, endpoint: EndpointId) -> Vec<u8> {
    let mut w = Writer::with_capacity(4);
    w.u8(seq).u16_le(target.raw()).u8(endpoint.0);
    w.into_vec()
}

/// Encodes `Mgmt_Permit_Joining_req`.
///
/// `tc_significance` is `true` in every practical case; the spec allows `false`
/// but no stack relies on it.
#[must_use]
pub fn encode_permit_joining_req(seq: u8, seconds: u8, tc_significance: bool) -> Vec<u8> {
    let mut w = Writer::with_capacity(4);
    w.u8(seq).u8(seconds).u8(u8::from(tc_significance));
    w.into_vec()
}

/// Encodes `Mgmt_Leave_req`.
#[must_use]
pub fn encode_leave_req(seq: u8, target: Ieee, remove_children: bool, rejoin: bool) -> Vec<u8> {
    let mut w = Writer::with_capacity(10);
    // Bit 6 is "remove children", bit 7 is "rejoin".
    let flags = (u8::from(remove_children) << 6) | (u8::from(rejoin) << 7);
    w.u8(seq).ieee_le(target).u8(flags);
    w.into_vec()
}

/// What kind of node this is, from the node descriptor's logical type field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalType {
    /// Coordinator.
    Coordinator,
    /// Router.
    Router,
    /// End device.
    EndDevice,
    /// A reserved value, preserved rather than coerced.
    Reserved(u8),
}

impl LogicalType {
    const fn from_bits(b: u8) -> Self {
        match b & 0b111 {
            0 => Self::Coordinator,
            1 => Self::Router,
            2 => Self::EndDevice,
            other => Self::Reserved(other),
        }
    }
}

/// A decoded node descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeDescriptor {
    /// The sequence number echoed from the request.
    pub sequence: u8,
    /// The address the descriptor describes.
    pub of: Nwk,
    /// Node type.
    pub logical_type: LogicalType,
    /// The node's manufacturer code. This is the field the Tuya and Control4
    /// interview quirks key on, so it is load-bearing rather than informational.
    pub manufacturer_code: u16,
    /// Maximum APS payload the node accepts.
    pub max_incoming_transfer_size: u16,
    /// Raw MAC capability flags.
    pub mac_capability_flags: u8,
    /// Raw server mask.
    pub server_mask: u16,
}

impl NodeDescriptor {
    /// True when the node keeps its receiver on while idle, i.e. it is not a
    /// sleepy device. MAC capability bit 3.
    #[must_use]
    pub const fn rx_on_when_idle(&self) -> bool {
        self.mac_capability_flags & 0b0000_1000 != 0
    }

    /// True when the node is mains powered. MAC capability bit 2.
    #[must_use]
    pub const fn mains_powered(&self) -> bool {
        self.mac_capability_flags & 0b0000_0100 != 0
    }
}

/// Encodes a `Bind_req`, binding a device's cluster to a destination.
///
/// A binding is what tells a device where to send its reports. Without one,
/// configuring reporting succeeds on many devices and then nothing arrives:
/// the device dutifully generates reports and has nowhere to send them. That
/// failure is indistinguishable from a broken sensor, which is why binding is
/// not optional.
///
/// `source` names the device's own cluster instance; `destination` is where
/// reports should go, which on a coordinator-managed network is the
/// coordinator.
#[must_use]
pub fn encode_bind_req(
    sequence: u8,
    source: Ieee,
    source_endpoint: EndpointId,
    cluster: ClusterId,
    destination: Ieee,
    destination_endpoint: EndpointId,
) -> Vec<u8> {
    /// Address mode 3: a 64-bit address plus an endpoint. Mode 1 is a group
    /// binding, which carries no endpoint and is not what this encodes.
    const UNICAST_WITH_ENDPOINT: u8 = 0x03;

    let mut w = Writer::with_capacity(22);
    w.u8(sequence);
    w.u64_le(source.raw());
    w.u8(source_endpoint.0);
    w.u16_le(cluster.0);
    w.u8(UNICAST_WITH_ENDPOINT);
    w.u64_le(destination.raw());
    w.u8(destination_endpoint.0);
    w.into_vec()
}

/// Decodes a `Bind_rsp`, which carries only a sequence and a status.
///
/// # Errors
///
/// [`ZdoError::Truncated`] if the frame is shorter than that, and
/// [`ZdoError::Status`] when the device refused the binding. The two are
/// distinct because a refusal is the device answering, and a device that
/// refuses a binding often still works for everything else.
pub fn decode_bind_rsp(payload: &[u8]) -> Result<u8, ZdoError> {
    let mut r = Reader::new(payload);
    let sequence = r.u8()?;
    let status = ZdoStatus::from_u8(r.u8()?);
    if status.is_success() {
        Ok(sequence)
    } else {
        Err(ZdoError::Status { sequence, status })
    }
}

/// Decodes a `Node_Desc_rsp` payload.
///
/// Layout: sequence, status, address, then the 13-octet descriptor.
pub fn decode_node_desc_rsp(payload: &[u8]) -> Result<NodeDescriptor, ZdoError> {
    let mut r = Reader::new(payload);
    let sequence = r.u8()?;
    let status = ZdoStatus::from_u8(r.u8()?);
    if !status.is_success() {
        return Err(ZdoError::Status { sequence, status });
    }
    let of = Nwk::new(r.u16_le()?);

    let byte0 = r.u8()?;
    let _byte1 = r.u8()?; // aps flags and frequency band; nothing reads them yet
    let mac_capability_flags = r.u8()?;
    let manufacturer_code = r.u16_le()?;
    let _max_buffer_size = r.u8()?;
    let max_incoming_transfer_size = r.u16_le()?;
    let server_mask = r.u16_le()?;

    Ok(NodeDescriptor {
        sequence,
        of,
        logical_type: LogicalType::from_bits(byte0),
        manufacturer_code,
        max_incoming_transfer_size,
        mac_capability_flags,
        server_mask,
    })
}

/// A decoded `Active_EP_rsp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveEndpoints {
    /// Echoed sequence.
    pub sequence: u8,
    /// The address described.
    pub of: Nwk,
    /// The node's active endpoints.
    pub endpoints: Vec<EndpointId>,
}

/// Decodes an `Active_EP_rsp` payload.
pub fn decode_active_ep_rsp(payload: &[u8]) -> Result<ActiveEndpoints, ZdoError> {
    let mut r = Reader::new(payload);
    let sequence = r.u8()?;
    let status = ZdoStatus::from_u8(r.u8()?);
    if !status.is_success() {
        return Err(ZdoError::Status { sequence, status });
    }
    let of = Nwk::new(r.u16_le()?);
    let count = usize::from(r.u8()?);

    // The count is device-supplied and therefore untrusted: read exactly what
    // is present and report a short list rather than trusting the claim.
    let bytes = r.bytes(count).map_err(|_| ZdoError::Truncated {
        what: "active endpoint list",
        claimed: count,
        available: r.remaining(),
    })?;

    Ok(ActiveEndpoints {
        sequence,
        of,
        endpoints: bytes.iter().copied().map(EndpointId).collect(),
    })
}

/// A decoded `Simple_Desc_rsp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleDescriptor {
    /// Echoed sequence.
    pub sequence: u8,
    /// The address described.
    pub of: Nwk,
    /// The endpoint described.
    pub endpoint: EndpointId,
    /// Application profile.
    pub profile: ProfileId,
    /// Device id within the profile.
    pub device_id: u16,
    /// Device version, low nibble of the version octet.
    pub device_version: u8,
    /// Server-side clusters.
    pub input_clusters: Vec<ClusterId>,
    /// Client-side clusters.
    pub output_clusters: Vec<ClusterId>,
}

/// Decodes a `Simple_Desc_rsp` payload.
pub fn decode_simple_desc_rsp(payload: &[u8]) -> Result<SimpleDescriptor, ZdoError> {
    let mut r = Reader::new(payload);
    let sequence = r.u8()?;
    let status = ZdoStatus::from_u8(r.u8()?);
    if !status.is_success() {
        return Err(ZdoError::Status { sequence, status });
    }
    let of = Nwk::new(r.u16_le()?);
    let _length = r.u8()?; // descriptor length; the fields below are self-describing
    let endpoint = EndpointId(r.u8()?);
    let profile = ProfileId(r.u16_le()?);
    let device_id = r.u16_le()?;
    let device_version = r.u8()? & 0x0f;

    let input_clusters = read_cluster_list(&mut r, "input cluster list")?;
    let output_clusters = read_cluster_list(&mut r, "output cluster list")?;

    Ok(SimpleDescriptor {
        sequence,
        of,
        endpoint,
        profile,
        device_id,
        device_version,
        input_clusters,
        output_clusters,
    })
}

fn read_cluster_list(r: &mut Reader<'_>, what: &'static str) -> Result<Vec<ClusterId>, ZdoError> {
    let count = usize::from(r.u8()?);
    // Each cluster is two octets. A device claiming more than it sent is the
    // shape of frame that turns an indexing implementation into a crash.
    let needed = count.saturating_mul(2);
    if needed > r.remaining() {
        return Err(ZdoError::Truncated {
            what,
            claimed: needed,
            available: r.remaining(),
        });
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(ClusterId(r.u16_le()?));
    }
    Ok(out)
}

/// Why a ZDO response could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ZdoError {
    /// The response decoded but reported a non-success status.
    ///
    /// A distinct variant because it is not a protocol failure: a device
    /// answering `NotSupported` has answered, and the interview must treat that
    /// differently from silence.
    #[error("ZDO request {sequence} was answered with {status:?}")]
    Status {
        /// Echoed sequence.
        sequence: u8,
        /// The reported status.
        status: ZdoStatus,
    },
    /// A length field claimed more than the frame contained.
    #[error("{what} claimed {claimed} bytes but only {available} remain")]
    Truncated {
        /// Which field.
        what: &'static str,
        /// The claimed length.
        claimed: usize,
        /// What was actually left.
        available: usize,
    },
    /// The frame ended early.
    #[error("malformed ZDO response: {0}")]
    Codec(#[from] CodecError),
}

/// Which response cluster a request expects.
#[must_use]
pub const fn response_for(request: ZdoClusterId) -> ZdoClusterId {
    request.response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_desc_req_is_sequence_then_target_little_endian() {
        assert_eq!(
            encode_node_desc_req(0x2a, Nwk::new(0x1234)),
            [0x2a, 0x34, 0x12]
        );
        // The coordinator is a legitimate target; that is how a stack builds
        // its own device record without any other node on the network.
        assert_eq!(
            encode_node_desc_req(1, Nwk::COORDINATOR),
            [0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn simple_desc_req_carries_the_endpoint() {
        assert_eq!(
            encode_simple_desc_req(7, Nwk::new(0xabcd), EndpointId(1)),
            [0x07, 0xcd, 0xab, 0x01]
        );
    }

    #[test]
    fn leave_req_flag_bits_are_where_the_spec_puts_them() {
        let p = encode_leave_req(3, Ieee::new(0x0017_8801_00dc_4d3f), true, false);
        assert_eq!(p.first(), Some(&3));
        // IEEE goes out little-endian.
        assert_eq!(
            p.get(1..9),
            Some(&[0x3f, 0x4d, 0xdc, 0x00, 0x01, 0x88, 0x17, 0x00][..])
        );
        assert_eq!(p.get(9), Some(&0b0100_0000), "remove-children is bit 6");

        let p = encode_leave_req(3, Ieee::ZERO, false, true);
        assert_eq!(p.get(9), Some(&0b1000_0000), "rejoin is bit 7");
    }

    #[test]
    fn a_coordinator_node_descriptor_decodes() {
        // sequence, status ok, address 0x0000, then the descriptor: logical
        // type 0 (coordinator), rx-on-when-idle and mains-powered set.
        let payload = [
            0x01, 0x00, 0x00, 0x00, // seq, status, addr
            0x00, 0x40, 0x8e, // byte0 (coordinator), byte1, mac caps
            0x49, 0x10, // manufacturer 0x1049 (Silicon Labs)
            0x52, // max buffer
            0x52, 0x00, // max incoming transfer
            0x2c, 0x00, // server mask
        ];
        let d = decode_node_desc_rsp(&payload).expect("decodes");
        assert_eq!(d.sequence, 1);
        assert_eq!(d.of, Nwk::COORDINATOR);
        assert_eq!(d.logical_type, LogicalType::Coordinator);
        assert_eq!(d.manufacturer_code, 0x1049);
        assert!(d.rx_on_when_idle(), "a coordinator is never sleepy");
        assert!(d.mains_powered());
    }

    #[test]
    fn a_sleepy_end_device_descriptor_reports_itself_as_such() {
        // Logical type 2, MAC capabilities with rx-on-when-idle clear: this is
        // the distinction the reachability policy depends on.
        let payload = [
            0x02, 0x00, 0x34, 0x12, 0x02, 0x40, 0x80, 0x00, 0x00, 0x52, 0x52, 0x00, 0x00, 0x00,
        ];
        let d = decode_node_desc_rsp(&payload).expect("decodes");
        assert_eq!(d.logical_type, LogicalType::EndDevice);
        assert!(!d.rx_on_when_idle());
        assert!(!d.mains_powered());
    }

    #[test]
    fn a_non_success_status_is_a_distinct_error_from_a_malformed_frame() {
        // A device answering NotSupported has answered. The interview must be
        // able to tell that apart from silence or corruption.
        let payload = [0x05, 0x84, 0x00, 0x00];
        match decode_node_desc_rsp(&payload) {
            Err(ZdoError::Status {
                sequence: 5,
                status,
            }) => {
                assert_eq!(status, ZdoStatus::NotSupported);
            }
            other => panic!("expected a Status error, got {other:?}"),
        }
    }

    #[test]
    fn active_endpoints_decode() {
        let payload = [0x03, 0x00, 0x00, 0x00, 0x02, 0x01, 0xf2];
        let a = decode_active_ep_rsp(&payload).expect("decodes");
        assert_eq!(a.sequence, 3);
        assert_eq!(a.endpoints, [EndpointId(1), EndpointId(0xf2)]);
    }

    #[test]
    fn an_endpoint_count_larger_than_the_frame_is_refused() {
        // Device-supplied counts are untrusted. Claiming 200 endpoints while
        // sending two must be an error, not a read past the end.
        let payload = [0x03, 0x00, 0x00, 0x00, 200, 0x01, 0x02];
        match decode_active_ep_rsp(&payload) {
            Err(ZdoError::Truncated {
                claimed: 200,
                available: 2,
                ..
            }) => {}
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn a_simple_descriptor_decodes_both_cluster_lists() {
        let payload = [
            0x04, 0x00, 0x00, 0x00, // seq, status, addr
            0x14, // descriptor length
            0x01, // endpoint 1
            0x04, 0x01, // profile 0x0104 (Home Automation)
            0x00, 0x01, // device id 0x0100
            0x01, // version
            0x03, 0x00, 0x00, 0x03, 0x00, 0x06, 0x00, // 3 input clusters
            0x01, 0x19, 0x00, // 1 output cluster (genOta)
        ];
        let d = decode_simple_desc_rsp(&payload).expect("decodes");
        assert_eq!(d.endpoint, EndpointId(1));
        assert_eq!(d.profile, ProfileId::HA);
        assert_eq!(d.device_id, 0x0100);
        assert_eq!(
            d.input_clusters,
            [ClusterId(0x0000), ClusterId(0x0003), ClusterId(0x0006)]
        );
        assert_eq!(d.output_clusters, [ClusterId(0x0019)]);
    }

    #[test]
    fn a_cluster_count_larger_than_the_frame_is_refused() {
        let payload = [
            0x04, 0x00, 0x00, 0x00, 0x14, 0x01, 0x04, 0x01, 0x00, 0x01, 0x01,
            50, // claims 50 input clusters
            0x00, 0x00, // sends one
        ];
        match decode_simple_desc_rsp(&payload) {
            Err(ZdoError::Truncated {
                what: "input cluster list",
                claimed: 100,
                ..
            }) => {}
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frames_error_rather_than_panic() {
        // Every prefix of a valid frame must be an error, never a panic. This
        // is the parse-path invariant, checked exhaustively.
        let valid = [
            0x04, 0x00, 0x00, 0x00, 0x14, 0x01, 0x04, 0x01, 0x00, 0x01, 0x01, 0x01, 0x00, 0x00,
            0x00,
        ];
        for n in 0..valid.len() {
            let prefix = valid.get(..n).expect("in range");
            let _ = decode_simple_desc_rsp(prefix);
            let _ = decode_node_desc_rsp(prefix);
            let _ = decode_active_ep_rsp(prefix);
        }
        // And arbitrary noise.
        for seed in 0u8..=255 {
            let noise = [seed; 9];
            let _ = decode_node_desc_rsp(&noise);
            let _ = decode_active_ep_rsp(&noise);
            let _ = decode_simple_desc_rsp(&noise);
        }
    }

    #[test]
    fn empty_input_is_an_error_on_every_decoder() {
        assert!(decode_node_desc_rsp(&[]).is_err());
        assert!(decode_active_ep_rsp(&[]).is_err());
        assert!(decode_simple_desc_rsp(&[]).is_err());
    }

    #[test]
    fn requests_map_to_their_response_clusters() {
        assert_eq!(
            response_for(ZdoClusterId::NODE_DESC_REQ),
            ZdoClusterId(0x8002)
        );
        assert_eq!(
            response_for(ZdoClusterId::SIMPLE_DESC_REQ),
            ZdoClusterId(0x8004)
        );
    }
}
