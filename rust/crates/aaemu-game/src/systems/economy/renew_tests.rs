//! Flow tests of `RenewEquipment` (57) and `PhysicalEnchantWeapon` / `PhysicalEnchantArmor`
//! (20 / 21), stage 1. They drive the real special-effect dispatch and the economy hook; no
//! MySQL is needed.

use std::sync::Arc;

use aaemu_commons::{Marshal, PacketStream};

use crate::data::items::{EquipTemplate, Holdable, ItemTemplate, TemplateKind, WearableKind};
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::opcodes::sc;
use crate::packets::world::SCErrorMsgPacket;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::loot::ScriptedRng;
use crate::systems::economy::renew::{
    renewable_pair, EnchantKind, PhysicalEnchantAbility, RenewData,
};
use crate::systems::economy::tests::{give, test_data, World};
use crate::systems::skills::effects::special;
use crate::systems::skills::effects::{EffectArgs, EffectSource};
use crate::systems::skills::geometry::Target;
use crate::systems::skills::state::SkillRun;
use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, PlayerRef, WorldObject};

const SRC_ARMOR: u32 = 6100;
const TGT_ARMOR: u32 = 6101;
const OTHER_ARMOR: u32 = 6102;
const SRC_WEAPON: u32 = 6110;
const TGT_WEAPON: u32 = 6111;
const OTHER_WEAPON: u32 = 6112;
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
    d
}

fn world() -> World {
    let server = Arc::new(test_server_with(data()));
    server.set_self();
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

/// Runs the real `RenewEquipment` / `PhysicalEnchant*` special effect through
/// `special::execute`, exactly as the skill pipeline does.
fn run_effect(
    w: &World,
    kind: i32,
    source: u64,
    source_slot: u32,
    target: u64,
    target_slot: u32,
    enchanter: Option<&crate::world::NpcRef>,
) {
    let skill = SkillRun::new(
        Arc::new(crate::systems::skills::tests::skill_template(10521)),
        1,
    );
    let unit_target = enchanter
        .map(|n| Target::Obj(WorldObject::from(n)))
        .unwrap_or_else(|| Target::Obj(WorldObject::from(&w.pa)));
    let args = EffectArgs {
        caster: Some(WorldObject::from(&w.pa)),
        caster_caster: SkillCaster::Item {
            obj_id: source_slot,
            item_id: source,
            item_template_id: item(&w.pa, source).map_or(0, |i| i.template_id),
            type1: 0,
            type2: 0,
        },
        target: unit_target,
        target_caster: SkillCastTarget::Item {
            obj_id: target_slot,
            id: target,
            type1: 0,
            type2: 0,
        },
        cast: CastAction::Skill {
            skill_id: 10521,
            tl_id: 1,
        },
        source: EffectSource::from_skill(skill),
        skill_object: &SkillObject::None,
    };
    special::execute(&w.server, kind, &args, [0; 4]);
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
async fn valid_physical_enchant_pair_rolls_without_mutating_items() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, TGT_ARMOR, 1, 1);
    let before_source = item(&w.pa, source).unwrap();
    let before_target = item(&w.pa, target).unwrap();
    let mut rng = ScriptedRng::new(&[0], &[]);
    let ok = super::renew::renew_equipment(
        &w.server,
        &w.pa,
        source,
        SRC_ARMOR,
        0,
        target,
        1,
        EnchantKind::Armor,
        Some(NPC),
        &mut rng,
    );
    assert!(ok);
    assert_eq!(rng.int_calls, vec![(0, 100)]);
    assert_eq!(item(&w.pa, source).unwrap(), before_source);
    assert_eq!(item(&w.pa, target).unwrap(), before_target);
    assert!(!w.pa.data().skill_char.skill_cancelled);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn physical_enchant_effect_rolls_and_keeps_both_items() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, TGT_ARMOR, 1, 1);
    let before_source = item(&w.pa, source).unwrap();
    let before_target = item(&w.pa, target).unwrap();
    run_effect(&w, 21, source, 0, target, 1, Some(&npc));
    assert_eq!(item(&w.pa, source).unwrap(), before_source);
    assert_eq!(item(&w.pa, target).unwrap(), before_target);
    assert!(!w.pa.data().skill_char.skill_cancelled);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn invalid_pair_is_refused_with_code_1() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, OTHER_ARMOR, 1, 1);
    let before_source = item(&w.pa, source).unwrap();
    let before_target = item(&w.pa, target).unwrap();
    run_effect(&w, 21, source, 0, target, 1, Some(&npc));
    assert_eq!(w.a.drain(), vec![error(error_message::INTERNAL_ERROR)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
    assert_eq!(item(&w.pa, source).unwrap(), before_source);
    assert_eq!(item(&w.pa, target).unwrap(), before_target);
}

#[tokio::test]
async fn missing_target_item_is_refused_with_code_0x1b() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    run_effect(&w, 21, source, 0, 0xDEAD_BEEF, 1, Some(&npc));
    assert_eq!(w.a.drain(), vec![error(error_message::NOT_ENOUGH_ITEM)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn operand_outside_the_bag_is_refused_with_code_0xce() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::EQUIPMENT, TGT_ARMOR, 1, 2);
    run_effect(&w, 21, source, 0, target, 1, Some(&npc));
    assert_eq!(w.a.drain(), vec![error(error_message::NOT_READY)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn operand_slot_out_of_range_is_refused_with_code_0x14() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, TGT_ARMOR, 1, 1);
    run_effect(&w, 21, source, 0, target, 200, Some(&npc));
    assert_eq!(w.a.drain(), vec![error(error_message::MONEY_LOCKED)]);
    assert!(w.pa.data().skill_char.skill_cancelled);
}

#[tokio::test]
async fn friendship_gate_refuses_without_a_roll() {
    let mut w = world();
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_ARMOR, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, TGT_ARMOR, 1, 1);
    // Level 2 needs friendship 80, which nothing in 1.2 grants.
    {
        let c = Inv::of(&w.pa).bag();
        c.lock()
            .unwrap()
            .get_item_by_id_mut(source)
            .unwrap()
            .details
            .temper_physical = 101;
    }
    let mut rng = ScriptedRng::new(&[0], &[]);
    let ok = super::renew::renew_equipment(
        &w.server,
        &w.pa,
        source,
        SRC_ARMOR,
        0,
        target,
        1,
        EnchantKind::Armor,
        Some(NPC),
        &mut rng,
    );
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
    run_effect(&w, 57, source, 0, target, 1, None);
    assert_eq!(item(&w.pa, source).unwrap(), before_source);
    assert_eq!(item(&w.pa, target).unwrap(), before_target);
    assert!(!w.pa.data().skill_char.skill_cancelled);
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn physical_enchant_weapon_arm_matches_weapon_pairs() {
    let mut w = world();
    let npc = spawn_enchanter(&mut w);
    let source = give(&w, &w.pa, slot_type::INVENTORY, SRC_WEAPON, 1, 0);
    let target = give(&w, &w.pa, slot_type::INVENTORY, TGT_WEAPON, 1, 1);
    run_effect(&w, 20, source, 0, target, 1, Some(&npc));
    assert!(!w.pa.data().skill_char.skill_cancelled);
    assert!(w.a.drain().is_empty());
}

/// Real 1.2 data: the enchanter ladder and the client's pair predicate.
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
}
