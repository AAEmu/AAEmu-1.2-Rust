use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::packets::world::SCUnitPointsPacket;
use crate::systems::instances::encounters::hooks as encounter_hooks;
use crate::systems::skills::controllers::calculate_distance;
use crate::systems::skills::types::KillReason;
use crate::systems::skills::{buffs, combat as skills_combat};
use crate::world::now_ms;

use super::super::combat::{self, set_in_battle};
use super::super::framework::{AiCtx, BehaviorKind};

use super::chase::{geo_data_mode, moves_freely, reset_chase_path};
use super::npc;
use super::roaming::wall_ahead;
use super::support::{
    base_move_speed, broadcast_posture, delta_seconds, npc_move_towards, owner_pos,
    template_scalars,
};
use super::targeting::{current_target, set_target};

/// `BaseCombatBehavior.ShouldReturn`.
///
/// Deviation: the C# returned only while the NPC was within `absolute_return_distance` of its
/// idle position ("if it's greater, then we need a teleport to the spawn point", never written),
/// so a mob pulled past it never returned and could be dragged anywhere (the Sealstone Guardian
/// 8945 has return 50 m = absolute 50 m: no leash at all). Past `return_distance` it now always
/// returns; [`return_state_enter`] teleports it home when it is past the absolute distance.
pub(super) fn should_return(c: &AiCtx<'_>) -> bool {
    // Never trigger return while a SkillController is actively driving the NPC.
    if npc::skill_controller_running(&c.obj) {
        return false;
    }
    let (return_distance, _) = return_distances(c);
    let Some(target) = current_target(c) else {
        return true; // no target, returning
    };
    let me = owner_pos(c);
    let target_position = npc::world_pos(c.server, &target);
    if let Some(leash) =
        encounter_hooks::for_zone(c.obj.transform().zone_id).leash(c, me, target_position)
    {
        return leash;
    }
    let distance_to_target = calculate_distance(me, target_position, true);
    let distance_to_idle = calculate_distance(me, c.ai.idle_position, true);
    distance_to_target > return_distance || distance_to_idle > return_distance
}

/// The template's `(return_distance, absolute_return_distance)`, defaulting to 50 m / 200 m.
fn return_distances(c: &AiCtx<'_>) -> (f32, f32) {
    let t = template_scalars(c);
    let positive_or = |v: f32, default: f32| if v > 0.0 { v } else { default };
    (
        positive_or(t.return_distance, 50.0),
        positive_or(t.absolute_return_distance, 200.0),
    )
}

pub(super) fn return_state_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    // Ending a fight (the abuser died, left or was cleared) must let a later fight against the
    // same object share with the group again.
    c.npc.data().npc_group_last_share_abuser = 0;
    // A group follower returns to its formation point, not its own spawn (1.2
    // `idle` / `hold_position` re-anchor a member's idle position on the leader).
    super::group::sync_return_to_formation(c);
    if !c.npc.data().unit.skill_state.aggro_table.is_empty() {
        combat::npc_clear_all_aggro(c);
    }
    set_target(c, None);
    set_in_battle(c.server, &c.obj, false);
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    broadcast_posture(c, false);
    buffs::add_buff_by_id(c.server, &c.obj, npc::BUFF_NPC_RETURN, &c.obj, 0);
    let restore =
        c.ai.param
            .as_ref()
            .is_none_or(|p| p.restoration_on_return());
    if restore {
        let (hp, max_hp, max_mp) = {
            let n = c.npc.data();
            (n.unit.hp, n.unit.max_hp, n.unit.max_mp)
        };
        skills_combat::post_update_current_hp(
            c.server,
            &c.obj,
            Some(&c.obj),
            hp,
            max_hp,
            KillReason::Unknown,
        );
        {
            let mut n = c.npc.data();
            n.unit.hp = max_hp;
            n.unit.mp = max_mp;
        }
        npc::broadcast_packet(
            c,
            &SCUnitPointsPacket {
                obj_id: c.npc.obj_id,
                hp: max_hp,
                mp: max_mp,
            },
        );
    }
    // Past `absolute_return_distance` the NPC is teleported to its idle position (the C#
    // `ShouldReturn` note "we need a teleport to the spawn point"; see `should_return`).
    let (_, absolute_return_distance) = return_distances(c);
    let beyond_absolute =
        calculate_distance(owner_pos(c), c.ai.idle_position, true) > absolute_return_distance;
    if beyond_absolute
        || c.ai
            .param
            .as_ref()
            .is_some_and(|p| p.always_teleport_on_return)
    {
        on_completed_return(c);
        return;
    }
    if c.ai.param.as_ref().is_some_and(|p| !p.go_return_state()) {
        on_completed_return_no_teleport(c);
    } else {
        // Not for an NPC that was just sent idle.
        plan_return_route(c);
    }
    let s = c.st(k);
    s.timeout_time = now_ms() + 20_000;
    s.enter = true;
}

/// The straight way home crosses a wall: walk the navmesh route instead (deviation from the
/// C#, see `PORTING.md`). Without a route (no collision service, `GeoDataMode` off, no
/// navmesh) the NPC walks straight and the 20 s timeout teleport remains the backstop.
fn plan_return_route(c: &mut AiCtx<'_>) {
    reset_chase_path(c);
    if !geo_data_mode(c) || moves_freely(c) {
        return;
    }
    let idle = c.ai.idle_position;
    if wall_ahead(c, idle).is_none() {
        return;
    }
    let steps = super::geodata::plan_route(c, idle);
    tracing::debug!(
        npc = c.npc.obj_id,
        steps = c.ai.path_node.found_path.len(),
        raw = steps,
        "return route planned around a wall"
    );
}

pub(super) fn return_state_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    // The leader may have moved since the fight ended: recompute the formation point from its
    // current position every tick instead of freezing the spot chosen at ReturnState entry.
    super::group::sync_return_to_formation(c);
    let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
    let move_flags = npc::real_movement_flags(move_speed);
    move_speed *= delta_seconds(delta_ms);
    let idle = c.ai.idle_position;
    // Corners of the planned detour first (see `plan_return_route`); reached in XY like the
    // chase waypoints.
    let mut goal = idle;
    let pos = owner_pos(c);
    while let Some(next) = c.ai.path_node.found_path.front().copied() {
        if calculate_distance(pos, next, false) > 0.25 {
            goal = next;
            break;
        }
        c.ai.path_node.found_path.pop_front();
    }
    let detour = goal != idle;
    let arrived = npc_move_towards(
        c,
        goal,
        move_speed as f32,
        move_flags,
        if detour { 0.25 } else { 1.0 },
    ) && !detour;
    let can_fly = c.npc.data().can_fly;
    // Flyers return through the air: the ground 2D distance would end the return while the
    // NPC is still below its home altitude at the same XY.
    let distance_to_idle = calculate_distance(idle, owner_pos(c), can_fly);
    // MoveTowards reports arrival at its 1 m tolerance; completing only below 1 m would leave
    // a flyer that stopped exactly 1 m above its home stuck in ReturnState.
    if arrived || distance_to_idle < 1.0 {
        on_completed_return_no_teleport(c);
        return;
    }
    if now_ms() > c.st(k).timeout_time {
        on_completed_return(c);
    }
}

fn on_completed_return(c: &mut AiCtx<'_>) {
    let idle = c.ai.idle_position;
    let can_fly = c.npc.data().can_fly;
    // The timeout teleport counts vertical displacement: a flyer below its home at the same
    // XY is still far away and must be lifted back to the idle position.
    let distance_to_idle = calculate_distance(idle, owner_pos(c), can_fly);
    if distance_to_idle > 2.0 * 2.0 {
        npc_move_towards(c, idle, 1_000_000.0, 4, 1.0);
        npc::stop_movement(c.server, &c.npc);
    }
    on_completed_return_no_teleport(c);
}

fn on_completed_return_no_teleport(c: &mut AiCtx<'_>) {
    reset_chase_path(c);
    c.go_to_idle();
}
