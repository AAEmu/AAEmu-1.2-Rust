//! Port of `Core/Managers/World/StreamManager` (+ the stream-server state that hangs off it).
//!
//! * Ticket map: `AddToken` (from `EnterWorldManager.Login`), `RemoveToken` (game disconnect) and
//!   `Login` (stream `CTJoinPacket`). Deviation: the join token is a random per-connection ticket
//!   (`X2EnterWorldResponsePacket.token`), not the sequential game connection id, so a stream
//!   peer cannot guess or break another player's session.
//! * `RequestCell` / `ContinueCell` (doodad streaming) live in [`crate::stream::cells`], because
//!   they need the per-connection request table.
//! * The stream connection table, the [`UccManager`] and the [`StreamWorld`] hook are owned here
//!   so `GameServer` (which only constructs `StreamManager::default()`) carries all stream state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::stream::connection::StreamConnectionTable;
use crate::stream::ucc::UccManager;
use crate::stream::world::{NullStreamWorld, StreamWorld};

/// Result of [`StreamManager::login`] (`TCJoinResponsePacket` value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinResult {
    /// Response `0`; `game_connection_id` is the game connection the ticket belongs to.
    Accepted { game_connection_id: u32 },
    /// Response `1`.
    Rejected,
}

impl JoinResult {
    pub fn response(self) -> u8 {
        match self {
            JoinResult::Accepted { .. } => 0,
            JoinResult::Rejected => 1,
        }
    }
}

/// What a stream join ticket stands for.
#[derive(Debug, Clone, Copy)]
struct Ticket {
    account_id: u32,
    connection_id: u32,
}

/// The live join tickets: ticket -> owner, and game connection id -> ticket.
#[derive(Debug, Default)]
struct Tickets {
    by_ticket: HashMap<u32, Ticket>,
    by_connection: HashMap<u32, u32>,
}

pub struct StreamManager {
    /// Join tickets handed out in `X2EnterWorldResponsePacket`.
    tickets: Mutex<Tickets>,
    /// `StreamConnectionTable`.
    pub connections: StreamConnectionTable,
    /// `UccManager`.
    pub ucc: UccManager,
    /// World / character / item lookups the stream packets need (see [`StreamWorld`]).
    world: RwLock<Arc<dyn StreamWorld>>,
}

impl Default for StreamManager {
    fn default() -> Self {
        Self {
            tickets: Mutex::new(Tickets::default()),
            connections: StreamConnectionTable::default(),
            ucc: UccManager::default(),
            world: RwLock::new(Arc::new(NullStreamWorld)),
        }
    }
}

impl std::fmt::Debug for StreamManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamManager")
            .field("tickets", &self.tickets().by_ticket.len())
            .field("connections", &self.connections.len())
            .finish_non_exhaustive()
    }
}

impl StreamManager {
    fn tickets(&self) -> std::sync::MutexGuard<'_, Tickets> {
        self.tickets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `AddToken(accountId, connectionId)`: issues the join ticket of a game connection, a random
    /// nonzero u32 that no live ticket uses (same source as `GameServer::new_reconnect_token`).
    /// C# uses `Dictionary.Add`, which throws when the same connection enters twice; `None` is
    /// returned in that case (nothing changes) so the caller can fail the packet like the C#
    /// exception does.
    pub fn add_token(&self, account_id: u32, connection_id: u32) -> Option<u32> {
        let mut tickets = self.tickets();
        if tickets.by_connection.contains_key(&connection_id) {
            return None;
        }
        let ticket = loop {
            let ticket = rand::random::<u32>();
            if ticket != 0 && !tickets.by_ticket.contains_key(&ticket) {
                break ticket;
            }
        };
        Self::insert(&mut tickets, ticket, account_id, connection_id);
        Some(ticket)
    }

    /// [`Self::add_token`] with a chosen ticket (tests).
    #[cfg(test)]
    pub fn add_token_with_ticket(&self, account_id: u32, connection_id: u32, ticket: u32) -> bool {
        let mut tickets = self.tickets();
        if tickets.by_connection.contains_key(&connection_id)
            || tickets.by_ticket.contains_key(&ticket)
        {
            return false;
        }
        Self::insert(&mut tickets, ticket, account_id, connection_id);
        true
    }

    fn insert(tickets: &mut Tickets, ticket: u32, account_id: u32, connection_id: u32) {
        tickets.by_ticket.insert(
            ticket,
            Ticket {
                account_id,
                connection_id,
            },
        );
        tickets.by_connection.insert(connection_id, ticket);
    }

    /// Drops the ticket of a game connection (its disconnect).
    pub fn remove_token(&self, connection_id: u32) {
        let mut tickets = self.tickets();
        if let Some(ticket) = tickets.by_connection.remove(&connection_id) {
            tickets.by_ticket.remove(&ticket);
        }
    }

    /// The account a ticket was issued for.
    pub fn account_for(&self, ticket: u32) -> Option<u32> {
        self.tickets().by_ticket.get(&ticket).map(|t| t.account_id)
    }

    /// The ticket issued to a game connection.
    pub fn ticket_for(&self, connection_id: u32) -> Option<u32> {
        self.tickets().by_connection.get(&connection_id).copied()
    }

    /// `StreamManager.Login(connection, accountId, token)` minus the packet send.
    ///
    /// * known ticket, matching account -> accepted (the ticket is kept, so it can be reused);
    /// * known ticket, other account -> rejected. Deviation: the C# removed the token then; with
    ///   it any stream peer could break another player's session;
    /// * unknown ticket -> rejected.
    pub fn login(&self, account_id: u32, ticket: u32) -> JoinResult {
        match self.tickets().by_ticket.get(&ticket) {
            Some(t) if t.account_id == account_id => JoinResult::Accepted {
                game_connection_id: t.connection_id,
            },
            _ => JoinResult::Rejected,
        }
    }

    /// Current world hook.
    pub fn world(&self) -> Arc<dyn StreamWorld> {
        self.world.read().expect("stream world lock").clone()
    }

    /// Replaces the world hook (to be called once the world / character / item managers exist).
    pub fn set_world(&self, world: Arc<dyn StreamWorld>) {
        *self.world.write().expect("stream world lock") = world;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_flow() {
        let m = StreamManager::default();
        let ticket = m.add_token(1000, 7).expect("first ticket");
        assert_ne!(ticket, 0);
        // A connection gets one ticket.
        assert_eq!(m.add_token(1001, 7), None);
        assert_eq!(m.ticket_for(7), Some(ticket));
        assert_eq!(m.account_for(ticket), Some(1000));
        assert_eq!(
            m.login(1000, ticket),
            JoinResult::Accepted {
                game_connection_id: 7
            }
        );
        // The ticket survives a successful join.
        assert_eq!(m.login(1000, ticket).response(), 0);
        // The game connection id is not a ticket.
        assert_eq!(m.login(1000, 7), JoinResult::Rejected);
        // Wrong account: rejected, and the owner's ticket stays usable.
        assert_eq!(m.login(999, ticket), JoinResult::Rejected);
        assert_eq!(m.account_for(ticket), Some(1000));
        assert_eq!(m.login(1000, ticket).response(), 0);
        // Removed with the game connection.
        m.remove_token(7);
        assert_eq!(m.login(1000, ticket), JoinResult::Rejected);
        assert_eq!(m.ticket_for(7), None);
        let other = m.add_token(5, 9).unwrap();
        assert_ne!(other, ticket);
        m.remove_token(9);
        assert_eq!(m.login(5, other).response(), 1);
    }
}
