//! `Core/Managers/SaveManager`: the periodic save (`AutoSaveInterval` minutes) of items and
//! online characters in one transaction, and the final save on shutdown. The subsystems' rows
//! (houses, mails, auction, crimes, slaves) are written by `systems::save_stage` at the C#
//! positions of the transaction.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;

use crate::db_character::save_character;
use crate::models::character::Character;
use crate::server::GameServer;
use crate::systems::{save_stage, SaveStage};
use crate::world::PlayerRef;

pub(crate) struct FinancialSnapshot {
    pub(crate) houses: crate::systems::housing::manager::HousingSaveBatch,
    pub(crate) dominions: crate::systems::housing::dominion::state::SaveBatch,
    pub(crate) mails: crate::systems::economy::mail::MailSaveBatch,
    pub(crate) auctions: crate::systems::economy::auction::AuctionSaveBatch,
    pub(crate) items: super::item::ItemSaveBatch,
    pub(crate) shop_purchases: Vec<crate::systems::economy::shop_limits::ShopPurchaseRow>,
    pub(crate) schedule_items:
        crate::systems::npcs::schedule_item_persistence::PreparedScheduleSave,
    pub(crate) characters: Vec<(PlayerRef, Character)>,
}

/// Which characters a snapshot writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotScope {
    /// Autosave / shutdown / tests: every online and departing character.
    Everyone,
    /// A logout: the pending departures plus the other online characters whose wallet
    /// (`money`, `money2`) differs from what the database holds, or that carry unflushed labor.
    /// A clean character has nothing to commit with the property state, so a logout wave is
    /// O(departures) instead of O(online) per logout.
    Departure,
}

/// Capture property transactions before any database await. Claim/capture, tax payments,
/// certificates, receipts and wallets must describe the same logical operation boundary.
pub(crate) fn capture_financial_snapshot(server: &GameServer) -> FinancialSnapshot {
    capture_financial_snapshot_for(server, SnapshotScope::Everyone)
}

pub(crate) fn capture_financial_snapshot_for(
    server: &GameServer,
    scope: SnapshotScope,
) -> FinancialSnapshot {
    let _items = server.systems.economy.operation_scope();
    let _auction = server.systems.economy.auction.operation_lock();
    let _housing = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    let houses = crate::systems::housing::manager::prepare_save(server);
    let dominions = server.systems.housing.dominions.prepare_save();
    let _economy = server.systems.economy.ops_lock();
    // Captured with the wallets: `System::save(AfterCharacters)` writes these rows in the same
    // transaction, so a pledge and its debit commit or roll back together.
    server.systems.social.bounties.prepare_save();
    let mut players = server.world.all_characters();
    if scope == SnapshotScope::Departure {
        // Money moves between characters (trade, mail, auction, houses, treasury): a wallet
        // that differs from the committed one belongs to the same transaction as the property
        // rows above. Reads one data lock at a time, then the committed-money map.
        players.retain(|p| server.save.is_financially_dirty(p));
    }
    for player in server
        .save
        .departing
        .lock()
        .expect("departure saves poisoned")
        .values()
    {
        if !players
            .iter()
            .any(|live| live.character_id == player.character_id)
        {
            players.push(player.clone());
        }
    }
    order_snapshot_players(&mut players);
    FinancialSnapshot {
        houses,
        dominions,
        mails: server.systems.economy.mail.prepare_save(),
        auctions: server.systems.economy.auction.prepare_save(server),
        items: server.items.prepare_save(),
        // Captured under `ops_lock` with the wallet / item snapshot it belongs to.
        shop_purchases: server.systems.economy.shop_limits.prepare_save(),
        // Captured under the same `ops_lock` as the granted items / mails of a take.
        schedule_items: server.systems.npcs.schedule_items.prepare_save(),
        characters: players
            .into_iter()
            .map(|p| {
                // Resolved before locking the character: `world_pos_rot` locks transforms.
                let world = save_world_position(server, &p);
                let mut character = p.data().clone();
                if let Some(world) = world {
                    let t = &mut character.unit.transform;
                    t.set_position(
                        world.x,
                        world.y,
                        world.z,
                        world.rot_x,
                        world.rot_y,
                        world.rot_z,
                    );
                    t.parent_obj_id = 0;
                }
                (p, character)
            })
            .collect(),
    }
}

/// Orders the snapshot's characters by `(account_id, character_id)`.
///
/// `save_character` flushes `labor_power_unsaved` into `accounts.labor`, so a save transaction
/// takes the `accounts` row locks of its characters in this order. The 5 s online tracking
/// writes `accounts.divine_clock_time` for a chunk with `WHERE account_id IN (...)`, which
/// InnoDB locks in primary-key order. Two different orders deadlock (MySQL 1213; the tracking
/// statement was rolled back three times during the 7 h cloud bot run of 2026-11-11), so both
/// writers acquire the account rows ascending.
fn order_snapshot_players(players: &mut [PlayerRef]) {
    players.sort_by_cached_key(|p| {
        let c = p.data();
        (c.account_id, c.id)
    });
}

/// The world position of a character attached to a parent (ship, mate, transfer): its
/// transform is then relative to the parent, but the row must hold world coordinates
/// (`Character.Save` writes `Transform.World.Position` / `Rotation`). `None` when the
/// character has no parent, or the parent is gone (the local values are kept, with a warning).
fn save_world_position(
    server: &GameServer,
    player: &PlayerRef,
) -> Option<crate::models::transform::PositionAndRotation> {
    let obj = crate::world::WorldObject::from(player);
    let parent_id = obj.transform().parent_obj_id;
    if parent_id == 0 {
        return None;
    }
    let Some(world) = server.world.world_of(&obj) else {
        tracing::warn!(
            "Saving character {}: no world instance for its parent {parent_id}; \
             saving the parent-relative position",
            player.character_id
        );
        return None;
    };
    if world.object(parent_id).is_none() {
        tracing::warn!(
            "Saving character {}: parent object {parent_id} not found; \
             saving the parent-relative position",
            player.character_id
        );
        return None;
    }
    Some(world.world_pos_rot(&obj))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::slave::Slave;
    use crate::test_support::{spawn_player, test_character, test_server, TestClient};
    use crate::world::{Entity, WorldObject};

    /// The 5 s tracking only queues the seconds; the next save writes them, so the statement
    /// never contends with the save's account rows. An account with several characters adds up.
    #[test]
    fn divine_clock_seconds_accumulate_per_account_until_a_save() {
        let s = SaveManager::default();
        s.add_divine_clock(2, 5);
        s.add_divine_clock(1, 5);
        s.add_divine_clock(2, 3);
        s.add_divine_clock(3, 0);
        assert_eq!(
            s.pending_divine_clock(),
            BTreeMap::from([(1, 5), (2, 8)]),
            "zero samples and a repeated account are folded"
        );
    }

    #[tokio::test]
    async fn snapshot_saves_world_position_of_attached_character() {
        let s = test_server();
        let client = TestClient::new(&s, 1, 1);
        let player = spawn_player(
            &s,
            &client,
            test_character(1, "Deck", 1003.0, 1002.0),
            0x100,
        );
        let world = s.world.world(0).unwrap();
        let obj = WorldObject::from(&player);
        let mut hull = Slave::new(1, 1);
        hull.unit
            .transform
            .set_position(1000.0, 2000.0, 305.0, 0.0, 0.0, 0.0);
        let slave = Entity::new(0x300, hull);
        world.spawn(&slave, &s.view_ctx());
        world.attach_child(&WorldObject::from(&slave), &obj);
        player.data().unit.transform.set_xyz(2.0, -3.0, 4.0);

        let snapshot = capture_financial_snapshot(&s);
        let (_, saved) = snapshot
            .characters
            .iter()
            .find(|(p, _)| p.character_id == player.character_id)
            .expect("character in snapshot");
        let t = &saved.unit.transform;
        assert_eq!(t.parent_obj_id, 0);
        assert!((t.x - 1002.0).abs() < 1e-3, "x = {}", t.x);
        assert!((t.y - 1997.0).abs() < 1e-3, "y = {}", t.y);
        assert!((t.z - 309.0).abs() < 1e-3, "z = {}", t.z);

        let live = player.data().unit.transform;
        assert_eq!(live.parent_obj_id, slave.obj_id);
        assert_eq!((live.x, live.y, live.z), (2.0, -3.0, 4.0));
    }

    fn captured_ids(snapshot: &FinancialSnapshot) -> Vec<u32> {
        let mut ids: Vec<u32> = snapshot
            .characters
            .iter()
            .map(|(p, _)| p.character_id)
            .collect();
        ids.sort_unstable();
        ids
    }

    #[tokio::test]
    async fn departure_snapshot_skips_clean_online_characters() {
        let s = test_server();
        let client = TestClient::new(&s, 1, 1);
        let leaving = spawn_player(&s, &client, test_character(1, "Leaver", 1.0, 1.0), 0x100);
        let clean = spawn_player(&s, &client, test_character(2, "Clean", 2.0, 2.0), 0x101);
        let _unknown = spawn_player(&s, &client, test_character(3, "Unknown", 3.0, 3.0), 0x102);
        s.save.seed_committed_money(&clean);
        s.save.remember_departing(&leaving);
        s.world.try_remove_character(leaving.obj_id);

        let snapshot = capture_financial_snapshot_for(&s, SnapshotScope::Departure);
        // `unknown` has no committed wallet: dirty by default.
        assert_eq!(captured_ids(&snapshot), vec![1, 3]);
    }

    #[tokio::test]
    async fn a_learned_craft_puts_a_clean_character_in_the_departure_snapshot() {
        let s = test_server();
        let client = TestClient::new(&s, 1, 1);
        let learner = spawn_player(&s, &client, test_character(1, "Learner", 1.0, 1.0), 0x100);
        s.save.seed_committed_money(&learner);
        assert!(capture_financial_snapshot_for(&s, SnapshotScope::Departure)
            .characters
            .is_empty());
        learner.data().learn_craft(987);
        let snapshot = capture_financial_snapshot_for(&s, SnapshotScope::Departure);
        assert_eq!(captured_ids(&snapshot), vec![1]);
        assert!(snapshot.characters[0]
            .1
            .learned_crafts_unsaved
            .contains(&987));
    }

    #[tokio::test]
    async fn departure_snapshot_includes_counterpart_with_changed_money_or_labor() {
        let s = test_server();
        let client = TestClient::new(&s, 1, 1);
        let leaving = spawn_player(&s, &client, test_character(1, "Leaver", 1.0, 1.0), 0x100);
        let trader = spawn_player(&s, &client, test_character(2, "Trader", 2.0, 2.0), 0x101);
        let laborer = spawn_player(&s, &client, test_character(3, "Laborer", 3.0, 3.0), 0x102);
        let clean = spawn_player(&s, &client, test_character(4, "Clean", 4.0, 4.0), 0x103);
        for p in [&trader, &laborer, &clean] {
            s.save.seed_committed_money(p);
        }
        s.save.remember_departing(&leaving);
        s.world.try_remove_character(leaving.obj_id);

        // A trade paid the counterpart.
        trader.data().money += 500;
        laborer.data().labor_power_unsaved = -3;
        let snapshot = capture_financial_snapshot_for(&s, SnapshotScope::Departure);
        assert_eq!(captured_ids(&snapshot), vec![1, 2, 3]);

        // The committed ack makes the trader clean again.
        let money = {
            let c = trader.data();
            (c.money, c.money2)
        };
        s.save.set_committed_money(2, money);
        let snapshot = capture_financial_snapshot_for(&s, SnapshotScope::Departure);
        assert_eq!(captured_ids(&snapshot), vec![1, 3]);
    }

    #[tokio::test]
    async fn autosave_snapshot_captures_every_character() {
        let s = test_server();
        let client = TestClient::new(&s, 1, 1);
        let a = spawn_player(&s, &client, test_character(1, "A", 1.0, 1.0), 0x100);
        let b = spawn_player(&s, &client, test_character(2, "B", 2.0, 2.0), 0x101);
        s.save.seed_committed_money(&a);
        s.save.seed_committed_money(&b);
        let snapshot = capture_financial_snapshot(&s);
        assert_eq!(captured_ids(&snapshot), vec![1, 2]);
    }

    /// `save_character` updates `accounts.labor`, so the snapshot must take the `accounts` rows
    /// in ascending account order: the 5 s online tracking updates `accounts.divine_clock_time`
    /// with one `WHERE account_id IN (...)` statement, which InnoDB locks in primary-key order.
    /// Two orders deadlocked on the cloud bot server (MySQL 1213). The departing player is
    /// appended after the (single, so deterministically ordered) world character, which the
    /// snapshot has to reorder.
    #[tokio::test]
    async fn autosave_snapshot_orders_account_rows_ascending() {
        let s = test_server();
        let client = TestClient::new(&s, 1, 1);
        let online = spawn_player(&s, &client, test_character(10, "Online", 1.0, 1.0), 0x100);
        online.data().account_id = 20;
        let departing = spawn_player(&s, &client, test_character(11, "Leaving", 2.0, 2.0), 0x101);
        departing.data().account_id = 10;
        s.world.try_remove_character(departing.obj_id);
        s.save.remember_departing(&departing);

        let snapshot = capture_financial_snapshot(&s);
        let order: Vec<(u32, u32)> = snapshot
            .characters
            .iter()
            .map(|(_, c)| (c.account_id, c.id))
            .collect();
        assert_eq!(order, [(10, 11), (20, 10)]);
    }
}

struct CharacterSaveAck {
    player: PlayerRef,
    updated: i64,
    removed: [Vec<u32>; 4],
    money: (i64, i64),
    /// The captured clone's `labor_power_unsaved` when `save_character` ran (flushed into
    /// `accounts.labor` by it).
    labor: i32,
    /// The captured clone's learned crafts written by `save_character`.
    learned_crafts: Vec<u32>,
}

pub(crate) struct FinancialSaveReceipt {
    counts: (usize, usize, usize, usize, usize),
    schedule_items: crate::systems::npcs::schedule_item_manager::ScheduleSaveToken,
    characters: Vec<CharacterSaveAck>,
}

pub(crate) async fn write_financial_snapshot(
    server: &GameServer,
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    snapshot: FinancialSnapshot,
) -> crate::Result<FinancialSaveReceipt> {
    let mut systems =
        crate::systems::save_prepared_financial_stage(server, tx, SaveStage::BeforeItems).await?;
    let (updated, deleted) =
        crate::systems::housing::manager::write_prepared(server, tx, snapshot.houses).await?;
    systems += updated + deleted;
    systems += server
        .systems
        .housing
        .dominions
        .save_prepared(tx, snapshot.dominions)
        .await?;
    let (updated, deleted) = server
        .systems
        .economy
        .mail
        .write_prepared(tx, snapshot.mails)
        .await?;
    systems += updated + deleted;
    let (items, deleted, containers) = server.items.write_prepared(tx, snapshot.items).await?;
    systems +=
        crate::systems::economy::shop_limits::write_prepared(tx, snapshot.shop_purchases).await?;
    let (auction_updated, auction_deleted) = server
        .systems
        .economy
        .auction
        .write_prepared(tx, snapshot.auctions)
        .await?;
    systems += auction_updated + auction_deleted;
    let schedule_items =
        crate::systems::npcs::schedule_item_manager::ScheduleItemManager::write_prepared(
            tx,
            snapshot.schedule_items,
        )
        .await?;
    systems += schedule_items.written();
    systems +=
        crate::systems::save_prepared_financial_stage(server, tx, SaveStage::AfterItems).await?;
    let mut characters = Vec::new();
    for (player, mut character) in snapshot.characters {
        let removed = [
            character.removed_friends.clone(),
            character.removed_blocked.clone(),
            character.skills.removed.clone(),
            character.quests.removed.clone(),
        ];
        // Captured before the save: `save_character` flushes this delta into `accounts.labor`
        // and clears it on the clone.
        let labor = character.labor_power_unsaved;
        let mut learned_crafts: Vec<u32> =
            character.learned_crafts_unsaved.iter().copied().collect();
        learned_crafts.sort_unstable();
        if let Err(error) = save_character(tx, &mut character, Utc::now().timestamp()).await {
            tracing::error!(
                "Failed to save character {} - {}: {error}",
                character.id,
                character.unit.name
            );
            return Err(error);
        }
        characters.push(CharacterSaveAck {
            player,
            updated: character.updated,
            removed,
            money: (character.money, character.money2),
            labor,
            learned_crafts,
        });
    }
    systems += save_stage(server, tx, SaveStage::AfterCharacters).await?;
    Ok(FinancialSaveReceipt {
        counts: (items, deleted, containers, characters.len(), systems),
        schedule_items,
        characters,
    })
}

pub(crate) fn commit_financial_snapshot(server: &GameServer, receipt: FinancialSaveReceipt) {
    crate::systems::housing::manager::commit_save(server);
    server.systems.housing.dominions.commit_save();
    server.systems.economy.mail.commit_save();
    server.systems.economy.auction.commit_save();
    server.systems.social.bounties.commit_save();
    server.systems.social.nations.commit_save();
    server.items.commit_save();
    server
        .systems
        .npcs
        .schedule_items
        .commit_save(receipt.schedule_items);
    for ack in receipt.characters {
        server
            .save
            .set_committed_money(ack.player.character_id, ack.money);
        let mut live = ack.player.data();
        let unchanged_money = (live.money, live.money2) == ack.money;
        live.updated = ack.updated;
        let [friends, blocked, skills, quests] = ack.removed;
        live.removed_friends.retain(|id| !friends.contains(id));
        live.removed_blocked.retain(|id| !blocked.contains(id));
        live.skills.removed.retain(|id| !skills.contains(id));
        live.quests.removed.retain(|id| !quests.contains(id));
        let learned_crafts = ack.learned_crafts;
        live.learned_crafts_unsaved
            .retain(|id| !learned_crafts.contains(id));
        // The saved snapshot flushed `ack.labor` into `accounts.labor`; whatever the live
        // character spent since the capture stays pending for the next save.
        live.labor_power_unsaved -= ack.labor;
        let labor_pending = live.labor_power_unsaved != 0;
        drop(live);
        if unchanged_money && !labor_pending {
            let mut departing = server
                .save
                .departing
                .lock()
                .expect("departure saves poisoned");
            if departing
                .get(&ack.player.character_id)
                .is_some_and(|p| Arc::ptr_eq(p, &ack.player))
            {
                departing.remove(&ack.player.character_id);
                drop(departing);
                server.save.forget_committed_money(ack.player.character_id);
                if server
                    .world
                    .character_by_obj_id(ack.player.obj_id)
                    .is_some_and(|p| Arc::ptr_eq(&p, &ack.player))
                {
                    server.world.try_remove_character(ack.player.obj_id);
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct SaveManager {
    /// `_isSaving`.
    saving: AtomicBool,
    /// `_enabled`.
    enabled: AtomicBool,
    /// Prevent a late world snapshot from overwriting a disconnect's newer character save.
    transactions: tokio::sync::Mutex<()>,
    /// Keep departed wallets available for a full transaction retry after database failure.
    departing: Mutex<HashMap<u32, PlayerRef>>,
    /// `(money, money2)` last committed to (or loaded from) the database per online character.
    /// A character without an entry counts as financially dirty. Never held with a data lock.
    committed_money: Mutex<HashMap<u32, (i64, i64)>>,
    /// Online-tracking seconds not yet committed to `accounts.divine_clock_time`, per account.
    /// The 5 s tracking task queues them and the next financial save writes them after its
    /// commit: as a separate 5 s statement the clock update waited on the account rows the
    /// save's transaction held (1-2.5 s on the 2026-11-11 cloud run).
    pending_divine_clock: Mutex<BTreeMap<u32, u32>>,
}

/// Account ids per `UPDATE ... IN (...)` statement of the divine-clock flush.
const CLOCK_UPDATE_CHUNK: usize = 500;

impl SaveManager {
    /// Queues online-tracking seconds for an account; the next financial save writes them.
    pub(crate) fn add_divine_clock(&self, account_id: u32, delta: u32) {
        if delta == 0 {
            return;
        }
        let mut pending = self
            .pending_divine_clock
            .lock()
            .expect("divine clock poisoned");
        let total = pending.entry(account_id).or_insert(0);
        *total = total.saturating_add(delta);
    }

    /// Pending per-account seconds (tests).
    #[cfg(test)]
    pub(crate) fn pending_divine_clock(&self) -> BTreeMap<u32, u32> {
        self.pending_divine_clock
            .lock()
            .expect("divine clock poisoned")
            .clone()
    }

    /// Writes the queued divine-clock seconds, one statement per distinct delta (accounts with
    /// the same number of seconds share it). Called after a save committed while the caller
    /// still holds the save transaction lock: the committed save no longer holds the account
    /// rows, so no statement waits for its commit. A failed statement puts its accounts back
    /// for the next save.
    async fn flush_divine_clock(&self, server: &GameServer) {
        let pending: BTreeMap<u32, u32> = std::mem::take(
            &mut *self
                .pending_divine_clock
                .lock()
                .expect("divine clock poisoned"),
        );
        if pending.is_empty() {
            return;
        }
        let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (account, delta) in pending {
            if delta != 0 {
                groups.entry(delta).or_default().push(account);
            }
        }
        for (delta, ids) in groups {
            for chunk in ids.chunks(CLOCK_UPDATE_CHUNK) {
                if !crate::systems::services::accounts::add_divine_clock_time(server, chunk, delta)
                    .await
                {
                    for &id in chunk {
                        self.add_divine_clock(id, delta);
                    }
                }
            }
        }
    }

    /// Seeds the committed wallet of a character that just entered the world from the database.
    pub(crate) fn seed_committed_money(&self, player: &PlayerRef) {
        let money = {
            let c = player.data();
            (c.money, c.money2)
        };
        self.set_committed_money(player.character_id, money);
    }

    fn set_committed_money(&self, character_id: u32, money: (i64, i64)) {
        self.committed_money
            .lock()
            .expect("committed money poisoned")
            .insert(character_id, money);
    }

    fn forget_committed_money(&self, character_id: u32) {
        self.committed_money
            .lock()
            .expect("committed money poisoned")
            .remove(&character_id);
    }

    /// True when the wallet differs from the committed one (or none is known), labor is
    /// pending, or a learned craft is not yet committed. Money is the only character-row field
    /// other characters' operations change; the learned crafts commit in the same snapshot as
    /// the recipe-item deletion.
    fn is_financially_dirty(&self, player: &PlayerRef) -> bool {
        let (money, labor_pending, craft_pending) = {
            let c = player.data();
            (
                (c.money, c.money2),
                c.labor_power_unsaved != 0,
                !c.learned_crafts_unsaved.is_empty(),
            )
        };
        labor_pending
            || craft_pending
            || self
                .committed_money
                .lock()
                .expect("committed money poisoned")
                .get(&player.character_id)
                != Some(&money)
    }

    pub(crate) fn pending_departure(&self, character_id: u32) -> Option<PlayerRef> {
        self.departing
            .lock()
            .expect("departure saves poisoned")
            .get(&character_id)
            .cloned()
    }

    /// Every departing player of `account_id` (a sibling character loading must flush their
    /// pending labor first). The departing map lock is released before any player data lock.
    pub(crate) fn pending_departures_of_account(&self, account_id: u32) -> Vec<PlayerRef> {
        let candidates: Vec<PlayerRef> = self
            .departing
            .lock()
            .expect("departure saves poisoned")
            .values()
            .cloned()
            .collect();
        candidates
            .into_iter()
            .filter(|p| p.data().account_id == account_id)
            .collect()
    }

    /// Test hook: pretend the departure save was acknowledged.
    #[cfg(test)]
    pub(crate) fn forget_departing(&self, character_id: u32) {
        self.departing
            .lock()
            .expect("departure saves poisoned")
            .remove(&character_id);
    }

    pub(crate) fn remember_departing(&self, player: &PlayerRef) {
        self.departing
            .lock()
            .expect("departure saves poisoned")
            .insert(player.character_id, player.clone());
    }

    pub(crate) async fn transaction_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.transactions.lock().await
    }

    /// Caller owns transaction_lock. A logout writes the wallet with its property, mail,
    /// certificate and treasury state, rather than committing the wallet alone.
    pub(crate) async fn save_departing_locked(
        &self,
        server: &GameServer,
        player: &PlayerRef,
    ) -> bool {
        self.remember_departing(player);
        let result: crate::Result<()> = async {
            let snapshot = capture_financial_snapshot_for(server, SnapshotScope::Departure);
            let mut tx = server.db.begin().await?;
            let receipt = write_financial_snapshot(server, &mut tx, snapshot).await?;
            tx.commit().await?;
            commit_financial_snapshot(server, receipt);
            self.flush_divine_clock(server).await;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(
                "Departure financial save failed for {}: {error}",
                player.character_id
            );
            return false;
        }
        true
    }

    /// Caller owns `transaction_lock`. Saves every departing player of `account_id` that still
    /// holds unflushed labor (`labor_power_unsaved != 0`), because `accounts.labor` is
    /// account-level and a sibling character loading must not read it stale. Returns `false`
    /// when a save fails or leaves labor pending.
    pub(crate) async fn flush_account_labor_locked(
        &self,
        server: &GameServer,
        account_id: u32,
    ) -> bool {
        for departing in self.pending_departures_of_account(account_id) {
            // Read the pending delta into a local: no data lock is held across the await below.
            let pending = departing.data().labor_power_unsaved != 0;
            if !pending {
                continue;
            }
            if !self.save_departing_locked(server, &departing).await
                || departing.data().labor_power_unsaved != 0
            {
                return false;
            }
        }
        true
    }

    /// `DoSave()`. Returns `true` when the data was committed (or there was nothing to save).
    pub async fn do_save(&self, server: &GameServer) -> bool {
        if self.saving.swap(true, Ordering::SeqCst) {
            return false;
        }
        let _transaction = self.transaction_lock().await;
        let started = std::time::Instant::now();
        tracing::debug!("Saving DB ...");
        let res: crate::Result<bool> = async {
            let snapshot = capture_financial_snapshot(server);
            let mut tx = server.db.begin().await?;
            let receipt = write_financial_snapshot(server, &mut tx, snapshot).await?;
            let (items, deleted, containers, characters, systems) = receipt.counts;
            let total = items + deleted + containers + characters + systems;
            if total == 0 {
                tracing::debug!("No data to update ...");
            }
            tx.commit().await?;
            commit_financial_snapshot(server, receipt);
            self.flush_divine_clock(server).await;
            if items + deleted > 0 {
                tracing::debug!(
                    "Updated {items} and deleted {deleted} items in {containers} containers ..."
                );
            }
            if containers > 0 {
                tracing::debug!("Updated {containers} item containers ...");
            }
            if characters > 0 {
                tracing::debug!("Updated {characters} characters ...");
            }
            Ok(true)
        }
        .await;
        let saved = match res {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("DoSave Exception: {e}");
                false
            }
        };
        tracing::debug!("Saving data took {:?}", started.elapsed());
        self.saving.store(false, Ordering::SeqCst);
        saved
    }

    /// `Initialize()` + `SaveTickStart()`.
    pub fn start(server: Arc<GameServer>) {
        server.save.enabled.store(true, Ordering::SeqCst);
        let minutes = server.config.world.auto_save_interval.max(0.01);
        tokio::spawn(async move {
            let period = Duration::from_secs_f64(minutes * 60.0);
            let mut iv = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            loop {
                iv.tick().await;
                if !server.save.enabled.load(Ordering::SeqCst) {
                    tracing::warn!("Auto-Saving disabled, skipping ...");
                    continue;
                }
                server.save.do_save(&server).await;
            }
        });
    }

    /// `StopAsync()`: disable the tick and do one final save.
    pub async fn stop(&self, server: &GameServer) {
        self.enabled.store(false, Ordering::SeqCst);
        // Wait for the active world/logout transaction, then capture the final state.
        // do_save deliberately returns early when busy and cannot serve as this barrier.
        let _transaction = self.transaction_lock().await;
        let result: crate::Result<()> = async {
            let snapshot = capture_financial_snapshot(server);
            let mut tx = server.db.begin().await?;
            let receipt = write_financial_snapshot(server, &mut tx, snapshot).await?;
            tx.commit().await?;
            commit_financial_snapshot(server, receipt);
            self.flush_divine_clock(server).await;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!("Final financial save failed: {error}");
        }
    }
}
