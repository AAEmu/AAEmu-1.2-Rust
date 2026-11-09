//! Abuse limits of the public login listener (Rust only): a failed-password limiter keyed by
//! client IP or account name, and a per-IP account creation limit. Both are pure over a caller
//! supplied clock and keep their maps bounded.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Most keys a limiter tracks; further keys are not tracked until entries expire.
const MAX_TRACKED: usize = 65_536;
/// Expired entries are dropped whenever the table has this many entries.
const PRUNE_AT: usize = 1024;
/// Longest account name key (names beyond the valid length are truncated, not stored whole).
const MAX_KEY_CHARS: usize = 64;

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    window_start: Instant,
    blocked_until: Option<Instant>,
}

/// `limit` failures within `window` block the key for `block`. `limit == 0` switches it off.
#[derive(Debug)]
pub struct FailureLimiter<K: Eq + Hash> {
    limit: u32,
    window: Duration,
    block: Duration,
    keys: Mutex<HashMap<K, Entry>>,
}

impl<K: Eq + Hash> FailureLimiter<K> {
    pub fn new(limit: u32, window: Duration, block: Duration) -> Self {
        Self {
            limit,
            window,
            block,
            keys: Mutex::new(HashMap::new()),
        }
    }

    /// `true` while `key` is blocked.
    pub fn is_blocked(&self, key: &K, now: Instant) -> bool {
        if self.limit == 0 {
            return false;
        }
        let mut keys = self.keys.lock().unwrap_or_else(|p| p.into_inner());
        match keys.get(key).and_then(|e| e.blocked_until) {
            Some(until) if now < until => true,
            Some(_) => {
                keys.remove(key);
                false
            }
            None => false,
        }
    }

    /// Counts one failure of `key`; returns `true` when this failure started a block.
    pub fn record_failure(&self, key: K, now: Instant) -> bool {
        if self.limit == 0 {
            return false;
        }
        let mut keys = self.keys.lock().unwrap_or_else(|p| p.into_inner());
        if keys.len() >= PRUNE_AT {
            let window = self.window;
            keys.retain(|_, e| match e.blocked_until {
                Some(until) => now < until,
                None => now.saturating_duration_since(e.window_start) < window,
            });
        }
        if keys.len() >= MAX_TRACKED && !keys.contains_key(&key) {
            return false;
        }
        let entry = keys.entry(key).or_insert(Entry {
            failures: 0,
            window_start: now,
            blocked_until: None,
        });
        if entry.blocked_until.is_some_and(|until| now < until) {
            return false;
        }
        if entry.blocked_until.is_some()
            || now.saturating_duration_since(entry.window_start) >= self.window
        {
            *entry = Entry {
                failures: 0,
                window_start: now,
                blocked_until: None,
            };
        }
        entry.failures += 1;
        if entry.failures >= self.limit {
            entry.blocked_until = Some(now + self.block);
            return true;
        }
        false
    }

    /// Forgets the failures of `key` (a successful login).
    pub fn clear(&self, key: &K) {
        self.keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.keys.lock().unwrap().len()
    }
}

/// The account-name key of the failure limiter: case-folded (MySQL compares names
/// case-insensitively) and bounded in length.
pub fn account_key(username: &str) -> String {
    username
        .chars()
        .take(MAX_KEY_CHARS)
        .flat_map(char::to_lowercase)
        .collect()
}

/// The failed-password limiters of [`crate::controllers::login::LoginController`].
#[derive(Debug)]
pub struct PasswordLimits {
    by_ip: FailureLimiter<IpAddr>,
    by_account: FailureLimiter<String>,
}

/// Which key of [`PasswordLimits::record_failure`] started a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NewBlocks {
    pub ip: bool,
    pub account: bool,
}

impl PasswordLimits {
    pub fn new(limit: u32, window: Duration, block: Duration) -> Self {
        Self {
            by_ip: FailureLimiter::new(limit, window, block),
            by_account: FailureLimiter::new(limit, window, block),
        }
    }

    pub fn is_blocked(&self, ip: IpAddr, username: &str, now: Instant) -> bool {
        self.by_ip.is_blocked(&ip, now) || self.by_account.is_blocked(&account_key(username), now)
    }

    pub fn record_failure(&self, ip: IpAddr, username: &str, now: Instant) -> NewBlocks {
        NewBlocks {
            ip: self.by_ip.record_failure(ip, now),
            account: self.by_account.record_failure(account_key(username), now),
        }
    }

    /// A successful login clears the account counter (the IP counter keeps counting).
    pub fn record_success(&self, username: &str) {
        self.by_account.clear(&account_key(username));
    }
}

/// New accounts per IP per hour (`0` = unlimited); loopback addresses are exempt.
#[derive(Debug)]
pub struct CreationLimit {
    per_window: u32,
    window: Duration,
    created: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl CreationLimit {
    pub fn new(per_window: u32, window: Duration) -> Self {
        Self {
            per_window,
            window,
            created: Mutex::new(HashMap::new()),
        }
    }

    /// Reserves one creation for `ip`; `false` when `ip` already created its share within the
    /// window. A refused or untracked-because-full request does not consume anything.
    pub fn try_acquire(&self, ip: IpAddr, now: Instant) -> bool {
        if self.per_window == 0 || ip.is_loopback() {
            return true;
        }
        let mut created = self.created.lock().unwrap_or_else(|p| p.into_inner());
        if created.len() >= PRUNE_AT {
            let window = self.window;
            created.retain(|_, times| {
                times
                    .back()
                    .is_some_and(|t| now.saturating_duration_since(*t) < window)
            });
        }
        if created.len() >= MAX_TRACKED && !created.contains_key(&ip) {
            // Fail closed: an attacker that fills the table must not get unlimited accounts.
            return false;
        }
        let times = created.entry(ip).or_default();
        while times
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            times.pop_front();
        }
        if times.len() >= self.per_window as usize {
            return false;
        }
        times.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    const WINDOW: Duration = Duration::from_secs(300);

    #[test]
    fn blocks_after_the_limit_and_recovers() {
        let l = FailureLimiter::new(3, WINDOW, WINDOW);
        let t0 = Instant::now();
        assert!(!l.record_failure(ip(1), t0));
        assert!(!l.record_failure(ip(1), t0 + Duration::from_secs(1)));
        assert!(!l.is_blocked(&ip(1), t0 + Duration::from_secs(1)));
        assert!(l.record_failure(ip(1), t0 + Duration::from_secs(2)));
        assert!(l.is_blocked(&ip(1), t0 + Duration::from_secs(3)));
        assert!(!l.is_blocked(&ip(2), t0 + Duration::from_secs(3)));
        // The block expires after `block`, and counting starts afresh.
        let later = t0 + Duration::from_secs(2) + WINDOW;
        assert!(!l.is_blocked(&ip(1), later));
        assert!(!l.record_failure(ip(1), later));
    }

    #[test]
    fn failures_outside_the_window_do_not_add_up() {
        let l = FailureLimiter::new(2, WINDOW, WINDOW);
        let t0 = Instant::now();
        assert!(!l.record_failure(ip(1), t0));
        assert!(!l.record_failure(ip(1), t0 + WINDOW));
        assert!(!l.is_blocked(&ip(1), t0 + WINDOW));
    }

    #[test]
    fn zero_limit_is_off() {
        let l = FailureLimiter::new(0, WINDOW, WINDOW);
        let t0 = Instant::now();
        for _ in 0..50 {
            assert!(!l.record_failure(ip(1), t0));
        }
        assert!(!l.is_blocked(&ip(1), t0));
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn the_table_stays_bounded() {
        let l = FailureLimiter::new(5, WINDOW, WINDOW);
        let t0 = Instant::now();
        // Many one-off keys: old entries are pruned once the window has passed.
        for i in 0..3000u32 {
            let ip = IpAddr::from([10, (i >> 16) as u8, (i >> 8) as u8, i as u8]);
            l.record_failure(ip, t0 + Duration::from_secs(u64::from(i)));
        }
        assert!(l.len() < 3000);
        // Names are bounded in length.
        assert_eq!(account_key(&"A".repeat(500)).len(), MAX_KEY_CHARS);
        assert_eq!(account_key("Nick"), "nick");
    }

    #[test]
    fn password_limits_track_ip_and_account_separately() {
        let l = PasswordLimits::new(2, WINDOW, WINDOW);
        let t0 = Instant::now();
        assert_eq!(l.record_failure(ip(1), "Alice", t0), NewBlocks::default());
        // Another IP guessing the same account name blocks the account, not the IP.
        let blocks = l.record_failure(ip(2), "alice", t0);
        assert_eq!(
            blocks,
            NewBlocks {
                ip: false,
                account: true
            }
        );
        assert!(l.is_blocked(ip(3), "ALICE", t0));
        assert!(!l.is_blocked(ip(2), "bob", t0));
        // The first IP spraying another account reaches its own limit.
        let blocks = l.record_failure(ip(1), "bob", t0);
        assert!(blocks.ip && !blocks.account);
        assert!(l.is_blocked(ip(1), "carol", t0));
    }

    #[test]
    fn a_success_clears_the_account_counter() {
        let l = PasswordLimits::new(3, WINDOW, WINDOW);
        let t0 = Instant::now();
        l.record_failure(ip(1), "alice", t0);
        l.record_failure(ip(2), "alice", t0);
        l.record_success("Alice");
        assert!(!l.record_failure(ip(3), "alice", t0).account);
        assert!(!l.is_blocked(ip(9), "alice", t0));
    }

    #[test]
    fn creation_limit_is_per_ip_per_window_and_exempts_loopback() {
        let hour = Duration::from_secs(3600);
        let l = CreationLimit::new(2, hour);
        let t0 = Instant::now();
        assert!(l.try_acquire(ip(1), t0));
        assert!(l.try_acquire(ip(1), t0 + Duration::from_secs(10)));
        assert!(!l.try_acquire(ip(1), t0 + Duration::from_secs(20)));
        assert!(l.try_acquire(ip(2), t0 + Duration::from_secs(20)));
        // The oldest creation ages out of the window.
        assert!(l.try_acquire(ip(1), t0 + hour));
        assert!(!l.try_acquire(ip(1), t0 + hour + Duration::from_secs(1)));
        // Loopback (local dev, bot farms) and 0 (unlimited) never refuse.
        for _ in 0..20 {
            assert!(l.try_acquire(IpAddr::from([127, 0, 0, 1]), t0));
            assert!(l.try_acquire(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), t0));
        }
        let off = CreationLimit::new(0, hour);
        for _ in 0..20 {
            assert!(off.try_acquire(ip(1), t0));
        }
    }
}
