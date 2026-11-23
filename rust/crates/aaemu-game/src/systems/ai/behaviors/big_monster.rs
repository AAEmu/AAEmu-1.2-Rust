use rand::Rng;

use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::instances::encounters::registry as encounters;

use super::super::combat::set_in_battle;
use super::super::framework::{AiCtx, BehaviorKind};
use super::super::params::{AiParamType, AiParams, AiParamsKind};
use super::super::skills::{self as ai_skills, SkillUseConditionKind};

use super::chase::move_in_range;
use super::npc;
use super::return_state::should_return;
use super::skill_use::{
    can_strafe, can_use_skill, health_ratio, is_using_skill, pick_skill_and_use_it,
    set_weapon_range, use_skill, MELEE_ATTACK_RANGE,
};
use super::support::{broadcast_posture, template_ai_params, template_scalars};
use super::targeting::{current_target, update_target};

// ===========================================================================================
// BigMonster
// ===========================================================================================

pub(super) fn big_monster_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    set_in_battle(c.server, &c.obj, true);
    super::npc_events::on_combat_started(c);
    c.ai.param = template_ai_params(c);
    // A big monster's chase range is that of its base attack when the attack reaches beyond
    // melee. The combat-skill picks never set it, and the fallback pick keeps the range it
    // found (see `big_monster_attack_tick`), so a ranged monster otherwise closed to
    // `attack_start_range_scale` metres: Element Fanatic Blangar's bow (10431, 15 m) and the
    // Maddened Shaman's Flame Cluster (19981, 20 m) were used from melee range in Kroloal
    // Cradle, like Sharpwind's Sniper, Fortuneteller, Illusionist and Colossus before them.
    // Dungeon bosses keep closing in: Calleil (21084, 15 m) follows his victim through the
    // north gate and Musperosa round her room.
    let boss = npc::template(c.server, &c.npc).is_some_and(|t| t.npc_grade_id == BOSS_GRADE);
    if let Some(skill) = ai_skills::new_skill(c, template_scalars(c).base_skill_id as u32)
        .filter(|skill| !boss && ranged_base_attack(&skill.template))
    {
        set_weapon_range(c, k, &skill);
    }
    c.st(k).enter = true;
}

/// `npcs.npc_grade_id` of the dungeon bosses (Hieronimus, Sorvell, Velarre, Calleil,
/// Musperosa, Viboros).
const BOSS_GRADE: u8 = 5;

/// A base attack whose range does not come from a weapon and reaches beyond melee range.
pub(super) fn ranged_base_attack(t: &crate::systems::skills::data::SkillTemplate) -> bool {
    t.id != 2 && t.weapon_slot_for_range_id <= 0 && t.max_range as f32 > MELEE_ATTACK_RANGE
}

pub(super) fn big_monster_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::BigMonsterRoaming));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::BigMonster { combat_skills } = &params.kind else {
        return;
    };
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    if can_strafe(c, k) && !is_using_skill(c) {
        let t = current_target(c);
        move_in_range(c, k, t.as_ref(), delta_ms);
    }
    if !can_use_skill(c, k) {
        return;
    }
    c.st(k).strafe_during_delay = false;
    let target = current_target(c);
    let target_dist = target
        .as_ref()
        .map_or(0.0, |t| npc::distance_to(c.server, &c.obj, t, false));
    // RequestAvailableSkills
    let hr = health_ratio(c);
    let available: Vec<super::params::BigMonsterCombatSkill> = combat_skills
        .iter()
        .filter(|s| s.health_range_min <= hr && hr <= s.health_range_max)
        .filter(|s| !ai_skills::on_cooldown(c, s.skill_type))
        .filter(|s| {
            c.server
                .game_data
                .systems
                .skills
                .skill(s.skill_type)
                .is_some_and(|tpl| {
                    !tpl.source_dead
                        && ((target_dist >= tpl.min_range as f32
                            && target_dist <= tpl.max_range as f32)
                            || ai_skills::is_self_target(tpl))
                })
        })
        .cloned()
        .collect();
    // PickSkill
    let t = template_scalars(c);
    // At half health the Illusionist splits once the authored cooldown permits it.
    let illusion = available
        .iter()
        .find(|skill| encounters::is_priority_combat_skill(skill.skill_type));
    let selected = if let Some(illusion) = illusion {
        Some(illusion.clone())
    } else if !available.is_empty() {
        Some(available[rand::thread_rng().gen_range(0..available.len())].clone())
    } else if !ai_skills::on_cooldown(c, t.base_skill_id as u32) {
        Some(super::params::BigMonsterCombatSkill {
            skill_type: t.base_skill_id as u32,
            skill_delay: t.base_skill_delay,
            strafe_during_delay: t.base_skill_strafe,
            ..Default::default()
        })
    } else {
        None
    };
    let Some(selected) = selected else {
        // If skill list is empty, get Base skill
        if let Some(target) = target {
            // The fallback pick sets the weapon range from whatever template skill it chose.
            // A big monster's chase range must not follow it: Viboros kept the 30 m of a
            // ranged pick and stood 15 m from the party for the rest of the fight.
            let (min_range, max_range) = {
                let s = c.st(k);
                (s.min_weapon_range, s.max_weapon_range)
            };
            pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
            let s = c.st(k);
            s.min_weapon_range = min_range;
            s.max_weapon_range = max_range;
        }
        return;
    };
    let Some(skill) = ai_skills::new_skill(c, selected.skill_type) else {
        return;
    };
    use_skill(c, k, skill, target.as_ref(), selected.skill_delay);
    c.st(k).strafe_during_delay = selected.strafe_during_delay;
}
