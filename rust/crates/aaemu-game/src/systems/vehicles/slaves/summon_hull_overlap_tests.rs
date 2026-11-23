use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aaemu_physics::geometry::primitives::{BoxPrim, Primitive};
use aaemu_physics::math::{Quat, Vec3};
use aaemu_physics::world::World;

use crate::data::client_files::ClientFileManager;
use crate::data::items::{ItemTemplate, TemplateKind};
use crate::models::container::slot_type;
use crate::models::doodad::Doodad;
use crate::models::item::Item;
use crate::systems::ai::collision::models::{ModelPart, ModelPhys};
use crate::systems::ai::collision::terrain::tests::Plane;
use crate::systems::ai::collision::world::{CellContent, StaticInstance};
use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};
use crate::systems::skills::data::SkillTemplate;
use crate::systems::vehicles::data::{slave_kind, SlaveTemplate};
use crate::test_support::{spawn_player, test_character, TestClient};

use super::{
    create_for_owner, get_active_slave_by_owner_obj_id, hull_clear_at, item_task_type,
    model_box_clear_at, overhead_clear_at, summon_candidate_1_2, summon_candidate_points,
    summon_model_box, summon_offset_z, summon_placement_clear_1_2, summon_terrain,
    validated_summon_placement, HullEnvelope, HullFootprint, ShipModel,
};
use crate::systems::skills::ext::SummonPlacement;
use aaemu_commons::PacketStream;

/// Waterline and seabed of the synthetic world; the seabed is deep enough for the Cutter
/// (min depth 19.5 m), so only the hull-envelope overlap can reject these spots.
const WATER: f32 = 100.0;
const GROUND: f32 = 60.0;

/// Cutter-sized envelope: keel 2 m below the waterline, hull top 2 m above it, plus the
/// 1 m overhead margin.
fn envelope() -> HullEnvelope {
    HullEnvelope {
        footprint: HullFootprint {
            half_beam: 5.0,
            length: (-15.0, 15.0),
        },
        bottom: -2.0,
        top: 3.0,
    }
}

fn static_box(half: Vec3, center: Vec3, vegetation: bool) -> StaticInstance {
    let w = World::new();
    let g = w
        .register_primitive(&Primitive::Box(BoxPrim::aligned(Vec3::ZERO, half)))
        .expect("box geometry");
    StaticInstance {
        model: Arc::new(ModelPhys {
            path: "test/box.cgf".into(),
            parts: vec![ModelPart {
                slot: 0,
                geom: g,
                frame: None,
            }],
            blob_bytes: 0,
        }),
        pos: center,
        q: Quat::IDENTITY,
        scale: 1.0,
        flags_default: if vegetation { 0x0401_8FFF } else { 0x18FFF },
        density: 0.0,
        ent_flags_or: 0x8000,
        vegetation,
    }
}

/// A test server whose instance-0 collision world has the terrain plane and the given
/// static boxes in cell (0, 0).
fn world_at(ground: f32, statics: &[(Vec3, Vec3, bool)]) -> Arc<crate::GameServer> {
    world_at_with(crate::data::GameData::empty(), ground, statics)
}

fn world_at_with(
    data: crate::data::GameData,
    ground: f32,
    statics: &[(Vec3, Vec3, bool)],
) -> Arc<crate::GameServer> {
    world_on(
        data,
        Plane {
            a: ground,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        statics,
    )
}

fn world_on(
    data: crate::data::GameData,
    plane: Plane,
    statics: &[(Vec3, Vec3, bool)],
) -> Arc<crate::GameServer> {
    let s = Arc::new(crate::test_support::test_server_with(data));
    s.set_self();
    s.systems
        .ai
        .collision
        .test_files
        .set(Arc::new(HashMap::new()))
        .unwrap();
    s.systems
        .ai
        .collision
        .test_terrain
        .set(Arc::new(plane))
        .unwrap_or_else(|_| panic!("terrain"));
    let pw = crate::systems::ai::collision::phys_world(&s, 0).expect("physics world");
    let mut content = CellContent {
        cell: (0, 0),
        ..Default::default()
    };
    for (center, half, vegetation) in statics {
        content
            .instances
            .push(static_box(*half, *center, *vegetation));
    }
    // PWI's entity-box lookup returns nothing until the entity pools exist (the first
    // static creates them); keep a tiny far-away static so an empty test cell still has
    // them. It is far outside every query below.
    content.instances.push(static_box(
        Vec3::splat(0.1),
        Vec3::new(1000.0, 1000.0, 1000.0),
        false,
    ));
    assert!(pw.lock().insert_cell(content), "cell (0, 0)");
    s
}

fn world(statics: &[(Vec3, Vec3)]) -> Arc<crate::GameServer> {
    let statics: Vec<_> = statics
        .iter()
        .map(|(center, half)| (*center, *half, false))
        .collect();
    world_at(GROUND, &statics)
}

fn clear(s: &crate::GameServer) -> bool {
    hull_clear_at(s, 0, 500.0, 500.0, WATER, 0.0, envelope())
}

const RULE_MODEL_ID: u32 = 1000;
const RULE_SLAVE: u32 = 21;
const RULE_ITEM: u32 = 52_230;
const RULE_SKILL: u32 = 58_002;

/// A Cutter-shaped template and its ship model, for the 1.2 summon rule tests.
fn ship_template() -> SlaveTemplate {
    SlaveTemplate {
        id: RULE_SLAVE,
        name: "Cutter".into(),
        model_id: RULE_MODEL_ID,
        spawn_x_offset: 30.0,
        spawn_y_offset: 20.0,
        slave_kind: slave_kind::BOAT,
        ..Default::default()
    }
}

fn with_ship_data(mut data: crate::data::GameData) -> crate::data::GameData {
    let ship = ShipModel {
        id: 6,
        mass: 45_000.0,
        mass_center_z: -4.0,
        mass_box_size_x: 6.5,
        mass_box_size_y: 30.0,
        mass_box_size_z: 14.5,
        keel_height: 0.2,
        ..Default::default()
    };
    data.systems
        .vehicles
        .model_types
        .insert(RULE_MODEL_ID, (6, "ShipModel".into()));
    data.systems.vehicles.ship_models.insert(6, ship);
    // The AI model (keyed by model id) is what the 1.2 model box falls back to when the
    // client model reference is unavailable.
    data.systems.ai.ai.ship_models.insert(
        RULE_MODEL_ID,
        crate::systems::ai::data::ShipModel {
            id: 6,
            mass: 45_000.0,
            mass_center_z: -4.0,
            mass_box_size_x: 6.5,
            mass_box_size_y: 30.0,
            mass_box_size_z: 14.5,
            keel_height: 0.2,
            ..Default::default()
        },
    );
    data.systems
        .vehicles
        .slave_templates
        .insert(RULE_SLAVE, ship_template());
    data
}

fn ship_server_at(ground: f32, statics: &[(Vec3, Vec3)]) -> Arc<crate::GameServer> {
    let statics: Vec<_> = statics
        .iter()
        .map(|(center, half)| (*center, *half, false))
        .collect();
    world_at_with(
        with_ship_data(crate::data::GameData::empty()),
        ground,
        &statics,
    )
}

fn search_player(s: &crate::GameServer) -> (TestClient, crate::world::PlayerRef) {
    let mut client = TestClient::new(s, 1, 1);
    // Above the object id manager's 0x100 start (the tests do not prime it with the
    // player's id, and a spawned slave must not collide with it).
    let p = spawn_player(
        s,
        &client,
        test_character(1, "P", 500.0, 500.0),
        0x00F0_0001,
    );
    // The fixture has no character stat pools; the cast validation reads hp and the 1.2
    // summon use skill is `source_alive`.
    p.data().unit.max_hp = 1000;
    p.data().unit.hp = 1000;
    client.conn.active_char = Some(p.clone());
    (client, p)
}

/// A thick deck slab whose top is above the overhead ray start (water + hull top + 1 m =
/// 103). The upward ray finds its underside, the strict hull envelope overlaps it and the
/// player 1.2 model box does too.
#[tokio::test]
async fn a_thick_deck_the_downward_ray_starts_inside_is_rejected() {
    let s = world(&[(Vec3::new(500.0, 500.0, 102.75), Vec3::new(20.0, 20.0, 1.25))]);
    assert!(
        !overhead_clear_at(&s, 0, 500.0, 500.0, WATER, 2.0),
        "the upward ray finds the deck underside"
    );
    assert!(!clear(&s), "the hull envelope overlaps the deck slab");
    let model = BoxPrim::aligned(Vec3::new(0.0, 0.0, 1.0), Vec3::new(5.0, 15.0, 2.0));
    assert!(
        !model_box_clear_at(&s, 0, 500.0, 500.0, WATER, 0.0, &model, 0.0),
        "the 1.2 model box overlaps the deck slab"
    );
}

/// A stone quay from the seabed to above the water that fully contains the hull box.
#[tokio::test]
async fn a_solid_quay_containing_the_hull_centre_is_rejected() {
    let s = world(&[(Vec3::new(500.0, 500.0, 97.5), Vec3::new(30.0, 30.0, 7.5))]);
    assert!(!clear(&s));
}

#[tokio::test]
async fn open_water_is_accepted() {
    let s = world(&[]);
    assert!(clear(&s));
}

/// A bridge whose underside is above the hull top + 1 m margin clearance.
#[tokio::test]
async fn a_bridge_above_the_margin_is_accepted() {
    let s = world(&[(Vec3::new(500.0, 500.0, 105.0), Vec3::new(20.0, 20.0, 1.0))]);
    assert!(clear(&s));
}

/// A harbour platform that is terrain (heightmap) above the water: the box includes the
/// heightfield, so the spot is rejected even though the depth ray starts below its top.
#[tokio::test]
async fn a_terrain_platform_above_the_water_is_rejected() {
    let s = world_at(101.0, &[]);
    assert!(!clear(&s));
}

/// A mooring post crossing the hull box edge is below [`HULL_OBSTACLE_MIN_SIZE`] and must
/// not reject the spot (the point-containment rays ignore it too).
#[tokio::test]
async fn a_small_post_at_the_envelope_edge_is_ignored() {
    let s = world(&[(Vec3::new(505.0, 500.0, 100.0), Vec3::new(0.3, 0.3, 0.6))]);
    let found = crate::systems::ai::collision::box_obstacles(
        &s,
        0,
        [500.0, 500.0, 100.5],
        [5.0, 15.0, 2.5],
        0.0,
        crate::systems::ai::collision::query::mask::STATIC_TERRAIN,
    )
    .unwrap();
    assert!(!found.is_empty(), "the post intersects the envelope box");
    assert!(clear(&s));
}

/// The batched query keeps the client's 1.2 hull box exact in every round: a pier half a
/// millimetre outside the envelope is not a contact. (The earlier batching grew its first
/// box by a millimetre to defeat the identical-box cache and would report this pier.)
#[tokio::test]
async fn a_pier_half_a_millimetre_outside_the_hull_is_clear() {
    let s = world(&[(Vec3::new(507.0005, 500.0, 100.0), Vec3::new(2.0, 2.0, 2.0))]);
    let found = crate::systems::ai::collision::box_obstacles(
        &s,
        0,
        [500.0, 500.0, 100.5],
        [5.0, 15.0, 2.5],
        0.0,
        crate::systems::ai::collision::query::mask::STATIC_TERRAIN,
    )
    .unwrap();
    assert!(
        found.is_empty(),
        "the exact envelope stops short of the pier: {found:?}"
    );
    assert!(clear(&s));
}

/// Vegetation is decorative for hull placement, whatever its size.
#[tokio::test]
async fn vegetation_inside_the_envelope_is_ignored() {
    let s = world_at(
        GROUND,
        &[(
            Vec3::new(505.0, 500.0, 100.0),
            Vec3::new(3.0, 3.0, 3.0),
            true,
        )],
    );
    let found = crate::systems::ai::collision::box_obstacles(
        &s,
        0,
        [500.0, 500.0, 100.5],
        [5.0, 15.0, 2.5],
        0.0,
        crate::systems::ai::collision::query::mask::STATIC_TERRAIN,
    )
    .unwrap();
    assert!(
        found.iter().any(|(owner, _)| matches!(
            owner,
            crate::systems::ai::collision::world::Owner::Vegetation { .. }
        )),
        "the vegetation entity intersects the envelope box"
    );
    assert!(clear(&s));
}

/// The physics contact pool holds 64 records and drops further parts silently
/// (`prim_intersect.rs` gate; `pwi.rs:402-404`). Vegetation is filtered only after the
/// query, so a wall of 64 vegetation parts can fill the pool and hide a pier that is tested
/// later. `box_obstacles` batches with a skip list of the entities already seen, so the
/// pier behind the vegetation is still found and the spot is refused.
#[tokio::test]
async fn a_pier_behind_vegetation_filling_the_contact_pool_is_found() {
    let mut statics = vec![(
        Vec3::new(503.0, 507.0, 100.0),
        Vec3::new(2.5, 2.5, 2.5),
        false,
    )];
    for i in 0..64 {
        statics.push((
            Vec3::new(
                496.0 + (i % 8) as f32 * 1.1,
                491.0 + (i / 8) as f32 * 2.5,
                103.0,
            ),
            Vec3::splat(0.4),
            true,
        ));
    }
    let s = world_at(GROUND, &statics);
    assert!(
        !clear(&s),
        "the pier overlaps the hull envelope behind the vegetation"
    );
}

/// Dense vegetation alone fills the contact pool too; the batched query collects the
/// remaining parts and, with every owner vegetation, the spot stays accepted (batching, not
/// the fail-safe refusal).
#[tokio::test]
async fn dense_vegetation_filling_the_contact_pool_is_still_accepted() {
    let mut statics = Vec::new();
    for i in 0..80 {
        statics.push((
            Vec3::new(
                496.0 + (i % 8) as f32 * 1.1,
                487.0 + (i / 8) as f32 * 2.5,
                103.0,
            ),
            Vec3::splat(0.4),
            true,
        ));
    }
    let s = world_at(GROUND, &statics);
    assert!(clear(&s), "vegetation is never a hull obstacle");
}

/// Yaw 90: the off-centre footprint must put the box mid 5 m along -X with the length
/// along X and the beam along Y. A pier near the stern only overlaps the correctly placed
/// box; a pier beyond the bow must not overlap (it would if the mid offset were negated).
#[tokio::test]
async fn a_yaw_90_off_centre_pier_is_rejected_with_the_ship_length_axis() {
    let envelope = HullEnvelope {
        footprint: HullFootprint {
            half_beam: 3.0,
            length: (-5.0, 15.0),
        },
        bottom: -2.0,
        top: 3.0,
    };
    let yaw = std::f32::consts::FRAC_PI_2;
    let stern_pier = world(&[(Vec3::new(488.0, 502.0, 100.0), Vec3::new(1.0, 2.0, 2.0))]);
    assert!(
        !hull_clear_at(&stern_pier, 0, 500.0, 500.0, WATER, yaw, envelope),
        "the stern-side pier is inside the yaw-90 envelope"
    );
    let bow_pier = world(&[(Vec3::new(512.0, 502.0, 100.0), Vec3::new(1.0, 2.0, 2.0))]);
    assert!(
        hull_clear_at(&bow_pier, 0, 500.0, 500.0, WATER, yaw, envelope),
        "the pier beyond the bow is outside the yaw-90 envelope"
    );
}

/// The 1.2 player box is the model bbox: no +1 m margin and the model top (masts), not the
/// clipped hull top. A bridge 1.5 m above the old hull box is inside the model box.
#[tokio::test]
async fn the_model_box_has_no_overhead_margin() {
    let s = world(&[(Vec3::new(500.0, 500.0, 104.5), Vec3::new(20.0, 20.0, 0.5))]);
    let model = BoxPrim::aligned(Vec3::new(0.0, 0.0, 3.0), Vec3::new(5.0, 15.0, 2.0));
    assert!(
        !model_box_clear_at(&s, 0, 500.0, 500.0, WATER, 0.0, &model, 0.0),
        "the bridge is inside the model bbox top"
    );
    assert!(
        hull_clear_at(&s, 0, 500.0, 500.0, WATER, 0.0, envelope()),
        "the old hull box (+1 m margin) ends below the bridge"
    );
}

/// The model box includes the terrain (mask 0x10f in 1.2): a seabed below the water but
/// inside the model box rejects the spot even without the old point sampling.
#[tokio::test]
async fn the_model_box_terrain_part_rejects_a_shallow_seabed() {
    let s = world_at(95.0, &[]);
    let into_seabed = BoxPrim::aligned(Vec3::new(0.0, 0.0, -2.0), Vec3::new(5.0, 15.0, 5.0));
    assert!(!model_box_clear_at(
        &s,
        0,
        500.0,
        500.0,
        WATER,
        0.0,
        &into_seabed,
        0.0
    ));
    let above_seabed = BoxPrim::aligned(Vec3::new(0.0, 0.0, 10.0), Vec3::new(5.0, 15.0, 2.0));
    assert!(model_box_clear_at(
        &s,
        0,
        500.0,
        500.0,
        WATER,
        0.0,
        &above_seabed,
        0.0
    ));
}

// ---- the 1.2 client summon rule -----------------------------------------------------

#[test]
fn the_1_2_candidates_are_40_forward_steps_with_three_lateral_offsets() {
    let points = summon_candidate_points(0.0, 0.0, 0.0, 30.0, 20.0);
    assert_eq!(points.len(), 120);
    assert_eq!(
        &points[..6],
        &[
            (0.0, 30.0),
            (20.0, 30.0),
            (-20.0, 30.0),
            (0.0, 31.0),
            (20.0, 31.0),
            (-20.0, 31.0),
        ]
    );
}

#[tokio::test]
async fn the_1_2_search_takes_the_forward_offset_candidate() {
    let s = ship_server_at(GROUND, &[]);
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    let t = summon_candidate_1_2(&s, &world, &p, &ship_template()).expect("candidate");
    assert!(
        (t.x - 500.0).abs() < 1e-4 && (t.y - 530.0).abs() < 1e-4,
        "{t:?}"
    );
    assert!(
        (t.z - (WATER
            + summon_offset_z(&ShipModel {
                mass_center_z: -4.0,
                keel_height: 0.2,
                ..Default::default()
            })))
        .abs()
            < 1e-4
    );
    assert!((t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
}

#[tokio::test]
async fn the_1_2_search_rejects_land_above_the_water() {
    let s = ship_server_at(101.0, &[]);
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    assert!(summon_candidate_1_2(&s, &world, &p, &ship_template()).is_none());
}

#[tokio::test]
async fn the_1_2_search_rejects_a_blocked_line_of_sight() {
    let s = ship_server_at(
        GROUND,
        &[(Vec3::new(500.0, 510.0, 100.0), Vec3::new(100.0, 5.0, 10.0))],
    );
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    assert!(summon_candidate_1_2(&s, &world, &p, &ship_template()).is_none());
}

#[tokio::test]
async fn the_1_2_search_skips_a_blocked_box_and_takes_the_lateral_candidate() {
    let s = ship_server_at(
        GROUND,
        &[(Vec3::new(500.0, 530.0, 100.0), Vec3::new(1.5, 1.5, 5.0))],
    );
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    let t = summon_candidate_1_2(&s, &world, &p, &ship_template()).expect("candidate");
    assert!(
        (t.x - 520.0).abs() < 1e-4 && (t.y - 530.0).abs() < 1e-4,
        "{t:?}"
    );
}

#[tokio::test]
async fn the_1_2_search_caps_the_candidate_distance_at_80_m() {
    let s = ship_server_at(GROUND, &[]);
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    let mut template = ship_template();
    template.spawn_x_offset = 81.0;
    template.spawn_y_offset = 0.0;
    assert!(summon_candidate_1_2(&s, &world, &p, &template).is_none());
}

/// A cart-shaped land template (no ship model, so the 1.2 box check is skipped).
fn land_template() -> SlaveTemplate {
    SlaveTemplate {
        id: RULE_SLAVE,
        name: "Farm Wagon".into(),
        model_id: 0,
        spawn_x_offset: 30.0,
        spawn_y_offset: 0.0,
        slave_kind: slave_kind::MACHINE,
        ..Default::default()
    }
}

fn land_data() -> crate::data::GameData {
    let mut data = search_data();
    data.systems
        .vehicles
        .slave_templates
        .insert(RULE_SLAVE, land_template());
    data
}

/// A test server on the plane `z = a + c·y` (rising along the yaw-0 facing).
fn slope_server(a: f32, c: f32, statics: &[(Vec3, Vec3)]) -> Arc<crate::GameServer> {
    let statics: Vec<_> = statics
        .iter()
        .map(|(center, half)| (*center, *half, false))
        .collect();
    world_on(
        crate::data::GameData::empty(),
        Plane {
            a,
            b: 0.0,
            c,
            n: 1024,
        },
        &statics,
    )
}

/// The player stands on the slope `z = 100 + 0.2·y` (200 at y = 500); the first candidate
/// is 1 m uphill, inside the 10 m step. The old ray ended at the player's foot height,
/// under that uphill ground, and refused it.
#[tokio::test]
async fn the_1_2_land_rule_limits_the_terrain_step_to_10_m() {
    let mut template = land_template();
    template.spawn_x_offset = 5.0;
    template.spawn_y_offset = 5.0;
    let near = slope_server(100.0, 0.2, &[]);
    let (_client, p) = search_player(&near);
    p.data().unit.transform.set_height(200.0);
    let world = near.world.world(0).unwrap();
    let t = summon_candidate_1_2(&near, &world, &p, &template).expect("candidate");
    assert!((t.z - 201.0).abs() < 1e-4, "{t:?}");
    // z = 100 + 2.1·y: 1150 at the player, 1160.5 at the first candidate (10.5 m, over the
    // 1.2 `terrain - player_z <= 10` boundary).
    let above = slope_server(100.0, 2.1, &[]);
    let (_client, p) = search_player(&above);
    p.data().unit.transform.set_height(1150.0);
    let world = above.world.world(0).unwrap();
    assert!(summon_candidate_1_2(&above, &world, &p, &template).is_none());
    // z = 100 + 2.5·y: 1350 at the player, 1362.5 at the first candidate (12.5 m).
    let far = slope_server(100.0, 2.5, &[]);
    let (_client, p) = search_player(&far);
    p.data().unit.transform.set_height(1350.0);
    let world = far.world.world(0).unwrap();
    assert!(summon_candidate_1_2(&far, &world, &p, &template).is_none());
}

/// 6 m up over 30 m: the candidate's terrain point is above the player's feet, so the ray
/// must end above the candidate's own terrain (the 1.2 rule), not at the player's feet.
#[tokio::test]
async fn an_uphill_land_summon_is_accepted() {
    let s = slope_server(100.0, 0.2, &[]);
    let (_client, p) = search_player(&s);
    p.data().unit.transform.set_height(200.0);
    let world = s.world.world(0).unwrap();
    let t = summon_candidate_1_2(&s, &world, &p, &land_template()).expect("uphill candidate");
    assert!((t.y - 530.0).abs() < 1e-4, "{t:?}");
    assert!((t.z - 206.0).abs() < 1e-4, "{t:?}");
}

/// A client `SummonPos` uphill is validated with the same terrain-point ray.
#[tokio::test]
async fn an_uphill_client_placement_is_validated() {
    let s = slope_server(100.0, 0.2, &[]);
    let (_client, p) = search_player(&s);
    p.data().unit.transform.set_height(200.0);
    let world = s.world.world(0).unwrap();
    let placement = SummonPlacement {
        x: 500.0,
        y: 530.0,
        z: 206.0,
        yaw: 0.0,
    };
    let t = validated_summon_placement(&s, &world, &p, &land_template(), Some(placement), 0.0)
        .expect("uphill placement");
    assert!((t.z - 206.0).abs() < 1e-4, "{t:?}");
    assert!(summon_placement_clear_1_2(
        &s,
        &world,
        &p,
        &land_template(),
        &t
    ));
}

/// Without the collision service the client placement's land surface is the same terrain
/// helper as every other land path (`summon_terrain`: heightmap, else 0), not the client's
/// `z`: the 1.2 rule has no static ground to fall back to.
#[tokio::test]
async fn a_client_placement_without_collision_uses_the_terrain_fallback() {
    // A low ocean level keeps the terrain fallback (0) on land; the world default (100)
    // would make this sea floor and the new surface check refuse it.
    let mut world = crate::data::world_templates::WorldTemplate::synthetic(
        0,
        "main_world",
        2,
        2,
        crate::test_support::TEST_ZONE,
    );
    world.ocean_level = -1000.0;
    let s = crate::test_support::test_server_with_world(land_data(), world);
    s.config.update(|c| c.physics.enable = false);
    let files = Arc::new(ClientFileManager::from_sources(
        &[],
        std::path::Path::new("."),
    ));
    s.systems
        .npcs
        .heightmaps
        .install(Arc::new(HeightmapWorlds::new(
            files,
            &s.game_data.worlds,
            HeightmapConfig {
                heightmaps_enable: true,
                ..Default::default()
            },
        )));
    let (_client, p) = search_player(&s);
    // x = -2 makes the heightmap read fail (`x % 1024 / 2 < 0`), so the terrain helper
    // falls back to 0; the old placement fallback kept the client's z (200).
    p.data()
        .unit
        .transform
        .set_position(-2.0, 500.0, 200.0, 0.0, 0.0, 0.0);
    let world = s.world.world(0).unwrap();
    let placement = SummonPlacement {
        x: -2.0,
        y: 530.0,
        z: 200.0,
        yaw: 0.0,
    };
    let t = validated_summon_placement(&s, &world, &p, &land_template(), Some(placement), 0.0)
        .expect("placement");
    assert!(
        (t.z - summon_terrain(&s, &world, t.x, t.y)).abs() < 1e-4,
        "the fallback must be the terrain helper: {t:?}"
    );
    assert!((t.z - 0.0).abs() < 1e-4, "no heightmap data: {t:?}");
}

/// A client placement whose *spawn surface* is under water is refused even when the
/// client's z is above sea level (the old check tested the client z, not the terrain).
#[tokio::test]
async fn a_client_placement_over_the_sea_floor_is_refused() {
    let s = world_on(
        land_data(),
        Plane {
            a: 60.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    let (mut client, p) = search_player(&s);
    p.data().unit.transform.set_height(100.0);
    let world = s.world.world(0).unwrap();
    let placement = SummonPlacement {
        x: 500.0,
        y: 530.0,
        z: 100.0,
        yaw: 0.0,
    };
    assert!(
        validated_summon_placement(&s, &world, &p, &land_template(), Some(placement), 0.0)
            .is_none()
    );
    assert!(super::validated_client_spawn(
        &s,
        &world,
        &p,
        &land_template(),
        500.0,
        530.0,
        100.0,
        0.0
    )
    .is_none());
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert_eq!(
        codes,
        vec![
            super::error::INVALID_SUMMON_LOCATION,
            super::error::INVALID_SUMMON_LOCATION
        ]
    );
}

/// A wall between the player and the spot still refuses the summon.
#[tokio::test]
async fn a_wall_between_the_player_and_a_land_spot_refuses() {
    let s = slope_server(
        100.0,
        0.0,
        &[(Vec3::new(500.0, 515.0, 103.0), Vec3::new(5.0, 1.0, 3.0))],
    );
    let (_client, p) = search_player(&s);
    p.data().unit.transform.set_height(100.0);
    let world = s.world.world(0).unwrap();
    assert!(summon_candidate_1_2(&s, &world, &p, &land_template()).is_none());
}

/// The 1.2 land rule takes the candidate height from the terrain only
/// (`I3DEngine::GetTerrainElevation`, `FUN_39347fb0`). A deck between the player's eyes and
/// the candidate's terrain point is therefore hit by the line-of-sight ray (statics + terrain,
/// mask 0x107) and the summon is refused with error 100, instead of the deck becoming the
/// spawn floor.
#[tokio::test]
async fn a_deck_above_the_candidate_terrain_refuses_a_land_summon() {
    // A 1 m slab with its top 5 m above the terrain, under the player and every candidate.
    let s = world_on(
        land_data(),
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[(
            Vec3::new(500.0, 515.0, 104.5),
            Vec3::new(10.0, 25.0, 0.5),
            false,
        )],
    );
    install_empty_hull_loader(&s);
    let (mut client, p) = search_player(&s);
    client.conn.active_char = Some(p.clone());
    p.data().unit.transform.set_height(105.0);
    give_summon_item(&s, &p, 0x9110);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9110, RULE_ITEM, false, None, None, 0.0),
    )
    .await
    .expect("summon job");
    assert!(result.is_none(), "the deck blocks every candidate's ray");
    assert!(
        s.items.item_by_id(0x9110).is_some(),
        "a refused summon keeps the item"
    );
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(codes.contains(&100), "{codes:?}");
}

/// The same deck above the terrain refuses a client-supplied `SummonPos`: the 1.2
/// placement check's line-of-sight ray hits it (error 100) and the item is not used.
#[tokio::test]
async fn a_deck_above_the_candidate_terrain_refuses_a_client_placement() {
    let s = world_on(
        land_data(),
        Plane {
            a: 110.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[(
            Vec3::new(500.0, 515.0, 114.5),
            Vec3::new(10.0, 25.0, 0.5),
            false,
        )],
    );
    install_empty_hull_loader(&s);
    let (mut client, p) = search_player(&s);
    client.conn.active_char = Some(p.clone());
    p.data().unit.transform.set_height(115.0);
    give_summon_item(&s, &p, 0x9210);
    let placement = SummonPlacement {
        x: 500.0,
        y: 530.0,
        z: 110.0,
        yaw: 0.0,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9210, RULE_ITEM, false, None, Some(placement), 0.0),
    )
    .await
    .expect("summon job");
    assert!(
        result.is_none(),
        "the deck blocks the client placement's ray"
    );
    assert!(
        s.items.item_by_id(0x9210).is_some(),
        "a refused summon keeps the item"
    );
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(codes.contains(&100), "{codes:?}");
}

/// A positioned land spawn re-grounds to the terrain, not to a low static floor: the 1.2
/// land rule never uses statics as the ground (the search, placement and recheck paths do
/// the same).
#[tokio::test]
async fn a_land_spawn_under_a_low_deck_regrounds_to_the_terrain() {
    let s = world_on(
        land_data(),
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[(
            Vec3::new(500.0, 530.0, 100.75),
            Vec3::new(10.0, 10.0, 0.25),
            false,
        )],
    );
    install_empty_hull_loader(&s);
    let (_client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9111);
    let mut at = crate::models::transform::Transform::default();
    at.set_position(500.0, 530.0, 100.0, 0.0, 0.0, 0.0);
    let slave = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9111, RULE_ITEM, false, Some(at), None, 0.0),
    )
    .await
    .expect("summon job")
    .expect("the cart spawns");
    let t = slave.data().unit.transform;
    assert!(
        (t.z - 100.0).abs() < 1e-4,
        "the terrain, not the low deck top: {t:?}"
    );
}

/// Sends a `CSSpawnSlave` through the packet handler, as the client does. The handler now
/// starts the item's use-skill cast; the spawn (or refusal) runs asynchronously.
async fn send_spawn_slave(
    s: &crate::GameServer,
    client: &mut TestClient,
    slave_id: u32,
    at: (f32, f32, f32),
    z_rot: f32,
    item_id: u64,
) {
    use crate::net::registry::ClientPacket;
    use crate::systems::vehicles::packets::CSSpawnSlavePacket;
    CSSpawnSlavePacket {
        slave_id,
        x: at.0,
        y: at.1,
        z: at.2,
        z_rot,
        item_id,
        slot_type: 0,
        slot: 0,
        hide_spawn_effect: false,
    }
    .handle(s, &mut client.conn)
    .await
    .unwrap();
}

fn error_codes(frames: &[(u16, Vec<u8>)]) -> Vec<i16> {
    frames
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect()
}

/// The `SCSkillStarted` result bytes (the last byte of the failure body).
fn started_results(frames: &[(u16, Vec<u8>)]) -> Vec<u8> {
    frames
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCSkillStartedPacket)
        .filter_map(|f| f.1.last().copied())
        .collect()
}

async fn wait_for_slaves(s: &crate::GameServer, count: usize) -> Vec<super::SlaveRef> {
    let world = s.world.world(0).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let live = super::all_slaves(&world);
            if live.len() >= count {
                return live;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_default()
}

async fn wait_for_error(client: &mut TestClient, codes: &[i16]) -> Vec<(u16, Vec<u8>)> {
    let mut all = Vec::new();
    for _ in 0..400 {
        all.extend(client.drain());
        if all.iter().any(|f| {
            f.0 == crate::opcodes::sc::SCErrorMsgPacket
                && codes.contains(&i16::from_le_bytes([f.1[0], f.1[1]]))
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    all
}

/// `CSSpawnSlave` runs the item-use cast: it spawns at the client's position and yaw after
/// the cast, commits the summon item exactly once and starts the skill cooldown.
#[tokio::test]
async fn csspawnslave_spawns_at_the_client_position_and_commits_once() {
    let s = world_on(
        land_data(),
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        1.25,
        0x9110,
    )
    .await;
    let spawned = wait_for_slaves(&s, 1).await;
    let frames = client.drain();
    assert_eq!(spawned.len(), 1, "the cart spawns");
    let slave = &spawned[0];
    let t = slave.data().unit.transform;
    assert!(
        (t.x - 500.0).abs() < 1e-4 && (t.y - 530.0).abs() < 1e-4,
        "{t:?}"
    );
    assert!((t.z - 100.0).abs() < 1e-4, "{t:?}");
    assert!(
        (t.rot_z - 1.25).abs() < 1e-4,
        "the client yaw is kept: {t:?}"
    );
    let tasks: Vec<&(u16, Vec<u8>)> = frames
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCItemTaskSuccessPacket)
        .collect();
    assert_eq!(tasks.len(), 1, "one item update: {:x?}", frames);
    assert_eq!(tasks[0].1[0], item_task_type::UPDATE_SUMMON_MATE_ITEM);
    let item = crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110).expect("the item stays");
    assert_eq!(item.details.slave_db_id, slave.data().id);
    assert_eq!(item.details.slave_type, 2);
    assert!(
        p.data().unit.cooldowns.cooldowns.contains_key(&RULE_SKILL),
        "the cast started the skill cooldown"
    );
}

/// The item-cast gates apply to the synthesized cast: a dead or stunned sender and a skill
/// on cooldown are refused before any placement / spawn work.
#[tokio::test]
async fn csspawnslave_refuses_dead_stunned_and_cooldown_senders() {
    use crate::systems::skills::types::skill_result;

    let flat = || {
        world_on(
            land_data(),
            Plane {
                a: 100.0,
                b: 0.0,
                c: 0.0,
                n: 1024,
            },
            &[],
        )
    };
    // Dead.
    let s = flat();
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    p.data().unit.hp = 0;
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = client.drain();
    assert_eq!(
        started_results(&frames).last().copied(),
        Some(skill_result::SOURCE_DIED),
        "{:x?}",
        frames
    );
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());

    // Stunned.
    let mut data = land_data();
    data.systems
        .skills
        .add_buff(crate::systems::skills::data::BuffTemplate {
            id: 10,
            stun: true,
            ..Default::default()
        });
    let s = world_on(
        data,
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    let mut buff = crate::models::buff::Buff::new(
        crate::systems::skills::buffs::net_template(
            &s,
            s.game_data.systems.skills.buff(10).unwrap(),
        ),
        crate::systems::skills::buffs::unit_caster(p.obj_id),
        Some((1, 10)),
        crate::world::now_ms(),
    );
    buff.state = crate::models::buff::effect_state::ACTING;
    p.data().unit.buffs.effects.push(buff);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = client.drain();
    assert_eq!(
        started_results(&frames).last().copied(),
        Some(skill_result::CANNOT_CAST_IN_STUN),
        "{:x?}",
        frames
    );
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());

    // On cooldown.
    let s = flat();
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    p.data()
        .unit
        .cooldowns
        .add_cooldown(RULE_SKILL, 10_000, crate::world::now_ms());
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = client.drain();
    assert_eq!(
        started_results(&frames).last().copied(),
        Some(skill_result::COOLDOWN_TIME),
        "{:x?}",
        frames
    );
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());
}

/// The item-use zone gate (`item_cannot_use_here` 440 + `ZONE_BANNED` 58) applies to the
/// synthesized cast.
#[tokio::test]
async fn csspawnslave_refuses_a_zone_banned_item() {
    use crate::data::world_data::Zone;
    use crate::systems::skills::data::zone_rules::BannedTag;
    use crate::test_support::TEST_ZONE;

    let mut data = land_data();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            id: 1,
            zone_key: TEST_ZONE,
            group_id: 45,
            ..Default::default()
        },
    );
    data.zones.zone_id_to_key.insert(1, TEST_ZONE);
    data.systems
        .skills
        .zone_rules
        .item_tags
        .insert(RULE_ITEM, vec![471]);
    data.systems.skills.zone_rules.banned_tags.insert(
        45,
        vec![BannedTag {
            id: 1,
            tag_id: 471,
            banned_periods_id: 0,
        }],
    );
    let s = world_on(
        data,
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = client.drain();
    assert!(
        error_codes(&frames).contains(&440),
        "item_cannot_use_here: {:x?}",
        frames
    );
    assert_eq!(
        started_results(&frames).last().copied(),
        Some(crate::systems::skills::unit_reqs::skill_result_for_key(
            "SKILL_CANNOT_USE_HERE",
        )),
        "{:x?}",
        frames
    );
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());
}

/// A spot beyond `spawn_valid_area_range`, terrain more than 10 m above the owner, and a
/// land spot whose spawn surface is under water are refused with
/// `InvalidLocation`, and no item is committed.
#[tokio::test]
async fn csspawnslave_refuses_a_far_high_or_wrong_surface_spot() {
    let s = world_on(
        land_data(),
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 560.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = wait_for_error(&mut client, &[super::error::INVALID_SUMMON_LOCATION]).await;
    assert!(error_codes(&frames).contains(&super::error::INVALID_SUMMON_LOCATION));
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());
    assert_eq!(
        crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110)
            .unwrap()
            .details
            .slave_db_id,
        0,
        "a refused placement does not commit the item"
    );

    // The 1.2 land rule: the terrain is at most 10 m above the player.
    let high = world_on(
        land_data(),
        Plane {
            a: 111.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&high);
    install_cast_hooks(&high);
    let (mut client, p) = search_player(&high);
    give_summon_item(&high, &p, 0x9110);
    send_spawn_slave(
        &high,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 111.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = wait_for_error(&mut client, &[super::error::INVALID_SUMMON_LOCATION]).await;
    assert!(error_codes(&frames).contains(&super::error::INVALID_SUMMON_LOCATION));
    assert!(super::all_slaves(&high.world.world(0).unwrap()).is_empty());

    // The client z is above sea level, but the spawn surface (terrain 60) is the sea floor.
    let wet = world_on(
        land_data(),
        Plane {
            a: 60.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&wet);
    install_cast_hooks(&wet);
    let (mut client, p) = search_player(&wet);
    give_summon_item(&wet, &p, 0x9110);
    send_spawn_slave(
        &wet,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = wait_for_error(&mut client, &[super::error::INVALID_SUMMON_LOCATION]).await;
    assert!(error_codes(&frames).contains(&super::error::INVALID_SUMMON_LOCATION));
    assert!(super::all_slaves(&wet.world.world(0).unwrap()).is_empty());
}

/// Without the item the packet names, `CSSpawnSlave` starts no cast and spawns nothing.
#[tokio::test]
async fn csspawnslave_without_an_owned_matching_item_spawns_nothing() {
    // A second real land template so a mismatched slave id is not refused by the template
    // lookup alone: only the item/template match can stop it spawning the item's slave.
    let mut data = land_data();
    let mut other = land_template();
    other.id = RULE_SLAVE + 1;
    data.systems
        .vehicles
        .slave_templates
        .insert(RULE_SLAVE + 1, other);
    let s = world_on(
        data,
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    // Not owned; then owned but for another slave template.
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        0.0,
        0xDEAD,
    )
    .await;
    give_summon_item(&s, &p, 0x9110);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE + 1,
        (500.0, 530.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    send_spawn_slave(&s, &mut client, RULE_SLAVE, (500.0, 530.0, 101.0), 0.0, 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(client.drain().is_empty(), "no cast was started");
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());
    assert_eq!(
        crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110)
            .unwrap()
            .details
            .slave_db_id,
        0
    );
}

/// The item path's one-active-vehicle check still applies: a prior hull that cannot be
/// deleted refuses the packet spawn and keeps the prior vehicle and its item binding.
#[tokio::test]
async fn csspawnslave_keeps_the_item_path_one_active_vehicle_refusal() {
    let s = world_on(
        land_data(),
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let world = s.world.world(0).unwrap();
    let prior = wait_for_slaves(&s, 1)
        .await
        .into_iter()
        .next()
        .expect("first cart");
    let prior_obj = prior.obj_id;
    let bound = crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110)
        .unwrap()
        .details
        .slave_db_id;
    let mut doodad = Doodad::new(1);
    doodad.item_id = 1;
    doodad.item_template_id = 1;
    let doodad = crate::world::Entity::new(0x00F0_0101, doodad);
    world.add_object(&doodad);
    prior.data().attached_doodads.push(doodad.obj_id);
    // The first cast started its 10 s cooldown; clear it so this second press reaches the
    // one-active-vehicle check instead of being refused as a cooldown re-press.
    p.data().unit.cooldowns.remove_cooldown(RULE_SKILL);
    p.data().unit.skill_last_used_ms = 0;
    p.data().unit.global_cooldown_ms = 0;

    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 520.0, 101.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = wait_for_error(&mut client, &[super::error::SLAVE_EQUIPMENT_LOADED_ITEM]).await;
    assert!(error_codes(&frames).contains(&super::error::SLAVE_EQUIPMENT_LOADED_ITEM));
    let live = super::all_slaves(&world);
    assert_eq!(live.len(), 1, "the prior vehicle stays");
    assert_eq!(live[0].obj_id, prior_obj);
    assert_eq!(
        crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110)
            .unwrap()
            .details
            .slave_db_id,
        bound,
        "the binding is unchanged"
    );
}

/// A boat `CSSpawnSlave` on water spawns on the water surface (the hull offset folded in).
#[tokio::test]
async fn csspawnslave_boat_on_water_spawns_on_the_surface() {
    let s = world_at_with(search_data(), GROUND, &[]);
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9110);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, WATER),
        1.0,
        0x9110,
    )
    .await;
    let spawned = wait_for_slaves(&s, 1).await;
    assert_eq!(spawned.len(), 1, "the boat spawns");
    let t = spawned[0].data().unit.transform;
    let offset = summon_offset_z(&ShipModel {
        mass_center_z: -4.0,
        keel_height: 0.2,
        ..Default::default()
    });
    assert!(
        (t.z - (WATER + offset)).abs() < 1e-3,
        "the water surface + hull offset: {t:?}"
    );
}

/// The 1.2 full placement check refusal answers `SLAVE_SPAWN_SHIP_NEED_MORE_SPACE` (100):
/// a deck between the player and the chosen terrain point blocks the line of sight.
#[tokio::test]
async fn csspawnslave_refuses_a_hull_box_blocked_spot_with_100() {
    let s = world_on(
        land_data(),
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[(
            Vec3::new(500.0, 515.0, 104.5),
            Vec3::new(10.0, 25.0, 0.5),
            false,
        )],
    );
    install_empty_hull_loader(&s);
    install_cast_hooks(&s);
    let (mut client, p) = search_player(&s);
    p.data().unit.transform.set_height(105.0);
    give_summon_item(&s, &p, 0x9110);
    send_spawn_slave(
        &s,
        &mut client,
        RULE_SLAVE,
        (500.0, 530.0, 105.0),
        0.0,
        0x9110,
    )
    .await;
    let frames = wait_for_error(
        &mut client,
        &[super::error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE],
    )
    .await;
    assert!(
        error_codes(&frames).contains(&super::error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE),
        "{:x?}",
        frames
    );
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());
    assert_eq!(
        crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110)
            .unwrap()
            .details
            .slave_db_id,
        0
    );
}

/// A refused item summon (terrain > 10 m above the owner) returns before the item commit and
/// the old-vehicle deletion: the old vehicle and the item binding stay.
#[tokio::test]
async fn a_refused_land_summon_keeps_the_old_vehicle_and_binding() {
    let s = world_on(
        land_data(),
        Plane {
            a: 100.0,
            b: 0.0,
            c: 0.4,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    let (mut client, p) = search_player(&s);
    p.data()
        .unit
        .transform
        .set_position(500.0, 500.0, 300.0, 0.0, 0.0, 0.0);
    give_summon_item(&s, &p, 0x9110);
    let mut first_at = crate::models::transform::Transform::default();
    first_at.set_position(500.0, 520.0, 308.0, 0.0, 0.0, 0.0);
    let first = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9110, RULE_ITEM, false, Some(first_at), None, 0.0),
    )
    .await
    .expect("first summon")
    .expect("the first cart spawns");
    let world = s.world.world(0).unwrap();
    let bound = crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110)
        .unwrap()
        .details
        .slave_db_id;
    assert_eq!(bound, first.data().id);

    // 312 at y = 530 is 12 m above the player (300).
    let mut second_at = crate::models::transform::Transform::default();
    second_at.set_position(500.0, 530.0, 312.0, 0.0, 0.0, 0.0);
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9110, RULE_ITEM, false, Some(second_at), None, 0.0),
    )
    .await
    .expect("second summon");
    assert!(second.is_none(), "the 10 m rule refuses the second spot");
    assert!(
        world.slave(first.obj_id).is_some(),
        "the old vehicle is not deleted"
    );
    assert_eq!(
        crate::systems::vehicles::mates::owned_item(&s, &p, 0x9110)
            .unwrap()
            .details
            .slave_db_id,
        bound,
        "the item binding is unchanged"
    );
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert_eq!(codes, vec![super::error::INVALID_SUMMON_LOCATION]);
}

/// GM / ownerless spawns pass no item and keep the old behaviour at a cliff.
#[tokio::test]
async fn an_ownerless_spawn_at_a_cliff_skips_the_item_height_rule() {
    let s = world_on(
        land_data(),
        Plane {
            a: 111.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    let mut at = crate::models::transform::Transform::default();
    at.set_position(500.0, 530.0, 111.0, 0.0, 0.0, 0.0);
    let spawned = super::create_core(
        &s,
        &world,
        Some(&p),
        None,
        RULE_SLAVE,
        None,
        false,
        Some(at),
        None,
    );
    assert!(
        spawned.is_some(),
        "an item-less spawn at terrain 11 m above the owner still spawns"
    );
}

/// A position override (no 1.2 search) whose land terrain is more than 10 m above the
/// owner is refused with `InvalidLocation` before the summon item is committed.
#[tokio::test]
async fn a_land_position_override_11_m_above_the_owner_is_refused() {
    let s = world_on(
        land_data(),
        Plane {
            a: 111.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        },
        &[],
    );
    install_empty_hull_loader(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9120);
    let mut at = crate::models::transform::Transform::default();
    at.set_position(500.0, 530.0, 111.0, 0.0, 0.0, 0.0);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9120, RULE_ITEM, false, Some(at), None, 0.0),
    )
    .await
    .expect("summon job");
    assert!(result.is_none());
    assert!(super::all_slaves(&s.world.world(0).unwrap()).is_empty());
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert_eq!(codes, vec![super::error::INVALID_SUMMON_LOCATION]);
    assert_eq!(
        crate::systems::vehicles::mates::owned_item(&s, &p, 0x9120)
            .unwrap()
            .details
            .slave_db_id,
        0,
        "the refusal is before the item commit"
    );
}

#[tokio::test]
async fn a_provided_placement_is_validated_with_the_same_1_2_checks() {
    let s = ship_server_at(GROUND, &[]);
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    let placement = SummonPlacement {
        x: 500.0,
        y: 530.0,
        z: WATER,
        yaw: 0.0,
    };
    let t = validated_summon_placement(&s, &world, &p, &ship_template(), Some(placement), 0.0)
        .expect("valid placement");
    assert!(summon_placement_clear_1_2(
        &s,
        &world,
        &p,
        &ship_template(),
        &t
    ));
    let platform = ship_server_at(101.0, &[]);
    let (_client, p) = search_player(&platform);
    let world = platform.world.world(0).unwrap();
    assert!(!summon_placement_clear_1_2(
        &platform,
        &world,
        &p,
        &ship_template(),
        &t
    ));
}

/// The placement transform is the final spawn pose: yaw = player yaw + 90 and the ship
/// waterline offset folded in. The 1.2 check uses that yaw, so a hull-side obstruction
/// rejects the spot even though the client-supplied yaw would miss it.
#[tokio::test]
async fn a_placement_is_checked_at_the_pose_it_is_spawned_at() {
    let s = ship_server_at(
        GROUND,
        &[(Vec3::new(510.0, 530.0, 100.75), Vec3::new(2.0, 20.0, 1.25))],
    );
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    let placement = SummonPlacement {
        x: 500.0,
        y: 530.0,
        z: WATER,
        yaw: 0.0,
    };
    let t = validated_summon_placement(&s, &world, &p, &ship_template(), Some(placement), 0.0)
        .expect("valid placement");
    let ship = s
        .game_data
        .systems
        .vehicles
        .ship_model(RULE_MODEL_ID)
        .unwrap();
    assert!(
        (t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
        "{t:?}"
    );
    assert!(
        (t.z - (WATER + summon_offset_z(ship))).abs() < 1e-4,
        "{t:?}"
    );
    // The client yaw is checked first: PWI keeps stale contacts for an empty entity list,
    // so the clear pose must not inherit the obstruction's contact.
    let mut client_yaw = t;
    client_yaw.rot_z = placement.yaw;
    assert!(
        summon_placement_clear_1_2(&s, &world, &p, &ship_template(), &client_yaw),
        "the client yaw would miss the obstruction"
    );
    assert!(
        !summon_placement_clear_1_2(&s, &world, &p, &ship_template(), &t),
        "the obstruction is inside the turned hull"
    );
}

/// With no authored model reference the 1.2 box comes from the ship model's mass box, so
/// shallow water under the waterline is still rejected instead of skipping the box check.
#[tokio::test]
async fn a_missing_model_reference_falls_back_to_the_mass_box() {
    let s = ship_server_at(95.0, &[]);
    assert!(summon_model_box(&s, &ship_template()).is_some());
    let (_client, p) = search_player(&s);
    let world = s.world.world(0).unwrap();
    assert!(
        summon_candidate_1_2(&s, &world, &p, &ship_template()).is_none(),
        "the mass box reaches below the water into the seabed"
    );
}

#[tokio::test]
async fn no_candidate_sends_error_100_keeps_the_item_and_refunds_the_cooldown() {
    let mut data = with_ship_data(crate::data::GameData::empty());
    data.items.templates.insert(
        RULE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonSlave {
                slave_id: RULE_SLAVE,
            },
            use_skill_id: RULE_SKILL,
            ..ItemTemplate::plain(RULE_ITEM, 1)
        },
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: RULE_SKILL,
        ..Default::default()
    });
    let s = world_at_with(data, 101.0, &[]);
    let (mut client, p) = search_player(&s);
    client.conn.active_char = Some(p.clone());
    let bag = p
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    let template = s.game_data.items.template(RULE_ITEM).unwrap().clone();
    let mut item = Item::create(0x9001, &template, 1, 0, 1, &s.game_data.items, 0);
    item.owner_id = u64::from(p.character_id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    bag.lock().unwrap().items.push(item);
    s.items.add_container(bag);
    p.data().unit.cooldowns.add_cooldown(RULE_SKILL, 10_000, 0);
    p.data().unit.global_cooldown_ms = 10_000;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9001, RULE_ITEM, false, None, None, 0.0),
    )
    .await
    .expect("summon job");
    assert!(result.is_none());
    assert!(get_active_slave_by_owner_obj_id(&s.world.world(0).unwrap(), p.obj_id).is_none());
    assert!(s.items.item_by_id(0x9001).is_some());
    let frames = client.drain();
    let codes: Vec<i16> = frames
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(codes.contains(&100), "{codes:?}");
    let reset = frames
        .iter()
        .find(|(op, _)| *op == crate::opcodes::sc::SCSkillCooldownResetPacket)
        .expect("the client cooldown is reset too");
    let mut e = PacketStream::new();
    e.write_bc(p.obj_id)
        .write_u32(RULE_SKILL)
        .write_u32(0)
        .write_bool(true);
    assert_eq!(reset.1, e.into_bytes());
    assert!(!p.data().unit.cooldowns.cooldowns.contains_key(&RULE_SKILL));
    assert_eq!(p.data().unit.global_cooldown_ms, 0);
}

/// A SummonPos placement outside the data range fails with error 215 before any spawn; the
/// skill cooldown/GCD must be refunded like the no-candidate refusal.
#[tokio::test]
async fn an_invalid_placement_refunds_the_cooldown() {
    let mut data = with_ship_data(crate::data::GameData::empty());
    data.items.templates.insert(
        RULE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonSlave {
                slave_id: RULE_SLAVE,
            },
            use_skill_id: RULE_SKILL,
            ..ItemTemplate::plain(RULE_ITEM, 1)
        },
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: RULE_SKILL,
        ..Default::default()
    });
    data.systems
        .vehicles
        .slave_templates
        .get_mut(&RULE_SLAVE)
        .unwrap()
        .spawn_valid_area_range = 10;
    let s = world_at_with(data, GROUND, &[]);
    let (mut client, p) = search_player(&s);
    let bag = p
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    let template = s.game_data.items.template(RULE_ITEM).unwrap().clone();
    let mut item = Item::create(0x9002, &template, 1, 0, 1, &s.game_data.items, 0);
    item.owner_id = u64::from(p.character_id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    bag.lock().unwrap().items.push(item);
    s.items.add_container(bag);
    p.data().unit.cooldowns.add_cooldown(RULE_SKILL, 10_000, 0);
    p.data().unit.global_cooldown_ms = 10_000;
    let placement = crate::systems::skills::ext::SummonPlacement {
        x: 530.0,
        y: 500.0,
        z: WATER,
        yaw: 0.0,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9002, RULE_ITEM, false, None, Some(placement), 0.0),
    )
    .await
    .expect("summon job");
    assert!(result.is_none());
    assert!(s.items.item_by_id(0x9002).is_some());
    assert!(get_active_slave_by_owner_obj_id(&s.world.world(0).unwrap(), p.obj_id).is_none());
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(
        codes.contains(&super::error::INVALID_SUMMON_LOCATION),
        "{codes:?}"
    );
    assert!(!p.data().unit.cooldowns.cooldowns.contains_key(&RULE_SKILL));
    assert_eq!(p.data().unit.global_cooldown_ms, 0);
}

/// The summon job waits (reservations, DB, hull load) while the player can cast again. Its
/// delayed error-100 refund must not clear the newer GCD or a newer cooldown of the same
/// skill: only the entries captured when the job started are rolled back.
#[tokio::test]
async fn a_newer_skill_cooldown_survives_a_delayed_summon_refusal() {
    let mut data = with_ship_data(crate::data::GameData::empty());
    data.items.templates.insert(
        RULE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonSlave {
                slave_id: RULE_SLAVE,
            },
            use_skill_id: RULE_SKILL,
            ..ItemTemplate::plain(RULE_ITEM, 1)
        },
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: RULE_SKILL,
        ..Default::default()
    });
    let s = world_at_with(data, 101.0, &[]);
    let (mut client, p) = search_player(&s);
    client.conn.active_char = Some(p.clone());
    let bag = p
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    let template = s.game_data.items.template(RULE_ITEM).unwrap().clone();
    let mut item = Item::create(0x9003, &template, 1, 0, 1, &s.game_data.items, 0);
    item.owner_id = u64::from(p.character_id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    bag.lock().unwrap().items.push(item);
    s.items.add_container(bag);
    p.data().unit.cooldowns.add_cooldown(RULE_SKILL, 10_000, 0);
    p.data().unit.global_cooldown_ms = 10_000;
    // The job captures the cooldowns when it is created; the next skill replaces them
    // before the detached task is polled.
    let job = create_for_owner(&s, &p, 0x9003, RULE_ITEM, false, None, None, 0.0);
    p.data().unit.global_cooldown_ms = 20_000;
    p.data().unit.cooldowns.cooldowns.insert(RULE_SKILL, 20_000);
    let result = tokio::time::timeout(Duration::from_secs(2), job)
        .await
        .expect("summon job");
    assert!(result.is_none());
    let frames = client.drain();
    let codes: Vec<i16> = frames
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(codes.contains(&100), "{codes:?}");
    assert!(
        !frames
            .iter()
            .any(|(op, _)| *op == crate::opcodes::sc::SCSkillCooldownResetPacket),
        "the newer client cooldown must not be reset"
    );
    assert_eq!(
        p.data().unit.global_cooldown_ms,
        20_000,
        "the newer GCD belongs to the later skill"
    );
    assert_eq!(
        p.data().unit.cooldowns.cooldowns.get(&RULE_SKILL),
        Some(&20_000),
        "the newer cooldown of the same skill is kept"
    );
}

/// The invalid-placement refusal path uses the same guarded refund: a newer GCD survives,
/// while the summon skill's cooldown (still the captured entry) is cleared.
#[tokio::test]
async fn an_invalid_placement_keeps_a_newer_gcd() {
    let mut data = with_ship_data(crate::data::GameData::empty());
    data.items.templates.insert(
        RULE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonSlave {
                slave_id: RULE_SLAVE,
            },
            use_skill_id: RULE_SKILL,
            ..ItemTemplate::plain(RULE_ITEM, 1)
        },
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: RULE_SKILL,
        ..Default::default()
    });
    data.systems
        .vehicles
        .slave_templates
        .get_mut(&RULE_SLAVE)
        .unwrap()
        .spawn_valid_area_range = 10;
    let s = world_at_with(data, GROUND, &[]);
    let (mut client, p) = search_player(&s);
    let bag = p
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    let template = s.game_data.items.template(RULE_ITEM).unwrap().clone();
    let mut item = Item::create(0x9004, &template, 1, 0, 1, &s.game_data.items, 0);
    item.owner_id = u64::from(p.character_id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    bag.lock().unwrap().items.push(item);
    s.items.add_container(bag);
    p.data().unit.cooldowns.add_cooldown(RULE_SKILL, 10_000, 0);
    p.data().unit.global_cooldown_ms = 10_000;
    let placement = crate::systems::skills::ext::SummonPlacement {
        x: 530.0,
        y: 500.0,
        z: WATER,
        yaw: 0.0,
    };
    let job = create_for_owner(&s, &p, 0x9004, RULE_ITEM, false, None, Some(placement), 0.0);
    p.data().unit.global_cooldown_ms = 20_000;
    let result = tokio::time::timeout(Duration::from_secs(2), job)
        .await
        .expect("summon job");
    assert!(result.is_none());
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(
        codes.contains(&super::error::INVALID_SUMMON_LOCATION),
        "{codes:?}"
    );
    assert_eq!(
        p.data().unit.global_cooldown_ms,
        20_000,
        "the newer GCD belongs to the later skill"
    );
    assert!(
        !p.data().unit.cooldowns.cooldowns.contains_key(&RULE_SKILL),
        "the summon skill's captured cooldown is still refunded"
    );
}

/// GM `/slave spawn` bypasses the client-side rule: it keeps the C# in-front placement and
/// still spawns when the 1.2 search would refuse.
#[tokio::test]
async fn gm_spawn_bypasses_the_1_2_search() {
    let s = ship_server_at(101.0, &[]);
    let world = s.world.world(0).unwrap();
    s.systems.npcs.spawns.get_or_create(&world);
    let (_client, p) = search_player(&s);
    // `/slave spawn` passes no item and no placement, so the item search is skipped and the
    // C# in-front placement in `create_spawn_committed` is used (the async GM wrapper only
    // adds the DB/hull bookkeeping).
    let slave = super::create_core(
        &s,
        &world,
        Some(&p),
        None,
        RULE_SLAVE,
        None,
        false,
        None,
        None,
    )
    .expect("GM spawn must bypass the 1.2 search");
    let t = slave.data().unit.transform;
    assert_eq!(slave.data().summoner_obj_id, p.obj_id);
    assert!(
        (t.x - 500.0).abs() < 1e-4 && (t.y - 520.0).abs() < 1e-4,
        "GM keeps the in-front placement: {t:?}"
    );
}

/// The 1.2 search with a summon item and cooldown skill, for the post-wait recheck tests.
fn search_data() -> crate::data::GameData {
    let mut data = with_ship_data(crate::data::GameData::empty());
    // MaxHp / MaxMp of a slave (unit_formulas kind 14 / 26, owner type 2) so a spawned
    // hull is alive; the no-stat fixture made it die at once and mark the item destroyed.
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[(1, "1000", 14, 2), (2, "100", 26, 2)],
        &[],
    );
    data.items.templates.insert(
        RULE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonSlave {
                slave_id: RULE_SLAVE,
            },
            use_skill_id: RULE_SKILL,
            ..ItemTemplate::plain(RULE_ITEM, 1)
        },
    );
    // The summon item cast: target type 12 (`SummonPos`), effect 60 (`SpawnSlave`), a
    // cooldown / GCD so the cast route's admission is observable.
    data.systems.skills.add_effect(
        crate::systems::skills::data::EffectKind::SpecialEffect,
        crate::systems::skills::data::EffectTemplate::Special(
            crate::systems::skills::data::effects::SpecialEffect {
                id: 1,
                special_effect_type_id: 60,
                value1: 0,
                value2: 0,
                value3: 0,
                value4: 0,
            },
        ),
        1,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: RULE_SKILL,
        target_type: 12,
        cooldown_time: 10_000,
        default_gcd: true,
        source_alive: true,
        effects: vec![crate::systems::skills::tests::skill_effect(
            1,
            crate::systems::skills::data::EffectKind::SpecialEffect,
            1,
        )],
        ..Default::default()
    });
    data
}

fn install_empty_hull_loader(s: &crate::GameServer) {
    *s.systems.vehicles.slave_summons.test_loader.lock().unwrap() =
        Some(super::TestSlaveLoader(Arc::new(|_, _| {
            Box::pin(async { Ok(super::LoadedHull::default()) })
        })));
}

/// The item-use cast path needs the economy item hooks and the npcs / vehicles effect hooks
/// (the same set a live server installs), or `start_skill` cannot resolve the item.
fn install_cast_hooks(s: &crate::GameServer) {
    crate::systems::economy::bridges::install(s);
    crate::systems::npcs::install_hooks(s);
    crate::systems::vehicles::install_hooks(s);
}

fn give_summon_item(s: &crate::GameServer, p: &crate::world::PlayerRef, item_id: u64) {
    let bag = p
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    let template = s.game_data.items.template(RULE_ITEM).unwrap().clone();
    let mut item = Item::create(item_id, &template, 1, 0, 1, &s.game_data.items, 0);
    item.owner_id = u64::from(p.character_id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    bag.lock().unwrap().items.push(item);
    s.items.add_container(bag);
}

/// Moves the player once the item wait is over, i.e. after the search chose its spot.
fn move_player_after_item_validation(
    s: &crate::GameServer,
    p: &crate::world::PlayerRef,
    x: f32,
    y: f32,
) {
    let p = p.clone();
    *s.systems
        .vehicles
        .slave_summons
        .test_after_item_validation
        .lock()
        .unwrap() = Some(super::TestPublicationHook(Arc::new(move |_| {
        p.data()
            .unit
            .transform
            .set_position(x, y, WATER, 0.0, 0.0, 0.0);
    })));
}

/// Turns the player in place once the item wait is over (the turn during the hull load).
fn turn_player_after_item_validation(s: &crate::GameServer, p: &crate::world::PlayerRef, yaw: f32) {
    let p = p.clone();
    *s.systems
        .vehicles
        .slave_summons
        .test_after_item_validation
        .lock()
        .unwrap() = Some(super::TestPublicationHook(Arc::new(move |_| {
        p.data().unit.transform.rot_z = yaw;
    })));
}

/// Adds a deck above the player and the chosen spot once the item wait is over: the spot
/// the search accepted becomes covered while the summon job waits.
fn cover_spot_after_item_validation(s: &crate::GameServer) {
    *s.systems
        .vehicles
        .slave_summons
        .test_after_item_validation
        .lock()
        .unwrap() = Some(super::TestPublicationHook(Arc::new(|server| {
        let pw = crate::systems::ai::collision::phys_world(server, 0).expect("physics world");
        let mut st = pw.lock();
        assert!(st
            .add_static_instance(
                &static_box(
                    Vec3::new(10.0, 25.0, 0.5),
                    Vec3::new(500.0, 515.0, 114.5),
                    false,
                ),
                crate::systems::ai::collision::world::Owner::Brush { cell: (0, 0) },
            )
            .is_some());
    })));
}

/// A deck that appears above the player's chosen spot during the waits is caught by the
/// post-wait recheck: the recheck's line-of-sight ray hits it, the rerun search finds no
/// candidate either, and the summon refuses with error 100 without using the item.
#[tokio::test]
async fn a_deck_appearing_after_the_search_refuses_the_post_wait_recheck() {
    let s = world_at_with(land_data(), 110.0, &[]);
    install_empty_hull_loader(&s);
    let (mut client, p) = search_player(&s);
    client.conn.active_char = Some(p.clone());
    p.data().unit.transform.set_height(115.0);
    give_summon_item(&s, &p, 0x9211);
    p.data().unit.cooldowns.add_cooldown(RULE_SKILL, 10_000, 0);
    p.data().unit.global_cooldown_ms = 10_000;
    cover_spot_after_item_validation(&s);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9211, RULE_ITEM, false, None, None, 0.0),
    )
    .await
    .expect("summon job");
    assert!(
        result.is_none(),
        "the deck blocks the recheck and the rerun search"
    );
    assert!(s.items.item_by_id(0x9211).is_some(), "the item is not used");
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(codes.contains(&100), "{codes:?}");
    assert!(!p.data().unit.cooldowns.cooldowns.contains_key(&RULE_SKILL));
    assert_eq!(p.data().unit.global_cooldown_ms, 0);
}

/// A spot chosen before the waits is rechecked at the player's current pose; when it no
/// longer passes, the search reruns from there instead of spawning the stale spot.
#[tokio::test]
async fn a_searched_spot_is_rechecked_after_the_waits_and_moved_with_the_player() {
    let s = world_at_with(search_data(), GROUND, &[]);
    install_empty_hull_loader(&s);
    let (_client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9101);
    move_player_after_item_validation(&s, &p, 700.0, 500.0);
    let slave = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9101, RULE_ITEM, false, None, None, 0.0),
    )
    .await
    .expect("summon job")
    .expect("the search must rerun from the current pose");
    let t = slave.data().unit.transform;
    assert!(
        (t.x - 700.0).abs() < 1e-4 && (t.y - 530.0).abs() < 1e-4,
        "the stale spot must move with the player: {t:?}"
    );
    assert!(
        (t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
        "{t:?}"
    );
}

/// When the player moved somewhere with no room, the recheck refuses with error 100, keeps
/// the item and refunds the skill cooldown exactly like the pre-wait refusal.
#[tokio::test]
async fn a_stale_search_spot_with_no_current_candidate_refuses_and_refunds() {
    let s = world_at_with(
        search_data(),
        GROUND,
        &[(
            Vec3::new(900.0, 515.0, 100.0),
            Vec3::new(40.0, 5.0, 10.0),
            false,
        )],
    );
    install_empty_hull_loader(&s);
    let (mut client, p) = search_player(&s);
    client.conn.active_char = Some(p.clone());
    give_summon_item(&s, &p, 0x9102);
    p.data().unit.cooldowns.add_cooldown(RULE_SKILL, 10_000, 0);
    p.data().unit.global_cooldown_ms = 10_000;
    move_player_after_item_validation(&s, &p, 900.0, 500.0);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9102, RULE_ITEM, false, None, None, 0.0),
    )
    .await
    .expect("summon job");
    assert!(result.is_none());
    assert!(s.items.item_by_id(0x9102).is_some());
    assert!(get_active_slave_by_owner_obj_id(&s.world.world(0).unwrap(), p.obj_id).is_none());
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(codes.contains(&100), "{codes:?}");
    assert!(!p.data().unit.cooldowns.cooldowns.contains_key(&RULE_SKILL));
    assert_eq!(p.data().unit.global_cooldown_ms, 0);
}

/// A `SummonPos` cleared beside a pier at the cast is invalid once the owner turns 90
/// degrees during the waits: the final ship yaw follows the owner's current pose, so the
/// full 1.2 placement check must run again on that transform and refuse (error 100),
/// instead of spawning the turned hull across the pier. The item is not used and the
/// cooldown is refunded.
#[tokio::test]
async fn a_turned_owner_makes_the_post_wait_placement_check_refuse_a_pier() {
    let s = world_at_with(
        search_data(),
        GROUND,
        &[(
            Vec3::new(500.0, 542.0, 100.0),
            Vec3::new(10.0, 1.0, 5.0),
            false,
        )],
    );
    install_empty_hull_loader(&s);
    let (mut client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9201);
    p.data().unit.cooldowns.add_cooldown(RULE_SKILL, 10_000, 0);
    p.data().unit.global_cooldown_ms = 10_000;
    let placement = SummonPlacement {
        x: 500.0,
        y: 530.0,
        z: WATER,
        yaw: std::f32::consts::FRAC_PI_2,
    };
    // The cast-time checks pass: yaw 90 puts the hull's length along X and the pier 12 m
    // north of the centre is outside the 3.25 m beam.
    let world = s.world.world(0).unwrap();
    let cast = validated_summon_placement(&s, &world, &p, &ship_template(), Some(placement), 0.0)
        .expect("the cast placement is valid");
    assert!(summon_placement_clear_1_2(
        &s,
        &world,
        &p,
        &ship_template(),
        &cast
    ));
    turn_player_after_item_validation(&s, &p, std::f32::consts::FRAC_PI_2);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9201, RULE_ITEM, false, None, Some(placement), 0.0),
    )
    .await
    .expect("summon job");
    assert!(result.is_none(), "the turned hull overlaps the pier");
    assert!(s.items.item_by_id(0x9201).is_some(), "the item is not used");
    assert!(get_active_slave_by_owner_obj_id(&s.world.world(0).unwrap(), p.obj_id).is_none());
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(
        codes.contains(&crate::systems::vehicles::error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE),
        "{codes:?}"
    );
    assert!(!p.data().unit.cooldowns.cooldowns.contains_key(&RULE_SKILL));
    assert_eq!(p.data().unit.global_cooldown_ms, 0);
}

/// Without the turn the same `SummonPos` still passes the post-wait full check and spawns.
#[tokio::test]
async fn an_unturned_client_placement_still_spawns_after_the_waits() {
    let s = world_at_with(
        search_data(),
        GROUND,
        &[(
            Vec3::new(500.0, 542.0, 100.0),
            Vec3::new(10.0, 1.0, 5.0),
            false,
        )],
    );
    install_empty_hull_loader(&s);
    let (_client, p) = search_player(&s);
    give_summon_item(&s, &p, 0x9202);
    let placement = SummonPlacement {
        x: 500.0,
        y: 530.0,
        z: WATER,
        yaw: std::f32::consts::FRAC_PI_2,
    };
    let slave = tokio::time::timeout(
        Duration::from_secs(2),
        create_for_owner(&s, &p, 0x9202, RULE_ITEM, false, None, Some(placement), 0.0),
    )
    .await
    .expect("summon job")
    .expect("the clear placement spawns");
    let t = slave.data().unit.transform;
    assert!(
        (t.x - 500.0).abs() < 1e-4 && (t.y - 530.0).abs() < 1e-4,
        "{t:?}"
    );
    assert!(
        (t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
        "{t:?}"
    );
}
