//! Quest-spawn data additions of 2026-11-12 (audit `quest-spawn-gaps-2026-11-12`).
//!
//! Two batches are covered:
//! * B1: the Burnt Castle Armory / Hadir Farm mentor-daily reward chests (doodads 7659 and
//!   7661). The 10.x level places two of each chest; the 1.2 start group is the invisible
//!   placeholder that the boss's on-death skill (`doodad_func_skill_hits` 18024 / 18031) hits,
//!   so the chests must never start in a lootable phase.
//! * B2: the 14 class-b sibling NPCs of eight open-zone kill quests. They are members of a
//!   NORMAL group spawner that is placed by another member, but `SelectSpawnerId` only selects
//!   the AUTOCREATED single-NPC spawner of a placement, so they need a row of their own.
//!
//! The real-data tests are gated by `AAEMU_COMPACT_DB` (read-only 1.2 `compact.sqlite3`):
//! `AAEMU_COMPACT_DB=... cargo test -p aaemu-game --lib real_data_quest_spawn -- --ignored`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use sqlx::mysql::MySqlPoolOptions;

use crate::config::AppConfig;
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::phase;
use crate::systems::npcs::doodad::spawner::{parse_list, NpcSpawnerDef};
use crate::systems::npcs::doodad::templates::{doodad_func_group_kind, FuncTemplate};
use crate::systems::npcs::npc::spawner_data::{npc_spawner_category, NpcSpawnerData};
use crate::systems::npcs::spawn_manager::{backend::TestBackend, LoadStats, TemplateExists};
use crate::test_support::TEST_ZONE;
use crate::world::{DoodadRef, NpcRef, WorldInstance, WorldObject};

const BURNTCASTLE: &str = "instance_burntcastle_armory";
const HADIR: &str = "instance_hadir_farm";

/// The two authored placements of each chest in the 10.x `doodad.g` of its instance (they are
/// also the authored `chest_count = 2` of the mentoring runtime).
const BURNTCASTLE_CHESTS: [(f32, f32, f32); 2] =
    [(201.739, 313.111, 20.5188), (201.744, 310.903, 20.5188)];
const HADIR_CHESTS: [(f32, f32, f32); 2] =
    [(758.372, 683.263, 136.045), (760.658, 683.337, 136.109)];

/// `(UnitId, X, Y, Z)` of the class-b sibling rows added to
/// `main_world/npc_spawns_quest_fill.json`.
const CLASS_B: [(u32, f32, f32, f32); 14] = [
    (527, 10548.683, 11752.29, 162.23685),
    (4946, 11253.278, 11382.39, 133.0489),
    (2352, 15488.32, 11520.91, 93.3608),
    (2354, 15570.91, 11498.07, 78.54807),
    (2357, 15536.16, 11490.74, 78.110565),
    (2483, 21416.85, 7385.75, 234.86296),
    (3105, 23469.77, 11270.21, 315.9892),
    (3106, 23521.49, 11257.1, 321.8799),
    (3478, 13851.72, 14482.15, 120.79872),
    (1763, 12554.296, 10531.682, 116.43928),
    (732, 12451.787, 14091.77, 119.5487),
    (2918, 15765.35, 8446.72, 78.98558),
    (2919, 15744.41, 8389.17, 79.29809),
    (2941, 15707.99, 8333.49, 84.423164),
];

/// The eight open-zone kill quests whose audit rows were class b, with the unit of each audit
/// row (the unit that used to have no placement of its own).
const CLASS_B_QUESTS: [(u32, &[u32]); 8] = [
    (182, &[527, 4946]),
    (556, &[2352, 2354, 2357]),
    (1191, &[2483]),
    (2120, &[3105, 3106]),
    (2573, &[3478]),
    (2725, &[1763]),
    (2742, &[732]),
    (3865, &[2918, 2919, 2941]),
];

struct Backend;

impl TestBackend for Backend {
    fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
        if template_id == 0 {
            return None;
        }
        let id = if obj_id == 0 {
            server.ids.object.get_next_id().ok()?
        } else {
            obj_id
        };
        let mut npc = crate::models::npc::Npc::new(template_id);
        npc.unit.hp = 100;
        npc.unit.max_hp = 100;
        Some(crate::world::Entity::new(id, npc))
    }

    fn doodad_create(
        &self,
        server: &GameServer,
        world: &Arc<WorldInstance>,
        obj_id: u32,
        template_id: u32,
    ) -> Option<DoodadRef> {
        let id = if obj_id == 0 {
            server.ids.object.get_next_id().ok()?
        } else {
            obj_id
        };
        let doodad = crate::world::Entity::new(id, crate::models::doodad::Doodad::new(template_id));
        doodad.data().transform.instance_id = world.id;
        Some(doodad)
    }
}

/// Synthetic world templates `(id, name, zone)`; `main_world` needs the 32x32 cells that hold the
/// open-zone positions.
fn server_with_worlds(
    npc_spawners: NpcSpawnerData,
    worlds: impl IntoIterator<Item = (u32, String, u32)>,
) -> Arc<GameServer> {
    server_with_worlds_and_doodads(None, npc_spawners, worlds)
}

/// [`server_with_worlds`] with real doodad templates (the live spawn tests need them for the
/// start phase and the phase funcs).
fn server_with_worlds_and_doodads(
    doodads: Option<DoodadData>,
    npc_spawners: NpcSpawnerData,
    worlds: impl IntoIterator<Item = (u32, String, u32)>,
) -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.systems.npcs.npc_spawners = npc_spawners;
    if let Some(doodads) = doodads {
        data.systems.npcs.doodad = doodads;
    }
    data.worlds = WorldTemplates::from_templates(
        worlds
            .into_iter()
            .map(|(id, name, zone)| {
                let cells = if name == "main_world" { 48 } else { 2 };
                WorldTemplate::synthetic(id, &name, cells, cells, zone)
            })
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

fn instance_world(server: &Arc<GameServer>, id: u32, name: &str) -> Arc<WorldInstance> {
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(name)
        .expect("world template")
        .clone();
    server.world.create_world_instance(Arc::new(template), id)
}

fn load_world(server: &Arc<GameServer>, conn: &Connection, world_id: u32, name: &str) -> LoadStats {
    let mut npcs = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM npcs").unwrap();
        for id in stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap() {
            npcs.insert(id.unwrap() as u32);
        }
    }
    let mut doodads = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM doodad_almighties").unwrap();
        for id in stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap() {
            doodads.insert(id.unwrap() as u32);
        }
    }
    let none = |_: u32| false;
    let npc = |id: u32| npcs.contains(&id);
    let doodad = |id: u32| doodads.contains(&id);
    let exists = TemplateExists {
        npc: &npc,
        doodad: &doodad,
        transfer: &none,
        gimmick: &none,
        slave: &none,
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds")
        .join(name);
    let world = server.world.world(world_id).expect("world instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.load_spawn_files(server, &path, &exists).unwrap()
}

fn same_position(p: &crate::data::world_templates::WorldSpawnPosition, e: (f32, f32, f32)) -> bool {
    (p.x - e.0).abs() < 0.01 && (p.y - e.1).abs() < 0.01 && (p.z - e.2).abs() < 0.01
}

fn all_placed_units() -> HashSet<u32> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("app/Data/Worlds");
    let mut placed = HashSet::new();
    for world in std::fs::read_dir(&root).expect("Worlds") {
        let dir = world.expect("world dir").path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir).expect("world files") {
            let path = file.expect("spawn file").path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("npc_spawns") || !name.ends_with(".json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for def in parse_list(&text, NpcSpawnerDef::from_json).expect(name) {
                placed.insert(def.base.unit_id);
            }
        }
    }
    placed
}

fn detail_npc(conn: &Connection, table: &str, detail: u32) -> Vec<u32> {
    let sql = format!("SELECT npc_id FROM {table} WHERE id = ?1");
    let mut stmt = conn.prepare(&sql).unwrap();
    stmt.query_map([detail], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|v| v.unwrap() as u32)
        .collect()
}

/// Start-component targets (each `Vec` is one act; the engine accepts the component when any
/// one act's units are satisfied) and progress group-hunt targets of a quest, resolved the way
/// the audit did (`quest_components` kinds 2 and 4, `quest_acts`, the act detail tables).
fn quest_unit_requirements(conn: &Connection, quest: u32) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
    let mut starts = Vec::new();
    let mut hunts = Vec::new();
    let mut comps = conn
        .prepare("SELECT id FROM quest_components WHERE quest_context_id = ?1 AND component_kind_id IN (2, 4)")
        .unwrap();
    let comp_ids: Vec<u32> = comps
        .query_map([quest], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|v| v.unwrap() as u32)
        .collect();
    for comp in comp_ids {
        let mut acts = conn
            .prepare("SELECT act_detail_id, act_detail_type FROM quest_acts WHERE quest_component_id = ?1")
            .unwrap();
        let rows: Vec<(u32, String)> = acts
            .query_map([comp], |r| {
                Ok((r.get::<_, i64>(0)? as u32, r.get::<_, String>(1)?))
            })
            .unwrap()
            .map(|v| v.unwrap())
            .collect();
        for (detail, kind) in rows {
            match kind.as_str() {
                "QuestActConAcceptNpcKill" => {
                    starts.push(detail_npc(conn, "quest_act_con_accept_npc_kills", detail))
                }
                "QuestActConAcceptNpc" => {
                    starts.push(detail_npc(conn, "quest_act_con_accept_npcs", detail))
                }
                "QuestActObjTalk" => starts.push(detail_npc(conn, "quest_act_obj_talks", detail)),
                "QuestActObjMonsterHunt" => {
                    starts.push(detail_npc(conn, "quest_act_obj_monster_hunts", detail))
                }
                "QuestActConReportNpc" => {
                    starts.push(detail_npc(conn, "quest_act_con_report_npcs", detail))
                }
                "QuestActObjMonsterGroupHunt" => {
                    let group: i64 = conn
                        .query_row(
                            "SELECT quest_monster_group_id FROM quest_act_obj_monster_group_hunts WHERE id = ?1",
                            [detail],
                            |r| r.get(0),
                        )
                        .unwrap();
                    let mut members = conn
                        .prepare("SELECT npc_id FROM quest_monster_npcs WHERE quest_monster_group_id = ?1")
                        .unwrap();
                    let group_members: Vec<u32> = members
                        .query_map([group], |r| r.get::<_, i64>(0))
                        .unwrap()
                        .map(|v| v.unwrap() as u32)
                        .collect();
                    hunts.push(group_members);
                }
                _ => {}
            }
        }
    }
    (starts, hunts)
}

/// The two instance chests load exactly at their two authored positions, start in the invisible
/// placeholder phase whose `DoodadFuncSkillHit` is the boss on-death skill, and the phase chain
/// from there grants exactly the four quest items (31589/31590 on 7659, 31593/31594 on 7661).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_quest_spawn_mentor_chests_load_hidden_with_their_loot() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let server = server_with_worlds(
        NpcSpawnerData::load_from(&conn).unwrap(),
        [
            (0, "main_world".to_string(), TEST_ZONE),
            (1, BURNTCASTLE.to_string(), 236),
            (2, HADIR.to_string(), 46),
        ],
    );
    instance_world(&server, 1, BURNTCASTLE);
    instance_world(&server, 2, HADIR);
    load_world(&server, &conn, 1, BURNTCASTLE);
    load_world(&server, &conn, 2, HADIR);

    for (world_id, template, authored, what) in [
        (1, 7659, BURNTCASTLE_CHESTS, "Hieronimus's Secret Chest"),
        (2, 7661, HADIR_CHESTS, "Marmas's Secret Chest"),
    ] {
        let m = server.systems.npcs.spawns.get(world_id).unwrap();
        let spawners = m.get_doodad_spawners_by_unit_id(template);
        assert_eq!(
            spawners.len(),
            authored.len(),
            "{what} ({template}) must load exactly {} times",
            authored.len()
        );
        for expected in authored {
            assert!(
                spawners
                    .iter()
                    .any(|s| same_position(&s.position, expected)),
                "{what} ({template}) has no spawner at {expected:?}"
            );
        }
    }

    let doodads = DoodadData::load_from(&conn).unwrap();
    for (template, start, death_skill, quest_items, stages) in [
        (
            7659u32,
            20853u32,
            18024u32,
            [31589u32, 31590u32],
            [21051u32, 20854, 20855, 20856, 20857],
        ),
        (
            7661,
            20848,
            18031,
            [31593, 31594],
            [21056, 20877, 20887, 20888, 20889],
        ),
    ] {
        assert_eq!(
            phase::get_func_group_id(&doodads, template),
            start,
            "template {template} must start in its invisible placeholder phase"
        );
        let group = doodads.func_group(start).expect("start group");
        assert_eq!(
            group.group_kind_id,
            doodad_func_group_kind::START,
            "template {template} start group kind"
        );
        assert_eq!(group.almighty, template);

        let start_funcs = doodads.funcs_for_group(start);
        assert_eq!(start_funcs.len(), 1, "template {template} start funcs");
        let hit = &start_funcs[0];
        assert_eq!(hit.func_type, "DoodadFuncSkillHit");
        match doodads.func_template(hit.func_id, "DoodadFuncSkillHit") {
            Some(FuncTemplate::SkillHit(f)) => assert_eq!(
                f.skill_id, death_skill,
                "template {template} is revealed by the boss on-death skill"
            ),
            other => panic!("template {template}: unexpected start func {other:?}"),
        }
        assert!(
            !start_funcs
                .iter()
                .any(|f| f.func_type == "DoodadFuncLootItem"),
            "template {template} must not be lootable from the start phase"
        );

        // Walk the authored `next_phase` chain and collect what it can loot.
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        let mut loot = HashSet::new();
        while let Some(group) = stack.pop() {
            if !seen.insert(group) {
                continue;
            }
            assert_eq!(
                doodads.func_group(group).map(|g| g.almighty),
                Some(template),
                "phase {group} belongs to template {template}"
            );
            for func in doodads.funcs_for_group(group) {
                if func.func_type == "DoodadFuncLootItem" {
                    if let Some(FuncTemplate::LootItem(l)) =
                        doodads.func_template(func.func_id, "DoodadFuncLootItem")
                    {
                        loot.insert(l.item_id);
                    }
                }
                if func.next_phase > 0 {
                    stack.push(func.next_phase as u32);
                }
            }
        }
        for item in quest_items {
            assert!(
                loot.contains(&item),
                "template {template} phase chain must grant quest item {item}"
            );
        }
        for stage in stages {
            assert_eq!(
                doodads.func_group(stage).map(|g| g.almighty),
                Some(template),
                "mentoring stage {stage} belongs to template {template}"
            );
        }
    }
}

/// The authored chest rows are actually spawned by the spawn manager: each one starts in the
/// invisible placeholder phase, a different boss's on-death skill leaves it hidden, and the
/// instance's own on-death skill moves the live phase to the next authored phase. This drives
/// the live phase machine, not a second `DoodadData` copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_quest_spawn_mentor_chests_spawn_hidden_and_the_boss_skill_reveals_them() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let server = server_with_worlds_and_doodads(
        Some(DoodadData::load_from(&conn).unwrap()),
        NpcSpawnerData::load_from(&conn).unwrap(),
        [
            (0, "main_world".to_string(), TEST_ZONE),
            (1, BURNTCASTLE.to_string(), 236),
            (2, HADIR.to_string(), 46),
        ],
    );
    // The real doodad manager applies the template's start phase; the spawn test stub does not.
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = None;
    instance_world(&server, 1, BURNTCASTLE);
    instance_world(&server, 2, HADIR);
    load_world(&server, &conn, 1, BURNTCASTLE);
    load_world(&server, &conn, 2, HADIR);

    for (world_id, template, start, death_skill, after, other_skill, boss) in [
        (
            1u32, 7659u32, 20853u32, 18024u32, 21051u32, 18031u32, 10518u32,
        ),
        (2, 7661, 20848, 18031, 21056, 18024, 10098),
    ] {
        // The dying boss is the skill caster the chest hit sees.
        let boss = WorldObject::from(&crate::world::Entity::new(0x9000, Npc::new(boss)));
        let m = server.systems.npcs.spawns.get(world_id).unwrap();
        let spawners = m.get_doodad_spawners_by_unit_id(template);
        assert_eq!(spawners.len(), 2, "template {template} spawners");
        let world = server.world.world(world_id).unwrap();
        for spawner in &spawners {
            let chest = spawner.spawn(&server, 0).expect("the chest spawns");
            assert!(
                server
                    .world
                    .world(world_id)
                    .unwrap()
                    .doodad(chest.obj_id)
                    .is_some_and(|live| Arc::ptr_eq(&live, &chest)),
                "template {template}: the spawner did not place the chest in the world"
            );
            {
                let d = chest.data();
                assert_eq!(
                    d.func_group_id, start,
                    "template {template} must spawn in its hidden placeholder phase"
                );
                assert_eq!(
                    d.current_func_types,
                    vec!["DoodadFuncSkillHit".to_string()],
                    "template {template}: only the reveal func may be live at spawn"
                );
            }
            // Another instance's boss skill is not the authoring: the chest stays hidden.
            crate::systems::npcs::doodad::manager::on_skill_hit(
                &server,
                &world,
                &chest,
                Some(&boss),
                other_skill,
            );
            assert_eq!(
                chest.data().func_group_id,
                start,
                "template {template}: skill {other_skill} must not reveal it"
            );
            // The instance's own on-death skill moves the live phase to the next authored one.
            crate::systems::npcs::doodad::manager::on_skill_hit(
                &server,
                &world,
                &chest,
                Some(&boss),
                death_skill,
            );
            assert_eq!(
                chest.data().func_group_id,
                after,
                "template {template}: skill {death_skill} must reveal it"
            );
        }
    }
}

/// Each class-b sibling NPC has exactly one row in the quest fill, exactly one loaded AUTOCREATED
/// (single-NPC) spawner at that position and that clone is the one `SelectSpawnerId` selects; the
/// group spawner it is a member of stays idle. All units the eight quests list are then placed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_quest_spawn_class_b_siblings_are_placed_once() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let world_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("app/Data/Worlds/main_world");
    let text = std::fs::read_to_string(world_path.join("npc_spawns_quest_fill.json"))
        .expect("npc_spawns_quest_fill.json");
    let defs = parse_list(&text, NpcSpawnerDef::from_json).expect("quest fill");

    let server = server_with_worlds(
        NpcSpawnerData::load_from(&conn).unwrap(),
        [(0, "main_world".to_string(), TEST_ZONE)],
    );
    load_world(&server, &conn, 0, "main_world");
    let m = server.systems.npcs.spawns.get(0).unwrap();
    let data = &server.game_data.systems.npcs.npc_spawners;

    for (unit, x, y, z) in CLASS_B {
        let rows: Vec<_> = defs.iter().filter(|d| d.base.unit_id == unit).collect();
        assert_eq!(
            rows.len(),
            1,
            "npc {unit} must have exactly one quest-fill row"
        );
        assert!(
            same_position(&rows[0].base.position, (x, y, z)),
            "npc {unit} row position"
        );

        // The unit's own AUTOCREATED spawner is the only one `SelectSpawnerId` can select next
        // to the NORMAL group spawners (which stay idle without a schedule).
        let ids = data.get_spawner_ids(unit).expect("spawner ids");
        let auto: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|id| {
                data.get_npc_spawner_template(*id)
                    .is_some_and(|t| t.npc_spawner_category_id == npc_spawner_category::AUTOCREATED)
            })
            .collect();
        assert_eq!(
            auto.len(),
            1,
            "npc {unit} must have one AUTOCREATED spawner"
        );
        let auto_template = data.get_npc_spawner_template(auto[0]).unwrap();
        assert_eq!(auto_template.npcs.len(), 1);
        assert_eq!(auto_template.npcs[0].member_id, unit);

        let loaded: Vec<_> = m
            .get_all_spawners()
            .into_values()
            .flatten()
            .filter(|s| s.unit_id == unit && same_position(&s.position(), (x, y, z)))
            .collect();
        assert!(
            !loaded.is_empty(),
            "npc {unit} has no loaded spawner at ({x}, {y}, {z})"
        );
        let optimal: Vec<_> = loaded
            .iter()
            .filter(|s| s.is_optimal_spawner(&server))
            .collect();
        assert_eq!(
            optimal.len(),
            1,
            "npc {unit}: exactly one spawner may be selected"
        );
        assert_eq!(
            optimal[0].spawner_id, auto[0],
            "npc {unit}: the AUTOCREATED single spawner must be selected"
        );
    }

    // Every unit the eight quests list is now placed (any world's spawn file counts, like the
    // audit's placement rule).
    let placed = all_placed_units();
    for (quest, units) in CLASS_B_QUESTS {
        for unit in units {
            assert!(
                placed.contains(unit),
                "quest {quest}: required unit {unit} is still unplaced"
            );
        }
        let (starts, hunts) = quest_unit_requirements(&conn, quest);
        assert!(
            starts
                .iter()
                .any(|act| !act.is_empty() && act.iter().all(|u| placed.contains(u))),
            "quest {quest} has no fully placed start act"
        );
        for members in hunts {
            assert!(
                members.iter().any(|u| placed.contains(u)),
                "quest {quest}: no group-hunt member is placed"
            );
        }
    }
}
