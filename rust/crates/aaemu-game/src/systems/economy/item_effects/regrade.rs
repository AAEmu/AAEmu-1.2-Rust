use std::collections::HashMap;

use crate::data::formulas::FormulaKind;
use crate::data::items::{FullGradeTemplate, ItemGradeEnchantingSupport, TemplateKind};
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemTask};
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::PlayerRef;

use super::super::inventory::{send_error_message_to, Inv, ItemTxn};
use super::super::item_effect_packets::{
    grade_enchant_result, SCGradeEnchantBroadcastPacket, SCGradeEnchantResultPacket,
    SCSkillEndedPacket,
};
use super::super::{error_message, inventory};
use super::LootRng;
use super::{broadcast_with_self, lock, task_packet, SkillItem};

// ---------------------------------------------------------------------------------------------
// GradeEnchant
// ---------------------------------------------------------------------------------------------

/// The 1.2 client's highest regrade target grade (`FUN_393a0130`: `item.grade == 0xb`).
const GRADE_ENCHANT_MAX_GRADE: u8 = 11;

/// The item class `FUN_393a0130` compares with the scroll's `special_effects.value3`
/// (`X2ItemDesc` `+0x48`): 1 weapon, 2 armor, 24 accessory, anything else 0.
fn grade_enchant_kind(kind: &TemplateKind) -> i32 {
    match kind {
        TemplateKind::Weapon(_) => 1,
        TemplateKind::Armor(_) => 2,
        TemplateKind::Accessory(_) => 24,
        _ => 0,
    }
}

/// Refuses the effect but keeps the cast's source item: `skill_cancelled` makes
/// `apply_effects` drop the `CastSourceDebit` and refund the scroll.
fn cancel_grade_enchant(player: &PlayerRef) {
    player.data().skill_char.skill_cancelled = true;
}

/// `GradeEnchant.Execute` (special effect `GradeEnchant`; regrade scrolls). `value1 != 0` =
/// lucky scroll (great success possible), `value3` = item kind (1 weapon, 2 armor, 24
/// accessory). `support_item_id` = `SkillObjectItemGradeEnchantingSupport.SupportItemId`.
/// Sends `SCSkillEndedPacket(skill_tl_id)` itself, as the C# body does.
#[allow(clippy::too_many_arguments)]
pub fn grade_enchant(
    server: &GameServer,
    player: &PlayerRef,
    scroll: Option<SkillItem>,
    scroll_item: Option<&Item>,
    target_item_id: Option<u64>,
    support_item_id: Option<u64>,
    values: [i32; 4],
    skill_tl_id: u16,
    rng: &mut dyn LootRng,
) {
    let [value1, value2, value3, value4] = values;
    tracing::debug!(
        "Special effects: GradeEnchant value1 {value1}, value2 {value2}, value3 {value3}, value4 {value4}"
    );
    let Some(scroll) = scroll else { return };
    let Some(target_id) = target_item_id else {
        // A malformed cast still took the scroll; refund it.
        cancel_grade_enchant(player);
        return;
    };
    let use_charm = support_item_id.is_some_and(|id| id != 0);
    let is_lucky = value1 != 0;
    let data = &server.game_data.items;

    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let Some(holder) = inv.holder_of(target_id) else {
        cancel_grade_enchant(player); // Invalid item
        return;
    };
    let Some(item) = lock(&holder).get_item_by_id(target_id).cloned() else {
        cancel_grade_enchant(player);
        return;
    };
    // 1.2 client `FUN_393a0130` (the `GradeEnchant` special effect's target check, switch case
    // 0x5c): an item whose `items.grade_enchantable` is false is refused with SkillResult
    // INVALID_TARGET (0x22) before any roll. The C# never read the column, so it regraded
    // anything. Cancel the cast like `item_socketing` does, so the scroll (the cast's source
    // item, `use_skill_as_reagent`) is refunded and no gold or charm is taken.
    let Some(template) = data.template(item.template_id) else {
        tracing::error!("GradeEnchant: unknown item template {}", item.template_id);
        cancel_grade_enchant(player);
        return;
    };
    if !template.grade_enchantable {
        send_error_message_to(&mut txn.out(), player, error_message::INVALID_TARGET);
        drop(txn);
        cancel_grade_enchant(player);
        return;
    }
    // `FUN_393a0130`: the target's `level_requirement` (`+0x54`) must reach the scroll's
    // `special_effects.value2`; the client answers SkillResult INVALID_TARGET (0x22) with
    // error 0x206 (`level_low_to_use`).
    if template.level_requirement < value2 {
        send_error_message_to(&mut txn.out(), player, error_message::LEVEL_LOW_TO_USE);
        drop(txn);
        cancel_grade_enchant(player);
        return;
    }
    // `FUN_393a0130`: the target's item class (`+0x48`) must equal the scroll's
    // `special_effects.value3` (1 weapon, 2 armor, 24 accessory; -1 = any), else
    // INVALID_TARGET (0x22).
    if value3 != -1 && grade_enchant_kind(&template.kind) != value3 {
        send_error_message_to(&mut txn.out(), player, error_message::INVALID_TARGET);
        drop(txn);
        cancel_grade_enchant(player);
        return;
    }
    if super::item_security::refuse_if_locked(player, &item) {
        cancel_grade_enchant(player);
        return;
    }
    let initial_grade = item.grade;
    // `FUN_393a0130`: a grade-11 target is refused with `grade_enchant_max` (0x299).
    if initial_grade == GRADE_ENCHANT_MAX_GRADE {
        send_error_message_to(&mut txn.out(), player, error_message::GRADE_ENCHANT_MAX);
        drop(txn);
        cancel_grade_enchant(player);
        return;
    }
    let Some(grade_template) = data.full_grade(i32::from(item.grade)).cloned() else {
        // C#: NullReferenceException in GoldCost.
        tracing::error!("GradeEnchant: no grade template for grade {}", item.grade);
        cancel_grade_enchant(player);
        return;
    };
    let Some(cost) = gold_cost(server, &grade_template, &item, value3) else {
        cancel_grade_enchant(player);
        return;
    };
    if cost == -1 {
        cancel_grade_enchant(player); // No gold on template, invalid ?
        return;
    }
    if player.data().money < i64::from(cost) {
        send_error_message_to(&mut txn.out(), player, error_message::NOT_ENOUGH_MONEY);
        cancel_grade_enchant(player);
        return;
    }
    let has_scroll = match scroll_item {
        Some(item) => item.template_id == scroll.item_template_id && item.count > 0,
        None => inv.check_items(slot_type::INVENTORY, scroll.item_template_id, 1),
    };
    if !has_scroll {
        // No scroll
        send_error_message_to(
            &mut txn.out(),
            player,
            error_message::NOT_ENOUGH_REQUIRED_ITEM,
        );
        cancel_grade_enchant(player);
        return;
    }
    let mut charm: Option<(Item, ItemGradeEnchantingSupport)> = None;
    if use_charm {
        let Some(charm_item) = inv.item_by_id(support_item_id.unwrap_or(0)) else {
            cancel_grade_enchant(player);
            return;
        };
        let Some(info) = data.enchanting_support(charm_item.template_id).copied() else {
            // C#: NullReferenceException on charmInfo.RequireGradeMin.
            tracing::error!(
                "GradeEnchant: item {} is not a grade enchanting support",
                charm_item.template_id
            );
            cancel_grade_enchant(player);
            return;
        };
        if info.require_grade_min != -1 && i32::from(item.grade) < info.require_grade_min {
            send_error_message_to(
                &mut txn.out(),
                player,
                error_message::NOT_ENOUGH_REQUIRED_ITEM,
            );
            cancel_grade_enchant(player);
            return;
        }
        if info.require_grade_max != -1 && i32::from(item.grade) > info.require_grade_max {
            send_error_message_to(&mut txn.out(), player, error_message::GRADE_ENCHANT_MAX);
            cancel_grade_enchant(player);
            return;
        }
        charm = Some((charm_item, info));
    }

    // All seems to be in order, roll item, consume items and send the results
    let Some((result, new_grade)) = roll_regrade(
        server,
        &grade_template,
        is_lucky,
        charm.as_ref().map(|c| &c.1),
        rng,
    ) else {
        cancel_grade_enchant(player);
        return;
    };
    let after = if result == grade_enchant_result::BREAK {
        // Poof
        lock(&holder)
            .remove_item_task(task_type::GRADE_ENCHANT, target_id, true, &mut txn.env)
            .unwrap_or_else(|| item.clone())
    } else {
        // No Poof
        let mut g = lock(&holder);
        let Some(it) = g.get_item_by_id_mut(target_id) else {
            return;
        };
        if let Some(grade) = new_grade {
            it.grade = grade;
            it.dirty = true;
        }
        let after = it.clone();
        drop(g);
        txn.out().send(
            player,
            task_packet(
                task_type::GRADE_ENCHANT,
                vec![ItemTask::grade_change(&after, after.grade)],
            ),
        );
        after
    };

    // Consume
    inventory::subtract_money(
        &mut txn.out(),
        player,
        slot_type::INVENTORY,
        cost,
        task_type::DEPOSIT_MONEY,
    );
    // The scroll is consumed by the skill as a reagent.
    if let Some((charm_item, _)) = &charm {
        lock(&inv.bag()).consume_item(
            task_type::GRADE_ENCHANT,
            charm_item.template_id,
            1,
            Some(charm_item.id),
            &mut txn.env,
        );
    }
    txn.out().send(
        player,
        SCGradeEnchantResultPacket {
            result,
            item: after.clone(),
            type1: initial_grade,
            type2: after.grade,
        },
    );
    drop(txn);
    broadcast_with_self(server, player, &SCSkillEndedPacket { tl: skill_tl_id });
    report_enchant(server, player, result, after.grade);

    // Let the world know if we got lucky enough
    if after.grade >= 8
        && (result == grade_enchant_result::SUCCESS
            || result == grade_enchant_result::GREAT_SUCCESS)
    {
        let name = player.data().unit.name.clone();
        server
            .world
            .broadcast_to_server(&SCGradeEnchantBroadcastPacket {
                char_name: name,
                result,
                item: after.clone(),
                type1: initial_grade,
                type2: after.grade,
            });
    }
}

/// ENCHANT_ITEM (the resulting grade, successes only) / ENCHANT_FAILURE (a plain failure)
/// achievement credit for one regrade.
fn report_enchant(server: &GameServer, player: &PlayerRef, result: u8, grade: u8) {
    let input = if result == grade_enchant_result::SUCCESS
        || result == grade_enchant_result::GREAT_SUCCESS
    {
        AchievementInput::Increment {
            kind_id: char_record_kind::ENCHANT_ITEM,
            amount: 1,
            matcher: RecordMatch::Value1(u32::from(grade)),
        }
    } else if result == grade_enchant_result::FAIL {
        AchievementInput::Increment {
            kind_id: char_record_kind::ENCHANT_FAILURE,
            amount: 1,
            matcher: RecordMatch::Any,
        }
    } else {
        return;
    };
    crate::systems::report_achievement(server, player, input);
}

/// `GradeEnchant.RollRegrade`: `(result, new grade)` (`None` new grade = unchanged). `None`
/// where the C# throws (`GetGradeTemplateByOrder` returned null).
pub fn roll_regrade(
    server: &GameServer,
    grade_template: &FullGradeTemplate,
    is_lucky: bool,
    charm: Option<&ItemGradeEnchantingSupport>,
    rng: &mut dyn LootRng,
) -> Option<(u8, Option<u8>)> {
    let success_roll = rng.next_i32(0, 10000);
    let break_roll = rng.next_i32(0, 10000);
    let downgrade_roll = rng.next_i32(0, 10000);
    let great_success_roll = rng.next_i32(0, 10000);
    let g = grade_template;
    let chance = |base: i32, f: fn(&ItemGradeEnchantingSupport) -> (i32, i32)| match charm {
        Some(c) => {
            let (ratio, mul) = f(c);
            charm_chance(base, ratio, mul)
        }
        None => base,
    };
    let success_chance = chance(g.enchant_success_ratio, |c| {
        (c.add_success_ratio, c.add_success_mul)
    });
    let great_success_chance = chance(g.enchant_great_success_ratio, |c| {
        (c.add_great_success_ratio, c.add_great_success_mul)
    });
    let break_chance = chance(g.enchant_break_ratio, |c| {
        (c.add_break_ratio, c.add_break_mul)
    });
    let downgrade_chance = chance(g.enchant_downgrade_ratio, |c| {
        (c.add_downgrade_ratio, c.add_downgrade_mul)
    });
    let next_grade = |change: i32| -> Option<u8> {
        match server
            .game_data
            .items
            .grade_by_order(g.grade_order + change)
        {
            Some(t) => Some(t.grade as u8),
            None => {
                // C#: NullReferenceException on GetNextGrade(...).Grade
                tracing::error!(
                    "GradeEnchant: no grade with order {}",
                    g.grade_order + change
                );
                None
            }
        }
    };

    if success_roll < success_chance {
        if is_lucky && great_success_roll < great_success_chance {
            let increase = charm.map_or(2, |c| 2 + c.add_great_success_grade);
            let grade = next_grade(increase)?;
            return Some((grade_enchant_result::GREAT_SUCCESS, Some(grade)));
        }
        let grade = next_grade(1)?;
        return Some((grade_enchant_result::SUCCESS, Some(grade)));
    }
    if break_roll < break_chance {
        return Some((grade_enchant_result::BREAK, None));
    }
    if downgrade_roll < downgrade_chance {
        // `newGrade < 0` is never true for a byte.
        let new_grade = rng.next_i32(g.enchant_downgrade_min, g.enchant_downgrade_max) as u8;
        return Some((grade_enchant_result::DOWNGRADE, Some(new_grade)));
    }
    Some((grade_enchant_result::FAIL, None))
}

/// `GradeEnchant.GetCharmChance`.
pub fn charm_chance(base_chance: i32, charm_ratio: i32, charm_mul: i32) -> i32 {
    base_chance
        .wrapping_add(charm_ratio)
        .wrapping_add((f64::from(base_chance) * (f64::from(charm_mul) / 100.0)) as i32)
}

/// `GradeEnchant.GoldCost(gradeTemplate, item, itemType)`: `Some(-1)` = no slot type,
/// `None` where the C# throws (template cast / missing enchanting cost / formula).
pub fn gold_cost(
    server: &GameServer,
    grade_template: &FullGradeTemplate,
    item: &Item,
    item_type: i32,
) -> Option<i32> {
    let data = &server.game_data.items;
    let Some(t) = data.template(item.template_id) else {
        tracing::error!("GradeEnchant: unknown item template {}", item.template_id);
        return None;
    };
    let slot_type_id = match (item_type, &t.kind) {
        (1, TemplateKind::Weapon(e))
        | (2, TemplateKind::Armor(e))
        | (24, TemplateKind::Accessory(e)) => e.slot_type_id,
        (1 | 2 | 24, _) => {
            // C#: InvalidCastException on the template cast.
            tracing::error!(
                "GradeEnchant: item {} does not match item type {item_type}",
                item.template_id
            );
            return None;
        }
        _ => 0,
    };
    if slot_type_id == 0 {
        return Some(-1);
    }
    let Some(enchanting_cost) = data.equip_slot_enchanting_cost(slot_type_id) else {
        tracing::error!("GradeEnchant: no enchanting cost for slot type {slot_type_id}");
        return None;
    };
    let params: HashMap<&str, f64> = [
        ("item_grade", f64::from(grade_template.enchant_cost)),
        ("item_level", f64::from(t.level)),
        ("equip_slot_enchant_cost", f64::from(enchanting_cost.cost)),
    ]
    .into_iter()
    .collect();
    let Some(formula) = server
        .game_data
        .formulas
        .get_formula_by_kind(FormulaKind::GradeEnchantCost)
    else {
        tracing::error!("GradeEnchant: no GradeEnchantCost formula");
        return None;
    };
    Some(formula.evaluate(&params) as i32)
}
