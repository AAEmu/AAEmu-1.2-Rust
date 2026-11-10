//! NPC group rules of `npc_groups` / `npc_group_members` on the AI side:
//!
//! * formation movement: the 1.2 `idle` / `roaming` / `hold_position` behaviors run
//!   `formation_GoToOnPoint` for a group follower (`pipe_manager_x2.lua`): while the group's move
//!   leader moves, the follower walks to the leader position plus its `formation_offset_*` and
//!   stops within `formation_tension`; while the leader stands still it waits. After combat it
//!   returns to that formation point (the leader's yaw is not applied to the offsets: rotating
//!   them is capture-pending, see `PORTING.md`).
//! * shared aggro: `npc_groups.aggro_rule_id != 0` makes every living member of the same spawned
//!   group join a fight another member started (10.x
//!   `NpcAggroTable::CollectSharedAggroMembers`; what distinguishes rule 1 from 2 is unknown).
//!
//! The membership itself is attached at spawn by `npcs::npc::group::attach_spawned_group`.

use crate::systems::npcs::npc::group;
use crate::systems::skills::controllers::calculate_distance;
use crate::systems::skills::state::AggroKind;
use crate::world::{NpcRef, WorldObject};

use super::behaviors;
use super::combat::add_unit_aggro;
use super::framework::{AiCtx, BehaviorKind};
use super::npc;

/// Search radius (m) for the spawned group's other members. The authored offsets are at most
/// 15 / 20 m; the aggro-link search radius is scaled by the member template and some group
/// templates scale the 200 m help radius down to 2 m, so it cannot find the group.
const GROUP_SHARE_RADIUS: f32 = 200.0;

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

/// `formation_GoToOnPoint` step for a group follower. Returns true when the NPC handled this
/// tick: a follower must not run its own idle roaming / out-of-area logic.
///
/// Leader dead or despawned (or the group has no `is_move_leader` member) returns false, so the
/// caller's normal idle behaviour takes over.
pub fn follower_tick(c: &mut AiCtx<'_>, delta_ms: i64) -> bool {
    if !group::is_follower(&c.npc) {
        return false;
    }
    // The spawn settles in the shared `Idle` behavior, which has no `OnAggroTargetChanged`
    // transition; a follower must reach its own idle behavior (`HoldPosition` / `Roaming`) so
    // the normal combat transitions stay available. Only universal `Idle` AIs stay here.
    if c.ai.current_behavior() == Some(BehaviorKind::Idle)
        && c.ai.default_behavior() != Some(BehaviorKind::Idle)
    {
        c.go_to_default_behavior();
        return true;
    }
    let Some(leader) = group::move_leader(c.server, &c.npc) else {
        stop_formation_move(c);
        return false;
    };
    // While the leader stands still the follower waits (1.2 `AI.IsMoving(leader.id, 0)`).
    if !group::leader_is_moving(&leader) {
        stop_formation_move(c);
        return true;
    }
    let target = group::formation_point(c.server, &c.npc, &leader);
    let pos = npc::world_pos(c.server, &c.obj);
    let tension = c.npc.data().npc_group_formation_tension;
    if calculate_distance(pos, target, false) <= tension {
        stop_formation_move(c);
        return true;
    }
    let mut speed = npc::real_movement_speed(
        &c.obj,
        f64::from(npc::base_move_speed(c.server, &c.npc, false)),
    );
    let flags = npc::real_movement_flags(speed);
    speed *= delta_seconds(delta_ms);
    behaviors::npc_move_towards(c, target, speed as f32, flags, tension);
    c.npc.data().npc_group_formation_moving = true;
    true
}

/// A follower returning from combat goes to its formation point, not its own spawn (the 1.2
/// behaviors re-anchor a group member's idle position on the leader). Called from
/// `ReturnState.Enter`; a dead / missing leader leaves the normal idle position in place.
pub fn sync_return_to_formation(c: &mut AiCtx<'_>) {
    let Some(leader) = group::move_leader(c.server, &c.npc) else {
        return;
    };
    c.ai.idle_position = group::formation_point(c.server, &c.npc, &leader);
}

/// `NpcAggroTable::CollectSharedAggroMembers`: with a non-zero `aggro_rule_id`, every living
/// member of the same spawned group gets `abuser` on its aggro table and is told about the new
/// target. Reuses the aggro-link help path's `AddUnitAggro` + `OnAggroTargetChanged`.
pub fn share_aggro_with_group(c: &mut AiCtx<'_>, abuser: &WorldObject) {
    let (instance, rule, last_shared) = {
        let d = c.npc.data();
        (
            d.npc_group_instance,
            d.npc_group_aggro_rule_id,
            d.npc_group_last_share_abuser,
        )
    };
    // Rule 0 groups do not share; rules 1 and 2 both do (difference unknown). Running the
    // member scan again for the same abuser on every combat tick is not needed: the members
    // already joined (cleared with the aggro table).
    if instance == 0 || rule == 0 || last_shared == abuser.obj_id() {
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
