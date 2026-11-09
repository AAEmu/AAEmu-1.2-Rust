//! Flow tests of the item skill effects and the mate gear packets (no MySQL).

use std::sync::{Arc, Mutex};

use aaemu_commons::{Marshal, PacketStream};

use crate::data::formulas::FormulaManager;
use crate::data::items::{
    EquipSlotGroup, EquipSlotGroupMap, EquipTemplate, FullGradeTemplate, ItemCapScale,
    ItemGradeEnchantingSupport, ItemSocket, ItemSocketLevelLimit, ItemSocketNumLimit, ItemTemplate,
    TemplateKind, ITEM_CREST_STAMP, ITEM_DAWN_STONE,
};
use crate::data::GameData;
use crate::managers::chat::chat_type;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::{Item, ITEM_FLAG_SECURE};
use crate::models::item_task::{task_type, ItemTask};
use crate::models::mate::Mate;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::world::{SCChatMessagePacket, SCErrorMsgPacket};
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::{Inv, ItemTxn};
use crate::systems::economy::item_effect_packets::*;
use crate::systems::economy::item_effects::*;
use crate::systems::economy::item_packets::{
    SCItemTaskSuccessPacket, SCUnitEquipmentsChangedPacket,
};
use crate::systems::economy::item_timers;
use crate::systems::economy::items_data::{ItemConversionProduct, ItemConversionReagent};
use crate::systems::economy::loot::ScriptedRng;
use crate::systems::economy::mate_equipment::*;
use crate::systems::economy::mate_equipment_data::MateSlotPack;
use crate::systems::economy::tests::{give, item_at, test_data, World, STACK};
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::skills::data::effects::SpecialEffect;
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillEffect, SkillTemplate};
use crate::systems::skills::handlers::CSStartSkillPacket;
use crate::systems::skills::packets::SCExpChangedPacket;
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, PlayerRef};

const ARMOR: u32 = 2100; // chest, no binding, level 10
const NON_ENCHANTABLE_ARMOR: u32 = 2101; // chest, `grade_enchantable` false
const WEAPON: u32 = 2102; // one-hand weapon, `grade_enchantable` true
const SOCKET_ARMOR: u32 = 2110; // chest, level 20, level requirement 50
const LOW_ARMOR: u32 = 2111; // chest, level 20, level requirement 40
const FORBIDDEN_SCALE_ARMOR: u32 = 2120; // chest listed in `item_cap_scale_forbids`
const SCROLL: u32 = 5000;
const CHARM: u32 = 5100;
const GEM: u32 = 5200; // chest lunagem, requires a level-50 item
const HANDS_GEM: u32 = 5201; // hands lunagem, no level limit
const LOW_GEM: u32 = 5202; // chest lunagem, no level limit (only the item-level gate applies)
const FAIL_GEM: u32 = 5203; // hands lunagem requiring a level-50 item (fails level and group)
const POWDER: u32 = 5300;
const SLAVE_ITEM: u32 = 5400;
const BACKPACK: u32 = 5500;
const TIMEGLASS: u32 = 5600;
const PACK: u32 = 5510; // a trade pack (back slot)
const GLIDER_HI: u32 = 5520; // another glider template
const RECHARGEABLE: u32 = 5700;
const TL: u16 = 0x77;
// Mate gear slot pack fixtures: the same four armour slot types the 1.2 packs use (head 1,
// chest 3, waist 4, feet 7; `item_armors.slot_type_id = equip slot + 1`).
const HEAD_ARMOR: u32 = 2130;
const WAIST_ARMOR: u32 = 2131;
const FEET_ARMOR: u32 = 2132;
// NPC templates with the 1.2 `mate_equip_slot_packs` ids (1 riding, 2 combat, 3 pet; 0 = a
// template whose pack is missing, which the client refuses every slot for).
const MATE_RIDING: u32 = 501;
const MATE_COMBAT: u32 = 502;
const MATE_PET: u32 = 503;
const MATE_NO_PACK: u32 = 504;

/// Sets `ItemFlag.Secure` on a bag item without going through the packet handler.
fn set_locked(w: &World, id: u64) {
    let c = Inv::of(&w.pa).bag();
    c.lock().unwrap().get_item_by_id_mut(id).unwrap().flags |= ITEM_FLAG_SECURE;
}

/// Clears the new-item dirty flag so an effect's own marking is what the save selection sees.
fn mark_clean(w: &World, id: u64) {
    let c = Inv::of(&w.pa).bag();
    c.lock().unwrap().get_item_by_id_mut(id).unwrap().dirty = false;
}

/// The item is selected by the next `ItemManager.Save` snapshot.
fn selected_for_save(w: &World, id: u64) -> bool {
    w.server.items.prepare_save().item_ids().contains(&id)
}

fn formulas() -> FormulaManager {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE unit_formulas (id INTEGER, formula TEXT, kind_id INTEGER, owner_type_id INTEGER);
         CREATE TABLE unit_formula_variables (id INTEGER, unit_formula_id INTEGER, variable_kind_id INTEGER, key INTEGER, value REAL);
         CREATE TABLE wearable_formulas (id INTEGER, kind_id INTEGER, formula TEXT);
         CREATE TABLE formulas (id INTEGER, formula TEXT);
         INSERT INTO formulas VALUES (22, 'item_grade * 100 + item_level + equip_slot_enchant_cost');",
    )
    .unwrap();
    FormulaManager::load(&conn).unwrap()
}

fn data() -> GameData {
    let mut d = test_data();
    d.formulas = formulas();
    for g in 0..=12 {
        d.items.insert_grade(FullGradeTemplate {
            grade: g,
            grade_order: g,
            durability: 1.0,
            enchant_success_ratio: 5000,
            enchant_great_success_ratio: 3000,
            enchant_break_ratio: 1000,
            enchant_downgrade_ratio: 2000,
            enchant_downgrade_min: 1,
            enchant_downgrade_max: 3,
            enchant_cost: g + 1,
            ..FullGradeTemplate::default()
        });
    }
    let mut armor = ItemTemplate::new(
        ARMOR,
        TemplateKind::Armor(EquipTemplate {
            slot_type_id: 3,
            coverage: 10,
            durability_ratio: 1.0,
            durability_multiplier: 100,
            ..EquipTemplate::default()
        }),
    );
    armor.max_count = 1;
    armor.level = 10;
    armor.disenchantable = true;
    armor.grade_enchantable = true;
    armor.impl_id = 3;
    d.items.templates.insert(ARMOR, armor);
    // The real data has 16 918 `grade_enchantable = 'f'` items, most of them NPC gear.
    let mut non_enchantable = ItemTemplate::new(
        NON_ENCHANTABLE_ARMOR,
        TemplateKind::Armor(EquipTemplate {
            slot_type_id: 3,
            coverage: 10,
            durability_ratio: 1.0,
            durability_multiplier: 100,
            ..EquipTemplate::default()
        }),
    );
    non_enchantable.max_count = 1;
    non_enchantable.level = 10;
    d.items
        .templates
        .insert(NON_ENCHANTABLE_ARMOR, non_enchantable);
    let mut weapon = ItemTemplate::new(
        WEAPON,
        TemplateKind::Weapon(EquipTemplate {
            slot_type_id: 2,
            durability_ratio: 1.0,
            durability_multiplier: 100,
            ..EquipTemplate::default()
        }),
    );
    weapon.max_count = 1;
    weapon.level = 10;
    weapon.grade_enchantable = true;
    d.items.templates.insert(WEAPON, weapon);
    for (template, level_requirement) in [(SOCKET_ARMOR, 50), (LOW_ARMOR, 40)] {
        let mut socket_armor = ItemTemplate::new(
            template,
            TemplateKind::Armor(EquipTemplate {
                slot_type_id: 3,
                coverage: 10,
                durability_ratio: 1.0,
                durability_multiplier: 100,
                ..EquipTemplate::default()
            }),
        );
        socket_armor.max_count = 1;
        socket_armor.level = 20;
        socket_armor.level_requirement = level_requirement;
        d.items.templates.insert(template, socket_armor);
    }
    for (id, max) in [
        (SCROLL, 10),
        (CHARM, 10),
        (GEM, 10),
        (HANDS_GEM, 10),
        (LOW_GEM, 10),
        (FAIL_GEM, 10),
        (POWDER, 10),
        (ITEM_DAWN_STONE, 10),
        (TIMEGLASS, 10),
    ] {
        d.items.templates.insert(id, ItemTemplate::plain(id, max));
    }
    // Socket rules used by the socketing tests: chest slot type 3 fits group 23, hands fits 18.
    d.items.item_socket_num_limits.insert(
        (3, 0),
        ItemSocketNumLimit {
            id: 1,
            slot_id: 3,
            grade_id: 0,
            num_socket: 2,
        },
    );
    d.items.item_socket_level_limits.insert(
        GEM,
        ItemSocketLevelLimit {
            id: 1,
            item_id: GEM,
            level: 50,
        },
    );
    d.items.item_socket_level_limits.insert(
        HANDS_GEM,
        ItemSocketLevelLimit {
            id: 2,
            item_id: HANDS_GEM,
            level: 0,
        },
    );
    d.items.item_socket_level_limits.insert(
        FAIL_GEM,
        ItemSocketLevelLimit {
            id: 3,
            item_id: FAIL_GEM,
            level: 50,
        },
    );
    d.items.item_sockets.insert(
        GEM,
        ItemSocket {
            id: 1,
            item_id: GEM,
            equip_slot_group_id: 23,
        },
    );
    d.items.item_sockets.insert(
        HANDS_GEM,
        ItemSocket {
            id: 2,
            item_id: HANDS_GEM,
            equip_slot_group_id: 18,
        },
    );
    d.items.item_sockets.insert(
        LOW_GEM,
        ItemSocket {
            id: 3,
            item_id: LOW_GEM,
            equip_slot_group_id: 23,
        },
    );
    d.items.item_sockets.insert(
        FAIL_GEM,
        ItemSocket {
            id: 4,
            item_id: FAIL_GEM,
            equip_slot_group_id: 18,
        },
    );
    d.items.equip_slot_group_maps.insert(
        23,
        vec![EquipSlotGroupMap {
            id: 1,
            equip_slot_group_id: 23,
            equip_slot_type_id: 3,
        }],
    );
    d.items.equip_slot_group_maps.insert(
        18,
        vec![EquipSlotGroupMap {
            id: 2,
            equip_slot_group_id: 18,
            equip_slot_type_id: 6,
        }],
    );
    for (id, name) in [(23, "socket_chest"), (18, "socket_hands")] {
        d.items.equip_slot_groups.insert(
            id,
            EquipSlotGroup {
                id,
                name: name.to_string(),
            },
        );
    }
    // A rechargeable equip (the 1.2 "costume wash" mechanic: recharge_buff_id + charge window).
    let mut rechargeable = ItemTemplate::new(
        RECHARGEABLE,
        TemplateKind::Armor(EquipTemplate {
            slot_type_id: 3,
            coverage: 10,
            durability_ratio: 1.0,
            durability_multiplier: 100,
            charge_lifetime: 60,
            charge_count: 3,
            recharge_buff_id: 7777,
            ..EquipTemplate::default()
        }),
    );
    rechargeable.max_count = 1;
    rechargeable.level = 10;
    d.items.templates.insert(RECHARGEABLE, rechargeable);
    d.items.templates.insert(
        ITEM_CREST_STAMP,
        ItemTemplate {
            max_count: 1,
            ..ItemTemplate::new(ITEM_CREST_STAMP, TemplateKind::Ucc)
        },
    );
    d.items.templates.insert(
        SLAVE_ITEM,
        ItemTemplate {
            max_count: 1,
            ..ItemTemplate::new(SLAVE_ITEM, TemplateKind::SummonSlave { slave_id: 3 })
        },
    );
    d.items.templates.insert(
        BACKPACK,
        ItemTemplate {
            max_count: 1,
            ..ItemTemplate::new(
                BACKPACK,
                TemplateKind::Backpack {
                    backpack_type: crate::data::items::backpack_type::GLIDER,
                },
            )
        },
    );
    for (id, kind, level) in [
        (PACK, crate::data::items::backpack_kind::TRADE_PACK, 1),
        (GLIDER_HI, crate::data::items::backpack_type::GLIDER, 1),
    ] {
        d.items.templates.insert(
            id,
            ItemTemplate {
                max_count: 1,
                level,
                ..ItemTemplate::new(
                    id,
                    TemplateKind::Backpack {
                        backpack_type: kind,
                    },
                )
            },
        );
    }
    d.items.enchanting_costs.insert(
        3,
        crate::data::items::EquipSlotEnchantingCost {
            id: 1,
            slot_type_id: 3,
            cost: 5,
        },
    );
    d.items.enchanting_supports.insert(
        CHARM,
        ItemGradeEnchantingSupport {
            id: 1,
            item_id: CHARM,
            require_grade_min: -1,
            require_grade_max: 6,
            add_great_success_grade: 6,
            ..ItemGradeEnchantingSupport::default()
        },
    );
    d.items.socket_chance.insert(1, 10000);
    d.items.socket_chance.insert(2, 0);
    d.items.item_cap_scales.insert(
        900,
        ItemCapScale {
            id: 1,
            skill_id: 900,
            scale_min: 0,
            scale_max: 100,
        },
    );
    let mut forbidden = ItemTemplate::new(
        FORBIDDEN_SCALE_ARMOR,
        TemplateKind::Armor(EquipTemplate {
            slot_type_id: 3,
            coverage: 10,
            durability_ratio: 1.0,
            durability_multiplier: 100,
            ..EquipTemplate::default()
        }),
    );
    forbidden.max_count = 1;
    forbidden.level = 10;
    d.items.templates.insert(FORBIDDEN_SCALE_ARMOR, forbidden);
    d.items.item_cap_scale_forbids.insert(FORBIDDEN_SCALE_ARMOR);
    let conv = &mut d.systems.economy.items.conversion;
    conv.reagents.push(ItemConversionReagent {
        conversion_id: 8,
        impl_id: 3,
        min_level: 1,
        max_level: 20,
        min_item_grade: 0,
        max_item_grade: 5,
        ..ItemConversionReagent::default()
    });
    conv.products.push(ItemConversionProduct {
        conversion_id: 8,
        chance_rate: 5000,
        output_item_id: STACK,
        weight: 1,
        min_output: 2,
        max_output: 4,
    });
    d.systems
        .economy
        .item_effects
        .repairable_slaves
        .insert(3, 42);
    d
}

/// [`data`] plus the mate equipment tables: three slot packs and mate templates 501-504, with
/// the client's per-mate item list naming only [`WAIST_ARMOR`] for the riding mate.
fn mate_pack_data() -> GameData {
    let mut d = data();
    for (template, slot_type_id) in [(HEAD_ARMOR, 1_u32), (WAIST_ARMOR, 4), (FEET_ARMOR, 7)] {
        let mut armor = ItemTemplate::new(
            template,
            TemplateKind::Armor(EquipTemplate {
                slot_type_id,
                coverage: 10,
                durability_ratio: 1.0,
                durability_multiplier: 100,
                ..EquipTemplate::default()
            }),
        );
        armor.max_count = 1;
        armor.level = 1;
        d.items.templates.insert(template, armor);
    }
    for (id, pack_id) in [
        (MATE_RIDING, 1),
        (MATE_COMBAT, 2),
        (MATE_PET, 3),
        (MATE_NO_PACK, 0),
    ] {
        d.systems.npcs.npc.templates.insert(
            id,
            NpcTemplate {
                id,
                mate_equip_slot_pack_id: pack_id,
                ..NpcTemplate::default()
            },
        );
    }
    let rules = &mut d.systems.economy.mate_equipment;
    rules.slot_packs.insert(
        1,
        MateSlotPack {
            head: true,
            chest: false,
            waist: true,
            feet: true,
        },
    );
    rules.slot_packs.insert(
        2,
        MateSlotPack {
            head: true,
            chest: true,
            waist: false,
            feet: true,
        },
    );
    rules.slot_packs.insert(3, MateSlotPack::default());
    rules
        .pack_items
        .insert(MATE_RIDING, [WAIST_ARMOR].into_iter().collect());
    d
}

/// [`mate_pack_data`] with the FeatureSet bit the client gates the mate item list on (78 =
/// fset byte 9 bit 6, clear in `DEFAULT_FSET`).
fn mate_pack_data_with_whitelist(enabled: bool) -> GameData {
    let mut d = mate_pack_data();
    d.features.set_bit(78, enabled);
    d
}

/// Two players next to each other (char ids 1 / 2), their containers registered in the item
/// manager (`ItemManager.GetItemByItemId` lookups).
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

fn body(p: &impl Marshal) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write(&mut s);
    s.into_bytes()
}

fn tasks(action: u8, t: Vec<ItemTask>) -> Vec<u8> {
    body(&SCItemTaskSuccessPacket {
        action,
        tasks: t,
        force_remove: Vec::new(),
    })
}

fn error(e: i16) -> (u16, Vec<u8>) {
    (
        sc::SCErrorMsgPacket,
        body(&SCErrorMsgPacket::new(e, 0, true)),
    )
}

fn error_with_kind(e: i16, kind: u32) -> (u16, Vec<u8>) {
    (
        sc::SCErrorMsgPacket,
        body(&SCErrorMsgPacket::new(e, kind, true)),
    )
}

/// The `SkillResult` byte of the first `SCSkillStartedPacket` in `frames` (`None` when the cast
/// did not answer with a started packet).
fn started_skill_result(frames: &[(u16, Vec<u8>)]) -> Option<u8> {
    let (_, body) = frames
        .iter()
        .find(|(op, _)| *op == sc::SCSkillStartedPacket)?;
    let mut s = PacketStream::from_bytes(body.clone());
    s.read_u32().ok()?;
    s.read_u16().ok()?;
    let caster = s.read_u8().ok()?;
    SkillCaster::read(caster, &mut s).ok()?;
    let target = s.read_u8().ok()?;
    SkillCastTarget::read(target, &mut s).ok()?;
    let object = s.read_u8().ok()?;
    SkillObject::read(object, &mut s).ok()?;
    s.read_u16().ok()?;
    s.read_u16().ok()?;
    s.read_u8().ok()?;
    let flags = s.read_u8().ok()?;
    if flags & 1 == 0 {
        Some(0)
    } else {
        s.read_u8().ok()
    }
}

fn item(w: &World, id: u64) -> Option<Item> {
    Inv::of(&w.pa).item_by_id(id)
}

fn set_grade(w: &World, id: u64, grade: u8) {
    let c = Inv::of(&w.pa).holder_of(id).unwrap();
    c.lock().unwrap().get_item_by_id_mut(id).unwrap().grade = grade;
}

fn scroll() -> Option<SkillItem> {
    Some(SkillItem {
        item_id: 0,
        item_template_id: SCROLL,
    })
}

// ---------------------------------------------------------------------------------------------
// GradeEnchant
// ---------------------------------------------------------------------------------------------

fn enchant(w: &World, id: u64, support: Option<u64>, lucky: bool, rolls: &[i32]) -> ScriptedRng {
    let mut rng = ScriptedRng::new(rolls, &[]);
    grade_enchant(
        &w.server,
        &w.pa,
        scroll(),
        None,
        Some(id),
        support,
        [i32::from(lucky), 0, 2, 0],
        TL,
        &mut rng,
    );
    rng
}

#[tokio::test]
async fn grade_enchant_success() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 3);
    give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 1, 4);
    set_grade(&w, id, 2);
    w.pa.data().money = 1000;
    let rng = enchant(&w, id, None, false, &[4999, 9999, 9999, 9999]);
    assert_eq!(rng.int_calls, vec![(0, 10000); 4]);
    let after = item(&w, id).unwrap();
    assert_eq!(after.grade, 3);
    assert!(after.dirty);
    // cost = item_grade(enchant_cost 3) * 100 + level 10 + slot cost 5
    assert_eq!(w.pa.data().money, 1000 - 315);
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(
                    task_type::GRADE_ENCHANT,
                    vec![ItemTask::grade_change(&after, 3)]
                )
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-315)])
            ),
            (
                sc::SCGradeEnchantResultPacket,
                body(&SCGradeEnchantResultPacket {
                    result: grade_enchant_result::SUCCESS,
                    item: after.clone(),
                    type1: 2,
                    type2: 3,
                })
            ),
            (sc::SCSkillEndedPacket, body(&SCSkillEndedPacket { tl: TL })),
        ]
    );
    // Neighbours see the skill end; grade 3 is not announced.
    assert_eq!(w.b.drain_ops(), vec![sc::SCSkillEndedPacket]);
}

#[tokio::test]
async fn grade_enchant_great_success_with_charm_is_announced() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 1, 1);
    let charm = give(&w, &w.pa.clone(), slot_type::INVENTORY, CHARM, 2, 2);
    set_grade(&w, id, 1);
    w.pa.data().money = 1000;
    // lucky: success + great success (+2 + charm's 6 grades)
    enchant(&w, id, Some(charm), true, &[0, 0, 0, 0]);
    let after = item(&w, id).unwrap();
    assert_eq!(after.grade, 9);
    let charm_after = item(&w, charm).unwrap();
    assert_eq!(charm_after.count, 1);
    let result = SCGradeEnchantResultPacket {
        result: grade_enchant_result::GREAT_SUCCESS,
        item: after.clone(),
        type1: 1,
        type2: 9,
    };
    let announce = SCGradeEnchantBroadcastPacket {
        char_name: "Alice".into(),
        result: grade_enchant_result::GREAT_SUCCESS,
        item: after.clone(),
        type1: 1,
        type2: 9,
    };
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(
                    task_type::GRADE_ENCHANT,
                    vec![ItemTask::grade_change(&after, 9)]
                )
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-215)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(
                    task_type::GRADE_ENCHANT,
                    vec![ItemTask::count_update(&charm_after, -1)]
                )
            ),
            (sc::SCGradeEnchantResultPacket, body(&result)),
            (sc::SCSkillEndedPacket, body(&SCSkillEndedPacket { tl: TL })),
            (sc::SCGradeEnchantBroadcastPacket, body(&announce)),
        ]
    );
    assert_eq!(
        w.b.drain(),
        vec![
            (sc::SCSkillEndedPacket, body(&SCSkillEndedPacket { tl: TL })),
            (sc::SCGradeEnchantBroadcastPacket, body(&announce)),
        ]
    );
}

#[tokio::test]
async fn grade_enchant_break_removes_the_item() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 6);
    give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 1, 1);
    w.pa.data().money = 1000;
    let before = item(&w, id).unwrap();
    enchant(&w, id, None, false, &[9999, 999, 0, 0]);
    assert!(item(&w, id).is_none());
    assert_eq!(w.pa.data().money, 1000 - 115);
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(
                    task_type::GRADE_ENCHANT,
                    vec![ItemTask::remove_slot(&before)]
                )
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-115)])
            ),
            (
                sc::SCGradeEnchantResultPacket,
                body(&SCGradeEnchantResultPacket {
                    result: grade_enchant_result::BREAK,
                    item: before.clone(),
                    type1: 0,
                    type2: 0,
                })
            ),
            (sc::SCSkillEndedPacket, body(&SCSkillEndedPacket { tl: TL })),
        ]
    );
}

#[tokio::test]
async fn grade_enchant_downgrade_and_fail() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 1);
    set_grade(&w, id, 4);
    w.pa.data().money = 10_000;
    // Downgrade: Random.Next(1, 3) -> 2
    let rng = enchant(&w, id, None, false, &[9999, 9999, 1999, 9999, 2]);
    assert_eq!(rng.int_calls.last(), Some(&(1, 3)));
    let after = item(&w, id).unwrap();
    assert_eq!(after.grade, 2);
    let f = w.a.drain();
    assert_eq!(
        f[0].1,
        tasks(
            task_type::GRADE_ENCHANT,
            vec![ItemTask::grade_change(&after, 2)]
        )
    );
    assert_eq!(
        f[2].1,
        body(&SCGradeEnchantResultPacket {
            result: grade_enchant_result::DOWNGRADE,
            item: after.clone(),
            type1: 4,
            type2: 2,
        })
    );

    // Fail: nothing changes but money; the grade change task is still sent.
    enchant(&w, id, None, false, &[9999, 9999, 9999, 9999]);
    assert_eq!(item(&w, id).unwrap().grade, 2);
    let f = w.a.drain();
    assert_eq!(f.len(), 4);
    assert_eq!(
        f[2].1,
        body(&SCGradeEnchantResultPacket {
            result: grade_enchant_result::FAIL,
            item: after.clone(),
            type1: 2,
            type2: 2,
        })
    );
}

#[tokio::test]
async fn grade_enchant_refusals() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    // No money
    w.pa.data().money = 10;
    enchant(&w, id, None, false, &[0, 0, 0, 0]);
    assert_eq!(w.a.drain(), vec![error(error_message::NOT_ENOUGH_MONEY)]);
    // No scroll
    w.pa.data().money = 1000;
    enchant(&w, id, None, false, &[0, 0, 0, 0]);
    assert_eq!(
        w.a.drain(),
        vec![error(error_message::NOT_ENOUGH_REQUIRED_ITEM)]
    );
    // Charm above its max grade
    give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 1, 1);
    let charm = give(&w, &w.pa.clone(), slot_type::INVENTORY, CHARM, 1, 2);
    set_grade(&w, id, 7);
    enchant(&w, id, Some(charm), false, &[0, 0, 0, 0]);
    assert_eq!(w.a.drain(), vec![error(error_message::GRADE_ENCHANT_MAX)]);
    // Non-enchantable stack: the grade_enchantable gate refuses it before the kind check.
    let stack = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 1, 3);
    enchant(&w, stack, None, false, &[0, 0, 0, 0]);
    assert_eq!(w.a.drain(), vec![error(error_message::INVALID_TARGET)]);
    // Wrong item kind for value3 (2 = armor; a weapon is no armor): the client's
    // `FUN_393a0130` kind check refuses with INVALID_TARGET and refunds the scroll.
    let weapon = give(&w, &w.pa.clone(), slot_type::INVENTORY, WEAPON, 1, 4);
    w.pa.data().skill_char.skill_cancelled = false;
    enchant(&w, weapon, None, false, &[0, 0, 0, 0]);
    assert_eq!(w.a.drain(), vec![error(error_message::INVALID_TARGET)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
    assert_eq!(w.pa.data().money, 1000);
    assert!(w.b.drain().is_empty());
}

/// The 1.2 client refuses a non-`grade_enchantable` target before it sends the cast
/// (`FUN_393a0130`, switch case 0x5c -> SkillResult INVALID_TARGET 0x22). The server must refuse
/// it too, with the client's item error, before any roll, gold, charm or scroll.
#[tokio::test]
async fn grade_enchant_refuses_a_non_enchantable_target() {
    let mut w = world();
    let id = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        NON_ENCHANTABLE_ARMOR,
        1,
        0,
    );
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 1, 1);
    let charm = give(&w, &w.pa.clone(), slot_type::INVENTORY, CHARM, 1, 2);
    set_grade(&w, id, 3);
    w.pa.data().money = 1000;
    w.pa.data().skill_char.skill_cancelled = false;
    let rng = enchant(&w, id, Some(charm), true, &[0, 0, 0, 0]);
    assert!(rng.int_calls.is_empty(), "a refused target must not roll");
    assert_eq!(w.a.drain(), vec![error(error_message::INVALID_TARGET)]);
    assert!(
        w.pa.data().skill_char.skill_cancelled,
        "a refused regrade must cancel the cast so the scroll is refunded"
    );
    assert_eq!(item(&w, id).unwrap().grade, 3);
    assert_eq!(item(&w, scroll).unwrap().count, 1);
    assert_eq!(item(&w, charm).unwrap().count, 1);
    assert_eq!(w.pa.data().money, 1000);
    assert!(w.b.drain_ops().is_empty());
}

/// Runs the real `GradeEnchant` special effect: `special::execute` routes to the installed
/// economy hook, which calls `item_effects::grade_enchant`.
fn grade_enchant_effect(w: &World, scroll_item_id: u64, target_item_id: u64) {
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::state::SkillRun;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::WorldObject;

    let skill = SkillRun::new(
        Arc::new(crate::systems::skills::tests::skill_template(22520)),
        1,
    );
    let args = EffectArgs {
        caster: Some(WorldObject::from(&w.pa)),
        caster_caster: SkillCaster::Item {
            obj_id: w.pa.obj_id,
            item_id: scroll_item_id,
            item_template_id: SCROLL,
            type1: 0,
            type2: 0,
        },
        target: Target::Obj(WorldObject::from(&w.pa)),
        target_caster: SkillCastTarget::Item {
            obj_id: w.pa.obj_id,
            id: target_item_id,
            type1: 0,
            type2: 0,
        },
        cast: CastAction::Skill {
            skill_id: 22520,
            tl_id: 1,
        },
        source: EffectSource::from_skill(skill),
        skill_object: &SkillObject::None,
    };
    crate::systems::skills::effects::special::execute(&w.server, 92, &args, [0, 0, 2, 0]);
}

#[tokio::test]
async fn grade_enchant_effect_refuses_a_non_enchantable_target() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        NON_ENCHANTABLE_ARMOR,
        1,
        0,
    );
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 1, 1);
    set_grade(&w, target, 3);
    w.pa.data().money = 1000;
    w.pa.data().skill_char.skill_cancelled = false;
    grade_enchant_effect(&w, scroll, target);
    assert_eq!(w.a.drain(), vec![error(error_message::INVALID_TARGET)]);
    assert!(
        w.pa.data().skill_char.skill_cancelled,
        "a refused regrade must cancel the cast so the scroll is refunded"
    );
    assert_eq!(item(&w, target).unwrap().grade, 3);
    assert_eq!(item(&w, scroll).unwrap().count, 1);
    assert_eq!(w.pa.data().money, 1000);
}

#[tokio::test]
async fn grade_enchant_refuses_a_locked_target() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 1, 1);
    set_locked(&w, id);
    w.pa.data().money = 1000;
    enchant(&w, id, None, false, &[0, 0, 0, 0]);
    assert_eq!(
        w.a.drain(),
        vec![error(error_message::ITEM_SECURE_CONDITION)]
    );
    assert_eq!(item(&w, id).unwrap().grade, 0);
    assert_eq!(w.pa.data().money, 1000);
}

// ---------------------------------------------------------------------------------------------
// GradeEnchant through the real cast path (the scroll is debited and must be refunded)
// ---------------------------------------------------------------------------------------------

const GRADE_SKILL: u32 = 22520;
const GRADE_EFFECT: u32 = 12421;
const CHARM_MIN: u32 = 5101;
const CHARM_MAX: u32 = 5102;

fn grade_effect(value2: i32, value3: i32) -> SpecialEffect {
    SpecialEffect {
        id: GRADE_EFFECT,
        special_effect_type_id: 92,
        value1: 0,
        value2,
        value3,
        value4: 0,
    }
}

fn add_charm_support(data: &mut GameData, id: u32, min: i32, max: i32) {
    data.items.templates.insert(id, ItemTemplate::plain(id, 10));
    data.items.enchanting_supports.insert(
        id,
        ItemGradeEnchantingSupport {
            id,
            item_id: id,
            require_grade_min: min,
            require_grade_max: max,
            ..ItemGradeEnchantingSupport::default()
        },
    );
}

/// The server `systems::start_all` wires for a real item cast: skill hooks + the economy's
/// special-effect routing.
fn grade_cast_world(mut data: GameData, effect: SpecialEffect) -> World {
    {
        let scroll = data
            .items
            .templates
            .get_mut(&SCROLL)
            .expect("scroll template");
        scroll.use_skill_id = GRADE_SKILL;
        scroll.use_skill_as_reagent = true;
    }
    data.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(effect),
        GRADE_EFFECT,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: GRADE_SKILL,
        target_type: 9,
        max_range: 4,
        effects: vec![SkillEffect {
            effect_id: GRADE_EFFECT,
            template: Some((EffectKind::SpecialEffect, GRADE_EFFECT)),
            weight: 0,
            start_level: 0,
            end_level: 255,
            friendly: true,
            non_friendly: true,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            source_buff_tag_id: 0,
            source_no_buff_tag_id: 0,
            chance: 100,
            front: false,
            back: false,
            target_npc_tag_id: 0,
            application_method: 1,
            consume_source_item: false,
            consume_item_id: 0,
            consume_item_count: 1,
            always_hit: true,
            item_set_id: 0,
            interaction_success_hit: false,
        }],
        ..SkillTemplate::default()
    });
    let w = world_with_data(data);
    crate::systems::skills::bind(&w.server);
    super::bridges::install(&w.server);
    w
}

async fn cast_grade(w: &mut World, scroll: u64, target: u64, charm: Option<u64>) {
    CSStartSkillPacket {
        skill_id: GRADE_SKILL,
        caster: SkillCaster::Item {
            obj_id: w.pa.obj_id,
            item_id: scroll,
            item_template_id: SCROLL,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Item {
            obj_id: w.pa.obj_id,
            id: target,
            type1: 0,
            type2: 0,
        },
        flag: 0,
        skill_object: match charm {
            Some(support_item_id) => SkillObject::ItemGradeEnchantingSupport {
                id: 0,
                support_item_id,
                auto_use_aa_point: false,
            },
            None => SkillObject::None,
        },
    }
    .handle(&w.server, &mut w.a.conn)
    .await
    .unwrap();
}

/// The scroll was taken as the cast's source item; every refusal must refund it. The cast
/// clears `skill_cancelled` when it accepts, so a true value here was set by this cast's own
/// effect (proof the effect ran rather than the handler refusing the cast).
fn assert_scroll_refunded(w: &World, scroll: u64) {
    assert!(
        w.pa.data().skill_char.skill_cancelled,
        "the refusal must cancel the cast"
    );
    assert_eq!(
        item(w, scroll).expect("the scroll is back").count,
        2,
        "a refused regrade must refund the cast's source scroll"
    );
}

#[tokio::test]
async fn grade_enchant_cast_consumes_the_scroll_on_success() {
    let mut data = data();
    {
        let grade = data.items.full_grades.get_mut(&0).expect("grade 0");
        grade.enchant_success_ratio = 10_000;
        grade.enchant_great_success_ratio = 0;
        grade.enchant_break_ratio = 0;
        grade.enchant_downgrade_ratio = 0;
    }
    let mut w = grade_cast_world(data, grade_effect(0, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    w.pa.data().money = 1000;
    cast_grade(&mut w, scroll, target, None).await;
    assert!(
        !w.pa.data().skill_char.skill_cancelled,
        "an accepted regrade must not cancel the cast"
    );
    assert_eq!(item(&w, scroll).unwrap().count, 1);
    assert_eq!(item(&w, target).unwrap().grade, 1);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_when_the_target_is_locked() {
    let mut w = grade_cast_world(data(), grade_effect(0, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    set_locked(&w, target);
    w.pa.data().money = 1000;
    cast_grade(&mut w, scroll, target, None).await;
    assert_scroll_refunded(&w, scroll);
    assert!(w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
    assert_eq!(item(&w, target).unwrap().grade, 0);
    assert_eq!(w.pa.data().money, 1000);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_when_money_is_missing() {
    let mut w = grade_cast_world(data(), grade_effect(0, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    w.pa.data().money = 0;
    cast_grade(&mut w, scroll, target, None).await;
    assert_scroll_refunded(&w, scroll);
    assert!(w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
    assert_eq!(item(&w, target).unwrap().grade, 0);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_when_the_kind_does_not_match_value3() {
    let mut w = grade_cast_world(data(), grade_effect(0, 1));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    w.pa.data().money = 1000;
    cast_grade(&mut w, scroll, target, None).await;
    assert_scroll_refunded(&w, scroll);
    assert!(w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
    assert_eq!(item(&w, target).unwrap().grade, 0);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_below_the_scrolls_level_requirement() {
    let mut data = data();
    data.items
        .templates
        .get_mut(&ARMOR)
        .unwrap()
        .level_requirement = 10;
    let mut w = grade_cast_world(data, grade_effect(50, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    w.pa.data().money = 1000;
    cast_grade(&mut w, scroll, target, None).await;
    assert_scroll_refunded(&w, scroll);
    assert!(w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
    assert_eq!(item(&w, target).unwrap().grade, 0);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_at_the_max_grade() {
    let mut w = grade_cast_world(data(), grade_effect(0, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    set_grade(&w, target, 11);
    w.pa.data().money = 1000;
    cast_grade(&mut w, scroll, target, None).await;
    assert_scroll_refunded(&w, scroll);
    assert!(w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
    assert_eq!(item(&w, target).unwrap().grade, 11);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_below_the_charms_min_grade() {
    let mut data = data();
    add_charm_support(&mut data, CHARM_MIN, 5, -1);
    let mut w = grade_cast_world(data, grade_effect(0, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    let charm = give(&w, &w.pa.clone(), slot_type::INVENTORY, CHARM_MIN, 1, 2);
    set_grade(&w, target, 2);
    w.pa.data().money = 1000;
    cast_grade(&mut w, scroll, target, Some(charm)).await;
    assert_scroll_refunded(&w, scroll);
    assert!(w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
    assert_eq!(item(&w, charm).unwrap().count, 1);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_above_the_charms_max_grade() {
    let mut data = data();
    add_charm_support(&mut data, CHARM_MAX, -1, 6);
    let mut w = grade_cast_world(data, grade_effect(0, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    let charm = give(&w, &w.pa.clone(), slot_type::INVENTORY, CHARM_MAX, 1, 2);
    set_grade(&w, target, 7);
    w.pa.data().money = 10_000;
    cast_grade(&mut w, scroll, target, Some(charm)).await;
    assert_scroll_refunded(&w, scroll);
    assert!(w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
    assert_eq!(item(&w, charm).unwrap().count, 1);
}

#[tokio::test]
async fn grade_enchant_cast_refunds_the_scroll_when_the_target_template_is_unknown() {
    let mut w = grade_cast_world(data(), grade_effect(0, 2));
    let scroll = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(target)
        .unwrap()
        .template_id = 999_999;
    w.pa.data().money = 1000;
    cast_grade(&mut w, scroll, target, None).await;
    assert_scroll_refunded(&w, scroll);
    assert_eq!(item(&w, target).unwrap().grade, 0);
}

#[test]
fn charm_chance_math() {
    assert_eq!(charm_chance(5000, 100, 1000), 5000 + 100 + 50000);
    assert_eq!(charm_chance(333, 0, 75), 333 + 249); // (int)(333 * 0.75) = 249
    assert_eq!(charm_chance(1000, 0, -100), 0);
}

// ---------------------------------------------------------------------------------------------
// Socketing / tempering / looks / conversion
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn socketing_success_failure_and_dawnstone() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, SOCKET_ARMOR, 1, 0);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, GEM, 3, 1);
    let dawn = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        ITEM_DAWN_STONE,
        1,
        2,
    );
    let gem_caster = Some(SkillItem {
        item_id: gem,
        item_template_id: GEM,
    });
    // 1st socket: chance 10000
    mark_clean(&w, id);
    let mut rng = ScriptedRng::new(&[9999], &[]);
    item_socketing(
        &w.server,
        &w.pa,
        gem_caster,
        None,
        Some(id),
        [0; 4],
        TL,
        &mut rng,
    );
    let after = item(&w, id).unwrap();
    assert_eq!(after.details.gem_ids, [GEM, 0, 0, 0, 0, 0, 0]);
    assert!(after.dirty, "socketing must dirty the item");
    assert!(
        selected_for_save(&w, id),
        "the next save must include the socketed item"
    );
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::SOCKETING, vec![ItemTask::update(&after)])
            ),
            (
                sc::SCItemSocketingLunagemResultPacket,
                body(&SCItemSocketingLunagemResultPacket {
                    result: 1,
                    item_id: id,
                    template_id: GEM,
                    install: true,
                })
            ),
            (sc::SCSkillEndedPacket, body(&SCSkillEndedPacket { tl: TL })),
        ]
    );
    assert_eq!(w.b.drain_ops(), vec![sc::SCSkillEndedPacket]);

    // 2nd socket: chance 0 -> every gem is lost
    mark_clean(&w, id);
    let mut rng = ScriptedRng::new(&[0], &[]);
    item_socketing(
        &w.server,
        &w.pa,
        gem_caster,
        None,
        Some(id),
        [0; 4],
        TL,
        &mut rng,
    );
    let after = item(&w, id).unwrap();
    assert_eq!(after.details.gem_ids, [0; 7]);
    assert!(
        after.dirty,
        "a failed socket removes gems and must dirty the item"
    );
    assert!(selected_for_save(&w, id));
    let f = w.a.drain();
    assert_eq!(
        f[1].1,
        body(&SCItemSocketingLunagemResultPacket {
            result: 0,
            item_id: id,
            template_id: GEM,
            install: true,
        })
    );

    // Dawnstone: clears, result 1, not installed, no roll
    {
        let c = Inv::of(&w.pa).bag();
        c.lock()
            .unwrap()
            .get_item_by_id_mut(id)
            .unwrap()
            .details
            .gem_ids[3] = 77;
    }
    let mut rng = ScriptedRng::new(&[], &[]);
    let dawn_caster = Some(SkillItem {
        item_id: dawn,
        item_template_id: ITEM_DAWN_STONE,
    });
    mark_clean(&w, id);
    item_socketing(
        &w.server,
        &w.pa,
        dawn_caster,
        None,
        Some(id),
        [0; 4],
        TL,
        &mut rng,
    );
    assert!(rng.int_calls.is_empty());
    assert_eq!(item(&w, id).unwrap().details.gem_ids, [0; 7]);
    assert!(item(&w, id).unwrap().dirty, "DawnStone must dirty the item");
    assert!(selected_for_save(&w, id));
    let f = w.a.drain();
    assert_eq!(
        f[1].1,
        body(&SCItemSocketingLunagemResultPacket {
            result: 1,
            item_id: id,
            template_id: ITEM_DAWN_STONE,
            install: false,
        })
    );

    // Removing from an item with no gems is rejected with `item_sockets_empty`, without a roll.
    w.pa.data().skill_char.skill_cancelled = false;
    mark_clean(&w, id);
    let mut rng = ScriptedRng::new(&[], &[]);
    item_socketing(
        &w.server,
        &w.pa,
        dawn_caster,
        None,
        Some(id),
        [0; 4],
        TL,
        &mut rng,
    );
    assert!(rng.int_calls.is_empty());
    assert_eq!(w.a.drain(), vec![error(error_message::ITEM_SOCKETS_EMPTY)]);
    assert_eq!(item(&w, id).unwrap().details.gem_ids, [0; 7]);
    assert!(!item(&w, id).unwrap().dirty);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

/// Runs the real `ItemSocketing` special effect: `special::execute` routes to the installed
/// economy hook, which calls `item_effects::item_socketing`.
fn socketing_effect(w: &World, gem_item_id: u64, gem_template_id: u32, target_item_id: u64) {
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::state::SkillRun;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::WorldObject;

    let skill = SkillRun::new(
        Arc::new(crate::systems::skills::tests::skill_template(23728)),
        1,
    );
    let args = EffectArgs {
        caster: Some(WorldObject::from(&w.pa)),
        caster_caster: SkillCaster::Item {
            obj_id: w.pa.obj_id,
            item_id: gem_item_id,
            item_template_id: gem_template_id,
            type1: 0,
            type2: 0,
        },
        target: Target::Obj(WorldObject::from(&w.pa)),
        target_caster: SkillCastTarget::Item {
            obj_id: w.pa.obj_id,
            id: target_item_id,
            type1: 0,
            type2: 0,
        },
        cast: CastAction::Skill {
            skill_id: 23728,
            tl_id: 1,
        },
        source: EffectSource::from_skill(skill),
        skill_object: &SkillObject::None,
    };
    crate::systems::skills::effects::special::execute(&w.server, 106, &args, [0; 4]);
}

fn fill_gems(w: &World, id: u64, gems: &[u32]) {
    let c = Inv::of(&w.pa).bag();
    let mut g = c.lock().unwrap();
    let item = g.get_item_by_id_mut(id).unwrap();
    item.details.gem_ids.fill(0);
    item.details.gem_ids[..gems.len()].copy_from_slice(gems);
    item.dirty = false;
}

#[tokio::test]
async fn socketing_rejects_gem_for_the_wrong_slot_group() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, SOCKET_ARMOR, 1, 0);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, HANDS_GEM, 1, 1);
    mark_clean(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, HANDS_GEM, target);
    assert_eq!(w.a.drain(), vec![error(error_message::INVALID_TARGET)]);
    assert_eq!(item(&w, target).unwrap().details.gem_ids, [0; 7]);
    assert!(!item(&w, target).unwrap().dirty);
    assert!(
        w.pa.data().skill_char.skill_cancelled,
        "a rejected socketing must cancel the cast so the gem is refunded"
    );
}

#[tokio::test]
async fn socketing_rejects_a_gem_above_the_target_level_limit() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, LOW_ARMOR, 1, 0);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, GEM, 1, 1);
    mark_clean(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, GEM, target);
    assert_eq!(w.a.drain(), vec![error(error_message::SOCKET_TARGET_LEVEL)]);
    assert_eq!(item(&w, target).unwrap().details.gem_ids, [0; 7]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn socketing_rejects_a_target_below_the_minimum_item_level() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, LOW_GEM, 1, 1);
    mark_clean(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, LOW_GEM, target);
    assert_eq!(w.a.drain(), vec![error(error_message::SOCKET_TARGET_LEVEL)]);
    assert_eq!(item(&w, target).unwrap().details.gem_ids, [0; 7]);
}

#[tokio::test]
async fn socketing_rejects_a_target_with_no_num_socket_limit() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, SOCKET_ARMOR, 1, 0);
    // Slot 3 has a limit only for grade 0; grade 1 has no row (0 sockets).
    set_grade(&w, target, 1);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, GEM, 1, 1);
    mark_clean(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, GEM, target);
    assert_eq!(w.a.drain(), vec![error(error_message::INVALID_TARGET)]);
    assert_eq!(item(&w, target).unwrap().details.gem_ids, [0; 7]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn socketing_level_limit_wins_over_the_slot_group() {
    let mut w = world();
    super::bridges::install(&w.server);
    // LOW_ARMOR requires level 40; FAIL_GEM needs 50 and is a hands gem on a chest armor.
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, LOW_ARMOR, 1, 0);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, FAIL_GEM, 1, 1);
    mark_clean(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, FAIL_GEM, target);
    assert_eq!(w.a.drain(), vec![error(error_message::SOCKET_TARGET_LEVEL)]);
    assert_eq!(item(&w, target).unwrap().details.gem_ids, [0; 7]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn socketing_enforces_the_num_socket_limit() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, SOCKET_ARMOR, 1, 0);
    // Slot 3 (chest), grade 0 allows 2 gems in the test data.
    fill_gems(&w, target, &[GEM, GEM]);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, GEM, 1, 1);
    mark_clean(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, GEM, target);
    assert_eq!(w.a.drain(), vec![error(error_message::ITEM_SOCKETS_FULL)]);
    assert_eq!(item(&w, target).unwrap().details.gem_ids[2], 0);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn socketing_refuses_a_locked_target() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, SOCKET_ARMOR, 1, 0);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, GEM, 1, 1);
    set_locked(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, GEM, target);
    assert_eq!(
        w.a.drain(),
        vec![error(error_message::ITEM_SECURE_CONDITION)]
    );
    assert_eq!(item(&w, target).unwrap().details.gem_ids, [0; 7]);
    assert!(item(&w, gem).is_some());
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn socketing_a_valid_gem_still_succeeds() {
    let mut w = world();
    super::bridges::install(&w.server);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, SOCKET_ARMOR, 1, 0);
    let gem = give(&w, &w.pa.clone(), slot_type::INVENTORY, GEM, 1, 1);
    mark_clean(&w, target);
    w.pa.data().skill_char.skill_cancelled = false;
    socketing_effect(&w, gem, GEM, target);
    let after = item(&w, target).unwrap();
    assert_eq!(after.details.gem_ids, [GEM, 0, 0, 0, 0, 0, 0]);
    assert!(after.dirty);
    assert!(!w.pa.data().skill_char.skill_cancelled);
    let frames = w.a.drain();
    assert!(frames.iter().any(|(opcode, raw)| {
        *opcode == sc::SCItemSocketingLunagemResultPacket
            && *raw
                == body(&SCItemSocketingLunagemResultPacket {
                    result: 1,
                    item_id: target,
                    template_id: GEM,
                    install: true,
                })
    }));
}

/// The lunafrost skill's `InteractionEffect` (`MAGICAL_ENCHANT`) carries the rune as the
/// `SkillItem` caster and the armor as the `SkillCastItemTarget`: the rune must reach
/// `magical_enchant` through the effect bridge.
fn lunafrost_interaction(
    w: &World,
    caster: crate::systems::skills::types::SkillCaster,
    target: crate::systems::skills::types::SkillCastTarget,
) {
    use crate::systems::npcs::doodad::interactions::world_interaction_type as wi;
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::state::SkillRun;
    use crate::systems::skills::types::{CastAction, SkillObject};
    use crate::world::WorldObject;

    let skill = SkillRun::new(
        Arc::new(crate::systems::skills::tests::skill_template(11285)),
        1,
    );
    let args = EffectArgs {
        caster: Some(WorldObject::from(&w.pa)),
        caster_caster: caster,
        target: Target::Obj(WorldObject::from(&w.pa)),
        target_caster: target,
        cast: CastAction::Skill {
            skill_id: 11285,
            tl_id: 0,
        },
        source: EffectSource::from_skill(skill),
        skill_object: &SkillObject::None,
    };
    crate::systems::npcs::bridges::interaction_effect(
        &w.server,
        wi::MAGICAL_ENCHANT as i32,
        0,
        &args,
    );
}

#[tokio::test]
async fn lunafrost_interaction_effect_enchants_bag_and_equipped_items() {
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster};

    let w = world();
    super::bridges::install(&w.server);
    let rune = SkillCaster::Item {
        obj_id: w.pa.obj_id,
        item_id: 1,
        item_template_id: GEM,
        type1: 0,
        type2: 0,
    };
    let target = |id| SkillCastTarget::Item {
        obj_id: w.pa.obj_id,
        id,
        type1: 0,
        type2: 0,
    };
    let bag = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    let worn = give(
        &w,
        &w.pa.clone(),
        slot_type::EQUIPMENT,
        ARMOR,
        1,
        equip_slot::CHEST,
    );

    // Not an item target: nothing is enchanted.
    lunafrost_interaction(&w, rune, SkillCastTarget::Unit(w.pa.obj_id));
    assert_eq!(item(&w, bag).unwrap().details.rune_id, 0);

    lunafrost_interaction(&w, rune, target(bag));
    assert_eq!(item(&w, bag).unwrap().details.rune_id, GEM);
    assert!(item(&w, bag).unwrap().dirty);

    // An equipped item can be enchanted too.
    lunafrost_interaction(&w, rune, target(worn));
    assert_eq!(item(&w, worn).unwrap().details.rune_id, GEM);

    // A non-SkillItem caster enchants nothing.
    let other = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    lunafrost_interaction(&w, SkillCaster::Unit(w.pa.obj_id), target(other));
    assert_eq!(item(&w, other).unwrap().details.rune_id, 0);
}

#[tokio::test]
async fn lunastone_cap_scale_and_reset() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    let caster = Some(SkillItem {
        item_id: 1,
        item_template_id: GEM,
    });
    mark_clean(&w, id);
    magical_enchant(&w.server, &w.pa, caster, Some(id));
    let after = item(&w, id).unwrap();
    assert_eq!(after.details.rune_id, GEM);
    assert!(after.dirty, "a lunastone must dirty the item");
    assert!(
        selected_for_save(&w, id),
        "the next save must include the lunastone"
    );
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemSocketingLunastoneResultPacket,
                body(&SCItemSocketingLunastoneResultPacket {
                    result: true,
                    item_id: id,
                    template_id: GEM,
                })
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::ENCHANT_MAGICAL, vec![ItemTask::update(&after)])
            ),
        ]
    );

    let mut rng = ScriptedRng::new(&[55, 77], &[]);
    mark_clean(&w, id);
    item_cap_scale(&w.server, &w.pa, caster, Some(id), 900, [0; 4], &mut rng);
    assert_eq!(rng.int_calls, vec![(0, 100), (0, 100)]);
    let after = item(&w, id).unwrap();
    assert_eq!(
        (after.details.temper_physical, after.details.temper_magical),
        (55, 77)
    );
    assert!(after.dirty, "tempering must dirty the item");
    assert!(
        selected_for_save(&w, id),
        "the next save must include the tempered item"
    );
    let msg = |m: &str| SCChatMessagePacket {
        chat_type: chat_type::SYSTEM,
        sender: None,
        message: m.into(),
        ability: 0,
        language_type: 0,
    };
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::ENCHANT_PHYSICAL, vec![ItemTask::update(&after)])
            ),
            (
                sc::SCChatMessagePacket,
                body(&msg(
                    "Temper:\n |cFFFFFFFF55%|r Physical\n|cFFFFFFFF77%|r Magical"
                ))
            ),
        ]
    );

    mark_clean(&w, id);
    item_cap_scale_reset(&w.server, &w.pa, caster, Some(id), [0; 4]);
    let after = item(&w, id).unwrap();
    assert_eq!(after.details.temper_physical, 0);
    assert!(after.dirty, "a temper reset must dirty the item");
    assert!(selected_for_save(&w, id));
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::ENCHANT_PHYSICAL, vec![ItemTask::update(&after)])
            ),
            (sc::SCChatMessagePacket, body(&msg("Tempering reset"))),
        ]
    );
}

/// `item_cap_scale_forbids` (1.2 client `FUN_395a72d0`, the item's "scalable" property;
/// the cap-scale target check `FUN_3939dc20` answers SkillResult INVALID_TARGET 0x22): a listed
/// template cannot be tempered or reset, and the refusal cancels the cast so the source item
/// (the temper) is refunded.
const WRAPPED_ROD: u32 = 2740;
const ROD_SKILL: u32 = 6200;
const ROD_EFFECT: u32 = 6201;

/// The wrapped fishing rods use an item skill that consumes the wrapped item and gives the
/// unwrapped rod (effect type 27); the use action is gated by `items.actability_requirement`.
fn rod_cast_world(mut data: GameData) -> World {
    let mut rod = ItemTemplate::new(WRAPPED_ROD, TemplateKind::Item);
    rod.max_count = 1;
    rod.use_skill_id = ROD_SKILL;
    rod.actability_group_id = 7;
    rod.actability_requirement = 10000;
    data.items.templates.insert(WRAPPED_ROD, rod);
    data.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: ROD_EFFECT,
            special_effect_type_id: 27,
            value1: STACK as i32,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        ROD_EFFECT,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: ROD_SKILL,
        target_type: 0,
        effects: vec![SkillEffect {
            effect_id: ROD_EFFECT,
            template: Some((EffectKind::SpecialEffect, ROD_EFFECT)),
            weight: 0,
            start_level: 0,
            end_level: 255,
            friendly: true,
            non_friendly: true,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            source_buff_tag_id: 0,
            source_no_buff_tag_id: 0,
            chance: 100,
            front: false,
            back: false,
            target_npc_tag_id: 0,
            application_method: 1,
            consume_source_item: true,
            consume_item_id: 0,
            consume_item_count: 1,
            always_hit: true,
            item_set_id: 0,
            interaction_success_hit: false,
        }],
        ..SkillTemplate::default()
    });
    let w = world_with_data(data);
    crate::systems::skills::bind(&w.server);
    super::bridges::install(&w.server);
    w
}

async fn cast_wrapped_rod(w: &mut World, item_id: u64) {
    CSStartSkillPacket {
        skill_id: ROD_SKILL,
        caster: SkillCaster::Item {
            obj_id: w.pa.obj_id,
            item_id,
            item_template_id: WRAPPED_ROD,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(w.pa.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    }
    .handle(&w.server, &mut w.a.conn)
    .await
    .unwrap();
}

const CAP_SCALE_SKILL: u32 = 900;
const CAP_SCALE_EFFECT: u32 = 9100;

/// A skill world where `SCROLL` carries the real cap-scale use skill: special effect 112, item
/// target, source item consumed. `data()` already has the `item_cap_scales` row for skill 900
/// and `FORBIDDEN_SCALE_ARMOR` in `item_cap_scale_forbids`.
fn cap_scale_cast_world(mut data: GameData) -> World {
    {
        let temper = data
            .items
            .templates
            .get_mut(&SCROLL)
            .expect("temper template");
        temper.use_skill_id = CAP_SCALE_SKILL;
    }
    data.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: CAP_SCALE_EFFECT,
            special_effect_type_id: 112,
            value1: 0,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        CAP_SCALE_EFFECT,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: CAP_SCALE_SKILL,
        target_type: 9,
        max_range: 4,
        effects: vec![SkillEffect {
            effect_id: CAP_SCALE_EFFECT,
            template: Some((EffectKind::SpecialEffect, CAP_SCALE_EFFECT)),
            weight: 0,
            start_level: 0,
            end_level: 255,
            friendly: true,
            non_friendly: true,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            source_buff_tag_id: 0,
            source_no_buff_tag_id: 0,
            chance: 100,
            front: false,
            back: false,
            target_npc_tag_id: 0,
            application_method: 1,
            consume_source_item: true,
            consume_item_id: 0,
            consume_item_count: 1,
            always_hit: true,
            item_set_id: 0,
            interaction_success_hit: false,
        }],
        ..SkillTemplate::default()
    });
    let w = world_with_data(data);
    crate::systems::skills::bind(&w.server);
    super::bridges::install(&w.server);
    w
}

async fn cast_cap_scale(w: &mut World, temper: u64, target: u64) {
    CSStartSkillPacket {
        skill_id: CAP_SCALE_SKILL,
        caster: SkillCaster::Item {
            obj_id: w.pa.obj_id,
            item_id: temper,
            item_template_id: SCROLL,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Item {
            obj_id: w.pa.obj_id,
            id: target,
            type1: 0,
            type2: 0,
        },
        flag: 0,
        skill_object: SkillObject::None,
    }
    .handle(&w.server, &mut w.a.conn)
    .await
    .unwrap();
}

/// The 1.2 client refuses the cap-scale cast on an `item_cap_scale_forbids` target with
/// SkillResult INVALID_TARGET 0x22 (`FUN_3939dc20`); the server answers the same result and
/// keeps the temper item.
#[tokio::test]
async fn item_cap_scale_cast_fails_with_invalid_target_for_forbidden_items() {
    let mut w = cap_scale_cast_world(data());
    let temper = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        FORBIDDEN_SCALE_ARMOR,
        1,
        1,
    );
    mark_clean(&w, target);
    w.a.drain();
    cast_cap_scale(&mut w, temper, target).await;
    let frames = w.a.drain();
    assert_eq!(
        started_skill_result(&frames),
        Some(skill_result::INVALID_TARGET)
    );
    assert_eq!(
        item(&w, temper).expect("the temper is kept").count,
        2,
        "a refused cap-scale cast must not consume the temper"
    );
    let after = item(&w, target).unwrap();
    assert_eq!(
        (after.details.temper_physical, after.details.temper_magical),
        (0, 0)
    );
    assert!(!after.dirty);
}

#[tokio::test]
async fn item_cap_scale_cast_tempers_an_allowed_target() {
    let mut w = cap_scale_cast_world(data());
    let temper = give(&w, &w.pa.clone(), slot_type::INVENTORY, SCROLL, 2, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    w.a.drain();
    cast_cap_scale(&mut w, temper, target).await;
    assert_eq!(
        item(&w, temper).expect("temper").count,
        1,
        "an accepted cap-scale cast consumes one temper"
    );
    assert!(item(&w, target).expect("target").dirty);
}

/// The wrapped rod's use skill is refused below `actability_requirement` (client
/// `FUN_394e1a90`, error 508) and the wrapped item is kept; at the requirement it is consumed
/// and the unwrapped item is given.
#[tokio::test]
async fn using_a_wrapped_fishing_rod_requires_its_actability_points() {
    let mut w = rod_cast_world(data());
    let wrapped = give(&w, &w.pa.clone(), slot_type::INVENTORY, WRAPPED_ROD, 1, 0);
    w.a.drain();
    cast_wrapped_rod(&mut w, wrapped).await;
    let frames = w.a.drain();
    assert!(
        frames.contains(&error_with_kind(
            error_message::ACTABILITY_NOT_ENOUGH_POINT,
            7
        )),
        "the use below the requirement shows error 508 with the fishing group"
    );
    assert_eq!(item(&w, wrapped).expect("the wrapped rod is kept").count, 1);

    w.pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 7,
            point: 10000,
            step: 1,
        });
    cast_wrapped_rod(&mut w, wrapped).await;
    assert!(
        item(&w, wrapped).is_none(),
        "the accepted use consumes the wrapped rod"
    );
    assert!(
        Inv::of(&w.pa)
            .bag()
            .lock()
            .unwrap()
            .items
            .iter()
            .any(|i| i.template_id == STACK),
        "the use gives the unwrapped item"
    );
}

#[tokio::test]
async fn item_cap_scale_refuses_forbidden_targets() {
    let mut w = world();
    let id = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        FORBIDDEN_SCALE_ARMOR,
        1,
        0,
    );
    let caster = Some(SkillItem {
        item_id: 1,
        item_template_id: GEM,
    });
    mark_clean(&w, id);
    w.pa.data().skill_char.skill_cancelled = false;
    let mut rng = ScriptedRng::new(&[], &[]);
    item_cap_scale(&w.server, &w.pa, caster, Some(id), 900, [0; 4], &mut rng);
    assert!(w.pa.data().skill_char.skill_cancelled);
    assert!(
        rng.int_calls.is_empty(),
        "the refusal must happen before the scale roll"
    );
    let after = item(&w, id).unwrap();
    assert_eq!(
        (after.details.temper_physical, after.details.temper_magical),
        (0, 0)
    );
    assert!(!after.dirty, "a refused temper must not dirty the item");
    assert!(
        w.a.drain().is_empty(),
        "the cast validation is what reports INVALID_TARGET; the direct call is silent"
    );

    // The reset tool is refused by the same set and leaves the existing temper untouched.
    {
        let c = Inv::of(&w.pa).bag();
        let mut c = c.lock().unwrap();
        let it = c.get_item_by_id_mut(id).unwrap();
        it.details.temper_physical = 5;
        it.details.temper_magical = 6;
        it.dirty = false;
    }
    w.pa.data().skill_char.skill_cancelled = false;
    item_cap_scale_reset(&w.server, &w.pa, caster, Some(id), [0; 4]);
    assert!(w.pa.data().skill_char.skill_cancelled);
    let after = item(&w, id).unwrap();
    assert_eq!(
        (after.details.temper_physical, after.details.temper_magical),
        (5, 6)
    );
    assert!(!after.dirty);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn cleanup_look_convert_flow() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    let powder = give(&w, &w.pa.clone(), slot_type::INVENTORY, POWDER, 2, 1);
    let caster = Some(SkillItem {
        item_id: powder,
        item_template_id: POWDER,
    });
    // No image yet
    assert_eq!(
        cleanup_look_convert(&w.server, &w.pa, caster, None, false, Some(id), [0; 4]),
        Outcome::Cancelled
    );
    assert_eq!(
        w.a.drain(),
        vec![error(
            error_message::ITEM_LOOK_CONVERT_AS_INVALID_COMBINATION
        )]
    );
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .details
        .image_item_template_id = 999;
    assert_eq!(
        cleanup_look_convert(&w.server, &w.pa, caster, None, false, Some(id), [0; 4]),
        Outcome::Done
    );
    let after = item(&w, id).unwrap();
    assert_eq!(after.details.image_item_template_id, 0);
    let p = item(&w, powder).unwrap();
    assert_eq!(p.count, 1);
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::CONVERT_ITEM_LOOK, vec![ItemTask::update(&after)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(
                    task_type::CONVERT_ITEM_LOOK,
                    vec![ItemTask::count_update(&p, -1)]
                )
            ),
        ]
    );
}

#[tokio::test]
async fn item_conversion_flow() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 0);
    let before = item(&w, id).unwrap();
    // Product roll 4999 < 5000, amount Random.Next(2, 5) -> 3
    let mut rng = ScriptedRng::new(&[4999, 3], &[]);
    assert_eq!(
        item_conversion(&w.server, &w.pa, Some(id), [0; 4], &mut rng),
        Outcome::Done
    );
    assert_eq!(rng.int_calls, vec![(0, 10000), (2, 5)]);
    assert!(item(&w, id).is_none());
    let product = item_at(&w.pa, slot_type::INVENTORY, 1).unwrap();
    assert_eq!((product.template_id, product.count), (STACK, 3));
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::CONVERSION, vec![ItemTask::add(&product)])
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::CONVERSION, vec![ItemTask::remove_slot(&before)])
            ),
            // ConsumeItem's own (empty) task list
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::CONVERSION, vec![])
            ),
        ]
    );

    // Failed product roll: the item is still consumed
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 5);
    let mut rng = ScriptedRng::new(&[5000], &[]);
    assert_eq!(
        item_conversion(&w.server, &w.pa, Some(id), [0; 4], &mut rng),
        Outcome::Done
    );
    assert!(item(&w, id).is_none());
    assert_eq!(
        w.a.drain_ops(),
        vec![sc::SCItemTaskSuccessPacket, sc::SCItemTaskSuccessPacket]
    );

    // Not in the bag / not a SkillCastItemTarget: cancelled
    assert_eq!(
        item_conversion(&w.server, &w.pa, Some(12345), [0; 4], &mut rng),
        Outcome::Cancelled
    );
    assert_eq!(
        item_conversion(&w.server, &w.pa, None, [0; 4], &mut rng),
        Outcome::Cancelled
    );
}

#[tokio::test]
async fn gain_item_effect() {
    let mut w = world();
    gain_item(&w.server, &w.pa, [STACK as i32, 0, 0, 0]);
    let i = item_at(&w.pa, slot_type::INVENTORY, 0).unwrap();
    assert_eq!((i.template_id, i.count), (STACK, 1));
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCItemTaskSuccessPacket,
            tasks(task_type::SKILL_EFFECT_GAIN_ITEM, vec![ItemTask::add(&i)])
        )]
    );
    gain_item(&w.server, &w.pa, [0, 5, 0, 0]);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn gain_item_that_does_not_fit_reports_a_full_bag() {
    let mut w = world();
    Inv::of(&w.pa).bag().lock().unwrap().set_container_size(0);
    gain_item(&w.server, &w.pa, [STACK as i32, 1, 0, 0]);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].0, sc::SCErrorMsgPacket);
    assert_eq!(
        &frames[0].1[..2],
        &super::error_message::BAG_FULL.to_le_bytes()
    );
}

// ---------------------------------------------------------------------------------------------
// GainItemWithPosImprint / ExpToItem / RechargeItemBuff (data-derived bodies of C# stubs)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn gain_item_with_pos_imprint_adds_the_guide_item() {
    let mut w = world();
    gain_item_with_pos_imprint(&w.server, &w.pa, [STACK as i32, 0, 0, 0]);
    let i = item_at(&w.pa, slot_type::INVENTORY, 0).unwrap();
    assert_eq!((i.template_id, i.count), (STACK, 1));
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCItemTaskSuccessPacket,
            tasks(
                task_type::SKILL_EFFECT_GAIN_ITEM_WITH_POS,
                vec![ItemTask::add(&i)]
            )
        )]
    );
    // Row 405's template 5054 is missing in the 1.2 data; a non-positive id is invalid too.
    // Neither panics nor adds an item.
    gain_item_with_pos_imprint(&w.server, &w.pa, [999_999, 0, 0, 0]);
    gain_item_with_pos_imprint(&w.server, &w.pa, [0, 0, 0, 0]);
    assert!(w.a.drain().is_empty());
    assert_eq!(Inv::of(&w.pa).bag().lock().unwrap().items.len(), 1);
}

#[tokio::test]
async fn exp_to_item_spends_experience_and_creates_the_item() {
    let mut w = world();
    w.pa.data().experience = 25_000;
    assert_eq!(
        exp_to_item(&w.server, &w.pa, [10_000, TIMEGLASS as i32, 0, 0]),
        Outcome::Done
    );
    assert_eq!(w.pa.data().experience, 15_000);
    let i = item_at(&w.pa, slot_type::INVENTORY, 0).unwrap();
    assert_eq!((i.template_id, i.count), (TIMEGLASS, 1));
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCExpChangedPacket,
                body(&SCExpChangedPacket {
                    obj_id: w.pa.obj_id,
                    exp: -10_000,
                    should_add_ability_exp: false,
                })
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(task_type::SKILL_EFFECT_EXP_TO_ITEM, vec![ItemTask::add(&i)])
            ),
        ]
    );
}

#[tokio::test]
async fn exp_to_item_cancels_without_enough_experience() {
    let mut w = world();
    w.pa.data().experience = 5_000;
    assert_eq!(
        exp_to_item(&w.server, &w.pa, [10_000, TIMEGLASS as i32, 0, 0]),
        Outcome::Cancelled
    );
    assert_eq!(w.pa.data().experience, 5_000);
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_none());
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn recharge_item_buff_starts_a_new_charge_window() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, RECHARGEABLE, 1, 0);
    {
        let c = Inv::of(&w.pa).bag();
        let mut g = c.lock().unwrap();
        let it = g.get_item_by_id_mut(id).unwrap();
        it.charge_start_time = 1000;
        it.charge_count = 0;
        it.dirty = false;
    }
    w.a.drain();
    let now = chrono::Utc::now().timestamp();
    assert_eq!(
        recharge_item_buff(&w.server, &w.pa, Some(id), [0; 4]),
        Outcome::Done
    );
    let after = item(&w, id).unwrap();
    assert!(
        after.charge_start_time >= now && after.charge_start_time <= chrono::Utc::now().timestamp(),
        "the charge window must start now"
    );
    assert_eq!(after.charge_count, 3, "charge_count resets to the template");
    assert!(after.dirty, "the new charge window must be saved");
    assert!(selected_for_save(&w, id));
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCItemTaskSuccessPacket,
            tasks(task_type::RECHARGE_BUFF, vec![ItemTask::update(&after)])
        )]
    );
}

#[tokio::test]
async fn recharge_item_buff_cancels_on_a_non_rechargeable_target() {
    let mut w = world();
    let armor = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 0);
    let before = item(&w, armor).unwrap();
    assert_eq!(
        recharge_item_buff(&w.server, &w.pa, Some(armor), [0; 4]),
        Outcome::Cancelled
    );
    assert_eq!(item(&w, armor).unwrap(), before);
    assert!(w.a.drain().is_empty());
    assert_eq!(
        recharge_item_buff(&w.server, &w.pa, None, [0; 4]),
        Outcome::Cancelled
    );
    assert_eq!(
        recharge_item_buff(&w.server, &w.pa, Some(12345), [0; 4]),
        Outcome::Cancelled
    );
}

// ---------------------------------------------------------------------------------------------
// UCC
// ---------------------------------------------------------------------------------------------

fn ucc_world() -> World {
    ucc_world_with(true)
}

fn ucc_world_with(as_reagent: bool) -> World {
    ucc_world_with_tables(as_reagent, &[], &[])
}

/// [`ucc_world_with`] with the WP-U2 tables: `applicable` = `ucc_applicables` kind-2 template
/// ids, `patterns` = `emblem_patterns` ids (empty = permissive).
fn ucc_world_with_tables(as_reagent: bool, applicable: &[u32], patterns: &[u32]) -> World {
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    let mut d = data();
    // Authentic 1.2 Crest Ink / Crest Bleach are use-skill reagents. Their effect rows
    // consume_source_item=false; the skill still debits one source after successful apply.
    for (item_id, skill_id) in [(17662, 14433), (19780, 16046)] {
        let mut item = ItemTemplate::plain(item_id, 10);
        item.use_skill_id = skill_id;
        item.use_skill_as_reagent = as_reagent;
        d.items.templates.insert(item_id, item);
    }
    d.systems.skills.add_effect(
        EffectKind::ImprintUccEffect,
        EffectTemplate::ImprintUcc { id: 5, item_id: 0 },
        0,
    );
    d.systems.skills.add_effect(
        EffectKind::CleanupUccEffect,
        EffectTemplate::CleanupUcc { id: 1 },
        0,
    );
    for id in applicable {
        d.systems.economy.ucc.insert_applicable_item(*id);
    }
    for id in patterns {
        d.systems.economy.ucc.insert_pattern(*id);
    }
    let w = world_with_data(d);
    super::bridges::install(&w.server);
    w
}

/// A stamp item carrying `ucc_id`, registered in the `UccManager` with the given patterns.
fn ucc_stamp(w: &World, slot: i32, ucc_id: u64, pattern1: u32, pattern2: u32) -> u64 {
    use crate::stream::models::{Ucc, UccType};
    let stamp = give(w, &w.pa, slot_type::INVENTORY, STACK, 1, slot);
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(stamp)
        .unwrap()
        .set_ucc(ucc_id);
    let mut ucc = Ucc::new(UccType::Simple, 1);
    ucc.id = ucc_id as u32;
    ucc.pattern1 = pattern1;
    ucc.pattern2 = pattern2;
    w.server.streams.ucc.insert(ucc);
    stamp
}

fn ucc_source_template(cleanup: bool) -> u32 {
    if cleanup {
        19780
    } else {
        17662
    }
}

// UCC and ownership are not both included in every item class's wire details.
fn ucc_item_state(w: &World, id: u64) -> (Vec<u8>, u64, u64, bool) {
    // Unlike the older single-owner `item` helper, this observation must cover Bob's
    // registered containers too, before and after the attempted foreign-item cast.
    let item = w.server.items.item_by_id(id).expect("UCC fixture item");
    (body(&item), item.ucc_id, item.owner_id, item.dirty)
}

/// [`ucc_item_state`] without `dirty`: a cancelled cast that debited a partial stack marks it
/// dirty while restoring its count, which is not a data change.
fn ucc_item_values(w: &World, id: u64) -> (Vec<u8>, u64, u64) {
    let (body, ucc, owner, _) = ucc_item_state(w, id);
    (body, ucc, owner)
}

/// The `SCItemTaskSuccessPacket` bodies a cast sent, in order.
fn item_task_bodies(packets: &[(u16, Vec<u8>)]) -> Vec<Vec<u8>> {
    packets
        .iter()
        .filter(|(op, _)| *op == sc::SCItemTaskSuccessPacket)
        .map(|(_, body)| body.clone())
        .collect()
}

/// The take/refund pair of a cancelled partial debit of one unit: count -1 then +1.
fn partial_debit_refund_tasks(item: &crate::models::item::Item) -> Vec<Vec<u8>> {
    vec![
        tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::count_update(item, -1)],
        ),
        tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::count_update(item, 1)],
        ),
    ]
}

/// Exercise the real effect dispatch followed by skill-owned source consumption, rather
/// than directly calling an effect and simulating its debit in the test.
fn cast_ucc(
    w: &World,
    cleanup: bool,
    source: SkillItem,
    target_id: u64,
    target_player: &PlayerRef,
) -> crate::systems::skills::state::SkillRef {
    cast_ucc_flags(w, cleanup, source, target_id, target_player, &[false])
}

/// One effect per entry of `consume_flags` (its `consume_source_item`), in order.
fn cast_ucc_flags(
    w: &World,
    cleanup: bool,
    source: SkillItem,
    target_id: u64,
    target_player: &PlayerRef,
    consume_flags: &[bool],
) -> crate::systems::skills::state::SkillRef {
    use crate::systems::skills::data::{EffectKind, SkillEffect, SkillTemplate};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::skill::{apply_effects, CastCtx};
    use crate::systems::skills::state::SkillRun;
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::WorldObject;

    let (skill_id, kind, effect_id) = if cleanup {
        (16046, EffectKind::CleanupUccEffect, 1)
    } else {
        (14433, EffectKind::ImprintUccEffect, 5)
    };
    let effects: Vec<SkillEffect> = consume_flags
        .iter()
        .map(|&consume| SkillEffect {
            effect_id,
            template: Some((kind, effect_id)),
            weight: 0,
            start_level: 1,
            end_level: 99,
            friendly: true,
            non_friendly: true,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            source_buff_tag_id: 0,
            source_no_buff_tag_id: 0,
            chance: 100,
            front: true,
            back: true,
            target_npc_tag_id: 0,
            application_method: 1,
            consume_source_item: consume,
            consume_item_id: 0,
            consume_item_count: 1,
            always_hit: false,
            item_set_id: 0,
            interaction_success_hit: false,
        })
        .collect();
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            id: skill_id,
            target_type: 9,
            target_selection: 2,
            effects,
            ..SkillTemplate::default()
        }),
        1,
    );
    apply_effects(
        &w.server,
        &skill,
        &CastCtx {
            caster: WorldObject::from(&w.pa),
            caster_caster: SkillCaster::Item {
                obj_id: w.pa.obj_id,
                item_id: source.item_id,
                item_template_id: source.item_template_id,
                type1: 0,
                type2: 0,
            },
            target: Target::Obj(WorldObject::from(target_player)),
            target_caster: SkillCastTarget::Item {
                obj_id: target_player.obj_id,
                id: target_id,
                type1: 0,
                type2: 0,
            },
            skill_object: SkillObject::None,
        },
    );
    skill
}

#[tokio::test]
async fn ucc_owned_dispatch_consumes_one_source_after_imprint_or_cleanup() {
    for cleanup in [false, true] {
        let mut w = ucc_world();
        let source_id = give(
            &w,
            &w.pa,
            slot_type::INVENTORY,
            ucc_source_template(cleanup),
            2,
            0,
        );
        let target_id = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
        {
            let bag = Inv::of(&w.pa).bag();
            let mut bag = bag.lock().unwrap();
            bag.get_item_by_id_mut(source_id).unwrap().set_ucc(0xABCD);
            bag.get_item_by_id_mut(target_id).unwrap().set_ucc(0x1234);
        }
        let skill = cast_ucc(
            &w,
            cleanup,
            SkillItem {
                item_id: source_id,
                item_template_id: ucc_source_template(cleanup),
            },
            target_id,
            &w.pa,
        );
        assert!(!skill.cancelled());
        assert_eq!(item(&w, source_id).unwrap().count, 1);
        assert_eq!(
            item(&w, target_id).unwrap().ucc_id,
            if cleanup { 0 } else { 0xABCD }
        );
        let ops = w.a.drain_ops();
        assert!(ops.contains(&sc::SCItemUccDataChangedPacket));
        assert!(ops.contains(&sc::SCItemTaskSuccessPacket));
        assert!(w.b.drain().is_empty());
    }
}

#[tokio::test]
async fn source_item_is_consumed_once_when_only_a_non_last_effect_consumes_it() {
    // First effect consumes the source item, the last one does not (mixed flags, e.g. skill
    // 22465): the item must still be debited, exactly once.
    let w = ucc_world_with(false);
    let source_id = give(
        &w,
        &w.pa,
        slot_type::INVENTORY,
        ucc_source_template(false),
        3,
        0,
    );
    let target_id = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
    {
        let bag = Inv::of(&w.pa).bag();
        let mut bag = bag.lock().unwrap();
        bag.get_item_by_id_mut(source_id).unwrap().set_ucc(0xABCD);
    }
    let skill = cast_ucc_flags(
        &w,
        false,
        SkillItem {
            item_id: source_id,
            item_template_id: ucc_source_template(false),
        },
        target_id,
        &w.pa,
        &[true, false],
    );
    assert!(!skill.cancelled());
    assert_eq!(item(&w, source_id).unwrap().count, 2);
}

#[tokio::test]
async fn ucc_foreign_source_target_or_resolved_player_cancels_without_debit() {
    for cleanup in [false, true] {
        for case in [
            "foreign_source",
            "foreign_target",
            "foreign_player",
            "both_foreign",
        ] {
            let mut w = ucc_world();
            let source_owner = if matches!(case, "foreign_source" | "both_foreign") {
                &w.pb
            } else {
                &w.pa
            };
            let target_owner = if matches!(case, "foreign_target" | "both_foreign") {
                &w.pb
            } else {
                &w.pa
            };
            let source_id = give(
                &w,
                source_owner,
                slot_type::INVENTORY,
                ucc_source_template(cleanup),
                2,
                0,
            );
            let target_id = give(&w, target_owner, slot_type::INVENTORY, ARMOR, 1, 1);
            Inv::of(source_owner)
                .bag()
                .lock()
                .unwrap()
                .get_item_by_id_mut(source_id)
                .unwrap()
                .set_ucc(0xABCD);
            Inv::of(target_owner)
                .bag()
                .lock()
                .unwrap()
                .get_item_by_id_mut(target_id)
                .unwrap()
                .set_ucc(0x1234);
            let source_before = ucc_item_values(&w, source_id);
            let target_before = ucc_item_values(&w, target_id);
            let source_item = w.server.items.item_by_id(source_id).expect("source");
            let resolved_player = if matches!(case, "foreign_player" | "both_foreign") {
                &w.pb
            } else {
                &w.pa
            };
            let skill = cast_ucc(
                &w,
                cleanup,
                SkillItem {
                    item_id: source_id,
                    item_template_id: ucc_source_template(cleanup),
                },
                target_id,
                resolved_player,
            );
            assert!(skill.cancelled(), "{case}, cleanup={cleanup}");
            assert_eq!(ucc_item_values(&w, source_id), source_before, "{case}");
            assert_eq!(ucc_item_values(&w, target_id), target_before, "{case}");
            // Only a source in the caster's own bag is debited and refunded; a foreign source
            // is refused before the debit.
            let own_source = !matches!(case, "foreign_source" | "both_foreign");
            let expected = if own_source {
                partial_debit_refund_tasks(&source_item)
            } else {
                Vec::new()
            };
            assert_eq!(item_task_bodies(&w.a.drain()), expected, "{case}");
            assert!(w.b.drain().is_empty(), "{case}");
        }
    }
}

#[tokio::test]
async fn ucc_invalid_source_count_template_or_same_item_cancels_without_mutation() {
    for cleanup in [false, true] {
        for case in [
            "empty_source",
            "forged_template",
            "same_item",
            "wrong_owner_metadata",
        ] {
            let mut w = ucc_world();
            let source_id = give(
                &w,
                &w.pa,
                slot_type::INVENTORY,
                ucc_source_template(cleanup),
                2,
                0,
            );
            let target_id = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
            {
                let bag = Inv::of(&w.pa).bag();
                let mut bag = bag.lock().unwrap();
                let source = bag.get_item_by_id_mut(source_id).unwrap();
                source.set_ucc(0xABCD);
                if case == "empty_source" {
                    source.count = 0;
                }
                if case == "wrong_owner_metadata" {
                    source.owner_id = u64::from(w.pb.character_id);
                }
                bag.get_item_by_id_mut(target_id).unwrap().set_ucc(0x1234);
            }
            let source_before = ucc_item_values(&w, source_id);
            let target_before = ucc_item_values(&w, target_id);
            let source_item = w.server.items.item_by_id(source_id).expect("source");
            let skill = cast_ucc(
                &w,
                cleanup,
                SkillItem {
                    item_id: source_id,
                    item_template_id: if case == "forged_template" {
                        POWDER
                    } else {
                        ucc_source_template(cleanup)
                    },
                },
                if case == "same_item" {
                    source_id
                } else {
                    target_id
                },
                &w.pa,
            );
            assert!(skill.cancelled(), "{case}, cleanup={cleanup}");
            assert_eq!(ucc_item_values(&w, source_id), source_before, "{case}");
            assert_eq!(ucc_item_values(&w, target_id), target_before, "{case}");
            // Only `same_item` takes a valid partial debit before the effect refuses it.
            let expected = if case == "same_item" {
                partial_debit_refund_tasks(&source_item)
            } else {
                Vec::new()
            };
            assert_eq!(item_task_bodies(&w.a.drain()), expected, "{case}");
            assert!(w.b.drain().is_empty(), "{case}");
        }
    }
}

#[tokio::test]
async fn ucc_imprint_cleanup_and_stamp() {
    let mut w = world();
    let stamp = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 1, 0);
    let target = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 1);
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(stamp)
        .unwrap()
        .set_ucc(0xABCD);
    let caster = Some(SkillItem {
        item_id: stamp,
        item_template_id: STACK,
    });
    imprint_ucc(&w.server, Some(&w.pa), caster, None, Some(target));
    let after = item(&w, target).unwrap();
    assert_eq!(after.ucc_id, 0xABCD);
    assert_ne!(after.flags & crate::models::item::ITEM_FLAG_HAS_UCC, 0);
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemUccDataChangedPacket,
                body(&SCItemUccDataChangedPacket {
                    ucc_id: 0xABCD,
                    player_id: 1,
                    target_item_id: target,
                })
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(
                    task_type::GAIN_ITEM_WITH_UCC,
                    vec![ItemTask::update_bits(&after)]
                )
            ),
        ]
    );
    // The stamp is not consumed by the effect.
    assert!(item(&w, stamp).is_some());

    cleanup_ucc(&w.server, Some(&w.pa), caster, None, Some(target));
    let after = item(&w, target).unwrap();
    assert_eq!(
        (
            after.ucc_id,
            after.flags & crate::models::item::ITEM_FLAG_HAS_UCC
        ),
        (0, 0)
    );
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCItemUccDataChangedPacket,
                body(&SCItemUccDataChangedPacket {
                    ucc_id: 0,
                    player_id: 1,
                    target_item_id: target,
                })
            ),
            (
                sc::SCItemTaskSuccessPacket,
                tasks(
                    task_type::GAIN_ITEM_WITH_UCC,
                    vec![ItemTask::update_bits(&after)]
                )
            ),
        ]
    );

    // Emblem imprint: a new crest stamp carrying the ink's UCC in the target's bag
    gain_item_with_emblem_imprint(&w.server, Some(&w.pb), caster, None, [0; 4]);
    let crest = item_at(&w.pb, slot_type::INVENTORY, 0).unwrap();
    assert_eq!(
        (crest.template_id, crest.ucc_id),
        (ITEM_CREST_STAMP, 0xABCD)
    );
    assert_eq!(
        w.b.drain(),
        vec![(
            sc::SCItemTaskSuccessPacket,
            tasks(task_type::GAIN_ITEM_WITH_UCC, vec![ItemTask::add(&crest)])
        )]
    );
}

#[tokio::test]
async fn ucc_imprint_accepts_an_applicable_item_with_known_patterns() {
    let w = ucc_world_with_tables(true, &[ARMOR], &[3, 7]);
    let stamp = ucc_stamp(&w, 0, 0xABCD, 3, 7);
    let target = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
    assert_eq!(
        imprint_ucc(
            &w.server,
            Some(&w.pa),
            Some(SkillItem {
                item_id: stamp,
                item_template_id: STACK,
            }),
            None,
            Some(target),
        ),
        Outcome::Done
    );
    assert_eq!(item(&w, target).unwrap().ucc_id, 0xABCD);
}

#[tokio::test]
async fn ucc_imprint_refuses_a_non_applicable_item_and_keeps_it_unchanged() {
    let mut w = ucc_world_with_tables(true, &[STACK], &[]);
    let stamp = ucc_stamp(&w, 0, 0xABCD, 0, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
    let target_before = ucc_item_values(&w, target);
    let stamp_before = ucc_item_values(&w, stamp);
    assert_eq!(
        imprint_ucc(
            &w.server,
            Some(&w.pa),
            Some(SkillItem {
                item_id: stamp,
                item_template_id: STACK,
            }),
            None,
            Some(target),
        ),
        Outcome::Cancelled
    );
    assert_eq!(ucc_item_values(&w, target), target_before);
    assert_eq!(ucc_item_values(&w, stamp), stamp_before);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn ucc_imprint_refuses_an_unknown_emblem_pattern_and_keeps_item_and_ucc() {
    let mut w = ucc_world_with_tables(true, &[ARMOR], &[3]);
    let stamp = ucc_stamp(&w, 0, 0xABCD, 3, 99);
    let target = give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
    let target_before = ucc_item_values(&w, target);
    let stamp_before = ucc_item_values(&w, stamp);
    assert_eq!(
        imprint_ucc(
            &w.server,
            Some(&w.pa),
            Some(SkillItem {
                item_id: stamp,
                item_template_id: STACK,
            }),
            None,
            Some(target),
        ),
        Outcome::Cancelled
    );
    assert_eq!(ucc_item_values(&w, target), target_before);
    assert_eq!(ucc_item_values(&w, stamp), stamp_before);
    assert!(w.a.drain().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Slaves / backpacks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn repair_slave_item_flow() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, SLAVE_ITEM, 1, 0);
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id_mut(id)
        .unwrap()
        .details
        .slave_is_destroyed = 1;
    // Wrong repair effect
    repair_slave_item(&w.server, 41, Some((id, w.pa.obj_id)));
    assert_eq!(w.a.drain(), vec![error(error_message::ITEM_FAILED_REPAIR)]);
    repair_slave_item(&w.server, 42, Some((id, w.pa.obj_id)));
    let after = item(&w, id).unwrap();
    assert_eq!(after.details.slave_is_destroyed, 0);
    assert!(after.dirty);
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCItemTaskSuccessPacket,
            tasks(task_type::REPAIR_SLAVES, vec![ItemTask::update(&after)])
        )]
    );
}

#[derive(Default)]
struct RecordingHooks(Mutex<Vec<BackpackDoodadSpawn>>);

impl ItemEffectHooks for RecordingHooks {
    fn spawn_backpack_doodad(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        spawn: &BackpackDoodadSpawn,
    ) -> bool {
        self.0.lock().unwrap().push(*spawn);
        true
    }
}

#[tokio::test]
async fn put_down_backpack_drops_and_reequips_the_glider() {
    let mut w = world();
    w.pa.data().unit.hp = 100;
    let pack = give(
        &w,
        &w.pa.clone(),
        slot_type::EQUIPMENT,
        BACKPACK,
        1,
        equip_slot::BACKPACK,
    );
    let glider = give(&w, &w.pa.clone(), slot_type::INVENTORY, BACKPACK, 1, 4);
    w.server
        .systems
        .economy
        .set_previous_backpack(1, glider, BACKPACK);
    let hooks = Arc::new(RecordingHooks::default());
    w.server
        .systems
        .economy
        .item_effects
        .hooks
        .install(hooks.clone());
    let caster = Some(SkillItem {
        item_id: pack,
        item_template_id: BACKPACK,
    });
    put_down_backpack(&w.server, &w.pa, 4321, caster);
    let spawned = hooks.0.lock().unwrap().clone();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].doodad_template_id, 4321);
    assert_eq!(spawned[0].item_id, pack);
    // yaw 0: one metre north
    assert_eq!((spawned[0].x, spawned[0].y), (100.0, 101.0));
    // The pack is in the system container, the glider is back on the back.
    let sys = Inv::of(&w.pa).get(slot_type::SYSTEM).unwrap();
    assert!(sys.lock().unwrap().get_item_by_id(pack).is_some());
    assert_eq!(
        Inv::of(&w.pa).equipped(equip_slot::BACKPACK).unwrap().id,
        glider
    );
    let ops = w.a.drain_ops();
    assert_eq!(ops[0], sc::SCItemTaskSuccessPacket); // DropBackpack
    assert_eq!(*ops.last().unwrap(), sc::SCItemTaskSuccessPacket); // SwapItems
                                                                   // Bob: the equipment leave event, then the glider equip.
    assert_eq!(
        w.b.drain(),
        vec![
            (
                sc::SCUnitEquipmentsChangedPacket,
                body(&SCUnitEquipmentsChangedPacket {
                    obj_id: w.pa.obj_id,
                    items: vec![(equip_slot::BACKPACK as u8, None)],
                })
            ),
            (
                sc::SCUnitEquipmentsChangedPacket,
                body(&SCUnitEquipmentsChangedPacket {
                    obj_id: w.pa.obj_id,
                    items: vec![(
                        equip_slot::BACKPACK as u8,
                        Inv::of(&w.pa).equipped(equip_slot::BACKPACK)
                    )],
                })
            ),
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Glider re-equip after a trade pack left the back slot (any route)
// ---------------------------------------------------------------------------------------------

use crate::systems::economy::inventory::{lock_pair, reequip_previous_glider, split_or_move_item};

/// A trade pack on the back, its glider (taken off) in the bag and recorded.
fn pack_on_back(w: &World) -> (u64, u64) {
    w.pa.data().unit.hp = 100;
    let pack = give(
        w,
        &w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    let glider = give(w, &w.pa, slot_type::INVENTORY, BACKPACK, 1, 4);
    w.server
        .systems
        .economy
        .set_previous_backpack(1, glider, BACKPACK);
    (pack, glider)
}

fn back(w: &World) -> Option<u64> {
    Inv::of(&w.pa).equipped(equip_slot::BACKPACK).map(|i| i.id)
}

/// Turn-in / sale / destroy: the pack is removed from the equipment container.
fn remove_pack(w: &World, pack: u64) {
    let mut txn = ItemTxn::begin(&w.server);
    let removed = Inv::of(&w.pa).equipment().lock().unwrap().remove_item_task(
        task_type::DESTROY,
        pack,
        true,
        &mut txn.env,
    );
    assert!(removed.is_some());
}

fn recorded(w: &World) -> Option<(u64, u32)> {
    w.server.systems.economy.previous_backpack(1)
}

#[tokio::test]
async fn glider_returns_when_the_pack_is_removed() {
    let mut w = world();
    let (pack, glider) = pack_on_back(&w);
    w.b.drain();
    remove_pack(&w, pack);
    assert_eq!(back(&w), Some(glider));
    assert_eq!(recorded(&w), None);
    // Bob saw the pack leave and the glider arrive.
    let seen = w.b.drain();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1],
        (
            sc::SCUnitEquipmentsChangedPacket,
            body(&SCUnitEquipmentsChangedPacket {
                obj_id: w.pa.obj_id,
                items: vec![(
                    equip_slot::BACKPACK as u8,
                    Inv::of(&w.pa).equipped(equip_slot::BACKPACK)
                )],
            })
        )
    );
}

#[tokio::test]
async fn glider_returns_when_the_pack_moves_to_another_container() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    let inv = Inv::of(&w.pa);
    {
        let mut txn = ItemTxn::begin(&w.server);
        let system = inv.get(slot_type::SYSTEM).unwrap();
        let equipment = inv.equipment();
        let data = txn.data();
        let (mut sys, eq) = lock_pair(&system, &equipment);
        assert!(sys.move_item_from(
            &mut eq.unwrap(),
            task_type::DROP_BACKPACK,
            pack,
            -1,
            data,
            &mut txn.env.fx
        ));
    }
    assert_eq!(back(&w), Some(glider));
}

#[tokio::test]
async fn glider_waits_for_the_resurrection() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    w.pa.data().unit.hp = 0;
    remove_pack(&w, pack);
    assert_eq!(back(&w), None);
    assert!(recorded(&w).is_some());
    w.pa.data().unit.hp = 10;
    assert!(reequip_previous_glider(&w.server, &w.pa));
    assert_eq!(back(&w), Some(glider));
    assert_eq!(recorded(&w), None);
}

#[tokio::test]
async fn glider_stays_in_the_bank() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    {
        let inv = Inv::of(&w.pa);
        let mut txn = ItemTxn::begin(&w.server);
        let slot = inv.warehouse().lock().unwrap().get_unused_slot(-1);
        assert!(split_or_move_item(
            &mut txn,
            &inv,
            task_type::SWAP_ITEMS,
            glider,
            slot_type::INVENTORY,
            4,
            0,
            slot_type::BANK,
            slot as u8,
            0
        ));
    }
    remove_pack(&w, pack);
    assert_eq!(back(&w), None);
    assert_eq!(recorded(&w), None);
}

#[tokio::test]
async fn glider_stays_when_destroyed() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    {
        let mut txn = ItemTxn::begin(&w.server);
        Inv::of(&w.pa).bag().lock().unwrap().remove_item_task(
            task_type::DESTROY,
            glider,
            true,
            &mut txn.env,
        );
    }
    remove_pack(&w, pack);
    assert_eq!(back(&w), None);
    assert_eq!(recorded(&w), None);
}

#[tokio::test]
async fn glider_stays_when_the_id_now_has_another_template() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    w.server
        .systems
        .economy
        .set_previous_backpack(1, glider, GLIDER_HI);
    remove_pack(&w, pack);
    assert_eq!(back(&w), None);
    assert_eq!(recorded(&w), None);
}

#[tokio::test]
async fn glider_stays_when_it_belongs_to_another_character() {
    let w = world();
    let (pack, _) = pack_on_back(&w);
    let theirs = give(&w, &w.pb, slot_type::INVENTORY, BACKPACK, 1, 4);
    w.server
        .systems
        .economy
        .set_previous_backpack(1, theirs, BACKPACK);
    remove_pack(&w, pack);
    assert_eq!(back(&w), None);
    assert_eq!(recorded(&w), None);
    assert!(Inv::of(&w.pb).equipped(equip_slot::BACKPACK).is_none());
}

#[tokio::test]
async fn glider_stays_when_the_player_equipped_something_else_in_between() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    // The pack goes away while the character is dead (record kept) ...
    w.pa.data().unit.hp = 0;
    remove_pack(&w, pack);
    assert!(recorded(&w).is_some());
    // ... and the player equips another glider before the resurrection.
    let other = give(&w, &w.pa, slot_type::INVENTORY, BACKPACK, 1, 5);
    {
        let inv = Inv::of(&w.pa);
        let mut txn = ItemTxn::begin(&w.server);
        assert!(split_or_move_item(
            &mut txn,
            &inv,
            task_type::SWAP_ITEMS,
            other,
            slot_type::INVENTORY,
            5,
            0,
            slot_type::EQUIPMENT,
            equip_slot::BACKPACK as u8,
            0
        ));
    }
    assert_eq!(recorded(&w), None);
    w.pa.data().unit.hp = 10;
    reequip_previous_glider(&w.server, &w.pa);
    assert_eq!(back(&w), Some(other));
    assert!(Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id(glider)
        .is_some());
}

#[tokio::test]
async fn glider_stays_when_the_back_slot_is_taken_or_the_container_refuses() {
    // The back slot is occupied: nothing is swapped out, the record is kept.
    let w = world();
    let (_, glider) = pack_on_back(&w);
    assert!(!reequip_previous_glider(&w.server, &w.pa));
    assert_ne!(back(&w), Some(glider));
    assert!(recorded(&w).is_some());

    // The equipment container refuses the glider (not a bag item of this character's gear).
    let w = world();
    w.pa.data().unit.hp = 100;
    let scroll = give(&w, &w.pa, slot_type::INVENTORY, SCROLL, 1, 4);
    w.server
        .systems
        .economy
        .set_previous_backpack(1, scroll, SCROLL);
    assert!(!reequip_previous_glider(&w.server, &w.pa));
    assert_eq!(back(&w), None);
    assert_eq!(recorded(&w), None);
}

#[tokio::test]
async fn equipping_a_trade_pack_keeps_the_record_and_the_pack_in_place() {
    let w = world();
    w.pa.data().unit.hp = 100;
    let glider = give(
        &w,
        &w.pa,
        slot_type::EQUIPMENT,
        BACKPACK,
        1,
        equip_slot::BACKPACK,
    );
    {
        let inv = Inv::of(&w.pa);
        let mut txn = ItemTxn::begin(&w.server);
        assert!(crate::systems::economy::inventory::try_equip_new_backpack(
            &mut txn,
            &inv,
            task_type::SWAP_ITEMS,
            PACK,
            1,
            0,
            0
        ));
    }
    let on_back = Inv::of(&w.pa).equipped(equip_slot::BACKPACK).unwrap();
    assert_eq!(on_back.template_id, PACK);
    assert_eq!(recorded(&w), Some((glider, BACKPACK)));
    // The pack is then removed: the glider comes back.
    remove_pack(&w, on_back.id);
    assert_eq!(back(&w), Some(glider));
}

#[tokio::test]
async fn resurrection_keeps_the_record_while_a_non_dropped_pack_is_worn() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    w.pa.data().unit.hp = 0;
    w.pa.data().unit.hp = 10; // resurrected, the pack was not dropped on death
    assert!(!reequip_previous_glider(&w.server, &w.pa));
    assert_eq!(back(&w), Some(pack));
    assert!(recorded(&w).is_some());
    remove_pack(&w, pack);
    assert_eq!(back(&w), Some(glider));
}

#[tokio::test]
async fn glider_reequip_in_an_unlocked_transaction_waits_for_the_drain() {
    let w = world();
    let (pack, glider) = pack_on_back(&w);
    {
        let mut txn = ItemTxn::unlocked(&w.server);
        let removed = Inv::of(&w.pa).equipment().lock().unwrap().remove_item_task(
            task_type::DESTROY,
            pack,
            true,
            &mut txn.env,
        );
        assert!(removed.is_some());
    }
    assert_eq!(back(&w), None);
    assert!(recorded(&w).is_some());
    crate::systems::economy::flush_deferred_glider_reequips(&w.server);
    assert_eq!(back(&w), Some(glider));
    assert_eq!(recorded(&w), None);
}

// ---------------------------------------------------------------------------------------------
// Mate gear
// ---------------------------------------------------------------------------------------------

fn spawn_mate(w: &mut World) -> crate::world::MateRef {
    spawn_mate_with(w, 500)
}

fn spawn_mate_with(w: &mut World, template_id: u32) -> crate::world::MateRef {
    install_default_summon_hooks(&w.server);
    let mut m = Mate::new(77, template_id);
    m.unit.tl_id = TL;
    m.owner_obj_id = w.pa.obj_id;
    m.unit.transform.set_xyz(100.5, 100.0, 100.0);
    let mate = Entity::new(0x300, m);
    let world = w.server.world.world(0).unwrap();
    world.spawn(&mate, &w.server.view_ctx());
    w.a.drain();
    w.b.drain();
    mate
}

fn mate_body(player_item: Option<&Item>, mate_item: Option<&Item>, ps: u8, ms: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(1)
        .write_u16(TL)
        .write_u32(0)
        .write_bool(false)
        .write_u8(1);
    match player_item {
        Some(i) => i.write(&mut s),
        None => {
            s.write_u32(0);
        }
    }
    match mate_item {
        Some(i) => i.write(&mut s),
        None => {
            s.write_u32(0);
        }
    }
    s.write_u8(slot_type::INVENTORY)
        .write_u8(ps)
        .write_u8(slot_type::EQUIPMENT_MATE)
        .write_u8(ms);
    s.into_bytes()
}

fn run_mate(w: &mut World, body: &[u8]) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let p = CSChangeMateEquipmentPacket::read(&mut s).expect("read");
    futures::executor::block_on(p.handle(&w.server, &mut w.a.conn)).expect("handle");
}

#[tokio::test]
async fn mate_equipment_rejects_another_characters_mate() {
    let mut w = world();
    let mate = spawn_mate(&mut w);
    mate.data().owner_obj_id = w.pb.obj_id;
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 3);
    let wire = item(&w, id).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&wire), None, 3, equip_slot::CHEST as u8),
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 3).unwrap().id, id);
    assert!(mate.data().equipment.iter().all(Option::is_none));
    assert!(w.a.drain().is_empty());
    assert!(w.b.drain().is_empty());
}

#[tokio::test]
async fn mate_equipment_equip_unequip_and_refusal() {
    let mut w = world();
    let mate = spawn_mate(&mut w);
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 3);
    let wire = item(&w, id).unwrap();

    // Equip bag slot 3 -> chest of the mate
    run_mate(
        &mut w,
        &mate_body(Some(&wire), None, 3, equip_slot::CHEST as u8),
    );
    let equipped = w
        .server
        .items
        .item_by_id(id)
        .expect("still known to the item manager");
    assert_eq!(
        (equipped.slot_type, equipped.slot),
        (slot_type::EQUIPMENT_MATE, equip_slot::CHEST)
    );
    assert!(item_at(&w.pa, slot_type::INVENTORY, 3).is_none());
    assert_eq!(
        mate.data().equipment[equip_slot::CHEST as usize]
            .as_ref()
            .map(|i| i.id),
        Some(id)
    );
    let mate_gear = SCUnitEquipmentsChangedPacket {
        obj_id: mate.obj_id,
        items: vec![(equip_slot::CHEST as u8, Some(equipped.clone()))],
    };
    assert_eq!(
        w.a.drain(),
        vec![
            (sc::SCUnitEquipmentsChangedPacket, body(&mate_gear)),
            (
                sc::SCMateEquipmentChangedPacket,
                body(&SCMateEquipmentChangedPacket {
                    item_on_pet: ItemAndLocation {
                        item: Some(equipped.clone()),
                        slot_type: slot_type::INVENTORY,
                        slot: 3,
                    },
                    item_in_bag: ItemAndLocation {
                        item: None,
                        slot_type: slot_type::EQUIPMENT_MATE,
                        slot: equip_slot::CHEST as u8,
                    },
                    mate_tl_id: TL,
                    character_id: 1,
                    passenger_id: 0,
                    bts: false,
                    success: true,
                })
            ),
        ]
    );
    assert_eq!(
        w.b.drain(),
        vec![(sc::SCUnitEquipmentsChangedPacket, body(&mate_gear))]
    );

    // Un-equip the chest into bag slot 7
    run_mate(
        &mut w,
        &mate_body(None, Some(&equipped), 7, equip_slot::CHEST as u8),
    );
    let back = item_at(&w.pa, slot_type::INVENTORY, 7).unwrap();
    assert_eq!(back.id, id);
    assert!(mate.data().equipment[equip_slot::CHEST as usize].is_none());
    let mate_gear = SCUnitEquipmentsChangedPacket {
        obj_id: mate.obj_id,
        items: vec![(equip_slot::CHEST as u8, None)],
    };
    assert_eq!(
        w.a.drain(),
        vec![
            (sc::SCUnitEquipmentsChangedPacket, body(&mate_gear)),
            (
                sc::SCMateEquipmentChangedPacket,
                body(&SCMateEquipmentChangedPacket {
                    item_on_pet: ItemAndLocation {
                        item: None,
                        slot_type: slot_type::INVENTORY,
                        slot: 7,
                    },
                    item_in_bag: ItemAndLocation {
                        item: Some(back.clone()),
                        slot_type: slot_type::EQUIPMENT_MATE,
                        slot: equip_slot::CHEST as u8,
                    },
                    mate_tl_id: TL,
                    character_id: 1,
                    passenger_id: 0,
                    bts: false,
                    success: true,
                })
            ),
        ]
    );

    assert_eq!(
        w.b.drain(),
        vec![(sc::SCUnitEquipmentsChangedPacket, body(&mate_gear))]
    );

    // Refused (armor into the head slot): the failure packet, bag side first
    run_mate(
        &mut w,
        &mate_body(Some(&back), None, 7, equip_slot::HEAD as u8),
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 7).unwrap().id, id);
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCMateEquipmentChangedPacket,
            body(&SCMateEquipmentChangedPacket {
                item_on_pet: ItemAndLocation {
                    item: Some(back.clone()),
                    slot_type: slot_type::INVENTORY,
                    slot: 7,
                },
                item_in_bag: ItemAndLocation {
                    item: None,
                    slot_type: slot_type::EQUIPMENT_MATE,
                    slot: equip_slot::HEAD as u8,
                },
                mate_tl_id: TL,
                character_id: 1,
                passenger_id: 0,
                bts: false,
                success: false, // rejected move
            })
        )]
    );
    assert!(w.b.drain().is_empty());

    // Unknown mate: nothing
    let mut body = mate_body(Some(&back), None, 7, equip_slot::CHEST as u8);
    body[4] = 0x99;
    run_mate(&mut w, &body);
    assert!(w.a.drain().is_empty());
}

/// The shipped data has no timed mate item, so this gives an equipped one an end time in the
/// past: `update_item_timers` removes it from the mate container, answers the owner with
/// `SCMateEquipmentExpired` and keeps an unexpired item in the same container.
#[tokio::test]
async fn mate_equipment_expires_and_notifies_the_owner() {
    let mut w = world_with_data(mate_pack_data());
    let mate = spawn_mate(&mut w);
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 3);
    let wire = item(&w, id).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&wire), None, 3, equip_slot::CHEST as u8),
    );
    w.a.drain();
    w.b.drain();
    let container = w
        .server
        .items
        .find_container_for_character(w.pa.character_id, slot_type::EQUIPMENT_MATE, 77)
        .expect("mate equipment container");
    let kept = w
        .server
        .items
        .create(
            WAIST_ARMOR,
            1,
            0,
            1,
            &w.server.game_data.items,
            &w.server.ids.item,
            0,
        )
        .expect("item");
    let kept_id = kept.id;
    {
        let mut c = container.lock().unwrap();
        c.get_item_by_id_mut(id).unwrap().expiration_time = 1000;
        assert!(c
            .add_new_item(kept, equip_slot::WAIST, &w.server.game_data.items)
            .is_ok());
        c.get_item_by_id_mut(kept_id).unwrap().expiration_time = 3_000_000;
    }

    w.server.systems.economy.item_timers.reset(1_000_000);
    assert_eq!(item_timers::update_item_timers(&w.server, 2_000_000), 1);

    {
        let c = container.lock().unwrap();
        assert!(c.get_item_by_id(id).is_none());
        assert_eq!(c.get_item_by_id(kept_id).map(|i| i.id), Some(kept_id));
    }
    assert!(w.server.items.item_by_id(id).is_none());
    assert!(mate.data().equipment[equip_slot::CHEST as usize].is_none());
    assert_eq!(
        w.a.drain(),
        vec![
            (
                sc::SCMateEquipmentExpiredPacket,
                body(&SCMateEquipmentExpiredPacket {
                    mate_tl_id: TL,
                    slot_type: slot_type::EQUIPMENT_MATE,
                    slot_index: equip_slot::CHEST as u8,
                })
            ),
            (
                sc::SCUnitEquipmentsChangedPacket,
                body(&SCUnitEquipmentsChangedPacket {
                    obj_id: mate.obj_id,
                    items: vec![(equip_slot::CHEST as u8, None)],
                })
            ),
        ]
    );
    assert_eq!(
        w.b.drain(),
        vec![(
            sc::SCUnitEquipmentsChangedPacket,
            body(&SCUnitEquipmentsChangedPacket {
                obj_id: mate.obj_id,
                items: vec![(equip_slot::CHEST as u8, None)],
            })
        )]
    );
}

/// A mate that is not spawned keeps its persistent container, so its timed gear still expires;
/// the packet is skipped because the client only acts on its current pet.
#[tokio::test]
async fn mate_equipment_expiry_skips_the_packet_for_an_unspawned_mate() {
    let mut w = world();
    let container = w.server.items.container_for_character(
        w.pa.character_id,
        slot_type::EQUIPMENT_MATE,
        999,
        &w.server.ids.container,
    );
    let item = w
        .server
        .items
        .create(
            ARMOR,
            1,
            0,
            1,
            &w.server.game_data.items,
            &w.server.ids.item,
            0,
        )
        .expect("item");
    let id = item.id;
    {
        let mut c = container.lock().unwrap();
        assert!(c
            .add_new_item(item, equip_slot::CHEST, &w.server.game_data.items)
            .is_ok());
        c.get_item_by_id_mut(id).unwrap().expiration_time = 1000;
    }
    w.server.systems.economy.item_timers.reset(1_000_000);
    assert_eq!(item_timers::update_item_timers(&w.server, 2_000_000), 1);
    assert!(container.lock().unwrap().items.is_empty());
    assert!(w.server.items.item_by_id(id).is_none());
    assert!(w.a.drain().is_empty());
    assert!(w.b.drain().is_empty());
}

/// The bag rule for offline owners applies to mate gear too.
#[tokio::test]
async fn mate_equipment_timers_skip_an_offline_owner() {
    let mut w = world();
    let container = w.server.items.container_for_character(
        w.pa.character_id,
        slot_type::EQUIPMENT_MATE,
        999,
        &w.server.ids.container,
    );
    let item = w
        .server
        .items
        .create(
            ARMOR,
            1,
            0,
            1,
            &w.server.game_data.items,
            &w.server.ids.item,
            0,
        )
        .expect("item");
    let id = item.id;
    {
        let mut c = container.lock().unwrap();
        assert!(c
            .add_new_item(item, equip_slot::CHEST, &w.server.game_data.items)
            .is_ok());
        c.get_item_by_id_mut(id).unwrap().expiration_time = 1000;
    }
    assert!(w.server.world.try_remove_character(w.pa.obj_id));
    w.server.systems.economy.item_timers.reset(1_000_000);
    assert_eq!(item_timers::update_item_timers(&w.server, 2_000_000), 0);
    assert_eq!(container.lock().unwrap().items.len(), 1);
    assert!(w.a.drain().is_empty());
}

/// The riding pack (`mate_equip_slot_packs` 1) allows head, waist and feet.
#[tokio::test]
async fn mate_equipment_riding_pack_allows_head_waist_and_feet() {
    let mut w = world_with_data(mate_pack_data());
    let mate = spawn_mate_with(&mut w, MATE_RIDING);
    for (template, slot) in [
        (HEAD_ARMOR, equip_slot::HEAD),
        (WAIST_ARMOR, equip_slot::WAIST),
        (FEET_ARMOR, equip_slot::FEET),
    ] {
        let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, template, 1, 3);
        let wire = item(&w, id).unwrap();
        run_mate(&mut w, &mate_body(Some(&wire), None, 3, slot as u8));
        assert!(item_at(&w.pa, slot_type::INVENTORY, 3).is_none());
        assert_eq!(
            mate.data().equipment[slot as usize].as_ref().map(|i| i.id),
            Some(id)
        );
        assert!(w
            .a
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCMateEquipmentChangedPacket));
        assert!(w
            .b
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket));
    }
}

/// The combat pack (`mate_equip_slot_packs` 2) has a chest slot but no waist slot.
#[tokio::test]
async fn mate_equipment_combat_pack_refuses_waist_and_keeps_the_item() {
    let mut w = world_with_data(mate_pack_data());
    let mate = spawn_mate_with(&mut w, MATE_COMBAT);
    let chest = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 3);
    let chest_wire = item(&w, chest).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&chest_wire), None, 3, equip_slot::CHEST as u8),
    );
    assert_eq!(
        mate.data().equipment[equip_slot::CHEST as usize]
            .as_ref()
            .map(|i| i.id),
        Some(chest)
    );
    w.a.drain();
    w.b.drain();

    let waist = give(&w, &w.pa.clone(), slot_type::INVENTORY, WAIST_ARMOR, 1, 4);
    let waist_wire = item(&w, waist).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&waist_wire), None, 4, equip_slot::WAIST as u8),
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 4).unwrap().id, waist);
    assert!(mate.data().equipment[equip_slot::WAIST as usize].is_none());
    let frames = w.a.drain();
    assert!(frames
        .iter()
        .any(|(op, _)| *op == sc::SCMateEquipmentChangedPacket));
    assert!(frames.contains(&error(error_message::ITEM_NOT_FOR_PET)));
    assert!(w.b.drain().is_empty());
}

/// The riding pack has no chest slot: the item stays in the bag and the client gets the
/// rejected-move packet plus the error.
#[tokio::test]
async fn mate_equipment_riding_pack_refuses_chest_and_keeps_the_item() {
    let mut w = world_with_data(mate_pack_data());
    let mate = spawn_mate_with(&mut w, MATE_RIDING);
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 3);
    let wire = item(&w, id).unwrap();
    w.a.drain();
    w.b.drain();
    run_mate(
        &mut w,
        &mate_body(Some(&wire), None, 3, equip_slot::CHEST as u8),
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 3).unwrap().id, id);
    assert!(mate.data().equipment[equip_slot::CHEST as usize].is_none());
    let frames = w.a.drain();
    assert!(frames.contains(&(
        sc::SCMateEquipmentChangedPacket,
        body(&SCMateEquipmentChangedPacket {
            item_on_pet: ItemAndLocation {
                item: Some(wire.clone()),
                slot_type: slot_type::INVENTORY,
                slot: 3,
            },
            item_in_bag: ItemAndLocation {
                item: None,
                slot_type: slot_type::EQUIPMENT_MATE,
                slot: equip_slot::CHEST as u8,
            },
            mate_tl_id: TL,
            character_id: 1,
            passenger_id: 0,
            bts: false,
            success: false,
        })
    )));
    assert!(frames.contains(&error(error_message::ITEM_NOT_FOR_PET)));
    assert!(w.b.drain().is_empty());
}

/// The pet pack (`mate_equip_slot_packs` 3) allows no gear slot at all.
#[tokio::test]
async fn mate_equipment_pet_pack_refuses_every_slot_and_keeps_the_items() {
    for (template, slot) in [
        (HEAD_ARMOR, equip_slot::HEAD),
        (ARMOR, equip_slot::CHEST),
        (WAIST_ARMOR, equip_slot::WAIST),
        (FEET_ARMOR, equip_slot::FEET),
    ] {
        let mut w = world_with_data(mate_pack_data());
        let mate = spawn_mate_with(&mut w, MATE_PET);
        let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, template, 1, 3);
        let wire = item(&w, id).unwrap();
        w.a.drain();
        w.b.drain();
        run_mate(&mut w, &mate_body(Some(&wire), None, 3, slot as u8));
        assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 3).unwrap().id, id);
        assert!(mate.data().equipment[slot as usize].is_none());
        let frames = w.a.drain();
        assert!(frames
            .iter()
            .any(|(op, _)| *op == sc::SCMateEquipmentChangedPacket));
        assert!(frames.contains(&error(error_message::ITEM_NOT_FOR_PET)));
        assert!(w.b.drain().is_empty());
    }
}

/// Unequipping to the bag is always allowed, even with a pack that permits no slot.
#[tokio::test]
async fn mate_equipment_pet_pack_still_unequips_to_the_bag() {
    let mut w = world_with_data(mate_pack_data());
    let mate = spawn_mate_with(&mut w, MATE_PET);
    let container = w
        .server
        .systems
        .economy
        .item_effects
        .hooks()
        .mate_equipment(&w.server, &mate)
        .unwrap();
    let item = w
        .server
        .items
        .create(
            ARMOR,
            1,
            0,
            1,
            &w.server.game_data.items,
            &w.server.ids.item,
            0,
        )
        .unwrap();
    let id = item.id;
    assert!(container
        .lock()
        .unwrap()
        .add_new_item(item, equip_slot::CHEST, &w.server.game_data.items)
        .is_ok());
    let worn = container
        .lock()
        .unwrap()
        .get_item_by_slot(equip_slot::CHEST)
        .cloned()
        .unwrap();
    mate.data().equipment[equip_slot::CHEST as usize] = Some(worn.clone());
    w.a.drain();
    w.b.drain();

    run_mate(
        &mut w,
        &mate_body(None, Some(&worn), 7, equip_slot::CHEST as u8),
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 7).unwrap().id, id);
    assert!(mate.data().equipment[equip_slot::CHEST as usize].is_none());
    assert!(w
        .a
        .drain()
        .iter()
        .any(|(op, _)| *op == sc::SCMateEquipmentChangedPacket));
    assert!(w
        .b
        .drain()
        .iter()
        .any(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket));
}

/// The per-mate item list is the client's whitelist, gated on FeatureSet bit 78. With the bit
/// off (`DEFAULT_FSET`) an item the list does not name is accepted and only logged.
#[tokio::test]
async fn mate_equipment_item_outside_the_client_list_is_still_allowed_when_the_feature_is_off() {
    let mut w = world_with_data(mate_pack_data());
    assert!(!w.server.systems.services.features(&w.server).check_bit(78));
    let rules = &w.server.game_data.systems.economy.mate_equipment;
    assert!(rules.pack_items[&MATE_RIDING].contains(&WAIST_ARMOR));
    assert!(!rules.pack_items[&MATE_RIDING].contains(&FEET_ARMOR));
    let mate = spawn_mate_with(&mut w, MATE_RIDING);
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, FEET_ARMOR, 1, 3);
    let wire = item(&w, id).unwrap();
    w.a.drain();
    w.b.drain();
    run_mate(
        &mut w,
        &mate_body(Some(&wire), None, 3, equip_slot::FEET as u8),
    );
    assert!(item_at(&w.pa, slot_type::INVENTORY, 3).is_none());
    assert_eq!(
        mate.data().equipment[equip_slot::FEET as usize]
            .as_ref()
            .map(|i| i.id),
        Some(id)
    );
    let frames = w.a.drain();
    assert!(!frames.contains(&error(error_message::ITEM_NOT_FOR_PET)));
    assert!(w
        .b
        .drain()
        .iter()
        .any(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket));
}

/// With FeatureSet bit 78 on, the client's `mate_equip_pack_groups` list is enforced: the named
/// item is accepted in an allowed slot, an unnamed one is refused and stays in the bag.
#[tokio::test]
async fn mate_equipment_item_outside_the_client_list_is_refused_when_the_feature_is_on() {
    let mut w = world_with_data(mate_pack_data_with_whitelist(true));
    assert!(w.server.systems.services.features(&w.server).check_bit(78));
    let mate = spawn_mate_with(&mut w, MATE_RIDING);

    let waist = give(&w, &w.pa.clone(), slot_type::INVENTORY, WAIST_ARMOR, 1, 3);
    let waist_wire = item(&w, waist).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&waist_wire), None, 3, equip_slot::WAIST as u8),
    );
    assert_eq!(
        mate.data().equipment[equip_slot::WAIST as usize]
            .as_ref()
            .map(|i| i.id),
        Some(waist)
    );
    w.a.drain();
    w.b.drain();

    let feet = give(&w, &w.pa.clone(), slot_type::INVENTORY, FEET_ARMOR, 1, 3);
    let feet_wire = item(&w, feet).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&feet_wire), None, 3, equip_slot::FEET as u8),
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 3).unwrap().id, feet);
    assert!(mate.data().equipment[equip_slot::FEET as usize].is_none());
    let frames = w.a.drain();
    assert!(frames
        .iter()
        .any(|(op, _)| *op == sc::SCMateEquipmentChangedPacket));
    assert!(frames.contains(&error(error_message::ITEM_NOT_FOR_PET)));
    assert!(w.b.drain().is_empty());
}

/// A mate template whose slot pack is 0 / missing refuses every slot (`FUN_3933a9c0` returns 0).
#[tokio::test]
async fn mate_equipment_missing_slot_pack_refuses_every_slot() {
    for (template, slot) in [
        (HEAD_ARMOR, equip_slot::HEAD),
        (ARMOR, equip_slot::CHEST),
        (WAIST_ARMOR, equip_slot::WAIST),
        (FEET_ARMOR, equip_slot::FEET),
    ] {
        let mut w = world_with_data(mate_pack_data());
        let mate = spawn_mate_with(&mut w, MATE_NO_PACK);
        let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, template, 1, 3);
        let wire = item(&w, id).unwrap();
        w.a.drain();
        w.b.drain();
        run_mate(&mut w, &mate_body(Some(&wire), None, 3, slot as u8));
        assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 3).unwrap().id, id);
        assert!(mate.data().equipment[slot as usize].is_none());
        let frames = w.a.drain();
        assert!(frames
            .iter()
            .any(|(op, _)| *op == sc::SCMateEquipmentChangedPacket));
        assert!(frames.contains(&error(error_message::ITEM_NOT_FOR_PET)));
        assert!(w.b.drain().is_empty());
    }
}

/// Equipping into a slot that already holds a mate item swaps both items.
#[tokio::test]
async fn mate_equipment_swap_onto_an_occupied_slot() {
    let mut w = world_with_data(mate_pack_data());
    let mate = spawn_mate_with(&mut w, MATE_RIDING);
    let first = give(&w, &w.pa.clone(), slot_type::INVENTORY, FEET_ARMOR, 1, 3);
    let first_wire = item(&w, first).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&first_wire), None, 3, equip_slot::FEET as u8),
    );
    assert_eq!(
        mate.data().equipment[equip_slot::FEET as usize]
            .as_ref()
            .map(|i| i.id),
        Some(first)
    );
    w.a.drain();
    w.b.drain();

    let second = give(&w, &w.pa.clone(), slot_type::INVENTORY, FEET_ARMOR, 1, 4);
    let second_wire = item(&w, second).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&second_wire), None, 4, equip_slot::FEET as u8),
    );
    assert_eq!(
        mate.data().equipment[equip_slot::FEET as usize]
            .as_ref()
            .map(|i| i.id),
        Some(second)
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 4).unwrap().id, first);
    assert!(item_at(&w.pa, slot_type::INVENTORY, 3).is_none());
}

/// Unequipping into an occupied bag slot swaps the bag item onto the mate; an allowed bag item
/// swaps through, a disallowed one refuses the whole move and keeps both items in place.
#[tokio::test]
async fn mate_equipment_unequip_swap_checks_the_bag_item() {
    let mut w = world_with_data(mate_pack_data());
    let mate = spawn_mate_with(&mut w, MATE_RIDING);
    let worn = give(&w, &w.pa.clone(), slot_type::INVENTORY, FEET_ARMOR, 1, 3);
    let worn_wire = item(&w, worn).unwrap();
    run_mate(
        &mut w,
        &mate_body(Some(&worn_wire), None, 3, equip_slot::FEET as u8),
    );
    assert_eq!(
        mate.data().equipment[equip_slot::FEET as usize]
            .as_ref()
            .map(|i| i.id),
        Some(worn)
    );
    w.a.drain();
    w.b.drain();

    // Allowed swap: another feet item in bag slot 4 lands on the mate.
    let allowed = give(&w, &w.pa.clone(), slot_type::INVENTORY, FEET_ARMOR, 1, 4);
    run_mate(
        &mut w,
        &mate_body(None, Some(&worn_wire), 4, equip_slot::FEET as u8),
    );
    assert_eq!(
        mate.data().equipment[equip_slot::FEET as usize]
            .as_ref()
            .map(|i| i.id),
        Some(allowed)
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 4).unwrap().id, worn);
    w.a.drain();
    w.b.drain();

    // Disallowed swap: a chest item in bag slot 5 would land on the mate, so the move is
    // refused and both items stay where they are.
    let chest = give(&w, &w.pa.clone(), slot_type::INVENTORY, ARMOR, 1, 5);
    let chest_wire = item(&w, chest).unwrap();
    run_mate(
        &mut w,
        &mate_body(None, Some(&chest_wire), 5, equip_slot::FEET as u8),
    );
    // The wire item only selects the direction; the actual mate item is still `allowed`.
    assert_eq!(
        mate.data().equipment[equip_slot::FEET as usize]
            .as_ref()
            .map(|i| i.id),
        Some(allowed)
    );
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 5).unwrap().id, chest);
    let frames = w.a.drain();
    assert!(frames
        .iter()
        .any(|(op, _)| *op == sc::SCMateEquipmentChangedPacket));
    assert!(frames.contains(&error(error_message::ITEM_NOT_FOR_PET)));
    assert!(w.b.drain().is_empty());
}

#[test]
fn slave_and_repair_packets_decode() {
    use crate::systems::vehicles::packets::{CSChangeSlaveEquipmentPacket, WireItem};
    let mut s = PacketStream::new();
    s.write_u32(5)
        .write_u16(6)
        .write_u32(7)
        .write_bool(true)
        .write_u32(0) // no item in slot 1
        .write_u32(0) // no item in slot 2
        .write_u8(slot_type::INVENTORY)
        .write_u8(3)
        .write_u8(slot_type::EQUIPMENT_SLAVE)
        .write_u8(4);
    let p = CSChangeSlaveEquipmentPacket::read(&mut s).unwrap();
    assert_eq!(
        p,
        CSChangeSlaveEquipmentPacket {
            character_id: 5,
            slave_tl: 6,
            db_slave_id: 7,
            bts: true,
            item1: WireItem::default(),
            item2: WireItem::default(),
            slot1_type: slot_type::INVENTORY,
            slot1: 3,
            slot2_type: slot_type::EQUIPMENT_SLAVE,
            slot2: 4,
        }
    );
    let mut s = PacketStream::new();
    s.write_bc(0x123456);
    assert_eq!(
        CSRepairPetItemsPacket::read(&mut s).unwrap().npc_id,
        0x123456
    );
    let mut s = PacketStream::new();
    s.write_bc(0x10);
    assert_eq!(CSRepairSlaveItemsPacket::read(&mut s).unwrap().npc_id, 0x10);
}

#[test]
#[ignore]
fn real_compact_db_item_effect_tables() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = rusqlite::Connection::open(path).unwrap();
    let d = ItemEffectsData::load(&conn).unwrap();
    let with_fish = d
        .fishing_loot_packs
        .values()
        .filter(|p| p.sea > 0 || p.land > 0)
        .count();
    assert_eq!(with_fish, 40);
    assert!(!d.repairable_slaves.is_empty());
    let f = FormulaManager::load(&conn).unwrap();
    let formula = f
        .get_formula_by_kind(crate::data::formulas::FormulaKind::GradeEnchantCost)
        .unwrap();
    let params: std::collections::HashMap<&str, f64> = [
        ("item_grade", 1.0),
        ("item_level", 30.0),
        ("equip_slot_enchant_cost", 5.0),
    ]
    .into_iter()
    .collect();
    // (((30/3)^5) * (1/50000) + 150000 + 5000) * 0.5
    assert_eq!(formula.evaluate(&params) as i32, 77_501);
}

// ---------------------------------------------------------------------------------------------
// Item-cast plot end (`Plot.RunAsync`): ItemUnlock of the source item
// ---------------------------------------------------------------------------------------------

/// An item-cast plot (the stone-throwing plot 349 of skill 11268) of Alice with `source_id` as
/// the cast item, already run through its tree.
fn item_plot_state(w: &World, source_id: u64) -> Arc<crate::systems::skills::plots::PlotState> {
    use crate::systems::skills::data::SkillTemplate;
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::plots::PlotState;
    use crate::systems::skills::skill::CastCtx;
    use crate::systems::skills::state::SkillRun;
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::WorldObject;

    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            id: 11268,
            ..SkillTemplate::default()
        }),
        1,
    );
    PlotState::new(
        skill,
        CastCtx {
            caster: WorldObject::from(&w.pa),
            caster_caster: SkillCaster::Item {
                obj_id: w.pa.obj_id,
                item_id: source_id,
                item_template_id: STACK,
                type1: 0,
                type2: 0,
            },
            target: Target::Obj(WorldObject::from(&w.pb)),
            target_caster: SkillCastTarget::Unit(w.pb.obj_id),
            skill_object: SkillObject::None,
        },
    )
}

#[tokio::test]
async fn item_plot_end_unlocks_the_source_item() {
    let mut w = world();
    super::bridges::install(&w.server);
    let source_id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 2, 0);
    w.a.drain();
    let state = item_plot_state(&w, source_id);
    crate::systems::skills::plots::finish_item_plot(&w.server, 349, &state);
    let i = item(&w, source_id).unwrap();
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCItemTaskSuccessPacket,
            tasks(task_type::ITEM_UNLOCK, vec![ItemTask::update(&i)])
        )]
    );
}

/// The last stone of a stack is consumed by the plot's effects, so the source item is gone when
/// the plot ends: no `ItemUnlock` (nothing left to unlock) and no panic.
#[tokio::test]
async fn item_plot_end_skips_unlock_of_a_consumed_source_item() {
    let mut w = world();
    super::bridges::install(&w.server);
    let source_id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 1, 0);
    let state = item_plot_state(&w, source_id);
    w.server
        .systems
        .skills
        .ext
        .items()
        .consume_item_by_id(&w.server, &w.pa, source_id, 1);
    assert!(item(&w, source_id).is_none());
    assert!(w.server.items.item_by_id(source_id).is_none());
    w.a.drain();
    crate::systems::skills::plots::finish_item_plot(&w.server, 349, &state);
    assert!(w.a.drain().is_empty());
}

/// Records the template ids of `Character.ItemUse` quest events.
#[derive(Default)]
struct ItemUseLog(std::sync::Mutex<Vec<u32>>);

impl crate::systems::skills::ext::QuestSkillHooks for ItemUseLog {
    fn item_use(&self, _: &crate::server::GameServer, _: &crate::world::PlayerRef, _: u64) {
        panic!("the plot end must not look the used item up by id");
    }

    fn item_use_template(
        &self,
        _: &crate::server::GameServer,
        _: &crate::world::PlayerRef,
        template_id: u32,
    ) {
        self.0.lock().unwrap().push(template_id);
    }
}

/// The plot end raises the quest item-use event from the template id captured at cast start, so
/// it still fires when the plot consumed the last unit of its source item (quests 1315 / 3541
/// use the source item as their own reagent), and not when the plot was cancelled.
#[tokio::test]
async fn item_plot_end_raises_item_use_for_a_consumed_source_item() {
    let mut w = world();
    super::bridges::install(&w.server);
    let log = Arc::new(ItemUseLog::default());
    w.server.systems.skills.ext.quests.install(log.clone());
    let source_id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 1, 0);
    let state = item_plot_state(&w, source_id);
    w.server
        .systems
        .skills
        .ext
        .items()
        .consume_item_by_id(&w.server, &w.pa, source_id, 1);
    assert!(item(&w, source_id).is_none());
    w.a.drain();

    crate::systems::skills::plots::finish_item_plot(&w.server, 349, &state);
    assert_eq!(*log.0.lock().unwrap(), vec![STACK]);

    state.request_cancellation();
    crate::systems::skills::plots::finish_item_plot(&w.server, 349, &state);
    assert_eq!(*log.0.lock().unwrap(), vec![STACK]);
}

// ---------------------------------------------------------------------------------------------
// FishingLoot (basic fishing, skill 21571, a `Pos` target skill)
// ---------------------------------------------------------------------------------------------

fn bag_count(w: &World, template: u32) -> i32 {
    Inv::of(&w.pa)
        .bag()
        .lock()
        .unwrap()
        .items
        .iter()
        .filter(|i| i.template_id == template)
        .map(|i| i.count)
        .sum()
}

/// The fishing plot's `FishingLoot` effect targets the cast position (the skill's target type is
/// `Pos`); the catch must still be given from the zone group's sea pack.
#[tokio::test]
async fn fishing_loot_from_a_position_target_gives_the_sea_pack_item() {
    use crate::data::world_data::Zone;
    use crate::systems::economy::loot_data::{Loot, LootGroups, LootPack};
    use crate::systems::skills::geometry::{PosUnit, Target};

    let mut d = data();
    d.zones.zones.insert(
        129,
        Zone {
            id: 1,
            zone_key: 129,
            group_id: 1,
            ..Default::default()
        },
    );
    d.systems
        .economy
        .item_effects
        .fishing_loot_packs
        .insert(1, FishingLootPacks { land: 0, sea: 900 });
    let loot = Loot {
        id: 1,
        group: 1,
        item_id: STACK,
        drop_rate: 1,
        min_amount: 1,
        max_amount: 1,
        loot_pack_id: 900,
        grade_id: 0,
        always_drop: true,
    };
    d.systems.economy.loot.packs.insert(
        900,
        LootPack {
            id: 900,
            group_count: 1,
            loots: vec![loot],
            groups: vec![(
                1,
                LootGroups {
                    id: 1,
                    pack_id: 900,
                    group_no: 1,
                    drop_rate: 0,
                    item_grade_distribution_id: 0,
                },
            )],
            loots_by_group_no: vec![(1, vec![loot])],
            ..Default::default()
        },
    );
    let w = world_with_data(d);
    let target = Target::Pos(PosUnit {
        instance_id: 0,
        zone_id: 129,
        pos: crate::models::transform::PositionAndRotation {
            x: 100.0,
            y: 100.0,
            z: 90.0,
            ..Default::default()
        },
    });
    assert_eq!(bag_count(&w, STACK), 0);
    let mut rng = crate::systems::economy::loot::SharedRandom;
    fishing_loot(&w.server, &w.pa, &target, [0; 4], &mut rng);
    assert!(bag_count(&w, STACK) >= 1, "no fish was given");
}

#[tokio::test]
async fn full_refund_on_a_full_bag_keeps_the_item_loose_without_a_slot_add() {
    use crate::systems::skills::ext::{ItemSkillHooks, TakenCastItem};

    let mut w = world();
    let bag = Inv::of(&w.pa).bag();
    let size = bag.lock().unwrap().container_size();
    for slot in 0..size {
        give(&w, &w.pa, slot_type::INVENTORY, ARMOR, 1, slot);
    }
    assert_eq!(bag.lock().unwrap().free_slot_count(), 0, "bag is full");

    let mut item = w
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
        .expect("debited item");
    let id = item.id;
    item.owner_id = u64::from(w.pa.character_id);
    item.slot = 0;
    item.slot_type = slot_type::INVENTORY;
    let taken = TakenCastItem {
        item: item.clone(),
        container: bag.clone(),
        slot: 0,
        amount: 1,
        removed: true,
    };

    w.a.drain();
    super::bridges::EconomyForSkills.refund_cast_source_item(&w.server, taken);
    let loose = w.server.items.item_by_id(id).expect("loose refund");
    assert!(loose.dirty, "the loose refund must be saved");
    let bodies: Vec<Vec<u8>> =
        w.a.drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCItemTaskSuccessPacket)
            .map(|(_, body)| body)
            .collect();
    assert!(
        bodies.is_empty(),
        "a loose item is in no slot, so no slot-add is reported"
    );
}

fn take_partial(
    w: &World,
    item_id: u64,
    template_id: u32,
    amount: i32,
) -> crate::systems::skills::ext::TakenCastItem {
    use crate::systems::skills::ext::ItemSkillHooks;
    super::bridges::EconomyForSkills
        .take_cast_source_item(&w.server, &w.pa, item_id, template_id, amount)
        .expect("tracked items")
        .expect("source taken")
}

fn refund(w: &World, taken: crate::systems::skills::ext::TakenCastItem) {
    use crate::systems::skills::ext::ItemSkillHooks;
    super::bridges::EconomyForSkills.refund_cast_source_item(&w.server, taken);
}

fn only_item_tasks(w: &mut World) -> Vec<Vec<u8>> {
    w.a.drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCItemTaskSuccessPacket)
        .map(|(_, body)| body)
        .collect()
}

#[tokio::test]
async fn partial_refund_adds_back_after_a_merge_into_the_kept_stack() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 2, 0);
    let taken = take_partial(&w, id, STACK, 1);
    {
        let bag = Inv::of(&w.pa).bag();
        let mut bag = bag.lock().unwrap();
        bag.get_item_by_id_mut(id).unwrap().count += 2;
        bag.update_free_slot_count();
    }
    w.a.drain();
    refund(&w, taken);
    let after = item_at(&w.pa, slot_type::INVENTORY, 0).expect("kept stack");
    assert_eq!(after.count, 4, "1 left + 2 merged + 1 refunded");
    assert_eq!(
        only_item_tasks(&mut w),
        vec![tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::count_update(&after, 1)]
        )],
        "the refund count update names the merged stack and slot"
    );
}

#[tokio::test]
async fn partial_refund_adds_back_after_a_split_from_the_kept_stack() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 3, 0);
    let taken = take_partial(&w, id, STACK, 1);
    {
        let bag = Inv::of(&w.pa).bag();
        let mut bag = bag.lock().unwrap();
        bag.get_item_by_id_mut(id).unwrap().count = 1;
        bag.update_free_slot_count();
    }
    // The split half lives in another slot.
    let split = give(&w, &w.pa, slot_type::INVENTORY, STACK, 1, 4);
    w.a.drain();
    refund(&w, taken);
    let after = item_at(&w.pa, slot_type::INVENTORY, 0).expect("kept stack");
    assert_eq!(after.count, 2, "1 left + 1 refunded");
    assert_eq!(
        item_at(&w.pa, slot_type::INVENTORY, 4)
            .expect("split half")
            .id,
        split
    );
    assert_eq!(
        only_item_tasks(&mut w),
        vec![tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::count_update(&after, 1)]
        )]
    );
}

#[tokio::test]
async fn partial_refund_follows_the_kept_stack_to_a_new_slot() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 2, 0);
    let taken = take_partial(&w, id, STACK, 1);
    {
        let bag = Inv::of(&w.pa).bag();
        let mut bag = bag.lock().unwrap();
        bag.get_item_by_id_mut(id).unwrap().slot = 5;
    }
    w.a.drain();
    refund(&w, taken);
    let after = item_at(&w.pa, slot_type::INVENTORY, 5).expect("moved stack");
    assert_eq!(after.count, 2);
    assert_eq!(
        only_item_tasks(&mut w),
        vec![tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::count_update(&after, 1)]
        )],
        "the count update names the slot the stack now holds"
    );
}

#[tokio::test]
async fn partial_refund_recreates_the_debited_units_when_the_remainder_left() {
    let mut w = world();
    let id = give(&w, &w.pa, slot_type::INVENTORY, STACK, 2, 0);
    {
        let bag = Inv::of(&w.pa).bag();
        bag.lock().unwrap().get_item_by_id_mut(id).unwrap().ucc_id = 0xABCD;
    }
    let taken = take_partial(&w, id, STACK, 1);
    {
        let bag = Inv::of(&w.pa).bag();
        bag.lock().unwrap().items.retain(|i| i.id != id);
    }
    w.a.drain();
    refund(&w, taken);
    let bag = Inv::of(&w.pa).bag();
    let bag = bag.lock().unwrap();
    assert!(bag.get_item_by_id(id).is_none(), "the old stack is gone");
    let restored = bag
        .items
        .iter()
        .find(|i| i.template_id == STACK && i.id != id)
        .expect("fresh stack");
    assert_eq!(restored.count, 1);
    assert_eq!(restored.ucc_id, 0xABCD, "properties come from the snapshot");
    assert!(restored.dirty);
    let restored = restored.clone();
    drop(bag);
    assert_eq!(
        only_item_tasks(&mut w),
        vec![tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::add(&restored)]
        )]
    );
}

#[tokio::test]
async fn equipped_refund_restores_the_same_slot_with_equip_enter() {
    use crate::models::container::equip_slot;

    let mut w = world();
    let id = give(&w, &w.pa, slot_type::EQUIPMENT, ARMOR, 1, equip_slot::CHEST);
    let taken = take_partial(&w, id, ARMOR, 1);
    w.a.drain();
    w.b.drain();
    refund(&w, taken);
    let after =
        item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::CHEST as u8).expect("same equipment slot");
    assert_eq!(after.id, id);
    assert!(after.dirty);
    assert_eq!(
        only_item_tasks(&mut w),
        vec![tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::add(&after)]
        )]
    );
    let expected = body(&SCUnitEquipmentsChangedPacket {
        obj_id: w.pa.obj_id,
        items: vec![(equip_slot::CHEST as u8, Some(after))],
    });
    assert!(
        w.b.drain()
            .iter()
            .any(|(op, body)| *op == sc::SCUnitEquipmentsChangedPacket && *body == expected),
        "nearby players see the item back in its chest slot"
    );
}

#[tokio::test]
async fn equipped_refund_with_its_slot_taken_goes_to_the_bag_without_equip_enter() {
    use crate::models::container::equip_slot;

    let mut w = world();
    let id = give(&w, &w.pa, slot_type::EQUIPMENT, ARMOR, 1, equip_slot::CHEST);
    let taken = take_partial(&w, id, ARMOR, 1);
    // Another item refilled the chest slot in the meantime.
    let other = give(&w, &w.pa, slot_type::EQUIPMENT, ARMOR, 1, equip_slot::CHEST);
    w.a.drain();
    w.b.drain();
    refund(&w, taken);
    assert!(
        item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::CHEST as u8)
            .is_some_and(|i| i.id == other),
        "the refilled equipment slot is untouched"
    );
    let bag_item = item_at(&w.pa, slot_type::INVENTORY, 0)
        .filter(|i| i.id == id)
        .expect("refunded to the bag");
    assert_eq!(bag_item.slot_type, slot_type::INVENTORY);
    assert_eq!(
        only_item_tasks(&mut w),
        vec![tasks(
            task_type::SKILL_REAGENTS,
            vec![ItemTask::add(&bag_item)]
        )],
        "the client is told the item went to the bag"
    );
    assert!(
        !w.b.drain()
            .iter()
            .any(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket),
        "no equipment change is announced for a bag refund"
    );
}
