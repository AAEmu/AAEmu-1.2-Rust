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
use crate::world::object::Entity;
use crate::world::{self, session, GimmickRef, WorldInstance, WorldObject};

/// `GimmickManager.Delay`: the tick period.
pub const TICK_DELAY: Duration = Duration::from_millis(50);
/// The template id that gets `GimmickMovementFloatToSurface` (Recovered Treasure Chest).
pub const FLOAT_TO_SURFACE_TEMPLATE_ID: u32 = 37;
/// Contact radius of a gimmick for `GimmickTemplate.CollisionSkillId` hits. The `gimmicks`
/// table has no size / radius column and a gimmick has no actor `model_id` (`ModelManager`'s
/// radii are keyed by actor model ids, and `new_gimmick` never sets one), so the model radius
/// is not available: this is the documented stand-in. The touched unit's own actor radius is
/// added to it (a large NPC is touched at its edge).
pub const COLLISION_RADIUS: f32 = 1.5;
/// Cap of the per-gimmick contact-hit set ([`GimmickState::collision_hits`]). Object ids are
/// reused and a gimmick lives at most `life_time`; a touch beyond the cap is ignored instead of
/// growing the set without bound.
const MAX_COLLISION_HITS: usize = 256;

/// `(uint)(DateTime.UtcNow - DateTime.UtcNow.Date).TotalMilliseconds`.
pub fn time_of_day_ms(now_ms: i64) -> u32 {
    now_ms.rem_euclid(86_400_000) as u32
}

/// The C# `Gimmick` runtime members.
#[derive(Debug, Clone, Default)]
pub struct GimmickState {
    /// Kinematic velocities supplied by a doodad's SpawnGimmick phase function.
    pub authored_motion: bool,
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
    /// `SkillStarted`.
    pub skill_started: bool,
    /// `Despawn != DateTime.MinValue`.
    pub despawned: bool,
    /// `Respawn` (unix ms, 0 = `DateTime.MinValue`).
    pub respawn_ms: i64,
    /// Units this gimmick has already hit with its `collision_skill_id` (obj ids, at most
    /// [`MAX_COLLISION_HITS`]). Not a C# member: the C# never implemented contact hits.
    pub collision_hits: Vec<u32>,
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

/// A gimmick in `GimmickManager._activeGimmicks`.
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
fn add_active_gimmick(
    server: &GameServer,
    world_id: u32,
    gimmick: GimmickRef,
    spawner: Arc<GimmickSpawner>,
    template: Option<GimmickTemplate>,
) -> Arc<ActiveGimmick> {
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
    let state = GimmickState {
        time: time_of_day_ms(world::now_ms()),
        ..Default::default()
    };
    let active = Arc::new(ActiveGimmick {
        gimmick,
        world_id,
        spawner,
        template,
        handler,
        state: Mutex::new(state),
    });
    server
        .systems
        .npcs
        .gimmicks
        .for_world(world_id)
        .add(active.clone());
    active
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
    // effects start at level 1, so a level-0 gimmick caster skipped every effect (verified live
    // on skill 18717: "caster level outside the effect's level range"). The lowest level the
    // data accepts keeps the level-based damage terms meaningful.
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
) -> Option<Arc<ActiveGimmick>> {
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
    Some(add_active_gimmick(
        server, world.id, gimmick, spawner, template,
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
        Some(a) => Some(a.gimmick.clone()),
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
    {
        let t = &mut g.unit.transform;
        t.instance_id = caster_t.instance_id;
        t.world_id = caster_t.world_id;
        t.zone_id = caster_t.zone_id;
        t.set_position(
            caster_world.x,
            caster_world.y,
            caster_world.z,
            caster_world.rot_x,
            caster_world.rot_y,
            caster_world.rot_z,
        );
    }
    g.entity_guid = 0;
    g.spawner_unit_id = caster.obj_id();
    g.grasper_unit_id = 0;
    match params.offset_coordinate_id {
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
    // `SpawnGimmickEffect.Velocity*` / `AngVel*`: the C# `GimmickSpawner` constructor stored
    // them but never applied them, so effect-spawned gimmicks never moved. The shipped rows are
    // authored in the caster's local frame (forward +Y, right +X, up +Z: (0,25,6) and (0,4,0)
    // are forward / up launches), so rotate them by the caster's world yaw. The doodad spawn
    // path below overrides both with its world-space values (WZGimmickSetMovement semantics).
    // Every nonzero shipped row uses coordinate type 2; other types have no nonzero row.
    let yaw = caster_world.rot_z;
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let rotate_local = |v: [f32; 3]| {
        [
            v[0] * cos_yaw - v[1] * sin_yaw,
            v[0] * sin_yaw + v[1] * cos_yaw,
            v[2],
        ]
    };
    if params.velocity_coordinate_id == 2 {
        g.vel = rotate_local([params.velocity_x, params.velocity_y, params.velocity_z]);
    }
    if params.ang_vel_coordinate_id == 2 {
        g.ang_vel = rotate_local([params.ang_vel_x, params.ang_vel_y, params.ang_vel_z]);
    }
    let gimmick: GimmickRef = Entity::new(obj_id, g);
    let changes = world.spawn(&gimmick, &server.view_ctx());
    session::handle_zone_changes(server, changes);
    let active = add_active_gimmick(server, world.id, gimmick.clone(), spawner, Some(template));
    if params.velocity_coordinate_id == 2 || params.ang_vel_coordinate_id == 2 {
        active.state().authored_motion = true;
    }
    Some(gimmick)
}

/// Doodad phase spawns use the doodad's world transform and the authored world offsets,
/// faction, linear velocity and angular velocity (WZGimmickSetMovement semantics).
pub fn spawn_from_doodad(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    source: &WorldObject,
    f: &crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick,
) -> Option<GimmickRef> {
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
    let gimmick = spawn_from_effect(server, world, &params, source)?;
    {
        let mut g = gimmick.data();
        g.unit.faction_id = f.faction_id;
        g.vel = [f.velocity_x, f.velocity_y, f.velocity_z];
        g.ang_vel = [f.angle_x, f.angle_y, f.angle_z];
    }
    if let Some(active) = server
        .systems
        .npcs
        .gimmicks
        .for_world(world.id)
        .get(gimmick.obj_id)
    {
        active.state().authored_motion = true;
    }
    Some(gimmick)
}

/// `GimmickSpawner.Despawn(gimmick)`: removes it from the manager and the world; releases its
/// ids unless a respawn is pending.
pub fn despawn(server: &GameServer, active: &ActiveGimmick) {
    if let Some(gimmicks) = server.systems.npcs.gimmicks.existing(active.world_id) {
        gimmicks.remove(active.gimmick.obj_id);
    }
    if let Some(world) = server.world.world_if_exists(active.world_id) {
        world.delete(&active.gimmick);
    }
    if active.state().respawn_ms == 0 {
        if active.gimmick.obj_id > 0 {
            server.ids.object.release_id(active.gimmick.obj_id);
        }
        let gimmick_id = active.gimmick.data().gimmick_id;
        if gimmick_id > 0 {
            server.ids.gimmick.release_id(u32::from(gimmick_id));
        }
    }
}

/// `Gimmick.DoGimmickSkill(skillId)`: runs the skill once through the skills hook (caster =
/// the spawner unit, a `SkillDoodad` caster of the gimmick, cast at the gimmick's position) and
/// announces it in the system chat around the gimmick.
fn do_gimmick_skill(
    server: &GameServer,
    world: &WorldInstance,
    active: &ActiveGimmick,
    skill_id: u32,
) {
    if skill_id == 0 {
        return;
    }
    {
        let mut st = active.state();
        if st.skill_started {
            return;
        }
        st.skill_started = true;
    }
    let (spawner_unit_id, pos) = {
        let g = active.gimmick.data();
        (g.spawner_unit_id, g.unit.transform.local())
    };
    let caster = world.unit(spawner_unit_id);
    let me = WorldObject::from(&active.gimmick);
    if let Some(h) = server.systems.npcs.hooks.skills.get() {
        h.use_gimmick_skill(
            server,
            caster.as_ref(),
            &me,
            skill_id,
            (pos.x, pos.y, pos.z),
        );
    }
    let msg = SCChatMessagePacket {
        chat_type: chat_type::SYSTEM,
        sender: None,
        message: format!("Gimmick {} used skill {skill_id}", active.gimmick.obj_id),
        ability: 0,
        language_type: 0,
    };
    world::broadcast(world, &me, &msg, false);
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

/// `GimmickTemplate.CollisionSkillId` contact check. Retail's zone process reported the touch
/// (`ZWGimmickCollided`, op 42) and the collision skill was cast from the gimmick; the 1.2
/// client has no gimmick packet (only SC 0x13c-0x141), so the server detects the touch here.
///
/// One touch fires one skill, never one per touched unit: most collision skills are area skills
/// (18717 r20, 14649 r30, ...) and firing them once per unit multiplied the explosion. The
/// firing latches every unit it covers (the skill's area radius around the gimmick plus every
/// touched unit), so a unit already hit by a firing never triggers another one. A
/// `disappear_by_collision` gimmick fires once in total and despawns. Returns `true` when the
/// gimmick was despawned.
fn collision_tick(
    server: &GameServer,
    world: &WorldInstance,
    active: &ActiveGimmick,
    template: &GimmickTemplate,
) -> bool {
    // Never call into the world while the gimmick's data lock is held.
    let (origin, vel) = {
        let g = active.gimmick.data();
        let t = g.unit.transform;
        ([t.x, t.y, t.z], g.vel)
    };
    let speed = (vel[0] * vel[0] + vel[1] * vel[1] + vel[2] * vel[2]).sqrt();
    if !speed.is_finite() || speed < template.collision_min_speed {
        return false;
    }
    let skill = server
        .game_data
        .systems
        .skills
        .skill(template.collision_skill_id)
        .cloned();
    let area = skill
        .as_ref()
        .map_or(0.0, |t| t.target_area_radius.max(0) as f32);
    let me = WorldObject::from(&active.gimmick);
    let mut contacts: Vec<(u32, WorldObject)> = Vec::new();
    let mut covered: Vec<u32> = Vec::new();
    for unit in world.around(&me, |o| o.is_unit().then(|| o.clone())) {
        if !contact_unit(world, &unit) {
            continue;
        }
        let p = world.world_pos_rot(&unit);
        let model = crate::systems::skills::geometry::model_size(
            server,
            &crate::systems::skills::geometry::Target::Obj(unit.clone()),
        );
        let dx = p.x - origin[0];
        let dy = p.y - origin[1];
        let dz = p.z - origin[2];
        let distance_sq = dx * dx + dy * dy + dz * dz;
        let reach = COLLISION_RADIUS + model;
        if distance_sq <= reach * reach {
            contacts.push((unit.obj_id(), unit.clone()));
        }
        if area > 0.0 {
            // The skill centers its area on the first touched unit (AnyUnit target) or on the
            // gimmick (position target); include the contact radius so either center's area is
            // covered by the latch.
            let cover = area + COLLISION_RADIUS + model;
            if distance_sq <= cover * cover {
                covered.push(unit.obj_id());
            }
        }
    }
    let mut fire_with: Option<WorldObject> = None;
    {
        // Check and latch under one lock; no world call while it is held.
        let mut st = active.state();
        for (id, unit) in &contacts {
            if !st.collision_hits.contains(id) {
                fire_with = Some(unit.clone());
                break;
            }
        }
        for id in contacts.iter().map(|(id, _)| *id).chain(covered) {
            if st.collision_hits.len() >= MAX_COLLISION_HITS {
                break;
            }
            if !st.collision_hits.contains(&id) {
                st.collision_hits.push(id);
            }
        }
    }
    let Some(unit) = fire_with else {
        return false;
    };
    if let Some(h) = server.systems.npcs.hooks.skills.get() {
        tracing::debug!(
            gimmick = me.obj_id(),
            unit = unit.obj_id(),
            skill_id = template.collision_skill_id,
            speed,
            "Gimmick collision: casting the collision skill"
        );
        h.use_gimmick_collision_skill(server, &me, &unit, template.collision_skill_id);
    }
    if template.disappear_by_collision {
        active.state().despawned = true;
        despawn(server, active);
        return true;
    }
    false
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
        None => {
            if active.state().authored_motion {
                let mut g = active.gimmick.data();
                let vel = g.vel;
                let ang = g.ang_vel;
                let t = &mut g.unit.transform;
                t.x += vel[0] * delta_time;
                t.y += vel[1] * delta_time;
                t.z += vel[2] * delta_time;
                t.rot_x += ang[0] * delta_time;
                t.rot_y += ang[1] * delta_time;
                t.rot_z += ang[2] * delta_time;
            }
        }
    }

    // Keep the grid region current: `around`, visibility and the movement broadcast all use it
    // (same update as a moving NPC, `WorldManager.AddVisibleObject`).
    crate::systems::ai::npc::check_moved_position(server, &WorldObject::from(&active.gimmick));

    // Handle Delayed Skills
    if let Some(t) = &active.template {
        let started = active.state().skill_started;
        let total = active.state().total_life_time;
        if t.skill_delay > 0
            && !started
            && last_life_time.as_secs_f64() * 1000.0 < f64::from(t.skill_delay)
            && total.as_secs_f64() * 1000.0 >= f64::from(t.skill_delay)
        {
            do_gimmick_skill(server, world, active, t.skill_id);
        }
    }
    // Contact hits (`GimmickTemplate.CollisionSkillId`): after the movement of this tick.
    if let Some(t) = &active.template {
        if t.collision_skill_id != 0 && collision_tick(server, world, active, t) {
            return; // despawned by the collision
        }
    }

    let time = time_of_day_ms(now_ms);
    let authored_motion = active.state().authored_motion;
    let (packet, pos, rot) = {
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
        )
    };
    {
        let mut st = active.state();
        st.time = time;
    }
    world::broadcast(world, &active.gimmick, &packet, false);
    {
        let mut st = active.state();
        st.last_pos = pos;
        st.last_rot = rot;
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
            let mut st = active.state();
            let due = t.life_time > 0
                && !st.despawned
                && st.total_life_time.as_secs_f64() * 1000.0 >= f64::from(t.life_time);
            if due {
                st.despawned = true;
            }
            due
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
        use crate::systems::skills::data::{EffectKind, EffectTemplate};
        use crate::systems::skills::tests::{damage_effect, skill_effect, skill_template};
        let mut data = crate::data::GameData::empty();
        let s = &mut data.systems.skills;
        s.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(damage_effect(1, 100))),
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
        skill.effects = vec![effect];
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
        _client: crate::test_support::TestClient,
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
        let server = crate::test_support::test_server_with(contact_data(template, area_radius));
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
            _client: client,
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
        let mut victim_client = crate::test_support::TestClient::new(&server, 2, 2);
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
        let f = contact_fixture(contact_template(false), (300.0, 300.0));
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
