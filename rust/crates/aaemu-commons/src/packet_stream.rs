//! Port of `AAEmu.Commons.Network.PacketStream`.
//!
//! All multi-byte values are little-endian, matching the client. Unlike the C# version, which
//! logs and returns a default value on an out-of-bounds read, reads here return
//! [`MarshalError::EndOfStream`] so callers can decide how to recover.

use std::fmt;

use chrono::{DateTime, TimeZone, Utc};

use crate::helpers;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MarshalError {
    #[error("attempted to read {wanted} bytes at {pos}, but only {len} are available")]
    EndOfStream {
        pos: usize,
        wanted: usize,
        len: usize,
    },
    #[error("invalid value: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, MarshalError>;

/// Types that can be read from / written to a [`PacketStream`]
/// (the C# `PacketMarshaler`).
pub trait Marshal {
    fn write(&self, stream: &mut PacketStream);
}

pub trait Unmarshal: Sized {
    fn read(stream: &mut PacketStream) -> Result<Self>;
}

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct PacketStream {
    buf: Vec<u8>,
    pos: usize,
}

impl PacketStream {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(128),
            pos: 0,
        }
    }

    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            buf: bytes.into(),
            pos: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.buf.len());
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn unread(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.pos = 0;
    }

    pub fn rollback(&mut self) {
        self.pos = 0;
    }

    pub fn rollback_by(&mut self, len: usize) {
        self.pos = self.pos.saturating_sub(len);
    }

    pub fn insert(&mut self, offset: usize, bytes: &[u8]) {
        self.buf.splice(offset..offset, bytes.iter().copied());
    }

    /// Overwrites bytes at `offset` (used to patch length/sequence fields after encoding).
    pub fn patch(&mut self, offset: usize, bytes: &[u8]) {
        self.buf[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    // ---- reads -------------------------------------------------------------------------------

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if n > self.remaining() {
            return Err(MarshalError::EndOfStream {
                pos: self.pos,
                wanted: n,
                len: self.buf.len(),
            });
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? == 1)
    }
    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(self.read_u8()? as i8)
    }
    pub fn read_i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.take_array()?))
    }
    pub fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take_array()?))
    }
    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take_array()?))
    }
    pub fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take_array()?))
    }
    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take_array()?))
    }
    pub fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take_array()?))
    }
    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take_array()?))
    }
    pub fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take_array()?))
    }

    /// 24-bit unsigned integer ("BC" — used for object ids).
    pub fn read_bc(&mut self) -> Result<u32> {
        let b = self.take_array::<3>()?;
        Ok(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
    }

    pub fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>> {
        Ok(self.take(count)?.to_vec())
    }

    /// Reads a byte array prefixed by an `i16` length.
    pub fn read_bytes_sized(&mut self) -> Result<Vec<u8>> {
        let n = self.read_i16()?.max(0) as usize;
        self.read_bytes(n)
    }

    /// Reads a nested stream prefixed by an `i16` length.
    pub fn read_packet_stream(&mut self) -> Result<PacketStream> {
        Ok(PacketStream::from_bytes(self.read_bytes_sized()?))
    }

    /// Reads a UTF-8 string prefixed by an `i16` length, trimming NULs.
    pub fn read_string(&mut self) -> Result<String> {
        let bytes = self.read_bytes_sized()?;
        Ok(decode_string(&bytes))
    }

    /// Reads a fixed-length UTF-8 string, trimming NULs.
    pub fn read_string_fixed(&mut self, len: usize) -> Result<String> {
        let bytes = self.take(len)?;
        Ok(decode_string(bytes))
    }

    pub fn read_date_time(&mut self) -> Result<DateTime<Utc>> {
        let secs = self.read_i64()?;
        Ok(Utc
            .timestamp_opt(secs, 0)
            .single()
            .unwrap_or(DateTime::<Utc>::MIN_UTC))
    }

    pub fn read<T: Unmarshal>(&mut self) -> Result<T> {
        T::read(self)
    }

    /// Reads an `i32`-counted collection.
    pub fn read_collection<T: Unmarshal>(&mut self) -> Result<Vec<T>> {
        let n = self.read_i32()?.max(0) as usize;
        (0..n).map(|_| T::read(self)).collect()
    }

    /// Packed Integer Size Compression: a header byte with two bits per value
    /// (00 = u8, 01 = u16, 10 = bc, 11 = u32), up to 4 values.
    fn read_pisc(&mut self, count: usize) -> Result<Vec<i64>> {
        let header = self.read_u8()?;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let lo = header >> (i * 2) & 1 != 0;
            let hi = header >> (i * 2 + 1) & 1 != 0;
            out.push(match (lo, hi) {
                (true, true) => i64::from(self.read_u32()?),
                (false, true) => i64::from(self.read_bc()?),
                (true, false) => i64::from(self.read_u16()?),
                (false, false) => i64::from(self.read_u8()?),
            });
        }
        Ok(out)
    }

    pub fn read_pisc_w(&mut self, count: usize) -> Result<Vec<i64>> {
        let mut out = Vec::with_capacity(count);
        let mut left = count;
        while left > 0 {
            let n = left.min(4);
            out.extend(self.read_pisc(n)?);
            left -= n;
        }
        Ok(out)
    }

    pub fn read_position(&mut self) -> Result<(f32, f32, f32)> {
        let b = self.take_array::<9>()?;
        Ok(helpers::decode_position(&b))
    }

    pub fn read_vector3_f32(&mut self) -> Result<[f32; 3]> {
        Ok([self.read_f32()?, self.read_f32()?, self.read_f32()?])
    }

    pub fn read_vector3_short(&mut self) -> Result<[f32; 3]> {
        Ok([
            f32::from(self.read_i16()?) * SHORT_SCALE,
            f32::from(self.read_i16()?) * SHORT_SCALE,
            f32::from(self.read_i16()?) * SHORT_SCALE,
        ])
    }

    /// Reads x/y/z as shorts; w is reconstructed from the unit-length constraint.
    pub fn read_quaternion_short(&mut self) -> Result<[f32; 4]> {
        let [x, y, z] = self.read_vector3_short()?;
        let norm = x * x + y * y + z * z;
        let w = if norm < 0.9975 {
            (1.0 - f64::from(norm)).sqrt() as f32
        } else {
            0.0
        };
        Ok([x, y, z, w])
    }

    // ---- writes ------------------------------------------------------------------------------

    pub fn write_bool(&mut self, v: bool) -> &mut Self {
        self.write_u8(u8::from(v))
    }
    pub fn write_u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn write_i8(&mut self, v: i8) -> &mut Self {
        self.write_u8(v as u8)
    }
    pub fn write_i16(&mut self, v: i16) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_u16(&mut self, v: u16) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_i32(&mut self, v: i32) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_u32(&mut self, v: u32) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_i64(&mut self, v: i64) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_u64(&mut self, v: u64) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_f32(&mut self, v: f32) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_f64(&mut self, v: f64) -> &mut Self {
        self.write_raw(&v.to_le_bytes())
    }
    pub fn write_bc(&mut self, v: u32) -> &mut Self {
        self.write_raw(&v.to_le_bytes()[..3])
    }

    /// Appends raw bytes with no length prefix.
    pub fn write_raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Appends bytes prefixed by a `u16` length.
    pub fn write_bytes_sized(&mut self, bytes: &[u8]) -> &mut Self {
        self.write_u16(bytes.len() as u16).write_raw(bytes)
    }

    /// Writes a `u16`-length-prefixed UTF-8 string.
    pub fn write_str(&mut self, s: &str) -> &mut Self {
        self.write_bytes_sized(s.as_bytes())
    }

    /// Writes a `u16`-length-prefixed UTF-8 string including a trailing NUL.
    pub fn write_str_nul(&mut self, s: &str) -> &mut Self {
        self.write_u16(s.len() as u16 + 1)
            .write_raw(s.as_bytes())
            .write_u8(0)
    }

    /// Writes a string as raw bytes padded/truncated to `len`.
    pub fn write_str_fixed(&mut self, s: &str, len: usize) -> &mut Self {
        let mut b = s.as_bytes().to_vec();
        b.resize(len, 0);
        self.write_raw(&b)
    }

    pub fn write_date_time(&mut self, t: DateTime<Utc>) -> &mut Self {
        self.write_i64(t.timestamp().max(0))
    }

    pub fn write<T: Marshal + ?Sized>(&mut self, v: &T) -> &mut Self {
        v.write(self);
        self
    }

    /// Writes an `i32`-counted collection.
    pub fn write_collection<T: Marshal>(&mut self, items: &[T]) -> &mut Self {
        self.write_i32(items.len() as i32);
        for it in items {
            it.write(self);
        }
        self
    }

    pub fn write_pisc(&mut self, values: &[i64]) -> &mut Self {
        debug_assert!(values.len() <= 4);
        let mut header = 0u8;
        let mut body = PacketStream::new();
        for (i, &v) in values.iter().enumerate() {
            if v <= 0xFF {
                body.write_u8(v as u8);
            } else if v <= 0xFFFF {
                header |= 1 << (i * 2);
                body.write_u16(v as u16);
            } else if v <= 0xFF_FFFF {
                header |= 1 << (i * 2 + 1);
                body.write_bc(v as u32);
            } else {
                header |= 3 << (i * 2);
                body.write_u32(v as u32);
            }
        }
        self.write_u8(header).write_raw(&body.buf)
    }

    pub fn write_pisc_w(&mut self, values: &[i64]) -> &mut Self {
        for chunk in values.chunks(4) {
            self.write_pisc(chunk);
        }
        self
    }

    pub fn write_position(&mut self, x: f32, y: f32, z: f32) -> &mut Self {
        self.write_raw(&helpers::encode_position(x, y, z))
    }

    pub fn write_vector3_f32(&mut self, v: [f32; 3]) -> &mut Self {
        self.write_f32(v[0]).write_f32(v[1]).write_f32(v[2])
    }

    pub fn write_vector3_short(&mut self, v: [f32; 3]) -> &mut Self {
        for c in v {
            self.write_i16(to_short(c));
        }
        self
    }

    pub fn write_quaternion_short(&mut self, q: [f32; 4], scalar: bool) -> &mut Self {
        self.write_vector3_short([q[0], q[1], q[2]]);
        if scalar {
            self.write_i16(q[3].round() as i16);
        }
        self
    }
}

/// Same literal as the C# (`1 / 32767`); kept verbatim so values match bit for bit.
#[allow(clippy::excessive_precision)]
pub const SHORT_SCALE: f32 = 0.000_030_518_509;

/// Matches C# `Convert.ToInt16(v * 32767f)` (banker's rounding, saturating).
fn to_short(v: f32) -> i16 {
    let x = f64::from(v * 32767.0);
    let r = x.round();
    let r = if (x - x.trunc()).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        r
    };
    r.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

fn decode_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches('\0')
        .to_string()
}

impl fmt::Debug for PacketStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PacketStream(pos={}, ", self.pos)?;
        for (i, b) in self.buf.iter().enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            write!(f, "{b:02X}")?;
        }
        f.write_str(")")
    }
}

impl From<Vec<u8>> for PacketStream {
    fn from(v: Vec<u8>) -> Self {
        Self::from_bytes(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Reader = fn(&mut PacketStream) -> Result<()>;

    #[test]
    fn primitives_roundtrip() {
        let mut s = PacketStream::new();
        s.write_u8(1)
            .write_u16(0x1234)
            .write_u32(0xDEADBEEF)
            .write_u64(7)
            .write_bc(0x00AB_CDEF);
        s.write_str("hello").write_f32(1.5).write_bool(true);
        assert_eq!(&s.as_bytes()[1..3], &[0x34, 0x12]);
        assert_eq!(s.read_u8().unwrap(), 1);
        assert_eq!(s.read_u16().unwrap(), 0x1234);
        assert_eq!(s.read_u32().unwrap(), 0xDEADBEEF);
        assert_eq!(s.read_u64().unwrap(), 7);
        assert_eq!(s.read_bc().unwrap(), 0x00AB_CDEF);
        assert_eq!(s.read_string().unwrap(), "hello");
        assert_eq!(s.read_f32().unwrap(), 1.5);
        assert!(s.read_bool().unwrap());
        assert!(matches!(s.read_u8(), Err(MarshalError::EndOfStream { .. })));
    }

    #[test]
    fn oversized_byte_read_preserves_cursor() {
        let mut s = PacketStream::from_bytes([0x11, 0x22, 0x33]);
        assert_eq!(s.read_u8().unwrap(), 0x11);
        assert_eq!(
            s.read_bytes(usize::MAX),
            Err(MarshalError::EndOfStream {
                pos: 1,
                wanted: usize::MAX,
                len: 3,
            })
        );
        assert_eq!(s.pos(), 1);
        assert_eq!(s.unread(), &[0x22, 0x33]);
        assert_eq!(s.read_u16().unwrap(), 0x3322);
    }

    #[test]
    fn oversized_fixed_string_read_preserves_cursor() {
        let mut s = PacketStream::from_bytes(b"abc".as_slice());
        assert_eq!(s.read_u8().unwrap(), b'a');
        assert_eq!(
            s.read_string_fixed(usize::MAX),
            Err(MarshalError::EndOfStream {
                pos: 1,
                wanted: usize::MAX,
                len: 3,
            })
        );
        assert_eq!(s.pos(), 1);
        assert_eq!(s.unread(), b"bc");
        assert_eq!(s.read_string_fixed(2).unwrap(), "bc");
    }

    #[test]
    fn truncated_fixed_width_reads_preserve_cursor_and_can_retry() {
        let readers: &[(usize, Reader)] = &[
            (1, |s| s.read_bool().map(|_| ())),
            (1, |s| s.read_u8().map(|_| ())),
            (1, |s| s.read_i8().map(|_| ())),
            (2, |s| s.read_i16().map(|_| ())),
            (2, |s| s.read_u16().map(|_| ())),
            (3, |s| s.read_bc().map(|_| ())),
            (3, |s| s.read_string_fixed(3).map(|_| ())),
            (4, |s| s.read_i32().map(|_| ())),
            (4, |s| s.read_u32().map(|_| ())),
            (4, |s| s.read_f32().map(|_| ())),
            (8, |s| s.read_i64().map(|_| ())),
            (8, |s| s.read_u64().map(|_| ())),
            (8, |s| s.read_f64().map(|_| ())),
            (9, |s| s.read_position().map(|_| ())),
        ];
        for &(width, read) in readers {
            for available in 0..width {
                let mut s = PacketStream::from_bytes(vec![0; 1 + available]);
                s.read_u8().unwrap();
                assert_eq!(
                    read(&mut s),
                    Err(MarshalError::EndOfStream {
                        pos: 1,
                        wanted: width,
                        len: 1 + available,
                    })
                );
                assert_eq!(s.pos(), 1);
                assert_eq!(s.unread(), vec![0; available]);
                s.write_raw(&vec![0; width - available]);
                read(&mut s).unwrap();
                assert_eq!(s.pos(), 1 + width);
                assert_eq!(s.remaining(), 0);
            }
        }
    }

    #[test]
    fn cursor_clamping_rollback_and_empty_reads() {
        let mut s = PacketStream::from_bytes([0x11, 0x22]);
        s.set_pos(usize::MAX);
        assert_eq!(s.pos(), 2);
        assert_eq!(s.remaining(), 0);
        assert!(s.read_bytes(0).unwrap().is_empty());
        assert_eq!(s.read_string_fixed(0).unwrap(), "");
        assert!(matches!(s.read_u8(), Err(MarshalError::EndOfStream { .. })));
        assert_eq!(s.pos(), 2);
        s.rollback_by(1);
        assert_eq!(s.read_u8().unwrap(), 0x22);
        s.rollback_by(usize::MAX);
        assert_eq!(s.pos(), 0);
        assert_eq!(s.read_u8().unwrap(), 0x11);
        s.rollback();
        assert_eq!(s.read_u16().unwrap(), 0x2211);
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.pos(), 0);
        assert!(s.unread().is_empty());
        s.write_u8(0x33);
        assert_eq!(s.read_u8().unwrap(), 0x33);
    }

    #[test]
    fn truncated_length_prefixed_reads_consume_only_complete_fields() {
        let readers: [Reader; 3] = [
            |s| s.read_bytes_sized().map(|_| ()),
            |s| s.read_string().map(|_| ()),
            |s| s.read_packet_stream().map(|_| ()),
        ];
        let wire = [3, 0, b'a', b'b', b'c'];
        for read in readers {
            for end in 0..wire.len() {
                let mut s = PacketStream::from_bytes(&wire[..end]);
                let (pos, wanted) = if end < 2 { (0, 2) } else { (2, 3) };
                assert_eq!(
                    read(&mut s),
                    Err(MarshalError::EndOfStream {
                        pos,
                        wanted,
                        len: end,
                    })
                );
                assert_eq!(s.pos(), pos);
                assert_eq!(s.unread(), &wire[pos..end]);
                s.write_raw(&wire[end..]);
                s.rollback();
                read(&mut s).unwrap();
                assert_eq!(s.pos(), wire.len());
            }
        }
    }

    #[test]
    fn nonpositive_i16_lengths_leave_following_bytes_unread() {
        // Preserve the signed length/C# behavior, including the high-bit boundary.
        for prefix in [[0, 0], [0, 0x80], [0xff, 0xff]] {
            let wire = [prefix[0], prefix[1], 0x42];
            let mut s = PacketStream::from_bytes(wire);
            assert!(s.read_bytes_sized().unwrap().is_empty());
            assert_eq!(s.pos(), 2);
            assert_eq!(s.read_u8().unwrap(), 0x42);

            let mut s = PacketStream::from_bytes(wire);
            assert_eq!(s.read_string().unwrap(), "");
            assert_eq!(s.pos(), 2);
            assert_eq!(s.read_u8().unwrap(), 0x42);

            let mut s = PacketStream::from_bytes(wire);
            let nested = s.read_packet_stream().unwrap();
            assert!(nested.is_empty());
            assert_eq!(nested.pos(), 0);
            assert_eq!(s.pos(), 2);
            assert_eq!(s.read_u8().unwrap(), 0x42);
        }
    }

    #[test]
    fn maximum_positive_i16_length_reads_entire_payload() {
        let payload = vec![0x42; i16::MAX as usize];
        let mut s = PacketStream::new();
        s.write_bytes_sized(&payload).write_u8(0x99);
        assert_eq!(&s.as_bytes()[..2], &[0xff, 0x7f]);
        assert_eq!(s.read_bytes_sized().unwrap(), payload);
        assert_eq!(s.read_u8().unwrap(), 0x99);
    }

    #[test]
    fn strings_replace_invalid_utf8_and_trim_only_edge_nuls() {
        let body = [0, b'a', 0xff, 0, b'b', 0];
        let mut s = PacketStream::new();
        s.write_bytes_sized(&body).write_u8(0x42);
        assert_eq!(s.read_string().unwrap(), "a\u{fffd}\0b");
        assert_eq!(s.read_u8().unwrap(), 0x42);

        let mut s = PacketStream::from_bytes(body);
        assert_eq!(s.read_string_fixed(body.len()).unwrap(), "a\u{fffd}\0b");
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn pisc_width_boundaries_have_exact_wire_bytes() {
        let cases: &[(i64, &[u8])] = &[
            (0, &[0, 0]),
            (0xff, &[0, 0xff]),
            (0x100, &[1, 0, 1]),
            (0xffff, &[1, 0xff, 0xff]),
            (0x1_0000, &[2, 0, 0, 1]),
            (0xff_ffff, &[2, 0xff, 0xff, 0xff]),
            (0x100_0000, &[3, 0, 0, 0, 1]),
            (0xffff_ffff, &[3, 0xff, 0xff, 0xff, 0xff]),
        ];
        for &(value, wire) in cases {
            let mut s = PacketStream::new();
            s.write_pisc(&[value]);
            assert_eq!(s.as_bytes(), wire, "value {value:#x}");
            let mut s = PacketStream::from_bytes(wire);
            assert_eq!(s.read_pisc_w(1).unwrap(), [value]);
            assert_eq!(s.remaining(), 0);
        }
    }

    #[test]
    fn pisc_groups_have_exact_headers_and_partial_final_group() {
        let values = [
            0xff,
            0x100,
            0x1_0000,
            0x100_0000,
            0xffff,
            0xff_ffff,
            0xffff_ffff,
        ];
        let wire = [
            0xe4, 0xff, 0, 1, 0, 0, 1, 0, 0, 0, 1, // u8, u16, bc, u32
            0x39, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // u16, bc, u32
        ];
        let mut s = PacketStream::new();
        s.write_pisc_w(&values);
        assert_eq!(s.as_bytes(), wire);
        let mut s = PacketStream::from_bytes(wire);
        assert_eq!(s.read_pisc_w(values.len()).unwrap(), values);
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn truncated_pisc_groups_stop_before_incomplete_value() {
        let wire = [
            0xe4, 0xff, 0, 1, 0, 0, 1, 0, 0, 0, 1, // u8, u16, bc, u32
            0x39, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // u16, bc, u32
        ];
        // Header/value offsets and widths, including the second group's header.
        for (pos, wanted) in [
            (0, 1),
            (1, 1),
            (2, 2),
            (4, 3),
            (7, 4),
            (11, 1),
            (12, 2),
            (14, 3),
            (17, 4),
        ] {
            for end in pos..pos + wanted {
                let mut s = PacketStream::from_bytes(&wire[..end]);
                assert_eq!(
                    s.read_pisc_w(7),
                    Err(MarshalError::EndOfStream {
                        pos,
                        wanted,
                        len: end
                    })
                );
                assert_eq!(s.pos(), pos);
                assert_eq!(s.unread(), &wire[pos..end]);
                assert_eq!(s.read_bytes(end - pos).unwrap(), wire[pos..end]);
            }
        }
    }

    #[test]
    fn empty_pisc_groups_do_not_consume_or_write_bytes() {
        let mut s = PacketStream::from_bytes([0x42]);
        s.write_pisc_w(&[]);
        assert_eq!(s.as_bytes(), &[0x42]);
        assert!(s.read_pisc_w(0).unwrap().is_empty());
        assert_eq!(s.pos(), 0);
        assert_eq!(s.read_u8().unwrap(), 0x42);
    }

    #[test]
    fn pisc_roundtrip() {
        let vals = [5, 300, 70_000, 20_000_000, 1];
        let mut s = PacketStream::new();
        s.write_pisc_w(&vals);
        assert_eq!(s.read_pisc_w(vals.len()).unwrap(), vals);
    }

    #[test]
    fn position_roundtrip() {
        let mut s = PacketStream::new();
        s.write_position(12345.5, 6789.25, 123.0);
        let (x, y, z) = s.read_position().unwrap();
        assert!((x - 12345.5).abs() < 0.01);
        assert!((y - 6789.25).abs() < 0.01);
        assert!((z - 123.0).abs() < 0.01);
    }
}
