//! Flow tests of `RenewEquipment` (57) and `PhysicalEnchantWeapon` / `PhysicalEnchantArmor`
//! (20 / 21), stage 1. The single-item enchant tests drive the real `CSStartSkill` path; the
//! renew tests drive the economy hook (renew's `SkillItem` + target pair is not an item use
//! skill, so the item-cast gate in `CSStartSkill` does not apply to it). No MySQL is needed.

use std::sync::Arc;

use aaemu_commons::{Marshal, PacketStream};

use crate::data::items::{EquipTemplate, Holdable, ItemTemplate, TemplateKind, WearableKind};
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::world::SCErrorMsgPacket;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::loot::ScriptedRng;
use crate::systems::economy::renew::{renewable_pair, PhysicalEnchantAbility, RenewData};
use crate::systems::economy::tests::{give, test_data, World};
use crate::systems::skills::data::effects::{EffectKind, EffectTemplate, SpecialEffect};
use crate::systems::skills::handlers::CSStartSkillPacket;
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{
    open_client_skills, spawn_player, test_character, test_server_with, TestClient,
};
use crate::world::{Entity, PlayerRef, WorldObject};

const SRC_ARMOR: u32 = 6100;
const TGT_ARMOR: u32 = 6101;
const OTHER_ARMOR: u32 = 6102;
const SRC_WEAPON: u32 = 6110;
const TGT_WEAPON: u32 = 6111;
const OTHER_WEAPON: u32 = 6112;
/// A single armor / weapon the physical enchant works on.
const ENCHANT_ARMOR: u32 = 6120;
const ENCHANT_WEAPON: u32 = 6121;
/// A plain item: the physical enchant must refuse it (ui_text 1436 `invalid_item_msg`).
const NON_EQUIP: u32 = 6130;
const SKILL_WEAPON: u32 = 10521;
const SKILL_ARMOR: u32 = 10522;
const NPC: u32 = 947;
const NPC_OBJ: u32 = 0x200;

fn data() -> GameData {
    let mut d = test_data();
    let armor = |id, base_equipment, armor_type_id| ItemTemplate {
        max_count: 1,
        ..ItemTemplate::new(
            id,
            TemplateKind::Armor(EquipTemplate {
                slot_type_id: 3,
                base_equipment,
                armor_type_id,
                ..EquipTemplate::default()
            }),
        )
    };
    d.items
        .templates
        .insert(SRC_ARMOR, armor(SRC_ARMOR, true, 1));
    d.items
        .templates
        .insert(TGT_ARMOR, armor(TGT_ARMOR, false, 1));
    d.items
        .templates
        .insert(OTHER_ARMOR, armor(OTHER_ARMOR, false, 2));
    d.items
        .templates
        .insert(ENCHANT_ARMOR, armor(ENCHANT_ARMOR, false, 1));
    d.items
        .templates
        .insert(NON_EQUIP, ItemTemplate::plain(NON_EQUIP, 1));
    d.items.wearable_kinds.insert(
        1,
        WearableKind {
            type_id: 1,
            ..Default::default()
        },
    );
    d.items.wearable_kinds.insert(
        2,
        WearableKind {
            type_id: 2,
            ..Default::default()
        },
    );
    let weapon = |id, base_equipment, holdable_id| ItemTemplate {
        max_count: 1,
        ..ItemTemplate::new(
            id,
            TemplateKind::Weapon(EquipTemplate {
                slot_type_id: 17,
                base_equipment,
                holdable_id,
                ..EquipTemplate::default()
            }),
        )
    };
    d.items
        .templates
        .insert(SRC_WEAPON, weapon(SRC_WEAPON, true, 100));
    d.items
        .templates
        .insert(TGT_WEAPON, weapon(TGT_WEAPON, false, 101));
    d.items
        .templates
        .insert(OTHER_WEAPON, weapon(OTHER_WEAPON, false, 102));
    d.items
        .templates
        .insert(ENCHANT_WEAPON, weapon(ENCHANT_WEAPON, false, 101));
    for (id, renew_category) in [(100, 2), (101, 2), (102, 4)] {
        d.items.holdables.insert(
            id,
            Holdable {
                id,
                slot_type_id: 17,
                renew_category,
                ..Default::default()
            },
        );
    }
    d.systems.economy.renew.physical_enchant_abilities.insert(
        NPC,
        vec![
            PhysicalEnchantAbility {
                id: 1,
                npc_id: NPC,
                armor: true,
                enchant_level: 1,
                min_friendship: 0,
                success_ratio: 100,
            },
            PhysicalEnchantAbility {
                id: 2,
                npc_id: NPC,
                armor: false,
                enchant_level: 1,
                min_friendship: 0,
                success_ratio: 100,
            },
            PhysicalEnchantAbility {
                id: 3,
                npc_id: NPC,
                armor: true,
                enchant_level: 2,
                min_friendship: 80,
                success_ratio: 60,
            },
        ],
    );
    // The real 1.2 skills are common skills (auto-learn, no learn requirement, ability 0) with
    // `target_type_id 9` (Item) and special effects 20 / 21.
    let s = &mut d.systems.skills;
    s.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: 1,
            special_effect_type_id: 20,
            value1: 1,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        1,
    );
    s.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: 2,
            special_effect_type_id: 21,
            value1: 1,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        2,
    );
    for (id, effect, kind) in [
        (SKILL_WEAPON, 1, EffectKind::SpecialEffect),
        (SKILL_ARMOR, 2, EffectKind::SpecialEffect),
    ] {
        s.add_skill(crate::systems::skills::data::SkillTemplate {
            target_type: 9,
            min_range: 0,
            max_range: 4,
            effects: vec![crate::systems::skills::tests::skill_effect(
                effect, kind, effect,
            )],
            ..crate::systems::skills::tests::skill_template(id)
        });
    }
    d
}

fn world() -> World {
    let mut data = data();
    open_client_skills(&mut data);
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    super::bridges::install(&server);
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

fn item(p: &PlayerRef, id: u64) -> Option<Item> {
    Inv::of(p).item_by_id(id)
}

fn spawn_enchanter(w: &mut World) -> crate::world::NpcRef {
    let mut npc = crate::models::npc::Npc::new(NPC);
    npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let npc = Entity::new(NPC_OBJ, npc);
    w.server
        .world
        .world_of(&WorldObject::from(&w.pa))
        .unwrap()
        .spawn(&npc, &w.server.view_ctx());
    w.a.drain();
    npc
}

fn error(e: i16) -> (u16, Vec<u8>) {
    let mut s = PacketStream::new();
    SCErrorMsgPacket::new(e, 0, true).write(&mut s);
    (sc::SCErrorMsgPacket, s.into_bytes())
}

fn errors(frames: &[(u16, Vec<u8>)]) -> Vec<(u16, Vec<u8>)> {
    frames
        .iter()
        .filter(|(op, _)| *op == sc::SCErrorMsgPacket)
        .cloned()
        .collect()
}

/// The real client's physical enchant cast: a common skill, caster = the player, target = the
/// one `SkillCastItemTarget` selected in the NPC dialog. Its `obj_id` is the item's owner (see
/// `FUN_394e92d0`), not a bag slot; the effect resolves the item by `id`.
async fn cast(w: &mut World, skill_id: u32, item_id: u64) {
    CSStartSkillPacket {
        skill_id,
        caster: SkillCaster::Unit(w.pa.obj_id),
        target: SkillCastTarget::Item {
            obj_id: w.pa.obj_id,
            id: item_id,
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

#[test]
fn pair_predicate_matches_the_client() {
    let d = data();
    let t = |id| d.items.template(id).unwrap();
    assert!(renewable_pair(&d.items, t(SRC_ARMOR), t(TGT_ARMOR)));
    assert!(!renewable_pair(&d.items, t(TGT_ARMOR), t(SRC_ARMOR)));
    assert!(!renewable_pair(&d.items, t(SRC_ARMOR), t(OTHER_ARMOR)));
    assert!(renewable_pair(&d.items, t(SRC_WEAPON), t(TGT_WEAPON)));
    assert!(!renewable_pair(&d.items, t(SRC_WEAPON), t(OTHER_WEAPON)));
    assert!(!renewable_pair(&d.items, t(TGT_WEAPON), t(SRC_WEAPON)));
    // An armor source with a weapon target and vice versa is not a pair.
    assert!(!renewable_pair(&d.items, t(SRC_ARMOR), t(TGT_WEAPON)));
}

#[tokio::test]
async fn single_item_armor_enchant_is_accepted_through_start_skill() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    // The dialog keeps the blacksmith interaction open; the cast itself names no NPC.
    w.pa.data().unit.current_interaction_object = npc.obj_id;
    let item_id = give(&w, &w.pa, slot_type::INVENTORY, ENCHANT_ARMOR, 1, 0);
    let before = item(&w.pa, item_id).unwrap();
    cast(&mut w, SKILL_ARMOR, item_id).await;
    assert!(!w.pa.data().skill_char.skill_cancelled);
    let frames = w.a.drain();
    assert_eq!(errors(&frames), vec![], "unexpected refusal: {frames:?}");
    assert_eq!(item(&w.pa, item_id).unwrap(), before);
}

#[tokio::test]
async fn single_item_weapon_enchant_is_accepted_through_start_skill() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    w.pa.data().unit.current_interaction_object = npc.obj_id;
    let item_id = give(&w, &w.pa, slot_type::INVENTORY, ENCHANT_WEAPON, 1, 0);
    let before = item(&w.pa, item_id).unwrap();
    cast(&mut w, SKILL_WEAPON, item_id).await;
    assert!(!w.pa.data().skill_char.skill_cancelled);
    let frames = w.a.drain();
    assert_eq!(errors(&frames), vec![], "unexpected refusal: {frames:?}");
    assert_eq!(item(&w.pa, item_id).unwrap(), before);
}

#[tokio::test]
async fn physical_enchant_invalid_item_is_refused_with_item_cannot_use() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    w.pa.data().unit.current_interaction_object = npc.obj_id;
    let item_id = give(&w, &w.pa, slot_type::INVENTORY, ENCHANT_ARMOR, 1, 0);
    // The armor arm (21) on an armor is valid; a weapon arm (20) on it is the dialog's
    // "can't enchant this item" refusal.
    cast(&mut w, SKILL_WEAPON, item_id).await;
    let frames = w.a.drain();
    assert_eq!(errors(&frames), vec![error(error_message::ITEM_CANNOT_USE)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn physical_enchant_non_equipment_is_refused_with_item_cannot_use() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    w.pa.data().unit.current_interaction_object = npc.obj_id;
    let item_id = give(&w, &w.pa, slot_type::INVENTORY, NON_EQUIP, 1, 0);
    cast(&mut w, SKILL_ARMOR, item_id).await;
    let frames = w.a.drain();
    assert_eq!(errors(&frames), vec![error(error_message::ITEM_CANNOT_USE)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn physical_enchant_missing_item_is_refused_with_code_0x1b() {
    let mut w = world();
    let mut rng = ScriptedRng::new(&[0], &[]);
    let ok =
        super::renew::physical_enchant(&w.server, &w.pa, 0xDEAD_BEEF, true, Some(NPC), &mut rng);
    assert!(!ok);
    assert_eq!(w.a.drain(), vec![error(error_message::NOT_ENOUGH_ITEM)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn physical_enchant_ability_gate_refuses_without_a_roll() {
    let mut w = world();
    let item_id = give(&w, &w.pa, slot_type::INVENTORY, ENCHANT_ARMOR, 1, 0);
    // temper_physical 102 = step 2, so the next step is 3; the fixture's ladder ends at 2.
    {
        let bag = Inv::of(&w.pa).bag();
        bag.lock()
            .unwrap()
            .get_item_by_id_mut(item_id)
            .unwrap()
            .details
            .temper_physical = 102;
    }
    let mut rng = ScriptedRng::new(&[0], &[]);
    let ok = super::renew::physical_enchant(&w.server, &w.pa, item_id, true, Some(NPC), &mut rng);
    assert!(!ok);
    assert!(rng.int_calls.is_empty());
    assert_eq!(w.a.drain(), vec![error(error_message::NOT_READY)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn physical_enchant_friendship_gate_refuses_without_a_roll() {
    let mut w = world();
    let item_id = give(&w, &w.pa, slot_type::INVENTORY, ENCHANT_ARMOR, 1, 0);
    // temper_physical 101 = step 1, so the next step is 2; that row needs friendship 80,
    // which nothing in 1.2 grants.
    {
        let bag = Inv::of(&w.pa).bag();
        bag.lock()
            .unwrap()
            .get_item_by_id_mut(item_id)
            .unwrap()
            .details
            .temper_physical = 101;
    }
    let mut rng = ScriptedRng::new(&[0], &[]);
    let ok = super::renew::physical_enchant(&w.server, &w.pa, item_id, true, Some(NPC), &mut rng);
    assert!(!ok);
    assert!(rng.int_calls.is_empty());
    assert_eq!(
        w.a.drain(),
        vec![error(error_message::NOT_ENOUGH_FRIENDSHIP)]
    );
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn renew_equipment_validates_and_logs_without_mutating() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_WEAPON, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, TGT_WEAPON, 1, 1);
    let before_source = item(&w.pa, source).unwrap();
    let before_target = item(&w.pa, target).unwrap();
    let ok = super::renew::renew_equipment(&w.server, &w.pa, source, SRC_WEAPON, 0, target, 1);
    assert!(ok);
    assert_eq!(item(&w.pa, source).unwrap(), before_source);
    assert_eq!(item(&w.pa, target).unwrap(), before_target);
    assert!(!w.pa.data().skill_char.skill_cancelled);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn renew_invalid_pair_is_refused_with_code_1() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, OTHER_ARMOR, 1, 1);
    let before_source = item(&w.pa, source).unwrap();
    let before_target = item(&w.pa, target).unwrap();
    let ok = super::renew::renew_equipment(&w.server, &w.pa, source, SRC_ARMOR, 0, target, 1);
    assert!(!ok);
    assert_eq!(w.a.drain(), vec![error(error_message::INTERNAL_ERROR)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
    assert_eq!(item(&w.pa, source).unwrap(), before_source);
    assert_eq!(item(&w.pa, target).unwrap(), before_target);
}

#[tokio::test]
async fn renew_missing_target_item_is_refused_with_code_0x1b() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let ok = super::renew::renew_equipment(&w.server, &w.pa, source, SRC_ARMOR, 0, 0xDEAD_BEEF, 1);
    assert!(!ok);
    assert_eq!(w.a.drain(), vec![error(error_message::NOT_ENOUGH_ITEM)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn renew_operand_outside_the_bag_is_refused_with_code_0xce() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::EQUIPMENT, TGT_ARMOR, 1, 2);
    let ok = super::renew::renew_equipment(&w.server, &w.pa, source, SRC_ARMOR, 0, target, 1);
    assert!(!ok);
    assert_eq!(w.a.drain(), vec![error(error_message::NOT_READY)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn renew_operand_slot_out_of_range_is_refused_with_code_0x14() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, TGT_ARMOR, 1, 1);
    let ok = super::renew::renew_equipment(&w.server, &w.pa, source, SRC_ARMOR, 0, target, 200);
    assert!(!ok);
    assert_eq!(w.a.drain(), vec![error(error_message::MONEY_LOCKED)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

/// Real 1.2 data: the enchanter ladder, the client's pair predicate, and the evidence that the
/// physical enchant skills are single-item (`target_type_id 9`) effects 20 / 21.
#[test]
#[ignore]
fn physical_enchant_abilities_and_pairs_from_the_real_data() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = rusqlite::Connection::open(path).unwrap();
    let renew = RenewData::load(&conn).unwrap();
    let rows: usize = renew
        .physical_enchant_abilities
        .values()
        .map(Vec::len)
        .sum();
    assert_eq!(rows, 86);
    assert_eq!(renew.physical_enchant_abilities.len(), 11);
    let a = renew.ability(1, true, 1).unwrap();
    assert_eq!((a.min_friendship, a.success_ratio), (0, 100));
    let a = renew.ability(1, false, 8).unwrap();
    assert_eq!((a.min_friendship, a.success_ratio), (3001, 9));

    let items = crate::data::items::ItemData::load(&conn).unwrap();
    // Source 8473 (`base_equipment` true, renew 1, slot 17) with material 26517 (false, 1, 17).
    assert!(renewable_pair(
        &items,
        items.template(8473).unwrap(),
        items.template(26517).unwrap()
    ));
    // Armor type 1 source / type 1 material.
    assert!(renewable_pair(
        &items,
        items.template(8559).unwrap(),
        items.template(30069).unwrap()
    ));

    for (skill_id, effect_type) in [(10521, 20), (10522, 21)] {
        let target_type: i32 = conn
            .query_row(
                "SELECT target_type_id FROM skills WHERE id = ?1",
                [skill_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(target_type, 9, "skill {skill_id} must target one Item");
        let actual: i32 = conn
            .query_row(
                "SELECT se.special_effect_type_id FROM special_effects se \
                 JOIN effects e ON e.actual_id = se.id AND e.actual_type = 'SpecialEffect' \
                 JOIN skill_effects ske ON ske.effect_id = e.id \
                 WHERE ske.skill_id = ?1",
                [skill_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(actual, effect_type, "skill {skill_id} effect");
    }
}
