//! Walking height resolution tests for [`super::reference_height_in`]: cave walkers keep
//! their level under a surface (roof) above them, an actual collision floor is preferred,
//! ordinary outdoor steps stay intact and flying / idle / AI-less NPCs keep the reference
//! height.
//!
//! The synthetic world's heightmap is a flat surface at the test Z; the collision service
//! builds its physics terrain from it, so a downward ground probe from inside a "cave" finds
//! no floor and the terrain fallback (the surface above) must be rejected.

use std::collections::HashMap;
use std::sync::Arc;

use aaemu_physics::geometry::primitives::BoxPrim;
use aaemu_physics::geometry::shapes::stream::{save_phys_geometry, PhysGeomHeader};
use aaemu_physics::geometry::shapes::BoxGeom;
use aaemu_physics::math::{Quat, Vec3 as PhysicsVec3};

use crate::data::client_files::ClientFileManager;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::ai::collision::objects::{apply_doodads, collision_flags, DoodadInfo};
use crate::systems::ai::collision::terrain::tests::Plane;
use crate::systems::npcs::heightmap::{
    HeightmapConfig, HeightmapWorlds, ReferenceHeightAi, WorldHeightmap, CELL_HMAP_RESOLUTION,
};
use crate::systems::skills::controllers::Vec3;
use crate::test_support::{test_server_with, TEST_ZONE};
use crate::world::Entity;

use super::reference_height_in;

/// `HeightMaxCoefficient` of the synthetic `main_world` (as in the heightmap tests).
const COEF: f64 = 65535.0 / 4096.0;
const CELL_POINTS: usize = (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize;

/// A flat heightmap at `surface` metres for the test zone's cell (0, 0).
fn heightmaps(surface: f32) -> Arc<HeightmapWorlds> {
    heightmaps_with_holes(surface, &[])
}

/// [`heightmaps`] with terrain holes at the given grid points of cell (0, 0).
fn heightmaps_with_holes(surface: f32, holes: &[(i32, i32)]) -> Arc<HeightmapWorlds> {
    let t = WorldHeightmap::with_size(0, "main_world", 2, 2, COEF);
    let value = (f64::from(surface) * COEF) as u16;
    assert!(t.insert_cell_with_holes(0, 0, vec![value; CELL_POINTS], holes));
    Arc::new(HeightmapWorlds::from_parts(
        Arc::new(ClientFileManager::new()),
        vec![t],
        HashMap::from([(TEST_ZONE, 0)]),
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    ))
}

/// A test server whose world surface (heightmap and physics terrain) is at `surface`.
fn cave_server(surface: f32) -> GameServer {
    let server = test_server_with(GameData::empty());
    server.systems.npcs.heightmaps.install(heightmaps(surface));
    server
}

/// A test server whose terrain has a hole at grid point (250, 250): the 2 m square
/// `[500, 502) x [500, 502)`.
fn hole_server(surface: f32) -> GameServer {
    let server = test_server_with(GameData::empty());
    server
        .systems
        .npcs
        .heightmaps
        .install(heightmaps_with_holes(surface, &[(250, 250)]));
    server
}

/// A 2 × 2 × 2 m CGF box whose top is at `top_z`, added as a static doodad collider.
fn add_cave_floor(server: &GameServer, top_z: f32) {
    let box_g = BoxGeom::new(&BoxPrim::aligned(
        PhysicsVec3::ZERO,
        PhysicsVec3::splat(1.0),
    ));
    let header = PhysGeomHeader {
        inertia: PhysicsVec3::splat(1.0),
        q: Quat::IDENTITY,
        origin: PhysicsVec3::ZERO,
        volume: 8.0,
        unk52: 1,
        surface_idx: 0,
    };
    let mut blob = Vec::new();
    save_phys_geometry(&header, &box_g, &mut blob);
    let files = HashMap::from([(
        "game/objects/test/cave_floor.cgf".into(),
        crate::systems::ai::collision::cgf::tests::synthetic_cgf(&blob),
    )]);
    assert!(server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(files))
        .is_ok());
    let pw = crate::systems::ai::collision::phys_world(server, 0).expect("physics world");
    let files = crate::systems::ai::collision::files(server).expect("files");
    let models = crate::systems::ai::collision::model_cache(server, &files);
    let floor = DoodadInfo {
        obj_id: 7,
        model: "cgf://objects/test/cave_floor.cgf".into(),
        pos: PhysicsVec3::new(500.0, 500.0, top_z - 1.0),
        q: Quat::IDENTITY,
        scale: 1.0,
        flags: collision_flags(false, false, false),
    };
    let mut shapes = HashMap::new();
    assert_eq!(
        apply_doodads(&pw, &files, &models, &mut shapes, &[floor]),
        (1, 0)
    );
}

/// A walking NPC AI reference (`GetReferenceHeight` inputs).
fn walker(world_z: f32, spawner_z: f32) -> ReferenceHeightAi {
    ReferenceHeightAi {
        can_fly: false,
        hold_or_idle: false,
        spawner_z: Some(spawner_z),
        world_z,
    }
}

fn walker_towards_surface(server: &GameServer, world_z: f32) -> f32 {
    reference_height_in(
        server,
        0,
        Some(walker(world_z, world_z)),
        500.0,
        500.0,
        world_z,
        TEST_ZONE,
    )
}

#[tokio::test]
async fn cave_walker_below_the_surface_keeps_its_level() {
    let server = cave_server(200.0);
    // The probe at the walker's column sees no floor (the heightmap surface is above it),
    // so `ground_height` falls back to the terrain elevation — the resolution the walker
    // must not accept.
    let g = crate::systems::ai::collision::ground_height(&server, 0, 500.0, 500.0, 100.0)
        .expect("service on");
    assert!((g - 200.0).abs() < 0.1, "fixture surface: {g}");
    // Walking keeps the current level (not the surface, not the spawner Z).
    assert_eq!(walker_towards_surface(&server, 100.0), 100.0);
    // A walker that drifted to another cave level keeps that level too.
    assert_eq!(walker_towards_surface(&server, 96.5), 96.5);
}

#[tokio::test]
async fn cave_walker_is_not_lifted_by_a_chase_step_towards_the_surface() {
    let server = cave_server(200.0);
    let npc = Entity::new(0x200, {
        let mut n = Npc::new(1);
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
        n.unit.transform.set_xyz(500.0, 500.0, 100.0);
        n.unit.transform.zone_id = TEST_ZONE;
        n.spawner_z = Some(100.0);
        n
    });
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    super::move_towards(
        &server,
        &npc,
        Some(super::MoveAi {
            hold_or_idle: false,
        }),
        Vec3::new(600.0, 500.0, 200.0),
        2.0,
        4,
        1.0,
    );
    let pos = npc.data().unit.transform;
    assert!((pos.x - 502.0).abs() < 0.001, "{pos:?}");
    assert!(
        (pos.z - 100.0).abs() < 0.001,
        "the walker was lifted off its cave level: {pos:?}"
    );
}

#[tokio::test]
async fn cave_walker_stands_on_the_collision_floor_over_the_surface() {
    let server = cave_server(200.0);
    add_cave_floor(&server, 100.0);
    // The static floor top at 100 m is within the step range and preferred over the
    // heightmap surface (200 m) above the cave.
    let z = walker_towards_surface(&server, 100.6);
    assert!((z - 100.0).abs() < 0.01, "{z}");
}

#[tokio::test]
async fn outdoor_walker_still_follows_the_terrain_within_a_step() {
    let server = cave_server(100.0);
    // Heightmap-only mode: the collision service is off.
    server.config.update(|c| c.physics.enable = false);
    // 1 m below the terrain: a normal uphill step is accepted.
    let z = walker_towards_surface(&server, 99.0);
    assert!((z - 100.0).abs() < 0.1, "{z}");
    // Flat ground: unchanged.
    let z = walker_towards_surface(&server, 100.0);
    assert!((z - 100.0).abs() < 0.1, "{z}");
    // Downhill: a column 3 m below is still a reachable floor.
    let z = walker_towards_surface(&server, 103.0);
    assert!((z - 100.0).abs() < 0.1, "{z}");
}

/// Kroloal Cradle: the NPCs stand at 192-194 m, the terrain under the hall is at 2-5 m and the
/// hall's own floor model is not in the collision world, so the ground probe only finds that
/// terrain 191 m below. It is not the ground under the walker: the first step used to drop it
/// there, out of everyone's reach.
#[tokio::test]
async fn hall_walker_does_not_drop_to_the_terrain_far_below() {
    let server = cave_server(2.0);
    let g = crate::systems::ai::collision::ground_height(&server, 0, 500.0, 500.0, 193.0)
        .expect("service on");
    assert!(
        (g - 2.0).abs() < 0.1,
        "the probe finds the far terrain: {g}"
    );
    // Keeps its level, not the terrain, not the spawner Z.
    let z = reference_height_in(
        &server,
        0,
        Some(walker(193.0, 150.0)),
        500.0,
        500.0,
        193.0,
        TEST_ZONE,
    );
    assert_eq!(z, 193.0);
}

#[tokio::test]
async fn walker_steps_down_a_ledge_but_not_to_a_floor_beyond_the_drop_limit() {
    let server = cave_server(100.0);
    let drop = super::WALKER_MAX_DROP;
    // A floor exactly one drop limit below is reachable ...
    let z = walker_towards_surface(&server, 100.0 + drop - 0.5);
    assert!((z - 100.0).abs() < 0.1, "{z}");
    // ... one beyond it is another floor: the walker keeps its level.
    let z = walker_towards_surface(&server, 100.0 + drop + 0.5);
    assert_eq!(z, 100.0 + drop + 0.5);
}

/// The navigation height (`GeoDataMode`) is the floor of an instance hall: with the physics
/// terrain far below, the walker takes the navmesh height next to it.
#[tokio::test]
async fn hall_walker_takes_the_navmesh_height_when_the_terrain_is_far_below() {
    let server = test_server_with(GameData::empty());
    let t = WorldHeightmap::with_size(0, "main_world", 2, 2, COEF);
    let value = (2.0 * COEF) as u16;
    assert!(t.insert_cell(0, 0, vec![value; CELL_POINTS]));
    let hm = Arc::new(HeightmapWorlds::from_parts(
        Arc::new(ClientFileManager::new()),
        vec![t],
        HashMap::from([(TEST_ZONE, 0)]),
        HeightmapConfig {
            heightmaps_enable: true,
            geo_data_mode: true,
            ..Default::default()
        },
    ));
    hm.install_geo_data(Arc::new(|_template, _x, _y, _z| 192.4));
    server.systems.npcs.heightmaps.install(hm);
    let z = reference_height_in(
        &server,
        0,
        Some(walker(193.0, 193.0)),
        500.0,
        500.0,
        193.0,
        TEST_ZONE,
    );
    assert_eq!(z, 192.4, "the navmesh floor next to the walker");
}

#[tokio::test]
async fn walking_without_a_service_or_heightmap_keeps_the_level() {
    // No heightmaps installed and no collision service: the heightmap fallback is unknown
    // (0) and must not restore the spawner Z.
    let server = test_server_with(GameData::empty());
    let z = reference_height_in(
        &server,
        0,
        Some(walker(100.0, 150.0)),
        500.0,
        500.0,
        100.0,
        TEST_ZONE,
    );
    assert_eq!(z, 100.0, "the spawner Z must not be restored");
}

#[tokio::test]
async fn walker_accepts_a_collision_floor_at_zero() {
    let server = test_server_with(GameData::empty());
    // Force the collision service on without heightmaps: an empty file map plus a terrain
    // plane at sea level.
    assert!(server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(HashMap::new()))
        .is_ok());
    assert!(server
        .systems
        .ai
        .collision
        .test_terrain
        .set(Arc::new(Plane {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        }))
        .is_ok());
    let z = reference_height_in(
        &server,
        0,
        Some(walker(1.0, 1.0)),
        500.0,
        500.0,
        1.0,
        TEST_ZONE,
    );
    assert_eq!(z, 0.0, "a floor at Z = 0 is a collision floor");
}

#[tokio::test]
async fn missed_probe_with_zero_terrain_keeps_the_level() {
    let server = test_server_with(GameData::empty());
    // Sea-level terrain more than `GROUND_PROBE_DOWN` below the walker: the step-up probe
    // cannot reach it, and its zero terrain fallback is "unknown", not a floor.
    assert!(server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(HashMap::new()))
        .is_ok());
    assert!(server
        .systems
        .ai
        .collision
        .test_terrain
        .set(Arc::new(Plane {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        }))
        .is_ok());
    let probe = crate::systems::ai::collision::ground_height_probe(
        &server,
        0,
        500.0,
        500.0,
        2100.0,
        crate::systems::ai::collision::GROUND_PROBE_UP,
        crate::systems::ai::collision::GROUND_PROBE_DOWN,
    );
    assert_eq!(probe, Some(None), "the probe must miss");
    let z = reference_height_in(
        &server,
        0,
        Some(walker(2100.0, 2100.0)),
        500.0,
        500.0,
        2100.0,
        TEST_ZONE,
    );
    assert_eq!(z, 2100.0, "an unknown zero fallback must keep the level");
}

#[tokio::test]
async fn flying_idle_and_ai_less_npcs_keep_the_reference_height() {
    let server = cave_server(200.0);
    let fly = ReferenceHeightAi {
        can_fly: true,
        ..walker(100.0, 300.0)
    };
    assert_eq!(
        reference_height_in(&server, 0, Some(fly), 500.0, 500.0, 100.0, TEST_ZONE),
        300.0
    );
    let idle = ReferenceHeightAi {
        hold_or_idle: true,
        ..walker(100.0, 300.0)
    };
    assert_eq!(
        reference_height_in(&server, 0, Some(idle), 500.0, 500.0, 100.0, TEST_ZONE),
        300.0
    );
    // No AI: the terrain / navigation height, unclamped (the C# default path).
    let z = reference_height_in(&server, 0, None, 500.0, 500.0, 100.0, TEST_ZONE);
    assert!((z - 200.0).abs() < 0.1, "{z}");
}

// --- spawn_ground_z (`npc_spawn_on_terrain`) ---

fn spawn_z_at(server: &GameServer, x: f32, y: f32, z: f32) -> f32 {
    let world = server.world.world(0).expect("main_world");
    crate::systems::npcs::npc::spawner::spawn_ground_z(server, &world, x, y, z).unwrap_or(z)
}

fn spawn_z(server: &GameServer, z: f32) -> f32 {
    spawn_z_at(server, 500.0, 500.0, z)
}

#[tokio::test]
async fn spawn_snaps_down_from_2m_above_the_ground() {
    let server = cave_server(100.0);
    let z = spawn_z(&server, 102.0);
    assert!((z - 100.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn spawn_8m_above_the_ground_stays() {
    let server = cave_server(100.0);
    assert_eq!(spawn_z(&server, 108.0), 108.0);
}

#[tokio::test]
async fn spawn_half_a_metre_above_the_ground_snaps() {
    let server = cave_server(100.0);
    let z = spawn_z(&server, 100.5);
    assert!((z - 100.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn spawn_in_a_cave_with_no_floor_stays() {
    let server = cave_server(200.0);
    // Far under solid terrain: probably a cave whose floor we lack.
    assert_eq!(spawn_z(&server, 100.0), 100.0);
    assert_eq!(spawn_z(&server, 190.0), 190.0);
}

#[tokio::test]
async fn spawn_sunk_a_few_metres_into_the_terrain_is_lifted_onto_it() {
    let server = cave_server(200.0);
    let z = spawn_z(&server, 197.0);
    assert!((z - 200.0).abs() < 0.1, "{z}");
    // Right under the sink limit.
    let z = spawn_z(
        &server,
        200.0 - crate::systems::npcs::npc::spawner::SPAWN_SINK_SNAP + 0.5,
    );
    assert!((z - 200.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn spawn_under_a_terrain_hole_stays() {
    let server = hole_server(200.0);
    assert_eq!(spawn_z_at(&server, 500.5, 500.5, 197.0), 197.0);
    // Just outside the hole the same sunk spawn is lifted.
    let z = spawn_z_at(&server, 504.5, 500.5, 197.0);
    assert!((z - 200.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn swimmer_above_a_seabed_under_the_water_surface_stays() {
    // Ocean level 100 m: the seabed at 96 m is 4 m under the surface.
    let server = cave_server(96.0);
    assert_eq!(spawn_z(&server, 99.2), 99.2);
    // Within the C# 1 m it still snaps to the floor.
    let z = spawn_z(&server, 96.5);
    assert!((z - 96.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn spawn_3m_above_a_box_snaps_onto_the_box_not_the_terrain() {
    let server = cave_server(100.0);
    add_cave_floor(&server, 103.0);
    // Terrain is 6 m below the spawn (out of range); the box top 3 m below it is the floor.
    let z = spawn_z(&server, 106.0);
    assert!((z - 103.0).abs() < 0.01, "{z}");
}

#[tokio::test]
async fn spawn_without_the_collision_service_keeps_the_1m_rule() {
    let server = cave_server(100.0);
    server.config.update(|c| c.physics.enable = false);
    let z = spawn_z(&server, 100.5);
    assert!((z - 100.0).abs() < 0.1, "{z}");
    assert_eq!(spawn_z(&server, 102.0), 102.0);
}

// --- terrain holes ---

fn static_ray_z(server: &GameServer, x: f32, y: f32) -> Option<f32> {
    use crate::systems::ai::collision::query::{mask, ray_flags};
    crate::systems::ai::collision::raycast(
        server,
        0,
        [x, y, 110.0],
        [x, y, 0.0],
        mask::STATIC_TERRAIN,
        ray_flags::DEFAULT,
    )
    .expect("service on")
    .map(|h| h.pos.z)
}

#[tokio::test]
async fn terrain_hole_lets_rays_through_to_the_box_below() {
    let server = hole_server(100.0);
    add_cave_floor(&server, 60.0);
    // The query helper sees the hole (the box is 2 m wide around (500, 500)).
    use crate::systems::ai::collision::terrain_is_hole;
    assert_eq!(terrain_is_hole(&server, 0, 500.5, 500.5), Some(true));
    assert_eq!(terrain_is_hole(&server, 0, 501.9, 500.0), Some(true));
    assert_eq!(terrain_is_hole(&server, 0, 499.9, 500.5), Some(false));
    assert_eq!(terrain_is_hole(&server, 0, 510.0, 500.0), Some(false));
    // Through the hole the ray misses the terrain and lands on the box top.
    let z = static_ray_z(&server, 500.5, 500.5).expect("hit");
    assert!((z - 60.0).abs() < 0.01, "{z}");
    // Next to the hole the terrain is still there.
    let z = static_ray_z(&server, 504.5, 500.5).expect("hit");
    assert!((z - 100.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn terrain_without_the_hole_stops_the_ray_at_the_surface() {
    let server = cave_server(100.0);
    add_cave_floor(&server, 60.0);
    let z = static_ray_z(&server, 500.5, 500.5).expect("hit");
    assert!((z - 100.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn spawn_above_the_saturated_heightmap_ceiling_keeps_its_height() {
    // 4100 m does not fit the u16 heightmap (4096 m ceiling): the "terrain" reads 4095.98 and
    // is only a lower bound, so a spawn on the real surface must not be pulled down onto it.
    let server = cave_server(4100.0);
    assert_eq!(spawn_z(&server, 4097.0), 4097.0);
    assert_eq!(spawn_z(&server, 4100.5), 4100.5);
}

#[tokio::test]
async fn spawn_on_a_mountain_top_above_1024m_snaps_onto_the_terrain() {
    // The heightmap used to saturate at 1024 m; a 1500 m summit is a real floor now.
    let server = cave_server(1500.0);
    let z = spawn_z(&server, 1501.5);
    assert!((z - 1500.0).abs() < 0.1, "{z}");
}

#[tokio::test]
async fn spawn_inside_a_building_over_a_missing_floor_keeps_its_height() {
    use crate::systems::ai::collision::world::CellContent;
    // The hall's floor has no collision: the probe under the spawn only finds the terrain 4 m
    // lower. The cell's brush bounds (the building shell) show the spawn is indoors.
    let server = cave_server(100.0);
    let pw = crate::systems::ai::collision::phys_world(&server, 0).expect("physics world");
    assert!(pw.begin_load((0, 0)));
    pw.end_load(
        Some(CellContent {
            cell: (0, 0),
            brush_bounds: vec![[
                PhysicsVec3::new(490.0, 490.0, 95.0),
                PhysicsVec3::new(530.0, 530.0, 120.0),
            ]],
            ..CellContent::default()
        }),
        (0, 0),
    );
    assert_eq!(spawn_z_at(&server, 500.0, 500.0, 104.0), 104.0);
    // Outside the bounds the same gap snaps onto the terrain, and within 1 m even inside.
    let z = spawn_z_at(&server, 600.0, 500.0, 104.0);
    assert!((z - 100.0).abs() < 0.1, "{z}");
    let z = spawn_z_at(&server, 500.0, 500.0, 100.5);
    assert!((z - 100.0).abs() < 0.1, "{z}");
}

// --- displaced / blinked / teleported NPCs (knockback, impulse, explosion, Blink, TeleportToUnit) ---

mod displaced {
    use super::*;
    use crate::systems::skills::data::effects::{ImpulseEffect, PhysicalExplosionEffect};
    use crate::systems::skills::effects::EffectSource;
    use crate::systems::skills::effects::{special, unit_fx, EffectArgs};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::{NpcRef, WorldObject};

    fn spawn_at(server: &GameServer, obj_id: u32, x: f32, z: f32) -> NpcRef {
        let npc = Entity::new(obj_id, {
            let mut n = Npc::new(1);
            n.unit.hp = 1000;
            n.unit.max_hp = 1000;
            n.unit.transform.set_xyz(x, 500.0, z);
            n.unit.transform.zone_id = TEST_ZONE;
            n.spawner_z = Some(z);
            n
        });
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());
        npc
    }

    fn args(caster: &NpcRef, target: &NpcRef) -> EffectArgs<'static> {
        let (c, t) = (WorldObject::from(caster), WorldObject::from(target));
        EffectArgs {
            caster: Some(c.clone()),
            caster_caster: SkillCaster::Unit(c.obj_id()),
            target: Target::Obj(t.clone()),
            target_caster: SkillCastTarget::Unit(t.obj_id()),
            cast: CastAction::Skill {
                skill_id: 1,
                tl_id: 1,
            },
            source: EffectSource::default(),
            skill_object: &SkillObject::None,
        }
    }

    fn z_of(npc: &NpcRef) -> f32 {
        npc.data().unit.transform.z
    }

    /// The hall: its floor (193 m) is not in the collision world, the terrain is at 2 m.
    fn hall() -> (GameServer, NpcRef, NpcRef) {
        let server = cave_server(2.0);
        let caster = spawn_at(&server, 0x300, 490.0, 193.0);
        let target = spawn_at(&server, 0x301, 500.0, 193.0);
        (server, caster, target)
    }

    #[tokio::test]
    async fn hall_knockback_keeps_the_npc_at_its_level() {
        let (server, caster, target) = hall();
        special::execute(&server, 13, &args(&caster, &target), [5000, 0, 0, 0]);
        let p = target.data().unit.transform;
        assert!(p.x > 504.0, "knocked away: {p:?}");
        assert!((p.z - 193.0).abs() <= 5.0, "dropped to the terrain: {p:?}");
    }

    #[tokio::test]
    async fn hall_impulse_keeps_the_npc_at_its_level() {
        let (server, caster, target) = hall();
        let e = ImpulseEffect {
            id: 1,
            vel_impulse: [0.0; 3],
            angvel_impulse: [0.0; 3],
            impulse: [0.0, 5000.0, 0.0],
            ang_impulse: [0.0; 3],
        };
        unit_fx::impulse(&server, &e, &args(&caster, &target));
        let p = target.data().unit.transform;
        assert!((p.x - 500.0).abs() > 0.5, "pushed: {p:?}");
        assert!((p.z - 193.0).abs() <= 5.0, "dropped to the terrain: {p:?}");
    }

    #[tokio::test]
    async fn hall_physical_explosion_keeps_the_npc_at_its_level() {
        let (server, caster, target) = hall();
        let e = PhysicalExplosionEffect {
            id: 1,
            radius: 20.0,
            hole_size: 0.5,
            pressure: 5000.0,
        };
        unit_fx::physical_explosion(&server, &e, &args(&caster, &target));
        let p = target.data().unit.transform;
        assert!(p.x > 502.0, "blown away: {p:?}");
        assert!((p.z - 193.0).abs() <= 5.0, "dropped to the terrain: {p:?}");
    }

    #[tokio::test]
    async fn knockback_still_lands_on_a_real_floor_a_few_metres_lower() {
        // A real floor (collision terrain) 2 m below the NPC is reachable.
        let server = cave_server(191.0);
        let caster = spawn_at(&server, 0x300, 490.0, 193.0);
        let target = spawn_at(&server, 0x301, 500.0, 193.0);
        special::execute(&server, 13, &args(&caster, &target), [5000, 0, 0, 0]);
        let p = target.data().unit.transform;
        assert!(p.x > 504.0, "{p:?}");
        assert!(
            (p.z - 191.0).abs() < 0.1,
            "did not land on the floor: {p:?}"
        );
    }

    #[tokio::test]
    async fn knockback_ignores_a_floor_beyond_the_drop_limit() {
        let server = cave_server(193.0 - super::super::WALKER_MAX_DROP - 1.0);
        let caster = spawn_at(&server, 0x300, 490.0, 193.0);
        let target = spawn_at(&server, 0x301, 500.0, 193.0);
        special::execute(&server, 13, &args(&caster, &target), [5000, 0, 0, 0]);
        assert!((z_of(&target) - 193.0).abs() < 0.1, "{}", z_of(&target));
    }

    #[tokio::test]
    async fn hall_npc_blink_keeps_its_level() {
        let (server, caster, target) = hall();
        special::execute(&server, 11, &args(&caster, &target), [3, 0, 0, 0]);
        assert!((z_of(&caster) - 193.0).abs() <= 5.0, "{}", z_of(&caster));
    }

    #[tokio::test]
    async fn hall_npc_teleport_to_unit_lands_at_the_target_level() {
        let (server, caster, target) = hall();
        special::execute(&server, 47, &args(&caster, &target), [3000, 3000, 0, 0]);
        let p = caster.data().unit.transform;
        assert!(p.x > 495.0, "teleported next to the target: {p:?}");
        assert!((p.z - 193.0).abs() <= 1.0, "dropped to the terrain: {p:?}");
    }
}
