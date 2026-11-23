use crate::data::items::TemplateKind;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::ItemClass;
use crate::models::item_task::{task_type, ItemTask};
use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

use super::super::error_message;
use super::super::inventory::{lock_pair, send_error_message_to, Inv, ItemTxn};
use super::super::item_packets::SCUnitEquipmentsChangedPacket;
use super::{lock, task_packet, with_any_item_mut, BackpackDoodadSpawn, SkillItem};

// ---------------------------------------------------------------------------------------------
// Backpacks / slaves
// ---------------------------------------------------------------------------------------------

/// `PutDownBackpackEffect.Apply` (drop the equipped trade pack as a doodad in front of the
/// character; the glider the pack replaced comes back through
/// `inventory::reequip_previous_glider`).
pub fn put_down_backpack(
    server: &GameServer,
    player: &PlayerRef,
    backpack_doodad_id: u32,
    pack_item: Option<SkillItem>,
) {
    tracing::debug!("PutDownBackpackEffect");
    let Some(pack_item) = pack_item else { return };
    let economy = &server.systems.economy;
    let hooks = economy.item_effects.hooks();
    let inv = Inv::of(player);
    let equipment = inv.equipment();
    let Some(item) = lock(&equipment).get_item_by_id(pack_item.item_id).cloned() else {
        return;
    };
    if super::item_security::refuse_if_locked(player, &item) {
        return;
    }
    if hooks.in_public_farm(server, player) {
        let mut txn = ItemTxn::begin(server);
        send_error_message_to(
            &mut txn.out(),
            player,
            error_message::COMMON_FARM_NOT_ALLOWED_TYPE,
        );
        return;
    }
    let char_id = player.character_id;
    // pos = in front of the character (1 m), always facing north
    let obj = WorldObject::from(player);
    let here = match server.world.world_of(&obj) {
        Some(w) => w.world_pos_rot(&obj),
        None => obj.transform().local(),
    };
    let x = here.x + -here.rot_z.sin(); // -distance * sin(yaw), distance = 1
    let y = here.y + 1.0f32 * here.rot_z.cos();
    let z = here.z;

    let house = hooks.house_at(server, player, x, y);
    if let Some(h) = house {
        // Trying to put on a house location, we need to do some checks
        if !h.allowed_to_interact {
            let mut txn = ItemTxn::begin(server);
            send_error_message_to(&mut txn.out(), player, error_message::BACKPACK);
            return;
        }
    }

    {
        let mut txn = ItemTxn::begin(server);
        let Some(system) = inv.get(slot_type::SYSTEM) else {
            tracing::error!("PutDownBackpack: character {char_id} has no system container");
            return;
        };
        let data = txn.data();
        let moved = {
            let (mut sys, eq) = lock_pair(&system, &equipment);
            match eq {
                Some(mut eq) => sys.move_item_from(
                    &mut eq,
                    task_type::DROP_BACKPACK,
                    item.id,
                    -1,
                    data,
                    &mut txn.env.fx,
                ),
                None => false,
            }
        };
        if !moved {
            return;
        }
    }

    // Spawn doodad
    tracing::debug!("PutDownPackEffect");
    let spawn = BackpackDoodadSpawn {
        doodad_template_id: backpack_doodad_id,
        item_id: item.id,
        item_template_id: item.template_id,
        ucc_id: item.ucc_id,
        x,
        y,
        z,
        rot: [0.0; 3],
        house,
    };
    hooks.spawn_backpack_doodad(server, player, &spawn);
    // The glider the pack replaced is put back by `fx::flush` when the pack left the back slot
    // (`inventory::reequip_previous_glider`), which also broadcasts the equipment changes.
}

/// `Character.DropTradePackToFloor()` (`Character.DoDie`): an equipped trade pack is moved to
/// the system container and spawned as a doodad (the `PutDownBackpackEffect` doodad of the
/// pack's use skill) where the character died, on the ground.
pub fn drop_trade_pack_to_floor(server: &GameServer, player: &PlayerRef) {
    let inv = Inv::of(player);
    let Some(item) = inv.equipped(equip_slot::BACKPACK) else {
        return;
    };
    if super::item_security::is_locked(&item) {
        return;
    }
    let use_skill_id = match server.game_data.items.template(item.template_id) {
        Some(t) => match t.kind {
            TemplateKind::Backpack { backpack_type }
                if backpack_type == crate::data::items::backpack_kind::TRADE_PACK =>
            {
                t.use_skill_id
            }
            _ => return,
        },
        None => return,
    };
    // Find the linked doodad of this item's put down effect
    let skills = &server.game_data.systems.skills;
    let Some(item_skill) = skills.skill(use_skill_id) else {
        // C#: `itemSkill.Effects` of a null skill throws.
        tracing::error!("DropTradePackToFloor: skill {use_skill_id} not found");
        return;
    };
    use crate::systems::skills::data::effects::{EffectKind, EffectTemplate};
    let backpack_doodad_id = item_skill
        .effects
        .iter()
        .filter_map(|e| e.template)
        .find_map(|(kind, id)| match (kind, skills.effect(kind, id)) {
            (
                EffectKind::PutDownBackpackEffect,
                Some(EffectTemplate::PutDownBackpack {
                    backpack_doodad_id, ..
                }),
            ) => Some(*backpack_doodad_id),
            _ => None,
        })
        .unwrap_or(0);
    if backpack_doodad_id == 0 {
        return;
    }
    let equipment = inv.equipment();
    {
        let mut txn = ItemTxn::begin(server);
        let Some(system) = inv.get(slot_type::SYSTEM) else {
            tracing::error!(
                "DropTradePackToFloor: character {} has no system container",
                player.character_id
            );
            return;
        };
        let data = txn.data();
        let moved = {
            let (mut sys, eq) = lock_pair(&system, &equipment);
            match eq {
                Some(mut eq) => sys.move_item_from(
                    &mut eq,
                    task_type::DROP_BACKPACK,
                    item.id,
                    -1,
                    data,
                    &mut txn.env.fx,
                ),
                None => false,
            }
        };
        if !moved {
            return;
        }
    }
    // Spawn doodad
    tracing::trace!("Spawn tradepack on floor on death");
    let obj = WorldObject::from(player);
    let here = match server.world.world_of(&obj) {
        Some(w) => w.world_pos_rot(&obj),
        None => obj.transform().local(),
    };
    let ground = crate::systems::ai::geodata::geo_data_height(
        server,
        &obj,
        crate::systems::skills::controllers::Vec3::new(here.x, here.y, here.z),
    );
    let spawn = BackpackDoodadSpawn {
        doodad_template_id: backpack_doodad_id,
        item_id: item.id,
        item_template_id: item.template_id,
        ucc_id: item.ucc_id,
        x: here.x,
        y: here.y,
        z: ground,
        rot: [here.rot_x, here.rot_y, here.rot_z],
        house: None,
    };
    let hooks = server.systems.economy.item_effects.hooks();
    if !hooks.spawn_backpack_doodad(server, player, &spawn) {
        return;
    }
    if let Some(world) = server.world.world_of(&obj) {
        crate::world::broadcast(
            &world,
            obj.clone(),
            &SCUnitEquipmentsChangedPacket {
                obj_id: player.obj_id,
                items: vec![(equip_slot::BACKPACK as u8, None)],
            },
            false,
        );
    }
}

/// `RepairSlaveEffect.Apply` (repair kit on a destroyed vehicle's summon item).
/// `item_target` = `SkillCastItemTarget` `(Id, ObjId)`; `effect_id` = the effect's `Id`.
pub fn repair_slave_item(server: &GameServer, effect_id: u32, item_target: Option<(u64, u32)>) {
    let Some((item_id, obj_id)) = item_target else {
        tracing::warn!("RepairSlaveEffect target is not a SkillCastItemTarget");
        return;
    };
    let mut txn = ItemTxn::begin(server);
    let item = server.items.item_by_id(item_id);
    let Some(target_player) = server.world.character_by_obj_id(obj_id) else {
        tracing::warn!("RepairSlaveEffect target unit {obj_id} is not a player");
        return;
    };
    let Some(item) = item.filter(|i| i.class == ItemClass::SummonSlave) else {
        tracing::warn!("RepairSlaveEffect target item {item_id} is not a salve summon item");
        return;
    };
    let Some(TemplateKind::SummonSlave { slave_id }) = server
        .game_data
        .items
        .template(item.template_id)
        .map(|t| t.kind.clone())
    else {
        return;
    };
    if !server
        .game_data
        .systems
        .economy
        .item_effects
        .has_repair_effect_id(slave_id, effect_id)
    {
        send_error_message_to(
            &mut txn.out(),
            &target_player,
            error_message::ITEM_FAILED_REPAIR,
        );
        tracing::warn!(
            "{} tried to use the wrong repair item {} (template: {} for slave type {slave_id}",
            target_player.data().unit.name,
            item.id,
            item.template_id
        );
        return;
    }
    let Some(after) = with_any_item_mut(server, item_id, |i| {
        i.details.slave_is_destroyed = 0;
        i.details.slave_repair_start_time = chrono::Utc::now().timestamp();
        i.dirty = true;
        i.clone()
    }) else {
        return;
    };
    txn.out().send(
        &target_player,
        task_packet(task_type::REPAIR_SLAVES, vec![ItemTask::update(&after)]),
    );
    tracing::debug!(
        "{} repaired slave on item {}",
        target_player.data().unit.name,
        after.id
    );
}
