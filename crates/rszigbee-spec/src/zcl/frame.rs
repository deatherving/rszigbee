//! The ZCL frame header and frame envelope.
//!
//! Layout (ZCL 8, section 2.4.1):
//!
//! ```text
//! ┌───────────────┬──────────────────┬─────┬────────────┬──────────┐
//! │ frame control │ manufacturer code│ TSN │ command id │ payload  │
//! │    1 octet    │ 0 or 2 octets    │  1  │     1      │ variable │
//! └───────────────┴──────────────────┴─────┴────────────┴──────────┘
//! ```
//!
//! Frame control bits: `0..2` frame type, `2` manufacturer specific,
//! `3` direction, `4` disable default response, `5..8` reserved.

use alloc::vec::Vec;

use crate::codec::{CodecError, Reader, Writer};
use crate::ids::{CommandId, ManufacturerCode};

/// Whether a command belongs to the global (foundation) set or to a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Foundation commands: read, write, report, configure reporting, ...
    Global,
    /// Cluster-specific commands.
    Specific,
    /// Reserved frame types 2 and 3, kept representable so a malformed frame
    /// round-trips instead of being silently rewritten.
    Reserved(u8),
}

impl FrameType {
    const fn from_bits(b: u8) -> Self {
        match b & 0b11 {
            0 => Self::Global,
            1 => Self::Specific,
            other => Self::Reserved(other),
        }
    }

    const fn to_bits(self) -> u8 {
        match self {
            Self::Global => 0,
            Self::Specific => 1,
            Self::Reserved(o) => o & 0b11,
        }
    }
}

/// Which way a command is travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Client to server: the usual direction for a coordinator's commands.
    ClientToServer,
    /// Server to client: responses and attribute reports.
    ServerToClient,
}

/// A parsed ZCL frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZclHeader {
    /// Global or cluster-specific.
    pub frame_type: FrameType,
    /// Present when the frame is manufacturer specific.
    pub manufacturer: Option<ManufacturerCode>,
    /// Travel direction.
    pub direction: Direction,
    /// When set, the peer should not send a Default Response.
    pub disable_default_response: bool,
    /// Reserved frame-control bits, preserved verbatim.
    pub reserved: u8,
    /// Transaction sequence number, used to correlate responses and to
    /// de-duplicate retransmissions.
    pub tsn: u8,
    /// The command identifier.
    pub command: CommandId,
}

impl ZclHeader {
    /// A client-to-server cluster-specific command header.
    #[must_use]
    pub const fn command(tsn: u8, command: CommandId) -> Self {
        Self {
            frame_type: FrameType::Specific,
            manufacturer: None,
            direction: Direction::ClientToServer,
            disable_default_response: false,
            reserved: 0,
            tsn,
            command,
        }
    }

    /// A client-to-server foundation command header.
    #[must_use]
    pub const fn global(tsn: u8, command: CommandId) -> Self {
        Self {
            frame_type: FrameType::Global,
            manufacturer: None,
            direction: Direction::ClientToServer,
            disable_default_response: false,
            reserved: 0,
            tsn,
            command,
        }
    }

    /// Marks the frame manufacturer specific.
    #[must_use]
    pub const fn with_manufacturer(mut self, code: ManufacturerCode) -> Self {
        self.manufacturer = Some(code);
        self
    }

    /// Sets the disable-default-response bit, which many devices require
    /// (`meta.disableDefaultResponse` upstream).
    #[must_use]
    pub const fn with_disable_default_response(mut self, v: bool) -> Self {
        self.disable_default_response = v;
        self
    }

    /// Sets the direction.
    #[must_use]
    pub const fn with_direction(mut self, d: Direction) -> Self {
        self.direction = d;
        self
    }

    /// Decodes a header, leaving the reader positioned at the payload.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        let fc = r.u8()?;
        let frame_type = FrameType::from_bits(fc);
        let manufacturer_specific = fc & 0b0000_0100 != 0;
        let direction = if fc & 0b0000_1000 == 0 {
            Direction::ClientToServer
        } else {
            Direction::ServerToClient
        };
        let disable_default_response = fc & 0b0001_0000 != 0;
        let reserved = (fc >> 5) & 0b111;

        let manufacturer = if manufacturer_specific {
            Some(ManufacturerCode(r.u16_le()?))
        } else {
            None
        };

        Ok(Self {
            frame_type,
            manufacturer,
            direction,
            disable_default_response,
            reserved,
            tsn: r.u8()?,
            command: CommandId(r.u8()?),
        })
    }

    /// Encodes the header.
    pub fn encode(&self, w: &mut Writer) {
        let mut fc = self.frame_type.to_bits();
        if self.manufacturer.is_some() {
            fc |= 0b0000_0100;
        }
        if matches!(self.direction, Direction::ServerToClient) {
            fc |= 0b0000_1000;
        }
        if self.disable_default_response {
            fc |= 0b0001_0000;
        }
        fc |= (self.reserved & 0b111) << 5;

        w.u8(fc);
        if let Some(code) = self.manufacturer {
            w.u16_le(code.0);
        }
        w.u8(self.tsn);
        w.u8(self.command.0);
    }

    /// Wire size of this header in octets.
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        if self.manufacturer.is_some() { 5 } else { 3 }
    }
}

/// A ZCL frame: a header plus an undecoded payload.
///
/// The payload stays as bytes here on purpose. Decoding it needs the cluster
/// registry (to know an attribute's type) and, for manufacturer-specific
/// clusters, the specific device — neither of which belongs in a sans-IO frame
/// codec. The runtime decodes the payload once it has resolved the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZclFrame<'a> {
    /// The header.
    pub header: ZclHeader,
    /// Everything after the header.
    pub payload: &'a [u8],
}

impl<'a> ZclFrame<'a> {
    /// Decodes a frame from a complete ZCL APS payload.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(bytes);
        let header = ZclHeader::decode(&mut r)?;
        Ok(Self {
            header,
            payload: r.rest(),
        })
    }

    /// Encodes header and payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(self.header.encoded_len() + self.payload.len());
        self.header.encode(&mut w);
        w.bytes(self.payload);
        w.into_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_real_on_command() {
        // frame control 0x01 (cluster-specific, client to server), tsn 0x2a,
        // command 0x01 (on), no payload — the most common frame in Zigbee.
        let f = ZclFrame::decode(&[0x01, 0x2a, 0x01]).unwrap();
        assert_eq!(f.header.frame_type, FrameType::Specific);
        assert_eq!(f.header.direction, Direction::ClientToServer);
        assert_eq!(f.header.manufacturer, None);
        assert_eq!(f.header.tsn, 0x2a);
        assert_eq!(f.header.command, CommandId(0x01));
        assert!(f.payload.is_empty());
    }

    #[test]
    fn decodes_a_report_attributes_frame() {
        // 0x18 = global + server-to-client + disable default response,
        // tsn 0x07, command 0x0a (report attributes), then attr 0x0000 uint8 1.
        let f = ZclFrame::decode(&[0x18, 0x07, 0x0a, 0x00, 0x00, 0x20, 0x01]).unwrap();
        assert_eq!(f.header.frame_type, FrameType::Global);
        assert_eq!(f.header.direction, Direction::ServerToClient);
        assert!(f.header.disable_default_response);
        assert_eq!(f.header.command, CommandId(0x0a));
        assert_eq!(f.payload, &[0x00, 0x00, 0x20, 0x01]);
    }

    #[test]
    fn decodes_a_manufacturer_specific_frame() {
        // Philips 0x100b on cluster 0xfc03: frame control 0x05 sets the
        // manufacturer-specific bit, so the code occupies two extra octets.
        let f = ZclFrame::decode(&[0x05, 0x0b, 0x10, 0x11, 0x00, 0xaa]).unwrap();
        assert_eq!(f.header.manufacturer, Some(ManufacturerCode(0x100b)));
        assert_eq!(f.header.tsn, 0x11);
        assert_eq!(f.header.command, CommandId(0x00));
        assert_eq!(f.payload, &[0xaa]);
        assert_eq!(f.header.encoded_len(), 5);
    }

    #[test]
    fn headers_round_trip_including_reserved_bits() {
        // A frame with reserved bits set must survive unchanged: rewriting a
        // device's frame is how you create bugs nobody can reproduce.
        for fc in 0u8..=255 {
            let bytes = if fc & 0b100 == 0 {
                alloc::vec![fc, 0x42, 0x0b]
            } else {
                alloc::vec![fc, 0x34, 0x12, 0x42, 0x0b]
            };
            let h = ZclHeader::decode(&mut Reader::new(&bytes)).unwrap();
            let mut w = Writer::new();
            h.encode(&mut w);
            assert_eq!(w.as_slice(), bytes.as_slice(), "frame control 0x{fc:02x}");
        }
    }

    #[test]
    fn frames_round_trip() {
        let original: &[u8] = &[0x18, 0x07, 0x0a, 0x00, 0x00, 0x20, 0x01];
        assert_eq!(ZclFrame::decode(original).unwrap().encode(), original);
    }

    #[test]
    fn truncated_headers_error_rather_than_panic() {
        assert!(ZclFrame::decode(&[]).is_err());
        assert!(ZclFrame::decode(&[0x01]).is_err());
        assert!(ZclFrame::decode(&[0x01, 0x2a]).is_err());
        // Manufacturer-specific bit set but the code is cut short.
        assert!(ZclFrame::decode(&[0x05, 0x0b]).is_err());
        assert!(ZclFrame::decode(&[0x05, 0x0b, 0x10, 0x11]).is_err());
    }

    #[test]
    fn builders_produce_the_expected_bytes() {
        let mut w = Writer::new();
        ZclHeader::command(7, CommandId(0x01)).encode(&mut w);
        assert_eq!(w.as_slice(), &[0x01, 0x07, 0x01]);

        let mut w = Writer::new();
        ZclHeader::global(9, CommandId(0x00))
            .with_disable_default_response(true)
            .encode(&mut w);
        assert_eq!(w.as_slice(), &[0x10, 0x09, 0x00]);
    }
}
