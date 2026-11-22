use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::skills::controllers::Vec3;

use super::super::combat::{clear_aggro_of_unit, set_in_battle};
use super::super::framework::{AiCtx, BehaviorKind};
use super::super::params::{AiParamType, AiParams, AiParamsKind};
use super::super::skills::{self as ai_skills, SkillUseConditionKind};

use super::chase::{find_path, geo_data_mode};
use super::help::update_aggro_help;
use super::npc;
use super::skill_use::pick_skill_and_use_it;
use super::support::{broadcast_posture, owner_pos, template_ai_params};
use super::targeting::{current_target, set_target, sorted_abusers};

// ===========================================================================================
// Flytrap
// ===========================================================================================

pub(super) fn flytrap_alert_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::ALERT);
    broadcast_posture(c, false);
    super::npc_events::in_alert(c);
    c.st(k).enter = true;
}

pub(super) fn flytrap_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    super::npc_events::on_combat_started(c);
    c.ai.param = template_ai_params(c);
    c.st(k).enter = true;
}

pub(super) fn flytrap_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::Flytrap));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    if !matches!(params.kind, AiParamsKind::Flytrap { .. }) {
        return;
    }
    c.st(k).ai_params = Some(params.clone());
    if !flytrap_update_target(c) {
        c.on_no_aggro_target();
        return;
    }
    let Some(target) = current_target(c) else {
        return;
    };
    // if (Ai.Owner.Gimmick?.CurrentTarget != null) MoveInRange(Ai.Owner.Gimmick.CurrentTarget, delta)
    if let Some(g) = super::gimmick::npc_gimmick(c.server, &c.obj) {
        let gt = g.gimmick.data().unit.current_target;
        if gt != 0 {
            if let Some(t) = c.server.world.object(gt) {
                super::gimmick::flytrap_move_in_range(c, &g, &t, delta_ms);
            }
        }
    }
    set_in_battle(c.server, &c.obj, true);
    let target_dist = npc::distance_to(c.server, &c.obj, &target, false);
    pick_skill_and_use_it(c, k, SkillUseConditionKind::InCombat, &target, target_dist);
    flytrap_update(c, k);
}

/// `FlytrapAttackBehavior.UpdateTarget()`.
fn flytrap_update_target(c: &mut AiCtx<'_>) -> bool {
    for abuser in sorted_abusers(c).into_iter().flatten() {
        let ap = npc::world_pos(c.server, &abuser);
        npc::look_towards(c.server, &c.npc, ap, 4);
        if c.ai.already_targeted {
            return true;
        }
        if npc::unit_is_visible(c.server, &c.obj, &abuser) && !npc::is_dead(&abuser) {
            npc::set_current_aggro_target(&c.npc, Some(&abuser));
            set_target(c, Some(&abuser));
            update_aggro_help(c, &abuser, 200.0);
            if geo_data_mode(c) {
                find_path(c, &abuser, "new target");
            }
            return true;
        }
        clear_aggro_of_unit(c.server, &c.obj, &abuser);
    }
    match current_target(c) {
        Some(t) if t.is_unit() => {
            let hp = t.with_unit(|u| u.unit().hp).unwrap_or(0);
            if hp <= 0 || npc::is_dead(&t) {
                set_target(c, None);
            }
        }
        _ => set_target(c, None),
    }
    false
}

/// `FlytrapAttackBehavior.Update()`.
fn flytrap_update(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let Some(abuser) = current_target(c) else {
        return;
    };
    let Some(params) = c.st(k).ai_params.clone() else {
        return;
    };
    let AiParamsKind::Flytrap {
        attack_end_distance,
        ..
    } = params.kind
    else {
        return;
    };
    let abuser_pos = npc::world_pos(c.server, &abuser);
    let current = owner_pos(c);
    let idle = c.ai.idle_position;
    let dist_sq = |a: Vec3, b: Vec3| {
        let dx = f64::from(a.x) - f64::from(b.x);
        let dy = f64::from(a.y) - f64::from(b.y);
        (dx * dx + dy * dy) as f32
    };
    // Check out of idle pos
    let teleport =
        c.ai.param
            .as_ref()
            .is_some_and(|p| p.always_teleport_on_return);
    if teleport && dist_sq(current, idle) > 3.0 * 3.0 {
        clear_aggro_of_unit(c.server, &c.obj, &abuser);
        c.on_no_aggro_target();
        return;
    }
    // Check that some target was gone out from attack end distance
    if dist_sq(abuser_pos, idle) > attack_end_distance * attack_end_distance {
        clear_aggro_of_unit(c.server, &c.obj, &abuser);
        c.on_no_aggro_target();
    }
}
