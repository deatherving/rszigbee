//! Byte reader and writer for every Zigbee codec in this crate.
//!
//! This is the equivalent of zigbee-herdsman's `Buffalo`, with one difference
//! that matters: **every read returns `Result`.** Radio frames are untrusted
//! input (README, "The parse-path invariant"), so the invariant this module exists to
//! enforce is that no malformed frame can panic the process — no slice
//! indexing, no `unwrap`, no arithmetic that can overflow.
//!
//! Zigbee is little-endian on the wire almost everywhere; the big-endian
//! readers exist for the handful of places that are not (notably Tuya
//! datapoint values).

use alloc::vec::Vec;
use core::fmt;

/// Why a read or write failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The buffer ended before the requested number of bytes.
    #[error(
        "unexpected end of input: wanted {wanted} bytes at offset {offset}, {available} available"
    )]
    Eof {
        /// Bytes requested.
        wanted: usize,
        /// Offset at which the read was attempted.
        offset: usize,
        /// Bytes actually remaining.
        available: usize,
    },
    /// A length prefix claimed more bytes than the buffer holds. Kept distinct
    /// from `Eof` because it usually means a hostile or corrupt frame rather
    /// than a truncated one.
    #[error(
        "length prefix of {claimed} exceeds the {available} bytes remaining at offset {offset}"
    )]
    BadLength {
        /// The length the frame claimed.
        claimed: usize,
        /// Bytes actually remaining.
        available: usize,
        /// Offset of the length prefix.
        offset: usize,
    },
    /// A string field was not valid UTF-8.
    #[error("invalid UTF-8 in string field at offset {offset}")]
    BadUtf8 {
        /// Offset of the string field.
        offset: usize,
    },
    /// A write ran out of room in a fixed-size buffer.
    #[error("output full: cannot write {wanted} more bytes")]
    OutputFull {
        /// Bytes the write needed.
        wanted: usize,
    },
}

/// A cursor over a byte slice that never panics.
#[derive(Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl fmt::Debug for Reader<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("pos", &self.pos)
            .field("len", &self.buf.len())
            .finish()
    }
}

impl<'a> Reader<'a> {
    /// Wraps a slice.
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Current offset from the start of the slice.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// True when the cursor is at the end.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The unconsumed bytes, without consuming them.
    #[must_use]
    pub fn peek_rest(&self) -> &'a [u8] {
        self.buf.get(self.pos..).unwrap_or(&[])
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self.pos.checked_add(n).ok_or(CodecError::Eof {
            wanted: n,
            offset: self.pos,
            available: self.remaining(),
        })?;
        let slice = self.buf.get(self.pos..end).ok_or(CodecError::Eof {
            wanted: n,
            offset: self.pos,
            available: self.remaining(),
        })?;
        self.pos = end;
        Ok(slice)
    }

    /// Consumes and returns `n` bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        self.take(n)
    }

    /// Consumes everything left.
    pub fn rest(&mut self) -> &'a [u8] {
        let out = self.peek_rest();
        self.pos = self.buf.len();
        out
    }

    /// Skips `n` bytes.
    pub fn skip(&mut self, n: usize) -> Result<(), CodecError> {
        self.take(n).map(|_| ())
    }

    /// Reads a `u8`.
    pub fn u8(&mut self) -> Result<u8, CodecError> {
        let b = self.take(1)?;
        b.first().copied().ok_or(CodecError::Eof {
            wanted: 1,
            offset: self.pos,
            available: 0,
        })
    }

    /// Reads an `i8`.
    #[allow(clippy::cast_possible_wrap)]
    pub fn i8(&mut self) -> Result<i8, CodecError> {
        Ok(self.u8()? as i8)
    }

    /// Reads a little-endian `u16`.
    pub fn u16_le(&mut self) -> Result<u16, CodecError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([
            b.first().copied().unwrap_or_default(),
            b.get(1).copied().unwrap_or_default(),
        ]))
    }

    /// Reads a big-endian `u16`.
    pub fn u16_be(&mut self) -> Result<u16, CodecError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([
            b.first().copied().unwrap_or_default(),
            b.get(1).copied().unwrap_or_default(),
        ]))
    }

    /// Reads a little-endian `u24`, widened to `u32`.
    pub fn u24_le(&mut self) -> Result<u32, CodecError> {
        let b = self.take(3)?;
        Ok(u32::from_le_bytes([
            b.first().copied().unwrap_or_default(),
            b.get(1).copied().unwrap_or_default(),
            b.get(2).copied().unwrap_or_default(),
            0,
        ]))
    }

    /// Reads a little-endian `u32`.
    pub fn u32_le(&mut self) -> Result<u32, CodecError> {
        let b = self.take(4)?;
        let mut a = [0u8; 4];
        for (i, slot) in a.iter_mut().enumerate() {
            *slot = b.get(i).copied().unwrap_or_default();
        }
        Ok(u32::from_le_bytes(a))
    }

    /// Reads a big-endian `u32`.
    pub fn u32_be(&mut self) -> Result<u32, CodecError> {
        let b = self.take(4)?;
        let mut a = [0u8; 4];
        for (i, slot) in a.iter_mut().enumerate() {
            *slot = b.get(i).copied().unwrap_or_default();
        }
        Ok(u32::from_be_bytes(a))
    }

    /// Reads a little-endian `u64`.
    pub fn u64_le(&mut self) -> Result<u64, CodecError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        for (i, slot) in a.iter_mut().enumerate() {
            *slot = b.get(i).copied().unwrap_or_default();
        }
        Ok(u64::from_le_bytes(a))
    }

    /// Reads a little-endian IEEE address (the wire order).
    pub fn ieee_le(&mut self) -> Result<crate::ids::Ieee, CodecError> {
        Ok(crate::ids::Ieee::new(self.u64_le()?))
    }

    /// Reads a length-prefixed byte string with a `u8` prefix.
    ///
    /// A prefix of `0xff` means "no value" in ZCL octet and character strings,
    /// which is why this returns `Option`.
    pub fn octstr(&mut self) -> Result<Option<&'a [u8]>, CodecError> {
        let at = self.pos;
        let len = self.u8()?;
        if len == 0xff {
            return Ok(None);
        }
        let n = usize::from(len);
        if n > self.remaining() {
            return Err(CodecError::BadLength {
                claimed: n,
                available: self.remaining(),
                offset: at,
            });
        }
        self.take(n).map(Some)
    }

    /// Reads a `u8`-length-prefixed UTF-8 string.
    ///
    /// ZCL character strings are nominally ASCII, but real devices ship UTF-8
    /// and worse — a decode failure is an error, never a panic.
    pub fn string(&mut self) -> Result<Option<&'a str>, CodecError> {
        let at = self.pos;
        match self.octstr()? {
            None => Ok(None),
            Some(raw) => core::str::from_utf8(raw)
                .map(Some)
                .map_err(|_| CodecError::BadUtf8 { offset: at }),
        }
    }
}

/// A growable byte writer.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// A new empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// A new writer with reserved capacity.
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The written bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the writer, yielding the bytes.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// Appends a `u8`.
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    /// Appends an `i8`.
    #[allow(clippy::cast_sign_loss)]
    pub fn i8(&mut self, v: i8) -> &mut Self {
        self.buf.push(v as u8);
        self
    }

    /// Appends a little-endian `u16`.
    pub fn u16_le(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Appends a big-endian `u16`.
    pub fn u16_be(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    /// Appends the low three bytes of `v`, little-endian.
    pub fn u24_le(&mut self, v: u32) -> &mut Self {
        let b = v.to_le_bytes();
        self.buf.extend_from_slice(b.get(..3).unwrap_or(&[]));
        self
    }

    /// Appends a little-endian `u32`.
    pub fn u32_le(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Appends a big-endian `u32`.
    pub fn u32_be(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    /// Appends a little-endian `u64`.
    pub fn u64_le(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Appends an IEEE address in wire (little-endian) order.
    pub fn ieee_le(&mut self, v: crate::ids::Ieee) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Appends raw bytes.
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    /// Appends a `u8`-length-prefixed byte string, or the `0xff` "no value"
    /// marker for `None`.
    pub fn octstr(&mut self, v: Option<&[u8]>) -> Result<&mut Self, CodecError> {
        match v {
            None => {
                self.buf.push(0xff);
            }
            Some(raw) => {
                let len = u8::try_from(raw.len())
                    .map_err(|_| CodecError::OutputFull { wanted: raw.len() })?;
                if len == 0xff {
                    // 255 bytes is indistinguishable from "no value" on the wire.
                    return Err(CodecError::OutputFull { wanted: raw.len() });
                }
                self.buf.push(len);
                self.buf.extend_from_slice(raw);
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_integers() {
        let mut r = Reader::new(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(r.u16_le().unwrap(), 0x0201);
        assert_eq!(r.u16_le().unwrap(), 0x0403);
        assert_eq!(r.u32_le().unwrap(), 0x0807_0605);
        assert!(r.is_empty());
    }

    #[test]
    fn reads_big_endian_where_needed() {
        // Tuya datapoint values are big-endian; getting this wrong is a classic
        // source of "why is my temperature 5888 degrees".
        let mut r = Reader::new(&[0x00, 0x00, 0x01, 0x0e]);
        assert_eq!(r.u32_be().unwrap(), 270);
    }

    #[test]
    fn truncated_input_errors_and_never_panics() {
        let mut r = Reader::new(&[0x01]);
        assert!(matches!(r.u16_le(), Err(CodecError::Eof { wanted: 2, .. })));
        // The cursor must not have advanced past the end on failure.
        assert_eq!(r.position(), 0);
        assert_eq!(r.remaining(), 1);
    }

    #[test]
    fn empty_input_errors_on_every_reader() {
        let mut r = Reader::new(&[]);
        assert!(r.u8().is_err());
        assert!(r.u16_le().is_err());
        assert!(r.u24_le().is_err());
        assert!(r.u32_le().is_err());
        assert!(r.u64_le().is_err());
        assert!(r.ieee_le().is_err());
        assert!(r.octstr().is_err());
        assert!(r.string().is_err());
        assert_eq!(r.rest(), &[] as &[u8]);
    }

    #[test]
    fn a_hostile_length_prefix_is_rejected_not_trusted() {
        // Claims 200 bytes of string, supplies 3. This is the shape of frame
        // that turns an indexing implementation into a crash.
        let mut r = Reader::new(&[200, b'a', b'b', b'c']);
        assert!(matches!(
            r.octstr(),
            Err(CodecError::BadLength {
                claimed: 200,
                available: 3,
                offset: 0
            })
        ));
    }

    #[test]
    fn no_value_strings_decode_as_none() {
        let mut r = Reader::new(&[0xff]);
        assert_eq!(r.octstr().unwrap(), None);
        let mut r = Reader::new(&[0xff]);
        assert_eq!(r.string().unwrap(), None);
    }

    #[test]
    fn strings_round_trip() {
        let mut w = Writer::new();
        w.octstr(Some(b"SNZB-02D")).unwrap();
        let mut r = Reader::new(w.as_slice());
        assert_eq!(r.string().unwrap(), Some("SNZB-02D"));
        assert!(r.is_empty());
    }

    #[test]
    fn invalid_utf8_is_an_error_not_a_panic() {
        // Devices really do ship malformed manufacturerName fields.
        let mut r = Reader::new(&[0x02, 0xff, 0xfe]);
        assert!(matches!(r.string(), Err(CodecError::BadUtf8 { offset: 0 })));
    }

    #[test]
    fn ieee_uses_wire_order_and_round_trips() {
        let ieee = crate::ids::Ieee::new(0x0017_8801_00dc_4d3f);
        let mut w = Writer::new();
        w.ieee_le(ieee);
        assert_eq!(
            w.as_slice(),
            &[0x3f, 0x4d, 0xdc, 0x00, 0x01, 0x88, 0x17, 0x00]
        );
        assert_eq!(Reader::new(w.as_slice()).ieee_le().unwrap(), ieee);
    }

    #[test]
    fn u24_round_trips() {
        let mut w = Writer::new();
        w.u24_le(0x00ab_cdef);
        assert_eq!(w.as_slice(), &[0xef, 0xcd, 0xab]);
        assert_eq!(Reader::new(w.as_slice()).u24_le().unwrap(), 0x00ab_cdef);
    }

    #[test]
    fn a_255_byte_string_is_refused_because_it_collides_with_no_value() {
        let long = [b'x'; 255];
        let mut w = Writer::new();
        assert!(w.octstr(Some(&long)).is_err());
    }
}
