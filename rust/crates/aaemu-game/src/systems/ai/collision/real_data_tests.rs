//! Gated tests against the real 1.2 client data:
//! `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_collision -- --ignored --nocapture`.
//!
//! * the sector index order of `brush.dat` / `vegetation.dat` ([`super::cells::SECTOR_X_MAJOR`])
//!   is the one that puts trees on the terrain;
//! * a few main_world cells load (counts, models, timing, memory of the physics blobs);
//! * ground heights from the physics world (rays 0x101) match the NPC spawn heights of
//!   `Data/Worlds/main_world/npc_spawns.json` and survive unit-state serialization even where
//!   the approximate navigation height differs;
//! * rays hit brushes / vegetation (static entities) above the terrain.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use aaemu_physics::math::Vec3;

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::WorldTemplates;
use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds, CELL_HMAP_RESOLUTION};

use super::cells;
use super::models::ModelCache;
use super::query::{mask, ray_flags};
use super::terrain::{terrain_elevation, TerrainSource};
use super::world::{prepare_cell, Owner, PhysWorld};
use super::{Files, HeightmapTerrain};

fn app_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app")
}

struct Env {
    files: Arc<ClientFileManager>,
    hm: Arc<HeightmapWorlds>,
    template_id: u32,
    cells: (i32, i32),
    ocean: f32,
}

fn env() -> Option<Env> {
    env_with(false)
}

/// [`env`] with `GeoDataMode` (the navmesh heights) on or off in the heightmaps.
fn env_with(geo_data_mode: bool) -> Option<Env> {
    let pak = std::env::var("AAEMU_GAME_PAK").ok()?;
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app_path()));
    let worlds = WorldTemplates::load(&files, &app_path()).expect("world templates");
    let t = worlds
        .templates()
        .iter()
        .find(|t| t.name == "main_world")
        .expect("main_world")
        .clone();
    let hm = Arc::new(HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            geo_data_mode,
            pre_load_terrain: false,
            placement_strict_surface: false,
        },
    ));
    Some(Env {
        files,
        hm,
        template_id: t.id,
        cells: (t.cell_x, t.cell_y),
        ocean: t.ocean_level,
    })
}

fn terrain(e: &Env) -> Arc<HeightmapTerrain> {
    Arc::new(HeightmapTerrain {
        worlds: e.hm.clone(),
        template_id: e.template_id,
        size: (
            e.cells.0 * CELL_HMAP_RESOLUTION,
            e.cells.1 * CELL_HMAP_RESOLUTION,
        ),
    })
}

/// NPC spawn positions of main_world (`"X": …, "Y": …, "Z": …` in the lenient JSON).
fn npc_spawns() -> Vec<[f32; 3]> {
    let p = app_path().join("Data/Worlds/main_world/npc_spawns.json");
    let Ok(text) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    let re = regex::Regex::new(
        r#""X"\s*:\s*(-?[0-9.]+)\s*,\s*"Y"\s*:\s*(-?[0-9.]+)\s*,\s*"Z"\s*:\s*(-?[0-9.]+)"#,
    )
    .unwrap();
    re.captures_iter(&text)
        .filter_map(|c| Some([c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?]))
        .collect()
}

/// The `(sector, local position)` of every vegetation record (raw, to test both orders).
fn raw_vegetation(d: &[u8]) -> Vec<(usize, Vec3)> {
    let u = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    let f = |o: usize| f32::from_bits(u(o));
    let mut out = Vec::new();
    for i in 0..16 {
        let (off, size) = (u(4 + 4 * i) as usize, u(68 + 4 * i) as usize);
        for k in 0..size / 64 {
            let r = off + k * 64;
            out.push((i, Vec3::new(f(r + 39), f(r + 43), f(r + 47))));
        }
    }
    out
}

#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_collision_sector_order() {
    let Some(e) = env() else { return };
    let t = terrain(&e);
    let (mut good_x_major, mut good_y_major, mut total) = (0usize, 0usize, 0usize);
    for cell in [(9, 14), (10, 14), (16, 20), (20, 16), (12, 22)] {
        let path = format!(
            "game/worlds/main_world/cells/{:03}_{:03}/client/vegetation.dat",
            cell.0, cell.1
        );
        let Some(d) = e.files.read_file(&path) else {
            continue;
        };
        for (i, p) in raw_vegetation(&d) {
            let base = (cell.0 as f32 * 1024.0, cell.1 as f32 * 1024.0);
            let xm = ((i / 4) as f32 * 256.0, (i % 4) as f32 * 256.0);
            let ym = ((i % 4) as f32 * 256.0, (i / 4) as f32 * 256.0);
            let hx = terrain_elevation(t.as_ref(), base.0 + xm.0 + p.x, base.1 + xm.1 + p.y);
            let hy = terrain_elevation(t.as_ref(), base.0 + ym.0 + p.x, base.1 + ym.1 + p.y);
            total += 1;
            good_x_major += usize::from((hx - p.z).abs() < 2.0);
            good_y_major += usize::from((hy - p.z).abs() < 2.0);
        }
    }
    println!(
        "vegetation on terrain: x-major {good_x_major}/{total}, y-major {good_y_major}/{total}"
    );
    assert!(total > 1000, "{total}");
    let chosen = if cells::SECTOR_X_MAJOR {
        good_x_major
    } else {
        good_y_major
    };
    assert!(chosen * 10 > total * 8, "{chosen}/{total}");
    assert!(chosen >= good_x_major.max(good_y_major));
}

#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_collision_cells_load_and_answer() {
    let Some(e) = env() else { return };
    let files = Files::Client(e.files.clone());
    let models = ModelCache::new(Arc::new(files.clone()));
    let xml = e
        .files
        .read_file("game/worlds/main_world/vegetation.xml")
        .map(|b| crate::data::client_files::decode_text(&b))
        .unwrap_or_default();
    let groups: HashMap<u16, cells::VegetationGroup> = cells::read_vegetation_groups(&xml)
        .into_iter()
        .map(|g| (g.id, g))
        .collect();
    println!("vegetation groups {}", groups.len());
    assert!(groups.len() > 1000);
    {
        // why vegetation records are skipped
        let d = e
            .files
            .read_file("game/worlds/main_world/cells/013_014/client/vegetation.dat")
            .unwrap();
        let recs = cells::read_vegetation(&d).unwrap();
        let (mut no_group, mut no_model, mut no_file, mut no_coll, mut ok) = (0, 0, 0, 0, 0);
        let mut no_coll_models: HashMap<String, usize> = HashMap::new();
        for r in &recs {
            match groups.get(&r.group) {
                None => no_group += 1,
                Some(g) if g.model.is_empty() => no_model += 1,
                Some(g) => match models.get(&g.model) {
                    None => {
                        no_file += 1;
                        *no_coll_models.entry(g.model.clone()).or_default() += 1;
                    }
                    Some(m) if !m.collides() => no_coll += 1,
                    Some(_) => ok += 1,
                },
            }
        }
        let mut top: Vec<_> = no_coll_models.into_iter().collect();
        top.sort_by_key(|t| std::cmp::Reverse(t.1));
        top.truncate(5);
        println!("veg 013_014: no group {no_group}, no model {no_model}, no file/physics {no_file}, no default slot {no_coll}, ok {ok}; top {top:?}");
    }
    let spawns = npc_spawns();
    let worlds = WorldTemplates::load(&e.files, &app_path()).expect("world templates");
    let nav = super::super::geodata::manager::AiGeoData::new(
        worlds
            .get_world_template_by_id(e.template_id)
            .expect("main_world"),
        true,
        Arc::new(super::super::geodata::ClientBaiFiles(e.files.clone())),
        Arc::new(super::super::geodata::bai::UsedAreaNames::default()),
        None,
    );
    let mut prefix = aaemu_commons::PacketStream::new();
    prefix
        .write_bc(1)
        .write_str("")
        .write_u8(1) // NPC
        .write_bc(1)
        .write_u32(1)
        .write_u32(0)
        .write_u8(0)
        .write_str("");
    let position_offset = prefix.len();
    // the cells with the most NPC spawns
    let mut per_cell: HashMap<(i32, i32), usize> = HashMap::new();
    for s in &spawns {
        *per_cell
            .entry(((s[0] / 1024.0) as i32, (s[1] / 1024.0) as i32))
            .or_default() += 1;
    }
    let mut cells_sorted: Vec<_> = per_cell.into_iter().collect();
    cells_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let chosen: Vec<(i32, i32)> = cells_sorted.iter().take(4).map(|c| c.0).collect();
    println!("cells {chosen:?}");
    let pw = PhysWorld::new(
        1,
        e.template_id,
        "main_world",
        e.cells.0,
        e.cells.1,
        e.ocean,
        Some(terrain(&e) as Arc<dyn TerrainSource>),
    );
    let t0 = Instant::now();
    for &c in &chosen {
        let tc = Instant::now();
        let content = prepare_cell(&files, &models, "main_world", &groups, c, true, true);
        println!(
            "cell {c:?}: {} brush records, {} vegetation records, {} instances, {} skipped, prepare {:?}",
            content.brush_records,
            content.vegetation_records,
            content.instances.len(),
            content.skipped,
            tc.elapsed()
        );
        pw.end_load(Some(content), c);
    }
    let st = pw.lock();
    println!(
        "loaded {} cells in {:?}: {} brushes, {} vegetation; models {} (loaded {}, missing {}, no physics {}, failed {}), blobs {} KB, model load {} ms",
        st.cells.len(),
        t0.elapsed(),
        st.stats.brushes,
        st.stats.vegetation,
        models.len(),
        models.stats.loaded.load(std::sync::atomic::Ordering::Relaxed),
        models.stats.missing.load(std::sync::atomic::Ordering::Relaxed),
        models.stats.no_physics.load(std::sync::atomic::Ordering::Relaxed),
        models.stats.failed.load(std::sync::atomic::Ordering::Relaxed),
        models.blob_bytes() / 1024,
        models.stats.load_micros.load(std::sync::atomic::Ordering::Relaxed) / 1000,
    );
    assert!(st.stats.brushes + st.stats.vegetation > 100);
    assert_eq!(
        models
            .stats
            .failed
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    drop(st);

    // ground under the NPC spawns of the loaded cells
    let t = terrain(&e);
    let (mut n, mut near, mut on_static, mut terrain_near) = (0, 0, 0, 0);
    let mut worst = Vec::new();
    let (mut nav_above, mut nav_below) = (0, 0);
    let mut nav_worst = Vec::new();
    let tq = Instant::now();
    for s in spawns
        .iter()
        .filter(|s| chosen.contains(&((s[0] / 1024.0) as i32, (s[1] / 1024.0) as i32)))
    {
        let mut st = pw.lock();
        let hit = st.raycast(
            Vec3::new(s[0], s[1], s[2] + 2.0),
            Vec3::new(s[0], s[1], s[2] - 100.0),
            mask::STATIC_TERRAIN,
            ray_flags::DEFAULT,
            &[],
        );
        drop(st);
        n += 1;
        let te = terrain_elevation(t.as_ref(), s[0], s[1]);
        if (te - s[2]).abs() < 1.0 {
            terrain_near += 1;
        }
        if let Some(h) = hit {
            if (h.pos.z - s[2]).abs() < 1.0 {
                near += 1;
                let mut npc = crate::models::npc::Npc::new(1);
                npc.has_ai = true;
                npc.spawner_z = Some(s[2]);
                npc.unit.transform.zone_id =
                    worlds.get_zone_id(worlds.get_world_template_by_id(e.template_id), s[0], s[1]);
                npc.unit
                    .transform
                    .set_position(s[0], s[1], h.pos.z, 0.0, 0.0, 0.0);
                let before = npc.unit.transform;
                let mut position = aaemu_commons::PacketStream::new();
                position.write_position(s[0], s[1], h.pos.z);
                let state = crate::world::unit_state::npc_unit_state(1, &mut npc, "", 0);
                assert_eq!(npc.unit.transform, before);
                assert_eq!(
                    &state.body[position_offset..position_offset + position.len()],
                    position.as_bytes(),
                    "collision floor at {s:?}"
                );
                let nav_z = nav.get_height(crate::systems::skills::controllers::Vec3::new(
                    s[0], s[1], h.pos.z,
                ));
                if nav_z != 0.0 {
                    let delta = nav_z - h.pos.z;
                    nav_above += usize::from(delta > 1.0);
                    nav_below += usize::from(delta < -1.0);
                    nav_worst.push((*s, h.pos.z, nav_z, delta));
                }
            } else if worst.len() < 5 {
                worst.push((*s, h.pos.z, h.owner));
            }
            if !matches!(h.owner, Some(Owner::Terrain)) {
                on_static += 1;
            }
        }
    }
    println!(
        "{n} spawns: ground within 1 m {near}, terrain elevation within 1 m {terrain_near}, on static geometry {on_static}; {:?} per ray; e.g. {worst:?}",
        tq.elapsed() / n.max(1) as u32
    );
    nav_worst.sort_by(|a, b| b.3.total_cmp(&a.3));
    nav_worst.truncate(5);
    println!(
        "grounded spawns vs navmesh: {nav_above} over 1 m above ground, {nav_below} over 1 m below; worst {nav_worst:?}"
    );
    assert!(n > 50);
    assert!(near * 10 >= n * 9, "{near}/{n}");
    assert!(near >= terrain_near);
    assert!(
        nav_above > 0,
        "fixture covers a navmesh height above the collision floor"
    );
    // some spawns stand on brushes (bridges, floors, docks)
    assert!(on_static > 0);
}

/// The doodad model references of the DB resolve to colliders (cgf / vegetation / prefab).
#[test]
#[ignore = "needs AAEMU_GAME_PAK and AAEMU_COMPACT_DB"]
fn real_collision_doodad_models_resolve() {
    let Some(e) = env() else { return };
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("db");
    let data = super::data::CollisionData::load(&conn).expect("collision data");
    println!(
        "{} func group models, {} actor models",
        data.func_group_models.len(),
        data.living.len()
    );
    assert!(data.func_group_models.len() > 10_000);
    let files = Files::Client(e.files.clone());
    let models = ModelCache::new(Arc::new(files.clone()));
    let mut prefab_xml = HashMap::new();
    let mut by_kind: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut refs: Vec<&String> = data.func_group_models.values().collect();
    refs.sort();
    refs.dedup();
    for m in refs.iter().take(1500) {
        let r = super::objects::parse_model_ref(m);
        let kind = match &r {
            super::objects::ModelRef::Brush(_) => "cgf",
            super::objects::ModelRef::Vegetation(_) => "vegetation",
            super::objects::ModelRef::Prefab { .. } => "prefab",
            super::objects::ModelRef::Cga(_) => "cga",
            super::objects::ModelRef::Dummy => "dummy",
        };
        let ok = super::objects::resolve_shape(&files, &models, &mut prefab_xml, &r).is_some();
        let c = by_kind.entry(kind).or_default();
        c.0 += 1;
        c.1 += usize::from(ok);
    }
    println!("resolved (refs, with physics): {by_kind:?}");
    assert!(by_kind.get("cgf").is_some_and(|c| c.1 * 2 > c.0));
    assert!(by_kind.get("prefab").is_some_and(|c| c.1 > 0));
    // list the models whose physics fails to load
    for m in refs.iter().take(1500) {
        if let super::objects::ModelRef::Brush(p)
        | super::objects::ModelRef::Cga(p)
        | super::objects::ModelRef::Vegetation(p) = super::objects::parse_model_ref(m)
        {
            if let Some(b) = e.files.read_file(&p) {
                if let Err(err) = super::models::load_model(&p, &b) {
                    println!("load failure: {err}");
                }
            }
        }
    }
    assert_eq!(
        models
            .stats
            .failed
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

/// A real server: main_world as instance 0 (`WorldManager.CreateStaticInstances`) with the
/// client heightmaps and the collision service (lazy pool, never used; needs a tokio runtime).
fn real_server(e: &Env) -> crate::server::GameServer {
    real_server_with(e, false)
}

/// [`real_server`] with `World.GeoDataMode` on or off in the configuration.
fn real_server_with(e: &Env, geo_data_mode: bool) -> crate::server::GameServer {
    let worlds = WorldTemplates::load(&e.files, &app_path()).expect("world templates");
    let mut config = crate::config::AppConfig {
        id: 1,
        ..Default::default()
    };
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    config.world.geo_data_mode = geo_data_mode;
    if std::env::var_os("AAEMU_NO_VISAREAS").is_some() {
        // The audit's "before" run: brush.dat and vegetation.dat only.
        config.physics.vis_areas = false;
        config.physics.voxels = false;
    }
    let db = sqlx::mysql::MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let mut data = crate::data::GameData::empty();
    data.worlds = worlds;
    let server = crate::server::GameServer::new(config, db, data);
    server.systems.npcs.heightmaps.install(e.hm.clone());
    server
}

/// Walking height resolution for the six Mistfiend (NPC 4227) spawns of Blackbreath Cave
/// (`Data/Worlds/main_world/npc_spawns.json`): their authored Z sits below the heightmap
/// surface, so the production `systems::ai::npc::reference_height_in` must never lift a
/// walker above its level plus the 2 m step-up probe, and must prefer a collision floor when
/// the probe finds one. Prints independent terrain / probe / production diagnostics.
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_collision_mistfiend_cave_walking_height() {
    use crate::systems::npcs::json;
    let Some(e) = env() else { return };
    let text = std::fs::read_to_string(app_path().join("Data/Worlds/main_world/npc_spawns.json"))
        .expect("npc_spawns.json");
    let doc = json::parse(&text).expect("spawns parse");
    let mut spawns = Vec::new();
    for entry in json::array(&doc).expect("spawn array") {
        let Some(o) = entry.as_object() else { continue };
        if json::u32_or(o, "UnitId", 0).unwrap_or(0) != 4227 {
            continue;
        }
        let Some(p) = json::prop(o, "Position").and_then(|p| p.as_object()) else {
            continue;
        };
        spawns.push([
            json::f32_or(p, "X", 0.0).unwrap_or(0.0),
            json::f32_or(p, "Y", 0.0).unwrap_or(0.0),
            json::f32_or(p, "Z", 0.0).unwrap_or(0.0),
        ]);
    }
    assert_eq!(spawns.len(), 6, "NPC 4227 spawns: {spawns:?}");

    let server = real_server(&e);

    let mut above = 0;
    for s in &spawns {
        let (x, y, z) = (s[0], s[1], s[2]);
        let worlds = &server.game_data.worlds;
        let zone = worlds.get_zone_id(worlds.get_world_template_by_name("main_world"), x, y);
        let ai = crate::systems::npcs::heightmap::ReferenceHeightAi {
            can_fly: false,
            hold_or_idle: false,
            spawner_z: Some(z),
            world_z: z,
        };
        let resolved =
            crate::systems::ai::npc::reference_height_in(&server, 0, Some(ai), x, y, z, zone);
        let probe = super::ground_height_probe(
            &server,
            0,
            x,
            y,
            z,
            super::GROUND_PROBE_UP,
            super::GROUND_PROBE_DOWN,
        );
        let terrain = super::terrain_height(&server, 0, x, y);
        above += usize::from(terrain.is_some_and(|t| t > z + super::GROUND_PROBE_UP));
        println!(
            "Mistfiend ({x:.2}, {y:.2}, {z:.3}): probe {probe:?}, terrain {terrain:?}, resolved {resolved:.3}"
        );
        // A probe hit is the preferred collision floor (a hit at Z = 0 included).
        if let Some(h) = probe.flatten() {
            assert!(
                (resolved - h).abs() < 1e-3,
                "walking must stand on the collision floor {h:.3} at ({x:.2}, {y:.2}, {z:.3}), got {resolved:.3}"
            );
        }
        assert!(
            resolved <= z + super::GROUND_PROBE_UP + 1e-3,
            "walking must not elevate ({x:.2}, {y:.2}, {z:.3}) to {resolved:.3}"
        );
    }
    println!("{above}/6 columns have the heightmap surface above the step range");
    assert!(above > 0, "the cave fixture must cover a surface above");
}

/// Boat-summon points the client sent on the shared server (`x, y, z` of the refused
/// "a ship must be summoned on water" placements; all open sea).
const REFUSED_SUMMON_POINTS: [(f32, f32, f32); 10] = [
    (13841.5, 10491.5, 100.3),
    (13831.4, 10480.7, 100.6),
    (13822.1, 10501.5, 100.3),
    (9800.4, 8609.2, 100.4),
    (9802.4, 8613.8, 100.2),
    (16300.0, 9016.3, 100.1),
    (16382.9, 8961.7, 100.058),
    (9806.8, 8620.4, 100.6),
    (13731.0, 14070.8, 100.3),
    (20083.7, 18003.8, 100.1),
];

/// Why the shared server refused open-sea boat summons: the ocean has no water volume, only
/// the template level (`IsWater` is `z <= level`), and the client's points sit 0.06-0.6 m above
/// it. The tolerant summon check accepts them and still rejects a beach just above sea level.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_water_boat_summon_points() {
    use crate::systems::ai::physics::water::WaterBodies;
    use crate::systems::skills::controllers::Vec3 as WVec3;
    use crate::systems::vehicles::ship_physics::{point_on_summon_water, SUMMON_WATER_TOLERANCE};
    let Some(e) = env() else { return };
    let wh = e.hm.template(e.template_id).expect("main_world heightmap");
    let water = WaterBodies::new(e.ocean);
    let tolerant = |water: &WaterBodies, p: [f32; 3]| {
        point_on_summon_water(
            p,
            SUMMON_WATER_TOLERANCE,
            |q| water.is_water(WVec3::new(q[0], q[1], q[2])).is_some(),
            |q| water.get_water_surface(WVec3::new(q[0], q[1], q[2])).0,
            |x, y| e.hm.world_height(e.template_id, x, y).ok(),
        )
    };
    for &(x, y, z) in &REFUSED_SUMMON_POINTS {
        // reading the height loads the cell, so its water volumes can be ingested
        let ground = e.hm.world_height(e.template_id, x, y).unwrap();
        water.sync_loaded_cells(wh, e.hm.files(), "main_world", e.ocean);
        let strict = water.is_water(WVec3::new(x, y, z)).is_some();
        println!(
            "({x}, {y}, {z}): ground {ground}, surface {}, strict {strict}, tolerant {}",
            water.get_water_surface(WVec3::new(x, y, z)).0,
            tolerant(&water, [x, y, z]),
        );
        assert!(ground < e.ocean, "open sea at ({x}, {y})");
        assert!(z > e.ocean, "the client point is above the nominal surface");
        assert!(!strict, "the strict check refused ({x}, {y}, {z})");
        assert!(tolerant(&water, [x, y, z]), "({x}, {y}, {z}) is open sea");
        assert!(
            !tolerant(&water, [x, y, e.ocean + SUMMON_WATER_TOLERANCE + 0.5]),
            "a point well above the sea is not water"
        );
    }
    // Land just above sea level (beaches) never becomes a boat spawn.
    let mut beaches = 0;
    for (cx, cy) in [(13, 10), (9, 8), (16, 8), (13, 13), (19, 17)] {
        for ix in cx * 512..(cx + 1) * 512 {
            for iy in cy * 512..(cy + 1) * 512 {
                let ground = wh.grid_sample(e.hm.files(), ix, iy);
                if ground <= e.ocean + 0.05 || ground >= e.ocean + SUMMON_WATER_TOLERANCE {
                    continue;
                }
                let (x, y) = ((ix * 2) as f32, (iy * 2) as f32);
                water.sync_loaded_cells(wh, e.hm.files(), "main_world", e.ocean);
                let p = [x, y, ground + 0.05];
                if water.is_water(WVec3::new(p[0], p[1], p[2])).is_some() {
                    continue; // inside an authored water volume
                }
                beaches += 1;
                assert!(!tolerant(&water, p), "beach ({x}, {y}, {}) is land", p[2]);
            }
        }
    }
    println!("{beaches} beach samples stayed land");
    assert!(beaches > 1000);
}

/// Skill obstacle rays (`validation.rs`, `FUN_390B5540`: objtypes `SOLID_TERRAIN`, flags
/// `SKILL_OBSTACLE`, `to` already shortened by the target radius) that the shared server logged
/// as "Skill line of sight blocked" while the feet-level ray of 28286101a was in use: `(from, to,
/// production-logged distance, distance on the current grid)`. All are skill 11918 at NPCs and
/// hit plain terrain on gentle slopes (hit normals within 0.17 of straight up).
///
/// The production distances were logged while the heightmap u16 grid used the C# coefficient
/// (`65535 / (4096 / 4)`, 1/64 m steps, truncated). On 2026-11-08 the grid moved to
/// `65535 / 4096` (1/16 m steps, rounded) so that heights above 1024 m fit (PORTING.md), which
/// moves a stored sample by up to 4.7 cm (the rounding adds up to 3.1 cm to the old truncation's
/// 1.6 cm downward bias). Ray 1 runs nearly parallel to its slope (it drops 0.72 m over the
/// 4.36 m span, the terrain 0.62 m), so that shift moves its intersection from 3.638 m to
/// 2.078 m while the terrain is the client's: at grid corner (20080, 7340) `heightmap.dat`
/// holds 190.250 m, the old grid read 190.237 m and the current grid 190.253 m; `get_height` at
/// the ray start reads 190.042 m / 190.059 m old / current, and the original-client recheck
/// 0.4 km away at Hatora (20028.4, 7599.8) 179.2648 m matches the current grid (179.2027 m)
/// about as well as the old one (179.1996 m). The production value stays for provenance; the
/// current grid's value is the pinned replay.
const REFUSED_TERRAIN_RAYS: [([f32; 3], [f32; 3], f32, f32); 6] = [
    (
        [20081.496, 7340.0938, 190.1103],
        [20085.46, 7341.905, 189.39467],
        3.638,
        2.0780995,
    ),
    (
        [20105.953, 7335.5586, 187.4872],
        [20115.842, 7332.403, 187.5405],
        9.265,
        8.231512,
    ),
    (
        [20549.164, 7251.6543, 194.1449],
        [20552.47, 7263.1826, 191.80545],
        0.706,
        0.49256143,
    ),
    (
        [20550.197, 7255.8984, 193.4926],
        [20552.162, 7262.3022, 192.07394],
        1.900,
        2.326965,
    ),
    (
        [20550.803, 7258.49, 193.0254],
        [20551.89, 7261.642, 192.25285],
        0.805,
        1.1499643,
    ),
    (
        [20529.824, 7269.074, 188.9108],
        [20520.742, 7270.4155, 187.92221],
        3.848,
        3.1512997,
    ),
];

/// A seventh logged refusal, over open sea off the Two Crowns coast (heightmap 93-97 m, ray at
/// 100.3-101.1 m). The server logged a hit (owner not recorded), but the static world and the
/// terrain answer nothing there at feet level, so the production hit must have come from a live
/// object collider (a ship or doodad) that this replay does not have.
const OPEN_SEA_RAY: ([f32; 3], [f32; 3]) = (
    [13821.418, 10522.8125, 100.333],
    [13818.172, 10503.699, 101.13336],
);

/// The logged terrain rays hit again at feet level (reproducing the production refusals), and
/// the same rays raised by `OBSTACLE_RAY_HEIGHT` at both ends clear, as does the open-sea ray.
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_skill_obstacle_rays_refused_at_feet_level_clear_when_raised() {
    use crate::systems::skills::validation::OBSTACLE_RAY_HEIGHT;
    let Some(e) = env() else { return };
    let server = real_server(&e);
    let cast = |from: [f32; 3], to: [f32; 3], raised: f32| {
        super::raycast(
            &server,
            0,
            [from[0], from[1], from[2] + raised],
            [to[0], to[1], to[2] + raised],
            mask::SOLID_TERRAIN,
            ray_flags::SKILL_OBSTACLE,
        )
        .expect("collision service")
    };
    let mut still_blocked = Vec::new();
    for (i, (from, to, logged, expected)) in REFUSED_TERRAIN_RAYS.iter().enumerate() {
        let feet = cast(*from, *to, 0.0).unwrap_or_else(|| panic!("ray {} must hit", i + 1));
        assert!(
            (feet.dist - expected).abs() < 0.01 && matches!(feet.owner, Some(Owner::Terrain)),
            "ray {} hit {:?} at {}, expected terrain at {expected} (production logged {logged})",
            i + 1,
            feet.owner,
            feet.dist
        );
        let raised = cast(*from, *to, OBSTACLE_RAY_HEIGHT);
        println!(
            "ray {}: feet terrain at {:.3}; raised {:?}",
            i + 1,
            feet.dist,
            raised.as_ref().map(|h| (h.pos, h.dist, &h.owner))
        );
        if raised.is_some() {
            still_blocked.push(i + 1);
        }
    }
    let sea = cast(OPEN_SEA_RAY.0, OPEN_SEA_RAY.1, OBSTACLE_RAY_HEIGHT);
    println!(
        "open sea ray: raised {:?}",
        sea.as_ref().map(|h| (h.pos, h.dist))
    );
    if sea.is_some() {
        still_blocked.push(7);
    }
    assert!(
        still_blocked.is_empty(),
        "rays {still_blocked:?} still hit {OBSTACLE_RAY_HEIGHT} m above both ends"
    );
}

/// Dry ground where Infinitb drowned on the shared server (quest area around (22 450, 7 100),
/// ground z 272-282). Water volumes of cells 24-7 / 24-8 reaching past the old "local" band were
/// stretched to the world origin and covered this land with 8-15 m of phantom water.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_water_no_phantom_water_on_land() {
    use crate::systems::ai::physics::water::WaterBodies;
    use crate::systems::skills::controllers::Vec3 as WVec3;
    let Some(e) = env() else { return };
    let wh = e.hm.template(e.template_id).expect("main_world heightmap");
    let water = WaterBodies::new(e.ocean);
    for cx in 19..26 {
        for cy in 4..10 {
            let (x, y) = (cx as f32 * 1024.0 + 5.0, cy as f32 * 1024.0 + 5.0);
            e.hm.world_height(e.template_id, x, y).unwrap();
        }
    }
    water.sync_loaded_cells(wh, e.hm.files(), "main_world", e.ocean);
    for a in water.areas_snapshot() {
        let b = a.bounding_box;
        assert!(
            b.width < 4096.0 && b.height < 4096.0,
            "{} spans {:?}",
            a.name,
            b
        );
    }
    let mut wet = 0;
    for ix in 0..60 {
        for iy in 0..40 {
            let (x, y) = (22350.0 + ix as f32 * 4.0, 7020.0 + iy as f32 * 4.0);
            let ground = e.hm.world_height(e.template_id, x, y).unwrap();
            if water.is_water(WVec3::new(x, y, ground + 0.1)).is_some() {
                wet += 1;
            }
        }
    }
    assert_eq!(wet, 0, "dry ground samples read as water");
}

/// The authored `(x, y, z)` of every NPC spawn of one world's `npc_spawns.json`.
fn world_npc_spawns(world: &str) -> Vec<[f32; 3]> {
    use crate::systems::npcs::json;
    let text =
        std::fs::read_to_string(app_path().join(format!("Data/Worlds/{world}/npc_spawns.json")))
            .expect("npc_spawns.json");
    let doc = json::parse(&text).expect("spawns parse");
    let mut spawns = Vec::new();
    for entry in json::array(&doc).expect("spawn array") {
        let Some(o) = entry.as_object() else { continue };
        let Some(p) = json::prop(o, "Position").and_then(|p| p.as_object()) else {
            continue;
        };
        spawns.push([
            json::f32_or(p, "X", 0.0).unwrap_or(0.0),
            json::f32_or(p, "Y", 0.0).unwrap_or(0.0),
            json::f32_or(p, "Z", 0.0).unwrap_or(0.0),
        ]);
    }
    spawns
}

/// The instance halls whose floor model was not in the collision world before `visareas.dat`
/// was loaded (the halls are `visareas.dat` brushes, not `object.dat` / `big_object.dat`): the NPC
/// spawns sit 44-200 m above the terrain, which is all the ground probe finds without them. A walker must keep standing where the world authored it (the
/// navmesh height with `GeoDataMode`, else its level) instead of dropping to that terrain with its
/// first step (Kroloal Cradle monsters stood 191 m below their spawns, out of reach).
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_instance_walkers -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_instance_walkers_keep_the_floor_of_their_hall() {
    use crate::systems::npcs::heightmap::ReferenceHeightAi;
    let Some(e) = env_with(true) else { return };
    let server = std::sync::Arc::new(real_server_with(&e, true));
    // The navmesh heights, as the server installs them at startup (`GeoDataMode`).
    crate::systems::ai::geodata::start(&server);
    for name in [
        "instance_cradle_of_destruction",
        "instance_howling_abyss",
        "instance_cuttingwind_deadmine",
        "instance_library",
        "instance_sal_temple",
        "instance_nachashgar",
        "instance_burntcastle_armory",
        "instance_hadir_farm",
    ] {
        let template = server
            .game_data
            .worlds
            .get_world_template_by_name(name)
            .unwrap_or_else(|| panic!("{name}"))
            .clone();
        let zone = template.zone_keys.first().copied().unwrap_or(0);
        server.world.create_world_instance(Arc::new(template), 100);
        let spawns = world_npc_spawns(name);
        assert!(!spawns.is_empty(), "{name}");
        let (mut navmesh, mut kept, mut worst) = (0, 0, 0.0f32);
        for s in spawns.iter().take(400) {
            let (x, y, z) = (s[0], s[1], s[2]);
            let ai = ReferenceHeightAi {
                can_fly: false,
                hold_or_idle: false,
                spawner_z: Some(z),
                world_z: z,
            };
            let resolved =
                crate::systems::ai::npc::reference_height_in(&server, 100, Some(ai), x, y, z, zone);
            worst = worst.max(z - resolved);
            if (resolved - z).abs() < 1e-3 {
                kept += 1;
            } else {
                navmesh += 1;
            }
            assert!(
                z - resolved <= crate::systems::ai::npc::WALKER_MAX_DROP + 1e-3,
                "{name}: a walker at ({x:.1}, {y:.1}, {z:.1}) was resolved to {resolved:.1}"
            );
            assert!(
                resolved <= z + 2.0 + 1e-3,
                "{name}: ({x}, {y}, {z}) rose to {resolved}"
            );
        }
        println!(
            "{name}: {} spawns, kept {kept}, re-resolved {navmesh}, worst drop {worst:.2}",
            spawns.len().min(400)
        );
        server.world.remove_world(100);
        server.systems.ai.collision.remove(100);
    }
}

/// The Profane Temple (`instance_sal_temple`) end boss Akmit (12189) knocked back in its hall: the
/// ground probe only finds the terrain far under the hall, which testers saw the boss land on
/// (2026-11-08). The displaced position must stay at the hall level.
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_sal_temple_boss -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_sal_temple_boss_knockback_stays_on_the_hall_floor() {
    use crate::models::npc::Npc;
    use crate::systems::skills::effects::special::movement::move_npc;
    use crate::world::{Entity, WorldObject};
    let Some(e) = env_with(true) else { return };
    let server = std::sync::Arc::new(real_server_with(&e, true));
    crate::systems::ai::geodata::start(&server);
    let name = "instance_sal_temple";
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(name)
        .expect(name)
        .clone();
    let zone = template.zone_keys.first().copied().unwrap_or(0);
    server.world.create_world_instance(Arc::new(template), 100);
    let (x, y, z) = (615.5f32, 434.4f32, 148.8f32);
    assert!(
        super::ground_height(&server, 100, x, y, z).is_some_and(|h| (h - 148.84).abs() < 0.2),
        "the Akmit floor of the hall"
    );
    let npc = Entity::new(0x400, {
        let mut n = Npc::new(12189);
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
        n.unit.transform.set_xyz(x, y, z);
        n.unit.transform.zone_id = zone;
        n.unit.transform.instance_id = 100;
        n.unit.transform.world_id = 100;
        n.spawner_z = Some(z);
        n
    });
    server
        .world
        .world(100)
        .expect("instance")
        .spawn(&npc, &server.view_ctx());
    let _ = WorldObject::from(&npc);
    for (dx, dy) in [(5.0, 0.0), (-5.0, 0.0), (0.0, 5.0), (0.0, -5.0)] {
        npc.data().unit.transform.set_xyz(x, y, z);
        move_npc(&server, &npc, (x + dx, y + dy, z), None, true);
        let p = npc.data().unit.transform;
        println!("knocked by ({dx}, {dy}): z = {:.2}", p.z);
        assert!(
            (p.z - z).abs() <= 12.0,
            "the boss was displaced from {z} to {} under the hall",
            p.z
        );
    }
    server.world.remove_world(100);
    server.systems.ai.collision.remove(100);
}

/// Every `visareas.dat` and `object.dat` of every world parses strictly (no leftover bytes) and
/// the voxels inflate to a physics mesh.
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_visareas_parse -- --ignored --nocapture`
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_visareas_parse_every_file() {
    let Some(e) = env() else { return };
    let worlds = WorldTemplates::load(&e.files, &app_path()).expect("world templates");
    let (mut files, mut voxels, mut bad) = (0, 0, Vec::new());
    for t in worlds.templates() {
        for cx in 0..t.cell_x {
            for cy in 0..t.cell_y {
                let dir = format!("game/worlds/{}/cells/{cx:03}_{cy:03}/client", t.name);
                if let Some(d) = e.files.read_file(&format!("{dir}/visareas.dat")) {
                    files += 1;
                    match cells::read_visareas(&d) {
                        Ok(f) => {
                            for v in &f.voxels {
                                voxels += 1;
                                let ok = v.inflate().is_some_and(|b| {
                                    super::models::load_model("v", &b)
                                        .ok()
                                        .flatten()
                                        .is_some_and(|m| m.collides())
                                });
                                if !ok {
                                    bad.push(format!("{dir}: voxel without a mesh"));
                                }
                            }
                        }
                        Err(err) => bad.push(format!("{dir}/visareas.dat: {err}")),
                    }
                }
                if let Some(d) = e.files.read_file(&format!("{dir}/object.dat")) {
                    files += 1;
                    match cells::read_object_dat(&d) {
                        Ok(f) => voxels += f.voxels.len(),
                        Err(err) => bad.push(format!("{dir}/object.dat: {err}")),
                    }
                }
            }
        }
    }
    println!("{files} files, {voxels} voxels, {} failures", bad.len());
    assert!(bad.is_empty(), "{bad:#?}");
    assert!(files > 0 && voxels > 0);
}

/// Floors the static collision of `visareas.dat` and the cave voxels now provide: the Profane
/// Temple hall under Akmit, the Dahuta cave and a library hall.
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_visareas_and_voxel_floors() {
    let Some(e) = env_with(true) else { return };
    let server = std::sync::Arc::new(real_server_with(&e, true));
    let t = server
        .game_data
        .worlds
        .get_world_template_by_name("instance_sal_temple")
        .expect("sal_temple")
        .clone();
    server.world.create_world_instance(Arc::new(t), 100);
    let h = super::ground_height(&server, 100, 615.5, 434.4, 148.8);
    println!("sal_temple Akmit floor {h:?}");
    assert!(h.is_some_and(|h| (h - 148.84).abs() < 0.2), "{h:?}");
    let h = super::ground_height(&server, 0, 11935.0, 16408.0, 291.8);
    println!("Dahuta cave floor {h:?}");
    assert!(h.is_some_and(|h| (h - 291.11).abs() < 1.0), "{h:?}");
    let t = server
        .game_data
        .worlds
        .get_world_template_by_name("instance_library")
        .expect("library")
        .clone();
    server.world.create_world_instance(Arc::new(t), 101);
    let spawns = world_npc_spawns("instance_library");
    let on_floor = spawns
        .iter()
        .take(200)
        .filter(|s| {
            super::ground_height(&server, 101, s[0], s[1], s[2])
                .is_some_and(|h| (h - s[2]).abs() < 2.0)
        })
        .count();
    println!(
        "library: {on_floor}/{} spawns with a floor within 2 m",
        spawns.len().min(200)
    );
    assert!(on_floor * 2 > spawns.len().min(200));
}

/// Static audit: how many NPC spawns of each world have a floor within 2 m of their Z through
/// `ground_height`. Run with `AAEMU_NO_VISAREAS=1` for the "before" numbers.
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_spawn_floor_audit -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_spawn_floor_audit() {
    let Some(e) = env_with(true) else { return };
    let server = std::sync::Arc::new(real_server_with(&e, true));
    let t0 = Instant::now();
    let (mut total, mut ok_total) = (0, 0);
    for (i, t) in server
        .game_data
        .worlds
        .templates()
        .to_vec()
        .into_iter()
        .enumerate()
    {
        let path = app_path().join(format!("Data/Worlds/{}/npc_spawns.json", t.name));
        if !path.exists() {
            continue;
        }
        let spawns = world_npc_spawns(&t.name);
        let id = if t.name == "main_world" {
            0
        } else {
            200 + i as u32
        };
        if id != 0 {
            server.world.create_world_instance(Arc::new(t.clone()), id);
        }
        let ok = spawns
            .iter()
            .filter(|s| {
                super::ground_height(&server, id, s[0], s[1], s[2])
                    .is_some_and(|h| (h - s[2]).abs() <= 2.0)
            })
            .count();
        println!(
            "{}: {} spawns, {} with a floor within 2 m",
            t.name,
            spawns.len(),
            ok
        );
        total += spawns.len();
        ok_total += ok;
        if id != 0 {
            server.world.remove_world(id);
            server.systems.ai.collision.remove(id);
        }
    }
    println!(
        "total {total}, floor within 2 m: {ok_total}, {:?}",
        t0.elapsed()
    );
}

/// Roam-target path checks (2026-11-08 measurement): for the NPC spawns of main_world (open
/// terrain / town split by the brushes within 12 m) and two instance halls, how often is the
/// straight segment spawn -> random point in 6 m blocked, by `clip_move` (capsule blink,
/// shortfall > 0.5 m), by a slope-following ray at 1 m (static + terrain, 0x101) and by the
/// same ray against static geometry only (0x001).
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_roam_path_block_rates -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_roam_path_block_rates() {
    use crate::systems::npcs::heightmap::ReferenceHeightAi;
    let Some(e) = env_with(true) else { return };
    let server = std::sync::Arc::new(real_server_with(&e, true));
    crate::systems::ai::geodata::start(&server);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 40) as f32 / (1u64 << 24) as f32
    };
    let worlds: [(&str, u32, usize); 3] = [
        ("main_world", 0, 3000),
        ("instance_sal_temple", 100, 400),
        ("instance_library", 101, 400),
    ];
    for (name, id, take) in worlds {
        if id != 0 {
            let t = server
                .game_data
                .worlds
                .get_world_template_by_name(name)
                .expect(name)
                .clone();
            server.world.create_world_instance(Arc::new(t), id);
        }
        let zone = server
            .game_data
            .worlds
            .get_world_template_by_name(name)
            .unwrap()
            .zone_keys
            .first()
            .copied()
            .unwrap_or(0);
        let spawns = world_npc_spawns(name);
        let step = (spawns.len() / take).max(1);
        // [open, town] for main_world; [all] otherwise
        #[derive(Default, Clone, Copy)]
        struct Acc {
            n: u32,
            clip: u32,
            ray_ts: u32,
            ray_s: u32,
            only_clip: u32,
            only_ray_ts: u32,
            clip_us: u128,
            ray_us: u128,
            h05: u32,
            h15: u32,
            veg: u32,
            floorish: u32,
        }
        let mut acc = [Acc::default(), Acc::default()];
        for s in spawns.iter().step_by(step) {
            let from = *s;
            let town = if id == 0 {
                let boxes = super::box_obstacles(
                    &server,
                    id,
                    [from[0], from[1], from[2] + 2.0],
                    [12.0, 12.0, 6.0],
                    0.0,
                    0x001,
                )
                .unwrap_or_default();
                boxes
                    .iter()
                    .filter(|(o, _)| matches!(o, Owner::Brush { .. }))
                    .count()
                    >= 3
            } else {
                false
            };
            for _ in 0..3 {
                let x = (rnd() - 0.5) * 12.0 + from[0];
                let y = (rnd() - 0.5) * 12.0 + from[1];
                let z = crate::systems::ai::npc::reference_height_in(
                    &server,
                    id,
                    Some(ReferenceHeightAi {
                        can_fly: false,
                        hold_or_idle: false,
                        spawner_z: Some(from[2]),
                        world_z: from[2],
                    }),
                    x,
                    y,
                    from[2],
                    zone,
                );
                let to = [x, y, z];
                let a = &mut acc[usize::from(town)];
                a.n += 1;
                let t0 = Instant::now();
                let clipped = super::displacement::clip_move(&server, id, from, to);
                a.clip_us += t0.elapsed().as_micros();
                let full = ((to[0] - from[0]).powi(2) + (to[1] - from[1]).powi(2)).sqrt();
                let clip_blocked = clipped.is_some_and(|c| {
                    let got = ((c[0] - from[0]).powi(2) + (c[1] - from[1]).powi(2)).sqrt();
                    got < full - 0.5
                });
                let t1 = Instant::now();
                let ray = |mask: u32| {
                    super::raycast(
                        &server,
                        id,
                        [from[0], from[1], from[2] + 1.0],
                        [to[0], to[1], to[2] + 1.0],
                        mask,
                        ray_flags::DEFAULT,
                    )
                    .is_some_and(|h| h.is_some())
                };
                let r_ts = ray(mask::STATIC_TERRAIN);
                a.ray_us += t1.elapsed().as_micros();
                let r_s = ray(0x001);
                let hit_at = |h: f32| {
                    super::raycast(
                        &server,
                        id,
                        [from[0], from[1], from[2] + h],
                        [to[0], to[1], to[2] + h],
                        0x001,
                        ray_flags::DEFAULT,
                    )
                    .flatten()
                };
                a.h05 += u32::from(hit_at(0.5).is_some());
                a.h15 += u32::from(hit_at(1.5).is_some());
                if let Some(h) = hit_at(1.0) {
                    a.veg += u32::from(matches!(h.owner, Some(Owner::Vegetation { .. })));
                    a.floorish += u32::from(h.normal.z.abs() > 0.6);
                }
                a.clip += u32::from(clip_blocked);
                a.ray_ts += u32::from(r_ts);
                a.ray_s += u32::from(r_s);
                a.only_clip += u32::from(clip_blocked && !r_s);
                a.only_ray_ts += u32::from(r_ts && !r_s);
            }
        }
        for (label, a) in ["open/all", "town"].iter().zip(acc.iter()) {
            if a.n == 0 {
                continue;
            }
            println!(
                "{name} [{label}]: n={} clip_move blocked {} ({:.1}%), ray1m static+terrain {} ({:.1}%), ray1m static {} ({:.1}%); clip-only(not static ray) {}, terrain-only ray {}; clip {:.0} us, ray {:.0} us avg; static ray at 0.5 m {}, at 1.5 m {}; 1 m hits on vegetation {}, on floor-like surfaces (|n.z|>0.6) {}",
                a.n,
                a.clip,
                100.0 * a.clip as f32 / a.n as f32,
                a.ray_ts,
                100.0 * a.ray_ts as f32 / a.n as f32,
                a.ray_s,
                100.0 * a.ray_s as f32 / a.n as f32,
                a.only_clip,
                a.only_ray_ts,
                a.clip_us as f32 / a.n as f32,
                a.ray_us as f32 / a.n as f32,
                a.h05,
                a.h15,
                a.veg,
                a.floorish,
            );
        }
        if id != 0 {
            server.world.remove_world(id);
            server.systems.ai.collision.remove(id);
        }
    }
}

/// The production wall check (`walk::walk_blocked`) on the roam targets of the measurement above,
/// and on every segment of every authored `Data/Path/*.path` route in main_world (split into
/// 4 m pieces with the ground height under each end; routes in the air are skipped).
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_walk_check_rates -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_walk_check_rates() {
    use crate::systems::npcs::heightmap::ReferenceHeightAi;
    let Some(e) = env_with(true) else { return };
    let server = std::sync::Arc::new(real_server_with(&e, true));
    crate::systems::ai::geodata::start(&server);
    let mut seed = 0x1234_5678_9ABC_DEF1u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 40) as f32 / (1u64 << 24) as f32
    };
    // roam targets
    let spawns = world_npc_spawns("main_world");
    let step = (spawns.len() / 3000).max(1);
    let (mut n, mut blocked, mut town_n, mut town_blocked) = (0u32, 0u32, 0u32, 0u32);
    let t0 = Instant::now();
    for s in spawns.iter().step_by(step) {
        let town = super::box_obstacles(
            &server,
            0,
            [s[0], s[1], s[2] + 2.0],
            [12.0, 12.0, 6.0],
            0.0,
            0x001,
        )
        .unwrap_or_default()
        .iter()
        .filter(|(o, _)| matches!(o, Owner::Brush { .. }))
        .count()
            >= 3;
        for _ in 0..3 {
            let x = (rnd() - 0.5) * 12.0 + s[0];
            let y = (rnd() - 0.5) * 12.0 + s[1];
            let z = crate::systems::ai::npc::reference_height_in(
                &server,
                0,
                Some(ReferenceHeightAi {
                    can_fly: false,
                    hold_or_idle: false,
                    spawner_z: Some(s[2]),
                    world_z: s[2],
                }),
                x,
                y,
                s[2],
                0,
            );
            let b = super::walk::walk_blocked(&server, 0, *s, [x, y, z]).unwrap_or(false);
            if town {
                town_n += 1;
                town_blocked += u32::from(b);
            } else {
                n += 1;
                blocked += u32::from(b);
            }
        }
    }
    println!(
        "roam targets: open {blocked}/{n} blocked ({:.2}%), town {town_blocked}/{town_n} ({:.2}%), {:.0} us per check",
        100.0 * blocked as f32 / n.max(1) as f32,
        100.0 * town_blocked as f32 / town_n.max(1) as f32,
        t0.elapsed().as_micros() as f32 / (n + town_n).max(1) as f32
    );
    // authored routes
    let folder = app_path().join("Data/Path");
    let mut files: Vec<_> = std::fs::read_dir(&folder)
        .expect("Data/Path")
        .filter_map(|f| f.ok())
        .map(|f| f.path())
        .collect();
    files.sort();
    let max_xy = (e.cells.0 as f32 * 1024.0, e.cells.1 as f32 * 1024.0);
    let (mut segs, mut bad_segs, mut skipped_air, mut skipped_out, mut routes_bad) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut worst: Vec<(u32, String)> = Vec::new();
    let t1 = Instant::now();
    let mut checks = 0u32;
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        let pts = crate::systems::ai::path_handler::parse_path_file(&text);
        let mut route_bad = 0;
        for w in pts.windows(2) {
            let (a, b) = (w[0].position, w[1].position);
            let zero = |p: crate::systems::skills::controllers::Vec3| {
                p.x == 0.0 && p.y == 0.0 && p.z == 0.0
            };
            if zero(a) || zero(b) {
                continue;
            }
            if [a, b]
                .iter()
                .any(|p| p.x < 0.0 || p.y < 0.0 || p.x >= max_xy.0 || p.y >= max_xy.1)
            {
                skipped_out += 1;
                continue;
            }
            let len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
            let pieces = (len / 4.0).ceil().max(1.0) as usize;
            let ground = |t: f32| -> Option<[f32; 3]> {
                let (x, y) = (a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
                let z = a.z + (b.z - a.z) * t;
                let g = super::ground_height_probe(&server, 0, x, y, z, 2.0, 12.0).flatten()?;
                Some([x, y, g])
            };
            let mut prev = ground(0.0);
            let mut seg_blocked = false;
            let mut seg_air = prev.is_none();
            for i in 1..=pieces {
                let cur = ground(i as f32 / pieces as f32);
                match (prev, cur) {
                    (Some(p), Some(c)) => {
                        checks += 1;
                        seg_blocked |= super::walk::walk_blocked(&server, 0, p, c).unwrap_or(false);
                    }
                    _ => seg_air = true,
                }
                prev = cur;
            }
            if seg_air {
                skipped_air += 1;
                continue;
            }
            segs += 1;
            if seg_blocked {
                bad_segs += 1;
                route_bad += 1;
            }
        }
        if route_bad > 0 {
            routes_bad += 1;
            worst.push((
                route_bad,
                f.file_name().unwrap().to_string_lossy().into_owned(),
            ));
        }
    }
    worst.sort_by(|a, b| b.cmp(a));
    println!(
        "{} route files: {segs} ground segments checked ({checks} pieces, {:.0} us each), {bad_segs} blocked in {routes_bad} routes; skipped {skipped_air} (no ground within 12 m / air), {skipped_out} outside main_world",
        files.len(),
        t1.elapsed().as_micros() as f32 / checks.max(1) as f32
    );
    println!("most blocked routes: {:?}", &worst[..worst.len().min(15)]);
}

/// The type-2 vegetation records of `object.dat` (not in `vegetation.dat`): per world how many
/// there are, how many resolve to a colliding model, how many duplicate a `vegetation.dat`
/// instance of the same cell (same group, position rounded to 0.1 m), and what loading them adds
/// to the collision instances of main_world.
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_object_dat_vegetation -- --ignored --nocapture`
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_object_dat_vegetation_audit() {
    let Some(e) = env() else { return };
    let files = Files::Client(e.files.clone());
    let models = ModelCache::new(Arc::new(files.clone()));
    let worlds = WorldTemplates::load(&e.files, &app_path()).expect("world templates");
    let mut total_records = 0usize;
    for t in worlds.templates() {
        let xml = e
            .files
            .read_file(&format!("game/worlds/{}/vegetation.xml", t.name))
            .map(|b| crate::data::client_files::decode_text(&b))
            .unwrap_or_default();
        let groups: HashMap<u16, cells::VegetationGroup> = cells::read_vegetation_groups(&xml)
            .into_iter()
            .map(|g| (g.id, g))
            .collect();
        let (mut records, mut colliding, mut dups, mut bad) = (0usize, 0usize, 0usize, 0usize);
        let (mut base_inst, mut with_inst) = (0usize, 0usize);
        let full = t.name == "main_world";
        for cx in 0..t.cell_x {
            for cy in 0..t.cell_y {
                let dir = format!("game/worlds/{}/cells/{cx:03}_{cy:03}/client", t.name);
                let mut load = super::world::CellLoad {
                    brushes: true,
                    vegetation: true,
                    visareas: true,
                    voxels: true,
                    object_vegetation: false,
                };
                let mut cell_base = 0;
                if full {
                    cell_base = super::world::prepare_cell_with(
                        &files,
                        &models,
                        &t.name,
                        &groups,
                        (cx, cy),
                        &load,
                    )
                    .instances
                    .len();
                    base_inst += cell_base;
                    with_inst += cell_base;
                }
                let Some(d) = e.files.read_file(&format!("{dir}/object.dat")) else {
                    continue;
                };
                let Ok(f) = cells::read_object_dat(&d) else {
                    bad += 1;
                    continue;
                };
                if f.vegetation.is_empty() {
                    continue;
                }
                let key = |v: &cells::VegetationRecord| {
                    (
                        v.group,
                        (v.pos.x * 10.0).round() as i32,
                        (v.pos.y * 10.0).round() as i32,
                        (v.pos.z * 10.0).round() as i32,
                    )
                };
                let existing: std::collections::HashSet<_> = e
                    .files
                    .read_file(&format!("{dir}/vegetation.dat"))
                    .and_then(|b| cells::read_vegetation(&b).ok())
                    .unwrap_or_default()
                    .iter()
                    .map(key)
                    .collect();
                for v in &f.vegetation {
                    records += 1;
                    if existing.contains(&key(v)) {
                        dups += 1;
                    }
                    let m = groups
                        .get(&v.group)
                        .filter(|g| !g.model.is_empty())
                        .and_then(|g| models.get(&g.model));
                    if m.is_some_and(|m| m.collides()) {
                        colliding += 1;
                    }
                }
                if full {
                    load.object_vegetation = true;
                    let b = super::world::prepare_cell_with(
                        &files,
                        &models,
                        &t.name,
                        &groups,
                        (cx, cy),
                        &load,
                    );
                    with_inst += b.instances.len() - cell_base;
                }
            }
        }
        total_records += records;
        println!(
            "{}: object.dat vegetation {records}, colliding {colliding}, duplicates of vegetation.dat {dups}, unparsable files {bad}",
            t.name
        );
        if full {
            println!(
                "main_world: instances {base_inst} -> {with_inst} (+{}, {:.1}%), StaticInstance {} bytes",
                with_inst - base_inst,
                (with_inst - base_inst) as f64 * 100.0 / base_inst.max(1) as f64,
                std::mem::size_of::<super::world::StaticInstance>()
            );
        }
    }
    assert!(total_records > 0);
}

/// Ynystere NPCs that stand on a structure (Rose Estate Manor floors, Brimblaze Tower on
/// Marooned Isle, the Finnea Harbor docks, mansion walls) but whose `npc_spawns.json` Z had been
/// dropped onto the terrain under it, so they spawned inside the floor or under water (tester
/// report: "Rose Estate Rafflesia clipping inside the building floor"). Each row is
/// `(unit id, X, Y, Z)` with X / Y equal to a 1.2 client `npc_map/e_ynystere.dat` record of
/// the same NPC and Z that record's height, kept only where the 1.2 collision has a floor within
/// 1 m of it (`real_client_floor_fixes_stand_on_a_collision_floor`) and it is above the
/// terrain. The other 76 Ynystere records more than 2 m off the spawn Z were left alone: flying
/// or swimming NPCs without a floor there, points under the terrain, and a stacked marker site
/// whose heights step by 2 m.
const YNYSTERE_FLOOR_FIXES: [(u32, f32, f32, f32); 28] = [
    (1415, 19934.4, 12351.43, 126.281),
    (1419, 21733.3, 12565.5, 225.902),
    (1423, 20828.93, 13186.52, 105.703),
    (1441, 21971.24, 13088.98, 283.021),
    (1458, 21963.35, 13105.03, 283.306),
    (1467, 19727.66, 11769.462, 175.925),
    (1474, 20302.47, 12974.6, 147.769),
    (8015, 19774.31, 11751.003, 128.903),
    (8015, 19728.73, 11756.884, 126.414),
    (8015, 19797.41, 11738.576, 128.321),
    (8015, 19729.22, 11772.153, 126.414),
    (8015, 19757.47, 11760.059, 126.336),
    (8043, 21971.85, 13058.12, 281.448),
    (8043, 21967.59, 13067.439, 271.794),
    (8043, 21975.9, 13035.78, 271.513),
    (8043, 21983.33, 13049.47, 281.526),
    (8043, 21985.82, 13042.92, 272.681),
    (8043, 21976.87, 13050.38, 271.794),
    (8243, 20864.81, 13391.21, 106.183),
    (8779, 19824.02, 12763.67, 102.233),
    (8779, 19812.72, 12770.14, 102.53),
    (8779, 19843.81, 12751.38, 101.951),
    (8779, 19833.8, 12811.5, 110.963),
    (8779, 19854.92, 12796.39, 110.929),
    (10758, 20802.23, 13338.65, 147.154),
    (13070, 20902.43, 13346.811, 123.84),
    (13070, 20901.55, 13374.66, 124.378),
    (13070, 20900.83, 13316.93, 124.138),
];

/// The same correction for Silent Forest (`npc_map/e_ancient_forest.dat`, 764 records, 809
/// matched spawns, 107 more than 2 m off, these 11 pass the floor check). Tester reports: the
/// westmost tomb of the Ancient Necropolis had its quest giver (Ancient Ghost 3266, 10 m under
/// the tomb floor at 350.366) and four Cursed Swordsmen (7496, 2.7 m under it) inside the
/// terrain; Warlock Erik (3123) and Avidin (8604) of the Cloaked Forest camp stood 21-22 m
/// below their floors.
const SILENT_FOREST_FLOOR_FIXES: [(u32, f32, f32, f32); 11] = [
    (3123, 23579.02, 11168.734, 373.848),
    (3141, 22684.18, 12103.12, 287.478),
    (3266, 24222.01, 11551.15, 350.366),
    (3311, 23121.2, 12171.22, 250.633),
    (3335, 22922.17, 12403.75, 251.362),
    (7496, 24261.45, 11559.21, 342.753),
    (7496, 24239.6, 11545.189, 342.775),
    (7496, 24258.129, 11542.83, 342.775),
    (7496, 24246.5, 11566.68, 342.775),
    (7498, 24522.15, 11726.91, 367.701),
    (8604, 23478.87, 11354.01, 342.266),
];

/// Every client-floor correction: [`YNYSTERE_FLOOR_FIXES`] and [`SILENT_FOREST_FLOOR_FIXES`].
fn client_floor_fixes() -> impl Iterator<Item = (u32, f32, f32, f32)> {
    YNYSTERE_FLOOR_FIXES
        .into_iter()
        .chain(SILENT_FOREST_FLOOR_FIXES)
}

/// The spawn file carries the client floor height of every [`client_floor_fixes`] row.
#[test]
fn structure_npcs_keep_the_client_floor_height() {
    use crate::systems::npcs::doodad::spawner::{parse_list, NpcSpawnerDef};
    let Ok(text) =
        std::fs::read_to_string(app_path().join("Data/Worlds/main_world/npc_spawns.json"))
    else {
        return;
    };
    let defs = parse_list(&text, NpcSpawnerDef::from_json).expect("npc_spawns.json");
    for (uid, x, y, z) in client_floor_fixes() {
        let found: Vec<f32> = defs
            .iter()
            .filter(|d| d.base.unit_id == uid)
            .filter(|d| {
                (d.base.position.x - x).abs() < 0.01 && (d.base.position.y - y).abs() < 0.01
            })
            .map(|d| d.base.position.z)
            .collect();
        assert_eq!(found.len(), 1, "npc {uid} at ({x}, {y}): {found:?}");
        assert!(
            (found[0] - z).abs() < 0.001,
            "npc {uid} at ({x}, {y}) has Z {}, the client floor is {z}",
            found[0]
        );
    }
}

/// Every [`client_floor_fixes`] spawn lands on a 1.2 collision floor within 1 m of its Z
/// (the live `spawn_ground_z` path, GeoDataMode on), so it neither sinks back onto the terrain
/// under the structure nor floats.
/// `AAEMU_GAME_PAK=C:/AA1.2/game_pak cargo test -p aaemu-game real_client_floor_fixes -- --ignored`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_client_floor_fixes_stand_on_a_collision_floor() {
    let Some(e) = env_with(true) else { return };
    let server = real_server_with(&e, true);
    let world = server.world.world(0).expect("main world");
    for (uid, x, y, z) in client_floor_fixes() {
        let terrain = super::terrain_height(&server, 0, x, y).expect("terrain");
        let spawn = crate::systems::npcs::npc::spawner::spawn_ground_z(&server, &world, x, y, z)
            .expect("a floor under the spawn");
        println!("npc {uid} ({x}, {y}, {z}): spawns at {spawn}, terrain {terrain}");
        assert!(
            (spawn - z).abs() <= 1.0,
            "npc {uid} at ({x}, {y}, {z}) spawns at {spawn}"
        );
        assert!(
            z >= terrain - 0.5,
            "npc {uid} at ({x}, {y}, {z}) is under the terrain {terrain}"
        );
    }
}

/// Every `npc_spawns_quest_fill.json` entry spawns on the ground at its Z: the live NPC spawn
/// snap (`spawn_ground_z`, collision, GeoDataMode on) lands within 0.5 m of it.
/// `AAEMU_GAME_PAK=C:/AA1.2/game_pak cargo test -p aaemu-game real_npc_quest_fill -- --ignored`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_npc_quest_fill_spawns_on_the_ground() {
    use crate::systems::npcs::doodad::spawner::{parse_list, NpcSpawnerDef};
    let Some(e) = env_with(true) else { return };
    let server = real_server_with(&e, true);
    let world = server.world.world(0).expect("main world");
    let text = std::fs::read_to_string(
        app_path().join("Data/Worlds/main_world/npc_spawns_quest_fill.json"),
    )
    .expect("npc_spawns_quest_fill.json");
    let defs = parse_list(&text, NpcSpawnerDef::from_json).expect("parse");
    assert!(!defs.is_empty());
    for d in &defs {
        let (uid, p) = (d.base.unit_id, d.base.position);
        let spawn =
            crate::systems::npcs::npc::spawner::spawn_ground_z(&server, &world, p.x, p.y, p.z)
                .expect("a floor under the fill spawn");
        println!(
            "fill npc {uid} ({}, {}, {}): spawns at {spawn}",
            p.x, p.y, p.z
        );
        assert!(
            (spawn - p.z).abs() <= 0.5,
            "fill npc {uid} at {p:?} spawns at {spawn}"
        );
    }
}

/// The two Bloodhand Base Cannons (slave 57) of `slave_spawns.json` stand on the Marooned Isle
/// rampart floor (the wall walk at Z 126, 10 m above the terrain under it), and every Bloodhand
/// Pillager (8014) on the docks below is within the cannon skill's range (15134: 10-130 m) of
/// one of them. The slave spawner places a slave at its file Z (no ground snap).
/// `AAEMU_GAME_PAK=C:/AA1.2/game_pak cargo test -p aaemu-game real_bloodhand_cannons -- --ignored`
#[tokio::test]
#[ignore = "needs AAEMU_GAME_PAK"]
async fn real_bloodhand_cannons_stand_on_the_rampart_in_range_of_the_docks() {
    use crate::systems::npcs::doodad::spawner::{parse_list, NpcSpawnerDef, SimpleSpawnerDef};
    let Some(e) = env_with(true) else { return };
    let server = real_server_with(&e, true);
    let dir = app_path().join("Data/Worlds/main_world");
    let slaves = std::fs::read_to_string(dir.join("slave_spawns.json")).expect("slave_spawns");
    let cannons: Vec<_> = parse_list(&slaves, SimpleSpawnerDef::from_json)
        .expect("parse slave_spawns")
        .into_iter()
        .filter(|d| d.base.unit_id == 57)
        .map(|d| d.base.position)
        .collect();
    assert_eq!(cannons.len(), 2);
    for p in &cannons {
        let floor = super::ground_height_probe(&server, 0, p.x, p.y, p.z + 1.0, 0.0, 3.0)
            .flatten()
            .expect("a floor under the cannon");
        let terrain = super::terrain_height(&server, 0, p.x, p.y).expect("terrain");
        println!(
            "cannon ({}, {}, {}): floor {floor}, terrain {terrain}",
            p.x, p.y, p.z
        );
        assert!(
            (floor - p.z).abs() <= 0.3,
            "cannon at {} stands on {floor}",
            p.z
        );
        assert!(p.z > terrain + 5.0, "on the wall walk, not the ground");
    }
    let npcs = std::fs::read_to_string(dir.join("npc_spawns.json")).expect("npc_spawns");
    let pillagers: Vec<_> = parse_list(&npcs, NpcSpawnerDef::from_json)
        .expect("parse npc_spawns")
        .into_iter()
        .filter(|d| d.base.unit_id == 8014)
        .map(|d| d.base.position)
        .collect();
    assert!(!pillagers.is_empty());
    for t in &pillagers {
        let nearest = cannons
            .iter()
            .map(|c| ((c.x - t.x).powi(2) + (c.y - t.y).powi(2) + (c.z - t.z).powi(2)).sqrt())
            .fold(f32::MAX, f32::min);
        assert!(
            (10.0..=130.0).contains(&nearest),
            "pillager at ({}, {}) is {nearest} m from the nearest cannon",
            t.x,
            t.y
        );
    }
}
