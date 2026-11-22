use rand::Rng;

use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::skills::controllers::{calculate_distance, Vec3};

use super::super::combat::set_in_battle;
use super::super::framework::{AiCtx, ArcherPhase, BehaviorKind};
use super::super::params::{AiParamType, AiParams, AiParamsKind};
use super::super::skills as ai_skills;

use super::chase::move_in_range;
use super::npc;
use super::return_state::should_return;
use super::skill_use::{can_strafe, can_use_skill, is_using_skill, use_skill};
use super::support::{
    base_move_speed, broadcast_posture, delta_seconds, npc_move_towards, owner_pos,
    template_ai_params, template_scalars,
};
use super::targeting::{current_target, update_target};

// ===========================================================================================
// Archer
// ===========================================================================================

pub(super) fn archer_attack_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    {
        let s = c.st(k);
        s.archer_phase = ArcherPhase::Base;
        s.make_a_gap_count = 0;
    }
    ai_skills::interrupt_skills(c);
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::COMBAT);
    broadcast_posture(c, false);
    set_in_battle(c.server, &c.obj, true);
    super::npc_events::on_combat_started(c);
    c.ai.param = template_ai_params(c);
    c.st(k).enter = true;
}

pub(super) fn archer_attack_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if c.ai.param.is_none() {
        c.ai.param = Some(AiParams::empty(AiParamType::ArcherRoaming));
    }
    let Some(params) = c.ai.param.clone() else {
        return;
    };
    let AiParamsKind::Archer { combat_skills } = &params.kind else {
        return;
    };
    if !update_target(c) || should_return(c) {
        c.on_no_aggro_target();
        return;
    }
    if c.st(k).archer_phase == ArcherPhase::NeedMakeAGap {
        let idle = c.ai.idle_position;
        let me = owner_pos(c);
        let abuser = current_target(c).map_or(Vec3::default(), |t| npc::world_pos(c.server, &t));
        let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
        let move_flags = npc::real_movement_flags(move_speed);
        move_speed *= delta_seconds(delta_ms);
        npc_move_towards(c, idle, move_speed as f32, move_flags, 1.0);
        let dist = calculate_distance(me, idle, true);
        let dist2 = calculate_distance(me, abuser, true);
        if dist < 1.0 || dist2 > params.prefered_combat_dist {
            npc::stop_movement(c.server, &c.npc);
            let s = c.st(k);
            s.make_a_gap_count += 1;
            s.archer_phase = ArcherPhase::TryingRangedDefSkill;
        } else {
            return;
        }
    } else if can_strafe(c, k) && !is_using_skill(c) {
        let t = current_target(c);
        move_in_range(c, k, t.as_ref(), delta_ms);
    }
    if !can_use_skill(c, k) {
        return;
    }
    c.st(k).max_weapon_range = params.prefered_combat_dist;
    let target_dist = match current_target(c) {
        Some(t) => npc::distance_to(c.server, &c.obj, &t, false),
        None => 0.0,
    };
    let list = archer_request_available_skills(c, k, &params, combat_skills, target_dist);
    let selected = archer_pick_skill(c, &list);
    let Some(skill) = ai_skills::new_skill(c, selected) else {
        return;
    };
    let casting = skill.template.casting_time as f32 / 1000.0;
    let target = current_target(c);
    use_skill(c, k, skill, target.as_ref(), casting);
}

/// `ArcherAttackBehavior.OnUseSkillDone()`.
fn archer_on_use_skill_done(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let s = c.st(k);
    s.archer_phase = match s.archer_phase {
        ArcherPhase::TryingMeleeSkill => ArcherPhase::UsedMeleeSkill,
        ArcherPhase::TryingRangedDefSkill => ArcherPhase::UsedRangedDefSkill,
        ArcherPhase::TryingMakeAGapSkill | ArcherPhase::NeedMakeAGap => ArcherPhase::NeedMakeAGap,
        _ => ArcherPhase::Base,
    };
}

/// `ArcherAttackBehavior.RequestAvailableSkills(aiParams, trgDist)` (OnRequestSkillInfo).
fn archer_request_available_skills(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    params: &AiParams,
    base: &[super::params::ArcherCombatSkill],
    trg_dist: f32,
) -> Vec<u32> {
    let in_melee = trg_dist <= params.melee_attack_range;
    let mut list = Vec::new();
    let collect =
        |c: &mut AiCtx<'_>, f: fn(&super::params::ArcherCombatSkill) -> &Vec<u32>| -> Vec<u32> {
            let mut out = Vec::new();
            for acs in base {
                for id in f(acs) {
                    if !ai_skills::on_cooldown(c, *id) {
                        out.push(*id);
                    }
                }
            }
            out
        };
    match c.st(k).archer_phase {
        ArcherPhase::UsedMeleeSkill => {
            let need_make_a_gap =
                in_melee && c.st(k).make_a_gap_count < params.max_make_a_gap_count;
            if need_make_a_gap {
                list = collect(c, |a| &a.make_a_gap);
                c.st(k).archer_phase = ArcherPhase::TryingMakeAGapSkill;
            } else {
                c.st(k).archer_phase = ArcherPhase::Base;
                // call again with phase change ("base")
                return archer_request_available_skills(c, k, params, base, trg_dist);
            }
        }
        ArcherPhase::UsedRangedDefSkill => list = collect(c, |a| &a.ranged_strong),
        _ => {}
    }
    if list.is_empty() {
        if in_melee {
            list = collect(c, |a| &a.melee);
            c.st(k).archer_phase = ArcherPhase::TryingMeleeSkill;
        } else {
            list = collect(c, |a| &a.ranged_def);
            c.st(k).archer_phase = ArcherPhase::TryingRangedDefSkill;
        }
    }
    archer_on_use_skill_done(c, k);
    list
}

/// `ArcherAttackBehavior.PickSkill(skills)`.
fn archer_pick_skill(c: &mut AiCtx<'_>, skills: &[u32]) -> u32 {
    if !skills.is_empty() {
        return skills[rand::thread_rng().gen_range(0..skills.len())];
    }
    let base = template_scalars(c).base_skill_id as u32;
    if !ai_skills::on_cooldown(c, base) {
        return base;
    }
    0 // no melee Skill
}
