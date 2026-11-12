//! `SphereDoodadInteract`: Sharpwind Mines' bridge-collapse sphere (1552) uses skill 11284 on
//! the Derelict Bridge doodad (5058) when a player crosses the sphere area, which advances the
//! bridge to its broken phase (13147 -> 13148). The area comes from `sphere_area_fill.json`;
//! the sphere id, skill and the `DoodadFuncAreaTrigger` marker are the shipped data rows.
//!
//! The fixtures select by `doodad_family_id` 0 (the area / skill-range rule the original
//! tests cover) or by a family id (the level design's `FamilyId`, which ignores the area and
//! range: spheres 1836-1847 reach the stalactites hanging above their centres and sphere 1552
//! reaches the bridge by family 50000 once its spawn row carries it).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::data::DbSphere;
use super::spheres::{load_sphere_areas, SphereArea};
use super::unit_reqs::{req_kind, UnitReq};
use crate::data::GameData;
use crate::models::doodad::Doodad;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::phase::get_func_group_id;
use crate::systems::npcs::doodad::spawner::{parse_list, DoodadSpawnerDef};
use crate::systems::npcs::doodad::templates::{
    DoodadFunc, DoodadFuncAreaTrigger, DoodadFuncGroups, DoodadTemplate, FuncTemplate,
};
use crate::systems::npcs::sphere_quest::SphereDoodadInteracts;
use crate::systems::skills::data::SkillTemplate;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{DoodadRef, Entity};

const SPHERE: u32 = 1552;
const DETAIL: u32 = 55;
const SKILL: u32 = 11284;
const BRIDGE_TEMPLATE: u32 = 5058;
const BRIDGE_PHASE: u32 = 13147;
const BROKEN_PHASE: u32 = 13148;
/// The shipped `sphere_doodad_interacts` family of sphere 1552 (the bridge).
const BRIDGE_FAMILY: u32 = 50000;
const STALACTITE_FAMILY: u32 = 50011;

fn data() -> GameData {
    data_with_range(4)
}

/// The shipped rows with skill 11284's `max_range` replaced by `max_range` (4 m in the data)
/// and the sphere's family selector left at 0 (the area / skill-range rule).
fn data_with_range(max_range: i32) -> GameData {
    data_with_family(0, max_range)
}

/// [`data_with_range`] with the shipped `sphere_doodad_interacts.doodad_family_id` set to
/// `family` (0 = the area / skill-range rule).
fn data_with_family(family: u32, max_range: i32) -> GameData {
    let mut data = GameData::empty();
    data.systems.quests.spheres.insert(
        SPHERE,
        DbSphere {
            id: SPHERE,
            enter_or_leave: true,
            sphere_detail_id: DETAIL,
            sphere_detail_type: "SphereDoodadInteract".into(),
            ..DbSphere::default()
        },
    );
    data.systems.npcs.sphere.sphere_doodad_interacts.add(
        DETAIL,
        SphereDoodadInteracts {
            id: DETAIL,
            skill_id: SKILL,
            doodad_family_id: family,
        },
    );
    data.systems.skills.skills.insert(
        SKILL,
        Arc::new(SkillTemplate {
            id: SKILL,
            max_range,
            ..SkillTemplate::default()
        }),
    );

    // Doodad 5058 phase 13147: one `DoodadFuncAreaTrigger` row leading to 13148 (the broken
    // rail model). This is the shipped `doodad_funcs` row 11162.
    let doodads = &mut data.systems.npcs.doodad;
    let start = DoodadFuncGroups {
        id: BRIDGE_PHASE,
        almighty: BRIDGE_TEMPLATE,
        group_kind_id: crate::systems::npcs::doodad::templates::doodad_func_group_kind::START,
        ..DoodadFuncGroups::default()
    };
    let broken = DoodadFuncGroups {
        id: BROKEN_PHASE,
        almighty: BRIDGE_TEMPLATE,
        group_kind_id: crate::systems::npcs::doodad::templates::doodad_func_group_kind::NORMAL,
        ..DoodadFuncGroups::default()
    };
    for group in [start.clone(), broken.clone()] {
        doodads
            .all_func_groups_index
            .insert(group.id, doodads.all_func_groups.len());
        doodads.all_func_groups.push(group.clone());
    }
    let mut template = DoodadTemplate {
        id: BRIDGE_TEMPLATE,
        ..DoodadTemplate::default()
    };
    template.func_groups.push(start);
    template.func_groups.push(broken);
    doodads.template_order.push(template.id);
    doodads.templates.insert(template.id, template);
    let trigger = FuncTemplate::AreaTrigger(DoodadFuncAreaTrigger {
        id: 73,
        npc_id: 0,
        is_enter: true,
    });
    doodads
        .func_templates
        .entry("DoodadFuncAreaTrigger")
        .or_default()
        .insert(73, trigger);
    let func = DoodadFunc {
        group_id: BRIDGE_PHASE,
        func_id: 73,
        func_key: 11162,
        func_type: "DoodadFuncAreaTrigger".into(),
        next_phase: BROKEN_PHASE as i32,
        ..Default::default()
    };
    doodads.funcs_by_id.insert(func.func_key, func.clone());
    doodads
        .funcs_by_groups
        .entry(BRIDGE_PHASE)
        .or_default()
        .push(func);
    data
}

fn bridge(server: &crate::GameServer) -> DoodadRef {
    bridge_at(server, 100.0)
}

/// The bridge doodad standing at `(x, 100, 100)`.
fn bridge_at(server: &crate::GameServer, x: f32) -> DoodadRef {
    // The shipped level placement's `FamilyId`; the family-less selection ignores it.
    doodad_at(server, 0x200, x, BRIDGE_FAMILY)
}

/// A doodad of the bridge template / start phase (`DoodadFuncAreaTrigger` -> broken) with the
/// given level-design `FamilyId`.
fn doodad_at(server: &crate::GameServer, obj_id: u32, x: f32, family_id: u32) -> DoodadRef {
    let doodad = Entity::new(obj_id, Doodad::new(BRIDGE_TEMPLATE));
    doodad.data().func_group_id = BRIDGE_PHASE;
    doodad.data().family_id = family_id;
    doodad.data().transform.set_xyz(x, 100.0, 100.0);
    let world = server.world.world(0).unwrap();
    world.spawn(&doodad, &server.view_ctx());
    doodad
}

/// Nothing pre-sets the next-phase flag, so this also covers the `DoodadFuncAreaTrigger` arm
/// (`funcs::use_func`): with the arm back at the C# stub the bridge would stay in 13147.
#[tokio::test]
async fn crossing_the_bridge_sphere_advances_the_bridge_to_its_broken_phase() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge(&server);
    assert_eq!(bridge.data().func_group_id, BRIDGE_PHASE);

    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BROKEN_PHASE,
        "the party crossed the bridge area: the bridge must give way"
    );
}

#[tokio::test]
async fn a_player_outside_the_sphere_does_not_collapse_the_bridge() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 200.0, 200.0),
        0x100,
    );
    let bridge = bridge(&server);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "nobody crossed the trigger area"
    );
}

/// The bridge doodad lies 1.9 m from the centre of its 5 m area (shipped data), so it is out of
/// the skill's 4 m range of the point where a character crosses into the area (4.5 m or more
/// along the line through the bridge). The interaction is tried again on every tick while the
/// character stays inside and fires when it steps onto the bridge; it settles once it has acted,
/// so a respawned bridge is not collapsed again while the character stays.
#[tokio::test]
async fn a_player_who_enters_out_of_range_collapses_the_bridge_when_stepping_onto_it() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    // Inside the 5 m area, 4.9 m from the bridge (skill range 4 m).
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 104.9, 100.0),
        0x100,
    );
    let bridge = bridge(&server);

    server.systems.quests.spheres.tick_at(&server, 0);
    server.systems.quests.spheres.tick_at(&server, 500);
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "entered 4.9 m from the bridge: out of the skill's range"
    );

    // Still inside the area, now 2 m from the bridge.
    player.data().unit.transform.set_xyz(102.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 1_000);
    assert_eq!(
        bridge.data().func_group_id,
        BROKEN_PHASE,
        "stepping onto the bridge inside the area collapses it without a new entry"
    );

    // Settled: the same stay does not collapse the bridge again after it respawned.
    bridge.data().func_group_id = BRIDGE_PHASE;
    server.systems.quests.spheres.tick_at(&server, 1_500);
    assert_eq!(bridge.data().func_group_id, BRIDGE_PHASE);
}

/// The sphere's own `unit_reqs` gate the dispatch like they gate a `SphereAcceptQuest` offer
/// (sphere 1161 "needs quest 2288", 501 / 513 / 514 ...): a character that misses them does not
/// trigger the area, one that meets them does on the next tick (a level-up while inside) or on
/// its next entry.
#[tokio::test]
async fn a_sphere_whose_requirements_the_player_misses_does_not_collapse_the_bridge() {
    let mut data = data();
    data.systems
        .quests
        .unit_reqs
        .by_owner
        .entry("Sphere".into())
        .or_default()
        .insert(
            SPHERE,
            vec![UnitReq {
                id: 1,
                owner_id: SPHERE,
                owner_type: "Sphere".into(),
                kind: req_kind::LEVEL,
                value1: 50,
                value2: 0,
            }],
        );
    let server = test_server_with(data);
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge(&server);

    // Level 10 < 50: the enter edge is refused, the bridge stands.
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "a character that misses the sphere's requirements must not trigger it"
    );

    // Level 50 while still inside: the next tick fires, no new entry needed.
    player.data().unit.level = 50;
    server.systems.quests.spheres.tick_at(&server, 1_000);
    assert_eq!(bridge.data().func_group_id, BROKEN_PHASE);
}

/// A character that meets the requirements only after leaving the area triggers it on its next
/// entry (the pending retry ends with the stay).
#[tokio::test]
async fn a_player_who_meets_the_requirements_outside_triggers_the_sphere_on_the_next_entry() {
    // A range wider than the area, so that only the pending retry could reach the bridge from
    // outside the area.
    let mut data = data_with_range(100);
    data.systems
        .quests
        .unit_reqs
        .by_owner
        .entry("Sphere".into())
        .or_default()
        .insert(
            SPHERE,
            vec![UnitReq {
                id: 1,
                owner_id: SPHERE,
                owner_type: "Sphere".into(),
                kind: req_kind::LEVEL,
                value1: 50,
                value2: 0,
            }],
        );
    let server = test_server_with(data);
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge(&server);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(bridge.data().func_group_id, BRIDGE_PHASE);

    // Leave (20 m from the bridge, outside the 5 m area), level up there, enter again.
    player.data().unit.transform.set_xyz(120.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 1_000);
    player.data().unit.level = 50;
    server.systems.quests.spheres.tick_at(&server, 1_500);
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "nothing is pending outside the area"
    );
    player.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 2_000);
    assert_eq!(bridge.data().func_group_id, BROKEN_PHASE);
}

/// The candidates come from the player's region neighbourhood, not from every doodad of the
/// instance: a doodad one region away (70 m) inside the area and the skill's range is reached.
#[tokio::test]
async fn a_bridge_in_a_neighbouring_region_is_reached_inside_the_skill_range() {
    let server = test_server_with(data_with_range(100));
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 200.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge_at(&server, 170.0);
    let world = server.world.world(0).unwrap();
    assert_ne!(
        world.region_by_pos(100.0, 100.0),
        world.region_by_pos(170.0, 100.0),
        "the bridge is in the next region"
    );

    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(bridge.data().func_group_id, BROKEN_PHASE);
}

/// The skill's `max_range` still bounds the candidates: a doodad inside the area but out of
/// range of the entering player is left alone.
#[tokio::test]
async fn a_bridge_inside_the_area_but_out_of_the_skill_range_stands() {
    let server = test_server_with(data_with_range(4));
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 200.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge_at(&server, 170.0);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(bridge.data().func_group_id, BRIDGE_PHASE);
}

/// A non-zero `doodad_family_id` selects every doodad of the instance whose `FamilyId` equals
/// it, regardless of the sphere area and the skill's range: the Sharpwind stalactites (families
/// 50011-50022) hang 13-16 m above their sphere centres, outside both.
#[tokio::test]
async fn a_family_sphere_reaches_a_doodad_far_outside_the_area_and_range() {
    let server = test_server_with(data_with_family(BRIDGE_FAMILY, 4));
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    // 70 m away: outside the 5 m area and the 4 m skill range.
    let bridge = bridge_at(&server, 170.0);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BROKEN_PHASE,
        "the family selector ignores the area and the skill range"
    );
}

/// The family selector is exact: of two reacting doodads inside the same area and range, only
/// the one whose `FamilyId` matches is advanced.
#[tokio::test]
async fn a_family_sphere_selects_only_doodads_of_its_family() {
    let server = test_server_with(data_with_family(STALACTITE_FAMILY, 4));
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge_at(&server, 100.0);
    let stalactite = doodad_at(&server, 0x202, 101.0, STALACTITE_FAMILY);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        stalactite.data().func_group_id,
        BROKEN_PHASE,
        "the doodad of the selected family is advanced"
    );
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "family {} does not select the bridge (family {BRIDGE_FAMILY}), even inside the area and \
         the skill range",
        STALACTITE_FAMILY
    );
}

/// Family 0 keeps the old area + skill-range selection: the same distant doodad is left alone
/// (the family tests above prove the two paths differ).
#[tokio::test]
async fn a_family_less_sphere_keeps_the_area_and_range_selection() {
    let server = test_server_with(data_with_range(100));
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge_at(&server, 170.0);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "family 0 still requires the doodad to lie inside the sphere area"
    );
}

/// A doodad busy on another thread (or already deleted) is skipped instead of being advanced
/// without the phase reservation every other phase driver takes.
#[tokio::test]
async fn a_bridge_reserved_by_another_thread_is_not_advanced() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge(&server);
    // Another thread holds the doodad's phase operation.
    bridge.data().phase.operation_thread = Some(
        std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap(),
    );
    bridge.data().phase.operation_depth = 1;

    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "a busy doodad is skipped"
    );
    assert!(
        !bridge.data().phase.to_next_phase,
        "the skipped doodad keeps no stale next-phase flag"
    );
}

/// Every (sphere, doodad template) pair the generic `SphereDoodadInteract` dispatch can fire for
/// with the shipped data. A `sphere_doodad_interacts.doodad_family_id` other than 0 selects all
/// doodads of the world whose spawn row carries that `FamilyId`, anywhere in the instance; family
/// 0 keeps the area rule: the doodads of the shipped `doodad_spawns*.json` files that lie inside
/// a shipped `sphere_area_fill.json` area of the sphere (the skill range narrows it further at
/// run time). The doodad's start phase must hold an enter `DoodadFuncAreaTrigger` row with a
/// next phase either way. With the r208022 data and the placement data of this checkout the
/// dispatch selects the Sharpwind bridge 5058 (sphere 1552, family 50000) and the twelve
/// stalactites: 5365 under spheres 1836-1843 (families 50011-50018) and 5364 under spheres
/// 1844-1847 (families 50019-50022). A new pair is a new content effect that `PORTING.md`
/// ("Sharpwind opening chain") has to list.
///
/// `AAEMU_COMPACT_DB` must name a COPY of the r208022 `compact.sqlite3` (never the client
/// install): `cargo test -p aaemu-game --lib real_data_sphere_doodad_interact -- --ignored
/// --nocapture`.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (a copy of the r208022 compact.sqlite3)"]
fn real_data_sphere_doodad_interact_selection_matches_the_shipped_placements() {
    let Some(path) = std::env::var("AAEMU_COMPACT_DB").ok() else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let doodads = DoodadData::load_from(&conn).expect("doodad tables");

    // sphere id -> (authored skill id, doodad family) of every `SphereDoodadInteract` sphere
    // (skill 0 = no dispatch).
    let mut select_of_sphere: HashMap<u32, (u32, u32)> = HashMap::new();
    let mut stmt = conn
        .prepare(
            "SELECT s.id, d.skill_id, d.doodad_family_id FROM spheres s \
             JOIN sphere_doodad_interacts d ON d.id = s.sphere_detail_id \
             WHERE s.sphere_detail_type = 'SphereDoodadInteract'",
        )
        .expect("spheres / sphere_doodad_interacts");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, Option<u32>>(1)?.unwrap_or(0),
                r.get::<_, Option<u32>>(2)?.unwrap_or(0),
            ))
        })
        .expect("rows");
    for row in rows {
        let (sphere, skill, family) = row.expect("row");
        select_of_sphere.insert(sphere, (skill, family));
    }
    assert!(!select_of_sphere.is_empty());

    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let areas: Vec<SphereArea> = load_sphere_areas(&app, |id| select_of_sphere.contains_key(&id))
        .into_iter()
        .filter(|a| select_of_sphere[&a.sphere_id].0 != 0)
        .collect();
    assert!(!areas.is_empty(), "the shipped areas of the doodad spheres");

    // Distinct placements (sphere, template, position bits): a pile inside two overlapping areas
    // of one sphere counts once.
    let mut reached: BTreeSet<(u32, u32, [u32; 3])> = BTreeSet::new();
    let worlds: BTreeSet<&str> = areas.iter().map(|a| a.world.as_str()).collect();
    for world in worlds {
        let dir = app.join("Data").join("Worlds").join(world);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.retain(|p| {
            p.file_name().is_some_and(|n| {
                let n = n.to_string_lossy().to_ascii_lowercase();
                n.starts_with("doodad_spawns") && n.ends_with(".json")
            })
        });
        files.sort();
        let mut seen: HashSet<(u32, [u32; 3])> = HashSet::new();
        for file in files {
            let text = std::fs::read_to_string(&file).expect("spawn file");
            let defs = parse_list(&text, DoodadSpawnerDef::from_json).expect("doodad spawns");
            for def in defs {
                let (id, p) = (def.base.unit_id, def.base.position);
                let key = [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()];
                if !seen.insert((id, key)) {
                    continue; // the spawn loader drops a repeated (template, position)
                }
                let group = get_func_group_id(&doodads, id);
                let fires = doodads.funcs_for_group(group).iter().any(|f| {
                    f.func_type == "DoodadFuncAreaTrigger"
                        && f.next_phase > 0
                        && matches!(
                            doodads.func_template(f.func_id, &f.func_type),
                            Some(FuncTemplate::AreaTrigger(t)) if t.is_enter
                        )
                });
                if !fires {
                    continue;
                }
                for (sphere, (_, family)) in &select_of_sphere {
                    // Only a sphere with an area in this world can dispatch here.
                    let in_world: Vec<&SphereArea> = areas
                        .iter()
                        .filter(|a| a.world == world && a.sphere_id == *sphere)
                        .collect();
                    if in_world.is_empty() {
                        continue;
                    }
                    let selected = if *family != 0 {
                        def.family_id == *family
                    } else {
                        in_world.iter().any(|a| a.contains((p.x, p.y, p.z)))
                    };
                    if selected {
                        reached.insert((*sphere, id, key));
                    }
                }
            }
        }
    }
    let mut counts: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for (sphere, id, _) in &reached {
        *counts.entry((*sphere, *id)).or_default() += 1;
    }
    println!("(sphere, doodad template) -> selected placements: {counts:?}");
    let expected: BTreeMap<(u32, u32), usize> = BTreeMap::from([
        ((1552, 5058), 1),
        ((1836, 5365), 1),
        ((1837, 5365), 1),
        ((1838, 5365), 1),
        ((1839, 5365), 1),
        ((1840, 5365), 1),
        ((1841, 5365), 1),
        ((1842, 5365), 1),
        ((1843, 5365), 1),
        ((1844, 5364), 1),
        ((1845, 5364), 1),
        ((1846, 5364), 1),
        ((1847, 5364), 1),
    ]);
    assert_eq!(
        counts, expected,
        "the doodads the SphereDoodadInteract dispatch selects changed (a new placement with a \
         FamilyId, or the selection rule): update this expectation and PORTING.md"
    );
}
