//! Client network: `LoginConnection`, `LoginSession` (the login state machine), `LoginClient`,
//! `LoginConnectionTable` and the listener (`LoginConnectionHandler`).

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaemu_commons::net::FrameCodec;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;

use crate::auth::{AuthFlow, AuthFlowResult, KoreaStep, LoginClient};
use crate::config::LoginConfig;
use crate::controllers::game::{GameController, GameServer};
use crate::controllers::queue::Admission;
use crate::handlers;
use crate::id_manager::{ConnectionIdLease, IdError, SimpleIdManager};
use crate::models::{AccountId, ConnectionId, GameServerId, LoginCharacterInfo, LoginDeniedReason};
use crate::packets::c2l::C2lPacket;
use crate::packets::l2c::{
    AcAuthResponse, AcChallenge, AcChallenge2, AcEnterOtp, AcEnterPcCert, AcEnterWorldDenied,
    AcJoinResponse, AcLoginDenied, AcShowArs, AcWorldCookie, AcWorldList, AfsValue,
    JoinResponseReason, WorldInfo,
};
use crate::packets::{dump_unread, split_opcode, OutgoingPacket, PacketError};
use crate::server::LoginServer;

/// The state of a login session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginState {
    Connected,
    Authenticating,
    Authenticated,
    /// Waiting in the world login queue (`ACWorldQueuePacket`), not yet asked to enter.
    Queued,
    EnteringWorld,
    Disconnected,
}

/// How long an admitted request keeps its world-queue slot reservation when the game server never
/// answers (`EnterWorldTimeout` normally ends the wait much earlier; the timer is for a lost
/// response). Normal responses release the reservation immediately.
const RESERVATION_TIMEOUT: Duration = Duration::from_secs(30);

/// `EnterWorldResult`.
#[derive(Clone, Debug)]
pub struct EnterWorldResult {
    pub success: bool,
    pub server: Option<GameServer>,
    pub denial_reason: u8,
    /// The game server's world cookie; 0 = none (the connection id is sent instead).
    pub cookie: u32,
}

struct PendingEnterWorld {
    gs_id: GameServerId,
    cancel: CancellationToken,
    result_tx: Option<oneshot::Sender<EnterWorldResult>>,
}

struct SessionState {
    state: LoginState,
    current_flow: Option<AuthFlow>,
    enter_world: Option<PendingEnterWorld>,
    background: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct AccountData {
    account_id: AccountId,
    account_name: Option<String>,
    last_login: Option<chrono::DateTime<chrono::Utc>>,
    last_ip: Option<IpAddr>,
    /// Characters per game server, in insertion order.
    characters: Vec<(GameServerId, Vec<LoginCharacterInfo>)>,
}

/// A client connection plus its login session (`LoginConnection` + `LoginSession`, which are
/// 1:1 in the C# code). The session members are thread-safe.
pub struct LoginConnection {
    lease: ConnectionIdLease,
    ip: IpAddr,
    is_locally_connected: bool,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    closed: CancellationToken,
    data: Mutex<AccountData>,
    session: Mutex<SessionState>,
    /// The character-slot and pre-select values `send_auth_success` advertises.
    limits: LoginConfig,
}

impl std::fmt::Debug for LoginConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginConnection")
            .field("id", &self.id())
            .field("ip", &self.ip)
            .finish_non_exhaustive()
    }
}

impl LoginConnection {
    pub fn new(
        lease: ConnectionIdLease,
        ip: IpAddr,
        is_locally_connected: bool,
        tx: mpsc::UnboundedSender<Vec<u8>>,
        limits: LoginConfig,
    ) -> Self {
        Self {
            lease,
            ip,
            is_locally_connected,
            tx,
            closed: CancellationToken::new(),
            data: Mutex::new(AccountData::default()),
            session: Mutex::new(SessionState {
                state: LoginState::Connected,
                current_flow: None,
                enter_world: None,
                background: None,
            }),
            limits,
        }
    }

    /// `LoginConnectionFactory.Create`: rents a connection id and creates the connection for a
    /// client that connected from `remote_ip` to the local address `local_ip` (if known). The
    /// connection is local when both addresses are equal.
    pub fn create(
        ids: &Arc<SimpleIdManager>,
        local_ip: Option<IpAddr>,
        remote_ip: IpAddr,
        tx: mpsc::UnboundedSender<Vec<u8>>,
        limits: LoginConfig,
    ) -> Result<Self, IdError> {
        let lease = ConnectionIdLease::rent(ids)?;
        let is_local = local_ip == Some(remote_ip);
        Ok(Self::new(lease, remote_ip, is_local, tx, limits))
    }

    pub fn id(&self) -> ConnectionId {
        self.lease.id()
    }

    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// Whether the client connected from the same machine (local address == remote address).
    pub fn is_locally_connected(&self) -> bool {
        self.is_locally_connected
    }

    /// Triggered when the client connection is closed.
    pub fn connection_closed(&self) -> &CancellationToken {
        &self.closed
    }

    pub fn account_id(&self) -> AccountId {
        self.data.lock().unwrap().account_id
    }

    pub fn account_name(&self) -> Option<String> {
        self.data.lock().unwrap().account_name.clone()
    }

    pub fn last_ip(&self) -> Option<IpAddr> {
        self.data.lock().unwrap().last_ip
    }

    pub fn last_login(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.data.lock().unwrap().last_login
    }

    pub fn state(&self) -> LoginState {
        self.session.lock().unwrap().state
    }

    /// Queues a packet for the writer task.
    pub fn send_packet<P: OutgoingPacket>(&self, packet: &P) -> Result<(), PacketError> {
        let payload = packet.encode_payload()?;
        if self.tx.send(payload).is_err() {
            tracing::debug!(
                "LoginConnection {}: dropping packet 0x{:x}, connection closed",
                self.id(),
                P::OPCODE
            );
        }
        Ok(())
    }

    /// Aborts the connection.
    pub fn shutdown(&self) {
        self.closed.cancel();
    }

    pub fn has_characters(&self, gs_id: GameServerId) -> bool {
        self.data
            .lock()
            .unwrap()
            .characters
            .iter()
            .any(|(id, _)| *id == gs_id)
    }

    pub fn get_characters(&self) -> Vec<LoginCharacterInfo> {
        self.data
            .lock()
            .unwrap()
            .characters
            .iter()
            .flat_map(|(_, c)| c.iter().cloned())
            .collect()
    }

    /// Adds the known characters of the account on a game server. Like the C#
    /// `Dictionary.Add`, adding a game server twice is an error.
    pub fn add_characters(
        &self,
        gs_id: GameServerId,
        mut characters: Vec<LoginCharacterInfo>,
    ) -> anyhow::Result<()> {
        for c in &mut characters {
            c.gs_id = gs_id.0;
        }
        let mut data = self.data.lock().unwrap();
        if data.characters.iter().any(|(id, _)| *id == gs_id) {
            anyhow::bail!("An item with the same key has already been added. Key: {gs_id}");
        }
        data.characters.push((gs_id, characters));
        Ok(())
    }

    // ---- LoginSession ------------------------------------------------------------------------

    /// Starts a new authentication flow: validates the session is `Connected`, moves to
    /// `Authenticating`, runs the flow and sends the success/denied packet.
    ///
    /// If the flow fails with an error (C#: an exception), the session stays in
    /// `Authenticating`, as in the C# implementation.
    pub async fn authenticate(
        &self,
        server: &LoginServer,
        mut flow: AuthFlow,
    ) -> anyhow::Result<()> {
        let can_proceed = {
            let mut s = self.session.lock().unwrap();
            if s.state != LoginState::Connected {
                tracing::warn!(
                    "Cannot authenticate: invalid state {:?} for connection {}",
                    s.state,
                    self.id()
                );
                false
            } else {
                s.state = LoginState::Authenticating;
                true
            }
        };
        if !can_proceed {
            self.send_auth_result(&AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE))?;
            return Ok(());
        }
        let closes_on_denial = flow.denial_closes_connection();
        let result = match flow.start(server, self).await {
            Ok(result) => result,
            Err(e) => {
                if closes_on_denial {
                    self.session.lock().unwrap().state = LoginState::Disconnected;
                    self.shutdown();
                }
                return Err(e);
            }
        };
        self.process_auth_result(&result, flow);
        self.send_auth_result(&result)?;
        if closes_on_denial && matches!(result, AuthFlowResult::Denied(_)) {
            // Queue the denial (the writer flushes it), then close: a reconnect token is
            // single-use, so the same socket must not retry.
            self.session.lock().unwrap().state = LoginState::Disconnected;
            self.shutdown();
        }
        Ok(())
    }

    /// Continues the in-progress (Korea) flow: validates the session is `Authenticating` and that
    /// the current flow supports `step`; a mismatch disconnects the client.
    pub async fn continue_auth(&self, server: &LoginServer, step: KoreaStep) -> anyhow::Result<()> {
        let flow = {
            let mut s = self.session.lock().unwrap();
            if s.state != LoginState::Authenticating {
                tracing::warn!(
                    "Cannot continue auth: invalid state {:?} for connection {}",
                    s.state,
                    self.id()
                );
                None
            } else {
                match s.current_flow.take() {
                    Some(flow) if flow.awaits_continuation() => Some(flow),
                    other => {
                        tracing::warn!(
                            "Auth flow type mismatch: expected {}, got {} for connection {}",
                            step.interface_name(),
                            other.as_ref().map_or("null", AuthFlow::name),
                            self.id()
                        );
                        s.current_flow = None;
                        s.state = LoginState::Disconnected;
                        self.shutdown();
                        None
                    }
                }
            }
        };
        let Some(mut flow) = flow else {
            self.send_auth_result(&AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE))?;
            return Ok(());
        };
        let result = match flow.step(server, self, step).await {
            Ok(r) => r,
            Err(e) => {
                // C#: the exception leaves the flow in place and the state unchanged.
                self.session.lock().unwrap().current_flow = Some(flow);
                return Err(e);
            }
        };
        self.process_auth_result(&result, flow);
        self.send_auth_result(&result)?;
        Ok(())
    }

    fn send_auth_result(&self, result: &AuthFlowResult) -> Result<(), PacketError> {
        match result {
            AuthFlowResult::Success { account_id, .. } => self.send_auth_success(*account_id),
            AuthFlowResult::Denied(reason) => self.send_auth_denied(*reason),
            // Pending: flow is managing itself, nothing to send
            AuthFlowResult::Pending => Ok(()),
        }
    }

    fn process_auth_result(&self, result: &AuthFlowResult, flow: AuthFlow) {
        let mut s = self.session.lock().unwrap();
        match result {
            AuthFlowResult::Success {
                account_id,
                username,
            } => {
                {
                    let mut d = self.data.lock().unwrap();
                    d.account_id = *account_id;
                    if let Some(name) = username {
                        d.account_name = Some(name.clone());
                    }
                    d.last_login = Some(chrono::Utc::now());
                    d.last_ip = Some(self.ip);
                }
                s.current_flow = None;
                s.state = LoginState::Authenticated;
            }
            AuthFlowResult::Denied(_) => {
                s.current_flow = None;
                s.state = LoginState::Connected;
            }
            AuthFlowResult::Pending => {
                s.current_flow = Some(flow);
                // Stay in Authenticating
            }
        }
    }

    /// Initiates entering the world on `gs_id`: either queues the client (`ACWorldQueuePacket`)
    /// when the world is at its configured capacity, refuses it when the queue is full, or
    /// requests the player slot from the game server and waits for its answer in the background,
    /// which then sends the world cookie or a denial.
    ///
    /// The queue decision and the session transition happen under the session lock; the queue
    /// controller's lock is only ever taken inside it here, and the controller never calls back
    /// into the session while holding its own lock (see `GameController::drain_queues`).
    pub async fn initiate_enter_world(
        self: &Arc<Self>,
        server: &Arc<LoginServer>,
        gs_id: GameServerId,
    ) -> anyhow::Result<()> {
        let (decision, pending) = {
            let mut s = self.session.lock().unwrap();
            if s.state != LoginState::Authenticated {
                tracing::warn!(
                    "Cannot enter world: invalid state {:?} for connection {}",
                    s.state,
                    self.id()
                );
                return Ok(());
            }
            if !server
                .games
                .get_game_server(gs_id)
                .is_some_and(|g| g.active())
            {
                tracing::warn!(
                    "Cannot enter world: game server {} not available for connection {}",
                    gs_id,
                    self.id()
                );
                return Ok(());
            }
            let decision = server.games.try_enter_world(gs_id, self.id());
            let pending = match decision {
                Admission::Direct { .. } => {
                    s.state = LoginState::EnteringWorld;
                    let cancel = self.closed.child_token();
                    let (tx, rx) = oneshot::channel();
                    s.enter_world = Some(PendingEnterWorld {
                        gs_id,
                        cancel: cancel.clone(),
                        result_tx: Some(tx),
                    });
                    Some((rx, cancel))
                }
                Admission::Queued => {
                    s.state = LoginState::Queued;
                    None
                }
                Admission::Full => None,
            };
            (decision, pending)
        };

        match (decision, pending) {
            (Admission::Direct { reservation }, Some((rx, cancel))) => {
                self.start_enter_world_request(server, gs_id, rx, cancel, reservation);
            }
            (Admission::Queued, _) => {
                tracing::debug!("World {} is full: connection {} queued", gs_id, self.id());
                server.games.notify_queue(gs_id);
            }
            (Admission::Full, _) => {
                // The 1.2 client's ACEnterWorldDenied handler (FUN_393d51d0) ignores the reason
                // byte and shows "error_failed_to_connect_to_server"; the packet is the evidenced
                // refusal.
                tracing::info!(
                    "World {} queue is full: connection {} refused",
                    gs_id,
                    self.id()
                );
                self.send_enter_world_denied(0)?;
            }
            (Admission::Direct { .. }, None) => unreachable!("direct admission has a pending flow"),
        }
        Ok(())
    }

    /// Continues a queued client into the normal enter-world flow after a slot freed.
    /// Returns false when the client left the queue (cancel, disconnect, or it was never queued).
    pub async fn admit_from_queue(
        self: &Arc<Self>,
        server: &Arc<LoginServer>,
        gs_id: GameServerId,
        generation: u64,
    ) -> bool {
        let (rx, cancel) = {
            let mut s = self.session.lock().unwrap();
            if s.state != LoginState::Queued {
                return false;
            }
            s.state = LoginState::EnteringWorld;
            let cancel = self.closed.child_token();
            let (tx, rx) = oneshot::channel();
            s.enter_world = Some(PendingEnterWorld {
                gs_id,
                cancel: cancel.clone(),
                result_tx: Some(tx),
            });
            (rx, cancel)
        };
        tracing::debug!(
            "World {} admitted connection {} from the queue",
            gs_id,
            self.id()
        );
        self.start_enter_world_request(server, gs_id, rx, cancel, Some(generation));
        true
    }

    fn start_enter_world_request(
        self: &Arc<Self>,
        server: &Arc<LoginServer>,
        gs_id: GameServerId,
        rx: oneshot::Receiver<EnterWorldResult>,
        cancel: CancellationToken,
        reservation: Option<u64>,
    ) {
        server
            .games
            .request_enter_world(self.account_id(), self.id(), gs_id);

        let handle = tokio::spawn(run_enter_world_background(
            self.clone(),
            server.config.enter_world_timeout,
            rx,
            cancel,
        ));
        self.session.lock().unwrap().background = Some(handle);

        if let Some(generation) = reservation {
            // A game response normally releases this reservation (with the occupancy that
            // includes the pending cookie); this is the fallback for a lost response. Only this
            // generation is released: the connection id may be reused for a newer admission.
            let server = server.clone();
            let connection_id = self.id();
            let timeout = server.config.enter_world_timeout.max(RESERVATION_TIMEOUT);
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                server
                    .games
                    .release_reservation(gs_id, connection_id, generation);
                server.games.drain_queues(&server).await;
            });
        }
    }

    /// Cancels an in-progress enter world request or leaves the world queue.
    pub async fn cancel_enter_world(self: &Arc<Self>, server: &Arc<LoginServer>) {
        let was_queued = {
            let mut s = self.session.lock().unwrap();
            match s.state {
                LoginState::Queued => {
                    s.state = LoginState::Authenticated;
                    true
                }
                LoginState::EnteringWorld => {
                    tracing::debug!("Cancelling EnterWorld for connection {}", self.id());
                    if let Some(p) = &s.enter_world {
                        p.cancel.cancel();
                    }
                    false
                }
                _ => {
                    tracing::debug!(
                        "CancelEnterWorld ignored: {:?} state for connection {}",
                        s.state,
                        self.id()
                    );
                    return;
                }
            }
        };
        if was_queued {
            tracing::debug!("Connection {} left the world queue", self.id());
            server.games.cancel_queued(self.id());
            server.games.drain_queues(server).await;
        }
    }

    /// The queued client's world went away (game server unregistered): answer the pending
    /// enter-world request with the refusal packet and return the session to character select.
    pub fn world_unavailable(&self) {
        let mut s = self.session.lock().unwrap();
        if s.state == LoginState::Queued {
            s.state = LoginState::Authenticated;
            let _ = self.send_enter_world_denied(0);
        }
    }

    /// Completes a pending enter world request with the game server's result (0 = success).
    pub fn complete_enter_world_request(
        &self,
        games: &GameController,
        gs_id: GameServerId,
        result: u8,
        cookie: u32,
    ) {
        let mut s = self.session.lock().unwrap();
        if s.state != LoginState::EnteringWorld {
            tracing::warn!(
                "CompleteEnterWorldRequest ignored: not in EnteringWorld state for connection {}",
                self.id()
            );
            return;
        }
        let pending_gs = s.enter_world.as_ref().map(|p| p.gs_id);
        // The game link answers with the game server's own id. A client that selected a mirror
        // gets the mirror's id back from here, so accept the parent id for a mirror request.
        let is_mirror_response = pending_gs.is_some_and(|pending| {
            pending != gs_id && games.try_get_parent_id(pending) == Some(gs_id)
        });
        if pending_gs != Some(gs_id) && !is_mirror_response {
            tracing::warn!(
                "CompleteEnterWorldRequest ignored: gsId mismatch (expected {:?}, got {}) for connection {}",
                pending_gs,
                gs_id,
                self.id()
            );
            return;
        }
        let success = result == 0;
        // The world cookie must point at the world the client selected (the mirror's configured
        // address), not at the id the game happens to answer with.
        let server = if success {
            pending_gs
                .and_then(|pending| games.get_game_server(pending))
                .or_else(|| games.get_game_server(gs_id))
        } else {
            None
        };
        if let Some(tx) = s.enter_world.as_mut().and_then(|p| p.result_tx.take()) {
            let _ = tx.send(EnterWorldResult {
                success,
                server,
                denial_reason: result,
                cookie,
            });
        }
    }

    /// Takes the handle of the background task started by [`Self::initiate_enter_world`]
    /// (`LoginSession.EnterWorldBackgroundTask` in C#), for tests to await it.
    #[cfg(test)]
    pub(crate) fn take_enter_world_task(&self) -> Option<JoinHandle<()>> {
        self.session.lock().unwrap().background.take()
    }

    /// Marks the session as disconnected and cancels any pending operations.
    pub async fn disconnect(&self) {
        let background = {
            let mut s = self.session.lock().unwrap();
            if s.state == LoginState::Disconnected {
                return;
            }
            tracing::debug!("Session disconnected for connection {}", self.id());
            // Cancel any pending enter world operation
            if let Some(p) = &s.enter_world {
                p.cancel.cancel();
            }
            s.enter_world = None;
            s.state = LoginState::Disconnected;
            s.background.take()
        };
        if let Some(handle) = background {
            // Ignore errors during cleanup
            let _ = handle.await;
        }
    }
}

async fn run_enter_world_background(
    conn: Arc<LoginConnection>,
    timeout: Duration,
    rx: oneshot::Receiver<EnterWorldResult>,
    cancel: CancellationToken,
) {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            tracing::debug!("EnterWorld cancelled for connection {}", conn.id());
        }
        r = tokio::time::timeout(timeout, rx) => match r {
            Ok(Ok(result)) => {
                let sent = match (&result.success, &result.server) {
                    (true, Some(gs)) => {
                        // An old game server sends no cookie: fall back to the connection id.
                        let cookie = if result.cookie == 0 { conn.id().0 } else { result.cookie };
                        conn.send_world_cookie(cookie, &gs.host, gs.port)
                    }
                    _ => conn.send_enter_world_denied(result.denial_reason),
                };
                if let Err(e) = sent {
                    tracing::error!("EnterWorld result for connection {} could not be sent: {}", conn.id(), e);
                }
            }
            // The pending state was cleaned up (disconnect) before a result arrived.
            Ok(Err(_)) => tracing::debug!("EnterWorld cancelled for connection {}", conn.id()),
            Err(_) => {
                tracing::warn!("EnterWorld timed out for connection {}", conn.id());
                // Connection may be closed
                let _ = conn.send_enter_world_denied(0);
            }
        }
    }
    let mut s = conn.session.lock().unwrap();
    s.enter_world = None;
    if s.state == LoginState::EnteringWorld {
        s.state = LoginState::Authenticated;
    }
}

impl LoginClient for LoginConnection {
    fn send_auth_success(&self, account_id: AccountId) -> Result<(), PacketError> {
        self.send_packet(&AcJoinResponse::new(
            JoinResponseReason::Success,
            AfsValue {
                max_characters_per_account: self.limits.max_characters_per_account,
                additional_characters_per_server: self.limits.additional_characters_per_server,
                is_pre_select_character_period: self.limits.pre_select_character_period,
            },
        ))?;
        self.send_packet(&AcAuthResponse {
            account_id,
            slot_count: self.limits.max_characters_per_account,
        })
    }

    fn send_auth_denied(&self, reason: LoginDeniedReason) -> Result<(), PacketError> {
        self.send_packet(&AcLoginDenied::new(reason))
    }

    fn send_world_list(
        &self,
        worlds: Vec<WorldInfo>,
        characters: Vec<LoginCharacterInfo>,
    ) -> Result<(), PacketError> {
        self.send_packet(&AcWorldList { worlds, characters })
    }

    fn send_challenge(&self, salt: u32, hc: &[u32]) -> Result<(), PacketError> {
        self.send_packet(&AcChallenge {
            salt,
            hc: hc.to_vec(),
        })
    }

    fn send_challenge2(&self, round: i32, salt: &str, hc: &[u32]) -> Result<(), PacketError> {
        self.send_packet(&AcChallenge2 {
            round,
            salt: salt.to_string(),
            hc: hc.to_vec(),
        })
    }

    fn send_otp_prompt(&self, maximum_tries: i32, current_try: i32) -> Result<(), PacketError> {
        let p = if current_try == 0 {
            AcEnterOtp::initial()
        } else {
            AcEnterOtp::failure(maximum_tries, current_try)
        };
        self.send_packet(&p)
    }

    fn send_cert_prompt(&self, maximum_tries: i32, current_try: i32) -> Result<(), PacketError> {
        let p = if current_try == 0 {
            AcEnterPcCert::initial()
        } else {
            AcEnterPcCert::failure(maximum_tries, current_try)
        };
        self.send_packet(&p)
    }

    fn send_ars_prompt(&self, session_code: &str, timeout: Duration) -> Result<(), PacketError> {
        self.send_packet(&AcShowArs {
            number: session_code.to_string(),
            timeout,
        })
    }

    fn send_world_cookie(&self, cookie: u32, host: &str, port: u16) -> Result<(), PacketError> {
        self.send_packet(&AcWorldCookie::new(
            cookie,
            self.ip,
            self.is_locally_connected,
            host,
            port,
        ))
    }

    fn send_enter_world_denied(&self, reason: u8) -> Result<(), PacketError> {
        self.send_packet(&AcEnterWorldDenied { reason })
    }
}

/// `LoginConnectionTable`.
#[derive(Debug, Default)]
pub struct LoginConnectionTable {
    connections: DashMap<ConnectionId, Arc<LoginConnection>>,
}

impl LoginConnectionTable {
    pub fn add(&self, con: Arc<LoginConnection>) -> anyhow::Result<()> {
        match self.connections.entry(con.id()) {
            dashmap::Entry::Occupied(_) => {
                anyhow::bail!("Connection with the same ID already exists.")
            }
            dashmap::Entry::Vacant(v) => {
                v.insert(con);
                Ok(())
            }
        }
    }

    pub fn get(&self, id: ConnectionId) -> Option<Arc<LoginConnection>> {
        self.connections.get(&id).map(|c| c.clone())
    }

    pub fn remove(&self, id: ConnectionId) -> Option<Arc<LoginConnection>> {
        self.connections.remove(&id).map(|(_, c)| c)
    }

    pub fn all(&self) -> Vec<Arc<LoginConnection>> {
        self.connections.iter().map(|c| c.clone()).collect()
    }
}

/// Binds the client listener (`*` = all IPv4 interfaces, like `IPAddress.Any`).
pub async fn bind(host: &str, port: u16) -> std::io::Result<TcpListener> {
    let ip: IpAddr = if host == "*" {
        IpAddr::from([0, 0, 0, 0])
    } else {
        host.parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
    };
    TcpListener::bind(SocketAddr::new(ip, port)).await
}

/// Accepts client connections until `shutdown` is cancelled.
pub async fn run(server: Arc<LoginServer>, listener: TcpListener, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, addr)) => {
                    let server = server.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move { handle_connection(server, stream, addr, shutdown).await });
                }
                Err(e) => tracing::error!("Login listener accept error: {}", e),
            }
        }
    }
}

async fn handle_connection(
    server: Arc<LoginServer>,
    stream: TcpStream,
    addr: SocketAddr,
    shutdown: CancellationToken,
) {
    let _ = stream.set_nodelay(true);
    // checks if a connection is from the same machine
    let local_ip = stream.local_addr().ok().map(|l| l.ip());
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let conn = match LoginConnection::create(
        &server.connection_ids,
        local_ip,
        addr.ip(),
        tx,
        server.config.login,
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!("Error on connection from {}: {}", addr, e);
            return;
        }
    };
    tracing::debug!(
        "Connection from {} established, connection ID: {}",
        addr,
        conn.id()
    );

    let (read_half, write_half) = stream.into_split();
    let mut reader = FramedRead::new(read_half, FrameCodec);
    let mut writer = FramedWrite::new(write_half, FrameCodec);

    let closed = conn.closed.clone();
    let writer_task = tokio::spawn({
        let closed = closed.clone();
        async move {
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(payload) => {
                            if let Err(e) = writer.send(payload).await {
                                tracing::debug!("login write error: {}", e);
                                closed.cancel();
                                break;
                            }
                        }
                        None => break,
                    },
                    _ = closed.cancelled() => {
                        // Flush whatever was queued before the close (e.g. a final denial).
                        while let Ok(payload) = rx.try_recv() {
                            if writer.send(payload).await.is_err() {
                                break;
                            }
                        }
                        break;
                    }
                }
            }
        }
    });

    match server.connections.add(conn.clone()) {
        Ok(()) => dispatch_messages(&server, &conn, &mut reader, &shutdown).await,
        Err(e) => tracing::error!("Error on connection from {}: {}", addr, e),
    }

    // Shutdown
    conn.shutdown();
    tracing::debug!(
        "LoginConnection {} from {} disconnected",
        conn.id(),
        conn.ip()
    );
    server.connections.remove(conn.id());
    // Entries die with their connection (the queue position updates reach the others).
    server.games.connection_closed(conn.id());
    let _ = writer_task.await;
    // DisposeAsync: notify the session; the id lease is returned when the last Arc drops.
    conn.disconnect().await;
}

async fn dispatch_messages(
    server: &Arc<LoginServer>,
    conn: &Arc<LoginConnection>,
    reader: &mut FramedRead<tokio::net::tcp::OwnedReadHalf, FrameCodec>,
    shutdown: &CancellationToken,
) {
    // Rust addition: a connection that has not authenticated by the deadline is closed, so idle
    // sockets cannot pile up. `0` switches it off. Authenticated connections (world list,
    // character select, queue) are never timed out.
    let pre_auth_timeout =
        Duration::from_secs(server.config.security.pre_auth_timeout_seconds.into());
    let mut pre_auth_pending = !pre_auth_timeout.is_zero();
    let pre_auth_deadline = tokio::time::sleep(pre_auth_timeout);
    tokio::pin!(pre_auth_deadline);
    loop {
        let frame = tokio::select! {
            _ = conn.closed.cancelled() => break,
            _ = shutdown.cancelled() => break,
            () = &mut pre_auth_deadline, if pre_auth_pending => {
                if matches!(conn.state(), LoginState::Connected | LoginState::Authenticating) {
                    tracing::debug!(
                        "LoginConnection {} from {} did not authenticate within {} s, closing",
                        conn.id(),
                        conn.ip(),
                        pre_auth_timeout.as_secs()
                    );
                    break;
                }
                pre_auth_pending = false;
                continue;
            }
            frame = reader.next() => frame,
        };
        let payload = match frame {
            Some(Ok(p)) => p,
            // Client disconnected - this is normal, no need to log as error
            Some(Err(e)) => {
                tracing::debug!("LoginConnection {} read error: {}", conn.id(), e);
                break;
            }
            None => break,
        };
        // TODO (as in C#): could enforce a maximum packet size here.
        let (opcode, mut stream) = match split_opcode(payload) {
            Ok(v) => v,
            Err(e) => {
                // C#: slicing a negative data length throws and ends the connection.
                tracing::error!(
                    "Message dispatch error on login connection {} from {}: {}",
                    conn.id(),
                    conn.ip(),
                    e
                );
                break;
            }
        };
        match C2lPacket::decode(opcode, &mut stream) {
            Ok(Some(packet)) => {
                // Dispatch the packet sequentially
                if let Err(e) = handlers::handle_c2l(server, conn, packet).await {
                    tracing::error!("Error on packet dispatch {}: {:#}", opcode, e);
                }
            }
            Ok(None) => {
                tracing::error!(
                    "Unknown packet 0x{:02x} from {}:\n{}",
                    opcode,
                    conn.ip(),
                    dump_unread(&stream)
                );
            }
            Err(e) => tracing::error!("Error on packet dispatch {}: {}", opcode, e),
        }
    }
}

#[cfg(test)]
mod tests;
