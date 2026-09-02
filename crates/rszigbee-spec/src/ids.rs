//! Address and identifier newtypes shared by every layer.
//!
//! These are deliberately `Copy`, `Ord` and cheap: they end up as map keys, in
//! log fields and in hot lookup paths.

use core::fmt;

/// A 64-bit IEEE (EUI-64) address, the permanent identity of a Zigbee node.
///
/// `Display` renders the canonical Zigbee2MQTT-compatible form, lowercase hex
/// with an `0x` prefix and full 16 digits, because that form is load-bearing:
/// it is used as an MQTT topic component, an entity id and a database key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ieee(u64);

impl Ieee {
    /// The all-zero address. Not a valid node address; used as a sentinel by
    /// some adapters, so it is representable but never produced by parsing.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw `u64`.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw `u64`.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Big-endian bytes, the order used in ZDO descriptors and text form.
    #[must_use]
    pub const fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    /// Little-endian bytes, the order used on the wire in ZCL and EZSP.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// From big-endian bytes.
    #[must_use]
    pub const fn from_be_bytes(b: [u8; 8]) -> Self {
        Self(u64::from_be_bytes(b))
    }

    /// From little-endian bytes.
    #[must_use]
    pub const fn from_le_bytes(b: [u8; 8]) -> Self {
        Self(u64::from_le_bytes(b))
    }

    /// Parses the canonical `0x`-prefixed 16-digit hex form.
    ///
    /// Accepts upper or lower case and a missing `0x`, because both appear in
    /// the wild (`Zigbee2MQTT` writes lowercase with the prefix; users type
    /// whatever they copied). Rejects anything else rather than guessing.
    pub fn parse(s: &str) -> Result<Self, ParseIeeeError> {
        let body = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        if body.len() != 16 {
            return Err(ParseIeeeError::Length(body.len()));
        }
        u64::from_str_radix(body, 16)
            .map(Self)
            .map_err(|_| ParseIeeeError::NotHex)
    }
}

impl fmt::Display for Ieee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:016x}", self.0)
    }
}

impl fmt::Debug for Ieee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same as Display: a different Debug form here just makes logs
        // inconsistent with topics and database keys.
        write!(f, "0x{:016x}", self.0)
    }
}

impl core::str::FromStr for Ieee {
    type Err = ParseIeeeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Why an IEEE address string could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseIeeeError {
    /// Wrong number of hex digits after the optional `0x`.
    #[error("expected 16 hex digits, got {0}")]
    Length(usize),
    /// Contained a character that is not a hex digit.
    #[error("not valid hexadecimal")]
    NotHex,
}

/// A 16-bit network (short) address. Reassigned on rejoin, so never use it as
/// a durable identity — that is what [`Ieee`] is for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Nwk(u16);

impl Nwk {
    /// The coordinator is always `0x0000`.
    pub const COORDINATOR: Self = Self(0x0000);

    /// Wraps a raw `u16`.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The raw `u16`.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// True for addresses in the broadcast range `0xfff8..=0xffff`.
    #[must_use]
    pub const fn is_broadcast(self) -> bool {
        self.0 >= 0xfff8
    }
}

impl fmt::Display for Nwk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

impl fmt::Debug for Nwk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

/// A ZCL cluster identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ClusterId(pub u16);

impl fmt::Debug for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

/// A ZCL attribute identifier, scoped to a cluster.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttrId(pub u16);

impl fmt::Debug for AttrId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

/// A ZCL command identifier, scoped to a cluster and a direction.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(pub u8);

impl fmt::Debug for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:02x}", self.0)
    }
}

/// An endpoint number. `0` is ZDO; `1..=240` are application endpoints;
/// `255` is the broadcast endpoint.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(pub u8);

impl EndpointId {
    /// The ZDO endpoint.
    pub const ZDO: Self = Self(0);
    /// The endpoint every Home Automation coordinator registers.
    pub const HA: Self = Self(1);
    /// The broadcast endpoint.
    pub const BROADCAST: Self = Self(255);
}

impl fmt::Debug for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ep{}", self.0)
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A Zigbee group identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(pub u16);

impl fmt::Debug for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "group{}", self.0)
    }
}

/// A ZCL profile identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId(pub u16);

impl ProfileId {
    /// Home Automation, `0x0104` — what virtually every consumer device uses.
    pub const HA: Self = Self(0x0104);
    /// Zigbee Device Objects, `0x0000`.
    pub const ZDO: Self = Self(0x0000);
    /// Green Power, `0xa1e0`.
    pub const GREEN_POWER: Self = Self(0xa1e0);
}

impl fmt::Debug for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

/// A manufacturer code as allocated by the Connectivity Standards Alliance.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManufacturerCode(pub u16);

impl fmt::Debug for ManufacturerCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ieee_renders_the_zigbee2mqtt_canonical_form() {
        // This exact form is an MQTT topic component and a database key, so it
        // is a compatibility surface, not a formatting preference.
        assert_eq!(
            Ieee::new(0x0012_4b00_2218_9abc).to_string(),
            "0x00124b0022189abc"
        );
        assert_eq!(
            Ieee::new(0x0017_8801_00dc_4d3f).to_string(),
            "0x0017880100dc4d3f"
        );
        // Leading zeros must survive.
        assert_eq!(Ieee::new(0x1).to_string(), "0x0000000000000001");
    }

    #[test]
    fn ieee_round_trips_through_its_text_form() {
        let a = Ieee::new(0x0017_8801_00dc_4d3f);
        assert_eq!(Ieee::parse(&a.to_string()), Ok(a));
    }

    #[test]
    fn ieee_parse_accepts_the_forms_seen_in_the_wild() {
        let want = Ieee::new(0x0017_8801_00dc_4d3f);
        assert_eq!(Ieee::parse("0x0017880100dc4d3f"), Ok(want));
        assert_eq!(Ieee::parse("0X0017880100DC4D3F"), Ok(want));
        assert_eq!(Ieee::parse("0017880100dc4d3f"), Ok(want));
    }

    #[test]
    fn ieee_parse_rejects_rather_than_guesses() {
        assert_eq!(
            Ieee::parse("0x17880100dc4d3f"),
            Err(ParseIeeeError::Length(14))
        );
        assert_eq!(Ieee::parse(""), Err(ParseIeeeError::Length(0)));
        assert_eq!(
            Ieee::parse("0xzzzzzzzzzzzzzzzz"),
            Err(ParseIeeeError::NotHex)
        );
        // A device-supplied string is untrusted input; no panics on anything.
        assert!(Ieee::parse("0x................").is_err());
    }

    #[test]
    fn ieee_byte_orders_are_distinct_and_correct() {
        let a = Ieee::new(0x0011_2233_4455_6677);
        assert_eq!(
            a.to_be_bytes(),
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]
        );
        assert_eq!(
            a.to_le_bytes(),
            [0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00]
        );
        assert_eq!(Ieee::from_le_bytes(a.to_le_bytes()), a);
        assert_eq!(Ieee::from_be_bytes(a.to_be_bytes()), a);
    }

    #[test]
    fn nwk_broadcast_range_matches_the_spec() {
        assert!(!Nwk::new(0xfff7).is_broadcast());
        assert!(Nwk::new(0xfff8).is_broadcast());
        assert!(Nwk::new(0xffff).is_broadcast());
        assert!(!Nwk::COORDINATOR.is_broadcast());
    }

    #[test]
    fn short_addresses_render_as_four_hex_digits() {
        assert_eq!(Nwk::COORDINATOR.to_string(), "0x0000");
        assert_eq!(Nwk::new(0x1a2b).to_string(), "0x1a2b");
    }
}
