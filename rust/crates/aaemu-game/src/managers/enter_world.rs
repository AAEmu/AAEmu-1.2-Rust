//! Lobby half of `EnterWorldManager` (`AddAccount`, the token check of `Login`,
//! `Leave(ServerSelect)`). The in-world leave path (`LeaveWorldTask`, logout timers) is a later
//! phase.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a world cookie waits for the client to connect to the game server before it stops
/// counting as an occupied slot. A client connects immediately after `ACWorldCookie`; the expiry
/// only keeps a cookie whose client vanished from blocking the login queue for ever.
pub const PENDING_TOKEN_LIFETIME: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct PendingToken {
    account_id: u32,
    created: Instant,
    /// The cookie was accepted by `login()` and the account registration is in flight. It still
    /// occupies a slot (`AccountManager.count()` does not include the account yet), but a second
    /// `login()` with the same token must not succeed.
    consumed: bool,
}

/// Outcome of [`EnterWorldManager::login`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginDecision {
    /// The token was registered for this account; the entry has been consumed.
    Accepted,
    /// The token was registered for another account ("phishing attempt?"); the entry is kept.
    AccountMismatch { expected: u32 },
    /// Unknown token ("hacking attempt?").
    InvalidToken,
}

/// Outcome of [`EnterWorldManager::add_account`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddAccountResult {
    /// Send `GLPlayerEnterPacket` with this `result` byte.
    Reply(u8),
    /// The connection id was already registered for `stale_account` (a client that got its
    /// world cookie but never connected to the game server). The login server reuses a
    /// connection id only after that connection closed, so the old entry is replaced; reply 0.
    /// (C# `Dictionary.Add` throws there, no reply is sent and the login link breaks: every
    /// later login on the reused id is denied.)
    ReplacedStale { stale_account: u32 },
}

#[derive(Debug)]
pub struct EnterWorldManager {
    /// `_accounts`: login-server connection id (the client's cookie / token) -> pending account.
    accounts: Mutex<HashMap<u32, PendingToken>>,
    /// character id -> game connection id. The session that is entering or playing the character;
    /// taken before the select's MySQL load and kept until the departure save has finished, so a
    /// second connection can neither enter the same character twice nor load its row while the
    /// previous session is still saving it.
    characters: Mutex<HashMap<u32, u32>>,
    /// Rust addition for the login queue: how long an unconsumed world cookie counts as a slot.
    token_lifetime: Duration,
}

impl Default for EnterWorldManager {
    fn default() -> Self {
        Self {
            accounts: Mutex::new(HashMap::new()),
            characters: Mutex::new(HashMap::new()),
            token_lifetime: PENDING_TOKEN_LIFETIME,
        }
    }
}

impl EnterWorldManager {
    /// Removes unconsumed cookies older than the lifetime. An accepted (consumed) cookie is kept
    /// until its account registration finishes, so the occupancy never under-counts an in-flight
    /// login.
    fn prune_expired(accounts: &mut HashMap<u32, PendingToken>, lifetime: Duration) {
        let now = Instant::now();
        accounts.retain(|_, token| {
            token.consumed || now.saturating_duration_since(token.created) < lifetime
        });
    }

    /// Number of world cookies that still occupy a slot: unconsumed ones younger than the
    /// lifetime, plus accepted cookies whose account registration is in flight. Together with
    /// `AccountManager::count` this is the world's occupancy reported to the login queue.
    ///
    /// Expired entries are only skipped here, never removed: `login()` must keep accepting a
    /// cookie of any age (with the queue disabled its behaviour is exactly as before the queue
    /// existed). `add_account` prunes them when a connection id is reused.
    pub fn pending_count(&self) -> usize {
        let accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        accounts
            .values()
            .filter(|token| {
                token.consumed || now.saturating_duration_since(token.created) < self.token_lifetime
            })
            .count()
    }
}

impl EnterWorldManager {
    /// `AddAccount` bookkeeping. `already_connected` is `accountManager.Contains(accountId)`:
    /// such accounts are refused with result 1 and not registered; otherwise the cookie is
    /// registered and result 0 is returned.
    pub fn add_account(
        &self,
        account_id: u32,
        connection_id: u32,
        already_connected: bool,
    ) -> AddAccountResult {
        if already_connected {
            return AddAccountResult::Reply(1);
        }
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_expired(&mut accounts, self.token_lifetime);
        let pending = PendingToken {
            account_id,
            created: Instant::now(),
            consumed: false,
        };
        match accounts.insert(connection_id, pending) {
            Some(stale) => AddAccountResult::ReplacedStale {
                stale_account: stale.account_id,
            },
            None => AddAccountResult::Reply(0),
        }
    }

    /// The token check of `Login(connection, accountId, token)`. The cookie is only marked as
    /// accepted here (still counted as an occupied slot); [`Self::forget`] removes it once the
    /// account registration finished. A cookie has no age limit here: with the queue disabled its
    /// lifetime must behave exactly as before the queue existed.
    pub fn login(&self, account_id: u32, token: u32) -> LoginDecision {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match accounts.get_mut(&token) {
            Some(entry) if entry.consumed => LoginDecision::InvalidToken,
            Some(entry) if entry.account_id == account_id => {
                entry.consumed = true;
                LoginDecision::Accepted
            }
            Some(entry) => LoginDecision::AccountMismatch {
                expected: entry.account_id,
            },
            None => LoginDecision::InvalidToken,
        }
    }

    /// Drops an accepted cookie after its account registration finished (success or refusal).
    pub fn forget(&self, token: u32) {
        self.accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&token);
    }

    /// Atomically reserves `character_id` for `connection_id`. `Err(owner)` means another
    /// connection is entering or playing the character; `Ok(false)` that this connection already
    /// owns it, `Ok(true)` that the reservation was just taken.
    pub fn reserve_character(&self, character_id: u32, connection_id: u32) -> Result<bool, u32> {
        let mut characters = self
            .characters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match characters.entry(character_id) {
            std::collections::hash_map::Entry::Occupied(entry) if *entry.get() == connection_id => {
                Ok(false)
            }
            std::collections::hash_map::Entry::Occupied(entry) => Err(*entry.get()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(connection_id);
                Ok(true)
            }
        }
    }

    /// Drops the reservation of `character_id` when `connection_id` still owns it (the departure
    /// save has finished, or the entry was abandoned).
    pub fn release_character(&self, character_id: u32, connection_id: u32) {
        let mut characters = self
            .characters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if characters.get(&character_id) == Some(&connection_id) {
            characters.remove(&character_id);
        }
    }

    /// Drops every reservation owned by a finished connection and returns the character ids.
    /// Panic-safe: a poisoned lock still releases, so a failed disconnect cleanup cannot leave a
    /// character reserved for ever.
    pub fn release_connection(&self, connection_id: u32) -> Vec<u32> {
        let mut characters = self
            .characters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut released = Vec::new();
        characters.retain(|character_id, owner| {
            if *owner == connection_id {
                released.push(*character_id);
                false
            } else {
                true
            }
        });
        released
    }

    /// The connection currently entering or playing `character_id`.
    pub fn character_owner(&self, character_id: u32) -> Option<u32> {
        self.characters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&character_id)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_login_follow_csharp() {
        let m = EnterWorldManager::default();
        assert_eq!(m.add_account(10, 111, false), AddAccountResult::Reply(0));
        // Contains(accountId): refused, nothing registered.
        assert_eq!(m.add_account(11, 112, true), AddAccountResult::Reply(1));
        // A reused connection id replaces the stale entry (C#: Dictionary.Add throws).
        assert_eq!(
            m.add_account(12, 111, false),
            AddAccountResult::ReplacedStale { stale_account: 10 }
        );
        assert_eq!(
            m.login(10, 111),
            LoginDecision::AccountMismatch { expected: 12 }
        );
        assert_eq!(
            m.add_account(10, 111, false),
            AddAccountResult::ReplacedStale { stale_account: 12 }
        );
        // Wrong account for a known token: refused, entry kept.
        assert_eq!(
            m.login(99, 111),
            LoginDecision::AccountMismatch { expected: 10 }
        );
        assert_eq!(m.login(11, 112), LoginDecision::InvalidToken);
        assert_eq!(m.login(10, 111), LoginDecision::Accepted);
        // Consumed: a second connection with the same token is refused.
        assert_eq!(m.login(10, 111), LoginDecision::InvalidToken);
        assert_eq!(m.login(10, 0), LoginDecision::InvalidToken);
    }

    #[test]
    fn world_queue_slot_occupancy_counts_accepted_cookies_until_forgotten() {
        let m = EnterWorldManager::default();
        m.add_account(10, 111, false);
        m.add_account(10, 112, false);
        assert_eq!(m.pending_count(), 2);
        // Accepted: the account registration is in flight, so the slot is still occupied.
        assert_eq!(m.login(10, 111), LoginDecision::Accepted);
        assert_eq!(m.pending_count(), 2);
        // The same token cannot be used twice.
        assert_eq!(m.login(10, 111), LoginDecision::InvalidToken);
        m.forget(111);
        assert_eq!(m.pending_count(), 1);
    }

    #[test]
    fn world_queue_expiry_only_changes_the_occupancy_not_token_acceptance() {
        let m = EnterWorldManager {
            token_lifetime: Duration::ZERO,
            ..Default::default()
        };
        m.add_account(10, 111, false);
        // Not counted as an occupied slot...
        assert_eq!(m.pending_count(), 0);
        // ...but a client that connects late still enters, exactly as before the queue existed.
        assert_eq!(m.login(10, 111), LoginDecision::Accepted);
        assert_eq!(m.pending_count(), 1);
        m.forget(111);
        assert_eq!(m.pending_count(), 0);
    }

    #[test]
    fn character_reservation_is_atomic_across_connections() {
        let m = EnterWorldManager::default();
        assert_eq!(m.reserve_character(50, 1), Ok(true));
        // A second connection cannot enter the same character.
        assert_eq!(m.reserve_character(50, 2), Err(1));
        assert_eq!(m.character_owner(50), Some(1));
        // The owner may reserve again (select, then enter_world); nothing new was taken.
        assert_eq!(m.reserve_character(50, 1), Ok(false));
        // Only the owner can release it.
        m.release_character(50, 2);
        assert_eq!(m.character_owner(50), Some(1));
        m.release_character(50, 1);
        assert_eq!(m.character_owner(50), None);
        // Now the second connection gets it.
        assert_eq!(m.reserve_character(50, 2), Ok(true));
    }

    #[test]
    fn release_connection_drops_only_its_own_characters() {
        let m = EnterWorldManager::default();
        assert_eq!(m.reserve_character(50, 1), Ok(true));
        assert_eq!(m.reserve_character(51, 1), Ok(true));
        assert_eq!(m.reserve_character(52, 2), Ok(true));
        // Connection id 0 is not a live client session: only the dying connection's entries go.
        assert_eq!(m.reserve_character(53, 0), Ok(true));
        let mut released = m.release_connection(1);
        released.sort_unstable();
        assert_eq!(released, vec![50, 51]);
        assert_eq!(m.character_owner(52), Some(2));
        assert_eq!(m.character_owner(53), Some(0));
        // Idempotent.
        assert!(m.release_connection(1).is_empty());
    }

    #[test]
    fn a_poisoned_reservation_lock_still_releases() {
        let m = EnterWorldManager::default();
        assert_eq!(m.reserve_character(50, 1), Ok(true));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.characters.lock().expect("lock");
            panic!("lock holder panicked");
        }));
        // The poisoned mutex must not keep the character reserved after a failed cleanup.
        assert_eq!(m.release_connection(1), vec![50]);
        assert_eq!(m.character_owner(50), None);
    }
}
