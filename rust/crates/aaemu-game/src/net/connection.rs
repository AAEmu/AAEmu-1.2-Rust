//! Per-connection state (`GameConnection`) and the connection table (`GameConnectionTable`).
//!
//! Each client connection is driven by one tokio task that owns its [`GameConnection`] (so its
//! state needs no locking) plus a writer task fed through its [`Outbox`]. Other tasks (e.g. the
//! login-server link) reach a connection through its [`ConnectionHandle`].

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::models::character::Character;
use crate::models::item::UnixTime;
use crate::net::outbox::{outbox, OutFrame, Outbox, Push, MAX_QUEUED_BYTES};
use crate::net::packet::{encode, ServerPacket, INVALID_OPCODE};
use crate::net::reply_delay::{ReplyDelay, ReplyDelayConn};

/// `GameState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GameState {
    #[default]
    Lobby,
    World,
}

/// `PaymentMethodType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum PaymentMethodType {
    Premium = 1,
    Demo = 3,
    None = 5,
}

impl PaymentMethodType {
    /// The 1.2 `SCAccountInfo.payMethod` value (`FUN_397aeb90`); `None` for an unknown one.
    pub fn from_wire(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::Premium),
            3 => Some(Self::Demo),
            5 => Some(Self::None),
            _ => None,
        }
    }
}

/// `AccountPayment` (the Rust server loads the real payment from `aaemu_game.accounts`; see
/// `systems::services::premium`). The default is the owner decision of 2026-11-03: premium with
/// no end date.
#[derive(Debug, Clone)]
pub struct AccountPayment {
    pub method: PaymentMethodType,
    pub location: i32,
    /// `DateTime.MinValue` -> 0.
    pub start_time: UnixTime,
    /// Unix seconds; `0` = no end date (permanent), as for the default premium plan. GM grants
    /// always carry their own end.
    pub end_time: UnixTime,
}

impl AccountPayment {
    /// `PremiumState`: premium method and `now` inside `[StartTime, EndTime]` (unix seconds),
    /// where `EndTime == 0` means permanent. The C# used `new DateTime(2030, 1, 1)` as a
    /// placeholder end for all accounts; the owner decided on 2026-11-03 that the configured
    /// default premium is permanent.
    pub fn premium_state(&self, now: UnixTime) -> bool {
        self.method == PaymentMethodType::Premium
            && now >= self.start_time
            && (self.end_time == 0 || now <= self.end_time)
    }
}

impl Default for AccountPayment {
    fn default() -> Self {
        Self {
            method: PaymentMethodType::Premium,
            location: 1,
            start_time: 0,
            end_time: 0,
        }
    }
}

/// Commands other tasks can send to a connection task.
#[derive(Debug)]
pub enum ConnCommand {
    Send(Box<dyn ServerPacket>),
    Close,
    /// The logout timer of `EnterWorldManager.Leave` expired: run `LeaveWorldTask` with this
    /// `LeaveWorldTargetType` on the connection task.
    LeaveWorld(u8),
    /// `Connection.ActiveChar = null; Shutdown()` (GM `disconnectme`): the connection closes
    /// without saving / removing its character.
    DetachAndClose,
    /// Graceful server shutdown: leave the world at once (no logout timer) if in it, then ask
    /// the login server for the world list (`session::return_to_world_list`).
    ReturnToWorldList,
}

/// Cloneable handle stored in the [`ConnectionTable`].
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    pub id: u32,
    pub ip: SocketAddr,
    account_id: Arc<AtomicU32>,
    /// `LastPing` as unix milliseconds (`i64::MIN` = `DateTime.MinValue`, never pinged).
    last_ping_ms: Arc<AtomicI64>,
    /// The random token handed to the login server for `SCReconnectAuth` (`0` = none).
    reconnect_token: Arc<AtomicU32>,
    tx: mpsc::UnboundedSender<ConnCommand>,
    writer: Outbox,
    /// `Debug:ReplyDelay` (`None` = off).
    delay: Option<Arc<ReplyDelayConn>>,
}

impl ConnectionHandle {
    /// A handle whose packets go nowhere (C# `Character.Connection == null`): used for
    /// characters that exist before anyone plays them (character creation).
    pub fn detached(id: u32, ip: SocketAddr, account_id: u32) -> Self {
        let (tx, _) = mpsc::unbounded_channel();
        let (writer, _) = outbox();
        Self {
            id,
            ip,
            account_id: Arc::new(AtomicU32::new(account_id)),
            last_ping_ms: Arc::new(AtomicI64::new(i64::MIN)),
            reconnect_token: Arc::new(AtomicU32::new(0)),
            tx,
            writer,
            delay: None,
        }
    }

    pub fn account_id(&self) -> u32 {
        self.account_id.load(Ordering::Relaxed)
    }

    /// The random reconnect token last issued to this connection (`0` = none).
    pub fn reconnect_token(&self) -> u32 {
        self.reconnect_token.load(Ordering::Relaxed)
    }

    /// `LastPing` (`None` = `DateTime.MinValue`: no `PingPacket` received yet).
    pub fn last_ping(&self) -> Option<chrono::NaiveDateTime> {
        let ms = self.last_ping_ms.load(Ordering::Relaxed);
        (ms != i64::MIN)
            .then(|| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.naive_utc()))
            .flatten()
    }

    /// `GameConnection.SendPacket` from any task: the packet is encoded right away and queued
    /// on the connection's writer, so packets sent through handles and through the owning
    /// [`GameConnection`] keep the global call order (C# writes to the socket synchronously).
    pub fn send(&self, packet: impl ServerPacket + 'static) {
        self.send_dyn(&packet);
    }

    /// [`ConnectionHandle::send`] for a borrowed / type-erased packet.
    pub fn send_dyn(&self, packet: &dyn ServerPacket) {
        if let Some(bytes) = encode_for_send(packet) {
            self.send_frame(bytes);
        }
    }

    /// Queues an already encoded frame (see [`encode_for_send`] / [`Fanout`]). A client whose
    /// queue is over [`MAX_QUEUED_BYTES`] stopped reading: the frame is dropped and the
    /// connection closed.
    pub fn send_frame(&self, frame: Bytes) {
        let frame = match &self.delay {
            Some(delay) => delay.frame(frame),
            None => OutFrame::now(frame),
        };
        if let Push::Overflow { first: true } = self.writer.push(frame) {
            tracing::warn!(
                "session {}: more than {} MiB queued for the client, closing the connection",
                self.id,
                MAX_QUEUED_BYTES / (1024 * 1024)
            );
            self.close();
        }
    }

    pub fn close(&self) {
        let _ = self.tx.send(ConnCommand::Close);
    }

    /// `ActiveChar = null` then `Shutdown()` on the connection task.
    pub fn detach_and_close(&self) {
        let _ = self.tx.send(ConnCommand::DetachAndClose);
    }

    /// Queues [`ConnCommand::ReturnToWorldList`] for the connection task.
    pub fn return_to_world_list(&self) {
        let _ = self.tx.send(ConnCommand::ReturnToWorldList);
    }

    /// Records the character for the `Debug:ReplyDelay` character-name filter.
    pub fn observe_character_name(&self, name: &str) {
        if let Some(delay) = &self.delay {
            delay.observe_character_name(name);
        }
    }
}

/// `GameConnection.SendPacket(GamePacket)`: packets with the placeholder opcode `0xFFF` (no
/// 1.2 opcode) are dropped (`None`); everything else is encoded (`GamePacket.Encode`).
pub fn encode_for_send(packet: &dyn ServerPacket) -> Option<Bytes> {
    if packet.opcode() == INVALID_OPCODE {
        tracing::error!("Dropping invalid game packet with opcode 0xFFF.");
        return None;
    }
    let bytes = encode(packet);
    tracing::debug!(
        "GamePacket: S->C type {:03X} {} ({} bytes)",
        packet.opcode(),
        packet.name(),
        bytes.len()
    );
    tracing::trace!("{:?}", packet);
    Some(Bytes::from(bytes))
}

/// One packet sent to many connections (`BroadcastPacket` and friends): encoded once, on the
/// first recipient, and the same frame queued for every connection.
pub struct Fanout<'a> {
    packet: &'a dyn ServerPacket,
    frame: Option<Option<Bytes>>,
}

impl<'a> Fanout<'a> {
    pub fn new(packet: &'a dyn ServerPacket) -> Self {
        Self {
            packet,
            frame: None,
        }
    }

    pub fn send(&mut self, conn: &ConnectionHandle) {
        let packet = self.packet;
        if let Some(frame) = self.frame.get_or_insert_with(|| encode_for_send(packet)) {
            conn.send_frame(frame.clone());
        }
    }
}

/// `GameConnectionTable`: connections keyed by session id.
#[derive(Debug, Default)]
pub struct ConnectionTable {
    connections: DashMap<u32, ConnectionHandle>,
}

impl ConnectionTable {
    pub fn add(&self, handle: ConnectionHandle) {
        self.connections.insert(handle.id, handle);
    }

    pub fn remove(&self, id: u32) -> Option<ConnectionHandle> {
        self.connections.remove(&id).map(|(_, h)| h)
    }

    pub fn get(&self, id: u32) -> Option<ConnectionHandle> {
        self.connections.get(&id).map(|h| h.clone())
    }

    /// `GetConnectionByAccount`: first connection with this account id.
    pub fn get_by_account(&self, account_id: u32) -> Option<ConnectionHandle> {
        self.connections
            .iter()
            .find(|h| h.account_id() == account_id)
            .map(|h| h.clone())
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// `GetConnections()`.
    pub fn all(&self) -> Vec<ConnectionHandle> {
        self.connections.iter().map(|h| h.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// The connection that currently owns `token`, for the `LGPlayerReconnect` acknowledgement.
    pub fn get_by_reconnect_token(&self, token: u32) -> Option<ConnectionHandle> {
        if token == 0 {
            return None;
        }
        self.connections
            .iter()
            .find(|h| h.reconnect_token() == token)
            .map(|h| h.clone())
    }
}

/// `GameConnection`.
pub struct GameConnection {
    pub id: u32,
    pub ip: SocketAddr,
    account_id: Arc<AtomicU32>,
    last_ping_ms: Arc<AtomicI64>,
    pub state: GameState,
    /// Cached copy of `AccountManager::state` (`systems::services::accounts`); the registry / MySQL
    /// is the source of truth, refreshed at login and on every `CSListCharacter`.
    pub payment: AccountPayment,
    /// `Characters.Values` in insertion (load) order.
    pub characters: Vec<Character>,
    /// `ActiveChar`.
    pub active_char: Option<crate::world::PlayerRef>,
    /// `LeaveTask` (+ its `CancelTokenSource`): the pending logout timer.
    pub leave_task: Option<tokio::task::JoinHandle<()>>,
    pub last_ping: Instant,
    /// The random token handed to the login server for this session's reconnect (`0` = none).
    reconnect_token: Arc<AtomicU32>,
    writer: Outbox,
    commands: mpsc::UnboundedSender<ConnCommand>,
    closing: bool,
    /// `Debug:ReplyDelay` (`None` = off).
    delay: Option<Arc<ReplyDelayConn>>,
    /// Last allowed chat time per chat-spam group (`FUN_393C2540`'s `lastSend` array).
    pub chat_spam_last: [Option<std::time::Instant>; 15],
    /// Unknown or unreadable packets seen on this connection (Rust addition, log throttling).
    bad_packets: BadPacketLog,
    /// `CSSpawnCharacter` / `CSNotifyInGame` already ran for the selected character; a replay is
    /// ignored (it would subscribe to the time manager again and re-run the join hooks). Reset
    /// by `select_character`.
    pub spawn_handled: bool,
    pub in_game_notified: bool,
}

/// Unknown or unreadable packets of one connection that are logged at `error` per
/// [`BAD_PACKET_LOG_WINDOW`]; the rest of the window is logged at `debug` (the real client
/// legitimately sends some not-yet-ported opcodes).
pub const BAD_PACKET_ERROR_LOG_LIMIT: u32 = 10;
/// Length of the fixed window of [`BAD_PACKET_ERROR_LOG_LIMIT`]. A window instead of the
/// connection lifetime keeps new unported opcodes visible at `error` in a long session.
pub const BAD_PACKET_LOG_WINDOW: Duration = Duration::from_secs(60);

/// Throttle for the unknown-packet `error` logs of one connection (pure over the caller's clock).
#[derive(Debug, Default)]
pub struct BadPacketLog {
    window_start: Option<Instant>,
    count: u32,
}

impl BadPacketLog {
    /// Counts one bad packet at `now`; `true` while it is still worth an `error` log (the first
    /// [`BAD_PACKET_ERROR_LOG_LIMIT`] of the current window). Logs one `warn` when the window
    /// overflows.
    pub fn note(&mut self, now: Instant, session: u32) -> bool {
        match self.window_start {
            Some(start) if now.saturating_duration_since(start) < BAD_PACKET_LOG_WINDOW => {}
            _ => {
                self.window_start = Some(now);
                self.count = 0;
            }
        }
        self.count = self.count.saturating_add(1);
        if self.count == BAD_PACKET_ERROR_LOG_LIMIT + 1 {
            tracing::warn!(
                "session {session}: more than {BAD_PACKET_ERROR_LOG_LIMIT} unknown or unreadable packets in {} s, logging the rest of the window at debug",
                BAD_PACKET_LOG_WINDOW.as_secs()
            );
        }
        self.count <= BAD_PACKET_ERROR_LOG_LIMIT
    }
}

impl std::fmt::Debug for GameConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameConnection")
            .field("id", &self.id)
            .field("ip", &self.ip)
            .field("account_id", &self.account_id())
            .field("state", &self.state)
            .field("characters", &self.characters.len())
            .finish_non_exhaustive()
    }
}

impl GameConnection {
    /// Creates the connection state and its table handle. `writer` receives encoded frame payloads;
    /// `commands` is the sender half of the connection task's command channel.
    pub fn new(
        id: u32,
        ip: SocketAddr,
        writer: Outbox,
        commands: mpsc::UnboundedSender<ConnCommand>,
    ) -> (Self, ConnectionHandle) {
        let account_id = Arc::new(AtomicU32::new(0));
        let last_ping_ms = Arc::new(AtomicI64::new(i64::MIN));
        let reconnect_token = Arc::new(AtomicU32::new(0));
        let handle = ConnectionHandle {
            id,
            ip,
            account_id: account_id.clone(),
            last_ping_ms: last_ping_ms.clone(),
            reconnect_token: reconnect_token.clone(),
            tx: commands.clone(),
            writer: writer.clone(),
            delay: None,
        };
        let conn = Self {
            id,
            ip,
            account_id,
            last_ping_ms,
            reconnect_token,
            state: GameState::Lobby,
            payment: AccountPayment::default(),
            characters: Vec::new(),
            active_char: None,
            leave_task: None,
            last_ping: Instant::now(),
            writer,
            commands,
            closing: false,
            delay: None,
            chat_spam_last: [None; 15],
            bad_packets: BadPacketLog::default(),
            spawn_handled: false,
            in_game_notified: false,
        };
        (conn, handle)
    }

    /// Counts one unknown or unreadable packet; `true` while it is still worth an `error` log
    /// (the first [`BAD_PACKET_ERROR_LOG_LIMIT`] per [`BAD_PACKET_LOG_WINDOW`]).
    pub fn note_bad_packet(&mut self) -> bool {
        self.bad_packets.note(Instant::now(), self.id)
    }

    /// Attaches `Debug:ReplyDelay` to this connection and every handle made from it.
    pub fn set_reply_delay(&mut self, config: Option<Arc<ReplyDelay>>) {
        let id = self.id;
        self.delay = config.map(|config| ReplyDelayConn::new(id, config));
    }

    /// The `Debug:ReplyDelay` state (the writer needs the strict latch), `None` when off.
    pub(crate) fn reply_delay_state(&self) -> Option<Arc<ReplyDelayConn>> {
        self.delay.clone()
    }

    pub fn account_id(&self) -> u32 {
        self.account_id.load(Ordering::Relaxed)
    }

    /// Issues a fresh random reconnect token for this session (`0` clears it).
    pub fn set_reconnect_token(&mut self, token: u32) {
        self.reconnect_token.store(token, Ordering::Relaxed);
    }

    pub fn set_account_id(&mut self, id: u32) {
        self.account_id.store(id, Ordering::Relaxed);
    }

    /// `Connection.LastPing = DateTime.UtcNow` (`PingPacket`).
    pub fn touch_ping(&mut self) {
        self.last_ping = Instant::now();
        self.last_ping_ms
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
    }

    /// `GameConnection.SendPacket(GamePacket)`: packets with the placeholder opcode `0xFFF` (no
    /// 1.2 opcode) are dropped; everything else is encoded and queued for the writer task.
    pub fn send(&mut self, packet: &dyn ServerPacket) {
        let Some(bytes) = encode_for_send(packet) else {
            return;
        };
        let frame = match &self.delay {
            Some(delay) => delay.frame(bytes),
            None => OutFrame::now(bytes),
        };
        match self.writer.push(frame) {
            Push::Queued => {}
            Push::Closed => self.closing = true,
            Push::Overflow { first } => {
                if first {
                    tracing::warn!(
                        "session {}: more than {} MiB queued for the client, closing the connection",
                        self.id,
                        MAX_QUEUED_BYTES / (1024 * 1024)
                    );
                }
                self.closing = true;
            }
        }
    }

    /// A [`ConnectionHandle`] for this connection (same channels as the table entry).
    pub fn handle(&self) -> ConnectionHandle {
        ConnectionHandle {
            id: self.id,
            ip: self.ip,
            account_id: self.account_id.clone(),
            last_ping_ms: self.last_ping_ms.clone(),
            reconnect_token: self.reconnect_token.clone(),
            tx: self.commands.clone(),
            writer: self.writer.clone(),
            delay: self.delay.clone(),
        }
    }

    /// Queues a command for this connection's own task (e.g. the logout timer).
    pub fn command_sender(&self) -> mpsc::UnboundedSender<ConnCommand> {
        self.commands.clone()
    }

    /// `Shutdown()`: the connection task stops after the current packet.
    pub fn shutdown(&mut self) {
        self.closing = true;
        let _ = self.commands.send(ConnCommand::Close);
    }

    pub fn is_closing(&self) -> bool {
        self.closing
    }
}
