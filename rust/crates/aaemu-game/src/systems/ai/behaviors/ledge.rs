use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::world::{now_ms, WorldObject};

use super::super::combat::clear_aggro_of_unit;
use super::super::framework::{AiCtx, LedgeBlock};

use super::chase::{moves_freely, reset_chase_path};
use super::npc;
use super::support::{npc_move_towards, owner_pos};

/// How long (ms) a chaser may wait at a ledge without getting closer to its victim before it
/// gives the victim up.
const LEDGE_GIVE_UP_MS: i64 = 3_000;

/// How much closer (m) to its victim a chaser waiting at a ledge must get for that to count
/// as progress (a possible step sideways along the edge does not).
const LEDGE_PROGRESS: f32 = 1.0;

/// One chase step of [`chase_move`].
#[derive(Debug, Clone, Copy)]
pub(super) struct ChaseStep {
    /// Step length (m) before the speed modifier.
    pub(super) distance: f32,
    pub(super) flags: u8,
    /// Arrival tolerance (m).
    pub(super) tolerance: f32,
    /// The goal is a corner of the planned route, not the victim.
    pub(super) route_corner: bool,
}

/// A chase step towards `goal` (the victim or a route corner). A walker does not step off a
/// ledge ([`npc::step_leaves_ledge`]): it waits at the edge (deviation from the C#, which
/// walked it on at its own height through the air, see `PORTING.md`).
///
/// The wait ends only with progress: a step along the planned route, the chaser getting
/// [`LEDGE_PROGRESS`] closer to the victim than it was at the edge, or the victim coming back
/// within the walker's drop limit (a reachable chase again). A chaser that has made no
/// progress for [`LEDGE_GIVE_UP_MS`] drops the victim it cannot reach, even when some steps
/// in between were possible: a step sideways along the edge, or towards a victim that moves
/// about below, used to restart the wait, so it could pace the edge for the whole fight.
pub(super) fn chase_move(c: &mut AiCtx<'_>, victim: &WorldObject, goal: Vec3, step: ChaseStep) {
    let now = now_ms();
    let victim_id = victim.obj_id();
    if c.ai.ledge_block.is_some_and(|b| b.victim != victim_id) {
        c.ai.ledge_block = None;
    }
    let own = owner_pos(c);
    let victim_pos = npc::world_pos(c.server, victim);
    let to_victim = calculate_distance(own, victim_pos, true);
    if chase_step_leaves_ledge(c, goal, step.distance) {
        let block = *c.ai.ledge_block.get_or_insert(LedgeBlock {
            since_ms: now,
            victim: victim_id,
            distance: to_victim,
        });
        if c.npc.data().last_move_ms != 0 {
            npc::stop_movement(c.server, &c.npc);
        }
        if now - block.since_ms >= LEDGE_GIVE_UP_MS {
            give_up_unreachable_victim(c, victim);
        }
        return;
    }
    if let Some(block) = c.ai.ledge_block {
        let zone_id = c.obj.transform().zone_id;
        let victim_below = victim_pos.z < own.z - npc::walker_max_drop(zone_id);
        if ledge_wait_over(&block, &step, to_victim, victim_below) {
            c.ai.ledge_block = None;
        } else if now - block.since_ms >= LEDGE_GIVE_UP_MS {
            give_up_unreachable_victim(c, victim);
            return;
        }
    }
    npc_move_towards(c, goal, step.distance, step.flags, step.tolerance);
}

/// Whether a possible chase `step` ends the wait at a ledge `block`: a step along the planned
/// route, getting [`LEDGE_PROGRESS`] closer to the victim (`to_victim`, 3D), or a victim no
/// longer below the walker's drop limit (`victim_below` false: it climbed back up to the
/// walker's floor, so this is a reachable chase again, even when the victim keeps its
/// distance on a direct route). A step sideways towards a victim still below does not end it.
pub(super) fn ledge_wait_over(
    block: &LedgeBlock,
    step: &ChaseStep,
    to_victim: f32,
    victim_below: bool,
) -> bool {
    step.route_corner || !victim_below || to_victim < block.distance - LEDGE_PROGRESS
}

/// The end of a wait at a ledge: the victim leaves the aggro table and the route is dropped.
fn give_up_unreachable_victim(c: &mut AiCtx<'_>, victim: &WorldObject) {
    tracing::debug!(
        npc = c.npc.obj_id,
        victim = victim.obj_id(),
        "chase: victim below a ledge, giving up"
    );
    npc::stop_movement(c.server, &c.npc);
    reset_chase_path(c);
    clear_aggro_of_unit(c.server, &c.obj, victim);
}

/// Whether the next chase step towards `goal` would take a walker off a ledge.
fn chase_step_leaves_ledge(c: &AiCtx<'_>, goal: Vec3, distance: f32) -> bool {
    if moves_freely(c) {
        return false;
    }
    let pos = owner_pos(c);
    let remaining = calculate_distance(pos, goal, false);
    // The step `npc::move_towards` takes: `distance` already carries `MoveSpeedMul`.
    let step = distance;
    if remaining < 0.01 || step < 0.01 {
        return false;
    }
    // Look at least half a metre ahead so a short step cannot creep over the edge.
    let t = step.max(0.5).min(remaining) / remaining;
    let (instance_id, zone_id) = {
        let tr = c.obj.transform();
        (tr.instance_id, tr.zone_id)
    };
    let dir = ((goal.x - pos.x) / remaining, (goal.y - pos.y) / remaining);
    npc::step_leaves_ledge(
        c.server,
        instance_id,
        zone_id,
        pos.z,
        (pos.x + t * (goal.x - pos.x), pos.y + t * (goal.y - pos.y)),
        dir,
    )
}
