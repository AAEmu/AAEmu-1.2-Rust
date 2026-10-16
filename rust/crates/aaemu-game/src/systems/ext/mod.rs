//! AAEmu client extension channel (`rust/docs/client/CLIENT-EXTENSIONS.md`).
//!
//! A small versioned message channel between the server and the injected `aadriver.dll` of the
//! 1.2 client: two packets that carry `(channel, JSON)` pairs, a per-connection handshake
//! (`ext.hello` / `ext.welcome`) and a channel registry that feature subsystems fill at startup.
//!
//! * [`CSAAEmuExtPacket`] / [`SCAAEmuExtPacket`] (see [`packets`]): the wire format.
//! * [`ExtState`]: what `ext.hello` announced. It lives in the connection's
//!   [`ConnectionHandle`] (shared with its [`GameConnection`], like the account id), so it dies
//!   with the connection and every sender that holds a handle (tickers, other tasks) can use it.
//! * [`registry::ExtRegistry`]: `channel -> handler`, built when the server is created
//!   ([`System::default`]); a feature adds a `register_ext(&mut ExtRegistry)` function and one
//!   call to it in [`System::default`].
//! * [`send`] / [`send_player`] / [`reply`]: no-ops (debug log) for connections without ext state.
//!
//! The extension opcodes are not 1.2 packets, so they are *not* in the generated tables of
//! `opcodes.rs`; the constants below are the only definition.

pub mod packets;
pub mod registry;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::net::connection::{ConnectionHandle, GameConnection};
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::server::GameServer;
use crate::world::PlayerRef;

pub use packets::{CSAAEmuExtPacket, ExtMessage, SCAAEmuExtPacket};
pub use registry::ExtRegistry;

/// AAEmu extension opcode, client to game. The 1.2 CS range `0x0BE`-`0x0CA` has no packet and
/// the client never sends this type, so it is free (it is not part of `opcodes::cs`).
pub const CS_AAEMU_EXT: u16 = 0x0C0;
/// Default AAEmu extension opcode, game to client: the client's SC handler slot is empty. The
/// driver may claim another empty slot and reports it as `sc_opcode` in `ext.hello`.
pub const SC_AAEMU_EXT: u16 = 0x1F2;
/// The client dispatches SC opcodes below this value only.
pub const SC_OPCODE_LIMIT: u16 = 0x218;
/// Envelope version.
pub const PROTO: u8 = 1;
/// Longest channel name.
pub const MAX_CHANNEL_LEN: usize = 64;
/// Largest payload (the game frame length is a `u16`; there is no chunking in v1).
pub const MAX_PAYLOAD_LEN: usize = 32 * 1024;

/// What a connection announced in `ext.hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtState {
    pub proto: u8,
    pub driver: String,
    /// The SC opcode this client listens on.
    pub sc_opcode: u16,
    /// The features both sides support (what `ext.welcome` listed).
    pub features: Vec<String>,
}

impl ExtState {
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

/// Managers of this subsystem (`server.systems.ext`).
#[derive(Debug)]
pub struct System {
    pub registry: ExtRegistry,
}

impl Default for System {
    fn default() -> Self {
        let mut registry = ExtRegistry::default();
        // One line per feature subsystem that owns channels.
        crate::systems::housing::rebuild::register_ext(&mut registry);
        Self { registry }
    }
}

impl System {
    /// Startup: nothing to load; the registry is complete since the server was created.
    pub async fn start(server: &Arc<GameServer>) -> anyhow::Result<()> {
        let r = &server.systems.ext.registry;
        tracing::info!(
            "Client extensions: {} channels, features {:?}",
            r.channel_count(),
            r.features()
        );
        Ok(())
    }
}

/// Registers the extension packet.
pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSAAEmuExtPacket>(LEVEL_PLAIN, CS_AAEMU_EXT);
}

/// The connection closed: lets feature subsystems drop their per-connection state.
pub fn on_disconnect(server: &GameServer, connection_id: u32) {
    for hook in server.systems.ext.registry.disconnect_hooks() {
        hook(server, connection_id);
    }
}

/// The `ext.error` payload.
fn error_payload(channel: &str, error: &str) -> Value {
    serde_json::json!({ "ok": false, "error": error, "channel": channel })
}

/// Routes one decoded inbound message.
pub(crate) fn dispatch(server: &GameServer, conn: &mut GameConnection, msg: ExtMessage) {
    let payload: Result<Value, _> = if msg.payload.is_empty() {
        Ok(Value::Object(Default::default()))
    } else {
        serde_json::from_slice(&msg.payload)
    };
    let handle = conn.handle();
    if msg.channel == "ext.hello" {
        match payload {
            Ok(v) => hello(server, conn, &v),
            Err(e) => tracing::warn!("session {}: ext.hello with bad JSON: {e}", conn.id),
        }
        return;
    }
    // Everything else needs a completed handshake: the reply opcode is unknown before it.
    if handle.ext_state().is_none() {
        tracing::debug!(
            "session {}: ext message {} before ext.hello, ignored",
            conn.id,
            msg.channel
        );
        return;
    }
    let Some(handler) = server.systems.ext.registry.handler(&msg.channel) else {
        tracing::debug!("session {}: unknown ext channel {}", conn.id, msg.channel);
        send(
            &handle,
            "ext.error",
            &error_payload(&msg.channel, "unknown_channel"),
        );
        return;
    };
    match payload {
        Ok(v) if v.is_object() => {
            if let Err(error) = handler(server, conn, v) {
                send(&handle, "ext.error", &error_payload(&msg.channel, error));
            }
        }
        _ => {
            send(
                &handle,
                "ext.error",
                &error_payload(&msg.channel, "bad_payload"),
            );
        }
    }
}

/// `ext.hello { proto, driver, sc_opcode, features }` -> `ext.welcome { proto, features }`.
fn hello(server: &GameServer, conn: &GameConnection, v: &Value) {
    let proto = v.get("proto").and_then(Value::as_u64).unwrap_or(0);
    if proto != u64::from(PROTO) {
        tracing::warn!(
            "session {}: ext.hello with unsupported proto {proto}, ignored",
            conn.id
        );
        return;
    }
    let sc_opcode = match v.get("sc_opcode") {
        None | Some(Value::Null) => SC_AAEMU_EXT,
        Some(o) => match o
            .as_u64()
            .filter(|o| (1..u64::from(SC_OPCODE_LIMIT)).contains(o))
        {
            Some(o) => o as u16,
            None => {
                tracing::warn!("session {}: ext.hello with invalid sc_opcode {o}", conn.id);
                return;
            }
        },
    };
    let requested: Vec<&str> = v
        .get("features")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let features: Vec<String> = server
        .systems
        .ext
        .registry
        .features()
        .iter()
        .filter(|f| requested.contains(f))
        .map(|f| f.to_string())
        .collect();
    let state = ExtState {
        proto: PROTO,
        driver: v
            .get("driver")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(64)
            .collect(),
        sc_opcode,
        features: features.clone(),
    };
    tracing::info!(
        "session {}: client extension driver '{}' (sc opcode {:#x}, features {:?})",
        conn.id,
        state.driver,
        state.sc_opcode,
        features
    );
    let handle = conn.handle();
    handle.set_ext_state(Some(state));
    send(
        &handle,
        "ext.welcome",
        &serde_json::json!({ "proto": PROTO, "features": features }),
    );
}

/// Sends `payload` as JSON on `channel`. Returns false (nothing sent) when the connection has no
/// ext state, the channel is invalid, serialization fails or the payload is too large.
pub fn send(conn: &ConnectionHandle, channel: &str, payload: &impl Serialize) -> bool {
    let Some(state) = conn.ext_state() else {
        tracing::debug!("session {}: no ext state, not sending {channel}", conn.id);
        return false;
    };
    let bytes = match serde_json::to_vec(payload) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("ext {channel}: cannot serialize payload: {e}");
            return false;
        }
    };
    if !packets::valid_channel(channel) || bytes.len() > MAX_PAYLOAD_LEN {
        tracing::warn!(
            "ext {channel}: dropping outbound message ({} bytes, channel valid: {})",
            bytes.len(),
            packets::valid_channel(channel)
        );
        return false;
    }
    conn.send(SCAAEmuExtPacket {
        opcode: state.sc_opcode,
        channel: channel.to_string(),
        payload: bytes,
    });
    true
}

/// [`send`] to a player's connection.
pub fn send_player(player: &PlayerRef, channel: &str, payload: &impl Serialize) -> bool {
    send(&player.conn, channel, payload)
}

/// [`send`] to the connection a handler is running on.
pub fn reply(conn: &GameConnection, channel: &str, payload: &impl Serialize) -> bool {
    send(&conn.handle(), channel, payload)
}

/// True when the connection said hello and negotiated `feature`.
pub fn has_feature(conn: &ConnectionHandle, feature: &str) -> bool {
    conn.ext_state().is_some_and(|s| s.has_feature(feature))
}
