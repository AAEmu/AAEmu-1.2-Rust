use std::time::Instant;

use rand::Rng;

use crate::models::movement::convert_radian_to_direction;
use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use super::super::data::SkillControllerTemplate;
use super::super::geometry::world_of;
use super::MoveDirection;

/// `System.Numerics.Vector3` (positions).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    fn length_squared(self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z
    }

    /// `Vector3.Normalize` (`v / v.Length()`).
    fn normalize(self) -> Self {
        let len = self.length_squared().sqrt();
        Self::new(self.x / len, self.y / len, self.z / len)
    }
}

// ---- MathUtil / PositionAndRotation helpers ----------------------------------------------------

pub(super) const TWO_PI: f32 = std::f32::consts::PI * 2.0;

/// `MathUtil.CalculateAngleFrom(x1, y1, x2, y2)` (degrees, `% 360`).
pub fn calculate_angle_from(a: Vec3, b: Vec3) -> f64 {
    let rad = f64::from(b.y - a.y).atan2(f64::from(b.x - a.x));
    (rad / std::f64::consts::PI * 180.0) % 360.0
}

/// `MathUtil.CalculateDistance(Vector3, Vector3, includeZ)` (double precision).
pub fn calculate_distance(a: Vec3, b: Vec3, include_z: bool) -> f32 {
    let dx = f64::from(a.x) - f64::from(b.x);
    let dy = f64::from(a.y) - f64::from(b.y);
    if include_z {
        let dz = f64::from(a.z) - f64::from(b.z);
        (dx * dx + dy * dy + dz * dz).sqrt() as f32
    } else {
        (dx * dx + dy * dy).sqrt() as f32
    }
}

/// `MathUtil.AddDistanceToFront(distance, x, y, rotZRad)`.
pub fn add_distance_to_front(distance: f32, x: f32, y: f32, rot_z_rad: f32) -> (f32, f32) {
    let rad = f64::from(rot_z_rad);
    (
        distance * rad.cos() as f32 + x,
        distance * rad.sin() as f32 + y,
    )
}

/// `PositionAndRotation.AddDistanceToFront(travelDist, targetDist, position, other)`.
pub fn add_distance_to_front_between(
    travel_dist: f32,
    target_dist: f32,
    position: Vec3,
    other: Vec3,
) -> Vec3 {
    let ratio = travel_dist / target_dist;
    Vec3::new(
        (other.x - position.x) * ratio + position.x,
        (other.y - position.y) * ratio + position.y,
        (other.z - position.z) * ratio + position.z,
    )
}

/// The position after moving `travel` of `target_dist` from `cur` towards `end`. The final step
/// (`travel >= target_dist`) lands exactly on `end`: `add_distance_to_front_between` would
/// otherwise return it only up to float rounding, which at large map coordinates (f32 ulp is
/// ~0.002 m beyond 16384) can overshoot it.
pub(super) fn next_step_position(travel: f32, target_dist: f32, cur: Vec3, end: Vec3) -> Vec3 {
    if travel >= target_dist {
        end
    } else {
        add_distance_to_front_between(travel, target_dist, cur, end)
    }
}

/// The heading of a movement step. `new` lies on the segment `cur -> end`, so the heading of the
/// step is the same as `heading(new, end)` -- except when `new` is (within float noise) `end`:
/// there `atan2` of a near-zero vector is arbitrary and flipped the unit backwards on the final
/// tick. The noise is relative to the coordinates (f32 ulp grows with them), so the tolerance
/// scales with their magnitude instead of being a fixed distance.
pub(super) fn step_heading(cur: Vec3, new: Vec3, end: Vec3) -> (f32, i16, i16) {
    let magnitude = [new.x, new.y, end.x, end.y]
        .iter()
        .fold(1.0_f32, |m, v| m.max(v.abs()));
    let tolerance = (magnitude * f32::EPSILON * 4.0).max(1e-3);
    if calculate_distance(new, end, false) <= tolerance {
        heading(cur, end)
    } else {
        heading(new, end)
    }
}

/// `float.DegToRad()`.
pub(super) fn deg_to_rad_f32(v: f32) -> f32 {
    std::f32::consts::PI / 180.0 * v
}

/// `PositionAndRotation.ToRollPitchYawSBytesMovement()` of a rotation (radians).
pub fn roll_pitch_yaw_sbytes_movement(rx: f32, ry: f32, rz: f32) -> (i8, i8, i8) {
    let f = |v: f32| convert_radian_to_direction(f64::from(v - TWO_PI));
    (f(rx), f(ry), f(rz))
}

/// The step every moving controller sends: the yaw (radians, `SetRotationDegree(0, 0,
/// angle - 90)`) and the velocity (`AddDistanceToFront(4000, 0, 0, angle.DegToRad())`) for the
/// heading from `from` to `to`.
pub fn heading(from: Vec3, to: Vec3) -> (f32, i16, i16) {
    let angle = calculate_angle_from(from, to);
    let (vx, vy) = add_distance_to_front(
        4000.0,
        0.0,
        0.0,
        (std::f64::consts::PI / 180.0 * angle) as f32,
    );
    (deg_to_rad_f32(angle as f32 - 90.0), vx as i16, vy as i16)
}

/// `(uint)(DateTime.UtcNow - DateTime.UtcNow.Date).TotalMilliseconds`.
/// `Owner.ParentWorld.Template.GeoData.GetHeight(Owner.Transform.World.Position)` (the npcs
/// subsystem's heightmaps; 0 without a world or heightmaps, like the C# exception path).
pub(super) fn geo_height(server: &GameServer, owner: &WorldObject) -> f32 {
    let Some(w) = world_of(server, owner) else {
        return 0.0;
    };
    let p = w.world_pos_rot(owner);
    crate::systems::npcs::npc::spawner::geo_data_height(server, &w, p.x, p.y)
}

/// `FloatingSkillController.GetGroundHeight()`: the heightmap height, `_startZ` when it is not
/// positive.
pub(super) fn ground_height(server: &GameServer, owner: &WorldObject, start_z: f32) -> f32 {
    let geo = geo_height(server, owner);
    // A height more than `WALKER_MAX_DROP` below the start is the terrain under an instance hall
    // (its floor is not in the height data), not the ground the unit falls to.
    if geo > 0.0 && geo >= start_z - crate::systems::ai::npc::WALKER_MAX_DROP {
        geo
    } else {
        start_z
    }
}

pub(super) fn ms_of_day() -> u32 {
    now_ms().rem_euclid(86_400_000) as u32
}

/// `LeapSkillController.ApplyDirectionConstraint`.
pub fn apply_direction_constraint(
    direction: MoveDirection,
    owner_pos: Vec3,
    candidate_end: Vec3,
    target_pos: Vec3,
) -> Vec3 {
    let owner_to_target = calculate_distance(owner_pos, target_pos, true);
    let end_to_target = calculate_distance(candidate_end, target_pos, true);
    match direction {
        MoveDirection::BackwardOnly if end_to_target < owner_to_target => owner_pos,
        MoveDirection::ForwardOnly if end_to_target > owner_to_target => owner_pos,
        _ => candidate_end,
    }
}

// ---- per-kind state ----------------------------------------------------------------------------

/// Leap / Dash: a straight move to a fixed end position at a fixed speed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearMove {
    /// `Duration` (`Value[2]`, ms).
    pub duration: i32,
    /// `DistanceOffset` (`Value[3]`, mm).
    pub distance_offset: i32,
    /// `Direction` (`Value[6]`).
    pub direction: MoveDirection,
    pub end_position: Vec3,
    pub calculated_speed: f32,
}

pub(super) fn duration_sec(duration: i32) -> f32 {
    if duration > 0 {
        duration as f32 / 1000.0
    } else {
        0.5
    }
}

impl LinearMove {
    /// The `LeapSkillController` constructor math.
    pub fn leap(template: &SkillControllerTemplate, owner_pos: Vec3, target_pos: Vec3) -> Self {
        let duration = template.value[2];
        let distance_offset = template.value[3];
        let direction = MoveDirection::from_i32(template.value[6]);
        let angle_deg = calculate_angle_from(owner_pos, target_pos) as f32;
        let angle_rad = (f64::from(angle_deg) * std::f64::consts::PI / 180.0) as f32;
        let offset = distance_offset as f32 / 1000.0;
        let (ex, ey) = add_distance_to_front(offset, target_pos.x, target_pos.y, angle_rad);
        let mut end = Vec3::new(ex, ey, target_pos.z);
        let constrained = apply_direction_constraint(direction, owner_pos, end, target_pos);
        if constrained != end {
            tracing::debug!(
                "LeapSC: {direction:?} constraint violated — staying at owner position"
            );
            end = constrained;
        }
        let distance = calculate_distance(owner_pos, end, true);
        Self {
            duration,
            distance_offset,
            direction,
            end_position: end,
            calculated_speed: distance / duration_sec(duration),
        }
    }

    /// The `DashSkillController` constructor math.
    pub fn dash(template: &SkillControllerTemplate, owner_pos: Vec3, target_pos: Vec3) -> Self {
        let duration = template.value[2];
        let distance_offset = template.value[3];
        let direction = MoveDirection::from_i32(template.value[6]);
        let mut angle_deg = calculate_angle_from(owner_pos, target_pos) as f32;
        let dist = distance_offset as f32 / 1000.0;
        if direction == MoveDirection::BackwardOnly {
            angle_deg += 180.0;
        }
        let angle_rad = (f64::from(angle_deg) * std::f64::consts::PI / 180.0) as f32;
        let (ex, ey) = add_distance_to_front(dist, owner_pos.x, owner_pos.y, angle_rad);
        let end = Vec3::new(ex, ey, owner_pos.z);
        let total = calculate_distance(owner_pos, end, true);
        Self {
            duration,
            distance_offset,
            direction,
            end_position: end,
            calculated_speed: total / duration_sec(duration),
        }
    }
}

/// Fixed-position Leap motion, including the native self-target initialization used by
/// sport fish. Foreign unit targets still use `LinearMove`: their native closest-point
/// endpoint refresh and actor-contact state are not represented by this server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeapArc {
    pub end_position: Vec3,
    pub duration: f32,
    pub elapsed: f32,
    pub horizontal_speed: f32,
    pub vertical_speed: f32,
    pub vertical_acceleration: f32,
    minimum_speed: f32,
    stop_on_overshoot: bool,
    pub(super) rotate: bool,
}

impl LeapArc {
    /// 10.x 0x39231780 / 0x39231AB0: a position offset follows the normalized 3D
    /// owner-to-target vector. A self target instead follows quaternion forward (+Y).
    /// NPCs have no player input-direction selector; only their zero-input path is used.
    pub(super) fn new(
        template: &SkillControllerTemplate,
        start: Vec3,
        target: Vec3,
        self_forward: Option<Vec3>,
        animation_speed: f32,
    ) -> Option<Self> {
        if template.value[2] <= 0 || template.value[12] != 0 {
            return None; // runtime landing-rotation flag is not ported
        }
        let offset = template.value[3] as f32 * 0.001;
        let end = if let Some(forward) = self_forward {
            let length = (forward.x * forward.x + forward.y * forward.y).sqrt();
            let forward = if length > f32::EPSILON {
                Vec3::new(forward.x / length, forward.y / length, 0.0)
            } else {
                Vec3::new(0.0, 1.0, 0.0)
            };
            Vec3::new(
                start.x + forward.x * offset,
                start.y + forward.y * offset,
                start.z,
            )
        } else {
            let delta = Vec3::new(target.x - start.x, target.y - start.y, target.z - start.z);
            let length = delta.length_squared().sqrt();
            let direction = if length > f32::EPSILON {
                delta.normalize()
            } else {
                Vec3::default()
            };
            Vec3::new(
                target.x + direction.x * offset,
                target.y + direction.y * offset,
                target.z + direction.z * offset,
            )
        };
        let distance = calculate_distance(start, end, false);
        if distance < 0.01 {
            return None; // 0x39227620: already arrived, including self-target 7071/offset0
        }
        // 0x39227E10 uses AttackAnimSpeedMul (119), not movement-speed slow/root.
        // The additional 10.x attribute218 has no 1.2 UnitAttribute representation.
        let scale = if template.value[11] == 1 {
            animation_speed.max(0.001)
        } else {
            1.0
        };
        let duration_ms = if scale >= 0.01 {
            (template.value[2] as f32 / scale).floor().max(1.0)
        } else {
            template.value[2] as f32
        };
        let duration = duration_ms * 0.001;
        let minimum_speed = template.value[1] as f32 * scale;
        // 0x39227750: only the remaining-distance speed is capped at50.
        let horizontal_speed = minimum_speed.max((distance / duration).clamp(0.0, 50.0));
        let angle = deg_to_rad_f32(template.value[0] as f32);
        let vertical_speed = (((end.z - start.z) / distance).atan() + angle)
            .min(deg_to_rad_f32(80.0))
            .tan()
            * horizontal_speed;
        let eta = distance / horizontal_speed;
        let vertical_acceleration = if angle > 0.0 && eta > 0.0 {
            (2.0 * ((end.z - start.z) - eta * vertical_speed) / (eta * eta)).clamp(-3000.0, 3000.0)
        } else {
            0.0
        };
        Some(Self {
            end_position: end,
            duration,
            elapsed: 0.0,
            horizontal_speed,
            vertical_speed,
            vertical_acceleration,
            minimum_speed,
            stop_on_overshoot: template.value[7] == 1,
            rotate: template.value[8] == 1,
        })
    }

    /// The server integrates the verified initial velocity/acceleration over measured
    /// elapsed time. Native uses an active living actor; our proxies are inactive, so this
    /// is kinematic integration, not a claim of bit-identical physics scheduling.
    pub(super) fn advance(&mut self, current: Vec3, dt: f32) -> Option<(Vec3, Vec3, bool)> {
        let remaining = self.duration - self.elapsed;
        let distance = calculate_distance(current, self.end_position, false);
        if !dt.is_finite() || dt <= 0.0 || remaining <= 0.0 || distance < 0.01 {
            return None;
        }
        if distance >= 1.0 {
            self.horizontal_speed = self
                .minimum_speed
                .max((distance / remaining).clamp(0.0, 50.0));
        }
        let mut step = dt.min(remaining);
        let predicted_overshoot = self.horizontal_speed * step > distance;
        if predicted_overshoot && self.stop_on_overshoot {
            return None;
        }
        if self.horizontal_speed > 0.0 {
            step = step.min(distance / self.horizontal_speed);
        }
        let dx = (self.end_position.x - current.x) / distance;
        let dy = (self.end_position.y - current.y) / distance;
        let next = Vec3::new(
            current.x + dx * self.horizontal_speed * step,
            current.y + dy * self.horizontal_speed * step,
            current.z + self.vertical_speed * step + 0.5 * self.vertical_acceleration * step * step,
        );
        self.vertical_speed += self.vertical_acceleration * step;
        self.elapsed += step;
        let velocity = Vec3::new(
            dx * self.horizontal_speed,
            dy * self.horizontal_speed,
            self.vertical_speed,
        );
        let ended = self.elapsed >= self.duration
            || calculate_distance(next, self.end_position, false) < 0.01;
        Some((next, velocity, ended))
    }
}

/// `FloatingSkillController` state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatingMove {
    pub end_position: Vec3,
    pub speed: f32,
    pub pull_distance: f32,
    pub stop_distance: f32,
    pub is_lift_mode: bool,
    pub lift_height: f32,
    pub lift_speed: f32,
    pub lift_duration: f32,
    pub is_falling: bool,
    pub fall_speed: f32,
    pub start_z: f32,
    pub lift_start: Option<tokio::time::Instant>,
}

pub(super) const GRAVITY: f32 = 9.81;
pub(super) const MAX_FALL_SPEED: f32 = 20.0;

impl FloatingMove {
    /// The `FloatingSkillController` constructor math.
    pub fn new(
        template: Option<&SkillControllerTemplate>,
        owner_pos: Vec3,
        target_pos: Vec3,
        psychokinesis_speed: f32,
        lift_height: f32,
        lift_speed: f32,
        lift_duration: f32,
    ) -> Self {
        let lift_speed = if lift_speed > 0.0 { lift_speed } else { 3.0 };
        let is_lift_mode = lift_height > 0.0;
        let mut s = Self {
            end_position: owner_pos,
            speed: lift_speed,
            pull_distance: 0.0,
            stop_distance: 0.0,
            is_lift_mode,
            lift_height,
            lift_speed,
            lift_duration,
            is_falling: false,
            fall_speed: 0.0,
            start_z: 0.0,
            lift_start: None,
        };
        if is_lift_mode {
            return s;
        }
        s.speed = if psychokinesis_speed > 0.0 {
            psychokinesis_speed
        } else {
            match template {
                Some(t) if t.value[0] > 0 => t.value[0] as f32 / 1000.0,
                _ => 5.0,
            }
        };
        let dist = calculate_distance(owner_pos, target_pos, true);
        s.stop_distance = 1.5;
        s.pull_distance = (dist - s.stop_distance).max(0.0);
        if dist > s.stop_distance {
            let mut d = Vec3::new(
                target_pos.x - owner_pos.x,
                target_pos.y - owner_pos.y,
                target_pos.z - owner_pos.z,
            )
            .normalize();
            d.z = 0.0;
            if d.length_squared() < 0.01 {
                d = Vec3::new(1.0, 0.0, 0.0);
            }
            let d = d.normalize();
            s.end_position = Vec3::new(
                owner_pos.x + d.x * s.pull_distance,
                owner_pos.y + d.y * s.pull_distance,
                owner_pos.z,
            );
        }
        s
    }
}

/// `WanderingSkillController` state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WanderingMove {
    pub current_waypoint: Vec3,
    /// Seconds.
    pub duration: f32,
    /// Seconds.
    pub change_interval: f32,
    pub start: Instant,
    pub last_direction_change: Instant,
}

/// `WanderingSkillController.GetFearSpeed()`: actor model stances are not loaded in Rust, so this
/// is always the C# fallback.
pub(super) const FEAR_SPEED_FALLBACK: f32 = 7.0;

/// `PickNewDirection`: a random point 5..10 m away.
pub(super) fn random_waypoint(current: Vec3) -> Vec3 {
    let mut rng = rand::thread_rng();
    let angle = rng.gen::<f64>() * 2.0 * std::f64::consts::PI;
    let dist = 5.0 + (rng.gen::<f64>() * 5.0) as f32;
    Vec3::new(
        current.x + (angle.cos() * f64::from(dist)) as f32,
        current.y + (angle.sin() * f64::from(dist)) as f32,
        current.z,
    )
}
