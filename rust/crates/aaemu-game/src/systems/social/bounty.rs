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
//! * `SCUnitBountyMoney` is not sent: its writer has not been located in the 1.2 client (only
//!   the handler `391d6680`), so the layout is not confirmed (BOUNTY-3).

use std::collections::HashMap;
use std::sync::Mutex;

use sqlx::mysql::MySqlPool;
use sqlx::{MySql, Transaction};

use super::bounty_runtime::{BountyActor, BountyError, BountyLedger, BountyPledge, BountySnapshot};
use super::packets::trial::SCBountyPaidPacket;
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
}

/// The rows a save has to write, captured under the economy operation lock together with the
/// character wallets.
#[derive(Debug, Default)]
pub struct BountySaveBatch {
    pledges: BountySnapshot,
}

/// The bounty manager (`server.systems.social.bounties`).
#[derive(Debug, Default)]
pub struct BountyManager {
    state: Mutex<BountyState>,
    /// Captured by `capture_financial_snapshot` and written by `System::save`, so the pledge rows
    /// and the debited wallets describe the same instant.
    pending_save: Mutex<Option<BountySaveBatch>>,
}

impl BountyManager {
    /// `Load()`: every pledge, paid or not, in id order. A failed read fails startup.
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
        tracing::info!(
            "Loaded bounty pledges: {} ({} active)",
            state.ledger.pledges().count(),
            state.ledger.pledges().filter(|p| !p.paid).count(),
        );
        Ok(())
    }

    /// Captures the ledger for the next `System::save`, under the economy operation lock the
    /// caller already holds.
    pub(crate) fn prepare_save(&self) {
        let batch = BountySaveBatch {
            pledges: self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ledger
                .snapshot(),
        };
        *self.pending_save.lock().unwrap_or_else(|e| e.into_inner()) = Some(batch);
    }

    /// Writes the prepared rows (or a fresh snapshot when no capture ran), inside the caller's
    /// transaction. Returns the number of rows written.
    pub(crate) async fn save(&self, tx: &mut Transaction<'_, MySql>) -> Result<usize> {
        let batch = self
            .pending_save
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| BountySaveBatch {
                pledges: self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ledger
                    .snapshot(),
            });
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
        Ok(count)
    }

    /// The accumulated unpaid bounty on `target_character_id`.
    pub fn active_total(&self, target_character_id: u32) -> i64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ledger
            .active_total(target_character_id)
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
        if !inventory::subtract_money(
            &mut Out::Direct,
            payer,
            slot_type::INVENTORY,
            amount,
            task_type::SET_BOUNTY_MONEY,
        ) {
            return Err(PlacementError::NotEnoughMoney);
        }
        if let Err(error) = state.ledger.commit_placement(&plan) {
            // Cannot happen under the lock; refund rather than destroying the copper.
            drop(state);
            inventory::add_money(
                &mut Out::Direct,
                payer,
                slot_type::INVENTORY,
                amount,
                task_type::SET_BOUNTY_MONEY,
            );
            return Err(PlacementError::Ledger(error));
        }
        state.permits.remove(&payer.character_id);
        Ok(state.ledger.active_total(target_id))
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
        let _ops = server.systems.economy.ops_lock();
        let total = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match state.ledger.settle_arrest(target.character_id) {
                Ok(total) => total,
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
                &mut Out::Direct,
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
        total
    }
}
