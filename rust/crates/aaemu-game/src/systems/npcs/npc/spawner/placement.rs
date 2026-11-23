use super::*;

/// `ParentWorld.Template.GeoData.GetHeight(position)` (`AiGeoDataManager.GetHeight`): the
/// navmesh (`.bai`, GeoDataMode) is not ported, so this is its fallback
/// `GetRawHeightMapHeight((int)Round(x), (int)Round(y))`; exceptions give 0.
pub fn geo_data_height(server: &GameServer, world: &WorldInstance, x: f32, y: f32) -> f32 {
    let Some(h) = server.systems.npcs.heightmaps.get() else {
        return 0.0;
    };
    let Some(t) = h.template(world.template.id) else {
        return 0.0;
    };
    t.get_raw_height_map_height(
        h.files(),
        x.round_ties_even() as i32,
        y.round_ties_even() as i32,
    )
    .unwrap_or(0.0)
}

/// `NpcSpawnerNpc.Spawn(npcSpawner, ownerId)`: `Err` = the C# `InvalidOperationException` for
/// an unsupported `MemberType`; `Ok(None)` = the C# `null` (template missing).
pub fn spawn_member(
    server: &GameServer,
    nsn: &NpcSpawnerNpc,
    spawner: &Arc<NpcSpawner>,
    owner_id: u32,
) -> Result<Option<Vec<NpcRef>>, String> {
    match nsn.member_type.as_str() {
        "Npc" => spawn_npc(server, nsn, spawner, owner_id),
        // The C# `SpawnNpcGroup` called `SpawnNpc` with `MemberId`, which for these rows is an
        // `npc_groups.id`, not an NPC template id (see `spawn_npc_group`).
        "NpcGroup" => spawn_npc_group(server, nsn, spawner, owner_id),
        _ => Err(format!(
            "Tried spawning an unsupported line from NpcSpawnerNpc - Id: {}",
            nsn.id
        )),
    }
}

/// `NpcSpawnerNpc.SpawnNpcGroup` (implemented here even though the C# body was `SpawnNpc`):
/// spawns every NPC template of `nsn.member_id`'s `npc_groups` row at the spawner position plus
/// the member's `formation_offset`.
///
/// The offsets are applied as authored (world axes). No 1.2 capture or client data shows that
/// they are rotated by the spawner yaw (the client strings only expose runtime AI formations),
/// so rotating them is capture-pending (see `PORTING.md`).
fn spawn_npc_group(
    server: &GameServer,
    nsn: &NpcSpawnerNpc,
    spawner: &Arc<NpcSpawner>,
    owner_id: u32,
) -> Result<Option<Vec<NpcRef>>, String> {
    let groups = &server.game_data.systems.npcs.npc_groups;
    let Some(group) = groups.group(nsn.member_id) else {
        tracing::warn!(
            "NpcGroup {}, from spawner Id {} not exist at db. Spawner Position: {:?}",
            nsn.member_id,
            spawner.id,
            spawner.position()
        );
        return Ok(None);
    };
    let members = groups.members(group.id);
    if members.is_empty() {
        tracing::warn!(
            "NpcGroup {} ({}) from spawner Id {} has no members. Spawner Position: {:?}",
            group.id,
            group.name,
            spawner.id,
            spawner.position()
        );
        return Ok(None);
    }
    if spawner
        .parent_world()
        .and_then(|id| server.world.world(id))
        .is_none()
    {
        // Fail before creating anything: a partial group must not be spawned unregistered.
        return Err(format!("spawner {} has no parent world", spawner.id));
    }
    let mut spawned: Vec<(group_data::NpcGroupMember, NpcRef)> = Vec::with_capacity(members.len());
    for member in members {
        let offset = (
            member.formation_offset_x,
            member.formation_offset_y,
            member.formation_offset_z,
        );
        match spawn_npc_template(server, member.npc_id, spawner, owner_id, Some(offset), None)? {
            Some(npc) => spawned.push((member.clone(), npc)),
            // A missing member template is skipped like a missing ordinary member; the rest of
            // the group still spawns.
            None => tracing::warn!(
                "NpcGroup {} ({}) member npc {} from spawner Id {} does not exist at db",
                group.id,
                group.name,
                member.npc_id,
                spawner.id
            ),
        }
    }
    // Runtime membership: move leader, formation offsets / tension and `aggro_rule_id` for
    // `systems::ai::group` (the 1.2 formation follow and shared aggro), plus the partial
    // respawn state of an `enable_respawn` batch.
    group::attach_spawned_group(server, &spawned, group, Some(spawner));
    let npcs: Vec<NpcRef> = spawned.into_iter().map(|(_, npc)| npc).collect();
    Ok((!npcs.is_empty()).then_some(npcs))
}
/// `npc_spawn_on_terrain` for a walking NPC: the Z to spawn at, or `None` to keep the spawn Z.
///
/// A ground probe that hits geometry (terrain, a building, a cave floor; it starts only 2 m
/// above the spawn, so a cave roof is never hit) is a real floor: the spawn snaps onto it when
/// it is less than [`SPAWN_GROUND_SNAP`] above it (or below it, at most the probe's 2 m), except
/// that a spawn 1 m or more above a floor under the water surface is a swimmer and stays, a
/// spawn 1 m or more above a floor inside a building's bounds is on a floor we lack, and a
/// floor at the heightmap's height ceiling is not a floor (the data saturates there).
///
/// Without a hit the probe found no floor under the spawn. Over a terrain hole the spawn stays.
/// Under solid terrain less than [`SPAWN_SINK_SNAP`] deep it is lifted onto the terrain. Otherwise
/// (and without the collision service) the C# rule applies: the terrain elevation, else the
/// heightmap, snapped only within 1 m.
pub(crate) fn spawn_ground_z(
    server: &GameServer,
    world: &WorldInstance,
    x: f32,
    y: f32,
    z: f32,
) -> Option<f32> {
    use crate::systems::ai::collision as col;
    // The heightmap stores `height * coefficient` in a u16, so heights saturate at
    // `max_height`: a terrain "floor" there is only a lower bound.
    let saturated = |h: f32| h >= world.template.max_height - 0.05;
    if let Some(Some(floor)) = col::ground_height_probe(
        server,
        world.id,
        x,
        y,
        z,
        col::GROUND_PROBE_UP,
        col::GROUND_PROBE_DOWN,
    ) {
        if saturated(floor) {
            return None;
        }
        if z - floor >= 1.0
            && col::water_level(server, world.id, x, y, z).is_some_and(|level| floor + 0.1 < level)
        {
            return None;
        }
        // Inside a building's bounds the terrain "floor" is the ground under the building: the
        // hall's own floor may be missing from its collision (Two Crowns throne hall), so the
        // authored Z is kept instead of sinking the spawn through its floor.
        if z - floor >= 1.0 && col::inside_brush_bounds(server, world.id, x, y, z) == Some(true) {
            return None;
        }
        return (z - floor < SPAWN_GROUND_SNAP).then_some(floor);
    }
    if let Some(terrain) = col::terrain_height(server, world.id, x, y) {
        if saturated(terrain) || col::terrain_is_hole(server, world.id, x, y) == Some(true) {
            return None;
        }
        if terrain - z > 0.0 && terrain - z < SPAWN_SINK_SNAP {
            return Some(terrain);
        }
    }
    let ground = col::terrain_height_or(server, world.id, x, y, || {
        geo_data_height(server, world, x, y)
    });
    ((z - ground).abs() < 1.0).then_some(ground)
}

/// `NpcSpawnerNpc.SpawnNpc`.
fn spawn_npc(
    server: &GameServer,
    nsn: &NpcSpawnerNpc,
    spawner: &Arc<NpcSpawner>,
    owner_id: u32,
) -> Result<Option<Vec<NpcRef>>, String> {
    Ok(
        spawn_npc_template(server, nsn.member_id, spawner, owner_id, None, None)?
            .map(|npc| vec![npc]),
    )
}

/// `NpcSpawnerNpc.SpawnNpc` for one concrete NPC template, at the spawner position plus
/// `offset` (an `npc_group_members.formation_offset_*`, or `None` for an ordinary member).
///
/// `base` overrides `(x, y, z, yaw)` of the spawner position: the group partial respawn creates
/// a lost member at the move leader's current position (`RespawnLostGroupMembers`,
/// `sub_39341EF0`, Z:851444-851452).
pub(super) fn spawn_npc_template(
    server: &GameServer,
    template_id: u32,
    spawner: &Arc<NpcSpawner>,
    owner_id: u32,
    offset: Option<(f32, f32, f32)>,
    base: Option<(f32, f32, f32, f32)>,
) -> Result<Option<NpcRef>, String> {
    let Some(npc) = backend::npc_create(server, 0, template_id) else {
        tracing::warn!(
            "Npc {}, from spawner Id {} not exist at db. Spawner Position: {:?}",
            template_id,
            spawner.id,
            spawner.position()
        );
        return Ok(None);
    };
    let Some(world) = spawner.parent_world().and_then(|id| server.world.world(id)) else {
        // C#: npcSpawner.ParentWorld.Template (NullReferenceException).
        return Err(format!("spawner {} has no parent world", spawner.id));
    };
    spawn_manager::forget_object_fields(server, npc.obj_id);
    // npc.RegisterNpcEvents(): `ai::hooks::spawn_positioned` (before GoToSpawn).
    let (can_fly, model_id) = {
        let d = npc.data();
        (d.can_fly, d.unit.model_id)
    };
    // bUnderwaterCreature swims in 3D (as in `systems::ai::npc`): no ground to stand on.
    let underwater = server
        .game_data
        .systems
        .ai
        .ai
        .actor_model(model_id)
        .is_some_and(|m| m.underwater_creature);
    let mut pos = spawner.position();
    if let Some((x, y, z, yaw)) = base {
        pos.x = x;
        pos.y = y;
        pos.z = z;
        pos.yaw = yaw;
    }
    if let Some((dx, dy, dz)) = offset {
        // Formation offsets are authored in the spawner's local frame; applied as world-axis
        // deltas because yaw rotation is capture-pending (see `spawn_npc_group`).
        pos.x += dx;
        pos.y += dy;
        pos.z += dz;
    }
    if !can_fly && !underwater {
        match spawn_ground_z(server, &world, pos.x, pos.y, pos.z) {
            Some(z) => {
                pos.z = z;
                // An ordinary member keeps the C# behaviour of snapping the shared spawner
                // position; a group member must not move the whole group onto its own floor.
                if offset.is_none() {
                    spawner.set_position(pos);
                }
            }
            // A base override (the group partial respawn) over water / a missing floor must
            // not float: keep the original group spawn rule at the spawner position plus the
            // member's formation offset instead.
            None if base.is_some() => {
                pos = spawner.position();
                if let Some((dx, dy, dz)) = offset {
                    pos.x += dx;
                    pos.y += dy;
                    pos.z += dz;
                }
                if let Some(z) = spawn_ground_z(server, &world, pos.x, pos.y, pos.z) {
                    pos.z = z;
                }
            }
            None => {}
        }
    }
    // Rust only (server owner's request 2026-11-08): an ordinary spawn that would land on
    // another live NPC (the data places a spawner's NPCs and several entries on identical
    // coordinates) is scattered onto a ring around it. Formation members keep their authored
    // offsets, summons / quest spawns (an owner) their authored place, and flying / swimming
    // NPCs (no ground to snap to) their position. The spawner position itself is unchanged;
    // the AI home position is read from the transform below, so it is the scattered spot.
    if offset.is_none() && owner_id == 0 && !can_fly && !underwater {
        if let Some(params) = ScatterParams::from_config(&server.config.world) {
            if let Some((x, y, z)) =
                stacked_npcs::scatter_position(server, &world, 0, (pos.x, pos.y, pos.z), params)
            {
                pos.x = x;
                pos.y = y;
                pos.z = z;
                pos.zone_id = server
                    .game_data
                    .worlds
                    .get_zone_id(Some(&world.template), x, y);
            }
        }
    }
    {
        let mut d = npc.data();
        d.unit.owner_id = owner_id;
        // Transform.ApplyWorldSpawnPosition(Position)
        d.unit.transform.zone_id = pos.zone_id;
        d.unit
            .transform
            .set_position(pos.x, pos.y, pos.z, pos.roll, pos.pitch, pos.yaw);
        d.unit.transform.instance_id = world.id;
        d.spawner_id = spawner.id;
    }
    tracing::trace!(
        "Spawn npc templateId {} objId {} from spawnerId {} at Position: {:?}",
        template_id,
        npc.obj_id,
        spawner.spawner_id,
        pos
    );
    server
        .systems
        .npcs
        .spawns
        .set_npc_spawner(npc.obj_id, spawner.clone());
    // Ai.HomePosition / IdlePosition = world position, Ai.GoToSpawn() (systems::ai)
    if let Some(ai) = server.systems.hooks.ai.get() {
        ai.npc_spawn_positioned(server, &npc);
    }
    if let Some(t) = spawner.template() {
        spawner.set_respawn_time(random_float(t.spawn_delay_min, t.spawn_delay_max) as i32);
    }
    spawn_into_world(server, &world, &npc);
    // world.Events.OnUnitSpawn (dungeon events, systems::instances).
    crate::systems::instances::events::on_unit_spawn(server, &world, &npc);
    // RegisterNpcEvents, `npc.Simulation = new Simulation(npc)` and
    // `Ai.LoadAiPathPoints(FollowPath)` (systems::ai).
    if let Some(ai) = server.systems.hooks.ai.get() {
        ai.npc_spawned(server, &npc, &spawner.follow_path);
    }
    Ok(Some(npc))
}
