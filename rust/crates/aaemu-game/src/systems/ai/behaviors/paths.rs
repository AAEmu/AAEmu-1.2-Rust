use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::world::now_ms;

use super::super::framework::{AiCtx, BehaviorKind};
use super::super::path_handler::{PathStep, TARGET_STALL_TIMEOUT_MS};
use super::super::skills as ai_skills;

use super::npc;
use super::perception::{check_aggression, check_alert};
use super::support::{
    base_move_speed, delta_seconds, npc_move_towards, owner_pos, template_ai_params,
    template_scalars,
};
use super::targeting::{set_target, update_target};

/// `Behavior.CheckFollowPath()`.
pub(super) fn check_follow_path(c: &AiCtx<'_>) -> bool {
    c.ai.path_handler.has_path_movement_data()
}

pub(super) fn follow_path_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    c.st(k).skill_queue.clear();
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    c.st(k).combat_start_time = now_ms();
    if !c.npc.data().unit.is_in_battle {
        super::npc_events::on_combat_started(c);
    }
    c.ai.param = template_ai_params(c);
    c.npc.data().unit.is_in_patrol = true;
    if !c.ai.has_simulation {
        // Ai.Owner.Simulation is null for NPCs not spawned by a spawner: NullReferenceException.
        tracing::error!(
            "FollowPathBehavior.Enter: npc {} has no Simulation (NullReferenceException in C#)",
            c.npc.obj_id
        );
        return;
    }
    // Simulation.MoveToPathEnabled = true; Simulation.GoToPath(owner, true): GoToPath toggles
    // MoveToPathEnabled back to false and runs StopMove (StopMovement + IsInPatrol = false).
    npc::stop_movement(c.server, &c.npc);
    c.npc.data().unit.is_in_patrol = false;
    // A target carried over from an interrupted run gets a fresh stall window.
    let pos = owner_pos(c);
    let include_z = c.npc.data().can_fly;
    c.ai.path_handler
        .reset_target_progress(pos, include_z, now_ms());
    c.st(k).enter = true;
}

pub(super) fn follow_path_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    // These paths lead into a boss spawn. Let the actor finish the route
    // and run the remaining commands before responding to nearby combat.
    if c.running_encounter_intro() {
        run_current_path(c, delta_ms);
        return;
    }
    if !update_target(c) {
        set_target(c, None);
    }
    if check_aggression(c, k) {
        return;
    }
    if check_alert(c, k) {
        return;
    }
    // If still aggro, go back to combat
    let (in_battle, has_aggro) = {
        let n = c.npc.data();
        (n.unit.is_in_battle, n.unit.skill_state.has_engaged_aggro())
    };
    if in_battle && has_aggro {
        c.go_to_combat();
        return;
    }
    if !run_current_path(c, delta_ms) {
        c.go_to_idle();
        return;
    }
    let h = &c.ai.path_handler;
    let t = h.target_position;
    if t.x == 0.0
        && t.y == 0.0
        && t.z == 0.0
        && h.ai_path_points.is_empty()
        && h.ai_path_points_remaining.is_empty()
        // A sentinel just sent us back to RunCommandSet (the C# then overrode that with
        // `GoToIdle`, stranding the rest of the command set).
        && c.ai.current_behavior() == Some(k)
    {
        c.go_to_idle();
    }
}

/// `AiPathHandler.RunCurrentPath(delta)`.
fn run_current_path(c: &mut AiCtx<'_>, delta_ms: i64) -> bool {
    let pos = owner_pos(c);
    let scale = template_scalars(c).scale;
    // Ground walkers follow the terrain; only flying NPCs move along the authored Z.
    let include_z = c.npc.data().can_fly;
    match c
        .ai
        .path_handler
        .run_current_path_at(pos, scale, include_z, now_ms())
    {
        PathStep::ReturnToCommandSet => {
            c.go_to_run_command_set();
            true
        }
        PathStep::Move {
            target,
            speed,
            actor_flags,
            result,
        } => {
            let pace = npc::real_movement_speed(&c.obj, f64::from(speed));
            let actor_flags = actor_flags.unwrap_or_else(|| npc::real_movement_flags(pace));
            let move_speed = pace * delta_seconds(delta_ms);
            // Stop within the same radius that counts as an arrival (`Template.Scale`, at least
            // the movement default 1 m); a smaller tolerance kept the NPC short of the point.
            npc_move_towards(c, target, move_speed as f32, actor_flags, scale.max(1.0));
            // Move the idle "home" location along with the path
            c.ai.idle_position = owner_pos(c);
            result
        }
        PathStep::Idle(r) => r,
        PathStep::Stalled {
            target,
            path,
            result,
        } => {
            tracing::warn!(
                "NPC {}: follow path '{}' target ({}, {}, {}) made no progress for {} ms, skipping it",
                c.npc.obj_id,
                if path.is_empty() { "?" } else { &path },
                target.x,
                target.y,
                target.z,
                TARGET_STALL_TIMEOUT_MS
            );
            result
        }
        PathStep::Unsupported(msg) => {
            tracing::error!("{msg} (NotSupportedException)");
            true
        }
    }
}

pub(super) fn follow_unit_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if !update_target(c) {
        set_target(c, None);
    }
    if check_aggression(c, k) {
        return;
    }
    if check_alert(c, k) {
        return;
    }
    let follow = c.ai.follow_unit.clone();
    let alive = follow.as_ref().is_some_and(|f| {
        f.with_unit(|u| u.unit().hp > 0 && !u.unit().is_dead())
            .unwrap_or(false)
    });
    let Some(follow) = follow.filter(|_| alive) else {
        c.ai.follow_unit = None;
        // A `FollowUnit` AI command's set ends with the follow (INFERRED, see PORTING.md).
        super::interaction::release(c);
        c.go_to_idle();
        return;
    };
    let target_distance = npc::distance_to(c.server, &c.obj, &follow, true);
    let follow_speed_multiplier = (f64::from(target_distance) / 1.5).min(5.0) as f32;
    let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)))
        * f64::from(follow_speed_multiplier);
    let move_flags = npc::real_movement_flags(move_speed);
    move_speed *= delta_seconds(delta_ms);
    let fp = npc::world_pos(c.server, &follow);
    npc_move_towards(c, fp, move_speed as f32, move_flags, 1.0);
    c.ai.idle_position = owner_pos(c);
}
