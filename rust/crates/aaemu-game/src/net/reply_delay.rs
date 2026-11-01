//! `Debug:ReplyDelay`: an opt-in capture aid that holds selected S->C frames on their way to the
//! client.
//!
//! At localhost the server answers a C->S packet in about 1 ms, inside the same client frame, so a
//! capture cannot tell a client rule from a reaction to our reply (evidence audit 2026-11-01, F2).
//! With this switch on, selected replies arrive `DelayMs` later and the capture shows which client
//! events move with the reply.
//!
//! The decision is made where a frame is queued ([`GameConnection::send`] /
//! [`ConnectionHandle::send_frame`]) and the deadline travels with the frame
//! ([`crate::net::outbox::OutFrame`]); the connection writer writes each frame at its deadline.
//! Frames queued in order therefore stay in order while they wait, and frames that are not
//! selected keep their own order, but an undelayed frame queued after a held one is written first.
//! `DelayEverythingAfter` removes that reordering: once a selected frame is held, every later frame
//! of that connection is held for the same delay, so the full order is preserved.
//!
//! Nothing is allocated, timed or logged per packet while the switch is off: the server keeps
//! `reply_delay: None` and the connection keeps no delay state.
//!
//! [`GameConnection::send`]: crate::net::connection::GameConnection::send
//! [`ConnectionHandle::send_frame`]: crate::net::connection::ConnectionHandle::send_frame

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::time::Instant;

use crate::config::ReplyDelayConfig;
use crate::net::outbox::OutFrame;
use crate::net::packet::{frame_level_opcode, LEVEL_PLAIN, LEVEL_PROXY};
use crate::opcodes::{cs, pp, sc};

/// Resolved `Debug:ReplyDelay` shared by every connection; `None` on the server = off.
#[derive(Debug)]
pub struct ReplyDelay {
    delay: Duration,
    /// S->C opcodes selected by `Packets`.
    packets: HashSet<u16>,
    /// C->S `(level << 16) | opcode` keys selected by `RepliesTo`.
    replies_to: HashSet<u32>,
    /// Lower-cased `CharacterName` (`None` = every connection).
    character_name: Option<String>,
    delay_everything_after: bool,
}

impl ReplyDelay {
    /// Builds the switch from its configuration. Returns `None` (off) when `DelayMs` is 0 or no
    /// selector resolves, logging why; logs the active delay and selectors once at startup.
    pub fn resolve(config: &ReplyDelayConfig) -> Option<Arc<Self>> {
        if config.delay_ms == 0 {
            if !config.packets.is_empty() || !config.replies_to.is_empty() {
                tracing::warn!(
                    "Debug:ReplyDelay ignored: DelayMs is 0 (packets {:?}, replies to {:?})",
                    config.packets,
                    config.replies_to
                );
            }
            return None;
        }

        let mut packets = HashSet::new();
        let mut packet_names = Vec::new();
        for name in &config.packets {
            match resolve_name(sc::NAMES, name) {
                Some((opcode, resolved)) => {
                    packets.insert(opcode);
                    packet_names.push(resolved);
                }
                None => tracing::warn!(
                    "Debug:ReplyDelay: unknown S->C packet selector {name:?} (see opcodes::sc::NAMES); ignored"
                ),
            }
        }

        let mut replies_to = HashSet::new();
        let mut reply_names = Vec::new();
        for name in &config.replies_to {
            match resolve_name(cs::NAMES, name)
                .map(|(opcode, resolved)| (LEVEL_PLAIN, opcode, resolved))
                .or_else(|| {
                    resolve_name(pp::NAMES, name)
                        .map(|(opcode, resolved)| (LEVEL_PROXY, opcode, resolved))
                }) {
                Some((level, opcode, resolved)) => {
                    replies_to.insert(reply_key(level, opcode));
                    reply_names.push(resolved);
                }
                None => tracing::warn!(
                    "Debug:ReplyDelay: unknown C->S reply selector {name:?} (see opcodes::cs::NAMES / pp::NAMES); ignored"
                ),
            }
        }

        if packets.is_empty() && replies_to.is_empty() {
            tracing::warn!(
                "Debug:ReplyDelay ignored: none of the configured selectors names a 1.2 packet"
            );
            return None;
        }

        let character_name = config
            .character_name
            .as_ref()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .map(str::to_lowercase);
        tracing::info!(
            "Debug:ReplyDelay on: {} ms; S->C packets [{}]; replies to C->S [{}]; character {}; delay-everything-after {}",
            config.delay_ms,
            packet_names.join(", "),
            reply_names.join(", "),
            character_name.as_deref().unwrap_or("(any)"),
            config.delay_everything_after
        );
        Some(Arc::new(Self {
            delay: Duration::from_millis(config.delay_ms),
            packets,
            replies_to,
            character_name,
            delay_everything_after: config.delay_everything_after,
        }))
    }
}

/// Per-connection state of the switch: the frame decision and the reply context of the C->S
/// packet currently handled by the connection task.
#[derive(Debug)]
pub struct ReplyDelayConn {
    config: Arc<ReplyDelay>,
    /// `(level << 16) | opcode` of the C->S packet being handled, plus 1 (`0` = none).
    reply_to: AtomicU32,
    /// `0` = no character yet, `1` = matches `CharacterName`, `2` = does not.
    name_match: AtomicU8,
    /// `DelayEverythingAfter`: a selected frame was held on this connection.
    strict: AtomicBool,
}

impl ReplyDelayConn {
    pub fn new(config: Arc<ReplyDelay>) -> Arc<Self> {
        Arc::new(Self {
            config,
            reply_to: AtomicU32::new(0),
            name_match: AtomicU8::new(0),
            strict: AtomicBool::new(false),
        })
    }

    /// Marks the C->S packet whose handler is about to run ([`crate::server::GameServer::handle_frame`]).
    pub fn set_reply_to(&self, level: u8, opcode: u16) {
        self.reply_to
            .store(reply_key(level, opcode) + 1, Ordering::Relaxed);
    }

    /// Clears the reply context once the handler returned.
    pub fn clear_reply_to(&self) {
        self.reply_to.store(0, Ordering::Relaxed);
    }

    /// Records the character attached to the connection (`CharacterName` filter).
    pub fn observe_character_name(&self, name: &str) {
        let Some(wanted) = &self.config.character_name else {
            return;
        };
        let matched = name.to_lowercase() == *wanted;
        self.name_match
            .store(if matched { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// The queue entry for an encoded frame: immediate, or held until `now + DelayMs`.
    pub fn frame(&self, bytes: Bytes) -> OutFrame {
        let selected = self.selects(&bytes);
        if selected && self.config.delay_everything_after {
            self.strict.store(true, Ordering::Relaxed);
        }
        let held =
            selected || (self.config.delay_everything_after && self.strict.load(Ordering::Relaxed));
        if held {
            OutFrame::delayed(bytes, Instant::now() + self.config.delay)
        } else {
            OutFrame::now(bytes)
        }
    }

    fn selects(&self, frame: &[u8]) -> bool {
        if self.config.character_name.is_some() && self.name_match.load(Ordering::Relaxed) != 1 {
            return false;
        }
        let Some((level, opcode)) = frame_level_opcode(frame) else {
            return false;
        };
        // Name selectors are the level-1 S->C table; a proxy opcode that happens to share a
        // number is not the packet the name means.
        if level == LEVEL_PLAIN && self.config.packets.contains(&opcode) {
            return true;
        }
        let reply_to = self.reply_to.load(Ordering::Relaxed);
        reply_to != 0 && self.config.replies_to.contains(&(reply_to - 1))
    }
}

fn reply_key(level: u8, opcode: u16) -> u32 {
    u32::from(level) << 16 | u32::from(opcode)
}

/// `SCSkillStarted`, `SCSkillStartedPacket` and `scskillstarted` all name the same packet.
fn resolve_name(table: &[(u16, &'static str)], wanted: &str) -> Option<(u16, &'static str)> {
    let wanted = normalise_name(wanted);
    table
        .iter()
        .find_map(|(opcode, name)| (normalise_name(name) == wanted).then_some((*opcode, *name)))
}

fn normalise_name(name: &str) -> String {
    let name = name.trim().to_ascii_lowercase();
    match name.strip_suffix("packet") {
        Some(stripped) => stripped.to_owned(),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReplyDelayConfig;
    use crate::net::packet::encode_raw;
    use crate::opcodes::{cs, sc};
    use crate::test_support::{test_character, TestClient};

    fn config(packets: &[&str], replies_to: &[&str]) -> ReplyDelayConfig {
        ReplyDelayConfig {
            delay_ms: 600,
            packets: packets.iter().map(|s| (*s).to_string()).collect(),
            replies_to: replies_to.iter().map(|s| (*s).to_string()).collect(),
            character_name: None,
            delay_everything_after: false,
        }
    }

    fn frame(opcode: u16) -> Bytes {
        Bytes::from(encode_raw(LEVEL_PLAIN, opcode, &[]))
    }

    #[test]
    fn names_accept_the_packet_suffix_and_any_case() {
        assert_eq!(
            resolve_name(sc::NAMES, "SCSkillStarted"),
            resolve_name(sc::NAMES, "SCSkillStartedPacket")
        );
        assert_eq!(
            resolve_name(cs::NAMES, "csstartskill").map(|(op, _)| op),
            Some(cs::CSStartSkillPacket)
        );
        assert_eq!(resolve_name(sc::NAMES, "NoSuchPacket"), None);
    }

    #[test]
    fn off_without_a_delay_or_a_selector() {
        assert!(ReplyDelay::resolve(&ReplyDelayConfig::default()).is_none());
        let mut no_selector = config(&[], &[]);
        no_selector.delay_ms = 600;
        assert!(ReplyDelay::resolve(&no_selector).is_none());
        let mut unknown = config(&["NoSuchPacket"], &[]);
        unknown.delay_ms = 600;
        assert!(ReplyDelay::resolve(&unknown).is_none());
    }

    #[test]
    fn selects_by_packet_name_and_by_the_packet_being_handled() {
        let by_name = ReplyDelay::resolve(&config(&["SCSkillStarted"], &[])).unwrap();
        let conn = ReplyDelayConn::new(by_name);
        assert!(conn
            .frame(frame(sc::SCSkillStartedPacket))
            .release_at
            .is_some());
        assert!(conn
            .frame(frame(sc::SCSkillFiredPacket))
            .release_at
            .is_none());

        let by_reply = ReplyDelay::resolve(&config(&[], &["CSStartSkill"])).unwrap();
        let conn = ReplyDelayConn::new(by_reply);
        assert!(conn
            .frame(frame(sc::SCSkillStartedPacket))
            .release_at
            .is_none());
        conn.set_reply_to(LEVEL_PLAIN, cs::CSStartSkillPacket);
        assert!(conn
            .frame(frame(sc::SCSkillFiredPacket))
            .release_at
            .is_some());
        conn.clear_reply_to();
        assert!(conn
            .frame(frame(sc::SCSkillFiredPacket))
            .release_at
            .is_none());
    }

    #[test]
    fn character_filter_matches_case_insensitively_and_holds_until_the_name_is_known() {
        let mut cfg = config(&["SCSkillStarted"], &[]);
        cfg.character_name = Some("Capture".into());
        let conn = ReplyDelayConn::new(ReplyDelay::resolve(&cfg).unwrap());

        // No character yet: lobby frames of the capture account are not held.
        assert!(conn
            .frame(frame(sc::SCSkillStartedPacket))
            .release_at
            .is_none());
        conn.observe_character_name("someone else");
        assert!(conn
            .frame(frame(sc::SCSkillStartedPacket))
            .release_at
            .is_none());
        conn.observe_character_name("capture");
        assert!(conn
            .frame(frame(sc::SCSkillStartedPacket))
            .release_at
            .is_some());
    }

    #[test]
    fn delay_everything_after_holds_all_later_frames() {
        let mut cfg = config(&["SCSkillStarted"], &[]);
        cfg.delay_everything_after = true;
        let conn = ReplyDelayConn::new(ReplyDelay::resolve(&cfg).unwrap());

        let before = conn.frame(frame(sc::SCErrorMsgPacket));
        assert!(before.release_at.is_none());
        let selected = conn.frame(frame(sc::SCSkillStartedPacket));
        assert!(selected.release_at.is_some());
        // Not selected by name, but the connection is strict after the selected frame.
        let after = conn.frame(frame(sc::SCErrorMsgPacket));
        assert!(after.release_at.is_some());
        assert!(after.release_at >= selected.release_at);
    }

    fn server_with(reply_delay: ReplyDelayConfig) -> crate::server::GameServer {
        use crate::config::AppConfig;
        use crate::data::GameData;
        use crate::server::GameServer;
        use sqlx::mysql::MySqlPoolOptions;
        use std::time::Duration;

        let mut cfg = AppConfig {
            id: 1,
            ..AppConfig::default()
        };
        cfg.debug.reply_delay = reply_delay;
        cfg.connections.mysql_provider.host = "127.0.0.1".into();
        cfg.connections.mysql_provider.port = 1;
        let db = MySqlPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy_with(cfg.connections.mysql_provider.connect_options());
        GameServer::new(cfg, db, GameData::empty())
    }

    #[tokio::test]
    async fn player_new_registers_the_capture_character_on_its_connection_only() {
        use crate::systems::skills::packets::SCCombatClearedPacket;
        use crate::world::Player;

        let mut cfg = config(&["SCCombatCleared"], &[]);
        cfg.character_name = Some("Capture".into());
        let server = server_with(cfg);

        let mut capture = TestClient::new(&server, 1, 1000);
        let capture_player = Player::new(
            test_character(1, "Capture", 100.0, 100.0),
            capture.conn.handle(),
        );
        let mut other = TestClient::new(&server, 2, 1001);
        let other_player = Player::new(
            test_character(2, "Other", 100.0, 100.0),
            other.conn.handle(),
        );
        capture_player.send(SCCombatClearedPacket {
            obj_id: capture_player.obj_id,
        });
        other_player.send(SCCombatClearedPacket {
            obj_id: other_player.obj_id,
        });

        let frames = capture.drain_frames();
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0].release_at.is_some(),
            "the capture character's reply is held"
        );
        let frames = other.drain_frames();
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0].release_at.is_none(),
            "another character's reply is not held"
        );
    }

    #[tokio::test]
    async fn handler_sets_the_reply_context_of_the_cs_packet_it_handles() {
        use crate::net::packet::encode_raw;
        use crate::opcodes::pp;
        use aaemu_commons::PacketStream;

        // `Ping` resolves to `PingPacket` (the name check accepts the suffix too).
        let server = server_with(config(&[], &["Ping"]));
        let mut client = TestClient::new(&server, 1, 1000);
        let mut body = PacketStream::new();
        body.write_i64(11).write_i64(22).write_u32(33);
        server
            .handle_frame(
                &mut client.conn,
                &encode_raw(LEVEL_PROXY, pp::PingPacket, body.as_bytes()),
            )
            .await
            .unwrap();

        let frames = client.drain_frames();
        assert_eq!(frames.len(), 1, "Ping -> Pong");
        assert!(
            frames[0].release_at.is_some(),
            "a frame sent while CS Ping is handled is held"
        );

        // The context is cleared after the handler, so a later timer send is not held.
        client
            .conn
            .handle()
            .send_frame(Bytes::from(encode_raw(LEVEL_PROXY, pp::PongPacket, &[])));
        let frames = client.drain_frames();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].release_at.is_none());
    }
}
