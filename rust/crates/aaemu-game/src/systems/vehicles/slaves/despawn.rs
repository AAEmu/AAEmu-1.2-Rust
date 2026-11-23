use std::sync::Arc;

use crate::models::movement::{convert_radian_to_direction, MoveKind, MoveType, UnitMove};
use crate::packets::world::SCOneUnitMovementPacket;
use crate::server::GameServer;
use crate::systems::npcs::spawn_manager;
use crate::systems::skills::effects::EffectArgs;
use crate::world::{PlayerRef, SlaveRef, WorldInstance, WorldObject};

use super::mounting::{clear_slave_attachment_state, unbind_child_riders};
use super::packets::{SCSlaveDespawnPacket, SCSlaveRemovedPacket};
use super::spawners::spawn_manager_of;
use super::{
    attach_reason, broadcast, error, get_slave_by_tl_id, owned_roots, player_world, save,
    unbind_slave,
};

// ---------------------------------------------------------------------------------------------
// Delete / despawn
// ---------------------------------------------------------------------------------------------

pub(super) fn audit_retirement(server: &GameServer, world: &WorldInstance, root: &SlaveRef) {
    let diagnostics = &server.systems.vehicles.slave_summons.diagnostics;
    if !world
        .slave(root.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, root))
    {
        // Numeric cached children cannot identify this lifetime after the hull ID was reused.
        diagnostics.retiring(world.id, root, &[]);
        return;
    }
    let (children, doodads) = {
        let d = root.data();
        (d.attached_slaves.clone(), d.attached_doodads.clone())
    };
    let doodads: Vec<_> = doodads
        .into_iter()
        .filter_map(|id| world.doodad(id))
        .filter(|rack| super::audit::rack_parent_matches(&rack.data(), world.id, root.obj_id))
        .collect();
    diagnostics.retiring(world.id, root, &doodads);
    for child in children.into_iter().filter_map(|id| world.slave(id)) {
        diagnostics.retiring(world.id, &child, &[]);
    }
}

/// `Delete(owner, objId, ignoreAttachedItemWarning)`.
pub fn delete(
    server: &GameServer,
    owner: Option<&PlayerRef>,
    instance_id: u32,
    obj_id: u32,
    ignore_attached_item_warning: bool,
) {
    try_delete(
        server,
        owner,
        instance_id,
        obj_id,
        ignore_attached_item_warning,
    );
}

pub(super) fn can_delete(
    world: &WorldInstance,
    slave: &SlaveRef,
    owner: Option<&PlayerRef>,
    ignore_attached_item_warning: bool,
) -> bool {
    if owner.is_some_and(|player| slave.data().summoner_id != player.character_id) {
        return false;
    }
    if !ignore_attached_item_warning {
        let doodads = slave.data().attached_doodads.clone();
        for id in doodads {
            if let Some(doodad) = world.doodad(id) {
                let loaded = {
                    let d = doodad.data();
                    d.item_id != 0 || d.item_template_id != 0
                };
                if loaded {
                    if let Some(owner) = owner {
                        error(owner, error::SLAVE_EQUIPMENT_LOADED_ITEM);
                    }
                    return false;
                }
            }
        }
    }
    true
}

/// Refusal is checked before saves, rider/gunner detachment, physics or success packets.
pub(super) fn try_delete(
    server: &GameServer,
    owner: Option<&PlayerRef>,
    instance_id: u32,
    obj_id: u32,
    ignore_attached_item_warning: bool,
) -> bool {
    let _items = server.systems.economy.operation_scope();
    let _publication = server
        .systems
        .vehicles
        .slave_summons
        .publication_gate
        .lock();
    let Some(world) = server.world.world(instance_id) else {
        return false;
    };
    let Some(slave) = world.slave(obj_id) else {
        return false;
    };
    if !can_delete(&world, &slave, owner, ignore_attached_item_warning) {
        return false;
    }
    audit_retirement(server, &world, &slave);
    save(server, &slave);
    let (attached_chars, tl) = {
        let d = slave.data();
        (d.attached_characters.clone(), u32::from(d.unit.tl_id))
    };
    for (_, c) in attached_chars {
        if let Some(p) = server.world.character_by_obj_id(c) {
            unbind_slave(server, &p, tl, attach_reason::SLAVE_BINDING);
        }
    }
    unbind_child_riders(server, &slave, attach_reason::SLAVE_BINDING);
    let (doodad_ids, slave_ids, portal_time) = {
        let d = slave.data();
        let portal = server
            .game_data
            .systems
            .vehicles
            .slave_template(d.unit.template_id)
            .map_or(0.0, |t| t.portal_time);
        (
            d.attached_doodads.clone(),
            d.attached_slaves.clone(),
            portal,
        )
    };
    let now = spawn_manager::now();
    let despawn_delayed = spawn_manager::add_secs(now, f64::from(portal_time - 0.5));
    // slaveInfo.Transform.DetachAll(): a remaining child (deck stander, attached doodad) keeps
    // its world position as its new local one, so the owner's world pose survives the hull.
    let slave_obj = WorldObject::from(&slave);
    world.detach_child(&slave_obj);
    let mut deck_standers = Vec::new();
    for child in slave_obj.core().take_children() {
        world.detach_child(&child);
        if let Some(player) = child.as_player() {
            world.set_sticky_parent(&child, None);
            deck_standers.push((player.clone(), child.transform()));
        }
    }
    let spawns = spawn_manager_of(server, world.id);
    for id in &doodad_ids {
        let Some(d) = world.doodad(*id) else { continue };
        if owner.is_some() {
            d.data().is_persistent = false;
        }
        if let Some(s) = &spawns {
            s.add_despawn(server, WorldObject::from(&d), Some(despawn_delayed));
        }
    }
    for id in &slave_ids {
        let Some(s) = world.slave(*id) else { continue };
        world.remove_object(*id);
        if let Some(sp) = &spawns {
            sp.add_despawn(server, WorldObject::from(&s), Some(despawn_delayed));
        }
    }
    super::ship_physics::remove_ship(server, &slave);
    // Stop publishing active hull motion before the native despawn controller starts.
    world.remove_object(obj_id);
    // Leaving a deck changes the movement frame within the same world. Publish an
    // ordinary world-space move so the client can drop the frame without a loading screen.
    for (player, t) in deck_standers {
        broadcast(
            server,
            &WorldObject::from(&player),
            &SCOneUnitMovementPacket {
                obj_id: player.obj_id,
                move_type: MoveType::new(MoveKind::Unit(UnitMove {
                    x: t.x,
                    y: t.y,
                    z: t.z,
                    rotation_x: convert_radian_to_direction(f64::from(t.rot_x)),
                    rotation_y: convert_radian_to_direction(f64::from(t.rot_y)),
                    rotation_z: convert_radian_to_direction(f64::from(t.rot_z)),
                    ..Default::default()
                })),
            },
            true,
        );
    }
    if let Some(o) = owner {
        // The hull stays in the visibility grid for the portal effect. Release the
        // client's deck frame first so it cannot follow that retiring hull.
        crate::world::broadcast(&world, &slave_obj, &SCSlaveDespawnPacket { obj_id }, false);
        broadcast(
            server,
            &WorldObject::from(o),
            &SCSlaveRemovedPacket {
                obj_id: o.obj_id,
                tl_id: tl as u16,
            },
            true,
        );
    }
    if let Some(s) = &spawns {
        s.add_despawn(
            server,
            slave_obj,
            Some(spawn_manager::add_secs(now, f64::from(portal_time + 0.5))),
        );
    }
    true
}

/// `DestroyAndSpawnSlave` (special effect 72; row 8224 is the only row, on skill 19165 "destroy
/// the isolated ship and make a new one appear", carried by slave template 125's mount skill).
/// A spawner-backed slave is removed through its spawner and the spawner creates the
/// replacement at the authored transform. A slave without a spawner stays destroyed.
pub fn destroy_and_spawn(server: &GameServer, args: &EffectArgs<'_>) {
    let Some(slave) = args
        .target
        .as_obj()
        .and_then(WorldObject::as_slave)
        .cloned()
    else {
        tracing::warn!("Special effects: DestroyAndSpawnSlave target is not a slave");
        return;
    };
    let (spawner_id, instance_id, template_id) = {
        let d = slave.data();
        (
            d.spawner_id,
            d.unit.transform.instance_id,
            d.unit.template_id,
        )
    };
    let Some(world) = server.world.world(instance_id) else {
        return;
    };
    if spawner_id != 0 {
        if let Some(spawner) = server.systems.vehicles.slave_spawners.get(spawner_id) {
            let old_obj_id = slave.obj_id;
            spawner.despawn(server, &slave);
            match spawner.spawn(server) {
                Some(new_obj_id) => tracing::debug!(
                    "Special effects: DestroyAndSpawnSlave replaced slave {old_obj_id} with {new_obj_id} from spawner {spawner_id}"
                ),
                None => tracing::warn!(
                    "Special effects: DestroyAndSpawnSlave spawner {spawner_id} could not respawn template {template_id}"
                ),
            }
            return;
        }
    }
    tracing::warn!(
        "Special effects: DestroyAndSpawnSlave slave {} has no spawner; leaving it destroyed",
        slave.obj_id
    );
    world.delete(&slave);
}

/// `RemoveActiveSlave(character, slaveTlId, forceDelete)`.
pub fn remove_active_slave(server: &GameServer, character: &PlayerRef, tl_id: u16, force: bool) {
    let Some(world) = player_world(server, character) else {
        return;
    };
    let Some(slave) = get_slave_by_tl_id(&world, u32::from(tl_id)) else {
        return;
    };
    let (summoner, name) = {
        let d = slave.data();
        (d.summoner_obj_id, d.unit.name.clone())
    };
    if summoner == 0 || summoner != character.obj_id {
        tracing::warn!(
            "Non-owner is trying to desummon a slave {} => {name} (ObjId: {})",
            character.data().unit.name,
            slave.obj_id
        );
        return;
    }
    delete(server, Some(character), world.id, slave.obj_id, force);
}

/// `RemoveAndDespawnAllActiveOwnedSlaves(owner, forceDelete)`.
pub fn remove_and_despawn_all_active_owned_slaves(
    server: &GameServer,
    owner: &PlayerRef,
    force: bool,
) {
    let mut removed = true;
    for (world, active) in owned_roots(server, owner.character_id) {
        removed &= try_delete(server, Some(owner), world.id, active.obj_id, force);
    }
    if removed {
        clear_slave_attachment_state(server, owner);
    }
}
