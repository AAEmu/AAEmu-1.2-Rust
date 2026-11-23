use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::npcs::npc::packets::SCAggroTargetChangedPacket;
use crate::world::now_ms;

use super::super::framework::{AiCtx, BehaviorKind};
use super::super::skills::{self as ai_skills, SkillUseConditionKind};

use super::npc;
use super::paths::check_follow_path;
use super::perception::{check_aggression, check_alert};
use super::skill_use::pick_skill_and_use_it;
use super::support::throttle;
use super::talk::check_for_talk;
use super::targeting::{current_target, set_target};

/// Settling abandons the AI's previous skill choice. Authored events can still be running their
/// timed plots after the NPC enters its default behavior.
fn interrupt_unless_event_controller(c: &AiCtx<'_>) {
    if !crate::systems::instances::tower_def::owns_npc(c.server, &c.npc) {
        crate::systems::skills::skill::interrupt_ai_skills(c.server, &c.obj);
    }
}

pub(super) fn hold_position_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    interrupt_unless_event_controller(c);
    npc::stop_movement(c.server, &c.npc);
    // Ai.Owner.CurrentTarget = Ai.Owner (no packet)
    c.npc.data().unit.current_target = c.npc.obj_id;
    let s = c.st(k);
    s.last_skill_check = now_ms();
    s.following_npc = false;
    s.initialized = true;
}

pub(super) fn hold_position_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).initialized {
        tracing::warn!(
            "HoldPositionBehavior.Tick called before initialization for unit {}",
            c.npc.obj_id
        );
        return;
    }
    if !throttle(c, k) {
        return;
    }
    // 1. Combat states
    if check_aggression(c, k) {
        c.st(k).following_npc = false;
        return;
    }
    if check_alert(c, k) {
        c.st(k).following_npc = false;
        return;
    }
    // 2. A group follower's `formation_GoToOnPoint` movement replaces the hold-position walk;
    // its idle activity (self-skills, follow-default, talk) still runs below (1.2
    // `hold_position.UpdateHoldPosition`).
    super::group::follower_tick(c, delta_ms);
    // 3. Try to use idle self-skills (1 s between tries)
    let now = now_ms();
    if now - c.st(k).last_skill_check >= 1000 {
        c.st(k).last_skill_check = now;
        if let Some(t) = current_target(c) {
            let dist = npc::distance_to(c.server, &c.obj, &t, false);
            let me = c.obj.clone();
            pick_skill_and_use_it(c, k, SkillUseConditionKind::InIdle, &me, dist);
        }
    }
    // 4. Optional follow default NPC
    if !c.st(k).following_npc && c.do_follow_default_nearest_npc() {
        c.st(k).following_npc = true;
    }
    // 5. Talk has low priority
    check_for_talk(c, k);
}

pub(super) fn idle_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    // Reset the AI for idle activity.
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    interrupt_unless_event_controller(c);
    npc::stop_movement(c.server, &c.npc);
    set_target(c, None);
    if let Some(t) = current_target(c) {
        if let Some(p) = t.as_player() {
            p.send(SCAggroTargetChangedPacket {
                npc_id: c.npc.obj_id,
                target_id: 0,
            });
        }
    }
    super::npc_events::in_idle(c);
    c.st(k).initialized = true;
}

pub(super) fn idle_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).initialized {
        tracing::warn!(
            "IdleBehavior.Tick called before initialization for unit {}",
            c.npc.obj_id
        );
        return;
    }
    if !throttle(c, k) {
        return;
    }
    // Try to use a skill if possible
    if let Some(t) = current_target(c) {
        let dist = npc::distance_to(c.server, &c.obj, &t, false);
        let me = c.obj.clone();
        pick_skill_and_use_it(c, k, SkillUseConditionKind::InIdle, &me, dist);
    }
    // Check for state transitions
    if check_aggression(c, k) {
        c.go_to_combat();
    } else if check_alert(c, k) {
        c.go_to_alert();
    } else if check_follow_path(c) {
        c.go_to_follow_path();
    } else {
        // A group follower's movement is the formation follow (1.2 `idle.UpdateIdle`); its idle
        // activity still runs.
        super::group::follower_tick(c, delta_ms);
        // Try to follow the nearest NPC, otherwise go to default behavior
        if c.do_follow_default_nearest_npc() {
            return;
        }
        c.go_to_default_behavior();
    }
    // Talk has low priority
    check_for_talk(c, k);
}
