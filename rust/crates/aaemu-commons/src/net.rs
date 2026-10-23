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

    #[test]
    fn frame_lengths_have_exact_wire_bytes() {
        for (len, header) in [(0, [0, 0]), (258, [2, 1]), (65_535, [0xff, 0xff])] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut c = FrameCodec;
            let mut buf = BytesMut::new();
            c.encode(&payload, &mut buf).unwrap();
            assert_eq!(&buf[..2], &header);
            assert_eq!(&buf[2..], payload);
            assert_eq!(c.decode(&mut buf).unwrap(), Some(payload));
            assert!(buf.is_empty());
            assert_eq!(c.decode(&mut buf).unwrap(), None);
        }
    }

    #[test]
    fn decodes_at_every_fragmentation_point() {
        let payload: Vec<u8> = (0..258).map(|i| (i % 251) as u8).collect();
        let mut wire = vec![2, 1];
        wire.extend_from_slice(&payload);
        for split in 0..=wire.len() {
            let mut c = FrameCodec;
            let mut buf = BytesMut::from(&wire[..split]);
            if split < wire.len() {
                for _ in 0..2 {
                    assert_eq!(c.decode(&mut buf).unwrap(), None, "split {split}");
                    assert_eq!(&buf[..], &wire[..split], "split {split}");
                }
            }
            buf.extend_from_slice(&wire[split..]);
            assert_eq!(c.decode(&mut buf).unwrap(), Some(payload.clone()));
            assert!(buf.is_empty());
            assert_eq!(c.decode(&mut buf).unwrap(), None);
        }
    }

    #[test]
    fn coalesced_frames_preserve_incomplete_trailing_frame() {
        let cases: &[(&[u8], &[u8], &[u8])] = &[
            (&[3], &[0, 9, 8, 7], &[9, 8, 7]),
            (&[3, 0, 9], &[8, 7], &[9, 8, 7]),
        ];
        for &(trailing, rest, payload) in cases {
            let mut c = FrameCodec;
            let mut buf = BytesMut::from(&[0, 0, 2, 0, 0x10, 0x20][..]);
            buf.extend_from_slice(trailing);
            assert_eq!(c.decode(&mut buf).unwrap(), Some(vec![]));
            assert_eq!(c.decode(&mut buf).unwrap(), Some(vec![0x10, 0x20]));
            assert_eq!(c.decode(&mut buf).unwrap(), None);
            assert_eq!(&buf[..], trailing);
            buf.extend_from_slice(rest);
            assert_eq!(c.decode(&mut buf).unwrap(), Some(payload.to_vec()));
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn decode_eof_rejects_every_truncated_frame() {
        let wire = [3, 0, 1, 2, 3];
        for end in 1..wire.len() {
            let mut c = FrameCodec;
            let mut buf = BytesMut::from(&wire[..end]);
            let err = c.decode_eof(&mut buf).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::Other);
            assert_eq!(&buf[..], &wire[..end]);
        }
    }

    #[test]
    fn decode_eof_drains_complete_frames_before_truncated_tail() {
        let mut c = FrameCodec;
        let mut buf = BytesMut::from(&[0, 0, 1, 0, 9, 2][..]);
        assert_eq!(c.decode_eof(&mut buf).unwrap(), Some(vec![]));
        assert_eq!(c.decode_eof(&mut buf).unwrap(), Some(vec![9]));
        assert!(c.decode_eof(&mut buf).is_err());
        assert_eq!(&buf[..], &[2]);
    }

    #[test]
    fn decode_eof_accepts_complete_frame_and_empty_buffer() {
        let mut c = FrameCodec;
        let mut buf = BytesMut::new();
        assert_eq!(c.decode_eof(&mut buf).unwrap(), None);
        buf.extend_from_slice(&[1, 0, 9]);
        assert_eq!(c.decode_eof(&mut buf).unwrap(), Some(vec![9]));
        assert!(buf.is_empty());
        assert_eq!(c.decode_eof(&mut buf).unwrap(), None);
    }

    #[test]
    fn oversized_encode_preserves_destination() {
        let mut c = FrameCodec;
        let mut buf = BytesMut::from(&[1, 0, 9][..]);
        let before = buf.clone();
        let payload = vec![0; usize::from(u16::MAX) + 1];
        let err = c.encode(&payload, &mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(buf, before);
        c.encode([8], &mut buf).unwrap();
        assert_eq!(&buf[..], &[1, 0, 9, 1, 0, 8]);
        assert_eq!(c.decode(&mut buf).unwrap(), Some(vec![9]));
        assert_eq!(c.decode(&mut buf).unwrap(), Some(vec![8]));
        assert!(buf.is_empty());
    }
}
