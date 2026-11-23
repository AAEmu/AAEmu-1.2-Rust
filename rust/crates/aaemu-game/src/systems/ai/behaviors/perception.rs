use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::models::transform::PositionAndRotation;
use crate::systems::instances::encounters::registry as encounters;
use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::systems::skills::state::AggroKind;
use crate::world::{now_ms, WorldObject};

use super::super::combat::{add_unit_aggro, can_attack};
use super::super::framework::{AiCtx, BehaviorKind};
use super::super::skills as ai_skills;

use super::chase::check_pipe_name;
use super::npc;
use super::support::{broadcast_posture, template_scalars};
use super::targeting::{current_target, set_target};

/// `Behavior.OnEnemySeen(target)`.
fn on_enemy_seen(c: &mut AiCtx<'_>, target: &WorldObject) {
    add_unit_aggro(c.server, &c.obj, AggroKind::Damage, target, 1);
    c.go_to_combat();
}

/// The units sorted by 3D distance (`CheckAggression` / `CheckAlert`), with their positions.
fn units_by_distance(c: &AiCtx<'_>, radius: f32) -> Vec<(WorldObject, f32, PositionAndRotation)> {
    let _profile = super::profile::step(super::profile::Step::TargetSearch);
    let me = c.obj.clone();
    let origin = npc::world_pos(c.server, &me);
    let mut v: Vec<(WorldObject, f32, PositionAndRotation)> =
        npc::units_around(c.server, &me, radius, false)
            .into_iter()
            .map(|(u, p)| {
                let d = calculate_distance(origin, Vec3::new(p.x, p.y, p.z), true);
                (u, d, p)
            })
            .collect();
    v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

fn unit_dead(u: &WorldObject) -> bool {
    u.with_unit(|x| x.unit().is_dead() || x.unit().hp <= 0)
        .unwrap_or(false)
}

/// The shared scan of `CheckAggression` / `CheckAlert`.
fn scan_for_enemy(
    c: &AiCtx<'_>,
    radius: f32,
    gap_fly: f32,
    gap_ground: f32,
    near_mul: f32,
) -> Option<WorldObject> {
    let t = template_scalars(c);
    let (can_fly, scale, separate_floors) = {
        let n = c.npc.data();
        (
            n.can_fly,
            n.unit.scale,
            encounters::floor_bound(n.unit.transform.zone_id, n.unit.template_id),
        )
    };
    let model_size = npc::model_size(c.server, &c.obj);
    let origin = npc::world_pos_and_rot(c.server, &c.obj);
    let my_z = origin.z;
    for (unit, range, pos) in units_by_distance(c, radius) {
        if unit_dead(&unit) || (separate_floors && (my_z - pos.z).abs() > encounters::FLOOR_GAP) {
            continue;
        }
        // Arbitrary value
        let max_height_gap = if can_fly {
            model_size * scale * gap_fly
        } else {
            model_size * scale * gap_ground
        };
        let uz = pos.z;
        let in_front = npc::is_front_at(&origin, &pos, f64::from(t.sight_fov_scale))
            && (my_z - uz).abs() < max_height_gap;
        if (in_front || range < near_mul * t.sight_range_scale)
            && can_attack(c.server, &c.obj, &unit)
            && npc::can_see_target(c.server, &c.obj, &unit)
            && !c
                .npc
                .data()
                .unit
                .skill_state
                .has_pending_aggro_on(unit.obj_id())
        {
            return Some(unit);
        }
    }
    None
}

/// `Behavior.CheckAggression()`.
pub fn check_aggression(c: &mut AiCtx<'_>, _k: BehaviorKind) -> bool {
    let _profile = super::profile::step(super::profile::Step::Aggression);
    let t = template_scalars(c);
    if !t.aggression {
        return false;
    }
    match scan_for_enemy(c, t.attack_start_range_scale * 10.0, 3.5, 1.5, 1.5) {
        Some(unit) => {
            on_enemy_seen(c, &unit);
            true
        }
        None => false,
    }
}

/// `Behavior.OnEnemyAlert(target)`.
fn on_enemy_alert(c: &mut AiCtx<'_>, target: &WorldObject) {
    let now = now_ms();
    c.ai.alert_end_time = now + 5000;
    c.ai.next_alert_check_time = now + 7000;
    set_target(c, Some(target));
    c.go_to_alert();
}

/// `Behavior.CheckAlert()`.
pub fn check_alert(c: &mut AiCtx<'_>, _k: BehaviorKind) -> bool {
    if c.ai.next_alert_check_time > now_ms() {
        return false;
    }
    // Don't do alerts if already in combat
    if c.npc.data().unit.is_in_battle {
        return false;
    }
    let _profile = super::profile::step(super::profile::Step::Alert);
    let t = template_scalars(c);
    match scan_for_enemy(c, t.sight_range_scale * 15.0, 4.0, 1.75, 2.0) {
        Some(unit) => {
            on_enemy_alert(c, &unit);
            true
        }
        None => false,
    }
}

pub(super) fn alert_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    check_pipe_name(c, k);
    npc::stop_movement(c.server, &c.npc);
    broadcast_posture(c, false);
    if let Some(t) = current_target(c) {
        let r = c.obj.transform();
        c.st(k).old_rotation = (r.rot_x, r.rot_y, r.rot_z);
        let tp = npc::world_pos(c.server, &t);
        npc::look_towards(c.server, &c.npc, tp, 4);
    }
    npc::set_game_stance(&c.npc, GameStanceType::COMBAT);
    npc::set_alertness(&c.npc, MoveTypeAlertness::ALERT);
    super::npc_events::in_alert(c);
    let s = c.st(k);
    s.pipe_name = "phase_dragon_ground".into();
    s.enter = true;
}

pub(super) fn alert_tick(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if !c.st(k).enter {
        return; // not initialized yet Enter()
    }
    if let Some(t) = current_target(c) {
        let tp = npc::world_pos(c.server, &t);
        npc::look_towards(c.server, &c.npc, tp, 4);
    }
    let skill_task = c.npc.data().unit.skill_state.skill_task.is_some();
    if now_ms() > c.ai.alert_end_time && !skill_task {
        let (x, y, z) = c.st(k).old_rotation;
        c.obj.with_transform(|t| {
            t.rot_x = x;
            t.rot_y = y;
            t.rot_z = z;
        });
        set_target(c, None);
        c.go_to_idle();
        return;
    }
    check_aggression(c, k);
}
