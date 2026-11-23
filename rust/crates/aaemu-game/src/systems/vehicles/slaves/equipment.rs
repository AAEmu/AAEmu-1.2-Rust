use std::sync::Arc;

use crate::models::container::{slot_type, ContainerRef};
use crate::models::doodad::doodad_owner_type;
use crate::models::item::Item;
use crate::models::item_task::task_type;
use crate::models::slave::Slave;
use crate::models::unit::BaseUnitType;
use crate::packets::world::SCUnitPointsPacket;
use crate::server::GameServer;
use crate::systems::economy::inventory::{self, Inv, ItemTxn};
use crate::systems::economy::item_effect_packets::{
    ItemAndLocation, SCSlaveEquipmentChangedPacket,
};
use crate::systems::economy::item_packets::SCUnitEquipmentsChangedPacket;
use crate::systems::npcs::spawn_manager;
use crate::systems::skills::types::KillReason;
use crate::world::{Entity, PlayerRef, SlaveRef, WorldInstance, WorldObject};

use super::super::{now_ms, stats};
use super::data::SlaveTemplate;
use super::mounting::unbind_riders_of;
use super::spawn::{apply_attach_point_location, apply_slave_bonuses};
use super::{
    attach_reason, broadcast, error, get_active_slave_by_owner_obj_id, get_slave_by_db_id,
    get_slave_by_tl_id, player_world,
};

// ---------------------------------------------------------------------------------------------
// Slave equipment (`CSChangeSlaveEquipmentPacket`)
// ---------------------------------------------------------------------------------------------

/// `SlotMatches(wireItem, serverItem)`: the snapshot the client sent must describe the slot the
/// server has, otherwise the move would touch an unrelated item.
fn equipment_slot_matches(wire: &super::packets::WireItem, actual: Option<&Item>) -> bool {
    if wire.template_id == 0 {
        return actual.is_none();
    }
    actual.is_some_and(|i| i.template_id == wire.template_id && (wire.id == 0 || i.id == wire.id))
}

/// Reply to one `CSChangeSlaveEquipment` entry with the pre-move server snapshots (10.x
/// `SCSlaveEquipmentChangedPacket`; the client applies the copies only when `success` is set).
fn reply_slave_equipment(
    character: &PlayerRef,
    p: &super::packets::CSChangeSlaveEquipmentPacket,
    on_slave: Option<&Item>,
    in_bag: Option<&Item>,
    success: bool,
) {
    character.send(SCSlaveEquipmentChangedPacket {
        item_on_slave: ItemAndLocation {
            item: on_slave.cloned(),
            slot_type: p.slot1_type,
            slot: p.slot1,
        },
        item_in_bag: ItemAndLocation {
            item: in_bag.cloned(),
            slot_type: p.slot2_type,
            slot: p.slot2,
        },
        slave_tl_id: p.slave_tl,
        character_id: p.character_id,
        db_slave_id: p.db_slave_id,
        bts: p.bts,
        success,
    });
}

/// Copies the persistent equipment container into the slave model mirror (stats and visibility
/// read the mirror).
fn mirror_slave_equipment(slave: &SlaveRef, container: &ContainerRef) {
    let items = container.lock().expect("container poisoned").items.clone();
    let mut d = slave.data();
    d.equipment.fill(None);
    for it in items {
        let slot = it.slot.max(0) as usize;
        if let Some(entry) = d.equipment.get_mut(slot) {
            *entry = Some(it);
        }
    }
}

/// Initial equipment, including legacy static defaults that have an exact legal item equivalent.
/// A multi-point slot is migrated only when one item reproduces every authored binding. Parts
/// without such an equivalent remain static, rather than disappearing merely because a slot exists.
pub(super) fn initial_slave_equipment(
    server: &GameServer,
    template: &SlaveTemplate,
) -> Vec<super::data::SlaveInitialItem> {
    let data = &server.game_data.systems.vehicles;
    let mut initial = data
        .slave_initial_item_pack(template.slave_initial_item_pack_id)
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|item| server.game_data.items.template(item.item_id).is_some())
        .collect::<Vec<_>>();
    for slot in data.slave_equip_slots(template.id) {
        if initial
            .iter()
            .any(|i| i.equip_slot_id == slot.equip_slot_id)
        {
            continue;
        }
        let points = data.slave_equip_attach_points(template.id, slot.equip_slot_id);
        let equivalent = data
            .item_slave_equipments
            .values()
            .filter(|item| {
                item.require_item_id == 0
                    && server.game_data.items.template(item.item_id).is_some()
                    && data.can_equip_item_on_slave(item.item_id, template.id, slot.equip_slot_id)
                    && points.iter().all(|point| {
                        let child = template
                            .slave_bindings
                            .iter()
                            .find(|b| b.owner_type == "Slave" && b.attach_point_id == *point);
                        let doodad = template
                            .doodad_bindings
                            .iter()
                            .find(|b| b.attach_point_id == *point);
                        (child.is_some() || doodad.is_some())
                            && child.map_or(0, |b| b.slave_id) == item.slave_id
                            && doodad.map_or(0, |b| b.doodad_id) == item.doodad_id
                    })
            })
            .min_by_key(|item| item.item_id);
        if let Some(item) = equivalent {
            initial.push(super::data::SlaveInitialItem {
                pack_id: template.slave_initial_item_pack_id,
                equip_slot_id: slot.equip_slot_id,
                item_id: item.item_id,
            });
        }
    }
    initial
}

/// Static bindings remain authored defaults unless the slot has initial equipment (explicit or
/// migrated). That decision is independent of its current contents: an intentionally empty saved
/// slot must not grow its static default again on the next summon.
pub(super) fn is_binding_attach_point(
    server: &GameServer,
    template: &SlaveTemplate,
    point: u8,
) -> bool {
    let data = &server.game_data.systems.vehicles;
    let initial = initial_slave_equipment(server, template);
    if data.slave_equip_slots(template.id).iter().any(|slot| {
        slot.attach_point_id == point
            && initial
                .iter()
                .any(|i| i.equip_slot_id == slot.equip_slot_id)
    }) {
        return false;
    }
    template
        .doodad_bindings
        .iter()
        .any(|b| b.attach_point_id == point)
        || template
            .slave_bindings
            .iter()
            .any(|b| b.attach_point_id == point)
        || template
            .healing_point_doodads
            .iter()
            .any(|b| b.attach_point_id == point)
}

/// Removes the equipment-driven visual at one attach point (a child slave / doodad created by
/// [`spawn_or_replace_equipment_visual`]). Static bindings are never removed.
fn remove_equipment_visual_at(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    slave: &SlaveRef,
    point: u8,
) {
    let data = &server.game_data.systems.vehicles;
    if data
        .slave_template(slave.data().unit.template_id)
        .is_some_and(|t| is_binding_attach_point(server, t, point))
    {
        return;
    }
    let (attached_doodads, attached_slaves) = {
        let d = slave.data();
        (d.attached_doodads.clone(), d.attached_slaves.clone())
    };
    let doodads: Vec<_> = attached_doodads
        .into_iter()
        .filter(|id| {
            world
                .doodad(*id)
                .is_some_and(|dd| dd.data().attach_point == point)
        })
        .collect();
    let children: Vec<_> = attached_slaves
        .into_iter()
        .filter(|id| {
            world
                .slave(*id)
                .is_some_and(|s| s.data().attach_point_id as u8 == point)
        })
        .collect();
    slave
        .data()
        .attached_doodads
        .retain(|id| !doodads.contains(id));
    for id in &doodads {
        if let Some(doodad) = world.doodad(*id) {
            spawn_manager::backend::doodad_delete(server, world, &doodad);
        }
    }
    slave
        .data()
        .attached_slaves
        .retain(|id| !children.contains(id));
    for id in &children {
        if let Some(child) = world.slave(*id) {
            unbind_riders_of(server, &child, attach_reason::SLAVE_BINDING);
            world.delete(WorldObject::from(&child));
        }
    }
}

/// `SlaveManager.SpawnEquipmentDoodad`: the doodad a ship part becomes (sails, masts, lamps, ...).
fn spawn_equipment_doodad(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    slave: &SlaveRef,
    point: u8,
    doodad_id: u32,
    item: &Item,
) {
    let Some(doodad) =
        crate::systems::npcs::doodad::manager::create(server, world, 0, doodad_id, None, true)
    else {
        tracing::warn!(
            "Slave {}: doodad template {doodad_id} missing for equipment visual",
            slave.data().unit.template_id
        );
        return;
    };
    let slave_obj = WorldObject::from(slave);
    let (owner_obj_id, owner_id, slave_db_id, faction, spawn_time) = {
        let d = slave.data();
        (
            d.summoner_obj_id,
            d.summoner_id,
            d.id,
            d.unit.faction_id,
            d.spawn_time_ms,
        )
    };
    {
        let mut d = doodad.data();
        d.owner_obj_id = owner_obj_id;
        d.parent_obj_id = slave.obj_id;
        d.attach_point = point;
        d.owner_id = owner_id;
        d.plant_time = spawn_time / 1000;
        d.owner_type = doodad_owner_type::SLAVE;
        d.owner_db_id = slave_db_id;
        d.data = i32::from(point);
        d.faction_id = faction;
        d.type2 = 1;
        d.scale = 1.0;
        d.transform.instance_id = world.id;
        d.transform.set_position(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        if item.flags & crate::models::item::ITEM_FLAG_HAS_UCC != 0 && item.ucc_id > 0 {
            d.ucc_id = item.ucc_id;
        }
    }
    let start = crate::systems::npcs::doodad::phase::get_func_group_id(
        &server.game_data.systems.npcs.doodad,
        doodad_id,
    );
    crate::systems::npcs::doodad::manager::set_func_group_id(server, &doodad, start);
    let dobj = WorldObject::from(&doodad);
    world.attach_child(&slave_obj, &dobj);
    apply_attach_point_location(server, world, slave, &dobj, point);
    slave.data().attached_doodads.push(doodad.obj_id);
    crate::systems::npcs::doodad::manager::init_doodad(server, world, &doodad);
    crate::systems::npcs::doodad::manager::spawn(server, world, &doodad);
}

/// `SlaveManager.SpawnEquipmentChildSlave`: the child slave a part becomes (cannons, harpoons).
fn spawn_equipment_child_slave(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    slave: &SlaveRef,
    point: u8,
    child_template_id: u32,
) {
    let data = &server.game_data.systems.vehicles;
    let Some(child_template) = data.slave_template(child_template_id).cloned() else {
        tracing::warn!(
            "Slave {}: child template {child_template_id} missing for equipment visual",
            slave.data().unit.template_id
        );
        return;
    };
    let Some(child_tl) = server.ids.tl.get_next_id().ok() else {
        return;
    };
    let Some(child_obj_id) = server.ids.object.get_next_id().ok() else {
        return;
    };
    let (summoner_id, summoner_obj_id, summoner_name, faction, slave_db_id) = {
        let d = slave.data();
        (
            d.summoner_id,
            d.summoner_obj_id,
            d.summoner_name.clone(),
            d.unit.faction_id,
            d.id,
        )
    };
    let mut c = Slave::new(0, child_template.id);
    c.unit.tl_id = child_tl as u16;
    c.unit.name = child_template.name.clone();
    c.unit.level = child_template.level as u8;
    c.unit.model_id = child_template.model_id;
    c.unit.faction_id = faction;
    c.summoner_id = summoner_id;
    c.summoner_obj_id = summoner_obj_id;
    c.summoner_name = summoner_name;
    c.spawn_time_ms = now_ms();
    c.attach_point_id = point as i8;
    c.owner_obj_id = slave.obj_id;
    c.owner_type = BaseUnitType::Slave;
    c.unit.owner_id = slave_db_id;
    let child = Entity::new(child_obj_id, c);
    let child_obj = WorldObject::from(&child);
    apply_slave_bonuses(server, &child_obj, &child_template);
    stats::refresh_object_pools(server, &child_obj);
    let w = world.world_pos_rot(&WorldObject::from(slave));
    {
        let mut d = child.data();
        d.unit.hp = d.unit.max_hp;
        d.unit.mp = d.unit.max_mp;
        d.unit.transform.instance_id = world.id;
        d.unit
            .transform
            .set_position(w.x, w.y, w.z, w.rot_x, w.rot_y, w.rot_z);
    }
    let slave_obj = WorldObject::from(slave);
    world.attach_child(&slave_obj, &child_obj);
    apply_attach_point_location(server, world, slave, &child_obj, point);
    slave.data().attached_slaves.push(child_obj_id);
    let changes = world.spawn(&child, &server.view_ctx());
    crate::world::session::handle_zone_changes(server, changes);
    let hp = child.data().unit.hp;
    crate::systems::skills::combat::post_update_current_hp(
        server,
        &child_obj,
        Some(&child_obj),
        0,
        hp,
        KillReason::Unknown,
    );
}

/// `SlaveManager.SpawnOrReplaceEquipmentVisual`: at every attach point the equipment slot
/// occupies, remove the previous visual and spawn the one the item maps to (`None` unequips).
pub fn spawn_or_replace_equipment_visual(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    slave: &SlaveRef,
    item: Option<&Item>,
    equip_slot_id: u8,
) {
    let data = &server.game_data.systems.vehicles;
    let Some(template) = data.slave_template(slave.data().unit.template_id).cloned() else {
        return;
    };
    let points = data.slave_equip_attach_points(template.id, equip_slot_id);
    if points.is_empty() {
        tracing::debug!(
            "No slave_equip_slots entry for slave={} slot={equip_slot_id}",
            template.id
        );
        return;
    }
    for point in points {
        if is_binding_attach_point(server, &template, point) {
            tracing::debug!(
                "Skip equip visual at {point} on slave tpl={}: attach point is a static binding",
                template.id
            );
            continue;
        }
        remove_equipment_visual_at(server, world, slave, point);
        let Some(item) = item else {
            continue;
        };
        let Some(visual) = data.item_slave_equipment(item.template_id) else {
            tracing::warn!(
                "No item_slave_equipments visual for item={} on slave tpl={} slot={equip_slot_id}",
                item.template_id,
                template.id
            );
            continue;
        };
        if visual.slave_id > 0 {
            spawn_equipment_child_slave(server, world, slave, point, visual.slave_id);
        } else if visual.doodad_id > 0 {
            spawn_equipment_doodad(server, world, slave, point, visual.doodad_id, item);
        }
    }
}

/// `SlaveManager.SpawnEquipmentVisualsFromInventory`: spawn the visual of every equipped part.
pub fn spawn_equipment_visuals_from_inventory(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    slave: &SlaveRef,
) {
    let items: Vec<(u8, Item)> = {
        let d = slave.data();
        d.equipment
            .iter()
            .enumerate()
            .filter_map(|(slot, item)| {
                item.as_ref()
                    .map(|i| (u8::try_from(slot).unwrap_or(u8::MAX), i.clone()))
            })
            .collect()
    };
    for (slot, item) in items {
        spawn_or_replace_equipment_visual(server, world, slave, Some(&item), slot);
    }
}

/// `CSChangeSlaveEquipmentPacket` (10.x `CSChangeSlaveEquipmentPacket.Read`, adapted to the 1.2
/// one-pair body and opcode): equip, unequip or re-slot a ship / vehicle part. The persistent
/// `EquipmentSlave` container is keyed by `(character id, slave DB id)` so the item rows survive
/// a despawn and a server restart.
pub fn change_slave_equipment(
    server: &GameServer,
    character: &PlayerRef,
    p: &super::packets::CSChangeSlaveEquipmentPacket,
) {
    tracing::debug!(
        "ChangeSlaveEquipment, Char: {}, Tl: {}, DbSlaveId: {}, Bts: {}",
        p.character_id,
        p.slave_tl,
        p.db_slave_id,
        p.bts
    );
    if p.character_id != character.character_id {
        return;
    }
    let Some(world) = player_world(server, character) else {
        return;
    };
    let slave = (if p.slave_tl != 0 {
        get_slave_by_tl_id(&world, u32::from(p.slave_tl))
    } else {
        None
    })
    .or_else(|| {
        if p.db_slave_id != 0 {
            get_slave_by_db_id(&world, p.db_slave_id)
        } else {
            None
        }
    })
    .or_else(|| get_active_slave_by_owner_obj_id(&world, character.obj_id));
    let Some(slave) = slave else {
        tracing::warn!(
            "ChangeSlaveEquipment: {} has no slave tl={} db={}",
            character.data().unit.name,
            p.slave_tl,
            p.db_slave_id
        );
        reply_slave_equipment(character, p, None, None, false);
        return;
    };
    let (slave_db_id, owned) = {
        let d = slave.data();
        (d.id, d.summoner_id == character.character_id)
    };
    if p.db_slave_id != 0 && slave_db_id != 0 && slave_db_id != p.db_slave_id {
        tracing::warn!(
            "ChangeSlaveEquipment: slave {} does not match requested db id {}",
            slave.obj_id,
            p.db_slave_id
        );
        reply_slave_equipment(character, p, None, None, false);
        return;
    }
    if !owned {
        tracing::warn!(
            "ChangeSlaveEquipment: {} does not own slave tl={} db={}",
            character.data().unit.name,
            p.slave_tl,
            p.db_slave_id
        );
        reply_slave_equipment(character, p, None, None, false);
        return;
    }
    if slave_db_id == 0 {
        reply_slave_equipment(character, p, None, None, false);
        return;
    }
    if p.slot2_type != slot_type::EQUIPMENT_SLAVE
        || !matches!(
            p.slot1_type,
            slot_type::INVENTORY | slot_type::EQUIPMENT_SLAVE
        )
    {
        tracing::warn!(
            "ChangeSlaveEquipment: unsupported slots {}#{} <-> {}#{}",
            p.slot1_type,
            p.slot1,
            p.slot2_type,
            p.slot2
        );
        reply_slave_equipment(character, p, None, None, false);
        return;
    }
    let equipment = server
        .items
        .find_container_for_character(
            character.character_id,
            slot_type::EQUIPMENT_SLAVE,
            slave_db_id,
        )
        .unwrap_or_else(|| {
            server.items.container_for_character(
                character.character_id,
                slot_type::EQUIPMENT_SLAVE,
                slave_db_id,
                &server.ids.container,
            )
        });
    let bag = Inv::of(character).bag();
    let size_ok = |c: &ContainerRef, slot: u8| {
        let size = c.lock().expect("container poisoned").container_size();
        size <= 0 || i32::from(slot) < size
    };
    if !size_ok(&equipment, p.slot2)
        || (p.slot1_type == slot_type::EQUIPMENT_SLAVE && !size_ok(&equipment, p.slot1))
        || (p.slot1_type == slot_type::INVENTORY && !size_ok(&bag, p.slot1))
    {
        tracing::warn!(
            "ChangeSlaveEquipment: slot out of range {}#{} <-> {}#{}",
            p.slot1_type,
            p.slot1,
            p.slot2_type,
            p.slot2
        );
        reply_slave_equipment(character, p, None, None, false);
        return;
    }
    let item1 = if p.slot1_type == slot_type::INVENTORY {
        bag.lock()
            .expect("container poisoned")
            .get_item_by_slot(i32::from(p.slot1))
            .cloned()
    } else {
        equipment
            .lock()
            .expect("container poisoned")
            .get_item_by_slot(i32::from(p.slot1))
            .cloned()
    };
    let item2 = equipment
        .lock()
        .expect("container poisoned")
        .get_item_by_slot(i32::from(p.slot2))
        .cloned();
    if !equipment_slot_matches(&p.item1, item1.as_ref())
        || !equipment_slot_matches(&p.item2, item2.as_ref())
    {
        tracing::warn!(
            "ChangeSlaveEquipment: slot contents mismatch, client {}/{} server {}/{} for {}#{} <-> {}#{}",
            p.item1.template_id,
            p.item2.template_id,
            item1.as_ref().map_or(0, |i| i.template_id),
            item2.as_ref().map_or(0, |i| i.template_id),
            p.slot1_type,
            p.slot1,
            p.slot2_type,
            p.slot2
        );
        reply_slave_equipment(character, p, item1.as_ref(), item2.as_ref(), false);
        return;
    }
    // Ship / vehicle parts only, and only where the hull / slot packs allow them. The 1.2 data
    // carries the validation that the 10.x server left to the client:
    // `item_slave_equipments.slave_equip_pack_id` -> `allow_to_equip_slaves` and
    // `slot_pack_id` -> `allow_to_equip_slots`.
    let slave_template_id = slave.data().unit.template_id;
    let data = &server.game_data.systems.vehicles;
    let allowed = |item: &Item, target_slot: u8| {
        data.can_equip_item_on_slave(item.template_id, slave_template_id, target_slot)
    };
    for (item, target_slot) in [
        (item1.as_ref(), p.slot2),
        (
            (p.slot1_type == slot_type::EQUIPMENT_SLAVE)
                .then_some(item2.as_ref())
                .flatten(),
            p.slot1,
        ),
    ] {
        let Some(item) = item else { continue };
        if !allowed(item, target_slot) {
            tracing::warn!(
                "ChangeSlaveEquipment: item {} (template {}) cannot be equipped on slave tpl={} slot {}",
                item.id,
                item.template_id,
                slave_template_id,
                target_slot
            );
            error(character, super::error::ITEM_NOT_FOR_PET);
            reply_slave_equipment(character, p, item1.as_ref(), item2.as_ref(), false);
            return;
        }
    }
    // Source / target: the 1.2 client always puts the slave slot second. If its first slot is
    // empty the second item moves to it (unequip); both filled swaps them.
    let move_args = match (&item1, &item2) {
        (Some(i1), _) => Some((
            if p.slot1_type == slot_type::INVENTORY {
                bag.clone()
            } else {
                equipment.clone()
            },
            equipment.clone(),
            p.slot1_type,
            p.slot1,
            i1.id,
            slot_type::EQUIPMENT_SLAVE,
            p.slot2,
            item2.as_ref().map_or(0, |i| i.id),
        )),
        (None, Some(i2)) => Some((
            equipment.clone(),
            if p.slot1_type == slot_type::INVENTORY {
                bag.clone()
            } else {
                equipment.clone()
            },
            slot_type::EQUIPMENT_SLAVE,
            p.slot2,
            i2.id,
            p.slot1_type,
            p.slot1,
            0,
        )),
        (None, None) => None,
    };
    let Some((src, dst, from_type, from_slot, from_id, to_type, to_slot, to_id)) = move_args else {
        reply_slave_equipment(character, p, item1.as_ref(), item2.as_ref(), false);
        return;
    };
    let inv = Inv::of(character);
    let mut txn = ItemTxn::begin(server);
    let moved = inventory::split_or_move_item_ex(
        &mut txn,
        &inv,
        task_type::INVALID,
        Some(src),
        Some(dst),
        from_id,
        from_type,
        from_slot,
        to_id,
        to_type,
        to_slot,
        0,
    );
    crate::systems::economy::fx::flush(server, txn.into_fx());
    if !moved {
        reply_slave_equipment(character, p, item1.as_ref(), item2.as_ref(), false);
        return;
    }
    mirror_slave_equipment(&slave, &equipment);
    let mut touched = Vec::new();
    for (slot, st) in [(p.slot1, p.slot1_type), (p.slot2, p.slot2_type)] {
        if st == slot_type::EQUIPMENT_SLAVE && !touched.contains(&slot) {
            touched.push(slot);
        }
    }
    for slot in touched {
        let item = equipment
            .lock()
            .expect("container poisoned")
            .get_item_by_slot(i32::from(slot))
            .cloned();
        spawn_or_replace_equipment_visual(server, &world, &slave, item.as_ref(), slot);
    }
    let slave_obj = WorldObject::from(&slave);
    crate::systems::skills::stats::update_vehicle_gear_bonuses(
        server,
        &slave_obj,
        item1.as_ref(),
        item2.as_ref(),
    );
    stats::refresh_object_pools(server, &slave_obj);
    let (hp, mp) = {
        let mut d = slave.data();
        d.unit.hp = d.unit.hp.min(d.unit.max_hp);
        d.unit.mp = d.unit.mp.min(d.unit.max_mp);
        (d.unit.hp, d.unit.mp)
    };
    let mut changed = Vec::new();
    for (slot, st) in [(p.slot1, p.slot1_type), (p.slot2, p.slot2_type)] {
        if st != slot_type::EQUIPMENT_SLAVE || changed.iter().any(|(s, _)| *s == slot) {
            continue;
        }
        let item = equipment
            .lock()
            .expect("container poisoned")
            .get_item_by_slot(i32::from(slot))
            .cloned();
        changed.push((slot, item));
    }
    if !changed.is_empty() {
        broadcast(
            server,
            &slave_obj,
            &SCUnitEquipmentsChangedPacket {
                obj_id: slave.obj_id,
                items: changed,
            },
            false,
        );
    }
    broadcast(
        server,
        &slave_obj,
        &SCUnitPointsPacket {
            obj_id: slave.obj_id,
            hp,
            mp,
        },
        true,
    );
    reply_slave_equipment(character, p, item1.as_ref(), item2.as_ref(), true);
}
