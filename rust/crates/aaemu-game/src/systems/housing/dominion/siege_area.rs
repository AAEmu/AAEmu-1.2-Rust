//! The area the siege rules apply to.
//!
//! Evidence: the common relationship engine's siege branch (`sub_39695460`) tests two circles
//! while the siege period runs — the lodestone's siege circle and the offense-headquarters
//! circle (`D:/ArcheAge Returns 10.0.2.13 .../decompiled/x2game-dev_dedicate.dll.c`, cited as
//! `Z:2790654` in `D:/aaemu-audits/gap-audit-2026-11-10/verify-claims-10x.md`, claim 2). The
//! 1.2 data names both radii: `guard_tower_settings.radius_siege` (250 m small / 450 m large
//! castles) and `radius_offense_hq` (100 / 120 m).

use crate::server::GameServer;
use crate::systems::housing::dominion::runtime;
use crate::systems::housing::dominion::state::{DominionState, SiegeSide};
use crate::world::WorldObject;

/// Fallback when a data row has no `radius_siege` (the shipped rows have 250 / 450).
const DEFAULT_RADIUS_SIEGE: f64 = 250.0;
/// Fallback when a data row has no `radius_offense_hq` (the shipped rows have 100 / 120).
const DEFAULT_RADIUS_OFFENSE_HQ: f64 = 100.0;

fn horizontal_distance2(a: (f32, f32), b: (f32, f32)) -> f64 {
    (f64::from(a.0) - f64::from(b.0)).powi(2) + (f64::from(a.1) - f64::from(b.1)).powi(2)
}

/// Whether `(x, y)` lies inside the active siege's area in `instance_id`: the lodestone's
/// `radius_siege` circle or, when one is bound, the offense HQ's `radius_offense_hq` circle.
pub fn inside(
    server: &GameServer,
    state: &DominionState,
    instance_id: u32,
    x: f32,
    y: f32,
) -> bool {
    if !x.is_finite() || !y.is_finite() || state.tower.instance_id != instance_id {
        return false;
    }
    let Some(tower) = server.systems.housing.manager.by_id(state.tower.house_id) else {
        return false;
    };
    let tower_object = WorldObject::from(&tower);
    if !runtime::is_live_object(server, &tower_object) {
        return false;
    }
    let Some(world) = server.world.world(instance_id) else {
        return false;
    };
    let tower_pos = world.world_pos_rot(&tower_object);
    let setting = server
        .game_data
        .systems
        .housing
        .castles
        .guard_tower_settings
        .get(&state.tower.guard_tower_setting_id);
    let radius_siege = setting.map_or(DEFAULT_RADIUS_SIEGE, |s| f64::from(s.radius_siege));
    if radius_siege > 0.0
        && horizontal_distance2((x, y), (tower_pos.x, tower_pos.y)) <= radius_siege.powi(2)
    {
        return true;
    }
    let Some(hq) = server
        .systems
        .housing
        .headquarters
        .snapshot(server, state, SiegeSide::Attacker)
    else {
        return false;
    };
    let radius_hq = setting.map_or(DEFAULT_RADIUS_OFFENSE_HQ, |s| f64::from(s.radius_offense_hq));
    radius_hq > 0.0
        && horizontal_distance2((x, y), (hq.position.x, hq.position.y)) <= radius_hq.powi(2)
}
