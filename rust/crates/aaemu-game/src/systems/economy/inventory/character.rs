use std::collections::HashMap;

use crate::data::items::ItemTemplate;
use crate::models::container::slot_type;
use crate::models::item_task::{ItemFx, ItemTask};
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

use super::super::error_message;
use super::super::item_packets::{SCCharacterLaborPowerChangedPacket, SCGamePointChangedPacket};
use super::{Inv, Out};

pub mod game_point_kind {
    pub const HONOR: u8 = 0;
    pub const VOCATION: u8 = 1;
}

// ---------------------------------------------------------------------------------------------
// Character members
// ---------------------------------------------------------------------------------------------

/// `Character.SendErrorMessage(errorMsgType, type, isNotify)`.
pub fn send_error_message(player: &PlayerRef, error: i16, kind: u32, is_notify: bool) {
    player.send(SCErrorMsgPacket::new(error, kind, is_notify));
}

/// `SendErrorMessage` through `out`.
pub fn send_error_message_to(out: &mut Out<'_>, player: &PlayerRef, error: i16) {
    out.send(player, SCErrorMsgPacket::new(error, 0, true));
}

/// `Character.ChangeMoney(typeFrom, typeTo, amount, itemTaskType)`. The packet is sent even
/// when nothing changed (`SlotType.None` -> `None`), as in C#.
pub fn change_money(
    out: &mut Out<'_>,
    player: &PlayerRef,
    from: u8,
    to: u8,
    amount: i32,
    task: u8,
) -> bool {
    let mut tasks = Vec::new();
    let mut gold_report = None;
    {
        let mut c = player.data();
        let before = c.money;
        match from {
            slot_type::INVENTORY => {
                if i64::from(amount) > c.money {
                    drop(c);
                    send_error_message_to(out, player, error_message::NOT_ENOUGH_MONEY);
                    return false;
                }
                c.money -= i64::from(amount);
                tasks.push(ItemTask::MoneyChange(-amount));
            }
            slot_type::BANK => {
                if i64::from(amount) > c.money2 {
                    drop(c);
                    send_error_message_to(out, player, error_message::NOT_ENOUGH_MONEY);
                    return false;
                }
                c.money2 -= i64::from(amount);
                tasks.push(ItemTask::MoneyChangeBank(-amount));
            }
            _ => {}
        }
        match to {
            slot_type::INVENTORY => {
                c.money += i64::from(amount);
                tasks.push(ItemTask::MoneyChange(amount));
            }
            slot_type::BANK => {
                c.money2 += i64::from(amount);
                tasks.push(ItemTask::MoneyChangeBank(amount));
            }
            _ => {}
        }
        // `MY_GOLD` is a high-water record: only an upward change of the carried purse is
        // reported. The character lock is released here; the report itself runs from the item
        // effects (`Out::Queue`) or the achievement deferral (`Out::Direct`), never inside this
        // function's lock.
        if c.money > before {
            gold_report = Some(c.money);
        }
    }
    if let Some(money) = gold_report {
        match &mut *out {
            Out::Direct(server) => {
                crate::systems::social::achievement_hooks::report_gold(server, player, money);
            }
            Out::Queue(_, fx) => fx.push(ItemFx::Money {
                owner: player.character_id,
                money,
            }),
        }
    }
    out.send(
        player,
        super::item_packets::SCItemTaskSuccessPacket {
            action: task,
            tasks,
            force_remove: Vec::new(),
        },
    );
    true
}

/// `Character.ChangeMoney(moneyLocation, amount, itemTaskType)`.
pub fn change_money_at(
    out: &mut Out<'_>,
    player: &PlayerRef,
    location: u8,
    amount: i32,
    task: u8,
) -> bool {
    change_money(out, player, slot_type::NONE, location, amount, task)
}

/// `Character.AddMoney`.
pub fn add_money(
    out: &mut Out<'_>,
    player: &PlayerRef,
    location: u8,
    amount: i32,
    task: u8,
) -> bool {
    if amount < 0 {
        return false;
    }
    change_money(out, player, slot_type::NONE, location, amount, task)
}

/// `Character.SubtractMoney`.
pub fn subtract_money(
    out: &mut Out<'_>,
    player: &PlayerRef,
    location: u8,
    amount: i32,
    task: u8,
) -> bool {
    if amount < 0 {
        return false;
    }
    change_money(out, player, slot_type::NONE, location, -amount, task)
}

/// `Character.ChangeGamePoints(kind, change)`.
pub fn change_game_points(out: &mut Out<'_>, player: &PlayerRef, kind: u8, mut change: i32) {
    {
        let mut c = player.data();
        match kind {
            game_point_kind::HONOR => c.honor_point += change,
            game_point_kind::VOCATION => {
                use crate::models::unit::UnitAttribute;
                let now = crate::world::now_ms();
                let voc_add =
                    c.unit
                        .calculate_with_bonuses(0.0, UnitAttribute::LivingPointGain, now)
                        as f32;
                change = f64::from(change as f32 + voc_add).round_ties_even() as i32;
                let voc_mul =
                    c.unit
                        .calculate_with_bonuses(0.0, UnitAttribute::LivingPointGainMul, now)
                        as f32
                        + 100.0;
                change = f64::from(change as f32 * (voc_mul / 100.0)).round_ties_even() as i32;
                c.vocation_point += change;
            }
            _ => {
                tracing::error!("ChangeGamePoints - Unknown Game Point Type {kind}");
                return;
            }
        }
    }
    out.send(
        player,
        SCGamePointChangedPacket {
            kind,
            amount: change,
        },
    );
}

/// `Character.ChangeLabor(change, actabilityId)`.
pub fn change_labor(
    out: &mut Out<'_>,
    server: &GameServer,
    player: &PlayerRef,
    change: i16,
    actability_id: i32,
) {
    let mut actability_change = 0;
    let mut actability_step = 0u8;
    let mut exp_multiplier = 1f32;
    let level;
    {
        let mut c = player.data();
        level = c.unit.level;
        if actability_id > 0 {
            if let Some(a) = c
                .actability
                .actabilities
                .iter_mut()
                .find(|a| a.id == actability_id as u32)
            {
                // Get multiplier before adding points
                exp_multiplier = actability_exp_multiplier(a.step);
                actability_change =
                    (f64::from(change).abs() * server.config.world.actability_rate) as i32;
                actability_step = a.step;
                // CharacterActability.AddPoint
                let previous = a.point;
                a.point += actability_change;
                if let Some(t) = server.game_data.characters.expert_limit(a.step as usize) {
                    if a.point > t.up_limit {
                        a.point = t.up_limit;
                    }
                }
                actability_change = a.point - previous;
            } else {
                // C#: `Actabilities[(uint)actabilityId]` throws KeyNotFoundException.
                tracing::error!("ChangeLabor: unknown actability {actability_id}");
                return;
            }
        }
    }
    // Only grant xp if consuming labor
    if change < 0 {
        let mut params = HashMap::new();
        params.insert("labor_power", f64::from(-change));
        params.insert("pc_level", f64::from(level));
        let xp = server
            .game_data
            .formulas
            .get_formula_by_kind(crate::data::formulas::FormulaKind::ExpByLaborPower)
            .map_or(0.0, |f| f.evaluate(&params));
        let xp_to_add = (xp * f64::from(exp_multiplier)) as i32;
        match &mut *out {
            // Inside an `ItemTxn` (the ops lock is held): `AddExp` ends in quest events, which
            // take the non-reentrant ops lock themselves. Queue it behind the item tasks;
            // `fx::flush` applies it after the lock is released, before the labor packet below.
            Out::Queue(_, fx) => fx.push(ItemFx::Exp {
                owner: player.character_id,
                exp: xp_to_add,
                add_ability_exp: true,
            }),
            Out::Direct(_) => {
                if let Some(h) = server.systems.economy.hooks.progression.get() {
                    h.add_exp(server, player, xp_to_add, true);
                }
            }
        }
    }
    player.data().change_labor_power(change);
    out.send(
        player,
        SCCharacterLaborPowerChangedPacket {
            amount: i32::from(change),
            action: actability_id,
            point: actability_change,
            step: actability_step,
        },
    );
}

/// `Actability.GetExpMultiplier()` (1.2 table: all 1.0).
pub fn actability_exp_multiplier(_step: u8) -> f32 {
    1.0
}

/// `Actability.GetLaborCostMultiplier()`.
pub fn actability_labor_cost_multiplier(step: u8) -> f32 {
    const T: [f32; 8] = [1.00, 1.00, 0.95, 0.90, 0.85, 0.80, 0.77, 0.77];
    T[usize::from(step).min(T.len() - 1)]
}

/// `Actability.GetProductionTimeMultiplier()`.
pub fn actability_time_multiplier(step: u8) -> f32 {
    const T: [f32; 8] = [1.00, 1.00, 0.95, 0.90, 0.85, 0.80, 0.77, 0.77];
    T[usize::from(step).min(T.len() - 1)]
}

/// Validate item-cast location before costs, placement, summoning or item consumption.
pub(crate) fn item_use_zone_allowed(
    server: &GameServer,
    player: &PlayerRef,
    template_id: u32,
) -> bool {
    let banned =
        crate::systems::skills::data::zone_rules::current_group(server, &WorldObject::from(player))
            .is_some_and(|group| {
                server
                    .game_data
                    .systems
                    .skills
                    .zone_rules
                    .bans_item(group, template_id)
            });
    if banned {
        // Native 1.2 x2game12.c:1163648: ErrorMessage 440 = item_cannot_use_here.
        send_error_message(player, error_message::ITEM_CANNOT_USE_HERE, 0, true);
    }
    !banned
}

/// The 1.2 client's item actability check (`FUN_394e1a90`, called by the use/equip action
/// `FUN_390447d0` and the equip-drag validation `FUN_394e41f0`): with
/// `items.actability_requirement` > 0 the character's points in `items.actability_group_id`,
/// plus the house act-ability bonus (`FUN_390b3a80`), must reach the requirement. The client
/// shows `actability_not_enough_point` (508) through `FUN_39428370(0x1fc)`.
pub fn meets_item_actability(
    server: &GameServer,
    player: &PlayerRef,
    template: &ItemTemplate,
) -> bool {
    let requirement = i64::from(template.actability_requirement);
    if requirement <= 0 {
        return true;
    }
    let group_id = template.actability_group_id;
    let points = {
        let c = player.data();
        c.actability
            .actabilities
            .iter()
            .find(|a| a.id == group_id.max(0) as u32)
            .map_or(0, |a| i64::from(a.point))
    };
    let me = WorldObject::from(player);
    let bonus = server.world.world_of(&me).map_or(0, |world| {
        let pos = world.world_pos_rot(&me);
        crate::systems::housing::act_ability_bonus_at_in_world(
            server, world.id, group_id, pos.x, pos.y,
        )
    });
    points.saturating_add(i64::from(bonus)) >= requirement
}

/// Refuses the item use / equip with the client's `actability_not_enough_point` (508) and the
/// actability group as the error `kind` (as `craft.rs` does for crafts). Returns whether the
/// action was refused; the caller must then leave the item where it is.
pub fn refuse_item_actability(server: &GameServer, player: &PlayerRef, template_id: u32) -> bool {
    let Some(template) = server.game_data.items.template(template_id) else {
        return false;
    };
    if meets_item_actability(server, player, template) {
        return false;
    }
    send_error_message(
        player,
        error_message::ACTABILITY_NOT_ENOUGH_POINT,
        template.actability_group_id.max(0) as u32,
        true,
    );
    true
}

/// The 1.2 client's equip actability gate for a `SplitOrMoveItem` request: resolves the item
/// that would enter the character's equipment container and refuses it when the actability
/// requirement is unmet. Two request shapes put the *target* item into the equipment slot: the
/// client sends an empty equipment slot as the source (`DoEquipInEmptySlot`), and a swap of an
/// equipped item onto an occupied bag / bank slot moves that target item into the vacated
/// equipment slot (`DoSwap`). The source item may live in a coffer (opened through a bank
/// doodad), which `Inv` does not hold, so item ids also resolve through the server item manager.
#[allow(clippy::too_many_arguments)]
pub fn equip_actability_refused(
    server: &GameServer,
    player: &PlayerRef,
    from_type: u8,
    from_item_id: u64,
    to_type: u8,
    to_item_id: u64,
    to_slot: u8,
) -> bool {
    let inv = Inv::of(player);
    let resolve = |id: u64| {
        inv.item_by_id(id)
            .or_else(|| server.items.item_by_id(id))
            .map(|i| i.template_id)
    };
    let template_id = if from_type == slot_type::EQUIPMENT && to_type != slot_type::EQUIPMENT {
        inv.item_by_id(to_item_id)
            .or_else(|| inv.item_at(to_type, to_slot))
            .or_else(|| server.items.item_by_id(to_item_id))
            .map(|i| i.template_id)
    } else if from_type != slot_type::EQUIPMENT
        && to_type == slot_type::EQUIPMENT
        && from_item_id != 0
    {
        resolve(from_item_id)
    } else {
        None
    };
    template_id.is_some_and(|id| refuse_item_actability(server, player, id))
}

/// `Character.ItemUse(item)` (`Events.OnItemUse` -> quests).
pub fn item_use(server: &GameServer, player: &PlayerRef, template_id: u32) {
    if template_id == 0 {
        return;
    }
    if let Some(h) = server.systems.economy.hooks.quests.get() {
        h.on_item_use(server, player, template_id);
    }
}
