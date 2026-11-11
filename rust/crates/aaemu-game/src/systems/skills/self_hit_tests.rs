//! Area target selection never takes a unit the template's relation excludes.
//!
//! Owner report (Berserk Red Dragon Roar staff, item 28905 / use skill 27005): a ground-target
//! meteor (`target_type_id` 6 Pos, `target_selection_id` 2 Target, `target_relation_id` 4
//! Hostile, `target_area_radius` 3) tripped (buff 12883) its own caster. `apply_effects` mirrors
//! the C# "Add origin in case of no targets and using a target position cast" fallback and pushed
//! the caster into the target list without a relation check; the effect carries both
//! `friendly` and `non_friendly`, so neither per-target effect gate skips it there.

use std::sync::Arc;

use crate::data::items::ItemTemplate;
use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::systems::economy::inventory::Inv;
use crate::systems::skills::data::effects::{BuffEffect, HealEffect};
use crate::systems::skills::data::{BuffTemplate, EffectKind, EffectTemplate, SkillTemplate};
use crate::systems::skills::state::SkillRun;
use crate::systems::skills::types::{
    RelationState, SkillCastTarget, SkillCaster, SkillObject, SkillTargetRelation, SkillTargetType,
};
use crate::systems::skills::{handlers, relation, skill, tests};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, PlayerRef, WorldObject};

/// The reported skill (meteor of the Berserk Red Dragon Roar staff, item 28905).
const SKILL: u32 = 27_005;
const ITEM: u32 = 28_905;
/// A stand-in for the trip buff (12883): a debuff applied by the skill's BuffEffect.
const TRIP_BUFF: u32 = 50_883;
const HEAL_EFFECT: u32 = 900_301;
const BUFF_EFFECT: u32 = 900_302;
const NPC_OBJ: u32 = 0x401;
const PLAYER_FACTION: u32 = 109;
const HOSTILE_NPC_FACTION: u32 = 115;

fn faction(id: u32, mother: u32) -> SystemFaction {
    SystemFaction {
        id,
        mother_id: mother,
        ..SystemFaction::default()
    }
}

/// The player's Harani faction (109, mother 149) and the hostile kobold faction (115).
fn game_data(relation: SkillTargetRelation) -> GameData {
    let mut data = GameData::empty();
    for f in [
        faction(2, 0),
        faction(109, 149),
        faction(149, 0),
        faction(HOSTILE_NPC_FACTION, 0),
    ] {
        data.factions.add(f);
    }
    data.factions.relations.push(FactionRelation {
        id: 149,
        id2: HOSTILE_NPC_FACTION,
        state: RelationState::Hostile as u8,
        exp_time: 0,
    });
    let skills = &mut data.systems.skills;
    skills.add_buff(BuffTemplate {
        id: TRIP_BUFF,
        kind: 2, // BuffKind.Bad
        duration: 10_000,
        stack_rule: 1,
        ..Default::default()
    });
    skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: 1,
            buff_id: Some(TRIP_BUFF),
            chance: 100,
            stack: 0,
            ab_level: 0,
        }),
        BUFF_EFFECT,
    );
    skills.add_effect(
        EffectKind::HealEffect,
        EffectTemplate::Heal(HealEffect {
            id: 2,
            use_fixed_heal: true,
            fixed_min: 150,
            fixed_max: 150,
            use_level_heal: false,
            level_md: 0.0,
            level_va_start: 0,
            level_va_end: 0,
            percent: false,
            use_charged_buff: false,
            charged_buff_id: 0,
            charged_mul: 0.0,
            slave_applicable: false,
            ignore_heal_aggro: false,
            dps_multiplier: 0.0,
            actability_group_id: 0,
            actability_step: 0,
            actability_mul: 0.0,
            actability_add: 0.0,
        }),
        HEAL_EFFECT,
    );
    let mut template = SkillTemplate {
        id: SKILL,
        target_type: SkillTargetType::Pos as i32,
        target_relation: relation as i32,
        target_selection: 2, // Target
        target_area_radius: 3,
        max_range: 30,
        damage_type_id: 2, // Magic
        level_rule_no_consideration: true,
        ..Default::default()
    };
    let mut buff_effect = tests::skill_effect(BUFF_EFFECT, EffectKind::BuffEffect, 1);
    buff_effect.friendly = true;
    buff_effect.non_friendly = true;
    buff_effect.always_hit = true;
    template.effects.push(buff_effect);
    skills.add_skill(template);
    data
}

fn player(server: &crate::GameServer, client: &TestClient, zone: u32) -> PlayerRef {
    let mut c = test_character(1, "Caster", 100.0, 100.0);
    c.unit.faction_id = PLAYER_FACTION;
    c.unit.hp = 5_000;
    c.unit.max_hp = 5_000;
    c.unit.mp = 1_000;
    c.unit.max_mp = 1_000;
    let p = spawn_player(server, client, c, 0x100);
    p.data().unit.transform.zone_id = zone;
    p
}

fn spawn_npc(server: &crate::GameServer) -> Arc<Entity<Npc>> {
    let mut npc = Npc::new(900);
    npc.unit.faction_id = HOSTILE_NPC_FACTION;
    npc.unit.hp = 5_000;
    npc.unit.max_hp = 5_000;
    npc.unit.transform.set_xyz(101.0, 100.0, 100.0);
    let npc = Entity::new(NPC_OBJ, npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    npc
}

fn position() -> SkillCastTarget {
    position_at(100.0, 100.0)
}

fn position_at(x: f32, y: f32) -> SkillCastTarget {
    SkillCastTarget::Position {
        obj_id: 0,
        x,
        y,
        z: 100.0,
        rot: 0.0,
        obj_id1: 0,
        obj_id2: 0,
    }
}

fn run(server: &crate::GameServer, caster: &PlayerRef, target: &mut SkillCastTarget) {
    let template = server
        .game_data
        .systems
        .skills
        .skill(SKILL)
        .expect("fixture skill")
        .clone();
    let (result, _) = skill::use_skill(
        server,
        &SkillRun::new(template, 1),
        &WorldObject::from(caster),
        SkillCaster::Unit(caster.obj_id),
        target,
        Some(SkillObject::None),
        true,
    );
    assert_eq!(result, 0, "the fixture cast is accepted");
}

/// The reported cast: a HOSTILE area at a position with no unit in it must not take the caster.
#[tokio::test]
async fn a_hostile_position_area_does_not_trip_its_caster() {
    let server = Arc::new(test_server_with(game_data(SkillTargetRelation::Hostile)));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let caster = player(&server, &client, crate::test_support::TEST_ZONE);

    run(&server, &caster, &mut position());

    assert!(
        !caster.data().unit.buffs.check_buff(TRIP_BUFF),
        "a Hostile area does not target its own caster"
    );
}

/// A hostile NPC in the radius still takes the effect; the caster beside it does not.
#[tokio::test]
async fn a_hostile_position_area_trips_the_npc_and_not_the_caster() {
    let server = Arc::new(test_server_with(game_data(SkillTargetRelation::Hostile)));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let caster = player(&server, &client, crate::test_support::TEST_ZONE);
    let npc = spawn_npc(&server);

    run(&server, &caster, &mut position());

    assert!(
        npc.data().unit.buffs.check_buff(TRIP_BUFF),
        "the hostile NPC in the radius is hit"
    );
    assert!(
        !caster.data().unit.buffs.check_buff(TRIP_BUFF),
        "the caster standing in the same radius is not"
    );
}

/// The item-cast path of the report (`SkillCaster::Item`, `CSStartSkill`) is the same rule.
#[tokio::test]
async fn an_item_cast_of_the_hostile_area_does_not_trip_its_caster() {
    let mut data = game_data(SkillTargetRelation::Hostile);
    data.items.templates.insert(
        ITEM,
        ItemTemplate {
            use_skill_id: SKILL,
            ..ItemTemplate::plain(ITEM, 1)
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let caster = player(&server, &client, crate::test_support::TEST_ZONE);
    client.conn.active_char = Some(caster.clone());
    server.items.add_container(Inv::of(&caster).bag());
    let item = server
        .items
        .create(ITEM, 1, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .expect("fixture item");
    let item_id = item.id;
    Inv::of(&caster)
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, -1, &server.game_data.items)
        .expect("bag space");
    client.drain();
    handlers::start_skill(
        &server,
        &mut client.conn,
        handlers::CSStartSkillPacket {
            skill_id: SKILL,
            caster: SkillCaster::Item {
                obj_id: caster.obj_id,
                item_id,
                item_template_id: ITEM,
                type1: 0,
                type2: 0,
            },
            target: position(),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );

    assert!(
        caster.data().unit.skill_last_used_ms > 0,
        "the item cast ran"
    );
    assert!(
        !caster.data().unit.buffs.check_buff(TRIP_BUFF),
        "the item cast of a Hostile area does not trip its own caster"
    );
}

/// A FRIENDLY position area with no unit in the radius still reaches the caster: the C#
/// fallback ("add origin ...") is kept for the relations that admit the caster
/// (`IsRelationValid`: Friendly, Any, Party, Raid), even when the cast lands away from it.
#[tokio::test]
async fn a_friendly_position_area_still_heals_its_caster() {
    let mut data = game_data(SkillTargetRelation::Friendly);
    let mut template = (**data.systems.skills.skill(SKILL).unwrap()).clone();
    template.effects.clear();
    let mut heal = tests::skill_effect(HEAL_EFFECT, EffectKind::HealEffect, 2);
    heal.friendly = true;
    heal.non_friendly = false;
    heal.always_hit = true;
    template.effects.push(heal);
    data.systems.skills.add_skill(template);
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let caster = player(&server, &client, crate::test_support::TEST_ZONE);
    caster.data().unit.hp = 1_000;

    run(&server, &caster, &mut position_at(120.0, 100.0));

    assert!(
        caster.data().unit.hp > 1_000,
        "a Friendly area keeps the caster fallback and heals it"
    );
}

/// The pure selection decision behind the fix: the fallback caster is only admissible for the
/// relations that include it.
#[tokio::test]
async fn relation_validity_admits_the_caster_only_for_the_relations_that_include_it() {
    let server = Arc::new(test_server_with(game_data(SkillTargetRelation::Hostile)));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let caster = player(&server, &client, crate::test_support::TEST_ZONE);
    let caster_t = crate::systems::skills::geometry::Target::Obj(WorldObject::from(&caster));
    assert!(!relation::is_relation_valid(
        &server,
        SkillTargetRelation::Hostile,
        &caster_t,
        &caster_t
    ));
    assert!(!relation::is_relation_valid(
        &server,
        SkillTargetRelation::Others,
        &caster_t,
        &caster_t
    ));
    assert!(relation::is_relation_valid(
        &server,
        SkillTargetRelation::Friendly,
        &caster_t,
        &caster_t
    ));
    assert!(relation::is_relation_valid(
        &server,
        SkillTargetRelation::Any,
        &caster_t,
        &caster_t
    ));
}

/// Pins the real data the report and the fix rest on: item 28905's use skill 27005 is a
/// no-plot Hostile position area with two both-flag effects (the DamageEffect and the Trip
/// BuffEffect), the tooltip says "all enemies", and 76 no-plot Hostile position skills share
/// the shape. Run with `AAEMU_COMPACT_DB=... cargo test -p aaemu-game -- --ignored
/// real_compact_db_meteor_staff`.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_db_meteor_staff_is_a_hostile_area_for_enemies_only() {
    use rusqlite::{Connection, OpenFlags};

    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open compact.sqlite3");
    let use_skill: u32 = conn
        .query_row("SELECT use_skill_id FROM items WHERE id = 28905", [], |r| {
            r.get(0)
        })
        .expect("item 28905");
    assert_eq!(use_skill, SKILL, "the staff uses skill 27005");

    let (target_type, selection, relation, radius, count, plot): (
        i64,
        i64,
        i64,
        i64,
        i64,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT target_type_id, target_selection_id, target_relation_id,
                    target_area_radius, target_area_count, plot_id
             FROM skills WHERE id = 27005",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("skill 27005");
    assert_eq!(
        (target_type, selection, relation, radius, count),
        (6, 2, 4, 3, 20)
    );
    assert!(plot.is_none(), "no plot");

    // Both effects are applied to the area's targets and carry both flags, so neither
    // per-target relation gate skips the caster once the fallback put it in the list.
    let effects: Vec<(i64, String, String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT e.id, a.actual_type, e.friendly, e.non_friendly
                 FROM skill_effects e JOIN effects a ON a.id = e.effect_id
                 WHERE e.skill_id = 27005",
            )
            .expect("skill_effects query");
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("effect rows");
        rows.map(|r| r.expect("effect row")).collect()
    };
    assert_eq!(effects.len(), 2);
    for (effect, kind, friendly, non_friendly) in &effects {
        assert!(
            *effect == 31666 || *effect == 31667,
            "the report's two effects"
        );
        assert!(kind == "DamageEffect" || kind == "BuffEffect", "{kind}");
        assert_eq!((friendly.as_str(), non_friendly.as_str()), ("t", "t"));
    }

    let desc: String = conn
        .query_row(
            "SELECT en_us FROM localized_texts
             WHERE tbl_name = 'skills' AND tbl_column_name = 'desc' AND idx = 27005",
            [],
            |r| r.get(0),
        )
        .expect("skill 27005 tooltip");
    assert!(
        desc.contains("all enemies"),
        "the tooltip names enemies only: {desc}"
    );

    let pattern: i64 = conn
        .query_row(
            "SELECT count(DISTINCT s.id) FROM skills s
             JOIN skill_effects e ON e.skill_id = s.id
             WHERE s.target_relation_id = 4 AND s.target_area_radius > 0
               AND (s.plot_id IS NULL OR s.plot_id = 0)
               AND s.target_type_id IN (6, 11, 13, 14, 15)
               AND e.friendly = 't' AND e.non_friendly = 't'",
            [],
            |r| r.get(0),
        )
        .expect("pattern count");
    assert_eq!(
        pattern, 76,
        "no-plot Hostile position skills with both-flag effects"
    );
}
