use crate::packets::world_objects::SCUnitAttachedPacket;
use crate::server::GameServer;
use crate::systems::skills::types::{BuffRemoveOn, SkillCastTarget};
use crate::world::{PlayerRef, SlaveRef, WorldInstance, WorldObject};

use super::super::harpoon;
use super::data::attach_point;
use super::packets::{
    SCSlaveBoundPacket, SCSlaveDespawnPacket, SCSlaveRemovedPacket, SCUnitDetachedPacket,
};
use super::{
    add_distance_to_front, add_distance_to_right, attach_reason, bind_gap, broadcast, buff, error,
    get_active_slave_by_owner_obj_id, get_is_mounted, get_slave_by_tl_id, is_cannon_kind,
    player_world, slave_kind, world_of,
};

// ---------------------------------------------------------------------------------------------
// Bind / unbind
// ---------------------------------------------------------------------------------------------

/// `ClearSlaveAttachmentState(character)`.
pub(super) fn clear_slave_attachment_state(server: &GameServer, character: &PlayerRef) {
    let me = WorldObject::from(character);
    if let Some(w) = world_of(server, &me) {
        w.detach_child(&me);
        w.set_sticky_parent(&me, None);
    }
    let mut c = character.data();
    c.is_riding = false;
    c.attached_point = attach_point::NONE;
}

fn trigger_remove_on(server: &GameServer, obj: &WorldObject, on: BuffRemoveOn) {
    crate::systems::skills::buffs::trigger_remove_on(server, obj, on, 0);
    crate::systems::dispatch_buff_events(server, obj);
}

/// `UnbindSlave(character, tlId, reason)`.
pub fn unbind_slave(server: &GameServer, character: &PlayerRef, tl_id: u32, reason: u8) {
    let me = WorldObject::from(character);
    let slave = player_world(server, character).and_then(|w| get_slave_by_tl_id(&w, tl_id));
    let Some(slave) = slave else {
        clear_slave_attachment_state(server, character);
        trigger_remove_on(server, &me, BuffRemoveOn::Unmount);
        broadcast(
            server,
            &me,
            &SCUnitDetachedPacket {
                obj_id: character.obj_id,
                reason,
            },
            true,
        );
        return;
    };
    // Find and remove the character's own seat under the slave's one lock: a re-bind between a
    // check and a separate `retain` could remove the seat the character moved to.
    let removed = {
        let mut d = slave.data();
        d.attached_characters
            .iter()
            .position(|(_, c)| *c == character.obj_id)
            .map(|i| d.attached_characters.remove(i))
    };
    if let Some((point, _)) = removed {
        if point == attach_point::DRIVER {
            super::ship_physics::set_driver(server, &slave, false);
        }
        clear_slave_attachment_state(server, character);
        harpoon::on_operator_left_slave(server, &slave);
    }
    trigger_remove_on(server, &me, BuffRemoveOn::Unmount);
    clear_slave_attachment_state(server, character);
    broadcast(
        server,
        &me,
        &SCUnitDetachedPacket {
            obj_id: character.obj_id,
            reason,
        },
        true,
    );
}

/// Why [`bind_slave`] refused a seat.
enum BindRefusal {
    Dead,
    TooFar,
    CannonPassenger,
    NoSuchSeat,
    SeatTaken,
    OwnersMark,
}

/// Surface gap the client's own bind reach checks allow: the 12076 driver bind uses
/// `FUN_39346290` -> `FUN_39093860(..., 16.0)` (strictly under 4.0 m); the 13863 passenger
/// interaction uses `FUN_393bf270` -> `FUN_39093860(..., 9.0)` (strictly under 3.0 m), widened
/// to 12.25 (3.5 m) when the target's `+0x3c26` flag is set. The S11 client work reads that flag
/// as "riding a mate" (capture-pending S11-C5), keyed on the character's `is_riding`; the
/// decompile reads `+0x3c26` from the target, so the flag's owner is not proven. This keeps the
/// S11 reading the passenger-seat task asked for.
pub fn passenger_reach(character: &PlayerRef) -> f32 {
    if character.data().is_riding {
        3.5
    } else {
        3.0
    }
}

/// `Vector3.Transform(value, Quaternion.Inverse(rotation))`.
/// `BindSlave(character, objId, attachPoint, bondKind)`.
pub fn bind_slave(
    server: &GameServer,
    character: &PlayerRef,
    obj_id: u32,
    point: u8,
    bond_kind: u8,
    reach: Option<f32>,
) {
    let Some(world) = player_world(server, character) else {
        return;
    };
    let Some(slave) = world.slave(obj_id) else {
        // The target vanished between the request and the bind: clear the pending request.
        super::bind_error(character, error::SLAVE_CANNOT_BIND);
        return;
    };
    // One slave at a time: the 1.2 client refuses a second slave bind with
    // `slave_unbind_first` 0x66 (`FUN_39346290`). A mate is not in a slave's
    // `attached_characters`, so the S11-C5 mate case stays open. `get_is_mounted` walks every
    // slave, so it runs before this slave's lock.
    if get_is_mounted(&world, character.obj_id).is_some() {
        tracing::debug!(
            "BindSlave: character {} is already on a slave; refusing seat {} of {}",
            character.obj_id,
            point,
            obj_id
        );
        super::bind_error(character, error::SLAVE_UNBIND_FIRST);
        return;
    }
    // A transfer passenger must unboard first; otherwise the slave seat and the transfer
    // seat would both claim the character (and an unboard would drop the slave attachment).
    if super::transfers::is_boarded(server, character) {
        tracing::debug!(
            "BindSlave: character {} is on a transfer; refusing seat {} of {}",
            character.obj_id,
            point,
            obj_id
        );
        super::bind_error(character, error::SLAVE_UNBIND_FIRST);
        return;
    }
    let me = WorldObject::from(character);
    // The caller decides the reach: 12076 passes 4.0, 13863 the passenger reach, and the
    // doodad BoardTransfer path passes `None` because its own doodad func already checked the
    // range. The gap is snapshotted before the slave lock (the player is a different object
    // lock); the comparison and the reservation run under that one lock.
    let gap = reach.map(|_| bind_gap(server, &world, character, &slave, point));
    let cannon = is_cannon_kind(slave_kind(server, &slave));
    // The 1.2 client only offers a seat the model declares (`model_bindings`, the same list
    // `FUN_393bbb80` scans); the driver point 1 keeps the C# rules. The model data is immutable,
    // so this snapshot needs no lock; the check and the reservation below still run under the
    // slave's one lock.
    let template_id = slave.data().unit.template_id;
    let seat_exists = server
        .game_data
        .systems
        .vehicles
        .slave_has_attach_point(template_id, point);
    // Check and reserve the seat under the slave's one lock, so two binds racing for the same
    // point cannot both pass.
    let refusal = {
        let mut d = slave.data();
        if d.unit.is_dead() {
            Some(BindRefusal::Dead)
        } else if reach.zip(gap).is_some_and(|(reach, gap)| gap >= reach) {
            Some(BindRefusal::TooFar)
        } else if point != attach_point::DRIVER && cannon {
            Some(BindRefusal::CannonPassenger)
        } else if point != attach_point::DRIVER && !seat_exists {
            Some(BindRefusal::NoSuchSeat)
        } else if d.has_attached_at(point) {
            Some(BindRefusal::SeatTaken)
        } else if point == attach_point::DRIVER
            && d.unit.buffs.check_buff(buff::OWNERS_MARK)
            && d.summoner_obj_id != character.obj_id
        {
            Some(BindRefusal::OwnersMark)
        } else {
            d.attached_characters.push((point, character.obj_id));
            None
        }
    };
    if let Some(refusal) = refusal {
        match refusal {
            BindRefusal::Dead => {
                tracing::debug!(
                    "BindSlave: slave {} is dead; ignoring seat {}",
                    obj_id,
                    point
                );
                // A refusal must answer: the client keeps its pending bind request until an
                // SCErrorMsg code in `FUN_393457e0`'s clear set arrives (S11-C7).
                super::bind_error(character, error::SLAVE_CANNOT_BIND);
            }
            BindRefusal::TooFar => {
                tracing::debug!(
                    "BindSlave: character {} is {:.3} m from slave {} (reach {:.3} m)",
                    character.obj_id,
                    gap.unwrap_or_default(),
                    obj_id,
                    reach.unwrap_or_default()
                );
                super::bind_error(character, error::TOO_FAR_AWAY);
            }
            BindRefusal::CannonPassenger => {
                tracing::debug!(
                    "BindSlave: slave {} is a cannon and has no passenger seat",
                    obj_id
                );
                super::bind_error(character, error::SLAVE_CANNOT_BIND);
            }
            BindRefusal::NoSuchSeat => {
                // A forged seat the model does not declare. The 1.2 client never offers it; the
                // original server's answer is capture-pending (S11-C2), but a refusal must still
                // clear the client's pending bind request.
                tracing::debug!(
                    "BindSlave: slave {} (template {}) has no seat point {}; ignoring",
                    obj_id,
                    template_id,
                    point
                );
                super::bind_error(character, error::SLAVE_CANNOT_BIND);
            }
            BindRefusal::SeatTaken => {
                // The C# returns silently; the original server's answer is capture-pending
                // (S11-C2). `slave_already_has_master` is the closest clearing message.
                tracing::debug!(
                    "BindSlave: seat {} of slave {} is taken; ignoring",
                    point,
                    obj_id
                );
                super::bind_error(character, error::SLAVE_ALREADY_HAS_MASTER);
            }
            BindRefusal::OwnersMark => {
                // The C# sends `SlaveAlreadyHasMaster` for this driver-seat refusal only.
                super::bind_error(character, error::SLAVE_ALREADY_HAS_MASTER);
            }
        }
        return;
    }
    broadcast(
        server,
        &me,
        &SCUnitAttachedPacket {
            child_obj_id: character.obj_id,
            point,
            reason: bond_kind,
            obj_id,
        },
        true,
    );
    character.data().attached_point = point;
    if point == attach_point::DRIVER {
        let char_id = character.data().id;
        broadcast(
            server,
            &me,
            &SCSlaveBoundPacket {
                master_id: char_id,
                slave_obj_id: obj_id,
            },
            true,
        );
    }
    if point == attach_point::DRIVER {
        super::ship_physics::set_driver(server, &slave, true);
    }
    world.attach_child(&WorldObject::from(&slave), &me);
    // Only a manned cannon moves its gunner behind the barrel; every other seat keeps the C#
    // origin placement.
    let (sx, sy, sz) = if is_manned_cannon(server, &slave) {
        gunner_seat_offset(server, &slave, point)
    } else {
        (0.0, 0.0, 0.0)
    };
    character
        .data()
        .unit
        .transform
        .set_position(sx, sy, sz, 0.0, 0.0, 0.0);
}

/// Gunner seat of a cannon in the cannon's local frame when its model has no seat helper. Cannon
/// model 117 (every 1.2 cannon slave) is absent from `slave_attach_points*.json`, which only
/// covers ship / vehicle models, and no cannon-shaped model there declares a driver helper, so
/// the gunner stands 0.8 m behind the barrel pivot at its base height.
pub const GUNNER_SEAT_OFFSET: (f32, f32, f32) = (0.0, -0.8, 0.0);

/// The gunner's seat position on a cannon: the model's own `point` helper when the attach point
/// data has one, else [`GUNNER_SEAT_OFFSET`].
fn gunner_seat_offset(server: &GameServer, cannon: &SlaveRef, point: u8) -> (f32, f32, f32) {
    let model_id = cannon.data().unit.model_id;
    server
        .systems
        .vehicles
        .attach_points_for_slave(server, model_id)
        .and_then(|points| points.into_iter().find(|(p, _)| *p == point))
        .map_or(GUNNER_SEAT_OFFSET, |(_, v)| (v.x, v.y, v.z))
}

/// A cannon (slave kind 8) mounted on another slave: one gunner at the driver seat, the
/// `mount_attached_skills` seat of its fire skill. A cannon without a bearer keeps the plain
/// C# bind rules.
pub fn is_manned_cannon(server: &GameServer, slave: &SlaveRef) -> bool {
    let (template_id, bearer) = {
        let d = slave.data();
        (d.unit.template_id, d.owner_obj_id)
    };
    bearer != 0
        && server
            .game_data
            .systems
            .vehicles
            .slave_template(template_id)
            .is_some_and(|t| t.slave_kind == super::data::slave_kind::SLAVE_EQUIPMENT)
}

/// Who may man a ship cannon: the cannon's summoner, the owner of the ship it is mounted on, or
/// anybody aboard that ship (standing on it or seated on any of its attach points). The C#
/// `BindSlave` has no such rule and the 1.2 data carries no seat permissions; this keeps the
/// guns to the crew.
pub fn may_man_cannon(character: &PlayerRef, cannon: &SlaveRef, world: &WorldInstance) -> bool {
    let (bearer_id, summoner, summoner_id) = {
        let d = cannon.data();
        (d.owner_obj_id, d.summoner_obj_id, d.summoner_id)
    };
    let is_me = |obj: u32, id: u32| {
        obj == character.obj_id || (character.character_id != 0 && id == character.character_id)
    };
    if is_me(summoner, summoner_id) {
        return true;
    }
    let Some(bearer) = world.slave(bearer_id) else {
        return true; // Orphaned: nobody to ask.
    };
    let (owner, attached) = {
        let d = bearer.data();
        (
            is_me(d.summoner_obj_id, d.summoner_id),
            d.attached_characters
                .iter()
                .any(|(_, c)| *c == character.obj_id),
        )
    };
    owner || attached || character.data().unit.transform.parent_obj_id == bearer_id
}

/// A character who dies leaves any slave seat through the same `unbind_slave` path as logout.
/// The C# only force-dismounted cannon gunners here, so a ship driver or passenger kept a stale
/// seat and the one-slave guard then refused every later bind; the Rust unbinds every seat.
pub fn on_character_death(server: &GameServer, player: &PlayerRef) {
    let Some(world) = player_world(server, player) else {
        return;
    };
    if let Some((slave, _)) = get_is_mounted(&world, player.obj_id) {
        let tl = u32::from(slave.data().unit.tl_id);
        unbind_slave(server, player, tl, attach_reason::NONE);
    }
}

/// Unbinds everybody riding the cannons / harpoons mounted on `slave` (their carrier goes away).
pub(super) fn unbind_child_riders(server: &GameServer, slave: &SlaveRef, reason: u8) {
    let Some(world) = world_of(server, &WorldObject::from(slave)) else {
        return;
    };
    let children = slave.data().attached_slaves.clone();
    for id in children {
        let Some(child) = world.slave(id) else {
            continue;
        };
        unbind_riders_of(server, &child, reason);
    }
}

pub(super) fn unbind_riders_of(server: &GameServer, slave: &SlaveRef, reason: u8) {
    let (tl, riders) = {
        let d = slave.data();
        (u32::from(d.unit.tl_id), d.attached_characters.clone())
    };
    for (_, c) in riders {
        if let Some(p) = server.world.character_by_obj_id(c) {
            unbind_slave(server, &p, tl, reason);
        }
    }
}

/// `BindSlave(connection, tlId)` (`CSBindSlavePacket`). The 1.2 packet body is only the target
/// timeline id (`FUN_393464a0` writes `target+0x41c`; `FUN_39346290` holds the reach / riding
/// checks) and its only builder is skill 12076 "occupy" (`FUN_393bf310` -> `FUN_39362f50`), so
/// this path always asks for driver point 1. The passenger seat is skill 13863 -> `AttachTo`
/// (`special_effects` 2378 value1 2), which reaches [`bind_slave`] directly.
pub fn bind_slave_by_tl(server: &GameServer, character: &PlayerRef, tl_id: u32) {
    let Some(world) = player_world(server, character) else {
        return;
    };
    let Some(slave) = get_slave_by_tl_id(&world, tl_id) else {
        // The target despawned between the request and the bind: still clear the client's
        // pending bind request, or every later F press shows "already requested" (S11-C7).
        super::bind_error(character, error::SLAVE_CANNOT_BIND);
        return;
    };
    if slave.data().unit.is_dead() {
        super::bind_error(character, error::SLAVE_CANNOT_BIND);
        return;
    }
    if is_manned_cannon(server, &slave) && !may_man_cannon(character, &slave, &world) {
        super::bind_error(character, error::SLAVE_CANNOT_BIND);
        return;
    }
    bind_slave(
        server,
        character,
        slave.obj_id,
        attach_point::DRIVER,
        attach_reason::NEW_MASTER,
        Some(4.0),
    );
}

/// `RidersEscape(player, positionTarget)`.
pub fn riders_escape(server: &GameServer, player: &PlayerRef, target: &SkillCastTarget) {
    let SkillCastTarget::Position { x, y, z, rot, .. } = *target else {
        return;
    };
    let Some(world) = player_world(server, player) else {
        return;
    };
    let Some(slave) = get_active_slave_by_owner_obj_id(&world, player.obj_id) else {
        tracing::warn!(
            "{} using Rider's Escape with no slave active!",
            player.data().unit.name
        );
        return;
    };
    let obj = WorldObject::from(&slave);
    let tl = slave.data().unit.tl_id;
    broadcast(
        server,
        &obj,
        &SCSlaveDespawnPacket {
            obj_id: slave.obj_id,
        },
        true,
    );
    broadcast(
        server,
        &obj,
        &SCSlaveRemovedPacket {
            obj_id: slave.obj_id,
            tl_id: tl,
        },
        true,
    );
    // mySlave.SendPacket(SCUnitsRemovedPacket): a slave has no connection.
    let (sx, sy) = server
        .game_data
        .systems
        .vehicles
        .slave_template(slave.data().unit.template_id)
        .map_or((0.0, 0.0), |t| (t.spawn_x_offset, t.spawn_y_offset));
    {
        let mut d = slave.data();
        let t = &mut d.unit.transform;
        t.set_position(x, y, z, 0.0, 0.0, rot);
        add_distance_to_front(t, sx / 2.0);
        add_distance_to_right(t, sy / 2.0);
    }
    world.hide(&obj);
    let changes = world.spawn(&obj, &server.view_ctx());
    crate::world::session::handle_zone_changes(server, changes);
}

/// `Slave.OnZoneChange(last, new)`: every passenger's zone change.
pub fn on_zone_change(server: &GameServer, slave: &SlaveRef, last_zone: u32, new_zone: u32) {
    let attached = slave.data().attached_characters.clone();
    for (_, c) in attached {
        if let Some(p) = server.world.character_by_obj_id(c) {
            crate::world::session::on_zone_change(server, &p, last_zone, new_zone);
        }
    }
}
