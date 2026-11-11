//! NPC group rules of `npc_groups` / `npc_group_members` on the AI side:
//!
//! * formation movement: the 1.2 `idle` / `roaming` / `hold_position` behaviors run
//!   `formation_GoToOnPoint` for a group follower (`pipe_manager_x2.lua`): while the group's move
//!   leader moves, the follower walks to the leader position plus its `formation_offset_*` and
//!   stops within `formation_tension`; while the leader stands still it waits. After combat it
//!   returns to that formation point (the leader's yaw is not applied to the offsets: rotating
//!   them is capture-pending, see `PORTING.md`).
//! * shared aggro: `npc_groups.aggro_rule_id == 2` (`aggro_share`) mirrors the aggro table to
//!   every living member of the same spawned group (10.x
//!   `NpcAggroTable::CollectSharedAggroMembers`); rule 1 (`aggro_link`) does not mirror it but
//!   its members answer the attacked member's link-help call (`update_aggro_help`).
//!
//! The membership itself is attached at spawn by `npcs::npc::group::attach_spawned_group`.

use crate::systems::npcs::npc::group;
use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::systems::skills::state::AggroKind;
use crate::world::{NpcRef, WorldObject};

use super::collision::walk;
use super::combat::add_unit_aggro;
use super::framework::{AiCtx, BehaviorKind};
use super::npc;

/// Search radius (m) for the spawned group's other members. The authored offsets are at most
/// 15 / 20 m; the aggro-link search radius is scaled by the member template and some group
/// templates scale the 200 m help radius down to 2 m, so it cannot find the group.
const GROUP_SHARE_RADIUS: f32 = 200.0;

/// A formation follower farther than this from its leader (m) is out of its encounter: it stops
/// following and uses its own idle behaviour (e.g. an old batch whose leader id was reused).
pub const MAX_FOLLOW_DISTANCE: f32 = 60.0;

/// Slack (m) over `formation_tension` within which a follower waiting for a standing leader does
/// not walk (it only re-forms when it was actually left behind).
const FORMATION_SETTLE_MARGIN: f32 = 0.5;

fn delta_seconds(delta_ms: i64) -> f64 {
    (delta_ms.rem_euclid(1000)) as f64 / 1000.0
}

/// Stops a running formation move exactly once (`StopMovement` is broadcast, so it is not sent
/// on every idle tick).
fn stop_formation_move(c: &mut AiCtx<'_>) {
    if c.npc.data().npc_group_formation_moving {
        npc::stop_movement(c.server, &c.npc);
        c.npc.data().npc_group_formation_moving = false;
    }
}

/// The live move leader and its world position, when `c`'s follower may still follow it (same
/// batch, alive, within [`MAX_FOLLOW_DISTANCE`]).
fn follow_target(c: &AiCtx<'_>) -> Option<(NpcRef, Vec3)> {
    let leader = group::move_leader(c.server, &c.npc)?;
    let leader_pos = npc::world_pos(c.server, &WorldObject::from(&leader));
    let my_pos = npc::world_pos(c.server, &c.obj);
    if calculate_distance(my_pos, leader_pos, false) > MAX_FOLLOW_DISTANCE {
        return None;
    }
    Some((leader, leader_pos))
}

/// Whether the NPC ignores static walls when it moves (flyers and underwater creatures).
fn moves_freely(c: &AiCtx<'_>) -> bool {
    let n = c.npc.data();
    n.can_fly
        || c.server
            .game_data
            .systems
            .ai
            .ai
            .actor_model(n.unit.model_id)
            .is_some_and(|m| m.underwater_creature)
}

/// The formation point, unless the straight walk to it is blocked by a wall: then the leader's
/// own position (the formation is built around it), or stay when even that is blocked.
fn walkable_formation_target(c: &AiCtx<'_>, leader_pos: Vec3, point: Vec3) -> Vec3 {
    if moves_freely(c) {
        return point;
    }
    let from = npc::world_pos(c.server, &c.obj);
    let instance = c.obj.transform().instance_id;
    let blocked = |to: Vec3| {
        walk::walk_blocked(
            c.server,
            instance,
            [from.x, from.y, from.z],
            [to.x, to.y, to.z],
        ) == Some(true)
    };
    if !blocked(point) {
        point
    } else if !blocked(leader_pos) {
        leader_pos
    } else {
        from
    }
}

/// `formation_GoToOnPoint` step for a group follower. Returns true when the NPC handled this
/// tick: a follower must not run its own idle roaming / out-of-area logic.
///
/// Leader dead or despawned (or the group has no `is_move_leader` member) returns false, so the
/// caller's normal idle behaviour takes over.
pub fn follower_tick(c: &mut AiCtx<'_>, delta_ms: i64) -> bool {
    if !group::is_follower(&c.npc) {
        return false;
    }
    let _profile = super::profile::step(super::profile::Step::Follower);
    // The spawn settles in the shared `Idle` behavior, which has no `OnAggroTargetChanged`
    // transition; a follower must reach its own idle behavior (`HoldPosition` / `Roaming`) so
    // the normal combat transitions stay available. Only universal `Idle` AIs stay here.
    if c.ai.current_behavior() == Some(BehaviorKind::Idle)
        && c.ai.default_behavior() != Some(BehaviorKind::Idle)
    {
        c.go_to_default_behavior();
        return true;
    }
    let Some((leader, leader_pos)) = follow_target(c) else {
        stop_formation_move(c);
        return false;
    };
    let point = group::formation_point(c.server, &c.npc, &leader);
    let pos = npc::world_pos(c.server, &c.obj);
    let tension = c.npc.data().npc_group_formation_tension;
    // A standing leader is waited for, unless the follower was left behind (then it re-forms).
    let tolerance = if group::leader_is_moving(&leader) {
        tension
    } else {
        tension + FORMATION_SETTLE_MARGIN
    };
    let within = |to: Vec3| calculate_distance(pos, to, false) <= tolerance;
    // A waiting follower is the common case: it needs no wall check (raycasts) while it is
    // already within the tolerance of its formation point.
    if within(point) {
        stop_formation_move(c);
        return true;
    }
    let target = walkable_formation_target(c, leader_pos, point);
    if within(target) {
        stop_formation_move(c);
        return true;
    }
    let mut speed = npc::real_movement_speed(
        &c.obj,
        f64::from(npc::base_move_speed(c.server, &c.npc, false)),
    );
    let flags = npc::real_movement_flags(speed);
    speed *= delta_seconds(delta_ms);
    // A formation walk is a ground walk even for a HoldPosition / Idle NPC: the spawner Z is
    // not the floor under the formation point.
    npc::move_towards(
        c.server,
        &c.npc,
        Some(npc::MoveAi {
            hold_or_idle: false,
        }),
        target,
        speed as f32,
        flags,
        tolerance,
    );
    c.npc.data().npc_group_formation_moving = true;
    true
}

/// A follower returning from combat goes to its formation point, not its own spawn (the 1.2
/// behaviors re-anchor a group member's idle position on the leader). Called from
/// `ReturnState.Enter`; a dead / missing leader leaves the normal idle position in place.
pub fn sync_return_to_formation(c: &mut AiCtx<'_>) {
    let Some((leader, leader_pos)) = follow_target(c) else {
        return;
    };
    let point = group::formation_point(c.server, &c.npc, &leader);
    c.ai.idle_position = walkable_formation_target(c, leader_pos, point);
}

/// `NpcAggroTable::CollectSharedAggroMembers`: with `aggro_rule_id == 2` (`aggro_share`),
/// every living member of the same spawned group gets `abuser` on its aggro table and is told
/// about the new target. Reuses the aggro-link help path's `AddUnitAggro` +
/// `OnAggroTargetChanged`.
///
/// Rule 1 (`aggro_link`) does not mirror the table; its members answer the helper's normal
/// aggro-link help call instead (`update_aggro_help`). Rule 0 does neither. The 10.x spawn only
/// passes the group instance to the aggro table for rule 2 (Z:855825-855826) and rule 1 is
/// handled in the link-help scan (Z:823640-823646 / 823849-823851).
pub fn share_aggro_with_group(c: &mut AiCtx<'_>, abuser: &WorldObject) {
    let (instance, rule, last_shared) = {
        let d = c.npc.data();
        (
            d.npc_group_instance,
            d.npc_group_aggro_rule_id,
            d.npc_group_last_share_abuser,
        )
    };
    // Only rule 2 mirrors. Running the member scan again for the same abuser on every combat
    // tick is not needed: the members already joined (cleared with the aggro table).
    if instance == 0 || rule != 2 || last_shared == abuser.obj_id() {
        return;
    }
    c.npc.data().npc_group_last_share_abuser = abuser.obj_id();
    let members: Vec<NpcRef> = npc::around(c.server, &c.obj, GROUP_SHARE_RADIUS, false)
        .into_iter()
        .filter_map(|o| o.as_npc().cloned())
        .collect();
    for other_npc in members {
        let same_group = {
            let d = other_npc.data();
            d.npc_group_instance == instance && !d.unit.is_dead()
        };
        if !same_group || other_npc.obj_id == abuser.obj_id() {
            continue;
        }
        if !super::manager::has_ai(c.server, other_npc.obj_id) {
            continue;
        }
        let was_engaged = other_npc
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&abuser.obj_id());
        if was_engaged {
            continue;
        }
        add_unit_aggro(
            c.server,
            &WorldObject::from(&other_npc),
            AggroKind::Damage,
            abuser,
            1,
        );
        tracing::debug!(
            member = other_npc.obj_id,
            source = c.npc.obj_id,
            abuser = abuser.obj_id(),
            group = instance,
            "npc group: member joined the fight"
        );
        super::manager::post(c.server, other_npc.obj_id, |o| o.on_aggro_target_changed());
    }
}
