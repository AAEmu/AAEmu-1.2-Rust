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

/// `GimmickManager.AddActiveGimmick`: attaches the movement handler, sets `Time` and registers it.
/// `authored_motion` is part of the state from the start: a tick between the registration and a
/// later write of the flag would take the gimmick for a non-authored one and overwrite its
/// authored `vel` / `ang_vel` with the position delta. The same goes for `owner_character_id`: a
/// tick before a later write would see an ownerless gimmick.
fn new_active_gimmick(
    world_id: u32,
    gimmick: GimmickRef,
    spawner: Arc<GimmickSpawner>,
    template: Option<GimmickTemplate>,
    mut state: GimmickState,
) -> ActiveGimmick {
    let (template_id, entity_guid) = {
        let g = gimmick.data();
        (g.unit.template_id, g.entity_guid)
    };
    let handler = if template_id == 0 && entity_guid > 0 {
        // Elevators defined in gimmick_spawns.json
        Some(MovementHandler::Elevator)
    } else if template_id == FLOAT_TO_SURFACE_TEMPLATE_ID {
        Some(MovementHandler::FloatToSurface)
    } else {
        None
    };
    state.time = time_of_day_ms(world::now_ms());
    ActiveGimmick {
        gimmick,
        world_id,
        spawner,
        template,
        handler,
        state: Mutex::new(state),
    }
}

fn new_gimmick(
    server: &GameServer,
    template_id: u32,
    template: Option<&GimmickTemplate>,
) -> Option<Gimmick> {
    let gimmick_id = server.ids.gimmick.get_next_id().ok()? as u16;
    let mut g = Gimmick::new(template_id);
    g.gimmick_id = gimmick_id;
    g.model_path = template.map(|t| t.model_path.clone()).unwrap_or_default();
    g.unit.faction_id = 0; // new SystemFaction()

    // A gimmick has no level in the data. `Gimmick : Unit` left it 0, and the collision skills'
    // effects start at level 1 (`start_level` 1, `end_level` 99), so a level-0 gimmick caster
    // skipped every effect (verified live on skill 18717: "caster level outside the effect's
    // level range"). **Inferred (no 1.2 server evidence):** level 1, the lowest level the effect
    // rows accept. It does not make level-scaled damage meaningful: a gimmick has no stats
    // (`stats()` evaluates it through `base_unit_stat`, which returns 0 for `LevelDps`), so a
    // `use_level_damage` row that is not `use_fixed_damage` evaluates to 0 and lands the 1 HP
    // minimum. Only fixed-damage rows deal their authored value (see PORTING.md).
    g.unit.level = 1;
    Some(g)
}

/// `GimmickManager.Create(objectId, templateId, spawner)` + `Gimmick.Spawn` +
/// `AddActiveGimmick`. Note (C#): the obj id and gimmick id are taken before the origin check
/// and leak when it fails.
fn create(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    obj_id: u32,
    template_id: u32,
    spawner: Arc<GimmickSpawner>,
) -> Option<ActiveGimmick> {
    // For elevators: templateId = 0 and Template = null, but EntityGuid is used.
    let template = server
        .game_data
        .systems
        .npcs
        .gimmick
        .get_gimmick_template(template_id)
        .cloned();
    if template.is_none() && template_id != 0 {
        return None;
    }
    let obj_id = if obj_id > 0 {
        obj_id
    } else {
        server.ids.object.get_next_id().ok()?
    };
    let mut g = new_gimmick(server, template_id, template.as_ref())?;
    g.entity_guid = if template.is_none() {
        spawner.entity_guid
    } else {
        0
    };
    g.spawner_id = spawner.id;
    // Transform.ApplyWorldSpawnPosition(spawner.Position) (C#: NullReferenceException without one)
    let Some(pos) = spawner.position else {
        error!(
            "Can't spawn gimmick {template_id}: spawner {} has no Position",
            spawner.id
        );
        return None;
    };
    {
        let t = &mut g.unit.transform;
        t.instance_id = world.id;
        t.world_id = world.template.id;
        t.zone_id = pos.zone_id;
        t.set_position(pos.x, pos.y, pos.z, pos.roll, pos.pitch, pos.yaw);
        // Transform.Local.ApplyFromQuaternion(spawner rotation)
        let (roll, pitch, yaw) = PositionAndRotation::rotation_from_quaternion(Quaternion {
            x: spawner.rotation_x,
            y: spawner.rotation_y,
            z: spawner.rotation_z,
            w: spawner.rotation_w,
        });
        t.rot_x = roll;
        t.rot_y = pitch;
        t.rot_z = yaw;
    }
    g.vel = [0.0; 3];
    g.unit.scale = spawner.scale; // SetScale
    if g.unit.transform.x == 0.0 && g.unit.transform.y == 0.0 && g.unit.transform.z == 0.0 {
        error!("Can't spawn gimmick {template_id}");
        return None;
    }
    let gimmick: GimmickRef = Entity::new(obj_id, g);
    let changes = world.spawn(&gimmick, &server.view_ctx());
    session::handle_zone_changes(server, changes);
    // A spawner gimmick (an elevator, a raft) belongs to nobody.
    Some(new_active_gimmick(
        world.id,
        gimmick,
        spawner,
        template,
        GimmickState::default(),
    ))
}

/// `GimmickSpawner.Spawn(0)`: spawns the gimmick of `spawner` (template `UnitId`, 0 =
/// elevator) in `world`. The caller (SpawnManager) sets `Position.WorldId` / `ZoneId` first, as
/// `LoadGimmickSpawns` does. `None` = `"Gimmick {UnitId}, from spawn not exist at db"`.
pub fn spawn_from_spawner(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    spawner: &GimmickSpawner,
) -> Option<GimmickRef> {
    match create(server, world, 0, spawner.unit_id, Arc::new(spawner.clone())) {
        Some(active) => Some(active.activate(server)),
        None => {
            warn!("Gimmick {}, from spawn not exist at db", spawner.unit_id);
            None
        }
    }
}

/// The `SpawnGimmickEffect` members the skill-effect constructor reads.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SpawnGimmickParams {
    pub gimmick_id: u32,
    pub target_position: Option<PositionAndRotation>,
    pub offset_from_source: bool,
    pub offset_coordinate_id: i32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub offset_z: f32,
    pub scale: f32,
    pub velocity_coordinate_id: i32,
    pub velocity_x: f32,
    pub velocity_y: f32,
    pub velocity_z: f32,
    pub ang_vel_coordinate_id: i32,
    pub ang_vel_x: f32,
    pub ang_vel_y: f32,
    pub ang_vel_z: f32,
}

/// `new GimmickSpawner(parentWorld, sgEffect, caster)` (`SpawnGimmickEffect`): creates the
/// gimmick (`GimmickManager.Create(templateId)`) at the caster's world transform, applies the
/// offset, spawns it and makes it active. The caller sets `npc.Gimmick` for an NPC caster.
/// `None` when the template does not exist or the offset type is invalid (C# throws).
/// Note (C#): `OffsetCoordinateType.Unk0` moves by `AddDistanceToFront(1, 1, pos, pos)`, which
/// is a no-op.
pub fn spawn_from_effect(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    params: &SpawnGimmickParams,
    caster: &WorldObject,
) -> Option<GimmickRef> {
    Some(spawn_effect_gimmick(server, world, params, caster, false)?.activate(server))
}

/// Spawns the world object. Callers that need ticking activate it after initialization.
/// With `doodad_motion`, the caller supplies the authored velocities before activation.
fn spawn_effect_gimmick(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    params: &SpawnGimmickParams,
    caster: &WorldObject,
    doodad_motion: bool,
) -> Option<ActiveGimmick> {
    let template = server
        .game_data
        .systems
        .npcs
        .gimmick
        .get_gimmick_template(params.gimmick_id)
        .cloned()?;
    let spawner = Arc::new(GimmickSpawner {
        gimmick_id: params.gimmick_id,
        offset_from_source: params.offset_from_source,
        offset_coordinate_id: params.offset_coordinate_id,
        offset_x: params.offset_x,
        offset_y: params.offset_y,
        offset_z: params.offset_z,
        scale: params.scale,
        velocity_coordinate_id: params.velocity_coordinate_id,
        velocity_x: params.velocity_x,
        velocity_y: params.velocity_y,
        velocity_z: params.velocity_z,
        ang_vel_coordinate_id: params.ang_vel_coordinate_id,
        ang_vel_x: params.ang_vel_x,
        ang_vel_y: params.ang_vel_y,
        ang_vel_z: params.ang_vel_z,
        count: 1,
        respawn_time: 0, // don't respawn
        ..GimmickSpawner::default()
    });
    let obj_id = server.ids.object.get_next_id().ok()?;
    let mut g = new_gimmick(server, template.id, Some(&template))?;
    // gimmick.Transform = caster.Transform.CloneDetached(gimmick)
    let caster_t = caster.transform();
    let caster_world = world.world_pos_rot(caster);
    let falling_barrel = params.gimmick_id == SORVELL_BARREL_TEMPLATE_ID && !doodad_motion;
    let origin = if falling_barrel && !params.offset_from_source {
        params.target_position.unwrap_or(caster_world)
    } else {
        caster_world
    };
    let cannon_projectile = is_portable_cannon_projectile(template.id);
    let cannon_aim = params
        .target_position
        .filter(|aim| cannon_projectile && (aim.x - origin.x).hypot(aim.y - origin.y) >= 4.0);
    let yaw = cannon_aim.map_or(caster_world.rot_z, |aim| {
        (origin.x - aim.x).atan2(aim.y - origin.y)
    });
    {
        let t = &mut g.unit.transform;
        t.instance_id = caster_t.instance_id;
        t.world_id = caster_t.world_id;
        t.zone_id = caster_t.zone_id;
        t.set_position(
            origin.x,
            origin.y,
            origin.z,
            caster_world.rot_x,
            caster_world.rot_y,
            yaw,
        );
    }
    g.entity_guid = 0;
    g.spawner_unit_id = caster.obj_id();
    g.grasper_unit_id = 0;
    // `Gimmick : Unit` defaulted to a null faction, so every relation check treated the gimmick
    // as hostile to everyone. **Inferred (no 1.2 server evidence):** a collision skill belongs to
    // the side that spawned it, so the caster's faction is copied (the doodad spawn path
    // overrides it with the authored `faction_id`).
    g.unit.faction_id = caster.with_base_unit(|b| b.faction_id());
    match params.offset_coordinate_id {
        1 if falling_barrel => {
            g.unit.transform.set_xyz(
                origin.x + params.offset_x,
                origin.y + params.offset_y,
                origin.z + params.offset_z,
            );
        }
        0 | 1 | 3 => {}
        2 => {
            // Transform.Local.AddDistance(OffsetX, OffsetY, OffsetZ): a plain translation
            let t = &mut g.unit.transform;
            t.set_xyz(
                t.x + params.offset_x,
                t.y + params.offset_y,
                t.z + params.offset_z,
            );
        }
        other => {
            error!("SpawnGimmickEffect: invalid OffsetCoordinateId {other}");
            return None;
        }
    }
    g.unit.scale = params.scale;
    if (cannon_projectile || encounters::is_thrown_trap(template.id))
        && params.offset_coordinate_id == 2
    {
        let (sin_yaw, cos_yaw) = yaw.sin_cos();
        g.unit.transform.set_xyz(
            origin.x + params.offset_x * cos_yaw - params.offset_y * sin_yaw,
            origin.y + params.offset_x * sin_yaw + params.offset_y * cos_yaw,
            origin.z + params.offset_z,
        );
    }
    let barrel_floor = falling_barrel.then(|| {
        use crate::systems::ai::collision;
        let t = &mut g.unit.transform;
        let floor = collision::ground_height_probe(server, world.id, t.x, t.y, origin.z, 0.5, 6.0)
            .flatten()
            .unwrap_or(origin.z);
        // Keep the authored drop inside the room when the twenty-meter offset puts it
        // above a solid ceiling.
        if let Some(hit) = collision::raycast(
            server,
            world.id,
            [t.x, t.y, floor + 0.5],
            [t.x, t.y, t.z],
            collision::query::mask::STATIC_TERRAIN,
            collision::query::ray_flags::DEFAULT,
        )
        .flatten()
        {
            t.set_height((hit.pos.z - 0.25).max(floor));
        }
        floor
    });
    // `SpawnGimmickEffect.Velocity*` / `AngVel*`: the C# `GimmickSpawner` constructor stored
    // them but never applied them, so effect-spawned gimmicks never moved.
    //
    // **Inferred (no 1.2 server evidence):** the frame of the shipped rows. They look authored in
    // the caster's local frame (forward +Y, right +X, up +Z: (0,25,6) and (0,4,0) are forward /
    // up launches), so they are rotated by the caster's world yaw. Howling Abyss projectiles
    // also rotate their launch offset. The Palace Cellar boulder starts on the landing above
    // its ramp, using the C# `AddDistance` translation on the world axes.
    //
    // Coordinate type 2 supplies the authored local velocity. Sorvell's barrage (type 1)
    // supplies world-axis spin. The doodad spawn path overrides both with its own world values.
    //
    // Kinematic gimmicks use the authored velocity directly. Armory barrels also use their
    // gravity and a floor probe. Other dynamic gimmicks need their own collision model. The angular
    // velocity is applied either way.
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let rotate_local = |v: [f32; 3]| {
        [
            v[0] * cos_yaw - v[1] * sin_yaw,
            v[0] * sin_yaw + v[1] * cos_yaw,
            v[2],
        ]
    };
    let mut authored_motion = doodad_motion;
    if params.velocity_coordinate_id == 2
        && (template.mass <= 0.0 || falling_barrel || uses_ballistic_motion(template.id))
    {
        g.vel = rotate_local([params.velocity_x, params.velocity_y, params.velocity_z]);
        if let Some(aim) = cannon_aim {
            let t = &g.unit.transform;
            let [dx, dy, dz] = [aim.x - t.x, aim.y - t.y, aim.z - t.z];
            let launch_speed = params.velocity_x.hypot(params.velocity_y);
            if launch_speed > 0.0 {
                // Solve the same constant-gravity motion used by the tick, using
                // the authored horizontal launch speed to set the flight time.
                let flight_time = (dx.hypot(dy).hypot(dz) / launch_speed).max(0.05);
                g.vel = [
                    dx / flight_time,
                    dy / flight_time,
                    dz / flight_time + 0.5 * template.gravity * flight_time,
                ];
            }
        }
        authored_motion = true;
    }
    if params.ang_vel_coordinate_id == 2 {
        g.ang_vel = rotate_local([params.ang_vel_x, params.ang_vel_y, params.ang_vel_z]);
        authored_motion = true;
    } else if falling_barrel && params.ang_vel_coordinate_id == 1 {
        g.ang_vel = [params.ang_vel_x, params.ang_vel_y, params.ang_vel_z];
        authored_motion = true;
    }
    // The character the caster acts for, resolved now while the spawner chain is alive (a bomb
    // that spawns fragments is gone before they are). 0 for a doodad / NPC caster.
    let owner_character_id = crate::systems::skills::relation::owner_character(server, caster)
        .map_or(0, |owner| owner.character_id);
    // A thrown trap whose model has a cylinder proxy (the Howling Abyss keg 49; the cannon
    // shells and Trieste bolts are prefabs without one, the boulder has its own sphere motion)
    // flies and rolls on its side with its surface, not its origin, on the floor.
    let rolling_keg = (authored_motion
        && uses_ballistic_motion(template.id)
        && template.id != PALACE_BOULDER_TEMPLATE_ID
        && !cannon_projectile)
        .then(|| cylinder_proxy(server, &template, params.scale))
        .flatten()
        .map(|proxy| {
            let keg = RollingKeg::new(proxy, yaw);
            keg.apply(&mut g.unit.transform);
            keg
        });
    // A falling barrel lands on the lowest point of its model proxy.
    let barrel_proxy = falling_barrel
        .then(|| cylinder_proxy(server, &template, params.scale))
        .flatten();
    let initial_pos = [g.unit.transform.x, g.unit.transform.y, g.unit.transform.z];
    let gimmick: GimmickRef = Entity::new(obj_id, g);
    let changes = world.spawn(&gimmick, &server.view_ctx());
    session::handle_zone_changes(server, changes);
    Some(new_active_gimmick(
        world.id,
        gimmick,
        spawner,
        Some(template),
        GimmickState {
            authored_motion,
            owner_character_id,
            barrel_floor,
            barrel_proxy,
            rolling_keg,
            last_pos: if cannon_projectile {
                // Sweep the muzzle offset on the first tick as well, so a
                // cannon held against a wall cannot start on its far side.
                [origin.x, origin.y, origin.z + params.offset_z]
            } else if uses_ballistic_motion(params.gimmick_id) {
                initial_pos
            } else {
                [0.0; 3]
            },
            ..Default::default()
        },
    ))
}

/// Doodad phase spawns use the doodad's world transform and the authored world offsets,
/// faction, linear velocity and angular velocity (WZGimmickSetMovement semantics).
pub fn spawn_from_doodad(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    source: &WorldObject,
    f: &DoodadFuncSpawnGimmick,
) -> Option<GimmickRef> {
    Some(spawn_doodad_gimmick(server, world, source, f)?.activate(server))
}

/// Spawn a transient caster for a scripted blast. The world needs the caster for targeting
/// and packets, but the gimmick tick must never see it as an active collision actor.
pub(crate) fn cast_from_doodad_gimmick(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    source: &WorldObject,
    f: &DoodadFuncSpawnGimmick,
    skill_id: u32,
) -> bool {
    let Some(active) = spawn_doodad_gimmick(server, world, source, f) else {
        return false;
    };
    let fired = do_gimmick_skill(server, world, &active, skill_id);
    despawn(server, &active);
    fired
}

fn spawn_doodad_gimmick(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    source: &WorldObject,
    f: &DoodadFuncSpawnGimmick,
) -> Option<ActiveGimmick> {
    let params = SpawnGimmickParams {
        gimmick_id: f.gimmick_id,
        offset_from_source: true,
        offset_coordinate_id: 2,
        offset_x: f.offset_x,
        offset_y: f.offset_y,
        offset_z: f.offset_z,
        scale: f.scale,
        ..Default::default()
    };
    let active = spawn_effect_gimmick(server, world, &params, source, true)?;
    {
        let mut g = active.gimmick.data();
        g.unit.faction_id = f.faction_id;
        g.vel = [f.velocity_x, f.velocity_y, f.velocity_z];
        g.ang_vel = [f.angle_x, f.angle_y, f.angle_z];
    }
    Some(active)
}

/// `GimmickSpawner.Despawn(gimmick)`: removes it from the manager and the world; releases its
/// ids unless a respawn is pending. A retained tick snapshot may still hold the handle.
pub fn despawn(server: &GameServer, active: &ActiveGimmick) {
    let release_ids = {
        let mut st = active.state();
        if st.despawned {
            return;
        }
        st.despawned = true;
        st.respawn_ms == 0
    };
    if let Some(gimmicks) = server.systems.npcs.gimmicks.existing(active.world_id) {
        gimmicks.remove(active.gimmick.obj_id);
    }
    if let Some(world) = server.world.world_if_exists(active.world_id) {
        world.delete(&active.gimmick);
    }
    if release_ids {
        if active.gimmick.obj_id > 0 {
            server.ids.object.release_id(active.gimmick.obj_id);
        }
        let gimmick_id = active.gimmick.data().gimmick_id;
        if gimmick_id > 0 {
            server.ids.gimmick.release_id(u32::from(gimmick_id));
        }
    }
}

/// Casts a skill at the gimmick's position and announces an accepted cast in system chat.
///
/// The 1.2 data can name the same skill for the fuse and for contact (e.g. the Triste bolts
/// and the collapsing ceiling). The cast claims that skill before invoking effects so the
/// contact path cannot fire it again. If contact fired first, it has already spent the fuse.
fn do_gimmick_skill(
    server: &GameServer,
    world: &WorldInstance,
    active: &ActiveGimmick,
    skill_id: u32,
) -> bool {
    if skill_id == 0 {
        return false;
    }
    {
        let mut st = active.state();
        if st.despawned
            || st.skill_started.is_some()
            || (active
                .template
                .as_ref()
                .is_some_and(|t| t.collision_skill_id == skill_id)
                && (st.collision_casting || !st.collision_hits.is_empty()))
        {
            return false;
        }
        st.skill_started = Some(skill_id);
    }
    let (spawner_unit_id, pos) = {
        let g = active.gimmick.data();
        (g.spawner_unit_id, g.unit.transform.local())
    };
    let caster = world.unit(spawner_unit_id);
    let me = WorldObject::from(&active.gimmick);
    let fired = server.systems.npcs.hooks.skills.get().is_some_and(|h| {
        h.use_gimmick_skill(
            server,
            caster.as_ref(),
            &me,
            skill_id,
            (pos.x, pos.y, pos.z),
        )
    });
    if !fired {
        active.state().skill_started = None;
        return false;
    }
    let msg = SCChatMessagePacket {
        chat_type: chat_type::SYSTEM,
        sender: None,
        message: format!("Gimmick {} used skill {skill_id}", active.gimmick.obj_id),
        ability: 0,
        language_type: 0,
    };
    world::broadcast(world, &me, &msg, false);
    true
}

/// `Gimmick.StopMovement()` (NPC AI gimmicks): uses the template skill when it has a target.
pub fn stop_movement(server: &GameServer, active: &ActiveGimmick) {
    if active.gimmick.data().unit.current_target == 0 {
        return;
    }
    if let Some(world) = server.world.world_if_exists(active.world_id) {
        let skill = active.template.as_ref().map_or(0, |t| t.skill_id);
        do_gimmick_skill(server, &world, active, skill);
    }
}

/// A contact candidate: a living, visible unit the collision skill may target. Gimmicks,
/// doodads, houses, shipyards and transfers are props / structures, not contact targets.
fn contact_unit(world: &WorldInstance, obj: &WorldObject) -> bool {
    if !matches!(
        obj,
        WorldObject::Player(_) | WorldObject::Npc(_) | WorldObject::Slave(_) | WorldObject::Mate(_)
    ) {
        return false;
    }
    obj.with_unit(|u| {
        let unit = u.unit();
        unit.transform.instance_id == world.id && unit.hp > 0 && !unit.invisible
    })
    .unwrap_or(false)
}

/// `GimmickTemplate.CollisionSkillId` contact check. The 1.2 client has no gimmick packet (only
/// SC 0x13c-0x141), so the server detects the touch here. Retail's zone process has a
/// `ZWGimmickCollided` packet (op 42); the evidence is its RTTI name plus
/// `rust/docs/physics/ZONE-PHYSICS-USAGE.md:264-266`, not a decompile.
///
/// **Inferred (no 1.2 server evidence):** that retail cast the collision skill from the gimmick;
/// the gate on the gimmick's *own* speed (retail's rule is on a *relative* velocity, `relVel >=
/// per-gimmick minimum`, so a static gimmick with a `collision_min_speed` above 0, such as the
/// mines 7, 36 and 77, never fires on touch here); the contact radius
/// ([`COLLISION_RADIUS`]); and the permanent per-unit latch below (retail: a 3000 ms immunity).
///
/// One touch fires one skill, never one per touched unit: most collision skills are area skills
/// (18717 r20, 14649 r30, ...) and firing them once per unit multiplied the explosion. A
/// firing latches every unit it covers (the skill's area radius around the gimmick plus every
/// touched unit), so a unit already hit by a firing never triggers another one. A
/// `disappear_by_collision` gimmick fires once in total and despawns.
///
/// The latch is the gimmick's collision immunity (who may fire it), not a damage immunity: the
/// area of a later firing, started by a unit outside the earlier one, hits every unit inside it,
/// a latched one included. Only the contact templates that do not `disappear_by_collision` can
/// fire twice. **Inferred (no 1.2 server evidence):** retail's 3000 ms immunity is a rule of the
/// collision, and nothing says whether the area effects of another touch skipped a unit in
/// immunity.
///
/// Only an accepted cast latches and despawns: a refused one (no resources, ...) leaves the
/// units unlatched and the gimmick alive, and the next unlatched contact or tick tries again. A
/// gimmick whose skill row does not exist is skipped (r208022 has no skill 14768, 18424, 23476
/// or 24080, which gimmicks 36 and 77, 51, 75 and 78 name), and a gimmick with no player around
/// is not scanned (an instance nobody looks at is dormant, like the NPC AI). Returns `true` when
/// the gimmick was despawned.
fn collision_tick(
    server: &GameServer,
    world: &WorldInstance,
    active: &ActiveGimmick,
    template: &GimmickTemplate,
) -> bool {
    // Never call into the world while the gimmick's data lock is held.
    let (origin, vel, source_id) = {
        let g = active.gimmick.data();
        let t = g.unit.transform;
        ([t.x, t.y, t.z], g.vel, g.spawner_unit_id)
    };
    let segment_start = if uses_ballistic_motion(template.id) {
        active.state().last_pos
    } else {
        origin
    };
    let segment = std::array::from_fn::<_, 3, _>(|i| origin[i] - segment_start[i]);
    let segment_len_sq = segment.iter().map(|v| v * v).sum::<f32>();
    let speed = (vel[0] * vel[0] + vel[1] * vel[1] + vel[2] * vel[2]).sqrt();
    if !speed.is_finite() || speed < template.collision_min_speed {
        return false;
    }
    let Some(skill) = server
        .game_data
        .systems
        .skills
        .skill(template.collision_skill_id)
    else {
        return false;
    };
    let area = skill.target_area_radius.max(0) as f32;
    let me = WorldObject::from(&active.gimmick);
    let candidates = world.around(&me, |o| o.is_unit().then(|| o.clone()));
    if !candidates.iter().any(|o| o.as_player().is_some()) {
        return false;
    }
    // Where the cast centres its area (`Skill.GetInitialTarget`): on the touched unit for a
    // unit-targeted skill (AnyUnit, Hostile, ...), on the gimmick itself for a position or self
    // skill. Which units a firing hits therefore depends on the unit that fired it.
    use crate::systems::skills::types::SkillTargetType as T;
    let unit_centred = matches!(
        T::from_raw(i64::from(skill.target_type)),
        Some(
            T::Friendly
                | T::Hostile
                | T::AnyUnit
                | T::Doodad
                | T::Others
                | T::Building
                | T::FriendlyOthers
        )
    );
    // Every contact-eligible unit around: its world position and model size.
    let mut seen: Vec<([f32; 3], f32, u32)> = Vec::new();
    let mut contacts: Vec<(u32, WorldObject)> = Vec::new();
    for unit in candidates {
        if !contact_unit(world, &unit)
            || (uses_ballistic_motion(template.id) && unit.obj_id() == source_id)
        {
            continue;
        }
        let p = world.world_pos_rot(&unit);
        let model = crate::systems::skills::geometry::model_size(
            server,
            &crate::systems::skills::geometry::Target::Obj(unit.clone()),
        );
        let from_start = [
            p.x - segment_start[0],
            p.y - segment_start[1],
            p.z - segment_start[2],
        ];
        let along = if segment_len_sq > 0.0 {
            (from_start
                .iter()
                .zip(segment)
                .map(|(a, b)| a * b)
                .sum::<f32>()
                / segment_len_sq)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let [dx, dy, dz] = std::array::from_fn(|i| from_start[i] - segment[i] * along);
        let reach = COLLISION_RADIUS + model;
        if dx * dx + dy * dy + dz * dz <= reach * reach {
            contacts.push((unit.obj_id(), unit.clone()));
        }
        seen.push(([p.x, p.y, p.z], model, unit.obj_id()));
    }
    // The unlatched contacts, in world order. A full latch ignores every further touch (the cap
    // is checked first, so an unlatchable unit cannot fire the skill on every tick).
    let Some(h) = server.systems.npcs.hooks.skills.get() else {
        return false;
    };
    let unlatched: Vec<&WorldObject> = {
        let mut st = active.state();
        if st.despawned
            || st.collision_casting
            || st.skill_started == Some(template.collision_skill_id)
            || st.collision_hits.len() >= MAX_COLLISION_HITS
        {
            return false;
        }
        let units: Vec<_> = contacts
            .iter()
            .filter(|(id, _)| !st.collision_hits.contains(id))
            .map(|(_, unit)| unit)
            .collect();
        if units.is_empty() {
            return false;
        }
        st.collision_casting = true;
        units
    };
    // No lock is held across the cast. The first contact whose cast is accepted is the firing.
    let mut fired: Option<&WorldObject> = None;
    for unit in unlatched {
        tracing::debug!(
            gimmick = me.obj_id(),
            unit = unit.obj_id(),
            skill_id = template.collision_skill_id,
            speed,
            "Gimmick collision: casting the collision skill"
        );
        if h.use_gimmick_collision_skill(server, &me, unit, template.collision_skill_id) {
            fired = Some(unit);
            break;
        }
    }
    let Some(fired) = fired else {
        active.state().collision_casting = false;
        return false;
    };
    if template.disappear_by_collision {
        despawn(server, active);
        return true;
    }
    {
        // Only a firing latches units, and only the units the firing reached: the touching units
        // and the units inside the skill's area around its centre (a unit merely nearby, or
        // inside a larger sphere, was not hit and must still fire when it later touches).
        let fired_pos = world.world_pos_rot(fired);
        let (centre, centre_model) = if unit_centred {
            let model = crate::systems::skills::geometry::model_size(
                server,
                &crate::systems::skills::geometry::Target::Obj(fired.clone()),
            );
            ([fired_pos.x, fired_pos.y, fired_pos.z], model)
        } else {
            let model = crate::systems::skills::geometry::model_size(
                server,
                &crate::systems::skills::geometry::Target::Obj(me.clone()),
            );
            (origin, model)
        };
        let reach = area + centre_model;
        let vertical =
            crate::systems::skills::geometry::area_vertical_limit(area, skill.target_valid_height);
        let in_area = |pos: &[f32; 3], model: f32| {
            let dx = pos[0] - centre[0];
            let dy = pos[1] - centre[1];
            area > 0.0
                && dx * dx + dy * dy < reach * reach + model * model
                && (pos[2] - centre[2]).abs() <= vertical
        };
        let covered = seen
            .iter()
            .filter(|(pos, model, _)| in_area(pos, *model))
            .map(|(_, _, id)| *id);
        let mut st = active.state();
        for id in contacts.iter().map(|(id, _)| *id).chain(covered) {
            if st.collision_hits.len() >= MAX_COLLISION_HITS {
                break;
            }
            if !st.collision_hits.contains(&id) {
                st.collision_hits.push(id);
            }
        }
        st.collision_casting = false;
    }
    false
}

/// Move a launched trap or projectile and return whether it struck scenery.
fn tick_ballistic_motion(
    server: &GameServer,
    world: &WorldInstance,
    active: &ActiveGimmick,
    template: &GimmickTemplate,
    dt: f32,
) -> bool {
    use crate::systems::ai::collision;
    let keg = active.state().rolling_keg;
    if let Some(keg) = keg {
        return tick_rolling_keg(server, world, active, template, keg, dt);
    }
    let (from, mut velocity) = {
        let g = active.gimmick.data();
        let t = g.unit.transform;
        ([t.x, t.y, t.z], g.vel)
    };
    if velocity == [0.0; 3] {
        return false;
    }
    let mut to = [
        from[0] + velocity[0] * dt,
        from[1] + velocity[1] * dt,
        from[2] + velocity[2] * dt - 0.5 * template.gravity * dt * dt,
    ];
    velocity[2] -= template.gravity * dt;
    let rolling = template.id == PALACE_BOULDER_TEMPLATE_ID;
    if rolling {
        // The boulder's centre rides above the corridor floor. Landing retains
        // the authored forward speed so the trap rolls down the passage.
        if let Some(floor) =
            collision::ground_height_probe(server, world.id, to[0], to[1], from[2], 0.25, 4.0)
                .flatten()
        {
            let support = floor + COLLISION_RADIUS;
            if to[2] <= support {
                to[2] = support;
                velocity[2] = 0.0;
            }
        }
    }
    let mut end = to;
    let hit = if is_portable_cannon_projectile(template.id) {
        use collision::world::ObjKind;
        let ray_start = active.state().last_pos;
        let shooter_id = active.gimmick.data().spawner_unit_id;
        let mut ignored = vec![(ObjKind::Unit, shooter_id)];
        if let Some(ship) = world
            .unit(shooter_id)
            .and_then(|shooter| world.sticky_parent(&shooter))
            .filter(|parent| parent.as_slave().is_some())
        {
            ignored.push((ObjKind::Ship, ship.obj_id()));
        }
        collision::raycast_ignoring_objects(
            server,
            world.id,
            ray_start,
            to,
            collision::query::mask::SOLID_TERRAIN | collision::query::mask::WATER,
            collision::query::ray_flags::SHIP_PROJECTILE,
            &ignored,
        )
        .flatten()
    } else {
        collision::raycast(
            server,
            world.id,
            from,
            to,
            collision::query::mask::STATIC_TERRAIN,
            collision::query::ray_flags::DEFAULT,
        )
        .flatten()
    };
    if let Some(hit) = hit {
        let normal = [hit.normal.x, hit.normal.y, hit.normal.z];
        end = [
            hit.pos.x + normal[0] * 0.03,
            hit.pos.y + normal[1] * 0.03,
            hit.pos.z + normal[2] * 0.03,
        ];
        if rolling {
            velocity = [0.0; 3];
        } else if !template.disappear_by_collision {
            let approach = velocity.iter().zip(normal).map(|(a, b)| a * b).sum::<f32>();
            for (v, n) in velocity.iter_mut().zip(normal) {
                *v = (*v - (1.0 + BARREL_RESTITUTION) * approach * n) * 0.6;
            }
            if normal[2] > 0.5 && velocity.iter().map(|v| v * v).sum::<f32>() < 1.0 {
                velocity = [0.0; 3];
            }
        }
    }
    let mut g = active.gimmick.data();
    g.unit.transform.set_xyz(end[0], end[1], end[2]);
    if rolling {
        g.ang_vel = [
            -velocity[1] / COLLISION_RADIUS,
            velocity[0] / COLLISION_RADIUS,
            0.0,
        ];
        g.unit.transform.rot_x -= (end[1] - from[1]) / COLLISION_RADIUS;
        g.unit.transform.rot_y += (end[0] - from[0]) / COLLISION_RADIUS;
    }
    g.vel = velocity;
    hit.is_some()
}

/// Moves a thrown keg and returns whether it struck scenery. The keg flies under its
/// template gravity with its surface, not its centre, meeting walls and floors: the centre
/// stays one proxy radius off them. Hard impacts bounce and lose speed like the other launched
/// traps. Once an impact is too slow to bounce, the keg rolls with [`KEG_ROLL_DECELERATION`],
/// turning about its axis by the distance it covers, until it stops.
fn tick_rolling_keg(
    server: &GameServer,
    world: &WorldInstance,
    active: &ActiveGimmick,
    template: &GimmickTemplate,
    mut keg: RollingKeg,
    dt: f32,
) -> bool {
    use crate::systems::ai::collision;
    let (from, mut v) = {
        let g = active.gimmick.data();
        let t = g.unit.transform;
        ([t.x, t.y, t.z], g.vel)
    };
    if v == [0.0; 3] {
        return false;
    }
    let r = keg.proxy.r;
    let mut to = [
        from[0] + v[0] * dt,
        from[1] + v[1] * dt,
        from[2] + v[2] * dt - 0.5 * template.gravity * dt * dt,
    ];
    v[2] -= template.gravity * dt;
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let mut struck = false;
    let step = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let len = dot(step, step).sqrt();
    if len > 1e-4 {
        let reach = std::array::from_fn(|i| to[i] + step[i] / len * r);
        let hit = collision::raycast(
            server,
            world.id,
            from,
            reach,
            collision::query::mask::STATIC_TERRAIN,
            collision::query::ray_flags::DEFAULT,
        )
        .flatten();
        if let Some(hit) = hit {
            struck = true;
            let n = [hit.normal.x, hit.normal.y, hit.normal.z];
            to = [
                hit.pos.x + n[0] * r,
                hit.pos.y + n[1] * r,
                hit.pos.z + n[2] * r,
            ];
            let approach = dot(v, n);
            if approach < 0.0 {
                if n[2] > 0.5 && -approach * BARREL_RESTITUTION < BARREL_REST_SPEED {
                    // A slow landing keeps the speed along the floor.
                    v = std::array::from_fn(|i| v[i] - approach * n[i]);
                } else {
                    v = std::array::from_fn(|i| {
                        (v[i] - (1.0 + BARREL_RESTITUTION) * approach * n[i]) * 0.6
                    });
                }
            }
        }
    }
    // Rest on the floor under the centre (a ray along the path misses a floor the keg only
    // grazes while it rolls).
    let floor =
        collision::ground_height_probe(server, world.id, to[0], to[1], to[2], 0.25, r + 1.0)
            .flatten();
    let mut on_floor = false;
    if let Some(floor) = floor {
        let support = floor + keg.clearance();
        if to[2] <= support + 0.02 {
            to[2] = to[2].max(support);
            if v[2] < 0.0 {
                let rebound = -v[2] * BARREL_RESTITUTION;
                if rebound >= BARREL_REST_SPEED {
                    struck = true;
                    v = [v[0] * 0.6, v[1] * 0.6, rebound];
                } else {
                    v[2] = 0.0;
                }
            }
            on_floor = v[2] <= 0.0;
        }
    }
    let speed = v[0].hypot(v[1]);
    if speed > 0.05 {
        keg.heading = (-v[0]).atan2(v[1]);
    }
    if on_floor {
        let rolled = (speed - KEG_ROLL_DECELERATION * dt).max(0.0);
        if rolled < 0.05 {
            v = [0.0; 3];
            keg.spin_rate = 0.0;
        } else {
            let k = rolled / speed;
            v = [v[0] * k, v[1] * k, 0.0];
            keg.spin_rate = rolled / r.max(0.01);
        }
    }
    keg.spin = (keg.spin + keg.spin_rate * dt + std::f32::consts::PI)
        .rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI;
    {
        let mut g = active.gimmick.data();
        g.unit.transform.set_xyz(to[0], to[1], to[2]);
        keg.apply(&mut g.unit.transform);
        g.vel = v;
        g.ang_vel = keg.ang_vel();
    }
    active.state().rolling_keg = Some(keg);
    struck
}

/// `Gimmick.GimmickTick(delta)`.
pub fn gimmick_tick(
    server: &GameServer,
    world: &WorldInstance,
    active: &ActiveGimmick,
    delta: Duration,
    now_ms: i64,
) {
    let delta_time = delta.as_secs_f32();
    // A gimmick acts for the character that threw it (`relation::owner_character`). Once that
    // character has left the world nobody answers for what the gimmick hits and the rules keyed
    // on the owner (Peace, no-PvP, siege hostility, tagging, crime evidence) no longer apply to
    // it, so it goes with its thrower instead of fighting on ownerless (a disconnect must not
    // lift the protection of a player a bomb is about to reach).
    let owner = {
        let st = active.state();
        if st.despawned {
            return;
        }
        st.owner_character_id
    };
    if owner != 0 && server.world.character_by_id(owner).is_none() {
        despawn(server, active);
        return;
    }
    // LastLifeTime / TotalLifeTime, TimeLeft
    let last_life_time = {
        let mut st = active.state();
        let last = st.total_life_time;
        st.total_life_time += delta;
        if st.time_left(now_ms) > 0 {
            return;
        }
        last
    };

    let mut scenery_hit = false;
    // MovementHandler?.Tick(delta)
    match active.handler {
        Some(MovementHandler::Elevator) => {
            let mut st = active.state();
            let (z, vel) = {
                let g = active.gimmick.data();
                (g.unit.transform.z, g.vel)
            };
            let mut m = MovementState {
                z,
                vel,
                is_moving: st.is_moving,
                move_down: st.move_down,
            };
            movement::elevator_tick(&mut m, &active.spawner, delta_time);
            st.is_moving = m.is_moving;
            st.move_down = m.move_down;
            let mut g = active.gimmick.data();
            g.unit.transform.set_height(m.z);
            g.vel = m.vel;
        }
        Some(MovementHandler::FloatToSurface) => {
            // World.Water.IsWater(World.Position + (0, 0, movement + 1)) (systems::ai water)
            let p = world.world_pos_rot(&crate::world::WorldObject::from(active.gimmick.clone()));
            let in_water = |h: f32| {
                let check = crate::systems::skills::controllers::Vec3::new(p.x, p.y, p.z + h);
                crate::systems::ai::physics::is_water(server, world.id, check).is_some()
            };
            if let Some(dz) = movement::float_to_surface_tick(delta_time, in_water) {
                let mut g = active.gimmick.data();
                let z = g.unit.transform.z;
                g.unit.transform.set_height(z + dz);
            }
        }
        None if active
            .template
            .as_ref()
            .is_some_and(|t| uses_ballistic_motion(t.id))
            && active.state().authored_motion =>
        {
            scenery_hit = tick_ballistic_motion(
                server,
                world,
                active,
                active.template.as_ref().expect("checked template"),
                delta_time,
            );
        }
        None => {
            if active.state().authored_motion {
                let mut st = active.state();
                let capped = st.authored_time >= RUNAWAY_CAP_TIME
                    || st.authored_distance >= RUNAWAY_CAP_DISTANCE;
                let mut g = active.gimmick.data();
                let mut vel = g.vel;
                if capped {
                    // Stop the linear motion; the angular velocity keeps a mine spinning in
                    // place, which is all a capped gimmick may still do.
                    vel = [0.0; 3];
                    g.vel = vel;
                }
                let mut next_z = g.unit.transform.z + vel[2] * delta_time;
                let mut rot = {
                    let (t, ang) = (&g.unit.transform, g.ang_vel);
                    [
                        t.rot_x + ang[0] * delta_time,
                        t.rot_y + ang[1] * delta_time,
                        t.rot_z + ang[2] * delta_time,
                    ]
                };
                if let Some(floor) = st.barrel_floor {
                    let gravity = active.template.as_ref().map_or(0.0, |t| t.gravity);
                    next_z -= 0.5 * gravity * delta_time * delta_time;
                    g.vel[2] -= gravity * delta_time;
                    // The tumbling proxy's lowest point meets the floor, not the origin
                    // (`barrel_oak_b` stands on its origin, so on its side or upside down the
                    // origin is up to one barrel height above the floor).
                    let clearance = |rot| st.barrel_proxy.map_or(0.0, |p| p.floor_clearance(rot));
                    let support = floor + clearance(rot);
                    if next_z <= support {
                        next_z = support;
                        let rebound = -g.vel[2] * BARREL_RESTITUTION;
                        if rebound >= BARREL_REST_SPEED {
                            g.vel[2] = rebound;
                            g.ang_vel = g.ang_vel.map(|w| w * 0.5);
                        } else {
                            // Settle on an end or on the side, whichever is nearer.
                            if let Some(proxy) = st.barrel_proxy {
                                rot = proxy.rest_pose(rot);
                                next_z = floor + proxy.floor_clearance(rot);
                            }
                            st.barrel_floor = None;
                            vel = [0.0; 3];
                            g.vel = vel;
                            g.ang_vel = [0.0; 3];
                        }
                    }
                }
                let speed = (vel[0] * vel[0] + vel[1] * vel[1] + vel[2] * vel[2]).sqrt();
                st.authored_time += delta;
                st.authored_distance += speed * delta_time;
                let t = &mut g.unit.transform;
                t.x += vel[0] * delta_time;
                t.y += vel[1] * delta_time;
                t.z = next_z;
                [t.rot_x, t.rot_y, t.rot_z] = rot;
            }
        }
    }

    // Keep the grid region current: `around`, visibility and the movement broadcast all use it
    // (same update as a moving NPC, `WorldManager.AddVisibleObject`).
    crate::systems::ai::npc::check_moved_position(server, &WorldObject::from(&active.gimmick));
    // A gimmick that leaves the world grid is gone for the client too; an effect / doodad spawn
    // has no spawner to bring it back, so drop it instead of ticking a phantom.
    if active.state().authored_motion {
        let (x, y) = {
            let g = active.gimmick.data();
            (g.unit.transform.x, g.unit.transform.y)
        };
        if world.region_by_pos(x, y).is_none() {
            despawn(server, active);
            return;
        }
    }

    // Handle Delayed Skills
    if let Some(t) = &active.template {
        let total = active.state().total_life_time;
        if t.skill_delay > 0
            && last_life_time.as_secs_f64() * 1000.0 < f64::from(t.skill_delay)
            && total.as_secs_f64() * 1000.0 >= f64::from(t.skill_delay)
        {
            let fired = do_gimmick_skill(server, world, active, t.skill_id);
            // Glider Bomb and Nuclear Bomb have no lifetime. Once their fuse spends the
            // collision skill, remove them here because no later hit can remove them.
            if fired
                && t.disappear_by_collision
                && t.skill_id == t.collision_skill_id
                && t.life_time == 0
            {
                despawn(server, active);
                return;
            }
        }
    }
    // Contact hits (`GimmickTemplate.CollisionSkillId`): after the movement of this tick.
    if let Some(t) = &active.template {
        if t.collision_skill_id != 0 && collision_tick(server, world, active, t) {
            return; // despawned by the collision
        }
        if scenery_hit && t.disappear_by_collision {
            do_gimmick_skill(server, world, active, t.collision_skill_id);
            despawn(server, active);
            return;
        }
    }

    let time = time_of_day_ms(now_ms);
    let authored_motion = active.state().authored_motion;
    let (packet, pos, rot, vel) = {
        let last_pos = active.state().last_pos;
        let mut g = active.gimmick.data();
        let t = g.unit.transform;
        let pos = [t.x, t.y, t.z];
        let delta_position = [
            pos[0] - last_pos[0],
            pos[1] - last_pos[1],
            pos[2] - last_pos[2],
        ];
        // C#: `Vel = deltaPosition * deltaTime` (multiplied, not divided)
        if !authored_motion {
            g.vel = [
                delta_position[0] * delta_time,
                delta_position[1] * delta_time,
                delta_position[2] * delta_time,
            ];
            g.ang_vel = [0.0; 3];
        }
        (
            SCGimmickMovementPacket::new(active.gimmick.obj_id, time, &g),
            pos,
            [t.rot_x, t.rot_y, t.rot_z],
            g.vel,
        )
    };
    let send = {
        let mut st = active.state();
        st.time = time;
        let moved = pos != st.last_pos;
        let turned = rot != st.last_rot;
        // A gimmick that stops (a travel cap zeroes the velocity, an elevator ends its run) keeps
        // its position: without the velocity change no packet would tell the clients to stop.
        let stopped_or_started = vel != st.last_vel;
        st.last_pos = pos;
        st.last_rot = rot;
        st.last_vel = vel;
        let send = !st.broadcast_sent
            || moved
            || stopped_or_started
            || (turned && now_ms.saturating_sub(st.last_broadcast_ms) >= SPIN_BROADCAST_MS);
        if send {
            st.last_broadcast_ms = now_ms;
            st.broadcast_sent = true;
        }
        send
    };
    if send {
        world::broadcast(world, &active.gimmick, &packet, false);
    }
    {
        let mut st = active.state();
        // MovementHandler?.AfterMove(delta, deltaPosition)
        if active.handler == Some(MovementHandler::Elevator) {
            let vel = active.gimmick.data().vel;
            let mut m = MovementState {
                z: pos[2],
                vel,
                is_moving: st.is_moving,
                move_down: st.move_down,
            };
            if let Some(w) = movement::elevator_after_move(&mut m, pos[2], &active.spawner, now_ms)
            {
                st.wait_time_ms = w;
            }
            st.move_down = m.move_down;
        }
    }

    // Check LifeTime and apply despawn time if needed
    if let Some(t) = &active.template {
        let despawn_now = {
            let st = active.state();
            t.life_time > 0 && st.total_life_time.as_secs_f64() * 1000.0 >= f64::from(t.life_time)
        };
        if despawn_now {
            despawn(server, active);
        }
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
mod tests {
    use super::*;
    use crate::test_support::test_server;

    fn elevator_spawner() -> GimmickSpawner {
        GimmickSpawner {
            position: Some(crate::data::world_templates::WorldSpawnPosition {
                world_id: 1,
                zone_id: 0,
                x: 100.0,
                y: 200.0,
                z: 10.0,
                ..Default::default()
            }),
            top_z: 11.0,
            bottom_z: 10.0,
            wait_time: 1.0,
            scale: 1.0,
            rotation_w: 1.0,
            entity_guid: 0x4a0d_a1f4_d842_0a38,
            ..GimmickSpawner::default()
        }
    }

    #[tokio::test]
    async fn doodad_gimmick_uses_authored_offset_faction_and_motion() {
        let mut data = crate::data::GameData::empty();
        data.systems.npcs.gimmick.templates.insert(
            48,
            GimmickTemplate {
                id: 48,
                life_time: 1000,
                ..Default::default()
            },
        );
        let server = crate::test_support::test_server_with(data);
        let world = test_world(&server);
        let mut source = crate::models::doodad::Doodad::new(100);
        source.transform.set_xyz(100.0, 200.0, 10.0);
        let source = Entity::new(0x500, source);
        let g = spawn_from_doodad(
            &server,
            &world,
            &WorldObject::from(&source),
            &crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick {
                gimmick_id: 48,
                faction_id: 115,
                scale: 0.4,
                offset_z: -2.0,
                velocity_z: 5.0,
                angle_x: 3.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(g.data().unit.transform.z, 8.0);
        assert_eq!(g.data().unit.faction_id, 115);
        assert_eq!(g.data().unit.scale, 0.4);
        let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        gimmick_tick(&server, &world, &active, Duration::from_millis(100), 1000);
        assert_eq!(g.data().unit.transform.z, 8.5);
        assert!((g.data().unit.transform.rot_x - 0.3).abs() < 1e-6);
        assert_eq!(g.data().vel, [0.0, 0.0, 5.0]);
        gimmick_tick(&server, &world, &active, Duration::from_millis(900), 1900);
        assert!(world.gimmick(g.obj_id).is_none());
    }

    fn test_world(server: &GameServer) -> Arc<WorldInstance> {
        match server.world.worlds().into_iter().next() {
            Some(w) => w,
            None => {
                let t = Arc::new(crate::data::world_templates::WorldTemplate::synthetic(
                    0,
                    "main_world",
                    4,
                    4,
                    100,
                ));
                server.world.create_world_instance(t, 0)
            }
        }
    }

    fn cannon_fixture() -> (GameServer, Arc<WorldInstance>, WorldObject) {
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

        let mut data = crate::data::GameData::empty();
        let mut impact = crate::systems::skills::tests::skill_template(17741);
        impact.target_type = 6; // Pos
        data.systems.skills.add_skill(impact);
        data.systems.npcs.gimmick.templates.insert(
            45,
            GimmickTemplate {
                id: 45,
                life_time: 5000,
                gravity: 9.8,
                collision_skill_id: 17741,
                disappear_by_collision: true,
                ..Default::default()
            },
        );
        let server = test_server_with(data);
        crate::systems::skills::bridges::install(&server);
        crate::systems::ai::collision::flow_tests::enable_collision(&server, 90.0);
        let world = test_world(&server);
        let client = TestClient::new(&server, 1, 1);
        let mut player = test_character(1, "Gunner", 100.0, 100.0);
        player.unit.transform.set_height(102.0);
        let shooter = WorldObject::from(spawn_player(&server, &client, player, 0x500));
        (server, world, shooter)
    }

    fn fire_cannon(
        server: &GameServer,
        world: &Arc<WorldInstance>,
        shooter: &WorldObject,
        vertical_speed: f32,
    ) -> Arc<ActiveGimmick> {
        let shot = spawn_from_effect(
            server,
            world,
            &SpawnGimmickParams {
                gimmick_id: 45,
                offset_coordinate_id: 2,
                offset_y: 3.0,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_y: 20.0,
                velocity_z: vertical_speed,
                ..Default::default()
            },
            shooter,
        )
        .unwrap();
        server.systems.npcs.gimmicks.find(shot.obj_id).unwrap()
    }

    #[tokio::test]
    async fn portable_cannon_hits_a_wall_inside_the_muzzle_offset() {
        use aaemu_physics::math::Vec3;

        let (server, world, shooter) = cannon_fixture();
        crate::systems::ai::collision::flow_tests::add_wall_boxes(
            &server,
            &[(0x600, Vec3::new(100.0, 101.5, 102.0), 0.25)],
        );
        let shot = fire_cannon(&server, &world, &shooter, 0.0);
        assert_eq!(shot.gimmick.data().unit.transform.y, 103.0);

        gimmick_tick(&server, &world, &shot, Duration::from_millis(50), 1000);

        assert_eq!(shot.state().skill_started, Some(17741));
        assert!(world.gimmick(shot.gimmick.obj_id).is_none());
        let y = shot.gimmick.data().unit.transform.y;
        assert!((y - 101.22).abs() < 0.05, "impact y={y}");
    }

    #[tokio::test]
    async fn portable_cannon_detonates_at_the_water_surface() {
        let (server, world, shooter) = cannon_fixture();
        let shot = fire_cannon(&server, &world, &shooter, -20.0);
        gimmick_tick(&server, &world, &shot, Duration::from_millis(150), 1000);

        assert_eq!(shot.state().skill_started, Some(17741));
        assert!(world.gimmick(shot.gimmick.obj_id).is_none());
        let z = shot.gimmick.data().unit.transform.z;
        assert!((z - 100.03).abs() < 0.05, "impact z={z}");
    }

    #[tokio::test]
    async fn portable_cannon_passes_its_own_hull_and_hits_another_ship() {
        use crate::systems::ai::collision::{
            phys_world,
            world::{ObjKind, Owner},
        };
        use aaemu_physics::entities::base::add_geometry;
        use aaemu_physics::geometry::primitives::{BoxPrim, Primitive};
        use aaemu_physics::math::Vec3;
        use aaemu_physics::params::{GeomParams, ParamsPos};

        let (server, world, shooter) = cannon_fixture();
        let mut own_ship = crate::models::slave::Slave::new(1, 24);
        own_ship.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let own_ship = WorldObject::from(Entity::new(0x600, own_ship));
        world.spawn(&own_ship, &server.view_ctx());
        world.set_sticky_parent(&shooter, Some(&own_ship));
        {
            let hulls = phys_world(&server, world.id).unwrap();
            let mut state = hulls.lock();
            let geom = state
                .world
                .geometry(&Primitive::Box(BoxPrim::aligned(
                    Vec3::ZERO,
                    Vec3::splat(0.5),
                )))
                .unwrap();
            for (obj_id, y) in [(own_ship.obj_id(), 101.5), (0x601, 104.0)] {
                let pose = ParamsPos {
                    pos: Vec3::new(100.0, y, 102.0),
                    ..Default::default()
                };
                let hull = state
                    .world
                    .create_physical_entity(2, 0.0, Some(&pose), 0, 0, -1)
                    .unwrap();
                let params = GeomParams {
                    mass: 10.0,
                    flags: 0x80,
                    flags_collider: 0x80,
                    ..Default::default()
                };
                add_geometry(&mut state.world, hull, &geom, &params, -1);
                // Ship physics registers hulls by owner outside the normal object cache.
                state.owners.insert(
                    hull,
                    Owner::Object {
                        kind: ObjKind::Ship,
                        obj_id,
                    },
                );
            }
        }
        let shot = fire_cannon(&server, &world, &shooter, 0.0);
        gimmick_tick(&server, &world, &shot, Duration::from_millis(100), 1000);

        assert_eq!(shot.state().skill_started, Some(17741));
        assert!(world.gimmick(shot.gimmick.obj_id).is_none());
        let y = shot.gimmick.data().unit.transform.y;
        assert!((y - 103.47).abs() < 0.05, "impact y={y}");
    }

    #[tokio::test]
    async fn elevator_spawns_ticks_and_despawns() {
        let server = test_server();
        let world = test_world(&server);
        let g = spawn_from_spawner(&server, &world, &elevator_spawner()).expect("spawned");
        assert!(world.gimmick(g.obj_id).is_some());
        {
            let d = g.data();
            assert_eq!(d.unit.template_id, 0);
            assert_eq!(d.entity_guid, 0x4a0d_a1f4_d842_0a38);
            assert_eq!(d.unit.scale, 1.0);
            assert_eq!(d.unit.transform.instance_id, world.id);
        }
        let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        assert_eq!(active.handler, Some(MovementHandler::Elevator));
        // 1 m up at 4.5 m/s, 0.05 s ticks: 4.444 -> 5 ticks (4 steps + snap), then waits 1 s.
        let dt = Duration::from_millis(50);
        for i in 0..5 {
            gimmick_tick(&server, &world, &active, dt, 1_000 + i * 50);
        }
        assert_eq!(g.data().unit.transform.z, 11.0);
        let wait = active.state().wait_time_ms;
        assert_eq!(wait, 1_200 + 1_000);
        assert!(active.state().move_down);
        // waiting: no movement
        gimmick_tick(&server, &world, &active, dt, 1_300);
        assert_eq!(g.data().unit.transform.z, 11.0);
        // after the wait it goes down
        gimmick_tick(&server, &world, &active, dt, 2_300);
        assert!(g.data().unit.transform.z < 11.0);
        despawn(&server, &active);
        assert!(world.gimmick(g.obj_id).is_none());
        assert!(server.systems.npcs.gimmicks.find(g.obj_id).is_none());
    }

    // ----- contact hits (`GimmickTemplate.CollisionSkillId`) -----

    const CONTACT_GIMMICK: u32 = 900;
    const CONTACT_SKILL: u32 = 900_100;
    const TIMER_SKILL: u32 = 900_102;

    /// Game data with one contact gimmick template and the fixed 100-damage skill it fires
    /// (`AnyUnit` target, no area, `max_range` 0 like the shipped collision skills).
    fn contact_data(template: GimmickTemplate, area_radius: i32) -> crate::data::GameData {
        contact_data_with(
            template,
            area_radius,
            crate::systems::skills::tests::damage_effect(1, 100),
            |_| {},
        )
    }

    /// [`contact_data`] with another damage row and a change to the collision skill (a cooldown,
    /// a target rule, ...) applied last.
    fn contact_data_with(
        template: GimmickTemplate,
        area_radius: i32,
        damage: crate::systems::skills::data::effects::DamageEffect,
        tweak: impl FnOnce(&mut crate::systems::skills::data::SkillTemplate),
    ) -> crate::data::GameData {
        use crate::systems::skills::data::{EffectKind, EffectTemplate};
        use crate::systems::skills::tests::{skill_effect, skill_template};
        let mut data = crate::data::GameData::empty();
        let s = &mut data.systems.skills;
        s.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(damage)),
            900_001,
        );
        let mut skill = skill_template(CONTACT_SKILL);
        skill.target_type = 5; // AnyUnit
        skill.target_relation = 0; // Any
        skill.target_selection = 1; // Source: the area is centered on the first touched unit
        skill.target_area_radius = area_radius;
        skill.max_range = 0;
        // The shipped collision skills' effects start at level 1; a level-0 gimmick caster
        // would skip them all.
        let mut effect = skill_effect(900_001, EffectKind::DamageEffect, 1);
        effect.start_level = 1;
        effect.end_level = 99;
        effect.non_friendly = true; // the shipped collision effects are hostile-only
        skill.effects = vec![effect];
        tweak(&mut skill);
        s.add_skill(skill);
        data.systems
            .npcs
            .gimmick
            .templates
            .insert(template.id, template);
        data
    }

    /// [`contact_data`] plus a position-targeted timer skill (900_102) for the delayed-skill
    /// path (`GimmickTemplate.skill_id` / `skill_delay`).
    fn timer_data(template: GimmickTemplate) -> crate::data::GameData {
        use crate::systems::skills::data::{EffectKind, EffectTemplate};
        use crate::systems::skills::tests::{damage_effect, skill_effect, skill_template};
        let mut data = contact_data(template, 0);
        let s = &mut data.systems.skills;
        s.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(damage_effect(2, 100))),
            900_002,
        );
        let mut skill = skill_template(TIMER_SKILL);
        skill.target_type = 6; // Pos
        skill.target_selection = 1; // Source
        skill.target_relation = 0; // Any
        skill.target_area_radius = 3;
        skill.max_range = 5;
        let mut effect = skill_effect(900_002, EffectKind::DamageEffect, 2);
        effect.start_level = 1;
        effect.end_level = 99;
        skill.effects = vec![effect];
        s.add_skill(skill);
        data
    }

    struct ContactFixture {
        server: GameServer,
        world: Arc<WorldInstance>,
        client: crate::test_support::TestClient,
        victim: crate::world::PlayerRef,
    }

    fn contact_fixture(template: GimmickTemplate, victim_at: (f32, f32)) -> ContactFixture {
        contact_fixture_with_area(template, victim_at, 0)
    }

    fn contact_fixture_with_area(
        template: GimmickTemplate,
        victim_at: (f32, f32),
        area_radius: i32,
    ) -> ContactFixture {
        contact_fixture_with_data(contact_data(template, area_radius), victim_at)
    }

    fn contact_fixture_with_data(
        data: crate::data::GameData,
        victim_at: (f32, f32),
    ) -> ContactFixture {
        let server = crate::test_support::test_server_with(data);
        // The fixture ids (player 0x100, doodad 0x300) are assigned by hand: take the object
        // ids below them first so a spawned gimmick never gets the same id.
        while server.ids.object.get_next_id().expect("object id") < 0x400 {}
        crate::systems::skills::bridges::install(&server);
        let world = test_world(&server);
        let client = crate::test_support::TestClient::new(&server, 1, 1);
        let mut c = crate::test_support::test_character(1, "Victim", victim_at.0, victim_at.1);
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        let victim = crate::test_support::spawn_player(&server, &client, c, 0x100);
        ContactFixture {
            server,
            world,
            client,
            victim,
        }
    }

    fn contact_template(disappear_by_collision: bool) -> GimmickTemplate {
        GimmickTemplate {
            id: CONTACT_GIMMICK,
            collision_skill_id: CONTACT_SKILL,
            collision_min_speed: 1.0,
            disappear_by_collision,
            ..Default::default()
        }
    }

    /// A doodad source: a prop at `at`, so it is not itself a contact target.
    fn doodad_at(server: &GameServer, world: &WorldInstance, at: (f32, f32, f32)) -> WorldObject {
        let mut d = crate::models::doodad::Doodad::new(1);
        d.transform.set_xyz(at.0, at.1, at.2);
        let d = Entity::new(0x300, d);
        world.spawn(&d, &server.view_ctx());
        WorldObject::from(&d)
    }

    fn spawn_contact_gimmick(
        f: &ContactFixture,
        source: &WorldObject,
        velocity: [f32; 3],
    ) -> (GimmickRef, Arc<ActiveGimmick>) {
        let g = spawn_from_effect(
            &f.server,
            &f.world,
            &SpawnGimmickParams {
                gimmick_id: CONTACT_GIMMICK,
                offset_coordinate_id: 2,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_x: velocity[0],
                velocity_y: velocity[1],
                velocity_z: velocity[2],
                ..Default::default()
            },
            source,
        )
        .expect("gimmick spawned");
        let active = f
            .server
            .systems
            .npcs
            .gimmicks
            .find(g.obj_id)
            .expect("active gimmick");
        (g, active)
    }

    #[tokio::test]
    async fn a_shared_timer_and_contact_skill_spends_the_fuse_once() {
        // A disappearing projectile can reach a unit on its fuse tick. A persistent
        // gimmick can also hit first and still be present when its fuse comes due.
        for (disappear_by_collision, life_time, skill_delay) in
            [(true, 0, 50), (true, 1_000, 50), (false, 0, 100)]
        {
            let template = GimmickTemplate {
                collision_skill_id: TIMER_SKILL,
                skill_id: TIMER_SKILL,
                skill_delay,
                life_time,
                ..contact_template(disappear_by_collision)
            };
            let f = contact_fixture_with_data(timer_data(template), (100.0, 100.0));
            let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
            let (_, active) = spawn_contact_gimmick(&f, &source, [2.0, 0.0, 0.0]);

            for now in [1_000, 1_050] {
                gimmick_tick(&f.server, &f.world, &active, Duration::from_millis(50), now);
            }

            assert_eq!(f.victim.data().unit.hp, 400, "fuse at {skill_delay} ms");
            assert_eq!(
                active.state().despawned,
                disappear_by_collision && life_time == 0
            );
        }
    }

    #[tokio::test]
    async fn contact_gimmick_hits_a_unit_in_its_path_once() {
        let f = contact_fixture(contact_template(false), (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        let dt = Duration::from_millis(50);
        gimmick_tick(&f.server, &f.world, &active, dt, 1_000);
        assert_eq!(
            f.victim.data().unit.hp,
            400,
            "the touched unit takes the hit"
        );
        assert!((g.data().unit.transform.x - 100.25).abs() < 1e-3);
        // The same gimmick never hits the same unit again, however long it stays in reach.
        gimmick_tick(&f.server, &f.world, &active, dt, 1_050);
        gimmick_tick(&f.server, &f.world, &active, dt, 1_100);
        assert_eq!(f.victim.data().unit.hp, 400);
        // `disappear_by_collision = false`: it keeps rolling.
        assert!(f.world.gimmick(g.obj_id).is_some());
    }

    #[tokio::test]
    async fn effect_velocity_is_authored_in_the_casters_local_frame() {
        let f = contact_fixture(contact_template(false), (300.0, 300.0));
        // A caster facing east: its local +Y (forward) is world +X.
        let mut d = crate::models::doodad::Doodad::new(1);
        d.transform.set_xyz(100.0, 100.0, 100.0);
        d.transform.rot_z = -std::f32::consts::FRAC_PI_2;
        let source = Entity::new(0x300, d);
        f.world.spawn(&source, &f.server.view_ctx());
        let g = spawn_from_effect(
            &f.server,
            &f.world,
            &SpawnGimmickParams {
                gimmick_id: CONTACT_GIMMICK,
                offset_coordinate_id: 2,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_x: 0.0,
                velocity_y: 5.0,
                velocity_z: 1.0,
                ang_vel_coordinate_id: 2,
                ang_vel_x: 1.0,
                ang_vel_y: 0.0,
                ang_vel_z: 2.0,
                ..Default::default()
            },
            &WorldObject::from(&source),
        )
        .expect("gimmick spawned");
        {
            let data = g.data();
            assert!((data.vel[0] - 5.0).abs() < 1e-4, "vel {:?}", data.vel);
            assert!(data.vel[1].abs() < 1e-4, "vel {:?}", data.vel);
            assert!((data.vel[2] - 1.0).abs() < 1e-4, "vel {:?}", data.vel);
            // Local +X (right) rotates to world -Y.
            assert!(data.ang_vel[0].abs() < 1e-4, "angVel {:?}", data.ang_vel);
            assert!(
                (data.ang_vel[1] + 1.0).abs() < 1e-4,
                "angVel {:?}",
                data.ang_vel
            );
            assert!(
                (data.ang_vel[2] - 2.0).abs() < 1e-4,
                "angVel {:?}",
                data.ang_vel
            );
        }
        let active = f.server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        let t = g.data().unit.transform;
        assert!(t.x > 100.0, "launched east: x={}", t.x);
        assert!(
            (t.y - 100.0).abs() < 1e-4,
            "no north/south drift: y={}",
            t.y
        );
    }

    /// The world direction of a proxy's cylinder axis under a transform's rotation.
    fn world_axis(t: &crate::models::transform::Transform, proxy: &CylinderProxy) -> [f32; 3] {
        t.local().to_quaternion().rotate(proxy.axis)
    }

    #[tokio::test]
    async fn armory_barrage_barrel_falls_to_the_targets_floor() {
        let f = contact_fixture(
            GimmickTemplate {
                id: SORVELL_BARREL_TEMPLATE_ID,
                model_path: r"Objects\Env\01_nuia\001_ndeco\old\barrel_oak_b.cgf".into(),
                mass: 0.7,
                gravity: 9.8,
                life_time: 6_000,
                ..Default::default()
            },
            (140.0, 120.0),
        );
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 20.0));
        // `spawn_gimmick_effects` 105: a world-offset drop that tumbles about all three axes.
        let barrel = spawn_from_effect(
            &f.server,
            &f.world,
            &SpawnGimmickParams {
                gimmick_id: SORVELL_BARREL_TEMPLATE_ID,
                target_position: Some(PositionAndRotation::new(140.0, 120.0, 21.0, 0.0, 0.0, 0.0)),
                offset_coordinate_id: 1,
                offset_x: 2.0,
                offset_y: 2.0,
                offset_z: 20.0,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_z: -5.0,
                ang_vel_coordinate_id: 1,
                ang_vel_x: 5.0,
                ang_vel_y: 10.0,
                ang_vel_z: 5.0,
                ..Default::default()
            },
            &source,
        )
        .unwrap();
        let start = barrel.data().unit.transform;
        assert_eq!((start.x, start.y, start.z), (142.0, 122.0, 41.0));
        let active = f.server.systems.npcs.gimmicks.find(barrel.obj_id).unwrap();
        // No client files: the barrel_oak_b proxy, standing on the model origin.
        let proxy = OAK_BARREL_PROXY_CYLINDER;
        let mut bounced = false;
        let mut contact_clearance: f32 = 0.0;
        let mut was_falling = false;
        for timestep in 1..=100 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                timestep * 50,
            );
            let data = barrel.data();
            let t = data.unit.transform;
            let lowest = t.z - proxy.floor_clearance([t.rot_x, t.rot_y, t.rot_z]);
            assert!(
                lowest >= 21.0 - 1e-3,
                "the barrel's lowest point stays on the floor: {lowest} at {t:?}"
            );
            if was_falling && data.vel[2] > 0.0 {
                // A bounce: the tilted proxy touched the floor with the origin this high.
                bounced = true;
                contact_clearance = contact_clearance.max(t.z - 21.0);
            }
            was_falling = data.vel[2] < 0.0;
            assert_eq!((t.x, t.y), (142.0, 122.0), "a vertical drop does not glide");
        }
        assert!(bounced, "the first impact rebounds");
        assert!(
            contact_clearance > 0.1,
            "the tumbling barrel touches down on its rim or side, origin {contact_clearance} up"
        );
        let data = barrel.data();
        let t = data.unit.transform;
        assert_eq!(data.vel, [0.0; 3]);
        assert_eq!(data.ang_vel, [0.0; 3]);
        // At rest on an end or on its side, with that face on the floor.
        let axis = world_axis(&t, &proxy);
        let clearance = proxy.floor_clearance([t.rot_x, t.rot_y, t.rot_z]);
        assert!(
            axis[2].abs() > 0.999 || axis[2].abs() < 1e-3,
            "stable pose, axis {axis:?}"
        );
        assert!((t.z - (21.0 + clearance)).abs() < 1e-4, "{t:?}");
        let expected = if axis[2] > 0.999 {
            0.0 // upright on its base
        } else if axis[2] < -0.999 {
            2.0 * proxy.hh // upside down
        } else {
            proxy.r // on its side
        };
        assert!((clearance - expected).abs() < 1e-4, "{clearance} {axis:?}");
    }

    /// Every stable pose of the Armory barrel rests its proxy on the floor.
    #[test]
    fn barrel_rest_poses_rest_their_proxy_on_the_floor() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        let p = OAK_BARREL_PROXY_CYLINDER;
        // Upright: base on the floor.
        assert!(p.floor_clearance([0.0, 0.0, 1.0]).abs() < 1e-5);
        // Tipped 45°: the base rim is lowest, r·sin 45° below the origin.
        let tipped = p.floor_clearance([FRAC_PI_4, 0.0, 0.0]);
        assert!((tipped - p.r * FRAC_PI_4.sin()).abs() < 1e-4, "{tipped}");
        for (rot, pose, clearance) in [
            ([0.3, -0.2, 1.0], Some([0.0, 0.0, 1.0]), 0.0),
            ([PI - 0.3, 0.2, 1.0], Some([PI, 0.0, 1.0]), 2.0 * p.hh),
            ([FRAC_PI_2 - 0.2, 0.1, 1.0], None, p.r),
        ] {
            let rest = p.rest_pose(rot);
            if let Some(pose) = pose {
                assert_eq!(rest, pose);
            }
            let q = PositionAndRotation::new(0.0, 0.0, 0.0, rest[0], rest[1], rest[2]);
            let axis = q.to_quaternion().rotate(p.axis);
            assert!(axis[2].abs() > 0.999 || axis[2].abs() < 1e-4, "{axis:?}");
            assert!(
                (p.floor_clearance(rest) - clearance).abs() < 1e-4,
                "{rest:?}"
            );
        }
        // A keg lying across its heading rests one radius up for every spin.
        for spin in [0.0, 0.7, FRAC_PI_2, 2.5] {
            let keg = RollingKeg {
                spin,
                ..RollingKeg::new(KEG_PROXY_CYLINDER, 0.4)
            };
            assert!(
                (keg.clearance() - KEG_PROXY_CYLINDER.r).abs() < 1e-4,
                "{spin}"
            );
        }
    }

    struct HowlingTestFloor;

    impl crate::systems::ai::collision::terrain::TerrainSource for HowlingTestFloor {
        fn sample(&self, _: i32, _: i32) -> f32 {
            100.0
        }

        fn size(&self) -> (i32, i32) {
            (512, 512)
        }
    }

    #[tokio::test]
    async fn howling_bomber_keg_falls_and_bounces_off_the_floor() {
        let f = contact_fixture(
            GimmickTemplate {
                id: 49,
                model_path: "objects/env/01_nuia/001_ndeco/old/barrel_grape_e.cgf ".into(),
                gravity: 9.8,
                life_time: 6_150,
                ..Default::default()
            },
            (150.0, 150.0),
        );
        let collision = &f.server.systems.ai.collision;
        collision
            .test_files
            .set(Arc::new(Default::default()))
            .unwrap();
        assert!(collision
            .test_terrain
            .set(Arc::new(HowlingTestFloor))
            .is_ok());
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let barrel = spawn_from_effect(
            &f.server,
            &f.world,
            &SpawnGimmickParams {
                gimmick_id: 49,
                offset_coordinate_id: 2,
                offset_z: 3.0,
                velocity_coordinate_id: 2,
                velocity_y: 8.0,
                scale: 0.7,
                ..Default::default()
            },
            &source,
        )
        .unwrap();
        let active = f.server.systems.npcs.gimmicks.find(barrel.obj_id).unwrap();
        // No client files here: the keg uses the barrel_grape_e proxy at the authored scale.
        let proxy = KEG_PROXY_CYLINDER.scaled(0.7);
        let radius = proxy.r;
        let across = |t: &crate::models::transform::Transform| {
            let axis = world_axis(t, &proxy);
            assert!(axis[2].abs() < 1e-5, "lies on its side: {axis:?}");
            assert!(
                (axis[0].abs() - 1.0).abs() < 1e-5,
                "axis across +Y: {axis:?}"
            );
        };
        across(&barrel.data().unit.transform);
        let mut bounced = false;
        let mut rolled = false;
        let mut last_spin = barrel.data().unit.transform.local().to_quaternion();
        for step in 1..=60 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                step * 50,
            );
            let data = barrel.data();
            across(&data.unit.transform);
            assert!(data.unit.transform.z >= 100.0 + radius - 0.02);
            bounced |= data.vel[2] > 0.0;
            if data.vel[2] == 0.0 && data.vel[1] > 0.1 {
                // Rolling forward: the turn about the cross axis follows the distance.
                rolled = true;
                assert!((data.unit.transform.z - (100.0 + radius)).abs() < 0.02);
                assert!(data.ang_vel[0] < 0.0, "angVel {:?}", data.ang_vel);
                assert!(
                    (data.ang_vel[0] + data.vel[1] / radius).abs() < 1e-3,
                    "angVel {:?} vel {:?}",
                    data.ang_vel,
                    data.vel
                );
                // The turn since the last tick is about the negative cross axis (-X), the way
                // a body rolling towards +Y turns: the sent quaternion rolls the keg forward.
                let now = data.unit.transform.local().to_quaternion();
                let d = now
                    * Quaternion {
                        x: -last_spin.x,
                        y: -last_spin.y,
                        z: -last_spin.z,
                        w: last_spin.w,
                    };
                let s = d.w.signum();
                assert!(d.x * s < -1e-4, "turn {d:?}");
                assert!(d.y.abs() + d.z.abs() < 0.1 * d.x.abs(), "turn {d:?}");
            }
            last_spin = data.unit.transform.local().to_quaternion();
        }
        let data = barrel.data();
        assert!(bounced);
        assert!(rolled);
        assert!(data.unit.transform.y > 105.0);
        assert!((data.unit.transform.z - (100.0 + radius)).abs() < 0.02);
    }

    #[tokio::test]
    async fn howling_triestes_bolt_sweeps_through_units_between_ticks() {
        let f = contact_fixture(
            GimmickTemplate {
                id: 66,
                mass: 1.0,
                gravity: 9.8,
                life_time: 10_200,
                ..contact_template(true)
            },
            (103.0, 100.0),
        );
        f.victim.data().unit.transform.set_height(100.0);
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let bolt = spawn_from_effect(
            &f.server,
            &f.world,
            &SpawnGimmickParams {
                gimmick_id: 66,
                offset_coordinate_id: 2,
                velocity_coordinate_id: 2,
                velocity_x: 25.0,
                scale: 1.0,
                ..Default::default()
            },
            &source,
        )
        .unwrap();
        let active = f.server.systems.npcs.gimmicks.find(bolt.obj_id).unwrap();
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(240),
            240,
        );
        assert!(bolt.data().unit.transform.x > 105.0);
        assert!(
            active.state().despawned,
            "the bolt collided before its fuse"
        );
        assert!(f.victim.data().unit.hp < 500);
    }

    #[tokio::test]
    async fn moving_gimmick_updates_its_world_region() {
        let f = contact_fixture(contact_template(false), (300.0, 300.0));
        let source = doodad_at(&f.server, &f.world, (63.5, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [10.0, 0.0, 0.0]);
        let me = WorldObject::from(&g);
        assert_eq!(me.region(), Some((0, 0, 1)));
        // 1 s at 10 m/s crosses the 64 m sector boundary.
        for i in 0..20 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        assert_eq!(
            me.region(),
            Some((0, 1, 1)),
            "grid region follows the gimmick"
        );
        assert_eq!(f.world.region_of(&me), Some((1, 1)));
    }

    #[tokio::test]
    async fn effect_gimmick_faction_comes_from_the_caster() {
        // A player-thrown bomb belongs to the player's side: the hostile-only collision effect
        // hits the hostile NPC and leaves the friendly caster alone.
        use crate::data::world_data::{FactionRelation, SystemFaction};
        use crate::systems::skills::types::RelationState;
        let faction = |id: u32, mother: u32| SystemFaction {
            id,
            mother_id: mother,
            ..SystemFaction::default()
        };
        let mut data = contact_data(contact_template(false), 5);
        for f in [faction(101, 148), faction(115, 0), faction(148, 0)] {
            data.factions.add(f);
        }
        data.factions.relations.push(FactionRelation {
            id: 148,
            id2: 115,
            state: RelationState::Hostile as u8,
            exp_time: 0,
        });
        let server = crate::test_support::test_server_with(data);
        while server.ids.object.get_next_id().expect("object id") < 0x400 {}
        crate::systems::skills::bridges::install(&server);
        let world = test_world(&server);
        let caster_client = crate::test_support::TestClient::new(&server, 2, 2);
        let mut c = crate::test_support::test_character(2, "Caster", 100.0, 100.0);
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        let caster = crate::test_support::spawn_player(&server, &caster_client, c, 0x102);
        let mut n = crate::models::npc::Npc::new(1);
        n.unit.name = "Hostile".into();
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
        n.unit.faction_id = 115;
        n.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let npc = Entity::new(0x300, n);
        world.spawn(&npc, &server.view_ctx());
        let g = spawn_from_effect(
            &server,
            &world,
            &SpawnGimmickParams {
                gimmick_id: CONTACT_GIMMICK,
                offset_coordinate_id: 2,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_x: 5.0,
                ..Default::default()
            },
            &WorldObject::from(&caster),
        )
        .expect("gimmick spawned");
        assert_eq!(g.data().unit.faction_id, 101, "copied from the caster");
        let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        gimmick_tick(&server, &world, &active, Duration::from_millis(50), 1_000);
        assert_eq!(npc.data().unit.hp, 900, "the hostile NPC is hit");
        assert_eq!(caster.data().unit.hp, 500, "the friendly caster is not");
    }

    /// The x velocity (f32 at byte 44: obj id, time, two i64 coordinates, z, the quaternion) of
    /// every movement packet a client received.
    fn movement_velocities(client: &mut crate::test_support::TestClient) -> Vec<f32> {
        client
            .drain()
            .into_iter()
            .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCGimmickMovementPacket)
            .map(|(_, body)| f32::from_le_bytes([body[44], body[45], body[46], body[47]]))
            .collect()
    }

    #[tokio::test]
    async fn a_gimmick_stopping_at_the_travel_cap_broadcasts_the_stop() {
        // A moving gimmick that reaches the cap keeps its position, so only the velocity change
        // tells the clients to stop extrapolating the last non-zero velocity.
        let mut f = contact_fixture(
            GimmickTemplate {
                id: CONTACT_GIMMICK,
                ..Default::default()
            },
            (200.0, 100.0),
        );
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [10.0, 0.0, 0.0]);
        f.client.drain();
        for i in 0..500 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        let velocities = movement_velocities(&mut f.client);
        assert!(
            velocities.iter().any(|v| *v > 9.0),
            "the gimmick was broadcast while moving: {velocities:?}"
        );
        assert_eq!(
            velocities.last().copied(),
            Some(0.0),
            "the last packet is the stop (zero velocity)"
        );
        // Further ticks after the stop send nothing.
        for i in 0..10 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                40_000 + i * 50,
            );
        }
        assert!(
            movement_velocities(&mut f.client).is_empty(),
            "a stopped gimmick stays quiet"
        );
    }

    #[tokio::test]
    async fn authored_motion_stops_after_the_travel_cap() {
        let f = contact_fixture(contact_template(false), (300.0, 300.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [10.0, 0.0, 0.0]);
        // 25 s at 10 m/s would be 250 m; the 200 m cap stops it at x = 300.
        for i in 0..500 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        let x = g.data().unit.transform.x;
        assert!((x - 300.0).abs() < 0.6, "stopped at the 200 m cap: x={x}");
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            40_000,
        );
        assert_eq!(g.data().unit.transform.x, x, "no motion after the cap");
    }

    #[tokio::test]
    async fn effect_gimmick_out_of_the_world_grid_despawns() {
        let f = contact_fixture(contact_template(false), (300.0, 300.0));
        // The synthetic world is 2x2 cells (2048 m): launch it past the east edge.
        let source = doodad_at(&f.server, &f.world, (2040.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [100.0, 0.0, 0.0]);
        for i in 0..20 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        assert!(
            f.world.gimmick(g.obj_id).is_none(),
            "left the world grid and despawned"
        );
    }

    #[tokio::test]
    async fn spin_only_gimmick_broadcasts_at_a_lower_rate() {
        let mut f = contact_fixture(contact_template(false), (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let g = spawn_from_effect(
            &f.server,
            &f.world,
            &SpawnGimmickParams {
                gimmick_id: CONTACT_GIMMICK,
                offset_coordinate_id: 2,
                scale: 1.0,
                ang_vel_coordinate_id: 2,
                ang_vel_z: 2.0,
                ..Default::default()
            },
            &source,
        )
        .expect("gimmick spawned");
        let active = f.server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        f.client.drain();
        for i in 0..10 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        let movements = f
            .client
            .drain()
            .into_iter()
            .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCGimmickMovementPacket)
            .count();
        assert!(
            (2..=4).contains(&movements),
            "spin broadcast rate: {movements} packets in 500 ms"
        );
        assert!(g.data().unit.transform.rot_z > 0.0, "the spin continues");
    }

    #[tokio::test]
    async fn delayed_gimmick_skill_fires_at_the_moved_position() {
        // A barrel-bomb style gimmick: a position skill 1 s after spawn, launched 10 m/s
        // forward. The cast must happen from the gimmick's current position (10 m out), not
        // from the thrower (the old caster), whose max_range is 5.
        let template = GimmickTemplate {
            id: 901,
            skill_id: TIMER_SKILL,
            skill_delay: 1_000,
            ..Default::default()
        };
        let server = crate::test_support::test_server_with(timer_data(template));
        while server.ids.object.get_next_id().expect("object id") < 0x400 {}
        crate::systems::skills::bridges::install(&server);
        let world = test_world(&server);
        let client = crate::test_support::TestClient::new(&server, 1, 1);
        let mut c = crate::test_support::test_character(1, "Thrower", 100.0, 100.0);
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        let thrower = crate::test_support::spawn_player(&server, &client, c, 0x100);
        let victim_client = crate::test_support::TestClient::new(&server, 2, 2);
        let mut v = crate::test_support::test_character(2, "Victim", 100.0, 110.0);
        v.unit.hp = 500;
        v.unit.max_hp = 1000;
        let victim = crate::test_support::spawn_player(&server, &victim_client, v, 0x101);
        let g = spawn_from_effect(
            &server,
            &world,
            &SpawnGimmickParams {
                gimmick_id: 901,
                offset_coordinate_id: 2,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_y: 10.0,
                ..Default::default()
            },
            &WorldObject::from(&thrower),
        )
        .expect("gimmick spawned");
        let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        // 20 x 50 ms: the gimmick is at (100, 110) when the 1000 ms delay expires.
        for i in 0..20 {
            gimmick_tick(
                &server,
                &world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        assert!((g.data().unit.transform.y - 110.0).abs() < 1e-3);
        assert_eq!(victim.data().unit.hp, 400, "hit at the moved position");
        assert_eq!(thrower.data().unit.hp, 500, "not hit at the spawn position");
    }

    #[tokio::test]
    async fn contact_gimmick_does_not_latch_a_unit_merely_inside_the_area() {
        // A unit inside the skill's area but outside the contact radius is not hit and must
        // still trigger the firing when it later touches (only a firing latches units).
        let f = contact_fixture_with_area(contact_template(false), (104.0, 100.0), 5);
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(f.victim.data().unit.hp, 500, "not hit at 4 m");
        f.victim.data().unit.transform.set_xyz(100.5, 100.0, 100.0);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_050,
        );
        assert_eq!(f.victim.data().unit.hp, 400, "hit on touch");
    }

    #[tokio::test]
    async fn contact_gimmick_keeps_the_hit_of_a_unit_the_cast_did_not_reach() {
        // The skill centres its r5 area on the touched unit (AnyUnit). Unit A touches the
        // gimmick 1 m behind it; unit B stands 6 m ahead of the gimmick, 7 m from A: inside a
        // sphere around the gimmick that is larger than the area, but outside the cast. B was not
        // hit, so it must not be latched, and its own touch later still fires the skill.
        let mut f = contact_fixture_with_area(contact_template(false), (99.0, 100.0), 5);
        let (_client_b, b) = control_player(&f, 2, (106.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        f.client.drain();
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(f.victim.data().unit.hp, 400, "A touched it and was hit");
        assert_eq!(b.data().unit.hp, 500, "B is outside the cast's area");
        // A walks off; B reaches the gimmick: its first touch fires (a cast centred on B, 10 m
        // from A) and A, latched and out of that area, is not hit again.
        f.victim.data().unit.transform.set_xyz(90.0, 100.0, 100.0);
        b.data().unit.transform.set_xyz(100.6, 100.0, 100.0);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_050,
        );
        assert_eq!(b.data().unit.hp, 400, "B's first touch still hits");
        assert_eq!(f.victim.data().unit.hp, 400, "A keeps its single hit");
    }

    /// The latch is the gimmick's collision immunity (who may fire it), not a damage immunity: a
    /// later firing hits every unit in its own area, a latched one included (PORTING.md, "One
    /// touch fires one skill").
    #[tokio::test]
    async fn a_latched_unit_inside_a_later_firings_area_is_hit_by_it_again() {
        let mut f = contact_fixture_with_area(contact_template(false), (99.0, 100.0), 5);
        let (_client_b, b) = control_player(&f, 2, (106.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        f.client.drain();
        let dt = Duration::from_millis(50);
        gimmick_tick(&f.server, &f.world, &active, dt, 1_000);
        assert_eq!(f.victim.data().unit.hp, 400, "A touched it and was hit");
        assert_eq!(b.data().unit.hp, 500, "B is outside the cast's area");
        // A stays 1.6 m behind the gimmick, inside the area of the cast B's touch fires; A is
        // latched, so it cannot fire the skill again, but it is hit by B's firing.
        b.data().unit.transform.set_xyz(100.6, 100.0, 100.0);
        gimmick_tick(&f.server, &f.world, &active, dt, 1_050);
        assert_eq!(b.data().unit.hp, 400, "B's touch fires");
        assert_eq!(
            f.victim.data().unit.hp,
            300,
            "A, latched but inside the new area, is hit by this firing as well"
        );
        // Nothing fires again while both stay put: both are latched.
        gimmick_tick(&f.server, &f.world, &active, dt, 1_100);
        assert_eq!(b.data().unit.hp, 400);
        assert_eq!(f.victim.data().unit.hp, 300);
    }

    #[tokio::test]
    async fn contact_gimmick_fires_one_area_cast_for_many_touching_units() {
        // Five units touch in one tick. The area skill (r5) must fire once, not once per unit:
        // each unit takes 100 damage, not 500.
        let f = contact_fixture_with_area(contact_template(false), (100.0, 100.0), 5);
        let mut clients = Vec::new();
        let mut units = vec![f.victim.clone()];
        for id in 2..=5 {
            let (client, p) = control_player(&f, id, (100.0, 100.0));
            clients.push(client);
            units.push(p);
        }
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        for (i, unit) in units.iter().enumerate() {
            assert_eq!(
                unit.data().unit.hp,
                400,
                "unit {i} must take exactly one area hit"
            );
        }
    }

    /// Another player (its own connection) for the multi-unit and control assertions.
    fn control_player(
        f: &ContactFixture,
        id: u32,
        at: (f32, f32),
    ) -> (crate::test_support::TestClient, crate::world::PlayerRef) {
        let client = crate::test_support::TestClient::new(&f.server, id, id);
        let mut c = crate::test_support::test_character(id, &format!("Control{id}"), at.0, at.1);
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        let p = crate::test_support::spawn_player(&f.server, &client, c, 0x100 + id);
        (client, p)
    }

    #[tokio::test]
    async fn contact_gimmick_does_not_hit_a_unit_outside_its_radius() {
        let f = contact_fixture(contact_template(false), (105.0, 100.0));
        let (_control_client, control) = control_player(&f, 2, (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        // One 50 ms tick moves it to x = 100.25, 4.75 m from the victim (> 1.5 m radius).
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(
            control.data().unit.hp,
            400,
            "the unit at the gimmick is hit"
        );
        assert_eq!(f.victim.data().unit.hp, 500);
    }

    #[tokio::test]
    async fn contact_gimmick_hits_an_npc_when_the_skill_allows_it() {
        // The bystander player is in the gimmick's neighbourhood (a gimmick with no player
        // around is not scanned) but out of reach.
        let f = contact_fixture(contact_template(false), (110.0, 100.0));
        let mut n = crate::models::npc::Npc::new(1);
        n.unit.name = "Dummy".into();
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
        n.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let npc = Entity::new(0x300, n);
        f.world.spawn(&npc, &f.server.view_ctx());
        let source = WorldObject::from(&npc);
        let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(npc.data().unit.hp, 900);
        assert_eq!(f.victim.data().unit.hp, 500);
    }

    #[tokio::test]
    async fn contact_gimmick_skips_dead_and_invisible_units() {
        // A dead unit is not a contact target; the living control at the same spot is.
        let dead = contact_fixture(contact_template(false), (100.0, 100.0));
        dead.victim.data().unit.hp = 0;
        let (_control_client, control) = control_player(&dead, 2, (100.0, 100.0));
        let source = doodad_at(&dead.server, &dead.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&dead, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &dead.server,
            &dead.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(dead.victim.data().unit.hp, 0);
        assert_eq!(control.data().unit.hp, 400);

        // A GM-invisible unit is not a contact target either; the control is.
        let hidden = contact_fixture(contact_template(false), (100.0, 100.0));
        hidden.victim.data().unit.invisible = true;
        let (_control_client, control) = control_player(&hidden, 2, (100.0, 100.0));
        let source = doodad_at(&hidden.server, &hidden.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&hidden, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &hidden.server,
            &hidden.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(hidden.victim.data().unit.hp, 500);
        assert_eq!(control.data().unit.hp, 400);
    }

    #[tokio::test]
    async fn contact_gimmick_despawns_when_the_template_says_so() {
        let f = contact_fixture(contact_template(true), (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(
            f.victim.data().unit.hp,
            400,
            "the hit is cast before the despawn"
        );
        assert!(f.world.gimmick(g.obj_id).is_none());
        assert!(f.server.systems.npcs.gimmicks.find(g.obj_id).is_none());
    }

    #[tokio::test]
    async fn elevator_without_a_collision_skill_never_hits() {
        // The fixture's victim is the living control for the contact gimmick; the rider stands
        // on the elevator, which has no template and therefore no collision skill.
        let f = contact_fixture(contact_template(false), (200.0, 200.0));
        let rider = control_player(&f, 2, (100.0, 200.0));
        let elevator =
            spawn_from_spawner(&f.server, &f.world, &elevator_spawner()).expect("spawned");
        let active = f
            .server
            .systems
            .npcs
            .gimmicks
            .find(elevator.obj_id)
            .unwrap();
        assert!(active.template.is_none());
        for i in 0..5 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        assert_eq!(rider.1.data().unit.hp, 500, "the elevator rider is not hit");
        let source = doodad_at(&f.server, &f.world, (200.0, 200.0, 100.0));
        let (_, contact) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &contact,
            Duration::from_millis(50),
            1_400,
        );
        assert_eq!(f.victim.data().unit.hp, 400, "a contact gimmick still hits");
    }

    // ----- review fixes: owner, cooldowns, refused casts, caps, dynamic gimmicks -----

    /// The factions of a player-thrown gimmick: the thrower's side 101 (mother 148) is hostile to
    /// the NPC side 115.
    fn with_thrower_factions(mut data: crate::data::GameData) -> crate::data::GameData {
        use crate::data::world_data::{FactionRelation, SystemFaction};
        use crate::systems::skills::types::RelationState;
        for (id, mother) in [(101, 148), (115, 0), (148, 0)] {
            data.factions.add(SystemFaction {
                id,
                mother_id: mother,
                ..SystemFaction::default()
            });
        }
        data.factions.relations.push(FactionRelation {
            id: 148,
            id2: 115,
            state: RelationState::Hostile as u8,
            exp_time: 0,
        });
        data
    }

    /// A hostile (faction 115) NPC with 1000 HP standing at `x`, y = 100.
    fn hostile_npc(
        server: &GameServer,
        world: &WorldInstance,
        obj_id: u32,
        x: f32,
    ) -> crate::world::NpcRef {
        let mut n = crate::models::npc::Npc::new(1);
        n.unit.name = "Hostile".into();
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
        n.unit.faction_id = 115;
        n.unit.transform.set_xyz(x, 100.0, 100.0);
        let npc = Entity::new(obj_id, n);
        world.spawn(&npc, &server.view_ctx());
        npc
    }

    /// A player (faction 101) at (100, 100): the owner of the gimmicks its skills spawn.
    fn thrower_in(
        server: &GameServer,
    ) -> (crate::test_support::TestClient, crate::world::PlayerRef) {
        let client = crate::test_support::TestClient::new(server, 2, 2);
        let mut c = crate::test_support::test_character(2, "Thrower", 100.0, 100.0);
        c.unit.hp = 500;
        c.unit.max_hp = 1000;
        let player = crate::test_support::spawn_player(server, &client, c, 0x102);
        (client, player)
    }

    /// A gimmick that keeps rolling hits every unit it touches. The collision skill's global
    /// cooldown (23 of the 25 shipped ones) and its own cooldown (12006: 26 s) do not apply to a
    /// gimmick caster, although `owner_character` maps a player-thrown gimmick to its thrower, and
    /// the hit tags the thrower for the NPC.
    #[tokio::test]
    async fn a_rolling_gimmick_of_a_player_hits_each_unit_it_touches_and_tags_the_thrower() {
        let data = with_thrower_factions(contact_data_with(
            contact_template(false),
            0,
            crate::systems::skills::tests::damage_effect(1, 100),
            |s| {
                s.default_gcd = true;
                s.cooldown_time = 26_000;
            },
        ));
        let server = crate::test_support::test_server_with(data);
        while server.ids.object.get_next_id().expect("object id") < 0x400 {}
        crate::systems::skills::bridges::install(&server);
        let world = test_world(&server);
        let (_client, thrower) = thrower_in(&server);
        let first = hostile_npc(&server, &world, 0x300, 105.2);
        let second = hostile_npc(&server, &world, 0x301, 110.0);
        // 5 m in front of the thrower, rolling east at 5 m/s: the first NPC is touched on the
        // first tick, the second 14 ticks (700 ms) later.
        let g = spawn_from_effect(
            &server,
            &world,
            &SpawnGimmickParams {
                gimmick_id: CONTACT_GIMMICK,
                offset_coordinate_id: 2,
                offset_x: 5.0,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_x: 5.0,
                ..Default::default()
            },
            &WorldObject::from(&thrower),
        )
        .expect("gimmick spawned");
        let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        for i in 0..20 {
            gimmick_tick(
                &server,
                &world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        assert_eq!(first.data().unit.hp, 900, "touched at once, hit once");
        assert_eq!(
            second.data().unit.hp,
            900,
            "touched 700 ms later, hit although the skill has a global cooldown and a cooldown"
        );
        {
            let d = first.data();
            let tagging = &d.unit.skill_state.tagging;
            assert_eq!(tagging.tagger, thrower.obj_id, "the thrower tags the NPC");
            assert_eq!(tagging.taggers, vec![(thrower.obj_id, 100)]);
        }
        assert_eq!(thrower.data().unit.hp, 500, "the thrower is out of reach");
    }

    /// A gimmick a player threw goes with its thrower. `owner_character` finds the owner through
    /// the live spawner unit, so a gimmick that outlived its thrower would be ownerless and skip
    /// Peace / no-PvP / siege hostility: a disconnect must not lift the protection of a player a
    /// bomb is about to reach. A gimmick no character stands behind (a doodad's here) stays.
    #[tokio::test]
    async fn a_player_thrown_gimmick_is_removed_when_its_thrower_leaves_the_world() {
        let data = with_thrower_factions(contact_data(contact_template(false), 0));
        let server = crate::test_support::test_server_with(data);
        while server.ids.object.get_next_id().expect("object id") < 0x400 {}
        crate::systems::skills::bridges::install(&server);
        let world = test_world(&server);
        let (_client, thrower) = thrower_in(&server);
        let doodad = doodad_at(&server, &world, (200.0, 200.0, 100.0));
        let victim = hostile_npc(&server, &world, 0x301, 105.2);
        let throw = |source: &WorldObject| {
            let g = spawn_from_effect(
                &server,
                &world,
                &SpawnGimmickParams {
                    gimmick_id: CONTACT_GIMMICK,
                    offset_coordinate_id: 2,
                    offset_x: 5.0,
                    scale: 1.0,
                    velocity_coordinate_id: 2,
                    velocity_x: 5.0,
                    ..Default::default()
                },
                source,
            )
            .expect("gimmick spawned");
            let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
            (g, active)
        };
        // 5 m in front of the thrower, rolling east at 5 m/s: the first tick touches the victim.
        let (thrown, thrown_active) = throw(&WorldObject::from(&thrower));
        let (kept, kept_active) = throw(&doodad);
        assert_eq!(
            thrown_active.state().owner_character_id,
            thrower.character_id,
            "the thrower's character is recorded when the gimmick is spawned"
        );
        assert_eq!(
            kept_active.state().owner_character_id,
            0,
            "a doodad has none"
        );
        assert!(server.world.character_by_id(thrower.character_id).is_some());

        // The thrower leaves the way `leave_world` does: out of the grid, then out of the
        // character registry.
        world.remove_object(thrower.obj_id);
        assert!(server.world.try_remove_character(thrower.obj_id));

        let dt = Duration::from_millis(50);
        gimmick_tick(&server, &world, &thrown_active, dt, 1_000);
        gimmick_tick(&server, &world, &kept_active, dt, 1_000);
        assert!(
            world.gimmick(thrown.obj_id).is_none(),
            "the gimmick left the world with its thrower"
        );
        assert!(server.systems.npcs.gimmicks.find(thrown.obj_id).is_none());
        assert_eq!(
            victim.data().unit.hp,
            1000,
            "it was removed before its contact could hit as an ownerless gimmick"
        );
        assert!(world.gimmick(kept.obj_id).is_some());
        assert!(server.systems.npcs.gimmicks.find(kept.obj_id).is_some());
    }

    /// A refused cast leaves everything as it was: nothing is latched and a
    /// `disappear_by_collision` gimmick is not removed for a cast that never ran. The skill here
    /// only accepts a dead target, so every living contact is refused.
    #[tokio::test]
    async fn a_refused_contact_cast_latches_nothing_and_keeps_the_gimmick() {
        let data = contact_data_with(
            contact_template(true),
            0,
            crate::systems::skills::tests::damage_effect(1, 100),
            |s| {
                s.target_alive = false;
                s.target_dead = true;
            },
        );
        let f = contact_fixture_with_data(data, (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(f.victim.data().unit.hp, 500, "the cast was refused");
        assert!(
            active.state().collision_hits.is_empty(),
            "a refused cast latches no unit"
        );
        assert!(
            f.world.gimmick(g.obj_id).is_some(),
            "a refused cast does not despawn the gimmick"
        );
    }

    /// The latch is capped; a full latch ignores further touches instead of firing the skill on
    /// every tick for a unit that can no longer be latched.
    #[tokio::test]
    async fn a_full_contact_latch_ignores_further_touches() {
        let f = contact_fixture(contact_template(true), (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        active.state().collision_hits = (0..MAX_COLLISION_HITS as u32)
            .map(|i| 0x10_0000 + i)
            .collect();
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(f.victim.data().unit.hp, 500, "no cast after the cap");
        assert!(f.world.gimmick(g.obj_id).is_some());
        assert_eq!(active.state().collision_hits.len(), MAX_COLLISION_HITS);
    }

    /// r208022 has no skill 14768, 18424, 23476 or 24080, which five gimmicks name as their
    /// collision skill: such a gimmick is skipped, not despawned for a cast that cannot happen.
    #[tokio::test]
    async fn a_gimmick_whose_collision_skill_does_not_exist_is_skipped() {
        let template = GimmickTemplate {
            collision_skill_id: 999_999,
            ..contact_template(true)
        };
        let f = contact_fixture(template, (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(f.victim.data().unit.hp, 500);
        assert!(
            f.world.gimmick(g.obj_id).is_some(),
            "not despawned for nothing"
        );
        assert!(active.state().collision_hits.is_empty());
    }

    /// An instance nobody looks at is dormant: a gimmick with no player in its neighbourhood is
    /// not scanned, and it starts hitting once a player is around.
    #[tokio::test]
    async fn a_contact_gimmick_with_no_player_nearby_is_not_scanned() {
        let f = contact_fixture(contact_template(false), (300.0, 300.0));
        let mut n = crate::models::npc::Npc::new(1);
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
        n.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let npc = Entity::new(0x300, n);
        f.world.spawn(&npc, &f.server.view_ctx());
        let (_, active) = spawn_contact_gimmick(&f, &WorldObject::from(&npc), [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(npc.data().unit.hp, 1000, "the only player is 280 m away");
        // The player arrives in the neighbourhood (out of reach of the gimmick).
        f.victim.data().unit.transform.set_xyz(110.0, 100.0, 100.0);
        crate::systems::ai::npc::check_moved_position(&f.server, &WorldObject::from(&f.victim));
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_050,
        );
        assert_eq!(npc.data().unit.hp, 900, "scanned once a player is around");
        assert_eq!(f.victim.data().unit.hp, 500);
    }

    /// The six `use_level_damage` rows of the collision skills (518, 542, 743, 858, 1334, 1686:
    /// skills 11931, 12006, 12518, 12944, 14102, 14873) scale with the caster's `LevelDps` stat.
    /// A gimmick has no stats, so such a row evaluates to 0 and lands the 1 HP minimum; level 1
    /// does not change that (PORTING.md: the retail value is not known). This pins today's
    /// behaviour: it changes only if gimmicks ever get stats.
    #[tokio::test]
    async fn a_level_scaled_damage_row_lands_the_one_hp_minimum_from_a_gimmick() {
        let mut row = crate::systems::skills::tests::damage_effect(1, 0);
        row.use_fixed_damage = false;
        row.use_level_damage = true;
        row.level_md = 40.0;
        let f = contact_fixture_with_data(
            contact_data_with(contact_template(false), 0, row, |_| {}),
            (100.0, 100.0),
        );
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000,
        );
        assert_eq!(f.victim.data().unit.hp, 499, "1 HP, not a level-scaled hit");
    }

    /// Only a kinematic gimmick (`mass <= 0`) is launched by its authored linear velocity; a
    /// dynamic one (`mass > 0`) would fly straight until the runaway cap (no gravity or ground
    /// model) and stays where it spawns.
    #[tokio::test]
    async fn only_a_kinematic_gimmick_is_launched() {
        for (mass, launched) in [(-1.0f32, true), (0.0, true), (2.0, false)] {
            let template = GimmickTemplate {
                mass,
                ..contact_template(false)
            };
            let f = contact_fixture(template, (300.0, 300.0));
            let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
            let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
            assert_eq!(active.state().authored_motion, launched, "mass {mass}");
            for i in 0..4 {
                gimmick_tick(
                    &f.server,
                    &f.world,
                    &active,
                    Duration::from_millis(50),
                    1_000 + i * 50,
                );
            }
            let x = g.data().unit.transform.x;
            if launched {
                assert!((x - 101.0).abs() < 1e-3, "mass {mass}: x={x}");
            } else {
                assert!((x - 100.0).abs() < 1e-4, "mass {mass}: x={x}");
                assert!(g.data().vel.iter().all(|v| v.abs() < 1e-6));
            }
        }
    }

    /// A dynamic gimmick keeps its authored angular velocity (a mine spinning in place).
    #[tokio::test]
    async fn a_dynamic_gimmick_still_spins() {
        let template = GimmickTemplate {
            mass: 2.0,
            ..contact_template(false)
        };
        let f = contact_fixture(template, (300.0, 300.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let g = spawn_from_effect(
            &f.server,
            &f.world,
            &SpawnGimmickParams {
                gimmick_id: CONTACT_GIMMICK,
                offset_coordinate_id: 2,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_y: 5.0,
                ang_vel_coordinate_id: 2,
                ang_vel_z: 2.0,
                ..Default::default()
            },
            &source,
        )
        .expect("gimmick spawned");
        let active = f.server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        assert!(active.state().authored_motion);
        for i in 0..4 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        let t = g.data().unit.transform;
        assert!(t.rot_z > 0.0, "it spins");
        assert!((t.y - 100.0).abs() < 1e-4, "it is not launched: y={}", t.y);
    }

    /// The 30 s time cap of authored motion (the 200 m distance cap is not reached at 1 m/s).
    #[tokio::test]
    async fn authored_motion_stops_after_the_time_cap() {
        let f = contact_fixture(contact_template(false), (300.0, 300.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [1.0, 0.0, 0.0]);
        for i in 0..700 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        let x = g.data().unit.transform.x;
        assert!(
            (x - 130.0).abs() < 0.1,
            "stopped by the 30 s cap after 30 m: x={x}"
        );
        for i in 700..800 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        assert!(
            (g.data().unit.transform.x - x).abs() < 1e-4,
            "no motion after the cap"
        );
    }

    /// A gimmick spawned by a doodad has no spawner unit: the C# cast its timer skill from that
    /// missing unit and never fired. It is cast from the gimmick at its position.
    #[tokio::test]
    async fn a_doodad_spawned_gimmick_casts_its_timer_skill() {
        let template = GimmickTemplate {
            id: 901,
            skill_id: TIMER_SKILL,
            skill_delay: 1_000,
            ..Default::default()
        };
        let server = crate::test_support::test_server_with(timer_data(template));
        while server.ids.object.get_next_id().expect("object id") < 0x400 {}
        crate::systems::skills::bridges::install(&server);
        let world = test_world(&server);
        let client = crate::test_support::TestClient::new(&server, 2, 2);
        let mut v = crate::test_support::test_character(2, "Victim", 100.0, 100.0);
        v.unit.hp = 500;
        v.unit.max_hp = 1000;
        let victim = crate::test_support::spawn_player(&server, &client, v, 0x101);
        let mut d = crate::models::doodad::Doodad::new(100);
        d.transform.set_xyz(100.0, 100.0, 100.0);
        let doodad = Entity::new(0x500, d);
        let g = spawn_from_doodad(
            &server,
            &world,
            &WorldObject::from(&doodad),
            &crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick {
                gimmick_id: 901,
                scale: 1.0,
                ..Default::default()
            },
        )
        .expect("gimmick spawned");
        assert_eq!(g.data().spawner_unit_id, 0x500, "the doodad is the spawner");
        let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        for i in 0..20 {
            gimmick_tick(
                &server,
                &world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        assert_eq!(victim.data().unit.hp, 400, "the timer skill fired");
    }

    #[tokio::test]
    async fn unknown_template_or_origin_is_not_spawned() {
        let server = test_server();
        let world = test_world(&server);
        let s = GimmickSpawner {
            unit_id: 999_999,
            ..elevator_spawner()
        };
        assert!(spawn_from_spawner(&server, &world, &s).is_none());
        let s = GimmickSpawner {
            position: Some(Default::default()),
            ..elevator_spawner()
        };
        assert!(spawn_from_spawner(&server, &world, &s).is_none());
        let s = GimmickSpawner {
            position: None,
            ..elevator_spawner()
        };
        assert!(spawn_from_spawner(&server, &world, &s).is_none());
    }

    #[test]
    fn time_of_day() {
        assert_eq!(time_of_day_ms(86_400_000 + 1234), 1234);
    }
}
