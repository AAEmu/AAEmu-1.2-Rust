//! Item lock (`CSItemSecure` / `CSItemUnsecure` / `CSEquipmentsSecure` /
//! `CSEquipmentsUnsecure`): the flags the client sees, the unlock delay, and the item paths that
//! refuse a locked item.

use std::sync::Arc;

use aaemu_commons::{Marshal, PacketStream};

use crate::data::GameData;
use crate::models::container::{equip_slot, slot_type, ContainerKind, ItemContainer};
use crate::models::item::{Item, ITEM_FLAG_SECURE};
use crate::models::item_task::{task_type, ItemTask};
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::{self, Inv, ItemTxn};
use crate::systems::economy::item_effects::{self, Outcome};
use crate::systems::economy::item_handlers::*;
use crate::systems::economy::item_security;
use crate::systems::economy::item_timers;
use crate::systems::economy::loot::SharedRandom;
use crate::systems::economy::mail::{mail_result, mail_type};
use crate::systems::economy::mail_character;
use crate::systems::economy::tests::{
    give, item_at, run, spawn_service_npc, task_body, test_data, World, ARMOR, MERCHANT_TEMPLATE,
    SINGLE, STACK,
};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

const SECURE_CATEGORY: i32 = 100;

fn security_data() -> GameData {
    let mut d = test_data();
    d.systems
        .economy
        .items
        .secure_categories
        .insert(SECURE_CATEGORY);
    d.systems.economy.items.security_unlock_delay_minutes = 1;
    for t in d.items.templates.values_mut() {
        t.category_id = SECURE_CATEGORY;
    }
    d.items
        .templates
        .get_mut(&STACK)
        .expect("stack template")
        .disenchantable = true;
    d
}

fn world_with(data: GameData) -> World {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    // `UnitBase` defaults to `hp = 0` (dead, and the trade state check refuses a dead player):
    // the fixture players are alive, as in `economy::tests::world_with`.
    let mut alice = test_character(1, "Alice", 100.0, 100.0);
    alice.unit.hp = 100;
    alice.unit.max_hp = 100;
    let mut bob = test_character(2, "Bob", 101.0, 100.0);
    bob.unit.hp = 100;
    bob.unit.max_hp = 100;
    let pa = spawn_player(&server, &a, alice, 0x100);
    let pb = spawn_player(&server, &b, bob, 0x101);
    for p in [&pa, &pb] {
        for (_, c) in Inv::of(p).containers {
            server.items.add_container(c);
        }
    }
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    a.drain();
    b.drain();
    World {
        server,
        a,
        pa,
        b,
        pb,
    }
}

fn world() -> World {
    world_with(security_data())
}

fn secure_body(slot: u8, id: u64) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u8(0)
        .write_u8(slot_type::INVENTORY)
        .write_u8(0)
        .write_u8(slot)
        .write_u64(id);
    s.into_bytes()
}

fn lock(w: &mut World, which: u8, slot: u8, id: u64) {
    run::<CSItemSecurePacket>(w, which, &secure_body(slot, id));
}

fn error_frame(e: i16) -> Vec<u8> {
    let mut s = PacketStream::new();
    crate::packets::world::SCErrorMsgPacket::new(e, 0, true).write(&mut s);
    s.into_bytes()
}

fn item(w: &World, id: u64) -> Option<Item> {
    Inv::of(&w.pa).item_by_id(id)
}

#[tokio::test]
async fn secure_sets_the_flag_and_marks_the_item_for_save() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    w.a.drain();
    lock(&mut w, 0, 0, id);
    let it = item(&w, id).unwrap();
    assert!(it.has_flag(ITEM_FLAG_SECURE));
    assert_eq!(it.unsecure_time, 0);
    assert!(w.server.items.prepare_save().item_ids().contains(&id));
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].1,
        task_body(
            task_type::ITEM_LOCK,
            vec![ItemTask::update_security(
                &it, it.flags, false, false, false
            )]
        )
    );
}

#[tokio::test]
async fn non_lockable_category_and_exception_items_are_refused() {
    let mut d = security_data();
    d.systems.economy.items.secure_categories.clear();
    let mut w = world_with(d);
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    w.a.drain();
    lock(&mut w, 0, 0, id);
    assert!(!item(&w, id).unwrap().has_flag(ITEM_FLAG_SECURE));
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, crate::opcodes::sc::SCErrorMsgPacket);
    assert_eq!(
        frames[0].1,
        error_frame(error_message::ITEM_SECURE_CONDITION)
    );

    let mut d = security_data();
    d.systems.economy.items.secure_exceptions.insert(STACK);
    let mut w = world_with(d);
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    w.a.drain();
    lock(&mut w, 0, 0, id);
    assert!(!item(&w, id).unwrap().has_flag(ITEM_FLAG_SECURE));
}

#[tokio::test]
async fn unsecure_keeps_the_item_locked_until_the_delay_elapses() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    w.a.drain();
    lock(&mut w, 0, 0, id);
    w.a.drain();

    run::<CSItemUnsecurePacket>(&mut w, 0, &secure_body(0, id));
    let it = item(&w, id).unwrap();
    assert!(
        it.has_flag(ITEM_FLAG_SECURE),
        "still locked during the delay"
    );
    assert!(it.unsecure_time > 0);
    let unsecure_time = it.unsecure_time;
    // content_configs[43] is minutes: the fixture's 1 means a 60 s deadline.
    let now = chrono::Utc::now().timestamp();
    assert!(
        (unsecure_time - now - 60).abs() <= 1,
        "deadline {unsecure_time} is not now+60 s (now {now})"
    );
    let frames = w.a.drain();
    assert_eq!(
        frames[0].1,
        task_body(
            task_type::ITEM_UNLOCK,
            vec![ItemTask::update_security(
                &it,
                it.flags,
                false,
                true,
                it.has_flag(crate::models::item::ITEM_FLAG_UNPACKED)
            )]
        )
    );

    // One second before the deadline nothing happens.
    item_timers::update_item_timers(&w.server, (unsecure_time - 1) * 1000);
    assert!(item(&w, id).unwrap().has_flag(ITEM_FLAG_SECURE));
    assert!(w.a.drain().is_empty());

    // At the deadline the flag is cleared and the client is told (`ITEM_UNLOCK_EXCESS`).
    item_timers::update_item_timers(&w.server, unsecure_time * 1000);
    let it = item(&w, id).unwrap();
    assert!(!it.has_flag(ITEM_FLAG_SECURE));
    assert_eq!(it.unsecure_time, 0);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].1,
        task_body(
            task_type::ITEM_UNLOCK_EXCESS,
            vec![ItemTask::update_security(&it, it.flags, true, false, false)]
        )
    );
}

#[tokio::test]
async fn equipments_secure_and_unsecure_use_the_same_delay() {
    let mut w = world();
    let armor = give(
        &w,
        &w.pa,
        slot_type::EQUIPMENT,
        ARMOR,
        1,
        i32::from(equip_slot::CHEST as u8),
    );
    w.a.drain();
    run::<CSEquipmentsSecurePacket>(&mut w, 0, &[]);
    assert!(item(&w, armor).unwrap().has_flag(ITEM_FLAG_SECURE));
    assert_eq!(w.a.drain().len(), 1);

    // Everything is already locked: the client's "all equipments are secured" error.
    run::<CSEquipmentsSecurePacket>(&mut w, 0, &[]);
    assert_eq!(w.a.drain_ops(), vec![crate::opcodes::sc::SCErrorMsgPacket]);

    run::<CSEquipmentsUnsecurePacket>(&mut w, 0, &[]);
    assert!(item(&w, armor).unwrap().unsecure_time > 0);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
}

#[tokio::test]
async fn sell_refuses_a_locked_item() {
    let mut w = world();
    spawn_service_npc(&mut w, 0x300, MERCHANT_TEMPLATE, 101.0);
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    lock(&mut w, 0, 0, id);
    w.a.drain();

    let mut s = PacketStream::new();
    s.write_bc(0x300)
        .write_bc(0)
        .write_u8(1)
        .write_u8(slot_type::INVENTORY)
        .write_u8(0)
        .write_u64(id)
        .write_u32(0);
    run::<crate::systems::economy::store::CSSellItemsPacket>(&mut w, 0, s.as_bytes());
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, crate::opcodes::sc::SCErrorMsgPacket);
    assert_eq!(
        frames[0].1,
        error_frame(error_message::ITEM_SECURE_CONDITION)
    );
    assert!(item(&w, id).is_some());
}

#[tokio::test]
async fn trade_refuses_a_locked_item() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    lock(&mut w, 0, 0, id);
    w.a.drain();
    w.b.drain();
    // Start Alice -> Bob (as `trade_tests::start`).
    let mut s = PacketStream::new();
    s.write_bc(0x101);
    run::<crate::systems::economy::trade::CSCanStartTradePacket>(&mut w, 0, s.as_bytes());
    let mut s = PacketStream::new();
    s.write_bc(0x100);
    run::<crate::systems::economy::trade::CSStartTradePacket>(&mut w, 1, s.as_bytes());
    w.a.drain();
    w.b.drain();
    assert_eq!(
        w.server.systems.economy.trade.trades().len(),
        1,
        "the trade did not start; the put-up assertion below would be vacuous"
    );

    let mut s = PacketStream::new();
    s.write_u8(slot_type::INVENTORY).write_u8(0).write_i32(1);
    run::<crate::systems::economy::trade::CSPutupTradeItemPacket>(&mut w, 0, s.as_bytes());
    let ops = w.a.drain_ops();
    assert!(ops.contains(&crate::opcodes::sc::SCErrorMsgPacket));
    assert!(!ops.contains(&crate::opcodes::sc::SCTradeItemPutupPacket));
}

#[tokio::test]
async fn mail_refuses_a_locked_attachment() {
    let w = world();
    w.server.names.add_character(1, "Alice", 1001);
    w.server.names.add_character(2, "Bob", 1002);
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    item_security::secure_item(&w.server, &w.pa, slot_type::INVENTORY, 0, id);
    let mut txn = ItemTxn::begin(&w.server);
    let result = mail_character::send_mail_to_player(
        &mut txn,
        &w.pa,
        mail_type::NORMAL,
        "Bob",
        "title",
        "text",
        1,
        0,
        0,
        0,
        0,
        &[(slot_type::INVENTORY, 0)],
    );
    assert_eq!(result, mail_result::ITEM_LOCKED);
}

#[tokio::test]
async fn auction_refuses_a_locked_item() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    lock(&mut w, 0, 0, id);
    w.a.drain();
    w.server
        .systems
        .economy
        .auction
        .post_lot_on_auction(&w.server, &w.pa, id, 10, 100, 0);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, crate::opcodes::sc::SCErrorMsgPacket);
    assert_eq!(
        frames[0].1,
        error_frame(error_message::ITEM_SECURE_CONDITION)
    );
    assert!(item(&w, id).is_some());
    assert!(w
        .server
        .items
        .item_by_id(id)
        .is_some_and(|i| i.slot_type == slot_type::INVENTORY));
}

#[tokio::test]
async fn destroy_refuses_a_locked_item() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    lock(&mut w, 0, 0, id);
    w.a.drain();
    let mut s = PacketStream::new();
    s.write_u64(id)
        .write_u8(slot_type::INVENTORY)
        .write_u8(0)
        .write_i32(5);
    run::<CSDestroyItemPacket>(&mut w, 0, s.as_bytes());
    assert!(item(&w, id).is_some());
    let frames = w.a.drain();
    assert_eq!(frames[0].0, crate::opcodes::sc::SCErrorMsgPacket);
}

#[tokio::test]
async fn convert_item_look_refuses_a_locked_item() {
    let mut w = world();
    let base = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
    let look = give(&w, &w.pa, slot_type::INVENTORY, SINGLE, 1, 2);
    lock(&mut w, 0, 1, base);
    w.a.drain();
    crate::systems::economy::store::convert_item_look(&w.server, &w.pa, base, look);
    assert!(item(&w, base).is_some());
    assert!(item(&w, look).is_some());
    let frames = w.a.drain();
    assert_eq!(frames[0].0, crate::opcodes::sc::SCErrorMsgPacket);
    assert_eq!(
        frames[0].1,
        error_frame(error_message::ITEM_SECURE_CONDITION)
    );
}

#[tokio::test]
async fn disenchant_refuses_a_locked_item() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    lock(&mut w, 0, 0, id);
    w.a.drain();
    let outcome =
        item_effects::item_conversion(&w.server, &w.pa, Some(id), [0; 4], &mut SharedRandom);
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(item(&w, id).is_some());
    let frames = w.a.drain();
    assert_eq!(frames[0].0, crate::opcodes::sc::SCErrorMsgPacket);
}

#[tokio::test]
async fn split_and_merge_refuse_a_locked_stack() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, STACK, 10, 0);
    lock(&mut w, 0, 0, source);
    w.a.drain();

    let mut s = PacketStream::new();
    s.write_u64(source)
        .write_u64(0)
        .write_u8(slot_type::INVENTORY)
        .write_u8(0)
        .write_u8(slot_type::INVENTORY)
        .write_u8(1)
        .write_i32(3);
    run::<CSSplitBagItemPacket>(&mut w, 0, s.as_bytes());
    let frames = w.a.drain();
    assert_eq!(frames[0].0, crate::opcodes::sc::SCErrorMsgPacket);
    assert_eq!(
        frames[0].1,
        error_frame(error_message::ITEM_SECURE_CONDITION)
    );
    assert_eq!(item(&w, source).unwrap().count, 10);
    assert!(item_at(&w.pa, slot_type::INVENTORY, 1).is_none());

    let target = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 2);
    let mut s = PacketStream::new();
    s.write_u64(source)
        .write_u64(target)
        .write_u8(slot_type::INVENTORY)
        .write_u8(0)
        .write_u8(slot_type::INVENTORY)
        .write_u8(2);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    let frames = w.a.drain();
    assert!(frames
        .iter()
        .any(|(op, body)| *op == crate::opcodes::sc::SCErrorMsgPacket
            && *body == error_frame(error_message::ITEM_SECURE_CONDITION)));
    assert_eq!(item(&w, source).unwrap().count, 10);
    assert_eq!(item(&w, target).unwrap().count, 5);
}

#[tokio::test]
async fn coffer_move_refuses_a_locked_stack() {
    let w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    item_security::secure_item(&w.server, &w.pa, slot_type::INVENTORY, 0, id);
    let coffer = ItemContainer::new(ContainerKind::Coffer, 1, slot_type::TRADE, 999).into_ref();
    let inv = Inv::of(&w.pa);
    let mut txn = ItemTxn::begin(&w.server);
    assert!(!inventory::split_or_move_item_ex(
        &mut txn,
        &inv,
        task_type::SWAP_COFFER_ITEMS,
        Some(inv.bag()),
        Some(coffer.clone()),
        id,
        slot_type::INVENTORY,
        0,
        0,
        slot_type::TRADE,
        0,
        1,
    ));
    drop(txn);
    assert!(item(&w, id).is_some());
    assert!(coffer.lock().expect("coffer").items.is_empty());
}

#[tokio::test]
async fn consume_item_skips_a_locked_stack() {
    let w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    item_security::secure_item(&w.server, &w.pa, slot_type::INVENTORY, 0, id);
    let inv = Inv::of(&w.pa);
    {
        let mut txn = ItemTxn::begin(&w.server);
        let used = inv.bag().lock().expect("bag").consume_item(
            task_type::SKILL_REAGENTS,
            STACK,
            3,
            None,
            &mut txn.env,
        );
        assert_eq!(used, 0);
    }
    assert_eq!(item(&w, id).unwrap().count, 5);

    item_security::unsecure_item(&w.server, &w.pa, slot_type::INVENTORY, 0, id);
    let expiry = item(&w, id).unwrap().unsecure_time;
    item_timers::update_item_timers(&w.server, expiry * 1000);
    {
        let mut txn = ItemTxn::begin(&w.server);
        let used = inv.bag().lock().expect("bag").consume_item(
            task_type::SKILL_REAGENTS,
            STACK,
            3,
            None,
            &mut txn.env,
        );
        assert_eq!(used, 3);
    }
    assert_eq!(item(&w, id).unwrap().count, 2);
}

#[tokio::test]
async fn locked_reagent_reports_item_locked() {
    let mut d = security_data();
    d.systems.skills.skill_reagents = vec![crate::systems::skills::data::SkillItemAmount {
        id: 1,
        skill_id: 100,
        item_id: STACK,
        amount: 1,
    }];
    let w = world_with(d);
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 1, 0);
    item_security::secure_item(&w.server, &w.pa, slot_type::INVENTORY, 0, id);
    let template = crate::systems::skills::tests::skill_template(100);
    let result = crate::systems::skills::validation::check_reagents(
        &w.server,
        &template,
        &crate::world::WorldObject::from(&w.pa),
    );
    assert_eq!(
        result,
        Err((
            crate::systems::skills::types::skill_result::ITEM_LOCKED,
            STACK
        ))
    );
}

/// Byte-exact `ItemUpdateSecurity` body (`SCItemTaskSuccessPacket`), checked against the
/// client's reader fields (`FUN_397cfe40`).
#[test]
fn item_update_security_body_is_byte_exact() {
    let mut item = Item::new(1);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 3;
    item.id = 0x1122_3344_5566_7788;
    item.template_id = STACK;
    item.flags = 0x04 | 0x10;
    item.unsecure_time = 0x0102_0304;
    item.unpack_time = 0x0506_0708;
    let task = ItemTask::update_security(&item, item.flags, true, false, true);
    let mut s = PacketStream::new();
    task.write(&mut s);
    let mut want = Vec::new();
    want.push(0x0b); // ItemUpdateSecurity task type (`ItemTask::action`)
    want.push(slot_type::INVENTORY);
    want.push(3);
    want.extend_from_slice(&item.id.to_le_bytes());
    want.push(item.flags);
    want.extend_from_slice(&[1, 0, 1]);
    want.extend_from_slice(&item.unsecure_time.to_le_bytes());
    want.extend_from_slice(&item.unpack_time.to_le_bytes());
    assert_eq!(s.into_bytes(), want);
}
