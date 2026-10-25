//! Closed-zone transport from `a2ab80abd^:AAEmu.Game/Models/Game/Char/Character.cs`,
//! `OnZoneChange` (five 2 s warnings, dismount/despawn, 200 ms grace, return portal).
//! Unknown keys can be
//! world.xml placeholders (e.g. main-world 202, o_temp_b), not necessarily closed zones.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::models::character::DEFAULT_INSTANCE_ID;
use crate::models::transform::Transform;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::npcs::portal;
use crate::systems::skills::packets_special::{SCCharacterResurrectedPacket, SCLoadInstancePacket};
use crate::world::{broadcast, PlayerRef, WorldObject};

/// Native 1.2 `x2game12.c:1163240`, error-name switch case 287 = `closed_zone`.
pub const CLOSED_ZONE: i16 = 287;

#[derive(Debug, Clone, Default)]
pub struct ZoneAccessState {
    // Retain a completed attempt until departure: a missing safe route must not start a loop.
    attempt: Option<CancellationToken>,
    safe_position: Option<Transform>,
}

fn is_closed(server: &GameServer, zone: u32) -> bool {
    server
        .game_data
        .zones
        .zone_by_key(zone)
        .is_some_and(|z| z.closed)
}

/// Check both the saved zone and the actual destination sector before charging or moving.
/// A missing static zone row alone is not evidence that a coordinate is forbidden.
pub fn zone_access_allowed(
    server: &GameServer,
    player: &PlayerRef,
    instance: u32,
    zone: u32,
    x: f32,
    y: f32,
) -> bool {
    if player.data().access_level >= 100 {
        return true;
    }
    !is_closed(server, zone)
        && !server
            .world
            .worlds()
            .iter()
            .any(|w| w.id == instance && is_closed(server, w.template.zone_id_at(x, y)))
}

pub(super) fn cancel(player: &PlayerRef) {
    if let Some(token) = player.data().zone_access.attempt.take() {
        token.cancel();
    }
}

fn world_transform(server: &GameServer, player: &PlayerRef) -> Option<Transform> {
    let mut t = player.data().unit.transform;
    let world = server.world.world(t.instance_id)?;
    let pos = world.world_pos_rot(&WorldObject::from(player));
    t.set_position(pos.x, pos.y, pos.z, pos.rot_x, pos.rot_y, pos.rot_z);
    t.parent_obj_id = 0;
    t.world_id = world.template.id;
    t.zone_id = world.template.zone_id_at(t.x, t.y);
    Some(t)
}

// Eviction destinations require positive open-zone evidence, a live world and real coordinates.
// This deliberately rejects GetClosestReturnPortal's synthetic current-position fail-safe.
fn safe_destination(server: &GameServer, t: &Transform) -> bool {
    [t.x, t.y, t.z, t.rot_x, t.rot_y, t.rot_z]
        .iter()
        .all(|v| v.is_finite())
        && server
            .game_data
            .zones
            .zone_by_key(t.zone_id)
            .is_some_and(|z| !z.closed)
        && server.world.worlds().iter().any(|w| {
            w.id == t.instance_id
                && w.template.id == t.world_id
                && w.template.zone_id_at(t.x, t.y) == t.zone_id
        })
}

pub(super) fn remember_open_position(server: &GameServer, player: &PlayerRef) {
    if let Some(t) = world_transform(server, player).filter(|t| safe_destination(server, t)) {
        player.data().zone_access.safe_position = Some(t);
    }
}

fn needs_return(server: &GameServer, player: &PlayerRef) -> bool {
    let (online, access, t) = {
        let c = player.data();
        (c.is_online, c.access_level, c.unit.transform)
    };
    online
        && access < 100
        && !server.is_shutting_down()
        && server
            .world
            .character_by_obj_id(player.obj_id)
            .is_some_and(|p| Arc::ptr_eq(&p, player))
        && (is_closed(server, t.zone_id)
            || world_transform(server, player).is_some_and(|t| is_closed(server, t.zone_id)))
}

pub(super) fn update(server: &GameServer, player: &PlayerRef) {
    if !needs_return(server, player) {
        cancel(player);
        remember_open_position(server, player);
        return;
    }
    let Some(server) = server.self_arc() else {
        return;
    };
    let token = {
        let mut c = player.data();
        if c.zone_access.attempt.is_some() {
            return;
        }
        let token = CancellationToken::new();
        c.zone_access.attempt = Some(token.clone());
        token
    };
    // Weak handles prevent a sleeping warning from retaining a departed session/server.
    let weak_server = Arc::downgrade(&server);
    let weak_player = Arc::downgrade(player);
    let started = tokio::time::Instant::now();
    player.send(SCErrorMsgPacket::new(CLOSED_ZONE, 0, false));
    tokio::spawn(async move {
        for tick in 1..=5 {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep_until(started + Duration::from_secs(tick * 2)) => {}
            }
            let (Some(server), Some(player)) = (weak_server.upgrade(), weak_player.upgrade())
            else {
                return;
            };
            if token.is_cancelled() {
                return;
            }
            if !needs_return(&server, &player) {
                cancel(&player);
                return;
            }
            if tick < 5 {
                player.send(SCErrorMsgPacket::new(CLOSED_ZONE, 0, false));
            }
        }
        let (Some(server), Some(player)) = (weak_server.upgrade(), weak_player.upgrade()) else {
            return;
        };
        let Some(destination) = return_destination(&server, &player) else {
            tracing::warn!(
                character_id = player.character_id,
                "Closed zone: no verified open return route"
            );
            return;
        };
        crate::systems::vehicles::force_dismount(
            &server,
            &player,
            crate::systems::vehicles::slaves::attach_reason::PREFAB_CHANGED,
        );
        let instance = player.data().unit.transform.instance_id;
        crate::systems::vehicles::mates::remove_and_despawn_all_active_owned_mates(
            &server,
            instance,
            player.character_id,
        );
        // Match the historical 200 ms dismount grace; recheck cancellation and identity after it.
        tokio::select! {
            _ = token.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        if token.is_cancelled() {
            return;
        }
        if !needs_return(&server, &player) || !safe_destination(&server, &destination) {
            cancel(&player);
            return;
        }
        transport(&server, &player, destination);
    });
}

fn return_destination(server: &GameServer, player: &PlayerRef) -> Option<Transform> {
    let (instance, recorded, main) = {
        let c = player.data();
        (
            c.unit.transform.instance_id,
            c.zone_access.safe_position,
            c.main_world_position,
        )
    };
    // In an instance, prefer its recorded open route over a respawn in another world.
    if let Some(t) = recorded.filter(|t| {
        instance != DEFAULT_INSTANCE_ID && t.instance_id == instance && safe_destination(server, t)
    }) {
        return Some(t);
    }
    if instance != DEFAULT_INSTANCE_ID {
        if let Some(t) = main.filter(|t| safe_destination(server, t)) {
            return Some(t);
        }
    }
    let p = portal::closest_return_portal(server, player);
    let t = Transform {
        instance_id: DEFAULT_INSTANCE_ID,
        world_id: 0,
        zone_id: p.zone_id,
        x: p.x,
        y: p.y,
        z: p.z,
        rot_z: p.z_rot,
        ..Transform::default()
    };
    safe_destination(server, &t)
        .then_some(t)
        .or_else(|| recorded.filter(|t| safe_destination(server, t)))
}

fn transport(server: &GameServer, player: &PlayerRef, destination: Transform) {
    let old = player.data().unit.transform;
    let Some(world) = server.world.world(destination.instance_id) else {
        return;
    };
    if old.instance_id != destination.instance_id
        && !crate::systems::vehicles::on_instance_change(server, player)
    {
        return;
    }
    if old.instance_id != destination.instance_id {
        leave_instance_membership(server, player, old.instance_id);
    }
    if let Some(old_world) = server.world.world_of(&WorldObject::from(player)) {
        old_world.detach_child(&WorldObject::from(player));
        // Old observers must receive the forced displacement before the player leaves their view.
        broadcast(
            &old_world,
            player,
            &SCCharacterResurrectedPacket {
                unit_id: player.obj_id,
                x: destination.x,
                y: destination.y,
                z: destination.z,
                z_rot: destination.rot_z,
            },
            true,
        );
    }
    if old.instance_id != destination.instance_id {
        player.send(SCLoadInstancePacket {
            instance_id: destination.instance_id,
            zone_id: destination.zone_id,
            x: destination.x,
            y: destination.y,
            z: destination.z,
            ang_x: destination.rot_x,
            ang_y: destination.rot_y,
            ang_z: destination.rot_z,
        });
    }
    {
        let mut c = player.data();
        c.set_disabled_set_position(true);
        // Native OnCharacterResurrected (391D3AD0) does not acknowledge a same-world
        // displacement with TeleportEnded. Only a world load waits for InstanceLoaded.
        if old.instance_id == destination.instance_id {
            c.set_disabled_set_position(false);
        }
        c.unit.transform = destination;
    }
    server
        .world
        .leave_other_instance(&WorldObject::from(player), destination.instance_id);
    if old.instance_id != destination.instance_id {
        if let Some(previous) = server.world.world(old.instance_id) {
            previous.remove_object(player.obj_id);
        }
    }
    world.add_object(player);
    let changes = world.add_visible_object(player, &server.view_ctx());
    super::handle_zone_changes(server, changes);
    if old.zone_id != destination.zone_id {
        super::on_zone_change(server, player, old.zone_id, destination.zone_id);
    }
    update(server, player);
}

/// The membership part of a normal instance leave, without replaying its saved return
/// transform (which may itself be closed). The verified destination is applied by transport.
fn leave_instance_membership(server: &GameServer, player: &PlayerRef, instance: u32) {
    use crate::systems::instances::events::CharEvent;
    let events = &server.systems.instances.events.characters;
    if let Some(dungeon) = server.systems.instances.dungeon_of_world(instance) {
        {
            let mut state = dungeon.st();
            state
                .leave_requests
                .retain(|(id, _)| *id != player.character_id);
            state.enter_requests.retain(|p| !Arc::ptr_eq(p, player));
        }
        for event in [
            CharEvent::TeamJoin,
            CharEvent::TeamKick,
            CharEvent::TeamLeave,
            CharEvent::DungeonLeave,
            CharEvent::Disconnect,
        ] {
            events.unsubscribe(player, event, dungeon.uid);
        }
    }
    if let Some(game) = server.systems.instances.instant_games.current(player) {
        crate::systems::instances::instant_game::on_disconnect(server, player);
        crate::systems::instances::instant_game::remove_player(server, &game, player);
        crate::systems::social::faction::restore_temporary_faction(server, player);
        events.unsubscribe(player, CharEvent::Kill, game.uid);
    }
}
