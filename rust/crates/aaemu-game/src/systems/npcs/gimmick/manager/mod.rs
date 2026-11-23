//! `GimmickManager` (one per world instance), `Gimmick.GimmickTick`, `GimmickSpawner.Spawn` /
//! `Despawn`, and the skill-effect spawner constructor.
//!
//! The C# `Gimmick` runtime members (`Time`, `MoveDown`, `IsMoving`, `WaitTime`, `LastPos`,
//! `TotalLifeTime`, `MovementHandler`, `Spawner`, `Template`, `Respawn`, `Despawn`) live in
//! [`ActiveGimmick`] next to the [`GimmickRef`]; the members the packets read (`Vel`, `AngVel`,
//! `Scale`, transform) are in `models::gimmick::Gimmick`.
//!
//! Locking: an [`ActiveGimmick`]'s state mutex is taken before (never while holding) the
//! gimmick's data lock, and neither is held while calling into the world.

mod spawn;
mod tick;

pub(crate) use self::spawn::cast_from_doodad_gimmick;
pub use self::spawn::{
    despawn, spawn_from_doodad, spawn_from_effect, spawn_from_spawner, SpawnGimmickParams,
};
pub use self::tick::{gimmick_tick, stop_movement};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use super::movement::{self, MovementHandler, MovementState};
use super::packets::SCGimmickMovementPacket;
use super::spawner::GimmickSpawner;
use super::GimmickTemplate;
use crate::managers::chat::chat_type;
use crate::models::gimmick::Gimmick;
use crate::models::transform::{PositionAndRotation, Quaternion};
use crate::packets::world::SCChatMessagePacket;
use crate::server::GameServer;
use crate::systems::instances::encounters::registry as encounters;
use crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick;
use crate::world::object::Entity;
use crate::world::{self, session, GimmickRef, WorldInstance, WorldObject};

/// `GimmickManager.Delay`: the tick period.
pub const TICK_DELAY: Duration = Duration::from_millis(50);
/// The template id that gets `GimmickMovementFloatToSurface` (Recovered Treasure Chest).
pub const FLOAT_TO_SURFACE_TEMPLATE_ID: u32 = 37;
pub(in crate::systems::npcs) const SORVELL_BARREL_TEMPLATE_ID: u32 = encounters::FALLING_BARREL;
const PALACE_BOULDER_TEMPLATE_ID: u32 = encounters::ROLLING_BOULDER;
// Barrel impacts retain a fraction of their speed, then settle before the fuse.
const BARREL_RESTITUTION: f32 = 0.24;
const BARREL_REST_SPEED: f32 = 0.8;
/// The `proxy` cylinder of r208022's `objects/env/01_nuia/001_ndeco/old/barrel_grape_e.cgf`
/// (the Howling Abyss Bomber keg 49 and the other `폭발통` / `폭약통` gimmicks) in model space.
/// The raw primitive's axis is its node's Z, but the proxy node is turned so that the axis is
/// model X, the long axis of the render mesh (x ±0.911, y ±0.634, z ±0.640): the model already
/// lies on its side. Centred on the origin. Used only when the client files are not loaded.
pub(in crate::systems::npcs) const KEG_PROXY_CYLINDER: CylinderProxy = CylinderProxy {
    center: [0.0; 3],
    axis: [1.0, 0.0, 0.0],
    r: 0.661,
    hh: 0.916,
};
/// The `proxy` cylinder of r208022's `Objects/Env/01_nuia/001_ndeco/old/barrel_oak_b.cgf`
/// (Sorvell's Armory barrel 39): upright along model Z with its base on the origin (centre
/// z 0.529; render mesh z -0.006..1.095, x/y ±0.488). Used only when the client files are not
/// loaded.
pub(in crate::systems::npcs) const OAK_BARREL_PROXY_CYLINDER: CylinderProxy = CylinderProxy {
    center: [0.0, 0.0, 0.529],
    axis: [0.0, 0.0, 1.0],
    r: 0.474,
    hh: 0.529,
};
/// Rolling resistance of a landed keg (m/s²). **Inferred (no 1.2 server evidence):** the
/// `gimmicks` row has no rolling friction (`damping` 0). With this value a keg thrown at the
/// authored 8 m/s rolls about 3 m after it settles, well inside its six-second fuse, so it
/// still lands about where the previous sliding keg stopped.
const KEG_ROLL_DECELERATION: f32 = 4.0;

fn uses_ballistic_motion(template_id: u32) -> bool {
    template_id == PALACE_BOULDER_TEMPLATE_ID
        || encounters::is_thrown_trap(template_id)
        || is_portable_cannon_projectile(template_id)
}

fn is_portable_cannon_projectile(template_id: u32) -> bool {
    matches!(template_id, 45 | 46)
}
/// Contact radius of a gimmick for `GimmickTemplate.CollisionSkillId` hits. The `gimmicks`
/// table has no size / radius column and a gimmick has no actor `model_id` (`ModelManager`'s
/// radii are keyed by actor model ids, and `new_gimmick` never sets one), so the model radius
/// is not available: this is a stand-in. The touched unit's own actor radius is added to it (a
/// large NPC is touched at its edge).
///
/// **Inferred (no 1.2 server evidence):** the 1.5 m value, and that the authored `scale` (0.3-20;
/// gimmick 5 spawns at scale 15, gimmicks 6 and 7 at 20) does not change it. Retail's zone
/// process reports a collision from its physics (`ZWGimmickCollided`); the evidence is that
/// packet's RTTI name plus `rust/docs/physics/ZONE-PHYSICS-USAGE.md:264-266`, not a decompile.
pub const COLLISION_RADIUS: f32 = 1.5;
/// Cap of the per-gimmick contact-hit set ([`GimmickState::collision_hits`]). Object ids are
/// reused and a gimmick lives at most `life_time`; a touch beyond the cap is ignored instead of
/// growing the set without bound. **Inferred (no 1.2 server evidence):** the value is a Rust-only
/// bound.
const MAX_COLLISION_HITS: usize = 256;
/// Authored (effect / doodad spawn) motion stops after this time or distance when the template
/// has no shorter `life_time`: a `life_time = 0` gimmick would otherwise fly forever.
/// **Inferred (no 1.2 server evidence):** 30 s and 200 m are Rust-only bounds.
pub const RUNAWAY_CAP_TIME: Duration = Duration::from_secs(30);
pub const RUNAWAY_CAP_DISTANCE: f32 = 200.0;
/// Movement packets for a gimmick that only rotates in place (position unchanged) go out at
/// this rate instead of every 50 ms tick. **Inferred (no 1.2 server evidence):** the 5 Hz rate
/// and that the client interpolates between the queued `SCGimmickMovement` states (nothing of the
/// client was checked); the repo's physics doc says retail synced gimmick state every
/// `sg_sync_period` 100 ms (`rust/docs/physics/ZONE-PHYSICS-USAGE.md:264`).
pub const SPIN_BROADCAST_MS: i64 = 200;

/// `(uint)(DateTime.UtcNow - DateTime.UtcNow.Date).TotalMilliseconds`.
pub fn time_of_day_ms(now_ms: i64) -> u32 {
    now_ms.rem_euclid(86_400_000) as u32
}

/// The C# `Gimmick` runtime members.
#[derive(Debug, Clone, Default)]
pub struct GimmickState {
    /// Kinematic velocities supplied by a doodad's SpawnGimmick phase function.
    pub authored_motion: bool,
    /// Floor under an Armory barrel, resolved when the barrel is created.
    barrel_floor: Option<f32>,
    /// The falling barrel's model proxy: the floor stops its lowest point, not its origin.
    barrel_proxy: Option<CylinderProxy>,
    /// A thrown keg rolling on its side.
    rolling_keg: Option<RollingKeg>,
    /// `Time`.
    pub time: u32,
    /// `MoveDown`.
    pub move_down: bool,
    /// `IsMoving`.
    pub is_moving: bool,
    /// `WaitTime` (unix ms, 0 = `DateTime.MinValue`).
    pub wait_time_ms: i64,
    /// `TotalLifeTime`.
    pub total_life_time: Duration,
    /// `LastPos`.
    pub last_pos: [f32; 3],
    /// `LastRot`.
    pub last_rot: [f32; 3],
    /// The skill claimed by a timer or an explicit cast. A contact cannot cast the same skill.
    skill_started: Option<u32>,
    /// The contact scan has claimed the collision skill until the cast is refused or recorded.
    collision_casting: bool,
    /// `Despawn != DateTime.MinValue`.
    pub despawned: bool,
    /// `Respawn` (unix ms, 0 = `DateTime.MinValue`).
    pub respawn_ms: i64,
    /// Units this gimmick has already hit with its `collision_skill_id` (obj ids, at most
    /// [`MAX_COLLISION_HITS`]). Not a C# member: the C# never implemented contact hits.
    /// **Inferred (no 1.2 server evidence):** a permanent per-unit latch; retail's collision
    /// packet carries a 3000 ms immunity instead (`rust/docs/physics/ZONE-PHYSICS-USAGE.md:264-266`).
    pub collision_hits: Vec<u32>,
    /// Time spent under authored (spawn-velocity) motion and the distance travelled, for
    /// [`RUNAWAY_CAP_TIME`] / [`RUNAWAY_CAP_DISTANCE`].
    pub authored_time: Duration,
    pub authored_distance: f32,
    /// Last movement broadcast (unix ms) and whether one was ever sent; a spin-only gimmick is
    /// rate-limited to [`SPIN_BROADCAST_MS`] and a static one stops broadcasting.
    pub last_broadcast_ms: i64,
    pub broadcast_sent: bool,
    /// The velocity of the last movement packet: a change (a gimmick stopping at a travel cap or
    /// an elevator reaching its stop) is broadcast even though the position no longer changes,
    /// otherwise clients keep extrapolating the last non-zero velocity.
    pub last_vel: [f32; 3],
    /// The character this gimmick acts for, resolved when it was spawned: the thrower of a
    /// gimmick that a player (or a unit of a player: mate, slave, another gimmick) cast, 0 for a
    /// spawner / doodad / NPC gimmick. Not a C# member. `gimmick_tick` removes the gimmick as soon
    /// as that character is no longer in the world: `relation::owner_character` finds the owner
    /// through the live spawner unit, so a gimmick that outlived its thrower would be owner-less
    /// and skip every rule keyed on the character (Peace, no-PvP, siege hostility, tagging).
    pub owner_character_id: u32,
}

/// A gimmick's cylinder physics proxy in model space (the proxy node's rotation applied, as the
/// collision applies it), scaled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::systems::npcs) struct CylinderProxy {
    pub center: [f32; 3],
    /// Unit axis.
    pub axis: [f32; 3],
    pub r: f32,
    pub hh: f32,
}

impl CylinderProxy {
    fn scaled(self, scale: f32) -> Self {
        let s = if scale > 0.0 { scale } else { 1.0 };
        Self {
            center: self.center.map(|c| c * s),
            r: self.r * s,
            hh: self.hh * s,
            ..self
        }
    }

    /// How far the origin sits above the floor when the proxy's lowest point rests on it, with
    /// the gimmick turned by `rot` (roll, pitch, yaw).
    pub(in crate::systems::npcs) fn floor_clearance(&self, rot: [f32; 3]) -> f32 {
        let q = PositionAndRotation::new(0.0, 0.0, 0.0, rot[0], rot[1], rot[2]).to_quaternion();
        let c = q.rotate(self.center);
        let az = q.rotate(self.axis)[2].abs().min(1.0);
        self.hh * az + self.r * (1.0 - az * az).sqrt() - c[2]
    }

    /// The model axis (0 = X, 1 = Y, 2 = Z) closest to the cylinder axis.
    fn model_axis(&self) -> usize {
        let a = self.axis.map(f32::abs);
        if a[0] >= a[1] && a[0] >= a[2] {
            0
        } else if a[1] >= a[2] {
            1
        } else {
            2
        }
    }

    /// The orientation (roll, pitch, yaw) that lays the cylinder axis horizontal along
    /// `(cos heading, sin heading)`, turned by `spin` about it the way a body rolling towards
    /// `(-sin heading, cos heading)` turns (angular velocity along the negative axis). The
    /// spin is a single Euler angle for every model axis, so it never meets a gimbal lock.
    fn lying(&self, heading: f32, spin: f32) -> [f32; 3] {
        use std::f32::consts::FRAC_PI_2;
        match self.model_axis() {
            0 => [-spin, 0.0, heading],
            1 => [0.0, -spin, heading - FRAC_PI_2],
            // Roll 90° lays model Z along -Y; pitch then turns about that axis.
            _ => [FRAC_PI_2, spin, heading + FRAC_PI_2],
        }
    }

    /// The stable pose nearest `rot`: on an end when the axis is within 45° of vertical
    /// (upright or upside down), otherwise on its side along the axis's current heading.
    fn rest_pose(&self, rot: [f32; 3]) -> [f32; 3] {
        use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, PI};
        let q = PositionAndRotation::new(0.0, 0.0, 0.0, rot[0], rot[1], rot[2]).to_quaternion();
        let a = q.rotate(self.axis);
        if a[2].abs() < FRAC_1_SQRT_2 {
            return self.lying(a[1].atan2(a[0]), 0.0);
        }
        // The model axis turned to world +Z (or -Z when the other end is down).
        let up = (a[2] > 0.0) == (self.axis[self.model_axis()] > 0.0);
        match (self.model_axis(), up) {
            (0, true) => [0.0, -FRAC_PI_2, rot[2]],
            (0, false) => [0.0, FRAC_PI_2, rot[2]],
            (1, true) => [FRAC_PI_2, 0.0, rot[2]],
            (1, false) => [-FRAC_PI_2, 0.0, rot[2]],
            (_, true) => [0.0, 0.0, rot[2]],
            (_, false) => [PI, 0.0, rot[2]],
        }
    }
}

/// A keg lying on its side: its cylinder axis is horizontal and across its heading, and it
/// turns about that axis by the distance it rolls.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RollingKeg {
    /// Scaled model proxy.
    proxy: CylinderProxy,
    /// Yaw of the rolling direction (forward = `(-sin, cos)`, the unit convention).
    heading: f32,
    /// Turn about the cylinder axis (rad).
    spin: f32,
    /// Spin rate kept while the keg is airborne (rad/s).
    spin_rate: f32,
}

impl RollingKeg {
    fn new(proxy: CylinderProxy, heading: f32) -> Self {
        Self {
            proxy,
            heading,
            spin: 0.0,
            spin_rate: 0.0,
        }
    }

    /// Writes the orientation: the cylinder axis across the heading, turned by the spin.
    fn apply(&self, t: &mut crate::models::transform::Transform) {
        [t.rot_x, t.rot_y, t.rot_z] = self.proxy.lying(self.heading, self.spin);
    }

    /// Height of the origin above the floor it rolls on.
    fn clearance(&self) -> f32 {
        self.proxy
            .floor_clearance(self.proxy.lying(self.heading, self.spin))
    }

    /// World angular velocity of the rolling motion (about the negative cross axis).
    fn ang_vel(&self) -> [f32; 3] {
        let (sin, cos) = self.heading.sin_cos();
        [-cos * self.spin_rate, -sin * self.spin_rate, 0.0]
    }
}

/// The scaled cylinder proxy of a gimmick's model: read from the model when the client files
/// are loaded, else the known proxy of that model file ([`KEG_PROXY_CYLINDER`],
/// [`OAK_BARREL_PROXY_CYLINDER`]). `None` for a model without a cylinder proxy (prefabs).
pub(in crate::systems::npcs) fn cylinder_proxy(
    server: &GameServer,
    template: &GimmickTemplate,
    scale: f32,
) -> Option<CylinderProxy> {
    use crate::systems::ai::collision;
    use aaemu_physics::geometry::primitives::Primitive;
    use aaemu_physics::math::Vec3;
    let path = collision::cells::normalize_model_path(&template.model_path);
    let from_model = collision::files(server).and_then(|files| {
        let model = collision::model_cache(server, &files).get(&path)?;
        let part = model
            .parts
            .iter()
            .find(|p| p.slot == collision::models::phys_slot::DEFAULT)?;
        let Primitive::Cylinder(c) = part.geom.geom.primitive(0)? else {
            return None;
        };
        let turn = |v: Vec3| {
            let v = part.frame.map_or(v, |f| f.q.rotate(v));
            [v.x, v.y, v.z]
        };
        Some(CylinderProxy {
            center: turn(c.center),
            axis: turn(c.axis),
            r: c.r,
            hh: c.hh,
        })
    });
    let file = path.rsplit('/').next().unwrap_or_default();
    from_model
        .or(match file {
            "barrel_grape_e.cgf" => Some(KEG_PROXY_CYLINDER),
            "barrel_oak_b.cgf" => Some(OAK_BARREL_PROXY_CYLINDER),
            _ => None,
        })
        .map(|p| p.scaled(scale))
}

impl GimmickState {
    /// `TimeLeft` (ms).
    pub fn time_left(&self, now_ms: i64) -> u32 {
        if self.wait_time_ms > now_ms {
            (self.wait_time_ms - now_ms) as u32
        } else {
            0
        }
    }
}

/// Runtime state for a spawned gimmick. Activation registers it for ticks.
#[derive(Debug)]
pub struct ActiveGimmick {
    pub gimmick: GimmickRef,
    /// `ParentWorld.Id`.
    pub world_id: u32,
    /// `Spawner`.
    pub spawner: Arc<GimmickSpawner>,
    /// `Template` (`None` for elevators).
    pub template: Option<GimmickTemplate>,
    /// `MovementHandler`.
    pub handler: Option<MovementHandler>,
    state: Mutex<GimmickState>,
}

impl ActiveGimmick {
    pub fn state(&self) -> MutexGuard<'_, GimmickState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn activate(self, server: &GameServer) -> GimmickRef {
        let gimmick = self.gimmick.clone();
        server
            .systems
            .npcs
            .gimmicks
            .for_world(self.world_id)
            .add(Arc::new(self));
        gimmick
    }
}

/// `WorldInstance.GimmickManager`: the active gimmicks of one instance (insertion order).
#[derive(Debug, Default)]
pub struct WorldGimmicks {
    active: Mutex<Vec<Arc<ActiveGimmick>>>,
}

impl WorldGimmicks {
    fn lock(&self) -> MutexGuard<'_, Vec<Arc<ActiveGimmick>>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `GetActiveGimmicks()` (a snapshot).
    pub fn active(&self) -> Vec<Arc<ActiveGimmick>> {
        self.lock().clone()
    }

    /// `_activeGimmicks.TryGetValue(objId)`.
    pub fn get(&self, obj_id: u32) -> Option<Arc<ActiveGimmick>> {
        self.lock()
            .iter()
            .find(|a| a.gimmick.obj_id == obj_id)
            .cloned()
    }

    /// `_activeGimmicks.TryAdd`.
    fn add(&self, g: Arc<ActiveGimmick>) {
        let mut l = self.lock();
        if !l.iter().any(|a| a.gimmick.obj_id == g.gimmick.obj_id) {
            l.push(g);
        }
    }

    /// `RemoveActiveGimmick`.
    pub fn remove(&self, obj_id: u32) -> bool {
        let mut l = self.lock();
        let before = l.len();
        l.retain(|a| a.gimmick.obj_id != obj_id);
        l.len() != before
    }
}

/// `server.systems.npcs.gimmicks`: the `GimmickManager` of every world instance.
#[derive(Debug, Default)]
pub struct GimmickManager {
    worlds: Mutex<HashMap<u32, Arc<WorldGimmicks>>>,
}

impl GimmickManager {
    /// The `GimmickManager` of instance `world_id` (created on first use).
    pub fn for_world(&self, world_id: u32) -> Arc<WorldGimmicks> {
        self.worlds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(world_id)
            .or_default()
            .clone()
    }

    /// The `GimmickManager` of instance `world_id` when it has one (a lookup never creates it,
    /// so an instance that is gone cannot get a registry back).
    pub fn existing(&self, world_id: u32) -> Option<Arc<WorldGimmicks>> {
        self.worlds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&world_id)
            .cloned()
    }

    /// Drops the `GimmickManager` of a removed instance (`false` = it had none). Its gimmicks
    /// are not ticked or despawned any more; the instance cleanup removed their objects.
    pub fn remove_world(&self, world_id: u32) -> bool {
        self.worlds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&world_id)
            .is_some()
    }

    fn all(&self) -> Vec<(u32, Arc<WorldGimmicks>)> {
        let mut v: Vec<_> = self
            .worlds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, w)| (*k, w.clone()))
            .collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    /// The active gimmick with `obj_id` in any instance.
    pub fn find(&self, obj_id: u32) -> Option<Arc<ActiveGimmick>> {
        self.all().into_iter().find_map(|(_, w)| w.get(obj_id))
    }
}

/// `GimmickManager.GimmickTick` of every instance.
pub fn tick_all(server: &GameServer, delta: Duration) {
    let now = world::now_ms();
    for (world_id, gimmicks) in server.systems.npcs.gimmicks.all() {
        let Some(world) = server.world.world_if_exists(world_id) else {
            // The instance was destroyed (a dungeon): nothing of this registry is alive any
            // more, and every pass would otherwise ask for the gone instance again.
            server.systems.npcs.gimmicks.remove_world(world_id);
            continue;
        };
        for g in gimmicks.active() {
            gimmick_tick(server, &world, &g, delta, now);
        }
    }
}

/// `GimmickManager.Initialize`: the 50 ms tick task.
pub fn start_tick(server: &Arc<GameServer>) {
    info!("GimmickTickTask: Started");
    let weak: Weak<GameServer> = Arc::downgrade(server);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK_DELAY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last = Instant::now();
        loop {
            interval.tick().await;
            let Some(server) = weak.upgrade() else {
                break;
            };
            let now = Instant::now();
            let delta = now - last;
            last = now;
            crate::systems::run_blocking(&server, move |s| tick_all(s, delta)).await;
        }
    });
}

#[cfg(test)]
mod tests;
