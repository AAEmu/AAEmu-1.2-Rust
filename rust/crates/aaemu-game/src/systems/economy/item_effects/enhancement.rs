use crate::data::items::{ItemData, ItemTemplate};
use crate::models::container::slot_type;
use crate::models::item::{Item, ITEM_FLAG_SECURE};
use crate::models::item_task::{task_type, ItemTask};
use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

use super::super::error_message;
use super::super::inventory::{send_error_message_to, Inv, ItemTxn};
use super::super::item_effect_packets::{
    SCItemSocketingLunagemResultPacket, SCItemSocketingLunastoneResultPacket, SCSkillEndedPacket,
};
use super::LootRng;
use super::{broadcast_with_self, lock, system_message, task_packet, Outcome, SkillItem};

// ---------------------------------------------------------------------------------------------
// Socketing / tempering / looks
// ---------------------------------------------------------------------------------------------

/// The 1.2 client refuses to socket into items below this item level (`content_configs` id 62,
/// kind 24, value 20; checked before the install/remove branch of `FUN_397ceb80`).
const ITEM_SOCKET_MIN_ITEM_LEVEL: i32 = 20;

/// Gate for installing one lunagem, mirroring the client's `FUN_397ceb80`:
/// `item_socket_level_limits` is checked first (`socket_target_level`), then the gem's equip
/// slot group (`item_sockets`) against the target slot type through `equip_slot_group_maps`,
/// and `item_socket_num_limits` for the target's slot type and grade. A missing/zero socket
/// limit is `invalid_target` (`FUN_395aca00` returns 0, the client maps it to 0x12); only an
/// existing limit reached by the current gem count is `item_sockets_full`.
/// Returns the `SCErrorMsg` code the 1.2 client uses for the rejected rule.
fn socket_install_rejection(
    data: &ItemData,
    target: &Item,
    template: &ItemTemplate,
    gem_template_id: u32,
    gem_count: usize,
) -> Result<(), i16> {
    if let Some(required_level) = data.item_socket_level_limit(gem_template_id) {
        if template.level_requirement < required_level {
            return Err(error_message::SOCKET_TARGET_LEVEL);
        }
    }
    let Some(group_id) = data.item_socket_group(gem_template_id) else {
        return Err(error_message::INVALID_TARGET);
    };
    let Some(slot_type_id) = template.gear_slot_type() else {
        return Err(error_message::INVALID_TARGET);
    };
    let limit = data.item_socket_num_limit(slot_type_id, target.grade);
    if limit == 0 || !data.socket_group_allows(group_id, slot_type_id) {
        return Err(error_message::INVALID_TARGET);
    }
    if gem_count >= usize::from(limit) {
        return Err(error_message::ITEM_SOCKETS_FULL);
    }
    Ok(())
}

/// `ItemSocketing.Execute` (lunagems; `Item.DawnStone` removes every gem). Sends
/// `SCSkillEndedPacket(skill_tl_id)` itself. A rejected attempt sends the client's error and
/// sets `skill_cancelled`, so the cast refunds the source gem and no socket roll happens.
#[allow(clippy::too_many_arguments)]
pub fn item_socketing(
    server: &GameServer,
    player: &PlayerRef,
    gem: Option<SkillItem>,
    gem_item: Option<&Item>,
    target_item_id: Option<u64>,
    values: [i32; 4],
    skill_tl_id: u16,
    rng: &mut dyn LootRng,
) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: ItemSocketing value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let Some(gem) = gem else {
        tracing::error!("Special effects: ItemSocketing casterObj is not a SkillItem");
        return;
    };
    let Some(target_id) = target_item_id else {
        tracing::error!("Special effects: ItemSocketing targetObj is not a SkillCastItemTarget");
        return;
    };
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let target_holder = inv.holder_of(target_id);
    let gem_item = gem_item.cloned().or_else(|| inv.item_by_id(gem.item_id));
    let (Some(holder), Some(gem_item)) = (target_holder, gem_item) else {
        tracing::warn!(
            "Special effects: ItemSocketing targetItem {target_id} or gemItem {} not found",
            gem.item_id
        );
        drop(txn);
        player.data().skill_char.skill_cancelled = true;
        return;
    };
    let mut g = lock(&holder);
    let Some(equip) = g.get_item_by_id_mut(target_id) else {
        drop(g);
        player.data().skill_char.skill_cancelled = true;
        return;
    };
    if equip.has_flag(ITEM_FLAG_SECURE) {
        drop(g);
        send_error_message_to(&mut txn.out(), player, error_message::ITEM_SECURE_CONDITION);
        player.data().skill_char.skill_cancelled = true;
        return;
    }
    if !equip.is_equip() {
        tracing::warn!("Special effects: ItemSocketing targetItem {target_id} was not a EquipItem");
        drop(g);
        player.data().skill_char.skill_cancelled = true;
        return;
    }
    let data = &server.game_data.items;
    let mut rejection = None;
    let mut result = 0u8;
    let mut installed = false;
    match data.template(equip.template_id) {
        None => rejection = Some(error_message::INVALID_TARGET),
        Some(template) if template.level < ITEM_SOCKET_MIN_ITEM_LEVEL => {
            rejection = Some(error_message::SOCKET_TARGET_LEVEL);
        }
        Some(template) => {
            let gem_count = equip.details.gem_ids.iter().filter(|&&id| id != 0).count();
            if gem_item.template_id == crate::data::items::ITEM_DAWN_STONE {
                // DawnStone: the 1.2 client rejects a removal with no gems (`item_sockets_empty`).
                if gem_count == 0 {
                    rejection = Some(error_message::ITEM_SOCKETS_EMPTY);
                } else {
                    equip.details.gem_ids.fill(0);
                    result = 1;
                }
            } else if let Err(error) =
                socket_install_rejection(data, equip, template, gem_item.template_id, gem_count)
            {
                rejection = Some(error);
            } else {
                // All rules pass: roll for success. A rejected attempt never reaches this roll.
                let gem_roll = rng.next_i32(0, 10000);
                let gem_chance = data.socket_chance(gem_count as u32);
                if i64::from(gem_roll) < i64::from(gem_chance) {
                    equip.details.gem_ids[gem_count] = gem_item.template_id;
                    result = 1;
                } else {
                    // Failed: a failed socket attempt removes ALL currently socketed gems.
                    equip.details.gem_ids.fill(0);
                }
                installed = true;
            }
        }
    }
    if let Some(error) = rejection {
        drop(g);
        send_error_message_to(&mut txn.out(), player, error);
        drop(txn);
        // The cast already took the gem as its source item; cancelling refunds it and keeps
        // a rejected attempt free of a socket roll.
        player.data().skill_char.skill_cancelled = true;
        return;
    }
    // GemIds is a plain array, so no C# property setter marked the item dirty; without this the
    // next `ItemManager.Save` skips the socketing.
    equip.dirty = true;
    let after = equip.clone();
    drop(g);
    txn.out().send(
        player,
        task_packet(task_type::SOCKETING, vec![ItemTask::update(&after)]),
    );
    txn.out().send(
        player,
        SCItemSocketingLunagemResultPacket {
            result,
            item_id: after.id,
            template_id: gem_item.template_id,
            install: installed,
        },
    );
    drop(txn);
    // The gems' unit modifiers are part of the gear bonuses of an equipped item.
    if after.slot_type == slot_type::EQUIPMENT {
        refresh_gear_bonuses(server, player);
    }
    broadcast_with_self(server, player, &SCSkillEndedPacket { tl: skill_tl_id });
}

/// Recomputes the gear bonuses after an equipped item changed in place (gems, lunastone).
fn refresh_gear_bonuses(server: &GameServer, player: &PlayerRef) {
    if let Some(hooks) = server.systems.economy.hooks.gear.get() {
        hooks.update_gear_bonuses(server, &WorldObject::from(player), None, None);
    }
}

/// `MagicalEnchant.Execute` (world interaction: lunastones). The C# checks `targetType` before
/// `casterType`.
pub fn magical_enchant(
    server: &GameServer,
    player: &PlayerRef,
    skill_item: Option<SkillItem>,
    target_item_id: Option<u64>,
) {
    let Some(target_id) = target_item_id else {
        return;
    };
    let Some(skill_item) = skill_item else { return };
    let mut txn = ItemTxn::begin(server);
    // Deviation: the C# searched the bag only (`Bag.GetItemByItemId`); like the lunagem
    // socketing this also finds the item in the equipment (and bank) container.
    let Some(holder) = Inv::of(player).holder_of(target_id) else {
        return;
    };
    let mut g = lock(&holder);
    let Some(equip) = g.get_item_by_id_mut(target_id) else {
        return;
    };
    if !equip.is_equip() {
        return;
    }
    equip.details.rune_id = skill_item.item_template_id;
    // A plain details field: mark the item so the next save writes the lunastone.
    equip.dirty = true;
    let after = equip.clone();
    drop(g);
    txn.out().send(
        player,
        SCItemSocketingLunastoneResultPacket {
            result: true,
            item_id: after.id,
            template_id: skill_item.item_template_id,
        },
    );
    txn.out().send(
        player,
        task_packet(task_type::ENCHANT_MAGICAL, vec![ItemTask::update(&after)]),
    );
    drop(txn);
    // The lunastone's unit modifiers are part of the gear bonuses of an equipped item.
    if after.slot_type == slot_type::EQUIPMENT {
        refresh_gear_bonuses(server, player);
    }
}

/// `ItemCapScale.Execute` (tempering). `skill_id` = `skill.Id` (`GetItemCapScale(skillId)`).
pub fn item_cap_scale(
    server: &GameServer,
    player: &PlayerRef,
    temper: Option<SkillItem>,
    target_item_id: Option<u64>,
    skill_id: u32,
    values: [i32; 4],
    rng: &mut dyn LootRng,
) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: ItemCapScale value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    // C#: the (SkillItem) / (SkillCastItemTarget) casts throw on other types.
    let (Some(_), Some(target_id)) = (temper, target_item_id) else {
        tracing::error!("ItemCapScale: caster or target object of the wrong type");
        return;
    };
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let Some(holder) = inv.holder_of(target_id) else {
        return;
    };
    let mut g = lock(&holder);
    let Some(equip) = g.get_item_by_id_mut(target_id) else {
        return;
    };
    if !equip.is_equip() {
        tracing::error!("ItemCapScale: item {target_id} is not an EquipItem"); // InvalidCast
        return;
    }
    // `item_cap_scale_forbids` (1.2 client `FUN_395a72d0`, the item's "scalable" property;
    // `FUN_3939dc20` answers the cast with SkillResult INVALID_TARGET 0x22, refused by the cast
    // validation before the source item is taken). This is the last line of defence for direct
    // effect calls: cancel so the source item (the temper) is refunded.
    if server
        .game_data
        .items
        .cap_scale_forbidden(equip.template_id)
    {
        drop(g);
        drop(txn);
        player.data().skill_char.skill_cancelled = true;
        return;
    }
    let Some(scale) = server.game_data.items.item_cap_scale(skill_id).copied() else {
        tracing::error!("ItemCapScale: no item cap scale for skill {skill_id}"); // NRE
        return;
    };
    let physical = rng.next_i32(scale.scale_min, scale.scale_max) as u16;
    let magical = rng.next_i32(scale.scale_min, scale.scale_max) as u16;
    equip.details.temper_physical = physical;
    equip.details.temper_magical = magical;
    // Tempering must survive the next save.
    equip.dirty = true;
    let after = equip.clone();
    drop(g);
    // The temper item is consumed as a skill reagent.
    txn.out().send(
        player,
        task_packet(task_type::ENCHANT_PHYSICAL, vec![ItemTask::update(&after)]),
    );
    system_message(
        &mut txn,
        player,
        format!("Temper:\n |cFFFFFFFF{physical}%|r Physical\n|cFFFFFFFF{magical}%|r Magical"),
    );
}

/// `ItemCapScaleReset.Execute`.
pub fn item_cap_scale_reset(
    server: &GameServer,
    player: &PlayerRef,
    temper: Option<SkillItem>,
    target_item_id: Option<u64>,
    values: [i32; 4],
) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: ItemCapScaleReset value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let (Some(_), Some(target_id)) = (temper, target_item_id) else {
        tracing::error!("ItemCapScaleReset: caster or target object of the wrong type");
        return;
    };
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let Some(holder) = inv.holder_of(target_id) else {
        return;
    };
    let mut g = lock(&holder);
    let Some(equip) = g.get_item_by_id_mut(target_id) else {
        return;
    };
    if !equip.is_equip() {
        tracing::error!("ItemCapScaleReset: item {target_id} is not an EquipItem");
        return;
    }
    // The reset tool is refused by the same `item_cap_scale_forbids` set as `ItemCapScale`
    // (`FUN_395a72d0`; reset-tool path unverified, see PORTING.md); cancel so its source item is
    // refunded.
    if server
        .game_data
        .items
        .cap_scale_forbidden(equip.template_id)
    {
        drop(g);
        drop(txn);
        player.data().skill_char.skill_cancelled = true;
        return;
    }
    equip.details.temper_physical = 0;
    equip.details.temper_magical = 0;
    // A temper reset is a persisted equipment change too.
    equip.dirty = true;
    let after = equip.clone();
    drop(g);
    txn.out().send(
        player,
        task_packet(task_type::ENCHANT_PHYSICAL, vec![ItemTask::update(&after)]),
    );
    system_message(&mut txn, player, "Tempering reset".to_string());
}

/// `RechargeItemBuff.Execute` (special 95). The C# body is a `// TODO` stub; 1.2 data rows
/// 12446 / 17017 (all values 0) are cast by the consumed tool item 28242 (skill 22560 "Cleaner
/// and Clearer", target type 9 = Item), so the item comes from the cast's item target. The
/// target must be an equip item whose template carries `recharge_buff_id` (with
/// `charge_lifetime` minutes and/or `charge_count`); the effect starts a new charge window and
/// re-applies the recharge buff when the item is equipped. The item timers read the charge
/// fields every tick, so no timer is re-scheduled.
pub fn recharge_item_buff(
    server: &GameServer,
    player: &PlayerRef,
    target_item_id: Option<u64>,
    values: [i32; 4],
) -> Outcome {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: RechargeItemBuff value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let Some(target_id) = target_item_id else {
        return Outcome::Cancelled;
    };
    let inv = Inv::of(player);
    let Some(holder) = inv.holder_of(target_id) else {
        return Outcome::Cancelled;
    };
    let now = chrono::Utc::now().timestamp();
    let (after, equipped) = {
        let mut g = lock(&holder);
        let Some(item) = g.get_item_by_id_mut(target_id) else {
            return Outcome::Cancelled;
        };
        if !item.is_equip() {
            tracing::debug!("RechargeItemBuff: item {target_id} is not an EquipItem");
            return Outcome::Cancelled;
        }
        let Some(equip) = server
            .game_data
            .items
            .template(item.template_id)
            .and_then(|t| t.equip())
        else {
            return Outcome::Cancelled;
        };
        if equip.recharge_buff_id == 0 {
            return Outcome::Cancelled;
        }
        item.charge_start_time = now;
        if equip.charge_count > 0 {
            item.charge_count = equip.charge_count;
        }
        // The new window must survive the next save.
        item.dirty = true;
        let equipped = item.slot_type == slot_type::EQUIPMENT;
        (item.clone(), equipped)
    };
    {
        let mut txn = ItemTxn::begin(server);
        txn.out().send(
            player,
            task_packet(task_type::RECHARGE_BUFF, vec![ItemTask::update(&after)]),
        );
    }
    if equipped {
        // Refresh the gear buff without stacking a second copy: the removed item drops the old
        // recharge buff, the added one re-adds it from the new charge window.
        server.systems.skills.ext.items().apply_gear_buffs(
            server,
            &WorldObject::from(player),
            Some(&after),
            Some(&after),
        );
    }
    Outcome::Done
}

/// `CleanupLookConvert.Execute` (removes an item's look image, consumes one powder).
pub fn cleanup_look_convert(
    server: &GameServer,
    player: &PlayerRef,
    powder: Option<SkillItem>,
    powder_item: Option<&Item>,
    powder_debited: bool,
    target_item_id: Option<u64>,
    values: [i32; 4],
) -> Outcome {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: CleanupLookConvert value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let Some(target_id) = target_item_id else {
        return Outcome::Cancelled;
    };
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let Some(holder) = inv.holder_of(target_id) else {
        return Outcome::Cancelled;
    };
    let image = lock(&holder)
        .get_item_by_id(target_id)
        .map_or(0, |i| i.details.image_item_template_id);
    if image == 0 {
        // Is not an item with an image on it
        send_error_message_to(
            &mut txn.out(),
            player,
            error_message::ITEM_LOOK_CONVERT_AS_INVALID_COMBINATION,
        );
        return Outcome::Cancelled;
    }
    let Some(powder) = powder else {
        return Outcome::Cancelled;
    };
    let powder_item = match powder_item {
        Some(item) => item.clone(),
        None => {
            let Some(item) = inv.item_by_id(powder.item_id) else {
                return Outcome::Cancelled;
            };
            item
        }
    };
    if powder_item.count < 1 {
        return Outcome::Cancelled;
    }
    let after = {
        let mut g = lock(&holder);
        let Some(it) = g.get_item_by_id_mut(target_id) else {
            return Outcome::Cancelled;
        };
        it.details.image_item_template_id = 0;
        it.dirty = true;
        it.clone()
    };
    txn.out().send(
        player,
        task_packet(task_type::CONVERT_ITEM_LOOK, vec![ItemTask::update(&after)]),
    );
    // The cast's debit already consumed the powder; only the fallback pays for it here.
    if !powder_debited {
        if let Some(c) = inv.holder_of_any(powder_item.id) {
            lock(&c).consume_item(
                task_type::CONVERT_ITEM_LOOK,
                powder_item.template_id,
                1,
                Some(powder_item.id),
                &mut txn.env,
            );
        }
    }
    Outcome::Done
}
