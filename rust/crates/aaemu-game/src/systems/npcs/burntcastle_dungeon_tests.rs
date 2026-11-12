//! Burnt Castle Armory (`instance_burntcastle_armory`, zone 236, zone group 45) scripted data.
//!
//! The 32 event spawners (the Hieronimus's Dominated waves 46629-46656, the intro / 2nd-boss-end
//! / 3rd-boss souls 65999, 66025/66026, the 3rd-boss cutscene actors 66033/66037-66052) and the
//! ten invisible trigger NPCs of the 13 `tagged_npcs` groups are loadable placements. The gates
//! 3612/3613/4417 and the exits 3673/4923 are `DoodadFuncSkillHit` / `DoodadFuncUse` doodads.
//! Placements are the 10.x level editor's (**Inferred** for the 1.2 server, see `PORTING.md`);
//! templates, spawner ids, skills and tags are the 1.2 `compact.sqlite3`.
//!
//! The event spawners are *not* placed at instance start (`activation_state = 'f'` / `연출`), so
//! a plain `npc_spawns.json` row for the same NPC would either never spawn or (at the same
//! position) shadow the event row through the loader's duplicate filter. The plain 8962 row
//! the 2026 spawn import added at the 3rd boss's event-spawner position is therefore gone;
//! the boss appears through the 66033 event row, as in the level.
//!
//! The real-data tests read `AAEMU_COMPACT_DB` like the Sharpwind suites:
//! `$env:AAEMU_COMPACT_DB = '...\compact.sqlite3'; cargo test -p aaemu-game burntcastle -- --ignored`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::WorldTemplate;
use crate::data::GameData;
use crate::server::GameServer;
use crate::systems::npcs::doodad::spawner::{parse_list, DoodadSpawnerDef, NpcSpawnerDef};
use crate::systems::npcs::doodad::{phase, runtime::DoodadCtx};
use crate::systems::npcs::spawn_manager::{LoadStats, TemplateExists};
use crate::systems::quests::spheres::{load_sphere_areas, SphereArea};
use crate::systems::DataLoadCtx;
use crate::test_support::{spawn_player, test_character, test_server_with_world, TestClient};
use crate::world::{DoodadRef, NpcRef, PlayerRef, WorldInstance, WorldObject};

const WORLD: &str = "instance_burntcastle_armory";
const ZONE: u32 = 236;
const INSTANCE_ID: u32 = 0;

/// `(spawner id, unit id)` of every event row the dungeon's skills spawn.
const EVENT_SPAWNERS: &[(u32, u32)] = &[
    (46629, 8968),
    (46631, 8969),
    (46632, 8969),
    (46635, 8969),
    (46636, 8968),
    (46637, 8965),
    (46638, 8964),
    (46639, 8966),
    (46640, 8964),
    (46641, 8965),
    (46642, 8965),
    (46644, 8966),
    (46645, 8968),
    (46646, 8967),
    (46647, 8968),
    (46648, 8964),
    (46649, 8967),
    (46650, 8968),
    (46651, 8964),
    (46653, 8965),
    (46654, 8969),
    (46656, 8966),
    (65999, 10473),
    (66025, 10503),
    (66026, 10503),
    (66033, 8962),
    (66037, 10534),
    (66039, 10534),
    (66044, 10534),
    (66045, 10534),
    (66046, 10534),
    (66052, 10534),
];

/// The trigger NPCs of the 13 `tagged_npcs` groups that are *not* event-spawned: tags 535,
/// 548, 549, 552, 554, 561, 607, 532, 540, 542. Tags 528/539/553 target the event-spawned
/// souls 10473/10503, so they must not be plain rows.
const TRIGGER_NPCS: &[(u32, f32, f32, f32)] = &[
    (10452, 186.030, 98.926, 20.538),
    (10455, 185.923, 100.765, 20.538),
    (10474, 197.959, 101.286, 20.538),
    (10499, 241.435, 312.625, 20.538),
    (10528, 231.119, 320.401, 20.853),
    (10571, 174.430, 186.661, 20.538),
    (10573, 288.093, 248.450, 20.538),
    (10574, 231.196, 311.845, 20.858),
    (10575, 258.364, 279.845, 20.538),
    (10815, 210.269, 304.687, 20.520),
];

/// The five static doodads: `(template, start phase, x, y, z)`.
const DOODADS: &[(u32, u32, f32, f32, f32)] = &[
    (3612, 8670, 246.225, 161.039, 20.006),
    (3613, 8673, 246.421, 312.587, 20.250),
    (3673, 8794, 165.587, 99.906, 20.858),
    (4417, 10833, 200.834, 120.954, 20.060),
    (4923, 12256, 173.024, 312.007, 20.520),
];

fn app_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("app")
}

fn world_path() -> PathBuf {
    app_path().join("Data/Worlds").join(WORLD)
}

fn npc_spawn_defs() -> Vec<NpcSpawnerDef> {
    let text =
        std::fs::read_to_string(world_path().join("npc_spawns.json")).expect("npc_spawns.json");
    parse_list(&text, NpcSpawnerDef::from_json).expect("npc_spawns.json parses")
}

fn doodad_spawn_defs() -> Vec<DoodadSpawnerDef> {
    let text = std::fs::read_to_string(world_path().join("doodad_spawns.json"))
        .expect("doodad_spawns.json");
    parse_list(&text, DoodadSpawnerDef::from_json).expect("doodad_spawns.json parses")
}

fn is_event(def: &NpcSpawnerDef) -> bool {
    !def.npc_spawner_ids.is_empty() && def.npc_spawner_ids != [0]
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

/// Every scripted placement is present exactly once, the event rows are event rows (plain rows
/// for the same NPC would be skipped or shadow the event row), the tagged trigger NPCs are
/// plain rows, and the level doodads are the only added doodads (the secret chests belong to
/// the quest-spawns lane, skill-summoned orbs are never placed statically).
#[test]
fn burnt_scripted_rows_are_placed_once() {
    let defs = npc_spawn_defs();
    let events: Vec<&NpcSpawnerDef> = defs.iter().filter(|d| is_event(d)).collect();
    let plain: Vec<&NpcSpawnerDef> = defs.iter().filter(|d| !is_event(d)).collect();

    assert_eq!(events.len(), EVENT_SPAWNERS.len(), "32 event rows");
    for (spawner, unit) in EVENT_SPAWNERS {
        let matching: Vec<&&NpcSpawnerDef> = events
            .iter()
            .filter(|d| d.npc_spawner_ids == [*spawner])
            .collect();
        assert_eq!(matching.len(), 1, "event spawner {spawner} once");
        assert_eq!(matching[0].base.unit_id, *unit, "spawner {spawner} unit");
    }
    assert!(
        !events.iter().any(|d| d.npc_spawner_ids.contains(&65528)),
        "65528 is not in the level and has no placement"
    );

    for (unit, x, y, z) in TRIGGER_NPCS {
        let matching: Vec<&&NpcSpawnerDef> =
            plain.iter().filter(|d| d.base.unit_id == *unit).collect();
        assert_eq!(matching.len(), 1, "trigger NPC {unit} once");
        let p = &matching[0].base.position;
        assert!(
            close(p.x, *x) && close(p.y, *y) && close(p.z, *z),
            "trigger NPC {unit} at the level position, got ({}, {}, {})",
            p.x,
            p.y,
            p.z
        );
    }
    // The souls are event-spawned; a plain row would duplicate/skip the event placement.
    for unit in [10473, 10503, 8962] {
        assert!(
            !plain.iter().any(|d| d.base.unit_id == unit),
            "NPC {unit} must not have a plain start row"
        );
    }
    // The 1.2-era rows stay (only the 2026 spawn-import 8962 row was replaced by the script).
    for unit in [8960, 12190, 10518, 502, 8839, 3086] {
        assert!(
            plain.iter().any(|d| d.base.unit_id == unit),
            "the 1.2-era row for NPC {unit} is kept"
        );
    }

    // The loader's duplicate filter (`stored` only holds plain rows that added spawners) is
    // < 2 m on all axes within one unit id: no plain row may be silently dropped. Event rows
    // are exempt, and the Dominated wave rows 46641/46642 intentionally stand 1 m apart.
    for (i, a) in plain.iter().enumerate() {
        for b in plain.iter().skip(i + 1) {
            if a.base.unit_id != b.base.unit_id {
                continue;
            }
            let (pa, pb) = (&a.base.position, &b.base.position);
            let same =
                (pa.x - pb.x).abs() < 2.0 && (pa.y - pb.y).abs() < 2.0 && (pa.z - pb.z).abs() < 2.0;
            assert!(!same, "duplicate plain NPC {} rows", a.base.unit_id);
        }
    }

    let doodads = doodad_spawn_defs();
    for (template, _, x, y, z) in DOODADS {
        let matching: Vec<&DoodadSpawnerDef> = doodads
            .iter()
            .filter(|d| d.base.unit_id == *template)
            .collect();
        assert_eq!(matching.len(), 1, "doodad {template} once");
        let p = &matching[0].base.position;
        assert!(
            close(p.x, *x) && close(p.y, *y) && close(p.z, *z),
            "doodad {template} at the level position, got ({}, {}, {})",
            p.x,
            p.y,
            p.z
        );
    }
    // The Sharpwind exit the old file stood in for the missing entrance exit is gone; the real
    // Burnt Castle Armory exit 3673 is at the same doorway.
    assert!(
        !doodads.iter().any(|d| d.base.unit_id == 5723),
        "the Sharpwind Mines exit 5723 must not stand in Burnt Castle Armory"
    );
    // The two Hieronimus's Secret Chests come from the quest-spawns rows (quest_spawn_tests);
    // this lane adds no third one.
    assert_eq!(
        doodads.iter().filter(|d| d.base.unit_id == 7659).count(),
        2,
        "7659 is placed only by the two quest-spawns rows"
    );
    // Summoned by the death / cutscene InteractionEffects (3499, 3657, 3602, 3611, 4416,
    // 4495, 4874); never a static placement.
    for template in [3499, 3602, 3611, 3657, 4416, 4495, 4874] {
        assert!(
            !doodads.iter().any(|d| d.base.unit_id == template),
            "skill-summoned doodad {template} must not be placed"
        );
    }
}

/// A server with the real skills / npcs / quests / AI tables and a synthetic Burnt world.
fn real_server(db: &str) -> (Arc<GameServer>, Arc<WorldInstance>) {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app_path = app_path();
    let files = ClientFileManager::from_sources(&[], &app_path);
    let ctx = DataLoadCtx {
        conn: &conn,
        app_path: &app_path,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).expect("formulas");
    data.systems.skills = crate::systems::skills::Data::load(&ctx).expect("skills data");
    data.systems.npcs = crate::systems::npcs::Data::load(&ctx).expect("npcs data");
    data.systems.quests = crate::systems::quests::Data::load(&ctx).expect("quests data");
    data.systems.ai = crate::systems::ai::Data::load(&ctx).expect("ai data");
    let template = WorldTemplate::synthetic(0, WORLD, 32, 32, ZONE);
    let server = Arc::new(test_server_with_world(data, template));
    server.set_self();
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(WORLD)
        .expect("Burnt world template")
        .clone();
    let world = server
        .world
        .create_world_instance(Arc::new(template), INSTANCE_ID);
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::ai::hooks::install(&server);
    (server, world)
}

/// `load_spawn_files` for the shipped Burnt world directory.
fn load_burnt(server: &Arc<GameServer>) -> LoadStats {
    let gd = &server.game_data;
    let npc = |id: u32| gd.systems.npcs.npc.exist(id);
    let doodad = |id: u32| gd.systems.npcs.doodad.exist(id);
    let transfer = |id: u32| {
        gd.systems
            .npcs
            .npc_spawners
            .transfer_template_ids
            .contains(&id)
    };
    let gimmick = |id: u32| gd.systems.npcs.gimmick.exist(id);
    let slave = |id: u32| {
        gd.systems
            .npcs
            .npc_spawners
            .slave_template_ids
            .contains(&id)
    };
    let exists = TemplateExists {
        npc: &npc,
        doodad: &doodad,
        transfer: &transfer,
        gimmick: &gimmick,
        slave: &slave,
    };
    let world = server.world.world(INSTANCE_ID).expect("Burnt instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.load_spawn_files(server, &world_path(), &exists)
        .expect("Burnt spawn files load")
}

fn npcs_of(world: &Arc<WorldInstance>, template_id: u32) -> Vec<NpcRef> {
    world
        .all_npcs()
        .into_iter()
        .filter(|n| n.data().unit.template_id == template_id)
        .collect()
}

/// Finds the plain (spawn-file) spawner of `unit` and force-spawns the NPC at the authored
/// placement (`GetNpcSpawner` only answers event spawners, so this walks the ordinary table).
fn force_spawn_unit(server: &Arc<GameServer>, unit: u32) -> NpcRef {
    let world = server.world.world(INSTANCE_ID).expect("Burnt instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    let spawner = m
        .get_all_spawners()
        .into_values()
        .flatten()
        .find(|s| s.unit_id == unit && s.is_placement())
        .unwrap_or_else(|| panic!("no plain spawner for NPC {unit}"));
    spawner
        .force_spawn(server, 0)
        .unwrap_or_else(|| panic!("spawner of NPC {unit} did not spawn"))
}

fn spawned_count(server: &Arc<GameServer>, spawner_id: u32) -> i32 {
    let world = server.world.world(INSTANCE_ID).expect("Burnt instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.get_npc_spawner(spawner_id)
        .first()
        .map_or(0, |s| s.current_spawn_count())
}

/// The shipped files register all 32 event spawners (the activating skills' effect lookup
/// names their ids), the ten tagged trigger NPCs spawn at their level placements, and the
/// event-only actors (the souls and the killable Prince Hieronimus) are absent at start.
///
/// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game --lib burntcastle -- --ignored`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_burnt_event_spawners_and_trigger_npcs_resolve() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let stats = load_burnt(&server);
    eprintln!("Burnt load: {stats:?}");
    assert_eq!(stats.npc_missing_templates, 0, "{stats:?}");
    assert_eq!(stats.doodad_missing_templates, 0, "{stats:?}");
    assert_eq!(stats.npc_event_spawners, 32, "{stats:?}");

    // Event-only actors must not stand at start: the 3rd boss appears through 66033.
    for unit in [8962, 10473, 10503, 10534] {
        assert!(
            npcs_of(&world, unit).is_empty(),
            "event-only NPC {unit} must not be placed at instance start"
        );
    }

    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    for (spawner, unit) in EVENT_SPAWNERS {
        let found = m.get_npc_spawner(*spawner);
        assert_eq!(found.len(), 1, "event spawner {spawner}");
        let npc = found[0].force_spawn(&server, 0).expect("event spawn");
        assert_eq!(
            npc.data().unit.template_id,
            *unit,
            "event spawner {spawner} unit"
        );
    }

    for (unit, x, y, z) in TRIGGER_NPCS {
        let npc = force_spawn_unit(&server, *unit);
        let t = npc.data().unit.transform;
        assert!(
            close(t.x, *x) && close(t.y, *y) && close(t.z, *z),
            "trigger NPC {unit} at ({}, {}, {}), got ({}, {}, {})",
            x,
            y,
            z,
            t.x,
            t.y,
            t.z
        );
    }
    // The tags the skills filter by resolve to the placed templates.
    for (tag, unit) in [
        (532, 10474),
        (535, 10452),
        (540, 10499),
        (542, 10528),
        (548, 10571),
        (549, 10573),
        (552, 10574),
        (554, 10575),
        (561, 10455),
        (607, 10815),
    ] {
        assert!(
            server
                .game_data
                .systems
                .skills
                .npcs_by_tag(tag)
                .contains(&unit),
            "tag {tag} -> NPC {unit}"
        );
    }
}

fn place_player(
    server: &Arc<GameServer>,
    pid: u32,
    x: f32,
    y: f32,
    z: f32,
) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, pid, pid);
    let mut ch = test_character(pid, &format!("P{pid}"), x, y);
    ch.unit.level = 40;
    ch.unit.hp = 1000;
    ch.unit.max_hp = 1000;
    ch.unit.transform.set_xyz(x, y, z);
    ch.unit.transform.zone_id = ZONE;
    let p = spawn_player(server, &client, ch, 0x10_0000 + pid);
    (client, p)
}

fn sphere_areas(sphere_ids: &[u32]) -> Vec<SphereArea> {
    load_sphere_areas(&app_path(), |id| sphere_ids.contains(&id))
        .into_iter()
        .filter(|a| a.world == WORLD)
        .collect()
}

/// The two souls the spheres bring in: sphere 1379 (intro) casts 17081, whose buff 3081 trigger
/// spawns son 10473 from event spawner 65999; sphere 1410 (after the 2nd boss) casts 17249,
/// whose buff 3153 trigger spawns son 10503 from event spawner 66025.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_burnt_spheres_spawn_the_sons() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let _ = load_burnt(&server);
    let trigger_10474 = force_spawn_unit(&server, 10474);
    let trigger_10575 = force_spawn_unit(&server, 10575);
    assert_eq!(trigger_10474.data().unit.template_id, 10474);
    assert_eq!(trigger_10575.data().unit.template_id, 10575);
    assert!(
        npcs_of(&world, 10473).is_empty(),
        "the intro son is not at start"
    );
    assert!(
        npcs_of(&world, 10503).is_empty(),
        "the 2nd-boss-end son is not at start"
    );

    let areas = sphere_areas(&[1379, 1410]);
    assert_eq!(areas.iter().filter(|a| a.sphere_id == 1379).count(), 6);
    assert_eq!(areas.iter().filter(|a| a.sphere_id == 1410).count(), 5);
    server.systems.quests.spheres.set_areas(areas);

    // Intro: the first shipped area point, trigger NPC 10474 nearby (tag 532).
    let (_c1, player) = place_player(&server, 1, 198.004, 101.883, 20.5377);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(spawned_count(&server, 65999), 1, "event spawner 65999");
    let sons = npcs_of(&world, 10473);
    assert_eq!(sons.len(), 1, "the intro son appears");
    let t = sons[0].data().unit.transform;
    assert!(
        close(t.x, 208.524) && close(t.y, 106.159) && close(t.z, 20.538),
        "son at the 65999 level placement, got ({}, {}, {})",
        t.x,
        t.y,
        t.z
    );

    // Move to the 2nd-boss-end sphere; entering it casts 17249 and buff 3153 spawns 10503.
    player
        .data()
        .unit
        .transform
        .set_xyz(256.805, 268.111, 20.859);
    server.systems.quests.spheres.tick_at(&server, 500);
    assert_eq!(spawned_count(&server, 66025), 1, "event spawner 66025");
    let sons = npcs_of(&world, 10503);
    assert_eq!(sons.len(), 1, "the after-2nd-boss son appears");
    let t = sons[0].data().unit.transform;
    assert!(
        close(t.x, 259.933) && close(t.y, 273.762) && close(t.z, 20.538),
        "son at the 66025 level placement, got ({}, {}, {})",
        t.x,
        t.y,
        t.z
    );
}

fn doodad_of(world: &Arc<WorldInstance>, template: u32) -> DoodadRef {
    let matching: Vec<DoodadRef> = world
        .all_doodads()
        .into_iter()
        .filter(|d| d.data().template_id == template)
        .collect();
    assert_eq!(matching.len(), 1, "exactly one placed doodad {template}");
    matching[0].clone()
}

/// The boss death skills open the authored gates and the exit: Sorvell's 17754 hits 3612,
/// Winters's 17755 hits 3613, the intro set's 17103 hits 4417 and Hieronimus's 18024 turns the
/// invisible 4923 into the usable exit portal. A different skill leaves the phase untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_burnt_boss_death_opens_gates_and_exit() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let _ = load_burnt(&server);
    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    assert!(m.spawn_doodads(&server) >= DOODADS.len(), "doodads spawn");

    let (_client, player) = place_player(&server, 1, 165.587, 99.906, 20.858);
    let caster = WorldObject::from(player.clone());

    for (template, start, _, _, _) in DOODADS {
        let d = doodad_of(&world, *template);
        assert_eq!(
            d.data().func_group_id,
            *start,
            "doodad {template} starts in phase {start}"
        );
        let ctx = DoodadCtx::of(&server, &d).expect("doodad world");
        phase::on_skill_hit(&ctx, Some(&caster), 1);
        assert_eq!(
            d.data().func_group_id,
            *start,
            "a skill that is not the gate's skill does not open {template}"
        );
    }

    for (template, skill, next) in [
        (4417, 17103, 10834),
        (3612, 17754, 8671),
        (3613, 17755, 8674),
        (4923, 18024, 12257),
    ] {
        let d = doodad_of(&world, template);
        let ctx = DoodadCtx::of(&server, &d).expect("doodad world");
        phase::on_skill_hit(&ctx, Some(&caster), skill);
        assert_eq!(
            d.data().func_group_id,
            next,
            "skill {skill} opens doodad {template}"
        );
    }

    // The 1.2 interaction skill (the exit skill 15667) must find a Use func in each exit.
    let data = &server.game_data.systems.npcs.doodad;
    for group in [8794, 12257] {
        assert!(
            data.func_for_skill(group, 15667).is_some(),
            "exit func group {group} answers skill 15667"
        );
    }
    // The invisible start phase of 4923 carries the 18024 SkillHit that turns it into 12257.
    assert!(
        data.func_for_skill(12256, 18024).is_some(),
        "4923 start phase answers 18024"
    );
}
