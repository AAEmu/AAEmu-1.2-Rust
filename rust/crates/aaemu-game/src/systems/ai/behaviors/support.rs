use std::sync::Arc;

use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::npcs::npc::packets::SCUnitModelPostureChangedPacket;
use crate::systems::skills::controllers::Vec3;
use crate::systems::skills::types::{SkillCastTarget, SkillCaster};
use crate::world::now_ms;

use super::super::combat;
use super::super::framework::{AiCtx, BehaviorKind};
use super::super::params::{AiParams, AiParamsKind};
use super::super::skills::{self as ai_skills, SkillUseConditionKind};

use super::npc::{self, MoveAi};
use super::perception::check_aggression;

use BehaviorKind as B;

// ===========================================================================================
// shared helpers (Behavior / BaseCombatBehavior)
// ===========================================================================================

pub(super) fn posture_packet(c: &AiCtx<'_>, activate: bool) -> SCUnitModelPostureChangedPacket {
    let n = c.npc.data();
    SCUnitModelPostureChangedPacket {
        obj_id: c.npc.obj_id,
        posture: crate::models::unit::UnitData::model_posture_type(&*n),
        has_loot_left: n.unit.has_loot_left,
        anim_action_id: n.anim_action_id,
        activate_animation: activate,
        turret_pitch: 0.0,
        turret_yaw: 0.0,
    }
}

/// `Ai.Owner.BroadcastPacket(new SCUnitModelPostureChangedPacket(Ai.Owner, AnimActionId, x), false)`.
pub(super) fn broadcast_posture(c: &AiCtx<'_>, activate: bool) {
    let p = posture_packet(c, activate);
    npc::broadcast_packet(c, &p);
}

/// `this.CheckInterval(interval)` at call site `site` of behavior instance `k`.
pub(super) fn behavior_check_interval(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    site: u8,
    interval_ms: i64,
) -> bool {
    let now = now_ms();
    let s = c.st(k);
    let next = s.intervals.get(&site).copied().unwrap_or(now);
    if next > now {
        return false;
    }
    s.intervals.insert(site, now + interval_ms);
    true
}

/// `Behavior.Throttle(interval = 0.1)`.
pub(super) fn throttle(c: &mut AiCtx<'_>, k: BehaviorKind) -> bool {
    let now = now_ms();
    let s = c.st(k);
    if let Some(last) = s.last_tick {
        if now - last < 100 {
            return false;
        }
    }
    s.last_tick = Some(now);
    true
}

pub(super) fn template_scalars(c: &AiCtx<'_>) -> TemplateScalars {
    match npc::template(c.server, &c.npc) {
        Some(t) => TemplateScalars {
            aggression: t.aggression,
            attack_start_range_scale: t.attack_start_range_scale,
            sight_range_scale: t.sight_range_scale,
            sight_fov_scale: t.sight_fov_scale,
            base_skill_id: t.base_skill_id,
            base_skill_delay: t.base_skill_delay,
            base_skill_strafe: t.base_skill_strafe,
            use_range_mod: t.use_range_mod,
            skills_count: t.skills.len(),
            return_distance: t.return_distance,
            absolute_return_distance: t.absolute_return_distance,
            npc_ai_param_id: t.npc_ai_param_id,
            npc_nickname_id: t.npc_nickname_id,
            scale: t.scale,
        },
        None => TemplateScalars::default(),
    }
}

/// The `NpcTemplate` values the behaviors read.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TemplateScalars {
    pub(super) aggression: bool,
    pub(super) attack_start_range_scale: f32,
    pub(super) sight_range_scale: f32,
    pub(super) sight_fov_scale: f32,
    pub(super) base_skill_id: i32,
    pub(super) base_skill_delay: f32,
    pub(super) base_skill_strafe: bool,
    pub(super) use_range_mod: bool,
    pub(super) skills_count: usize,
    pub(super) return_distance: f32,
    pub(super) absolute_return_distance: f32,
    npc_ai_param_id: i32,
    pub(super) npc_nickname_id: i32,
    pub(super) scale: f32,
}

/// `Ai.Owner.Template.AiParams`.
pub(super) fn template_ai_params(c: &AiCtx<'_>) -> Option<Arc<AiParams>> {
    let id = template_scalars(c).npc_ai_param_id;
    c.server.game_data.systems.ai.ai.ai_params_for_id(id as u32)
}

pub(crate) fn delta_seconds(delta_ms: i64) -> f64 {
    // `delta.Milliseconds / 1000.0`: the millisecond component of the TimeSpan.
    (delta_ms.rem_euclid(1000)) as f64 / 1000.0
}

fn move_ai(c: &AiCtx<'_>) -> Option<MoveAi> {
    Some(MoveAi {
        hold_or_idle: matches!(c.ai.current_behavior(), Some(B::HoldPosition | B::Idle)),
    })
}

/// `Ai.Owner.MoveTowards(target, distance, flags, rangeTolerance)`.
pub fn npc_move_towards(
    c: &AiCtx<'_>,
    target: Vec3,
    distance: f32,
    flags: u8,
    tolerance: f32,
) -> bool {
    npc::move_towards(
        c.server,
        &c.npc,
        move_ai(c),
        target,
        distance,
        flags,
        tolerance,
    )
}

/// `Ai.Owner.BaseMoveSpeed`.
pub(crate) fn base_move_speed(c: &AiCtx<'_>) -> f32 {
    npc::base_move_speed(
        c.server,
        &c.npc,
        c.ai.current_behavior() == Some(B::ReturnState),
    )
}

pub(crate) fn owner_pos(c: &AiCtx<'_>) -> Vec3 {
    npc::world_pos(c.server, &c.obj)
}

pub(super) fn dead_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::stop_movement(c.server, &c.npc);
    combat::npc_clear_all_aggro(c);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    // The set is over (release); a window still open on the dying NPC is closed, as for a
    // despawn (INFERRED, see PORTING.md WP-5b).
    super::interaction::release(c);
    super::interaction::end_open_interactions(c, c.npc.obj_id);
    super::npc_events::on_death(c);
    c.st(k).enter = true;
}

pub(super) fn despawning_enter(c: &mut AiCtx<'_>) {
    super::interaction::release(c);
    super::interaction::end_open_interactions(c, c.npc.obj_id);
    super::npc_events::on_despawn(c);
}

pub(super) fn spawning_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    super::npc_events::on_spawn(c);
    if let Some(p) = template_ai_params(c) {
        if let AiParamsKind::Almighty { .. } = p.kind {
            if p.alert_to_attack && p.alert_duration == 0.0 {
                check_aggression(c, k);
            }
        }
    }
    c.st(k).enter = true;
}

pub(super) fn spawning_tick(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if !c.st(k).enter {
        return;
    }
    if !c.st(k).used_spawn_skills {
        let skills = ai_skills::template_skills_if_any(c, SkillUseConditionKind::OnSpawn);
        if let Some(skills) = skills {
            c.st(k).used_spawn_skills = true;
            for id in skills {
                let Some(skill) = ai_skills::new_skill(c, id) else {
                    tracing::error!("Spawning: OnSpawn skill {id} has no template");
                    continue;
                };
                let mut t = SkillCastTarget::Unit(c.npc.obj_id);
                ai_skills::use_skill(c, &skill, SkillCaster::Unit(c.npc.obj_id), &mut t, true);
            }
        }
    }
    // This follows the game's way of doing it.
    c.go_to_run_command_set();
}
