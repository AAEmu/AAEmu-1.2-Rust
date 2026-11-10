//! Runtime membership of a spawned `npc_groups` encounter: the move leader, the followers'
//! formation offsets / tension and the group's `aggro_rule_id`.
//!
//! The spawner attaches this data to the NPCs its `"NpcGroup"` row spawned
//! ([`attach_spawned_group`], called by `npc::spawner::spawn_npc_group`); `systems::ai::group`
//! reads it for the 1.2 `formation_GoToOnPoint` follow and the `NpcAggroTable` shared-aggro
//! rule. `enable_respawn` stays unused (no consumer was found, see `PORTING.md`).

use std::sync::atomic::{AtomicU32, Ordering};

use crate::server::GameServer;
use crate::systems::skills::controllers::Vec3;
use crate::world::{now_ms, NpcRef, WorldObject};

use super::group_data::{NpcGroup, NpcGroupMember};

/// Monotonic spawn-batch id of the next `spawn_npc_group` batch (never the leader's object id:
/// object ids are reused after a despawn, batch ids are not).
static NEXT_BATCH_ID: AtomicU32 = AtomicU32::new(1);

fn next_batch_id() -> u32 {
    loop {
        let id = NEXT_BATCH_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

/// `formation_GoToOnPoint`'s tension default when the member row has 0 (1.2
/// `pipe_manager_x2.lua`: `local tension = 1.0`).
pub const DEFAULT_FORMATION_TENSION: f32 = 1.0;

/// How long after its last `MoveTowards` step a move leader still counts as moving (ms). The AI
/// ticks every 100 ms; a slow-ticked leader (250 ms) keeps the stamp fresh while it walks and
/// `StopMovement` / an arrival clears it at once.
pub const LEADER_MOVING_TIMEOUT_MS: i64 = 1000;

/// Attaches the spawned batch's runtime membership to every member `spawn_npc_group` created.
///
/// `spawned` pairs each `npc_group_members` row with the NPC it produced (a missing NPC
/// template is skipped). The batch id is a monotonic counter value, so object-id reuse can never
/// make a follower mistake an unrelated NPC for its leader.
pub fn attach_spawned_group(spawned: &[(NpcGroupMember, NpcRef)], group: &NpcGroup) {
    if spawned.is_empty() {
        return;
    }
    let instance = next_batch_id();
    let leader_obj_id = spawned
        .iter()
        .find(|(member, _)| member.is_move_leader)
        .map(|(_, npc)| npc.obj_id)
        .unwrap_or(0);
    for (member, npc) in spawned {
        let mut d = npc.data();
        d.npc_group_instance = instance;
        d.npc_group_id = group.id;
        d.npc_group_aggro_rule_id = group.aggro_rule_id;
        d.npc_group_is_move_leader = member.is_move_leader;
        d.npc_group_leader_obj_id = leader_obj_id;
        d.npc_group_formation_offset = (
            member.formation_offset_x,
            member.formation_offset_y,
            member.formation_offset_z,
        );
        d.npc_group_formation_tension = if member.formation_tension > 0.0 {
            member.formation_tension
        } else {
            DEFAULT_FORMATION_TENSION
        };
    }
}

/// Whether `npc` is a follower of a spawned group (not its move leader).
pub fn is_follower(npc: &NpcRef) -> bool {
    let d = npc.data();
    d.npc_group_instance != 0 && !d.npc_group_is_move_leader && d.npc_group_leader_obj_id != 0
}

/// The live move leader of `npc`'s spawned group, if it still exists, is alive and is the same
/// spawn (the object id may be reused after the leader despawned).
pub fn move_leader(server: &GameServer, npc: &NpcRef) -> Option<NpcRef> {
    let (instance, leader_obj_id) = {
        let d = npc.data();
        if d.npc_group_instance == 0 || d.npc_group_is_move_leader || d.npc_group_leader_obj_id == 0
        {
            return None;
        }
        (d.npc_group_instance, d.npc_group_leader_obj_id)
    };
    let leader = server.world.object(leader_obj_id)?.as_npc()?.clone();
    {
        let d = leader.data();
        if d.npc_group_instance != instance
            || !d.npc_group_is_move_leader
            || d.npc_group_leader_obj_id != leader_obj_id
            || d.unit.is_dead()
        {
            return None;
        }
    }
    Some(leader)
}

/// The 1.2 `AI.IsMoving(leader.id, 0)` read of the leader's own movement stamp.
pub fn leader_is_moving(leader: &NpcRef) -> bool {
    let last = leader.data().last_move_ms;
    last != 0 && now_ms() - last <= LEADER_MOVING_TIMEOUT_MS
}

/// The follower's formation point: the leader's world position plus the member's
/// `formation_offset_*`.
///
/// The offsets are applied on the world axes; rotating them by the leader's yaw is
/// capture-pending (the 1.2 client files expose only the runtime AI formation strings, see
/// `PORTING.md` and `spawn_npc_group`).
pub fn formation_point(server: &GameServer, follower: &NpcRef, leader: &NpcRef) -> Vec3 {
    let offset = follower.data().npc_group_formation_offset;
    let base = crate::systems::ai::npc::world_pos(server, &WorldObject::from(leader));
    Vec3::new(base.x + offset.0, base.y + offset.1, base.z + offset.2)
}
