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
//! 5762 (Arkanis's death skill 19236, then `DoodadFuncExitIndun`). The 15 Carnivore's Egg event
//! rows are kept although their 1.2 spawn-effect rows have no `effects` row (no 1.2 skill can
//! trigger them; see the gated data test and `PORTING.md`).
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

/// The 15 Carnivore's Egg level spawners (the 10.x level has no 88973). Their
/// `npc_spawner_spawn_effects` rows (588, 591-604) have no 1.2 `effects` row, so no 1.2 skill
/// reaches them; the event rows stay (see `PORTING.md`).
const CARNIVORE_EGG_SPAWNERS: &[u32] = &[
    88966, 88967, 88968, 88969, 88970, 88971, 88972, 88974, 88975, 88976, 88977, 88978, 88979,
    88980, 88981,
];

/// The 45 scripted + interactive doodad rows: template, level family, position (Inferred).
#[rustfmt::skip]
const DOODAD_ROWS: [(u32, u32, f32, f32, f32); 45] = [
    (5038, 0, 219.433, 504.340, 178.0),
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
    (5550, 0, 297.226, 741.934, 184.691),
    (5550, 0, 301.526, 742.803, 184.436),
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

    // No decor row was added, and the skill-summoned doodads are never static placements. The
    // mushroom room's Poison Cloud Gate 6002 and walls 5545 / 6050 are placed since the layout
    // follow-up (see `cradle_level_gates_boulder_spikes_and_stones_are_the_authored_rows`).
    assert!(
        !rows.iter().any(|(t, _, _, _, _)| *t == 5320),
        "decor doodad 5320 must not be placed"
    );
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

    // The 15 Carnivore's Egg event rows are kept even though no 1.2 effect reaches them (their
    // spawn-effect rows have no `effects` row; see PORTING.md and the gated data test below).
    let eggs = defs
        .iter()
        .filter(|d| {
            d.npc_spawner_ids.len() == 1
                && CARNIVORE_EGG_SPAWNERS.contains(&d.npc_spawner_ids[0])
                && d.base.unit_id == 11162
        })
        .count();
    assert_eq!(
        eggs,
        CARNIVORE_EGG_SPAWNERS.len(),
        "the egg event rows are kept"
    );

    // The dummy Princess Surya 11555 stays a plain start row: compact template 12794 is its
    // only 1.2 spawner, and the despawn effect's level id 92695 has no spawn effect, so it
    // cannot start an event row instead (see PORTING.md).
    assert_eq!(
        defs.iter()
            .filter(|d| d.npc_spawner_ids.is_empty() && d.base.unit_id == 11555)
            .count(),
        1,
        "the dummy Princess Surya start row"
    );
    assert!(
        !defs.iter().any(|d| d.npc_spawner_ids == [92695]),
        "no event row for the 10.x level spawner 92695"
    );

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
    let localization = crate::data::Localization::load(&conn, "en_us").expect("localization");
    data.factions = crate::data::FactionData::load(&conn, &localization).expect("factions");
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).expect("formulas");
    data.skills = crate::data::skills::SkillData::load(&conn).expect("buff data");
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

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_treasure_chest_keeps_its_reward_when_the_bag_fills() {
    use crate::models::item_task::task_type;
    use crate::systems::economy::inventory::Inv;
    use crate::systems::npcs::doodad::{manager, phase};
    use crate::systems::skills::{skill, types::skill_result};
    use std::time::Duration;

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = super::burntcastle_dungeon_tests::real_encounter_server(&db, WORLD, ZONE);
    let chest = manager::create(&server, &world, 0, 5550, None, true).unwrap();
    chest.data().transform.set_xyz(297.0, 743.0, 185.0);
    manager::spawn(&server, &world, &chest);
    let entrance = manager::create(&server, &world, 0, 5549, None, true).unwrap();
    entrance.data().transform.set_xyz(294.0, 743.0, 185.0);
    manager::spawn(&server, &world, &entrance);
    phase::set_func_group_id(&server, &entrance, 14331);
    let player = place_player(&server, &world, 1, 297.0, 742.0, 185.0);
    let caster = WorldObject::from(&player);
    let target = WorldObject::from(&chest);
    let economy = server.systems.npcs.hooks.economy.get().unwrap();
    Inv::of(&player).bag().lock().unwrap().set_container_size(1);
    let fill = || assert!(economy.acquire_item(&server, &player, task_type::LOOT, 15694, 1, -1));
    let empty = || {
        assert_eq!(
            economy.bag_consume_item(&server, &player, task_type::LOOT, 15694, 1),
            1
        )
    };
    let open = || skill::unit_use_skill(&server, &caster, 19691, &target);

    fill();
    assert_eq!(open(), skill_result::BAG_FULL);
    assert_eq!(chest.data().func_group_id, 14303);
    empty();

    assert_eq!(open(), skill_result::SUCCESS);
    fill();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(chest.data().func_group_id, 14303);
    assert_eq!(economy.item_count(&server, &player, 25660), 0);
    empty();

    assert_eq!(open(), skill_result::SUCCESS);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_ne!(chest.data().func_group_id, 14303);
    assert_eq!(economy.item_count(&server, &player, 25660), 1);
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

    assert_eq!(
        stats.npc_entries, 126,
        "71 kept + 36 event + 11 plain rows + 8 level patrol rows"
    );
    assert_eq!(stats.npc_duplicates, 0);
    assert_eq!(stats.npc_missing_templates, 0);
    assert_eq!(stats.npc_event_spawners, 36);
    assert_eq!(
        stats.npc_event_only_spawners_skipped, 0,
        "no plain statue row reaches an inactive statue spawner"
    );
    assert_eq!(stats.npc_cutscene_spawners_skipped, 0);
    assert_eq!(
        stats.doodad_entries, 213,
        "78 + 114 fire jets + 9 pool hazards + 12 from the layout follow-up (gate 6002 and two \
         walls for two 5395, 12 barriers for five 5516, 22 stones for 18)"
    );
    assert_eq!(stats.doodad_duplicates, 0);
    assert_eq!(stats.doodad_missing_templates, 0);
    assert_eq!(stats.doodad_spawners, 213);

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
    // The dummy Princess Surya starts from compact 12794; the level id the despawn effect names
    // (92695) has no event row, so `GetNpcSpawner(92695)` stays empty (see PORTING.md).
    assert!(
        placed.contains(&12794),
        "dummy Princess Surya start spawner"
    );
    assert!(!placed.contains(&92695));
    for spawner in [13467, 13471] {
        assert!(
            !placed.contains(&spawner),
            "inactive spawner {spawner} must not be placed at start"
        );
    }

    // The doodads spawn (their phase init is asynchronous but the placements are synchronous).
    m.spawn_doodads(&server);
    assert_eq!(world.all_doodads().len(), 213);
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
    for (group, skill) in [(15343, 20229), (14303, 19691), (14351, 19698)] {
        assert!(
            data.funcs_for_group(group)
                .iter()
                .any(|f| f.func_type == "DoodadFuncUse" && f.skill_id == skill),
            "group {group} Use {skill}"
        );
    }
}

/// The 1.2 compact proves why two Cradle residuals are not row mistakes:
///
/// * the 15 Carnivore's Egg spawn-effect rows (588, 591-604) have no `effects` row, so no 1.2
///   skill or buff trigger can reach them; the event rows stay for a future data fix;
/// * skill 19707's `NpcSpawnerDespawnEffect` (row 35) names the 10.x level spawner 92695, which
///   is not a compact spawner and has no spawn effect; the 1.2 start-active template for 11555
///   is 12794, so the dummy Princess Surya stays after the moving princess appears.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_db_cradle_egg_and_surya_gaps_are_the_1_2_data_shape() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = conn_of(&path);

    let mut effect_rows = Vec::new();
    for spawner in CARNIVORE_EGG_SPAWNERS {
        let id: u32 = conn
            .query_row(
                "SELECT id FROM npc_spawner_spawn_effects WHERE spawner_id = ?1",
                [spawner],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| panic!("npc_spawner_spawn_effects row for spawner {spawner}"));
        effect_rows.push(id);
    }
    effect_rows.sort_unstable();
    assert_eq!(
        effect_rows,
        vec![588, 591, 592, 593, 594, 595, 596, 597, 598, 599, 600, 601, 602, 603, 604],
        "the egg spawn-effect rows"
    );
    for id in effect_rows {
        let refs: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM effects \
                 WHERE actual_type = 'NpcSpawnerSpawnEffect' AND actual_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            refs, 0,
            "npc_spawner_spawn_effects {id} has no 1.2 effects row"
        );
    }

    let despawn: u32 = conn
        .query_row(
            "SELECT spawner_id FROM npc_spawner_despawn_effects WHERE id = 35",
            [],
            |r| r.get(0),
        )
        .expect("despawn effect 35");
    assert_eq!(despawn, 92695, "skill 19707 despawns the level spawner id");
    let level_row: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM npc_spawners WHERE id = 92695",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        level_row, 0,
        "92695 is a 10.x level spawner id, not a 1.2 compact template"
    );
    let spawn_effect: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM npc_spawner_spawn_effects WHERE spawner_id = 92695",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        spawn_effect, 0,
        "no 1.2 spawn effect can start an event row 92695 at instance start"
    );
    let compact: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM npc_spawners WHERE id = 12794 AND activation_state = 't'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        compact, 1,
        "11555's 1.2 start-active compact template is 12794"
    );
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
    server
        .systems
        .npcs
        .spawns
        .get(INSTANCE_ID)
        .unwrap()
        .spawn_doodads(&server);
    let gate = find_doodad(&world, 5038);
    assert_eq!(gate.data().func_group_id, 13054);

    let areas: Vec<crate::systems::quests::spheres::SphereArea> =
        crate::systems::quests::spheres::load_sphere_areas(&app_root(), |id| id == 1546)
            .into_iter()
            .filter(|a| a.world == WORLD)
            .collect();
    assert_eq!(areas.len(), 1, "the shipped sphere 1546 fill row");
    server.systems.quests.spheres.set_areas(areas);

    let _player = place_player(&server, &world, 1, 179.866, 593.683, 174.052);
    // A second player waits in the statue room. Before the sphere wakes them, no statue
    // stands there: the level has only the four inactive statue spawners, so the four
    // death skills the gate counts all come from the awakened statues.
    let _waiting = place_player(&server, &world, 2, 185.0, 510.0, 173.3);
    for _ in 0..6 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let statue_count = || {
        [11163, 11181, 11183, 11184]
            .into_iter()
            .map(|unit| npcs_of(&world, unit).len())
            .sum::<usize>()
    };
    assert_eq!(statue_count(), 0, "no statue stands before the sphere");
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
    let spawn_manager = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    let mut statues = Vec::new();
    for (spawner, unit) in [
        (88999, 11163),
        (89000, 11181),
        (89001, 11183),
        (89002, 11184),
    ] {
        let spawners = spawn_manager.get_npc_spawner(spawner);
        let event = spawners.first().expect("statue event spawner");
        assert_eq!(event.current_spawn_count(), 1, "statue spawner {spawner}");
        let statue = event
            .spawned_npcs()
            .into_values()
            .flatten()
            .next()
            .expect("awakened statue");
        assert_eq!(statue.data().unit.template_id, unit);
        statues.push(statue);
    }
    assert!(
        npcs_of(&world, 11268).is_empty(),
        "skill 18270 kills the invisible statue"
    );
    assert_eq!(statue_count(), 4, "only the four awakened statues");

    for (index, npc) in statues.iter().enumerate() {
        npc.data().unit.hp = 0;
        let statue = WorldObject::from(npc);
        assert_eq!(
            crate::systems::skills::skill::unit_use_skill(&server, &statue, 18263, &statue),
            crate::systems::skills::types::skill_result::SUCCESS,
        );
        let expected = if index + 1 == statues.len() {
            13055
        } else {
            13054
        };
        assert_eq!(gate.data().func_group_id, expected);
    }
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

/// The mushroom room (Deathspice Lab) is the level's chain: the five Poisonous Mushrooms 5257
/// (families 80000-80004) feed controller 5406 (family 80010, `DoodadFuncLogic` AND, 1 s),
/// which pulses Poison Cloud Gate 6002 (family 80013). The gate's roll picks the exit: phase
/// 15657 pulses `t` and clears the south wall 5545 (14252 -> 15493), 15658 pulses `f` and
/// clears the north wall 6050 (15656 -> 15659); both walls are family 80012. Four mushrooms
/// open nothing, and the wall that stays keeps its deadly Poison Cloud 4465.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_mushroom_doors_require_all_five_mushrooms() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    // (forced roll, wall that opens and its open phase, wall that stays and its phase, gate)
    for (roll, opened, kept, gate_phase) in [
        (0, (5545, 15493), (6050, 15656), 15657),
        (5000, (6050, 15659), (5545, 14252), 15658),
    ] {
        mushroom_room_opens_one_wall(&db, roll, opened, kept, gate_phase).await;
    }
}

async fn mushroom_room_opens_one_wall(
    db: &str,
    roll: i32,
    (opened, open_phase): (u32, u32),
    (kept, kept_phase): (u32, u32),
    gate_phase: u32,
) {
    use crate::systems::skills::state::SkillRun;
    use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster};

    let (server, world) = real_server(db);
    load_cradle(&server, &conn_of(db));
    server
        .systems
        .npcs
        .spawns
        .get(INSTANCE_ID)
        .unwrap()
        .spawn_doodads(&server);
    let doodads = world.all_doodads();
    let matching = |template_id| {
        doodads
            .iter()
            .filter(move |d| d.data().template_id == template_id)
    };
    let mut mushrooms: Vec<_> = matching(5257).cloned().collect();
    assert_eq!(mushrooms.len(), 5);
    assert_eq!(matching(5395).count(), 0, "no inferred stone gate");
    let gate = find_doodad(&world, 6002);
    let opened = find_doodad(&world, opened);
    let kept = find_doodad(&world, kept);
    assert_eq!(gate.data().family_id, 80013);
    assert_eq!(opened.data().family_id, 80012);
    assert_eq!(kept.data().family_id, 80012);
    let walls_closed = || {
        gate.data().func_group_id == 15491
            && matching(5545).all(|d| d.data().func_group_id == 14252)
            && matching(6050).all(|d| d.data().func_group_id == 15656)
    };
    assert!(
        walls_closed(),
        "the gate's ratio rows do not move it at spawn"
    );
    // The roll the gate makes when the controller's pulse arrives (forced after spawning).
    server
        .systems
        .npcs
        .doodads
        .set_forced_phase_ratio(Some(roll));

    let last = mushrooms.pop().unwrap();
    let at = last.data().transform;
    let player = place_player(&server, &world, 1, at.x + 2.0, at.y, at.z);
    let me = WorldObject::from(&player);
    let npc_hooks = server.systems.skills.ext.npcs();
    for mushroom in mushrooms {
        npc_hooks.doodad_skill_hit(&server, &WorldObject::from(&mushroom), &me, 20212);
        assert_eq!(mushroom.data().func_group_id, 13635);
    }
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    assert!(walls_closed(), "four mushrooms open nothing");

    // Other nearby objects must not fill the purifier's five targets.
    for i in 0..6_u16 {
        let clutter = super::doodad::manager::create(&server, &world, 0, 5252, None, true).unwrap();
        clutter
            .data()
            .transform
            .set_xyz(at.x + f32::from(i), at.y, at.z);
        super::doodad::manager::spawn(&server, &world, &clutter);
    }
    let skill = |id| {
        SkillRun::new(
            server.game_data.systems.skills.skill(id).unwrap().clone(),
            1,
        )
    };
    let caster = SkillCaster::Unit(player.obj_id);
    let (result, _) = crate::systems::skills::skill::use_skill(
        &server,
        &skill(20211),
        &me,
        caster,
        &mut SkillCastTarget::Unit(player.obj_id),
        None,
        true,
    );
    assert_eq!(result, skill_result::SUCCESS);
    assert!(player.data().unit.buffs.check_buff(4788));
    let mut landing = SkillCastTarget::Position {
        obj_id: player.obj_id,
        x: at.x + 1.5,
        y: at.y,
        z: at.z,
        rot: 0.0,
        obj_id1: 0,
        obj_id2: 0,
    };
    let (result, _) = crate::systems::skills::skill::use_skill(
        &server,
        &skill(20212),
        &me,
        caster,
        &mut landing,
        None,
        true,
    );
    assert_eq!(result, skill_result::SUCCESS);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(last.data().func_group_id, 13635);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(gate.data().func_group_id, gate_phase, "roll {roll}");
    assert_eq!(opened.data().func_group_id, open_phase, "roll {roll}");
    assert_eq!(kept.data().func_group_id, kept_phase, "roll {roll}");

    // The open wall lets a player through; the other one still kills.
    let open_at = opened.data().transform;
    let kept_at = kept.data().transform;
    let through = place_player(&server, &world, 2, open_at.x, open_at.y, open_at.z);
    let blocked = place_player(&server, &world, 3, kept_at.x, kept_at.y, kept_at.z);
    for _ in 0..3 {
        server.systems.npcs.area_triggers.tick(&server);
        tokio::time::sleep(std::time::Duration::from_millis(210)).await;
    }
    assert!(!through.data().unit.buffs.check_buff(4465), "roll {roll}");
    assert_eq!(through.data().unit.hp, 10_000, "roll {roll}");
    let poisoned = {
        let d = blocked.data();
        d.unit.buffs.check_buff(4465) || d.unit.hp == 0
    };
    assert!(poisoned, "roll {roll}: the closed wall still poisons");
}

/// The stair room's Impaling Barriers 5423 (1.5 s) and 5424 (2 s) are the level's twelve
/// placements. Each pulse (clout 1266 / 1267, 200 ms, fixed physical 1500-2500) damages a
/// player on it and roots them with Impaled 4351 for its full 2 s; the barrier then rests in
/// its timer phase again.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_spike_pulse_damages_and_leaves_the_injury() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    load_cradle(&server, &conn_of(&db));
    server
        .systems
        .npcs
        .spawns
        .get(INSTANCE_ID)
        .unwrap()
        .spawn_doodads(&server);
    assert!(
        !world
            .all_doodads()
            .iter()
            .any(|d| d.data().template_id == 5516),
        "no Howling Abyss gate trap in the Cradle"
    );
    for (pid, (template, pulse, rest)) in [(5423, 14014, 14013), (5424, 14016, 14015)]
        .into_iter()
        .enumerate()
    {
        let spike = find_doodad(&world, template);
        assert_eq!(spike.data().func_group_id, rest, "{template} rests");
        let pos = spike.data().transform;
        let player = place_player(&server, &world, pid as u32 + 1, pos.x, pos.y, pos.z);
        super::doodad::manager::do_change_phase(&server, &world, &spike, None, pulse);
        {
            let data = player.data();
            assert!(data.unit.hp < 10_000, "{template} pulse damages");
            assert!(data.unit.buffs.check_buff(4351), "{template} impales");
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert_eq!(
            spike.data().func_group_id,
            rest,
            "{template} back to its timer"
        );
        assert!(
            player.data().unit.buffs.check_buff(4351),
            "{template}: Impaled outlasts the 200 ms pulse"
        );
    }
}

/// The secret room stays sealed, as the 2014 videos show. Its boulder 5547 has no family: the
/// level's family 80022 would wire it to the Destruction Device 5549, whose "Open Secret Room"
/// use (phase 14331: `Pulse(t)` to family 80022) breaks it (14281 -> 14287).
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_secret_room_boulder_ignores_the_device() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    load_cradle(&server, &conn_of(&db));
    server
        .systems
        .npcs
        .spawns
        .get(INSTANCE_ID)
        .unwrap()
        .spawn_doodads(&server);
    let boulder = find_doodad(&world, 5547);
    let device = find_doodad(&world, 5549);
    assert_eq!(boulder.data().func_group_id, 14281, "closed");
    assert_eq!(boulder.data().family_id, 0);
    assert_eq!(device.data().family_id, 80020);
    let at = device.data().transform;
    let player = place_player(&server, &world, 1, at.x + 1.0, at.y, at.z);
    let me = WorldObject::from(&player);
    super::doodad::manager::do_change_phase(&server, &world, &device, Some(&me), 14331);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(boulder.data().func_group_id, 14281, "the room stays sealed");

    // With the level's family the same pulse opens it: the missing family is what seals it.
    boulder.data().family_id = 80022;
    super::doodad::manager::do_change_phase(&server, &world, &device, Some(&me), 14304);
    super::doodad::manager::do_change_phase(&server, &world, &device, Some(&me), 14331);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(boulder.data().func_group_id, 14287, "the device's chain");
}

#[path = "cradle_event_tests.rs"]
mod events;

/// A placed fire jet's flame phase (5250: timer 13551 -> clout 13552, 7 s each) applies Fire
/// Pillar 3996 (300 ms, fixed fire damage 1500-2000 per tick) once to a player who steps into
/// it. Standing on the grate does not renew it: the next hit comes with the jet's next flame
/// phase, which is a new area trigger. A player 4 m away is never hit.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_fire_jet_flame_hits_once_per_entry() {
    use std::time::Duration;

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    let doodad_tasks = &server.systems.npcs.doodads.tasks;
    doodad_tasks.set_manual(true);
    load_cradle(&server, &conn_of(&db));
    // Isolate one authored jet so neighbouring hazards cannot affect either player.
    let jet = server
        .systems
        .npcs
        .spawns
        .get(INSTANCE_ID)
        .unwrap()
        .get_doodad_spawners_by_unit_id(5250)
        .into_iter()
        .min_by_key(|spawner| spawner.id)
        .expect("placed 7 s fire jet")
        .spawn(&server, 0)
        .expect("spawned fire jet");
    let pos = jet.data().transform;
    let player = place_player(&server, &world, 1, pos.x + 0.3, pos.y, pos.z);
    let bystander = place_player(&server, &world, 2, pos.x + 4.0, pos.y, pos.z);
    super::doodad::manager::do_change_phase(&server, &world, &jet, None, 13552);
    assert_eq!(jet.data().func_group_id, 13552, "flame phase");
    let triggers = &server.systems.npcs.area_triggers;
    let advance = |ms| {
        doodad_tasks.advance(&server, ms);
        tokio::time::sleep(Duration::from_millis(ms))
    };
    triggers.tick(&server);
    assert!(
        player.data().unit.buffs.check_buff(3996),
        "Fire Pillar is applied on entry"
    );
    // Fire Pillar's 200 ms timer can take three callbacks to expire. Let Tokio
    // auto-advance through each callback before observing the buff at 630 ms.
    advance(630).await;
    let after_entry = player.data().unit.hp;
    assert!(after_entry < 10_000, "the flame burns on entry");
    assert!(
        !player.data().unit.buffs.check_buff(3996),
        "the hit has expired"
    );
    // About 2 s more on the grate, still in the same flame phase: no further hit.
    for _ in 0..10 {
        advance(210).await;
        triggers.tick(&server);
    }
    assert_eq!(jet.data().func_group_id, 13552, "still the same flame");
    assert_eq!(
        player.data().unit.hp,
        after_entry,
        "no second hit while standing"
    );
    assert!(
        !player.data().unit.buffs.check_buff(3996),
        "standing does not reapply Fire Pillar"
    );
    assert_eq!(bystander.data().unit.hp, 10_000, "4 m away is safe");
}
// ---------------------------------------------------------------------------------------------
// World and hazards (tester follow-up 2026-11-22)
// ---------------------------------------------------------------------------------------------

/// The Kitchen of Fearful Tastes has one Putrid Demon: the level spawner 88939 (compact
/// 12784, NPC 11552 "placed in the Kitchen") with its patrol `AIPath_264_nopeople_496`. The
/// 1.2-era 11357 row 0.7 m away was a second demon on the same spot.
#[test]
fn cradle_kitchen_has_one_patrolling_putrid_demon() {
    let defs = npc_spawn_defs();
    let demons: Vec<_> = defs
        .iter()
        .filter(|d| {
            let p = &d.base.position;
            matches!(d.base.unit_id, 11357 | 11552 | 12389)
                && d.npc_spawner_ids.is_empty()
                && (p.x - 710.0).hypot(p.y - 744.0) < 10.0
        })
        .collect();
    assert_eq!(demons.len(), 1, "one demon in the Kitchen");
    assert_eq!(demons[0].base.unit_id, 11552);
    assert_eq!(demons[0].base.follow_path, KITCHEN_DEMON_PATH);
}

const KITCHEN_DEMON_PATH: &str = "AIPath_264_nopeople_496";

/// The statue room's gate counts four statue death skills (18263, `act_count` 4). The level
/// has only the four inactive statue spawners 88999-89002 that sphere 1546 wakes, so no
/// statue may start as a plain row: the five 1.2-era 11163 rows let the gate open after any
/// four of nine statues.
#[test]
fn cradle_statue_room_has_only_the_four_awakened_statues() {
    let defs = npc_spawn_defs();
    for unit in [11163, 11181, 11183, 11184] {
        let rows: Vec<_> = defs.iter().filter(|d| d.base.unit_id == unit).collect();
        assert_eq!(rows.len(), 1, "statue {unit}");
        assert_eq!(
            rows[0].npc_spawner_ids.len(),
            1,
            "statue {unit} is an event row"
        );
    }
}

/// The fire jets, pool hazards and rowboats are the level placements of the Kroloal Cradle
/// editor data (`level_design/cells/000_000/doodad.g`, `zone/264/editor/slave.xml`). Each jet
/// sits on the r208022 floor (see the collision real-data test). The jets PR #155 inferred
/// before the Dreadkiss Sitting Room, and the area fire trap 5271, are not in the level.
#[test]
fn cradle_level_jets_hazards_and_boats_are_the_authored_placements() {
    let defs = doodad_spawn_defs();
    let count = |template: u32| defs.iter().filter(|d| d.base.unit_id == template).count();
    assert_eq!(count(5246), 30, "Fire Pillar (3 s)");
    assert_eq!(count(5249), 37, "Fire Pillar (5 s)");
    assert_eq!(count(5250), 47, "Fire Pillar (7 s)");
    assert_eq!(count(5271), 0, "no area fire trap in the level");
    let jets: Vec<_> = defs
        .iter()
        .filter(|d| matches!(d.base.unit_id, 5246 | 5249 | 5250))
        .map(|d| d.base.position)
        .collect();
    let in_box = |p: &crate::data::world_templates::WorldSpawnPosition,
                  (x0, y0, x1, y1): (f32, f32, f32, f32)| {
        p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1
    };
    // The four jet fields: the tunnel to Bedlam Quarters, the passage after the Kitchen, and
    // the two fields of the northern passage.
    for (field, area, expected) in [
        ("Bedlam tunnel", (180.0, 780.0, 230.0, 840.0), 41),
        ("after the Kitchen", (595.0, 740.0, 650.0, 800.0), 42),
        ("northern passage", (750.0, 880.0, 795.0, 930.0), 27),
        ("northern passage west", (730.0, 920.0, 745.0, 932.0), 4),
    ] {
        let n = jets.iter().filter(|p| in_box(p, area)).count();
        assert_eq!(n, expected, "{field} jets");
    }
    assert!(
        !jets.iter().any(|p| in_box(p, (540.0, 830.0, 575.0, 850.0))),
        "no jet before the Dreadkiss Sitting Room"
    );

    // Pool hazards: template, authored position, and the buff the clout applies.
    for (template, x, y, z) in [
        (5055, 409.576, 835.929, 169.642), // Toxicity 3813, Dreadkiss pool
        (5439, 561.262, 510.808, 169.128), // Cursed Blood 4362, corridor before the stairs
        (5439, 654.728, 550.134, 101.63),  // Cursed Blood 4362, stair room
        (5889, 652.027, 516.924, 89.891),  // Cursed Blood 4362, stair room
        (5875, 183.417, 614.619, 163.462), // Poison 4483, stepping stones
        (5877, 180.969, 567.052, 161.476), // Poison 4483, stepping stones
        (5878, 319.915, 508.742, 161.476), // Poison 4483, Soulscour Bathroom
        (5885, 316.32, 825.412, 142.59),   // Poison 4483, Death's Door
        (5893, 656.929, 391.709, 21.8444), // Lava 4683, Arkanis's room
    ] {
        let n = defs
            .iter()
            .filter(|d| {
                let p = &d.base.position;
                d.base.unit_id == template
                    && near(p.x, x, 0.001)
                    && near(p.y, y, 0.001)
                    && near(p.z, z, 0.001)
            })
            .count();
        assert_eq!(n, 1, "hazard {template} at ({x}, {y}, {z})");
    }
    for template in [5885, 5893] {
        assert_eq!(count(template), 1, "one {template} placement");
    }

    // Rowboats: the Dreadkiss pool and the red-water corridor before the stair room.
    let text =
        std::fs::read_to_string(world_dir().join("slave_spawns.json")).expect("slave_spawns");
    let boats = parse_list(
        &text,
        crate::systems::npcs::doodad::spawner::SimpleSpawnerDef::from_json,
    )
    .expect("slave_spawns parses");
    let boats: Vec<_> = boats
        .iter()
        .map(|b| (b.base.unit_id, b.base.position))
        .collect();
    assert_eq!(boats.len(), 2);
    for (x, y, yaw) in [(430.083, 843.409, 87.0), (550.424, 510.19, -82.0)] {
        assert!(
            boats.iter().any(|(unit, p)| *unit == 15
                && near(p.x, x, 0.001)
                && near(p.y, y, 0.001)
                && near(p.yaw, yaw, 0.1)),
            "rowboat at ({x}, {y})"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Layout follow-up (2026-11-22): patrols, mushroom-room walls, secret boulder, barriers, stones
// ---------------------------------------------------------------------------------------------

/// The 44 level spawners with a designer patrol (`zone/264/editor/npc_spawners.g` of the
/// ArcheAge Returns r575 level): level spawner, the row's NPC (the compact spawner's member),
/// the level spawn and yaw, and the number of `AIPath_264_nopeople_<n>`. 36 rows were the
/// 1.2-era rows of the same NPC in the same room, eight are added (four Cinderhorn Boars,
/// three Broken Toys, the Venomous Marksman 88945).
#[rustfmt::skip]
const PATROL_ROWS: [(u32, u32, f32, f32, f32, f32, u32); 44] = [
    (88922, 11357, 807.042, 842.292, 192.504, -94.0, 1),
    (88973, 11557, 590.053, 845.057, 182.688, -142.0, 22),
    (92937, 12160, 168.485, 748.184, 180.188, 0.0, 82),
    (92938, 12160, 159.653, 745.58, 179.49, 0.0, 83),
    (92939, 12160, 154.557, 739.158, 180.067, 0.0, 84),
    (92943, 12160, 178.195, 757.619, 179.168, 0.0, 85),
    (92940, 12160, 191.952, 748.427, 179.514, 0.0, 86),
    (92942, 12160, 197.362, 750.317, 178.834, 0.0, 87),
    (92941, 12160, 197.335, 744.567, 179.362, 0.0, 88),
    (94649, 11357, 308.613, 867.066, 163.748, -117.0, 172),
    (92508, 11357, 287.899, 869.337, 165.965, 98.0, 173),
    (92493, 11357, 279.328, 795.247, 181.8, 51.0, 174),
    (88924, 11357, 768.424, 911.242, 198.487, -153.0, 206),
    (92488, 11365, 763.249, 915.39, 200.0, -115.0, 207),
    (92487, 11365, 760.714, 920.465, 201.461, -134.0, 208),
    (88925, 11365, 752.782, 922.789, 201.135, -122.0, 209),
    (88947, 11365, 766.747, 761.979, 184.179, -43.0, 210),
    (88946, 11357, 764.242, 761.216, 183.304, -43.0, 211),
    (88948, 11365, 749.291, 750.792, 178.537, -55.0, 212),
    (92490, 11365, 621.576, 759.516, 181.27, 0.0, 213),
    (92491, 11365, 611.967, 769.308, 181.824, -142.0, 214),
    (88942, 11365, 599.48, 790.531, 182.666, -166.0, 215),
    (88945, 11155, 612.772, 914.777, 197.976, -38.0, 216),
    (88944, 11365, 615.271, 914.929, 197.233, -56.0, 217),
    (92489, 11365, 597.579, 889.024, 189.06, 0.0, 218),
    (88992, 11357, 214.185, 826.343, 180.674, -71.0, 219),
    (92497, 11365, 193.669, 804.856, 180.981, -35.0, 220),
    (88993, 11357, 183.92, 782.626, 179.449, -59.0, 221),
    (92494, 11365, 191.195, 804.077, 181.097, 0.0, 222),
    (98020, 11357, 646.329, 747.943, 176.204, 0.0, 290),
    (98021, 11357, 613.165, 773.722, 180.25, 0.0, 291),
    (98022, 11365, 787.801, 887.98, 194.181, 0.0, 292),
    (98023, 11357, 729.887, 930.329, 200.954, 0.0, 293),
    (98025, 11365, 206.784, 814.883, 180.141, 0.0, 294),
    (98026, 11365, 196.967, 808.982, 181.27, 0.0, 352),
    (88915, 11156, 792.6, 846.579, 192.534, 11.0, 427),
    (102900, 11357, 767.199, 839.485, 192.687, 0.0, 492),
    (92705, 11357, 706.792, 929.671, 199.783, 0.0, 493),
    (88939, 11552, 709.579, 744.144, 173.076, -46.0, 496),
    (89004, 11164, 248.289, 487.329, 172.948, 66.0, 497),
    (89005, 11164, 248.396, 517.807, 173.439, 90.0, 498),
    (92904, 11164, 493.997, 521.91, 181.018, 75.0, 499),
    (92898, 11164, 500.264, 492.827, 180.895, 0.0, 500),
    (92897, 11357, 487.898, 504.139, 180.663, 0.0, 501),
];

fn patrol_path(n: u32) -> String {
    format!("AIPath_264_nopeople_{n}")
}

/// Every level patrol has exactly one start row at its level spawn, with its path, and no
/// other row patrols.
#[test]
fn cradle_level_patrols_are_placed_once_each_with_their_path() {
    let defs = npc_spawn_defs();
    for (spawner, unit, x, y, z, yaw, n) in PATROL_ROWS {
        let path = patrol_path(n);
        let rows: Vec<_> = defs.iter().filter(|d| d.base.follow_path == path).collect();
        assert_eq!(rows.len(), 1, "{path} (level {spawner})");
        let d = rows[0];
        let p = &d.base.position;
        assert!(d.npc_spawner_ids.is_empty(), "{path}: a start row");
        assert_eq!(d.base.unit_id, unit, "{path}");
        assert!(
            near(p.x, x, 0.001) && near(p.y, y, 0.001) && near(p.z, z, 0.001),
            "{path}: {p:?}"
        );
        assert!(near(p.yaw, yaw, 0.01), "{path}: yaw {}", p.yaw);
    }
    assert_eq!(
        defs.iter()
            .filter(|d| !d.base.follow_path.is_empty())
            .count(),
        PATROL_ROWS.len()
    );
}

/// The patrols are designer paths of the r208022 client's zone 264 `areasmission0.bai`, and
/// each starts at its row (the level spawn is the path's first point). 21 are closed loops
/// (the Kitchen demon's among them), 23 are one-way routes the server walks out and back.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_client_cradle_level_patrols_start_at_their_spawns() {
    use crate::systems::ai::framework::{designer_patrol_cycle, DESIGNER_PATH_OPEN_GAP};
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let files = ClientFileManager::from_sources(&[pak], &app_root());
    let data = files
        .read_file(&format!("game/worlds/{WORLD}/zone/264/areasmission0.bai"))
        .expect("zone 264 areasmission0.bai");
    let areas = crate::systems::ai::geodata::bai::AreasMission::read(
        &data,
        264,
        crate::systems::skills::controllers::Vec3::new(0.0, 0.0, 0.0),
        &Default::default(),
    )
    .expect("areas parse");
    let mut open = 0;
    for (spawner, _, x, y, z, _, n) in PATROL_ROWS {
        let name = patrol_path(n);
        let path = areas
            .designer_paths
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{name} (level {spawner}) is in the r208022 client"));
        let first = path.points[0];
        let last = path.points[path.points.len() - 1];
        assert!(path.points.len() >= 4, "{name}");
        assert!(
            (first.x - x).hypot(first.y - y) < 2.0,
            "{name} starts at its spawn: {first:?}"
        );
        assert!((first.z - z).abs() < 0.5, "{name}: {first:?} vs Z {z}");
        if (first.x - last.x).hypot(first.y - last.y) > DESIGNER_PATH_OPEN_GAP {
            open += 1;
            let cycle = designer_patrol_cycle(path.points.clone());
            assert_eq!(
                cycle.len(),
                2 * path.points.len() - 2,
                "{name} out and back"
            );
        }
    }
    assert_eq!(open, 23, "one-way routes");

    // The Kitchen demon's loop stays in the Kitchen, on its floor.
    let kitchen = areas
        .designer_paths
        .iter()
        .find(|p| p.name == KITCHEN_DEMON_PATH)
        .expect("the Kitchen demon's designer path");
    assert_eq!(kitchen.points.len(), 27);
    let first = kitchen.points[0];
    let last = kitchen.points[kitchen.points.len() - 1];
    assert!((first.x - last.x).hypot(first.y - last.y) < 1.5, "a loop");
    for p in &kitchen.points {
        assert!(
            (p.x - 695.0).hypot(p.y - 744.0) < 25.0,
            "{p:?} in the Kitchen"
        );
        assert!((p.z - 173.0).abs() < 1.0, "{p:?} on the Kitchen floor");
    }
}

/// The level's mushroom-room chain, secret boulder, stair barriers and stepping stones replace
/// PR #155's guesses: the gate 6002 and the walls 5545 / 6050 for the two stone gates 5395, the
/// boulder 20 m further north at full scale, twelve barriers 5423 / 5424 for five Howling Abyss
/// gate traps 5516, and 22 stones for 18.
#[test]
fn cradle_level_gates_boulder_spikes_and_stones_are_the_authored_rows() {
    let defs = doodad_spawn_defs();
    let rows =
        |template: u32| -> Vec<_> { defs.iter().filter(|d| d.base.unit_id == template).collect() };
    for template in [5395, 5516] {
        assert!(rows(template).is_empty(), "no {template}");
    }
    // Template, family, level position (yaw 0, scale 1).
    for (template, family, x, y, z) in [
        (5406, 80010, 813.06, 844.007, 192.896),
        (6002, 80013, 812.817, 840.877, 192.904),
        (5545, 80012, 789.123, 807.109, 193.024),
        (6050, 80012, 787.802, 873.579, 192.201),
        // The level's family 80022 is left out: it would let device 5549 open the room.
        (5547, 0, 301.536, 778.214, 184.777),
    ] {
        let r = rows(template);
        assert_eq!(r.len(), 1, "{template}");
        let p = &r[0].base.position;
        assert_eq!(r[0].family_id, family, "{template}");
        assert!(
            near(p.x, x, 0.001) && near(p.y, y, 0.001) && near(p.z, z, 0.001),
            "{template}: {p:?}"
        );
        assert!(near(r[0].scale, 1.0, 1e-6), "{template} at scale 1");
    }
    assert_eq!(rows(5423).len(), 8, "Impaling Barrier (1.5 s)");
    assert_eq!(rows(5424).len(), 4, "Impaling Barrier (2 s)");
    let stones = rows(5880);
    assert_eq!(stones.len(), 22);
    let in_box = |(x0, y0, x1, y1): (f32, f32, f32, f32)| {
        stones
            .iter()
            .filter(|d| {
                let p = &d.base.position;
                p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1
            })
            .count()
    };
    assert_eq!(in_box((170.0, 545.0, 190.0, 625.0)), 13, "the west pools");
    assert_eq!(
        in_box((300.0, 495.0, 340.0, 515.0)),
        7,
        "Soulscour Bathroom"
    );
    assert_eq!(in_box((305.0, 810.0, 320.0, 830.0)), 2, "Death's Door");
}

/// The r208022 collision under the layout rows. The patrol spawns, the mushroom-room chain and
/// the twelve barriers stand on the floor. The secret room (chests at (297, 742)) is reachable
/// from the passage below it without the boulder, and sealed with the boulder's model
/// (`stone_door_2`, a 35 x 31 x 51 m cave rock) at its row. The mushroom room leads on only
/// through an open Poison Cloud wall, and either wall alone is a way through. Each stepping
/// stone (`stop_rock`, its top 10.87 m above the origin) stands out of its pool's water.
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_client_cradle_layout_rows_stand_on_the_r208022_floor() {
    use crate::systems::ai::collision::{self, objects, world::ObjKind};
    use crate::systems::skills::controllers::Vec3 as UnitVec3;
    use aaemu_physics::math::{Quat, Vec3};
    let Some(server) = super::howling_world_tests::pak_server() else {
        return;
    };
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(WORLD)
        .unwrap()
        .clone();
    let world = server.world.create_world_instance(Arc::new(template), 100);
    let id = world.id;
    let pw = collision::phys_world(&server, id).expect("collision");
    collision::ensure_box(
        &server,
        &pw,
        Vec3::new(120.0, 450.0, 60.0),
        Vec3::new(900.0, 960.0, 260.0),
    );
    let water = crate::systems::ai::physics::world_water(&server, &world);
    let floor = |x: f32, y: f32, z: f32| {
        collision::ground_height_probe(&server, id, x, y, z, 0.5, 5.0)
            .flatten()
            .unwrap_or(f32::NAN)
    };

    // The level spawns sit on the floor or up to 0.6 m above it (the walkers are grounded on
    // spawn), never below it.
    for (spawner, _, x, y, z, _, n) in PATROL_ROWS {
        let f = floor(x, y, z);
        assert!(
            z - f > -0.05 && z - f < 0.6,
            "patrol {n} (level {spawner}): floor {f} vs {z}"
        );
    }
    let defs = doodad_spawn_defs();
    for d in defs
        .iter()
        .filter(|d| matches!(d.base.unit_id, 5406 | 6002 | 5545 | 6050 | 5423 | 5424))
    {
        let p = &d.base.position;
        let f = floor(p.x, p.y, p.z);
        assert!(
            (f - p.z).abs() < 0.05,
            "{} at ({}, {}, {}): floor {f}",
            d.base.unit_id,
            p.x,
            p.y,
            p.z
        );
    }

    let files = collision::files(&server).expect("client files");
    let models = collision::model_cache(&server, &files);
    let shape = |model: &str| {
        objects::resolve_shape(
            &files,
            &models,
            &mut Default::default(),
            &objects::parse_model_ref(model),
        )
        .expect(model)
    };
    let mut next_id = 990_000;
    let mut place = |model: &str, p: &crate::data::world_templates::WorldSpawnPosition| {
        let key = objects::shape_key(
            model,
            Vec3::new(p.x, p.y, p.z),
            Quat::IDENTITY,
            1.0,
            objects::collision_flags(false, false, false),
        );
        next_id += 1;
        pw.lock()
            .set_object_shape(ObjKind::Doodad, next_id, key, Some(&shape(model)), false);
    };

    // The secret room: a walk from the patrol spawn of `AIPath_264_nopeople_174` (the passage
    // north of the room) reaches the chests only while the boulder is absent.
    let boulder = defs.iter().find(|d| d.base.unit_id == 5547).unwrap();
    let bp = &boulder.base.position;
    let f = floor(bp.x, bp.y, bp.z + 1.0);
    assert!((f - bp.z).abs() < 1.5, "boulder on its ledge: floor {f}");
    let start = [279.328, 795.247, 181.8];
    let chests = [297.226, 741.934];
    assert!(
        walks_to(&server, id, start, chests, SECRET_AREA, &[]),
        "the room without the boulder"
    );
    place(
        "prefab://prefabs/interaction_d.xml/cradle_of_destruction.stone_door_2",
        bp,
    );
    assert!(
        !walks_to(&server, id, start, chests, SECRET_AREA, &[]),
        "the boulder seals the room"
    );

    // The mushroom room: with both Poison Cloud walls standing (their clouts' 14 x 4 m box,
    // taken as 7 x 2 m half sizes) nothing leads on to the Carnivore's Egg room. Either wall
    // alone opens a way there, so the gate's roll never strands a party.
    let wall = |template: u32| {
        let p = defs
            .iter()
            .find(|d| d.base.unit_id == template)
            .unwrap()
            .base
            .position;
        [p.x, p.y, 7.0, 2.0]
    };
    let (south, north) = (wall(5545), wall(6050));
    let lab = [795.0, 840.0, 192.6];
    let eggs = [590.0, 845.0];
    let east = [540.0, 680.0, 900.0, 980.0];
    assert!(
        !walks_to(&server, id, lab, eggs, east, &[south, north]),
        "both walls close the room"
    );
    assert!(
        walks_to(&server, id, lab, eggs, east, &[north]),
        "the south exit"
    );
    assert!(
        walks_to(&server, id, lab, eggs, east, &[south]),
        "the north exit"
    );

    // Stepping stones: the top of each stone is above its pool's surface, below a step up.
    for d in defs.iter().filter(|d| d.base.unit_id == 5880) {
        let p = d.base.position;
        place(
            "prefab://prefabs/interaction_d.xml/cradle_of_destruction.stop_rock",
            &p,
        );
        let (surface, _) = water.get_water_surface(UnitVec3::new(p.x, p.y, p.z));
        assert!(
            water.is_water(UnitVec3::new(p.x, p.y, p.z)).is_some(),
            "{p:?} in a pool"
        );
        let top = collision::ground_height_probe(&server, id, p.x, p.y, surface + 3.0, 0.0, 3.0)
            .flatten()
            .unwrap_or(f32::NAN);
        eprintln!(
            "stone ({}, {}): top {top}, {:.2} m above the surface",
            p.x,
            p.y,
            top - surface
        );
        assert!(
            top - surface > 0.15 && top - surface < 1.5,
            "stone ({}, {}, {}): top {top}, surface {surface}",
            p.x,
            p.y,
            p.z
        );
    }
}

/// The secret room and the passages around it: `[x0, y0, x1, y1]`.
const SECRET_AREA: [f32; 4] = [120.0, 650.0, 380.0, 900.0];

/// Whether a walker on the r208022 collision gets from `start` to within 2 m of `goal` inside
/// `area` (`[x0, y0, x1, y1]`): a flood over a 1 m grid, each step at most 1 m up or 2.5 m
/// down, with the straight line between the two floor points clear at knee and head height,
/// and never into a `closed` box (`[centre x, centre y, half x, half y]`).
fn walks_to(
    server: &GameServer,
    id: u32,
    start: [f32; 3],
    goal: [f32; 2],
    area: [f32; 4],
    closed: &[[f32; 4]],
) -> bool {
    use crate::systems::ai::collision::{self, query};
    let mut seen = HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    let Some(Some(z)) =
        collision::ground_height_probe(server, id, start[0], start[1], start[2], 1.0, 3.0)
    else {
        return false;
    };
    queue.push_back([start[0], start[1], z]);
    while let Some([x, y, z]) = queue.pop_front() {
        if (x - goal[0]).hypot(y - goal[1]) < 2.0 {
            return true;
        }
        if seen.len() > 150_000 {
            return false;
        }
        for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
            let (nx, ny) = (x + dx, y + dy);
            if !(area[0]..area[2]).contains(&nx)
                || !(area[1]..area[3]).contains(&ny)
                || closed
                    .iter()
                    .any(|b| (nx - b[0]).abs() < b[2] && (ny - b[1]).abs() < b[3])
            {
                continue;
            }
            let Some(Some(nz)) = collision::ground_height_probe(server, id, nx, ny, z, 1.0, 2.5)
            else {
                continue;
            };
            let key = (nx as i32, ny as i32, (nz * 2.0).round() as i32);
            if seen.contains(&key) {
                continue;
            }
            let blocked = [0.9_f32, 1.7].iter().any(|h| {
                collision::raycast(
                    server,
                    id,
                    [x, y, z + h],
                    [nx, ny, nz + h],
                    query::mask::STATIC_TERRAIN,
                    query::ray_flags::DEFAULT,
                )
                .flatten()
                .is_some()
            });
            if !blocked {
                seen.insert(key);
                queue.push_back([nx, ny, nz]);
            }
        }
    }
    false
}
