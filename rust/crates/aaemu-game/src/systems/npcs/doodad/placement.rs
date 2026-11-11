//! The 1.2 seed-builder placement rules (`FUN_393a8fa0`), used by the one planting entry path
//! (`CSCreateDoodadPacket`), so a modified client cannot bypass them. Server-spawned doodads
//! (crime evidence, the fake-trial GM command) are not plantings and do not use these rules.
//!
//! Evidence (see PORTING.md, `systems::npcs::doodad`):
//! * range: `FUN_393a8fa0` step 3, refuse only when `900.0 < dist3D^2` (exactly 30 m is
//!   allowed); F1's screen sweep cannot pin the boundary.
//! * water: step 1, group 40 (marine crops) needs water, every other group refuses it; the
//!   capture's text for the refusal is `DOODAD_SIM_AREA` (276) `Can't be installed here.`
//! * overlap: step 5, sphere test `dist3D < r_new + r_other` when both doodads are round
//!   (`FUN_3983b360`), radius `max(sim_radius / 1000, 0.1)`; step 6, units use the new doodad's
//!   radius. Non-round pairs use oriented boxes / physics colliders that are not decoded, so
//!   they are skipped (the server may be more permissive, never stricter).
//! * dominion: step 7, only item 29800 ("Blueprint: Territory Statue", `FUN_393a8fa0`
//!   `0x393a9236`) is gated. The spot must be in a non-zero zone group whose dominion record is
//!   owned by the caster's own expedition and lies within the tower's 2D `radius_dominion`,
//!   else `LOCATE_INVALID_DOMINION_AREA` (749). Every other item skips the gate.
//! * footprint terrain spread: step 2 (`FUN_393a7720` -> `FUN_393a73a0`), the terrain heights
//!   at the four rotated bottom corners of the ghost's render OBB and their centre; refuse
//!   `max - min > 6.0` m (strict) with `DOODAD_SIM_LOW_TERRAIN` (279) "Terrain is too low.".
//!   Skipped for template groups 2, 3, 5, 12, 37, 40 and 80 (`doodad_almighties.group_id`).
//!   The client uses the render OBB, which the server does not have: the model's collision
//!   bounds (`collision::objects::shape_bounds`) are the documented proxy, and a model
//!   without bounds skips the check rather than refusing. The client's z snap to the minimum
//!   sample is not mirrored (a modded client's z is only bounded by the spread).
//!
//! Not implemented (open items in PORTING.md):
//! * the house/farm area rule of step 0/0b (the server keeps its own house/public-farm checks);
//! * the area-box test `FUN_393287c0` (0x114), the overlap skip list
//!   (`FUN_397a7cc0` / `FUN_397a7ce0`), and the farm-plot radius call `FUN_3983b280(..,1)`; the
//!   skip list compares four template pointers whose ids are not initialized in the decompile.
//!
//! The client's water test (`worldvtbl+0xf4`) was not decoded; this uses the server's
//! `WorldInstance.IsWater` (water volumes + ocean), the same query class.

use aaemu_physics::geometry::primitives::BoxPrim;

use crate::server::GameServer;
use crate::systems::ai::collision;
use crate::systems::housing::dominion::fort;
use crate::systems::npcs::doodad::packets::error_message;
use crate::systems::npcs::doodad::templates::DoodadTemplate;
use crate::systems::npcs::terrain_surface::SurfaceMaterial;
use crate::world::{PlayerRef, WorldInstance, WorldObject};

/// `FUN_393a7720` step 1: the client skips the footprint terrain spread for these template
/// groups (`doodad_almighties.group_id`, `DoodadTemplate::group_id`): 37 is the static
/// `0x399d2450`, `FUN_3983b2f0` covers 2, 3 and 12, `FUN_3983b320` 5 and 80, `FUN_3983b340`
/// 40.
pub const FOOTPRINT_SKIP_GROUPS: [u32; 7] = [2, 3, 5, 12, 37, 40, 80];
/// `0x3999ecf4` (static 6.0): the client refuses `max - min > 6.0` m (strict).
pub use crate::systems::ai::collision::placement::MAX_FOOTPRINT_SPREAD;

/// `FUN_393a8fa0` step 3: at most 30 m in 3D (`900.0 < d²` refuses, so exactly 30 m is
/// allowed). F1's screen-projected sweep cannot pin the boundary; the decompile can.
pub const PLACE_RANGE: f32 = 30.0;
/// `DAT_39ac774c` (static 29800): "Blueprint: Territory Statue", the only item whose placement
/// `FUN_393a8fa0` gates on a dominion (`systems::housing::dominion::fort::state_at`).
pub const TERRITORY_STATUE_BLUEPRINT: u32 = 29800;
/// `FUN_3983b340`: marine crops, the only group that needs water (and refuses dry land).
pub const MARINE_CROPS: u32 = 40;
/// The client PR's water depth assumption (`height > spot.z + 0.3`); the decompiled water test
/// (`worldvtbl+0xf4`) and capture F5 were not decoded, so the 0.3 m margin is capture-pending.
pub const WATER_DEPTH_MARGIN: f32 = 0.3;
/// `FUN_3983b360`: the groups whose doodads overlap as spheres.
pub const ROUND_GROUPS: [u32; 6] = [2, 3, 5, 12, 40, 80];
/// `FUN_3983b360`: this template overlaps as a sphere whatever its group.
pub const SPHERE_TEMPLATE: u32 = 1219;
/// `FUN_3983b360`: this group overlaps as a sphere whatever the template.
pub const SPHERE_GROUP: u32 = 38;
/// `FUN_393a7170` step 0 skips the place-skin rule for these templates (Pathfinder Map / Door).
pub const PLACE_SKIN_SKIP_TEMPLATES: [u32; 2] = [1218, 1219];
/// `FUN_393a7170` step 0 skips the place-skin rule for this group.
pub const PLACE_SKIN_SKIP_GROUP: u32 = 37;

/// Why a spot cannot hold the doodad, with the `ErrorMessageType` the 1.2 client shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `DOODAD_SIM_AREA` (276), the 1.2 area refusal ("Can't be installed here.").
    Area,
    /// `DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD` (277).
    Doodad,
    /// `DOODAD_SIM_TOO_CLOSE_TO_OTHER_UNIT` (278).
    Unit,
    /// `DOODAD_SIM_LOW_TERRAIN` (279): the model footprint's terrain spread exceeds 6 m.
    Terrain,
    /// `TOO_FAR_AWAY` (154).
    TooFar,
    /// `LOCATE_INVALID_DOMINION_AREA` (749): the Territory Statue blueprint's spot is not
    /// inside the caster's own guild dominion.
    Dominion,
}

impl Refusal {
    pub const fn error_message(self) -> i16 {
        match self {
            Self::Area => error_message::DOODAD_SIM_AREA,
            Self::Doodad => error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD,
            Self::Unit => error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_UNIT,
            Self::Terrain => error_message::DOODAD_SIM_LOW_TERRAIN,
            Self::TooFar => error_message::TOO_FAR_AWAY,
            Self::Dominion => error_message::LOCATE_INVALID_DOMINION_AREA,
        }
    }
}

/// `FUN_3983b280`: the room a doodad keeps around it, `max(sim_radius * 0.001, 0.1)` m.
pub fn radius(template: &DoodadTemplate) -> f32 {
    (template.sim_radius as f32 * 0.001).max(0.1)
}

/// `FUN_3983b360`: does this doodad overlap as a sphere?
pub fn is_round(template: &DoodadTemplate) -> bool {
    ROUND_GROUPS.contains(&template.group_id)
        || template.id == SPHERE_TEMPLATE
        || template.group_id == SPHERE_GROUP
}

/// A doodad obstruction at a world position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    pub pos: (f32, f32, f32),
    pub radius: f32,
    pub round: bool,
}

impl Shape {
    pub fn of(template: &DoodadTemplate, pos: (f32, f32, f32)) -> Self {
        Self {
            pos,
            radius: radius(template),
            round: is_round(template),
        }
    }
}

fn distance_squared(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)
}

/// `FUN_393a8fa0` step 3: the spot is at most 30 m from the player in 3D (`900.0 < d²` refuses,
/// so exactly 30 m is allowed).
pub fn in_place_range(player: (f32, f32, f32), pos: (f32, f32, f32)) -> bool {
    let distance_squared = distance_squared(player, pos);
    distance_squared.is_finite() && distance_squared <= PLACE_RANGE * PLACE_RANGE
}

/// The 1.2 doodad-overlap test: spheres on the sum of radii when both doodads are round. Every
/// other pair uses an oriented box test (`FUN_390318f0` / `FUN_39030ae0`) that is not decoded, so
/// it is skipped: the server must never be stricter than the client (groups 72/69 plantables keep
/// 3 m of room in the client and must remain placeable here).
pub fn doodad_overlaps(new: Shape, other: Shape) -> bool {
    if !(new.round && other.round) {
        return false;
    }
    let limit = new.radius + other.radius;
    distance_squared(new.pos, other.pos) < limit * limit
}

/// The 1.2 unit-overlap test: a sphere of the new doodad's radius for a round doodad. A non-round
/// doodad uses a physics box collider (`FUN_39180d50`), not decoded, so this skips it.
pub fn unit_overlaps(new: Shape, other: (f32, f32, f32)) -> bool {
    if !new.round {
        return false;
    }
    let limit_squared = new.radius * new.radius;
    distance_squared(new.pos, other) < limit_squared
}

/// The D1a dominion gate of `FUN_393a8fa0` (`dll.c:761340-761378`, disasm
/// `0x393a9236-0x393a92b7`): `fort::state_at` answers the non-zero zone group, the live tower's
/// 2D `radius_dominion` (the `+0xa0` short; the field order is INFERRED from
/// `DominionTerritoryData`'s wire order) and the dominion owner. The caster must be in an
/// expedition and it must be that owner, not the nation king (`FUN_39053360`).
fn in_own_dominion(
    server: &GameServer,
    world: &WorldInstance,
    player: &PlayerRef,
    pos: (f32, f32),
) -> bool {
    let Some(state) = fort::state_at(server, world.id, pos.0, pos.1) else {
        return false;
    };
    // `FUN_392fa6b0`: a character in no expedition answers 0 and fails the owner comparison.
    let expedition = server
        .expeditions
        .expedition_of_character(player.character_id);
    expedition != 0 && state.owner_expedition_id == expedition
}

/// `FUN_393a7170` step 0: the place-skin terrain rule is skipped for templates 1218/1219, group
/// 37 and a spot inside a house box the player may build on. The server's house query is the
/// garden square (`house_at_location_in_world`), not the client's shrunken box, and the
/// permission check is `allowed_to_interact`: permissive, never stricter.
fn place_skin_skipped(
    server: &GameServer,
    world: &WorldInstance,
    player: &PlayerRef,
    template: &DoodadTemplate,
    x: f32,
    y: f32,
) -> bool {
    if PLACE_SKIN_SKIP_TEMPLATES.contains(&template.id)
        || template.group_id == PLACE_SKIN_SKIP_GROUP
    {
        return true;
    }
    crate::systems::housing::house_at_location_in_world(server, world.id, x, y)
        .is_some_and(|house| crate::systems::housing::allowed_to_interact(server, &house, player))
}

/// `FUN_393a8fa0` step "place skin" (`Housing.PlacementStrictSurface` only): the terrain material
/// under the spot must map to one of the template's `doodad_place_skins` kinds. A material whose
/// engine surface type is not one of the client's four names is refused, like a known kind the
/// template does not allow; no usable surface data (world/cell/hole/unknown id) allows.
fn surface_material_refusal(
    server: &GameServer,
    world: &WorldInstance,
    template: &DoodadTemplate,
    x: f32,
    y: f32,
) -> Option<Refusal> {
    let heightmaps = server.systems.npcs.heightmaps.get()?;
    let map = heightmaps.template(world.template.id)?;
    let material = map.material_at(heightmaps.files(), x, y);
    let allowed = server
        .game_data
        .systems
        .npcs
        .doodad
        .place_skins
        .get(&template.id);
    match material {
        SurfaceMaterial::Unknown => None,
        SurfaceMaterial::Unmapped => Some(Refusal::Area),
        SurfaceMaterial::Kind(kind) => {
            if allowed.is_some_and(|kinds| kinds.contains(&kind.kind_id())) {
                None
            } else {
                Some(Refusal::Area)
            }
        }
    }
}

/// The starting func-group model of the template (`DoodadManager.Create`'s
/// `GetFuncGroupId`) as collision bounds: the documented proxy for the client's ghost render
/// OBB. `None` without a model reference or physics geometry.
fn template_bounds(server: &GameServer, template: &DoodadTemplate) -> Option<BoxPrim> {
    let group = crate::systems::npcs::doodad::phase::get_func_group_id(
        &server.game_data.systems.npcs.doodad,
        template.id,
    );
    let model = server
        .game_data
        .systems
        .ai
        .collision
        .func_group_model(group)?;
    collision::objects::model_bounds(server, model)
}

/// `FUN_393a7720` -> `FUN_393a73a0` step 2: the terrain height spread over the placed model's
/// footprint. The client samples the ghost's render OBB; the server substitutes the collision
/// bounds and skips the check when the model has none (render bounds can be larger, so the
/// proxy only ever under-refuses). The client's z snap to the minimum sample is not mirrored.
fn footprint_refusal(
    server: &GameServer,
    world: &WorldInstance,
    template: &DoodadTemplate,
    pos: (f32, f32, f32),
    z_rot: f32,
    scale: f32,
) -> Option<Refusal> {
    if FOOTPRINT_SKIP_GROUPS.contains(&template.group_id) {
        return None;
    }
    let bounds = template_bounds(server, template)?;
    let scale = scale.max(0.0);
    let bounds = BoxPrim::aligned(bounds.center * scale, bounds.size * scale);
    let (min, max) =
        collision::placement::footprint_terrain_spread(server, world.id, pos, z_rot, &bounds)?;
    (max - min > MAX_FOOTPRINT_SPREAD).then_some(Refusal::Terrain)
}

/// The placement refusal of `pos` for `template`, or `None` when the spot is free.
///
/// Order follows `FUN_393a8fa0`: area (restrict zone) -> water -> range -> doodads -> units ->
/// dominion (only for the Territory Statue blueprint, by `item_template_id`). The place-skin
/// terrain material rule of step 0 runs last, only with `Housing.PlacementStrictSurface` on,
/// and the footprint terrain spread completes the checks.
#[allow(clippy::too_many_arguments)]
pub fn refusal(
    server: &GameServer,
    world: &WorldInstance,
    player: &PlayerRef,
    template: &DoodadTemplate,
    item_template_id: u32,
    pos: (f32, f32, f32),
    z_rot: f32,
    scale: f32,
) -> Option<Refusal> {
    let (x, y, z) = pos;
    // F11: the 1.2 client refuses the restricted-zone seed with 276 before it starts a builder.
    if template.restrict_zone_id != 0 && template.restrict_zone_id != world.zone_id_at(x, y) {
        return Some(Refusal::Area);
    }
    let wet = crate::systems::ai::collision::is_water(server, world.id, x, y, z)
        && crate::systems::ai::collision::water_level(server, world.id, x, y, z)
            .is_some_and(|surface| surface > z + WATER_DEPTH_MARGIN);
    if template.group_id == MARINE_CROPS {
        if !wet {
            return Some(Refusal::Area);
        }
    } else if wet {
        return Some(Refusal::Area);
    }
    let player_pos = world.world_pos_rot(&WorldObject::from(player));
    let player_pos = (player_pos.x, player_pos.y, player_pos.z);
    // `_DAT_399d2464 < d²` in `FUN_393a8fa0` (~761319): exactly 900 (30 m) is allowed.
    if !in_place_range(player_pos, pos) {
        return Some(Refusal::TooFar);
    }
    let new = Shape::of(template, pos);
    let mut units: Vec<(f32, f32, f32)> = Vec::new();
    for obj in nearby_objects(world, x, y) {
        if obj.obj_id() == player.obj_id {
            continue;
        }
        let other_pos = world.world_pos_rot(&obj);
        let other_pos = (other_pos.x, other_pos.y, other_pos.z);
        if let Some(doodad) = obj.as_doodad() {
            let Some(other_template) = server
                .game_data
                .systems
                .npcs
                .doodad
                .template(doodad.data().template_id)
            else {
                continue;
            };
            if doodad_overlaps(new, Shape::of(other_template, other_pos)) {
                return Some(Refusal::Doodad);
            }
        } else if matches!(obj, WorldObject::House(_) | WorldObject::Shipyard(_))
            || obj.with_unit(|u| u.unit().is_dead()).unwrap_or(false)
        {
            continue;
        } else {
            units.push(other_pos);
        }
    }
    if units.iter().any(|&p| unit_overlaps(new, p)) {
        return Some(Refusal::Unit);
    }
    // Step 7 is gated on the item template, not the doodad template: every other item jumps
    // straight to success (`this+0x14 == 0 or id != 29800`, `0x393a9305`).
    if item_template_id == TERRITORY_STATUE_BLUEPRINT
        && !in_own_dominion(server, world, player, (x, y))
    {
        return Some(Refusal::Dominion);
    }
    if server.config.housing.placement_strict_surface
        && !place_skin_skipped(server, world, player, template, x, y)
    {
        if let Some(refusal) = surface_material_refusal(server, world, template, x, y) {
            return Some(refusal);
        }
    }
    if let Some(refusal) = footprint_refusal(server, world, template, pos, z_rot, scale) {
        return Some(refusal);
    }
    None
}

/// The visible objects of the spot's region and its neighbours.
fn nearby_objects(world: &WorldInstance, x: f32, y: f32) -> Vec<WorldObject> {
    let Some(region) = world.region_by_pos(x, y) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for neighbor in world.neighbors(region) {
        out.extend(world.objects_in_region(neighbor));
    }
    out
}
