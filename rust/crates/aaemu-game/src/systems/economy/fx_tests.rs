//! Persistence effects must become visible before deferred callbacks can start another save.

use std::sync::{Arc, Mutex};

use crate::data::items::ItemTemplate;
use crate::models::item::Item;
use crate::models::item_task::{ItemFx, ItemFxList};
use crate::server::GameServer;
use crate::systems::economy::hooks::QuestItemHooks;
use crate::systems::economy::inventory::ItemTxn;
use crate::test_support::{
    finishes_in_time, spawn_player, test_character, test_server, test_server_with, TestClient,
};
use crate::world::PlayerRef;

struct ObservePersistence {
    removed: u64,
    loose: u64,
    events: Mutex<Vec<(bool, i32)>>,
}

impl ObservePersistence {
    fn observe(&self, server: &GameServer, acquired: bool, count: i32) {
        // A callback can begin another economy operation, so the original guard must be gone.
        let _guard = server.systems.economy.ops_lock();
        assert!(server.items.removed_items().contains(&self.removed));
        assert!(server.items.item_by_id(self.loose).is_some());
        self.events.lock().unwrap().push((acquired, count));
    }
}

impl QuestItemHooks for ObservePersistence {
    fn on_items_acquired(&self, server: &GameServer, _: &PlayerRef, _: u32, count: i32) {
        self.observe(server, true, count);
    }

    fn on_items_consumed(&self, server: &GameServer, _: &PlayerRef, _: u32, count: i32) {
        self.observe(server, false, count);
    }
}

fn loose_item(server: &GameServer, id: u64) -> Item {
    Item::create(
        id,
        &ItemTemplate::plain(500, 10),
        1,
        0,
        1,
        &server.game_data.items,
        77,
    )
}

fn queue_effects(txn: &mut ItemTxn<'_>, removed: u64, loose: u64) {
    txn.env.fx.push(ItemFx::Acquired {
        owner: 1,
        template_id: 500,
        count: 3,
    });
    txn.env.fx.push(ItemFx::ReleaseId(removed));
    txn.env.fx.push(ItemFx::Consumed {
        owner: 1,
        template_id: 500,
        count: 2,
    });
    txn.env
        .fx
        .push(ItemFx::Orphaned(Box::new(loose_item(txn.server, loose))));
}

#[tokio::test]
async fn deferred_transaction_persists_items_before_callbacks_and_does_not_release_twice() {
    let server = Arc::new(test_server());
    server.set_self();
    let client = TestClient::new(&server, 1, 1001);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "Persistence", 100.0, 100.0),
        0x100,
    );
    let removed = u64::from(server.ids.item.get_next_id().unwrap());
    let loose = u64::from(server.ids.item.get_next_id().unwrap());
    let observer = Arc::new(ObservePersistence {
        removed,
        loose,
        events: Mutex::new(Vec::new()),
    });
    server
        .systems
        .economy
        .hooks
        .quests
        .install(observer.clone());
    let mut txn = ItemTxn::begin(&server);
    queue_effects(&mut txn, removed, loose);
    let fx = txn.into_fx();

    // This is the state a financial snapshot sees after the operation lock is released,
    // even when its caller deliberately delays notification/quest/world effects.
    let guard = server.systems.economy.ops_lock();
    assert_eq!(server.items.removed_items(), vec![removed]);
    assert_eq!(server.items.item_by_id(loose).unwrap().id, loose);
    assert!(observer.events.lock().unwrap().is_empty());
    assert_eq!(fx.len(), 2);
    drop(guard);
    // Re-reserve the freed ID before flushing callbacks. Replaying ReleaseId would free it.
    assert_eq!(u64::from(server.ids.item.get_next_id().unwrap()), removed);
    super::flush(&server, fx);
    assert_ne!(u64::from(server.ids.item.get_next_id().unwrap()), removed);
    assert_eq!(
        *observer.events.lock().unwrap(),
        vec![(true, 3), (false, 2)]
    );
    assert_eq!(server.items.removed_items(), vec![removed]);
    assert!(server.items.take_loose(loose).is_some());
    assert!(server.items.take_loose(loose).is_none());
}

#[tokio::test]
async fn transaction_drop_applies_persistence_before_reentrant_callbacks() {
    let server = Arc::new(test_server());
    server.set_self();
    let client = TestClient::new(&server, 1, 1001);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "Persistence", 100.0, 100.0),
        0x100,
    );
    let removed = u64::from(server.ids.item.get_next_id().unwrap());
    let loose = u64::from(server.ids.item.get_next_id().unwrap());
    let observer = Arc::new(ObservePersistence {
        removed,
        loose,
        events: Mutex::new(Vec::new()),
    });
    server
        .systems
        .economy
        .hooks
        .quests
        .install(observer.clone());
    let mut txn = ItemTxn::begin(&server);
    queue_effects(&mut txn, removed, loose);
    drop(txn);
    assert_eq!(
        *observer.events.lock().unwrap(),
        vec![(true, 3), (false, 2)]
    );
}

#[tokio::test]
async fn persistence_flush_preserves_release_and_orphan_order() {
    let server = test_server();
    let first = u64::from(server.ids.item.get_next_id().unwrap());
    let second = u64::from(server.ids.item.get_next_id().unwrap());
    let mut fx = ItemFxList(vec![
        ItemFx::Orphaned(Box::new(loose_item(&server, first))),
        ItemFx::ReleaseId(first),
        ItemFx::ReleaseId(second),
        ItemFx::Orphaned(Box::new(loose_item(&server, second))),
    ]);
    super::flush_persistence(&server, &mut fx);
    assert!(fx.is_empty());
    assert!(server.items.item_by_id(first).is_none());
    assert_eq!(server.items.item_by_id(second).unwrap().id, second);
    assert_eq!(server.items.removed_items(), vec![first, second]);
    super::flush_persistence(&server, &mut fx);
    assert_eq!(server.items.removed_items(), vec![first, second]);
}

/// The real `AddExp` ends in `quests::events::on_level_up`, a quest operation that takes the
/// economy ops lock. This stub keeps that behavior (the real hook needs the experience tables)
/// and records that the lock was free by then.
struct LevelUpProgression;

impl crate::systems::economy::hooks::ProgressionHooks for LevelUpProgression {
    fn add_exp(&self, server: &GameServer, player: &PlayerRef, exp: i32, add_ability_exp: bool) {
        assert!(!crate::systems::economy::System::ops_held_on_this_thread());
        player.send(crate::systems::skills::packets::SCExpChangedPacket {
            obj_id: player.obj_id,
            exp,
            should_add_ability_exp: add_ability_exp,
        });
        crate::systems::quests::events::on_level_up(server, player);
    }
}

/// `Character.ChangeLabor` inside an `ItemTxn` (doodad placement, mail money, specialty sale):
/// the labor experience must run after the ops lock is released, not re-take it, and the
/// packets keep the C# order: item operation packets, `SCExpChanged`, then the labor packet.
#[tokio::test]
async fn change_labor_in_a_transaction_defers_the_experience_until_the_lock_is_released() {
    use crate::opcodes::sc;
    let server = Arc::new(test_server());
    server.set_self();
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Laborer", 100.0, 100.0),
        0x100,
    );
    server
        .systems
        .economy
        .hooks
        .progression
        .install(Arc::new(LevelUpProgression));
    player.data().labor_power = 50;
    client.drain_ops();
    let (s, p) = (server.clone(), player.clone());
    finishes_in_time("change_labor in a transaction", move || {
        let mut txn = ItemTxn::begin(&s);
        crate::systems::economy::inventory::send_error_message_to(&mut txn.out(), &p, 1);
        crate::systems::economy::inventory::change_labor(&mut txn.out(), &s, &p, -5, 0);
        drop(txn);
    });
    assert_eq!(
        client.drain_ops(),
        vec![
            sc::SCErrorMsgPacket,
            sc::SCExpChangedPacket,
            sc::SCCharacterLaborPowerChangedPacket
        ]
    );
    assert_eq!(player.data().labor_power, 45);
}

/// `with_quests` takes the non-reentrant ops lock itself: called with it already held on this
/// thread it must refuse (error log, `None`) instead of hanging the server.
#[tokio::test]
async fn with_quests_refuses_while_the_ops_lock_is_held_on_this_thread() {
    let server = Arc::new(test_server());
    server.set_self();
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Quester", 100.0, 100.0),
        0x100,
    );
    let (s, p) = (server.clone(), player.clone());
    let ran = finishes_in_time("with_quests under the ops lock", move || {
        let _ops = s.systems.economy.ops_lock();
        crate::systems::quests::events::with_quests(&s, &p, None, |_| ())
    });
    assert!(ran.is_none());
    // Without the lock held it runs as before.
    assert!(crate::systems::quests::events::with_quests(&server, &player, None, |_| ()).is_some());
}

/// A quest hook that re-reads the character's bags like `ObjItemGather` does
/// (`quests::engine::items_by_template`): it locks the bag container and the character.
struct BagReadingQuestHooks {
    seen: Mutex<Vec<(u32, i32)>>,
}

impl BagReadingQuestHooks {
    fn read(&self, player: &PlayerRef, template_id: u32) {
        let units = crate::systems::quests::engine::items_count(&player.data(), template_id);
        self.seen.lock().unwrap().push((template_id, units));
    }
}

impl QuestItemHooks for BagReadingQuestHooks {
    fn on_items_acquired(&self, _: &GameServer, player: &PlayerRef, template_id: u32, _: i32) {
        self.read(player, template_id);
    }

    fn on_items_consumed(&self, _: &GameServer, player: &PlayerRef, template_id: u32, _: i32) {
        self.read(player, template_id);
    }
}

fn server_with_item_template(template_id: u32) -> Arc<GameServer> {
    let mut data = crate::data::GameData::empty();
    data.items
        .templates
        .insert(template_id, ItemTemplate::plain(template_id, 100));
    Arc::new(test_server_with(data))
}

fn player_with_bag_item(
    server: &Arc<GameServer>,
    template_id: u32,
    count: i32,
) -> (PlayerRef, Arc<BagReadingQuestHooks>) {
    server.set_self();
    let client = TestClient::new(server, 1, 1001);
    let player = spawn_player(
        server,
        &client,
        test_character(1, "Gatherer", 100.0, 100.0),
        0x100,
    );
    let id = u64::from(server.ids.item.get_next_id().unwrap());
    let mut item = Item::create(
        id,
        &ItemTemplate::plain(template_id, 100),
        count,
        0,
        1,
        &server.game_data.items,
        0,
    );
    {
        let bag = player.data().inventory.bag().clone();
        let mut bag = bag.lock().unwrap();
        item.slot = bag.get_unused_slot(-1);
        bag.items.push(item);
        bag.update_free_slot_count();
    }
    let hooks = Arc::new(BagReadingQuestHooks {
        seen: Mutex::new(Vec::new()),
    });
    server.systems.economy.hooks.quests.install(hooks.clone());
    (player, hooks)
}

/// The bridge functions end with `lock(&bag).consume_item(.., &mut txn.env)`. A temporary guard
/// in a tail expression is dropped after the locals (edition 2021), so the `ItemTxn` flushed the
/// `Consumed` quest event while the bag was still locked, and the quest code re-locking the bag
/// on the same thread deadlocked the server (holding the economy ops lock).
#[tokio::test]
async fn bridge_consume_does_not_hold_the_bag_while_quest_events_run() {
    use crate::models::item_task::task_type;
    use crate::systems::economy::bridges::{EconomyForServices, EconomyForSkills};
    use crate::systems::services::item_ops::CharacterItemOps;
    use crate::systems::skills::ext::ItemSkillHooks;

    let server = server_with_item_template(500);
    let (player, hooks) = player_with_bag_item(&server, 500, 10);
    let (s, p, h) = (server.clone(), player.clone(), hooks.clone());
    finishes_in_time("bridge consume with a bag-reading quest hook", move || {
        assert_eq!(
            EconomyForServices.consume_item(&s, &p, task_type::QUEST_SUPPLY_ITEMS, 500, 3),
            3
        );
        assert_eq!(
            EconomyForSkills.consume_bag_item(&s, &p, task_type::QUEST_SUPPLY_ITEMS, 500, 2),
            2
        );
        assert_eq!(
            *h.seen.lock().unwrap(),
            vec![(500, 7), (500, 5)],
            "each Consumed event sees the bag after the removal"
        );
    });
}

/// Same hazard on the acquiring side (`acquire_default_item` bridge).
#[tokio::test]
async fn bridge_acquire_does_not_hold_the_bag_while_quest_events_run() {
    use crate::models::item_task::task_type;
    use crate::systems::economy::bridges::EconomyForSkills;
    use crate::systems::skills::ext::ItemSkillHooks;

    let server = server_with_item_template(500);
    let (player, hooks) = player_with_bag_item(&server, 500, 10);
    let (s, p) = (server.clone(), player.clone());
    finishes_in_time("bridge acquire with a bag-reading quest hook", move || {
        assert!(EconomyForSkills.acquire_default_item(
            &s,
            &p,
            task_type::QUEST_SUPPLY_ITEMS,
            500,
            1
        ));
    });
    assert_eq!(*hooks.seen.lock().unwrap(), vec![(500, 11)]);
}
