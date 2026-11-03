//! World login queue state (`ACWorldQueuePacket`, AC 0x009).
//!
//! The queue itself is pure state: [`GameController`](super::game::GameController) owns one
//! [`WorldQueue`] per configured world and performs the I/O (the final `ACWorldQueuePacket`, the
//! normal enter-world request) outside the lock. Keeping the state here makes the FIFO, capacity,
//! reservation and queue-length rules testable without sockets.
//!
//! Capacity is the game server's slot occupancy: accounts in the lobby/world plus world cookies
//! (including one whose account registration is in flight), as reported by
//! `GL_GAME_SERVER_LOAD` and `GL_PLAYER_ENTER`. A `CAEnterWorld` that arrives while the world is
//! full joins the queue instead of being refused. The login grants a *reservation* for every
//! request it sends to the game and removes it when the matching `GL_PLAYER_ENTER` arrives; that
//! answer's occupancy snapshot already includes the cookie. A report generated before the game
//! processed the request omits the cookie, so until the answer the reservation is counted on top
//! of the last reported occupancy: that can only double-count (conservative), never under-count.
//! Each report carries a sequence so an older snapshot cannot overwrite a newer one.
//!
//! Mirrors share their parent world's queue (`GameController` resolves `try_get_parent_id`): the
//! game link has no world id, so the parent's report and response are the only occupancy the login
//! sees. Each entry keeps the world id the client asked for, which is the id sent back in
//! `ACWorldQueuePacket`.
//!
//! Every reservation carries a monotonically increasing generation. Connection ids are reused and
//! a client may cancel and enter again, so a late timeout (or any other release path that knows
//! the generation) must only drop the reservation it was created for; the game's answer has no
//! generation, so it releases the oldest outstanding reservation for that connection (the link
//! answers requests in order).

use std::collections::VecDeque;

use crate::models::{ConnectionId, GameServerId};

/// The result of asking a world whether a `CAEnterWorld` may proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Proceed with the normal enter-world flow. `reservation` is the queue generation when the
    /// world has a queue configured and this request took one of its slots (the caller must
    /// release it on failure, on the matching game response, or when its timeout fires).
    Direct { reservation: Option<u64> },
    /// The world is at capacity; the client is in the queue.
    Queued,
    /// The world is at capacity and its queue is full; the client must be refused.
    Full,
}

/// One queued client's position as sent in `ACWorldQueuePacket`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueNotice {
    pub connection_id: ConnectionId,
    /// The world id the client selected (a mirror keeps its own id).
    pub world_id: GameServerId,
    /// 1-based position in the queue; 0 means "the wait is over" and is never queued here.
    pub my_turn: u16,
    /// Total normal-lane length including this client.
    pub normal_length: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueuedEntry {
    connection_id: ConnectionId,
    world_id: GameServerId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reservation {
    connection_id: ConnectionId,
    world_id: GameServerId,
    generation: u64,
}

/// Per-world FIFO. All methods run under the owning controller's lock; they never touch I/O.
#[derive(Debug)]
pub struct WorldQueue {
    /// `GameServers[].MaxConcurrent`; 0 disables the queue (today's behaviour).
    capacity: u32,
    /// `GameServers[].QueueLimit`; 0 means the packet limit (`u16::MAX`).
    limit: u16,
    /// Last occupancy reported by the game (accounts + pending cookies).
    online: u32,
    /// Sequence of the last applied occupancy report (0 = unsequenced old game server).
    last_sequence: u32,
    /// FIFO of clients waiting for a slot.
    queue: VecDeque<QueuedEntry>,
    /// Requests already sent to the game whose pending cookie the next occupancy report will
    /// include. Counted on top of `online` until the matching `GL_PLAYER_ENTER` arrives.
    reservations: Vec<Reservation>,
    next_generation: u64,
    warned_missing_occupancy: bool,
}

impl Default for WorldQueue {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl WorldQueue {
    pub fn new(capacity: u32, limit: u16) -> Self {
        Self {
            capacity,
            limit,
            online: 0,
            last_sequence: 0,
            queue: VecDeque::new(),
            reservations: Vec::new(),
            next_generation: 0,
            warned_missing_occupancy: false,
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

    fn next_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        self.next_generation
    }

    fn position_of(&self, connection_id: ConnectionId) -> Option<u16> {
        self.queue
            .iter()
            .position(|e| e.connection_id == connection_id)
            .map(|i| i as u16 + 1)
    }

    pub fn is_queued(&self, connection_id: ConnectionId) -> bool {
        self.position_of(connection_id).is_some()
    }

    pub fn is_reserved(&self, connection_id: ConnectionId) -> bool {
        self.reservations
            .iter()
            .any(|r| r.connection_id == connection_id)
    }

    /// Decides what to do with a new `CAEnterWorld` for `world_id` (the selected world, which may
    /// be a mirror; the queue itself belongs to the parent).
    pub fn try_enter(&mut self, world_id: GameServerId, connection_id: ConnectionId) -> Admission {
        if !self.is_enabled() {
            return Admission::Direct { reservation: None };
        }
        // FIFO: while anyone is queued, a new request goes behind them even if a slot has just
        // become free (the drain will admit the head first).
        if !self.queue.is_empty() {
            return self.enqueue(world_id, connection_id);
        }
        if self.occupied() < self.capacity {
            let generation = self.next_generation();
            self.reservations.push(Reservation {
                connection_id,
                world_id,
                generation,
            });
            Admission::Direct {
                reservation: Some(generation),
            }
        } else {
            self.enqueue(world_id, connection_id)
        }
    }

    fn enqueue(&mut self, world_id: GameServerId, connection_id: ConnectionId) -> Admission {
        // The session state machine prevents duplicates; keep the queue invariant anyway.
        if self.is_queued(connection_id) {
            return Admission::Queued;
        }
        if self.queue.len() >= self.limit() || self.queue.len() >= usize::from(u16::MAX) {
            return Admission::Full;
        }
        self.queue.push_back(QueuedEntry {
            connection_id,
            world_id,
        });
        Admission::Queued
    }

    /// Takes the head of the queue if a slot is free and reserves it for the normal enter-world
    /// flow. Returns the connection, the world the client selected, the reservation generation and
    /// the queue length after the pop (for `ACWorldQueue`).
    pub fn pop_for_admission(&mut self) -> Option<(ConnectionId, GameServerId, u64, u16)> {
        if !self.is_enabled() || self.occupied() >= self.capacity {
            return None;
        }
        let entry = self.queue.pop_front()?;
        let generation = self.next_generation();
        self.reservations.push(Reservation {
            connection_id: entry.connection_id,
            world_id: entry.world_id,
            generation,
        });
        Some((
            entry.connection_id,
            entry.world_id,
            generation,
            self.normal_length(),
        ))
    }

    /// Sets the authoritative occupancy. Reports carry a sequence so a snapshot taken before a
    /// concurrent change cannot overwrite a newer one; `0` means the game server predates the
    /// extension, in which case the report is always applied.
    pub fn set_online(&mut self, online: u32, sequence: u32) {
        if sequence != 0 {
            if sequence <= self.last_sequence {
                return;
            }
            self.last_sequence = sequence;
        }
        self.online = online;
    }

    /// Drops the exact reservation generation (timeouts, cancellations). `true` if it existed.
    pub fn release_reservation(&mut self, connection_id: ConnectionId, generation: u64) -> bool {
        if let Some(i) = self
            .reservations
            .iter()
            .position(|r| r.connection_id == connection_id && r.generation == generation)
        {
            self.reservations.remove(i);
            true
        } else {
            false
        }
    }

    /// The generations of the outstanding reservations for `connection_id`, oldest first.
    #[cfg(test)]
    pub fn reserved_generations(&self, connection_id: ConnectionId) -> Vec<u64> {
        self.reservations
            .iter()
            .filter(|r| r.connection_id == connection_id)
            .map(|r| r.generation)
            .collect()
    }

    /// Drops the oldest outstanding reservation for a connection: one game `GLPlayerEnter`
    /// answer per request, in link order. `true` if one existed.
    pub fn release_oldest_reservation(&mut self, connection_id: ConnectionId) -> bool {
        if let Some(i) = self
            .reservations
            .iter()
            .position(|r| r.connection_id == connection_id)
        {
            self.reservations.remove(i);
            true
        } else {
            false
        }
    }

    /// Drops the newest outstanding reservation for a connection (a cancel of the current
    /// admission while an older one is still waiting for its answer). `true` if one existed.
    pub fn release_newest_reservation(&mut self, connection_id: ConnectionId) -> bool {
        if let Some(i) = self
            .reservations
            .iter()
            .rposition(|r| r.connection_id == connection_id)
        {
            self.reservations.remove(i);
            true
        } else {
            false
        }
    }

    /// Removes a waiting client (cancel or disconnect). `true` if it was queued.
    pub fn remove_queued(&mut self, connection_id: ConnectionId) -> bool {
        if let Some(i) = self
            .queue
            .iter()
            .position(|e| e.connection_id == connection_id)
        {
            self.queue.remove(i);
            true
        } else {
            false
        }
    }

    /// Current 1-based position and total normal-lane length.
    pub fn position(&self, connection_id: ConnectionId) -> Option<QueueNotice> {
        let my_turn = self.position_of(connection_id)?;
        let entry = self
            .queue
            .iter()
            .find(|e| e.connection_id == connection_id)?;
        Some(QueueNotice {
            connection_id,
            world_id: entry.world_id,
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
            .map(|(i, entry)| QueueNotice {
                connection_id: entry.connection_id,
                world_id: entry.world_id,
                my_turn: i as u16 + 1,
                normal_length,
            })
            .collect()
    }

    /// Empties the queue and reservations and returns every connection it held (world removed).
    pub fn clear_for_unregister(&mut self) -> Vec<ConnectionId> {
        let mut entries: Vec<ConnectionId> =
            self.queue.drain(..).map(|e| e.connection_id).collect();
        entries.extend(self.reservations.drain(..).map(|r| r.connection_id));
        entries.sort_unstable_by_key(|c| c.0);
        entries.dedup();
        // Keep capacity/limit so re-registering the game server does not disable the queue; the
        // new process starts its occupancy and sequence from zero again.
        self.online = 0;
        self.last_sequence = 0;
        self.warned_missing_occupancy = false;
        entries
    }

    /// Whether an occupancy report without the Rust extension deserves one warning: only when a
    /// queue is configured, and only once until the world re-registers.
    pub fn needs_missing_occupancy_warning(&mut self) -> bool {
        if self.is_enabled() && !self.warned_missing_occupancy {
            self.warned_missing_occupancy = true;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ConnectionId = ConnectionId(1);
    const B: ConnectionId = ConnectionId(2);
    const C: ConnectionId = ConnectionId(3);
    const WORLD: GameServerId = GameServerId(1);
    const MIRROR: GameServerId = GameServerId(2);

    #[test]
    fn default_capacity_never_queues() {
        let mut q = WorldQueue::new(0, 100);
        q.set_online(1_000, 1);
        assert_eq!(
            q.try_enter(WORLD, A),
            Admission::Direct { reservation: None }
        );
        assert_eq!(q.queue_len(), 0);
        assert!(!q.is_reserved(A));
        assert!(!q.needs_missing_occupancy_warning());
    }

    #[test]
    fn full_world_queues_in_fifo_order() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(1, 1);
        assert_eq!(q.try_enter(WORLD, A), Admission::Queued);
        assert_eq!(q.try_enter(WORLD, B), Admission::Queued);
        assert_eq!(
            q.position(A),
            Some(QueueNotice {
                connection_id: A,
                world_id: WORLD,
                my_turn: 1,
                normal_length: 2
            })
        );
        assert_eq!(q.position(B).map(|n| n.my_turn), Some(2));
    }

    #[test]
    fn head_proceeds_when_a_slot_frees_and_positions_move() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(1, 1);
        q.try_enter(WORLD, A);
        q.try_enter(WORLD, B);
        q.try_enter(WORLD, C);

        assert_eq!(q.pop_for_admission(), None);
        q.set_online(0, 2);
        let (connection, world, generation, length) = q.pop_for_admission().unwrap();
        assert_eq!((connection, world, length), (A, WORLD, 2));
        assert!(q.is_reserved(A));
        // A is no longer in the queue; B and C move up.
        assert_eq!(q.notices()[0].connection_id, B);
        assert_eq!(q.notices()[0].my_turn, 1);
        assert_eq!(q.notices()[1].my_turn, 2);
        // The reservation fills the slot until the game acknowledges it.
        assert_eq!(q.pop_for_admission(), None);
        assert!(q.release_reservation(A, generation));
        assert!(!q.release_reservation(A, generation));
        let (connection, _, _, length) = q.pop_for_admission().unwrap();
        assert_eq!((connection, length), (B, 1));
    }

    #[test]
    fn a_queued_client_can_cancel_or_disconnect() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(1, 1);
        q.try_enter(WORLD, A);
        q.try_enter(WORLD, B);
        q.try_enter(WORLD, C);

        assert!(q.remove_queued(B));
        assert_eq!(q.notices()[1].connection_id, C);
        assert_eq!(q.notices()[1].my_turn, 2);
        assert!(!q.remove_queued(B));
        assert!(!q.is_queued(B));
    }

    #[test]
    fn a_full_queue_refuses_new_arrivals() {
        let mut q = WorldQueue::new(1, 2);
        q.set_online(1, 1);
        assert_eq!(q.try_enter(WORLD, A), Admission::Queued);
        assert_eq!(q.try_enter(WORLD, B), Admission::Queued);
        assert_eq!(q.try_enter(WORLD, C), Admission::Full);
        assert_eq!(q.queue_len(), 2);
    }

    #[test]
    fn a_direct_admission_reserves_the_slot_immediately() {
        let mut q = WorldQueue::new(1, 10);
        let Admission::Direct {
            reservation: Some(generation),
        } = q.try_enter(WORLD, A)
        else {
            panic!("expected a reserved direct admission");
        };
        // The slot is taken locally until the game's report includes the pending cookie.
        assert_eq!(q.try_enter(WORLD, B), Admission::Queued);
        assert_eq!(q.pop_for_admission(), None);
        // A disappeared before it connected: the game reports 0 and the reservation is released.
        q.set_online(0, 2);
        assert!(q.release_reservation(A, generation));
        assert_eq!(
            q.pop_for_admission().map(|(c, _, _, l)| (c, l)),
            Some((B, 0))
        );
    }

    #[test]
    fn occupancy_reports_ignore_stale_sequences() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(5, 10);
        q.set_online(3, 9); // older snapshot: ignored
        assert_eq!(q.online(), 5);
        q.set_online(4, 11);
        assert_eq!(q.online(), 4);
        // An old game server without the extension is always applied.
        q.set_online(2, 0);
        assert_eq!(q.online(), 2);
    }

    #[test]
    fn a_mirror_keeps_its_own_world_id_in_the_queue_packet() {
        let mut q = WorldQueue::new(1, 10);
        q.set_online(1, 1);
        q.try_enter(MIRROR, A);
        assert_eq!(q.notices()[0].world_id, MIRROR);
        q.set_online(0, 2);
        let (connection, world, _, _) = q.pop_for_admission().unwrap();
        assert_eq!((connection, world), (A, MIRROR));
    }

    #[test]
    fn only_the_matching_reservation_generation_is_released() {
        let mut q = WorldQueue::new(2, 10);
        // Two admissions of the same (reused) connection, e.g. a cancel and a re-enter.
        let Admission::Direct {
            reservation: Some(first),
        } = q.try_enter(WORLD, A)
        else {
            panic!("first admission");
        };
        let Admission::Direct {
            reservation: Some(second),
        } = q.try_enter(WORLD, A)
        else {
            panic!("second admission");
        };
        assert_ne!(first, second);
        // A late release of the second generation must leave the first untouched (a stale
        // timeout for the first admission is harmless, a stale one for a newer admission is not).
        assert!(q.release_reservation(A, second));
        assert_eq!(q.reserved_generations(A), vec![first]);
        assert!(q.release_reservation(A, first));
        assert!(!q.is_reserved(A));
    }

    #[test]
    fn game_answers_release_the_oldest_reservation_in_order() {
        let mut q = WorldQueue::new(2, 10);
        let Admission::Direct {
            reservation: Some(_),
        } = q.try_enter(WORLD, A)
        else {
            panic!("first admission");
        };
        let Admission::Direct {
            reservation: Some(_),
        } = q.try_enter(WORLD, A)
        else {
            panic!("second admission");
        };
        assert!(q.release_oldest_reservation(A));
        assert!(q.is_reserved(A));
        assert!(q.release_oldest_reservation(A));
        assert!(!q.is_reserved(A));
        // A cancel drops the newest (the admission the client is cancelling now).
        let Admission::Direct {
            reservation: Some(_),
        } = q.try_enter(WORLD, A)
        else {
            panic!("third admission");
        };
        let Admission::Direct {
            reservation: Some(_),
        } = q.try_enter(WORLD, A)
        else {
            panic!("fourth admission");
        };
        assert!(q.release_newest_reservation(A));
        assert!(q.is_reserved(A));
    }

    #[test]
    fn unregister_keeps_capacity_and_rearms_the_warning() {
        let mut q = WorldQueue::new(2, 10);
        q.set_online(2, 7);
        q.try_enter(WORLD, A);
        assert!(q.needs_missing_occupancy_warning());
        assert!(!q.needs_missing_occupancy_warning());
        assert_eq!(q.clear_for_unregister(), vec![A]);
        assert_eq!(q.online(), 0);
        assert_eq!(q.capacity(), 2);
        assert!(q.is_enabled());
        assert!(q.needs_missing_occupancy_warning());
    }
}
