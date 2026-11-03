//! Connection bookkeeping half of `AccountManager` (`Add`, `Remove`, `Contains`, `Count`) and
//! the per-account locks (`_locks`). The account-details queries live in [`crate::db`]; the
//! full `Add` (timed rewards), the credits / loyalty / labor / divine-clock updates and the
//! dead-connection sweeper are in `systems::services::accounts`.
//!
//! WP-B adds the per-account runtime state [`AccountState`] (payment + premium point), loaded
//! from `aaemu_game.accounts` by `systems::services::accounts`. The timed-reward task and the
//! skill requirement checks only have an account id, so the state must live here rather than on
//! `GameConnection` (which is a cached copy).

use std::sync::Arc;

use dashmap::DashMap;

use crate::net::connection::AccountPayment;

/// Payment state and premium point of an account, as last read from / written to MySQL.
#[derive(Debug, Clone, Default)]
pub struct AccountState {
    pub payment: AccountPayment,
    /// `accounts.premium_point`, clamped to `premium_configs.max_point` on load.
    pub premium_point: i32,
}

#[derive(Debug, Default)]
pub struct AccountManager {
    /// account id -> game connection id (`_accounts`).
    accounts: DashMap<u32, u32>,
    /// account id -> payment / premium state (WP-B).
    states: DashMap<u32, AccountState>,
    /// `_locks` (account id -> lock serialising that account's DB updates).
    locks: DashMap<u32, Arc<tokio::sync::Mutex<()>>>,
}

impl AccountManager {
    /// Registers the account; returns `false` if it was already present (C# `Add` then returns
    /// early and skips the login-time update).
    pub fn try_add(&self, account_id: u32, connection_id: u32) -> bool {
        match self.accounts.entry(account_id) {
            dashmap::mapref::entry::Entry::Occupied(_) => false,
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(connection_id);
                true
            }
        }
    }

    /// `Remove(id)`. Note: C# removes the account by id, even if the entry belongs to another
    /// connection of the same account (e.g. a connection whose token check failed).
    pub fn remove(&self, account_id: u32) {
        self.accounts.remove(&account_id);
        self.states.remove(&account_id);
    }

    /// `Remove(id)` that only drops the entry registered by `connection_id`. Deviation from the
    /// C# (which removes by account id): a lingering old connection that closes after the
    /// account already logged in again on a new connection must not unregister the new one.
    /// Returns `true` when this connection owned the entry.
    pub fn remove_if_owned(&self, account_id: u32, connection_id: u32) -> bool {
        let removed = self
            .accounts
            .remove_if(&account_id, |_, conn| *conn == connection_id)
            .is_some();
        if removed {
            self.states.remove(&account_id);
        }
        removed
    }

    pub fn contains(&self, account_id: u32) -> bool {
        self.accounts.contains_key(&account_id)
    }

    /// The connection currently registered for `account_id` (`None` when it is free).
    pub fn connection_of(&self, account_id: u32) -> Option<u32> {
        self.accounts.get(&account_id).map(|entry| *entry)
    }

    pub fn count(&self) -> usize {
        self.accounts.len()
    }

    /// `_accounts` as (account id, connection id) pairs.
    pub fn entries(&self) -> Vec<(u32, u32)> {
        self.accounts
            .iter()
            .map(|e| (*e.key(), *e.value()))
            .collect()
    }

    /// The last loaded payment / premium state (`None` = not loaded / already removed).
    pub fn state(&self, account_id: u32) -> Option<AccountState> {
        self.states.get(&account_id).map(|s| s.clone())
    }

    /// Replaces the cached state of an account (the callers write MySQL first, under
    /// [`AccountManager::lock_of`]).
    pub fn set_state(&self, account_id: u32, state: AccountState) {
        self.states.insert(account_id, state);
    }

    /// The lock object of `account_id` (`lock (_locks) { ... }` + `lock (accLock)`).
    pub fn lock_of(&self, account_id: u32) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.entry(account_id).or_default().clone()
    }
}
