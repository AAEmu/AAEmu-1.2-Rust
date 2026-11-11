//! Flow tests of `items.limited_sale_count` / `items.one_time_sale`: the NPC store purchase
//! (`CSBuyItemsPacket`) and the doodad purchase (`DoodadFuncPurchase` through
//! `CSBuyCoinItemPacket`), plus the real-data and MySQL persistence checks.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::items::ItemTemplate;
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::models::doodad::Doodad;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::packets::world::SCErrorMsgPacket;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::store::{CSBuyCoinItemPacket, CSBuyItemsPacket, CSSellItemsPacket};
use crate::systems::economy::tests::{give, run, test_data, World, STACK};
use crate::systems::npcs::doodad::templates::{DoodadFunc, DoodadFuncPurchase, FuncTemplate};
use crate::systems::npcs::npc::data::{MerchantGoods, NpcTemplate};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{DoodadRef, Entity, NpcRef};

const MERCHANT_NPC: u32 = 0x500;
const DOODAD_OBJ: u32 = 0x400;
const PURCHASE_COIN: u32 = 9000;
const PURCHASE_BACKPACK: u32 = 9100;
const PURCHASE_FUNC_KEY: u32 = 1;
const PURCHASE_TEMPLATE: u32 = 600;

fn world_with_data(data: GameData) -> World {
    world_with_data_for_accounts(data, 1001, 1002)
}

fn world_with_data_for_accounts(data: GameData, account_a: u32, account_b: u32) -> World {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    let mut a = TestClient::new(&server, 1, account_a);
    let mut b = TestClient::new(&server, 2, account_b);
    let mut ca = test_character(1, "Alice", 100.0, 100.0);
    ca.account_id = account_a;
    let mut cb = test_character(2, "Bob", 101.0, 100.0);
    cb.account_id = account_b;
    let pa = spawn_player(&server, &a, ca, 0x100);
    let pb = spawn_player(&server, &b, cb, 0x101);
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

fn spawn_npc(w: &mut World, obj_id: u32, template: u32, x: f32) -> NpcRef {
    let mut n = Npc::new(template);
    n.unit.transform.set_xyz(x, 100.0, 100.0);
    let n = Entity::new(obj_id, n);
    let world = w.server.world.world(0).unwrap();
    world.spawn(&n, &w.server.view_ctx());
    w.a.drain();
    w.b.drain();
    n
}

/// A merchant pack 7 NPC that sells `STACK` at grade 0, with the real NPC catalog bridge.
fn merchant_world(limited_sale_count: i32, one_time_sale: bool) -> World {
    merchant_world_with_accounts(limited_sale_count, one_time_sale, 1001, 1002)
}

fn merchant_world_with_accounts(
    limited_sale_count: i32,
    one_time_sale: bool,
    account_a: u32,
    account_b: u32,
) -> World {
    let mut d = test_data();
    d.systems.economy.store.refund_multipliers.insert(0, 100);
    let t = d.items.templates.get_mut(&STACK).unwrap();
    t.limited_sale_count = limited_sale_count;
    t.one_time_sale = one_time_sale;
    let mut goods = MerchantGoods {
        id: 7,
        items: Vec::new(),
    };
    goods.add_item_to_stock(STACK, 0);
    d.systems.npcs.npc.goods.insert(7, goods);
    d.systems.npcs.npc.templates.insert(
        1234,
        NpcTemplate {
            id: 1234,
            merchant: true,
            merchant_pack_ids: vec![7],
            ..Default::default()
        },
    );
    let mut w = world_with_data_for_accounts(d, account_a, account_b);
    crate::systems::npcs::bridges::install_economy(&w.server);
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    w.pa.data().money = 1000;
    w.pb.data().money = 1000;
    w
}

fn buy_body(npc: u32, buy: &[(u32, i32)], buy_back: &[i32]) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(npc)
        .write_bc(0)
        .write_u32(0)
        .write_u8(buy.len() as u8)
        .write_u8(buy_back.len() as u8);
    for (id, count) in buy {
        s.write_u32(*id).write_u8(0).write_i32(*count).write_u8(0);
    }
    for i in buy_back {
        s.write_i32(*i);
    }
    s.write_bool(false);
    s.into_bytes()
}

fn sell_body(npc: u32, entries: &[(u8, u8, u64)]) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(npc).write_bc(0).write_u8(entries.len() as u8);
    for (slot_type, slot, id) in entries {
        s.write_u8(*slot_type)
            .write_u8(*slot)
            .write_u64(*id)
            .write_u32(0);
    }
    s.into_bytes()
}

fn error_code(w: &mut World, which: u8) -> Option<i16> {
    let frames = if which == 0 { w.a.drain() } else { w.b.drain() };
    frames.iter().find_map(|(opcode, body)| {
        (*opcode == sc::SCErrorMsgPacket).then(|| {
            let mut s = PacketStream::from_bytes(body.clone());
            let p = SCErrorMsgPacket {
                error1: s.read_i16().unwrap(),
                error2: s.read_i16().unwrap(),
                kind: s.read_u32().unwrap(),
                is_notify: s.read_bool().unwrap(),
            };
            p.error1
        })
    })
}

fn purchased(w: &World, account_id: u32) -> i32 {
    w.server
        .systems
        .economy
        .shop_limits
        .purchased(account_id, STACK)
}

#[tokio::test]
async fn limited_sale_count_refuses_the_purchase_that_would_cross_it() {
    let mut w = merchant_world(2, false);
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 2);
    assert_eq!(w.pa.data().money, 980);
    assert_eq!(purchased(&w, 1001), 2);
    assert_eq!(error_code(&mut w, 0), None);

    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 2);
    assert_eq!(w.pa.data().money, 980);
    assert_eq!(purchased(&w, 1001), 2);
    assert_eq!(error_code(&mut w, 0), Some(752));

    // The other account has its own allowance.
    run::<CSBuyItemsPacket>(&mut w, 1, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(Inv::of(&w.pb).items_count(STACK, -1), 1);
    assert_eq!(purchased(&w, 1002), 1);
    assert!(error_code(&mut w, 1).is_none());
}

#[tokio::test]
async fn a_second_character_of_the_same_account_cannot_exceed_the_limit() {
    let mut w = merchant_world_with_accounts(1, false, 1001, 1001);
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(purchased(&w, 1001), 1);
    assert_eq!(error_code(&mut w, 0), None);

    // Bob is on the same account: the account's allowance is already spent.
    run::<CSBuyItemsPacket>(&mut w, 1, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(Inv::of(&w.pb).items_count(STACK, -1), 0);
    assert_eq!(w.pb.data().money, 1000);
    assert_eq!(purchased(&w, 1001), 1);
    assert_eq!(error_code(&mut w, 1), Some(752));
}

#[tokio::test]
async fn a_single_packet_cannot_cross_the_limit_with_several_lines() {
    let mut w = merchant_world(3, false);
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 2)], &[]));
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 2);
    assert_eq!(purchased(&w, 1001), 2);

    // Two lines of the same template add up to one claim.
    run::<CSBuyItemsPacket>(
        &mut w,
        0,
        &buy_body(MERCHANT_NPC, &[(STACK, 2), (STACK, 2)], &[]),
    );
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 2);
    assert_eq!(w.pa.data().money, 980);
    assert_eq!(purchased(&w, 1001), 2);
    assert_eq!(error_code(&mut w, 0), Some(752));
}

#[tokio::test]
async fn one_time_sale_allows_exactly_one_purchase() {
    let mut w = merchant_world(0, true);
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(w.pa.data().money, 990);
    assert_eq!(error_code(&mut w, 0), None);

    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(w.pa.data().money, 990);
    assert_eq!(purchased(&w, 1001), 1);
    assert_eq!(error_code(&mut w, 0), Some(752));
}

#[tokio::test]
async fn giving_the_item_is_not_a_purchase_and_selling_back_does_not_reset() {
    let mut w = merchant_world(2, false);
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    w.a.drain();

    // GM give / loot / quest grants do not pass the purchase path.
    give(&w, &w.pa, slot_type::INVENTORY, STACK, 1, 5);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 3);
    assert_eq!(purchased(&w, 1001), 2);

    // Sell one back and buy it back: the allowance is still spent.
    let sold =
        w.a.conn
            .active_char
            .as_ref()
            .and_then(|p| {
                let bag = Inv::of(p).bag();
                let bag = bag.lock().unwrap();
                bag.items
                    .iter()
                    .find(|i| i.template_id == STACK)
                    .map(|i| (i.slot_type, i.slot as u8, i.id))
            })
            .unwrap();
    run::<CSSellItemsPacket>(
        &mut w,
        0,
        &sell_body(MERCHANT_NPC, &[(sold.0, sold.1, sold.2)]),
    );
    assert_eq!(purchased(&w, 1001), 2);
    assert_eq!(
        w.server
            .systems
            .economy
            .store
            .buyback(1)
            .lock()
            .unwrap()
            .items
            .len(),
        1
    );

    let buyback_slot = w
        .server
        .systems
        .economy
        .store
        .buyback(1)
        .lock()
        .unwrap()
        .items[0]
        .slot;
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[], &[buyback_slot]));
    assert_eq!(purchased(&w, 1001), 2);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 3);
    assert_eq!(error_code(&mut w, 0), None);

    // A new purchase is still refused: the buy-back did not give the allowance back.
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1)], &[]));
    assert_eq!(purchased(&w, 1001), 2);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 3);
    assert_eq!(error_code(&mut w, 0), Some(752));
}

fn purchase_doodad_world(limit: i32, coin_count: i32) -> World {
    purchase_doodad_world_with(limit, PURCHASE_COIN, coin_count, 500, "DoodadFuncPurchase")
}

fn purchase_doodad_world_with(
    limit: i32,
    coin_item_id: u32,
    coin_count: i32,
    group_id: u32,
    func_type: &'static str,
) -> World {
    purchase_doodad_world_full(
        limit,
        coin_item_id,
        coin_count,
        group_id,
        func_type,
        STACK,
        1,
        -1,
    )
}

#[allow(clippy::too_many_arguments)]
fn purchase_doodad_world_full(
    limit: i32,
    coin_item_id: u32,
    coin_count: i32,
    group_id: u32,
    func_type: &'static str,
    item_id: u32,
    item_count: i32,
    next_phase: i32,
) -> World {
    let mut d = test_data();
    if item_id == PURCHASE_BACKPACK {
        d.items.templates.insert(
            PURCHASE_BACKPACK,
            ItemTemplate {
                max_count: 1,
                ..ItemTemplate::new(
                    PURCHASE_BACKPACK,
                    crate::data::items::TemplateKind::Backpack { backpack_type: 1 },
                )
            },
        );
    }
    if let Some(t) = d.items.templates.get_mut(&item_id) {
        t.limited_sale_count = limit;
    }
    d.items
        .templates
        .insert(PURCHASE_COIN, ItemTemplate::plain(PURCHASE_COIN, 1000));
    d.systems.npcs.doodad.funcs_by_id.insert(
        PURCHASE_FUNC_KEY,
        DoodadFunc {
            group_id,
            func_id: PURCHASE_TEMPLATE,
            func_key: PURCHASE_FUNC_KEY,
            func_type: func_type.into(),
            next_phase,
            sound_id: 0,
            skill_id: 0,
            perm_id: 0,
            count: 0,
        },
    );
    d.systems
        .npcs
        .doodad
        .func_templates
        .entry(func_type)
        .or_default()
        .insert(
            PURCHASE_TEMPLATE,
            FuncTemplate::Purchase(DoodadFuncPurchase {
                id: PURCHASE_TEMPLATE,
                item_id,
                count: item_count,
                coin_item_id,
                coin_count,
                currency_id: 0,
            }),
        );
    let w = world_with_data(d);
    crate::systems::npcs::bridges::install_economy(&w.server);
    crate::systems::economy::bridges::install(&w.server);
    crate::systems::economy::wiring::install(&w.server);
    w
}

fn spawn_doodad(w: &mut World) -> DoodadRef {
    spawn_doodad_at(w, 500, 100.0)
}

fn spawn_doodad_at(w: &mut World, group_id: u32, x: f32) -> DoodadRef {
    let mut d = Doodad::new(4321);
    d.func_group_id = group_id;
    d.transform.set_xyz(x, 100.0, 100.0);
    let d = Entity::new(DOODAD_OBJ, d);
    let world = w.server.world.world(0).unwrap();
    world.spawn(&d, &w.server.view_ctx());
    w.a.drain();
    d
}

fn buy_coin(w: &mut World) -> i16 {
    buy_coin_as(w, 0)
}

fn buy_coin_as(w: &mut World, which: u8) -> i16 {
    let mut s = PacketStream::new();
    s.write_bc(DOODAD_OBJ).write_u32(PURCHASE_FUNC_KEY);
    run::<CSBuyCoinItemPacket>(w, which, s.as_bytes());
    error_code(w, which).unwrap_or(0)
}

fn buy_doodad(w: &mut World, unk_id: u32, item_id: u32, count: i32) -> i16 {
    buy_doodad_at_grade(w, unk_id, item_id, 0, count)
}

fn buy_doodad_at_grade(w: &mut World, unk_id: u32, item_id: u32, grade: u8, count: i32) -> i16 {
    let mut s = PacketStream::new();
    s.write_bc(0)
        .write_bc(DOODAD_OBJ)
        .write_u32(unk_id)
        .write_u8(1)
        .write_u8(0)
        .write_u32(item_id)
        .write_u8(grade)
        .write_i32(count)
        .write_u8(0)
        .write_bool(false);
    run::<CSBuyItemsPacket>(w, 0, s.as_bytes());
    error_code(w, 0).unwrap_or(0)
}

#[tokio::test]
async fn doodad_purchase_uses_the_same_per_account_limit() {
    let mut w = purchase_doodad_world(1, 5);
    let doodad = spawn_doodad(&mut w);
    let _ = doodad;
    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);
    let coins = |w: &World| Inv::of(&w.pa).items_count(PURCHASE_COIN, -1);

    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(coins(&w), 5);
    assert_eq!(purchased(&w, 1001), 1);

    assert_eq!(buy_coin(&mut w), 752);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(coins(&w), 5);
    assert_eq!(purchased(&w, 1001), 1);
}

#[tokio::test]
async fn doodad_purchase_shares_the_limit_between_characters_of_one_account() {
    let mut w = purchase_doodad_world(1, 5);
    let account = w.pa.data().account_id;
    w.pb.data().account_id = account;
    let _d = spawn_doodad(&mut w);
    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);
    give(&w, &w.pb, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);

    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(purchased(&w, 1001), 1);

    // Bob is on the same account and is refused (the coin path goes through the economy bridge).
    assert_eq!(buy_coin_as(&mut w, 1), 752);
    assert_eq!(Inv::of(&w.pb).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pb).items_count(PURCHASE_COIN, -1), 10);
    assert_eq!(purchased(&w, 1001), 1);
}

#[tokio::test]
async fn a_failed_doodad_purchase_gives_the_claim_back() {
    let mut w = purchase_doodad_world(1, 5);
    let doodad = spawn_doodad(&mut w);
    let _ = doodad;
    // No coin: the claim is taken and must be released when the consumption fails. The 1.2
    // client's own dialog answers `not_enough_coin` (0x1e3 = 483) for this case.
    assert_eq!(buy_coin(&mut w), 483);
    assert_eq!(purchased(&w, 1001), 0);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);

    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 5, 0);
    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(purchased(&w, 1001), 1);
}

#[test]
fn buy_coin_item_body_is_bc_and_u32() {
    use crate::net::registry::ClientPacket;
    let mut s = PacketStream::new();
    s.write_bc(DOODAD_OBJ).write_u32(PURCHASE_FUNC_KEY);
    let p =
        <CSBuyCoinItemPacket as ClientPacket>::read(&mut PacketStream::from_bytes(s.into_bytes()))
            .unwrap();
    assert_eq!(
        p,
        CSBuyCoinItemPacket {
            obj_id: DOODAD_OBJ,
            id: PURCHASE_FUNC_KEY
        }
    );
}

#[tokio::test]
async fn coin_purchase_requires_the_purchase_func_of_the_current_phase() {
    let mut w = purchase_doodad_world(0, 5);
    // Group 501 is the doodad's current phase; the func belongs to 500.
    let _d = spawn_doodad_at(&mut w, 501, 100.0);
    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);
    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_COIN, -1), 10);
}

#[tokio::test]
async fn coin_purchase_refuses_a_func_that_is_not_a_purchase() {
    let mut w = purchase_doodad_world_with(0, PURCHASE_COIN, 5, 500, "DoodadFuncUse");
    let _d = spawn_doodad(&mut w);
    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);
    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_COIN, -1), 10);
}

#[tokio::test]
async fn coin_purchase_refuses_the_money_form_of_the_func() {
    let mut w = purchase_doodad_world_with(0, 0, 0, 500, "DoodadFuncPurchase");
    let _d = spawn_doodad(&mut w);
    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
}

#[tokio::test]
async fn coin_purchase_refuses_out_of_reach() {
    let mut w = purchase_doodad_world(0, 5);
    // 10 m away: the doodad store checkout range is 3 m.
    let _d = spawn_doodad_at(&mut w, 500, 110.0);
    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);
    assert_eq!(buy_coin(&mut w), 154);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_COIN, -1), 10);
}

#[tokio::test]
async fn a_money_priced_purchase_func_checkout_uses_the_func_id_in_unk() {
    let mut w = purchase_doodad_world_with(0, 0, 0, 500, "DoodadFuncPurchase");
    let _d = spawn_doodad(&mut w);
    w.pa.data().money = 100;
    assert_eq!(buy_doodad(&mut w, PURCHASE_FUNC_KEY, STACK, 1), 0);
    assert_eq!(w.pa.data().money, 90);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
}

#[tokio::test]
async fn a_money_purchase_func_checkout_refuses_another_item_or_func() {
    let mut w = purchase_doodad_world_with(0, 0, 0, 500, "DoodadFuncPurchase");
    let _d = spawn_doodad(&mut w);
    w.pa.data().money = 100;

    // Another template than the func's item is not sold by it.
    assert_eq!(buy_doodad(&mut w, PURCHASE_FUNC_KEY, PURCHASE_COIN, 1), 0);
    // No func id: no merchant catalog and no purchase func, so the checkout is refused.
    assert_eq!(buy_doodad(&mut w, 0, STACK, 1), 0);
    assert_eq!(w.pa.data().money, 100);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
}

#[tokio::test]
async fn a_coin_priced_purchase_func_cannot_check_out_through_buy_items() {
    let mut w = purchase_doodad_world(0, 5);
    let _d = spawn_doodad(&mut w);
    w.pa.data().money = 100;
    assert_eq!(buy_doodad(&mut w, PURCHASE_FUNC_KEY, STACK, 1), 0);
    assert_eq!(w.pa.data().money, 100);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_COIN, -1), 0);
}

#[tokio::test]
async fn a_money_purchase_refuses_a_grade_the_func_does_not_offer() {
    let mut w = purchase_doodad_world_with(0, 0, 0, 500, "DoodadFuncPurchase");
    let _d = spawn_doodad(&mut w);
    w.pa.data().money = 1_000_000;
    // `STACK` has `fixed_grade = -1` and is not gradable, so the func offers basic (0). A
    // forged grade-11 line would be bought at the template price and sold back for the grade-11
    // refund multiplier instead.
    assert_eq!(
        buy_doodad_at_grade(&mut w, PURCHASE_FUNC_KEY, STACK, 11, 1),
        0
    );
    assert_eq!(w.pa.data().money, 1_000_000);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);

    assert_eq!(
        buy_doodad_at_grade(&mut w, PURCHASE_FUNC_KEY, STACK, 0, 1),
        0
    );
    assert_eq!(w.pa.data().money, 999_990);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
}

#[tokio::test]
async fn a_money_purchase_sells_whole_func_bundles() {
    let mut w = purchase_doodad_world_full(0, 0, 0, 500, "DoodadFuncPurchase", STACK, 2, -1);
    let _d = spawn_doodad(&mut w);
    w.pa.data().money = 100;
    // One item is half a bundle.
    assert_eq!(buy_doodad(&mut w, PURCHASE_FUNC_KEY, STACK, 1), 0);
    assert_eq!(w.pa.data().money, 100);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);

    // Two bundles: four items at the per-item template price.
    assert_eq!(buy_doodad(&mut w, PURCHASE_FUNC_KEY, STACK, 4), 0);
    assert_eq!(w.pa.data().money, 60);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 4);
}

#[tokio::test]
async fn a_coin_purchase_advances_the_doodad_to_the_func_next_phase() {
    let mut w = purchase_doodad_world_full(
        0,
        PURCHASE_COIN,
        5,
        500,
        "DoodadFuncPurchase",
        STACK,
        1,
        501,
    );
    let doodad = spawn_doodad(&mut w);

    // A failed purchase does not move the phase.
    assert_eq!(buy_coin(&mut w), 483);
    assert_eq!(doodad.data().func_group_id, 500);

    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);
    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(doodad.data().func_group_id, 501);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_COIN, -1), 5);

    // The func belonged to group 500: a replay after the move is refused.
    assert_eq!(buy_coin(&mut w), 0);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_COIN, -1), 5);
}

#[tokio::test]
async fn a_money_purchase_advances_the_doodad_to_the_func_next_phase() {
    let mut w = purchase_doodad_world_full(0, 0, 0, 500, "DoodadFuncPurchase", STACK, 1, 501);
    let doodad = spawn_doodad(&mut w);
    w.pa.data().money = 100;
    assert_eq!(buy_doodad(&mut w, PURCHASE_FUNC_KEY, STACK, 1), 0);
    assert_eq!(doodad.data().func_group_id, 501);
    assert_eq!(w.pa.data().money, 90);
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
}

#[tokio::test]
async fn a_failed_grant_after_the_coin_was_taken_gives_the_coin_back() {
    // The purchased item is a backpack, so the auto-equip branch runs. A two-item bundle cannot
    // be equipped (one backpack per back slot), so the grant fails after the coin was consumed
    // and the coin must be refunded (`items.rs` `refund_coin`).
    let mut w = purchase_doodad_world_full(
        0,
        PURCHASE_COIN,
        5,
        500,
        "DoodadFuncPurchase",
        PURCHASE_BACKPACK,
        2,
        -1,
    );
    let _d = spawn_doodad(&mut w);
    give(&w, &w.pa, slot_type::INVENTORY, PURCHASE_COIN, 10, 0);
    assert_eq!(buy_coin(&mut w), 315);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_BACKPACK, -1), 0);
    assert_eq!(Inv::of(&w.pa).items_count(PURCHASE_COIN, -1), 10);
    assert_eq!(purchased(&w, 1001), 0);
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_db_lists_limited_items_and_their_sellers() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let items = crate::data::items::ItemData::load(&conn).unwrap();
    let limited: Vec<_> = items
        .templates
        .values()
        .filter(|t| t.purchase_limit().is_some())
        .collect();
    assert_eq!(
        limited.iter().filter(|t| t.limited_sale_count > 0).count(),
        31
    );
    assert_eq!(
        limited.iter().filter(|t| t.one_time_sale).count(),
        1,
        "items.one_time_sale"
    );
    assert!(limited.iter().all(|t| t.purchase_limit() == Some(1)));

    let mut packs = conn
        .prepare(
            "SELECT mg.merchant_pack_id, mp.name FROM merchant_goods mg \
             LEFT JOIN merchant_packs mp ON mp.id = mg.merchant_pack_id \
             WHERE mg.item_id = ? ORDER BY mg.merchant_pack_id",
        )
        .unwrap();
    let mut purchases = conn
        .prepare(
            "SELECT dfp.id, dfp.coin_item_id, dfp.coin_count \
             FROM doodad_func_purchases dfp WHERE dfp.item_id = ? ORDER BY dfp.id",
        )
        .unwrap();
    let mut sold_from_merchant = 0;
    let mut sold_from_doodad = 0;
    for item in &limited {
        let packs: Vec<(u32, String)> = packs
            .query_map([item.id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let funcs: Vec<(u32, Option<u32>, i32)> = purchases
            .query_map([item.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        if !packs.is_empty() {
            sold_from_merchant += 1;
        }
        if !funcs.is_empty() {
            sold_from_doodad += 1;
        }
        eprintln!(
            "limited item {} {:?} limit={:?} merchant packs={:?} doodad purchases={:?}",
            item.id,
            item.name,
            item.purchase_limit(),
            packs,
            funcs
        );
    }
    // The shipped 1.2 data reaches this path through `DoodadFuncPurchase`; no merchant pack
    // carries one of the limited templates.
    assert_eq!(sold_from_merchant, 0);
    assert_eq!(sold_from_doodad, 31);
    let (coin, coin_count): (Option<u32>, i32) = conn
        .query_row(
            "SELECT coin_item_id, coin_count FROM doodad_func_purchases WHERE item_id = 29797",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((coin, coin_count), (Some(23633), 1000));
}

#[tokio::test]
#[ignore = "private MySQL3317 via AAEMU_TEST_MYSQL; connection-local temporary table"]
async fn private_mysql_shop_purchase_counts_survive_save_and_reload() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let url = std::env::var("AAEMU_TEST_MYSQL").expect("AAEMU_TEST_MYSQL");
    let options = MySqlConnectOptions::from_str(&url).unwrap();
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let (_, ddl): (String, String) = sqlx::query_as("SHOW CREATE TABLE `account_shop_purchases`")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        &crate::systems::social::achievement_persistence::temporary_shadow_definition(&ddl),
    )
    .execute(&pool)
    .await
    .unwrap();

    let server = {
        let mut server = test_server_with(test_data());
        server.db = pool.clone();
        let server = Arc::new(server);
        server.set_self();
        server
    };
    crate::systems::economy::shop_limits::load(&server)
        .await
        .unwrap();
    assert_eq!(server.systems.economy.shop_limits.purchased(1, STACK), 0);

    let claim = crate::systems::economy::shop_limits::ShopClaim {
        item_id: STACK,
        count: 2,
        limit: 3,
    };
    assert!(server.systems.economy.shop_limits.try_claim(1, &[claim]));
    let rows = server.systems.economy.shop_limits.prepare_save();
    let mut tx = pool.begin().await.unwrap();
    let written = crate::systems::economy::shop_limits::write_prepared(&mut tx, rows)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(written, 1);

    let reloaded = {
        let mut server = test_server_with(test_data());
        server.db = pool.clone();
        let server = Arc::new(server);
        server.set_self();
        server
    };
    crate::systems::economy::shop_limits::load(&reloaded)
        .await
        .unwrap();
    assert_eq!(reloaded.systems.economy.shop_limits.purchased(1, STACK), 2);
    assert!(!reloaded.systems.economy.shop_limits.try_claim(
        1,
        &[crate::systems::economy::shop_limits::ShopClaim {
            item_id: STACK,
            count: 2,
            limit: 3,
        }]
    ));
    assert!(reloaded.systems.economy.shop_limits.try_claim(
        1,
        &[crate::systems::economy::shop_limits::ShopClaim {
            item_id: STACK,
            count: 1,
            limit: 3,
        }]
    ));
    pool.close().await;
}
