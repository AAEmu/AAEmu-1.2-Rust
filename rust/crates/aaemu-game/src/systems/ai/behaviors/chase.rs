use crate::systems::instances::encounters::hooks as encounter_hooks;
use crate::systems::skills::buffs;
use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::world::{now_ms, WorldObject};

use super::super::framework::{AiCtx, BehaviorKind, ChasePath};

use super::ledge::{chase_move, ChaseStep};
use super::npc;
use super::skill_use::{cast_path_blocked, MELEE_ATTACK_RANGE};
use super::support::{base_move_speed, delta_seconds, owner_pos, template_scalars};

pub(super) fn geo_data_mode(c: &AiCtx<'_>) -> bool {
    c.server.config.world.geo_data_mode
}

/// Drops the chase route and its bookkeeping, so the next chase step plans from scratch.
pub(super) fn reset_chase_path(c: &mut AiCtx<'_>) {
    c.ai.path_node.found_path.clear();
    c.ai.path_node.end_point_pos = Vec3::default();
    c.ai.chase_path = None;
    // A wait at a ledge belongs to that chase as well; a later fight starts its own.
    c.ai.ledge_block = None;
}

/// Flying and swimming NPCs (`ActorModel.MovementId == 2` / `bUnderwaterCreature`) move in
/// 3D. The A* graph is a walking-surface navmesh, so following its (ground level, detouring)
/// waypoints only drags them off the straight line to the victim.
pub(super) fn moves_freely(c: &AiCtx<'_>) -> bool {
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

/// `Npc.FindPath(abuser)` (geodata A*); `reason` is only logged. Free movers
/// ([`moves_freely`]) never get a route (deviation from the C# code, see `PORTING.md`).
pub(super) fn find_path(c: &mut AiCtx<'_>, abuser: &WorldObject, reason: &str) {
    if moves_freely(c) {
        reset_chase_path(c);
        return;
    }
    super::geodata::npc_find_path(c, abuser);
    let had_route = !c.ai.path_node.found_path.is_empty();
    c.ai.chase_path = Some(ChasePath {
        target: abuser.obj_id(),
        at_ms: now_ms(),
        had_route,
    });
    tracing::debug!(
        npc = c.npc.obj_id,
        target = abuser.obj_id(),
        reason,
        steps = c.ai.path_node.found_path.len(),
        "chase path planned"
    );
}

/// The victim has to move this far (m) from the route's end before the route is planned
/// again; the lower bound of [`REPATH_MOVE_FRACTION`] of the distance to the victim. A
/// player's reported position wobbles by a few tenths of a metre while standing or fighting,
/// and every replan starts at the NPC's new position, so replanning at the 0.5 m model-size
/// threshold flipped the route around navmesh obstacles on every other tick.
const REPATH_MIN_MOVE: f32 = 2.0;
/// Fraction of the NPC-to-victim distance the victim may move before a replan: a far chaser
/// does not need a fresh route for a small sidestep, a near one is limited by the minimum.
const REPATH_MOVE_FRACTION: f32 = 0.2;
/// Minimum time between two plans for the same victim (ms); the AI ticks every 100 ms.
const REPATH_MIN_INTERVAL_MS: i64 = 500;

/// Why the chase route must be planned again, or `None` to keep following it.
fn repath_reason(
    c: &AiCtx<'_>,
    target_id: u32,
    my_pos: Vec3,
    target_pos: Vec3,
    now: i64,
) -> Option<&'static str> {
    let end = c.ai.path_node.end_point_pos;
    let moved = calculate_distance(end, target_pos, true)
        > REPATH_MIN_MOVE.max(REPATH_MOVE_FRACTION * calculate_distance(my_pos, target_pos, true));
    let Some(last) = c.ai.chase_path else {
        // No bookkeeping: reset (zero end point) or a route set outside the chase loop.
        return if end == Vec3::default() {
            Some("new target")
        } else {
            moved.then_some("target moved")
        };
    };
    if last.target != target_id {
        return Some("new target");
    }
    if now - last.at_ms < REPATH_MIN_INTERVAL_MS {
        return None;
    }
    if moved {
        Some("target moved")
    } else if last.had_route && c.ai.path_node.found_path.is_empty() {
        Some("path exhausted")
    } else {
        None
    }
}

/// `BaseCombatBehavior.CheckPipeName()`.
///
/// `phase_dragon_ground` / phase 1 grounds the owner. The C# used `GeoData.GetHeight` (the
/// navigation mesh) for every NPC; the 1.2 navmesh is a walking-surface graph that lies above
/// the terrain (measured navmesh minus raw heightmap: 2.8 m at the Dewstone Nui statue, 4.9 m
/// south of Vedisa, 4.5-5.7 m at the Cloaked Forest flesh-golem camp), so grounding a walker
/// on it hovered it: `AlertBehavior.Enter` runs this and then `StopMovement`, which broadcasts
/// the lifted Z. A ground walker therefore uses the same floor as its movement steps
/// (`reference_height_in`: collision floor, then the navmesh / heightmap fallback); flying and
/// swimming NPCs keep the authored navigation height.
pub(super) fn check_pipe_name(c: &mut AiCtx<'_>, k: BehaviorKind) {
    let (pipe, phase) = {
        let s = c.st(k);
        (s.pipe_name.clone(), s.phase_type)
    };
    if pipe == "phase_dragon_ground" || phase == 1 {
        // Ground walkers take the floor their movement steps use (collision floor, then the
        // bounded navmesh / heightmap fallback, else their current Z). Flyers and swimmers keep
        // the C# navigation height: `geo_data_height` (navmesh, else heightmap), which answers
        // 0.0 when there is no geodata at all.
        let pos = owner_pos(c);
        let z = if moves_freely(c) {
            super::geodata::geo_data_height(c.server, &c.obj, pos)
        } else {
            let (instance_id, zone_id, spawner_z) = {
                let n = c.npc.data();
                (
                    n.unit.transform.instance_id,
                    n.unit.transform.zone_id,
                    n.spawner_z,
                )
            };
            npc::reference_height_in(
                c.server,
                instance_id,
                Some(crate::systems::npcs::heightmap::ReferenceHeightAi {
                    can_fly: false,
                    hold_or_idle: false,
                    spawner_z,
                    world_z: pos.z,
                }),
                pos.x,
                pos.y,
                pos.z,
                zone_id,
            )
        };
        c.obj.with_transform(|t| t.set_height(z));
    } else if pipe == "phase_dragon_fly_hovering" || phase == 2 {
        c.obj.with_transform(|t| t.z += 15.0);
        npc::stop_movement(c.server, &c.npc);
    } else if pipe == "phase_dragon_fly_path" {
        c.go_to_follow_path();
    }
}

/// `BaseCombatBehavior.MoveInRange(target, delta)`.
pub(super) fn move_in_range(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    target: Option<&WorldObject>,
    delta_ms: i64,
) {
    if npc::is_displaced(&c.obj) {
        return;
    }
    if npc::is_rooted(c.server, &c.obj) || npc::is_dead(&c.obj) {
        return;
    }
    if npc::is_shackled_or_snared(c.server, &c.obj) {
        return;
    }
    if npc::skill_controller_running(&c.obj) {
        return;
    }
    let mut speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
    let move_flags = npc::real_movement_flags(speed);
    speed *= delta_seconds(delta_ms);
    // Sport-fish direction plots provide bounded movement through SkillController.
    // The former C# experimental global-axis drift added movement after each controller
    // and changed the heading used by the next relative plot (Dive even moved along Y).
    // Between authored controllers, stop without changing fish position or rotation.
    if buffs::check_buffs_by_tag(c.server, &c.obj, npc::tags::FISH) {
        npc::stop_movement(c.server, &c.npc);
        return;
    }
    let t = template_scalars(c);
    let mut range = t.attack_start_range_scale;
    let max_weapon_range = c.st(k).max_weapon_range;
    if t.use_range_mod && max_weapon_range != 0.0 {
        range *= max_weapon_range;
    }
    if t.base_skill_id == 2 && t.skills_count == 0 && range == 4.0 {
        range -= 1.0; // Fix that ID=7927, Plateau Earth Elemental can hit with a melee attack
    }
    let distance_to_target = match target {
        Some(t) => npc::distance_to(c.server, &c.obj, t, true),
        None => 0.0,
    };
    let Some(target) = target else {
        npc::stop_movement(c.server, &c.npc);
        return;
    };
    let target_pos = npc::world_pos(c.server, target);
    if distance_to_target <= range {
        let ranged_base = c
            .server
            .game_data
            .systems
            .skills
            .skill(t.base_skill_id as u32);
        let needs_line_of_sight = ranged_base.is_some_and(|skill| {
            skill.max_range > MELEE_ATTACK_RANGE as i32 && cast_path_blocked(c, skill, target)
        });
        if !needs_line_of_sight {
            // The victim is in reach again: a wait at a ledge is over.
            c.ai.ledge_block = None;
            npc::stop_movement(c.server, &c.npc);
            return;
        }
    }
    if encounter_hooks::for_zone(c.obj.transform().zone_id).chase(c, target_pos, speed, move_flags)
    {
        return;
    }
    if geo_data_mode(c) && moves_freely(c) {
        // Flyers and swimmers chase in a straight 3D line; drop any route left over.
        if c.ai.chase_path.is_some() || !c.ai.path_node.found_path.is_empty() {
            reset_chase_path(c);
        }
    } else if geo_data_mode(c) {
        // COPStick::Execute (10.x cryaisystem_dedicate 0x310F24D8) retains its route
        // until the target moves beyond the regeneration distance. The C# test was
        // reversed: a stationary target restarted the route at its first node every tick.
        let target_id = target.obj_id();
        if let Some(reason) = repath_reason(c, target_id, owner_pos(c), target_pos, now_ms()) {
            let started = std::time::Instant::now();
            find_path(c, target, reason);
            if started.elapsed() >= std::time::Duration::from_millis(1) {
                tracing::warn!(
                    "FindPath took {:?} for Ai.Owner.ObjId:{}",
                    started.elapsed(),
                    c.npc.obj_id
                );
            }
            c.ai.path_node.end_point_pos = target_pos;
        }
        // Navigation heights can differ from the collision floor, and a walking NPC follows
        // the ground, so waypoints are reached in XY; attack range applies to the victim,
        // not path corners.
        let pos = owner_pos(c);
        const WAYPOINT_TOLERANCE: f32 = 0.25;
        while let Some(next) = c.ai.path_node.found_path.front().copied() {
            if calculate_distance(pos, next, false) > WAYPOINT_TOLERANCE {
                // Every corner before the last is a step along the planned route; the last one
                // is the victim's position when the route was planned.
                let route_corner = c.ai.path_node.found_path.len() > 1;
                chase_move(
                    c,
                    target,
                    next,
                    ChaseStep {
                        distance: speed as f32,
                        flags: move_flags,
                        tolerance: WAYPOINT_TOLERANCE,
                        route_corner,
                    },
                );
                return;
            }
            c.ai.path_node.current_target_pos = next;
            c.ai.path_node.found_path.pop_front();
        }
    }
    chase_move(
        c,
        target,
        target_pos,
        ChaseStep {
            distance: speed as f32,
            flags: move_flags,
            tolerance: 1.0,
            route_corner: false,
        },
    );
}
