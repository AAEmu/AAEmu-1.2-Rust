//! World-instance lifecycle (`WorldManager.CreateWorldInstance` / `RemoveWorld`,
//! `WorldInstance.CleanupInstance`, `WorldIdManager`) and the character transforms the
//! instance code moves around (`Character.MainWorldPosition`, `Transform.CloneDetached`,
//! `Transform.ApplyWorldSpawnPosition`, `CSInstanceLoadedPacket`).
//!
//! Moving a player between instances works as in C#: the transform gets the new instance id and
//! the client is told with `SCLoadInstancePacket`. Because that packet empties the client's world,
//! the character is re-filed in the arrival instance when the client signals `CSInstanceLoadedPacket`
//! ([`instance_loaded`]): the old instance's region grid is left first
//! (`WorldManager::leave_other_instance`, the cross-world branch of the C# region change) and the
//! character is added to the arrival instance's visibility grid, which sends the units / doodads
//! / players in view and shows the character to them without waiting for a movement packet. The
//! later moves find the character already filed and only update its region.

use std::sync::{Arc, Mutex};

use dashmap::DashMap;

use crate::data::world_templates::{WorldSpawnPosition, WorldTemplate};
use crate::models::character::DEFAULT_INSTANCE_ID;
use crate::models::transform::Transform;
use crate::packets::world::{SCDetailedTimeOfDayPacket, SCErrorMsgPacket};
use crate::server::GameServer;
use crate::systems::npcs::portal::packets::SCLoadInstancePacket;
use crate::world::{session, PlayerRef, WorldInstance, WorldObject};

use super::packets::{SCProcessingInstancePacket, SCSystemDungeonChannelPacket};

/// `WorldInstance` members the Rust `world::WorldInstance` does not carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorldInfo {
    /// `ChannelId`.
    pub channel_id: u32,
    /// `IsFixedInstanceId` (the id is not released when the instance goes away).
    pub fixed_id: bool,
}

/// Per-instance bookkeeping of this subsystem (`server.systems.instances.worlds`).
#[derive(Debug, Default)]
pub struct InstanceWorlds {
    info: DashMap<u32, WorldInfo>,
    creation: Mutex<()>,
}

impl InstanceWorlds {
    /// `WorldInstance.ChannelId` (0 for instances not created here, e.g. main_world).
    pub fn channel_id(&self, world_id: u32) -> u32 {
        self.info.get(&world_id).map_or(0, |i| i.channel_id)
    }

    pub fn info(&self, world_id: u32) -> Option<WorldInfo> {
        self.info.get(&world_id).map(|i| *i)
    }
}

/// `MathUtil.DegToRad` (float).
pub fn deg_to_rad(v: f32) -> f32 {
    std::f32::consts::PI / 180.0 * v
}

/// `ErrorMessageType` values used by this subsystem.
pub mod error_message {
    pub const NO_SERVER_INSTANCE_RESOURCE: i16 = 253;
    pub const TRY_LATER_INSTANCE: i16 = 254;
    pub const INVALID_RETURN_POS_INSTANCE: i16 = 257;
    pub const PROHIBITED_IN_INSTANCE: i16 = 258;
    pub const ALREADY_UNBOUND_INSTANCE: i16 = 259;
    pub const INSTANCE_VISIT_LIMIT: i16 = 453;
    pub const INSTANCE_LEVEL: i16 = 470;
    pub const INSTANCE_QUOTA: i16 = 471;
    pub const INSTANCE_LEAVE_PARTY: i16 = 472;
    pub const CANNOT_USE_PORTAL_IN_TRIAL: i16 = 611;
    pub const ENTER_INST_REQ_ITEM: i16 = 632;
    pub const NEED_PARTY: i16 = 671;
}

/// `character.SendErrorMessage(type, subType = 0, isNotify = true)`.
pub fn send_error(p: &PlayerRef, error: i16, kind: u32) {
    p.send(SCErrorMsgPacket::new(error, kind, true));
}

/// `WorldManager.CreateWorldInstance(template, channelId, overrideInstanceId, fixedInstanceId,
/// notifyPlayer)`: registers the instance, tells `notify` (`SCProcessingInstancePacket`) and
/// creates its `SpawnManager` (the spawn files are read by [`load_spawns`]). Templates that
/// cannot be instanced return their existing instance. `Err` = the C# exceptions (fixed id too
/// large, duplicate id, no free id).
pub fn create_world_instance(
    server: &GameServer,
    template: &WorldTemplate,
    channel_id: u32,
    override_instance_id: bool,
    fixed_instance_id: u32,
    notify: Option<&PlayerRef>,
) -> Result<Arc<WorldInstance>, String> {
    let _creation = server
        .systems
        .instances
        .worlds
        .creation
        .lock()
        .expect("world creation poisoned");
    // If it's marked as an instance or if it only has 1 zone defined, then it's a "dungeon"
    let can_be_instanced =
        template.xml_world.is_instance > 0 || template.xml_world.zones.len() <= 1;
    if !can_be_instanced {
        let mut worlds = server.world.worlds();
        worlds.sort_by_key(|w| w.id);
        if let Some(previous) = worlds.into_iter().find(|w| w.template.id == template.id) {
            tracing::warn!(
                "Tried to create a new instance of {} which does not allow multiple instances. Using InstanceId {} instead!",
                template.name,
                previous.id
            );
            return Ok(previous);
        }
    }
    if fixed_instance_id > 100 {
        return Err("Fixed Instance ID is too large".into());
    }
    if server.world.worlds().len() as u64 >= u64::from(server.config.world.max_instances) {
        return Err("Maximum number of world instances reached".into());
    }
    let id = if override_instance_id {
        fixed_instance_id
    } else {
        server.ids.world.get_next_id().map_err(|e| e.to_string())?
    };
    if server.world.has_world(id) {
        return Err(format!("World instance with id {id} already exists"));
    }
    let world = server
        .world
        .create_world_instance(Arc::new(template.clone()), id);
    server.systems.instances.worlds.info.insert(
        id,
        WorldInfo {
            channel_id,
            fixed_id: override_instance_id,
        },
    );
    if let Some(p) = notify {
        p.send(SCProcessingInstancePacket {
            zone_id: template.zone_keys.first().copied().unwrap_or(0) as i32,
        });
    }
    // world.SpawnManager = new SpawnManager(world) (Load: `load_spawns`).
    server.systems.npcs.spawns.get_or_create(&world);
    Ok(world)
}

/// `world.SpawnManager.Load()` (spawn files of `Data/Worlds/<template name>`). The load future
/// is not `Send` (its template filters), so it runs on a blocking thread.
pub async fn load_spawns(server: &Arc<GameServer>, world: &Arc<WorldInstance>) {
    let (s, w) = (server.clone(), world.clone());
    let res = tokio::task::spawn_blocking(move || {
        let manager = s.systems.npcs.spawns.get_or_create(&w);
        tokio::runtime::Handle::current().block_on(manager.load(&s))
    })
    .await;
    match res {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::error!("SpawnManager.Load for {}: {e:#}", world_name(world)),
        Err(e) => tracing::error!("SpawnManager.Load for {}: {e}", world_name(world)),
    }
}

/// `WorldInstance.ToString()` (`{Id}-{Template.Name}({Template.Id})`).
pub fn world_name(world: &WorldInstance) -> String {
    format!(
        "{}-{}({})",
        world.id, world.template.name, world.template.id
    )
}

/// `WorldManager.RemoveWorld(worldInstanceId)`.
pub fn remove_world(server: &GameServer, world_id: u32) {
    let _items = server.systems.economy.operation_scope();
    let _serial = server.systems.npcs.spawns.spawn_lock();
    super::tower_def::cleanup_world(server, world_id);
    if !server.world.remove_world(world_id) {
        tracing::info!("[Dungeon] couldn't remove the dungeon id={world_id}!");
    }
    server.systems.instances.worlds.info.remove(&world_id);
    forget_instance(server, world_id);
}

/// Drops what the other subsystems keep per instance id once the instance is gone. In C# these
/// registries were members of the `WorldInstance` and went with it; here they are keyed by the
/// instance id, so each one has to be dropped, or its periodic tick keeps asking for the
/// removed instance (`GetWorld: No such World Instance`, once per tick for as long as the server
/// ran) and a recycled instance id inherits the old instance's gimmicks, triggers and managers.
pub fn forget_instance(server: &GameServer, world_id: u32) {
    let npcs = &server.systems.npcs;
    npcs.gimmicks.remove_world(world_id);
    npcs.spheres.remove_world(world_id);
    npcs.fish_schools.remove_world(world_id);
    let vehicles = &server.systems.vehicles;
    vehicles.mates.remove_world(world_id);
    vehicles.transfers.remove_world(world_id);
    let ai = &server.systems.ai;
    ai.physics.remove_world(world_id);
    ai.collision.remove(world_id);
}

/// `WorldIdManager.ReleaseId(id)` (ids below the first id log an error, as in C#).
pub fn release_world_id(server: &GameServer, world_id: u32) {
    server.ids.world.release_id(world_id);
}

/// `WorldInstance.CleanupInstance()`: stops the respawn loop, removes the spawners and what they
/// spawned, then despawns whatever is left; the instance's `SpawnManager` is dropped.
pub fn cleanup_instance(server: &GameServer, world_id: u32) {
    let _items = server.systems.economy.operation_scope();
    let _serial = server.systems.npcs.spawns.spawn_lock();
    super::tower_def::cleanup_world(server, world_id);
    let spawns = &server.systems.npcs.spawns;
    let Some(manager) = spawns.get(world_id) else {
        return;
    };
    manager.stop();
    manager.delete_all_spawners(server);
    let n = manager.despawn_all(server);
    spawns.remove(world_id);
    tracing::debug!("Removed objects from WorldInstance {world_id} ({n} despawned)");
}

/// `WorldInstance.HasCharacter(playerId)`.
pub fn has_character(world: &WorldInstance, character_id: u32) -> bool {
    world
        .all_characters()
        .iter()
        .any(|c| c.character_id == character_id)
}

/// `WorldInstance.GetCharacterCount()`.
pub fn character_count(world: &WorldInstance) -> usize {
    world.player_count()
}

/// `WorldInstance.ListPlayerNames(max)`.
pub fn list_player_names(world: &WorldInstance, max: usize) -> String {
    let players = world.all_characters();
    if players.len() > max {
        return players.len().to_string();
    }
    if players.is_empty() {
        return "[none]".into();
    }
    players
        .iter()
        .map(|p| p.data().unit.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The world template id of an instance (`Transform.WorldId` of objects in it).
fn template_id_of(server: &GameServer, instance_id: u32) -> u32 {
    server
        .world
        .worlds()
        .into_iter()
        .find(|w| w.id == instance_id)
        .map_or(0, |w| w.template.id)
}

/// The spawn point `Dungeon.MoveCharacterTo*` uses: the `XmlWorldZones` entry with the zone key
/// (its `world_spawns.json` position, `None` when the zone has none), else the template's own.
pub fn zone_spawn_position(
    server: &GameServer,
    template: &WorldTemplate,
    zone_key: u32,
) -> Option<WorldSpawnPosition> {
    let lookups = server.game_data.worlds.world_spawn_lookups();
    let lookup = |name: &str| {
        lookups
            .iter()
            .find(|l| l.name.as_deref() == Some(name))
            .and_then(|l| l.spawn_position)
    };
    for wz in template.xml_world_zones().values() {
        if wz.zone_key == zone_key {
            return lookup(&wz.name).map(|_| wz.spawn_position);
        }
    }
    lookup(&template.name).map(|_| template.spawn_position)
}

/// `Transform.CloneDetached(character)`: the current world position as a detached transform.
/// Note (C#): the clone's `ZoneId` setter runs `character.OnZoneChange(0, zoneId)`; mirrored.
pub fn clone_detached(server: &GameServer, p: &PlayerRef) -> Transform {
    let obj = WorldObject::from(p);
    let pr = match server.world.world_of(&obj) {
        Some(w) => w.world_pos_rot(&obj),
        None => p.data().unit.transform.local(),
    };
    let t = p.data().unit.transform;
    let clone = Transform {
        instance_id: t.instance_id,
        world_id: template_id_of(server, t.instance_id),
        zone_id: t.zone_id,
        x: pr.x,
        y: pr.y,
        z: pr.z,
        rot_x: pr.rot_x,
        rot_y: pr.rot_y,
        rot_z: pr.rot_z,
        parent_obj_id: 0,
    };
    if clone.zone_id != 0 {
        session::on_zone_change(server, p, 0, clone.zone_id);
    }
    clone
}

/// `Character.MainWorldPosition` save of the instance entry paths: `MainWorldPosition =
/// Transform.CloneDetached(character)` when it was never set or the player is still in the
/// default instance (the C# rule `DoodadFuncEnterInstance` / `DoodadFuncEnterSysInstance` and
/// the system-dungeon channel pick share).
pub fn ensure_main_world_position(server: &GameServer, p: &PlayerRef) {
    let (has_mwp, instance) = {
        let c = p.data();
        (
            c.main_world_position.is_some(),
            c.unit.transform.instance_id,
        )
    };
    if !has_mwp || instance == DEFAULT_INSTANCE_ID {
        p.data().main_world_position = Some(clone_detached(server, p));
    }
}

/// `character.Transform.ApplyWorldSpawnPosition(wsp, newInstanceId)`. Note (C#): the rotation
/// is stored as given (degrees from `world_spawns.json`) and the `ZoneId` setter fires
/// `OnZoneChange(old, new)`; both mirrored (the zone change, the instance membership and the
/// instance object tables are applied by [`session::teleport_character`]).
pub fn apply_world_spawn_position(
    server: &GameServer,
    p: &PlayerRef,
    wsp: &WorldSpawnPosition,
    new_instance_id: u32,
) {
    let instance_id = if new_instance_id != 0 {
        new_instance_id
    } else {
        p.data().unit.transform.instance_id
    };
    session::teleport_character(
        server,
        p,
        Transform {
            instance_id,
            zone_id: wsp.zone_id,
            x: wsp.x,
            y: wsp.y,
            z: wsp.z,
            rot_x: wsp.roll,
            rot_y: wsp.pitch,
            rot_z: wsp.yaw,
            ..Default::default()
        },
    );
}

/// `character.Transform = character.MainWorldPosition.Clone(); Transform.InstanceId =
/// DefaultInstanceId` + `SCLoadInstancePacket(MainWorldPosition.WorldId, ZoneId, World.Position,
/// World.Rotation.DegToRad())`. `None` when there is no `MainWorldPosition` (nothing changes).
///
/// Mirrored C# quirk: the rotation (already in radians) goes through `DegToRad` again.
/// Deviation: the C# clone's new `Transform` fired `OnZoneChange(0, zoneId)` (the instance's
/// zone chat channel was never left); the Rust passes the zone actually left so its zone-group
/// buff is removed (`PORTING.md`).
pub fn return_to_main_world(server: &GameServer, p: &PlayerRef) -> Option<Transform> {
    let mwp = p.data().main_world_position?;
    p.data().set_disabled_set_position(true);
    session::teleport_character(
        server,
        p,
        Transform {
            instance_id: DEFAULT_INSTANCE_ID,
            zone_id: mwp.zone_id,
            x: mwp.x,
            y: mwp.y,
            z: mwp.z,
            rot_x: mwp.rot_x,
            rot_y: mwp.rot_y,
            rot_z: mwp.rot_z,
            ..Default::default()
        },
    );
    p.send(SCLoadInstancePacket {
        instance_id: mwp.world_id,
        zone_id: mwp.zone_id,
        x: mwp.x,
        y: mwp.y,
        z: mwp.z,
        ang_x: deg_to_rad(mwp.rot_x),
        ang_y: deg_to_rad(mwp.rot_y),
        ang_z: deg_to_rad(mwp.rot_z),
    });
    Some(mwp)
}

/// `CSInstanceLoadedPacket.Read`: `SCUnitStatePacket(ActiveChar)`, `SCDetailedTimeOfDayPacket(12)`
/// and `DisabledSetPosition = false`.
///
/// A zone load empties the client's world, so the character has to be re-filed in the instance
/// it was moved to now: leave the old instance's region grid (the stale units / doodads there are
/// removed from the client) and `AddVisibleObject` in the arrival instance, which sends the units,
/// doodads and players in view and shows the character to them without waiting for a movement
/// packet. This mirrors the later C# fix (`ef0d41982`, `Transform.FinalizeTransform()` in
/// `CSInstanceLoadedPacket`); unlike that fix's `ResendVisibleObjectsToCharacter` repaint, an
/// arrival that is already filed in the target region sends nothing, so nothing is duplicated.
///
/// The exception is a zone load *within* the instance the character is already filed in (a
/// main-world `Return` gate, recall or resurrection point next to the caster): the client still
/// emptied its world, but the region diff of `add_visible_object` only sends the regions that are
/// new to the character, so the objects of the shared neighbourhood stayed invisible until a
/// relog. That load gets the full repaint (own mates first, as after a cut scene).
pub fn instance_loaded(server: &GameServer, p: &PlayerRef) {
    let packet = {
        let mut c = p.data();
        crate::world::unit_state::unit_state(&mut c, &server.game_data, crate::world::now_ms())
    };
    // Writing the unit state can add the forced buffs (their start runs in the skills hooks).
    crate::systems::dispatch_buff_events(server, &WorldObject::from(p));
    p.send(packet);
    p.send(SCDetailedTimeOfDayPacket { time: 12.0 });
    // A load the server sent locks the position; a repeated `CSInstanceLoaded` finds it unlocked.
    let load_pending = p.data().disabled_set_position;
    p.data().set_disabled_set_position(false);
    let instance_id = p.data().unit.transform.instance_id;
    if let Some(world) = server.world.world(instance_id) {
        let reloaded_in_place =
            load_pending && p.region().is_some_and(|(filed, _, _)| filed == world.id);
        // The C# `FinalizeTransform()` after `DisabledSetPosition = false`.
        let obj = WorldObject::from(p);
        server.world.leave_other_instance(&obj, world.id);
        let changes = world.add_visible_object(&obj, &server.view_ctx());
        session::handle_zone_changes(server, changes);
        if reloaded_in_place {
            crate::systems::vehicles::mates::resend_owner_mates_spawned(server, p);
            crate::systems::npcs::portal::resend_visible_objects_to_character(server, p);
        }
        // The arrival's `SCUnitState` exchange is done: resend the bounty totals of the wanted
        // characters now in sight, and this character's total to its viewers (WP-SL3a).
        crate::systems::social::bounty::on_character_entered(server, p);
    }
    // `SC 0x207` after the load: the client only accepts the index when the instance and zone
    // match the ones the load just set, so it waits for this confirmation.
    if let Some(pending) = server
        .systems
        .instances
        .indun
        .take_channel_select(p.character_id)
    {
        if pending.instance_id == instance_id {
            p.send(SCSystemDungeonChannelPacket {
                zone_id: pending.zone_id,
                instance_id,
                instance_index: pending.index,
            });
        }
    }
    tracing::debug!("InstanceLoaded.");
}
