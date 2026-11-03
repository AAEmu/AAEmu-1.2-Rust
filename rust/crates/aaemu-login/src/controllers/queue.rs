//! World login queue state (`ACWorldQueuePacket`, AC 0x009).
//!
//! The queue itself is pure state: [`GameController`](super::game::GameController) owns one
//! [`WorldQueue`] per configured game server and performs the I/O (the final `ACWorldQueuePacket`,
//! the normal enter-world request) outside the lock. Keeping the state here makes the FIFO,
//! capacity, reservation and queue-length rules testable without sockets.
//!
//! Capacity is the game server's slot occupancy: accounts in the lobby/world plus pending world
//! cookies, as reported by `GL_GAME_SERVER_LOAD` and `GL_PLAYER_ENTER`. A `CAEnterWorld` that
//! arrives while the world is full joins the queue instead of being refused. The login grants a
//! *reservation* for every request it sends to the game; the game's next occupancy report includes
//! the pending cookie, so the reservation is removed when the matching `GL_PLAYER_ENTER` arrives.
//! Until then the reservation is counted on top of the last reported occupancy. A report that was
//! generated before the game processed the request therefore double-counts it (conservative),
//! never the other way round.

use std::collections::VecDeque;

use crate::models::ConnectionId;

/// The result of asking a world whether a `CAEnterWorld` may proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Proceed with the normal enter-world flow. `reserved` is true when the world has a queue
    /// configured and this request took one of its slots (the caller must release it on failure
    /// or on the matching game response).
    Direct { reserved: bool },
    /// The world is at capacity; the client is in the queue.
    Queued,
    /// The world is at capacity and its queue is full; the client must be refused.
    Full,
}

/// One queued client's position as sent in `ACWorldQueuePacket`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueNotice {
    pub connection_id: ConnectionId,
    /// 1-based position in the queue; 0 means "the wait is over" and is never queued here.
    pub my_turn: u16,
    /// Total normal-lane length including this client.
    pub normal_length: u16,
}

/// Per-world FIFO. All methods run under the owning controller's lock; they never touch I/O.
#[derive(Debug, Default)]
pub struct WorldQueue {
    /// `GameServers[].MaxConcurrent`; 0 disables the queue (today's behaviour).
    capacity: u32,
    /// `GameServers[].QueueLimit`; 0 means the packet limit (`u16::MAX`).
    limit: u16,
    /// Last occupancy reported by the game (accounts + pending cookies).
    online: u32,
    /// FIFO of clients waiting for a slot.
    queue: VecDeque<ConnectionId>,
    /// Requests already sent to the game whose pending cookie the next occupancy report will
    /// include. Counted on top of `online` until the matching `GL_PLAYER_ENTER` arrives.
    reservations: Vec<ConnectionId>,
}

impl WorldQueue {
    pub fn new(capacity: u32, limit: u16) -> Self {
        Self {
            capacity,
            limit,
            online: 0,
            queue: VecDeque::new(),
            reservations: Vec::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.capacity > 0
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    pub fn online(&self) -> u32 {
        self.online
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    fn occupied(&self) -> u32 {
        self.online
            .saturating_add(self.reservations.len().try_into().unwrap_or(u32::MAX))
    }

    fn limit(&self) -> usize {
        if self.limit == 0 {
            usize::from(u16::MAX)
        } else {
            usize::from(self.limit)
        }
    }

    fn position_of(&self, connection_id: ConnectionId) -> Option<u16> {
        self.queue
            .iter()
            .position(|c| *c == connection_id)
            .map(|i| i as u16 + 1)
    }

    pub fn is_queued(&self, connection_id: ConnectionId) -> bool {
        self.position_of(connection_id).is_some()
    }

    pub fn is_reserved(&self, connection_id: ConnectionId) -> bool {
        self.reservations.contains(&connection_id)
    }

    /// Decides what to do with a new `CAEnterWorld`.
    pub fn try_enter(&mut self, connection_id: ConnectionId) -> Admission {
        if !self.is_enabled() {
            return Admission::Direct { reserved: false };
        }
        // FIFO: while anyone is queued, a new request goes behind them even if a slot has just
        // become free (the drain will admit the head first).
        if !self.queue.is_empty() {
            return self.enqueue(connection_id);
        }
        if self.occupied() < self.capacity {
            self.reservations.push(connection_id);
            Admission::Direct { reserved: true }
        } else {
            self.enqueue(connection_id)
        }
    }

    fn enqueue(&mut self, connection_id: ConnectionId) -> Admission {
        // The session state machine prevents duplicates; keep the queue invariant anyway.
        if self.is_queued(connection_id) {
            return Admission::Queued;
        }
        if self.queue.len() >= self.limit() || self.queue.len() >= usize::from(u16::MAX) {
            return Admission::Full;
        }
        self.queue.push_back(connection_id);
        Admission::Queued
    }

    /// Takes the head of the queue if a slot is free and reserves it for the normal enter-world
    /// flow. Returns the connection and the queue length after the pop (for `ACWorldQueue`).
    pub fn pop_for_admission(&mut self) -> Option<(ConnectionId, u16)> {
        if !self.is_enabled() || self.occupied() >= self.capacity {
            return None;
        }
        let connection_id = self.queue.pop_front()?;
        self.reservations.push(connection_id);
        Some((connection_id, self.normal_length()))
    }

    /// Sets the authoritative occupancy reported by the game.
    pub fn set_online(&mut self, online: u32) {
        self.online = online;
    }

    /// Drops an in-flight reservation (game response received, request failed, timed out or the
    /// client cancelled). `true` if it existed.
    pub fn release_reservation(&mut self, connection_id: ConnectionId) -> bool {
        if let Some(i) = self.reservations.iter().position(|c| *c == connection_id) {
            self.reservations.remove(i);
            true
        } else {
            false
        }
    }

    /// Removes a waiting client (cancel or disconnect). `true` if it was queued.
    pub fn remove_queued(&mut self, connection_id: ConnectionId) -> bool {
        if let Some(i) = self.queue.iter().position(|c| *c == connection_id) {
            self.queue.remove(i);
            true
        } else {
            false
        }
    }

    /// Current 1-based position and total normal-lane length.
    pub fn position(&self, connection_id: ConnectionId) -> Option<QueueNotice> {
        let my_turn = self.position_of(connection_id)?;
        Some(QueueNotice {
            connection_id,
            my_turn,
            normal_length: self.normal_length(),
        })
    }

    pub fn normal_length(&self) -> u16 {
        self.queue.len().try_into().unwrap_or(u16::MAX)
    }

    /// Every waiting client's position, in queue order (for position updates).
    pub fn notices(&self) -> Vec<QueueNotice> {
        let normal_length = self.normal_length();
        self.queue
            .iter()
            .enumerate()
            .map(|(i, connection_id)| QueueNotice {
                connection_id: *connection_id,
                my_turn: i as u16 + 1,
                normal_length,
            })
            .collect()
    }

    /// Empties the queue and returns every connection it held (world removed).
    pub fn drain_entries(&mut self) -> Vec<ConnectionId> {
        let mut entries: Vec<ConnectionId> = self.queue.drain(..).collect();
        entries.append(&mut self.reservations);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ConnectionId = ConnectionId(1);
    const B: ConnectionId = ConnectionId(2);
    const C: ConnectionId = ConnectionId(3);

    #[test]
    fn default_capacity_never_queues() {
        let mut q = WorldQueue::new(0, 100);
        q.set_online(1_000);
        assert_eq!(q.try_enter(A), Admission::Direct { reserved: false });
        assert_eq!(q.queue_len(), 0);
        assert!(!q.is_reserved(A));
    }

    #[test]
    fn full_world_queues_in_fifo_order() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(1);
        assert_eq!(q.try_enter(A), Admission::Queued);
        assert_eq!(q.try_enter(B), Admission::Queued);
        assert_eq!(
            q.position(A),
            Some(QueueNotice {
                connection_id: A,
                my_turn: 1,
                normal_length: 2
            })
        );
        assert_eq!(q.position(B).map(|n| n.my_turn), Some(2));
    }

    #[test]
    fn head_proceeds_when_a_slot_frees_and_positions_move() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(1);
        q.try_enter(A);
        q.try_enter(B);
        q.try_enter(C);

        assert_eq!(q.pop_for_admission(), None);
        q.set_online(0);
        assert_eq!(q.pop_for_admission(), Some((A, 2)));
        assert!(q.is_reserved(A));
        // A is no longer in the queue; B and C move up.
        assert_eq!(q.notices()[0].connection_id, B);
        assert_eq!(q.notices()[0].my_turn, 1);
        assert_eq!(q.notices()[1].my_turn, 2);
        // The reservation fills the slot until the game acknowledges it.
        assert_eq!(q.pop_for_admission(), None);
        q.release_reservation(A);
        assert_eq!(q.pop_for_admission(), Some((B, 1)));
    }

    #[test]
    fn a_queued_client_can_cancel_or_disconnect() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(1);
        q.try_enter(A);
        q.try_enter(B);
        q.try_enter(C);

        assert!(q.remove_queued(B));
        assert_eq!(q.notices()[1].connection_id, C);
        assert_eq!(q.notices()[1].my_turn, 2);
        assert!(!q.remove_queued(B));
        assert!(!q.is_queued(B));
    }

    #[test]
    fn a_full_queue_refuses_new_arrivals() {
        let mut q = WorldQueue::new(1, 2);
        q.set_online(1);
        assert_eq!(q.try_enter(A), Admission::Queued);
        assert_eq!(q.try_enter(B), Admission::Queued);
        assert_eq!(q.try_enter(C), Admission::Full);
        assert_eq!(q.queue_len(), 2);
    }

    #[test]
    fn a_direct_admission_reserves_the_slot_immediately() {
        let mut q = WorldQueue::new(1, 10);
        assert_eq!(q.try_enter(A), Admission::Direct { reserved: true });
        // The slot is taken locally until the game's report includes the pending cookie.
        assert_eq!(q.try_enter(B), Admission::Queued);
        assert_eq!(q.pop_for_admission(), None);
        // A disappeared before it connected: the game reports 0 and the reservation is released.
        q.set_online(0);
        q.release_reservation(A);
        assert_eq!(q.pop_for_admission(), Some((B, 0)));
    }
}
