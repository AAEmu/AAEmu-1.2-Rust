use super::*;

/// Casts a skill at the gimmick's position and announces an accepted cast in system chat.
///
/// The 1.2 data can name the same skill for the fuse and for contact (e.g. the Triste bolts
/// and the collapsing ceiling). The cast claims that skill before invoking effects so the
/// contact path cannot fire it again. If contact fired first, it has already spent the fuse.
pub(super) fn do_gimmick_skill(
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
