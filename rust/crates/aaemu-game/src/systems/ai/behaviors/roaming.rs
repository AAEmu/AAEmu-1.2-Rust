use rand::Rng;

use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::world::now_ms;

use super::super::framework::{AiCtx, BehaviorKind};

use super::chase::moves_freely;
use super::npc;
use super::perception::{check_aggression, check_alert};
use super::support::{
    base_move_speed, broadcast_posture, delta_seconds, npc_move_towards, owner_pos,
};

use BehaviorKind as B;

pub(super) fn roaming_tick(c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) {
    if !c.st(k).enter {
        return;
    }
    if !check_aggression(c, k) {
        check_alert(c, k);
    }
    // A group follower that already left Roaming (combat / alert) must not move here; ordinary
    // NPCs keep the original control flow (no early return outside the feature).
    if crate::systems::npcs::npc::group::is_follower(&c.npc) && c.ai.current_behavior() != Some(k) {
        return;
    }
    // A group follower never roams on its own: it follows its move leader's formation point
    // (1.2 `roaming.UpdateRoaming`).
    if super::group::follower_tick(c, delta_ms) {
        return;
    }
    let zero = |v: Vec3| v.x == 0.0 && v.y == 0.0 && v.z == 0.0;
    if zero(c.st(k).roam_target) && now_ms() > c.st(k).next_roaming {
        match choose_roaming_position(c) {
            Some(next) => {
                c.st(k).roam_target = next;
                broadcast_posture(c, false);
            }
            // Walled in: skip this roam cycle (deviation from the C#, see PORTING.md).
            None => c.st(k).next_roaming = now_ms() + rand::thread_rng().gen_range(3..6) * 1000,
        }
    }
    let target = c.st(k).roam_target;
    if zero(target) {
        return;
    }
    let mut move_speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
    let move_flags = npc::real_movement_flags(move_speed);
    move_speed *= delta_seconds(delta_ms);
    npc_move_towards(c, target, move_speed as f32, move_flags, 1.0);
    let dist = calculate_distance(owner_pos(c), target, false);
    if dist < 1.0 {
        npc::stop_movement(c.server, &c.npc);
        let s = c.st(k);
        s.roam_target = Vec3::default();
        s.next_roaming = now_ms() + rand::thread_rng().gen_range(3..6) * 1000;
        broadcast_posture(c, true);
    }
}

/// How far in front of a wall a shortened walk stops (m).
const WALL_MARGIN: f32 = 0.6;
/// A shortened roam walk must still be at least this long (m); roaming ends below 1 m.
const MIN_SHORT_WALK: f32 = 1.5;
/// Roam targets tried before the move is shortened or the cycle skipped.
const ROAM_CANDIDATES: usize = 4;

/// The horizontal distance to the first wall on the straight walk from the NPC to `to`, or
/// `None` when the way is clear, the NPC moves freely (flyers, swimmers) or the collision
/// service is off (the C# straight walk). The check runs once per chosen target / route
/// segment, not per movement step.
pub(super) fn wall_ahead(c: &AiCtx<'_>, to: Vec3) -> Option<f32> {
    if moves_freely(c) {
        return None;
    }
    let from = owner_pos(c);
    super::collision::walk::wall_distance(
        c.server,
        c.obj.transform().instance_id,
        [from.x, from.y, from.z],
        [to.x, to.y, to.z],
    )
    .flatten()
}

/// `calc_next_roaming_position` that never picks a target behind a wall: a few random
/// candidates, else the walk to the farthest one cut short before its wall, else `None`
/// (skip this roam cycle). Without the collision service the first candidate is taken, as in
/// the C#.
pub(super) fn choose_roaming_position(c: &AiCtx<'_>) -> Option<Vec3> {
    let mut best: Option<(f32, Vec3)> = None;
    for _ in 0..ROAM_CANDIDATES {
        let cand = calc_next_roaming_position(c);
        let Some(wall) = wall_ahead(c, cand) else {
            return Some(cand);
        };
        if best.is_none_or(|(d, _)| wall > d) {
            best = Some((wall, cand));
        }
    }
    let (wall, cand) = best?;
    let from = owner_pos(c);
    let stop = wall - WALL_MARGIN;
    let total = calculate_distance(from, cand, false);
    if stop < MIN_SHORT_WALK || total <= 0.0 {
        return None;
    }
    let f = stop / total;
    let (x, y) = (
        from.x + (cand.x - from.x) * f,
        from.y + (cand.y - from.y) * f,
    );
    let (can_fly, spawner_z, zone) = {
        let n = c.npc.data();
        (n.can_fly, n.spawner_z, n.unit.transform.zone_id)
    };
    let z = npc::reference_height_in(
        c.server,
        c.obj.transform().instance_id,
        Some(crate::systems::npcs::heightmap::ReferenceHeightAi {
            can_fly,
            hold_or_idle: false,
            spawner_z,
            world_z: from.z,
        }),
        x,
        y,
        from.z,
        zone,
    );
    Some(Vec3::new(x, y, z))
}

/// `AiUtils.CalcNextRoamingPosition(ai)` (taken from x2ai.lua).
pub fn calc_next_roaming_position(c: &AiCtx<'_>) -> Vec3 {
    let max_roaming_distance = 6.0f32;
    let idle = c.ai.idle_position;
    let mut rng = rand::thread_rng();
    let x = (rng.gen::<f32>() - 0.5) * max_roaming_distance * 2.0 + idle.x;
    let y = (rng.gen::<f32>() - 0.5) * max_roaming_distance * 2.0 + idle.y;
    // Get terrain height at new position
    let (can_fly, spawner_z, zone) = {
        let n = c.npc.data();
        (n.can_fly, n.spawner_z, n.unit.transform.zone_id)
    };
    let z = npc::reference_height_in(
        c.server,
        c.obj.transform().instance_id,
        Some(crate::systems::npcs::heightmap::ReferenceHeightAi {
            can_fly,
            hold_or_idle: matches!(c.ai.current_behavior(), Some(B::HoldPosition | B::Idle)),
            spawner_z,
            world_z: owner_pos(c).z,
        }),
        x,
        y,
        idle.z,
        zone,
    );
    Vec3::new(x, y, z)
}
