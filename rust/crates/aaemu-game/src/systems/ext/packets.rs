//! The two extension packets and their body codec.
//!
//! Both directions share one body (little-endian):
//!
//! ```text
//! u8   proto        = 1
//! u16  channel_len  (1..=64)
//! u8[] channel      ASCII, [a-z0-9_.]
//! u32  payload_len  (0..=32768)
//! u8[] payload      UTF-8 JSON, may be empty
//! ```
//!
//! Malformed inbound bodies never fail the connection: [`CSAAEmuExtPacket::read`] keeps the parse
//! result and the handler logs a warning and drops the message.

use aaemu_commons::PacketStream;
use futures::future::BoxFuture;

use crate::error::Result;
use crate::net::connection::GameConnection;
use crate::net::packet::{ServerPacket, LEVEL_PLAIN};
use crate::net::registry::ClientPacket;
use crate::server::GameServer;

use super::{MAX_CHANNEL_LEN, MAX_PAYLOAD_LEN, PROTO};

/// A decoded extension message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtMessage {
    pub proto: u8,
    pub channel: String,
    pub payload: Vec<u8>,
}

/// Why a body was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtParseError {
    Truncated,
    UnsupportedProto(u8),
    BadChannel,
    PayloadTooLarge(usize),
    TrailingBytes(usize),
}

impl std::fmt::Display for ExtParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated body"),
            Self::UnsupportedProto(p) => write!(f, "unsupported proto {p}"),
            Self::BadChannel => f.write_str("invalid channel name"),
            Self::PayloadTooLarge(n) => write!(f, "payload of {n} bytes exceeds {MAX_PAYLOAD_LEN}"),
            Self::TrailingBytes(n) => write!(f, "{n} trailing bytes"),
        }
    }
}

/// `1..=64` characters of `[a-z0-9_.]`.
pub fn valid_channel(channel: &str) -> bool {
    !channel.is_empty()
        && channel.len() <= MAX_CHANNEL_LEN
        && channel
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// Decodes a body (everything after the opcode).
pub fn parse_body(body: &[u8]) -> std::result::Result<ExtMessage, ExtParseError> {
    use ExtParseError::*;
    let (&proto, rest) = body.split_first().ok_or(Truncated)?;
    if proto != PROTO {
        return Err(UnsupportedProto(proto));
    }
    let (len, rest) = rest.split_first_chunk::<2>().ok_or(Truncated)?;
    let channel_len = usize::from(u16::from_le_bytes(*len));
    if channel_len == 0 || channel_len > MAX_CHANNEL_LEN {
        return Err(BadChannel);
    }
    if rest.len() < channel_len {
        return Err(Truncated);
    }
    let (channel, rest) = rest.split_at(channel_len);
    let channel = std::str::from_utf8(channel).map_err(|_| BadChannel)?;
    if !valid_channel(channel) {
        return Err(BadChannel);
    }
    let (len, rest) = rest.split_first_chunk::<4>().ok_or(Truncated)?;
    let payload_len = u32::from_le_bytes(*len) as usize;
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(PayloadTooLarge(payload_len));
    }
    if rest.len() < payload_len {
        return Err(Truncated);
    }
    if rest.len() > payload_len {
        return Err(TrailingBytes(rest.len() - payload_len));
    }
    Ok(ExtMessage {
        proto,
        channel: channel.to_string(),
        payload: rest.to_vec(),
    })
}

/// Encodes a body, validating the channel and payload size.
pub fn encode_body(channel: &str, payload: &[u8]) -> std::result::Result<Vec<u8>, ExtParseError> {
    if !valid_channel(channel) {
        return Err(ExtParseError::BadChannel);
    }
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(ExtParseError::PayloadTooLarge(payload.len()));
    }
    let mut s = PacketStream::new();
    s.write_u8(PROTO)
        .write_u16(channel.len() as u16)
        .write_raw(channel.as_bytes())
        .write_u32(payload.len() as u32)
        .write_raw(payload);
    Ok(s.into_bytes())
}

/// `CSAAEmuExtPacket` (client to game, level 1, [`super::CS_AAEMU_EXT`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSAAEmuExtPacket {
    pub msg: std::result::Result<ExtMessage, ExtParseError>,
}

impl ClientPacket for CSAAEmuExtPacket {
    const NAME: &'static str = "CSAAEmuExtPacket";

    fn read(stream: &mut PacketStream) -> Result<Self> {
        // Never an error: a malformed message is dropped by the handler, it must not close the
        // connection of a client that merely runs a different driver build.
        Ok(Self {
            msg: parse_body(stream.unread()),
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.msg {
                Ok(msg) => super::dispatch(server, conn, msg),
                Err(e) => {
                    tracing::warn!("session {}: dropping malformed ext message: {e}", conn.id)
                }
            }
            Ok(())
        })
    }
}

/// `SCAAEmuExtPacket`: same body as the client packet; the opcode is the one the connection
/// negotiated in `ext.hello` (so it is a value, not an associated constant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCAAEmuExtPacket {
    pub opcode: u16,
    pub channel: String,
    pub payload: Vec<u8>,
}

impl ServerPacket for SCAAEmuExtPacket {
    fn opcode(&self) -> u16 {
        self.opcode
    }
    fn level(&self) -> u8 {
        LEVEL_PLAIN
    }
    fn name(&self) -> &'static str {
        "SCAAEmuExtPacket"
    }
    fn write_body(&self, stream: &mut PacketStream) {
        // `super::send` validates before building the packet; an invalid value written
        // anyway produces a body the receiver drops.
        stream
            .write_u8(PROTO)
            .write_u16(self.channel.len() as u16)
            .write_raw(self.channel.as_bytes())
            .write_u32(self.payload.len() as u32)
            .write_raw(&self.payload);
    }
}
