//! World funcs: `DoodadFuncBinding` (recall point), `Attachment` (seats / beds / boarding),
//! and the instance funcs (`EnterInstance`, `EnterSysInstance`, `ExitIndun`, `RemoveInstance`).
//!
//! The instance funcs call `systems::instances` (`IndunManager`) and `TrialManager`;
//! `SlaveManager.BindSlave` is the vehicles subsystem's.

use super::{error_message as err, send_debug_message, send_error, set_to_next_phase};
use crate::models::doodad::BondDoodad;
use crate::models::transform::{PositionAndRotation, Quaternion};
use crate::packets::world::SCCharacterReturnDistrictsPacket;
use crate::systems::instances::packets::{SCSystemDungeonStatePacket, SystemDungeonChannel};
use crate::systems::npcs::doodad::packets::SCBondDoodadPacket;
use crate::systems::npcs::doodad::runtime::DoodadCtx;
use crate::systems::npcs::doodad::templates::{
    bond_kind, DoodadFuncAttachment, DoodadFuncBinding, DoodadFuncEnterInstance,
    DoodadFuncEnterSysInstance, DoodadFuncExitIndun, DoodadFuncRemoveInstance, FuncTemplate,
};
use crate::systems::npcs::portal;
use crate::world::{DoodadRef, WorldInstance, WorldObject};

/// `DoodadFuncBinding.Use`: sets the recall point of the caster.
pub fn binding(f: &DoodadFuncBinding, ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>) {
    let Some(player) = DoodadCtx::as_player(caster) else {
        return;
    };
    let server = ctx.server;
    let (faction_id, sub_zone_id) = {
        let c = player.data();
        (c.unit.faction_id, c.sub_zone_id)
    };
    let return_point_id = portal::district_return_point(server, f.district_id, faction_id);
    let msg = format!(
        "DoodadFuncBinding: DistrictId {} ==> ReturnPointId {return_point_id}, SubZonesId {sub_zone_id}",
        f.district_id
    );
    tracing::trace!("{msg}");
    send_debug_message(server, player, &msg);
    if return_point_id == 0 {
        return;
    }
    if let Some(portal) = portal::recall_by_id(server, return_point_id) {
        let portals = {
            let mut c = player.data();
            c.return_district_id = f.district_id;
            c.portals.district_portals.clone()
        };
        player.send(SCCharacterReturnDistrictsPacket {
            portals: portals
                .iter()
                .map(|p| portal::portal_wire(server, p))
                .collect(),
            return_district_id: portal.id,
        });
        let msg = format!(
            "DoodadFuncBinding: ReturnPointId {return_point_id} ==> Portal.Id {}",
            portal.id
        );
        tracing::trace!("{msg}");
        send_debug_message(server, player, &msg);
    } else {
        let msg = format!(
            "DoodadFuncBinding: Recall point {} not found!",
            f.district_id
        );
        tracing::warn!("{msg}");
        send_debug_message(server, player, &msg);
    }
    set_to_next_phase(ctx, true);
}

/// `DoodadFuncAttachment.Use`.
pub fn attachment(f: &DoodadFuncAttachment, ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>) {
    tracing::trace!("DoodadFuncAttachment");
    let Some(player) = DoodadCtx::as_player(caster) else {
        return;
    };
    // A transfer passenger must leave the carriage before taking a bench / ship seat.
    if crate::systems::vehicles::transfers::is_boarded(ctx.server, player) {
        crate::systems::vehicles::bind_error(
            player,
            crate::systems::economy::error_message::SLAVE_UNBIND_FIRST,
        );
        return;
    }
    if f.bond_kind_id > bond_kind::BOND_INVALID {
        let character_id = player.data().id;
        // ask for a free meta number for landing
        let spot = ctx
            .doodad
            .data()
            .seat
            .load_passenger(character_id, ctx.doodad.obj_id, f.space);
        if spot == -1 {
            return; // we leave if there is no place
        }
        let bond = BondDoodad {
            owner_obj_id: ctx.doodad.obj_id,
            attach_point: f.attach_point_id,
            kind: f.bond_kind_id as u8,
            space: f.space,
            spot,
        };
        player.data().bonding = Some(bond);
        crate::world::broadcast(
            &ctx.world,
            player,
            &SCBondDoodadPacket {
                unit_obj_id: player.obj_id,
                bond,
            },
            true,
        );
        // `VehicleSeat.AddSeat` (Parent = null, StickyParent = the seat doodad) then
        // `StickyParent = owner.StickyParent; Parent = owner.Transform`.
        let me = WorldObject::from(player);
        let owner = ctx.obj();
        ctx.world.detach_child(&me);
        ctx.world.set_sticky_parent(&me, Some(&owner));
        let owner_sticky = ctx.world.sticky_parent(&owner);
        ctx.world.set_sticky_parent(&me, owner_sticky.as_ref());
        attach_keeping_world(&ctx.world, &owner, &me);
    } else {
        // Ships. TODO (C#): check how to sit on the ship.
        // `SlaveManager.BindSlave(character, owner.ParentObjId, AttachPointId,
        // AttachUnitReason.BoardTransfer)` (vehicles subsystem).
        let parent = ctx.doodad.data().parent_obj_id;
        crate::systems::vehicles::slaves::bind_slave(
            ctx.server,
            player,
            parent,
            f.attach_point_id,
            crate::systems::vehicles::slaves::attach_reason::BOARD_TRANSFER,
            // The doodad func already checked its own use range; the ship's seat helper can be
            // far from the hull origin, so no bind reach here.
            None,
        );
    }
}

/// `child.Transform.Parent = parent.Transform` (`SetParent` + `InternalAttachChild`): detaches
/// the child from its old parent (back to world space), then converts its world position /
/// rotation into the parent's local space (unscaled by the parent's `Scale`).
pub fn attach_keeping_world(world: &WorldInstance, parent: &WorldObject, child: &WorldObject) {
    world.detach_child(child);
    let pw = world.world_pos_rot(parent);
    let scale = parent.scale();
    let inv = quaternion_inverse(pw.to_quaternion());
    let c = child.transform().local();
    let p = inv.rotate([c.x - pw.x, c.y - pw.y, c.z - pw.z]);
    let (rx, ry, rz) = PositionAndRotation::rotation_from_quaternion(inv * c.to_quaternion());
    child.with_transform(|t| t.set_position(p[0] / scale, p[1] / scale, p[2] / scale, rx, ry, rz));
    world.attach_child(parent, child);
}

/// `Quaternion.Inverse(value)` (.NET: conjugate / length²).
fn quaternion_inverse(q: Quaternion) -> Quaternion {
    let ls = q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w;
    let inv = 1.0 / ls;
    Quaternion {
        x: -q.x * inv,
        y: -q.y * inv,
        z: -q.z * inv,
        w: q.w * inv,
    }
}

/// `DoodadFuncEnterInstance.Use`.
pub fn enter_instance(
    f: &DoodadFuncEnterInstance,
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
) {
    tracing::info!(
        "DoodadFuncEnterInstance, ZoneId: {}, ItemId: {}",
        f.zone_id,
        f.item_id
    );
    let Some(player) = DoodadCtx::as_player(caster) else {
        return;
    };
    let server = ctx.server;
    if server
        .systems
        .social
        .trials
        .is_player_in_court(player.character_id)
    {
        // The door must answer: the C# dropped the use without a word.
        crate::systems::instances::worlds::send_error(
            player,
            crate::systems::instances::worlds::error_message::CANNOT_USE_PORTAL_IN_TRIAL,
            0,
        );
        return;
    }
    crate::systems::instances::worlds::ensure_main_world_position(server, player);
    crate::systems::instances::indun::request_dungeon_instance(server, player, f.zone_id, 0);
}

/// `DoodadFuncEnterSysInstance.Use`.
pub fn enter_sys_instance(
    f: &DoodadFuncEnterSysInstance,
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
) {
    tracing::info!(
        "DoodadFuncEnterSysInstance, ZoneId: {}, FactionId: {}",
        f.zone_id,
        f.faction_id
    );
    let Some(player) = DoodadCtx::as_player(caster) else {
        return;
    };
    let server = ctx.server;
    // A selective entrance of a channel dungeon opens the channel dialog instead of entering
    // channel 0. The live instances of the dungeon are the list; with none, the first player
    // creates the channel the old way (request channel 0 and enter it).
    if f.selective && crate::systems::instances::indun::instance_has_channels(server, f.zone_id) {
        if let Some(zone) = crate::systems::instances::indun::zone_by_id(server, f.zone_id) {
            let live = crate::systems::instances::indun::live_zone_instances(server, zone.zone_key);
            if !live.is_empty() {
                let channels: Vec<SystemDungeonChannel> = live
                    .iter()
                    .enumerate()
                    .map(|(index, (world, dungeon))| SystemDungeonChannel {
                        index: index as i32,
                        instance_id: world.id,
                        restrict: dungeon.indun_zone.max_players as i32,
                        current: crate::systems::instances::worlds::character_count(world) as i32,
                    })
                    .collect();
                tracing::info!(
                    "DoodadFuncEnterSysInstance: zone {} offers {} channel(s)",
                    f.zone_id,
                    channels.len()
                );
                player.send(SCSystemDungeonStatePacket {
                    zone_key: f.zone_id,
                    channels,
                });
                return;
            }
        }
    }
    // Set main world when requesting to exit the main world or if it was never set before
    crate::systems::instances::worlds::ensure_main_world_position(server, player);
    let _ = crate::systems::instances::indun::request_system_instance(server, player, f.zone_id, 0);
}

/// The doodad half of `CSSelectSysInstanceChannelPacket`: the client picked a row of the
/// `SC 0x206` channel dialog (`X2.EnterSystemDungeon` -> `FUN_3930c420` -> CS 0x13d). The
/// checks mirror that client function: the doodad exists, its first `DoodadFuncEnterSysInstance`
/// func is `selective`, it is in reach, and its zone group has `select_channel`.
pub fn select_sys_instance_channel(
    server: &crate::server::GameServer,
    player: &crate::world::PlayerRef,
    inst_id: u32,
    doodad_obj_id: u32,
) {
    let Some(world) = server.world.world_of(&WorldObject::from(player)) else {
        return;
    };
    let Some(doodad) = world.doodad(doodad_obj_id) else {
        tracing::debug!(
            "SelectSysInstance: doodad {doodad_obj_id} not found for {}",
            player.data().unit.name
        );
        return;
    };
    // The client checks the template's first EnterSysInstance func (FUN_3983c230).
    let Some((func, skill_id)) = template_enter_func(server, &doodad) else {
        tracing::debug!("SelectSysInstance: doodad {doodad_obj_id} has no EnterSysInstance func");
        return;
    };
    if !func.selective {
        tracing::debug!("SelectSysInstance: doodad {doodad_obj_id} entrance is not selective");
        return;
    }
    let Some(zone) = crate::systems::instances::indun::zone_by_id(server, func.zone_id) else {
        return;
    };
    if !server
        .game_data
        .systems
        .instances
        .indun
        .dungeon_zone(zone.group_id)
        .is_some_and(|z| z.select_channel)
    {
        tracing::debug!(
            "SelectSysInstance: zone group {} has no select_channel",
            zone.group_id
        );
        return;
    }
    if !crate::systems::npcs::doodad::packets::in_func_reach(server, player, &doodad, skill_id) {
        tracing::debug!(
            "SelectSysInstance: {} is out of reach of doodad {doodad_obj_id}",
            player.data().unit.name
        );
        return;
    }
    crate::systems::instances::indun::enter_selected_system_instance(
        server,
        player,
        zone.zone_key,
        zone.group_id,
        inst_id,
    );
}

/// The template's first `DoodadFuncEnterSysInstance` func and its `func_skill_id` (the client's
/// `FUN_3983c230` returns the first func of type 0x4a of the doodad template).
fn template_enter_func(
    server: &crate::server::GameServer,
    doodad: &DoodadRef,
) -> Option<(DoodadFuncEnterSysInstance, u32)> {
    let data = &server.game_data.systems.npcs.doodad;
    let (template_id, deleted) = {
        let d = doodad.data();
        (d.template_id, d.phase.deleted)
    };
    if deleted {
        return None;
    }
    let template = data.template(template_id)?;
    for group in &template.func_groups {
        for func in data.funcs_for_group(group.id) {
            if let Some(FuncTemplate::EnterSysInstance(f)) =
                data.func_template(func.func_id, &func.func_type)
            {
                return Some((f.clone(), func.skill_id));
            }
        }
    }
    None
}

/// `DoodadFuncExitIndun.Use`.
pub fn exit_indun(f: &DoodadFuncExitIndun, ctx: &DoodadCtx<'_>, caster: Option<&WorldObject>) {
    tracing::info!("DoodadFuncExitIndun, ReturnPointId: {}", f.return_point_id);
    let Some(player) = DoodadCtx::as_player(caster) else {
        return;
    };
    let has_mwp = player.data().main_world_position.is_some();
    if f.return_point_id == 0 && has_mwp {
        if !crate::systems::instances::indun::request_leave_instance(ctx.server, player) {
            // No instance lists the character, so nobody would ever move it out: bring it back
            // to the main world (the C# did nothing and the door stayed dead).
            tracing::warn!(
                "DoodadFuncExitIndun, {} ({}) is in no instance's character list",
                player.data().unit.name,
                player.character_id
            );
            let in_instance = player.data().unit.transform.instance_id
                != crate::models::character::DEFAULT_INSTANCE_ID;
            if in_instance {
                crate::systems::instances::worlds::return_to_main_world(ctx.server, player);
            } else {
                send_error(player, err::INVALID_RETURN_POS_INSTANCE);
            }
        }
        return;
    }
    // TODO (C#): in db not have a entries, but we can change this xD
    tracing::info!("DoodadFuncExitIndun, Not have return point!");
    send_error(player, err::INVALID_RETURN_POS_INSTANCE);
}

/// `DoodadFuncRemoveInstance.Use`.
pub fn remove_instance(
    f: &DoodadFuncRemoveInstance,
    ctx: &DoodadCtx<'_>,
    caster: Option<&WorldObject>,
) {
    tracing::debug!("DoodadFuncRemoveInstance, ZoneId: {}", f.zone_id);
    let zone = crate::systems::instances::indun::zone_by_id(ctx.server, f.zone_id);
    let Some(player) = DoodadCtx::as_player(caster) else {
        return;
    };
    let Some(zone) = zone else {
        return;
    };
    if crate::systems::instances::indun::request_deletion(ctx.server, player, Some(&zone)) {
        tracing::info!(
            "DoodadFuncRemoveInstance, ZoneId: {} has been removed",
            f.zone_id
        );
    }
}
