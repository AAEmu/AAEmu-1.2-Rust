//! Shared server state (the C# singletons used by this phase) and the frame dispatcher
//! (`GameProtocolHandler.OnReceive` after reassembly).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use sqlx::mysql::MySqlPool;

use crate::config::AppConfig;
use crate::data::GameData;
use crate::error::{GameError, Result};
use crate::login::LoginLink;
use crate::managers::chat::ChatManager;
use crate::managers::enter_world::EnterWorldThrottle;
use crate::managers::expedition::ExpeditionManager;
use crate::managers::friend::FriendManager;
use crate::managers::id::IdManagers;
use crate::managers::item::ItemManager;
use crate::managers::name::NameManager;
use crate::managers::save::SaveManager;
use crate::managers::time::TimeManager;
use crate::managers::{AccountManager, EnterWorldManager, StreamManager};
use crate::net::connection::{ConnectionTable, GameConnection};
use crate::net::packet::{decode_frame, LEVEL_PLAIN, LEVEL_PROXY};
use crate::net::registry::PacketRegistry;
use crate::net::reply_delay::ReplyDelay;
use crate::opcodes;
use crate::world::WorldManager;

pub struct GameServer {
    pub config: crate::config::ConfigCell,
    pub db: MySqlPool,
    pub game_data: Arc<GameData>,
    pub registry: PacketRegistry,
    pub connections: ConnectionTable,
    /// `Debug:ReplyDelay` (`None` = off), resolved once at startup.
    pub reply_delay: Option<Arc<ReplyDelay>>,
    pub enter_world: EnterWorldManager,
    /// Per-IP throttle for failed `X2EnterWorld` attempts.
    pub enter_world_throttle: EnterWorldThrottle,
    pub accounts: AccountManager,
    pub streams: StreamManager,
    pub login: LoginLink,
    /// The `Core/Managers/Id` singletons.
    pub ids: Arc<IdManagers>,
    pub names: Arc<NameManager>,
    /// `ItemManager` (user items / containers).
    pub items: Arc<ItemManager>,
    pub world: Arc<WorldManager>,
    pub chat: ChatManager,
    pub time: TimeManager,
    pub expeditions: ExpeditionManager,
    pub friends: FriendManager,
    pub save: SaveManager,
    /// The phase-3 subsystems' managers and behaviour hooks (`systems`).
    pub systems: crate::systems::Systems,
    /// A weak self reference for the timers that outlive a packet handler.
    self_ref: std::sync::OnceLock<std::sync::Weak<GameServer>>,
    next_connection_id: AtomicU32,
    /// Monotonic order of the slot-occupancy reports sent to the login server's world queue.
    occupancy_sequence: std::sync::Mutex<u32>,
    /// Set while the graceful shutdown returns the players to the world list: entering the world
    /// is refused (`systems::services::shutdown`).
    shutting_down: AtomicBool,
}

impl GameServer {
    pub fn new(config: AppConfig, db: MySqlPool, game_data: GameData) -> Self {
        tracing::info!(
            "Character slots: base {} per account (purchased extra_character_slots are added); the login server's Login:MaxCharactersPerAccount must match",
            config.characters.max_per_account
        );
        tracing::info!(
            "Character pre-select period {}; the login server's Login:PreSelectCharacterPeriod must match",
            if config.characters.pre_select_period {
                "on"
            } else {
                "off"
            }
        );
        let ids = Arc::new(IdManagers::default());
        let world = Arc::new(WorldManager::default());
        let reply_delay = ReplyDelay::resolve(&config.debug.reply_delay);
        // WorldManager.CreateStaticInstances: main_world as instance 0 (dungeon instances from
        // the configuration are a later phase).
        if let Some(t) = game_data.worlds.get_world_template_by_name("main_world") {
            world.create_world_instance(
                Arc::new(t.clone()),
                crate::models::character::DEFAULT_INSTANCE_ID,
            );
        }
        let names = NameManager::new(config.character_name_regex.as_deref());
        if config.character_name_use_allowed_chars {
            if game_data.characters.allowed_name_chars.is_empty() {
                tracing::warn!(
                    "CharacterNameUseAllowedChars is enabled but compact.sqlite3 has no allowed_name_chars rows; keeping the regex-only rule"
                );
            } else {
                tracing::info!(
                    "CharacterNameUseAllowedChars is enabled: the 1.2 allowed_name_chars whitelist overrides CharacterNameRegex for names it does not list (digits, Cyrillic)"
                );
                names.set_allowed_name_chars(
                    game_data.characters.allowed_name_chars.iter().copied(),
                );
            }
        }
        Self {
            names: Arc::new(names),
            chat: ChatManager::new(&ids.chat),
            ids,
            items: Arc::new(ItemManager::default()),
            world,
            time: TimeManager::default(),
            expeditions: ExpeditionManager::default(),
            friends: FriendManager::default(),
            save: SaveManager::default(),
            systems: crate::systems::Systems::default(),
            config: config.into(),
            db,
            game_data: Arc::new(game_data),
            registry: crate::packets::build_registry(),
            connections: ConnectionTable::default(),
            reply_delay,
            enter_world: EnterWorldManager::default(),
            enter_world_throttle: EnterWorldThrottle::default(),
            accounts: AccountManager::default(),
            streams: StreamManager::default(),
            login: LoginLink::default(),
            self_ref: std::sync::OnceLock::new(),
            next_connection_id: AtomicU32::new(1),
            occupancy_sequence: std::sync::Mutex::new(0),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// `true` while the graceful shutdown is returning players to the world list.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn set_shutting_down(&self, value: bool) {
        self.shutting_down.store(value, Ordering::SeqCst);
    }

    /// Registers the shared handle (call once after wrapping the server in an `Arc`).
    pub fn set_self(self: &Arc<Self>) {
        let _ = self.self_ref.set(Arc::downgrade(self));
    }

    /// The shared handle registered with [`GameServer::set_self`].
    pub fn self_arc(&self) -> Option<Arc<GameServer>> {
        self.self_ref.get().and_then(std::sync::Weak::upgrade)
    }

    /// The context the visibility packets are built with (`world::ViewCtx`).
    pub fn view_ctx(&self) -> crate::world::ViewCtx<'_> {
        crate::world::ViewCtx {
            data: &self.game_data,
            names: &self.names,
            now_ms: crate::world::now_ms(),
        }
    }

    /// Session ids. C# uses `RemoteEndPoint.GetHashCode()`; any unique u32 serves the same purpose.
    pub fn next_connection_id(&self) -> u32 {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }

    /// A random nonzero reconnect token that no live session currently owns. It is handed to the
    /// login server and the client (`SCReconnectAuth`); the login server binds it to the account
    /// and client IP and accepts it once.
    pub fn new_reconnect_token(&self) -> u32 {
        loop {
            let token = rand::random::<u32>();
            if token != 0 && self.connections.get_by_reconnect_token(token).is_none() {
                return token;
            }
        }
    }

    /// `LoadTask.Execute`: 0 = LOW, 1 = MIDDLE, 2 = HIGH.
    pub fn load_level(&self) -> u8 {
        let rate = self.accounts.count() as f64 / f64::from(self.config.network.num_connections);
        if rate >= 0.6 {
            2
        } else if rate >= 0.3 {
            1
        } else {
            0
        }
    }

    /// Slot occupancy reported to the login server's world queue: accounts in the lobby or world
    /// plus world cookies the client has not consumed yet (they expire, see
    /// `EnterWorldManager::PENDING_TOKEN_LIFETIME`).
    pub fn world_occupancy(&self) -> u32 {
        let accounts = u32::try_from(self.accounts.count()).unwrap_or(u32::MAX);
        let pending = u32::try_from(self.enter_world.pending_count()).unwrap_or(u32::MAX);
        accounts.saturating_add(pending)
    }

    /// The next occupancy report as `(sequence, occupancy)`. The sequence lock serialises the
    /// snapshot with its order, so the login server can discard a report that was taken before a
    /// newer one even when the two are sent from different tasks.
    pub fn next_occupancy(&self) -> (u32, u32) {
        let mut sequence = self
            .occupancy_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *sequence = sequence.wrapping_add(1);
        if *sequence == 0 {
            *sequence = 1;
        }
        let sequence = *sequence;
        (sequence, self.world_occupancy())
    }

    /// Decodes and dispatches one frame payload for `conn`.
    ///
    /// The C# handler only creates packet tables for levels 1 and 2 ("For 1.2 client we only have
    /// Level1 and Level2 packets"); `_packets[level]` on any other level throws
    /// `KeyNotFoundException` and the connection is shut down, which the returned error mirrors.
    pub async fn handle_frame(&self, conn: &mut GameConnection, payload: &[u8]) -> Result<()> {
        let raw = decode_frame(payload)?;
        if raw.level != LEVEL_PLAIN && raw.level != LEVEL_PROXY {
            return Err(GameError::Protocol(format!(
                "no packet table for level {}",
                raw.level
            )));
        }
        // Until `X2EnterWorld` succeeded (the account id is set) the only packet a peer may send
        // is `X2EnterWorld` itself; every lobby handler assumes an authenticated account.
        // (Rust addition; the 1.2 client and the headless bots send nothing else first.)
        if conn.account_id() == 0
            && !(raw.level == LEVEL_PLAIN && raw.opcode == opcodes::cs::X2EnterWorldPacket)
        {
            tracing::debug!(
                "session {}: packet 0x{:03x}(level {}) before X2EnterWorld from {}, closing",
                conn.id,
                raw.opcode,
                raw.level,
                conn.ip
            );
            conn.shutdown();
            return Ok(());
        }
        if tracing::enabled!(
            target: crate::net::FIELDS_TRACE_TARGET,
            tracing::Level::TRACE
        ) {
            let names = if raw.level == LEVEL_PROXY {
                opcodes::pp::NAMES
            } else {
                opcodes::cs::NAMES
            };
            let name = opcodes::name_of(names, raw.opcode).unwrap_or("unknown");
            tracing::trace!(
                target: crate::net::FIELDS_TRACE_TARGET,
                "C->S level={} opcode=0x{:03X} {} len={} body={}",
                raw.level,
                raw.opcode,
                name,
                raw.body.unread().len(),
                crate::net::body_hex(raw.body.unread())
            );
        }
        match self.registry.get(raw.level, raw.opcode) {
            Some(reg) => {
                let started = std::time::Instant::now();
                // `Debug:ReplyDelay` "replies to C->S X": task-local, so only this connection
                // task's sends while the handler runs are replies (not other tasks' broadcasts).
                let res = crate::net::reply_delay::with_reply_context(
                    conn.id,
                    raw.level,
                    raw.opcode,
                    (reg.handler)(self, conn, raw.body),
                )
                .await;
                // Achievement events queued under foreign locks run now that the handler is done.
                crate::systems::economy::flush_deferred_glider_reequips(self);
                crate::systems::flush_achievements(self);
                let took = started.elapsed();
                if took > std::time::Duration::from_millis(250) {
                    let names = if raw.level == LEVEL_PROXY {
                        opcodes::pp::NAMES
                    } else {
                        opcodes::cs::NAMES
                    };
                    tracing::warn!(
                        "Slow packet handler: {} took {} ms (session {})",
                        opcodes::name_of(names, raw.opcode).unwrap_or("?"),
                        took.as_millis(),
                        conn.id
                    );
                }
                res
            }
            None => {
                let names = if raw.level == LEVEL_PROXY {
                    opcodes::pp::NAMES
                } else {
                    opcodes::cs::NAMES
                };
                let known = opcodes::name_of(names, raw.opcode)
                    .map(|n| format!(" ({n}: not ported yet)"))
                    .unwrap_or_default();
                let dump = aaemu_commons::net::hex_dump(raw.body.unread());
                if conn.note_bad_packet() {
                    tracing::error!(
                        "Unknown packet 0x{:02x}({}){} from {}:\n{}",
                        raw.opcode,
                        raw.level,
                        known,
                        conn.ip,
                        dump
                    );
                } else {
                    tracing::debug!(
                        "Unknown packet 0x{:02x}({}){} from {}:\n{}",
                        raw.opcode,
                        raw.level,
                        known,
                        conn.ip,
                        dump
                    );
                }
                Ok(())
            }
        }
    }
}
