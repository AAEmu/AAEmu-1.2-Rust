//! The area the siege rules apply to, and the eviction of non-participants.
//!
//! Evidence: the common relationship engine's siege branch (`sub_39695460`) tests two circles
//! while the siege period runs — the lodestone's siege circle and the offense-headquarters
//! circle (`D:/ArcheAge Returns 10.0.2.13 .../decompiled/x2game-dev_dedicate.dll.c`, cited as
//! `Z:2790654` in `D:/aaemu-audits/gap-audit-2026-11-10/verify-claims-10x.md`, claim 2). The
//! 1.2 data names both radii: `guard_tower_settings.radius_siege` (250 m small / 450 m large
//! castles) and `radius_offense_hq` (100 / 120 m).
//!
//! The removal rule is the owner's source for the 1.2-era siege flow (IGN, 2014-09-26,
//! "Castles"; Total ArcheAge, 2014-11-20, grade C): "only designated participants can enter"
//! the perimeter and "those who do not qualify ... are transported out of the siege perimeter
//! when it begins". The 1.2 client has no separate warning text for it; the error table has
//! `SIEGE_NOT_PARTICIPANT` 446 (ui_texts 2505 "Not a siege participant"), which the forced
//! `SCTeleportUnitPacket` carries in its error-message field.
//!
//! The 10.x enum numbers the running siege 3; the 1.2 client numbers it 4 (siege.md 1.2).
//! Both callers gate on [`super::state::SiegeState::active_at`], not on a specific number.

use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::housing::dominion::runtime;
use crate::systems::housing::dominion::state::{DominionState, SiegeSide};
use crate::systems::skills::packets_special::{
    SCTeleportUnitPacket, TELEPORT_REASON_MOVE_TO_LOCATION,
};
use crate::world::session;
use crate::world::{PlayerRef, WorldObject};

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
    let radius_hq = setting.map_or(DEFAULT_RADIUS_OFFENSE_HQ, |s| {
        f64::from(s.radius_offense_hq)
    });
    radius_hq > 0.0
        && horizontal_distance2((x, y), (hq.position.x, hq.position.y)) <= radius_hq.powi(2)
}

/// The nearest safe resurrection point (`PortalManager.GetClosestReturnPortal`, the same
/// fallback the ordinary resurrection uses), or the character's saved main-world position,
/// provided the destination is outside the siege area. `None` when the data has no safe spot.
fn safe_destination(
    server: &GameServer,
    player: &PlayerRef,
    state: &DominionState,
    instance_id: u32,
) -> Option<SCTeleportUnitPacket> {
    let portal = crate::systems::npcs::portal::closest_return_portal(server, player);
    if portal.id != 0
        && [portal.x, portal.y, portal.z]
            .into_iter()
            .all(f32::is_finite)
        && !inside(server, state, instance_id, portal.x, portal.y)
    {
        let z = crate::systems::ai::collision::ground_height_or(
            server,
            instance_id,
            portal.x,
            portal.y,
            portal.z,
            || portal.z,
        );
        if z.is_finite() {
            return Some(SCTeleportUnitPacket {
                reason: TELEPORT_REASON_MOVE_TO_LOCATION,
                error_message: error_message::SIEGE_NOT_PARTICIPANT,
                x: portal.x,
                y: portal.y,
                z,
                z2: portal.z_rot.to_radians(),
            });
        }
    }
    let mwp = player.data().main_world_position?;
    if mwp.instance_id != instance_id
        || ![mwp.x, mwp.y, mwp.z].into_iter().all(f32::is_finite)
        || inside(server, state, instance_id, mwp.x, mwp.y)
    {
        return None;
    }
    Some(SCTeleportUnitPacket {
        reason: TELEPORT_REASON_MOVE_TO_LOCATION,
        error_message: error_message::SIEGE_NOT_PARTICIPANT,
        x: mwp.x,
        y: mwp.y,
        z: mwp.z,
        z2: mwp.rot_z,
    })
}

/// Move every online, living non-participant out of an active siege area. A teleport that is
/// already pending is skipped: the server position only follows the client's acknowledgement,
/// so re-sending would stack packets while the client loads.
///
/// Called once a second by the dominion participant tick, and by the tests.
pub fn evict_non_participants(server: &GameServer) {
    let now = chrono::Utc::now().timestamp();
    for player in server.world.all_characters() {
        let (online, dead, teleporting, transform) = {
            let c = player.data();
            (
                c.is_online,
                c.unit.hp <= 0,
                c.disabled_set_position,
                c.unit.transform,
            )
        };
        if !online || dead || teleporting {
            continue;
        }
        let Some(zone) = server
            .game_data
            .zones
            .zone_by_key(transform.zone_id)
            .map(|zone| zone.group_id)
        else {
            continue;
        };
        let Some(state) = server.systems.housing.dominions.get(zone) else {
            continue;
        };
        let Some(siege) = state.siege.as_ref() else {
            continue;
        };
        if !siege.active_at(now) || state.tower.instance_id != transform.instance_id {
            continue;
        }
        let object = WorldObject::from(&player);
        let Some(world) = server.world.world_of(&object) else {
            continue;
        };
        let position = world.world_pos_rot(&object);
        if !inside(
            server,
            &state,
            transform.instance_id,
            position.x,
            position.y,
        ) {
            continue;
        }
        if siege.participants.contains_key(&player.character_id) {
            continue;
        }
        let Some(packet) = safe_destination(server, &player, &state, transform.instance_id) else {
            tracing::warn!(
                "Dominion {zone}: no safe point outside the siege area for '{}' ({}); not moved",
                player.data().unit.name,
                player.character_id
            );
            continue;
        };
        tracing::info!(
            "Dominion {zone}: moving '{}' ({}) out of the siege area: not a participant",
            player.data().unit.name,
            player.character_id
        );
        session::send_teleport_unit(&player, packet);
    }
}
