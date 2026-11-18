//! Shared helpers for flow tests that need a `GameServer` without MySQL (see
//! `CONTRIBUTING-PORT.md`, "Testing"). The pool points at a closed port and is never used
//! unless a test calls into the DB; creating it needs a Tokio runtime (`#[tokio::test]`).

use std::net::SocketAddr;
use std::time::Duration;

use sqlx::mysql::MySqlPoolOptions;
use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::char_data::VisualOptions;
use crate::models::character::{Character, Inventory};
use crate::net::connection::{ConnCommand, GameConnection};
use crate::world::unit_state::UNIT_STATE_FORCED_BUFFS;
use crate::world::{Player, PlayerRef};
use crate::GameServer;

/// Zone key of the synthetic `main_world`.
pub const TEST_ZONE: u32 = 100;

/// A server with game data `data` plus a synthetic 2x2-cell `main_world` (32x32 sectors of
/// 64 m, zone [`TEST_ZONE`] everywhere) and the two buff templates `SCUnitStatePacket` forces
/// on characters.
pub fn test_server_with(data: GameData) -> GameServer {
    test_server_with_world(
        data,
        WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE),
    )
}

/// [`test_server_with`] with a caller-built `main_world` template (e.g. more than one zone key).
pub fn test_server_with_world(data: GameData, world: WorldTemplate) -> GameServer {
    let config = AppConfig {
        id: 1,
        // The flow-test fixtures exercise the KR 1.2 systems directly; opt into player
        // nations so they do not depend on the default era profile (`EuAuroria` has none).
        social: crate::systems::social::config::SocialConfig {
            player_nations: Some(true),
            ..Default::default()
        },
        ..AppConfig::default()
    };
    test_server_with_world_config(data, world, config)
}

/// [`test_server_with`] with a caller configuration (e.g. `character_name_use_allowed_chars`).
pub fn test_server_with_config(data: GameData, config: AppConfig) -> GameServer {
    test_server_with_world_config(
        data,
        WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE),
        config,
    )
}

/// [`test_server_with_world`] with a caller configuration.
pub fn test_server_with_world_config(
    mut data: GameData,
    world: WorldTemplate,
    mut config: AppConfig,
) -> GameServer {
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1; // nothing listens here
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    data.worlds = WorldTemplates::from_templates(vec![world], Vec::new()).expect("test world");
    for id in UNIT_STATE_FORCED_BUFFS {
        let mut t = BuffTemplate::simple(id, BuffKind::Good);
        t.duration = 0;
        data.skills.buffs.insert(id, t);
    }
    // Production derives the game clock from the wall clock (continuous across restarts). The
    // flow tests pin the former C# noon start so fixtures that do not set a time stay
    // deterministic; `TimeManager::set_test_clock` gives a test its own wall clock.
    let server = GameServer::new(config, db, data);
    server.time.set(12.0);
    server
}

/// Marks every skill template of `data` as handed to players by the static data, so the
/// fixtures that cast doodad / quest style skills reach the client cast fallback.
pub fn open_client_skills(data: &mut GameData) {
    for id in data.systems.skills.skill_ids() {
        data.skills.add_client_sourced_skill(id);
    }
}

/// [`test_server_with`] with empty game data.
pub fn test_server() -> GameServer {
    test_server_with(GameData::empty())
}

/// An `ExperienceTable` for levels `1..=up_to` (strictly increasing total / mate exp,
/// `skill_points = level`): tests that learn skills need `GetSkillPointsForLevel` /
/// `GetAbLevel` to answer.
pub fn test_experience(up_to: u8) -> crate::data::ExperienceTable {
    crate::data::ExperienceTable::from_templates(
        (1..=up_to)
            .map(|level| crate::data::LevelTemplate {
                level,
                total_exp: i32::from(level) * 1_000,
                total_mate_exp: i32::from(level) * 1_000,
                skill_points: i32::from(level),
            })
            .collect(),
        55,
    )
    .expect("test experience table")
}

/// A connection whose outgoing frames can be inspected.
pub struct TestClient {
    pub conn: GameConnection,
    out: crate::net::outbox::OutboxReceiver,
    _cmd: mpsc::UnboundedReceiver<ConnCommand>,
}

impl TestClient {
    pub fn new(server: &GameServer, id: u32, account: u32) -> Self {
        let (out_tx, out) = crate::net::outbox::outbox();
        let (cmd_tx, cmd) = mpsc::unbounded_channel();
        let peer: SocketAddr = "127.0.0.1:50000".parse().expect("addr");
        let (mut conn, _) = GameConnection::new(id, peer, out_tx, cmd_tx);
        conn.set_account_id(account);
        conn.set_reply_delay(server.reply_delay.clone());
        server.connections.add(conn.handle());
        Self {
            conn,
            out,
            _cmd: cmd,
        }
    }

    /// Queued frames with their `Debug:ReplyDelay` deadline (`None` = not held).
    pub fn drain_frames(&mut self) -> Vec<crate::net::outbox::OutFrame> {
        let mut v = Vec::new();
        while let Ok(f) = self.out.try_recv_frame() {
            v.push(f);
        }
        v
    }

    /// Drained frames as (opcode, body); level-2 frames are tagged `0x8000 | opcode`.
    pub fn drain(&mut self) -> Vec<(u16, Vec<u8>)> {
        let mut v = Vec::new();
        while let Ok(f) = self.out.try_recv() {
            if f[1] == 1 {
                v.push((u16::from_le_bytes([f[4], f[5]]), f[6..].to_vec()));
            } else {
                v.push((0x8000 | u16::from_le_bytes([f[2], f[3]]), f[4..].to_vec()));
            }
        }
        v
    }

    /// The `ConnCommand`s queued for this connection since the last call.
    pub fn commands(&mut self) -> Vec<ConnCommand> {
        let mut v = Vec::new();
        while let Ok(c) = self._cmd.try_recv() {
            v.push(c);
        }
        v
    }

    /// Opcodes of the drained frames.
    pub fn drain_ops(&mut self) -> Vec<u16> {
        self.drain().into_iter().map(|f| f.0).collect()
    }
}

/// A level-10 character at (x, y, 100) with visual options (so `SCUnitStatePacket` is complete).
pub fn test_character(id: u32, name: &str, x: f32, y: f32) -> Character {
    let mut c = Character::new(id, 1000 + id, Default::default());
    c.unit.name = name.into();
    c.race = 1;
    c.gender = 1;
    c.unit.level = 10;
    c.unit.faction_id = 101;
    c.num_inventory_slots = 50;
    c.num_bank_slots = 50;
    c.inventory = Inventory::standalone(id, 50, 50);
    c.unit.transform.set_xyz(x, y, 100.0);
    c.unit.transform.zone_id = TEST_ZONE;
    c.visual_options = Some(VisualOptions {
        flag: 1,
        stp: Some([0; 6]),
        ..VisualOptions::default()
    });
    c
}

/// A player handle for `character` on `client`'s connection, registered server-wide and
/// spawned (`Character.Spawn`) in instance 0.
pub fn spawn_player(
    server: &GameServer,
    client: &TestClient,
    mut character: Character,
    obj_id: u32,
) -> PlayerRef {
    character.obj_id = obj_id;
    let p = Player::new(character, client.conn.handle());
    server.world.try_add_character(&p);
    let world = server.world.world(0).expect("instance 0");
    world.spawn(&p, &server.view_ctx());
    p
}

/// Runs `f` on its own thread (inside the current Tokio runtime's context) and panics when it
/// does not finish in 10 s: a regression of a lock cycle must fail a test instead of hanging
/// the whole test run. The stuck thread is leaked.
pub fn finishes_in_time<R: Send + 'static>(
    what: &str,
    f: impl FnOnce() -> R + Send + 'static,
) -> R {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let _guard = handle.enter();
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("{what} did not finish: deadlocked"))
}

/// One event captured by [`capture_events`].
pub type CapturedEvent = (tracing::Level, String);

struct EventLog(std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>);

struct MessageOf(String);

impl tracing::field::Visit for MessageOf {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for EventLog {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut message = MessageOf(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .expect("event log poisoned")
            .push((*event.metadata().level(), message.0));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Never records anything. While it is alive next to a capturing subscriber, `tracing` keeps
/// every registered subscriber in its interest calculation. With a single registered
/// subscriber it would work the interest of a call site out from the thread that first hits
/// it, which in a parallel test run is another test's thread with no subscriber: the call site
/// then caches "never" and the capturing test misses its events.
struct Anchor;

impl tracing::Subscriber for Anchor {
    fn register_callsite(&self, _: &tracing::Metadata<'_>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Runs `f` and returns what it logged on this thread, as `(level, message)` (log level
/// regression tests: routine actions must not log at `WARN`).
pub fn capture_events<R>(f: impl FnOnce() -> R) -> (R, Vec<CapturedEvent>) {
    static ANCHOR: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    ANCHOR.get_or_init(|| tracing::Dispatch::new(Anchor));
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let result = tracing::subscriber::with_default(EventLog(log.clone()), || {
        // Call sites another test hit while no subscriber was set cache "never".
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let events = std::mem::take(&mut *log.lock().expect("event log poisoned"));
    (result, events)
}
