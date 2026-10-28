//! The slave side of the ship physics (`systems::ai::physics`): `world.Physics.AddShip` /
//! `RemoveShip`, the helm request and driver seat, `world.Water`, and the
//! [`ShipPhysicsHooks`] the physics thread calls (harpoon lines / tension / lifetime, the
//! `Throttle` / `Steering` written back after a move).

use crate::server::GameServer;
use crate::systems::ai::physics::harpoon::HarpoonLine;
use crate::systems::ai::physics::ship::ShipSpec;
use crate::systems::ai::physics::{self, ShipPhysicsHooks, SlaveKind};
use crate::systems::skills::controllers::Vec3;
use crate::world::{SlaveRef, WorldInstance, WorldObject};

use super::harpoon;

/// `world.Physics.AddShip(slave)`.
pub fn add_ship(server: &GameServer, slave: &SlaveRef) {
    let Some(srv) = server.self_arc() else {
        tracing::debug!("AddShip {}: no bound server", slave.obj_id);
        return;
    };
    let obj = WorldObject::from(slave);
    let Some(world) = super::world_of(server, &obj) else {
        return;
    };
    let (slave_id, template_id, scale, spawn_time_ms) = {
        let d = slave.data();
        (d.id, d.unit.template_id, d.unit.scale, d.spawn_time_ms)
    };
    let Some(template) = server
        .game_data
        .systems
        .vehicles
        .slave_template(template_id)
    else {
        return;
    };
    let p = world.world_pos_rot(&obj);
    let spec = ShipSpec {
        obj_id: slave.obj_id,
        slave_id,
        model_id: template.model_id,
        kind: SlaveKind(template.slave_kind as u8),
        scale,
        spawn_time_ms,
        portal_time: template.portal_time,
        position: Vec3::new(p.x, p.y, p.z),
        yaw: p.rot_z,
    };
    physics::add_ship(&srv, world.id, spec);
}

/// `world.Physics.RemoveShip(slave)`.
pub fn remove_ship(server: &GameServer, slave: &SlaveRef) {
    physics::remove_ship(server, slave.obj_id);
}

/// `ShipRequestMoveType`: `ThrottleRequest` / `SteeringRequest` handed to the physics.
pub fn set_request(server: &GameServer, slave: &SlaveRef, throttle: i8, steering: i8) {
    physics::set_ship_request(server, slave.obj_id, throttle, steering);
}

/// `AttachedCharacters.ContainsKey(AttachPointKind.Driver)` changed.
pub fn set_driver(server: &GameServer, slave: &SlaveRef, has_driver: bool) {
    physics::set_ship_driver(server, slave.obj_id, has_driver);
}

/// `world.Water.GetWaterSurface(point, out _)`.
pub fn water_surface(server: &GameServer, world: &WorldInstance, pos: [f32; 3]) -> f32 {
    physics::water_surface(server, world.id, Vec3::new(pos[0], pos[1], pos[2]))
        .map_or(world.template.ocean_level, |(h, _)| h)
}

/// `world.IsWater(point)`.
pub fn is_water(server: &GameServer, world: &WorldInstance, pos: [f32; 3]) -> bool {
    physics::is_water(server, world.id, Vec3::new(pos[0], pos[1], pos[2])).is_some()
}

/// How far above the nominal water surface (metres) a client-chosen boat summon point may sit.
/// The client picks the point on its animated, wave-displaced sea, so open-sea summon points
/// arrive 0.05-0.6 m above the template ocean level (`z = 100.3` over level 100.0), which the
/// strict [`is_water`] (`z <= surface`) refuses.
pub const SUMMON_WATER_TOLERANCE: f32 = 1.0;

/// [`is_water`] for a client-chosen boat summon point: the point itself, or the point lowered by
/// [`SUMMON_WATER_TOLERANCE`], must be in water, and in the second case the terrain under it
/// must be below the water surface (a beach just above sea level is land, not sea).
pub fn is_summon_water(server: &GameServer, world: &WorldInstance, pos: [f32; 3]) -> bool {
    point_on_summon_water(
        pos,
        SUMMON_WATER_TOLERANCE,
        |p| is_water(server, world, p),
        |p| water_surface(server, world, p),
        |x, y| crate::systems::npcs::heightmap::world_height(server, world, x, y).ok(),
    )
}

/// The tolerance logic of [`is_summon_water`] over the water queries (`is_water`, the surface
/// height at a point, the terrain height at `x, y`, `None` when unknown).
pub(crate) fn point_on_summon_water(
    pos: [f32; 3],
    tolerance: f32,
    is_water: impl Fn([f32; 3]) -> bool,
    surface: impl Fn([f32; 3]) -> f32,
    ground: impl Fn(f32, f32) -> Option<f32>,
) -> bool {
    if is_water(pos) {
        return true;
    }
    let probe = [pos[0], pos[1], pos[2] - tolerance];
    is_water(probe) && ground(pos[0], pos[1]).is_some_and(|ground| ground < surface(probe))
}

/// The vehicles subsystem's [`ShipPhysicsHooks`] (`server.systems.hooks.ship_physics`).
pub struct VehicleShipPhysics;

fn hull(server: &GameServer, obj_id: u32) -> Option<(std::sync::Arc<WorldInstance>, SlaveRef)> {
    let obj = server.world.object(obj_id)?;
    let slave = obj.as_slave()?.clone();
    let world = super::world_of(server, &obj)?;
    Some((world, slave))
}

impl ShipPhysicsHooks for VehicleShipPhysics {
    fn harpoon_lines(&self, server: &GameServer, hull_obj_id: u32) -> Vec<HarpoonLine> {
        let Some((world, hull)) = hull(server, hull_obj_id) else {
            return Vec::new();
        };
        harpoon::harpoon_lines(&world, &hull)
    }

    fn harpoon_recoil(&self, server: &GameServer, hull_obj_id: u32, dt: f32) -> (f32, f32) {
        let Some((_, hull)) = hull(server, hull_obj_id) else {
            return (0.0, 0.0);
        };
        let [x, y] = harpoon::tick_tension_tear_and_get_hull_recoil_delta_v(server, &hull, dt);
        (x, y)
    }

    fn harpoon_lifetime_tick(&self, server: &GameServer, hull_obj_id: u32) {
        if let Some((_, hull)) = hull(server, hull_obj_id) {
            harpoon::tick_harpoon_rope_controller_lifetime(server, &hull);
        }
    }

    fn ship_moved(&self, server: &GameServer, obj_id: u32) {
        // `Slave.Throttle` / `Steering` (the smoothed physics values `UseSlaveBase` reads).
        let Some(state) = physics::ship_state(server, obj_id) else {
            return;
        };
        if let Some(slave) = server
            .world
            .object(obj_id)
            .and_then(|o| o.as_slave().cloned())
        {
            let mut d = slave.data();
            d.throttle = state.throttle;
            d.steering = state.steering;
        }
    }
}

/// `StormShipLogic.RegularCannonModelId` (`SlaveTemplate.ModelId` of a regular ship cannon).
pub const REGULAR_CANNON_MODEL_ID: u32 = 117;

/// `StormShipLogic.TryBlockMountOrSlaveSkill(world, skillId, casterSlave)`: with the realistic
/// sea weather model, a regular cannon (not a harpoon skill) on a ship inside a storm cannot
/// fire. `true` = blocked (`SkillResult.NoPerm`).
pub fn storm_blocks_slave_skill(server: &GameServer, slave: &SlaveRef, skill_id: u32) -> bool {
    use crate::systems::ai::physics::forces::STORM_BUFF_ID;
    if server.config.world.sea_weather_model != crate::config::SeaWeatherModelType::Realistic {
        return false;
    }
    // Harpoon is also a ship-mounted weapon/slot; storm rules shouldn't block it.
    if skill_id == harpoon::SHIP_LAUNCH_HARPOON_SKILL_ID
        || skill_id == harpoon::SHIP_CUT_HARPOON_ROPE_SKILL_ID
    {
        return false;
    }
    let (template_id, owner_type, owner_obj_id) = {
        let d = slave.data();
        (d.unit.template_id, d.owner_type, d.owner_obj_id)
    };
    let is_cannon = server
        .game_data
        .systems
        .vehicles
        .slave_template(template_id)
        .is_some_and(|t| t.model_id == REGULAR_CANNON_MODEL_ID);
    if !is_cannon {
        return false;
    }
    // Cannon skills are cast by the cannon slave; its parent ship is stored as owner obj id.
    if owner_type != crate::models::unit::BaseUnitType::Slave || owner_obj_id == 0 {
        return false;
    }
    let Some(world) = super::world_of(server, &WorldObject::from(slave)) else {
        return false;
    };
    let Some(parent) = world.slave(owner_obj_id) else {
        return false;
    };
    let in_storm = parent.data().unit.buffs.check_buff(STORM_BUFF_ID);
    in_storm
}

#[cfg(test)]
mod summon_water_tests {
    use super::*;

    const OCEAN: f32 = 100.0;

    /// The template ocean: water at or below its level, nothing above.
    fn ocean(p: [f32; 3]) -> bool {
        p[2] <= OCEAN
    }

    fn surface(_: [f32; 3]) -> f32 {
        OCEAN
    }

    fn on_water(z: f32, ground: Option<f32>) -> bool {
        point_on_summon_water(
            [13841.5, 10491.5, z],
            SUMMON_WATER_TOLERANCE,
            ocean,
            surface,
            |_, _| ground,
        )
    }

    /// The refused production placements (z 100.06-100.6 over deep sea) are water.
    #[test]
    fn open_sea_points_slightly_above_the_surface_are_water() {
        for z in [99.0, 100.0, 100.058, 100.1, 100.3, 100.6, 101.0] {
            assert!(on_water(z, Some(88.0)), "z {z}");
        }
    }

    #[test]
    fn points_beyond_the_tolerance_above_the_surface_are_not_water() {
        for z in [101.01, 101.5, 105.0, 120.0] {
            assert!(!on_water(z, Some(88.0)), "z {z}");
        }
    }

    /// A beach a little above sea level must not become a boat spawn.
    #[test]
    fn terrain_at_or_above_the_surface_is_land_inside_the_tolerance_band() {
        assert!(!on_water(100.4, Some(100.4)));
        assert!(!on_water(100.4, Some(100.0)));
        assert!(!on_water(100.9, Some(101.2)));
        assert!(on_water(100.4, Some(99.9)));
    }

    #[test]
    fn unknown_terrain_is_refused_only_inside_the_tolerance_band() {
        assert!(!on_water(100.3, None));
        assert!(on_water(99.5, None), "strict water never needs the terrain");
    }

    /// A lake: water between `surface - depth` and `surface`; the client's point sits above it.
    #[test]
    fn lake_points_slightly_above_the_surface_are_water() {
        let (surface_z, depth) = (230.0f32, 6.0f32);
        let lake = |p: [f32; 3]| p[2] <= surface_z && p[2] >= surface_z - depth;
        let check = |z: f32, ground: f32| {
            point_on_summon_water(
                [5000.0, 6000.0, z],
                SUMMON_WATER_TOLERANCE,
                lake,
                |_| surface_z,
                |_, _| Some(ground),
            )
        };
        assert!(check(230.4, 226.0));
        assert!(!check(231.5, 226.0));
        assert!(!check(230.4, 230.4), "a lake shore is land");
    }
}
