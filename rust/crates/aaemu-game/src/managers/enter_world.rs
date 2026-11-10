//! Lobby half of `EnterWorldManager` (`AddAccount`, the token check of `Login`,
//! `Leave(ServerSelect)`). The in-world leave path (`LeaveWorldTask`, logout timers) is a later
//! phase.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a world cookie waits for the client to connect to the game server before it stops
/// counting as an occupied slot and is refused by [`EnterWorldManager::login`]. A client connects
/// immediately after `ACWorldCookie`; the expiry keeps a cookie whose client vanished from
/// blocking the login queue for ever and from being guessed long after it was issued.
pub const PENDING_TOKEN_LIFETIME: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct PendingToken {
    account_id: u32,
    /// The login server's connection id of the client the cookie was issued for.
    connection_id: u32,
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
    /// The account is already connected: answer `GLPlayerEnterPacket` with result 1 (and no
    /// cookie); nothing was registered.
    AlreadyConnected,
    /// A world cookie was issued: answer with result 0 and this random cookie, which the login
    /// server relays to the client in `ACWorldCookie`.
    Registered { cookie: u32 },
    /// The connection id was still pending for `stale_account` (a client that got its world
    /// cookie but never connected to the game server). The login server reuses a connection id
    /// only after that connection closed, so the old entry is replaced; reply 0 with the new
    /// `cookie`. (C# `Dictionary.Add` throws there, no reply is sent and the login link breaks:
    /// every later login on the reused id is denied.)
    ReplacedStale { stale_account: u32, cookie: u32 },
}

impl AddAccountResult {
    /// The cookie that was issued, if any.
    pub fn cookie(&self) -> Option<u32> {
        match *self {
            Self::AlreadyConnected => None,
            Self::Registered { cookie } | Self::ReplacedStale { cookie, .. } => Some(cookie),
        }
    }
}

/// Most IPs the [`EnterWorldThrottle`] tracks; further IPs are not tracked until entries expire.
const THROTTLE_MAX_TRACKED_IPS: usize = 65_536;
/// Expired entries are dropped whenever the table has this many entries.
const THROTTLE_PRUNE_AT: usize = 1024;

#[derive(Debug, Clone, Copy)]
struct ThrottleEntry {
    failures: u32,
    window_start: Instant,
    blocked_until: Option<Instant>,
}

/// Per-IP throttle for failed `X2EnterWorld` attempts (Rust addition): `limit` failures within
/// `window` block the IP for `block`. The caller passes the clock so the logic is testable.
#[derive(Debug, Default)]
pub struct EnterWorldThrottle {
    ips: Mutex<HashMap<IpAddr, ThrottleEntry>>,
}

impl EnterWorldThrottle {
    /// `true` while `ip` is blocked. `limit == 0` (throttle off) never blocks.
    pub fn is_blocked(&self, ip: IpAddr, limit: u32, now: Instant) -> bool {
        if limit == 0 {
            return false;
        }
        let mut ips = self.ips.lock().unwrap_or_else(|p| p.into_inner());
        match ips.get(&ip).and_then(|e| e.blocked_until) {
            Some(until) if now < until => true,
            Some(_) => {
                ips.remove(&ip);
                false
            }
            None => false,
        }
    }

    /// Counts one failed attempt from `ip`; returns `true` when this failure blocked the IP.
    pub fn record_failure(
        &self,
        ip: IpAddr,
        limit: u32,
        window: Duration,
        block: Duration,
        now: Instant,
    ) -> bool {
        if limit == 0 {
            return false;
        }
        let mut ips = self.ips.lock().unwrap_or_else(|p| p.into_inner());
        if ips.len() >= THROTTLE_PRUNE_AT {
            ips.retain(|_, e| match e.blocked_until {
                Some(until) => now < until,
                None => now.saturating_duration_since(e.window_start) < window,
            });
        }
        if ips.len() >= THROTTLE_MAX_TRACKED_IPS && !ips.contains_key(&ip) {
            return false;
        }
        let entry = ips.entry(ip).or_insert(ThrottleEntry {
            failures: 0,
            window_start: now,
            blocked_until: None,
        });
        if entry.blocked_until.is_some_and(|until| now < until) {
            return false;
        }
        if entry.blocked_until.is_some()
            || now.saturating_duration_since(entry.window_start) >= window
        {
            *entry = ThrottleEntry {
                failures: 0,
                window_start: now,
                blocked_until: None,
            };
        }
        entry.failures += 1;
        if entry.failures >= limit {
            entry.blocked_until = Some(now + block);
            return true;
        }
        false
    }

    /// Number of tracked IPs (tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.ips.lock().unwrap().len()
    }
}

#[derive(Debug)]
pub struct EnterWorldManager {
    /// `_accounts`: random world cookie (Rust: generated here, not the login connection id) ->
    /// pending account.
    accounts: Mutex<HashMap<u32, PendingToken>>,
    /// character id -> game connection id. The session that is entering or playing the character;
    /// taken before the select's MySQL load and kept until the departure save has finished, so a
    /// second connection can neither enter the same character twice nor load its row while the
    /// previous session is still saving it.
    characters: Mutex<HashMap<u32, u32>>,
    /// Rust addition: how long an unconsumed world cookie counts as a slot and can be used.
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
    /// Expired entries are only skipped here, never removed; `add_account` prunes them when a
    /// connection id is reused.
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
    /// such accounts are refused with result 1 and not registered; otherwise a random non-zero
    /// cookie, unique among the pending ones, is registered and returned (Rust deviation: the C#
    /// used the login connection id, a small sequential integer anyone could guess).
    pub fn add_account(
        &self,
        account_id: u32,
        connection_id: u32,
        already_connected: bool,
    ) -> AddAccountResult {
        if already_connected {
            return AddAccountResult::AlreadyConnected;
        }
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_expired(&mut accounts, self.token_lifetime);
        // A reused login connection id replaces its stale entry. An accepted (consumed) entry is
        // mid-registration and stays.
        let stale = accounts
            .iter()
            .find(|(_, token)| token.connection_id == connection_id && !token.consumed)
            .map(|(cookie, token)| (*cookie, token.account_id));
        if let Some((stale_cookie, _)) = stale {
            accounts.remove(&stale_cookie);
        }
        let cookie = loop {
            let cookie = rand::random::<u32>();
            if cookie != 0 && !accounts.contains_key(&cookie) {
                break cookie;
            }
        };
        accounts.insert(
            cookie,
            PendingToken {
                account_id,
                connection_id,
                created: Instant::now(),
                consumed: false,
            },
        );
        match stale {
            Some((_, stale_account)) => AddAccountResult::ReplacedStale {
                stale_account,
                cookie,
            },
            None => AddAccountResult::Registered { cookie },
        }
    }

    /// The token check of `Login(connection, accountId, token)`; `token` is the world cookie
    /// [`Self::add_account`] issued, not the login connection id. The cookie is only marked as
    /// accepted here (still counted as an occupied slot); [`Self::forget`] removes it once the
    /// account registration finished. Deviation from the C# (cookies never expire): an unconsumed
    /// cookie older than the token lifetime is refused like an unknown one.
    pub fn login(&self, account_id: u32, token: u32) -> LoginDecision {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        match accounts.get_mut(&token) {
            Some(entry) if entry.consumed => LoginDecision::InvalidToken,
            Some(entry) if now.saturating_duration_since(entry.created) >= self.token_lifetime => {
                LoginDecision::InvalidToken
            }
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
        let a = m.add_account(10, 111, false);
        let cookie_a = a.cookie().unwrap();
        assert_eq!(a, AddAccountResult::Registered { cookie: cookie_a });
        // Contains(accountId): refused, nothing registered.
        assert_eq!(
            m.add_account(11, 112, true),
            AddAccountResult::AlreadyConnected
        );
        // A reused connection id replaces the stale entry (C#: Dictionary.Add throws).
        let b = m.add_account(12, 111, false);
        let cookie_b = b.cookie().unwrap();
        assert_eq!(
            b,
            AddAccountResult::ReplacedStale {
                stale_account: 10,
                cookie: cookie_b
            }
        );
        // The stale cookie died with its entry.
        assert_eq!(m.login(10, cookie_a), LoginDecision::InvalidToken);
        assert_eq!(
            m.login(10, cookie_b),
            LoginDecision::AccountMismatch { expected: 12 }
        );
        let c = m.add_account(10, 111, false);
        let cookie_c = c.cookie().unwrap();
        assert_eq!(
            c,
            AddAccountResult::ReplacedStale {
                stale_account: 12,
                cookie: cookie_c
            }
        );
        // Wrong account for a known token: refused, entry kept.
        assert_eq!(
            m.login(99, cookie_c),
            LoginDecision::AccountMismatch { expected: 10 }
        );
        assert_eq!(m.login(11, 112), LoginDecision::InvalidToken);
        assert_eq!(m.login(10, cookie_c), LoginDecision::Accepted);
        // Consumed: a second connection with the same token is refused.
        assert_eq!(m.login(10, cookie_c), LoginDecision::InvalidToken);
        assert_eq!(m.login(10, 0), LoginDecision::InvalidToken);
    }

    #[test]
    fn the_cookie_is_random_and_not_the_login_connection_id() {
        let m = EnterWorldManager::default();
        let mut cookies = std::collections::HashSet::new();
        for connection_id in 1..=200u32 {
            let cookie = m
                .add_account(10 + connection_id, connection_id, false)
                .cookie()
                .unwrap();
            assert_ne!(cookie, 0);
            assert!(
                cookies.insert(cookie),
                "cookies are unique among pending ones"
            );
        }
        // Presenting the login connection id instead of the cookie is refused (a random cookie
        // colliding with a small id would name another account, never this one).
        for connection_id in 1..=200u32 {
            assert_ne!(
                m.login(10 + connection_id, connection_id),
                LoginDecision::Accepted
            );
        }
    }

    #[test]
    fn world_queue_slot_occupancy_counts_accepted_cookies_until_forgotten() {
        let m = EnterWorldManager::default();
        let first = m.add_account(10, 111, false).cookie().unwrap();
        m.add_account(10, 112, false);
        assert_eq!(m.pending_count(), 2);
        // Accepted: the account registration is in flight, so the slot is still occupied.
        assert_eq!(m.login(10, first), LoginDecision::Accepted);
        assert_eq!(m.pending_count(), 2);
        // The same token cannot be used twice.
        assert_eq!(m.login(10, first), LoginDecision::InvalidToken);
        m.forget(first);
        assert_eq!(m.pending_count(), 1);
    }

    #[test]
    fn an_expired_cookie_is_neither_counted_nor_accepted() {
        let m = EnterWorldManager {
            token_lifetime: Duration::ZERO,
            ..Default::default()
        };
        let cookie = m.add_account(10, 111, false).cookie().unwrap();
        assert_eq!(m.pending_count(), 0);
        assert_eq!(m.login(10, cookie), LoginDecision::InvalidToken);
        // A fresh cookie is accepted, and once accepted its age no longer matters.
        let m = EnterWorldManager::default();
        let cookie = m.add_account(10, 111, false).cookie().unwrap();
        assert_eq!(m.login(10, cookie), LoginDecision::Accepted);
        assert_eq!(m.pending_count(), 1);
        m.forget(cookie);
        assert_eq!(m.pending_count(), 0);
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    #[test]
    fn throttle_blocks_after_the_limit_and_recovers_after_the_block() {
        let t = EnterWorldThrottle::default();
        let (window, block) = (Duration::from_secs(60), Duration::from_secs(300));
        let t0 = Instant::now();
        for i in 0..4 {
            assert!(!t.record_failure(ip(1), 5, window, block, t0 + Duration::from_secs(i)));
            assert!(!t.is_blocked(ip(1), 5, t0 + Duration::from_secs(i)));
        }
        assert!(t.record_failure(ip(1), 5, window, block, t0 + Duration::from_secs(5)));
        assert!(t.is_blocked(ip(1), 5, t0 + Duration::from_secs(6)));
        // Other IPs are unaffected.
        assert!(!t.is_blocked(ip(2), 5, t0 + Duration::from_secs(6)));
        // Still blocked just before the block ends, free right after (the entry is pruned).
        assert!(t.is_blocked(ip(1), 5, t0 + Duration::from_secs(304)));
        assert!(!t.is_blocked(ip(1), 5, t0 + Duration::from_secs(306)));
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn throttle_failures_outside_the_window_do_not_add_up_and_zero_is_off() {
        let t = EnterWorldThrottle::default();
        let (window, block) = (Duration::from_secs(60), Duration::from_secs(300));
        let t0 = Instant::now();
        for i in 0..10 {
            // One failure every 61 s never reaches 5 within a window.
            assert!(!t.record_failure(ip(1), 5, window, block, t0 + Duration::from_secs(61 * i)));
        }
        assert!(!t.is_blocked(ip(1), 5, t0 + Duration::from_secs(700)));
        for _ in 0..100 {
            assert!(!t.record_failure(ip(3), 0, window, block, t0));
        }
        assert!(!t.is_blocked(ip(3), 0, t0));
        assert_eq!(t.len(), 1);
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
