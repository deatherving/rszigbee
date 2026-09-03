//! Tuya's manufacturer-specific datapoint protocol.
//!
//! Tuya devices do not use standard ZCL clusters. Everything — a switch, a soil
//! probe, a thermostat's whole configuration — is multiplexed through one
//! manufacturer cluster, `0xef00`, keyed by a *datapoint* number whose meaning
//! is device specific. That is why they need their own codec and their own
//! mapping table, and why a stack that only speaks standard clusters cannot
//! talk to them at all.
//!
//! They matter disproportionately at the cheap end of the market: measured
//! against zigbee-herdsman-converters, 34% of soil-moisture devices and 44% of
//! illuminance devices are Tuya.
//!
//! # The wire layout, and its two endiannesses
//!
//! ```text
//! seq (u16, little endian) | datapoint... where each is:
//!   dp (u8) | type (u8) | length (u16, BIG endian) | data (length bytes)
//! ```
//!
//! The mixed endianness is not a mistake in this description. `seq` is declared
//! as a ZCL `uint16` and so is little endian like every other ZCL field, while
//! the datapoint length and numeric payloads are big endian. Getting either
//! backwards produces a frame a device silently ignores, so the layout here was
//! confirmed by having zigbee-herdsman encode a known datapoint and reading the
//! bytes back, rather than by reading a specification:
//!
//! ```text
//! 11 07 00 | 34 12 | 02 | 02 | 00 04 | 00 00 04 d2
//!  header    seq      dp   ty   len     value 1234
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::{CodecError, Reader, Writer};
use crate::ids::{ClusterId, CommandId};

/// The Tuya manufacturer cluster.
pub const CLUSTER: ClusterId = ClusterId(0xef00);

/// `dataRequest`: the coordinator setting datapoints on the device.
pub const DATA_REQUEST: CommandId = CommandId(0x00);
/// `dataQuery`: asking the device to report everything it has.
pub const DATA_QUERY: CommandId = CommandId(0x03);
/// `dataResponse`: the device answering a request.
pub const DATA_RESPONSE: CommandId = CommandId(0x01);
/// `dataReport`: the device reporting unprompted.
pub const DATA_REPORT: CommandId = CommandId(0x02);
/// `activeStatusReport`, which some firmware uses instead of `dataReport`.
pub const ACTIVE_STATUS_REPORT: CommandId = CommandId(0x06);
/// `activeStatusReportAlt`, used by others.
pub const ACTIVE_STATUS_REPORT_ALT: CommandId = CommandId(0x05);

/// Whether a command id carries reported datapoints.
///
/// Four different command ids do, because Tuya firmware is inconsistent about
/// which it uses and a device whose reports arrive under an unhandled id looks
/// completely silent.
#[must_use]
pub fn is_report(command: CommandId) -> bool {
    matches!(
        command,
        DATA_RESPONSE | DATA_REPORT | ACTIVE_STATUS_REPORT | ACTIVE_STATUS_REPORT_ALT
    )
}

/// How a datapoint's bytes are to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataType {
    /// Opaque bytes.
    Raw,
    /// One byte, zero or one.
    Bool,
    /// A four-byte big-endian signed integer.
    Number,
    /// Text.
    Str,
    /// One byte naming a choice.
    Enum,
    /// One, two or four bytes of flags.
    Bitmap,
    /// A type this build does not model, preserved rather than guessed at.
    Unknown(u8),
}

impl DataType {
    /// Reads a type byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Raw,
            1 => Self::Bool,
            2 => Self::Number,
            3 => Self::Str,
            4 => Self::Enum,
            5 => Self::Bitmap,
            other => Self::Unknown(other),
        }
    }

    /// The type byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Bool => 1,
            Self::Number => 2,
            Self::Str => 3,
            Self::Enum => 4,
            Self::Bitmap => 5,
            Self::Unknown(other) => other,
        }
    }
}

/// One datapoint's value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Value {
    /// Opaque bytes, kept so a person can work out what they mean.
    Raw(Vec<u8>),
    /// A boolean.
    Bool(bool),
    /// A signed integer, before any scaling the definition applies.
    Number(i32),
    /// Text.
    Str(String),
    /// A choice.
    Enum(u8),
    /// Flags.
    Bitmap(u32),
}

/// One datapoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datapoint {
    /// The datapoint number, whose meaning is device specific.
    pub dp: u8,
    /// Its value.
    pub value: Value,
}

/// Why a Tuya payload could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TuyaError {
    /// The payload ended mid-datapoint.
    #[error("the Tuya payload ended part-way through a datapoint")]
    Truncated,
    /// A datapoint claimed more bytes than the payload holds.
    #[error("datapoint {dp} claimed {claimed} bytes but only {available} remain")]
    BadLength {
        /// Which datapoint.
        dp: u8,
        /// What it claimed.
        claimed: usize,
        /// What was left.
        available: usize,
    },
    /// A length that cannot hold the declared type.
    #[error("datapoint {dp} declares {ty:?} but carries {length} bytes")]
    WrongLength {
        /// Which datapoint.
        dp: u8,
        /// The declared type.
        ty: DataType,
        /// The length given.
        length: usize,
    },
    /// The codec rejected it.
    #[error("malformed Tuya payload: {0}")]
    Codec(#[from] CodecError),
}

/// Decodes a reported datapoint payload.
///
/// Returns the sequence number and every datapoint in the frame — one frame can
/// carry several, and a device that reports three values in one frame is common.
///
/// # Errors
///
/// Every length is device-supplied and therefore untrusted. A datapoint
/// claiming more bytes than the frame holds is a typed error, never a read past
/// the end.
pub fn decode(payload: &[u8]) -> Result<(u16, Vec<Datapoint>), TuyaError> {
    let mut r = Reader::new(payload);
    let seq = r.u16_le()?;
    let mut out = Vec::new();

    // A trailing byte is not an error: some firmware pads. Anything long enough
    // to be a header is parsed, and anything shorter is left alone.
    while r.remaining() >= 4 {
        let dp = r.u8()?;
        let ty = DataType::from_u8(r.u8()?);
        let length = usize::from(r.u16_be()?);
        if length > r.remaining() {
            return Err(TuyaError::BadLength {
                dp,
                claimed: length,
                available: r.remaining(),
            });
        }
        let bytes = r.bytes(length)?;
        out.push(Datapoint {
            dp,
            value: read_value(dp, ty, bytes)?,
        });
    }
    Ok((seq, out))
}

/// Interprets one datapoint's bytes.
fn read_value(dp: u8, ty: DataType, bytes: &[u8]) -> Result<Value, TuyaError> {
    let wrong = |length: usize| TuyaError::WrongLength { dp, ty, length };
    Ok(match ty {
        // A type this build does not model is preserved as bytes rather than
        // guessed at, the same as a declared raw datapoint: someone has to be
        // able to work out what a new type means, and the bytes are the only
        // evidence.
        DataType::Raw | DataType::Unknown(_) => Value::Raw(bytes.to_vec()),
        DataType::Bool => match bytes {
            [b] => Value::Bool(*b != 0),
            other => return Err(wrong(other.len())),
        },
        // Four bytes, big endian, signed. Signed matters: a temperature
        // datapoint below zero read as unsigned becomes about 4.29 billion.
        DataType::Number => match bytes {
            [a, b, c, d] => Value::Number(i32::from_be_bytes([*a, *b, *c, *d])),
            other => return Err(wrong(other.len())),
        },
        DataType::Str => Value::Str(String::from_utf8_lossy(bytes).into_owned()),
        DataType::Enum => match bytes {
            [b] => Value::Enum(*b),
            other => return Err(wrong(other.len())),
        },
        // One, two or four bytes, big endian. Devices use all three widths for
        // the same kind of flag field.
        DataType::Bitmap => match bytes {
            [a] => Value::Bitmap(u32::from(*a)),
            [a, b] => Value::Bitmap(u32::from(u16::from_be_bytes([*a, *b]))),
            [a, b, c, d] => Value::Bitmap(u32::from_be_bytes([*a, *b, *c, *d])),
            other => return Err(wrong(other.len())),
        },
    })
}

/// Encodes datapoints for a `dataRequest`.
#[must_use]
pub fn encode(seq: u16, datapoints: &[Datapoint]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u16_le(seq);
    for point in datapoints {
        let (ty, bytes) = write_value(&point.value);
        w.u8(point.dp);
        w.u8(ty.to_u8());
        // Big endian, unlike the sequence above. A device silently ignores a
        // frame whose length it reads as a huge number.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a datapoint payload cannot exceed one APS frame, so it \
                      always fits in u16; the truncation is unreachable"
        )]
        w.u16_be(bytes.len().min(usize::from(u16::MAX)) as u16);
        w.bytes(&bytes);
    }
    w.into_vec()
}

/// The type byte and bytes for one value.
fn write_value(value: &Value) -> (DataType, Vec<u8>) {
    match value {
        Value::Raw(bytes) => (DataType::Raw, bytes.clone()),
        Value::Bool(on) => (DataType::Bool, alloc::vec![u8::from(*on)]),
        Value::Number(n) => (DataType::Number, n.to_be_bytes().to_vec()),
        Value::Str(s) => (DataType::Str, s.as_bytes().to_vec()),
        Value::Enum(v) => (DataType::Enum, alloc::vec![*v]),
        // Always four bytes going out. A device accepts the wider form even
        // where it reports a narrower one, and guessing a width per datapoint
        // would need per-device knowledge the definition does not carry.
        Value::Bitmap(bits) => (DataType::Bitmap, bits.to_be_bytes().to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_layout_matches_what_herdsman_encodes() {
        // Byte for byte the payload from the doc comment above, which was
        // produced by asking zigbee-herdsman to encode dp 2 = 1234. The
        // sequence is little endian and the length is big endian; either one
        // reversed makes a frame devices ignore.
        let payload = [0x34, 0x12, 0x02, 0x02, 0x00, 0x04, 0x00, 0x00, 0x04, 0xd2];
        let (seq, points) = decode(&payload).expect("a well-formed payload");
        assert_eq!(seq, 0x1234);
        assert_eq!(
            points,
            alloc::vec![Datapoint {
                dp: 2,
                value: Value::Number(1234)
            }]
        );
    }

    #[test]
    fn encoding_round_trips_through_decoding() {
        let points = alloc::vec![
            Datapoint {
                dp: 1,
                value: Value::Bool(true)
            },
            Datapoint {
                dp: 2,
                value: Value::Number(-4200)
            },
            Datapoint {
                dp: 4,
                value: Value::Enum(3)
            },
        ];
        let encoded = encode(0x0102, &points);
        let (seq, decoded) = decode(&encoded).expect("our own encoding");
        assert_eq!(seq, 0x0102);
        assert_eq!(decoded, points);
    }

    #[test]
    fn a_negative_number_survives_the_round_trip() {
        // Read as unsigned, minus 100 becomes about 4.29 billion, which as a
        // temperature in tenths is 429 million degrees.
        let encoded = encode(
            0,
            &alloc::vec![Datapoint {
                dp: 5,
                value: Value::Number(-100)
            }],
        );
        let (_, decoded) = decode(&encoded).expect("round trip");
        assert_eq!(decoded[0].value, Value::Number(-100));
    }

    #[test]
    fn several_datapoints_in_one_frame_all_decode() {
        // Common: a sensor reports temperature, humidity and battery together.
        let payload = [
            0x00, 0x01, // seq
            0x01, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x64, // dp 1 = 100
            0x02, 0x01, 0x00, 0x01, 0x01, // dp 2 = true
            0x03, 0x04, 0x00, 0x01, 0x02, // dp 3 = enum 2
        ];
        let (_, points) = decode(&payload).expect("three datapoints");
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].value, Value::Number(100));
        assert_eq!(points[1].value, Value::Bool(true));
        assert_eq!(points[2].value, Value::Enum(2));
    }

    #[test]
    fn a_datapoint_claiming_more_bytes_than_exist_is_an_error_not_a_panic() {
        // Every length here is device supplied. This is the case that must not
        // read past the end of the buffer.
        let payload = [0x00, 0x00, 0x01, 0x02, 0xff, 0xff, 0x00];
        let error = decode(&payload).expect_err("a claimed length beyond the frame");
        assert!(
            matches!(error, TuyaError::BadLength { dp: 1, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn every_prefix_of_a_valid_frame_either_decodes_or_errors() {
        // The truncation sweep the parse-path invariant requires: no prefix of
        // a real frame may panic.
        let full = [
            0x00, 0x01, 0x01, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x64, 0x02, 0x01, 0x00, 0x01,
            0x01,
        ];
        for n in 0..=full.len() {
            let _ = decode(&full[..n]);
        }
    }

    #[test]
    fn a_number_of_the_wrong_width_is_reported_rather_than_padded() {
        // Three bytes cannot be a four-byte integer. Padding it would invent a
        // reading; the device is misbehaving and that should be visible.
        let payload = [0x00, 0x00, 0x07, 0x02, 0x00, 0x03, 0x01, 0x02, 0x03];
        let error = decode(&payload).expect_err("a three-byte number");
        assert!(
            matches!(
                error,
                TuyaError::WrongLength {
                    dp: 7,
                    ty: DataType::Number,
                    length: 3
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_bitmap_decodes_at_all_three_widths_devices_use() {
        for (bytes, expected) in [
            (alloc::vec![0x81u8], 0x81u32),
            (alloc::vec![0x01, 0x02], 0x0102),
            (alloc::vec![0x00, 0x00, 0x01, 0x02], 0x0102),
        ] {
            let mut payload = alloc::vec![0x00, 0x00, 0x09, 0x05];
            #[expect(
                clippy::cast_possible_truncation,
                reason = "test data, three bytes at most"
            )]
            let length = bytes.len() as u16;
            payload.extend_from_slice(&length.to_be_bytes());
            payload.extend_from_slice(&bytes);
            let (_, points) = decode(&payload).expect("a bitmap");
            assert_eq!(points[0].value, Value::Bitmap(expected), "{bytes:?}");
        }
    }

    #[test]
    fn an_unmodelled_type_is_preserved_as_bytes_rather_than_dropped() {
        // Someone has to be able to work out what a new type means, and the
        // bytes are the only evidence.
        let payload = [0x00, 0x00, 0x0a, 0x7f, 0x00, 0x02, 0xde, 0xad];
        let (_, points) = decode(&payload).expect("an unknown type");
        assert_eq!(points[0].value, Value::Raw(alloc::vec![0xde, 0xad]));
    }

    #[test]
    fn all_four_report_command_ids_are_recognised() {
        // Tuya firmware is inconsistent about which it uses, and a device whose
        // reports arrive under an unhandled id looks completely silent.
        for command in [
            DATA_RESPONSE,
            DATA_REPORT,
            ACTIVE_STATUS_REPORT,
            ACTIVE_STATUS_REPORT_ALT,
        ] {
            assert!(is_report(command), "{command:?}");
        }
        assert!(!is_report(DATA_REQUEST));
        assert!(!is_report(DATA_QUERY));
    }
}
