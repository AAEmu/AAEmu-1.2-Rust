use crate::models::transform::Transform;
use crate::server::GameServer;
use crate::systems::ai::collision::world::Owner;
use crate::systems::skills::types::{SkillCastTarget, SkillCaster};
use crate::world::{PlayerRef, WorldInstance, WorldObject};

use super::data::{ShipModel, SlaveTemplate};
use super::error;
use super::summon::owned_summon_item;

/// The floor under a summon candidate: the collision-world ground probe when it hits, the
/// terrain elevation otherwise. A probe miss (`Some(None)`) means "no static collider here",
/// not bottomless water, so the terrain (or the heightmap fallback) still applies.
pub(super) fn summon_floor(probe: Option<Option<f32>>, terrain: impl FnOnce() -> f32) -> f32 {
    match probe {
        Some(Some(h)) => h,
        _ => terrain(),
    }
}

/// The hull footprint of the summon probes at `scale` (the summon passes 1.0: a freshly created
/// slave's unit scale): the authored collision bounds the CryPhysics hull uses when available,
/// else the inertia mass box. `length` is the asymmetric (stern, bow) extent along the hull's
/// local length axis, in metres; the mass box's `size_y` is the hull length and `size_x` its
/// beam.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HullFootprint {
    /// Half extent across the hull (local X), metres.
    pub half_beam: f32,
    /// (stern, bow) extents along the hull's local length axis (local Y), metres.
    pub length: (f32, f32),
}

impl HullFootprint {
    /// The longitudinal midpoint of the footprint (the authored centre is not necessarily 0).
    pub(crate) fn mid_length(&self) -> f32 {
        (self.length.0 + self.length.1) * 0.5
    }
}

pub(crate) fn hull_footprint(
    bounds: Option<&aaemu_physics::geometry::primitives::BoxPrim>,
    mass_box_size_x: f32,
    mass_box_size_y: f32,
    scale: f32,
) -> HullFootprint {
    let s = scale.max(0.01);
    match bounds {
        Some(b) => HullFootprint {
            half_beam: b.size.x * s,
            length: ((b.center.y - b.size.y) * s, (b.center.y + b.size.y) * s),
        },
        None => HullFootprint {
            half_beam: mass_box_size_x * 0.5 * s,
            length: (-mass_box_size_y * 0.5 * s, mass_box_size_y * 0.5 * s),
        },
    }
}

/// The XY points the summon probes check for a hull centred at (`x`, `y`) and lying along `yaw`
/// (the FINAL ship yaw, not the player's facing): three stations along the length (stern,
/// midpoint, bow) at the centreline and both beam edges, so the middle of the long sides is
/// covered too. `yaw` rotates the local length axis to the world.
pub(crate) fn hull_probe_points(
    x: f32,
    y: f32,
    yaw: f32,
    footprint: HullFootprint,
) -> [(f32, f32); 9] {
    let (fx, fy) = (-yaw.sin(), yaw.cos());
    let (rx, ry) = (yaw.cos(), yaw.sin());
    let mut points = [(x, y); 9];
    for (station, l) in [
        footprint.length.0,
        footprint.mid_length(),
        footprint.length.1,
    ]
    .into_iter()
    .enumerate()
    {
        let (ox, oy) = (x + fx * l, y + fy * l);
        points[station * 3] = (ox, oy);
        points[station * 3 + 1] = (ox - rx * footprint.half_beam, oy - ry * footprint.half_beam);
        points[station * 3 + 2] = (ox + rx * footprint.half_beam, oy + ry * footprint.half_beam);
    }
    points
}

/// Extra room above the hull top required by the overhead clearance probe (metres).
const OVERHEAD_CLEARANCE_MARGIN: f32 = 1.0;
/// The overhead probe stops just above the water surface, so only structures above it count.
const OVERHEAD_SURFACE_MARGIN: f32 = 0.05;

/// The vertical hull envelope tested for overlap with static solids: the footprint plus the
/// keel bottom and the hull top (plus the overhead margin), relative to the ship's waterline
/// spawn z (the hull's z offset is already folded in).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HullEnvelope {
    pub footprint: HullFootprint,
    /// Keel bottom relative to the waterline, metres.
    pub bottom: f32,
    /// Hull top + [`OVERHEAD_CLEARANCE_MARGIN`] relative to the waterline, metres.
    pub top: f32,
}

impl HullEnvelope {
    /// The envelope the solid CryPhysics hull occupies when it is placed on the water at its
    /// spawn z: the clipped authored collision bounds, else the inertia mass box, at scale 1.0.
    pub(crate) fn from_hull(
        bounds: Option<&aaemu_physics::geometry::primitives::BoxPrim>,
        ship: &ShipModel,
        offset_z: f32,
    ) -> Self {
        let footprint = hull_footprint(bounds, ship.mass_box_size_x, ship.mass_box_size_y, 1.0);
        let (bottom, top) = match bounds {
            Some(b) => (b.center.z - b.size.z, b.center.z + b.size.z),
            None => {
                let half = ship.mass_box_size_z.max(0.01) * 0.5;
                (ship.mass_center_z - half, ship.mass_center_z + half)
            }
        };
        let bottom = offset_z + bottom;
        let top = (offset_z + top).max(0.5).max(bottom + 0.5);
        Self {
            footprint,
            bottom,
            top: top + OVERHEAD_CLEARANCE_MARGIN,
        }
    }

    /// The hull top without the margin, the start height of the overhead ray.
    pub(super) fn hull_top(&self) -> f32 {
        self.top - OVERHEAD_CLEARANCE_MARGIN
    }
}

/// A static collider must have a world AABB extent of at least this many metres to block a
/// summoned hull. Buoys, bollards and small mooring posts are below it; piers, quays, decks,
/// bridges and rocks are far above. Vegetation is ignored regardless of size.
const HULL_OBSTACLE_MIN_SIZE: f32 = 2.0;

/// Whether a static collider can reject a hull summon spot: terrain always can; vegetation
/// never (reeds, kelp and trees are not hull obstacles); any other static only when its world
/// AABB is at least [`HULL_OBSTACLE_MIN_SIZE`] metres across in some axis.
fn hull_obstacle(owner: Owner, bounds: [aaemu_physics::math::Vec3; 2]) -> bool {
    if matches!(owner, Owner::Vegetation { .. }) {
        return false;
    }
    if matches!(owner, Owner::Terrain) {
        return true;
    }
    let d = bounds[1] - bounds[0];
    d.x.max(d.y).max(d.z) >= HULL_OBSTACLE_MIN_SIZE
}

/// True when `p` lies inside a hull-blocking static solid: a downward ray that reports exit
/// faces hits a surface with a downward normal (the ray leaves the solid through its bottom).
/// A deck or bridge above the point is hit from outside first (upward normal) and does not
/// count; vegetation and tiny statics are ignored like in the box overlap.
fn point_inside_static(server: &GameServer, instance_id: u32, p: [f32; 3]) -> bool {
    let hit = crate::systems::ai::collision::raycast(
        server,
        instance_id,
        p,
        [p[0], p[1], p[2] - 1024.0],
        crate::systems::ai::collision::query::mask::STATIC,
        crate::systems::ai::collision::query::ray_flags::WITH_BACK_FACES,
    );
    let Some(Some(hit)) = hit else {
        return false;
    };
    if hit.normal.z >= 0.0 {
        return false;
    }
    match crate::systems::ai::collision::entity_owner_bounds(server, instance_id, hit.ent) {
        Some((owner, bounds)) => hull_obstacle(owner, bounds),
        None => false,
    }
}

/// The terrain part of the hull-envelope overlap: the collision heightfield must stay below
/// the keel at the candidate centre and the nine hull points. The PWI box intersect does not
/// report the heightfield entity, so this samples the collision terrain directly (the same
/// terrain-in-the-envelope rule as the `STATIC|TERRAIN` box mask intent).
fn hull_terrain_clear(
    server: &GameServer,
    instance_id: u32,
    x: f32,
    y: f32,
    hull_z: f32,
    yaw: f32,
    envelope: HullEnvelope,
) -> bool {
    let keel = hull_z + envelope.bottom;
    hull_probe_points(x, y, yaw, envelope.footprint)
        .into_iter()
        .chain(std::iter::once((x, y)))
        .all(|(px, py)| {
            crate::systems::ai::collision::terrain_height_or(server, instance_id, px, py, || {
                f32::NEG_INFINITY
            }) <= keel
        })
}

/// True when the hull envelope at (`x`, `y`, `hull_z`) and the final yaw does not intersect a
/// static collider. The box spans the keel to just above the hull top, so a thick deck or a
/// solid quay that a downward ray misses (the ray starts inside it and ignores exit faces)
/// still rejects the spot. The terrain heightfield is sampled separately at the same points (a
/// harbour platform above the water must reject the spot even when its top is above the depth
/// probe start), while the depth probes still keep legitimate keels above the seabed. The
/// box-box test emits only clipped edge points, so a solid that fully contains the box reports
/// no contact; the same nine hull grid points are then checked for point containment.
pub(super) fn hull_clear_at(
    server: &GameServer,
    instance_id: u32,
    x: f32,
    y: f32,
    hull_z: f32,
    yaw: f32,
    envelope: HullEnvelope,
) -> bool {
    let (fx, fy) = (-yaw.sin(), yaw.cos());
    let fp = envelope.footprint;
    let mid = fp.mid_length();
    let center = [
        x + fx * mid,
        y + fy * mid,
        hull_z + (envelope.bottom + envelope.top) * 0.5,
    ];
    let half = [
        fp.half_beam,
        (fp.length.1 - fp.length.0) * 0.5,
        (envelope.top - envelope.bottom) * 0.5,
    ];
    let obstacles = crate::systems::ai::collision::box_obstacles(
        server,
        instance_id,
        center,
        half,
        yaw,
        crate::systems::ai::collision::query::mask::STATIC_TERRAIN,
    );
    match obstacles {
        // No collision service: the depth probes and overhead rays already passed.
        None => true,
        Some(list)
            if list
                .iter()
                .any(|(owner, bounds)| hull_obstacle(*owner, *bounds)) =>
        {
            false
        }
        Some(_) => {
            hull_terrain_clear(server, instance_id, x, y, hull_z, yaw, envelope)
                && !hull_probe_points(x, y, yaw, fp)
                    .into_iter()
                    .any(|(px, py)| point_inside_static(server, instance_id, [px, py, center[2]]))
        }
    }
}

/// The overhead clearance decision for one probe point: a downward ray from above the hull to
/// just above the water surface blocks the spot when it hits a collider above the surface
/// (a pier, bridge or deck). A miss, or no collision service, is clear.
pub(super) fn overhead_clear(hit: Option<Option<f32>>, water: f32) -> bool {
    !matches!(hit, Some(Some(z)) if z > water + OVERHEAD_SURFACE_MARGIN)
}

/// [`overhead_clear`] with the production rays between `water + hull_top + margin` and just
/// above the water at (`x`, `y`), against static, rigid and terrain colliders. The downward ray
/// finds the top of a pier, bridge or deck; a ray does not hit the solid it starts in, so the
/// upward ray finds the underside of a deck thick enough to contain the downward ray's start.
pub(crate) fn overhead_clear_at(
    server: &GameServer,
    instance_id: u32,
    x: f32,
    y: f32,
    water: f32,
    hull_top: f32,
) -> bool {
    let low = [x, y, water + OVERHEAD_SURFACE_MARGIN];
    let high = [x, y, water + hull_top + OVERHEAD_CLEARANCE_MARGIN];
    let ray = |from: [f32; 3], to: [f32; 3]| {
        crate::systems::ai::collision::raycast(
            server,
            instance_id,
            from,
            to,
            crate::systems::ai::collision::query::mask::SOLID_TERRAIN,
            crate::systems::ai::collision::query::ray_flags::DEFAULT,
        )
        .map(|hit| hit.map(|h| h.pos.z))
    };
    overhead_clear(ray(high, low), water) && overhead_clear(ray(low, high), water)
}

/// The `!deep` summon fallback: 5 m in front of the owner along its facing, at the water
/// surface there (`water_at` is queried at the returned x/y). Never the owner's height: a pier
/// or deck z would leave the hull floating in the air.
pub(crate) fn fallback_summon_point(
    owner_x: f32,
    owner_y: f32,
    owner_yaw: f32,
    water_at: impl FnOnce(f32, f32) -> f32,
) -> (f32, f32, f32) {
    let x = owner_x - 5.0 * owner_yaw.sin();
    let y = owner_y + 5.0 * owner_yaw.cos();
    (x, y, water_at(x, y))
}

/// The authored hull bounds `physics::add_ship` gives the CryPhysics hull: the prefab collision
/// proxies clipped to the hull envelope. `None` when the physics uses the mass box instead
/// (CryPhysics ships off, or the client assets/model reference are unavailable).
/// The unclipped model bbox (the union of the prefab collision proxies): the 1.2 client's
/// summon box. The Rust hull collider clips the z range ([`summon_hull_bounds`]); the client's
/// box keeps the whole model (masts included).
fn summon_model_bounds(
    server: &GameServer,
    model_id: u32,
) -> Option<aaemu_physics::geometry::primitives::BoxPrim> {
    if !server.config.physics.cryphysics_ships {
        return None;
    }
    let ai = &server.game_data.systems.ai.ai;
    ai.ship_model(model_id)?;
    let reference = ai.ship_model_refs.get(&model_id)?;
    let files = crate::systems::ai::collision::files(server)?;
    let models = crate::systems::ai::collision::model_cache(server, &files);
    crate::systems::ai::physics::cry::model_collision_bounds(&files, &models, reference)
}

/// The clipped hull collider bounds (the 1.2 client's hull envelope, masts trimmed).
pub(super) fn summon_hull_bounds(
    server: &GameServer,
    model_id: u32,
) -> Option<aaemu_physics::geometry::primitives::BoxPrim> {
    let bounds = summon_model_bounds(server, model_id)?;
    let model = *server.game_data.systems.ai.ai.ship_model(model_id)?;
    Some(crate::systems::ai::physics::cry::clip_hull_bounds(
        bounds, &model,
    ))
}

/// The client-chosen `SummonPos` turned into a spawn transform, or `None` (with
/// `InvalidLocation`) when the spot is too far, on the wrong surface or inside
/// collision. `None` also means "no placement": the C# in-front placement is used.
pub(crate) fn validated_summon_placement(
    server: &GameServer,
    world: &WorldInstance,
    owner: &PlayerRef,
    template: &SlaveTemplate,
    placement: Option<crate::systems::skills::ext::SummonPlacement>,
    max_range: f32,
) -> Option<Transform> {
    let p = placement?;
    let invalid = |reason: &str| {
        tracing::warn!(
            "SlaveSpawn placement for template {} refused: {reason} at ({:.1},{:.1},{:.1})",
            template.id,
            p.x,
            p.y,
            p.z
        );
        error(owner, error::INVALID_SUMMON_LOCATION);
        None
    };
    let owner_pos = world.world_pos_rot(&WorldObject::from(owner));
    if [p.x, p.y, p.z, p.yaw].iter().any(|v| !v.is_finite()) {
        return invalid("non-finite coordinates");
    }
    // The final ship pose: the 1.2 client search places ships 90 degrees CCW of the player and
    // these coordinates are checked with, and spawned at, that same pose. The placement's own
    // `yaw` (the client's `PosRot`) is not used: the post-wait recheck re-derives the pose from
    // the owner's current facing and validates exactly the transform that is spawned, so a
    // stale client heading cannot skip the check.
    let final_yaw = owner_pos.rot_z + std::f32::consts::FRAC_PI_2;
    // `slaves.spawn_valid_area_range` is the data-side limit (50 m in the 1.2 DB); the skill's
    // own MaxRange is 0 for the boat summon (15802). Use the smaller nonzero limit.
    let limit = [max_range, template.spawn_valid_area_range as f32]
        .into_iter()
        .filter(|v| *v > 0.0)
        .fold(f32::INFINITY, f32::min);
    let limit = if limit.is_finite() { limit } else { 50.0 };
    let dx = p.x - owner_pos.x;
    let dy = p.y - owner_pos.y;
    if dx * dx + dy * dy > limit * limit {
        return invalid("too far from the owner");
    }
    let mut transform = Transform {
        instance_id: world.id,
        zone_id: owner.data().unit.transform.zone_id,
        ..Default::default()
    };
    let surface = if template.is_a_boat() {
        if !super::ship_physics::is_summon_water(server, world, [p.x, p.y, p.z]) {
            return invalid("a ship must be summoned on water");
        }
        let surface = super::ship_physics::water_surface(server, world, [p.x, p.y, p.z]);
        // The hull spawns on this surface; it must be the actual water, not a dry fallback.
        if !super::ship_physics::is_water(server, world, [p.x, p.y, surface]) {
            return invalid("a ship must be summoned on water");
        }
        surface
    } else {
        // The 1.2 land rule has no static ground: the spot sits on the terrain heightfield,
        // and a deck above it is refused later by the line-of-sight ray, not used as the floor.
        let terrain = summon_terrain(server, world, p.x, p.y);
        // Test the surface the vehicle is placed on, not the client's z: a client z above sea
        // level must not hide a sea-floor spot.
        if super::ship_physics::is_water(server, world, [p.x, p.y, terrain + 0.1]) {
            return invalid("a land vehicle must be summoned on ground");
        }
        terrain
    };
    // The spawn height already carries the ship's waterline offset, so the 1.2 check runs on
    // exactly the transform `create_spawn_committed` places.
    let offset_z = if template.is_a_boat() {
        summon_ship_hull(server, template).map_or(0.0, |(_, z, _)| z)
    } else {
        0.0
    };
    transform.set_position(p.x, p.y, surface + offset_z, 0.0, 0.0, final_yaw);
    // A half-metre sphere just above the surface: the hull/aabb is not needed, this only
    // rejects a spot inside a rock, cliff or doodad collider.
    let inside = crate::systems::ai::collision::overlaps_sphere(
        server,
        world.id,
        [transform.x, transform.y, surface + 0.8],
        0.5,
        crate::systems::ai::collision::query::mask::SOLID_TERRAIN
            & !crate::systems::ai::collision::query::mask::TERRAIN,
    )
    .unwrap_or(false);
    if inside {
        return invalid("inside collision");
    }
    Some(transform)
}

/// Turn `CSSpawnSlave` (0x02e) into the item-use cast the 1.2 client sends for a summon item:
/// the packet's item must be the sender's inventory `SummonSlave` item for `slave_id`, and the
/// cast is the item's own `use_skill_id` with a `SummonPos` target at the client's position.
/// `start_skill` then runs the normal item-cast checks (ownership, zone, actability, stun /
/// death / swim, cast time, cooldown, reagents); the `SpawnSlave` effect validates the placement
/// with [`validated_client_spawn`]. The internal `SkillObject::Unk5(hideSpawnEffect)` marker
/// tells the effect this placement came from the 0x02e packet.
#[allow(clippy::too_many_arguments)]
pub fn client_spawn_cast(
    server: &GameServer,
    owner: &PlayerRef,
    slave_id: u32,
    x: f32,
    y: f32,
    z: f32,
    z_rot: f32,
    item_id: u64,
    hide_spawn_effect: bool,
) -> Option<crate::systems::skills::handlers::CSStartSkillPacket> {
    if item_id == 0 {
        // The zero-item body has no evidenced sender; GM tools use their own spawn paths.
        tracing::warn!("SpawnSlave: itemId 0 for slave {slave_id}; no item summon, ignored");
        return None;
    }
    let found = super::mates::owned_item(server, owner, item_id)?;
    let item_template_id = found.template_id;
    // Exactly the item-summon ownership check (owner, inventory, class, template).
    let Some(item) = owned_summon_item(server, owner, item_id, item_template_id) else {
        tracing::warn!(
            "SpawnSlave: item {item_id} is not {}'s inventory slave summon item",
            owner.data().unit.name
        );
        return None;
    };
    let item_slave_id = match server
        .game_data
        .items
        .template(item.template_id)
        .map(|t| &t.kind)
    {
        Some(crate::data::items::TemplateKind::SummonSlave { slave_id }) => *slave_id,
        _ => {
            tracing::warn!("SpawnSlave: item {item_id} is not a slave summon item");
            return None;
        }
    };
    if item_slave_id != slave_id {
        tracing::warn!(
            "SpawnSlave: item {item_id} summons slave {item_slave_id}, not the requested {slave_id}"
        );
        return None;
    }
    let use_skill_id = server
        .game_data
        .items
        .template(item.template_id)
        .map_or(0, |t| t.use_skill_id);
    if use_skill_id == 0 {
        tracing::warn!("SpawnSlave: item {item_id} has no use skill");
        return None;
    }
    Some(crate::systems::skills::handlers::CSStartSkillPacket {
        skill_id: use_skill_id,
        caster: SkillCaster::Item {
            obj_id: owner.obj_id,
            item_id: item.id,
            item_template_id: item.template_id,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Position {
            obj_id: 0,
            x,
            y,
            z,
            rot: z_rot,
            obj_id1: 0,
            obj_id2: 0,
        },
        flag: 0,
        skill_object: crate::systems::skills::types::SkillObject::Unk5(i32::from(
            hide_spawn_effect,
        )),
    })
}

/// The `CSSpawnSlave` position check: the same distance / surface / collision rules as
/// [`validated_summon_placement`], keeping the client's yaw, plus the 1.2 land height rule.
/// A spot the basic rules refuse gets `InvalidLocation` (215); a spot the full 1.2
/// placement check refuses (line of sight / hull box) gets the placement path's
/// `SLAVE_SPAWN_SHIP_NEED_MORE_SPACE` (100).
#[allow(clippy::too_many_arguments)]
pub(super) fn validated_client_spawn(
    server: &GameServer,
    world: &WorldInstance,
    owner: &PlayerRef,
    template: &SlaveTemplate,
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
) -> Option<Transform> {
    let invalid = |reason: &str| {
        tracing::warn!(
            "SpawnSlave placement for template {} refused: {reason} at ({:.1},{:.1},{:.1})",
            template.id,
            x,
            y,
            z
        );
        error(owner, error::INVALID_SUMMON_LOCATION);
        None
    };
    if [x, y, z, yaw].iter().any(|v| !v.is_finite()) {
        return invalid("non-finite coordinates");
    }
    let owner_pos = world.world_pos_rot(&WorldObject::from(owner));
    // `slaves.spawn_valid_area_range` is the data-side limit (50 m in the 1.2 DB).
    let limit = template.spawn_valid_area_range as f32;
    let limit = if limit > 0.0 { limit } else { 50.0 };
    let dx = x - owner_pos.x;
    let dy = y - owner_pos.y;
    if dx * dx + dy * dy > limit * limit {
        return invalid("too far from the owner");
    }
    let mut transform = Transform {
        instance_id: world.id,
        zone_id: owner.data().unit.transform.zone_id,
        ..Default::default()
    };
    let surface = if template.is_a_boat() {
        if !super::ship_physics::is_summon_water(server, world, [x, y, z]) {
            return invalid("a ship must be summoned on water");
        }
        let surface = super::ship_physics::water_surface(server, world, [x, y, z]);
        if !super::ship_physics::is_water(server, world, [x, y, surface]) {
            return invalid("a ship must be summoned on water");
        }
        surface
    } else {
        let terrain = summon_terrain(server, world, x, y);
        // Test the surface the vehicle is placed on, not the client's z.
        if super::ship_physics::is_water(server, world, [x, y, terrain + 0.1]) {
            return invalid("a land vehicle must be summoned on ground");
        }
        if terrain - owner_pos.z > SUMMON_MAX_TERRAIN_ABOVE_OWNER {
            return invalid("terrain too far above the owner");
        }
        terrain
    };
    let offset_z = if template.is_a_boat() {
        summon_ship_hull(server, template).map_or(0.0, |(_, z, _)| z)
    } else {
        0.0
    };
    transform.set_position(x, y, surface + offset_z, 0.0, 0.0, yaw);
    // A half-metre sphere just above the surface, exactly like the `SummonPos` placement.
    let inside = crate::systems::ai::collision::overlaps_sphere(
        server,
        world.id,
        [transform.x, transform.y, surface + 0.8],
        0.5,
        crate::systems::ai::collision::query::mask::SOLID_TERRAIN
            & !crate::systems::ai::collision::query::mask::TERRAIN,
    )
    .unwrap_or(false);
    if inside {
        return invalid("inside collision");
    }
    if crate::systems::ai::collision::files(server).is_some()
        && !summon_placement_clear_1_2(server, world, owner, template, &transform)
    {
        tracing::warn!(
            "SpawnSlave placement for template {} refused: the 1.2 placement check failed at ({:.1},{:.1},{:.1})",
            template.id,
            transform.x,
            transform.y,
            transform.z
        );
        error(owner, error::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE);
        return None;
    }
    Some(transform)
}

/// Validate a spawn placement with the item-cast rules (distance, surface, collision, 1.2
/// placement check): `client` keeps the client yaw and applies the 10 m land rule
/// ([`validated_client_spawn`]); otherwise the C#/1.2 `SummonPos` yaw is the owner's + 90°.
#[allow(clippy::too_many_arguments)]
pub(super) fn validate_summon_placement(
    server: &GameServer,
    world: &WorldInstance,
    owner: &PlayerRef,
    template: &SlaveTemplate,
    placement: Option<crate::systems::skills::ext::SummonPlacement>,
    max_range: f32,
    client: bool,
) -> Option<Transform> {
    let p = placement?;
    if client {
        validated_client_spawn(server, world, owner, template, p.x, p.y, p.z, p.yaw)
    } else {
        validated_summon_placement(server, world, owner, template, Some(p), max_range)
    }
}

/// The hull's vertical offset from the waterline at creation:
/// `(mass_center_z < 0 ? mass_center_z / 2 : 0) - keel_height`.
pub(super) fn summon_offset_z(ship: &ShipModel) -> f32 {
    (if ship.mass_center_z < 0.0 {
        ship.mass_center_z / 2.0
    } else {
        0.0
    }) - ship.keel_height
}

/// The 1.2 client's summon candidates in order: 40 forward steps
/// (`spawn_x_offset + i`, i = 0..39) with the lateral offsets `{0, +spawn_y_offset,
/// -spawn_y_offset}` (120 points). `yaw` is the player's facing.
pub(crate) fn summon_candidate_points(
    x: f32,
    y: f32,
    yaw: f32,
    spawn_x_offset: f32,
    spawn_y_offset: f32,
) -> Vec<(f32, f32)> {
    let (fx, fy) = (-yaw.sin(), yaw.cos());
    let (rx, ry) = (yaw.cos(), yaw.sin());
    let mut out = Vec::with_capacity(120);
    for i in 0..40 {
        let forward = spawn_x_offset + i as f32;
        for lateral in [0.0, spawn_y_offset, -spawn_y_offset] {
            out.push((
                x + fx * forward + rx * lateral,
                y + fy * forward + ry * lateral,
            ));
        }
    }
    out
}

/// The land summon candidate height: `I3DEngine::GetTerrainElevation(x, y, 0)` (vtable
/// `+0x1fc`), i.e. the terrain heightfield only, never static geometry (`FUN_39347fb0`). The
/// heightmap is the fallback when the collision service has no terrain. A deck, bridge or quay
/// above the terrain is not a landing spot for the 1.2 rule; it blocks the line of sight instead.
pub(super) fn summon_terrain(server: &GameServer, world: &WorldInstance, x: f32, y: f32) -> f32 {
    crate::systems::ai::collision::terrain_height_or(server, world.id, x, y, || {
        crate::systems::npcs::heightmap::world_height(server, world, x, y).unwrap_or(0.0)
    })
}

/// How far above the candidate's terrain point a land summon line-of-sight ray ends. 1.2 ends
/// the ray exactly at the point; the 0.2 m lift is a deviation that avoids a float graze
/// between the ray end and the heightfield (the Bevy client port lifts its end the same way).
const SUMMON_LOS_END_LIFT: f32 = 0.2;

/// The 1.2 land summon rule (`FUN_39347fb0`): the candidate terrain may be at most this far
/// above the player (`terrain - player_z <= 10`).
pub(super) const SUMMON_MAX_TERRAIN_ABOVE_OWNER: f32 = 10.0;

/// The 1.2 line-of-sight rule: a ray from the player's eyes to the candidate hits nothing
/// (static + terrain, rays 0x8F). No collision service counts as clear.
fn summon_los_clear(server: &GameServer, instance_id: u32, from: [f32; 3], to: [f32; 3]) -> bool {
    !matches!(
        crate::systems::ai::collision::raycast(
            server,
            instance_id,
            from,
            to,
            crate::systems::ai::collision::query::mask::SOLID_TERRAIN,
            crate::systems::ai::collision::query::ray_flags::DEFAULT,
        ),
        Some(Some(_))
    )
}

/// The ship model, its waterline z offset and the hull envelope of a boat template.
fn summon_ship_hull(
    server: &GameServer,
    template: &SlaveTemplate,
) -> Option<(ShipModel, f32, HullEnvelope)> {
    if !template.is_a_boat() {
        return None;
    }
    let ship = server
        .game_data
        .systems
        .vehicles
        .ship_model(template.model_id)
        .copied()?;
    let offset_z = summon_offset_z(&ship);
    let bounds = summon_hull_bounds(server, template.model_id);
    let envelope = HullEnvelope::from_hull(bounds.as_ref(), &ship, offset_z);
    Some((ship, offset_z, envelope))
}

/// The 1.2 model box: the unclipped model bbox when the client assets provide it, else the
/// inertia mass box of the ship model (the fallback hull CryPhysics builds when the authored
/// model bounds are unavailable). `None` only when there is no collision world to test
/// against, or no ship model to build the box from (land slaves).
pub(super) fn summon_model_box(
    server: &GameServer,
    template: &SlaveTemplate,
) -> Option<aaemu_physics::geometry::primitives::BoxPrim> {
    if let Some(bounds) = summon_model_bounds(server, template.model_id) {
        return Some(bounds);
    }
    crate::systems::ai::collision::files(server)?;
    summon_mass_box(server, template)
}

/// The inertia mass box of a boat's `ShipModel`, in game/model axes (`size` are half extents).
fn summon_mass_box(
    server: &GameServer,
    template: &SlaveTemplate,
) -> Option<aaemu_physics::geometry::primitives::BoxPrim> {
    if !template.is_a_boat() {
        return None;
    }
    let ship = server
        .game_data
        .systems
        .ai
        .ai
        .ship_model(template.model_id)?;
    Some(aaemu_physics::geometry::primitives::BoxPrim::aligned(
        aaemu_physics::math::Vec3::new(ship.mass_center_x, ship.mass_center_y, ship.mass_center_z),
        aaemu_physics::math::Vec3::new(
            ship.mass_box_size_x.max(0.01) * 0.5,
            ship.mass_box_size_y.max(0.01) * 0.5,
            ship.mass_box_size_z.max(0.01) * 0.5,
        ),
    ))
}

/// The 1.2 player-summon model-box check at (`x`, `y`, `hull_z`) and the final yaw: the
/// oriented model bbox against statics (small/vegetation filtered) and the collision terrain
/// heightfield. No size margin, terrain point sampling or containment rays; those remain in
/// [`hull_clear_at`] for the GM/spawner path.
#[allow(clippy::too_many_arguments)]
pub(super) fn model_box_clear_at(
    server: &GameServer,
    instance_id: u32,
    x: f32,
    y: f32,
    hull_z: f32,
    yaw: f32,
    bounds: &aaemu_physics::geometry::primitives::BoxPrim,
    offset_z: f32,
) -> bool {
    let (fx, fy) = (-yaw.sin(), yaw.cos());
    let (rx, ry) = (yaw.cos(), yaw.sin());
    let center = [
        x + rx * bounds.center.x + fx * bounds.center.y,
        y + ry * bounds.center.x + fy * bounds.center.y,
        hull_z + offset_z + bounds.center.z,
    ];
    let half = [bounds.size.x, bounds.size.y, bounds.size.z];
    if let Some(list) = crate::systems::ai::collision::box_obstacles(
        server,
        instance_id,
        center,
        half,
        yaw,
        crate::systems::ai::collision::query::mask::STATIC,
    ) {
        if list
            .iter()
            .any(|(owner, entity_bounds)| hull_obstacle(*owner, *entity_bounds))
        {
            return false;
        }
    }
    !matches!(
        crate::systems::ai::collision::box_overlaps_terrain(server, instance_id, center, half, yaw,),
        Some(true)
    )
}

/// The 1.2 client summon search (`FUN_39347fb0`): the first candidate that passes all four
/// checks wins. The candidate z is the water surface for ships (terrain must not be above it)
/// and the terrain for land slaves (`GetTerrainElevation`, vtable `+0x1fc`; the terrain is at
/// most 10 m above the player). The model box is the unclipped
/// model bbox rotated by the player yaw plus 90 degrees; the distance cap is 80 m. Returns the
/// final spawn transform, or `None` when none of the 120 candidates passes. Debug logs the
/// per-check rejection counts; the separate `Physics:SummonDiagnostics` option keeps scanning
/// after a valid spot to add the old `hull_clear_at` box count for comparison.
pub(crate) fn summon_candidate_1_2(
    server: &GameServer,
    world: &WorldInstance,
    owner: &PlayerRef,
    template: &SlaveTemplate,
) -> Option<Transform> {
    let me = world.world_pos_rot(&WorldObject::from(owner));
    let final_yaw = me.rot_z + std::f32::consts::PI / 2.0;
    let is_boat = template.is_a_boat();
    let ship_hull = summon_ship_hull(server, template);
    let offset_z = ship_hull.as_ref().map_or(0.0, |(_, z, _)| *z);
    let envelope = ship_hull.as_ref().map(|(_, _, e)| *e);
    let model_box = summon_model_box(server, template);
    let debug = tracing::enabled!(tracing::Level::DEBUG);
    let compare = server.config.physics.summon_diagnostics;
    let (mut terrain_rej, mut distance_rej, mut los_rej, mut old_box_rej, mut new_box_rej) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut chosen = None;
    let max_d2 = 80.0 * 80.0;
    for (x, y) in summon_candidate_points(
        me.x,
        me.y,
        me.rot_z,
        template.spawn_x_offset,
        template.spawn_y_offset,
    ) {
        let terrain = summon_terrain(server, world, x, y);
        let water = if is_boat {
            Some(super::ship_physics::water_surface(
                server,
                world,
                [x, y, me.z],
            ))
        } else {
            None
        };
        let z = water.unwrap_or(terrain);
        let terrain_ok = water.map_or(terrain - me.z <= SUMMON_MAX_TERRAIN_ABOVE_OWNER, |water| {
            terrain <= water
        });
        if !terrain_ok {
            terrain_rej += 1;
        }
        let (dx, dy, dz) = (x - me.x, y - me.y, z - me.z);
        let distance_ok = dx * dx + dy * dy + dz * dz <= max_d2;
        if !distance_ok {
            distance_rej += 1;
        }
        // Ships are seen against the water surface. Land rays end at the candidate's terrain
        // point (lifted by [`SUMMON_LOS_END_LIFT`]): a deck between the eyes and the terrain is
        // hit and refuses the spot, and an uphill terrain point is not under the player's feet.
        let los_to = if is_boat {
            [x, y, z]
        } else {
            [x, y, z + SUMMON_LOS_END_LIFT]
        };
        let los_ok = summon_los_clear(server, world.id, [me.x, me.y, me.z + 1.0], los_to);
        if !los_ok {
            los_rej += 1;
        }
        let prior_ok = terrain_ok && distance_ok && los_ok;
        let new_box_ok = if prior_ok {
            match &model_box {
                Some(bounds) => {
                    model_box_clear_at(server, world.id, x, y, z, final_yaw, bounds, offset_z)
                }
                None => true,
            }
        } else {
            true
        };
        if prior_ok && !new_box_ok {
            new_box_rej += 1;
        }
        if compare {
            if let Some(envelope) = envelope {
                if !hull_clear_at(server, world.id, x, y, z, final_yaw, envelope) {
                    old_box_rej += 1;
                }
            }
        }
        if chosen.is_none() && prior_ok && new_box_ok {
            let mut transform = Transform {
                instance_id: world.id,
                zone_id: owner.data().unit.transform.zone_id,
                ..Default::default()
            };
            transform.set_position(x, y, z + offset_z, 0.0, 0.0, final_yaw);
            chosen = Some(transform);
            if !compare {
                break;
            }
        }
    }
    if debug || compare {
        tracing::debug!(
            "Slave {} summon search: terrain {terrain_rej} dist {distance_rej} los {los_rej} old_box {old_box_rej} new_box {new_box_rej} of 120 chosen {}",
            template.id,
            chosen.is_some()
        );
    }
    chosen
}

/// The same 1.2 checks for a chosen spawn transform (the client's `SummonPos` or the search's
/// candidate): the final pose the slave is spawned at, with the ship waterline offset already
/// folded into `transform.z` and the final yaw in `transform.rot_z`
/// ([`validated_summon_placement`] and [`summon_candidate_1_2`] both produce it).
pub(super) fn summon_placement_clear_1_2(
    server: &GameServer,
    world: &WorldInstance,
    owner: &PlayerRef,
    template: &SlaveTemplate,
    transform: &Transform,
) -> bool {
    let me = world.world_pos_rot(&WorldObject::from(owner));
    let is_boat = template.is_a_boat();
    let offset_z = if is_boat {
        summon_ship_hull(server, template).map_or(0.0, |(_, z, _)| z)
    } else {
        0.0
    };
    let water = transform.z - offset_z;
    let terrain = summon_terrain(server, world, transform.x, transform.y);
    // Land uses the terrain-only surface on every path; ships keep the water surface.
    let surface = if is_boat { water } else { terrain };
    let (dx, dy, dz) = (transform.x - me.x, transform.y - me.y, surface - me.z);
    if dx * dx + dy * dy + dz * dz > 80.0 * 80.0 {
        return false;
    }
    if is_boat {
        if terrain > water {
            return false;
        }
    } else if terrain - me.z > SUMMON_MAX_TERRAIN_ABOVE_OWNER {
        return false;
    }
    // Land rays end just above the candidate's terrain point ([`SUMMON_LOS_END_LIFT`]); a deck
    // between the eyes and that point is hit, so a bridge or quay above the terrain refuses.
    let los_to = if is_boat {
        [transform.x, transform.y, surface]
    } else {
        [transform.x, transform.y, surface + SUMMON_LOS_END_LIFT]
    };
    if !summon_los_clear(server, world.id, [me.x, me.y, me.z + 1.0], los_to) {
        return false;
    }
    if let Some(bounds) = summon_model_box(server, template) {
        if !model_box_clear_at(
            server,
            world.id,
            transform.x,
            transform.y,
            surface,
            transform.rot_z,
            &bounds,
            offset_z,
        ) {
            return false;
        }
    }
    true
}

/// Rechecks a searched summon transform after the async waits against the owner's current pose:
/// the 1.2 checks at the current yaw and surface height, then a fresh search when the old spot
/// no longer passes. `None` when no candidate passes now.
pub(super) fn revalidated_searched_placement(
    server: &GameServer,
    world: &WorldInstance,
    owner: &PlayerRef,
    template: &SlaveTemplate,
    transform: Transform,
) -> Option<Transform> {
    let me = world.world_pos_rot(&WorldObject::from(owner));
    let is_boat = template.is_a_boat();
    let offset_z = if is_boat {
        summon_ship_hull(server, template).map_or(0.0, |(_, z, _)| z)
    } else {
        0.0
    };
    let surface = if is_boat {
        super::ship_physics::water_surface(server, world, [transform.x, transform.y, me.z])
    } else {
        summon_terrain(server, world, transform.x, transform.y)
    };
    let mut check = transform;
    check.set_position(
        transform.x,
        transform.y,
        surface + offset_z,
        0.0,
        0.0,
        me.rot_z + std::f32::consts::PI / 2.0,
    );
    if summon_placement_clear_1_2(server, world, owner, template, &check) {
        return Some(check);
    }
    summon_candidate_1_2(server, world, owner, template)
}
