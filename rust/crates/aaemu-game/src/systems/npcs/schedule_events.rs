//! Scheduled-event announcements: `SCScheduledEventStartedPacket` (0x1ec) and
//! `SCScheduledEventEndedPacket` (0x1ed), behind the default-off `Events:ScheduledEvents` flag.
//!
//! # Evidence
//!
//! The 1.2 client reader of 0x1ec is `FUN_397b01f0` (`x2game.dll.c:1704749`); it calls
//! `FUN_398a7ed0` (`:1942981`) with the packet as `this` (the id is read at `this + 0xd0`, i.e.
//! packet `+0xdc`, confirmed by the constructor `FUN_391b6e80` allocating 0x160 bytes and
//! `FUN_398a7940` zeroing the struct). The helper reads, in wire order:
//!
//! ```text
//! u32 id; 21 x i32 values ("v"); i32 changes; u8 priority; u8 startDate; u8 periodDate;
//! u8 hour; u8 min; i32 periodMin; u8 weekDay; str openMsg (max 0x7f)
//! ```
//!
//! The 0x1ed reader `FUN_397b0260` (`:1704772`) reads `u32 id` (`+0xc`) and `str closeMsg`
//! (`+0x10`, max 0x7f). The handlers are `FUN_391e1e10` = `OnScheduledEventStarted` (stores the
//! struct, fires UI event 0x256 with `openMsg`) and `FUN_391e1f30` = `OnScheduledEventEnded`
//! (calls `FUN_3930b940`, which acts only when the id equals the stored active event id and then
//! fires UI event 0x257 with `closeMsg`).
//!
//! The client keeps **one** active event id (`manager + 0xdd8`): a second Started replaces it,
//! and an Ended for a replaced id is ignored by `FUN_3930b940`. Configure one event at a time.
//!
//! # Trigger and text (INFERRED)
//!
//! No original-server capture exists. The trigger below mirrors the natural source, the
//! `game_schedules` window that already gates spawners / doodads / quests
//! ([`super::schedule::check_data`]): an opening edge broadcasts Started, a closing edge
//! broadcasts Ended, and a player entering the world while an event is active gets Started (the
//! client keeps the state, so a late joiner would otherwise see no banner and no HUD button).
//! The message text comes from the configuration; the shipped `game_schedules.name` values are
//! Korean and are not used. The `values` / `changes` / priority / date fields are opaque: the
//! handler stores them but only the message is displayed (INFERRED: the decompile shows no other
//! consumer), so they are sent as zeros.
//!
//! # Behaviour
//!
//! The tick runs every [`TICK_SECONDS`] over the configured allow-list. The first tick after
//! startup seeds the previous state silently, so a server restart does not re-announce running
//! events; later ticks send only on an edge. Entry ([`on_enter`], from `CSNotifyInGame`) sends
//! Started for every currently active allow-listed id, independent of the tick.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aaemu_commons::{Marshal, PacketStream};
use chrono::Utc;

use crate::net::packet::LEVEL_PLAIN;
use crate::opcodes::sc;
use crate::packets::packet_def;
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::schedule::{self, ScheduleData};

/// Seconds between two schedule evaluations.
pub const TICK_SECONDS: u64 = 30;

/// The client reads messages into a fixed 0x80-byte buffer (max 0x7f characters plus the NUL).
pub const MESSAGE_MAX_BYTES: usize = 0x7f;

/// `SCScheduledEventStartedPacket` (0x1ec): the "day event" banner and the HUD event button.
///
/// Wire (reader `FUN_397b01f0` -> `FUN_398a7ed0` -> `FUN_398a7d80`): `u32 id`, 21 x `i32`
/// `values`, `i32 changes`, `u8 priority`, `u8 start_date`, `u8 period_date`, `u8 hour`,
/// `u8 min`, `i32 period_min`, `u8 week_day`, `str open_msg` (max 0x7f bytes). The fixed part is
/// 102 bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SCScheduledEventStartedPacket {
    pub id: u32,
    /// The client's 21-slot schedule value array ("v" in the decompile); meaning UNKNOWN.
    pub values: [i32; 21],
    /// Bit mask over `values`, recomputed by the client on write; meaning UNKNOWN.
    pub changes: i32,
    pub priority: u8,
    pub start_date: u8,
    pub period_date: u8,
    pub hour: u8,
    pub min: u8,
    pub period_min: i32,
    pub week_day: u8,
    pub open_msg: String,
}
packet_def!(
    SCScheduledEventStartedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCScheduledEventStartedPacket
);
impl Marshal for SCScheduledEventStartedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id);
        for value in &self.values {
            s.write_i32(*value);
        }
        s.write_i32(self.changes)
            .write_u8(self.priority)
            .write_u8(self.start_date)
            .write_u8(self.period_date)
            .write_u8(self.hour)
            .write_u8(self.min)
            .write_i32(self.period_min)
            .write_u8(self.week_day);
        write_message(s, &self.open_msg);
    }
}

/// `SCScheduledEventEndedPacket` (0x1ed): closes the banner of the active event.
///
/// Wire (reader `FUN_397b0260`): `u32 id`, `str close_msg` (max 0x7f bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SCScheduledEventEndedPacket {
    pub id: u32,
    pub close_msg: String,
}
packet_def!(
    SCScheduledEventEndedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCScheduledEventEndedPacket
);
impl Marshal for SCScheduledEventEndedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id);
        write_message(s, &self.close_msg);
    }
}

/// Writes a configured message as the client's `u16`-prefixed string, cut to
/// [`MESSAGE_MAX_BYTES`] on a UTF-8 boundary (the client's buffer is `0x80` bytes including the
/// terminator).
fn write_message(s: &mut PacketStream, msg: &str) {
    let mut end = msg.len().min(MESSAGE_MAX_BYTES);
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    s.write_str(&msg[..end]);
}

/// One validated allow-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    id: i32,
    open_msg: String,
    close_msg: String,
}

/// Runtime state of the scheduled-event announcer (`server.systems.npcs.schedule_events`).
#[derive(Debug, Default)]
pub struct ScheduleEventState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The first tick after startup (or after the flag was turned off) only records the state.
    seeded: bool,
    /// Ids active at the last tick.
    active: HashSet<i32>,
}

impl ScheduleEventState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Validates the allow-list at the current configuration: unknown ids and duplicates are
/// skipped (the first duplicate wins), and overlength messages are reported (`warn`) once at
/// startup. The order follows the configuration so the announcements are deterministic.
fn validated(server: &GameServer, warn: bool) -> Vec<Entry> {
    let schedules = &server.game_data.systems.npcs.schedule;
    let config = &server.config.events.scheduled_events;
    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(config.events.len());
    for row in &config.events {
        if !seen.insert(row.schedule_id) {
            if warn {
                tracing::warn!(
                    schedule_id = row.schedule_id,
                    "Events.ScheduledEvents: duplicate schedule id, the first entry wins"
                );
            }
            continue;
        }
        if row.schedule_id <= 0 || !schedules.game_schedules.contains_key(&row.schedule_id) {
            if warn {
                tracing::warn!(
                    schedule_id = row.schedule_id,
                    "Events.ScheduledEvents: no such game_schedules row, skipped"
                );
            }
            continue;
        }
        if warn
            && (row.open_msg.len() > MESSAGE_MAX_BYTES || row.close_msg.len() > MESSAGE_MAX_BYTES)
        {
            tracing::warn!(
                schedule_id = row.schedule_id,
                max = MESSAGE_MAX_BYTES,
                "Events.ScheduledEvents: a message is longer than the client buffer and will be \
                 truncated"
            );
        }
        entries.push(Entry {
            id: row.schedule_id,
            open_msg: row.open_msg.clone(),
            close_msg: row.close_msg.clone(),
        });
    }
    entries
}

/// Whether the schedule's window is open at `now` ([`schedule::check_data`]).
fn is_active(schedules: &ScheduleData, id: i32, now: i64) -> bool {
    schedules.game_schedules.get(&id).is_some_and(|gs| {
        let (has_started, has_ended) = schedule::check_data(gs, now);
        has_started && !has_ended
    })
}

fn started_packet(entry: &Entry) -> SCScheduledEventStartedPacket {
    SCScheduledEventStartedPacket {
        id: entry.id as u32,
        open_msg: entry.open_msg.clone(),
        ..SCScheduledEventStartedPacket::default()
    }
}

fn ended_packet(entry: &Entry) -> SCScheduledEventEndedPacket {
    SCScheduledEventEndedPacket {
        id: entry.id as u32,
        close_msg: entry.close_msg.clone(),
    }
}

/// Sends Started to every online character. Returns the number of packets sent.
fn broadcast_started(server: &GameServer, entry: &Entry) -> usize {
    let packet = started_packet(entry);
    let players = server.world.all_characters();
    for player in &players {
        player.send_dyn(&packet);
    }
    players.len()
}

/// Sends Ended to every online character. Returns the number of packets sent.
fn broadcast_ended(server: &GameServer, entry: &Entry) -> usize {
    let packet = ended_packet(entry);
    let players = server.world.all_characters();
    for player in &players {
        player.send_dyn(&packet);
    }
    players.len()
}

/// One evaluation pass over the allow-list at `now`; returns the number of packets sent.
///
/// The first pass seeds the state silently; an id that becomes active sends Started, one that
/// stops being active sends Ended, and an unchanged id sends nothing. With the flag off the
/// state is reset so that enabling it later seeds again instead of replaying the old edge.
pub(crate) fn tick_at(server: &GameServer, now: i64) -> usize {
    if !server.config.events.scheduled_events.enabled {
        let mut inner = server.systems.npcs.schedule_events.lock();
        inner.seeded = false;
        inner.active.clear();
        return 0;
    }
    let schedules = &server.game_data.systems.npcs.schedule;
    let (started, ended) = {
        let mut inner = server.systems.npcs.schedule_events.lock();
        let entries = validated(server, false);
        let current: HashSet<i32> = entries
            .iter()
            .filter(|entry| is_active(schedules, entry.id, now))
            .map(|entry| entry.id)
            .collect();
        if !inner.seeded {
            inner.seeded = true;
            inner.active = current;
            return 0;
        }
        let started: Vec<Entry> = entries
            .iter()
            .filter(|entry| current.contains(&entry.id) && !inner.active.contains(&entry.id))
            .cloned()
            .collect();
        let ended: Vec<Entry> = entries
            .iter()
            .filter(|entry| !current.contains(&entry.id) && inner.active.contains(&entry.id))
            .cloned()
            .collect();
        inner.active = current;
        (started, ended)
    };
    let mut sent = 0;
    for entry in &started {
        sent += broadcast_started(server, entry);
    }
    for entry in &ended {
        sent += broadcast_ended(server, entry);
    }
    sent
}

/// The live tick (every [`TICK_SECONDS`]).
pub fn tick(server: &GameServer) -> usize {
    tick_at(server, Utc::now().timestamp())
}

/// Sends Started for every currently active allow-listed event to `player` (entry into the
/// world, `CSNotifyInGame`).
pub fn on_enter(server: &GameServer, player: &PlayerRef) {
    if !server.config.events.scheduled_events.enabled {
        return;
    }
    let now = Utc::now().timestamp();
    let schedules = &server.game_data.systems.npcs.schedule;
    for entry in validated(server, false) {
        if is_active(schedules, entry.id, now) {
            player.send(started_packet(&entry));
        }
    }
}

/// Validates the configuration and starts the periodic tick.
pub fn start(server: &Arc<GameServer>) {
    if server.config.events.scheduled_events.enabled {
        let entries = validated(server, true);
        tracing::info!(
            "Scheduled event announcements enabled ({} configured schedule(s))",
            entries.len()
        );
    } else {
        tracing::info!(
            "Scheduled event announcements disabled (Events.ScheduledEvents.Enabled is false)"
        );
    }
    let weak = Arc::downgrade(server);
    tokio::spawn(async move {
        let period = Duration::from_secs(TICK_SECONDS);
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            interval.tick().await;
            let Some(server) = weak.upgrade() else { break };
            tick(&server);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, ScheduledEventEntry, ScheduledEventsConfig};
    use crate::data::GameData;
    use crate::net::packet::encode;
    use crate::systems::npcs::schedule::ScheduleData;
    use crate::systems::npcs::schedule::{day_of_week, GameSchedule};
    use crate::test_support::{spawn_player, test_character, test_server_with_config, TestClient};
    use chrono::NaiveDateTime;

    const EVENT_ID: i32 = 500;
    const FUTURE_ID: i32 = 501;

    fn ts(value: &str) -> i64 {
        NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_utc()
            .timestamp()
    }

    /// A `game_schedules` row: the date part gates the window, and `start_time` / `end_time`
    /// are the daily hour the evaluator compares (see `schedule::check_data`).
    fn schedule(
        id: i32,
        start_date: (i32, i32, i32),
        end_date: (i32, i32, i32),
        start_hour: i32,
        end_hour: i32,
    ) -> GameSchedule {
        GameSchedule {
            id,
            name: format!("event {id}"),
            day_of_week_id: day_of_week::INVALID,
            start_time: start_hour,
            start_time_min: 0,
            end_time: end_hour,
            end_time_min: 0,
            st_year: start_date.0,
            st_month: start_date.1,
            st_day: start_date.2,
            ed_year: end_date.0,
            ed_month: end_date.1,
            ed_day: end_date.2,
            ..GameSchedule::default()
        }
    }

    /// 2026-01-01 10:00 .. 2026-01-02 12:00 (UTC).
    fn event_schedule(id: i32) -> GameSchedule {
        schedule(id, (2026, 1, 1), (2026, 1, 2), 10, 12)
    }

    /// 2000-01-01 .. 2100-01-01: active at the real clock, for the entry path.
    fn live_schedule(id: i32) -> GameSchedule {
        schedule(id, (2000, 1, 1), (2100, 1, 1), 0, 0)
    }

    /// 2100-01-01 .. 2101-01-01: not started at the real clock.
    fn future_schedule(id: i32) -> GameSchedule {
        schedule(id, (2100, 1, 1), (2101, 1, 1), 0, 0)
    }

    fn config(enabled: bool, entries: &[(i32, &str, &str)]) -> AppConfig {
        let mut config = AppConfig {
            id: 1,
            ..AppConfig::default()
        };
        config.events.scheduled_events = ScheduledEventsConfig {
            enabled,
            events: entries
                .iter()
                .map(|(id, open, close)| ScheduledEventEntry {
                    schedule_id: *id,
                    open_msg: (*open).into(),
                    close_msg: (*close).into(),
                })
                .collect(),
        };
        config
    }

    struct Fixture {
        server: Arc<GameServer>,
        a: TestClient,
        b: TestClient,
    }

    fn fixture(
        schedules: Vec<GameSchedule>,
        enabled: bool,
        entries: &[(i32, &str, &str)],
    ) -> Fixture {
        let mut data = GameData::empty();
        data.systems.npcs.schedule =
            ScheduleData::from_rows(schedules, Vec::new(), Vec::new(), Vec::new());
        let server = Arc::new(test_server_with_config(data, config(enabled, entries)));
        server.set_self();
        let mut a = TestClient::new(&server, 1, 1);
        spawn_player(&server, &a, test_character(1, "Alpha", 100.0, 100.0), 0x100);
        let mut b = TestClient::new(&server, 2, 2);
        spawn_player(&server, &b, test_character(2, "Beta", 110.0, 100.0), 0x101);
        a.drain();
        b.drain();
        Fixture { server, a, b }
    }

    fn body<P: Marshal>(packet: &P) -> Vec<u8> {
        let mut stream = PacketStream::new();
        packet.write(&mut stream);
        stream.into_bytes()
    }

    fn started_id(body: &[u8]) -> u32 {
        u32::from_le_bytes(body[0..4].try_into().expect("id"))
    }

    /// The string starts after the 102-byte fixed part.
    fn started_msg(body: &[u8]) -> String {
        let len = u16::from_le_bytes(body[102..104].try_into().expect("len")) as usize;
        String::from_utf8(body[104..104 + len].to_vec()).expect("message")
    }

    fn ended_id(body: &[u8]) -> u32 {
        u32::from_le_bytes(body[0..4].try_into().expect("id"))
    }

    fn ended_msg(body: &[u8]) -> String {
        let len = u16::from_le_bytes(body[4..6].try_into().expect("len")) as usize;
        String::from_utf8(body[6..6 + len].to_vec()).expect("message")
    }

    #[test]
    fn scheduled_event_started_packet_is_byte_exact() {
        let values: [i32; 21] = std::array::from_fn(|i| i as i32 - 10);
        let packet = SCScheduledEventStartedPacket {
            id: 0x0102_0304,
            values,
            changes: 0x00ab_cdef,
            priority: 1,
            start_date: 2,
            period_date: 3,
            hour: 4,
            min: 5,
            period_min: -7,
            week_day: 6,
            open_msg: "open".into(),
        };
        // Independent literal: id, 21 x i32 (-10..=10), changes, 5 u8, periodMin, weekDay,
        // string length + bytes.
        let mut want = vec![4, 3, 2, 1];
        for value in -10..=10i32 {
            want.extend(value.to_le_bytes());
        }
        want.extend(0x00ab_cdefi32.to_le_bytes());
        want.extend([1, 2, 3, 4, 5]);
        want.extend((-7i32).to_le_bytes());
        want.push(6);
        want.extend([4, 0, b'o', b'p', b'e', b'n']);
        assert_eq!(body(&packet), want);
        assert_eq!(want.len(), 102 + 2 + 4);

        let mut frame = vec![0xdd, 1, 0, 0, 0xec, 0x01];
        frame.extend(&want);
        assert_eq!(encode(&packet), frame);
    }

    #[test]
    fn scheduled_event_started_packet_zero_defaults_are_102_bytes() {
        let packet = SCScheduledEventStartedPacket {
            id: 1,
            ..SCScheduledEventStartedPacket::default()
        };
        let bytes = body(&packet);
        assert_eq!(bytes.len(), 104);
        assert_eq!(&bytes[..4], 1u32.to_le_bytes());
        assert_eq!(&bytes[4..88], &[0u8; 84][..]);
        assert_eq!(&bytes[88..92], [0; 4]);
        assert_eq!(&bytes[92..97], [0; 5]);
        assert_eq!(&bytes[97..101], [0; 4]);
        assert_eq!(bytes[101], 0);
        assert_eq!(&bytes[102..], [0, 0]);
    }

    #[test]
    fn scheduled_event_ended_packet_is_byte_exact() {
        let packet = SCScheduledEventEndedPacket {
            id: 0x0102_0304,
            close_msg: "close".into(),
        };
        let want = [4, 3, 2, 1, 5, 0, b'c', b'l', b'o', b's', b'e'];
        assert_eq!(body(&packet), want);
        assert_eq!(want.len(), 4 + 2 + 5);

        let mut frame = vec![0xdd, 1, 0, 0, 0xed, 0x01];
        frame.extend(want);
        assert_eq!(encode(&packet), frame);
    }

    #[test]
    fn messages_are_cut_to_127_bytes_on_a_char_boundary() {
        let long = SCScheduledEventStartedPacket {
            id: 1,
            open_msg: "a".repeat(200),
            ..SCScheduledEventStartedPacket::default()
        };
        let bytes = body(&long);
        assert_eq!(bytes.len(), 102 + 2 + 127);
        assert_eq!(&bytes[104..], &[b'a'; 127][..]);

        // A three-byte character that would straddle the limit is dropped whole.
        let mut msg = "a".repeat(126);
        msg.push('€');
        let packet = SCScheduledEventStartedPacket {
            id: 1,
            open_msg: msg,
            ..SCScheduledEventStartedPacket::default()
        };
        let bytes = body(&packet);
        assert_eq!(u16::from_le_bytes(bytes[102..104].try_into().unwrap()), 126);
        assert_eq!(bytes.len(), 102 + 2 + 126);

        // A message under the limit keeps its bytes.
        let packet = SCScheduledEventStartedPacket {
            id: 1,
            open_msg: "한".into(),
            ..SCScheduledEventStartedPacket::default()
        };
        assert_eq!(&body(&packet)[104..], "한".as_bytes());

        let ended = SCScheduledEventEndedPacket {
            id: 1,
            close_msg: "b".repeat(200),
        };
        let bytes = body(&ended);
        assert_eq!(bytes.len(), 4 + 2 + 127);
        assert_eq!(&bytes[6..], &[b'b'; 127][..]);
    }

    #[tokio::test]
    async fn opening_and_closing_edges_send_one_started_and_one_ended() {
        let mut f = fixture(
            vec![event_schedule(EVENT_ID)],
            true,
            &[(EVENT_ID, "Event open", "Event close")],
        );
        // 2026-01-01 09:00: before the daily start -> the first tick seeds the empty state.
        assert_eq!(tick_at(&f.server, ts("2026-01-01 09:00:00")), 0);
        assert!(f.a.drain().is_empty());
        assert!(f.b.drain().is_empty());

        // 11:00: opening edge, one Started per connected character, nobody else.
        assert_eq!(tick_at(&f.server, ts("2026-01-01 11:00:00")), 2);
        let a = f.a.drain();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, sc::SCScheduledEventStartedPacket);
        assert_eq!(started_id(&a[0].1), EVENT_ID as u32);
        assert_eq!(started_msg(&a[0].1), "Event open");
        let b = f.b.drain();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, sc::SCScheduledEventStartedPacket);

        // A later tick inside the window sends nothing (no edge).
        assert_eq!(tick_at(&f.server, ts("2026-01-01 12:00:00")), 0);
        assert!(f.a.drain().is_empty());
        assert!(f.b.drain().is_empty());

        // 2026-01-02 12:00: closing edge, exactly one Ended per character.
        assert_eq!(tick_at(&f.server, ts("2026-01-02 12:00:00")), 2);
        let a = f.a.drain();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, sc::SCScheduledEventEndedPacket);
        assert_eq!(ended_id(&a[0].1), EVENT_ID as u32);
        assert_eq!(ended_msg(&a[0].1), "Event close");
        let b = f.b.drain();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, sc::SCScheduledEventEndedPacket);

        // After the end nothing is repeated.
        assert_eq!(tick_at(&f.server, ts("2026-01-02 13:00:00")), 0);
        assert!(f.a.drain().is_empty());
        assert!(f.b.drain().is_empty());
    }

    #[tokio::test]
    async fn restart_seeds_silently_and_a_late_join_gets_started() {
        let mut f = fixture(
            vec![live_schedule(EVENT_ID)],
            true,
            &[(EVENT_ID, "Event open", "Event close")],
        );
        // The first tick after startup (the event is already running) broadcasts nothing.
        assert_eq!(tick_at(&f.server, ts("2026-11-07 12:00:00")), 0);
        assert!(f.a.drain().is_empty());
        assert!(f.b.drain().is_empty());

        // A player entering the world while the event is active gets Started for it.
        let player = f.server.world.character_by_id(1).expect("player");
        on_enter(&f.server, &player);
        let frames = f.a.drain();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, sc::SCScheduledEventStartedPacket);
        assert_eq!(started_id(&frames[0].1), EVENT_ID as u32);
        assert_eq!(started_msg(&frames[0].1), "Event open");
        assert!(f.b.drain().is_empty());
    }

    #[tokio::test]
    async fn late_join_sends_only_currently_active_allow_listed_events() {
        let mut f = fixture(
            vec![live_schedule(EVENT_ID), future_schedule(FUTURE_ID)],
            true,
            &[
                (EVENT_ID, "first", "close"),
                (FUTURE_ID, "second", "close"),
                (EVENT_ID, "duplicate", "close"),
                (999, "unknown", "close"),
            ],
        );
        // 501 starts in 2100: only 500 is active. The duplicate 500 keeps "first".
        let player = f.server.world.character_by_id(1).expect("player");
        on_enter(&f.server, &player);
        let frames = f.a.drain();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, sc::SCScheduledEventStartedPacket);
        assert_eq!(started_id(&frames[0].1), EVENT_ID as u32);
        assert_eq!(started_msg(&frames[0].1), "first");
    }

    #[tokio::test]
    async fn disabled_flag_sends_nothing_and_resets_the_state() {
        let mut f = fixture(
            vec![event_schedule(EVENT_ID)],
            true,
            &[(EVENT_ID, "Event open", "Event close")],
        );
        // Seed while the event is active (the first tick is silent).
        assert_eq!(tick_at(&f.server, ts("2026-01-01 11:00:00")), 0);
        assert!(f.a.drain().is_empty());

        // While disabled, a tick at the closing edge must not evaluate (no Ended) and an
        // entering player gets nothing.
        f.server.config.update(|config| {
            config.events.scheduled_events.enabled = false;
        });
        assert_eq!(tick_at(&f.server, ts("2026-01-02 12:00:00")), 0);
        assert!(f.a.drain().is_empty());
        assert!(f.b.drain().is_empty());
        let player = f.server.world.character_by_id(1).expect("player");
        on_enter(&f.server, &player);
        assert!(f.a.drain().is_empty());

        // Turning the flag back on seeds again: the stale active id is not replayed as Ended.
        f.server.config.update(|config| {
            config.events.scheduled_events.enabled = true;
        });
        assert_eq!(tick_at(&f.server, ts("2026-01-02 13:00:00")), 0);
        assert!(f.a.drain().is_empty());
        on_enter(&f.server, &player);
        assert!(f.a.drain().is_empty());
    }
}
