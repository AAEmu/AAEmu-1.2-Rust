//! Achievement flow tests: the live producers and the entry snapshot, driven through the real
//! packet / gameplay entry points (no MySQL).

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::items::ItemTemplate;
use crate::data::GameData;
use crate::models::container::{slot_type, ContainerKind, ContainerRef, ItemContainer};
use crate::models::item::Item;
use crate::models::item_task::task_type;
use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::economy::inventory::{add_money, lock_pair, subtract_money, Inv, ItemTxn};
use crate::systems::economy::loot::{give_loot_pack, GeneratedLoot, ScriptedRng};
use crate::systems::economy::loot_data::LootPack;
use crate::systems::skills::progress::{add_exp_world_tail, ExpGain};
use crate::systems::social::achievement::{
    char_record_kind, AchievementGameData, AchievementObjectives, Achievements, CharRecords,
};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

/// The reward item the tests grant.
pub(crate) const REWARD_ITEM: u32 = 7001;

/// Content with one achievement (id `achievement_id`, `complete_num` to complete, optional item
/// reward) watching one record (`record_id`, of `kind_id` with `value1` / `value2`).
pub(crate) fn one_achievement(
    achievement_id: u32,
    complete_num: u32,
    reward_item: u32,
    record_id: u32,
    kind_id: u32,
    value1: u32,
    value2: u32,
) -> AchievementGameData {
    let mut data = AchievementGameData::default();
    let objective = AchievementObjectives {
        id: 1,
        achievement_id,
        record_id,
        ..Default::default()
    };
    data.achievements.insert(
        achievement_id,
        Achievements {
            id: achievement_id,
            complete_num,
            // Sum the record values (the shape of the threshold achievements).
            complete_or: false,
            is_active: true,
            item_id: reward_item,
            achievement_objectives: vec![objective],
            ..Default::default()
        },
    );
    data.achievement_objectives
        .insert(achievement_id, vec![objective]);
    data.char_records.insert(
        record_id,
        CharRecords {
            id: record_id,
            kind_id,
            value1,
            value2,
        },
    );
    data
}

/// A server with the reward item template and the given achievement content, the social hooks
/// installed (the producers report through them).
pub(crate) fn server_with_content(data: AchievementGameData) -> Arc<GameServer> {
    let mut game_data = GameData::empty();
    game_data
        .items
        .templates
        .insert(REWARD_ITEM, ItemTemplate::plain(REWARD_ITEM, 1));
    game_data.systems.social.achievements = data;
    let server = Arc::new(test_server_with(game_data));
    server.set_self();
    crate::systems::social::install_hooks(&server);
    server
}

/// A spawned, online character with an active connection and registered bag / mail escrow.
pub(crate) fn player_of(server: &GameServer) -> (TestClient, PlayerRef) {
    server.names.add_character(1, "Tester", 1);
    let mut client = TestClient::new(server, 1, 1001);
    let player = spawn_player(
        server,
        &client,
        test_character(1, "Tester", 100.0, 100.0),
        0x100,
    );
    player.data().is_online = true;
    client.conn.active_char = Some(player.clone());
    let inventory = Inv::of(&player);
    server.items.add_container(inventory.bag());
    if let Some(escrow) = inventory.get(slot_type::MAIL) {
        server.items.add_container(escrow);
    }
    client.drain();
    (client, player)
}

/// The achievement opcodes among `ops`, in order.
pub(crate) fn achievement_ops(ops: Vec<u16>) -> Vec<u16> {
    ops.into_iter()
        .filter(|op| {
            matches!(
                *op,
                sc::SCAchievementsPacket
                    | sc::SCAchievementChangedPacket
                    | sc::SCAchievementCompletedPacket
                    | sc::SCAchievementItemSentPacket
            )
        })
        .collect()
}

pub(crate) fn bag_count(player: &PlayerRef) -> usize {
    Inv::of(player)
        .bag()
        .lock()
        .expect("container poisoned")
        .items
        .len()
}

#[tokio::test]
async fn entering_the_world_sends_the_achievement_snapshot() {
    // Achievement 1 (reach level 5) is back-credited from the character's level on entry; the
    // list carries it and the reward is paid.
    let server = server_with_content(one_achievement(
        1,
        5,
        REWARD_ITEM,
        10,
        char_record_kind::CHAR_LEVEL,
        0,
        0,
    ));
    let (mut client, player) = player_of(&server);
    player.data().unit.level = 7;

    server
        .handle_frame(
            &mut client.conn,
            &encode_raw(1, cs::CSNotifyInGamePacket, PacketStream::new().as_bytes()),
        )
        .await
        .expect("handler");

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [sc::SCAchievementsPacket, sc::SCAchievementItemSentPacket]
    );
    assert_eq!(bag_count(&player), 1);
    assert!(player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn a_level_up_progresses_and_completes_an_achievement_and_pays_the_reward() {
    let server = server_with_content(one_achievement(
        1,
        10,
        REWARD_ITEM,
        10,
        char_record_kind::CHAR_LEVEL,
        0,
        0,
    ));
    let (mut client, player) = player_of(&server);
    player.data().unit.level = 10;
    let gain = ExpGain {
        exp: 100,
        should_add_ability_exp: false,
        new_level: Some(10),
        upgraded_skills: Vec::new(),
    };

    add_exp_world_tail(&server, &player, &gain);

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket,
            sc::SCAchievementItemSentPacket,
        ]
    );
    assert_eq!(bag_count(&player), 1);
    assert_eq!(player.data().achievement_state.amount(1), 10);
}

#[tokio::test]
async fn a_used_item_counts_towards_its_record() {
    let server = server_with_content(one_achievement(
        1,
        2,
        0,
        10,
        char_record_kind::USE_ITEM,
        555,
        0,
    ));
    let (mut client, player) = player_of(&server);

    crate::systems::quests::events::on_item_use(&server, &player, 555);
    assert_eq!(
        achievement_ops(client.drain_ops()),
        [sc::SCAchievementChangedPacket]
    );
    // Another item is not this record.
    crate::systems::quests::events::on_item_use(&server, &player, 556);
    assert!(achievement_ops(client.drain_ops()).is_empty());

    crate::systems::quests::events::on_item_use(&server, &player, 555);
    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket
        ]
    );
}

#[tokio::test]
async fn an_event_under_the_economy_lock_is_queued_until_the_transaction_ends() {
    use crate::systems::economy::inventory::ItemTxn;
    use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};

    let server = server_with_content(one_achievement(
        1,
        1,
        REWARD_ITEM,
        10,
        char_record_kind::USE_SKILL,
        77,
        0,
    ));
    let (mut client, player) = player_of(&server);

    let txn = ItemTxn::begin(&server);
    // The economy operation lock is not re-entrant: reporting (and paying the reward, which
    // opens its own transaction) here would deadlock, so the event waits for the release.
    crate::systems::report_achievement(
        &server,
        &player,
        AchievementInput::Increment {
            kind_id: char_record_kind::USE_SKILL,
            amount: 1,
            matcher: RecordMatch::Value1(77),
        },
    );
    assert!(achievement_ops(client.drain_ops()).is_empty());
    drop(txn);

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket,
            sc::SCAchievementItemSentPacket,
        ]
    );
    assert_eq!(bag_count(&player), 1);
}

/// An achievement that asks for `complete_num` gold (kind 14) and the record it watches.
fn millionaire_content(complete_num: u32) -> AchievementGameData {
    one_achievement(1, complete_num, 0, 14, char_record_kind::MY_GOLD, 0, 0)
}

#[tokio::test]
async fn an_upward_money_change_credits_my_gold_with_the_absolute_total() {
    let server = server_with_content(millionaire_content(1_000_000));
    let (mut client, player) = player_of(&server);
    // 900,000 gold carried; one deposit takes the purse to 1,000,000 gold.
    player.data().money = 9_000_000_000;

    let mut txn = ItemTxn::begin(&server);
    {
        let mut out = txn.out();
        assert!(add_money(
            &mut out,
            &player,
            slot_type::INVENTORY,
            1_000_000_000,
            task_type::DEPOSIT_MONEY,
        ));
    }
    // Under the economy operation lock the report must not run: the record stays untouched.
    assert!(achievement_ops(client.drain_ops()).is_empty());
    assert_eq!(player.data().achievement_state.record_value(14), 0);
    assert_eq!(player.data().money, 10_000_000_000);
    drop(txn);

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket
        ]
    );
    assert_eq!(player.data().achievement_state.record_value(14), 1_000_000);
    assert_eq!(player.data().achievement_state.amount(1), 1_000_000);
    assert!(player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn spending_gold_does_not_lower_the_my_gold_record() {
    let server = server_with_content(millionaire_content(1_000_000));
    let (mut client, player) = player_of(&server);
    player.data().money = 9_000_000_000;

    let mut txn = ItemTxn::begin(&server);
    {
        let mut out = txn.out();
        assert!(add_money(
            &mut out,
            &player,
            slot_type::INVENTORY,
            1_000_000_000,
            task_type::DEPOSIT_MONEY,
        ));
    }
    drop(txn);
    assert_eq!(player.data().achievement_state.record_value(14), 1_000_000);
    client.drain();

    // Spending is a downward change: it neither reports nor lowers the high-water record.
    let mut txn = ItemTxn::begin(&server);
    {
        let mut out = txn.out();
        assert!(subtract_money(
            &mut out,
            &player,
            slot_type::INVENTORY,
            1_000_000_000,
            task_type::DEPOSIT_MONEY,
        ));
    }
    drop(txn);
    assert!(achievement_ops(client.drain_ops()).is_empty());
    assert_eq!(player.data().money, 9_000_000_000);
    assert_eq!(player.data().achievement_state.record_value(14), 1_000_000);
}

#[tokio::test]
async fn a_money_change_under_the_economy_lock_reports_after_the_transaction() {
    // The deadlock regression shape: reporting (and paying a reward) inside the operation lock
    // would deadlock, so the queued effect waits for the lock release.
    let server = server_with_content(millionaire_content(1));
    let (mut client, player) = player_of(&server);

    let mut txn = ItemTxn::begin(&server);
    {
        let mut out = txn.out();
        assert!(add_money(
            &mut out,
            &player,
            slot_type::INVENTORY,
            20_000,
            task_type::DEPOSIT_MONEY,
        ));
    }
    assert!(achievement_ops(client.drain_ops()).is_empty());
    assert_eq!(player.data().achievement_state.record_value(14), 0);
    drop(txn);

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket
        ]
    );
    assert_eq!(player.data().achievement_state.record_value(14), 2);
}

#[tokio::test]
async fn several_money_changes_in_one_operation_report_once_with_the_final_value() {
    let server = server_with_content(millionaire_content(1_000_000));
    let (mut client, player) = player_of(&server);

    let mut txn = ItemTxn::begin(&server);
    {
        let mut out = txn.out();
        // Five deposits of 200,000 gold each: 1,000,000 gold, one report.
        for _ in 0..5 {
            assert!(add_money(
                &mut out,
                &player,
                slot_type::INVENTORY,
                2_000_000_000,
                task_type::DEPOSIT_MONEY,
            ));
        }
    }
    drop(txn);

    // One report of the batch's final value (1,000,000 gold), not one per change.
    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket
        ]
    );
    assert_eq!(player.data().achievement_state.record_value(14), 1_000_000);
}

#[tokio::test]
async fn a_queued_refresh_runs_when_the_packet_is_handled() {
    let server = server_with_content(one_achievement(
        1,
        4,
        0,
        10,
        char_record_kind::CHAR_LEVEL,
        0,
        0,
    ));
    let (mut client, player) = player_of(&server);
    player.data().unit.level = 4;

    // A caller under the team manager's lock cannot report; it queues, and the next handled
    // packet flushes the queue.
    crate::systems::refresh_achievements_later(&server, &player);
    assert!(achievement_ops(client.drain_ops()).is_empty());
    server
        .handle_frame(
            &mut client.conn,
            &encode_raw(1, cs::CSNotifyInGameCompletedPacket, &[]),
        )
        .await
        .expect("handler");

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// GET_ITEM_TYPE (kind 29): genuine item acquisitions only.
// ---------------------------------------------------------------------------------------------

/// The item the acquisition records name.
const GET_ITEM: u32 = 7100;
/// An item no record names.
const OTHER_ITEM: u32 = 7101;
/// The `char_records` row id used by the acquisition tests.
const GET_ITEM_RECORD: u32 = 29;

fn acquisition_server(complete_num: u32) -> Arc<GameServer> {
    let mut game_data = GameData::empty();
    for id in [GET_ITEM, OTHER_ITEM] {
        game_data
            .items
            .templates
            .insert(id, ItemTemplate::plain(id, 20));
    }
    game_data.systems.social.achievements = one_achievement(
        1,
        complete_num,
        0,
        GET_ITEM_RECORD,
        char_record_kind::GET_ITEM_TYPE,
        GET_ITEM,
        u32::MAX,
    );
    let server = Arc::new(test_server_with(game_data));
    server.set_self();
    crate::systems::social::install_hooks(&server);
    server
}

/// Acquires through the same `AcquireDefaultItem` path ground pick-up, loot, craft products,
/// quest rewards and shop purchases use.
fn acquire_items(server: &GameServer, player: &PlayerRef, template_id: u32, count: i32) -> bool {
    let inv = Inv::of(player);
    let bag = inv.bag();
    let mut txn = ItemTxn::begin(server);
    let ok = {
        let mut bag = bag.lock().expect("container poisoned");
        bag.acquire_default_item(task_type::GM, template_id, count, 0, 0, &mut txn.env)
    };
    drop(txn);
    ok
}

/// Puts an already owned item into one of the character's containers without raising any item
/// effect (the state a saved character loads with).
fn seed_owned_item(
    server: &GameServer,
    player: &PlayerRef,
    st: u8,
    template_id: u32,
    count: i32,
) -> u64 {
    let container = Inv::of(player).get(st).expect("container");
    seed_into(server, &container, st, template_id, count)
}

/// [`seed_owned_item`] into a container that is not one of `Inv`'s slots (a house coffer).
fn seed_into(
    server: &GameServer,
    container: &ContainerRef,
    st: u8,
    template_id: u32,
    count: i32,
) -> u64 {
    let id = u64::from(server.ids.item.get_next_id().unwrap());
    let template = server
        .game_data
        .items
        .template(template_id)
        .expect("item template");
    let mut item = Item::create(id, template, count, 0, 1, &server.game_data.items, 0);
    let mut c = container.lock().expect("container poisoned");
    item.slot_type = st;
    item.slot = c.get_unused_slot(-1);
    item.owner_id = u64::from(c.owner_id);
    c.items.push(item);
    c.update_free_slot_count();
    id
}

/// `AddOrMoveExistingItem` from one container into another.
fn move_item_between(
    server: &GameServer,
    from: &ContainerRef,
    to: &ContainerRef,
    item_id: u64,
    preferred_slot: i32,
) -> bool {
    let mut txn = ItemTxn::begin(server);
    let ok = {
        let (mut target, mut source) = lock_pair(to, from);
        match source.as_mut() {
            Some(source) => target.move_item_from(
                source,
                task_type::INVALID,
                item_id,
                preferred_slot,
                &server.game_data.items,
                &mut txn.env.fx,
            ),
            None => false,
        }
    };
    drop(txn);
    ok
}

/// [`move_item_between`] between two of the character's own containers.
fn move_between(
    server: &GameServer,
    player: &PlayerRef,
    from: u8,
    to: u8,
    item_id: u64,
    preferred_slot: i32,
) -> bool {
    let inv = Inv::of(player);
    let from = inv.get(from).expect("source container");
    let to = inv.get(to).expect("target container");
    move_item_between(server, &from, &to, item_id, preferred_slot)
}

#[tokio::test]
async fn a_new_item_credits_get_item_type_and_completes_the_record() {
    let server = acquisition_server(3);
    let (mut client, player) = player_of(&server);

    assert!(acquire_items(&server, &player, GET_ITEM, 3));

    assert_eq!(
        achievement_ops(client.drain_ops()),
        [
            sc::SCAchievementChangedPacket,
            sc::SCAchievementCompletedPacket
        ]
    );
    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        3
    );
    assert!(player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn a_loot_pack_grant_credits_get_item_type() {
    let server = acquisition_server(2);
    let (_client, player) = player_of(&server);
    let pack = LootPack {
        id: 77,
        ..Default::default()
    };

    let mut txn = ItemTxn::begin(&server);
    let given = give_loot_pack(
        &mut txn,
        &player,
        &pack,
        0,
        task_type::LOOT,
        Some(vec![GeneratedLoot {
            item_id: GET_ITEM,
            count: 2,
            grade: 0,
            group: 0,
        }]),
        None,
        &mut ScriptedRng::default(),
    );
    drop(txn);

    assert!(given);
    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        2
    );
    assert!(player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn a_template_without_a_matching_get_item_record_does_not_credit() {
    let server = acquisition_server(2);
    let (mut client, player) = player_of(&server);

    assert!(acquire_items(&server, &player, OTHER_ITEM, 5));
    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        0
    );
    assert!(achievement_ops(client.drain_ops()).is_empty());

    assert!(acquire_items(&server, &player, GET_ITEM, 2));
    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        2
    );
    assert!(player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn new_units_merged_into_an_owned_stack_credit_only_the_delta() {
    let server = acquisition_server(8);
    let (mut client, player) = player_of(&server);
    seed_owned_item(&server, &player, slot_type::INVENTORY, GET_ITEM, 5);

    assert!(acquire_items(&server, &player, GET_ITEM, 3));

    // The quest event carries the new total (8); the achievement counts the 3 new units.
    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        3
    );
    assert!(!player.data().achievement_state.is_complete(1));
    assert_eq!(
        achievement_ops(client.drain_ops()),
        [sc::SCAchievementChangedPacket]
    );
}

#[tokio::test]
async fn moving_between_the_characters_own_containers_does_not_credit() {
    let server = acquisition_server(1);
    let (_client, player) = player_of(&server);
    let id = seed_owned_item(&server, &player, slot_type::INVENTORY, GET_ITEM, 4);

    assert!(move_between(
        &server,
        &player,
        slot_type::INVENTORY,
        slot_type::BANK,
        id,
        -1
    ));
    assert!(move_between(
        &server,
        &player,
        slot_type::BANK,
        slot_type::INVENTORY,
        id,
        -1
    ));

    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        0
    );
    assert!(!player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn taking_an_item_out_of_mail_credits_get_item_type() {
    let server = acquisition_server(2);
    let (_client, player) = player_of(&server);
    let id = seed_owned_item(&server, &player, slot_type::MAIL, GET_ITEM, 2);

    assert!(move_between(
        &server,
        &player,
        slot_type::MAIL,
        slot_type::INVENTORY,
        id,
        -1
    ));

    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        2
    );
    assert!(player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn merging_an_owned_stack_into_another_counts_nothing() {
    let server = acquisition_server(1);
    let (_client, player) = player_of(&server);
    seed_owned_item(&server, &player, slot_type::INVENTORY, GET_ITEM, 5);
    let bank_id = seed_owned_item(&server, &player, slot_type::BANK, GET_ITEM, 4);

    // Slot 0 holds the existing stack, so this is a merge, not a new slot.
    assert!(move_between(
        &server,
        &player,
        slot_type::BANK,
        slot_type::INVENTORY,
        bank_id,
        0
    ));

    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        0
    );
    assert!(!player.data().achievement_state.is_complete(1));
}

#[tokio::test]
async fn receiving_an_item_from_another_character_credits_get_item_type() {
    let server = acquisition_server(2);
    let (mut client, player) = player_of(&server);
    let mut giver_client = TestClient::new(&server, 2, 1002);
    let giver = spawn_player(
        &server,
        &giver_client,
        test_character(2, "Giver", 101.0, 100.0),
        0x101,
    );
    let source = Inv::of(&giver).bag();
    let target = Inv::of(&player).bag();
    let id = seed_into(&server, &source, slot_type::INVENTORY, GET_ITEM, 2);

    assert!(move_item_between(&server, &source, &target, id, -1));

    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        2
    );
    assert!(player.data().achievement_state.is_complete(1));
    let _ = giver_client.drain();
    client.drain();
}

#[tokio::test]
async fn a_house_coffer_withdrawal_does_not_credit() {
    let server = acquisition_server(1);
    let (_client, player) = player_of(&server);
    // A coffer is a `Coffer` container whose slot type is TRADE; it is the character's own
    // storage, not a trade with another character.
    let coffer = ItemContainer::new(ContainerKind::Coffer, 2, slot_type::TRADE, 999).into_ref();
    let bag = Inv::of(&player).bag();
    let id = seed_into(&server, &coffer, slot_type::TRADE, GET_ITEM, 4);

    assert!(move_item_between(&server, &coffer, &bag, id, -1));

    assert_eq!(
        player
            .data()
            .achievement_state
            .record_value(GET_ITEM_RECORD),
        0
    );
    assert!(!player.data().achievement_state.is_complete(1));
}
