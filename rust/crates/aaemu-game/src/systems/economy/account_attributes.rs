//! `AccountAttributeEffect` and `SpecialEffect.AuctionPostAuthority` (special 117).
//!
//! Data: `account_attribute_effects` has three rows, all `kind_id = 1` (auction posting
//! authority): item 8000027 `경매장 등록권` adds it account-wide (`is_add`, `count` 0, `time` 0,
//! `bind_world` false), two `is_add = false` rows are the VIP-buff trigger and the removal
//! ticket (item 8000058). Special 117 fires on the Started triggers of buffs 7149 / 11034 /
//! 8000012 and grants kind 1 permanently. The former C# effect only traced and the special only
//! logged, so the behaviour below is derived from the data.
//!
//! The attributes are cached per account (`server.systems.economy.account_attributes`), loaded
//! when a character of the account is selected and persisted on every change. `has_account_attribute`
//! is ready for gating (the auction is not gated yet: every character currently gets buff 8000012
//! from `world/unit_state.rs` `UNIT_STATE_FORCED_BUFFS`, so a gate would be meaningless).

use std::collections::HashMap;
use std::sync::Mutex;

use sqlx::Row;

use crate::server::GameServer;

/// `kind_id` 1 of `account_attribute_effects` (auction posting authority).
pub const KIND_AUCTION_POST_AUTHORITY: i32 = 1;

/// One `account_attributes` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountAttribute {
    pub kind_id: i32,
    /// 0 = not world bound (`bind_world` false).
    pub world_id: i32,
    pub count: i32,
    /// Unix seconds UTC; `None` = permanent.
    pub end_time: Option<i64>,
}

impl AccountAttribute {
    /// Still granted at `now` (unix seconds).
    pub fn is_active(&self, now: i64) -> bool {
        self.end_time.is_none_or(|end| end > now)
    }
}

/// Per-account attribute cache (`AccountAttributes` in C# terms).
#[derive(Debug, Default)]
pub struct AccountAttributes {
    accounts: Mutex<HashMap<u32, Vec<AccountAttribute>>>,
}

impl AccountAttributes {
    /// The cached rows of `account_id` (empty when not loaded).
    pub fn get(&self, account_id: u32) -> Vec<AccountAttribute> {
        self.accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&account_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces the cache of `account_id` (after a load).
    pub fn replace(&self, account_id: u32, attributes: Vec<AccountAttribute>) {
        self.accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account_id, attributes);
    }

    /// Adds / refreshes one `(kind_id, world_id)` grant.
    pub fn upsert(&self, account_id: u32, attribute: AccountAttribute) {
        let mut map = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
        let rows = map.entry(account_id).or_default();
        rows.retain(|a| a.kind_id != attribute.kind_id || a.world_id != attribute.world_id);
        rows.push(attribute);
    }

    /// Deletes every grant of `kind_id` for the account (the `is_add = false` rows).
    pub fn remove_kind(&self, account_id: u32, kind_id: i32) {
        let mut map = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rows) = map.get_mut(&account_id) {
            rows.retain(|a| a.kind_id != kind_id);
        }
    }

    /// `has_account_attribute(account_id, kind)`: any active (non-expired) grant of `kind_id`,
    /// in any world. This is the helper a future auction gate can call.
    pub fn has_account_attribute(&self, account_id: u32, kind_id: i32) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.get(account_id)
            .iter()
            .any(|a| a.kind_id == kind_id && a.is_active(now))
    }
}

/// One `AccountAttributeEffect` template row, as applied to an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountAttributeGrant {
    pub kind_id: i32,
    pub bind_world: bool,
    pub is_add: bool,
    pub count: i32,
    /// Seconds; 0 = permanent (unit inferred, see [`apply_effect`]).
    pub time: i32,
}

/// `AccountAttributeEffect.Apply` + `SpecialEffect.AuctionPostAuthority`:
///
/// * `is_add` upserts `(kind_id, bind_world ? world_id : 0)`;
/// * otherwise every grant of `kind_id` is deleted (the removal tickets);
/// * `time == 0` is permanent, otherwise `end_time = now + time` seconds. **The data carries no
///   unit and the 1.2 values are all 0, so seconds are inferred** (matching the other effect
///   durations).
pub fn apply_effect(
    server: &GameServer,
    account_id: u32,
    world_id: u32,
    grant: AccountAttributeGrant,
) {
    let end_time = if grant.time == 0 {
        None
    } else {
        Some(chrono::Utc::now().timestamp() + i64::from(grant.time))
    };
    let attributes = &server.systems.economy.account_attributes;
    if grant.is_add {
        attributes.upsert(
            account_id,
            AccountAttribute {
                kind_id: grant.kind_id,
                world_id: if grant.bind_world { world_id as i32 } else { 0 },
                count: grant.count,
                end_time,
            },
        );
    } else {
        attributes.remove_kind(account_id, grant.kind_id);
    }
    persist_async(server, account_id);
}

/// Loads the account's rows into the cache (character select).
pub async fn load(server: &GameServer, account_id: u32) {
    match rows(server, account_id).await {
        Ok(attributes) => server
            .systems
            .economy
            .account_attributes
            .replace(account_id, attributes),
        Err(e) => tracing::error!("account_attributes load({account_id}): {e}"),
    }
}

/// Persists the cached rows of `account_id`, replacing the account's table rows.
pub async fn persist(server: &GameServer, account_id: u32) {
    let attributes = server.systems.economy.account_attributes.get(account_id);
    let result: Result<(), sqlx::Error> = async {
        let mut tx = server.db.begin().await?;
        sqlx::query("DELETE FROM account_attributes WHERE account_id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
        for a in &attributes {
            sqlx::query(
                "INSERT INTO account_attributes (account_id, kind_id, world_id, count, end_time) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(account_id)
            .bind(a.kind_id)
            .bind(a.world_id)
            .bind(a.count)
            .bind(
                a.end_time
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0).map(|d| d.naive_utc())),
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }
    .await;
    if let Err(e) = result {
        tracing::error!("account_attributes persist({account_id}): {e}");
    }
}

async fn rows(server: &GameServer, account_id: u32) -> Result<Vec<AccountAttribute>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT CAST(kind_id AS SIGNED) AS kind_id, CAST(world_id AS SIGNED) AS world_id, \
         CAST(count AS SIGNED) AS count, CAST(UNIX_TIMESTAMP(end_time) AS SIGNED) AS end_time \
         FROM account_attributes WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_all(&server.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AccountAttribute {
            kind_id: r.get::<i64, _>("kind_id") as i32,
            world_id: r.get::<i64, _>("world_id") as i32,
            count: r.get::<i64, _>("count") as i32,
            end_time: r.get::<Option<i64>, _>("end_time"),
        })
        .collect())
}

/// Saves in the background (skill effects are synchronous).
fn persist_async(server: &GameServer, account_id: u32) {
    let (Some(srv), Ok(handle)) = (server.self_arc(), tokio::runtime::Handle::try_current()) else {
        tracing::warn!("account_attributes: no Tokio runtime to persist account {account_id}");
        return;
    };
    handle.spawn(async move { persist(&srv, account_id).await });
}

/// Drops expired rows from the cache (not needed for `has_account_attribute`, which checks the
/// expiry; kept for callers that want the stored set pruned).
pub fn prune_expired(attributes: &mut Vec<AccountAttribute>, now: i64) {
    attributes.retain(|a| a.is_active(now));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(kind_id: i32, world_id: i32, end_time: Option<i64>) -> AccountAttribute {
        AccountAttribute {
            kind_id,
            world_id,
            count: 0,
            end_time,
        }
    }

    #[test]
    fn add_permanent_grants_the_kind() {
        let attributes = AccountAttributes::default();
        attributes.upsert(7, attr(KIND_AUCTION_POST_AUTHORITY, 0, None));
        assert!(attributes.has_account_attribute(7, KIND_AUCTION_POST_AUTHORITY));
        assert!(!attributes.has_account_attribute(7, 2));
        assert!(!attributes.has_account_attribute(8, KIND_AUCTION_POST_AUTHORITY));
        let stored = attributes.get(7);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].end_time, None);
    }

    #[test]
    fn remove_deletes_every_world_grant_of_the_kind() {
        let attributes = AccountAttributes::default();
        attributes.upsert(7, attr(KIND_AUCTION_POST_AUTHORITY, 0, None));
        attributes.upsert(7, attr(KIND_AUCTION_POST_AUTHORITY, 3, None));
        attributes.upsert(7, attr(2, 0, None));
        attributes.remove_kind(7, KIND_AUCTION_POST_AUTHORITY);
        assert!(!attributes.has_account_attribute(7, KIND_AUCTION_POST_AUTHORITY));
        assert!(attributes.has_account_attribute(7, 2));
    }

    #[test]
    fn expired_grants_are_inactive() {
        let attributes = AccountAttributes::default();
        let now = chrono::Utc::now().timestamp();
        attributes.upsert(7, attr(KIND_AUCTION_POST_AUTHORITY, 0, Some(now - 1)));
        assert!(!attributes.has_account_attribute(7, KIND_AUCTION_POST_AUTHORITY));
        attributes.upsert(7, attr(KIND_AUCTION_POST_AUTHORITY, 0, Some(now + 60)));
        assert!(attributes.has_account_attribute(7, KIND_AUCTION_POST_AUTHORITY));
    }

    #[test]
    fn upsert_replaces_the_same_kind_and_world_only() {
        let attributes = AccountAttributes::default();
        let now = chrono::Utc::now().timestamp();
        attributes.upsert(
            7,
            AccountAttribute {
                count: 1,
                ..attr(KIND_AUCTION_POST_AUTHORITY, 0, Some(now + 60))
            },
        );
        attributes.upsert(7, attr(KIND_AUCTION_POST_AUTHORITY, 3, None));
        attributes.upsert(
            7,
            AccountAttribute {
                count: 9,
                ..attr(KIND_AUCTION_POST_AUTHORITY, 0, None)
            },
        );
        let rows = attributes.get(7);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .find(|a| a.world_id == 0)
                .map(|a| (a.count, a.end_time)),
            Some((9, None))
        );
    }

    #[test]
    fn prune_drops_only_expired_rows() {
        let now = chrono::Utc::now().timestamp();
        let mut rows = vec![
            attr(KIND_AUCTION_POST_AUTHORITY, 0, Some(now - 1)),
            attr(2, 0, None),
        ];
        prune_expired(&mut rows, now);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind_id, 2);
    }
}
