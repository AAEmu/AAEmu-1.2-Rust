//! The 1.2 wanted-player bounty: placement and arrest payout.
//!
//! [`bounty_runtime`](super::bounty_runtime) holds the pure ledger; this manager is the server
//! glue around it: the placement permit, the wallet debit / credit, the MySQL rows and the
//! packets.
//!
//! Evidence and policy (details in `PORTING.md`, BOUNTY-1..BOUNTY-5):
//!
//! * Only an online, currently wanted character can be bountied. `SCSetBountyPermitted` answers
//!   the client's `allowed` flag (the client shows error 584 when it is false); a missing name
//!   is error 585 and every other refusal error 586, both through `SCSetBountyDone`
//!   (readers `397c6fc0` / `397c7080`).
//! * A placement is `> 0` copper and no more than the payer's purse; there is no fee.
//! * Pledges stack per target and never expire. They survive the target's or the payer's
//!   logout and a server restart (`bounty_pledges`).
//! * The arresting player receives the whole accumulated total when the arrest is decided, and
//!   the target's total resets. NPC, guard and mob kills never pay.
//! * `SCUnitBountyMoney` (`bc objId ; i64 moneyAmount`, writer `FUN_397b3b50`) carries the whole
//!   total, never a delta: to every viewer after a successful pledge, to a viewer whose client
//!   just received the target's entry `SCUnitState`, and 0 after the payout (BOUNTY-3).
//!   When the client draws it is INFERRED (`OnUnitBountyMoney`, handler `FUN_391d6680`).

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use sqlx::mysql::MySqlPool;
use sqlx::{MySql, Row, Transaction};

use super::bounty_runtime::{BountyActor, BountyError, BountyLedger, BountyPledge, BountySnapshot};
use super::crime::BountyDescription;
use super::packets::trial::{SCBountyPaidPacket, SCUnitBountyMoneyPacket, MAX_BOUNTY_LIST_ENTRIES};
use super::util;
use crate::db::int;
use crate::error::Result;
use crate::models::container::slot_type;
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::systems::economy::inventory::{self, Out};
use crate::systems::skills::buffs;
use crate::systems::skills::types::tags;
use crate::world::{PlayerRef, WorldObject};

/// How long a `RequestSetBountyMoney` permit stays usable, in milliseconds. Server-side policy:
/// the client opens its amount window from the reply and has no timeout of its own that we know
/// of; this only keeps the permit map bounded (BOUNTY-2).
pub const PERMIT_TTL_MS: i64 = 5 * 60_000;

/// `ErrorMessageType.CANNOT_SET_BOUNTY_FOR_SUCH_NAME` (585): no such online target.
pub const CANNOT_SET_BOUNTY_FOR_SUCH_NAME: i16 = 585;
/// `ErrorMessageType.CANNOT_SET_BOUNTY` (586): any other refusal.
pub const CANNOT_SET_BOUNTY: i16 = 586;

/// Sends `target`'s whole `total` as `SCUnitBountyMoney` to every viewer of it, and to the
/// target itself (`include_self`, like `GameObject.BroadcastPacket`). No object or manager lock
/// may be held: [`on_character_entered`] and [`BountyManager::place`] call this after dropping
/// theirs.
fn send_bounty_money(server: &GameServer, target: &PlayerRef, total: i64) {
    let obj = WorldObject::from(target.clone());
    let Some(world) = server.world.world_of(&obj) else {
        return;
    };
    crate::world::broadcast(
        &world,
        obj,
        &SCUnitBountyMoneyPacket {
            obj_id: target.obj_id,
            money_amount: total,
        },
        true,
    );
}

/// The `SCUnitBountyMoney` frames a character needs when it enters a world instance: its own
/// total to the players around it, and the total of every wanted player already around it to
/// the entering client. Called right after the entry `SCUnitState` exchange in
/// `world::session::spawn` and `instances::worlds::instance_loaded`. A total of 0 sends nothing.
pub fn on_character_entered(server: &GameServer, p: &PlayerRef) {
    let obj = WorldObject::from(p.clone());
    let Some(world) = server.world.world_of(&obj) else {
        return;
    };
    let own = server.systems.social.bounties.active_total(p.character_id);
    if own > 0 {
        crate::world::broadcast(
            &world,
            obj.clone(),
            &SCUnitBountyMoneyPacket {
                obj_id: p.obj_id,
                money_amount: own,
            },
            true,
        );
    }
    for other in world.players_around(&obj) {
        let total = server
            .systems
            .social
            .bounties
            .active_total(other.character_id);
        if total > 0 {
            p.send(SCUnitBountyMoneyPacket {
                obj_id: other.obj_id,
                money_amount: total,
            });
        }
    }
}

/// Why [`BountyManager::place`] refused a `SendBountyUpdate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementError {
    /// No such online character (it logged out after the permit, or the id is wrong).
    TargetOffline,
    /// The payer sent no matching `RequestSetBountyMoney`, or its permit expired / was used.
    NotPermitted,
    /// The target is the payer.
    SelfTarget,
    /// The target lost the wanted state after the request.
    NotWanted,
    /// The amount is not strictly positive.
    InvalidAmount,
    /// The payer's purse holds less than the amount.
    NotEnoughMoney,
    /// The ledger refused the transition (only a stale plan or an overflow can reach this).
    Ledger(BountyError),
}

impl PlacementError {
    /// The `SCSetBountyDone.errorMessage` the client shows.
    pub fn message_id(&self) -> i16 {
        match self {
            Self::TargetOffline => CANNOT_SET_BOUNTY_FOR_SUCH_NAME,
            Self::NotPermitted
            | Self::SelfTarget
            | Self::NotWanted
            | Self::InvalidAmount
            | Self::NotEnoughMoney
            | Self::Ledger(_) => CANNOT_SET_BOUNTY,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Permit {
    target_id: u32,
    expires_ms: i64,
}

#[derive(Debug, Default)]
struct BountyState {
    ledger: BountyLedger,
    /// One outstanding `RequestSetBountyMoney` per payer, replaced by the next request.
    permits: HashMap<u32, Permit>,
    /// Pledges whose current row is not in `bounty_pledges` yet (new or newly paid).
    dirty: BTreeSet<u64>,
}

/// The rows a save has to write, captured under the economy operation lock together with the
/// character wallets.
#[derive(Debug, Default)]
pub struct BountySaveBatch {
    pledges: BountySnapshot,
}

/// A built `CSNaviOpenBounty` answer: `total` is every character with an unpaid pledge and
/// `rows` the entries written (the caller serializes them, capped by the packet writer too).
#[derive(Debug, Default)]
pub struct BountyBoard {
    pub total: i32,
    pub rows: Vec<BountyDescription>,
}

/// One board row from a live character: `crime_point`, zone, position and `is_live = true` from
/// the unit, the counters from the character.
fn online_board_row(server: &GameServer, target_id: u32, amount: i64) -> Option<BountyDescription> {
    let player = server.world.character_by_id(target_id)?;
    let c = player.data();
    Some(BountyDescription {
        player_id: target_id,
        player_name: c.unit.name.clone(),
        player_level: c.unit.level,
        zone_id: c.unit.transform.zone_id,
        pos: (c.unit.transform.x, c.unit.transform.y, c.unit.transform.z),
        bounty_amount: u64::try_from(amount).unwrap_or(0),
        crime_points: c.crime_point,
        is_live: true,
        player_race: c.race,
        // The client turns this id into the left frame's faction name (`FUN_39080c00` field 9 ->
        // `FUN_392f8120(id, 1)`; the same call fills `"faction"` in `FUN_3907e830`).
        faction_id: c.unit.faction_id,
        arrest_count: c.arrest_count,
        accept_guilty_count: c.accept_guilty_count,
        accept_trial_count: c.accept_trial_count,
        guilty_count: c.guilty_count,
        not_guilty_count: c.not_guilty_count,
        // The column exists in the client row; no 1.2 meaning is known (no timer is invented).
        seconds: 0,
    })
}

/// One board row from the `characters` table (`is_live = false`). A deleted, missing or
/// unreadable row is skipped with a warning.
async fn offline_board_row(
    pool: &MySqlPool,
    target_id: u32,
    amount: i64,
) -> Option<BountyDescription> {
    let row = match sqlx::query(
        "SELECT `name`, CAST(`level` AS SIGNED) AS level, \
         CAST(`zone_id` AS SIGNED) AS zone_id, `x`, `y`, `z`, \
         CAST(`crime_point` AS SIGNED) AS crime_point, \
         CAST(`race` AS SIGNED) AS race, \
         CAST(`faction_id` AS SIGNED) AS faction_id, \
         CAST(`arrest_count` AS SIGNED) AS arrest_count, \
         CAST(`accept_guilty_count` AS SIGNED) AS accept_guilty_count, \
         CAST(`accept_trial_count` AS SIGNED) AS accept_trial_count, \
         CAST(`not_guilty_count` AS SIGNED) AS not_guilty_count, \
         CAST(`guilty_count` AS SIGNED) AS guilty_count \
         FROM `characters` WHERE `id` = ? AND `deleted` = 0",
    )
    .bind(target_id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                "Bounty board: characters row for target {target_id} unreadable: {error}"
            );
            return None;
        }
    };
    let count = |name: &str| -> i32 {
        row.try_get::<Option<i64>, _>(name)
            .ok()
            .flatten()
            .unwrap_or(0)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    Some(BountyDescription {
        player_id: target_id,
        player_name: row.try_get::<String, _>("name").ok()?,
        player_level: row.try_get::<i64, _>("level").ok()?.clamp(0, 255) as u8,
        zone_id: row
            .try_get::<i64, _>("zone_id")
            .ok()?
            .clamp(0, i64::from(u32::MAX)) as u32,
        pos: (
            row.try_get::<f32, _>("x").ok()?,
            row.try_get::<f32, _>("y").ok()?,
            row.try_get::<f32, _>("z").ok()?,
        ),
        bounty_amount: u64::try_from(amount).unwrap_or(0),
        crime_points: row
            .try_get::<i64, _>("crime_point")
            .ok()?
            .clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16,
        is_live: false,
        player_race: row.try_get::<i64, _>("race").ok()?.clamp(0, 255) as u8,
        faction_id: row
            .try_get::<i64, _>("faction_id")
            .ok()?
            .clamp(0, i64::from(u32::MAX)) as u32,
        arrest_count: count("arrest_count"),
        accept_guilty_count: count("accept_guilty_count"),
        accept_trial_count: count("accept_trial_count"),
        guilty_count: count("guilty_count"),
        not_guilty_count: count("not_guilty_count"),
        seconds: 0,
    })
}

/// The bounty manager (`server.systems.social.bounties`).
#[derive(Debug, Default)]
pub struct BountyManager {
    state: Mutex<BountyState>,
    /// Captured by `capture_financial_snapshot` and written by `System::save`, so the pledge rows
    /// and the debited wallets describe the same instant.
    pending_save: Mutex<Option<BountySaveBatch>>,
    /// The last written batch, applied to the dirty set only after the transaction commits.
    pending_commit: Mutex<Option<BountySaveBatch>>,
}

impl BountyManager {
    /// `Load()`: the pledges in id order. Paid rows are restored only to keep id allocation
    /// above them and are then dropped from memory. A failed read fails startup.
    pub async fn load(&self, pool: &MySqlPool) -> Result<()> {
        let rows = sqlx::query(
            "SELECT CAST(`id` AS SIGNED) AS id, CAST(`payer_id` AS SIGNED) AS payer_id, \
             CAST(`target_id` AS SIGNED) AS target_id, `amount`, `created`, \
             CAST(`paid` AS SIGNED) AS paid \
             FROM `bounty_pledges` ORDER BY `id`",
        )
        .fetch_all(pool)
        .await?;
        let mut pledges = Vec::with_capacity(rows.len());
        for row in &rows {
            let id = int(row, "id")?;
            let payer_id = int(row, "payer_id")?;
            let target_id = int(row, "target_id")?;
            let amount = int(row, "amount")?;
            let created = int(row, "created")?;
            let paid = int(row, "paid")? != 0;
            pledges.push(BountyPledge {
                id: u64::try_from(id)
                    .map_err(|_| crate::GameError::Protocol(format!("bounty id {id}")))?,
                payer: BountyActor {
                    character_id: u32::try_from(payer_id).map_err(|_| {
                        crate::GameError::Protocol(format!("bounty payer {payer_id}"))
                    })?,
                },
                target: BountyActor {
                    character_id: u32::try_from(target_id).map_err(|_| {
                        crate::GameError::Protocol(format!("bounty target {target_id}"))
                    })?,
                },
                amount,
                created,
                paid,
            });
        }
        let snapshot = BountySnapshot { pledges };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .ledger
            .restore(snapshot)
            .map_err(|e| crate::GameError::Protocol(format!("bounty_pledges row: {e}")))?;
        // Keep the restored `next_pledge_id` above paid ids, then drop the paid history from
        // memory.
        let paid: Vec<u64> = state
            .ledger
            .pledges()
            .filter(|p| p.paid)
            .map(|p| p.id)
            .collect();
        for id in paid {
            state.ledger.remove_pledge(id);
        }
        tracing::info!(
            "Loaded bounty pledges: {} active",
            state.ledger.pledges().filter(|p| !p.paid).count(),
        );
        Ok(())
    }

    /// The dirty rows as a save batch.
    fn dirty_batch(&self) -> BountySaveBatch {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        BountySaveBatch {
            pledges: BountySnapshot {
                pledges: state
                    .dirty
                    .iter()
                    .filter_map(|id| state.ledger.pledge(*id).cloned())
                    .collect(),
            },
        }
    }

    /// Captures the dirty rows for the next `System::save`, under the economy operation lock the
    /// caller already holds.
    pub(crate) fn prepare_save(&self) {
        let batch = self.dirty_batch();
        *self.pending_save.lock().unwrap_or_else(|e| e.into_inner()) = Some(batch);
    }

    /// Writes the prepared dirty rows (or the current dirty set when no capture ran), inside the
    /// caller's transaction. Returns the number of rows written.
    ///
    /// The dirty set is cleared only by [`Self::commit_save`] after the transaction commits, so a
    /// rolled-back save is retried on the next one.
    pub(crate) async fn save(&self, tx: &mut Transaction<'_, MySql>) -> Result<usize> {
        *self
            .pending_commit
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        let batch = self
            .pending_save
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| self.dirty_batch());
        let count = batch.pledges.pledges.len();
        for pledge in &batch.pledges.pledges {
            sqlx::query(
                "INSERT INTO `bounty_pledges` \
                 (`id`, `payer_id`, `target_id`, `amount`, `created`, `paid`) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE `paid` = VALUES(`paid`)",
            )
            .bind(pledge.id)
            .bind(pledge.payer.character_id)
            .bind(pledge.target.character_id)
            .bind(pledge.amount)
            .bind(pledge.created)
            .bind(i64::from(pledge.paid))
            .execute(&mut **tx)
            .await?;
        }
        *self
            .pending_commit
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(batch);
        Ok(count)
    }

    /// Applies a committed save: clears the dirty rows it wrote and drops paid rows from memory.
    /// A row that changed after the capture stays dirty for the next save.
    pub(crate) fn commit_save(&self) {
        let Some(batch) = self
            .pending_commit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for written in &batch.pledges.pledges {
            let current = state.ledger.pledge(written.id).cloned();
            if current.as_ref() != Some(written) {
                continue;
            }
            state.dirty.remove(&written.id);
            if written.paid {
                state.ledger.remove_pledge(written.id);
            }
        }
    }

    /// The number of pledge rows still held in memory (tests).
    #[cfg(test)]
    pub(crate) fn tracked_pledge_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ledger
            .pledges()
            .count()
    }

    /// The accumulated unpaid bounty on `target_character_id`.
    pub fn active_total(&self, target_character_id: u32) -> i64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ledger
            .active_total(target_character_id)
    }

    /// Every character with an unpaid pledge: `(character id, total)`, highest total first and
    /// ties by character id. The order is a server policy (the client stores the rows in a map
    /// keyed by id, so it does not depend on it); it only decides which rows the 20-entry cap
    /// keeps.
    pub fn active_targets(&self) -> Vec<(u32, i64)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut totals: HashMap<u32, i64> = HashMap::new();
        for pledge in state.ledger.pledges().filter(|p| !p.paid) {
            let target = pledge.target.character_id;
            if target != 0 {
                let total = totals.entry(target).or_default();
                *total = total.saturating_add(pledge.amount);
            }
        }
        let mut targets: Vec<(u32, i64)> =
            totals.into_iter().filter(|(_, total)| *total > 0).collect();
        targets.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        targets
    }

    /// The `SCBountyList` payload for `CSNaviOpenBounty`: every character with an unpaid pledge
    /// as `total`, and up to [`MAX_BOUNTY_LIST_ENTRIES`] rows (the client's cap).
    ///
    /// The board is only filled with `Social.BountyBoard = true`; the default keeps the empty
    /// list the C# server sent because the 1.2 client's `usertrial/bounty_bullet.alb` itself
    /// passes numeric counters to `LocalizeUiText` and indexes an undefined global `listCtrl`
    /// (see BOUNTY-board and the 2026-11-06 real-client log), so no row value can render it.
    /// Online characters are read from the live unit; an offline target's name, level, race,
    /// faction, zone and position come from its `characters` row. A missing or unreadable row is
    /// skipped instead of failing the whole board.
    pub async fn board(&self, server: &GameServer) -> BountyBoard {
        if !server.config.social.bounty_board {
            return BountyBoard::default();
        }
        let targets = self.active_targets();
        let mut total = i32::try_from(targets.len()).unwrap_or(i32::MAX);
        let mut rows = Vec::with_capacity(targets.len().min(MAX_BOUNTY_LIST_ENTRIES));
        for (index, (target_id, total_amount)) in targets.into_iter().enumerate() {
            if index >= MAX_BOUNTY_LIST_ENTRIES {
                break;
            }
            if let Some(row) = online_board_row(server, target_id, total_amount) {
                rows.push(row);
                continue;
            }
            if let Some(row) = offline_board_row(&server.db, target_id, total_amount).await {
                rows.push(row);
            } else {
                // A deleted or unreadable character row is skipped; keep the total equal to
                // what the server can actually resolve so the client can complete its map.
                total -= 1;
            }
        }
        BountyBoard { total, rows }
    }

    /// Remembers that `payer_id` may place a bounty on `target_id` (replaces an older request
    /// and prunes expired ones).
    pub fn permit(&self, payer_id: u32, target_id: u32) {
        let now = util::now_ms();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.permits.retain(|_, p| p.expires_ms > now);
        state.permits.insert(
            payer_id,
            Permit {
                target_id,
                expires_ms: now + PERMIT_TTL_MS,
            },
        );
    }

    /// Whether a live permit for `(payer_id, target_id)` exists.
    pub fn has_permit(&self, payer_id: u32, target_id: u32) -> bool {
        let now = util::now_ms();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.permits.retain(|_, p| p.expires_ms > now);
        state
            .permits
            .get(&payer_id)
            .is_some_and(|p| p.target_id == target_id)
    }

    /// Debits `amount` copper from `payer` and records the pledge, answering the target's new
    /// total. The permit is consumed only when the placement succeeds.
    ///
    /// The permit check, the funds check, the debit and the ledger commit run under the economy
    /// operation lock, so a concurrent wallet change cannot pass the funds check and a stale
    /// plan cannot overwrite another pledge.
    pub fn place(
        &self,
        server: &GameServer,
        payer: &PlayerRef,
        target_id: u32,
        amount: i32,
    ) -> std::result::Result<i64, PlacementError> {
        if amount <= 0 {
            return Err(PlacementError::InvalidAmount);
        }
        if payer.character_id == target_id {
            return Err(PlacementError::SelfTarget);
        }
        let _ops = server.systems.economy.ops_lock();
        let Some(target) = server.world.character_by_id(target_id) else {
            return Err(PlacementError::TargetOffline);
        };
        if !buffs::check_buff_tag(server, &WorldObject::from(target.clone()), tags::WANTED) {
            return Err(PlacementError::NotWanted);
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = util::now_ms();
        if !state
            .permits
            .get(&payer.character_id)
            .is_some_and(|p| p.target_id == target_id && p.expires_ms > now_ms)
        {
            return Err(PlacementError::NotPermitted);
        }
        let plan = state
            .ledger
            .prepare_placement(
                BountyActor {
                    character_id: payer.character_id,
                },
                BountyActor {
                    character_id: target_id,
                },
                i64::from(amount),
                true,
                util::now_secs(),
            )
            .map_err(PlacementError::Ledger)?;
        if payer.data().money < i64::from(amount) {
            return Err(PlacementError::NotEnoughMoney);
        }
        // `change_money` checks the purse and debits it under the same character lock. A plain
        // `subtract_money` reads from `None`, which never checks funds, so a concurrent wallet
        // change could overdraw the payer.
        if !inventory::change_money(
            &mut Out::Direct(server),
            payer,
            slot_type::INVENTORY,
            slot_type::NONE,
            amount,
            task_type::SET_BOUNTY_MONEY,
        ) {
            return Err(PlacementError::NotEnoughMoney);
        }
        let pledge = match state.ledger.commit_placement(&plan) {
            Ok(pledge) => pledge,
            Err(error) => {
                // Cannot happen under the lock; refund rather than destroying the copper.
                drop(state);
                inventory::add_money(
                    &mut Out::Direct(server),
                    payer,
                    slot_type::INVENTORY,
                    amount,
                    task_type::SET_BOUNTY_MONEY,
                );
                return Err(PlacementError::Ledger(error));
            }
        };
        state.permits.remove(&payer.character_id);
        state.dirty.insert(pledge.id);
        let total = state.ledger.active_total(target_id);
        // Never send into the world while the ledger lock (or the economy ops lock) is held.
        drop(state);
        drop(_ops);
        send_bounty_money(server, &target, total);
        Ok(total)
    }

    /// The arrest of `target`: pays the whole unpaid total to `arrestor` and resets the target's
    /// total. Nothing happens for a self kill or when there is no pledge, and a repeated call
    /// pays nothing.
    pub fn settle_arrest(
        &self,
        server: &GameServer,
        target: &PlayerRef,
        arrestor: &PlayerRef,
    ) -> i64 {
        if target.character_id == arrestor.character_id {
            return 0;
        }
        // The killer can have left between the death and the tail; then there is nobody to pay,
        // so keep the pledges instead of crediting an offline character.
        if server
            .world
            .character_by_id(arrestor.character_id)
            .is_none()
        {
            tracing::debug!(
                "Bounty settlement of {} skipped: arrester {} is no longer online",
                target.character_id,
                arrestor.character_id
            );
            return 0;
        }
        if crate::systems::economy::System::ops_held_on_this_thread() {
            // The ops lock below is not re-entrant; waiting for it on this thread would hang the
            // death path and every later economy operation.
            tracing::error!(
                "Bounty settlement of {} skipped: the economy ops lock is held on this thread",
                target.character_id
            );
            return 0;
        }
        let _ops = server.systems.economy.ops_lock();
        let total = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<u64> = state
                .ledger
                .pledges()
                .filter(|p| !p.paid && p.target.character_id == target.character_id)
                .map(|p| p.id)
                .collect();
            match state.ledger.settle_arrest(target.character_id) {
                Ok(total) => {
                    for id in ids {
                        state.dirty.insert(id);
                    }
                    total
                }
                Err(error) => {
                    tracing::error!(
                        "Bounty settlement of {} failed: {error}",
                        target.character_id
                    );
                    return 0;
                }
            }
        };
        if total <= 0 {
            return 0;
        }
        // `add_money` carries an i32 item task; pay a total above that in chunks rather than
        // losing the excess.
        let mut remaining = total;
        while remaining > 0 {
            let chunk = remaining.min(i64::from(i32::MAX)) as i32;
            if !inventory::add_money(
                &mut Out::Direct(server),
                arrestor,
                slot_type::INVENTORY,
                chunk,
                task_type::PAY_BOUNTY_MONEY,
            ) {
                tracing::error!(
                    "Bounty payout of {total} copper to {} failed after {} credited",
                    arrestor.character_id,
                    total - remaining
                );
                break;
            }
            remaining -= i64::from(chunk);
        }
        let hunter = arrestor.data().unit.name.clone();
        let arrested = target.data().unit.name.clone();
        arrestor.send(SCBountyPaidPacket {
            hunter,
            arrested,
            money_amount: total,
        });
        tracing::info!(
            "Bounty paid: {} arrested {} for {total} copper",
            arrestor.character_id,
            target.character_id
        );
        // The total reset to 0: every viewer clears the amount it draws (whole value, not a
        // delta). Drop the economy ops lock before the world send.
        drop(_ops);
        send_bounty_money(server, target, 0);
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managers::save::{
        capture_financial_snapshot, commit_financial_snapshot, write_financial_snapshot,
    };
    use crate::test_support::test_server;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    async fn temp_pool() -> Option<MySqlPool> {
        let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
            return None;
        };
        let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
        assert_eq!(
            options.get_port(),
            3317,
            "only private MySQL3317 is allowed"
        );
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("private MySQL");
        let ddl = include_str!("../../../../../../SQL/updates/2026-11-02_aaemu_game_bounty.sql")
            .lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n")
            .replace("CREATE TABLE IF NOT EXISTS", "CREATE TEMPORARY TABLE");
        for statement in ddl.split(';').filter(|s| !s.trim().is_empty()) {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        Some(pool)
    }

    fn insert_dirty(server: &GameServer, payer: u32, target: u32, amount: i64) -> u64 {
        let mut state = server
            .systems
            .social
            .bounties
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let plan = state
            .ledger
            .prepare_placement(
                BountyActor {
                    character_id: payer,
                },
                BountyActor {
                    character_id: target,
                },
                amount,
                true,
                1,
            )
            .unwrap();
        let pledge = state.ledger.commit_placement(&plan).unwrap();
        state.dirty.insert(pledge.id);
        pledge.id
    }

    fn settle_dirty(server: &GameServer, target: u32) -> i64 {
        let mut state = server
            .systems
            .social
            .bounties
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ids: Vec<u64> = state
            .ledger
            .pledges()
            .filter(|p| !p.paid && p.target.character_id == target)
            .map(|p| p.id)
            .collect();
        let total = state.ledger.settle_arrest(target).unwrap();
        for id in ids {
            state.dirty.insert(id);
        }
        total
    }

    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only a temporary table"]
    async fn a_second_save_without_changes_writes_nothing() {
        let Some(pool) = temp_pool().await else {
            return;
        };
        let server = test_server();
        insert_dirty(&server, 1, 2, 100);

        server.systems.social.bounties.prepare_save();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            server.systems.social.bounties.save(&mut tx).await.unwrap(),
            1
        );
        tx.commit().await.unwrap();
        server.systems.social.bounties.commit_save();
        assert_eq!(server.systems.social.bounties.tracked_pledge_count(), 1);

        server.systems.social.bounties.prepare_save();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            server.systems.social.bounties.save(&mut tx).await.unwrap(),
            0
        );
        tx.commit().await.unwrap();
        server.systems.social.bounties.commit_save();
    }

    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only a temporary table"]
    async fn a_paid_pledge_is_written_once_then_dropped_from_memory() {
        let Some(pool) = temp_pool().await else {
            return;
        };
        let server = test_server();
        insert_dirty(&server, 1, 2, 100);
        server.systems.social.bounties.prepare_save();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            server.systems.social.bounties.save(&mut tx).await.unwrap(),
            1
        );
        tx.commit().await.unwrap();
        server.systems.social.bounties.commit_save();
        assert_eq!(settle_dirty(&server, 2), 100);

        server.systems.social.bounties.prepare_save();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            server.systems.social.bounties.save(&mut tx).await.unwrap(),
            1
        );
        tx.commit().await.unwrap();
        server.systems.social.bounties.commit_save();
        assert_eq!(server.systems.social.bounties.tracked_pledge_count(), 0);

        let paid: i64 = sqlx::query_scalar(
            "SELECT CAST(`paid` AS SIGNED) FROM `bounty_pledges` WHERE `id` = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(paid, 1);
        server.systems.social.bounties.prepare_save();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            server.systems.social.bounties.save(&mut tx).await.unwrap(),
            0
        );
        tx.commit().await.unwrap();
        server.systems.social.bounties.commit_save();

        let restored = BountyManager::default();
        restored.load(&pool).await.unwrap();
        assert_eq!(restored.tracked_pledge_count(), 0);
        assert_eq!(restored.active_total(2), 0);
    }

    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only a temporary table"]
    async fn a_rolled_back_save_is_retried() {
        let Some(pool) = temp_pool().await else {
            return;
        };
        let server = test_server();
        insert_dirty(&server, 1, 2, 100);

        server.systems.social.bounties.prepare_save();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            server.systems.social.bounties.save(&mut tx).await.unwrap(),
            1
        );
        tx.rollback().await.unwrap();

        server.systems.social.bounties.prepare_save();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            server.systems.social.bounties.save(&mut tx).await.unwrap(),
            1
        );
        tx.commit().await.unwrap();
        server.systems.social.bounties.commit_save();

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM `bounty_pledges`")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only a temporary table"]
    async fn the_financial_snapshot_writes_unpaid_and_paid_rows() {
        let Some(pool) = temp_pool().await else {
            return;
        };
        let server = test_server();
        insert_dirty(&server, 1, 2, 100);
        insert_dirty(&server, 3, 4, 250);
        assert_eq!(settle_dirty(&server, 4), 250);

        let snapshot = capture_financial_snapshot(&server);
        let mut tx = pool.begin().await.unwrap();
        let receipt = write_financial_snapshot(&server, &mut tx, snapshot)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        commit_financial_snapshot(&server, receipt);

        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT CAST(`target_id` AS SIGNED), CAST(`paid` AS SIGNED) \
             FROM `bounty_pledges` ORDER BY `target_id`",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows, vec![(2, 0), (4, 1)]);
        assert_eq!(server.systems.social.bounties.tracked_pledge_count(), 1);

        let restored = BountyManager::default();
        restored.load(&pool).await.unwrap();
        assert_eq!(restored.active_total(2), 100);
        assert_eq!(restored.active_total(4), 0);
        assert_eq!(restored.tracked_pledge_count(), 1);
        // The paid id 1 is still reserved, so the next pledge gets id 3, not 1.
        {
            let mut state = restored.state.lock().unwrap();
            let plan = state
                .ledger
                .prepare_placement(
                    BountyActor { character_id: 5 },
                    BountyActor { character_id: 6 },
                    50,
                    true,
                    2,
                )
                .unwrap();
            assert_eq!(state.ledger.commit_placement(&plan).unwrap().id, 3);
        }
    }
}
