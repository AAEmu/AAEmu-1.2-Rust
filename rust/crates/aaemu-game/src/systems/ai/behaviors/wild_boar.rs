use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::skills::types::skill_result;

use super::super::combat::set_in_battle;
use super::super::framework::{AiCtx, BehaviorKind};
use super::super::params::{AiParamType, AiParams, AiParamsKind};
use super::super::skills::{self as ai_skills, SkillUseConditionKind};

use super::chase::move_in_range;
use super::npc;
use super::return_state::should_return;
use super::skill_use::{
    can_strafe, can_use_skill, is_using_skill, pick_skill_and_use_it, set_weapon_range, use_skill,
};
use super::support::{broadcast_posture, template_ai_params};
use super::targeting::{current_target, update_target};

// ===========================================================================================
// WildBoar
// ===========================================================================================

pub(super) fn wild_boar_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    c.ai.param = template_ai_params(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    super::npc_events::on_combat_started(c);
    set_in_battle(c.server, &c.obj, true);
    c.st(k).enter = true;
}

pub(super) fn wild_boar_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::WildBoarRoaming));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::WildBoar {
        on_combat_start_skills,
        on_spurt_skills,
    } = &params.kind
    else {
        return;
    };
    {
        let (hp, max) = {
            let n = c.npc.data();
            (n.unit.hp, n.unit.max_hp)
        };
        c.st(k).curr_health = hp as f32 / max as f32 * 100.0;
    }
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
    if !c.st(k).combat_start_skill {
        // On Combat Start Skill. Execute once
        let start = on_combat_start_skills.first().copied().unwrap_or(0);
        if start != 0 {
            npc::stop_movement(c.server, &c.npc);
            if let Some(skill) = ai_skills::new_skill(c, start) {
                let target = current_target(c);
                use_skill(c, k, skill, target.as_ref(), 0.0);
            }
            c.st(k).combat_start_skill = true;
        }
    }
    // Spurt or base?
    let Some(target) = current_target(c) else {
        return;
    };
    let target_dist = npc::distance_to(c.server, &c.obj, &target, false);
    if on_spurt_skills.is_empty() {
        pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
        return;
    }
    for skill_data in on_spurt_skills {
        if c.st(k).curr_health < skill_data.health_condition as f32 {
            let Some(skill) = ai_skills::new_skill(c, skill_data.skill_type) else {
                continue;
            };
            if target_dist >= skill.template.min_range as f32
                && target_dist <= skill.template.max_range as f32
            {
                set_weapon_range(c, k, &skill);
                let result = use_skill(c, k, skill, Some(&target), 0.0);
                if result == skill_result::COOLDOWN_TIME {
                    pick_skill_and_use_it(
                        c,
                        k,
                        SkillUseConditionKind::InCombat,
                        &target,
                        target_dist,
                    );
                }
            }
        } else {
            pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
        }
    }
}
