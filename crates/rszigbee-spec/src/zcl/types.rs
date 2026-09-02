//! ZCL data types and dynamically typed values.
//!
//! The type system has to be dynamic. A device's clusters are described by
//! data — the global cluster table plus per-device custom clusters registered
//! at runtime (README, "Manufacturer-specific clusters") — so the decoder learns an
//! attribute's type from the registry or from the frame, not from a Rust type
//! parameter. Static typing happens one level up, where a `Capability`
//! declares what it expects and rejects a mismatch.

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::{CodecError, Reader, Writer};

/// A ZCL data type discriminator, as it appears on the wire.
///
/// Only the types that real devices use are enumerated; everything else stays
/// accessible through [`ZclType::Unknown`] so an unrecognised type is a
/// decoding decision, not a hard failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ZclType {
    /// `0x00` — no data.
    NoData,
    /// `0x08`..=`0x0f` — opaque data of 1..=8 octets.
    Data(u8),
    /// `0x10` — boolean. `0xff` on the wire means "invalid".
    Bool,
    /// `0x18`..=`0x1f` — bitmap of 1..=8 octets.
    Bitmap(u8),
    /// `0x20`..=`0x27` — unsigned integer of 1..=8 octets.
    Uint(u8),
    /// `0x28`..=`0x2f` — signed integer of 1..=8 octets.
    Int(u8),
    /// `0x30` — 8-bit enumeration.
    Enum8,
    /// `0x31` — 16-bit enumeration.
    Enum16,
    /// `0x39` — IEEE 754 single precision.
    Single,
    /// `0x41` — octet string, `u8` length prefix.
    OctStr,
    /// `0x42` — character string, `u8` length prefix.
    CharStr,
    /// `0xe0`..=`0xe2` — time of day, date, UTC time.
    Time(u8),
    /// `0xe8` — cluster id.
    ClusterId,
    /// `0xe9` — attribute id.
    AttrId,
    /// `0xf0` — IEEE address.
    Ieee,
    /// `0xf1` — 128-bit security key.
    Key128,
    /// `0xff` — unknown/absent.
    Unk,
    /// A discriminator this build does not model.
    Unknown(u8),
}

impl ZclType {
    /// Maps a wire discriminator to a type.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub const fn from_u8(id: u8) -> Self {
        match id {
            0x00 => Self::NoData,
            0x08..=0x0f => Self::Data(id - 0x08 + 1),
            0x10 => Self::Bool,
            0x18..=0x1f => Self::Bitmap(id - 0x18 + 1),
            0x20..=0x27 => Self::Uint(id - 0x20 + 1),
            0x28..=0x2f => Self::Int(id - 0x28 + 1),
            0x30 => Self::Enum8,
            0x31 => Self::Enum16,
            0x39 => Self::Single,
            0x41 => Self::OctStr,
            0x42 => Self::CharStr,
            0xe0..=0xe2 => Self::Time(id - 0xe0),
            0xe8 => Self::ClusterId,
            0xe9 => Self::AttrId,
            0xf0 => Self::Ieee,
            0xf1 => Self::Key128,
            0xff => Self::Unk,
            other => Self::Unknown(other),
        }
    }

    /// The wire discriminator.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::NoData => 0x00,
            Self::Data(n) => 0x08 + n - 1,
            Self::Bool => 0x10,
            Self::Bitmap(n) => 0x18 + n - 1,
            Self::Uint(n) => 0x20 + n - 1,
            Self::Int(n) => 0x28 + n - 1,
            Self::Enum8 => 0x30,
            Self::Enum16 => 0x31,
            Self::Single => 0x39,
            Self::OctStr => 0x41,
            Self::CharStr => 0x42,
            Self::Time(n) => 0xe0 + n,
            Self::ClusterId => 0xe8,
            Self::AttrId => 0xe9,
            Self::Ieee => 0xf0,
            Self::Key128 => 0xf1,
            Self::Unk => 0xff,
            Self::Unknown(o) => o,
        }
    }

    /// Fixed wire size in octets, or `None` for variable-length types.
    // `Single` and `Time` are both four octets for entirely unrelated reasons;
    // merging those arms would put a float next to three time types and read
    // worse than the duplication.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn fixed_size(self) -> Option<usize> {
        match self {
            Self::NoData | Self::Unk => Some(0),
            Self::Bool | Self::Enum8 => Some(1),
            Self::Enum16 | Self::ClusterId | Self::AttrId => Some(2),
            Self::Single => Some(4),
            Self::Data(n) | Self::Bitmap(n) | Self::Uint(n) | Self::Int(n) => Some(n as usize),
            Self::Time(0..=2) => Some(4),
            Self::Ieee => Some(8),
            Self::Key128 => Some(16),
            // Variable length, or unknown length because the type is not
            // modelled — both mean "the caller cannot skip this field".
            Self::OctStr | Self::CharStr | Self::Time(_) | Self::Unknown(_) => None,
        }
    }
}

/// A decoded ZCL value.
///
/// `Invalid` is a first-class variant rather than an error because ZCL defines
/// per-type "invalid" encodings (`0xff` for `uint8`, `0xffff` for `uint16`,
/// `0x8000` for `int16`, and so on) that devices send routinely to mean "no
/// reading". Collapsing that into a decode error would turn normal traffic into
/// a stream of failures; collapsing it into `0` would report a temperature of
/// zero degrees when the sensor means "I do not know".
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ZclValue {
    /// No data.
    None,
    /// A boolean.
    Bool(bool),
    /// An unsigned integer, widened.
    Uint(u64),
    /// A signed integer, widened.
    Int(i64),
    /// An enumeration ordinal.
    Enum(u16),
    /// A bitmap, widened.
    Bitmap(u64),
    /// A single-precision float.
    Single(f32),
    /// An octet string.
    Octets(Vec<u8>),
    /// A character string.
    Str(String),
    /// An IEEE address.
    Ieee(crate::ids::Ieee),
    /// A 128-bit key. Never rendered by `Display`; see the security notes in
    /// the README, "The parse-path invariant".
    Key128([u8; 16]),
    /// Opaque bytes for a type this build does not model.
    Raw {
        /// The wire type discriminator.
        ty: u8,
        /// The bytes as received.
        bytes: Vec<u8>,
    },
    /// The type's defined "invalid"/"unknown" encoding.
    Invalid(ZclType),
}

impl ZclValue {
    /// The value as an unsigned integer, if it is one.
    #[must_use]
    pub const fn as_uint(&self) -> Option<u64> {
        match self {
            Self::Uint(v) | Self::Bitmap(v) => Some(*v),
            Self::Enum(v) => Some(*v as u64),
            Self::Bool(b) => Some(*b as u64),
            _ => None,
        }
    }

    /// The value as a signed integer, if it is one.
    #[must_use]
    pub const fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            #[allow(clippy::cast_possible_wrap)]
            Self::Uint(v) => Some(*v as i64),
            _ => None,
        }
    }

    /// True when this is one of the defined "no reading" encodings.
    #[must_use]
    pub const fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }
}

/// Decodes one value of the given type.
///
/// Every failure path returns [`CodecError`]; nothing here can panic on
/// arbitrary input, which is the invariant the fuzz targets assert.
pub fn decode_value(ty: ZclType, r: &mut Reader<'_>) -> Result<ZclValue, CodecError> {
    match ty {
        ZclType::NoData | ZclType::Unk => Ok(ZclValue::None),
        ZclType::Bool => match r.u8()? {
            0x00 => Ok(ZclValue::Bool(false)),
            0xff => Ok(ZclValue::Invalid(ty)),
            // ZCL says 0x01 is true; devices send other non-zero values, and
            // treating those as true is what every working stack does.
            _ => Ok(ZclValue::Bool(true)),
        },
        ZclType::Uint(n) | ZclType::Bitmap(n) => {
            let raw = read_uint(r, n)?;
            if ty.fixed_size().is_some() && is_invalid_uint(raw, n) {
                if matches!(ty, ZclType::Bitmap(_)) {
                    // Bitmaps have no invalid encoding: all bits set is a
                    // legitimate value.
                    return Ok(ZclValue::Bitmap(raw));
                }
                return Ok(ZclValue::Invalid(ty));
            }
            Ok(match ty {
                ZclType::Bitmap(_) => ZclValue::Bitmap(raw),
                _ => ZclValue::Uint(raw),
            })
        }
        ZclType::Int(n) => {
            let raw = read_uint(r, n)?;
            if is_invalid_int(raw, n) {
                return Ok(ZclValue::Invalid(ty));
            }
            Ok(ZclValue::Int(sign_extend(raw, n)))
        }
        ZclType::Enum8 => match r.u8()? {
            0xff => Ok(ZclValue::Invalid(ty)),
            v => Ok(ZclValue::Enum(u16::from(v))),
        },
        ZclType::Enum16 => match r.u16_le()? {
            0xffff => Ok(ZclValue::Invalid(ty)),
            v => Ok(ZclValue::Enum(v)),
        },
        ZclType::Single => {
            let bits = r.u32_le()?;
            let f = f32::from_bits(bits);
            if f.is_nan() {
                // ZCL uses NaN as the invalid encoding for floats.
                Ok(ZclValue::Invalid(ty))
            } else {
                Ok(ZclValue::Single(f))
            }
        }
        ZclType::OctStr => match r.octstr()? {
            None => Ok(ZclValue::Invalid(ty)),
            Some(b) => Ok(ZclValue::Octets(b.to_vec())),
        },
        ZclType::CharStr => match r.string()? {
            None => Ok(ZclValue::Invalid(ty)),
            Some(s) => Ok(ZclValue::Str(sanitise(s))),
        },
        ZclType::ClusterId | ZclType::AttrId => Ok(ZclValue::Uint(u64::from(r.u16_le()?))),
        ZclType::Time(_) => Ok(ZclValue::Uint(u64::from(r.u32_le()?))),
        ZclType::Ieee => Ok(ZclValue::Ieee(r.ieee_le()?)),
        ZclType::Key128 => {
            let b = r.bytes(16)?;
            let mut k = [0u8; 16];
            for (i, slot) in k.iter_mut().enumerate() {
                *slot = b.get(i).copied().unwrap_or_default();
            }
            Ok(ZclValue::Key128(k))
        }
        ZclType::Data(n) => Ok(ZclValue::Raw {
            ty: ty.to_u8(),
            bytes: r.bytes(usize::from(n))?.to_vec(),
        }),
        // An unmodelled type has unknown length, so the only safe thing is to
        // stop consuming and hand the caller what is left. Guessing a length
        // would desynchronise every subsequent field in the frame.
        ZclType::Unknown(o) => Ok(ZclValue::Raw {
            ty: o,
            bytes: r.rest().to_vec(),
        }),
    }
}

/// Encodes one value, with its type discriminator determined by the value.
pub fn encode_value(v: &ZclValue, ty: ZclType, w: &mut Writer) -> Result<(), CodecError> {
    match (v, ty) {
        (ZclValue::None, _) => Ok(()),
        (ZclValue::Bool(b), _) => {
            w.u8(u8::from(*b));
            Ok(())
        }
        (ZclValue::Uint(x) | ZclValue::Bitmap(x), ZclType::Uint(n) | ZclType::Bitmap(n)) => {
            write_uint(w, *x, n);
            Ok(())
        }
        #[allow(clippy::cast_sign_loss)]
        (ZclValue::Int(x), ZclType::Int(n)) => {
            write_uint(w, *x as u64, n);
            Ok(())
        }
        (ZclValue::Enum(x), ZclType::Enum8) => {
            w.u8(u8::try_from(*x).unwrap_or(0xfe));
            Ok(())
        }
        (ZclValue::Enum(x), ZclType::Enum16) => {
            w.u16_le(*x);
            Ok(())
        }
        (ZclValue::Single(f), _) => {
            w.u32_le(f.to_bits());
            Ok(())
        }
        (ZclValue::Octets(b), _) => w.octstr(Some(b)).map(|_| ()),
        (ZclValue::Str(s), _) => w.octstr(Some(s.as_bytes())).map(|_| ()),
        (ZclValue::Ieee(a), _) => {
            w.ieee_le(*a);
            Ok(())
        }
        (ZclValue::Key128(k), _) => {
            w.bytes(k);
            Ok(())
        }
        (ZclValue::Raw { bytes, .. }, _) => {
            w.bytes(bytes);
            Ok(())
        }
        (ZclValue::Invalid(t), _) => {
            write_invalid(w, *t);
            Ok(())
        }
        // A value/type mismatch is a programming error in the caller, but it
        // must not panic: report it rather than writing a wrong-width field
        // that silently corrupts the rest of the frame.
        _ => Err(CodecError::OutputFull { wanted: 0 }),
    }
}

fn read_uint(r: &mut Reader<'_>, n: u8) -> Result<u64, CodecError> {
    let bytes = r.bytes(usize::from(n))?;
    let mut acc: u64 = 0;
    // Little-endian: least significant byte first.
    for (i, b) in bytes.iter().enumerate() {
        let shift = u32::try_from(i).unwrap_or(0).saturating_mul(8);
        if shift < 64 {
            acc |= u64::from(*b) << shift;
        }
    }
    Ok(acc)
}

fn write_uint(w: &mut Writer, v: u64, n: u8) {
    for i in 0..usize::from(n) {
        let shift = u32::try_from(i).unwrap_or(0).saturating_mul(8);
        let byte = if shift < 64 {
            ((v >> shift) & 0xff) as u8
        } else {
            0
        };
        w.u8(byte);
    }
}

fn write_invalid(w: &mut Writer, ty: ZclType) {
    match ty {
        ZclType::Int(n) => {
            // The most negative value: 0x80 in the top byte, zeros below.
            for i in 0..usize::from(n) {
                w.u8(if i + 1 == usize::from(n) { 0x80 } else { 0x00 });
            }
        }
        ZclType::Single => {
            w.u32_le(f32::NAN.to_bits());
        }
        ZclType::OctStr | ZclType::CharStr => {
            w.u8(0xff);
        }
        other => {
            let n = other.fixed_size().unwrap_or(1);
            for _ in 0..n {
                w.u8(0xff);
            }
        }
    }
}

const fn is_invalid_uint(raw: u64, n: u8) -> bool {
    match n {
        1 => raw == 0xff,
        2 => raw == 0xffff,
        3 => raw == 0x00ff_ffff,
        4 => raw == 0xffff_ffff,
        _ => false,
    }
}

const fn is_invalid_int(raw: u64, n: u8) -> bool {
    match n {
        1 => raw == 0x80,
        2 => raw == 0x8000,
        4 => raw == 0x8000_0000,
        _ => false,
    }
}

#[allow(clippy::cast_possible_wrap)]
const fn sign_extend(raw: u64, n: u8) -> i64 {
    let bits = (n as u32).saturating_mul(8);
    if bits == 0 || bits >= 64 {
        return raw as i64;
    }
    let shift = 64 - bits;
    ((raw << shift) as i64) >> shift
}

/// Strips NUL and other C0 control characters from a device-supplied string.
///
/// This is not cosmetic. Upstream hit a real Home Assistant bug because Zigbee
/// devices ship a `manufacturerName` containing a NUL (see the note in
/// zigbee2mqtt's `publishEntityState`), and these strings end up in MQTT
/// topics, entity ids and log lines.
fn sanitise(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(ty: u8, bytes: &[u8]) -> ZclValue {
        let mut r = Reader::new(bytes);
        decode_value(ZclType::from_u8(ty), &mut r).expect("decode")
    }

    #[test]
    fn type_discriminators_round_trip_across_the_whole_byte_range() {
        for id in 0u8..=255 {
            let ty = ZclType::from_u8(id);
            assert_eq!(ty.to_u8(), id, "type 0x{id:02x} did not round-trip");
        }
    }

    #[test]
    fn unsigned_integers_decode_little_endian() {
        assert_eq!(dec(0x20, &[0x2a]), ZclValue::Uint(42));
        assert_eq!(dec(0x21, &[0x34, 0x12]), ZclValue::Uint(0x1234));
        assert_eq!(
            dec(0x23, &[0x78, 0x56, 0x34, 0x12]),
            ZclValue::Uint(0x1234_5678)
        );
    }

    #[test]
    fn signed_integers_sign_extend() {
        // -100 as int8, the shape of a temperature offset.
        assert_eq!(dec(0x28, &[0x9c]), ZclValue::Int(-100));
        // -1000 as int16, the shape of a sub-zero temperature (=-10.00 C).
        assert_eq!(dec(0x29, &[0x18, 0xfc]), ZclValue::Int(-1000));
        assert_eq!(dec(0x29, &[0xe8, 0x03]), ZclValue::Int(1000));
    }

    #[test]
    fn the_invalid_encodings_are_not_reported_as_readings() {
        // This is the difference between "sensor has no reading" and
        // "sensor reports 655.35 degrees".
        assert_eq!(dec(0x20, &[0xff]), ZclValue::Invalid(ZclType::Uint(1)));
        assert_eq!(
            dec(0x21, &[0xff, 0xff]),
            ZclValue::Invalid(ZclType::Uint(2))
        );
        assert_eq!(dec(0x29, &[0x00, 0x80]), ZclValue::Invalid(ZclType::Int(2)));
        assert_eq!(dec(0x30, &[0xff]), ZclValue::Invalid(ZclType::Enum8));
        assert_eq!(dec(0x10, &[0xff]), ZclValue::Invalid(ZclType::Bool));
        assert!(dec(0x20, &[0xff]).is_invalid());
    }

    #[test]
    fn all_bits_set_is_a_real_bitmap_value_not_an_invalid_one() {
        // Bitmaps have no invalid encoding, unlike uints of the same width.
        assert_eq!(dec(0x18, &[0xff]), ZclValue::Bitmap(0xff));
        assert_eq!(dec(0x19, &[0xff, 0xff]), ZclValue::Bitmap(0xffff));
    }

    #[test]
    fn booleans_accept_the_non_conforming_values_devices_actually_send() {
        assert_eq!(dec(0x10, &[0x00]), ZclValue::Bool(false));
        assert_eq!(dec(0x10, &[0x01]), ZclValue::Bool(true));
        assert_eq!(dec(0x10, &[0x7f]), ZclValue::Bool(true));
    }

    #[test]
    fn strings_are_stripped_of_control_characters() {
        // A manufacturerName with an embedded NUL caused a real downstream bug.
        let mut r = Reader::new(&[0x05, b'L', b'U', 0x00, b'M', b'I']);
        let v = decode_value(ZclType::CharStr, &mut r).unwrap();
        assert_eq!(v, ZclValue::Str("LUMI".into()));
    }

    #[test]
    fn truncated_values_error_rather_than_panic() {
        let mut r = Reader::new(&[0x12]);
        assert!(decode_value(ZclType::Uint(4), &mut r).is_err());
        let mut r = Reader::new(&[]);
        assert!(decode_value(ZclType::Ieee, &mut r).is_err());
        let mut r = Reader::new(&[0x0a, b'x']);
        assert!(decode_value(ZclType::CharStr, &mut r).is_err());
    }

    #[test]
    fn an_unmodelled_type_stops_consuming_instead_of_guessing_a_length() {
        // Desynchronising the rest of the frame is worse than losing one field.
        let mut r = Reader::new(&[0xaa, 0xbb, 0xcc]);
        let v = decode_value(ZclType::from_u8(0x4a), &mut r).unwrap();
        assert_eq!(
            v,
            ZclValue::Raw {
                ty: 0x4a,
                bytes: alloc::vec![0xaa, 0xbb, 0xcc]
            }
        );
        assert!(r.is_empty());
    }

    #[test]
    fn values_round_trip_through_encode_and_decode() {
        let cases: &[(ZclType, ZclValue)] = &[
            (ZclType::Uint(1), ZclValue::Uint(42)),
            (ZclType::Uint(2), ZclValue::Uint(0x1234)),
            (ZclType::Int(2), ZclValue::Int(-1000)),
            (ZclType::Bool, ZclValue::Bool(true)),
            (ZclType::Enum8, ZclValue::Enum(3)),
            (ZclType::Enum16, ZclValue::Enum(300)),
            (
                ZclType::Ieee,
                ZclValue::Ieee(crate::ids::Ieee::new(0x0017_8801_00dc_4d3f)),
            ),
            (ZclType::CharStr, ZclValue::Str("SNZB-02D".into())),
        ];
        for (ty, v) in cases {
            let mut w = Writer::new();
            encode_value(v, *ty, &mut w).expect("encode");
            let mut r = Reader::new(w.as_slice());
            assert_eq!(
                &decode_value(*ty, &mut r).expect("decode"),
                v,
                "type {ty:?}"
            );
            assert!(r.is_empty(), "type {ty:?} left trailing bytes");
        }
    }

    #[test]
    fn invalid_encodings_round_trip_too() {
        for ty in [
            ZclType::Uint(1),
            ZclType::Uint(2),
            ZclType::Int(2),
            ZclType::CharStr,
        ] {
            let mut w = Writer::new();
            encode_value(&ZclValue::Invalid(ty), ty, &mut w).unwrap();
            let mut r = Reader::new(w.as_slice());
            assert_eq!(
                decode_value(ty, &mut r).unwrap(),
                ZclValue::Invalid(ty),
                "{ty:?}"
            );
        }
    }

    #[test]
    fn fixed_sizes_match_the_discriminator_ranges() {
        assert_eq!(ZclType::from_u8(0x20).fixed_size(), Some(1));
        assert_eq!(ZclType::from_u8(0x27).fixed_size(), Some(8));
        assert_eq!(ZclType::from_u8(0xf0).fixed_size(), Some(8));
        assert_eq!(ZclType::from_u8(0xf1).fixed_size(), Some(16));
        assert_eq!(ZclType::from_u8(0x42).fixed_size(), None);
    }
}
