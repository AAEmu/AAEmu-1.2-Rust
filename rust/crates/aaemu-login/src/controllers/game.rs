//! `GameController`: the configured game servers, their registration by game server processes,
//! world lists and enter-world routing.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use dashmap::DashMap;

use super::queue::{Admission, WorldQueue};
use super::request::RequestController;
use crate::config::AppConfig;
use crate::models::{
    AccountId, ConnectionId, GameServerId, GsLoad, GsRegisterResult, LoginCharacterInfo,
};
use crate::network::internal::InternalConnection;
use crate::network::login::{LoginConnection, LoginConnectionTable};
use crate::packets::l2c::{AcWorldQueue, WorldInfo};
use crate::packets::l2g::{LgPlayerEnter, LgRegisterGameServer, LgRequestInfo};
use crate::packets::OutgoingPacket;
use crate::server::LoginServer;

/// `TODO Request 20s` in the C# code.
const WORLD_LIST_REQUEST_TIMEOUT: Duration = Duration::from_millis(20_000);

/// A configured game server (`Models/GameServer.cs`).
#[derive(Clone, Debug)]
pub struct GameServer {
    pub id: GameServerId,
    pub name: String,
    pub host: String,
    pub port: u16,
    /// The registered game server connection (mirrors share their parent's connection).
    pub connection: Option<Arc<InternalConnection>>,
    pub load: GsLoad,
    pub mirrors: Vec<GameServerId>,
}

impl GameServer {
    pub fn new(id: GameServerId, name: String, host: String, port: u16) -> Self {
        Self {
            id,
            name,
            host,
            port,
            connection: None,
            load: GsLoad::default(),
            mirrors: Vec::new(),
        }
    }

    pub fn active(&self) -> bool {
        self.connection.is_some()
    }

    pub fn send_packet<P: OutgoingPacket>(&self, packet: &P) {
        if let Some(c) = &self.connection {
            c.send_packet(packet);
        }
    }

    pub fn world_info(&self) -> WorldInfo {
        WorldInfo {
            id: self.id,
            name: self.name.clone(),
            active: self.active(),
            load: self.load,
        }
    }
}

/// The result of [`GameController::get_world_list`].
#[derive(Clone, Debug)]
pub struct WorldListResult {
    pub game_servers: Vec<GameServer>,
    pub characters: Vec<LoginCharacterInfo>,
}

pub struct GameController {
    requests: Arc<RequestController>,
    connections: Arc<LoginConnectionTable>,
    game_servers: DashMap<GameServerId, GameServer>,
    /// mirror id -> parent id
    mirrors_id: Mutex<HashMap<GameServerId, GameServerId>>,
    character_requests: DashMap<u32, Arc<CharacterRequest>>,
    /// `Login:PreSelectCharacterPeriod`, reported to a registering game server so it applies
    /// the same period the clients are told about.
    pre_select_character_period: std::sync::atomic::AtomicBool,
    /// One login queue per configured world (`GameServers[].MaxConcurrent`/`QueueLimit`).
    queues: Mutex<HashMap<GameServerId, WorldQueue>>,
    /// Worlds whose game server answered without a world cookie (an old binary) and were already
    /// warned about; cleared when the world registers or unregisters.
    legacy_cookie_warned: Mutex<std::collections::HashSet<GameServerId>>,
}

/// The recipient of an LGRequestInfo round-trip. The response has no world id on the wire.
struct CharacterRequest {
    connection_id: ConnectionId,
    gs_id: GameServerId,
    game_connection: Arc<InternalConnection>,
}

/// Resolves `host` to its first IPv4 address; on failure the host is returned un-parsed.
async fn resolve_host_name(host: &str) -> String {
    match tokio::net::lookup_host((host, 0)).await {
        Ok(addrs) => {
            if let Some(v4) = addrs.map(|a| a.ip()).find(IpAddr::is_ipv4) {
                tracing::debug!("Resolved {} to {}", host, v4);
                return v4.to_string();
            }
            tracing::warn!("Unable to resolve {} to an IPv4 address", host);
            host.to_string()
        }
        Err(e) => {
            // in case of errors, just return it un-parsed
            tracing::error!("Exception resolving {}: {}", host, e);
            host.to_string()
        }
    }
}

impl GameController {
    pub fn new(requests: Arc<RequestController>, connections: Arc<LoginConnectionTable>) -> Self {
        Self {
            requests,
            connections,
            game_servers: DashMap::new(),
            mirrors_id: Mutex::new(HashMap::new()),
            character_requests: DashMap::new(),
            pre_select_character_period: std::sync::atomic::AtomicBool::new(false),
            queues: Mutex::new(HashMap::new()),
            legacy_cookie_warned: Mutex::new(std::collections::HashSet::new()),
        }
    }

    fn queues_guard(&self) -> std::sync::MutexGuard<'_, HashMap<GameServerId, WorldQueue>> {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Login:PreSelectCharacterPeriod` (set by [`Self::load`]).
    pub fn pre_select_character_period(&self) -> bool {
        self.pre_select_character_period
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn try_get_parent_id(&self, gs_id: GameServerId) -> Option<GameServerId> {
        self.mirrors_id.lock().unwrap().get(&gs_id).copied()
    }

    /// Loads the non-hidden game servers from the configuration.
    pub async fn load(&self, config: &AppConfig) {
        self.pre_select_character_period.store(
            config.login.pre_select_character_period,
            std::sync::atomic::Ordering::Relaxed,
        );
        for gs in config.game_servers.iter().filter(|gs| !gs.hidden) {
            let id = GameServerId(gs.id);
            let host = if config.skip_host_resolve {
                gs.host.clone()
            } else {
                resolve_host_name(&gs.host).await
            };
            let server = GameServer::new(id, gs.name.clone(), host.clone(), gs.port);
            if self.game_servers.contains_key(&id) {
                tracing::error!(
                    "Game server {} ({}) has been defined more than once!",
                    id,
                    server.name
                );
            } else {
                self.game_servers.insert(id, server);
                self.queues_guard()
                    .insert(id, WorldQueue::new(gs.max_concurrent, gs.queue_limit));
            }
            if gs.max_concurrent > 0 {
                tracing::info!(
                    "Game Server {}: login queue at {} concurrent slots, queue limit {}",
                    id,
                    gs.max_concurrent,
                    if gs.queue_limit == 0 {
                        u16::MAX
                    } else {
                        gs.queue_limit
                    }
                );
            }
            let extra = if host != gs.host {
                format!("from {}", gs.host)
            } else if config.skip_host_resolve {
                " (unresolved)".to_string()
            } else {
                String::new()
            };
            tracing::info!(
                "Game Server {}: {} -> {}:{} {}",
                id,
                gs.name,
                host,
                gs.port,
                extra
            );
        }
        if self.game_servers.is_empty() {
            tracing::error!("No game servers have been defined!");
            return;
        }
        tracing::info!("Loaded {} game server(s)", self.game_servers.len());
    }

    /// Registers a game server connection (`GLRegisterGameServerPacket`).
    pub fn add(
        &self,
        gs_id: GameServerId,
        mirrors: Vec<GameServerId>,
        connection: &Arc<InternalConnection>,
    ) {
        {
            let Some(mut gs) = self.game_servers.get_mut(&gs_id) else {
                tracing::error!(
                    "GameServer connection from {} is requesting an invalid WorldId {}",
                    connection.ip,
                    gs_id
                );
                send_with_delay(
                    connection.clone(),
                    Duration::from_secs(5),
                    LgRegisterGameServer {
                        result: GsRegisterResult::Error,
                        pre_select_character_period: self.pre_select_character_period(),
                    },
                );
                return;
            };
            gs.connection = Some(connection.clone());
            self.legacy_cookie_warned.lock().unwrap().remove(&gs_id);
            gs.mirrors.extend(mirrors.iter().copied());
            connection.set_game_server(gs_id);
            gs.send_packet(&LgRegisterGameServer {
                result: GsRegisterResult::Success,
                pre_select_character_period: self.pre_select_character_period(),
            });
        }

        for mirror_id in mirrors {
            // C#: `_gameServers[mirrorId]` (KeyNotFoundException) and `_mirrorsId.Add` (duplicate
            // key ArgumentException) abort the remaining registration; mirrored here.
            match self.game_servers.get_mut(&mirror_id) {
                Some(mut mirror) => mirror.connection = Some(connection.clone()),
                None => {
                    tracing::error!(
                        "Error on packet dispatch: mirror game server {} is not configured",
                        mirror_id
                    );
                    return;
                }
            }
            let mut map = self.mirrors_id.lock().unwrap();
            if map.contains_key(&mirror_id) {
                tracing::error!(
                    "Error on packet dispatch: mirror {} is already registered",
                    mirror_id
                );
                return;
            }
            map.insert(mirror_id, gs_id);
        }

        if let Some(gs) = self.game_servers.get(&gs_id) {
            tracing::info!(
                "Registered GameServer {} ({}) from {}",
                gs.id,
                gs.name,
                connection.ip
            );
        }
    }

    /// Whether this connection currently owns the world (including registered mirrors).
    pub fn owns_connection(
        &self,
        gs_id: GameServerId,
        connection: &Arc<InternalConnection>,
    ) -> bool {
        self.game_servers.get(&gs_id).is_some_and(|gs| {
            gs.connection
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, connection))
        })
    }

    /// Unregisters a game server only if the closing connection still owns it.
    pub fn remove(&self, gs_id: GameServerId, connection: &Arc<InternalConnection>) {
        let mirrors = {
            let Some(mut gs) = self.game_servers.get_mut(&gs_id) else {
                return;
            };
            if !gs
                .connection
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, connection))
            {
                return;
            }
            gs.connection = None;
            std::mem::take(&mut gs.mirrors)
        };
        self.legacy_cookie_warned.lock().unwrap().remove(&gs_id);
        let mut cleared_ids = vec![gs_id];
        for mirror_id in mirrors {
            if let Some(mut server) = self.game_servers.get_mut(&mirror_id) {
                if !server
                    .connection
                    .as_ref()
                    .is_some_and(|owner| Arc::ptr_eq(owner, connection))
                {
                    continue;
                }
                server.connection = None;
            }
            cleared_ids.push(mirror_id);
            self.mirrors_id.lock().unwrap().remove(&mirror_id);
        }
        // The waiting clients cannot be admitted by a world that is gone. Keep the queue object
        // (capacity and limit) so a restarted/reconnected game server does not silently disable
        // the queue until the login server itself restarts.
        let mut waiting = Vec::new();
        {
            let mut queues = self.queues_guard();
            for id in cleared_ids {
                if let Some(queue) = queues.get_mut(&id) {
                    waiting.extend(queue.clear_for_unregister());
                }
            }
        }
        for connection_id in waiting {
            if let Some(conn) = self.connections.get(connection_id) {
                conn.world_unavailable();
            }
        }
    }

    fn snapshot(&self) -> Vec<GameServer> {
        let mut list: Vec<GameServer> = self
            .game_servers
            .iter()
            .map(|e| e.value().clone())
            .collect();
        // ConcurrentDictionary enumeration order is unspecified in C#; use id order.
        list.sort_by_key(|g| g.id);
        list
    }

    /// Collects the world list for a client, first asking every active game server for the
    /// account's characters (unless already known) and waiting for the answers (or timeouts).
    pub async fn get_world_list(
        &self,
        connection: &LoginConnection,
    ) -> anyhow::Result<WorldListResult> {
        let servers = self.snapshot();
        if servers.iter().any(GameServer::active) {
            let (request_ids, wait) = self
                .requests
                .create(servers.len(), WORLD_LIST_REQUEST_TIMEOUT)?;
            let mut character_requests = Vec::new();
            for (gs, &request_id) in servers.iter().zip(&request_ids) {
                let Some(game_connection) = &gs.connection else {
                    self.requests.release_id(request_id);
                    continue;
                };
                if connection.has_characters(gs.id) {
                    self.requests.release_id(request_id);
                    continue;
                }
                let request = Arc::new(CharacterRequest {
                    connection_id: connection.id(),
                    gs_id: gs.id,
                    game_connection: game_connection.clone(),
                });
                self.character_requests.insert(request_id, request.clone());
                character_requests.push((request_id, request));
                gs.send_packet(&LgRequestInfo {
                    connection_id: connection.id(),
                    request_id,
                    account_id: connection.account_id(),
                });
            }
            wait.await;
            for (request_id, request) in character_requests {
                // A completed id can already belong to another world-list request before
                // this waiter resumes. Never remove the new request's authorization.
                self.character_requests
                    .remove_if(&request_id, |_, current| Arc::ptr_eq(current, &request));
            }
        }
        // The C# list holds the live GameServer objects, so state is read after the wait.
        let game_servers = servers
            .iter()
            .filter_map(|s| self.get_game_server(s.id))
            .collect();
        Ok(WorldListResult {
            game_servers,
            characters: connection.get_characters(),
        })
    }

    /// Applies characters and completes only the request sent to this internal connection.
    pub fn complete_character_request(
        &self,
        connection: &Arc<InternalConnection>,
        connection_id: ConnectionId,
        request_id: u32,
        characters: Vec<LoginCharacterInfo>,
    ) -> anyhow::Result<()> {
        let request = self
            .character_requests
            .get(&request_id)
            .context("No pending character request for this response")?;
        anyhow::ensure!(
            request.connection_id == connection_id
                && Arc::ptr_eq(&request.game_connection, connection)
                && self.owns_connection(request.gs_id, connection),
            "Character response does not match the pending client and game server"
        );
        if !characters.is_empty() {
            let client = self
                .connections
                .get(connection_id)
                .context("Character response client is no longer connected")?;
            client.add_characters(request.gs_id, characters)?;
        }
        drop(request);
        self.character_requests.remove(&request_id);
        self.requests.release_id(request_id);
        Ok(())
    }

    pub fn set_load(&self, gs_id: GameServerId, load: GsLoad) -> anyhow::Result<()> {
        let mut gs = self.game_servers.get_mut(&gs_id).ok_or_else(|| {
            anyhow::anyhow!("The given key '{gs_id}' was not present in the dictionary.")
        })?;
        gs.load = load;
        Ok(())
    }

    // ---- world login queue ----------------------------------------------------------------

    /// The queue that owns `gs_id`: a mirror shares its parent's queue, because the game link
    /// carries no world id and the reports/answers carry the parent id only.
    fn queue_key(&self, gs_id: GameServerId) -> GameServerId {
        self.try_get_parent_id(gs_id).unwrap_or(gs_id)
    }

    /// Decides whether a `CAEnterWorld` may go to the game server now, must wait in the queue, or
    /// must be refused because the queue is full.
    pub fn try_enter_world(&self, gs_id: GameServerId, connection_id: ConnectionId) -> Admission {
        match self.queues_guard().get_mut(&self.queue_key(gs_id)) {
            Some(queue) => queue.try_enter(gs_id, connection_id),
            // A world without a configured queue entry (should not happen for a loaded,
            // active server) behaves exactly as before the queue existed.
            None => Admission::Direct { reservation: None },
        }
    }

    /// Records the authoritative occupancy reported by the game. `sequence` is the game's
    /// monotonic report order; `0` means the game server predates the extension.
    pub fn note_online(&self, gs_id: GameServerId, online: u32, sequence: u32) {
        if let Some(queue) = self.queues_guard().get_mut(&self.queue_key(gs_id)) {
            queue.set_online(online, sequence);
        }
    }

    /// Handles a `GL_PLAYER_ENTER` result: the game's occupancy now includes (or refused) the
    /// pending cookie for `connection_id`, so its reservation is no longer needed. The link
    /// answers requests in order, so the oldest outstanding reservation belongs to this response
    /// (connection ids are reused).
    pub fn note_enter_world_result(
        &self,
        gs_id: GameServerId,
        connection_id: ConnectionId,
        online: u32,
        sequence: u32,
    ) {
        if let Some(queue) = self.queues_guard().get_mut(&self.queue_key(gs_id)) {
            queue.set_online(online, sequence);
            queue.release_oldest_reservation(connection_id);
        }
    }

    /// Releases a reservation whose game response never arrived (timeout). Only the matching
    /// generation is dropped: the connection id may have been reused for a newer admission.
    pub fn release_reservation(
        &self,
        gs_id: GameServerId,
        connection_id: ConnectionId,
        generation: u64,
    ) {
        if let Some(queue) = self.queues_guard().get_mut(&self.queue_key(gs_id)) {
            queue.release_reservation(connection_id, generation);
        }
    }

    /// Removes a client that cancelled its queued enter-world request. The reservation of the
    /// admission being cancelled (the newest, if the request was already popped) is released too.
    pub fn cancel_queued(&self, connection_id: ConnectionId) {
        let affected = {
            let mut queues = self.queues_guard();
            let mut affected = Vec::new();
            for (gs_id, queue) in queues.iter_mut() {
                if queue.remove_queued(connection_id)
                    | queue.release_newest_reservation(connection_id)
                {
                    affected.push(*gs_id);
                }
            }
            affected
        };
        for gs_id in affected {
            self.notify_queue(gs_id);
        }
    }

    /// Removes a disconnected client from every queue. Reservations of admitted clients are left
    /// alone: the client may have received its world cookie already and be connecting to the
    /// game; the reservation is released by the game's response or its timeout.
    pub fn connection_closed(&self, connection_id: ConnectionId) {
        let affected = {
            let mut queues = self.queues_guard();
            let mut affected = Vec::new();
            for (gs_id, queue) in queues.iter_mut() {
                if queue.remove_queued(connection_id) {
                    affected.push(*gs_id);
                }
            }
            affected
        };
        for gs_id in affected {
            self.notify_queue(gs_id);
        }
    }

    /// Sends every waiting client its current position.
    pub fn notify_queue(&self, gs_id: GameServerId) {
        let notices = match self.queues_guard().get(&self.queue_key(gs_id)) {
            Some(queue) => queue.notices(),
            None => return,
        };
        for notice in notices {
            let Some(conn) = self.connections.get(notice.connection_id) else {
                continue;
            };
            let _ = conn.send_packet(&AcWorldQueue {
                world_id: notice.world_id.0,
                is_premium: false,
                my_turn: notice.my_turn,
                normal_length: notice.normal_length,
                premium_length: 0,
            });
        }
    }

    /// Warns once for a queue-enabled world whose game server does not send the occupancy
    /// extension (an old binary): its reports read as 0, so the queue gate never engages.
    pub fn warn_missing_occupancy(&self, gs_id: GameServerId) {
        let missing = match self.queues_guard().get_mut(&self.queue_key(gs_id)) {
            Some(queue) => queue.needs_missing_occupancy_warning(),
            None => false,
        };
        if missing {
            tracing::warn!(
                "Game Server {} has a login queue configured but reports no slot occupancy (old game server?): the queue will not engage",
                gs_id
            );
        }
    }

    /// Admits queued clients while the world has free slots. Called after every occupancy report,
    /// every enter-world result and every reservation release.
    pub async fn drain_queues(&self, server: &Arc<LoginServer>) {
        let worlds: Vec<GameServerId> = self.queues_guard().keys().copied().collect();
        for gs_id in worlds {
            let mut moved = false;
            loop {
                let next = match self.queues_guard().get_mut(&gs_id) {
                    Some(queue) => queue.pop_for_admission(),
                    None => None,
                };
                let Some((connection_id, world_id, generation, normal_length)) = next else {
                    break;
                };
                moved = true;
                let admitted = match self.connections.get(connection_id) {
                    Some(conn) => {
                        // The final position 0 tells the client the wait is over; the world
                        // cookie follows from the normal enter-world flow.
                        let _ = conn.send_packet(&AcWorldQueue {
                            world_id: world_id.0,
                            is_premium: false,
                            my_turn: 0,
                            normal_length,
                            premium_length: 0,
                        });
                        conn.admit_from_queue(server, world_id, generation).await
                    }
                    None => false,
                };
                if !admitted {
                    self.release_reservation(world_id, connection_id, generation);
                }
            }
            // Only a pop moves the remaining positions; a plain occupancy change does not.
            if moved {
                self.notify_queue(gs_id);
            }
        }
    }

    /// A game server by id (a snapshot).
    pub fn get_game_server(&self, gs_id: GameServerId) -> Option<GameServer> {
        self.game_servers.get(&gs_id).map(|g| g.clone())
    }

    /// Sends an enter world request to the game server (fire-and-forget); the response is
    /// routed via [`Self::route_enter_world_response`].
    pub fn request_enter_world(
        &self,
        account_id: AccountId,
        connection_id: ConnectionId,
        gs_id: GameServerId,
    ) {
        let Some(gs) = self.get_game_server(gs_id) else {
            tracing::warn!("RequestEnterWorld: game server {} not found", gs_id);
            return;
        };
        if !gs.active() {
            tracing::warn!("RequestEnterWorld: game server {} not active", gs_id);
            return;
        }
        gs.send_packet(&LgPlayerEnter {
            account_id,
            connection_id,
        });
    }

    /// Warns once per game server registration that a successful `GLPlayerEnterPacket` carried
    /// no world cookie (an old game server): the cookie falls back to the connection id, which
    /// anyone can guess. Login and game servers must be upgraded together.
    pub fn warn_legacy_cookie(&self, gs_id: GameServerId) {
        if self.legacy_cookie_warned.lock().unwrap().insert(gs_id) {
            tracing::warn!(
                "Game Server {} sent no world cookie (old game server?): falling back to the guessable connection id; upgrade the login and game servers together",
                gs_id
            );
        }
    }

    /// Routes a `GLPlayerEnterPacket` result to the client connection's session. `cookie` is the
    /// game server's world cookie (0 = none sent: the connection id is used).
    pub fn route_enter_world_response(
        &self,
        connection_id: ConnectionId,
        gs_id: GameServerId,
        result: u8,
        cookie: u32,
    ) {
        let Some(connection) = self.connections.get(connection_id) else {
            tracing::warn!(
                "RouteEnterWorldResponse: connection {} not found",
                connection_id
            );
            return;
        };
        connection.complete_enter_world_request(self, gs_id, result, cookie);
    }
}

pub(crate) fn send_with_delay<P: OutgoingPacket + Send + 'static>(
    connection: Arc<InternalConnection>,
    delay: Duration,
    packet: P,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        connection.send_packet(&packet);
    });
}

#[cfg(test)]
mod tests;
