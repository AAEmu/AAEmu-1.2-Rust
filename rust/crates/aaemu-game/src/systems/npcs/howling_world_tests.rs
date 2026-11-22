//! Howling Abyss world data: one NPC per recorded spawn, the Greater doodad facings, the Lion
//! Gate helper and the Bomber keg's model proxy.
//!
//! The data tests read only the shipped spawn files. The `real_` tests need
//! `AAEMU_COMPACT_DB` (spawn loading, doodad models) or `AAEMU_GAME_PAK` (the r208022 client
//! files: collision of the Greater rooms, the keg model).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::server::GameServer;
use crate::world::WorldInstance;

use super::{burntcastle_dungeon_tests::real_encounter_server, spawn_manager::TemplateExists};

const NORMAL: &str = "instance_howling_abyss";
const GREATER: &str = "instance_howling_abyss_2";
const ZONE: u32 = 280;
/// `Blackscale Corps Glutton` (Greater).
const GREATER_GLUTTON: u32 = 12767;

fn app_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app")
}

fn spawn_rows(world: &str, file: &str) -> Vec<Value> {
    let path = app_path().join("Data/Worlds").join(world).join(file);
    let text = std::fs::read_to_string(&path).expect("spawn file");
    match serde_json::from_str(&text).expect("spawn json") {
        Value::Array(rows) => rows,
        _ => panic!("{} is not an array", path.display()),
    }
}

fn num(row: &Value, key: &str) -> f64 {
    row[key].as_f64().unwrap_or(0.0)
}

fn pos(row: &Value) -> [f64; 4] {
    let p = &row["Position"];
    [num(p, "X"), num(p, "Y"), num(p, "Z"), num(p, "Yaw")]
}

fn is_event_row(row: &Value) -> bool {
    row["NpcSpawnerIds"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id.as_u64() != Some(0)))
}

fn yaw_difference(a: f64, b: f64) -> f64 {
    ((a - b).rem_euclid(360.0) + 180.0).rem_euclid(360.0) - 180.0
}

/// A plain NPC row's `Id` names one recorded NPC. The roaming Gluttons (spawner comment
/// `로머임`, "a roamer") and a few pulled NPCs were recorded at every point they were seen,
/// so the spawn files listed one NPC several times, as up to seven Gluttons in a room where
/// Normal has one.
#[test]
fn howling_npc_rows_with_one_id_place_one_npc() {
    for world in [NORMAL, GREATER] {
        let mut seen: HashMap<u64, [f64; 4]> = HashMap::new();
        for row in spawn_rows(world, "npc_spawns.json") {
            if is_event_row(&row) {
                continue;
            }
            let id = row["Id"].as_u64().expect("Id");
            if let Some(first) = seen.insert(id, pos(&row)) {
                panic!(
                    "{world}: Id {id} ({}) is placed at {first:?} and {:?}",
                    row["Title"],
                    pos(&row)
                );
            }
        }
    }
    // The Greater pack north of the Wailing Ranch matches Normal's: one Glutton with the Knight,
    // the Soulpriest and the Deceiver.
    let room = |p: [f64; 4]| (565.0..610.0).contains(&p[0]) && (525.0..555.0).contains(&p[1]);
    let gluttons = spawn_rows(GREATER, "npc_spawns.json")
        .into_iter()
        .filter(|r| r["UnitId"].as_u64() == Some(GREATER_GLUTTON as u64) && room(pos(r)))
        .count();
    assert_eq!(gluttons, 1);
}

/// Greater template of a gate and the Normal template at the same placement.
const GATE_PAIRS: &[(u64, u64)] = &[
    (6332, 5342), // Stone Gate (`boss03_battle_door_close`)
    (6346, 5127), // Iron Gate
    (6348, 5146), // Iron Gate
    (6355, 5746), // Door
    (6357, 5607), // Iron Gate
    (6360, 5748), // Howling Abyss Gate (Weeping Charnel House)
    (6363, 5608), // Iron Gate
    (6368, 5953), // Stone Gate (`boss04_door_close`)
];

/// The Greater file had no facing (`Yaw` 0) for most placements it shares with Normal,
/// including the gates of the Weeping Charnel House rooms, so they stood up to 162 degrees off
/// their doorways. Every shared placement now has Normal's facing.
#[test]
fn howling_greater_doodads_face_like_their_normal_placements() {
    let normal = spawn_rows(NORMAL, "doodad_spawns.json");
    let mut checked = 0;
    for row in spawn_rows(GREATER, "doodad_spawns.json") {
        let unit = row["UnitId"].as_u64().unwrap();
        let g = pos(&row);
        let partner = GATE_PAIRS
            .iter()
            .find(|(greater, _)| *greater == unit)
            .map_or(unit, |(_, n)| *n);
        // The Greater battle doors are several rows along each door's width line.
        let reach = if unit == 6332 { 8.0 } else { 1.0 };
        let Some(n) = normal
            .iter()
            .filter(|n| n["UnitId"].as_u64() == Some(partner))
            .map(pos)
            .filter(|n| {
                let d = (n[0] - g[0]).hypot(n[1] - g[1]).hypot(n[2] - g[2]);
                d < reach
            })
            .min_by(|a, b| {
                let da = (a[0] - g[0]).hypot(a[1] - g[1]);
                let db = (b[0] - g[0]).hypot(b[1] - g[1]);
                da.total_cmp(&db)
            })
        else {
            continue;
        };
        if g[3] != 0.0 && n[3] != g[3] {
            // Greater's own facing (kegs, the Awakening Hammer) is kept.
            continue;
        }
        assert!(
            yaw_difference(g[3], n[3]).abs() < 0.01,
            "Greater {} {unit} at {g:?} faces {} instead of Normal's {}",
            row["Title"],
            g[3],
            n[3]
        );
        checked += 1;
    }
    assert!(checked >= 75, "{checked} shared placements");
}

/// Greater's Lion Gate (6364) is the same invisible logic gate as Normal's (5603), but its
/// `doodad_almighties.model` is `경로: cgf://...` ("path: ..."): the client cannot load that
/// path and draws its placeholder model. Neither gate's logic family has a subscriber, so the
/// Greater placement uses 5603. Every doodad of both instances has a loadable model path.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_howling_doodad_models_are_loadable_paths() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let model = |id: u64| -> String {
        conn.query_row(
            "SELECT model FROM doodad_almighties WHERE id = ?1",
            [id as i64],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    };
    assert!(
        model(6364).starts_with("경로:"),
        "the broken 6364 path is the reason for the swap"
    );
    let subscribers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM doodad_func_logic_family_subscribers WHERE family_id IN \
             (SELECT family_id FROM doodad_func_logic_family_providers WHERE id IN (485, 559))",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(subscribers, 0, "the Lion Gates drive no subscriber");
    for world in [NORMAL, GREATER] {
        for row in spawn_rows(world, "doodad_spawns.json") {
            let unit = row["UnitId"].as_u64().unwrap();
            let m = model(unit);
            let m = m.trim().to_ascii_lowercase();
            assert!(
                m.is_empty()
                    || ["cgf://", "cga://", "cdf://", "chr://", "prefab://"]
                        .iter()
                        .any(|scheme| m.starts_with(scheme)),
                "{world}: doodad {unit} ({}) has model {m:?}",
                row["Title"]
            );
        }
    }
}

fn load_greater(db: &str) -> (Arc<GameServer>, Arc<WorldInstance>) {
    let (server, world) = real_encounter_server(db, GREATER, ZONE);
    let data = &server.game_data.systems.npcs;
    let no = |_: u32| false;
    let exists = TemplateExists {
        npc: &|id| data.npc.exist(id),
        doodad: &|id| data.doodad.exist(id),
        transfer: &no,
        gimmick: &no,
        slave: &no,
    };
    let folder = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds")
        .join(GREATER);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .load_spawn_files(&server, &folder, &exists)
        .unwrap();
    (server, world)
}

/// Spawning every Greater Glutton placement gives one Glutton per placement and one in the
/// pack north of the Wailing Ranch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_greater_gluttons_spawn_once_per_recorded_npc() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let m = server.systems.npcs.spawns.get(world.id).unwrap();
    let mut placements = 0;
    for spawners in m.get_all_spawners().into_values() {
        if spawners.iter().any(|s| s.unit_id == GREATER_GLUTTON) {
            placements += 1;
            for s in &spawners {
                s.do_spawn(&server);
            }
        }
    }
    let gluttons: Vec<_> = world
        .all_npcs()
        .into_iter()
        .filter(|n| n.data().unit.template_id == GREATER_GLUTTON)
        .map(|n| {
            let t = n.data().unit.transform;
            (t.x, t.y)
        })
        .collect();
    assert_eq!(gluttons.len(), placements);
    let north_pack = gluttons
        .iter()
        .filter(|(x, y)| (565.0..610.0).contains(x) && (525.0..555.0).contains(y))
        .count();
    assert_eq!(north_pack, 1, "{gluttons:?}");
}

/// The r208022 client files and a server whose collision loads them.
fn pak_server() -> Option<Arc<GameServer>> {
    use crate::data::client_files::ClientFileManager;
    use crate::data::world_templates::WorldTemplates;
    use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};
    let pak = std::env::var("AAEMU_GAME_PAK").ok()?;
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app_path()));
    let worlds = WorldTemplates::load(&files, &app_path()).expect("world templates");
    let hm = Arc::new(HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            geo_data_mode: true,
            pre_load_terrain: false,
            placement_strict_surface: false,
        },
    ));
    let mut config = crate::config::AppConfig {
        id: 1,
        ..Default::default()
    };
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    config.world.geo_data_mode = true;
    let db = sqlx::mysql::MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let mut data = crate::data::GameData::empty();
    data.worlds = worlds;
    let server = GameServer::new(config, db, data);
    server.systems.npcs.heightmaps.install(hm);
    Some(Arc::new(server))
}

/// Each Greater gate's width axis spans its doorway: the static walls are close on both sides
/// along the width axis and the passage is open through it. The facing comes from the spawn
/// file. The width axis is the model's local X for the battle doors and local Y for the
/// `ezna_city_irongate_default` gates.
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_howling_greater_gates_span_their_doorways() {
    use crate::systems::ai::collision::{self, query};
    let Some(server) = pak_server() else {
        return;
    };
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(GREATER)
        .unwrap()
        .clone();
    let instance = 100;
    let _world = server
        .world
        .create_world_instance(Arc::new(template), instance);
    // (spawn Id, width along local Y)
    let gates: &[(u64, bool)] = &[
        (135, false), // Stone Gate, Weeping Charnel House
        (136, false),
        (137, false),
        (138, false),
        (163, true),  // Iron Gate, vine room
        (165, true),  // Iron Gate
        (195, true),  // Door
        (197, true),  // Iron Gate
        (199, true),  // Howling Abyss Gate, Weeping Charnel House
        (202, true),  // Iron Gate
        (207, false), // Stone Gate (boss04)
    ];
    let rows = spawn_rows(GREATER, "doodad_spawns.json");
    let distance = |from: [f32; 3], dir: (f32, f32)| {
        let to = [from[0] + dir.0 * 20.0, from[1] + dir.1 * 20.0, from[2]];
        collision::raycast(
            &server,
            instance,
            from,
            to,
            query::mask::STATIC_TERRAIN,
            query::ray_flags::DEFAULT,
        )
        .flatten()
        .map_or(f32::INFINITY, |h| {
            (h.pos.x - from[0]).hypot(h.pos.y - from[1])
        })
    };
    for &(id, width_along_y) in gates {
        let row = rows
            .iter()
            .find(|r| r["Id"].as_u64() == Some(id))
            .expect("gate row");
        let [x, y, z, yaw] = pos(row);
        let yaw = (yaw as f32).to_radians();
        let width = if width_along_y {
            yaw + std::f32::consts::FRAC_PI_2
        } else {
            yaw
        };
        let (wx, wy) = (width.cos(), width.sin());
        let at = [x as f32, y as f32, z as f32 + 1.5];
        let sides = [distance(at, (wx, wy)), distance(at, (-wx, -wy))];
        let through = [distance(at, (-wy, wx)), distance(at, (wy, -wx))];
        assert!(
            sides.iter().all(|d| *d < 3.5),
            "gate {id} ({}): width axis reaches {sides:?}",
            row["Title"]
        );
        assert!(
            through.iter().any(|d| *d > 5.0),
            "gate {id} ({}): passage blocked {through:?}",
            row["Title"]
        );
    }
}

/// The keg's fallback proxy is the client model's: `barrel_grape_e.cgf` has a `proxy` cylinder
/// on its origin, upright along model Z, so the server must lift the keg's centre off the floor.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_howling_keg_proxy_matches_the_client_model() {
    use crate::systems::ai::collision::{cells, models};
    use aaemu_physics::geometry::primitives::Primitive;
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let files = crate::data::client_files::ClientFileManager::from_sources(&[pak], &app_path());
    let path = cells::normalize_model_path("objects/env/01_nuia/001_ndeco/old/barrel_grape_e.cgf ");
    let bytes = files.read_file(&path).expect("keg model");
    let model = models::load_model(&path, &bytes)
        .unwrap()
        .expect("keg physics");
    let geom = model.slot(models::phys_slot::DEFAULT).expect("proxy");
    let Some(Primitive::Cylinder(c)) = geom.geom.primitive(0) else {
        panic!("keg proxy is not a cylinder");
    };
    let (r, hh) = super::gimmick::manager::KEG_PROXY_CYLINDER;
    assert!((c.r - r).abs() < 1e-3 && (c.hh - hh).abs() < 1e-3, "{c:?}");
    assert!(c.axis.z.abs() > 0.999, "{c:?}");
    assert!(c.center.x.abs() < 0.01 && c.center.y.abs() < 0.01 && c.center.z.abs() < 0.01);
}

/// Greater carried two Normal mobs in the pack at (502, 506): Blackscale Corps Archer 11261
/// (level 44) and Combatant 11754 (level 41). The C# spawn file recorded them at Normal's
/// placements of the same pack (Normal has 11261 at the same point and 11938 where Greater
/// had 11754). Both come from Normal's group spawners (`울부짖는 구렁텅이_상위 그룹` 13197 and
/// `_하위 그룹` 13195). Greater's own groups (`영던_...` 14442 and 14453) list the Greater
/// template of every member in the same order: 11261 -> 12768 and 11754 -> 12773.
///
/// Every hostile Greater spawn is a Greater template: level 45 or more (Greater's mobs are
/// 45-50, Normal's 40-44) and no template Normal places. Normal's bosses are level 45, so the
/// level alone would not tell them apart.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_howling_greater_hostile_spawns_are_greater_templates() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let template = |id: u64| -> (i64, i64) {
        conn.query_row(
            "SELECT level, faction_id FROM npcs WHERE id = ?1",
            [id as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or_else(|_| panic!("npc template {id}"))
    };
    let normal: std::collections::HashSet<u64> = spawn_rows(NORMAL, "npc_spawns.json")
        .iter()
        .map(|r| r["UnitId"].as_u64().unwrap())
        .collect();
    let mut hostile = 0;
    for row in spawn_rows(GREATER, "npc_spawns.json") {
        let unit = row["UnitId"].as_u64().unwrap();
        let (level, faction) = template(unit);
        // Faction 1 is the friendly faction of the event NPCs (Godfrey's staging actor, the
        // captives, the cut-scene helpers).
        if faction == 1 {
            continue;
        }
        hostile += 1;
        assert!(
            level >= 45,
            "Greater {} {unit} at {:?} is level {level}",
            row["Title"],
            pos(&row)
        );
        assert!(
            !normal.contains(&unit),
            "Greater {} {unit} at {:?} is a Normal template",
            row["Title"],
            pos(&row)
        );
    }
    assert!(hostile > 150, "{hostile} hostile rows");
    let members = |spawner: i64| -> Vec<i64> {
        let mut s = conn
            .prepare("SELECT member_id FROM npc_spawner_npcs WHERE npc_spawner_id = ?1 ORDER BY member_id")
            .unwrap();
        s.query_map([spawner], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    for (normal_group, greater_group, normal_unit, greater_unit, at) in [
        (13197, 14442, 11261, 12768, [502.514, 507.952]),
        (13195, 14453, 11754, 12773, [500.986, 510.412]),
    ] {
        let n = members(normal_group);
        let g = members(greater_group);
        let i = n.iter().position(|m| *m == normal_unit).unwrap();
        assert_eq!(g[i], greater_unit, "group {greater_group} member {i}");
        let name = |id: i64| -> String {
            conn.query_row("SELECT name FROM npcs WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(name(normal_unit), name(greater_unit), "the same mob");
        let row = spawn_rows(GREATER, "npc_spawns.json")
            .into_iter()
            .find(|r| {
                let p = pos(r);
                (p[0] - at[0]).abs() < 0.01 && (p[1] - at[1]).abs() < 0.01
            })
            .expect("the pack row");
        assert_eq!(row["UnitId"].as_u64(), Some(greater_unit as u64));
    }
}
