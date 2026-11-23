use super::tick::do_gimmick_skill;
use super::*;

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
