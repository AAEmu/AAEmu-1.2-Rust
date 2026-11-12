//! Tests of the crafting / store / specialty / expert / item-look ports: byte-exact S->C
//! packets, C->S decoding, flows through the real handlers (no MySQL), gated real-DB loads.

use std::sync::{Arc, Mutex};

use aaemu_commons::{Marshal, PacketStream};

use crate::data::items::{backpack_type, EquipTemplate, ItemTemplate, TemplateKind};
use crate::data::world_data::Zone;
use crate::data::GameData;
use crate::models::container::{equip_slot, slot_type};
use crate::models::doodad::Doodad;
use crate::models::house::House;
use crate::models::item::{Item, ITEM_FLAG_SECURE};
use crate::models::item_task::{task_type, ItemTask};
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::economy::craft::{self, CSExecuteCraft, CraftHooks, SCCraftFailedPacket};
use crate::systems::economy::craft_data::{
    Craft, CraftData, CraftMaterial, CraftProduct, CraftSkill,
};
use crate::systems::economy::hooks::{NpcEconomyHooks, NpcShopInfo};
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::item_packets::{
    SCCharacterLaborPowerChangedPacket, SCGamePointChangedPacket,
};
use crate::systems::economy::specialty::{
    self, CSListSpecialtyGoodsPacket, CSRequestSpecialtyCurrentPacket, CSSpecialtyRatioPacket,
    SCSpecialtyCurrentPacket, SCSpecialtyGoodsPacket, SCSpecialtyRatioPacket, SpecialtyGood,
};
use crate::systems::economy::store::{self, *};
use crate::systems::economy::store_data::{
    ExpandExpertLimit, ItemLookConvert, SpecialtyBundleItem, SpecialtyNpc,
};
use crate::systems::economy::tests::{give, item_at, run, task_body, test_data, World, STACK};
use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate};
use crate::systems::housing::dominion::runtime as dominion_runtime;
use crate::systems::housing::dominion::state::SiegeSide;
use crate::systems::npcs::doodad::templates::{
    DoodadFunc, DoodadFuncStoreUi, DoodadTemplate, FuncTemplate,
};
use crate::systems::npcs::npc::packets::CSStartInteractionPacket;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{DoodadRef, Entity, NpcRef, PlayerRef};

const PRODUCT: u32 = 5000; // max 10
const PACK: u32 = 6000; // trade pack (backpack, auto-equip)
const GLIDER: u32 = 6001;
const SWORD: u32 = 7000; // weapon with a look convert
const POWDER: u32 = 7100;
const COIN_ITEM: u32 = 8000;
const CRAFT: u32 = 1;
const PACK_CRAFT: u32 = 2;
const ACT_CRAFT: u32 = 3; // needs 100 actability points of group 5
const NO_SKILL_CRAFT: u32 = 4; // its skill is not in the data
const SKILL: u32 = 10;
const ACT_SKILL: u32 = 11;
const MERCHANT_NPC: u32 = 0x500;
const TRADER_NPC: u32 = 0x501;

fn data() -> GameData {
    let mut d = test_data();
    d.items
        .templates
        .insert(PRODUCT, ItemTemplate::plain(PRODUCT, 10));
    d.items.templates.insert(
        PACK,
        ItemTemplate {
            refund: 1000,
            ..ItemTemplate::new(PACK, TemplateKind::Backpack { backpack_type: 1 })
        },
    );
    d.items.templates.get_mut(&PACK).unwrap().max_count = 1;
    d.items.templates.insert(
        GLIDER,
        ItemTemplate {
            max_count: 1,
            ..ItemTemplate::new(
                GLIDER,
                TemplateKind::Backpack {
                    backpack_type: backpack_type::GLIDER,
                },
            )
        },
    );
    d.items.templates.insert(
        SWORD,
        ItemTemplate {
            max_count: 1,
            ..ItemTemplate::new(
                SWORD,
                TemplateKind::Weapon(EquipTemplate {
                    slot_type_id: 1,
                    ..EquipTemplate::default()
                }),
            )
        },
    );
    d.items
        .templates
        .insert(POWDER, ItemTemplate::plain(POWDER, 100));
    d.items
        .templates
        .insert(COIN_ITEM, ItemTemplate::plain(COIN_ITEM, 1000));
    {
        let t = d.items.templates.get_mut(&STACK).unwrap();
        t.honor_price = 3;
    }
    let econ = &mut d.systems.economy;
    econ.craft.crafts.insert(
        CRAFT,
        Craft {
            id: CRAFT,
            skill_id: SKILL,
            craft_products: vec![CraftProduct {
                id: 1,
                craft_id: CRAFT,
                item_id: PRODUCT,
                amount: 2,
                ..CraftProduct::default()
            }],
            craft_materials: vec![CraftMaterial {
                id: 1,
                craft_id: CRAFT,
                item_id: STACK,
                amount: 3,
                main_grade: false,
                require_grade: -1,
            }],
            ..Craft::default()
        },
    );
    econ.craft.crafts.insert(
        PACK_CRAFT,
        Craft {
            id: PACK_CRAFT,
            skill_id: SKILL,
            craft_products: vec![CraftProduct {
                id: 2,
                craft_id: PACK_CRAFT,
                item_id: PACK,
                amount: 1,
                ..CraftProduct::default()
            }],
            ..Craft::default()
        },
    );
    econ.craft.crafts.insert(
        ACT_CRAFT,
        Craft {
            id: ACT_CRAFT,
            skill_id: ACT_SKILL,
            actability_limit: 100,
            ..Craft::default()
        },
    );
    econ.craft.crafts.insert(
        NO_SKILL_CRAFT,
        Craft {
            id: NO_SKILL_CRAFT,
            skill_id: 999,
            ..Craft::default()
        },
    );
    econ.craft.skills.insert(
        ACT_SKILL,
        CraftSkill {
            id: ACT_SKILL,
            consume_labor_power: 1,
            actability_group_id: 5,
            cooldown_time: 0,
        },
    );
    econ.craft.skills.insert(
        SKILL,
        CraftSkill {
            id: SKILL,
            consume_labor_power: 5,
            actability_group_id: 0,
            cooldown_time: 0,
        },
    );
    let store = &mut econ.store;
    store.refund_multipliers.insert(0, 100);
    store.refund_multipliers.insert(1, 50);
    store.expand_expert_limits = vec![ExpandExpertLimit {
        id: 1,
        expand_count: 1,
        life_point: 10,
        item_id: STACK,
        item_count: 2,
    }];
    store.weapon_holdables.insert(SWORD, 3);
    store.holdable_item_look_converts.insert(3, 9);
    store.item_look_converts.insert(
        9,
        ItemLookConvert {
            id: 9,
            slot_id: 0,
            required_item_id: POWDER,
            required_item_count: 2,
        },
    );
    store.specialty_npcs.insert(
        4321,
        SpecialtyNpc {
            id: 1,
            name: "trader".into(),
            npc_id: 4321,
            specialty_bundle_id: 77,
        },
    );
    let b = SpecialtyBundleItem {
        id: 1,
        item_id: PACK,
        specialty_bundle_id: 77,
        profit: 5000,
        ratio: 1200,
    };
    store.specialty_bundle_items.insert(1, b);
    store
        .specialty_bundle_items_mapped
        .entry(PACK)
        .or_default()
        .insert(77, b);
    store.specialty_packs = vec![(PACK, 5), (GLIDER, 6)];
    d
}

fn world() -> World {
    world_with_data(data())
}

fn world_with_data(data: GameData) -> World {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    let pa = spawn_player(&server, &a, test_character(1, "Alice", 100.0, 100.0), 0x100);
    let pb = spawn_player(&server, &b, test_character(2, "Bob", 101.0, 100.0), 0x101);
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

fn bytes(p: &impl Marshal) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write(&mut s);
    s.into_bytes()
}

fn err_bytes(e1: i16, e2: i16, kind: u32, notify: bool) -> Vec<u8> {
    bytes(&SCErrorMsgPacket {
        error1: e1,
        error2: e2,
        kind,
        is_notify: notify,
    })
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

fn spawn_doodad(w: &mut World, obj_id: u32, template: u32, group: u32, x: f32) -> DoodadRef {
    let mut d = Doodad::new(template);
    d.func_group_id = group;
    d.transform.set_xyz(x, 100.0, 100.0);
    let d = Entity::new(obj_id, d);
    let world = w.server.world.world(0).unwrap();
    world.spawn(&d, &w.server.view_ctx());
    w.a.drain();
    w.b.drain();
    d
}

/// `DoodadFuncStoreUi` of group 42 with pack `pack_id` (the `Shops` hook sells `STACK` in 7).
fn with_doodad_shop(pack_id: u32) -> GameData {
    let mut d = data();
    let group = 42;
    d.systems.npcs.doodad.funcs_by_groups.insert(
        group,
        vec![DoodadFunc {
            group_id: group,
            func_id: 1,
            func_key: 1,
            func_type: "DoodadFuncStoreUi".into(),
            next_phase: -1,
            sound_id: 0,
            skill_id: 0,
            perm_id: 0,
            count: 0,
        }],
    );
    d.systems
        .npcs
        .doodad
        .func_templates
        .entry("DoodadFuncStoreUi")
        .or_default()
        .insert(
            1,
            FuncTemplate::StoreUi(DoodadFuncStoreUi {
                id: 1,
                merchant_pack_id: pack_id,
            }),
        );
    d
}

fn doodad_buy_body(doodad: u32, buy: &[(u32, i32, u8)], buy_back: &[i32]) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(0)
        .write_bc(doodad)
        .write_u32(0)
        .write_u8(buy.len() as u8)
        .write_u8(buy_back.len() as u8);
    for (id, count, currency) in buy {
        s.write_u32(*id)
            .write_u8(0)
            .write_i32(*count)
            .write_u8(*currency);
    }
    for i in buy_back {
        s.write_i32(*i);
    }
    s.write_bool(false);
    s.into_bytes()
}

// ---------------------------------------------------------------------------------------------
// Byte-exact S->C packets / C->S decoding
// ---------------------------------------------------------------------------------------------

#[test]
fn sc_craft_failed_bytes() {
    let p = SCCraftFailedPacket {
        id: 7,
        item_id: 0x1234,
        count: 2,
    };
    let mut want = PacketStream::new();
    want.write_u32(7)
        .write_i32(2)
        .write_u32(0x1234)
        .write_u32(0x1234);
    assert_eq!(bytes(&p), want.into_bytes());

    // The client reads at most 20 entries: larger counts are capped, negative ones are empty.
    let capped = bytes(&SCCraftFailedPacket {
        id: 7,
        item_id: 0x1234,
        count: 99,
    });
    let mut want = PacketStream::new();
    want.write_u32(7).write_i32(20);
    for _ in 0..20 {
        want.write_u32(0x1234);
    }
    assert_eq!(capped, want.into_bytes());
    let negative = bytes(&SCCraftFailedPacket {
        id: 7,
        item_id: 0x1234,
        count: -3,
    });
    assert_eq!(negative, [7u32.to_le_bytes(), 0u32.to_le_bytes()].concat());
}

#[test]
fn sc_expert_expanded_and_specialty_bytes() {
    assert_eq!(bytes(&SCExpertExpandedPacket { next: 3 }), vec![3]);
    assert_eq!(
        bytes(&SCSpecialtyRatioPacket { ratio: 117 }),
        117i32.to_le_bytes().to_vec()
    );
    let p = SCSpecialtyCurrentPacket {
        from_zone_group: 5,
        to_zone_group: 9,
        results: vec![(PACK, 130), (GLIDER, 70)],
    };
    let mut want = PacketStream::new();
    // 1.2 reader FUN_397b87c0: i32 count, u16 from, u16 to, then (u32 item, i32 ratio) pairs.
    want.write_i32(2)
        .write_u16(5)
        .write_u16(9)
        .write_u32(PACK)
        .write_i32(130)
        .write_u32(GLIDER)
        .write_i32(70);
    assert_eq!(bytes(&p), want.into_bytes());
}

#[test]
fn sc_specialty_goods_bytes() {
    // 1.2 reader FUN_397aec90 + FUN_398a9e30: i32 count, then 10-byte records
    // (u32 itemId, i32 refund, u8 Currency, u8 unknown).
    let goods = vec![SpecialtyGood {
        item_id: 26488,
        refund: 4321,
        currency: 1,
        unknown: 7,
    }];
    let mut want = PacketStream::new();
    want.write_i32(1)
        .write_u32(26488)
        .write_i32(4321)
        .write_u8(1)
        .write_u8(7);
    assert_eq!(bytes(&SCSpecialtyGoodsPacket { goods }), want.into_bytes());
}

#[test]
fn sc_sold_item_list_keeps_first_twelve() {
    let items: Vec<Item> = (0..14u64)
        .map(|i| {
            let mut it = Item::create(
                100 + i,
                &ItemTemplate::plain(STACK, 20),
                1,
                0,
                1,
                &Default::default(),
                0,
            );
            it.slot = i as i32;
            it
        })
        .collect();
    let p = SCSoldItemListPacket::new(items.clone());
    let mut want = PacketStream::new();
    want.write_i32(12);
    for it in &items[..12] {
        it.write(&mut want);
    }
    assert_eq!(bytes(&p), want.into_bytes());
}

#[test]
fn cs_packets_decode() {
    use crate::net::registry::ClientPacket;
    let mut s = PacketStream::new();
    s.write_u32(55).write_bc(0x300).write_i32(4);
    let p = CSExecuteCraft::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
    assert_eq!(
        p,
        CSExecuteCraft {
            craft_id: 55,
            obj_id: 0x300,
            count: 4
        }
    );

    let mut s = PacketStream::new();
    s.write_bc(0x500)
        .write_bc(0)
        .write_u32(9)
        .write_u8(1)
        .write_u8(2)
        .write_u32(STACK)
        .write_u8(0)
        .write_i32(3)
        .write_u8(1)
        .write_i32(0)
        .write_i32(1)
        .write_bool(true);
    let p = CSBuyItemsPacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
    assert_eq!(
        p,
        CSBuyItemsPacket {
            npc_obj_id: 0x500,
            doodad_obj_id: 0,
            unk_id: 9,
            buy: vec![BuyEntry {
                item_id: STACK,
                grade: 0,
                count: 3,
                currency: 1
            }],
            buy_back: vec![0, 1],
            use_aa_point: true,
        }
    );

    let mut s = PacketStream::new();
    s.write_bc(0x500)
        .write_bc(0x10)
        .write_u8(1)
        .write_u8(2)
        .write_u8(7)
        .write_u64(0xAABB)
        .write_u32(1);
    let p = CSSellItemsPacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
    assert_eq!(
        p.items,
        vec![SellEntry {
            slot_type: 2,
            slot: 7,
            item_id: 0xAABB,
            unk_id: 1
        }]
    );
    assert_eq!(p.unk_obj_id, 0x10);

    let mut s = PacketStream::new();
    s.write_u8(0)
        .write_u8(1)
        .write_u8(0)
        .write_u8(3)
        .write_u8(0)
        .write_u8(2)
        .write_u8(0)
        .write_u8(4)
        .write_u64(11)
        .write_u64(12);
    let p = CSChangeItemLookPacket::read(&mut PacketStream::from_bytes(s.into_bytes())).unwrap();
    assert_eq!(
        (
            p.slot_type1,
            p.slot1,
            p.slot_type2,
            p.slot2,
            p.item_id,
            p.look_id
        ),
        (1, 3, 2, 4, 11, 12)
    );

    let mut s = PacketStream::new();
    s.write_u16(5).write_u16(9);
    let p = CSRequestSpecialtyCurrentPacket::read(&mut PacketStream::from_bytes(s.into_bytes()))
        .unwrap();
    assert_eq!((p.from_zone_group_id, p.to_zone_group_id), (5, 9));
}

// ---------------------------------------------------------------------------------------------
// Crafting flows
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct CraftRecorder {
    casts: Mutex<Vec<(u32, u32, f32)>>,
    cast_started: tokio::sync::Notify,
    cancels: Mutex<u32>,
    crafted: Mutex<Vec<u32>>,
    /// What `start_craft_cast` reports (`skill_result`).
    cast_result: Mutex<u8>,
}

impl CraftHooks for CraftRecorder {
    fn start_craft_cast(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        skill_id: u32,
        doodad_obj_id: u32,
        mul: f32,
    ) -> u8 {
        self.casts
            .lock()
            .unwrap()
            .push((skill_id, doodad_obj_id, mul));
        self.cast_started.notify_one();
        *self.cast_result.lock().unwrap()
    }

    fn cancel_craft_skill(&self, _server: &GameServer, _player: &PlayerRef) {
        *self.cancels.lock().unwrap() += 1;
    }

    fn on_craft(&self, _server: &GameServer, _player: &PlayerRef, craft_id: u32) {
        self.crafted.lock().unwrap().push(craft_id);
    }
}

fn install_craft_hooks(w: &World) -> Arc<CraftRecorder> {
    let rec = Arc::new(CraftRecorder::default());
    w.server.systems.economy.craft.hooks.install(rec.clone());
    rec
}

fn execute_craft(w: &mut World, craft_id: u32, count: i32) {
    let mut s = PacketStream::new();
    s.write_u32(craft_id).write_bc(0).write_i32(count);
    run::<CSExecuteCraft>(w, 0, s.as_bytes());
}

fn load_craft_sql(rows: &str) -> CraftData {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE crafts (id, cast_delay, tool_id, skill_id, wi_id, milestone_id,
            req_doodad_id, need_bind, ac_id, actability_limit, show_upper_crafts,
            recommend_level, visible_order);
        INSERT INTO crafts VALUES (1, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        CREATE TABLE craft_products (id, craft_id, item_id, amount, rate,
            show_lower_crafts, use_grade, item_grade_id);
        CREATE TABLE craft_materials (id, craft_id, item_id, amount, main_grade, require_grade);
        CREATE TABLE craft_pack_crafts (craft_id);
        CREATE TABLE skills (id, consume_lp, actability_group_id, cooldown_time);
        INSERT INTO skills VALUES (10, 5, 0, 0);",
    )
    .unwrap();
    conn.execute_batch(rows).unwrap();
    let files =
        crate::data::client_files::ClientFileManager::from_sources(&[], std::path::Path::new("."));
    CraftData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: std::path::Path::new("."),
        files: &files,
        language: "en_us",
    })
    .unwrap()
}

#[test]
fn craft_grade_flags_load_sqlite_text() {
    // Native 1.2 LoadCraftProductDescs / LoadCraftMaterialDescs (0x3982E6C0 / 0x3982E4D0)
    // read the text flags, not sqlite3_column_int('t') == 0.
    let d = load_craft_sql(
        "INSERT INTO craft_products VALUES
            (1, 1, 5000, 1, 100, 'f', 't', 3),
            (2, 1, 5000, 1, 100, 'f', 'f', 0),
            (3, 1, 5000, 1, 100, 'f', 1, 2),
            (4, 1, 5000, 1, 100, 'f', 0, 0),
            (5, 1, 5000, 1, 100, 'f', NULL, 0);
        INSERT INTO craft_materials VALUES
            (1, 1, 1000, 1, 'f', -1), (2, 1, 7000, 1, 't', 4),
            (3, 1, 1000, 1, 0, NULL), (4, 1, 7000, 1, 1, 6), (5, 1, 1000, 1, NULL, -1);",
    );
    let c = d.craft(CRAFT).unwrap();
    assert_eq!(
        c.craft_products
            .iter()
            .map(|p| (p.use_grade, p.item_grade_id))
            .collect::<Vec<_>>(),
        vec![(true, 3), (false, 0), (true, 2), (false, 0), (false, 0)]
    );
    assert_eq!(
        c.craft_materials
            .iter()
            .map(|m| (m.main_grade, m.require_grade))
            .collect::<Vec<_>>(),
        vec![(false, -1), (true, 4), (false, -1), (true, 6), (false, -1)]
    );
}

#[tokio::test]
async fn craft_loaded_grade_recipes_create_expected_inventory() {
    for (use_grade, main_grade, material, product_grade, fixed_grade, gradable, expected) in [
        ("t", "f", STACK, 3, -1, true, 3),
        ("f", "t", SWORD, 0, -1, true, 4),
        ("f", "t", STACK, 0, -1, true, 4),
        // Keep the 1.2 equipment-material heuristic when the main_grade flag is unset.
        ("f", "f", SWORD, 0, -1, true, 4),
        ("t", "f", STACK, 3, 2, false, 2),
        ("f", "f", STACK, 0, -1, true, 0),
    ] {
        let mut d = data();
        d.systems.economy.craft = load_craft_sql(&format!(
            "INSERT INTO craft_products VALUES (1, 1, {PRODUCT}, 1, 100, 'f', '{use_grade}', {product_grade});
            INSERT INTO craft_materials VALUES (1, 1, {material}, 1, '{main_grade}', -1);"
        ));
        let recipe = d.systems.economy.craft.craft(CRAFT).unwrap();
        assert_eq!(recipe.craft_products[0].use_grade, use_grade == "t");
        assert_eq!(recipe.craft_materials[0].main_grade, main_grade == "t");
        let t = d.items.templates.get_mut(&PRODUCT).unwrap();
        t.fixed_grade = fixed_grade;
        t.gradable = gradable;
        d.items.grades.insert(
            4,
            crate::data::items::GradeTemplate {
                grade: 4,
                grade_order: 4,
                durability: 1.0,
            },
        );
        let mut w = world_with_data(d);
        let rec = install_craft_hooks(&w);
        let pa = w.pa.clone();
        pa.data().labor_power = 100;
        let id = give(&w, &pa, slot_type::INVENTORY, material, 1, 0);
        Inv::of(&pa)
            .bag()
            .lock()
            .unwrap()
            .get_item_by_id_mut(id)
            .unwrap()
            .grade = 4;
        execute_craft(&mut w, CRAFT, 1);
        assert_eq!(rec.casts.lock().unwrap().len(), 1);
        craft::end_craft(&w.server, &pa);
        let inv = Inv::of(&pa);
        assert_eq!(inv.items_count(material, -1), 0);
        let bag = inv.bag();
        let bag = bag.lock().unwrap();
        assert_eq!(bag.items.len(), 1);
        let product = &bag.items[0];
        assert_eq!(
            (product.template_id, product.count, product.grade),
            (PRODUCT, 1, expected)
        );
        assert_eq!(*rec.crafted.lock().unwrap(), vec![CRAFT]);
    }
}

#[tokio::test]
async fn craft_material_requires_its_grade() {
    let mut d = data();
    d.systems.economy.craft = load_craft_sql(
        "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', 'f', 0);
        INSERT INTO craft_materials VALUES (1, 1, 1000, 1, 'f', 2);",
    );
    let mut w = world_with_data(d);
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;

    // A lower grade does not count and the refusal keeps the stack.
    let low = give(&w, &pa, slot_type::INVENTORY, STACK, 1, 0);
    Inv::of(&pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(low)
        .unwrap()
        .grade = 1;
    execute_craft(&mut w, CRAFT, 1);
    assert!(rec.casts.lock().unwrap().is_empty());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 722, 0, false))]
    );
    assert_eq!(
        Inv::of(&pa).items_count(STACK, 1),
        1,
        "refusal keeps the material"
    );

    // A higher grade does not count either: the client counts exactly `require_grade`
    // (GetCraftMaterialInfo 0x39462600 / GetExecutableCraftCount 0x394d3cc0 -> FUN_3961fbc0).
    Inv::of(&pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(low)
        .unwrap()
        .grade = 3;
    execute_craft(&mut w, CRAFT, 1);
    assert!(rec.casts.lock().unwrap().is_empty());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 722, 0, false))]
    );

    // Exactly the required grade pays, and only that stack is consumed.
    let right = give(&w, &pa, slot_type::INVENTORY, STACK, 1, 1);
    Inv::of(&pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(right)
        .unwrap()
        .grade = 2;
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    craft::end_craft(&w.server, &pa);
    let inv = Inv::of(&pa);
    assert_eq!(
        inv.items_count(STACK, 3),
        1,
        "the other grade stack is untouched"
    );
    assert_eq!(
        inv.items_count(STACK, 2),
        0,
        "the required grade stack paid"
    );
    assert_eq!(inv.items_count(PRODUCT, -1), 1);
}

#[tokio::test]
async fn craft_main_grade_sets_the_product_grade() {
    // Every main_grade row in compact.sqlite3 is on a product with use_grade = 'f', so the
    // material itself must drive the inherited grade.
    let mut d = data();
    d.systems.economy.craft = load_craft_sql(
        "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', 'f', 0);
        INSERT INTO craft_materials VALUES (1, 1, 7000, 1, 't', -1);",
    );
    d.items.grades.insert(
        3,
        crate::data::items::GradeTemplate {
            grade: 3,
            grade_order: 3,
            durability: 1.0,
        },
    );
    let mut w = world_with_data(d);
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    let id = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 0);
    Inv::of(&pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .grade = 3;
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    craft::end_craft(&w.server, &pa);
    let inv = Inv::of(&pa);
    assert_eq!(inv.items_count(SWORD, -1), 0);
    let bag = inv.bag();
    let bag = bag.lock().unwrap();
    assert_eq!((bag.items[0].template_id, bag.items[0].grade), (PRODUCT, 3));
}

#[tokio::test]
async fn craft_equipment_material_grade_inherits_for_any_product() {
    // The 1.2 equipment-material heuristic runs whenever no `main_grade` row exists, whatever
    // `use_grade` says: ~484 gear-upgrade crafts without grade rows (4993, 5285, 5086, ...)
    // keep their inherited grade and the free-regrade roll.
    for use_grade in ["t", "f"] {
        let mut d = data();
        d.systems.economy.craft = load_craft_sql(&format!(
            "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', '{use_grade}', 0);
            INSERT INTO craft_materials VALUES (1, 1, 7000, 1, 'f', -1);"
        ));
        d.items.grades.insert(
            2,
            crate::data::items::GradeTemplate {
                grade: 2,
                grade_order: 2,
                durability: 1.0,
            },
        );
        let mut w = world_with_data(d);
        let rec = install_craft_hooks(&w);
        let pa = w.pa.clone();
        pa.data().labor_power = 100;
        let id = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 0);
        Inv::of(&pa)
            .bag()
            .lock()
            .unwrap()
            .get_item_by_id_mut(id)
            .unwrap()
            .grade = 2;
        execute_craft(&mut w, CRAFT, 1);
        assert_eq!(rec.casts.lock().unwrap().len(), 1);
        craft::end_craft(&w.server, &pa);
        let bag = Inv::of(&pa).bag();
        let bag = bag.lock().unwrap();
        assert_eq!(
            (bag.items[0].template_id, bag.items[0].grade),
            (PRODUCT, 2),
            "use_grade = {use_grade}"
        );
    }
}

#[tokio::test]
async fn craft_unflagged_upgrade_inherits_the_equipment_grade() {
    // An upgrade recipe with no `main_grade` row and `use_grade = 'f'` (the bulk of the gear
    // upgrade crafts) still takes the equipment material's grade.
    let mut d = data();
    d.systems.economy.craft = load_craft_sql(
        "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', 'f', 0);
        INSERT INTO craft_materials VALUES (1, 1, 7000, 1, 'f', -1);",
    );
    d.items.grades.insert(
        4,
        crate::data::items::GradeTemplate {
            grade: 4,
            grade_order: 4,
            durability: 1.0,
        },
    );
    let mut w = world_with_data(d);
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    let id = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 0);
    Inv::of(&pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .grade = 4;
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    craft::end_craft(&w.server, &pa);
    let bag = Inv::of(&pa).bag();
    let bag = bag.lock().unwrap();
    assert_eq!((bag.items[0].template_id, bag.items[0].grade), (PRODUCT, 4));
}

#[tokio::test]
async fn craft_material_rows_of_one_item_are_summed() {
    // Craft 370's shape: the same item twice (8318 x3 + x1). Three units must not satisfy 4.
    let mut d = data();
    d.systems.economy.craft = load_craft_sql(
        "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', 'f', 0);
        INSERT INTO craft_materials VALUES
            (1, 1, 1000, 3, 'f', -1), (2, 1, 1000, 1, 'f', -1);",
    );
    let mut w = world_with_data(d);
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    execute_craft(&mut w, CRAFT, 1);
    assert!(rec.casts.lock().unwrap().is_empty());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 722, 0, false))]
    );
    assert_eq!(Inv::of(&pa).items_count(STACK, -1), 3);

    give(&w, &pa, slot_type::INVENTORY, STACK, 1, 1);
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    craft::end_craft(&w.server, &pa);
    let inv = Inv::of(&pa);
    assert_eq!(inv.items_count(STACK, -1), 0);
    assert_eq!(inv.items_count(PRODUCT, -1), 1);
}

#[tokio::test]
async fn craft_main_material_lowest_slot_decides_and_is_consumed() {
    // Two units of the main material at different grades in different slots: the lowest slot
    // is the grade source (consume_item's order) and only it is consumed.
    let mut d = data();
    d.systems.economy.craft = load_craft_sql(
        "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', 'f', 0);
        INSERT INTO craft_materials VALUES (1, 1, 7000, 1, 't', -1);",
    );
    for grade in [2i32, 3] {
        d.items.grades.insert(
            grade,
            crate::data::items::GradeTemplate {
                grade,
                grade_order: grade,
                durability: 1.0,
            },
        );
    }
    let mut w = world_with_data(d);
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    // Insertion order puts grade 2 first; slot 0 holds grade 3.
    let high = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 5);
    let low = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 0);
    {
        let bag = Inv::of(&pa).bag();
        let mut bag = bag.lock().unwrap();
        bag.get_item_by_id_mut(high).unwrap().grade = 2;
        bag.get_item_by_id_mut(low).unwrap().grade = 3;
    }
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    craft::end_craft(&w.server, &pa);
    let inv = Inv::of(&pa);
    assert_eq!(inv.items_count(SWORD, 2), 1, "the higher-slot stack stays");
    assert_eq!(inv.items_count(SWORD, 3), 0, "the lowest slot paid");
    let bag = inv.bag();
    let bag = bag.lock().unwrap();
    let product = bag.items.iter().find(|i| i.template_id == PRODUCT).unwrap();
    assert_eq!(product.grade, 3);
}

#[tokio::test]
async fn craft_secured_right_grade_material_is_locked_only() {
    // A secured stack of the right grade cannot pay; the refusal says ITEM_SECURE_CONDITION.
    let mut d = data();
    d.systems.economy.craft = load_craft_sql(
        "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', 'f', 0);
        INSERT INTO craft_materials VALUES (1, 1, 1000, 1, 'f', 2);",
    );
    let mut w = world_with_data(d);
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 1, 0);
    {
        let bag = Inv::of(&pa).bag();
        let mut bag = bag.lock().unwrap();
        let item = bag.get_item_by_id_mut(id).unwrap();
        item.grade = 2;
        item.flags |= ITEM_FLAG_SECURE;
    }
    execute_craft(&mut w, CRAFT, 1);
    assert!(rec.casts.lock().unwrap().is_empty());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 664, 0, false))]
    );
    assert_eq!(Inv::of(&pa).items_count(STACK, 2), 1);
}

#[tokio::test]
async fn craft_grade_lost_before_the_hand_out_keeps_the_materials() {
    let mut d = data();
    d.systems.economy.craft = load_craft_sql(
        "INSERT INTO craft_products VALUES (1, 1, 5000, 1, 100, 'f', 'f', 0);
        INSERT INTO craft_materials VALUES (1, 1, 1000, 1, 'f', 2);",
    );
    let mut w = world_with_data(d);
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 1, 0);
    Inv::of(&pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .grade = 2;
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    w.a.drain();

    // The required grade is gone before the cast's CraftEffect applies (a regrade, a move).
    Inv::of(&pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .grade = 1;
    craft::end_craft(&w.server, &pa);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 722, 0, false))]
    );
    let inv = Inv::of(&pa);
    assert_eq!(inv.items_count(PRODUCT, -1), 0);
    assert_eq!(inv.items_count(STACK, 1), 1, "refusal keeps the material");
    assert!(rec.crafted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn craft_batch_consumes_materials_adds_products_and_reschedules() {
    let mut w = world();
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::INVENTORY, STACK, 6, 0);

    execute_craft(&mut w, CRAFT, 2);
    assert_eq!(rec.casts.lock().unwrap().clone(), vec![(SKILL, 0, 1.0)]);
    let st = w.server.systems.economy.craft.state(1);
    assert!(st.is_crafting);
    assert_eq!(
        (st.count, st.current_craft, st.consume_labor_power),
        (2, Some(CRAFT), 5)
    );
    assert!(w.a.drain().is_empty());

    // The skills subsystem finishes the cast.
    craft::end_craft(&w.server, &pa);
    let product = item_at(&pa, 2, 1).expect("product");
    assert_eq!((product.template_id, product.count), (PRODUCT, 2));
    // Every crafted product records its crafter; only single-stack items also fill the older
    // `made_unit_id` carrier.
    assert_eq!(product.crafter_id, 1);
    assert_eq!(product.crafter(), 1);
    assert_eq!(product.made_unit_id, 0);
    let mat = item_at(&pa, 2, 0).unwrap();
    assert_eq!(mat.count, 3);
    let f = w.a.drain();
    assert_eq!(
        f,
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::CRAFT_ACT_SAVED, vec![ItemTask::add(&product)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(
                    task_type::CRAFT_ACT_SAVED,
                    vec![ItemTask::count_update(&mat, -3)]
                )
            ),
        ]
    );
    assert_eq!(rec.crafted.lock().unwrap().clone(), vec![CRAFT]);
    // EndCraft does not touch labor (Skill.EndSkill does).
    assert_eq!(pa.data().labor_power, 100);
    let st = w.server.systems.economy.craft.state(1);
    assert_eq!((st.count, st.is_crafting), (1, false));

    // The CraftTask re-runs Craft after the skill cooldown (0 ms here).
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let casts = rec.casts.lock().unwrap().len();
            if casts >= 2 {
                break;
            }
            rec.cast_started.notified().await;
        }
    })
    .await
    .expect("the next craft cast should start after its cooldown");
    assert_eq!(rec.casts.lock().unwrap().len(), 2);
    assert!(w.server.systems.economy.craft.is_crafting(1));

    craft::end_craft(&w.server, &pa);
    assert!(item_at(&pa, 2, 0).is_none()); // all materials used
    assert_eq!(item_at(&pa, 2, 1).unwrap().count, 4);
    let st = w.server.systems.economy.craft.state(1);
    // CancelCraft keeps ConsumeLaborPower.
    assert_eq!(
        st,
        craft::CraftState {
            consume_labor_power: 5,
            ..craft::CraftState::default()
        }
    );
    assert_eq!(*rec.cancels.lock().unwrap(), 1);
}

#[tokio::test]
async fn craft_refused_without_materials_or_labor() {
    let mut w = world();
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    give(&w, &pa, slot_type::INVENTORY, STACK, 2, 0);
    execute_craft(&mut w, CRAFT, 1);
    assert!(rec.casts.lock().unwrap().is_empty());
    assert_eq!(*rec.cancels.lock().unwrap(), 1);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 722, 0, false))]
    );

    // Enough materials, not enough labor at the end: error, no product, batch over.
    give(&w, &pa, slot_type::INVENTORY, STACK, 5, 1);
    pa.data().labor_power = 4;
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    craft::end_craft(&w.server, &pa);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 29, 0, false))]
    );
    assert_eq!(Inv::of(&pa).items_count(PRODUCT, -1), 0);
    assert_eq!(*rec.cancels.lock().unwrap(), 2);

    // Unknown craft (C# KeyNotFoundException, client left locked): the batch is ended with an
    // error instead.
    execute_craft(&mut w, 999, 1);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 136, 0, false))]
    );
}

#[tokio::test]
async fn refused_craft_cast_ends_the_batch_and_tells_the_client() {
    use crate::systems::skills::types::skill_result as r;
    let mut w = world();
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::INVENTORY, STACK, 6, 0);

    // `Skill.Use` refuses (labor, ...): the C# ignored the result, so the client's craft window
    // stayed locked ("greyed out") until relog.
    for (result, reason) in [
        (r::NEED_LABOR_POWER, 29),
        (r::NEED_REAGENT, 722),
        (r::NO_PERM, 135),
        (r::TOO_FAR_RANGE, 5),
        (r::COOLDOWN_TIME, 133),
    ] {
        *rec.cast_result.lock().unwrap() = result;
        execute_craft(&mut w, CRAFT, 5);
        assert_eq!(
            w.a.drain(),
            vec![(sc::SCErrorMsgPacket, err_bytes(148, reason, 0, false))],
            "skill result {result}"
        );
        // The server state is clear again and the craft skill was cancelled.
        assert_eq!(
            w.server.systems.economy.craft.state(1),
            craft::CraftState {
                consume_labor_power: 5,
                ..craft::CraftState::default()
            }
        );
    }
    assert_eq!(*rec.cancels.lock().unwrap(), 5);
    assert_eq!(rec.casts.lock().unwrap().len(), 5);

    // A successful cast sends nothing and keeps the batch.
    *rec.cast_result.lock().unwrap() = r::SUCCESS;
    execute_craft(&mut w, CRAFT, 5);
    assert!(w.a.drain().is_empty());
    let st = w.server.systems.economy.craft.state(1);
    assert_eq!((st.is_crafting, st.count), (true, 5));
}

#[tokio::test]
async fn failed_next_craft_of_a_batch_ends_it() {
    use crate::systems::skills::types::skill_result as r;
    let mut w = world();
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::INVENTORY, STACK, 9, 0);
    execute_craft(&mut w, CRAFT, 3);
    craft::end_craft(&w.server, &pa);
    w.a.drain();

    // Labor ran out before the second craft of the batch (the repeat is a plain `Craft` call).
    *rec.cast_result.lock().unwrap() = r::NEED_LABOR_POWER;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while rec.casts.lock().unwrap().len() < 2 {
            rec.cast_started.notified().await;
        }
    })
    .await
    .expect("the next craft of the batch should start");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 29, 0, false))]
    );
    let st = w.server.systems.economy.craft.state(1);
    assert_eq!(
        (st.is_crafting, st.count, st.current_craft),
        (false, 0, None)
    );
}

#[tokio::test]
async fn cast_failure_after_the_cast_started_ends_the_craft() {
    use crate::systems::skills::types::skill_result as r;
    let mut w = world();
    let _rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::INVENTORY, STACK, 6, 0);
    execute_craft(&mut w, CRAFT, 2);
    w.a.drain();

    // Another skill failing does not touch the craft.
    craft::cast_failed(&w.server, &pa, SKILL + 1, r::NEED_LABOR_POWER);
    assert!(w.a.drain().is_empty());
    assert!(w.server.systems.economy.craft.is_crafting(1));

    // The craft skill's own `Skill.Cast` re-validation failing does.
    craft::cast_failed(&w.server, &pa, SKILL, r::NEED_LABOR_POWER);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 29, 0, false))]
    );
    assert!(!w.server.systems.economy.craft.is_crafting(1));
}

#[tokio::test]
async fn craft_labor_check_uses_the_vocation_discounted_cost() {
    // Skill cost 5 at rank 5 (x0.80) = 4: a character with exactly 4 labor can finish the craft;
    // `Skill.EndSkill` charges that discounted cost, so `EndCraft` must not refuse it at 5.
    assert_eq!(craft::effective_labor_cost_for_tests(5, None), 5);
    assert_eq!(craft::effective_labor_cost_for_tests(5, Some(5)), 4);
    assert_eq!(craft::effective_labor_cost_for_tests(3, Some(7)), 2);
    assert_eq!(craft::effective_labor_cost_for_tests(1, Some(7)), 1);
    assert_eq!(craft::effective_labor_cost_for_tests(0, Some(7)), 0);

    let mut w = world();
    let _rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 4;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 0,
            step: 5,
            point: 0,
        });
    give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    // Craft skill 10 has actability group 0 in the test data: the entry above is that group.
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(
        w.server.systems.economy.craft.state(1).consume_labor_power,
        4
    );
    craft::end_craft(&w.server, &pa);
    assert_eq!(Inv::of(&pa).items_count(PRODUCT, -1), 2);
}

#[tokio::test]
async fn craft_trade_pack_equips_into_backpack_slot() {
    let mut w = world();
    let _rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    // A glider is on the back: it moves to the bag for the pack.
    give(
        &w,
        &pa,
        slot_type::EQUIPMENT,
        GLIDER,
        1,
        equip_slot::BACKPACK,
    );
    execute_craft(&mut w, PACK_CRAFT, 1);
    craft::end_craft(&w.server, &pa);
    let pack = Inv::of(&pa).equipped(equip_slot::BACKPACK).expect("pack");
    assert_eq!(pack.template_id, PACK);
    assert_eq!(pack.crafter_id, 1); // the pack records its crafter in the dedicated column
    assert_eq!(pack.made_unit_id, 1); // and keeps the older single-stack carrier
    assert_eq!(item_at(&pa, 2, 0).unwrap().template_id, GLIDER);
    let ops = w.a.drain_ops();
    assert_eq!(
        ops,
        vec![sc::SCItemTaskSuccessPacket, sc::SCItemTaskSuccessPacket]
    );

    // A second pack while one is worn: CanReplaceGliderInBackpackSlot refuses.
    execute_craft(&mut w, PACK_CRAFT, 1);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 315, 0, false))]
    );
}

#[tokio::test]
async fn free_regrade_rolls() {
    let w = world();
    // test data: grade 0 only (max grade order 0) -> never upgraded
    assert_eq!(craft::free_regrade_with(&w.server, 0, 0), 0);
    assert_eq!(craft::free_regrade_with(&w.server, 3, 0), 4);
    assert_eq!(craft::free_regrade_with(&w.server, 3, 1), 3);
}

// ---------------------------------------------------------------------------------------------
// Store flows
// ---------------------------------------------------------------------------------------------

struct Shops;
impl NpcEconomyHooks for Shops {
    fn shop_info(&self, _server: &GameServer, npc: &NpcRef) -> Option<NpcShopInfo> {
        Some(NpcShopInfo {
            merchant: npc.data().unit.template_id == 1234,
            merchant_pack_ids: vec![7],
            ..NpcShopInfo::default()
        })
    }

    fn merchant_sells(
        &self,
        _server: &GameServer,
        _buyer: &PlayerRef,
        _npc: Option<&NpcRef>,
        pack_ids: &[u32],
        item_id: u32,
        grade: u8,
    ) -> bool {
        pack_ids.contains(&7) && item_id == STACK && grade == 0
    }
}

fn buy_body(npc: u32, buy: &[(u32, i32, u8)], buy_back: &[i32]) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(npc)
        .write_bc(0)
        .write_u32(0)
        .write_u8(buy.len() as u8)
        .write_u8(buy_back.len() as u8);
    for (id, count, currency) in buy {
        s.write_u32(*id)
            .write_u8(0)
            .write_i32(*count)
            .write_u8(*currency);
    }
    for i in buy_back {
        s.write_i32(*i);
    }
    s.write_bool(false);
    s.into_bytes()
}

fn catalog_world(mut d: GameData, stock: &[(u32, u8)]) -> World {
    let mut goods = crate::systems::npcs::npc::data::MerchantGoods {
        id: 7,
        items: Vec::new(),
    };
    for &(item, grade) in stock {
        goods.add_item_to_stock(item, grade);
    }
    d.systems.npcs.npc.goods.insert(7, goods);
    d.systems.npcs.npc.templates.insert(
        1234,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: 1234,
            merchant: true,
            merchant_pack_ids: vec![7],
            ..Default::default()
        },
    );
    let mut w = world_with_data(d);
    // Use the real NPC catalog bridge, including its native fixed-grade normalization.
    crate::systems::npcs::bridges::install_economy(&w.server);
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    w
}

async fn buy_grade(w: &mut World, item_id: u32, grade: u8, count: i32, currency: u8) {
    buy_grade_as(w, false, item_id, grade, count, currency).await;
}

async fn buy_grade_as(
    w: &mut World,
    second_player: bool,
    item_id: u32,
    grade: u8,
    count: i32,
    currency: u8,
) {
    use crate::net::registry::ClientPacket;
    let conn = if second_player {
        &mut w.b.conn
    } else {
        &mut w.a.conn
    };
    CSBuyItemsPacket {
        npc_obj_id: MERCHANT_NPC,
        doodad_obj_id: 0,
        unk_id: 0,
        buy: vec![BuyEntry {
            item_id,
            grade,
            count,
            currency,
        }],
        buy_back: Vec::new(),
        use_aa_point: false,
    }
    .handle(&w.server, conn)
    .await
    .unwrap();
}

#[tokio::test]
async fn shop_catalog_grades_create_distinct_paid_inventory() {
    let mut d = data();
    d.items.templates.get_mut(&STACK).unwrap().gradable = true;
    let sword = d.items.templates.get_mut(&SWORD).unwrap();
    sword.gradable = true;
    sword.fixed_grade = -1;
    sword.honor_price = 9;
    let mut w = catalog_world(d, &[(STACK, 0), (STACK, 4), (STACK, 11), (SWORD, 4)]);
    w.pa.data().money = 1000;
    w.pa.data().honor_point = 100;
    give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    buy_grade(&mut w, STACK, 4, 2, shop_currency_type::MONEY).await;
    buy_grade(&mut w, STACK, 11, 1, shop_currency_type::MONEY).await;
    buy_grade(&mut w, SWORD, 4, 1, shop_currency_type::HONOR).await;
    buy_grade(&mut w, STACK, 0, 1, shop_currency_type::MONEY).await;
    let inv = Inv::of(&w.pa);
    assert_eq!(inv.items_count(STACK, 0), 6);
    assert_eq!(inv.items_count(STACK, 4), 2);
    assert_eq!(inv.items_count(STACK, 11), 1);
    assert_eq!(inv.items_count(SWORD, 4), 1);
    let c = w.pa.data();
    assert_eq!((c.money, c.honor_point, c.vocation_point), (960, 91, 0));
}

/// `merchants` rows are additive: the 1.2 client joins `merchants` x `merchant_goods` per
/// NPC (`strings.txt:13227`), so an NPC with two packs sells the union. Pack 7 comes first
/// (`STACK`), pack 8 last (`POWDER`): a last-wins loader would refuse the first purchase.
#[tokio::test]
async fn shop_npc_sells_items_from_every_merchant_pack() {
    let mut d = data();
    d.items.templates.get_mut(&POWDER).unwrap().price = 100;
    let mut first = crate::systems::npcs::npc::data::MerchantGoods {
        id: 7,
        items: Vec::new(),
    };
    first.add_item_to_stock(STACK, 0);
    let mut second = crate::systems::npcs::npc::data::MerchantGoods {
        id: 8,
        items: Vec::new(),
    };
    second.add_item_to_stock(POWDER, 0);
    d.systems.npcs.npc.goods.insert(7, first);
    d.systems.npcs.npc.goods.insert(8, second);
    d.systems.npcs.npc.templates.insert(
        1234,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: 1234,
            merchant: true,
            merchant_pack_ids: vec![7, 8],
            ..Default::default()
        },
    );
    let mut w = world_with_data(d);
    crate::systems::npcs::bridges::install_economy(&w.server);
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    w.pa.data().money = 1000;

    // The item of the first pack is sold; so is the item of the last pack.
    buy_grade(&mut w, STACK, 0, 1, shop_currency_type::MONEY).await;
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    buy_grade(&mut w, POWDER, 0, 1, shop_currency_type::MONEY).await;
    assert_eq!(Inv::of(&w.pa).items_count(POWDER, -1), 1);
    assert_eq!(w.pa.data().money, 1000 - 10 - 100);
}

// ---------------------------------------------------------------------------------------------
// Siege-time goods gate (Housing.SiegeShopGate, WP-SD4a)
// ---------------------------------------------------------------------------------------------

const SIEGE_ZONE_GROUP: u32 = 33;
const SIEGE_DEFENSE_EXPEDITION: u32 = 10;
const SIEGE_ATTACK_EXPEDITION: u32 = 20;
const SIEGE_HOUSE_TEMPLATE: u32 = 139;
const SIEGE_GUARD_TOWER_SETTING: u32 = 6;
const SIEGE_DEFENSE_PACK: u32 = 224;
const SIEGE_ATTACK_PACK: u32 = 225;

fn spawn_zone_npc(w: &mut World, obj_id: u32, template: u32, x: f32) -> NpcRef {
    let mut n = Npc::new(template);
    n.unit.transform.zone_id = crate::test_support::TEST_ZONE;
    n.unit.transform.set_xyz(x, 100.0, 100.0);
    let n = Entity::new(obj_id, n);
    let world = w.server.world.world(0).unwrap();
    world.spawn(&n, &w.server.view_ctx());
    w.a.drain();
    w.b.drain();
    n
}

/// A merchant selling the two inferred siege packs and an owned territory in the NPC's zone.
fn siege_shop_world(gate: bool) -> World {
    let mut d = data();
    d.items.templates.get_mut(&POWDER).unwrap().honor_price = 4;
    d.zones.zones.insert(
        crate::test_support::TEST_ZONE,
        Zone {
            zone_key: crate::test_support::TEST_ZONE,
            group_id: SIEGE_ZONE_GROUP,
            ..Default::default()
        },
    );
    d.systems.housing.add_template(HousingTemplate {
        id: SIEGE_HOUSE_TEMPLATE,
        guard_tower_setting_id: SIEGE_GUARD_TOWER_SETTING,
        ..Default::default()
    });
    d.systems.housing.castles.guard_tower_settings.insert(
        SIEGE_GUARD_TOWER_SETTING,
        GuardTowerSetting {
            id: SIEGE_GUARD_TOWER_SETTING,
            radius_dominion: 200,
            ..Default::default()
        },
    );
    for (pack, item) in [(SIEGE_DEFENSE_PACK, STACK), (SIEGE_ATTACK_PACK, POWDER)] {
        let mut goods = crate::systems::npcs::npc::data::MerchantGoods {
            id: pack,
            items: Vec::new(),
        };
        goods.add_item_to_stock(item, 0);
        d.systems.npcs.npc.goods.insert(pack, goods);
    }
    d.systems.npcs.npc.templates.insert(
        1234,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: 1234,
            merchant: true,
            merchant_pack_ids: vec![SIEGE_DEFENSE_PACK, SIEGE_ATTACK_PACK],
            ..Default::default()
        },
    );
    let mut w = world_with_data(d);
    w.server
        .config
        .update(|config| config.housing.siege_shop_gate = gate);
    crate::systems::npcs::bridges::install_economy(&w.server);
    spawn_zone_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    let mut house = House::new(61, SIEGE_HOUSE_TEMPLATE);
    house.current_step = -1;
    house.unit.transform.zone_id = crate::test_support::TEST_ZONE;
    house.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let tower = Entity::new(0x200, house);
    w.server.systems.housing.manager.insert(&tower);
    w.server
        .world
        .world(0)
        .unwrap()
        .spawn(&tower, &w.server.view_ctx());
    let (_, identity) = dominion_runtime::tower_identity(&w.server, &tower).unwrap();
    w.server
        .systems
        .housing
        .dominions
        .declare_unowned_with(
            SIEGE_ZONE_GROUP,
            identity,
            SIEGE_DEFENSE_EXPEDITION,
            chrono::Utc::now().timestamp() - 1_000,
            || true,
        )
        .unwrap();
    w
}

fn schedule_siege_at(w: &World, starts_in: i64) {
    let now = chrono::Utc::now().timestamp();
    w.server
        .systems
        .housing
        .dominions
        .schedule_siege(
            SIEGE_ZONE_GROUP,
            SIEGE_ATTACK_EXPEDITION,
            now + starts_in,
            now + 3_600,
            now - 20,
        )
        .unwrap();
}

fn register_siege_side(w: &World, character_id: u32, expedition: u32, side: SiegeSide) {
    w.server
        .systems
        .housing
        .dominions
        .register_participant(
            SIEGE_ZONE_GROUP,
            character_id,
            expedition,
            side,
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
}

#[tokio::test]
async fn siege_shop_gate_sells_224_to_defenders_and_225_to_attackers_in_warmup_and_siege() {
    for starts_in in [-10, 100] {
        let mut w = siege_shop_world(true);
        schedule_siege_at(&w, starts_in);
        register_siege_side(&w, 1, SIEGE_ATTACK_EXPEDITION, SiegeSide::Attacker);
        register_siege_side(&w, 2, SIEGE_DEFENSE_EXPEDITION, SiegeSide::Defender);
        w.pa.data().honor_point = 100;
        w.pb.data().honor_point = 100;

        // The attacker (Alice, character 1) buys the attack pack (225, POWDER).
        buy_grade(&mut w, POWDER, 0, 1, shop_currency_type::HONOR).await;
        assert_eq!(Inv::of(&w.pa).items_count(POWDER, -1), 1);
        assert_eq!(w.pa.data().honor_point, 96);
        // The defense pack (224, STACK) is refused for the attacker without a charge.
        buy_grade(&mut w, STACK, 0, 1, shop_currency_type::HONOR).await;
        assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
        assert_eq!(w.pa.data().honor_point, 96);

        // The defender (Bob, character 2) buys the defense pack; the attack pack is refused.
        buy_grade_as(&mut w, true, STACK, 0, 1, shop_currency_type::HONOR).await;
        assert_eq!(Inv::of(&w.pb).items_count(STACK, -1), 1);
        assert_eq!(w.pb.data().honor_point, 97);
        buy_grade_as(&mut w, true, POWDER, 0, 1, shop_currency_type::HONOR).await;
        assert_eq!(Inv::of(&w.pb).items_count(POWDER, -1), 0);
        assert_eq!(w.pb.data().honor_point, 97);
    }
}

#[tokio::test]
async fn siege_shop_gate_refuses_registered_sides_outside_warmup_and_siege() {
    let mut w = siege_shop_world(true);
    schedule_siege_at(&w, -10);
    register_siege_side(&w, 1, SIEGE_ATTACK_EXPEDITION, SiegeSide::Attacker);
    register_siege_side(&w, 2, SIEGE_DEFENSE_EXPEDITION, SiegeSide::Defender);
    // Cancelling keeps the roster but leaves the territory in peace (period 5).
    w.server
        .systems
        .housing
        .dominions
        .cancel_siege(SIEGE_ZONE_GROUP, chrono::Utc::now().timestamp())
        .unwrap();
    w.pa.data().honor_point = 100;
    w.pb.data().honor_point = 100;
    for second_player in [false, true] {
        for item in [STACK, POWDER] {
            buy_grade_as(&mut w, second_player, item, 0, 1, shop_currency_type::HONOR).await;
        }
    }
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pa).items_count(POWDER, -1), 0);
    assert_eq!(Inv::of(&w.pb).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pb).items_count(POWDER, -1), 0);
    assert_eq!(w.pa.data().honor_point, 100);
    assert_eq!(w.pb.data().honor_point, 100);
}

#[tokio::test]
async fn siege_shop_gate_off_keeps_both_packs_selling_at_any_time_to_anyone() {
    let mut w = siege_shop_world(false);
    w.pa.data().honor_point = 100;
    w.pb.data().honor_point = 100;
    // No siege is scheduled and neither player is a participant.
    buy_grade(&mut w, STACK, 0, 1, shop_currency_type::HONOR).await;
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 1);
    assert_eq!(w.pa.data().honor_point, 97);
    buy_grade_as(&mut w, true, POWDER, 0, 1, shop_currency_type::HONOR).await;
    assert_eq!(Inv::of(&w.pb).items_count(POWDER, -1), 1);
    assert_eq!(w.pb.data().honor_point, 96);
}

#[tokio::test]
async fn siege_shop_gate_refuses_non_participants_during_the_siege() {
    let mut w = siege_shop_world(true);
    schedule_siege_at(&w, -10);
    w.pa.data().honor_point = 100;
    w.pb.data().honor_point = 100;
    for second_player in [false, true] {
        for item in [STACK, POWDER] {
            buy_grade_as(&mut w, second_player, item, 0, 1, shop_currency_type::HONOR).await;
        }
    }
    assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pa).items_count(POWDER, -1), 0);
    assert_eq!(Inv::of(&w.pb).items_count(STACK, -1), 0);
    assert_eq!(Inv::of(&w.pb).items_count(POWDER, -1), 0);
    assert_eq!(w.pa.data().honor_point, 100);
    assert_eq!(w.pb.data().honor_point, 100);
}

#[tokio::test]
async fn shop_catalog_grades_reject_unlisted_grade_without_charge() {
    for currency in [shop_currency_type::MONEY, shop_currency_type::HONOR] {
        for grade in [0, 11] {
            let mut d = data();
            d.items.templates.get_mut(&STACK).unwrap().gradable = true;
            let mut w = catalog_world(d, &[(STACK, 4)]);
            w.pa.data().money = 1000;
            w.pa.data().honor_point = 100;
            buy_grade(&mut w, STACK, grade, 1, currency).await;
            assert!(Inv::of(&w.pa).bag().lock().unwrap().items.is_empty());
            let c = w.pa.data();
            assert_eq!((c.money, c.honor_point), (1000, 100));
        }
    }
}

#[tokio::test]
async fn shop_catalog_grades_respect_fixed_and_basic_templates() {
    // Stock grade zero is basic for gradable gear; it is not "any grade" or "use default".
    // Fixed grades are sent after normalization by native 1.2 sub_397CD260.
    for (stock, fixed, gradable, requested, accepted) in [
        (0, -1, true, 0, true),
        (0, -1, true, 11, false),
        (0, 2, false, 2, true),
        (0, 2, false, 0, false),
        (4, 2, false, 2, true),
        (4, 2, false, 4, false),
        (0, 2, true, 2, true),
        (0, 2, true, 0, false),
        (4, -1, false, 0, true),
        (4, -1, false, 4, false),
    ] {
        let mut d = data();
        let t = d.items.templates.get_mut(&STACK).unwrap();
        t.fixed_grade = fixed;
        t.gradable = gradable;
        let mut w = catalog_world(d, &[(STACK, stock)]);
        w.pa.data().money = 100;
        buy_grade(&mut w, STACK, requested, 1, shop_currency_type::MONEY).await;
        assert_eq!(Inv::of(&w.pa).items_count(STACK, -1), i32::from(accepted));
        assert_eq!(w.pa.data().money, if accepted { 90 } else { 100 });
        if accepted {
            assert_eq!(
                item_at(&w.pa, slot_type::INVENTORY, 0).unwrap().grade,
                requested
            );
        }
    }
}

#[tokio::test]
async fn shop_catalog_grades_full_bag_rolls_back() {
    let mut d = data();
    d.items.templates.get_mut(&STACK).unwrap().gradable = true;
    let mut w = catalog_world(d, &[(STACK, 0), (STACK, 4)]);
    w.pa.data().honor_point = 100;
    let bag = Inv::of(&w.pa).bag();
    bag.lock().unwrap().set_container_size(1);
    give(&w, &w.pa, slot_type::INVENTORY, STACK, 5, 0);
    let before = bag.lock().unwrap().items.clone();
    // A partial basic stack cannot take a higher-grade purchase.
    buy_grade(&mut w, STACK, 4, 1, shop_currency_type::HONOR).await;
    assert_eq!(bag.lock().unwrap().items, before);
    assert_eq!(w.pa.data().honor_point, 100);
    // The refused purchase tells the client why: the bag is full.
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            err_bytes(
                super::error_message::BAG_FULL,
                super::error_message::BAG_FULL,
                0,
                true
            )
        )]
    );
}

#[tokio::test]
async fn shop_checkout_rejects_money_only_and_mixed_currency_shortages() {
    for (money, honor, vocation, purchases) in [
        (5, 100, 100, vec![(STACK, 1, 0)]),
        (100, 0, 100, vec![(STACK, 1, 0), (STACK, 1, 1)]),
        (100, 100, 0, vec![(STACK, 1, 2)]),
    ] {
        let mut d = data();
        d.items
            .templates
            .get_mut(&STACK)
            .unwrap()
            .living_point_price = 4;
        let mut w = world_with_data(d);
        w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
        spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
        {
            let mut c = w.pa.data();
            c.money = money;
            c.honor_point = honor;
            c.vocation_point = vocation;
        }
        let before = Inv::of(&w.pa).bag().lock().unwrap().items.clone();
        run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &purchases, &[]));
        let c = w.pa.data();
        assert_eq!(
            (c.money, c.honor_point, c.vocation_point),
            (money, honor, vocation)
        );
        drop(c);
        assert_eq!(Inv::of(&w.pa).bag().lock().unwrap().items, before);
        assert!(w.a.drain().is_empty());
    }
}

#[tokio::test]
async fn shop_checkout_rejects_invalid_counts_unpriced_currency_and_overflow() {
    for (price, honor, purchases) in [
        (10, 3, vec![(STACK, 1, 255)]),
        (10, 0, vec![(STACK, 1, 1)]),
        (10, 3, vec![(STACK, 1, 2)]),
        (10, 3, vec![(STACK, 0, 0)]),
        (10, 3, vec![(STACK, -1, 0)]),
        (10, 3, vec![(STACK, i32::MAX, 0)]),
        (1_500_000_000, 3, vec![(STACK, 1, 0), (STACK, 1, 0)]),
    ] {
        let mut d = data();
        let template = d.items.templates.get_mut(&STACK).unwrap();
        template.price = price;
        template.honor_price = honor;
        let mut w = world_with_data(d);
        w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
        spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
        {
            let mut c = w.pa.data();
            c.money = 4_000_000_000;
            c.honor_point = 100;
            c.vocation_point = 100;
        }
        let pa = w.pa.clone();
        give(&w, &pa, 2, STACK, 4, 0);
        let before = Inv::of(&pa).bag().lock().unwrap().items.clone();
        run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &purchases, &[]));
        let c = pa.data();
        assert_eq!(
            (c.money, c.honor_point, c.vocation_point),
            (4_000_000_000, 100, 100)
        );
        drop(c);
        assert_eq!(Inv::of(&pa).bag().lock().unwrap().items, before);
        assert!(w.a.drain().is_empty());
    }
}

#[tokio::test]
async fn shop_checkout_capacity_failure_rolls_back_all_goods_and_buyback() {
    for with_buyback in [false, true] {
        let mut w = world();
        w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
        spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
        w.pa.data().money = 1000;
        Inv::of(&w.pa).bag().lock().unwrap().set_container_size(1);
        let buyback = w.server.systems.economy.store.buyback(w.pa.character_id);
        let back = w
            .server
            .items
            .create(
                STACK,
                1,
                0,
                1,
                &w.server.game_data.items,
                &w.server.ids.item,
                0,
            )
            .unwrap();
        buyback
            .lock()
            .unwrap()
            .add_new_item(back, 0, &w.server.game_data.items)
            .unwrap();
        let before_back = buyback.lock().unwrap().items.clone();
        let purchases = if with_buyback {
            vec![(STACK, 1, 0)]
        } else {
            vec![(STACK, 20, 0), (STACK, 1, 0)]
        };
        let indices = if with_buyback { vec![0] } else { vec![] };
        run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &purchases, &indices));
        assert!(Inv::of(&w.pa).bag().lock().unwrap().items.is_empty());
        assert_eq!(buyback.lock().unwrap().items, before_back);
        assert_eq!(w.pa.data().money, 1000);
        // The capacity failure of the goods is a full bag; a vanished buy-back item is silent.
        assert_eq!(
            w.a.drain().len(),
            usize::from(!with_buyback),
            "BAG_FULL only for the goods"
        );
    }
}

#[tokio::test]
async fn shop_checkout_mixed_payment_and_configured_free_goods_succeed() {
    let mut d = data();
    d.items
        .templates
        .get_mut(&STACK)
        .unwrap()
        .living_point_price = 4;
    let mut w = world_with_data(d);
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    {
        let mut c = w.pa.data();
        c.money = 100;
        c.honor_point = 10;
        c.vocation_point = 10;
    }
    run::<CSBuyItemsPacket>(
        &mut w,
        0,
        &buy_body(
            MERCHANT_NPC,
            &[(STACK, 1, 0), (STACK, 1, 1), (STACK, 1, 2)],
            &[],
        ),
    );
    assert_eq!(item_at(&w.pa, 2, 0).unwrap().count, 3);
    let c = w.pa.data();
    assert_eq!((c.money, c.honor_point, c.vocation_point), (90, 7, 6));
    drop(c);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 7);
    assert_eq!(
        frames[3],
        (
            sc::SCGamePointChangedPacket,
            bytes(&SCGamePointChangedPacket {
                kind: 0,
                amount: -3
            })
        )
    );
    assert_eq!(
        frames[4],
        (
            sc::SCGamePointChangedPacket,
            bytes(&SCGamePointChangedPacket {
                kind: 1,
                amount: -4
            })
        )
    );
    assert_eq!(
        frames[5],
        (
            sc::SCItemTaskSuccessPacket,
            task_body(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-10)])
        )
    );

    let mut d = data();
    d.items.templates.get_mut(&STACK).unwrap().price = 0;
    let mut w = world_with_data(d);
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1, 0)], &[]));
    assert_eq!(item_at(&w.pa, 2, 0).unwrap().count, 1);
    assert_eq!(w.pa.data().money, 0);
}

#[tokio::test]
async fn shop_checkout_vocation_earnings_bonuses_do_not_change_configured_price() {
    use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
    // Additive gains could reverse a debit; multipliers could overcharge or
    // discount it. Neither changes the price shown by the 1.2 shop UI.
    for (add, multiplier) in [(0, 0), (100, 0), (0, 200), (0, -50), (100, 200)] {
        for available in [49, 50, 100] {
            let mut d = data();
            d.items
                .templates
                .get_mut(&STACK)
                .unwrap()
                .living_point_price = 50;
            let mut w = world_with_data(d);
            w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
            spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
            {
                let mut c = w.pa.data();
                c.vocation_point = available;
                for (index, attribute, value) in [
                    (1, UnitAttribute::LivingPointGain, add),
                    (2, UnitAttribute::LivingPointGainMul, multiplier),
                ] {
                    c.unit.bonuses.add_bonus(
                        index,
                        Bonus {
                            template: BonusTemplate {
                                attribute,
                                modifier_type: UnitModifierType::Value,
                                value,
                                linear_level_bonus: 0,
                            },
                            value,
                        },
                    );
                }
            }
            run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1, 2)], &[]));
            assert_eq!(w.pa.data().money, 0);
            assert_eq!(w.pa.data().honor_point, 0);
            if available < 50 {
                assert_eq!(w.pa.data().vocation_point, available);
                assert!(Inv::of(&w.pa).bag().lock().unwrap().items.is_empty());
                assert!(w.a.drain().is_empty());
            } else {
                assert_eq!(w.pa.data().vocation_point, available - 50);
                assert_eq!(item_at(&w.pa, 2, 0).unwrap().count, 1);
                let point_changes: Vec<_> =
                    w.a.drain()
                        .into_iter()
                        .filter(|(opcode, _)| *opcode == sc::SCGamePointChangedPacket)
                        .collect();
                assert_eq!(
                    point_changes,
                    vec![(
                        sc::SCGamePointChangedPacket,
                        bytes(&SCGamePointChangedPacket {
                            kind: 1,
                            amount: -50,
                        }),
                    )]
                );
            }
        }
    }
}

#[tokio::test]
async fn buy_sell_and_buy_back() {
    let mut w = world();
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    let pa = w.pa.clone();
    pa.data().money = 1000;

    // Buy 3 x STACK (price 10) + an item the merchant does not sell (ignored).
    run::<CSBuyItemsPacket>(
        &mut w,
        0,
        &buy_body(MERCHANT_NPC, &[(STACK, 3, 0), (PRODUCT, 1, 0)], &[]),
    );
    let bought = item_at(&pa, 2, 0).unwrap();
    assert_eq!((bought.template_id, bought.count), (STACK, 3));
    assert_eq!(pa.data().money, 970);
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::STORE_BUY, vec![ItemTask::add(&bought)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-30)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::STORE_BUY, vec![])
            ),
        ]
    );

    // Honor currency: points packet instead of money.
    pa.data().honor_point = 100;
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 2, 1)], &[]));
    assert_eq!(pa.data().honor_point, 94);
    let f = w.a.drain();
    assert_eq!(
        f.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![
            sc::SCItemTaskSuccessPacket,
            sc::SCGamePointChangedPacket,
            sc::SCItemTaskSuccessPacket
        ]
    );
    assert_eq!(
        f[1].1,
        bytes(&SCGamePointChangedPacket {
            kind: 0,
            amount: -6
        })
    );

    // Sell the stack (refund 5 * 100% * 5 units = 25).
    let stack = item_at(&pa, 2, 0).unwrap();
    assert_eq!(stack.count, 5);
    let mut s = PacketStream::new();
    s.write_bc(MERCHANT_NPC)
        .write_bc(0)
        .write_u8(1)
        .write_u8(2)
        .write_u8(0)
        .write_u64(stack.id)
        .write_u32(0);
    run::<CSSellItemsPacket>(&mut w, 0, s.as_bytes());
    assert!(item_at(&pa, 2, 0).is_none());
    assert_eq!(pa.data().money, 995);
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::STORE_SELL, vec![ItemTask::remove_slot(&stack)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(25)])
            ),
        ]
    );
    let bb = w.server.systems.economy.store.buyback(1);
    assert_eq!(bb.lock().unwrap().items.len(), 1);
    assert_eq!(bb.lock().unwrap().items[0].slot_type, slot_type::NONE);

    // List: renumbered, sent with Item.Write.
    let mut s = PacketStream::new();
    s.write_bc(MERCHANT_NPC);
    run::<CSListSoldItemPacket>(&mut w, 0, s.as_bytes());
    let sold = bb.lock().unwrap().items.clone();
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, sc::SCSoldItemListPacket);
    assert_eq!(f[0].1, bytes(&SCSoldItemListPacket::new(sold.clone())));
    assert_eq!(sold[0].slot, 0);

    // Buy it back (index 0): back in the bag for the refund price.
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[], &[0]));
    let back = item_at(&pa, 2, 0).expect("bought back");
    assert_eq!(back.id, stack.id);
    assert_eq!(pa.data().money, 970);
    assert!(bb.lock().unwrap().items.is_empty());
    let f = w.a.drain();
    assert_eq!(
        f,
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::STORE_BUY, vec![ItemTask::add(&back)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(
                    task_type::STORE_BUY,
                    vec![ItemTask::ItemRemoveSlot {
                        item_id: stack.id,
                        slot_type: slot_type::NONE,
                        slot: 0
                    }]
                )
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-25)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::STORE_BUY, vec![ItemTask::buyback(&back)])
            ),
        ]
    );

    // Too far from the merchant.
    spawn_npc(&mut w, MERCHANT_NPC + 1, 1234, 120.0);
    run::<CSBuyItemsPacket>(
        &mut w,
        0,
        &buy_body(MERCHANT_NPC + 1, &[(STACK, 1, 0)], &[]),
    );
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(154, 154, 0, true))]
    );

    // Not a merchant: ignored.
    spawn_npc(&mut w, MERCHANT_NPC + 2, 99, 101.0);
    run::<CSBuyItemsPacket>(
        &mut w,
        0,
        &buy_body(MERCHANT_NPC + 2, &[(STACK, 1, 0)], &[]),
    );
    assert!(w.a.drain().is_empty());

    // Leaving the world wipes the buy-back items.
    let mut s = PacketStream::new();
    s.write_bc(MERCHANT_NPC)
        .write_bc(0)
        .write_u8(1)
        .write_u8(2)
        .write_u8(0)
        .write_u64(back.id)
        .write_u32(0);
    run::<CSSellItemsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(bb.lock().unwrap().items.len(), 1);
    w.server.systems.economy.store.wipe_buyback(&w.server, 1);
    assert!(bb.lock().unwrap().items.is_empty());
}

fn interaction_body(npc: u32, obj: u32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(npc)
        .write_bc(obj)
        .write_i32(0)
        .write_i32(0)
        .write_u8(1)
        .write_i32(0);
    s.into_bytes()
}

/// Tester report: the store window opens within 3 m of the NPC's shape plus the 2 m tolerance,
/// but buy/sell/buy-back compared centres at 3 m, so a large merchant refused every transaction
/// while its window stayed open. A 2 m-radius merchant beyond the flat centre reach but inside
/// the shape reach: the window opens and buy, sell and buy-back succeed; just beyond the shape
/// reach the window stays shut and buy and sell are refused.
#[tokio::test]
async fn shop_transactions_share_the_interaction_window_reach() {
    const BIG_MODEL: u32 = 9001;
    const NPC_RADIUS: f32 = 2.0;
    let mut d = data();
    d.systems.skills.model_radii.insert(BIG_MODEL, NPC_RADIUS);
    let mut w = catalog_world(d, &[(STACK, 0)]);
    // Compute from the configured reach: the 3 m client reach plus the interaction tolerance.
    let reach = crate::systems::npcs::npc::reach::interaction_reach(&w.server);
    // Edge distance = centre - NPC radius (the player model has no actor radius in this data).
    let success_at = 100.0 + reach + 1.0;
    let refusal_at = 100.0 + reach + 3.0;
    assert!(
        success_at - 100.0 > reach && success_at - 100.0 - NPC_RADIUS <= reach,
        "the success case must need the shape edge, not a flat centre reach"
    );
    assert!(
        refusal_at - 100.0 - NPC_RADIUS > reach,
        "the refusal case must be beyond the shape reach"
    );
    {
        let world = w.server.world.world(0).unwrap();
        let npc = world.npc(MERCHANT_NPC).unwrap();
        let mut n = npc.data();
        n.unit.model_id = BIG_MODEL;
        n.unit.scale = 1.0;
        n.unit.transform.set_xyz(success_at, 100.0, 100.0);
    }
    w.a.drain();
    w.b.drain();
    let pa = w.pa.clone();
    pa.data().money = 1000;

    // The window opens at the centre distance, measured from the shape edge.
    run::<CSStartInteractionPacket>(&mut w, 0, &interaction_body(MERCHANT_NPC, 0x100));
    assert_eq!(w.a.drain_ops(), vec![sc::SCNpcInteractionSkillListPacket]);

    // Buy, sell and buy back from the open window.
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 3, 0)], &[]));
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 3);
    assert_eq!(pa.data().money, 970);
    w.a.drain();

    let stack = item_at(&pa, 2, 0).unwrap();
    run::<CSSellItemsPacket>(
        &mut w,
        0,
        &sell_body(MERCHANT_NPC, &[(slot_type::INVENTORY, 0, stack.id)]),
    );
    assert!(item_at(&pa, 2, 0).is_none());
    assert_eq!(pa.data().money, 985);
    w.a.drain();

    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[], &[0]));
    assert_eq!(item_at(&pa, 2, 0).unwrap().id, stack.id);
    assert_eq!(pa.data().money, 970);
    w.a.drain();

    // Just beyond the shape reach: the window stays shut and buy and sell are refused.
    {
        let world = w.server.world.world(0).unwrap();
        world
            .npc(MERCHANT_NPC)
            .unwrap()
            .data()
            .unit
            .transform
            .set_xyz(refusal_at, 100.0, 100.0);
    }
    run::<CSStartInteractionPacket>(&mut w, 0, &interaction_body(MERCHANT_NPC, 0x100));
    assert!(w.a.drain().is_empty());

    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(STACK, 1, 0)], &[]));
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(154, 154, 0, true))]
    );
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 3);
    assert_eq!(pa.data().money, 970);

    run::<CSSellItemsPacket>(
        &mut w,
        0,
        &sell_body(MERCHANT_NPC, &[(slot_type::INVENTORY, 0, stack.id)]),
    );
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(154, 154, 0, true))]
    );
    assert_eq!(item_at(&pa, 2, 0).unwrap().id, stack.id);
    assert_eq!(pa.data().money, 970);
}

/// Real data: NPC 1000 is a `merchants` pack 140 seller with a 0.55 m actor radius. At 4 m
/// centre (3.45 m from the shape edge) the window opens and the checkout now passes; the old
/// flat 3 m centre check refused it. `AAEMU_COMPACT_DB=... cargo test -p aaemu-game
/// real_compact_db_merchant_1000 -- --ignored`.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
async fn real_compact_db_merchant_1000_sells_within_the_interaction_reach() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut data = GameData::empty();
    data.items = crate::data::ItemData::load(&conn).unwrap();
    data.systems.npcs.npc =
        crate::systems::npcs::npc::data::NpcData::load_conn(&conn, "en_us").unwrap();
    data.systems.skills.model_radii =
        crate::systems::skills::data::misc::load_actor_model_radii(&conn).unwrap();
    let (model_id, scale) = {
        let template = data.systems.npcs.npc.template(1000).unwrap();
        assert!(template.merchant);
        assert!(template.merchant_pack_ids.contains(&140));
        (template.model_id, template.scale)
    };

    let mut w = world_with_data(data);
    crate::systems::npcs::bridges::install_economy(&w.server);
    let mut n = Npc::new(1000);
    n.unit.model_id = model_id;
    n.unit.scale = scale;
    n.unit.transform.set_xyz(104.0, 100.0, 100.0);
    let world = w.server.world.world(0).unwrap();
    world.spawn(Entity::new(MERCHANT_NPC, n), &w.server.view_ctx());
    w.a.drain();
    let pa = w.pa.clone();
    pa.data().money = 10_000;
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(MERCHANT_NPC, &[(8582, 1, 0)], &[]));
    assert_eq!(item_at(&pa, 2, 0).unwrap().template_id, 8582);
    assert_eq!(pa.data().money, 5_000);
}

/// A doodad shop is bought from its own open store window too: its placement radius stands in
/// for the shape edge. Beyond the flat centre reach but inside the shape reach the stall sells;
/// just beyond the shape reach the buy is refused.
#[tokio::test]
async fn doodad_shop_buy_uses_the_interaction_window_reach() {
    const STALL_RADIUS: f32 = 2.0;
    let mut d = with_doodad_shop(7);
    d.systems.npcs.doodad.templates.insert(
        1234,
        DoodadTemplate {
            id: 1234,
            sim_radius: (STALL_RADIUS * 1000.0) as i32,
            ..Default::default()
        },
    );
    let mut w = world_with_data(d);
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    let reach = crate::systems::npcs::npc::reach::interaction_reach(&w.server);
    let success_at = 100.0 + reach + 1.0;
    let refusal_at = 100.0 + reach + 3.0;
    assert!(
        success_at - 100.0 > reach && success_at - 100.0 - STALL_RADIUS <= reach,
        "the success case must need the placement radius, not a flat centre reach"
    );
    assert!(
        refusal_at - 100.0 - STALL_RADIUS > reach,
        "the refusal case must be beyond the shape reach"
    );
    let stall = spawn_doodad(&mut w, 0x600, 1234, 42, success_at);
    let pa = w.pa.clone();
    pa.data().money = 1000;
    run::<CSBuyItemsPacket>(&mut w, 0, &doodad_buy_body(0x600, &[(STACK, 2, 0)], &[]));
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 2);
    assert_eq!(pa.data().money, 980);
    w.a.drain();

    stall.data().transform.set_xyz(refusal_at, 100.0, 100.0);
    run::<CSBuyItemsPacket>(&mut w, 0, &doodad_buy_body(0x600, &[(STACK, 1, 0)], &[]));
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(154, 154, 0, true))]
    );
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 2);
    assert_eq!(pa.data().money, 980);
}

#[tokio::test]
async fn expand_expert_flow() {
    let mut w = world();
    let pa = w.pa.clone();
    pa.data().vocation_point = 5;
    run::<CSExpandExpertPacket>(&mut w, 0, &[]);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(687, 687, 0, true))]
    );
    pa.data().vocation_point = 20;
    run::<CSExpandExpertPacket>(&mut w, 0, &[]);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(686, 686, 0, true))]
    );
    give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    run::<CSExpandExpertPacket>(&mut w, 0, &[]);
    let rest = item_at(&pa, 2, 0).unwrap();
    assert_eq!(rest.count, 1);
    assert_eq!(pa.data().expanded_expert, 1);
    // The vocation cost is charged (the C# added it).
    assert_eq!(pa.data().vocation_point, 10);
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCGamePointChangedPacket,
                bytes(&SCGamePointChangedPacket {
                    kind: 1,
                    amount: -10
                })
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(
                    task_type::EXPAND_EXPERT,
                    vec![ItemTask::count_update(&rest, -2)]
                )
            ),
            (sc::SCExpertExpandedPacket, vec![1]),
        ]
    );
    // No further step: the C# TODO's silent return now answers `store_unknown_error` (228).
    run::<CSExpandExpertPacket>(&mut w, 0, &[]);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(228, 228, 0, true))]
    );
}

#[tokio::test]
async fn convert_item_look_flow() {
    let mut w = world();
    let pa = w.pa.clone();
    let base = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 0);
    let look = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 1);
    let mut s = PacketStream::new();
    s.write_u64(base).write_u64(look);
    // No powder: NotEnoughRequiredItem(type = powder id)
    run::<store::CSConvertItemLookPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(722, 722, POWDER, true))]
    );
    give(&w, &pa, slot_type::INVENTORY, POWDER, 5, 2);
    run::<store::CSConvertItemLookPacket>(&mut w, 0, s.as_bytes());
    let converted = item_at(&pa, 2, 0).unwrap();
    assert_eq!(converted.details.image_item_template_id, SWORD);
    let look_item = Item {
        id: look,
        slot: 1,
        ..converted.clone()
    };
    let powder = item_at(&pa, 2, 2).unwrap();
    assert_eq!(powder.count, 3);
    assert!(item_at(&pa, 2, 1).is_none());
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                task_body(
                    task_type::SKILL_REAGENTS,
                    vec![ItemTask::count_update(&powder, -2)]
                )
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(
                    task_type::CONVERT_ITEM_LOOK,
                    vec![ItemTask::update(&converted)]
                )
            ),
            (
                sc::SCItemTaskSuccessPacket,
                task_body(
                    task_type::SKILL_REAGENTS,
                    vec![ItemTask::remove_slot(&look_item)]
                )
            ),
        ]
    );
    // Unknown item: FailedToUseItem
    let mut s = PacketStream::new();
    s.write_u64(base).write_u64(look);
    run::<store::CSConvertItemLookPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(207, 207, 0, true))]
    );
}

#[tokio::test]
async fn convert_item_look_needs_the_full_reagent_count() {
    let mut w = world();
    let pa = w.pa.clone();
    let base = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 0);
    let look = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 1);
    // The conversion costs 2 powder: one is not enough and nothing is consumed.
    give(&w, &pa, slot_type::INVENTORY, POWDER, 1, 2);
    let mut s = PacketStream::new();
    s.write_u64(base).write_u64(look);
    w.a.drain();
    run::<store::CSConvertItemLookPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(722, 722, POWDER, true))]
    );
    assert_eq!(item_at(&pa, 2, 2).unwrap().count, 1);
    assert_eq!(
        item_at(&pa, 2, 0).unwrap().details.image_item_template_id,
        0
    );
    assert!(item_at(&pa, 2, 1).is_some());
}

#[tokio::test]
async fn craft_batch_count_is_clamped() {
    let mut w = world();
    install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::INVENTORY, STACK, 6, 0);
    execute_craft(&mut w, CRAFT, i32::MIN);
    assert_eq!(w.server.systems.economy.craft.state(1).count, 1);
    craft::cancel_craft(&w.server, &pa);
    execute_craft(&mut w, CRAFT, i32::MAX);
    assert_eq!(
        w.server.systems.economy.craft.state(1).count,
        craft::MAX_CRAFT_BATCH
    );
}

// ---------------------------------------------------------------------------------------------
// Specialty
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Mailer(Mutex<Vec<SpecialtyMail>>);
impl StoreHooks for Mailer {
    fn npc_specialty_coin_id(&self, server: &GameServer, npc: &NpcRef) -> u32 {
        let template_id = npc.data().unit.template_id;
        server
            .game_data
            .systems
            .npcs
            .npc
            .template(template_id)
            .map_or(0, |t| t.specialty_coin_id)
    }

    fn send_specialty_mail(
        &self,
        _server: &GameServer,
        _seller: &PlayerRef,
        mail: &SpecialtyMail,
    ) -> bool {
        self.0.lock().unwrap().push(*mail);
        true
    }
}

#[tokio::test]
async fn sell_trade_pack_to_trader() {
    let mut w = world();
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer.clone());
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();

    // Ratio without a pack: 0
    let mut s = PacketStream::new();
    s.write_u32(0);
    run::<CSSpecialtyRatioPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCSpecialtyRatioPacket, 0i32.to_le_bytes().to_vec())]
    );

    // Not enough labor
    pa.data().labor_power = 10;
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(29, 29, 0, true))]
    );

    pa.data().labor_power = 100;
    // ChangeLabor indexes Actabilities[31] (C# KeyNotFoundException without it).
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 0,
        });
    let pack = give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    let mut s = PacketStream::new();
    s.write_u32(0);
    run::<CSSpecialtyRatioPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCSpecialtyRatioPacket, 130i32.to_le_bytes().to_vec())]
    );

    let pack_item = Inv::of(&pa).equipped(equip_slot::BACKPACK).unwrap();
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    // base = floor(5000 * 1.2) + refund 1000 = 7000; * 130% = 9100; + 5% = 9555
    let mails = mailer.0.lock().unwrap().clone();
    assert_eq!(
        mails,
        vec![SpecialtyMail {
            seller_character_id: 1,
            crafter_id: 0,
            trade_pack_template_id: PACK,
            trade_rate: 130,
            item_reward_template_id: crate::data::items::ITEM_COINS,
            item_count_base: 7000,
            item_count_bonus: 0,
            item_count_for_seller: 9555,
            item_count_for_crafter: 0,
            interest_rate: 5,
            for_crafter: false,
        }]
    );
    assert!(Inv::of(&pa).equipped(equip_slot::BACKPACK).is_none());
    assert_eq!(pa.data().labor_power, 40);
    let f = w.a.drain();
    assert_eq!(
        f,
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                task_body(
                    task_type::SELL_BACKPACK,
                    vec![ItemTask::remove_slot(&Item {
                        id: pack,
                        ..pack_item
                    })]
                )
            ),
            // ConsumeItem always sends its own (here empty) task list.
            (
                sc::SCItemTaskSuccessPacket,
                task_body(task_type::SELL_BACKPACK, vec![])
            ),
            (
                sc::SCCharacterLaborPowerChangedPacket,
                bytes(&SCCharacterLaborPowerChangedPacket {
                    amount: -60,
                    action: 31,
                    point: 60,
                    step: 0
                })
            ),
        ]
    );
    let sp = &w.server.systems.economy.specialty;
    assert_eq!(sp.sold_in_tick(PACK, 0), 1);
    sp.consume_ratio();
    assert_eq!(sp.ratio(PACK, 0), 129.0);
    assert_eq!(sp.sold_in_tick(PACK, 0), 0);
    sp.regen_ratio();
    assert_eq!(sp.ratio(PACK, 0), 130.0);

    // Route rates (packs made in zone group 5, traded in 0).
    let mut s = PacketStream::new();
    s.write_u16(5).write_u16(0);
    run::<CSRequestSpecialtyCurrentPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCSpecialtyCurrentPacket,
            bytes(&SCSpecialtyCurrentPacket {
                from_zone_group: 5,
                to_zone_group: 0,
                results: vec![(PACK, 130)]
            })
        )]
    );
    assert_eq!(
        specialty::get_ratios_for_target_route(&w.server, 6, 2),
        vec![(GLIDER, 130)]
    );
}

#[tokio::test]
async fn sell_trade_pack_shares_profit_with_crafter() {
    let mut w = world();
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer.clone());
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    Inv::of(&pa)
        .equipment()
        .lock()
        .unwrap()
        .get_item_by_slot_mut(equip_slot::BACKPACK)
        .unwrap()
        .made_unit_id = 2;
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    let mails = mailer.0.lock().unwrap().clone();
    assert_eq!(mails.len(), 2);
    // 9555 * 0.8 = 7644 for the seller, 1911 for the crafter
    assert_eq!(
        (
            mails[0].item_count_for_seller,
            mails[0].item_count_for_crafter
        ),
        (7644, 1911)
    );
    assert!(!mails[0].for_crafter && mails[1].for_crafter);
    assert_eq!(mails[1].crafter_id, 2);
}

#[tokio::test]
async fn sell_trade_pack_refused_without_mail() {
    let mut w = world();
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(169, 169, 0, true))]
    );
    assert!(Inv::of(&pa).equipped(equip_slot::BACKPACK).is_some());
}

#[tokio::test]
async fn list_specialty_goods_answers_empty_until_the_content_is_evidenced() {
    let mut w = world();
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    for obj_id in [TRADER_NPC, 0x777777] {
        let mut s = PacketStream::new();
        s.write_bc(obj_id);
        run::<CSListSpecialtyGoodsPacket>(&mut w, 0, s.as_bytes());
        let f = w.a.drain();
        assert_eq!(
            f,
            vec![(sc::SCSpecialtyGoodsPacket, 0i32.to_le_bytes().to_vec())]
        );
    }
}

#[tokio::test]
async fn specialty_ratio_reply_does_not_send_the_discarded_records_packet() {
    let mut w = world();
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer);
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 0,
        });
    give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(w.a.drain().len(), 3);

    // The trader-talk 0x43 answer is only SCSpecialtyRatio: the 1.2 client discards 0x9b.
    let mut s = PacketStream::new();
    s.write_u32(PACK);
    run::<CSSpecialtyRatioPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCSpecialtyRatioPacket, 130i32.to_le_bytes().to_vec())]
    );
}

#[tokio::test]
async fn sell_trade_pack_labor_uses_the_commerce_step_multiplier() {
    let mut w = world();
    w.server
        .config
        .update(|c| c.specialty.scale_sale_labor_by_commerce = true);
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer);
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 4, // Actability.GetLaborCostMultiplier(4) = 0.85 -> 60 * 0.85 = 51
        });
    give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(pa.data().labor_power, 49);
    let f = w.a.drain();
    let labor = f
        .iter()
        .find(|(op, _)| *op == sc::SCCharacterLaborPowerChangedPacket)
        .expect("labor packet");
    assert_eq!(
        labor.1,
        bytes(&SCCharacterLaborPowerChangedPacket {
            amount: -51,
            action: 31,
            point: 51,
            step: 4,
        })
    );
}

#[tokio::test]
async fn sell_trade_pack_labor_defaults_to_the_flat_60() {
    let mut w = world();
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer);
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 4,
        });
    give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(pa.data().labor_power, 40);
    let f = w.a.drain();
    let labor = f
        .iter()
        .find(|(op, _)| *op == sc::SCCharacterLaborPowerChangedPacket)
        .expect("labor packet");
    assert_eq!(
        labor.1,
        bytes(&SCCharacterLaborPowerChangedPacket {
            amount: -60,
            action: 31,
            point: 60,
            step: 4,
        })
    );
}

#[tokio::test]
async fn sell_trade_pack_profit_share_follows_the_global_feature_set() {
    let mut w = world();
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer.clone());
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 0,
        });
    // The runtime `FeaturesManager.Fsets` is the global set (a GM command can change it).
    assert!(w.server.systems.services.set_feature(
        &w.server,
        crate::data::Feature::BackpackProfitShare as u8,
        false
    ));

    let pack = give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    Inv::of(&pa)
        .equipment()
        .lock()
        .unwrap()
        .get_item_by_id_mut(pack)
        .unwrap()
        .made_unit_id = 2;
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(mailer.0.lock().unwrap().len(), 1);
    w.a.drain();

    assert!(w.server.systems.services.set_feature(
        &w.server,
        crate::data::Feature::BackpackProfitShare as u8,
        true
    ));
    pa.data().labor_power = 200;
    let pack = give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    Inv::of(&pa)
        .equipment()
        .lock()
        .unwrap()
        .get_item_by_id_mut(pack)
        .unwrap()
        .made_unit_id = 2;
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(mailer.0.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn specialty_sale_sends_the_configured_negotiation_bonus() {
    let mut w = world();
    w.server
        .config
        .update(|c| c.specialty.negotiation_bonus_percent = 10.0);
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer.clone());
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 0,
        });
    give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    // pre-interest 7000 * 130% = 9100; +10% bargaining = 910; +5% interest 455 -> 10465.
    let mails = mailer.0.lock().unwrap().clone();
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].item_count_bonus, 910);
    assert_eq!(mails[0].item_count_for_seller, 10465);
    // `SPECIALTY_MARGIN` (626, the notice for a money bonus).
    assert!(w
        .a
        .drain()
        .iter()
        .any(|(op, body)| *op == sc::SCErrorMsgPacket && *body == err_bytes(626, 626, 0, true)));

    // An out-of-range setting is clamped to 0..100: 100% -> 9100 bonus, total 18655.
    w.server
        .config
        .update(|c| c.specialty.negotiation_bonus_percent = 250.0);
    pa.data().labor_power = 200;
    give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    let mails = mailer.0.lock().unwrap().clone();
    assert_eq!(mails.len(), 2);
    assert_eq!(mails[1].item_count_bonus, 9100);
    assert_eq!(mails[1].item_count_for_seller, 18655);
}

#[tokio::test]
async fn sold_pack_with_a_crafter_delivers_the_crafter_mail() {
    let mut d = data();
    d.items.templates.insert(
        crate::data::items::ITEM_COINS,
        ItemTemplate::plain(crate::data::items::ITEM_COINS, 1_000_000),
    );
    let mut w = world_with_data(d);
    crate::systems::economy::wiring::install(&w.server);
    w.server.names.add_character(1, "Alice", 1001);
    w.server.names.add_character(2, "Bob", 1002);
    spawn_npc(&mut w, TRADER_NPC, 4321, 102.0);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 0,
        });
    let pack = give(&w, &pa, slot_type::EQUIPMENT, PACK, 1, equip_slot::BACKPACK);
    Inv::of(&pa)
        .equipment()
        .lock()
        .unwrap()
        .get_item_by_id_mut(pack)
        .unwrap()
        .made_unit_id = 2;
    let mut s = PacketStream::new();
    s.write_bc(TRADER_NPC);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    let mails: Vec<(u32, String, i32)> = w
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .map(|(_, m)| (m.header.receiver_id, m.title.clone(), m.body.copper_coins))
        .collect();
    assert_eq!(mails.len(), 2, "seller and crafter mails: {mails:?}");
    assert!(mails
        .iter()
        .any(|m| m.0 == 1 && m.1 == "Specialty Payment [Delivery]" && m.2 == 7644));
    assert!(mails
        .iter()
        .any(|m| m.0 == 2 && m.1 == "Specialty Payment [Crafter]" && m.2 == 1911));
}

// ---------------------------------------------------------------------------------------------
// Real compact.sqlite3
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore]
fn real_compact_db_craft_and_store_tables() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let files =
        crate::data::client_files::ClientFileManager::from_sources(&[], std::path::Path::new("."));
    let ctx = crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: std::path::Path::new("."),
        files: &files,
        language: "en_us",
    };
    let craft = crate::systems::economy::craft_data::CraftData::load(&ctx).unwrap();
    assert_eq!(craft.crafts.len(), 7010);
    let products: usize = craft.crafts.values().map(|c| c.craft_products.len()).sum();
    let materials: usize = craft.crafts.values().map(|c| c.craft_materials.len()).sum();
    assert_eq!(products, 6938 - 10);
    assert_eq!(materials, 23475 - 54);
    assert_eq!(craft.crafts.values().filter(|c| c.is_pack).count(), 2153);
    assert_eq!(
        craft
            .crafts
            .values()
            .flat_map(|c| &c.craft_products)
            .filter(|p| p.use_grade)
            .count(),
        356
    );
    assert_eq!(
        craft
            .crafts
            .values()
            .flat_map(|c| &c.craft_materials)
            .filter(|m| m.main_grade)
            .count(),
        101
    );
    // `require_grade` is set on the 14 grade-restricted rows (crafts 6184-6191 and 6889-6894).
    let restricted: Vec<(u32, u32, i32)> = craft
        .crafts
        .values()
        .flat_map(|c| &c.craft_materials)
        .filter(|m| m.require_grade > 0)
        .map(|m| (m.craft_id, m.item_id, m.require_grade))
        .collect();
    assert_eq!(restricted.len(), 14);
    assert_eq!(
        restricted
            .iter()
            .filter(|(craft_id, item_id, _)| *craft_id == 6184 && *item_id == 30803)
            .count(),
        1
    );
    assert!(restricted
        .iter()
        .all(|(_, _, grade)| (4..=11).contains(grade)));
    // The 't'/'f' columns read through GetBoolean(column) are all false.
    assert!(craft
        .crafts
        .values()
        .all(|c| !c.need_bind && !c.show_upper_craft));
    let with_skill = craft.crafts.values().filter(|c| c.skill_id != 0).count();
    assert_eq!(with_skill, 7010 - 5);
    assert!(craft
        .crafts
        .values()
        .filter(|c| c.skill_id != 0)
        .all(|c| craft.skills.contains_key(&c.skill_id)));

    let store = crate::systems::economy::store_data::StoreData::load(&ctx).unwrap();
    assert_eq!(store.specialties.len(), 2162);
    assert_eq!(
        store
            .specialties
            .values()
            .filter(|s| s.vendor_exist)
            .count(),
        1
    );
    assert_eq!(store.specialty_bundle_items.len(), 3311);
    assert_eq!(store.specialty_bundle_items_mapped.len(), 231);
    assert_eq!(store.specialty_npcs.len(), 37);
    assert_eq!(store.taxations.len(), 11);
    assert_eq!(store.taxations.values().filter(|t| t.show).count(), 10);
    assert_eq!(store.expand_expert_limits.len(), 14);
    assert_eq!(
        store
            .expand_expert_limits
            .iter()
            .map(|e| e.expand_count)
            .collect::<Vec<_>>(),
        (1..=14).collect::<Vec<u8>>()
    );
    assert_eq!(store.item_look_converts.len(), 27);
    assert_eq!(store.holdable_item_look_converts.len(), 22);
    assert_eq!(store.wearable_item_look_converts.len(), 7); // 8 rows, one duplicate slot (TryAdd)
    assert_eq!(store.refund_multipliers.len(), 12);
    assert_eq!(store.refund_multiplier(0), Some(100));
    assert_eq!(store.specialty_packs.len(), 255);
    println!(
        "crafts {} products {products} materials {materials} skills {} packs {}",
        craft.crafts.len(),
        craft.skills.len(),
        store.specialty_packs.len()
    );
}

/// The four AyinMaiden 2014-10 trade-pack runs (NA, grade C; see
/// `D:/aaemu-audits/video-evidence/trade-runs-2014-10-ayin.md`): the pack, its destination
/// trader, the destination town's zone key, the ratio the video shows, the packs sold in the
/// tick that bring the ratio there, and the amounts in the DB's copper-equivalent units
/// (before the coin-item `/10000` division): base price, the window's pre-interest payment and
/// the mailed payment with the 5 % interest.
struct VideoPack {
    name: &'static str,
    pack: u32,
    trader: u32,
    zone_key: u32,
    ratio: i32,
    packs_sold: i32,
    base: i32,
    shown: i32,
    mailed: i32,
    coin: u32,
}

const VIDEO_PACKS: [VideoPack; 4] = [
    VideoPack {
        name: "Arcum Iris Lavaspice -> Kalina",
        pack: 31866,
        trader: 10691,
        zone_key: 156, // Falcorth Plains
        ratio: 124,
        packs_sold: 11, // ceil(11 * 0.5) = 6 -> 130 - 6
        base: 34310,
        shown: 42544,
        mailed: 44672,
        coin: crate::data::items::ITEM_COINS,
    },
    VideoPack {
        name: "Falcorth Snowlion Yarn -> Robert",
        pack: 31845,
        trader: 10643,
        zone_key: 140, // Arcum Iris
        ratio: 111,
        packs_sold: 37, // ceil(37 * 0.5) = 19 -> 130 - 19
        base: 34310,
        shown: 38084,
        mailed: 39988,
        coin: crate::data::items::ITEM_COINS,
    },
    VideoPack {
        name: "Tigerspine Grape Jam -> Jasmine",
        pack: 31869,
        trader: 10659,
        zone_key: 141, // Sunrise Peninsula (Austera)
        ratio: 130,
        packs_sold: 0,
        base: 39910,
        shown: 51883,
        mailed: 54477,
        coin: crate::data::items::ITEM_COINS,
    },
    VideoPack {
        name: "Mahadevi Elephant Cookies -> Delara",
        pack: 31844,
        trader: 8000003,
        zone_key: 142, // Solzreed Peninsula
        ratio: 130,
        packs_sold: 0,
        base: 10750,
        shown: 13975,
        mailed: 14674,
        coin: 23633, // Gilda Star
    },
];

/// The game data the specialty sale reads, loaded from the real 1.2 `compact.sqlite3`.
fn real_specialty_data(path: &str) -> GameData {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = crate::data::client_files::ClientFileManager::from_sources(&[], &app);
    let ctx = crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.items = crate::data::ItemData::load(&conn).unwrap();
    data.zones = crate::data::ZoneData::load(&conn).unwrap();
    data.systems.economy.store =
        crate::systems::economy::store_data::StoreData::load(&ctx).unwrap();
    data.systems.npcs.npc = crate::systems::npcs::npc::data::NpcData::load(&ctx).unwrap();
    data
}

/// The 2014-10 video values through the live sale (`CSSellBackpackGoodsPacket` ->
/// `economy::specialty::sell_specialty` -> the store hook), on the real compact data. The
/// ratios are the video's town ratios, reached through the real sold-pack consume tick.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_specialty_payouts_match_the_2014_video() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let mut w = world_with_data(real_specialty_data(&path));
    let mailer = Arc::new(Mailer::default());
    w.server.systems.economy.store.hooks.install(mailer.clone());
    assert_eq!(
        w.server
            .systems
            .economy
            .mail
            .trade_pack_mail_delay_minutes(),
        1320.0
    );
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 0,
        });
    let npcs: Vec<NpcRef> = VIDEO_PACKS
        .iter()
        .enumerate()
        .map(|(i, case)| {
            spawn_npc(
                &mut w,
                0x5000 + i as u32,
                case.trader,
                100.5 + i as f32 * 0.5,
            )
        })
        .collect();

    for (i, case) in VIDEO_PACKS.iter().enumerate() {
        let group = w
            .server
            .game_data
            .zones
            .zone_by_key(case.zone_key)
            .unwrap()
            .group_id;
        pa.data().unit.transform.zone_id = case.zone_key;
        {
            let sp = &w.server.systems.economy.specialty;
            for _ in 0..case.packs_sold {
                sp.add_sold_pack(case.pack, group);
            }
            if case.packs_sold > 0 {
                sp.consume_ratio();
            }
            assert_eq!(
                sp.ratio(case.pack, group) as i32,
                case.ratio,
                "{}",
                case.name
            );
        }
        // The client asks about the pack and is answered the video's ratio.
        let mut s = PacketStream::new();
        s.write_u32(case.pack);
        run::<CSSpecialtyRatioPacket>(&mut w, 0, s.as_bytes());
        assert_eq!(
            w.a.drain(),
            vec![(
                sc::SCSpecialtyRatioPacket,
                case.ratio.to_le_bytes().to_vec()
            )],
            "{}",
            case.name
        );

        pa.data().labor_power = 200;
        give(
            &w,
            &pa,
            slot_type::EQUIPMENT,
            case.pack,
            1,
            equip_slot::BACKPACK,
        );
        let before = mailer.0.lock().unwrap().len();
        let mut s = PacketStream::new();
        s.write_bc(npcs[i].obj_id);
        run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
        let mails = mailer.0.lock().unwrap();
        assert_eq!(mails.len(), before + 1, "{}", case.name);
        let mail = &mails[before];
        let (expected_base, expected_mailed) = if case.coin == crate::data::items::ITEM_COINS {
            (case.base, case.mailed)
        } else {
            (
                (case.base as f32 / 10000f32).round_ties_even() as i32,
                (case.mailed as f32 / 10000f32).round_ties_even() as i32,
            )
        };
        assert_eq!(mail.trade_pack_template_id, case.pack, "{}", case.name);
        assert_eq!(mail.trade_rate, case.ratio, "{}", case.name);
        assert_eq!(mail.interest_rate, 5, "{}", case.name);
        assert_eq!(mail.item_count_bonus, 0, "{}", case.name);
        assert_eq!(mail.item_reward_template_id, case.coin, "{}", case.name);
        assert_eq!(mail.item_count_base, expected_base, "{}", case.name);
        assert_eq!(mail.item_count_for_seller, expected_mailed, "{}", case.name);
        assert_eq!(mail.item_count_for_crafter, 0, "{}", case.name);
        assert_eq!(mail.crafter_id, 0, "{}", case.name);
        // The formula pins the window payment (no interest) and the mail (+5 %).
        assert_eq!(
            specialty::sale_amounts(case.base, case.ratio, 0),
            (case.shown, case.mailed),
            "{}",
            case.name
        );
        assert!(Inv::of(&pa).equipped(equip_slot::BACKPACK).is_none());
        assert_eq!(pa.data().labor_power, 140, "{}", case.name);
        assert_eq!(
            w.server
                .systems
                .economy
                .specialty
                .sold_in_tick(case.pack, group),
            1,
            "{}",
            case.name
        );
        // The item-task / labor / sale frames of this case, before the next ratio request.
        w.a.drain();
    }
}

/// The real mail path of the 2014-10 sale: 20 % crafter / 80 % seller, the 22 h delay and the
/// 5 % interest on the mailed amount, and the Gilda Star trader's one-star payment.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_specialty_mail_split_and_delay_match_the_2014_video() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let mut w = world_with_data(real_specialty_data(&path));
    crate::systems::economy::wiring::install(&w.server);
    crate::systems::npcs::bridges::install_economy(&w.server);
    w.server.names.add_character(1, "Alice", 1001);
    w.server.names.add_character(2, "Bob", 1002);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 31,
            point: 0,
            step: 0,
        });

    // Tigerspine Grape Jam at Jasmine (Austera, 130 %): base 39910 -> 51883 shown -> 54477
    // mailed; the crafter gets 20 % = 10895, the seller 80 % = 43582.
    pa.data().unit.transform.zone_id = 141;
    let jasmine = spawn_npc(&mut w, 0x5100, 10659, 102.0);
    let pack = give(
        &w,
        &pa,
        slot_type::EQUIPMENT,
        31869,
        1,
        equip_slot::BACKPACK,
    );
    Inv::of(&pa)
        .equipment()
        .lock()
        .unwrap()
        .get_item_by_id_mut(pack)
        .unwrap()
        .made_unit_id = 2;
    let mut s = PacketStream::new();
    s.write_bc(jasmine.obj_id);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    let now = crate::systems::economy::mail::now();
    let mails: Vec<(u32, String, i32, i64)> = w
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .map(|(_, m)| {
            (
                m.header.receiver_id,
                m.title.clone(),
                m.body.copper_coins,
                m.body.recv_date,
            )
        })
        .collect();
    assert!(
        mails.iter().any(|m| m.0 == 1
            && m.1 == "Specialty Payment [Delivery]"
            && m.2 == 43582
            && (m.3 - now - 1320 * 60).abs() <= 5),
        "{mails:?}"
    );
    assert!(
        mails
            .iter()
            .any(|m| m.0 == 2 && m.1 == "Specialty Payment [Crafter]" && m.2 == 10895),
        "{mails:?}"
    );

    // Mahadevi Elephant Cookies at Delara (Solzreed, 130 %): 1 Gilda Star, not copper.
    pa.data().labor_power = 200;
    pa.data().unit.transform.zone_id = 142;
    let delara = spawn_npc(&mut w, 0x5101, 8000003, 101.0);
    give(
        &w,
        &pa,
        slot_type::EQUIPMENT,
        31844,
        1,
        equip_slot::BACKPACK,
    );
    let mut s = PacketStream::new();
    s.write_bc(delara.obj_id);
    run::<CSSellBackpackGoodsPacket>(&mut w, 0, s.as_bytes());
    let (attachment, recv_date) = {
        let state = w.server.systems.economy.mail.state();
        let mail = state
            .all
            .iter()
            .map(|(_, m)| m)
            .find(|m| m.header.receiver_id == 1 && m.title == "Specialty Payment")
            .expect("the Gilda Star seller mail");
        assert_eq!(mail.body.copper_coins, 0);
        assert_eq!(mail.body.attachments.len(), 1);
        (mail.body.attachments[0], mail.body.recv_date)
    };
    assert!((recv_date - crate::systems::economy::mail::now() - 1320 * 60).abs() <= 5);
    let mail_container = Inv::of(&pa).get(slot_type::MAIL).expect("mail container");
    let container = mail_container.lock().unwrap();
    let att = container
        .get_item_by_id(attachment)
        .expect("Gilda Star attachment");
    assert_eq!((att.template_id, att.count), (23633, 1));
}

fn sell_body(npc: u32, entries: &[(u8, u8, u64)]) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(npc).write_bc(0).write_u8(entries.len() as u8);
    for (st, slot, id) in entries {
        s.write_u8(*st).write_u8(*slot).write_u64(*id).write_u32(0);
    }
    s.into_bytes()
}

#[tokio::test]
async fn sell_pays_a_repeated_entry_only_once() {
    let mut w = world();
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    let pa = w.pa.clone();
    pa.data().money = 0;
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 5, 0);
    // The same slot listed 20 times: 5 units x refund 5 are paid once (25).
    let entries = vec![(slot_type::INVENTORY, 0u8, id); 20];
    run::<CSSellItemsPacket>(&mut w, 0, &sell_body(MERCHANT_NPC, &entries));
    assert!(item_at(&pa, 2, 0).is_none());
    assert_eq!(pa.data().money, 25);
}

#[tokio::test]
async fn sell_is_refused_far_from_the_merchant() {
    let mut w = world();
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 120.0);
    let pa = w.pa.clone();
    pa.data().money = 0;
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 5, 0);
    w.a.drain();
    run::<CSSellItemsPacket>(
        &mut w,
        0,
        &sell_body(MERCHANT_NPC, &[(slot_type::INVENTORY, 0, id)]),
    );
    assert!(item_at(&pa, 2, 0).is_some());
    assert_eq!(pa.data().money, 0);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(154, 154, 0, true))]
    );
}

#[tokio::test]
async fn buy_without_a_merchant_npc_is_refused() {
    let mut w = world();
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    let pa = w.pa.clone();
    pa.data().money = 1000;
    // npc 0 + doodad 0 used to skip the merchant filter: any template, here a free-for-all.
    run::<CSBuyItemsPacket>(&mut w, 0, &buy_body(0, &[(STACK, 3, 0)], &[]));
    assert!(item_at(&pa, 2, 0).is_none());
    assert_eq!(pa.data().money, 1000);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn sell_pays_nothing_when_the_item_cannot_move_to_buyback() {
    let mut w = world();
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_npc(&mut w, MERCHANT_NPC, 1234, 101.0);
    let pa = w.pa.clone();
    pa.data().money = 0;
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 5, 0);
    let buyback = w.server.systems.economy.store.buyback(pa.character_id);
    buyback.lock().unwrap().set_container_size(0);
    run::<CSSellItemsPacket>(
        &mut w,
        0,
        &sell_body(MERCHANT_NPC, &[(slot_type::INVENTORY, 0, id)]),
    );
    assert!(item_at(&pa, 2, 0).is_some());
    assert_eq!(pa.data().money, 0);
}

#[tokio::test]
async fn buy_at_a_doodad_shop_uses_its_catalog() {
    let mut w = world_with_data(with_doodad_shop(7));
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_doodad(&mut w, 0x600, 1234, 42, 101.0);
    let pa = w.pa.clone();
    pa.data().money = 1000;
    // Pack 7 sells STACK; PRODUCT is not in it and is skipped even though it used to be
    // purchasable through the unvalidated doodad path.
    run::<CSBuyItemsPacket>(
        &mut w,
        0,
        &doodad_buy_body(0x600, &[(STACK, 2, 0), (PRODUCT, 1, 0)], &[]),
    );
    assert_eq!(item_at(&pa, 2, 0).unwrap().template_id, STACK);
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 2);
    assert!(item_at(&pa, 2, 1).is_none());
    assert_eq!(pa.data().money, 980);
}

#[tokio::test]
async fn buy_at_a_doodad_without_a_store_func_is_refused() {
    let mut w = world();
    w.server.systems.economy.hooks.npcs.install(Arc::new(Shops));
    spawn_doodad(&mut w, 0x601, 1234, 0, 101.0);
    let pa = w.pa.clone();
    pa.data().money = 1000;
    run::<CSBuyItemsPacket>(&mut w, 0, &doodad_buy_body(0x601, &[(STACK, 2, 0)], &[]));
    assert!(item_at(&pa, 2, 0).is_none());
    assert_eq!(pa.data().money, 1000);
}

#[tokio::test]
async fn craft_counts_and_consumes_bag_materials_only() {
    let mut w = world();
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;
    // Materials only in the bank do not count: refused at the start.
    give(&w, &pa, slot_type::BANK, STACK, 6, 0);
    execute_craft(&mut w, CRAFT, 1);
    assert!(rec.casts.lock().unwrap().is_empty());
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 722, 0, false))]
    );

    // Materials taken out of the bag during the cast: no product, bank untouched.
    let bag_id = give(&w, &pa, slot_type::INVENTORY, STACK, 6, 0);
    execute_craft(&mut w, CRAFT, 1);
    assert_eq!(rec.casts.lock().unwrap().len(), 1);
    {
        let inv = Inv::of(&pa);
        let bag = inv.bag();
        let mut bag = bag.lock().unwrap();
        bag.get_item_by_id_mut(bag_id).unwrap().count = 0;
    }
    w.a.drain();
    craft::end_craft(&w.server, &pa);
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, err_bytes(148, 722, 0, false))]
    );
    assert_eq!(
        Inv::of(&pa).items_count_in(slot_type::INVENTORY, PRODUCT, -1),
        0
    );
    assert_eq!(Inv::of(&pa).items_count_in(slot_type::BANK, STACK, -1), 6);
}

/// Every `SCErrorMsg` among `frames`: `(error1, error2, notify)`.
fn error_msgs(frames: Vec<(u16, Vec<u8>)>) -> Vec<(i16, i16, bool)> {
    frames
        .into_iter()
        .filter(|(op, _)| *op == sc::SCErrorMsgPacket)
        .map(|(_, b)| {
            (
                i16::from_le_bytes([b[0], b[1]]),
                i16::from_le_bytes([b[2], b[3]]),
                *b.last().unwrap() != 0,
            )
        })
        .collect()
}

#[tokio::test]
async fn every_craft_abort_path_sends_nonzero_non_notify_craft_cant_act_any_more() {
    use crate::systems::skills::types::skill_result as r;
    let mut w = world();
    let rec = install_craft_hooks(&w);
    let pa = w.pa.clone();
    pa.data().labor_power = 100;

    // The 1.2 client resets its craft batch (greyed-out button) only on error1 148 with a
    // nonzero error2 and isNotify false: assert that for every way a started batch is stopped.
    let check = |w: &mut World, what: &str| {
        let msgs = error_msgs(w.a.drain());
        let (e1, e2, notify) = *msgs
            .last()
            .unwrap_or_else(|| panic!("{what}: no SCErrorMsg"));
        assert_eq!(e1, 148, "{what}");
        assert_ne!(e2, 0, "{what}");
        assert!(!notify, "{what}");
        let st = w.server.systems.economy.craft.state(1);
        assert!(
            !st.is_crafting && st.count == 0 && st.current_craft.is_none(),
            "{what}: craft state not cleared: {st:?}"
        );
    };

    execute_craft(&mut w, 999, 3);
    check(&mut w, "unknown craft");

    execute_craft(&mut w, CRAFT, 3); // no materials
    check(&mut w, "missing materials");

    execute_craft(&mut w, ACT_CRAFT, 3);
    let msgs = error_msgs(w.a.drain());
    // The message the C# showed (notify) stays, followed by the batch abort.
    assert_eq!(msgs.first().map(|m| m.2), Some(true));
    assert_eq!(msgs.last(), Some(&(148, error_message_actability(), false)));
    assert_eq!(w.server.systems.economy.craft.state(1).current_craft, None);

    execute_craft(&mut w, NO_SKILL_CRAFT, 3);
    check(&mut w, "unknown craft skill");

    give(&w, &pa, slot_type::INVENTORY, STACK, 20, 0);

    // Cast refused by the skill.
    *rec.cast_result.lock().unwrap() = r::NEED_LABOR_POWER;
    execute_craft(&mut w, CRAFT, 3);
    check(&mut w, "cast refused");
    *rec.cast_result.lock().unwrap() = r::SUCCESS;

    // Cast re-validation failure.
    execute_craft(&mut w, CRAFT, 3);
    w.a.drain();
    craft::cast_failed(&w.server, &pa, SKILL, r::NEED_LABOR_POWER);
    check(&mut w, "cast failed");

    // Labor ran out before the product is handed out: the batch stops, no further casts.
    execute_craft(&mut w, CRAFT, 3);
    w.a.drain();
    pa.data().labor_power = 4; // the craft skill costs 5
    let casts = rec.casts.lock().unwrap().len();
    craft::end_craft(&w.server, &pa);
    check(&mut w, "labor ran out");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(rec.casts.lock().unwrap().len(), casts, "no cast follows");
    assert_eq!(Inv::of(&pa).items_count(PRODUCT, -1), 0);

    // A trade pack is worn: crafting another one is refused.
    pa.data().labor_power = 100;
    execute_craft(&mut w, PACK_CRAFT, 1);
    craft::end_craft(&w.server, &pa);
    w.a.drain();
    execute_craft(&mut w, PACK_CRAFT, 1);
    check(&mut w, "backpack occupied");
}

fn error_message_actability() -> i16 {
    crate::systems::economy::error_message::ACTABILITY_NOT_ENOUGH_POINT
}
