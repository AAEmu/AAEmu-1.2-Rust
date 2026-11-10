//! Per-account shop purchase limits: `items.limited_sale_count` / `items.one_time_sale`.
//!
//! The 1.2 client loads both columns with the item template (`SELECT id, ..., limited_sale_count,
//! ..., one_time_sale, ... FROM items`), but it never reads them: the NPC store UI has no
//! remaining-count field and the only sale-limit error in its table is `store_one_time_sale`
//! (752, `0x2f0`). The original server therefore enforces the limit and only reports the error.
//! Its GM command list on the same client build carries `limited_sale_add`, `limited_sale_reset`,
//! `one_time_sale_add` and `one_time_sale_reset`, so the counters are reset by an administrator,
//! not automatically.
//!
//! Scope: the counter is per **account** (`characters.account_id`), it never expires, and it is
//! not shared with the server. The 1.2 item descriptions say so: the Solar / Lunar Scarecrow
//! Farm designs (30013 / 30014) and the Solar Scarecrow Farm Chest (30560) carry "Caution: Can be
//! purchased only once per account and can't be traded." (`localized_texts` rows for
//! `items.description` with `idx` 30013 / 30014 / 30560). `one_time_sale` is the same as a limit
//! of one; when both columns are set the smaller limit wins. Buying the last allowed copy is
//! allowed; a request that would cross the limit is refused whole. Selling an item back or to
//! another vendor does not change the counter, and giving or looting the template is not a
//! purchase and is not limited.
//!
//! Rows written before 2026-11-10 were keyed by character; the update script
//! `2026-11-10_aaemu_game_account_shop_purchase_limits.sql` folds them into one row per account
//! with the **maximum** per-character count (see that file for why).

use std::collections::HashMap;
use std::sync::Mutex;

use sqlx::{MySql, Row, Transaction};

use crate::data::items::ItemTemplate;
use crate::server::GameServer;

/// One `(item template, quantity)` a purchase wants to take from an account's allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShopClaim {
    pub item_id: u32,
    pub count: i32,
    /// `ItemTemplate::purchase_limit()` of `item_id` (> 0).
    pub limit: i32,
}

/// An `account_shop_purchases` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShopPurchaseRow {
    pub account_id: u32,
    pub item_id: u32,
    pub purchase_count: i32,
}

/// The allowance cache (`server.systems.economy.shop_limits`).
#[derive(Debug, Default)]
pub struct ShopLimits {
    /// `(account id, item template id)` -> copies bought so far.
    counts: Mutex<HashMap<(u32, u32), i32>>,
}

impl ShopLimits {
    /// Copies of `item_id` `account_id` has bought (0 when none).
    pub fn purchased(&self, account_id: u32, item_id: u32) -> i32 {
        self.counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(account_id, item_id))
            .copied()
            .unwrap_or(0)
    }

    /// Checks every claim and adds all of them, atomically under the counts lock. Returns false
    /// and changes nothing when any claim would cross its limit. Claims for a template without a
    /// limit are not passed here; a non-positive count is refused (nothing to sell).
    ///
    /// Callers hold the economy operation lock (`ItemTxn`) so the claim describes the same
    /// operation as the item / wallet changes around it.
    pub fn try_claim(&self, account_id: u32, claims: &[ShopClaim]) -> bool {
        if claims.iter().any(|c| c.count <= 0 || c.limit <= 0) {
            return false;
        }
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        for claim in claims {
            let current = counts
                .get(&(account_id, claim.item_id))
                .copied()
                .unwrap_or(0);
            let Some(total) = current.checked_add(claim.count) else {
                return false;
            };
            if total > claim.limit {
                return false;
            }
        }
        for claim in claims {
            *counts.entry((account_id, claim.item_id)).or_insert(0) += claim.count;
        }
        true
    }

    /// Gives back a claim whose purchase failed after it was taken (debit first, refund on
    /// failure). A missing claim is ignored, as is one for an unlimited template.
    pub fn release(&self, account_id: u32, claims: &[ShopClaim]) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        for claim in claims {
            if claim.count <= 0 {
                continue;
            }
            let key = (account_id, claim.item_id);
            let Some(current) = counts.get(&key).copied() else {
                continue;
            };
            let left = current.saturating_sub(claim.count);
            if left > 0 {
                counts.insert(key, left);
            } else {
                counts.remove(&key);
            }
        }
    }

    /// The rows to write in the save transaction (counts above zero, sorted for determinism).
    pub fn prepare_save(&self) -> Vec<ShopPurchaseRow> {
        let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<ShopPurchaseRow> = counts
            .iter()
            .filter(|(_, &count)| count > 0)
            .map(
                |(&(account_id, item_id), &purchase_count)| ShopPurchaseRow {
                    account_id,
                    item_id,
                    purchase_count,
                },
            )
            .collect();
        rows.sort_unstable_by_key(|r| (r.account_id, r.item_id));
        rows
    }

    /// Replaces the cache (a fresh server after [`load`], or a test).
    pub fn replace_all(&self, rows: Vec<ShopPurchaseRow>) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts.clear();
        for row in rows {
            if row.purchase_count > 0 {
                counts.insert((row.account_id, row.item_id), row.purchase_count);
            }
        }
    }

    /// The claims for one purchase request: one entry per distinct template, summing all lines
    /// that ask for it, with the template's effective limit. Only limited templates produce a
    /// claim.
    pub fn claims_for(
        templates: impl IntoIterator<Item = (u32, i32, Option<i32>)>,
    ) -> Vec<ShopClaim> {
        let mut by_item: HashMap<u32, ShopClaim> = HashMap::new();
        for (item_id, count, limit) in templates {
            let Some(limit) = limit else { continue };
            let claim = by_item.entry(item_id).or_insert(ShopClaim {
                item_id,
                count: 0,
                limit,
            });
            claim.count = claim.count.saturating_add(count);
        }
        let mut claims: Vec<ShopClaim> = by_item.into_values().collect();
        claims.sort_unstable_by_key(|c| c.item_id);
        claims
    }
}

/// `ItemTemplate::purchase_limit()` for `item_id`, or `None` when the template is unknown or
/// unlimited.
pub fn purchase_limit(server: &GameServer, item_id: u32) -> Option<i32> {
    server
        .game_data
        .items
        .template(item_id)
        .and_then(ItemTemplate::purchase_limit)
}

/// Loads every `account_shop_purchases` row at startup (`System::start`).
pub async fn load(server: &GameServer) -> crate::Result<()> {
    let rows = sqlx::query(
        "SELECT `account_id`, `item_id`, `purchase_count` FROM `account_shop_purchases`",
    )
    .fetch_all(&server.db)
    .await?;
    let mut loaded = Vec::with_capacity(rows.len());
    for row in rows {
        let account_id: u32 = row.try_get::<u32, _>("account_id")?;
        let item_id: u32 = row.try_get::<u32, _>("item_id")?;
        let purchase_count: i32 = row.try_get::<i32, _>("purchase_count")?;
        if purchase_count > 0 {
            loaded.push(ShopPurchaseRow {
                account_id,
                item_id,
                purchase_count,
            });
        }
    }
    server.systems.economy.shop_limits.replace_all(loaded);
    Ok(())
}

/// Writes the snapshot's rows inside the save transaction (the same transaction as the items
/// and the character/wallet row). Counts only grow; a save rewrites the row of every account
/// that has ever bought a limited template.
pub async fn write_prepared(
    tx: &mut Transaction<'_, MySql>,
    rows: Vec<ShopPurchaseRow>,
) -> crate::Result<usize> {
    let mut written = 0;
    for row in rows {
        sqlx::query(
            "INSERT INTO `account_shop_purchases` (`account_id`, `item_id`, `purchase_count`) \
             VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE `purchase_count` = VALUES(`purchase_count`)",
        )
        .bind(row.account_id)
        .bind(row.item_id)
        .bind(row.purchase_count)
        .execute(&mut **tx)
        .await?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(item_id: u32, count: i32, limit: i32) -> ShopClaim {
        ShopClaim {
            item_id,
            count,
            limit,
        }
    }

    #[test]
    fn claims_are_per_account_and_sum_duplicate_lines() {
        let limits = ShopLimits::default();
        assert_eq!(limits.purchased(1, 10), 0);
        assert!(limits.try_claim(1, &[claim(10, 1, 3), claim(20, 2, 2)]));
        assert!(limits.try_claim(1, &[claim(10, 1, 3)]));
        assert_eq!(limits.purchased(1, 10), 2);
        assert_eq!(limits.purchased(2, 10), 0);
        assert!(!limits.try_claim(1, &[claim(10, 2, 3)]));
        assert_eq!(limits.purchased(1, 10), 2);

        let merged = ShopLimits::claims_for([(10, 1, Some(3)), (10, 1, Some(3)), (30, 1, None)]);
        assert_eq!(merged, vec![claim(10, 2, 3)]);
    }

    #[test]
    fn a_refused_claim_changes_nothing_and_release_undoes_a_claim() {
        let limits = ShopLimits::default();
        assert!(!limits.try_claim(1, &[claim(10, 1, 2), claim(20, 3, 2)]));
        assert_eq!((limits.purchased(1, 10), limits.purchased(1, 20)), (0, 0));

        assert!(limits.try_claim(1, &[claim(10, 1, 2)]));
        assert_eq!(limits.prepare_save().len(), 1);
        limits.release(1, &[claim(10, 1, 2)]);
        assert_eq!(limits.purchased(1, 10), 0);
        assert!(limits.prepare_save().is_empty());
    }

    #[test]
    fn one_time_sale_and_limited_sale_count_limits() {
        let mut template = ItemTemplate::plain(1, 10);
        assert_eq!(template.purchase_limit(), None);
        template.limited_sale_count = 3;
        assert_eq!(template.purchase_limit(), Some(3));
        template.one_time_sale = true;
        assert_eq!(template.purchase_limit(), Some(1));
        template.limited_sale_count = 0;
        assert_eq!(template.purchase_limit(), Some(1));
    }

    #[test]
    fn save_rows_round_trip_from_the_cache() {
        let limits = ShopLimits::default();
        assert!(limits.try_claim(2, &[claim(20, 2, 5)]));
        assert!(limits.try_claim(1, &[claim(10, 1, 5)]));
        let rows = limits.prepare_save();
        assert_eq!(
            rows,
            vec![
                ShopPurchaseRow {
                    account_id: 1,
                    item_id: 10,
                    purchase_count: 1
                },
                ShopPurchaseRow {
                    account_id: 2,
                    item_id: 20,
                    purchase_count: 2
                },
            ]
        );

        let reloaded = ShopLimits::default();
        reloaded.replace_all(rows);
        assert_eq!(reloaded.purchased(1, 10), 1);
        assert_eq!(reloaded.purchased(2, 20), 2);
    }
}
