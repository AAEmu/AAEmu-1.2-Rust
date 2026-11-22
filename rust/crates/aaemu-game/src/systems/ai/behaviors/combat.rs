use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::instances::encounters::hooks as encounter_hooks;
use crate::systems::skills::controllers::Vec3;
use crate::world::now_ms;

use super::super::combat::set_in_battle;
use super::super::framework::{AiCtx, BehaviorKind, NpcIntervalSite};
use super::super::params::AiParamsKind;
use super::super::path_handler::{AiPathPoint, AiPathPointAction};
use super::super::skills::{self as ai_skills, SkillUseConditionKind};

use super::chase::{check_pipe_name, move_in_range};
use super::npc;
use super::return_state::should_return;
use super::skill_use::{
    can_strafe, can_use_skill, is_using_skill, pick_skill_and_use_it, refresh_skill_queue,
    use_skill,
};
use super::support::{broadcast_posture, template_ai_params};
use super::targeting::{current_target, update_target};

// ===========================================================================================
// Common behaviors
// ===========================================================================================

pub(super) fn almighty_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    c.st(k).skill_queue.clear();
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    c.st(k).combat_start_time = now_ms();
    if !c.npc.data().unit.is_in_battle {
        super::npc_events::on_combat_started(c);
    }
    c.ai.param = template_ai_params(c);
    c.st(k).enter = true;
}

pub(super) fn almighty_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::Almighty { ai_skill_lists, .. } = &params.kind else {
        return;
    };
    c.st(k).ai_params = Some(params.clone());
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    if can_strafe(c, k) && !is_using_skill(c) {
        let t = current_target(c);
        move_in_range(c, k, t.as_ref(), delta_ms);
    }
    check_pipe_name(c, k);
    if !can_use_skill(c, k) {
        return;
    }
    c.st(k).strafe_during_delay = false;
    // Will delay for 150 Milliseconds to eliminate the hanging of the skill
    if !c.npc_check_interval(NpcIntervalSite::AlmightyAttackBehavior, 150) {
        tracing::trace!("Skill: CooldownTime [150]!");
        return;
    }
    if c.st(k).skill_queue.is_empty() && !refresh_skill_queue(c, k, ai_skill_lists, &params) {
        return;
    }
    let Some(selected) = c.st(k).skill_queue.pop_front() else {
        return;
    };
    let Some(skill) = ai_skills::new_skill(c, selected.skill_id) else {
        return;
    };
    let target = current_target(c);
    use_skill(c, k, skill, target.as_ref(), selected.delay);
    c.st(k).strafe_during_delay = selected.strafe;
}

pub(super) fn almighty_attack_exit(c: &mut AiCtx<'_>, k: BehaviorKind) {
    // Experimental handling of guards returning to their home position after chasing somebody
    let aggro_empty = c.npc.data().unit.skill_state.aggro_table.is_empty();
    if aggro_empty && c.ai.path_handler.ai_path_points_remaining.is_empty() {
        c.ai.path_handler.target_position = Vec3::default();
        npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
        npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
        let home = c.ai.home_position;
        c.enqueue_path_point(AiPathPoint {
            action: AiPathPointAction::Speed,
            param: "3".into(),
            position: home,
        });
        c.go_to_follow_path();
    }
    c.st(k).enter = false;
}

pub(super) fn attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    set_in_battle(c.server, &c.obj, true);
    super::npc_events::on_combat_started(c);
    c.st(k).enter = true;
}

pub(super) fn attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    let Some(target) = current_target(c) else {
        return;
    };
    if encounter_hooks::for_zone(c.obj.transform().zone_id).combat_step(c, k, delta_ms) {
        return;
    }
    if can_strafe(c, k) && !is_using_skill(c) {
        move_in_range(c, k, Some(&target), delta_ms);
    }
    if !can_use_skill(c, k) {
        return;
    }
    // Will delay for 150 Milliseconds to eliminate the hanging of the skill
    if !c.npc_check_interval(NpcIntervalSite::AttackBehavior, 150) {
        tracing::trace!("Skill: CooldownTime [150]!");
        return;
    }
    let target = current_target(c).unwrap_or(target);
    let target_dist = npc::distance_to(c.server, &c.obj, &target, false);
    pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
}
