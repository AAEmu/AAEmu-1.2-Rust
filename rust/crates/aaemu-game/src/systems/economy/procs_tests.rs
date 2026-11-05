//! Item proc / equip-set proc tests: the active set from equipment, the chance roll, the
//! cooldown, the damage-kind filters, the destroyed-item rule and the cast guard.

use std::sync::Arc;

use crate::data::items::{
    EquipItemSet, EquipItemSetBonus, EquipTemplate, Holdable, ItemProcTemplate, ItemTemplate,
    TemplateKind,
};
use crate::data::GameData;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::Item;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::skills::data::effects::BuffEffect;
use crate::systems::skills::data::{BuffTemplate, EffectKind, EffectTemplate, SkillTemplate};
use crate::systems::skills::effects::{EffectArgs, EffectSource};
use crate::systems::skills::geometry::Target;
use crate::systems::skills::types::{
    DamageType, SkillCastTarget, SkillCaster, SkillHitType, SkillObject,
};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, NpcRef, PlayerRef, WorldObject};

use super::procs::{self, chance_kind};

const PLAYER_OBJ: u32 = 0x100;
const NPC_OBJ: u32 = 0x200;
const BUFF: u32 = 700;

fn add_buff_skill(data: &mut GameData, skill_id: u32, buff_id: u32, target_type: i32) {
    let effect_id = skill_id + 1000;
    data.systems.skills.add_buff(BuffTemplate {
        id: buff_id,
        kind: 1,
        duration: 60_000,
        ..Default::default()
    });
    data.systems.skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: effect_id,
            buff_id: Some(buff_id),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        0,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: skill_id,
        target_type,
        effects: vec![crate::systems::skills::tests::skill_effect(
            effect_id,
            EffectKind::BuffEffect,
            effect_id,
        )],
        ..crate::systems::skills::tests::skill_template(skill_id)
    });
}

fn add_damage_skill(data: &mut GameData, skill_id: u32, amount: i32, target_type: i32) {
    let effect_id = skill_id + 1000;
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(crate::systems::skills::tests::damage_effect(
            effect_id, amount,
        ))),
        0,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: skill_id,
        target_type,
        effects: vec![crate::systems::skills::tests::skill_effect(
            effect_id,
            EffectKind::DamageEffect,
            effect_id,
        )],
        ..crate::systems::skills::tests::skill_template(skill_id)
    });
}

fn add_proc(data: &mut GameData, id: u32, skill_id: u32, kind: u32, rate: u32, cooldown: u32) {
    data.items.item_procs.insert(
        id,
        ItemProcTemplate {
            id,
            skill_id,
            chance_kind: kind,
            chance_rate: rate,
            cooldown_sec: cooldown,
            ..Default::default()
        },
    );
}

fn add_armor(data: &mut GameData, id: u32, set_id: u32) {
    data.items.templates.insert(
        id,
        ItemTemplate {
            kind: TemplateKind::Armor(EquipTemplate {
                slot_type_id: 3,
                equip_item_set_id: set_id,
                ..Default::default()
            }),
            ..ItemTemplate::plain(id, 1)
        },
    );
}

fn add_weapon(data: &mut GameData, id: u32, holdable_id: u32) {
    data.items.templates.insert(
        id,
        ItemTemplate {
            kind: TemplateKind::Weapon(EquipTemplate {
                slot_type_id: 14,
                holdable_id,
                ..Default::default()
            }),
            ..ItemTemplate::plain(id, 1)
        },
    );
}

struct Fixture {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    npc: NpcRef,
}

fn fixture(data: GameData) -> Fixture {
    let server = Arc::new(test_server_with(data));
    crate::systems::skills::bind(&server);
    super::bridges::install(&server);
    let client = TestClient::new(&server, 1, 1);
    let mut character = test_character(1, "Procs", 100.0, 100.0);
    character.unit.hp = 10_000;
    character.unit.max_hp = 10_000;
    character.unit.mp = 1_000;
    character.unit.max_mp = 1_000;
    let player = spawn_player(&server, &client, character, PLAYER_OBJ);
    let mut npc = Npc::new(1);
    npc.unit.hp = 1_000;
    npc.unit.max_hp = 1_000;
    npc.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let npc = Entity::new(NPC_OBJ, npc);
    server
        .world
        .world(0)
        .expect("world")
        .spawn(&npc, &server.view_ctx());
    Fixture {
        server,
        client,
        player,
        npc,
    }
}

fn equip(
    server: &GameServer,
    player: &PlayerRef,
    template_id: u32,
    slot: i32,
    item_id: u64,
) -> Item {
    let template = server
        .game_data
        .items
        .template(template_id)
        .expect("template");
    let mut item = Item::create(item_id, template, 1, 0, 0, &server.game_data.items, 0);
    item.class = template.class_type();
    item.owner_id = u64::from(player.character_id);
    item.slot_type = slot_type::EQUIPMENT;
    item.slot = slot;
    player
        .data()
        .inventory
        .equipment()
        .lock()
        .unwrap()
        .items
        .push(item.clone());
    item
}

fn refresh(server: &GameServer, player: &PlayerRef, added: Option<&Item>, removed: Option<&Item>) {
    server
        .systems
        .economy
        .hooks
        .gear
        .get()
        .expect("gear hook")
        .update_gear_bonuses(server, &WorldObject::from(player), added, removed);
}

fn active(player: &PlayerRef) -> Vec<u32> {
    player.data().unit.item_procs.active_ids().collect()
}

fn has_buff(player: &PlayerRef, buff_id: u32) -> bool {
    player.data().unit.buffs.check_buff(buff_id)
}

fn drain(client: &mut TestClient) {
    client.drain();
}

fn skill_fired(client: &mut TestClient) -> usize {
    client
        .drain()
        .iter()
        .filter(|(op, _)| *op == sc::SCSkillFiredPacket)
        .count()
}

fn damage_args(player: &PlayerRef, npc: &NpcRef) -> EffectArgs<'static> {
    let caster = WorldObject::from(player);
    let target = WorldObject::from(npc);
    EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(caster.obj_id()),
        target: Target::Obj(target.clone()),
        target_caster: SkillCastTarget::Unit(target.obj_id()),
        cast: crate::systems::skills::types::CastAction::Skill {
            skill_id: 1,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    }
}

fn hit_player_on_npc(server: &GameServer, player: &PlayerRef, npc: &NpcRef) {
    let effect = crate::systems::skills::tests::damage_effect(1, 100);
    let args = damage_args(player, npc);
    crate::systems::skills::effects::combat::damage(server, &effect, &args, None);
}

#[tokio::test]
async fn set_proc_is_active_at_the_piece_count_and_fires_on_a_landed_hit() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 800, BUFF, 0);
    add_proc(&mut data, 7, 800, chance_kind::HIT_ANY, 100, 0);
    data.items.equip_item_sets.insert(
        900,
        EquipItemSet {
            id: 900,
            bonuses: vec![EquipItemSetBonus {
                num_pieces: 4,
                buff_id: 0,
                item_proc_id: 7,
            }],
        },
    );
    for id in 900..904 {
        add_armor(&mut data, id, 900);
    }
    let mut f = fixture(data);

    for (i, slot) in [3, 4, 5].into_iter().enumerate() {
        let item = equip(&f.server, &f.player, 900 + i as u32, slot, 10 + i as u64);
        refresh(&f.server, &f.player, Some(&item), None);
        assert!(active(&f.player).is_empty(), "3 pieces must not activate");
    }
    let fourth = equip(&f.server, &f.player, 903, 6, 20);
    refresh(&f.server, &f.player, Some(&fourth), None);
    assert_eq!(active(&f.player), vec![7]);

    drain(&mut f.client);
    hit_player_on_npc(&f.server, &f.player, &f.npc);
    assert!(has_buff(&f.player, BUFF), "the set proc fires on a hit");
    assert_eq!(skill_fired(&mut f.client), 1);
}

#[tokio::test]
async fn binding_and_holdable_procs_of_equipped_weapons_fire() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 801, BUFF + 1, 0);
    add_proc(&mut data, 8, 801, chance_kind::HIT_ANY, 100, 0);
    add_buff_skill(&mut data, 802, BUFF + 2, 0);
    add_proc(&mut data, 9, 802, chance_kind::HIT_ANY, 100, 0);
    add_weapon(&mut data, 500, 40);
    add_weapon(&mut data, 501, 0);
    data.items.item_proc_bindings.insert(500, vec![8]);
    data.items.holdables.insert(
        40,
        Holdable {
            id: 40,
            item_proc_id: 9,
            ..Default::default()
        },
    );
    data.items.item_proc_bindings.insert(501, vec![8]);
    let mut f = fixture(data);

    let weapon = equip(&f.server, &f.player, 500, equip_slot::MAINHAND, 30);
    refresh(&f.server, &f.player, Some(&weapon), None);
    assert_eq!(active(&f.player), vec![8, 9]);

    drain(&mut f.client);
    hit_player_on_npc(&f.server, &f.player, &f.npc);
    assert!(has_buff(&f.player, BUFF + 1), "binding proc fires");
    assert!(has_buff(&f.player, BUFF + 2), "holdable proc fires");
}

#[tokio::test]
async fn cooldown_blocks_a_second_fire_and_allows_it_after_it_elapses() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 803, BUFF + 3, 0);
    add_proc(&mut data, 10, 803, chance_kind::HIT_ANY, 100, 10);
    add_weapon(&mut data, 502, 0);
    data.items.item_proc_bindings.insert(502, vec![10]);
    let f = fixture(data);
    let weapon = equip(&f.server, &f.player, 502, equip_slot::MAINHAND, 31);
    refresh(&f.server, &f.player, Some(&weapon), None);
    let caster = WorldObject::from(&f.player);
    let target = WorldObject::from(&f.npc);

    procs::roll_for_kind_with(&f.server, &caster, chance_kind::HIT_ANY, &target, 1_000);
    assert!(has_buff(&f.player, BUFF + 3));
    crate::systems::skills::buffs::remove_buff(&f.server, &caster, BUFF + 3);

    procs::roll_for_kind_with(&f.server, &caster, chance_kind::HIT_ANY, &target, 10_999);
    assert!(!has_buff(&f.player, BUFF + 3), "inside the cooldown");

    procs::roll_for_kind_with(&f.server, &caster, chance_kind::HIT_ANY, &target, 11_000);
    assert!(has_buff(&f.player, BUFF + 3), "cooldown elapsed");
}

#[tokio::test]
async fn cooldown_survives_re_equipping_the_same_proc() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 812, BUFF + 11, 0);
    add_proc(&mut data, 19, 812, chance_kind::HIT_ANY, 100, 10);
    add_weapon(&mut data, 508, 0);
    data.items.item_proc_bindings.insert(508, vec![19]);
    let f = fixture(data);
    let caster = WorldObject::from(&f.player);
    let target = WorldObject::from(&f.npc);

    let weapon = equip(&f.server, &f.player, 508, equip_slot::MAINHAND, 80);
    refresh(&f.server, &f.player, Some(&weapon), None);
    procs::roll_for_kind_with(&f.server, &caster, chance_kind::HIT_ANY, &target, 1_000);
    assert!(has_buff(&f.player, BUFF + 11));
    crate::systems::skills::buffs::remove_buff(&f.server, &caster, BUFF + 11);

    f.player
        .data()
        .inventory
        .equipment()
        .lock()
        .unwrap()
        .items
        .retain(|i| i.id != weapon.id);
    refresh(&f.server, &f.player, None, Some(&weapon));
    assert!(active(&f.player).is_empty());
    let again = equip(&f.server, &f.player, 508, equip_slot::MAINHAND, 81);
    refresh(&f.server, &f.player, Some(&again), None);
    assert_eq!(active(&f.player), vec![19]);

    procs::roll_for_kind_with(&f.server, &caster, chance_kind::HIT_ANY, &target, 5_000);
    assert!(
        !has_buff(&f.player, BUFF + 11),
        "the cooldown survives the re-equip"
    );
    procs::roll_for_kind_with(&f.server, &caster, chance_kind::HIT_ANY, &target, 11_000);
    assert!(has_buff(&f.player, BUFF + 11));
}

#[tokio::test]
async fn chance_rate_is_a_percentage() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 804, BUFF + 4, 0);
    add_proc(&mut data, 11, 804, chance_kind::HIT_ANY, 1, 0);
    add_weapon(&mut data, 503, 0);
    data.items.item_proc_bindings.insert(503, vec![11]);
    let f = fixture(data);
    let weapon = equip(&f.server, &f.player, 503, equip_slot::MAINHAND, 32);
    refresh(&f.server, &f.player, Some(&weapon), None);
    // The chance check `roll_for_kind` uses: rate 1 fires on roll 0 only.
    let proc = crate::models::item::ItemProc::new(11);
    let data = &f.server.game_data.items;
    assert_eq!(proc.apply(data, 0, || 0), Some(804));
    assert_eq!(proc.apply(data, 0, || 1), None);
}

#[tokio::test]
async fn destroyed_item_drops_its_proc_but_the_set_count_keeps_the_set_proc() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 805, BUFF + 5, 0);
    add_proc(&mut data, 12, 805, chance_kind::HIT_ANY, 100, 0);
    add_buff_skill(&mut data, 806, BUFF + 6, 0);
    add_proc(&mut data, 13, 806, chance_kind::HIT_ANY, 100, 0);
    add_weapon(&mut data, 504, 0);
    data.items.item_proc_bindings.insert(504, vec![12]);
    data.items.equip_item_sets.insert(
        901,
        EquipItemSet {
            id: 901,
            bonuses: vec![EquipItemSetBonus {
                num_pieces: 2,
                buff_id: 0,
                item_proc_id: 13,
            }],
        },
    );
    data.items.templates.insert(
        504,
        ItemTemplate {
            kind: TemplateKind::Weapon(EquipTemplate {
                slot_type_id: 14,
                equip_item_set_id: 901,
                durability_ratio: 1.0,
                durability_multiplier: 1000,
                ..Default::default()
            }),
            ..ItemTemplate::plain(504, 1)
        },
    );
    data.items
        .insert_grade(crate::data::items::FullGradeTemplate {
            grade: 0,
            grade_order: 0,
            durability: 1.0,
            stat_multiplier: 100,
            refund_multiplier: 100,
            ..Default::default()
        });
    data.items.templates.insert(
        505,
        ItemTemplate {
            kind: TemplateKind::Armor(EquipTemplate {
                slot_type_id: 3,
                equip_item_set_id: 901,
                ..Default::default()
            }),
            ..ItemTemplate::plain(505, 1)
        },
    );
    data.items.config.durability_const = 1.0;
    data.items.config.holdable_durability_const = 1.0;
    let f = fixture(data);
    let first = equip(&f.server, &f.player, 504, equip_slot::MAINHAND, 40);
    refresh(&f.server, &f.player, Some(&first), None);
    equip(&f.server, &f.player, 505, 6, 41);
    refresh(&f.server, &f.player, None, None);
    assert_eq!(active(&f.player), vec![13, 12]);

    let broken = {
        let container = super::inventory::Inv::of(&f.player).equipment();
        let mut equipment = container.lock().unwrap();
        let item = equipment
            .items
            .iter_mut()
            .find(|i| i.id == first.id)
            .expect("equipped");
        item.details.durability = 0;
        item.clone()
    };
    assert!(
        !broken.is_not_destroyed(&f.server.game_data.items),
        "test item must be destroyable (max durability > 0)"
    );
    {
        let container = super::inventory::Inv::of(&f.player).equipment();
        let items = container.lock().unwrap().items.clone();
        assert_eq!(procs::active_ids_for(&f.server, &items), vec![13]);
    }
    refresh(&f.server, &f.player, None, Some(&broken));
    // The binding proc of the destroyed item is gone; the set still counts its piece.
    assert_eq!(active(&f.player), vec![13]);
}

#[tokio::test]
async fn unequipping_removes_the_proc() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 807, BUFF + 7, 0);
    add_proc(&mut data, 14, 807, chance_kind::HIT_ANY, 100, 0);
    add_weapon(&mut data, 505, 0);
    data.items.item_proc_bindings.insert(505, vec![14]);
    let f = fixture(data);
    let weapon = equip(&f.server, &f.player, 505, equip_slot::MAINHAND, 50);
    refresh(&f.server, &f.player, Some(&weapon), None);
    assert_eq!(active(&f.player), vec![14]);

    f.player
        .data()
        .inventory
        .equipment()
        .lock()
        .unwrap()
        .items
        .retain(|i| i.id != weapon.id);
    refresh(&f.server, &f.player, None, Some(&weapon));
    assert!(active(&f.player).is_empty());
}

#[tokio::test]
async fn damage_kinds_filter_melee_crit_and_ranged_and_victim_kinds() {
    let mut data = GameData::empty();
    add_buff_skill(&mut data, 808, BUFF + 8, 0);
    add_proc(&mut data, 15, 808, chance_kind::HIT_MELEE, 100, 0);
    add_buff_skill(&mut data, 809, BUFF + 9, 0);
    add_proc(&mut data, 16, 809, chance_kind::HIT_MELEE_CRIT, 100, 0);
    add_buff_skill(&mut data, 810, BUFF + 10, 0);
    add_proc(&mut data, 17, 810, chance_kind::TAKE_DAMAGE_ANY, 100, 0);
    add_weapon(&mut data, 506, 0);
    data.items.item_proc_bindings.insert(506, vec![15, 16, 17]);
    let f = fixture(data);
    let weapon = equip(&f.server, &f.player, 506, equip_slot::MAINHAND, 60);
    refresh(&f.server, &f.player, Some(&weapon), None);
    let player = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);

    procs::roll_damage(
        &f.server,
        &player,
        &npc,
        Some(DamageType::Melee),
        SkillHitType::MeleeHit,
    );
    assert!(has_buff(&f.player, BUFF + 8), "melee kind fires");
    assert!(!has_buff(&f.player, BUFF + 9), "crit kind needs a crit");
    for id in [BUFF + 8, BUFF + 9] {
        crate::systems::skills::buffs::remove_buff(&f.server, &player, id);
    }

    procs::roll_damage(
        &f.server,
        &player,
        &npc,
        Some(DamageType::Melee),
        SkillHitType::MeleeCritical,
    );
    assert!(has_buff(&f.player, BUFF + 8) && has_buff(&f.player, BUFF + 9));
    for id in [BUFF + 8, BUFF + 9] {
        crate::systems::skills::buffs::remove_buff(&f.server, &player, id);
    }

    procs::roll_damage(
        &f.server,
        &player,
        &npc,
        Some(DamageType::Ranged),
        SkillHitType::RangedHit,
    );
    assert!(
        !has_buff(&f.player, BUFF + 8),
        "a ranged hit must not fire a melee proc"
    );

    // The victim kinds fire on the player when the NPC "attacks" it.
    procs::roll_damage(
        &f.server,
        &npc,
        &player,
        Some(DamageType::Melee),
        SkillHitType::MeleeHit,
    );
    assert!(has_buff(&f.player, BUFF + 10), "take-damage kind fires");
}

#[tokio::test]
async fn a_proc_skills_own_damage_does_not_roll_procs_again() {
    let mut data = GameData::empty();
    // Kind 1, no cooldown: without the cast guard this would re-proc from its own hit.
    add_damage_skill(&mut data, 811, 100, 4);
    add_proc(&mut data, 18, 811, chance_kind::HIT_ANY, 100, 0);
    add_weapon(&mut data, 507, 0);
    data.items.item_proc_bindings.insert(507, vec![18]);
    let mut f = fixture(data);
    let weapon = equip(&f.server, &f.player, 507, equip_slot::MAINHAND, 70);
    refresh(&f.server, &f.player, Some(&weapon), None);
    drain(&mut f.client);

    let caster = WorldObject::from(&f.player);
    let target = WorldObject::from(&f.npc);
    procs::roll_for_kind_with(&f.server, &caster, chance_kind::HIT_ANY, &target, 1);

    assert_eq!(f.npc.data().unit.hp, 900, "exactly one proc hit");
    assert_eq!(skill_fired(&mut f.client), 1, "one proc skill cast");
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn real_compact_item_proc_sources_resolve() {
    use std::collections::HashSet;

    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = crate::data::items::ItemData::load(&conn).unwrap();
    let skills: HashSet<u32> = conn
        .prepare("SELECT id FROM skills")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    let mut proc_ids: Vec<u32> = data
        .item_proc_bindings
        .values()
        .flatten()
        .copied()
        .collect();
    proc_ids.extend(
        data.holdables
            .values()
            .filter(|h| h.item_proc_id > 0)
            .map(|h| h.item_proc_id as u32),
    );
    proc_ids.extend(
        data.equip_item_sets
            .values()
            .flat_map(|s| s.bonuses.iter())
            .filter(|b| b.item_proc_id != 0)
            .map(|b| b.item_proc_id),
    );
    assert_eq!(
        data.item_proc_bindings
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        51
    );
    assert_eq!(
        data.holdables
            .values()
            .filter(|h| h.item_proc_id > 0)
            .count(),
        22
    );
    assert_eq!(
        data.equip_item_sets
            .values()
            .flat_map(|s| s.bonuses.iter())
            .filter(|b| b.item_proc_id != 0)
            .count(),
        37
    );
    for proc_id in proc_ids {
        let proc = data
            .item_proc(proc_id)
            .unwrap_or_else(|| panic!("proc {proc_id} has no item_procs row"));
        assert!(
            skills.contains(&proc.skill_id),
            "proc {proc_id} skill {} has no skills row",
            proc.skill_id
        );
    }
}
