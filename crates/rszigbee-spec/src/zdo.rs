//! Zigbee Device Objects: cluster identifiers, request encoders and response
//! decoders for network management.
//!
//! Only the requests the interview and the vertical slice need are enumerated.
//! The full 116-cluster table plus response codecs is generated from upstream
//! data in a later phase (README, "What is transcoded, not invented").

pub mod codec;

pub use codec::{
    ActiveEndpoints, LogicalType, NodeDescriptor, SimpleDescriptor, ZdoError, decode_active_ep_rsp,
    decode_node_desc_rsp, decode_simple_desc_rsp, encode_active_ep_req, encode_leave_req,
    encode_node_desc_req, encode_permit_joining_req, encode_simple_desc_req,
};

/// A ZDO cluster identifier. Responses are the request id with bit 15 set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZdoClusterId(pub u16);

impl ZdoClusterId {
    /// `NWK_addr_req` — find a short address from an IEEE address.
    pub const NWK_ADDR_REQ: Self = Self(0x0000);
    /// `IEEE_addr_req` — find an IEEE address from a short address.
    pub const IEEE_ADDR_REQ: Self = Self(0x0001);
    /// `Node_Desc_req` — the first step of every interview.
    pub const NODE_DESC_REQ: Self = Self(0x0002);
    /// `Simple_Desc_req` — one endpoint's profile, device id and clusters.
    pub const SIMPLE_DESC_REQ: Self = Self(0x0004);
    /// `Active_EP_req` — which endpoints a device has.
    pub const ACTIVE_EP_REQ: Self = Self(0x0005);
    /// `Match_Desc_req`.
    pub const MATCH_DESC_REQ: Self = Self(0x0006);
    /// `Device_annce` — sent by a device on join or rejoin.
    pub const DEVICE_ANNCE: Self = Self(0x0013);
    /// `Bind_req`.
    pub const BIND_REQ: Self = Self(0x0021);
    /// `Unbind_req`.
    pub const UNBIND_REQ: Self = Self(0x0022);
    /// `Mgmt_Lqi_req` — neighbour table, used for the network map.
    pub const MGMT_LQI_REQ: Self = Self(0x0031);
    /// `Mgmt_Rtg_req` — routing table.
    pub const MGMT_RTG_REQ: Self = Self(0x0032);
    /// `Mgmt_Bind_req` — binding table.
    pub const MGMT_BIND_REQ: Self = Self(0x0033);
    /// `Mgmt_Leave_req` — ask a device to leave the network.
    pub const MGMT_LEAVE_REQ: Self = Self(0x0034);
    /// `Mgmt_Permit_Joining_req`.
    pub const MGMT_PERMIT_JOINING_REQ: Self = Self(0x0036);
    /// `Mgmt_NWK_Update_req` — channel change and energy scan.
    pub const MGMT_NWK_UPDATE_REQ: Self = Self(0x0038);

    /// The response cluster corresponding to this request.
    #[must_use]
    pub const fn response(self) -> Self {
        Self(self.0 | 0x8000)
    }

    /// True when this identifier is a response.
    #[must_use]
    pub const fn is_response(self) -> bool {
        self.0 & 0x8000 != 0
    }

    /// The request corresponding to this response.
    #[must_use]
    pub const fn request(self) -> Self {
        Self(self.0 & 0x7fff)
    }
}

/// ZDO status codes. Only the ones acted upon are named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ZdoStatus {
    /// The request succeeded.
    Success,
    /// The device does not implement the request.
    NotSupported,
    /// The requested device or endpoint is not known.
    DeviceNotFound,
    /// The endpoint is invalid.
    InvalidEndpoint,
    /// No matching binding entry.
    NoEntry,
    /// Something else; the raw code is preserved.
    Other(u8),
}

impl ZdoStatus {
    /// Maps a raw status byte.
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0x00 => Self::Success,
            0x84 => Self::NotSupported,
            0x81 => Self::DeviceNotFound,
            0x82 => Self::InvalidEndpoint,
            0x88 => Self::NoEntry,
            other => Self::Other(other),
        }
    }

    /// True only for `Success`.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_ids_are_the_request_with_the_top_bit_set() {
        assert_eq!(ZdoClusterId::NODE_DESC_REQ.response(), ZdoClusterId(0x8002));
        assert_eq!(ZdoClusterId::ACTIVE_EP_REQ.response(), ZdoClusterId(0x8005));
        assert!(ZdoClusterId(0x8005).is_response());
        assert!(!ZdoClusterId(0x0005).is_response());
        assert_eq!(ZdoClusterId(0x8005).request(), ZdoClusterId::ACTIVE_EP_REQ);
    }

    #[test]
    fn status_mapping_distinguishes_success_from_everything_else() {
        assert!(ZdoStatus::from_u8(0x00).is_success());
        assert_eq!(ZdoStatus::from_u8(0x84), ZdoStatus::NotSupported);
        assert_eq!(ZdoStatus::from_u8(0x81), ZdoStatus::DeviceNotFound);
        // An unrecognised code keeps its value rather than becoming "success".
        assert_eq!(ZdoStatus::from_u8(0x42), ZdoStatus::Other(0x42));
        assert!(!ZdoStatus::from_u8(0x42).is_success());
    }
}
