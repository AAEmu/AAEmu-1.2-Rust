//! Kroloal Cradle (`instance_cradle_of_destruction`, zone 264, indun group 52) scripted data.
//!
//! The 1.2-era `npc_spawns.json` holds only the start-active placements. The scripted actors
//! the dungeon's skills spawn are added as **event rows** (`NpcSpawnerIds` = the level
//! `spawnerId` named by the skill's `NpcSpawnerSpawnEffect`), the seven tag targets of
//! `NpcControlEffect` `RunCommandSet` as plain trigger rows, and the four start-active level
//! spawners that had no row at all. `doodad_spawns.json` gains the 41 scripted and 4
//! interactive level doodads (no decor; skill-summoned doodads are never placed statically).
//!
//! Positions and yaws are the 10.x level editor rows (`level_design/zone/264/...` and
//! `level_design/cells/.../doodad.g`), **Inferred** for the 1.2 server (see `PORTING.md`);
//! templates, spawner ids, skills and NPC tags are 1.2 `compact.sqlite3` data. The blockers are
//! doodad 5725 (worldgate, `DoodadFuncUse` skill 19616), 5726 (Sprigg's death skill 18670) and
//! 5762 (Arkanis's death skill 19236, then `DoodadFuncExitIndun`).
//!
//! The synthetic tests run in CI; the real-data tests are gated by `AAEMU_COMPACT_DB` like the
//! rest of the crate's data tests.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::WorldTemplate;
use crate::data::GameData;
use crate::server::GameServer;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::phase;
use crate::systems::npcs::doodad::spawner::{parse_list, DoodadSpawnerDef, NpcSpawnerDef};
use crate::systems::npcs::doodad::templates::FuncTemplate;
use crate::systems::npcs::spawn_manager::{LoadStats, TemplateExists};
use crate::systems::DataLoadCtx;
use crate::test_support::{test_character, TestClient, TEST_ZONE};
use crate::world::{NpcRef, Player, PlayerRef, WorldInstance, WorldObject};

const WORLD: &str = "instance_cradle_of_destruction";
const ZONE: u32 = 264;
const INSTANCE_ID: u32 = 100;

fn app_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("app")
}

fn world_dir() -> PathBuf {
    app_root().join("Data/Worlds").join(WORLD)
}

fn npc_spawn_defs() -> Vec<NpcSpawnerDef> {
    let text =
        std::fs::read_to_string(world_dir().join("npc_spawns.json")).expect("npc_spawns.json");
    parse_list(&text, NpcSpawnerDef::from_json).expect("npc_spawns.json parses")
}

fn doodad_spawn_defs() -> Vec<DoodadSpawnerDef> {
    let text = std::fs::read_to_string(world_dir().join("doodad_spawns.json"))
        .expect("doodad_spawns.json");
    parse_list(&text, DoodadSpawnerDef::from_json).expect("doodad_spawns.json parses")
}

fn near(actual: f32, expected: f32, tolerance: f32) -> bool {
    (actual - expected).abs() <= tolerance
}

/// The 36 event spawner rows: level `spawnerId`, member NPC and the level position (Inferred).
#[rustfmt::skip]
const EVENT_ROWS: [(u32, u32, f32, f32, f32); 36] = [
    (88966, 11162, 598.131, 826.634, 182.376),
    (88967, 11162, 608.766, 830.570, 182.729),
    (88968, 11162, 593.125, 831.136, 182.127),
    (88969, 11162, 592.161, 822.519, 182.387),
    (88970, 11162, 604.515, 823.015, 182.491),
    (88971, 11162, 601.857, 834.566, 182.633),
    (88972, 11162, 610.822, 837.037, 182.427),
    (88974, 11162, 589.122, 852.000, 182.400),
    (88975, 11162, 585.516, 847.543, 182.227),
    (88976, 11162, 604.701, 854.496, 182.670),
    (88977, 11162, 606.735, 849.030, 182.681),
    (88978, 11162, 585.215, 835.938, 182.455),
    (88979, 11162, 586.120, 830.395, 182.547),
    (88980, 11162, 585.542, 856.929, 182.784),
    (88981, 11162, 576.476, 854.684, 182.689),
    (88999, 11163, 168.872, 514.978, 172.826),
    (89000, 11181, 171.306, 493.347, 173.346),
    (89001, 11183, 198.599, 495.270, 173.176),
    (89002, 11184, 195.709, 517.421, 173.506),
    (92250, 11161, 637.931, 406.604, 113.433),
    (92257, 11161, 664.458, 379.491, 113.433),
    (93513, 11550, 651.436, 393.445, 113.433),
    (93593, 12389, 667.917, 404.518, 113.433),
    (96069, 12165, 179.936, 717.246, 182.988),
    (96253, 12166, 182.249, 718.220, 182.988),
    (96256, 12170, 177.603, 716.427, 182.988),
    (96271, 12177, 361.280, 513.501, 174.061),
    (96273, 12178, 365.209, 513.102, 175.079),
    (96274, 12179, 366.033, 508.451, 175.078),
    (96372, 12181, 366.280, 506.869, 174.620),
    (96373, 12182, 366.308, 513.763, 175.425),
    (96374, 12183, 361.117, 515.084, 174.083),
    (96467, 12185, 650.374, 389.197, 113.612),
    (96468, 12187, 644.759, 390.328, 113.433),
    (96469, 12186, 656.093, 390.392, 113.433),
    (96470, 12184, 650.346, 357.745, 113.433),
];

/// The trigger NPC plain rows (the `NpcControlEffect` tags 830/685/915/852/851) and the four
/// start-active level spawners that had no placement.
#[rustfmt::skip]
const PLAIN_ROWS: [(u32, f32, f32, f32); 11] = [
    (12157, 655.710, 435.099, 113.433),
    (11268, 175.522, 594.785, 174.747),
    (12180, 367.062, 511.090, 175.668),
    (12200, 506.689, 806.144, 186.616),
    (12198, 504.782, 847.533, 190.807),
    (12198, 504.888, 842.591, 190.527),
    (12199, 507.066, 845.170, 190.546),
    (11555, 650.346, 357.745, 113.433),
    (11368, 301.289, 737.207, 184.557),
    (11356, 685.530, 755.001, 173.062),
    (11552, 709.579, 744.144, 173.076),
];

/// The 45 scripted + interactive doodad rows: template, level family, position (Inferred).
#[rustfmt::skip]
const DOODAD_ROWS: [(u32, u32, f32, f32, f32); 45] = [
    (5038, 0, 219.433, 504.340, 173.844),
    (5252, 0, 684.907, 752.155, 173.000),
    (5252, 0, 700.698, 749.861, 172.679),
    (5257, 80001, 773.862, 841.167, 192.712),
    (5257, 80002, 785.845, 829.965, 192.128),
    (5257, 80000, 793.854, 848.721, 192.534),
    (5257, 80003, 797.630, 830.822, 192.410),
    (5265, 0, 547.703, 841.307, 184.592),
    (5267, 0, 434.554, 840.393, 186.573),
    (5268, 0, 179.048, 655.658, 178.649),
    (5269, 0, 461.655, 508.577, 187.952),
    (5278, 0, 465.180, 839.832, 188.246),
    (5278, 0, 502.962, 869.906, 188.514),
    (5278, 0, 512.368, 821.124, 187.709),
    (5278, 0, 513.373, 846.034, 188.881),
    (5300, 0, 640.273, 403.307, 113.137),
    (5301, 0, 639.726, 383.687, 113.137),
    (5302, 0, 660.643, 383.687, 113.137),
    (5303, 0, 661.780, 403.307, 113.137),
    (5316, 0, 651.302, 393.582, 92.500),
    (5332, 0, 481.675, 862.646, 190.002),
    (5347, 0, 650.235, 426.171, 112.966),
    (5391, 0, 650.336, 357.694, 113.433),
    (5414, 0, 373.718, 506.342, 218.344),
    (5414, 0, 380.360, 482.913, 218.344),
    (5414, 0, 383.215, 537.794, 218.344),
    (5414, 0, 388.941, 519.317, 218.344),
    (5414, 0, 390.401, 497.605, 218.942),
    (5414, 0, 399.361, 480.016, 218.344),
    (5414, 0, 408.603, 519.410, 218.344),
    (5414, 0, 409.366, 499.705, 218.344),
    (5414, 0, 412.794, 538.956, 218.344),
    (5414, 0, 422.038, 483.420, 218.344),
    (5414, 0, 424.712, 517.247, 218.344),
    (5414, 0, 428.707, 499.862, 218.344),
    (5570, 80023, 314.625, 819.411, 150.268),
    (5725, 30044, 874.348, 835.103, 193.828),
    (5726, 30043, 180.785, 716.275, 182.082),
    (5749, 0, 363.646, 511.074, 174.620),
    (5762, 0, 650.304, 352.796, 113.471),
    (5931, 0, 342.568, 508.714, 173.869),
    (5948, 0, 650.378, 433.429, 112.653),
    (5949, 0, 335.525, 508.775, 173.618),
    (5956, 0, 297.226, 741.934, 184.691),
    (5956, 0, 301.526, 742.803, 184.436),
];

/// Every added doodad row is placed exactly once, at its authored level position and family.
/// The three blocker doodads are checked individually, the decor / skill-summoned templates are
/// not in the file.
#[test]
fn cradle_added_doodad_rows_are_placed_once_each() {
    let defs = doodad_spawn_defs();
    let rows: Vec<(u32, u32, f32, f32, f32)> = defs
        .iter()
        .map(|d| {
            let p = &d.base.position;
            (d.base.unit_id, d.family_id, p.x, p.y, p.z)
        })
        .collect();

    for (template, family, x, y, z) in DOODAD_ROWS {
        let matches = rows
            .iter()
            .filter(|(t, f, rx, ry, rz)| {
                *t == template
                    && *f == family
                    && near(*rx, x, 0.001)
                    && near(*ry, y, 0.001)
                    && near(*rz, z, 0.001)
            })
            .count();
        assert_eq!(
            matches, 1,
            "doodad {template} family {family} at ({x}, {y}, {z})"
        );
    }

    // Blockers: the worldgate keeps its family (the Use func is bound to it), the gate and exit
    // are the invisible start phases Sprigg's / Arkanis's death skill hits.
    for (template, family, x, y, z) in [
        (5725, 30044, 874.348, 835.103, 193.828),
        (5726, 30043, 180.785, 716.275, 182.082),
        (5762, 0, 650.304, 352.796, 113.471),
    ] {
        assert!(
            rows.iter().any(|(t, f, rx, ry, rz)| {
                *t == template
                    && *f == family
                    && near(*rx, x, 0.001)
                    && near(*ry, y, 0.001)
                    && near(*rz, z, 0.001)
            }),
            "blocker doodad {template}"
        );
    }

    // No decor row was added, and the skill-summoned doodads are never static placements.
    for template in [5320, 5406, 5545, 5547, 6002, 6050] {
        assert!(
            !rows.iter().any(|(t, _, _, _, _)| *t == template),
            "decor doodad {template} must not be placed"
        );
    }
    for (template, name) in [
        (4914, "Hadir_Mob Spawn_Medium"),
        (5060, "Carnivore's Exploding Egg"),
        (5076, "Carnivore's Exploding Egg"),
        (5082, "Carnivore's Egg"),
        (5109, "Academy Apprentice's Poison Cloud"),
        (5111, "Academy Apprentice's Poison Cloud"),
        (5115, "Putrid Demon's Miasma"),
        (5151, "Putrid Demon's Poison Breath"),
        (5153, "Element Fanatic's Fire Totem"),
        (5154, "Kyrios's Arch General's Magic Circle"),
        (5158, "Maddened Shaman's Fire Flood"),
        (5166, "Maddened Shaman's Burning Torch"),
        (5187, "Kyrios's Arch General's Destructive Energy"),
        (5244, "Element Fanatic's Demon Summon Totem"),
        (5251, "Spice Bottle"),
        (5283, "Karim's Bane"),
        (5286, "Karim's Cursed Light"),
        (5292, "Karim's Mana Drain"),
        (5295, "Karim's Final Resistance"),
        (5339, "Arkanis's Fire Pillar"),
        (5350, "Sprigg's Bomb"),
        (5368, "Sprigg's Stinging Magnetic Field"),
        (5374, "Element Fanatic's Vortex"),
        (5440, "Destructive Guard Captain Scorch"),
        (5493, "Arkanis's Scorch"),
        (5525, "Karim's Nightmare Creation Set"),
        (5528, "Karim's Nightmare Explosion"),
        (5740, "Teleport"),
        (5743, "Hidden Suicide Team"),
        (5929, "Academy Apprentice's Poison Sphere Explosion"),
        (5934, "Toadstool Purifier"),
    ] {
        assert!(
            !rows.iter().any(|(t, _, _, _, _)| *t == template),
            "{name} ({template}) is summoned by a skill and must not be placed statically"
        );
    }
}

/// Every added event and plain NPC row exists exactly once with the right member; the trigger
/// NPCs are plain rows (start-active spawners) and the two inactive Royal Stormers are event
/// rows only, so the loader cannot pre-spawn them.
#[test]
fn cradle_added_npc_rows_are_placed_once_each() {
    let defs = npc_spawn_defs();
    let row_at = |unit: u32, x: f32, y: f32, z: f32| -> usize {
        defs.iter()
            .filter(|d| {
                let p = &d.base.position;
                d.npc_spawner_ids.is_empty()
                    && d.base.unit_id == unit
                    && near(p.x, x, 0.001)
                    && near(p.y, y, 0.001)
                    && near(p.z, z, 0.001)
            })
            .count()
    };
    let event = |spawner: u32, unit: u32, x: f32, y: f32, z: f32| -> usize {
        defs.iter()
            .filter(|d| {
                let p = &d.base.position;
                d.npc_spawner_ids == [spawner]
                    && d.base.unit_id == unit
                    && near(p.x, x, 0.001)
                    && near(p.y, y, 0.001)
                    && near(p.z, z, 0.001)
            })
            .count()
    };

    for (spawner, unit, x, y, z) in EVENT_ROWS {
        assert_eq!(
            event(spawner, unit, x, y, z),
            1,
            "event row {spawner} ({unit}) at ({x}, {y}, {z})"
        );
    }
    for (unit, x, y, z) in PLAIN_ROWS {
        assert_eq!(
            row_at(unit, x, y, z),
            1,
            "plain row {unit} at ({x}, {y}, {z})"
        );
    }

    // 12166 / 12170 (tags 845 / 844) have inactive level spawners (`activation_state = 'f'`);
    // they must stay event rows, otherwise the dungeon's start-active rule skips the plain row
    // and the cutscene never has its soldiers.
    for unit in [12166, 12170] {
        assert!(
            !defs
                .iter()
                .any(|d| d.npc_spawner_ids.is_empty() && d.base.unit_id == unit),
            "NPC {unit} must not have a plain start placement"
        );
    }

    // Two more `NpcSpawnerSpawnEffect` targets (104053 / 104060, skill 20189) have no
    // placement in this world's level; they are reported, not invented here.
    for spawner in [104053u32, 104060] {
        assert!(
            !defs.iter().any(|d| d.npc_spawner_ids == [spawner]),
            "the 10.x level has no placement for spawner {spawner}; reported, not invented"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Real data
// ---------------------------------------------------------------------------------------------

/// A server with the real skills / npcs / quests / AI tables and a synthetic cradle world (the
/// shipped spawn files and sphere area fill are used as they are).
fn real_server(db: &str) -> (Arc<GameServer>, Arc<WorldInstance>) {
    let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("compact db");
    let app = app_root();
    let files = ClientFileManager::from_sources(&[], &app);
    let ctx = DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).expect("formulas");
    data.systems.skills = crate::systems::skills::Data::load(&ctx).expect("skills data");
    data.systems.npcs = crate::systems::npcs::Data::load(&ctx).expect("npcs data");
    data.systems.quests = crate::systems::quests::Data::load(&ctx).expect("quests data");
    data.systems.ai = crate::systems::ai::Data::load(&ctx).expect("ai data");
    let template = WorldTemplate::synthetic(0, WORLD, 32, 32, TEST_ZONE);
    let server = Arc::new(crate::test_support::test_server_with_world(data, template));
    server.set_self();
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(WORLD)
        .expect("cradle world template")
        .clone();
    let world = server
        .world
        .create_world_instance(Arc::new(template), INSTANCE_ID);
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::ai::hooks::install(&server);
    server.systems.npcs.spawns.get_or_create(&world);
    (server, world)
}

fn npc_ids(conn: &Connection) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut stmt = conn.prepare("SELECT id FROM npcs").unwrap();
    for id in stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap() {
        out.insert(id.unwrap() as u32);
    }
    out
}

/// `load_spawn_files` of the shipped cradle directory for `INSTANCE_ID`.
fn load_cradle(server: &Arc<GameServer>, conn: &Connection) -> LoadStats {
    let npcs = npc_ids(conn);
    let doodads = &server.game_data.systems.npcs.doodad;
    let none = |_: u32| false;
    let npc = |id: u32| npcs.contains(&id);
    let doodad = |id: u32| doodads.template(id).is_some();
    let exists = TemplateExists {
        npc: &npc,
        doodad: &doodad,
        transfer: &none,
        gimmick: &none,
        slave: &none,
    };
    let world = server.world.world(INSTANCE_ID).expect("instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.load_spawn_files(server, &world_dir(), &exists)
        .expect("load cradle spawn files")
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
    ch.unit.hp = 10_000;
    ch.unit.max_hp = 10_000;
    ch.is_online = true;
    ch.unit.transform.instance_id = world.id;
    ch.unit.transform.zone_id = ZONE;
    ch.unit.transform.set_xyz(x, y, z);
    ch.obj_id = server.ids.object.get_next_id().unwrap();
    let p = Player::new(ch, client.conn.handle());
    server.world.try_add_character(&p);
    world.spawn(&p, &server.view_ctx());
    p
}

fn npcs_of(world: &Arc<WorldInstance>, template_id: u32) -> Vec<NpcRef> {
    world
        .all_npcs()
        .into_iter()
        .filter(|n| n.data().unit.template_id == template_id)
        .collect()
}

fn tick_ai(server: &Arc<GameServer>, ticks: usize) {
    for _ in 0..ticks {
        crate::systems::ai::manager::tick_all(server, 100);
    }
}

fn ai_queued(server: &Arc<GameServer>, obj_id: u32) -> usize {
    server
        .systems
        .ai
        .ais
        .inspect(obj_id, |a| a.commands_queue.len())
        .unwrap_or(0)
}

fn spawned_count(server: &Arc<GameServer>, spawner_id: u32) -> i32 {
    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    m.get_npc_spawner(spawner_id)
        .first()
        .map_or(0, |s| s.current_spawn_count())
}

fn find_doodad(world: &Arc<WorldInstance>, template_id: u32) -> crate::world::DoodadRef {
    world
        .all_doodads()
        .into_iter()
        .find(|d| d.data().template_id == template_id)
        .unwrap_or_else(|| panic!("doodad {template_id} placed"))
}

/// The shipped files load cleanly: every start-active placement and every event row the
/// dungeon's skills name is present, and the blockers' doodads exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_spawn_files_load_and_resolve_every_skill_spawner() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let (server, world) = real_server(&db);
    let stats = load_cradle(&server, &conn);
    eprintln!("cradle load: {stats:?}");

    assert_eq!(stats.npc_entries, 124, "77 kept + 36 event + 11 plain rows");
    assert_eq!(stats.npc_duplicates, 0);
    assert_eq!(stats.npc_missing_templates, 0);
    assert_eq!(stats.npc_event_spawners, 36);
    assert_eq!(
        stats.npc_event_only_spawners_skipped, 5,
        "the 5 inactive spawners the existing plain rows already skip"
    );
    assert_eq!(stats.npc_cutscene_spawners_skipped, 0);
    assert_eq!(stats.doodad_entries, 78, "33 kept + 45 added rows");
    assert_eq!(stats.doodad_duplicates, 0);
    assert_eq!(stats.doodad_missing_templates, 0);
    assert_eq!(stats.doodad_spawners, 78);

    // Every NpcSpawnerSpawnEffect target placed in this world's level resolves to its event row.
    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    for (spawner, unit, _, _, _) in EVENT_ROWS {
        let found = m.get_npc_spawner(spawner);
        assert_eq!(found.len(), 1, "event spawner {spawner} ({unit})");
    }
    // The two orphan references are not placements of this world (reported, not invented).
    for spawner in [104053u32, 104060] {
        assert!(
            m.get_npc_spawner(spawner).is_empty(),
            "the 10.x level has no placement for spawner {spawner}; reported, not invented"
        );
    }

    // The seven trigger spawners (and the four start-active NPCs) are plain placements.
    let placed: Vec<u32> = m
        .get_all_spawners()
        .into_values()
        .flatten()
        .map(|s| s.spawner_id)
        .collect();
    for spawner in [
        13455, 12499, 13481, 13506, 13505, 12794, 12600, 12588, 12784,
    ] {
        assert_eq!(
            placed.iter().filter(|&&s| s == spawner).count(),
            1,
            "plain spawner {spawner}"
        );
    }
    assert_eq!(
        placed.iter().filter(|&&s| s == 13504).count(),
        2,
        "both 12198 level spawners"
    );
    for spawner in [13467, 13471] {
        assert!(
            !placed.contains(&spawner),
            "inactive spawner {spawner} must not be placed at start"
        );
    }

    // The doodads spawn (their phase init is asynchronous but the placements are synchronous).
    m.spawn_doodads(&server);
    assert_eq!(world.all_doodads().len(), 78);
    let gate = find_doodad(&world, 5726);
    assert_eq!(gate.data().func_group_id, 14677, "closed gate phase");
    let exit = find_doodad(&world, 5762);
    assert_eq!(exit.data().func_group_id, 14769, "closed exit phase");
    let worldgate = find_doodad(&world, 5725);
    assert_eq!(worldgate.data().func_group_id, 14675);
    assert_eq!(worldgate.data().family_id, 30044);
}

/// The added templates exist in the 1.2 `compact.sqlite3`; the blocker funcs chain exactly as
/// the death skills expect (18670 opens the gate, 19236 opens the exit, the worldgate uses
/// 19616).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_db_cradle_scripted_doodad_templates_and_funcs() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open compact.sqlite3");
    let data = DoodadData::load_from(&conn).expect("doodad data");

    for (template, _, _, _, _) in DOODAD_ROWS {
        data.template(template)
            .unwrap_or_else(|| panic!("missing doodad template {template}"));
        assert!(
            !data.doodad_func_groups(template).is_empty(),
            "template {template} has no func groups"
        );
        assert_ne!(
            phase::get_func_group_id(&data, template),
            0,
            "template {template} start group"
        );
    }

    // 5725: start group 14675 (invisible) is empty; the Use func with skill 19616 lives in the
    // next group (this is the 1.2 data shape, kept as is).
    assert_eq!(phase::get_func_group_id(&data, 5725), 14675);
    assert!(
        data.funcs_for_group(14676)
            .iter()
            .any(|f| f.func_type == "DoodadFuncUse" && f.skill_id == 19616),
        "5725 Use 19616"
    );
    // 5726: closed gate hit by Sprigg's death skill 18670, opens to 14678.
    assert_eq!(phase::get_func_group_id(&data, 5726), 14677);
    assert!(
        data.funcs_for_group(14677).iter().any(|f| {
            matches!(
                data.func_template(f.func_id, &f.func_type),
                Some(FuncTemplate::SkillHit(h)) if h.skill_id == 18670
            )
        }),
        "5726 SkillHit 18670"
    );
    // 5762: closed exit hit by Arkanis's death skill 19236, opens to the ExitIndun func.
    assert_eq!(phase::get_func_group_id(&data, 5762), 14769);
    assert!(
        data.funcs_for_group(14769).iter().any(|f| {
            matches!(
                data.func_template(f.func_id, &f.func_type),
                Some(FuncTemplate::SkillHit(h)) if h.skill_id == 19236
            )
        }),
        "5762 SkillHit 19236"
    );
    assert!(
        data.funcs_for_group(14770)
            .iter()
            .any(|f| matches!(f.func_type.as_str(), "DoodadFuncExitIndun")),
        "5762 exit func"
    );
    // The interactive chest / destruction device / worldgate use their skills.
    for (group, skill) in [(15343, 20229), (14351, 19698)] {
        assert!(
            data.funcs_for_group(group)
                .iter()
                .any(|f| f.func_type == "DoodadFuncUse" && f.skill_id == skill),
            "group {group} Use {skill}"
        );
    }
}

/// Siranus's room: the entry sphere casts 19666, whose `NpcControlEffect` tag 915 commands the
/// invisible NPC 12180; command set 227 uses 19667, which spawns the Royal Stormers 12177-12179
/// through their event spawners and then removes the trigger NPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_siranus_sphere_runs_the_command_set() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let _ = load_cradle(&server, &conn_of(&db));

    let areas: Vec<crate::systems::quests::spheres::SphereArea> =
        crate::systems::quests::spheres::load_sphere_areas(&app_root(), |id| id == 1977)
            .into_iter()
            .filter(|a| a.world == WORLD)
            .collect();
    assert_eq!(areas.len(), 5, "the shipped sphere 1977 fill rows");
    server.systems.quests.spheres.set_areas(areas);

    let _player = place_player(&server, &world, 1, 352.09, 500.29, 174.094);
    for _ in 0..6 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let trigger = npcs_of(&world, 12180);
    assert_eq!(trigger.len(), 1, "the plain trigger row places 12180");

    server.systems.quests.spheres.tick_at(&server, 0);
    assert!(
        ai_queued(&server, trigger[0].obj_id) > 0,
        "sphere 1977 must cast 19666 and queue command set 227 on 12180"
    );

    let mut spawned = false;
    for _ in 0..300 {
        tick_ai(&server, 1);
        if !npcs_of(&world, 12177).is_empty()
            && !npcs_of(&world, 12178).is_empty()
            && !npcs_of(&world, 12179).is_empty()
        {
            spawned = true;
            break;
        }
    }
    assert!(spawned, "command set 227 spawns the three Royal Stormers");
    assert_eq!(spawned_count(&server, 96271), 1);
    assert_eq!(spawned_count(&server, 96273), 1);
    assert_eq!(spawned_count(&server, 96274), 1);
    // 19668 removes the invisible trigger NPC after the scene.
    for _ in 0..300 {
        if npcs_of(&world, 12180).is_empty() {
            break;
        }
        tick_ai(&server, 1);
    }
    assert!(
        npcs_of(&world, 12180).is_empty(),
        "skill 19668 removes the trigger NPC"
    );
}

fn conn_of(db: &str) -> Connection {
    Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// The statue corridor: the entry sphere casts 18272, which applies the 1 s trigger buff 3798
/// to the invisible statue (11268). The buff's Timeout trigger runs skill 18270, which spawns
/// the four Moving Stone Statues 11163/11181/11183/11184 through their event spawners and
/// removes the invisible man.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_statue_sphere_wakes_the_four_statues() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let _ = load_cradle(&server, &conn_of(&db));

    let areas: Vec<crate::systems::quests::spheres::SphereArea> =
        crate::systems::quests::spheres::load_sphere_areas(&app_root(), |id| id == 1546)
            .into_iter()
            .filter(|a| a.world == WORLD)
            .collect();
    assert_eq!(areas.len(), 1, "the shipped sphere 1546 fill row");
    server.systems.quests.spheres.set_areas(areas);

    let _player = place_player(&server, &world, 1, 179.866, 593.683, 174.052);
    for _ in 0..6 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let trigger = npcs_of(&world, 11268);
    assert_eq!(trigger.len(), 1, "the plain trigger row places 11268");

    server.systems.quests.spheres.tick_at(&server, 0);
    let owner = WorldObject::from(&trigger[0]);
    assert!(
        owner.with_unit(|u| u.unit().buffs.check_buff(3798)) == Some(true),
        "sphere 1546 casts 18272 and buffs the invisible statue"
    );
    // Buff 3798 lasts 1 s; its Timeout trigger (skill 18270) fires from the buff timer task.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    for _ in 0..300 {
        tick_ai(&server, 1);
        if !npcs_of(&world, 11163).is_empty()
            && !npcs_of(&world, 11181).is_empty()
            && !npcs_of(&world, 11183).is_empty()
            && !npcs_of(&world, 11184).is_empty()
        {
            break;
        }
    }
    for (spawner, unit) in [
        (88999, 11163),
        (89000, 11181),
        (89001, 11183),
        (89002, 11184),
    ] {
        assert_eq!(
            spawned_count(&server, spawner),
            1,
            "statue spawner {spawner}"
        );
        assert!(
            !npcs_of(&world, unit).is_empty(),
            "statue {unit} from spawner {spawner}"
        );
    }
    assert!(
        npcs_of(&world, 11268).is_empty(),
        "skill 18270 kills the invisible statue"
    );
}

/// Sprigg's death skill 18670 opens the stone gate (doodad 5726 phase 14677 -> 14678) and
/// spawns Captain Kuwon (12165), whose `OnSpawn` command set spawns the two Royal Stormers
/// 12166/12170.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_sprigg_death_opens_the_gate_and_spawns_the_squad() {
    use crate::systems::skills::types::skill_result;
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let _ = load_cradle(&server, &conn_of(&db));
    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    m.spawn_doodads(&server);

    let gate = find_doodad(&world, 5726);
    assert_eq!(gate.data().func_group_id, 14677);

    let _player = place_player(&server, &world, 1, 176.1, 723.5, 182.4);
    for _ in 0..8 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let sprigg = npcs_of(&world, 11169);
    assert_eq!(sprigg.len(), 1, "the plain row places Sprigg 11169");
    // The scene soldiers are hostile to the player; step out of their sight so the command set
    // (which the Attack behavior would pre-empt) runs.
    _player.data().unit.transform.set_xyz(176.1, 790.0, 182.4);
    let sprigg = WorldObject::from(&sprigg[0]);
    // The 1.2 `np_skills` OnDeath row of Sprigg is skill 18670; a death skill is a corpse cast
    // (`source_dead`), so the dying NPC's hp is already 0 when the AI casts it.
    sprigg.with_unit(|u| u.unit_mut().hp = 0);
    let result = crate::systems::skills::skill::unit_use_skill(&server, &sprigg, 18670, &sprigg);
    assert_eq!(result, skill_result::SUCCESS, "Sprigg's OnDeath 18670");
    assert_eq!(
        gate.data().func_group_id,
        14678,
        "18670 must advance the gate to its open phase"
    );
    assert_eq!(spawned_count(&server, 96069), 1, "Kuwon spawner");
    // Kuwon's OnSpawn skill 19643 queues command set 222 on itself. Running the whole set is
    // not asserted: its first commands are FollowPath rows (`commander_move_sprigun_1`), which
    // an event-spawned NPC cannot run without a simulation (see the PORTING.md note); the
    // sphere -> command set -> spawner chain is covered by the Siranus room test.
    let kuwon = npcs_of(&world, 12165);
    assert_eq!(kuwon.len(), 1, "the death skill spawns Captain Kuwon");
    let mut queued = 0;
    for _ in 0..200 {
        queued = ai_queued(&server, kuwon[0].obj_id);
        if queued > 0 {
            break;
        }
        tick_ai(&server, 1);
    }
    assert!(queued > 0, "Kuwon's OnSpawn must queue command set 222");
}

/// Arkanis's death skill 19236 opens the end exit (doodad 5762 phase 14769 -> 14770, the
/// `DoodadFuncExitIndun` phase) and spawns the three Royal Stormers 12185-12187.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_arkanis_death_opens_the_exit_and_spawns_the_squad() {
    use crate::systems::skills::types::skill_result;
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let _ = load_cradle(&server, &conn_of(&db));
    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    m.spawn_doodads(&server);

    let exit = find_doodad(&world, 5762);
    assert_eq!(exit.data().func_group_id, 14769);

    let _player = place_player(&server, &world, 1, 650.9, 393.2, 113.4);
    for _ in 0..8 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let arkanis = npcs_of(&world, 11171);
    assert!(
        !arkanis.is_empty(),
        "the plain row places Arkanis / Metamoze 11171"
    );
    _player.data().unit.transform.set_xyz(650.9, 460.0, 113.4);
    let arkanis = WorldObject::from(&arkanis[0]);
    // A death skill is a corpse cast (`source_dead`), so the dying NPC's hp is already 0.
    arkanis.with_unit(|u| u.unit_mut().hp = 0);
    let result = crate::systems::skills::skill::unit_use_skill(&server, &arkanis, 19236, &arkanis);
    assert_eq!(result, skill_result::SUCCESS, "Arkanis's OnDeath 19236");
    assert_eq!(
        exit.data().func_group_id,
        14770,
        "19236 must advance the exit to the ExitIndun phase"
    );
    for spawner in [96467, 96468, 96469] {
        assert_eq!(
            spawned_count(&server, spawner),
            1,
            "squad spawner {spawner}"
        );
    }
    tick_ai(&server, 10);
    for unit in [12185, 12186, 12187] {
        assert_eq!(npcs_of(&world, unit).len(), 1, "Royal Stormer {unit}");
    }
}
