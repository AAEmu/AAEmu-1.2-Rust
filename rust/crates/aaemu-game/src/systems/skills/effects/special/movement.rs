//! Special effect actions that move units: `Blink`, `TeleportToUnit`, `KnockBack`, `Escape`,
//! `Return`, plus the server-side NPC displacement they share with `ImpulseEffect`.
//!
//! NPC displacements snap the end position to the ground when `GeoData.GetHeight` (the npcs
//! subsystem's heightmaps; the navmesh path is not ported) is positive and within 5 m.

use crate::data::world_data::Portal;
use crate::data::world_templates::WorldSpawnPosition;
use crate::models::character::DEFAULT_INSTANCE_ID;
use crate::models::movement::{
    convert_radian_to_direction, MoveKind, MoveType, MoveTypeFlags, UnitMove,
};
use crate::models::transform::PositionAndRotation;
use crate::packets::world::{SCErrorMsgPacket, SCOneUnitMovementPacket};
use crate::server::GameServer;
use crate::systems::economy::error_message as err;
use crate::systems::skills::ground_snap_rules as ground_snap;
use crate::world::{now_ms, NpcRef, PlayerRef, WorldObject};

use super::super::super::broadcast;
use super::super::super::geometry::{world_of, world_pos, Target};
use super::super::super::packets_special::{
    SCBlinkUnitPacket, SCCharacterResurrectedPacket, SCLoadInstancePacket, SCTeleportUnitPacket,
    TELEPORT_REASON_MOVE_TO_LOCATION,
};
use super::super::{rand_int, EffectArgs};
use super::log_values;

const TWO_PI: f32 = std::f32::consts::PI * 2.0;

/// `NumericExtensions.RadToDeg(float)`.
pub(crate) fn rad_to_deg(v: f32) -> f32 {
    v / std::f32::consts::PI * 180.0
}

/// `NumericExtensions.DegToRad(float)`.
pub(crate) fn deg_to_rad(v: f32) -> f32 {
    std::f32::consts::PI / 180.0 * v
}

/// `MathUtil.AddDistanceToFrontDeg(distance, x, y, deg)`.
pub(crate) fn add_distance_to_front_deg(distance: f32, x: f32, y: f32, deg: f32) -> (f32, f32) {
    let rad = f64::from(deg) * std::f64::consts::PI / 180.0;
    (
        distance * rad.cos() as f32 + x,
        distance * rad.sin() as f32 + y,
    )
}

/// `PositionAndRotation.AddDistanceToFront(distance)` (horizontal only).
fn add_distance_to_front(p: &mut PositionAndRotation, distance: f32) {
    p.x += -distance * p.rot_z.sin();
    p.y += distance * p.rot_z.cos();
}

/// `ms since midnight` (`(uint)(DateTime.UtcNow - DateTime.UtcNow.Date).TotalMilliseconds`).
fn time_of_day_ms() -> u32 {
    (now_ms().rem_euclid(86_400_000)) as u32
}

/// The server-side NPC displacement of `Blink` / `TeleportToUnit` / `KnockBack` /
/// `ImpulseEffect`: optional `DisplacedUntil = now + 600 ms`, `Transform.Local.SetPosition`,
/// optional `SCBlinkUnitPacket`, the stopping `SCOneUnitMovementPacket` and
/// `CheckMovedPosition` (`AddVisibleObject`; nothing subscribes to `Events.OnMovement`).
pub(crate) fn move_npc(
    server: &GameServer,
    npc: &NpcRef,
    end: (f32, f32, f32),
    blink: Option<(f32, f32)>,
    displaced: bool,
) {
    let obj = WorldObject::from(npc);
    let (mut x, mut y, mut z) = end;
    let instance_id = obj.transform().instance_id;
    let start = world_pos(server, &Target::Obj(obj.clone()));
    // Displacements (knockback, impulse, explosion): the zone moves the unit's physics
    // (WZKnockBackUnit 0x39258BB0 / X2::Unit::Impulse 0x3925C7D0), so it stops at walls and
    // lands on the ground: `GetBlinkPosition` along the push, then the ground under the end
    // point (collision service).
    let physics_end = if displaced {
        crate::systems::ai::collision::displacement::clip_move(
            server,
            instance_id,
            [start.x, start.y, start.z],
            [x, y, start.z],
        )
        .and_then(|p| {
            let g = crate::systems::ai::collision::ground_height(
                server,
                instance_id,
                p[0],
                p[1],
                z.max(p[2]),
            )?;
            Some((p[0], p[1], g))
        })
    } else {
        None
    };
    if let Some(p) = physics_end {
        (x, y, z) = p;
    } else if let Some(world) = world_of(server, &obj) {
        // `GeoData.GetHeight(end)`: snap to the ground when it is within 5 m.
        let ground_z = crate::systems::npcs::npc::spawner::geo_data_height(server, &world, x, y);
        if ground_z > 0.0 && (z - ground_z).abs() < 5.0 {
            z = ground_z;
        }
    }
    let (rot_z, in_battle) = {
        let mut d = npc.data();
        if displaced {
            d.unit.skill_state.displaced_until_ms = now_ms() + 600;
        }
        d.unit.transform.set_xyz(x, y, z);
        (d.unit.transform.rot_z, d.unit.is_in_battle)
    };
    if let Some((distance, degree)) = blink {
        broadcast(
            server,
            &obj,
            &SCBlinkUnitPacket {
                obj_id: obj.obj_id(),
                distance,
                degree,
                x,
                y,
                z,
            },
            false,
        );
    }
    let (stance, alertness) = server.systems.skills.ext.npcs().npc_move_state(server, npc);
    let mut flags = MoveTypeFlags::STOPPING;
    if in_battle {
        flags = flags | MoveTypeFlags::IN_COMBAT;
    }
    let mut move_type = MoveType::new(MoveKind::Unit(UnitMove {
        x,
        y,
        z,
        rotation_z: convert_radian_to_direction(f64::from(rot_z - TWO_PI)),
        stance,
        alertness,
        ..UnitMove::default()
    }));
    move_type.flags = flags;
    move_type.time = time_of_day_ms();
    broadcast(
        server,
        &obj,
        &SCOneUnitMovementPacket {
            obj_id: obj.obj_id(),
            move_type,
        },
        false,
    );
    if let Some(world) = world_of(server, &obj) {
        let changes = world.add_visible_object(&obj, &server.view_ctx());
        crate::world::session::handle_zone_changes(server, changes);
    }
}

/// `Blink`: characters get `SCBlinkUnitPacket` (the client moves itself, mates are dismounted
/// first); NPCs are moved server side.
pub fn blink(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    tracing::debug!(
        "Special effects: Blink caster={:?} value1={}, value2={}, value3={}, value4={}",
        args.caster.as_ref().map(WorldObject::obj_id),
        v[0],
        v[1],
        v[2],
        v[3]
    );
    let Some(caster) = &args.caster else {
        return;
    };
    if let Some(p) = caster.as_player() {
        let mut new_pos = world_pos(server, &Target::Obj(caster.clone()));
        add_distance_to_front(&mut new_pos, v[0] as f32);
        blink_end(server, caster, &mut new_pos);
        server
            .systems
            .skills
            .ext
            .npcs()
            .unmount_active_mates(server, p);
        p.send(SCBlinkUnitPacket {
            obj_id: caster.obj_id(),
            distance: v[0] as f32,
            degree: v[1] as f32,
            x: new_pos.x,
            y: new_pos.y,
            z: new_pos.z,
        });
    } else if let Some(npc) = caster.as_npc() {
        let mut new_pos = world_pos(server, &Target::Obj(caster.clone()));
        add_distance_to_front(&mut new_pos, v[0] as f32);
        blink_end(server, caster, &mut new_pos);
        move_npc(
            server,
            npc,
            (new_pos.x, new_pos.y, new_pos.z),
            Some((v[0] as f32, v[1] as f32)),
            false,
        );
    }
}

/// `WZBlinkUnit` (x2game 0x39369B10 → `GetBlinkPosition` 0x3925E670): the zone sweeps the
/// unit's collider from its position towards the blink target against the terrain
/// (`entTypes 256`), stopping 0.1 m before a contact and stepping down 30° slopes. Without the
/// collision service the target is kept (C#).
fn blink_end(server: &GameServer, unit: &WorldObject, target: &mut PositionAndRotation) {
    let from = world_pos(server, &Target::Obj(unit.clone()));
    let dir = [target.x - from.x, target.y - from.y, target.z - from.z];
    let dist = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
    if dist <= 0.0 {
        return;
    }
    let instance_id = unit.transform().instance_id;
    // the unit's living-entity collider (sub_3925E670), the default capsule without a model
    let collider = match unit.with_unit(|u| u.unit().model_id) {
        Some(m) => crate::systems::ai::collision::displacement::Collider::from_living(
            &crate::systems::ai::collision::living::unit_params(server, m),
        ),
        None => crate::systems::ai::collision::displacement::Collider::default(),
    };
    if let Some(p) = crate::systems::ai::collision::displacement::blink_position(
        server,
        instance_id,
        [from.x, from.y, from.z],
        dir,
        dist,
        false,
        crate::systems::ai::collision::displacement::ENT_TYPES_BLINK_UNIT,
        false,
        &collider,
    ) {
        target.x = p[0];
        target.y = p[1];
        target.z = p[2];
    }
}

/// `TeleportToUnit`: places the caster `value1..value2` mm from the target at
/// `value3..value4` degrees relative to its facing.
pub fn teleport_to_unit(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    tracing::debug!(
        "Special effects: TeleportToUnit caster={:?} target={} value1 {}, value2 {}, value3 {}, value4 {}",
        args.caster.as_ref().map(WorldObject::obj_id),
        args.target.obj_id(),
        v[0],
        v[1],
        v[2],
        v[3]
    );
    let (Some(distance), Some(rotation_degrees)) = (rand_int(v[0], v[1]), rand_int(v[2], v[3]))
    else {
        tracing::error!(
            "Special effects: TeleportToUnit invalid ranges {:?} (ArgumentOutOfRangeException)",
            v
        );
        return;
    };
    let target_position = world_pos(server, &args.target);
    let (end_x, end_y) = add_distance_to_front_deg(
        distance as f32 / 1000.0,
        target_position.x,
        target_position.y,
        rad_to_deg(target_position.rot_z) + 90.0 + rotation_degrees as f32,
    );
    let Some(caster) = &args.caster else {
        return;
    };
    // the ground at the end point (collision service; else the target's z as in C#)
    let end_z = crate::systems::ai::collision::ground_height_or(
        server,
        caster.transform().instance_id,
        end_x,
        end_y,
        target_position.z,
        || target_position.z,
    );
    if let Some(p) = caster.as_player() {
        p.send(SCBlinkUnitPacket {
            obj_id: caster.obj_id(),
            distance: 0.0,
            degree: 0.0,
            x: end_x,
            y: end_y,
            z: end_z,
        });
    } else if let Some(npc) = caster.as_npc() {
        move_npc(server, npc, (end_x, end_y, end_z), Some((0.0, 0.0)), false);
    }
}

/// `target.Buffs.HasEffectsMatchingCondition(e => e.Template.KnockbackImmune ||
/// e.Template.NonPushable)`.
pub(crate) fn has_knockback_immunity(server: &GameServer, target: &WorldObject) -> bool {
    let data = &server.game_data.systems.skills;
    let ids: Vec<u32> =
        target.with_base_unit(|b| b.buffs().effects.iter().map(|e| e.template.id).collect());
    ids.into_iter().any(|id| {
        data.buff(id)
            .is_some_and(|t| t.knockback_immune || t.non_pushable)
    })
}

/// `npc.Template.NonPushableByActor` of an NPC target.
pub(crate) fn npc_non_pushable(server: &GameServer, target: &WorldObject) -> bool {
    target.as_npc().is_some_and(|n| {
        server
            .systems
            .skills
            .ext
            .npcs()
            .npc_non_pushable_by_actor(server, n)
    })
}

/// `KnockBack`: pushes an NPC target `value1` mm (default 3 m) away from the caster, lifted by
/// `value3` mm. Characters are left to the client (a server packet doubles the displacement).
pub fn knock_back(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    tracing::debug!(
        "Special effects: KnockBack caster={:?}, target={}, value1={}, value2={}, value3={}, value4={}",
        args.caster.as_ref().map(WorldObject::obj_id),
        args.target.obj_id(),
        v[0],
        v[1],
        v[2],
        v[3]
    );
    let Some(caster) = &args.caster else {
        return;
    };
    let Some(target) = args.target.as_obj() else {
        // A position unit has no buffs and is neither a character nor an NPC: nothing moves.
        return;
    };
    if has_knockback_immunity(server, target) || npc_non_pushable(server, target) {
        return;
    }
    let mut knock_dist = v[0] as f32 / 1000.0;
    if knock_dist <= 0.0 {
        knock_dist = 3.0;
    }
    let caster_pos = world_pos(server, &Target::Obj(caster.clone()));
    let target_pos = world_pos(server, &args.target);
    let mut dir = (target_pos.x - caster_pos.x, target_pos.y - caster_pos.y);
    if dir.0 * dir.0 + dir.1 * dir.1 < 0.01 {
        dir = (1.0, 0.0);
    }
    let len = (dir.0 * dir.0 + dir.1 * dir.1).sqrt();
    let dir = (dir.0 / len, dir.1 / len);
    let end = (
        target_pos.x + dir.0 * knock_dist,
        target_pos.y + dir.1 * knock_dist,
        target_pos.z + v[2] as f32 / 1000.0,
    );
    if target.as_player().is_some() {
        return;
    }

    if let Some(npc) = target.as_npc() {
        move_npc(server, npc, end, None, true);
    }
}

/// `MoveToGround` (special type 73): lands a unit on the terrain floor, at the selected position
/// when the target is a position unit and at the caster's own XY otherwise.
///
/// Ported from the C# `MoveToGround` / `GroundSnapRules` of commit `c78b3a659`; the snap policy
/// (0.4 m clearance, water surface over the seabed, the 3 m near band, the 8 m / 12 m lift / drop
/// caps) is [`ground_snap`]. A row with no reachable ground sample (heightmaps off, missing cell)
/// moves nothing. The 1.2 row 9639 is `[0, 0, 0, 0]`, and `value1` names no unit on the reachable
/// 10.x rows either, so no value is applied.
pub fn move_to_ground(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "MoveToGround", v);
    let Some(caster) = &args.caster else {
        return;
    };
    let current = world_pos(server, &Target::Obj(caster.clone()));
    // The raw Z is the selected position's own height when the cast carries one, so the lift /
    // drop caps measure the gap at the destination; a position without a height (0) falls back
    // to the caster's Z rather than dropping the unit to sea level.
    let (x, y, raw_z) = match &args.target {
        Target::Pos(p) if p.pos.z > 0.0 => (p.pos.x, p.pos.y, p.pos.z),
        Target::Pos(p) => (p.pos.x, p.pos.y, current.z),
        Target::Obj(_) => (current.x, current.y, current.z),
    };
    let instance_id = caster.transform().instance_id;
    let ground =
        crate::systems::ai::collision::ground_height_or(server, instance_id, x, y, raw_z, || {
            crate::systems::ai::collision::terrain_height_or(server, instance_id, x, y, || 0.0)
        });
    let over_water = crate::systems::ai::collision::is_water(server, instance_id, x, y, raw_z);
    let water_surface =
        crate::systems::ai::collision::water_level(server, instance_id, x, y, raw_z).unwrap_or(0.0);
    let Some(z) = ground_snap::try_get_floor_z(raw_z, ground, over_water, water_surface) else {
        return;
    };
    if let Some(p) = caster.as_player() {
        // As `Blink`: only `SCBlinkUnitPacket`; the client moves itself and its movement report
        // carries the new position back to the server.
        p.send(SCBlinkUnitPacket {
            obj_id: caster.obj_id(),
            distance: 0.0,
            degree: 0.0,
            x,
            y,
            z,
        });
    } else if let Some(npc) = caster.as_npc() {
        move_npc(server, npc, (x, y, z), Some((0.0, 0.0)), false);
    }
}

/// `Escape`: a character is "resurrected" (`SCCharacterResurrectedPacket`) at the instance's
/// spawn position or, in the main world, at the closest return point.
///
pub fn escape(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let Some(caster) = &args.caster else {
        return;
    };
    let Some(p) = caster.as_player() else {
        return;
    };
    tracing::debug!(
        "Special effects: Escape value1 {}, value2 {}, value3 {}, value4 {}",
        v[0],
        v[1],
        v[2],
        v[3]
    );
    let instance_id = p.data().unit.transform.instance_id;
    let (x, y, z, z_rot) = if DEFAULT_INSTANCE_ID != instance_id {
        let Some(w) = world_of(server, caster) else {
            return;
        };
        let sp = w.template.spawn_position;
        (sp.x, sp.y, sp.z, sp.yaw)
    } else {
        match server
            .systems
            .skills
            .ext
            .npcs()
            .closest_return_portal(server, p)
        {
            Some(portal) => (portal.x, portal.y, portal.z, portal.z_rot),
            None => {
                // GetClosestReturnPortal's fail-safe portal (current position, ZRot 0).
                let pos = world_pos(server, &Target::Obj(caster.clone()));
                (pos.x, pos.y, pos.z, 0.0)
            }
        }
    };
    broadcast(
        server,
        caster,
        &SCCharacterResurrectedPacket {
            unit_id: caster.obj_id(),
            x,
            y,
            z,
            z_rot,
        },
        true,
    );
}

/// The Escape buff4840 Timeout trigger moves its owner to the closest resurrection point.
pub fn move_to_rez_point(server: &GameServer, args: &EffectArgs<'_>) {
    let Some(target) = args.target.as_obj().filter(|u| u.as_player().is_some()) else {
        return;
    };
    let mut args = args.clone();
    args.caster = Some(target.clone());
    escape(server, &args, [0; 4]);
}

/// `PortalManager.GetRecallById(returnPointId)`.
fn recall_by_id(server: &GameServer, id: u32) -> Option<Portal> {
    server
        .game_data
        .portals
        .recalls
        .values()
        .flatten()
        .find(|p| p.id == id)
        .cloned()
}

/// `character.Transform = new Transform(character, null, zoneId, instanceId, x, y, z, roll,
/// pitch, yaw)` (the world parent link is left to the world code).
fn set_character_transform(
    p: &PlayerRef,
    instance_id: u32,
    zone_id: u32,
    pos: PositionAndRotation,
) {
    let mut c = p.data();
    let t = &mut c.unit.transform;
    t.instance_id = instance_id;
    t.zone_id = zone_id;
    t.set_position(pos.x, pos.y, pos.z, pos.rot_x, pos.rot_y, pos.rot_z);
}

/// Whether the world instance `instance_id` has a zone with key `zone_key`.
fn zone_in_instance_world(server: &GameServer, instance_id: u32, zone_key: u32) -> bool {
    server
        .world
        .worlds()
        .iter()
        .find(|w| w.id == instance_id)
        .is_some_and(|w| {
            w.template
                .xml_world_zones()
                .values()
                .any(|z| z.zone_key == zone_key)
        })
}

/// A zone that exists only in instance worlds (not in `main_world`).
fn is_instance_only_zone(server: &GameServer, zone_key: u32) -> bool {
    let worlds = &server.game_data.worlds;
    let in_main = worlds
        .get_world_template_by_name("main_world")
        .is_some_and(|m| m.xml_world_zones().values().any(|z| z.zone_key == zone_key));
    !in_main
        && worlds
            .templates()
            .iter()
            .any(|t| t.xml_world_zones().values().any(|z| z.zone_key == zone_key))
}

/// The "Need to add information to worldgates.json" hint for a missing gate id.
fn log_missing_gate(server: &GameServer, p: &PlayerRef, id: i32) {
    let tr = p.data().unit.transform;
    let pos = world_pos(server, &Target::Obj(WorldObject::from(p)));
    let sub_zone_id = p.data().sub_zone_id;
    tracing::info!(
        "Return: Need to add information to worldgates.json:
        \"Id\": {}
        \"ZoneId\": {},
        \"X\": {},
        \"Y\": {},
        \"Z\": {},
        \"Yaw\": {},
        \"SubZoneId\": {}
The coordinates need to be set correctly, these are just an example.",
        id,
        tr.zone_id,
        pos.x,
        pos.y,
        pos.z,
        pos.rot_z,
        sub_zone_id
    );
}

/// A gate of the caster's own instance world: keep the instance, move to the gate (zone change
/// hooks fire like for a spawn position) and send only `SCTeleportUnit` (no `SCLoadInstance`).
fn move_within_instance(server: &GameServer, p: &PlayerRef, t: &Portal) {
    let yaw = deg_to_rad(t.yaw);
    let wsp = WorldSpawnPosition {
        zone_id: t.zone_id,
        x: t.x,
        y: t.y,
        z: t.z,
        yaw,
        ..WorldSpawnPosition::default()
    };
    crate::systems::instances::worlds::apply_world_spawn_position(server, p, &wsp, 0);
    p.data().set_disabled_set_position(true);
    p.send(SCTeleportUnitPacket {
        reason: TELEPORT_REASON_MOVE_TO_LOCATION,
        error_message: 0,
        x: t.x,
        y: t.y,
        z: t.z,
        z2: yaw,
    });
    // The transform already moved. Re-file it now so TeleportEnded's repaint uses the
    // arrival neighbourhood and both sets of observers are updated without client movement.
    let instance_id = p.data().unit.transform.instance_id;
    if let Some(world) = server.world.world(instance_id) {
        let changes = world.add_visible_object(p, &server.view_ctx());
        crate::world::session::handle_zone_changes(server, changes);
    }
}

/// `Return` (recall / world gate return): `SCLoadInstancePacket` + `SCTeleportUnitPacket`.
///
/// The instance and rotation in both notifications match the resulting transform.
pub fn return_(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if super::caster_is_player(args) {
        tracing::info!(
            "Special effects: Return value1 {}, value2 {}, value3 {}, value4 {}",
            v[0],
            v[1],
            v[2],
            v[3]
        );
    }
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        return;
    };
    let npcs = server.systems.skills.ext.npcs();
    let instance_id = p.data().unit.transform.instance_id;
    let in_instance = instance_id != DEFAULT_INSTANCE_ID;
    let trp = if v[0] == 0 {
        // Memory Tome for Recall skill
        let (district, faction) = {
            let c = p.data();
            (c.return_district_id, c.unit.faction_id)
        };
        let return_point_id = server
            .game_data
            .portals
            .district_return_point(district, faction);
        let trp = recall_by_id(server, return_point_id);
        if return_point_id == 0 {
            return;
        }
        trp
    } else {
        // Worldgates
        let trp = npcs.world_gate(server, v[0] as u32);
        if trp.is_none() {
            tracing::warn!("Cannot find return WorldGate with id {}", v[0]);
        }
        // Deviation from C# (its Return.cs is a TODO placeholder that fell back to
        // `MainWorldPosition`): a gate is only used in the world its zone belongs to.
        let trp = trp.filter(|t| {
            let usable = if in_instance {
                zone_in_instance_world(server, instance_id, t.zone_id)
            } else {
                !is_instance_only_zone(server, t.zone_id)
            };
            if !usable {
                tracing::warn!(
                    "Return WorldGate {} (zone {}) does not belong to the caster's world (instance {})",
                    v[0],
                    t.zone_id,
                    instance_id
                );
            }
            usable
        });
        if trp.is_none() && in_instance {
            // Never eject the player to the main world: stay put.
            log_missing_gate(server, p, v[0]);
            p.send(SCErrorMsgPacket::new(err::NO_RETURN_POINT, 0, true));
            return;
        }
        if let Some(t) = &trp {
            if in_instance {
                if !crate::world::session::zone_access_allowed(
                    server,
                    p,
                    instance_id,
                    t.zone_id,
                    t.x,
                    t.y,
                ) {
                    p.send(SCErrorMsgPacket::new(
                        crate::world::session::CLOSED_ZONE,
                        0,
                        true,
                    ));
                    return;
                }
                move_within_instance(server, p, t);
                return;
            }
        }
        trp
    };
    if let Some(t) = &trp {
        if !crate::world::session::zone_access_allowed(
            server,
            p,
            DEFAULT_INSTANCE_ID,
            t.zone_id,
            t.x,
            t.y,
        ) {
            p.send(SCErrorMsgPacket::new(
                crate::world::session::CLOSED_ZONE,
                0,
                true,
            ));
            return;
        }
        p.data().set_disabled_set_position(true);
        p.send(SCLoadInstancePacket {
            instance_id: DEFAULT_INSTANCE_ID,
            zone_id: t.zone_id,
            x: t.x,
            y: t.y,
            z: t.z,
            ang_x: 0.0,
            ang_y: 0.0,
            ang_z: deg_to_rad(t.yaw),
        });
        set_character_transform(
            p,
            0,
            t.zone_id,
            PositionAndRotation::new(t.x, t.y, t.z, 0.0, 0.0, deg_to_rad(t.yaw)),
        );
    } else if let Some(m) = npcs.main_world_position(server, p).filter(|_| !in_instance) {
        if !crate::world::session::zone_access_allowed(
            server,
            p,
            m.instance_id,
            m.zone_id,
            m.pos.x,
            m.pos.y,
        ) {
            p.send(SCErrorMsgPacket::new(
                crate::world::session::CLOSED_ZONE,
                0,
                true,
            ));
            return;
        }
        p.data().set_disabled_set_position(true);
        p.send(SCLoadInstancePacket {
            instance_id: m.instance_id,
            zone_id: m.zone_id,
            x: m.pos.x,
            y: m.pos.y,
            z: m.pos.z,
            ang_x: m.pos.rot_x,
            ang_y: m.pos.rot_y,
            ang_z: m.pos.rot_z,
        });
        set_character_transform(p, m.instance_id, m.zone_id, m.pos);
        p.send(SCTeleportUnitPacket {
            reason: TELEPORT_REASON_MOVE_TO_LOCATION,
            error_message: 0,
            x: m.pos.x,
            y: m.pos.y,
            z: m.pos.z,
            z2: m.pos.rot_z,
        });
        return;
    }
    let Some(t) = trp else {
        log_missing_gate(server, p, v[0]);
        return;
    };
    p.data().set_disabled_set_position(true);
    p.send(SCTeleportUnitPacket {
        reason: TELEPORT_REASON_MOVE_TO_LOCATION,
        error_message: 0,
        x: t.x,
        y: t.y,
        z: t.z,
        z2: deg_to_rad(t.yaw),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_deg_matches_mathutil() {
        let (x, y) = add_distance_to_front_deg(2.0, 10.0, 20.0, 90.0);
        assert!((x - 10.0).abs() < 1e-5);
        assert!((y - 22.0).abs() < 1e-5);
        let (x, y) = add_distance_to_front_deg(1.0, 0.0, 0.0, 0.0);
        assert!((x - 1.0).abs() < 1e-6 && y.abs() < 1e-6);
    }

    #[test]
    fn deg_rad_round_trip() {
        assert!((rad_to_deg(std::f32::consts::PI) - 180.0).abs() < 1e-4);
        assert!((deg_to_rad(180.0) - std::f32::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn front_uses_yaw_facing_plus_y() {
        let mut p = PositionAndRotation::new(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        add_distance_to_front(&mut p, 5.0);
        assert!(p.x.abs() < 1e-6 && (p.y - 5.0).abs() < 1e-6);
    }
}
