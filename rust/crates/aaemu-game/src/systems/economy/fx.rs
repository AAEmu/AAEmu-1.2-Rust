//! Executes the [`ItemFx`] effects recorded by container operations, in order, once every
//! container / character lock is released: the packets the C# sends to `Container.Owner`
//! (resolved like the C# `Owner` getter: the character with that id in the world, nothing when
//! offline), the quest events and `EquipmentContainer.OnEnter/LeaveContainer`. Persistence
//! effects are applied before the item-operation lock is released; callbacks run afterwards.

use std::collections::HashMap;

use crate::models::item_task::{ItemFx, ItemFxList};
use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

use super::item_packets::{
    SCItemTaskSuccessPacket, SCSyncItemLifespanPacket, SCUnitEquipmentsChangedPacket,
};

/// Applies item-manager mutations while the item-operation lock is still held. These are
/// leaf operations; unlike notifications and hooks, they never access characters or the world.
/// Their original order is retained, and only the remaining callbacks stay in `fx`.
pub fn flush_persistence(server: &GameServer, fx: &mut ItemFxList) {
    if !fx
        .0
        .iter()
        .any(|e| matches!(e, ItemFx::ReleaseId(_) | ItemFx::Orphaned(_)))
    {
        return;
    }
    let mut callbacks = Vec::with_capacity(fx.len());
    for effect in fx.0.drain(..) {
        match effect {
            ItemFx::ReleaseId(id) => server.items.release_id(id, &server.ids.item),
            ItemFx::Orphaned(item) => server.items.add_loose(*item),
            callback => callbacks.push(callback),
        }
    }
    fx.0 = callbacks;
}

/// Runs `fx` (see the module docs).
pub fn flush(server: &GameServer, fx: ItemFxList) {
    flush_with(server, fx, !super::System::ops_held_on_this_thread());
}

/// Characters whose back slot lost a pack in `fx` while a taken-off glider is recorded. Also
/// drops the record when something other than the pack/glider swap equipped the back slot.
fn glider_reequip_candidates(server: &GameServer, fx: &ItemFxList) -> Vec<u32> {
    use crate::models::container::equip_slot;
    let economy = &server.systems.economy;
    let back = equip_slot::BACKPACK as u8;
    let mut out = Vec::new();
    for e in &fx.0 {
        match e {
            ItemFx::EquipEnter {
                owner,
                mate_id: 0,
                slot,
                item,
            } if *slot == back => {
                let Some((glider, _)) = economy.previous_backpack(*owner) else {
                    continue;
                };
                // The glider's own take-off + the pack equip is one operation: keep the record.
                let takeoff = fx.0.iter().any(|l| {
                    matches!(l, ItemFx::EquipLeave { owner: o, mate_id: 0, prev_slot, item: i }
                        if o == owner && *prev_slot == back && i.id == glider)
                });
                if item.id != glider && !takeoff {
                    economy.clear_previous_backpack(*owner);
                }
            }
            ItemFx::EquipLeave {
                owner,
                mate_id: 0,
                prev_slot,
                item,
            } if *prev_slot == back
                && economy
                    .previous_backpack(*owner)
                    .is_some_and(|(glider, _)| glider != item.id)
                && !out.contains(owner) =>
            {
                out.push(*owner);
            }
            _ => {}
        }
    }
    out
}

/// [`flush`]; `reequip` allows the glider re-equip (it starts an item transaction, so it must
/// not run under a character lock or inside another transaction).
pub fn flush_with(server: &GameServer, fx: ItemFxList, reequip: bool) {
    if fx.is_empty() {
        return;
    }
    let candidates = glider_reequip_candidates(server, &fx);
    let mut owners: HashMap<u32, Option<PlayerRef>> = HashMap::new();
    let mut owner = |id: u32| -> Option<PlayerRef> {
        if id == 0 {
            return None;
        }
        owners
            .entry(id)
            .or_insert_with(|| server.world.character_by_id(id))
            .clone()
    };
    let hooks = &server.systems.economy.hooks;
    for e in fx.0 {
        match e {
            ItemFx::Tasks {
                owner: o,
                task,
                tasks,
                force_remove,
            } => {
                if let Some(p) = owner(o) {
                    p.send(SCItemTaskSuccessPacket {
                        action: task,
                        tasks,
                        force_remove,
                    });
                }
            }
            ItemFx::Lifespan {
                owner: o,
                added,
                item_id,
                template_id,
                expire_time,
            } => {
                if let Some(p) = owner(o) {
                    p.send(SCSyncItemLifespanPacket {
                        added,
                        item_id,
                        template_id,
                        expire_time,
                    });
                }
            }
            ItemFx::Acquired {
                owner: o,
                template_id,
                count,
            } => {
                // OnAcquiredItem: only when count > 0
                if count > 0 {
                    if let (Some(p), Some(h)) = (owner(o), hooks.quests.get()) {
                        h.on_items_acquired(server, &p, template_id, count);
                    }
                }
            }
            ItemFx::Consumed {
                owner: o,
                template_id,
                count,
            } => {
                if let (Some(p), Some(h)) = (owner(o), hooks.quests.get()) {
                    h.on_items_consumed(server, &p, template_id, count);
                }
            }
            ItemFx::Exp {
                owner: o,
                exp,
                add_ability_exp,
            } => {
                if let (Some(p), Some(h)) = (owner(o), hooks.progression.get()) {
                    h.add_exp(server, &p, exp, add_ability_exp);
                }
            }
            ItemFx::EquipEnter {
                owner: o,
                mate_id,
                slot,
                item,
            } => {
                let Some(p) = owner(o) else { continue };
                if mate_id > 0 {
                    if let Some(h) = hooks.summons.get() {
                        h.on_mate_equipment_changed(server, &p, mate_id, &item, slot, true);
                    }
                    continue;
                }
                broadcast_equipment(server, &p, slot, Some(*item.clone()));
                if let Some(h) = hooks.gear.get() {
                    h.update_gear_bonuses(server, &WorldObject::from(&p), Some(&item), None);
                }
            }
            ItemFx::EquipLeave {
                owner: o,
                mate_id,
                prev_slot,
                item,
            } => {
                let Some(p) = owner(o) else { continue };
                if mate_id > 0 {
                    if let Some(h) = hooks.summons.get() {
                        h.on_mate_equipment_changed(server, &p, mate_id, &item, prev_slot, false);
                    }
                    continue;
                }
                broadcast_equipment(server, &p, prev_slot, None);
                if let Some(h) = hooks.gear.get() {
                    h.update_gear_bonuses(server, &WorldObject::from(&p), None, Some(&item));
                }
            }
            ItemFx::Packet { owner: o, packet } => {
                if let Some(p) = owner(o) {
                    p.send_dyn(packet.as_ref());
                }
            }
            ItemFx::ReleaseId(id) => server.items.release_id(id, &server.ids.item),
            ItemFx::Orphaned(item) => server.items.add_loose(*item),
        }
    }
    for id in candidates {
        if reequip {
            if let Some(p) = owner(id) {
                super::inventory::reequip_previous_glider(server, &p);
            }
        } else {
            let mut q = server
                .systems
                .economy
                .deferred_reequip
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !q.contains(&id) {
                q.push(id);
            }
        }
    }
}

/// `ParentUnit?.BroadcastPacket(new SCUnitEquipmentsChangedPacket(objId, slot, item), false)`.
fn broadcast_equipment(
    server: &GameServer,
    p: &PlayerRef,
    slot: u8,
    item: Option<crate::models::item::Item>,
) {
    let obj = WorldObject::from(p);
    let Some(world) = server.world.world_of(&obj) else {
        return;
    };
    let packet = SCUnitEquipmentsChangedPacket {
        obj_id: p.obj_id,
        items: vec![(slot, item)],
    };
    crate::world::broadcast(&world, obj, &packet, false);
}

#[cfg(test)]
#[path = "fx_tests.rs"]
mod tests;
