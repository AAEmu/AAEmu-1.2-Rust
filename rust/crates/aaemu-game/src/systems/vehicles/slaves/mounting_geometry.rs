use crate::data::world_templates::WorldSpawnPosition;
use crate::models::transform::PositionAndRotation;
use crate::server::GameServer;
use crate::world::{PlayerRef, SlaveRef, WorldInstance, WorldObject};

fn inverse_rotate(q: crate::models::transform::Quaternion, v: [f32; 3]) -> [f32; 3] {
    crate::models::transform::Quaternion {
        x: -q.x,
        y: -q.y,
        z: -q.z,
        w: q.w,
    }
    .rotate(v)
}

/// Point-to-box surface gap in the slave's local frame (the box is `center ± half`, in metres
/// after `scale`). Used for the ship mass box and the land vehicle's collision bounds.
fn box_gap(
    slave_pos: &PositionAndRotation,
    player: &PositionAndRotation,
    center: [f32; 3],
    half: [f32; 3],
) -> f32 {
    let local = inverse_rotate(
        slave_pos.to_quaternion(),
        [
            player.x - slave_pos.x,
            player.y - slave_pos.y,
            player.z - slave_pos.z,
        ],
    );
    let mut sum = 0.0;
    for i in 0..3 {
        let d = local[i] - center[i];
        let q = d.clamp(-half[i], half[i]);
        sum += (d - q) * (d - q);
    }
    sum.sqrt()
}

/// The per-model bounds cache. A model id is loaded once, including a negative (no geometry)
/// result, so a second bind does not re-read and re-parse the prefab library and a model
/// without geometry does not repeat its failed lookups. Two racing first binds may both load;
/// the value is immutable, so the last insert is equally correct.
pub(crate) fn cached_vehicle_bounds(
    cache: &dashmap::DashMap<u32, Option<aaemu_physics::geometry::primitives::BoxPrim>>,
    model_id: u32,
    load: impl FnOnce() -> Option<aaemu_physics::geometry::primitives::BoxPrim>,
) -> Option<aaemu_physics::geometry::primitives::BoxPrim> {
    if let Some(bounds) = cache.get(&model_id) {
        return *bounds;
    }
    let bounds = load();
    cache.insert(model_id, bounds);
    bounds
}

/// A land vehicle's collision bounds (the union of its prefab body collision proxies, see
/// [`crate::systems::ai::physics::cry::model_bounds_including_bodies`]). `None` when the
/// client assets or the `vehicle_models.normal` reference are unavailable. The model geometry
/// is immutable, so the result is cached per model id.
fn vehicle_model_bounds(
    server: &GameServer,
    model_id: u32,
) -> Option<aaemu_physics::geometry::primitives::BoxPrim> {
    // Only a result read from the model files is cached: the client files are unavailable
    // until the heightmaps load (or while physics is off), and that must not pin `None`.
    let reference = server
        .game_data
        .systems
        .vehicles
        .vehicle_model_refs
        .get(&model_id)?;
    let files = crate::systems::ai::collision::files(server)?;
    cached_vehicle_bounds(&server.systems.vehicles.vehicle_bounds, model_id, || {
        let models = crate::systems::ai::collision::model_cache(server, &files);
        crate::systems::ai::physics::cry::model_bounds_including_bodies(&files, &models, reference)
    })
}

/// Surface gap for a slave bind reach. The client measures the distance from the character's
/// collision primitive to each collision part of the target and keeps the smallest
/// (`FUN_39093860` -> `FUN_390b3ae0` for the target's parts, `FUN_3989bf90` -> `FUN_398b1ab0` for
/// the per-part shape distance). The server measures to the ship model's mass box for ships and
/// to the model's collision bounds for land vehicles (the union of the prefab body's collision
/// proxies; see [`vehicle_model_bounds`]). Only when no geometry exists does it fall back to the
/// requested attach point's world position and the nearest located attach point of the model
/// (`slave_attach_points*.json`). That fallback keeps the smaller of the origin distance and the
/// nearest located point: the origin lies inside the hull, so its distance is an upper bound on
/// the surface gap, and a model whose only located points sit at one end (the Red Farm Hauler's
/// cannon points at the rear) must not tighten the reach at the other. Only a model with no
/// located point at all uses the origin alone (carts: the origin sits at the seat); a ship's
/// bare origin is never the measure.
pub(super) fn bind_gap(
    server: &GameServer,
    world: &WorldInstance,
    character: &PlayerRef,
    slave: &SlaveRef,
    point: u8,
) -> f32 {
    let me = WorldObject::from(character);
    let player = world.world_pos_rot(&me);
    let player_radius = crate::systems::skills::geometry::model_size(
        server,
        &crate::systems::skills::geometry::Target::Obj(me),
    );
    let slave_obj = WorldObject::from(slave);
    let slave_pos = world.world_pos_rot(&slave_obj);
    let (model_id, scale) = {
        let d = slave.data();
        (
            d.unit.model_id,
            if d.unit.scale > 0.0 {
                d.unit.scale
            } else {
                1.0
            },
        )
    };
    let vehicles = &server.game_data.systems.vehicles;
    // Ships keep the mass box. A land vehicle without a ship model measures to its model's
    // collision bounds, which include the prefab's static `AnimObject` body. The ship box and
    // the model box are alternatives, never both.
    let mass_box_distance = vehicles.ship_model(model_id).map(|model| {
        box_gap(
            &slave_pos,
            &player,
            [
                model.mass_center_x * scale,
                model.mass_center_y * scale,
                model.mass_center_z * scale,
            ],
            [
                model.mass_box_size_x.abs() * scale * 0.5,
                model.mass_box_size_y.abs() * scale * 0.5,
                model.mass_box_size_z.abs() * scale * 0.5,
            ],
        )
    });
    let model_box_distance = if mass_box_distance.is_none() {
        vehicle_model_bounds(server, model_id).map(|bounds| {
            box_gap(
                &slave_pos,
                &player,
                [
                    bounds.center.x * scale,
                    bounds.center.y * scale,
                    bounds.center.z * scale,
                ],
                [
                    bounds.size.x * scale,
                    bounds.size.y * scale,
                    bounds.size.z * scale,
                ],
            )
        })
    } else {
        None
    };
    let points = vehicles.attach_points_for_slave(model_id);
    let location_gap = |location: &WorldSpawnPosition| {
        let rotated = slave_pos.to_quaternion().rotate([
            location.x * scale,
            location.y * scale,
            location.z * scale,
        ]);
        let dx = player.x - (slave_pos.x + rotated[0]);
        let dy = player.y - (slave_pos.y + rotated[1]);
        let dz = player.z - (slave_pos.z + rotated[2]);
        (dx * dx + dy * dy + dz * dz).sqrt()
    };
    let point_distance = points
        .and_then(|points| points.iter().find(|(p, _)| *p == point).map(|(_, l)| *l))
        .map(|location| location_gap(&location));
    // The requested seat has no location in the data (for example the Red Farm Hauler, model
    // 1553, declares point 2 but `slave_attach_points.json` only locates its cannon points).
    // Measure to the nearest located point of the same model instead of the bare origin, as the
    // client measures to the nearest collision part of the target.
    let nearest_point_distance = points
        .into_iter()
        .flatten()
        .map(|(_, location)| location_gap(location))
        .min_by(f32::total_cmp);
    let distance = match (mass_box_distance, model_box_distance, point_distance) {
        // A ship keeps the mass box; the requested attach point can sit outside it.
        (Some(mass_gap), _, Some(point_gap)) => mass_gap.min(point_gap),
        (Some(gap), _, None) => gap,
        // A land vehicle with collision geometry measures to the body only.
        (None, Some(gap), _) => gap,
        (None, None, Some(gap)) => gap,
        (None, None, None) => {
            let dx = player.x - slave_pos.x;
            let dy = player.y - slave_pos.y;
            let dz = player.z - slave_pos.z;
            let origin_gap = (dx * dx + dy * dy + dz * dz).sqrt();
            nearest_point_distance.map_or(origin_gap, |point_gap| point_gap.min(origin_gap))
        }
    };
    (distance - player_radius).max(0.0)
}
