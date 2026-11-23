use crate::server::GameServer;
use crate::systems::npcs::npc::packets::SCUnitModelPostureChangedPacket;
use crate::world::{PlayerRef, SlaveRef, WorldObject};

use super::super::now_ms;
use super::{player_world, world_of};

/// Server-side turret-aim relay throttle. The 1.2 client's `FUN_39356990` already sends at most
/// one packet per 200 ms (`DAT_3a169940 + 200 <= now`) and only when its model aim changed;
/// 150 ms here additionally collapses datagrams that arrive together without hiding the client
/// rate. The window only delays the relay: the newest aim is always stored and a suppressed
/// relay is sent by the next 1 s slave tick after the window ends (see
/// [`flush_pending_turret_state`]).
pub const TURRET_STATE_MIN_INTERVAL_MS: i64 = 150;

/// Applies a client turret aim (`CSTurretStatePacket`) to the slave named by `unit_id`.
///
/// The 1.2 client sends the object id of the unit it controls (`39356990` uses `this+8`), so a
/// ship cannon names the cannon child slave, a hull with an installed turret names the hull, and
/// the tank passenger seat names the hull. Any attach point of that slave counts as seated; a
/// sender who is not on it is ignored. Only a slave whose model is a `VehicleModel` has a turret:
/// the client producers (`39425490` / `39425580`) need a `VehicleUnitModel` and clamp / wrap the
/// angles to that model's `vehicle_models` limits before sending, so the server clamps the raw
/// model-frame radians to the same limits (defence against a modified client); a limit stored
/// as a non-numeric value (the text `'f'` in the real rows) is "no limit", and a reversed window
/// is sorted. A slave without a `VehicleModel` row has no turret and is ignored. The aim is kept
/// on the slave and is not cleared on unbind, so late viewers keep the last posture.
///
/// The newest aim is always stored (under the slave lock) so a suppressed relay can never
/// leave late viewers on an older one; the throttle only decides whether it is relayed now or
/// by the next 1 s slave tick once the window ends.
pub fn set_turret_state(
    server: &GameServer,
    character: &PlayerRef,
    unit_id: u32,
    pitch: f32,
    yaw: f32,
) {
    if !pitch.is_finite() || !yaw.is_finite() {
        return;
    }
    let Some(world) = player_world(server, character) else {
        return;
    };
    let Some(slave) = world.slave(unit_id) else {
        return;
    };
    let model_id = slave.data().unit.model_id;
    let Some(model) = server
        .game_data
        .systems
        .vehicles
        .vehicle_model(model_id)
        .copied()
    else {
        return;
    };
    let (pitch, yaw) = model.clamp_turret_angles(pitch, yaw);
    // Check and act under the same lock: only a character seated on this slave may aim it, an
    // unchanged aim is never relayed, and a changed aim inside the window waits for the tick.
    let posture = {
        let mut d = slave.data();
        if !d
            .attached_characters
            .iter()
            .any(|(_, c)| *c == character.obj_id)
        {
            return;
        }
        if pitch == d.turret_pitch && yaw == d.turret_yaw {
            return;
        }
        d.turret_pitch = pitch;
        d.turret_yaw = yaw;
        let now = now_ms();
        if now - d.turret_last_update_ms < TURRET_STATE_MIN_INTERVAL_MS {
            d.turret_relay_pending = true;
            d.turret_pending_sender_obj_id = character.obj_id;
            return;
        }
        d.turret_relayed_pitch = pitch;
        d.turret_relayed_yaw = yaw;
        d.turret_relay_pending = false;
        d.turret_pending_sender_obj_id = 0;
        d.turret_last_update_ms = now;
        SCUnitModelPostureChangedPacket {
            obj_id: unit_id,
            posture: crate::models::unit::ModelPostureType::TurretState,
            has_loot_left: false,
            anim_action_id: 0,
            activate_animation: false,
            turret_pitch: pitch,
            turret_yaw: yaw,
        }
    };
    let obj = WorldObject::from(&slave);
    for viewer in world.players_around(&obj) {
        if viewer.obj_id != character.obj_id {
            viewer.send(posture);
        }
    }
}

/// Trailing relay of the newest stored turret aim once the [`TURRET_STATE_MIN_INTERVAL_MS`]
/// window has passed. The 1 s slave tick ([`regen_tick`] via `vehicles::active_region_tick`)
/// calls this, so an aim suppressed by the window still reaches the players around; an aim
/// that reverted to the one they already have is not relayed again.
pub fn flush_pending_turret_state(server: &GameServer, slave: &SlaveRef) {
    let (posture, sender) = {
        let mut d = slave.data();
        if !d.turret_relay_pending
            || now_ms() - d.turret_last_update_ms < TURRET_STATE_MIN_INTERVAL_MS
        {
            return;
        }
        d.turret_relay_pending = false;
        let sender = std::mem::take(&mut d.turret_pending_sender_obj_id);
        if d.turret_pitch == d.turret_relayed_pitch && d.turret_yaw == d.turret_relayed_yaw {
            return;
        }
        d.turret_relayed_pitch = d.turret_pitch;
        d.turret_relayed_yaw = d.turret_yaw;
        d.turret_last_update_ms = now_ms();
        (
            SCUnitModelPostureChangedPacket {
                obj_id: slave.obj_id,
                posture: crate::models::unit::ModelPostureType::TurretState,
                has_loot_left: false,
                anim_action_id: 0,
                activate_animation: false,
                turret_pitch: d.turret_pitch,
                turret_yaw: d.turret_yaw,
            },
            sender,
        )
    };
    let obj = WorldObject::from(slave);
    let Some(world) = world_of(server, &obj) else {
        return;
    };
    for viewer in world.players_around(&obj) {
        if viewer.obj_id != sender {
            viewer.send(posture);
        }
    }
}
