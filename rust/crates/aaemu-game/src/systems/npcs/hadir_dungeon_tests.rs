//! Hadir Farm (`instance_hadir_farm`, zone 241) dungeon conformance.
//!
//! The 1.2 compact data carries the level chain: the `SphereSkill` spheres 1284/1285/
//! 1298-1312/1331/1457/2246 cast their `sphere_skills` rows, the 13 `tags` rows select the
//! invisible "Spawn Slave" / captive trigger NPCs, and the buff-trigger / command-set / NPC
//! event skills name 74 of the 86 level spawner ids (the sphere 1298/1299 camps 60909-60911,
//! 61146-61148 and 68235-68240 are 10.x-only effects; see `PORTING.md`). The shipped `npc_spawns.json` therefore needs the
//! level's event rows (one per referenced spawner id, `NpcSpawnerIds`), the trigger NPCs whose
//! level spawner is active at start as plain rows, the start-active skeleton camp (9473 x10) and
//! Bloodmage (10021) rows the 1.2-era file misses, and the scripted doodads of the golem and
//! cauldron chains. The event rows are listed first in the file: the loader's duplicate filter
//! keys on `(UnitId, position)`, and two level spawners (61148, 68236) stand within 2 m of a
//! kept 1.2-era plain row; an event row after them would be dropped before its spawner
//! registration (event entries never feed the duplicate position table).
//!
//! Placements are the 10.x level editor rows (**Inferred**, see `PORTING.md`); templates, tags,
//! skills, buffs and start phases are 1.2 `compact.sqlite3`. Tests without the gate run in CI;
//! the `AAEMU_COMPACT_DB` ones need a read-only copy of the 1.2 compact.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use sqlx::mysql::MySqlPoolOptions;

use crate::config::AppConfig;
use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::phase;
use crate::systems::npcs::doodad::spawner::{parse_list, DoodadSpawnerDef, NpcSpawnerDef};
use crate::systems::npcs::npc::spawner_data::NpcSpawnerData;
use crate::systems::npcs::spawn_manager::{backend::TestBackend, LoadStats, TemplateExists};
use crate::systems::quests::spheres::{load_sphere_areas, SphereArea};
use crate::systems::DataLoadCtx;
use crate::test_support::{test_character, TestClient, TEST_ZONE};
use crate::world::{Entity, NpcRef, PlayerRef, WorldInstance, WorldObject};

const WORLD: &str = "instance_hadir_farm";
const ZONE: u32 = 241;
const INSTANCE_ID: u32 = 1;

/// The 86 level spawners named by the dungeon's `NpcSpawnerSpawnEffect` rows (spawner id, the
/// NPC the level places there). Derived from the 1.2 `npc_spawner_spawn_effects` rows reached
/// through the sphere / buff-trigger / command-set / NPC-event chains.
const EVENT_SPAWNERS: &[(u32, u32)] = &[
    (58186, 9793),
    (59017, 10021),
    (59044, 9468),
    (59140, 9466),
    (59141, 9467),
    (60909, 9477),
    (60910, 9478),
    (60911, 9480),
    (61146, 9478),
    (61147, 9481),
    (61148, 9480),
    (61190, 10100),
    (61192, 10100),
    (61194, 10100),
    (61203, 9798),
    (61613, 9468),
    (61823, 10070),
    (61979, 9472),
    (61980, 9472),
    (61981, 9472),
    (61982, 9472),
    (61983, 9472),
    (61984, 9472),
    (61985, 9472),
    (61986, 9472),
    (61987, 9472),
    (61988, 9472),
    (61989, 9473),
    (61990, 9473),
    (61991, 9473),
    (61992, 9473),
    (61993, 9473),
    (61994, 9473),
    (61995, 9473),
    (61996, 9473),
    (61997, 9473),
    (61998, 9473),
    (61999, 9472),
    (62000, 9472),
    (62001, 9472),
    (62002, 9472),
    (62003, 9472),
    (62004, 9472),
    (62005, 9472),
    (62006, 9472),
    (62007, 9472),
    (62008, 9472),
    (62009, 9473),
    (62010, 9473),
    (62011, 9473),
    (62012, 9473),
    (62013, 9473),
    (62015, 9473),
    (62016, 9473),
    (62017, 9473),
    (62018, 9473),
    (62019, 10810),
    (62020, 10165),
    (62021, 9792),
    (62023, 10166),
    (62025, 10167),
    (68235, 9478),
    (68236, 9481),
    (68237, 9481),
    (68238, 9481),
    (68239, 9478),
    (68240, 9477),
    (68521, 9477),
    (68522, 9468),
    (68523, 9480),
    (68524, 10805),
    (68525, 10806),
    (68526, 9478),
    (68527, 9481),
    (68597, 9481),
    (68662, 9468),
    (68665, 9969),
    (68666, 10021),
    (68667, 10025),
    (68668, 10024),
    (68669, 10807),
    (68671, 10808),
    (68672, 10809),
    (72386, 10887),
    (123727, 13799),
    (123729, 13741),
];

/// The trigger NPCs whose level spawner is active at start (plain rows; unit, x, y, z, yaw and
/// the level spawner id they stand at). The inactive trigger NPCs 10805-10810 are reached only
/// through their `NpcSpawnerIds` event rows above.
const TRIGGER_ROWS: &[(u32, f32, f32, f32, f32, u32)] = &[
    (10067, 574.854, 507.608, 135.824, -174.0, 59459),
    (10067, 592.742, 589.738, 136.514, -174.0, 59462),
    (10068, 589.642, 588.345, 136.536, 131.0, 59460),
    (10068, 577.972, 505.96, 136.054, 131.0, 59458),
    (10076, 575.956, 519.605, 136.013, -126.0, 60906),
    (10076, 696.076, 579.37, 136.06, 0.0, 61971),
    (10076, 617.045, 605.235, 136.582, 0.0, 62139),
    (10076, 681.153, 532.47, 142.972, 0.0, 61977),
    (13851, 825.918, 653.717, 141.475, 0.0, 123724),
    (10164, 579.607, 540.513, 136.066, -126.0, 60907),
    (10164, 643.768, 559.495, 143.243, 0.0, 61978),
    (10164, 724.703, 569.151, 136.602, 0.0, 61976),
];

/// The eleven start-active level NPCs the 1.2-era rows miss: the ten skeleton-camp 9473 and the
/// 10021 Bloodmage: `(unit, x, y, z, yaw, level spawner id)`. Compact 10557 / 11113 are
/// `activation_state = 't'`; the inactive variants 11225 / 11220 are skipped by the loader, and
/// 10021's NORMAL farm group spawner 13501 stays idle without a schedule (the C# rule).
const START_ACTIVE_ROWS: &[(u32, f32, f32, f32, f32, u32)] = &[
    (9473, 684.421, 630.566, 145.656, 0.0, 58999),
    (9473, 686.317, 633.423, 145.656, 0.0, 59008),
    (9473, 681.827, 638.37, 145.656, 0.0, 59004),
    (9473, 683.629, 635.055, 145.656, 0.0, 59011),
    (9473, 676.083, 635.165, 145.656, 0.0, 59006),
    (9473, 676.968, 630.752, 145.657, 0.0, 59010),
    (9473, 676.466, 626.528, 145.657, 0.0, 58994),
    (9473, 679.499, 635.795, 145.656, 0.0, 59003),
    (9473, 683.498, 625.839, 145.657, 0.0, 58991),
    (10021, 704.74, 641.444, 145.818, 0.0, 59012),
    (9473, 680.718, 627.622, 145.657, 0.0, 59009),
];

/// The scripted doodads of the Hadir Farm chains (template, x, y, z). 5958 is the
/// attack-ban guard of the Rotten Flesh Golem, 4143 the Necromancer's Cauldron (skill 16577
/// spawns 61613 through the farm manager's command set). The two farm exits keep their
/// 1.2-era rows and are not repeated here; 7661 (the secret chest) belongs to the quest lane,
/// and 4142 (the Necronomicon) is decor.
const SCRIPTED_DOODADS: &[(u32, f32, f32, f32)] = &[
    (4143, 591.083, 590.757, 136.536),
    (4143, 753.031, 613.486, 136.431),
    (5958, 584.958, 612.772, 141.294),
    (5958, 592.726, 614.687, 141.294),
    (5958, 602.659, 564.776, 143.184),
    (5958, 608.019, 558.354, 143.184),
    (5958, 608.958, 570.344, 143.184),
    (5958, 613.517, 550.815, 143.184),
    (5958, 617.182, 574.909, 143.184),
    (5958, 626.251, 578.816, 143.184),
    (5958, 637.517, 579.478, 143.184),
    (5958, 646.434, 631.397, 145.581),
    (5958, 649.924, 575.09, 143.184),
    (5958, 650.068, 622.143, 145.581),
    (5958, 658.102, 615.092, 145.581),
    (5958, 660.686, 571.488, 143.184),
    (5958, 662.745, 610.702, 145.581),
];

fn world_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds")
        .join(WORLD)
}

fn npc_defs() -> Vec<NpcSpawnerDef> {
    let text = std::fs::read_to_string(world_path().join("npc_spawns.json")).expect("npc_spawns");
    parse_list(&text, NpcSpawnerDef::from_json).expect("npc_spawns.json parses")
}

fn doodad_defs() -> Vec<DoodadSpawnerDef> {
    let text =
        std::fs::read_to_string(world_path().join("doodad_spawns.json")).expect("doodad_spawns");
    parse_list(&text, DoodadSpawnerDef::from_json).expect("doodad_spawns.json parses")
}

fn event_row_rows(defs: &[NpcSpawnerDef]) -> Vec<(u32, u32)> {
    defs.iter()
        .filter(|d| !d.npc_spawner_ids.is_empty() && d.npc_spawner_ids != [0])
        .map(|d| (*d.npc_spawner_ids.last().expect("id"), d.base.unit_id))
        .collect()
}

/// Every level `NpcSpawnerSpawnEffect` spawner of the Hadir chains has exactly one event row
/// (`NpcSpawnerIds` = the level spawner id) and every active-at-start trigger NPC is a plain
/// row at its level placement. The 116 shipped 1.2-era rows are kept.
#[test]
fn hadir_level_event_spawners_and_trigger_npcs_are_placed_once() {
    let defs = npc_defs();
    assert_eq!(
        defs.len(),
        225,
        "86 event + 12 trigger + 11 start-active + 116 kept rows"
    );

    let events = event_row_rows(&defs);
    assert_eq!(events.len(), EVENT_SPAWNERS.len(), "event row count");
    for expected in EVENT_SPAWNERS {
        let matches = events.iter().filter(|row| *row == expected).count();
        assert_eq!(
            matches, 1,
            "event spawner {} (npc {}) must appear exactly once",
            expected.0, expected.1
        );
    }
    // The two level spawners that stand within 2 m of a kept plain row are still event rows:
    // they are listed before the plain rows so the loader's duplicate filter keeps them.
    let first_plain = defs
        .iter()
        .position(|d| d.npc_spawner_ids.is_empty())
        .expect("plain rows");
    for (index, def) in defs.iter().enumerate().take(first_plain) {
        assert!(
            !def.npc_spawner_ids.is_empty(),
            "row {index} must be an event row"
        );
    }

    let plain = &defs[first_plain..];
    for (unit, x, y, z, yaw, level_spawner) in TRIGGER_ROWS {
        let matches = plain
            .iter()
            .filter(|d| {
                let p = &d.base.position;
                d.base.unit_id == *unit
                    && (p.x - x).abs() < 0.001
                    && (p.y - y).abs() < 0.001
                    && (p.z - z).abs() < 0.001
                    && (p.yaw - yaw).abs() < 0.001
            })
            .count();
        assert_eq!(
            matches, 1,
            "trigger NPC {unit} of level spawner {level_spawner} at ({x}, {y}, {z})"
        );
    }
    for (unit, x, y, z, yaw, level_spawner) in START_ACTIVE_ROWS {
        let matches = plain
            .iter()
            .filter(|d| {
                let p = &d.base.position;
                d.base.unit_id == *unit
                    && (p.x - x).abs() < 0.001
                    && (p.y - y).abs() < 0.001
                    && (p.z - z).abs() < 0.001
                    && (p.yaw - yaw).abs() < 0.001
            })
            .count();
        assert_eq!(
            matches, 1,
            "start-active NPC {unit} of level spawner {level_spawner} at ({x}, {y}, {z})"
        );
    }
    // The secret chest 7661 is a doodad (quest-spawns rows), never an NPC row.
    assert!(!defs.iter().any(|d| d.base.unit_id == 7661));
}

/// The 17 scripted doodads (5958 x15, 4143 x2) are placed exactly once at their level
/// positions; the two farm exits keep their 1.2-era rows and the quest chest 7661 has only its
/// two quest-spawns rows.
#[test]
fn hadir_scripted_doodads_are_placed_once() {
    let defs = doodad_defs();
    assert_eq!(
        defs.len(),
        21,
        "17 scripted + 2 kept exit rows + 2 quest-spawns chests"
    );

    for (template, x, y, z) in SCRIPTED_DOODADS {
        let matches = defs
            .iter()
            .filter(|d| {
                let p = &d.base.position;
                d.base.unit_id == *template
                    && (p.x - x).abs() < 0.001
                    && (p.y - y).abs() < 0.001
                    && (p.z - z).abs() < 0.001
            })
            .count();
        assert_eq!(
            matches, 1,
            "doodad {template} at ({x}, {y}, {z}) must appear exactly once"
        );
    }
    for (template, count) in [(5958, 15), (4143, 2), (4289, 1), (4927, 1)] {
        let placed = defs.iter().filter(|d| d.base.unit_id == template).count();
        assert_eq!(placed, count, "doodad {template} rows");
    }
    assert_eq!(
        defs.iter().filter(|d| d.base.unit_id == 7661).count(),
        2,
        "the secret chest 7661 is placed only by the two quest-spawns rows"
    );
}

// -------------------------------------------------------------------------------------------
// Real 1.2 data
// -------------------------------------------------------------------------------------------

struct Backend;

impl TestBackend for Backend {
    fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
        let id = if obj_id == 0 {
            server.ids.object.get_next_id().ok()?
        } else {
            obj_id
        };
        let mut n = Npc::new(template_id);
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.level = 10;
        Some(Entity::new(id, n))
    }

    fn doodad_create(
        &self,
        _server: &GameServer,
        _world: &Arc<WorldInstance>,
        _obj_id: u32,
        _template_id: u32,
    ) -> Option<crate::world::DoodadRef> {
        None
    }
}

/// A server whose only world is `name` (the start-active rule depends on the world).
fn server_with_worlds(
    npc_spawners: NpcSpawnerData,
    worlds: impl IntoIterator<Item = (u32, String, u32)>,
) -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.systems.npcs.npc_spawners = npc_spawners;
    data.worlds = WorldTemplates::from_templates(
        worlds
            .into_iter()
            .map(|(id, name, zone)| WorldTemplate::synthetic(id, &name, 2, 2, zone))
            .collect(),
        Vec::new(),
    )
    .unwrap();
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, db, data));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    server
}

fn npc_ids(conn: &Connection) -> HashSet<u32> {
    let mut npcs = HashSet::new();
    let mut stmt = conn.prepare("SELECT id FROM npcs").unwrap();
    for id in stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap() {
        npcs.insert(id.unwrap() as u32);
    }
    npcs
}

/// `load_spawn_files` of `Data/Worlds/instance_hadir_farm` for the manager of `world_id`.
fn load_world(server: &Arc<GameServer>, conn: &Connection, world_id: u32) -> LoadStats {
    let npcs = npc_ids(conn);
    let none = |_: u32| false;
    let npc = |id: u32| npcs.contains(&id);
    let exists = TemplateExists {
        npc: &npc,
        doodad: &none,
        transfer: &none,
        gimmick: &none,
        slave: &none,
    };
    let world = server.world.world(world_id).expect("world instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.load_spawn_files(server, &world_path(), &exists).unwrap()
}

/// Loads the shipped `npc_spawns.json` / `doodad_spawns.json` of the real server's world 0
/// with the real template tables (the server used by the chain tests).
fn load_shipped(server: &Arc<GameServer>) -> LoadStats {
    let gd = &server.game_data;
    let sd = &gd.systems.npcs.npc_spawners;
    let npc = |id| gd.systems.npcs.npc.exist(id);
    let doodad = |id| gd.systems.npcs.doodad.exist(id);
    let transfer = |id| sd.transfer_template_ids.contains(&id);
    let gimmick = |id| gd.systems.npcs.gimmick.exist(id);
    let slave = |id| sd.slave_template_ids.contains(&id);
    let exists = TemplateExists {
        npc: &npc,
        doodad: &doodad,
        transfer: &transfer,
        gimmick: &gimmick,
        slave: &slave,
    };
    let world = server.world.world(0).expect("world instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.load_spawn_files(server, &world_path(), &exists)
        .expect("shipped spawn files load")
}

/// The shipped files load on the real 1.2 data: 86 event spawners, 176 start-active spawner
/// clones from the 125 plain rows (2 kept duplicate rows and 102 `activation_state = 'f'`
/// expansions skipped), and every event spawner id resolves through `get_npc_spawner`.
/// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game hadir_dungeon -- --ignored`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
async fn real_compact_db_hadir_instance_loads_the_authored_spawns() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let data = NpcSpawnerData::load_from(&conn).unwrap();
    let server = server_with_worlds(
        data,
        [
            (0, "main_world".to_string(), TEST_ZONE),
            (INSTANCE_ID, WORLD.to_string(), ZONE),
        ],
    );
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(WORLD)
        .expect("instance template")
        .clone();
    let world = server
        .world
        .create_world_instance(Arc::new(template), INSTANCE_ID);
    server.systems.npcs.spawns.get_or_create(&world);
    let stats = load_world(&server, &conn, INSTANCE_ID);
    eprintln!("hadir load: {stats:?}");
    assert_eq!(stats.npc_entries, 225);
    assert_eq!(stats.npc_duplicates, 2, "the two kept 1.2-era duplicates");
    assert_eq!(stats.npc_missing_templates, 0);
    assert_eq!(stats.npc_event_spawners, 86);
    assert_eq!(stats.npc_spawners, 188);
    assert_eq!(stats.npc_event_only_spawners_skipped, 113);
    assert_eq!(stats.npc_cutscene_spawners_skipped, 0);
    assert_eq!(
        stats.doodad_entries, 21,
        "17 scripted + 2 kept exit rows + 2 quest-spawns chests"
    );

    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    for (spawner, unit) in EVENT_SPAWNERS {
        let found = m.get_npc_spawner(*spawner);
        assert_eq!(found.len(), 1, "event spawner {spawner} (npc {unit})");
    }

    // The active-at-start trigger NPCs are plain rows: their kept spawner clones exist.
    for (unit, spawner) in [
        (10067, 11161),
        (10068, 11162),
        (10076, 11172),
        (10164, 11288),
        (13851, 15413),
    ] {
        let ids: Vec<u32> = m
            .get_all_spawners()
            .into_values()
            .flatten()
            .filter(|s| s.unit_id == unit)
            .map(|s| s.spawner_id)
            .collect();
        assert!(
            ids.contains(&spawner),
            "trigger NPC {unit} must start from spawner {spawner}, got {ids:?}"
        );
    }

    // The eleven start-active rows keep their compact spawners at the authored placements:
    // 9473 keeps the AUTOCREATED 10557 (and skips the inactive 11225); 10021 keeps 11113 and
    // clones the NORMAL farm group spawner 13501, which stays idle without a schedule.
    let placements: Vec<_> = m
        .get_all_spawners()
        .into_values()
        .flatten()
        .filter(|s| s.is_placement())
        .collect();
    for (unit, x, y, z, _, level_spawner) in START_ACTIVE_ROWS {
        let clones: Vec<_> = placements
            .iter()
            .filter(|s| {
                let p = s.position();
                s.unit_id == *unit
                    && (p.x - x).abs() < 0.001
                    && (p.y - y).abs() < 0.001
                    && (p.z - z).abs() < 0.001
            })
            .collect();
        if *unit == 10021 {
            let ids: Vec<u32> = clones.iter().map(|s| s.spawner_id).collect();
            assert!(
                ids.contains(&11113),
                "10021 at ({x}, {y}, {z}) keeps spawner 11113, got {ids:?}"
            );
            assert!(
                ids.contains(&13501),
                "10021 at ({x}, {y}, {z}) clones the farm group spawner 13501, got {ids:?}"
            );
            let group = clones
                .iter()
                .find(|s| s.spawner_id == 13501)
                .expect("farm group clone");
            assert!(
                !group.is_optimal_spawner(&server),
                "the NORMAL farm group clone stays idle without a schedule"
            );
        } else {
            assert_eq!(
                clones.len(),
                1,
                "9473 at ({x}, {y}, {z}) keeps one start-active spawner (level {level_spawner})"
            );
            assert_eq!(
                clones[0].spawner_id, 10557,
                "9473 at ({x}, {y}, {z}) start spawner"
            );
            assert!(clones[0].is_optimal_spawner(&server));
        }
    }

    // The scripted doodad templates and their 1.2 start phases.
    let doodads = DoodadData::load_from(&conn).expect("doodad data");
    for (template, start_group) in [(4143, 10038), (5958, 15355), (4289, 10546), (4927, 12276)] {
        assert!(
            doodads.template(template).is_some(),
            "missing doodad template {template}"
        );
        assert_eq!(
            phase::get_func_group_id(&doodads, template),
            start_group,
            "doodad {template} start phase"
        );
    }
}

/// The Annmit rows are in the 1.2 data: sphere 2246 casts 23512, whose command set 320 runs
/// 23513; buff 6776's timeout trigger 33633 is a `NpcSpawnerSpawnEffect` naming spawner
/// 123727. Skill 23505's command set 321 runs 23521, whose effect names spawner 123729.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_db_hadir_annmit_chain_is_in_the_1_2_data() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let sphere_skill: u32 = conn
        .query_row(
            "SELECT sk.skill_id FROM spheres s JOIN sphere_skills sk ON sk.id = s.sphere_detail_id \
             WHERE s.id = 2246",
            [],
            |r| r.get(0),
        )
        .expect("sphere 2246 -> sphere skill");
    assert_eq!(sphere_skill, 23512);
    let control: u32 = conn
        .query_row(
            "SELECT n.param_int FROM skill_effects se \
             JOIN effects e ON e.id = se.effect_id \
             JOIN npc_control_effects n ON e.actual_type = 'NpcControlEffect' AND n.id = e.actual_id \
             WHERE se.skill_id = 23512",
            [],
            |r| r.get(0),
        )
        .expect("23512 command set");
    assert_eq!(control, 320);
    let buff: u32 = conn
        .query_row(
            "SELECT b.buff_id FROM skill_effects se \
             JOIN effects e ON e.id = se.effect_id \
             JOIN buff_effects b ON e.actual_type = 'BuffEffect' AND b.id = e.actual_id \
             WHERE se.skill_id = 23513",
            [],
            |r| r.get(0),
        )
        .expect("23513 buff");
    let trigger_spawner: u32 = conn
        .query_row(
            "SELECT s.spawner_id FROM buff_triggers t \
             JOIN effects e ON e.id = t.effect_id \
             JOIN npc_spawner_spawn_effects s \
               ON e.actual_type = 'NpcSpawnerSpawnEffect' AND s.id = e.actual_id \
             WHERE t.buff_id = ?1 AND t.event_id = 6",
            [buff],
            |r| r.get(0),
        )
        .expect("buff timeout spawner");
    assert_eq!(trigger_spawner, 123727);
    let l50: u32 = conn
        .query_row(
            "SELECT s.spawner_id FROM skill_effects se \
             JOIN effects e ON e.id = se.effect_id \
             JOIN npc_spawner_spawn_effects s \
               ON e.actual_type = 'NpcSpawnerSpawnEffect' AND s.id = e.actual_id \
             WHERE se.skill_id = 23521",
            [],
            |r| r.get(0),
        )
        .expect("23521 spawner");
    assert_eq!(l50, 123729);
}

/// A server with the real skills / npcs / quests / AI tables and a synthetic Hadir world.
fn real_server(db: &str) -> Arc<GameServer> {
    let conn =
        Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("db");
    let app_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
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
    let world = WorldTemplate::synthetic(0, WORLD, 32, 32, ZONE);
    let server = Arc::new(crate::test_support::test_server_with_world(data, world));
    server.set_self();
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(WORLD)
        .expect("Hadir world template")
        .clone();
    server.world.create_world_instance(Arc::new(template), 0);
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::ai::hooks::install(&server);
    server
}

fn place_player(
    server: &Arc<GameServer>,
    world: &Arc<WorldInstance>,
    pid: u32,
    x: f32,
    y: f32,
    z: f32,
) -> PlayerRef {
    let client = TestClient::new(server, pid, pid);
    let mut ch = test_character(pid, &format!("P{pid}"), x, y);
    ch.is_online = true;
    ch.unit.transform.instance_id = world.id;
    ch.unit.transform.zone_id = ZONE;
    ch.unit.transform.set_xyz(x, y, z);
    ch.unit.level = 50;
    ch.unit.hp = 5000;
    ch.unit.max_hp = 5000;
    ch.obj_id = server.ids.object.get_next_id().unwrap();
    let p = crate::world::Player::new(ch, client.conn.handle());
    server.world.try_add_character(&p);
    world.spawn(&p, &server.view_ctx());
    p
}

fn spawned_count(server: &Arc<GameServer>, spawner_id: u32) -> i32 {
    let world = server.world.world(0).expect("world");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.get_npc_spawner(spawner_id)
        .first()
        .map_or(0, |s| s.current_spawn_count())
}

fn npcs_at(world: &Arc<WorldInstance>, template_id: u32, x: f32, y: f32) -> usize {
    world
        .all_npcs()
        .into_iter()
        .filter(|n| {
            let t = n.data().unit.transform;
            n.data().unit.template_id == template_id
                && (t.x - x).abs() < 1.5
                && (t.y - y).abs() < 1.5
        })
        .count()
}

fn npcs_of(world: &Arc<WorldInstance>, template_id: u32) -> Vec<NpcRef> {
    world
        .all_npcs()
        .into_iter()
        .filter(|n| n.data().unit.template_id == template_id)
        .collect()
}

fn npc_near(world: &Arc<WorldInstance>, template_id: u32, x: f32, y: f32) -> Option<NpcRef> {
    npcs_of(world, template_id).into_iter().find(|n| {
        let t = n.data().unit.transform;
        (t.x - x).abs() < 1.5 && (t.y - y).abs() < 1.5
    })
}

/// Force-spawns the shipped event spawner `spawner_id` (the NPC of its `NpcSpawnerIds` row).
fn spawner_force_spawn(server: &Arc<GameServer>, spawner_id: u32) -> Option<NpcRef> {
    let world = server.world.world(0).expect("world");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.get_npc_spawner(spawner_id)
        .first()
        .and_then(|s| s.force_spawn(server, 0))
}

fn tick_ai(server: &Arc<GameServer>, ticks: usize) {
    for _ in 0..ticks {
        crate::systems::ai::manager::tick_all(server, 100);
    }
}

/// The shipped chains with the real 1.2 data:
///
/// * sphere 1300 (area at 699.53, 580.65) casts skill 16748; its `BuffEffect` tag 469 reaches
///   the trigger NPC 10076, buff 2931's timeout trigger runs the ten `NpcSpawnerSpawnEffect`
///   rows 61979-61988 (Restless Skeleton Peasants at their level positions) and kills the
///   consumed trigger NPC;
/// * sphere 1308 casts 16756 on tag 594 (the trigger NPC 10805 that the Rotten Flesh Golem's
///   death skill spawns through 68524), buff 2939 spawns 68521-68523.
///
/// The trigger NPCs and every spawner come from the shipped `npc_spawns.json`; the start-active
/// plain rows are placed by the active-region pass as on a live instance.
///
/// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game hadir_dungeon -- --ignored`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
async fn real_data_hadir_spheres_run_the_buff_trigger_chains() {
    let Some(db) = std::env::var("AAEMU_COMPACT_DB").ok() else {
        return;
    };
    let server = real_server(&db);
    let stats = load_shipped(&server);
    eprintln!("hadir load: {stats:?}");
    let world = server.world.world(0).expect("world");

    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let areas: Vec<SphereArea> = load_sphere_areas(&app, |id| id == 1300 || id == 1308)
        .into_iter()
        .filter(|a| a.world == WORLD)
        .collect();
    assert_eq!(
        areas.iter().filter(|a| a.sphere_id == 1300).count(),
        3,
        "shipped sphere 1300 areas"
    );
    assert_eq!(
        areas.iter().filter(|a| a.sphere_id == 1308).count(),
        4,
        "shipped sphere 1308 areas"
    );
    server.systems.quests.spheres.set_areas(areas);

    // The trigger NPC 10076 of the sphere 1300 chain is a start-active plain row.
    let player = place_player(&server, &world, 1, 699.53, 580.65, 136.178);
    for _ in 0..4 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let slave1 = npc_near(&world, 10076, 696.076, 579.37).expect("trigger NPC 10076 near 61971");
    assert_eq!(npcs_of(&world, 10076).len(), 4, "four plain 10076 rows");

    server.systems.quests.spheres.tick_at(&server, 0);
    assert!(
        slave1.data().unit.buffs.check_buff(2931),
        "sphere 1300 must put the spawn buff on the tag-469 trigger NPC"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    for spawner in 61979..=61988 {
        assert_eq!(
            spawned_count(&server, spawner),
            1,
            "buff 2931's timeout trigger must spawn {spawner}"
        );
    }
    assert!(
        npcs_at(&world, 9472, 688.126, 585.682) == 1,
        "a Restless Skeleton Peasant stands at spawner 61979's level placement"
    );
    assert!(
        server.world.object(slave1.obj_id).is_none(),
        "the chain's KillNpcWithoutCorpseEffect consumes the trigger NPC"
    );

    // The trigger NPC 10805 is event-only (the Rotten Flesh Golem's death spawner 68524).
    let slave3 = spawner_force_spawn(&server, 68524).expect("trigger NPC 10805 from 68524");
    player
        .data()
        .unit
        .transform
        .set_xyz(645.465, 592.729, 136.044);
    server.systems.quests.spheres.tick_at(&server, 500);
    assert!(
        slave3.data().unit.buffs.check_buff(2939),
        "sphere 1308 must put the spawn buff on the tag-594 trigger NPC"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    for spawner in [68521, 68522, 68523] {
        assert_eq!(
            spawned_count(&server, spawner),
            1,
            "buff 2939's timeout trigger must spawn {spawner}"
        );
    }
    assert!(
        server.world.object(slave3.obj_id).is_none(),
        "the chain's KillNpcWithoutCorpseEffect consumes the trigger NPC"
    );
}

/// The Rotten Flesh Golem's `OnDeath` skill 17608 spawns the cleanup slaves 10805, 10806 and
/// 10810 through the shipped event rows 68524, 68525 and 62019.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
async fn real_data_hadir_golem_death_spawns_the_cleanup_slaves() {
    let Some(db) = std::env::var("AAEMU_COMPACT_DB").ok() else {
        return;
    };
    let server = real_server(&db);
    load_shipped(&server);
    let world = server.world.world(0).expect("world");
    let _player = place_player(&server, &world, 1, 622.0, 626.0, 136.0);
    let golem = spawner_force_spawn(&server, 58186).expect("Rotten Flesh Golem from 58186");
    tick_ai(&server, 2);
    golem.data().unit.hp = 0;
    crate::systems::skills::combat::do_die(
        &server,
        &WorldObject::from(&golem),
        None,
        crate::systems::skills::types::KillReason::Damage,
    );
    tick_ai(&server, 10);
    for (spawner, unit) in [(68524, 10805), (68525, 10806), (62019, 10810)] {
        assert_eq!(
            spawned_count(&server, spawner),
            1,
            "skill 17608 must spawn the cleanup slave {unit} through {spawner}"
        );
    }
}
