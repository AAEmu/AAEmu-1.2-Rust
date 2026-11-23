use crate::systems::instances::encounters::registry as encounters;
use crate::systems::skills::controllers::Vec3;
use crate::systems::skills::types::skill_result;
use crate::world::{now_ms, WorldObject};

use super::super::framework::AiCtx;
use super::super::path_handler::{AiPathPoint, AiPathPointAction};

use super::npc;
use super::targeting::{current_target, update_target};

pub(super) fn run_command_set_tick(c: &mut AiCtx<'_>, delta_ms: i64) {
    // If everything is executed, and still time remaining, wait for it before going back
    if c.ai.current_command_run_time_ms > 0 {
        c.ai.current_command_run_time_ms -= delta_ms;
        if c.ai.current_command_run_time_ms == 0 {
            // A wait which expires exactly on a tick is complete too.
            c.ai.current_command_run_time_ms = -1;
        }
        return;
    }
    // If there are commands in the AI Command queue, execute those first
    if c.ai.current_command.is_some() || !c.ai.commands_queue.is_empty() {
        if c.ai.current_command.is_none() {
            c.ai.current_command = c.ai.commands_queue.pop_front();
            c.ai.current_command_start_time = now_ms();
        }
        tick_current_ai_command(c, delta_ms);
        return;
    }
    super::interaction::release(c);
    // A summon can gain aggro during Spawning, before an aggro change has a
    // transition. Honor it here before running the idle reset skills.
    let has_aggro = c.npc.data().unit.skill_state.has_engaged_aggro();
    if has_aggro && update_target(c) {
        c.go_to_combat();
    } else {
        c.go_to_idle();
    }
}

/// `FollowUnit`'s search radius for the fallback target (INFERRED from
/// `DoFollowNearestNpc(..., 100f)`).
const FOLLOW_UNIT_RANGE: f32 = 100.0;

/// The nearest player within `range` of the NPC (`DoFollowNearestNpc`'s search, filtered to
/// players).
fn nearest_player(c: &AiCtx<'_>, range: f32) -> Option<WorldObject> {
    let mut nearest: Option<(f32, WorldObject)> = None;
    for o in npc::around(c.server, &c.obj, range, true) {
        if o.as_player().is_none() {
            continue;
        }
        let dist = npc::distance_to(c.server, &c.obj, &o, false);
        if nearest.as_ref().is_none_or(|(closest, _)| dist < *closest) {
            nearest = Some((dist, o));
        }
    }
    nearest.map(|(_, o)| o)
}

/// `RunCommandSetBehavior.TickCurrentAiCommand(aiCommand, delta)`.
fn tick_current_ai_command(c: &mut AiCtx<'_>, delta_ms: i64) {
    use super::params::AiCommandCategory as Cmd;
    if c.ai.current_command_run_time_ms < 0 {
        c.ai.current_command = None;
        c.ai.current_command_run_time_ms = 0;
        return;
    }
    // Check if we're still waiting
    if c.ai.current_command_run_time_ms > 0 {
        c.ai.current_command_run_time_ms -= delta_ms;
        return;
    }
    let Some(cmd) = c.ai.current_command.clone() else {
        return;
    };
    tracing::debug!(
        "{} executing AI Command: {:?}, CommandSet: {}, P1: {}, P2: {}",
        c.npc.obj_id,
        cmd.cmd_id,
        cmd.cmd_set_id,
        cmd.param1,
        cmd.param2
    );
    // WP-5b: `ai_command_sets.can_interact` (INFERRED rule, see PORTING.md).
    super::interaction::apply_command_set(c, cmd.cmd_set_id);
    match cmd.cmd_id {
        Cmd::FollowUnit => {
            // `ai.follow_unit` (set by the quest bridge or an earlier command) wins; otherwise
            // follow the nearest player within 100 m, like `DoFollowNearestNpc`. `param1` is 1
            // in all three shipped rows and its meaning is UNKNOWN: log it and ignore it.
            tracing::debug!(
                "NPC {}: FollowUnit param1 {} is UNKNOWN and ignored",
                c.npc.obj_id,
                cmd.param1
            );
            if c.ai.follow_unit.is_none() {
                c.ai.follow_unit = nearest_player(c, FOLLOW_UNIT_RANGE);
            }
            if c.ai.follow_unit.is_some() {
                c.go_to_follow_unit();
            }
        }
        Cmd::FollowPath => {
            // The command names its own route. The C# loaded `AiFileName` (the *previous*
            // command's name) before assigning `Param2`, so the first FollowPath of a set
            // loaded nothing, every later one loaded the previous route and the last route
            // was never loaded at all.
            let name = if cmd.param2.trim().is_empty() {
                c.ai.ai_file_name.clone()
            } else {
                cmd.param2.clone()
            };
            c.load_ai_path_points(&name, cmd.param1 == 1);
            if cmd.param1 == 1 {
                c.enqueue_path_point(AiPathPoint {
                    position: Vec3::default(),
                    action: AiPathPointAction::ReturnToCommandSet,
                    param: String::new(),
                });
            }
            c.go_to_follow_path();
            if let Some(flags) = encounters::route_runs_with_flags(&name) {
                c.ai.path_handler.ai_path_speed = npc::running_move_speed(c.server, &c.npc);
                c.ai.path_handler.ai_path_actor_flags = Some(flags);
            } else if encounters::walks_scene_routes(cmd.cmd_set_id) {
                // The route files carry no `Speed` point, so the C# default of 1 m/s applied:
                // well below the actor's own walk (1.8 m/s for the human model 18), which
                // the client showed as a slow, sliding walk. A `Speed` point still overrides.
                c.ai.path_handler.ai_path_speed = npc::walking_move_speed(c.server, &c.npc);
            }
            if cmd.param1 == 1 {
                // An empty Param2 walked the current route (see above): keep its name.
                if !cmd.param2.trim().is_empty() {
                    c.ai.ai_file_name = cmd.param2.clone();
                }
            } else {
                c.ai.ai_file_name2 = cmd.param2.clone();
            }
        }
        Cmd::UseSkill => {
            c.ai.ai_skill_id = cmd.param1;
            let skill_id = c.ai.ai_skill_id;
            if let Some(tpl) = c.server.game_data.systems.skills.skill(skill_id).cloned() {
                let target = current_target(c)
                    .filter(WorldObject::is_unit)
                    .unwrap_or_else(|| c.obj.clone());
                let r = crate::systems::skills::skill::unit_use_skill(
                    c.server, &c.obj, skill_id, &target,
                );
                if r == skill_result::SUCCESS {
                    // The AI casts through `unit_use_skill`, whose NPC skills run at the
                    // unit level (`ab_level_override` / level): pace the command with the
                    // same effective level the cast uses.
                    let level =
                        crate::systems::skills::skill::ab_level(c.server, &c.obj, tpl.ability_id);
                    let cool_down = crate::systems::skills::skill::attack_delay(
                        c.server, &tpl, level, &c.obj, false, 0.0,
                    );
                    c.ai.current_command_run_time_ms = cool_down as i64;
                }
            }
        }
        Cmd::Timeout => {
            // The authored pauses are seconds (the one 1500 row is milliseconds, see
            // `params::timeout_ms`); the C# read every value as milliseconds, so a scripted
            // scene ran its lines back to back.
            c.ai.ai_time_out = cmd.param1;
            c.ai.current_command_run_time_ms = cmd.timeout_ms();
        }
        Cmd::Other(v) => {
            tracing::error!("AI command {v}: NotSupportedException");
            return;
        }
    }
    if c.ai.current_command_run_time_ms == 0 {
        c.ai.current_command_run_time_ms = -1000;
    }
}
