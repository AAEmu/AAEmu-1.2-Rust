//! Runtime membership of a spawned `npc_groups` encounter: the move leader, the followers'
//! formation offsets / tension and the group's `aggro_rule_id`.
//!
//! The spawner attaches this data to the NPCs its `"NpcGroup"` row spawned
//! ([`attach_spawned_group`], called by `npc::spawner::spawn_npc_group`); `systems::ai::group`
//! reads it for the 1.2 `formation_GoToOnPoint` follow and the `NpcAggroTable` shared-aggro
//! rule.
//!
//! `npc_groups.enable_respawn = 't'` batches are additionally tracked in [`GroupBatches`]: the
//! 10.x `NpcGroupSupporter` respawns lost members individually after the spawner's
//! `spawn_delay_min/max`, while the batch still has a living member, no living member is in
//! combat and the move leader is alive (`sub_39343460` / `sub_393422D0` /
//! `sub_39341EF0`, Z:852468 / 851557 / 851350).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::time::Instant;

use crate::server::GameServer;
use crate::systems::ai::framework::BehaviorKind;
use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::world::{now_ms, NpcRef, WorldObject};

use super::group_data::{NpcGroup, NpcGroupMember};
use super::spawner::{random_float, NpcSpawner};

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
/// make a follower mistake an unrelated NPC for its leader. `spawner` is the spawner clone that
/// created the batch; an `enable_respawn` batch registers its partial-respawn state with it.
pub fn attach_spawned_group(
    server: &GameServer,
    spawned: &[(NpcGroupMember, NpcRef)],
    group: &NpcGroup,
    spawner: Option<&Arc<NpcSpawner>>,
) {
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
    if let Some(spawner) = spawner {
        server
            .systems
            .npcs
            .group_batches
            .attach(spawned, group, spawner, instance, leader_obj_id);
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

/// Runtime state of one `npc_groups.enable_respawn` batch: the member slots (including their
/// dead-but-not-yet-respawned ones) and the 10.x `NpcGroupSupporter` respawn timer.
///
/// The registry lives on `systems::npcs::System` (per server), so test servers never share
/// batches. Every mutation runs under the spawners' reentrant spawn lock (`attach`,
/// `NpcSpawner::do_despawn`, `NpcManager::delete`, the respawn tick), which keeps a batch's
/// slots consistent without holding NPC data locks across world calls.
#[derive(Debug, Default)]
pub struct GroupBatches {
    inner: Mutex<BTreeMap<u32, Batch>>,
}

/// One member slot of a tracked batch.
#[derive(Debug, Clone)]
struct BatchMember {
    /// `npc_group_members.npc_id` (the template to recreate).
    npc_id: u32,
    /// The spawned NPC (kept across its death / corpse removal for identity and state).
    npc: NpcRef,
    is_move_leader: bool,
    offset: (f32, f32, f32),
    tension: f32,
    /// Died in combat (its slot is respawned once its corpse is gone).
    dead: bool,
}

/// How far the move leader may stray from its spawn point before a lost member is re-anchored
/// on the spawn/home position instead of the leader's current spot (m).
pub const GROUP_RESPAWN_HOME_DISTANCE: f32 = 10.0;

#[derive(Debug, Clone)]
struct Batch {
    world_id: u32,
    group_id: u32,
    aggro_rule_id: i32,
    spawner: Arc<NpcSpawner>,
    /// The spawner's owner for this batch (`Unit.OwnerId` of the members; a summon / event
    /// spawner keeps its owner on every member it respawns).
    owner_id: u32,
    leader_obj_id: u32,
    /// The move leader's spawn position (its `home_position`), used when the leader is
    /// returning home or has strayed from it.
    leader_home: (f32, f32, f32),
    members: Vec<BatchMember>,
    /// 10.x timer: `last_check_ms` and a rolled `(min + rand * (max - min)) * 1000` delay.
    last_check: Instant,
    delay: Duration,
}

/// The 10.x delay: the spawner's authored `spawn_delay_min/max` re-rolled as
/// `min + rand * (max - min)` (`sub_39343460`, Z:852490-852500).
fn roll_delay(spawner: &NpcSpawner) -> Duration {
    let Some(t) = spawner.template() else {
        return Duration::ZERO;
    };
    let secs = random_float(t.spawn_delay_min, t.spawn_delay_max).max(0.0);
    Duration::from_secs_f64(f64::from(secs))
}

impl GroupBatches {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<u32, Batch>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a freshly spawned `enable_respawn` batch.
    fn attach(
        &self,
        spawned: &[(NpcGroupMember, NpcRef)],
        group: &NpcGroup,
        spawner: &Arc<NpcSpawner>,
        instance: u32,
        leader_obj_id: u32,
    ) {
        if !group.enable_respawn || spawned.is_empty() {
            return;
        }
        let world_id = spawner
            .parent_world()
            .unwrap_or_else(|| spawned[0].1.data().unit.transform.instance_id);
        let owner_id = spawned[0].1.data().unit.owner_id;
        let leader_home = spawned
            .iter()
            .find(|(member, _)| member.is_move_leader)
            .or_else(|| spawned.first())
            .map(|(_, npc)| {
                let t = npc.data().unit.transform;
                (t.x, t.y, t.z)
            })
            .unwrap_or((0.0, 0.0, 0.0));
        let members: Vec<BatchMember> = spawned
            .iter()
            .map(|(member, npc)| BatchMember {
                npc_id: member.npc_id,
                npc: npc.clone(),
                is_move_leader: member.is_move_leader,
                offset: (
                    member.formation_offset_x,
                    member.formation_offset_y,
                    member.formation_offset_z,
                ),
                tension: if member.formation_tension > 0.0 {
                    member.formation_tension
                } else {
                    DEFAULT_FORMATION_TENSION
                },
                dead: false,
            })
            .collect();
        let now = crate::systems::npcs::spawn_manager::now();
        self.lock().insert(
            instance,
            Batch {
                world_id,
                group_id: group.id,
                aggro_rule_id: group.aggro_rule_id,
                spawner: spawner.clone(),
                owner_id,
                leader_obj_id,
                leader_home,
                members,
                last_check: now,
                delay: roll_delay(spawner),
            },
        );
    }

    /// Marks a dead member's slot. Returns whether the spawner's own whole-batch respawn must be
    /// skipped for this death: a member of an `enable_respawn` batch still has a living group
    /// mate, so the group's partial respawn owns the lost slot (10.x `HandleGroupMemberRemoved`
    /// keeps the spawner count until the group mask is empty, Z:852395-852450 /
    /// 853860-853880). The last death removes the batch and keeps the normal spawner respawn.
    pub fn on_member_died(&self, npc: &NpcRef) -> bool {
        let (instance, obj_id) = {
            let d = npc.data();
            (d.npc_group_instance, npc.obj_id)
        };
        if instance == 0 {
            return false;
        }
        let mut batches = self.lock();
        let Some(batch) = batches.get_mut(&instance) else {
            return false;
        };
        let Some(slot) = batch
            .members
            .iter_mut()
            .find(|m| m.npc.obj_id == obj_id && Arc::ptr_eq(&m.npc, npc))
        else {
            return false;
        };
        slot.dead = true;
        let any_alive = batch.members.iter().any(|m| !m.dead);
        if !any_alive {
            batches.remove(&instance);
        }
        any_alive
    }

    /// Drops the batch when a living member is removed by a despawn / GM delete: the encounter
    /// is over and the spawner's own bookkeeping takes over. A dead member's corpse removal
    /// keeps its slot for the partial respawn.
    pub fn on_member_removed(&self, npc: &NpcRef) {
        let (instance, dead) = {
            let d = npc.data();
            (d.npc_group_instance, d.unit.is_dead())
        };
        if instance == 0 || dead {
            return;
        }
        self.lock().remove(&instance);
    }

    /// Dead member slots this placement still reserves: they count against the spawner's
    /// population target until the partial respawn recreates them, so a fill cannot spawn a
    /// whole new batch beside the surviving members.
    pub(crate) fn reserved_slots(&self, spawner: &NpcSpawner) -> i64 {
        self.lock()
            .values()
            .filter(|b| Arc::ptr_eq(b.spawner.shared(), spawner.shared()))
            .map(|b| b.members.iter().filter(|m| m.dead).count() as i64)
            .sum()
    }

    /// One iteration of the 10.x group respawn timer for `world_id`'s batches.
    ///
    /// Eligibility (`sub_393422D0`, Z:851557): false while every slot is alive and while any
    /// living member is in combat. While not eligible the timer restarts with a fresh random
    /// delay. When eligible and due, `RespawnLostGroupMembers` (`sub_39341EF0`, Z:851350)
    /// returns without respawning when the move leader is dead; otherwise every lost member is
    /// spawned at the leader's position plus its formation offset and re-joined to the batch.
    pub fn check_respawns(&self, server: &GameServer, world_id: u32) {
        let snapshots: Vec<(u32, Batch)> = {
            let mut batches = self.lock();
            // A batch of a removed instance would never be visited again: drop it here so the
            // registry cannot grow for the life of the server.
            batches.retain(|_, b| server.world.world(b.world_id).is_some());
            batches
                .iter()
                .filter(|(_, b)| b.world_id == world_id)
                .map(|(id, b)| (*id, b.clone()))
                .collect()
        };
        for (batch_id, batch) in snapshots {
            let now = crate::systems::npcs::spawn_manager::now();
            let mut all_alive = true;
            let mut any_alive = false;
            let mut any_in_battle = false;
            let mut leader_alive = false;
            let mut stale = false;
            let mut lost: Vec<usize> = Vec::new();
            for (i, member) in batch.members.iter().enumerate() {
                let present = server
                    .world
                    .object(member.npc.obj_id)
                    .and_then(|o| o.as_npc().cloned())
                    .is_some_and(|n| Arc::ptr_eq(&n, &member.npc));
                let dead = member.dead || member.npc.data().unit.is_dead();
                if !present {
                    if dead {
                        lost.push(i);
                    } else {
                        // A living member left the world without dying: the batch is stale
                        // (defensive; the living delete path also drops it).
                        stale = true;
                    }
                    all_alive = false;
                    continue;
                }
                if dead {
                    all_alive = false;
                    continue;
                }
                any_alive = true;
                if member.npc.data().unit.is_in_battle {
                    any_in_battle = true;
                }
                if member.is_move_leader {
                    leader_alive = true;
                }
            }
            // A batch with no living member can never respawn (the last death normally removes
            // it): drop it so its reserved slots cannot block the spawner's refills forever.
            if stale || !any_alive {
                self.lock().remove(&batch_id);
                continue;
            }
            let eligible = !all_alive && !any_in_battle;
            if !eligible {
                if let Some(b) = self.lock().get_mut(&batch_id) {
                    b.last_check = now;
                    b.delay = roll_delay(&batch.spawner);
                }
                continue;
            }
            if now.saturating_duration_since(batch.last_check) < batch.delay {
                continue;
            }
            // `RespawnLostGroupMembers` is anchored on the move leader: without it nothing is
            // respawned (the timer keeps retrying, as in `sub_39341EF0`).
            if !leader_alive || lost.is_empty() {
                continue;
            }
            // The partial respawn is a spawn: the spawner's own gates (management, time
            // window / event period, a queued despawn, the active clone and the population
            // caps) must allow it before a lost member comes back.
            if !batch
                .spawner
                .allows_group_partial_respawn(server, batch.members.len() as i64)
            {
                continue;
            }
            let Some(leader) = batch
                .members
                .iter()
                .find(|m| m.is_move_leader)
                .map(|m| m.npc.clone())
            else {
                continue;
            };
            let leader_pos =
                crate::systems::ai::npc::world_pos(server, &WorldObject::from(&leader));
            let leader_yaw = leader.data().unit.transform.rot_z;
            // A leader that is walking home (`ReturnState`) or has strayed from its spawn
            // point re-anchors the lost member on the spawn/home position instead of the
            // leader's temporary spot. `spawn_npc_template` falls back to the spawner's
            // authored position when the anchor has no ground (water / missing floor).
            let returning = server
                .systems
                .ai
                .ais
                .try_inspect(leader.obj_id, |ai| {
                    ai.current_behavior() == Some(BehaviorKind::ReturnState)
                })
                .unwrap_or(false);
            let home = Vec3::new(
                batch.leader_home.0,
                batch.leader_home.1,
                batch.leader_home.2,
            );
            let anchor = if returning
                || calculate_distance(home, leader_pos, false) > GROUP_RESPAWN_HOME_DISTANCE
            {
                home
            } else {
                leader_pos
            };
            let mut members = batch.members.clone();
            for i in lost {
                let member = &batch.members[i];
                let Some(npc) = batch.spawner.respawn_group_member(
                    server,
                    member.npc_id,
                    batch.owner_id,
                    (anchor.x, anchor.y, anchor.z, leader_yaw),
                    member.offset,
                ) else {
                    continue;
                };
                // Re-join the same batch (same id, formation offset / tension, leader).
                {
                    let mut d = npc.data();
                    d.npc_group_instance = batch_id;
                    d.npc_group_id = batch.group_id;
                    d.npc_group_aggro_rule_id = batch.aggro_rule_id;
                    d.npc_group_is_move_leader = false;
                    d.npc_group_leader_obj_id = batch.leader_obj_id;
                    d.npc_group_formation_offset = member.offset;
                    d.npc_group_formation_tension = member.tension;
                }
                tracing::debug!(
                    group = batch.group_id,
                    npc = npc.obj_id,
                    batch = batch_id,
                    "npc group: lost member respawned"
                );
                members[i] = BatchMember {
                    npc,
                    dead: false,
                    ..member.clone()
                };
            }
            if let Some(b) = self.lock().get_mut(&batch_id) {
                b.members = members;
                b.last_check = now;
                b.delay = roll_delay(&batch.spawner);
            }
        }
    }
}
