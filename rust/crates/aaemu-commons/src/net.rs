//! TCP framing shared by every AAEmu protocol (replaces NetCoreServer + the hand-rolled
//! reassembly in each `*ProtocolHandler.OnReceive`).
//!
//! Every frame on the wire is `[u16 LE length][payload]`, where `length` counts the payload only.

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// Splits a byte stream into length-prefixed frames. Yields the payload with the length removed.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Vec<u8>;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 2 {
            return Ok(None);
        }
        let len = usize::from(u16::from_le_bytes([src[0], src[1]]));
        if src.len() < 2 + len {
            src.reserve(2 + len - src.len());
            return Ok(None);
        }
        src.advance(2);
        Ok(Some(src.split_to(len).to_vec()))
    }
}

impl<T: AsRef<[u8]>> Encoder<T> for FrameCodec {
    type Error = std::io::Error;

    /// Prepends the `u16` length to `payload` (a `Vec<u8>`, or a shared `Bytes` frame).
    fn encode(&mut self, payload: T, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let payload = payload.as_ref();
        let len = u16::try_from(payload.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "frame exceeds 65535 bytes",
            )
        })?;
        dst.reserve(2 + payload.len());
        dst.put_u16_le(len);
        dst.extend_from_slice(payload);
        Ok(())
    }
}

/// Most bytes of a packet body that [`hex_dump`] prints.
pub const MAX_DUMP_BYTES: usize = 64;

/// Hex dump (`"0a 0b "`, a trailing space per byte like the C# `Helpers`) of the first
/// [`MAX_DUMP_BYTES`] bytes, followed by the total length when the body was longer. Used for the
/// "unknown packet" log lines so a peer cannot flood the log with arbitrary-size dumps.
pub fn hex_dump(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let shown = &bytes[..bytes.len().min(MAX_DUMP_BYTES)];
    let mut out = String::with_capacity(shown.len() * 3 + 24);
    for b in shown {
        let _ = write!(out, "{b:02x} ");
    }
    if bytes.len() > shown.len() {
        let _ = write!(out, "... ({} bytes total)", bytes.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump_is_capped() {
        assert_eq!(hex_dump(&[]), "");
        assert_eq!(hex_dump(&[1, 0xab]), "01 ab ");
        let exact = vec![0u8; MAX_DUMP_BYTES];
        assert_eq!(hex_dump(&exact).len(), MAX_DUMP_BYTES * 3);
        let long = vec![0xffu8; 5000];
        let dump = hex_dump(&long);
        assert_eq!(dump.matches("ff ").count(), MAX_DUMP_BYTES);
        assert!(dump.ends_with("... (5000 bytes total)"));
    }

    #[test]
    fn decodes_split_frames() {
        let mut c = FrameCodec;
        let mut buf = BytesMut::from(&[3u8, 0, 1, 2][..]);
        assert_eq!(c.decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(&[3, 1, 0, 9]);
        assert_eq!(c.decode(&mut buf).unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(c.decode(&mut buf).unwrap(), Some(vec![9]));
        assert_eq!(c.decode(&mut buf).unwrap(), None);
    }
}
