//! r223748 wire adapter. Internal packet types and game data remain r208022.
//! The maps and layout changes were recovered from both client builds and verified
//! against login, world entry, movement, chat, mining and combat captures.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::sync::LazyLock;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Sink;

static SC: LazyLock<HashMap<u16, u16>> =
    LazyLock::new(|| read_map(include_str!("r223748/sc-map.tsv")));
static CS: LazyLock<HashMap<u16, u16>> =
    LazyLock::new(|| read_map(include_str!("r223748/cs-map.tsv")));

fn read_map(text: &str) -> HashMap<u16, u16> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let (from, to) = line.split_once('\t').expect("checked-in map row");
        let from = u16::from_str_radix(from, 16).expect("checked-in source opcode");
        let to = u16::from_str_radix(to, 16).expect("checked-in target opcode");
        assert!(map.insert(from, to).is_none(), "duplicate source opcode");
    }
    map
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or_else(|| invalid("length overflow"))?;
        let bytes = self
            .data
            .get(self.at..end)
            .ok_or_else(|| invalid("truncated packet"))?;
        self.at = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("two bytes"),
        ))
    }
    fn string(&mut self) -> io::Result<Vec<u8>> {
        let size = self.u16()? as usize;
        Ok(self.take(size)?.to_vec())
    }
    fn variant(&mut self, sizes: &[usize]) -> io::Result<()> {
        let kind = self.byte()? as usize;
        let count = *sizes
            .get(kind)
            .ok_or_else(|| invalid("unknown packet variant"))?;
        self.take(count)?;
        Ok(())
    }
    fn caster(&mut self) -> io::Result<()> {
        self.variant(&[3, 3, 20, 7, 3])
    }
    fn plot_object(&mut self) -> io::Result<()> {
        match self.byte()? {
            1 => {
                self.take(3)?;
            }
            2 => {
                self.take(12)?;
            }
            _ => return Err(invalid("unknown plot object")),
        }
        Ok(())
    }
    fn pisc(&mut self) -> io::Result<()> {
        let flags = self.byte()?;
        self.take((0..3).map(|i| 1 + ((flags >> (i * 2)) & 3) as usize).sum())?;
        Ok(())
    }
    fn batch_body(&mut self, opcode: u16) -> io::Result<()> {
        match opcode {
            0xa7 => {
                self.variant(&[6, 14, 13, 11, 7])?;
                self.caster()?;
                self.take(7)?;
                self.pisc()?;
                self.pisc()?;
                self.take(5)?;
            }
            0xa8 => {
                self.variant(&[6, 14, 13, 11, 7])?;
                self.caster()?;
                self.take(14)?;
            }
            0xad => {
                self.take(12)?;
            }
            0xae => {
                self.take(10)?;
                self.plot_object()?;
                self.plot_object()?;
                self.take(18)?;
                let targets = self.byte()? as usize;
                self.take(targets * 3)?;
                if self.byte()? & 8 != 0 {
                    self.take(52)?;
                }
            }
            _ => return Err(invalid("unsupported compressed packet")),
        }
        Ok(())
    }
}

fn compressed(frame: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let count = u16::from_le_bytes(frame[2..4].try_into().expect("header checked"));
    let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(&frame[4..], 4 * 1024 * 1024)
        .map_err(|_| invalid("invalid or oversized compressed batch"))?;
    let mut reader = Cursor { data: &raw, at: 0 };
    let mut output = Vec::with_capacity(raw.len());
    for _ in 0..count {
        let start = reader.at;
        reader.u16()?; // hash/count
        let opcode = reader.u16()?;
        reader.batch_body(opcode)?;
        let mut item = vec![0xdd, 1];
        item.extend_from_slice(&raw[start..reader.at]);
        let translated =
            translate(&item, false)?.ok_or_else(|| invalid("unmapped batch opcode"))?;
        output.extend_from_slice(&translated[2..]);
    }
    if reader.at != raw.len() {
        return Err(invalid("trailing compressed batch data"));
    }
    let mut result = frame[..4].to_vec();
    result.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&output, 0));
    if result.len() > u16::MAX as usize {
        return Err(invalid("adapted compressed frame too large"));
    }
    Ok(Some(result))
}

/// Translate one frame, without its u16 length prefix. Unknown game IDs are withheld.
/// Proxy messages retain their IDs. `incoming` means client -> server.
pub fn translate(frame: &[u8], incoming: bool) -> io::Result<Option<Vec<u8>>> {
    if frame.len() < 2 {
        return Err(invalid("truncated frame"));
    }
    if frame[1] == 4 && !incoming {
        if frame.len() < 4 {
            return Err(invalid("truncated compressed header"));
        }
        return compressed(frame);
    }
    if frame[1] != 1 {
        return Ok(Some(frame.to_vec()));
    }
    if frame.len() < 6 {
        return Err(invalid("truncated game header"));
    }
    let op = u16::from_le_bytes([frame[4], frame[5]]);
    if op == 0 {
        return Ok(Some(frame.to_vec()));
    }
    let Some(&mapped) = (if incoming { &*CS } else { &*SC }).get(&op) else {
        tracing::warn!(opcode = op, incoming, "withholding unmapped r223748 packet");
        return Ok(None);
    };
    let mut out = frame.to_vec();
    if incoming {
        if (mapped == 0x20 && out.len() == 10) || (mapped == 0x25 && out.len() == 11) {
            out.resize(12, 0);
        }
    } else {
        match op {
            7 => {
                let mut r = Cursor { data: frame, at: 6 };
                let activate = r.byte()?;
                r.string()?; // obsolete auth URL
                let platform = r.string()?;
                let commerce = r.string()?;
                out.truncate(6);
                out.push(activate);
                for s in [platform, commerce, Vec::new(), Vec::new()] {
                    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
                    out.extend_from_slice(&s);
                }
            }
            0xd3 => {
                if out.len() < 7 {
                    return Err(invalid("truncated chat spam delay"));
                }
                out.splice(7..7, [0, 0]);
            }
            0x40 => {
                if out.len() < 99 {
                    return Err(invalid("truncated character state"));
                }
                let p = out.len() - 93;
                let mut totals = vec![0; 4];
                totals.extend_from_slice(&out[p..p + 12]);
                out.splice(p..p, totals);
            }
            0x116 => {
                if out.len() != 22 {
                    return Err(invalid("invalid unread mail counts"));
                }
                out.extend_from_slice(&frame[6..22]);
            }
            0xce => {
                if out.len() < 14 {
                    return Err(invalid("truncated chat message"));
                }
                let p = out.len() - 8;
                out.splice(p..p, [0; 20]);
            }
            _ => {}
        }
    }
    if ((incoming && mapped == 0x89) || (!incoming && op == 0x6c)) && out.len() >= 39 && out[9] == 1
    {
        let p = 38 + if out[14] & 0x10 != 0 { 5 } else { 0 };
        if p >= out.len() {
            return Err(invalid("truncated actor movement"));
        }
        if incoming {
            out.remove(p);
        } else {
            out.insert(p, 0);
        }
    }
    out[4..6].copy_from_slice(&mapped.to_le_bytes());
    if out.len() > u16::MAX as usize {
        return Err(invalid("adapted frame too large"));
    }
    Ok(Some(out))
}

/// Adapts every outgoing frame, including broadcasts and delayed/compressed batches,
/// at the socket boundary. r208022 sessions preserve the original buffers.
pub struct RevisionSink<S> {
    inner: S,
    revision: u32,
}
impl<S> RevisionSink<S> {
    pub fn new(inner: S, revision: u32) -> Self {
        Self { inner, revision }
    }
}
impl<S: Sink<Bytes, Error = io::Error> + Unpin> Sink<Bytes> for RevisionSink<S> {
    type Error = io::Error;
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_ready(cx)
    }
    fn start_send(self: Pin<&mut Self>, bytes: Bytes) -> io::Result<()> {
        let this = self.get_mut();
        if this.revision == 223748 {
            if let Some(out) = translate(&bytes, false)? {
                Pin::new(&mut this.inner).start_send(out.into())?;
            }
            Ok(())
        } else {
            Pin::new(&mut this.inner).start_send(bytes)
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        text.trim()
            .split('-')
            .map(|n| u8::from_str_radix(n, 16).unwrap())
            .collect()
    }
    fn op(frame: &[u8]) -> u16 {
        u16::from_le_bytes([frame[4], frame[5]])
    }

    #[test]
    fn complete_maps_and_distinct_character_requests() {
        assert_eq!(SC.len(), 515);
        assert_eq!(CS.len(), 244);
        for map in [&*SC, &*CS] {
            let targets: std::collections::HashSet<_> = map.values().collect();
            assert_eq!(targets.len(), map.len(), "map targets must be unique");
        }
        let check = [3, 1, 0, 0, 0x6a, 0, 4, 3, 2, 1, 1];
        let out = translate(&check, true).unwrap().unwrap();
        assert_eq!(op(&out), 0x11a);
        assert_eq!(&out[6..], &check[6..]);
        let mut select = check;
        select[4..6].copy_from_slice(&0x142u16.to_le_bytes());
        let out = translate(&select, true).unwrap().unwrap();
        assert_eq!(op(&out), 0x25);
        assert_eq!(&out[6..11], &check[6..]);
        assert_eq!(out[11], 0);
        let reply = translate(&[0xdd, 1, 0, 0, 0xce, 1, 4, 3, 2, 1, 1, 0], false)
            .unwrap()
            .unwrap();
        assert_eq!(op(&reply), 0x124);
        assert_eq!(reply.len(), 12);
    }

    #[test]
    fn captured_movement_preserves_coordinates_and_optional_fields() {
        let original = hex(include_str!("r223748/actor_movement.hex"));
        for optional in [false, true] {
            let mut old = original.clone();
            // Preserve the captured optional header, also exercise the alternate layout.
            if optional != (old[14] & 0x10 != 0) {
                if optional {
                    old.splice(15..15, [0; 5]);
                    old[14] |= 0x10;
                } else {
                    old.drain(15..20);
                    old[14] &= !0x10;
                }
            }
            let mut new = translate(&old, false).unwrap().unwrap();
            assert_eq!(op(&new), 0x1c4);
            assert_eq!(new.len(), old.len() + 1);
            let p = 38 + if optional { 5 } else { 0 };
            new[p] = 0x7f;
            new[4..6].copy_from_slice(&0x3cu16.to_le_bytes());
            let round = translate(&new, true).unwrap().unwrap();
            assert_eq!(op(&round), 0x89);
            assert_eq!(&round[6..], &old[6..]);
        }
    }

    #[test]
    fn mail_and_chat_preserve_following_fields() {
        let mut old = vec![0xdd, 1, 0, 0, 0x40, 0];
        old.extend(6u8..200);
        let new = translate(&old, false).unwrap().unwrap();
        assert_eq!(new.len(), old.len() + 16);
        assert_eq!(&new[123..], &old[107..]);
        let mut counts = vec![0xdd, 1, 0, 0, 0x16, 1];
        for n in [10i32, 11, 12, 13] {
            counts.extend_from_slice(&n.to_le_bytes());
        }
        let new = translate(&counts, false).unwrap().unwrap();
        assert_eq!(new.len(), 38);
        assert_eq!(&new[6..22], &counts[6..]);
        assert_eq!(&new[22..], &counts[6..]);
        let mut chat = vec![0xdd, 1, 0, 0, 0xce, 0];
        chat.extend_from_slice(&[9; 20]);
        chat.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let new = translate(&chat, false).unwrap().unwrap();
        assert_eq!(op(&new), 0xa2);
        assert_eq!(&new[26..46], &[0; 20]);
        assert_eq!(&new[46..], &chat[26..]);
    }

    #[test]
    fn captured_compressed_plot_and_damage_change_only_opcodes() {
        for (fixture, positions, expected) in [
            (include_str!("r223748/plot.hex"), vec![2, 47, 92], 0x1b3u16),
            (include_str!("r223748/damage.hex"), vec![2], 0x200u16),
        ] {
            let frame = hex(fixture);
            let out = translate(&frame, false).unwrap().unwrap();
            assert_eq!(&out[..4], &frame[..4]);
            let before = miniz_oxide::inflate::decompress_to_vec(&frame[4..]).unwrap();
            let after = miniz_oxide::inflate::decompress_to_vec(&out[4..]).unwrap();
            assert_eq!(before.len(), after.len());
            for &p in &positions {
                assert_eq!(&after[p..p + 2], &expected.to_le_bytes());
            }
            for i in 0..before.len() {
                if !positions.iter().any(|&p| i == p || i == p + 1) {
                    assert_eq!(before[i], after[i]);
                }
            }
        }
    }

    #[test]
    fn unsupported_and_truncated_frames_fail_closed() {
        assert!(translate(&[0xdd, 1, 0, 0, 0xff, 0xff], false)
            .unwrap()
            .is_none());
        assert!(translate(&[0xdd, 1, 0, 0, 7, 0, 1, 0xff, 0xff], false).is_err());
        assert!(translate(&[0xdd, 4, 1, 0, 0xff], false).is_err());
        assert!(translate(&[3, 1, 0], true).is_err());
        let proxy = [0xdd, 2, 1, 0, 9, 8, 7, 6];
        assert_eq!(translate(&proxy, false).unwrap().unwrap(), proxy);
    }
}
