//! `SlaveManager` (one per world instance in C#; here the state lives on the world objects and
//! the slave spawners), `SlaveSpawner`, `SummonSlave` item handling and the `Slave` behaviour
//! (`DoDie`, `DoHpChangeTrigger` / repair points, `RegenTick`, `Save`, `OnZoneChange`).
//!
//! Locking: one object data lock at a time; world calls never hold a data lock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use crate::models::transform::Transform;
use crate::models::unit::BaseUnitType;
use crate::server::GameServer;
use crate::systems::npcs::spawn_manager;
use crate::world::{PlayerRef, SlaveRef, WorldInstance, WorldObject};

use self::mounting_geometry::bind_gap;
use super::packets::{SCMySlavePacket, SCUnitNameChangedPacket};
use super::{
    audit, bind_error, broadcast, data, effects, error, mates, packets, ship_physics, transfers,
    world_of,
};

mod damage;
mod despawn;
mod equipment;
mod mounting;
mod mounting_geometry;
mod persistence;
mod spawn;
mod spawners;
mod summon;
mod summon_placement;
mod turret;

pub use self::damage::{
    do_die, do_hp_change_trigger, regen_tick, repair_slave_items, update_slave_repair_points,
};
pub(crate) use self::damage::{repair_point_health_band, ship_repair_target_at_full_health};
pub use self::despawn::{
    delete, destroy_and_spawn, remove_active_slave, remove_and_despawn_all_active_owned_slaves,
};
pub use self::equipment::{
    change_slave_equipment, spawn_equipment_visuals_from_inventory,
    spawn_or_replace_equipment_visual,
};
pub use self::mounting::{
    bind_slave, bind_slave_by_tl, is_manned_cannon, may_man_cannon, on_character_death,
    on_zone_change, passenger_reach, riders_escape, unbind_slave, GUNNER_SEAT_OFFSET,
};
pub(crate) use self::persistence::LoadedHull;
pub use self::persistence::{
    delete_slave_by_id, on_delete_slave_item, save, save_all, snapshot, write_row, SavedChildSlave,
    SavedSlave, SlaveRow,
};
pub(crate) use self::spawn::create_core;
pub use self::spawners::{spawn_all, SlaveSpawner, SlaveSpawners};
pub use self::summon::{create, create_for_owner, create_from_skill};
pub use self::summon_placement::client_spawn_cast;
pub use self::turret::{
    flush_pending_turret_state, set_turret_state, TURRET_STATE_MIN_INTERVAL_MS,
};

#[cfg(test)]
pub(super) use self::mounting_geometry::cached_vehicle_bounds;
#[cfg(test)]
pub(crate) use self::summon_placement::overhead_clear_at;
#[cfg(test)]
pub(super) use self::summon_placement::validated_summon_placement;

#[cfg(test)]
use self::damage::repair_destroyed_slave_items;
#[cfg(test)]
use self::despawn::audit_retirement;
#[cfg(test)]
use self::summon_placement::{
    fallback_summon_point, hull_clear_at, hull_footprint, hull_probe_points, model_box_clear_at,
    overhead_clear, summon_candidate_1_2, summon_candidate_points, summon_floor, summon_model_box,
    summon_offset_z, summon_placement_clear_1_2, summon_terrain, validated_client_spawn,
    HullEnvelope, HullFootprint,
};
#[cfg(test)]
use super::data::{ShipModel, SlaveTemplate};
#[cfg(test)]
use super::packets::item_task_type;
#[cfg(test)]
use crate::models::container::slot_type;
#[cfg(test)]
use crate::models::item::{Item, ItemClass};
#[cfg(test)]
use crate::models::slave::Slave;
#[cfg(test)]
use crate::systems::skills::types::SkillCaster;
#[cfg(test)]
use crate::world::Entity;

#[cfg(test)]
mod audit_tests;
#[cfg(test)]
mod siege_kill_tests;
#[cfg(test)]
mod slave_repair_tests;
#[cfg(test)]
mod spawn_floor_tests;
#[cfg(test)]
mod summon_hull_overlap_tests;
#[cfg(test)]
mod summon_probe_tests;

/// `AttachUnitReason`.
pub mod attach_reason {
    pub const NONE: u8 = 0;
    pub const SLAVE_BINDING: u8 = 5;
    pub const NEW_MASTER: u8 = 6;
    pub const BOARD_TRANSFER: u8 = 7;
    pub const PREFAB_CHANGED: u8 = 10;
}

/// `BuffConstants` used here.
pub mod buff {
    pub const UNTOUCHABLE: u32 = 545;
    pub const DETERIORATION: u32 = 3553;
    pub const TAX_PROTECTION: u32 = 3554;
    pub const OWNERS_MARK: u32 = 4867;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SummonKey {
    Owner(u32),
    Item(u64),
}

/// Owner/item guards survive database awaits; the separate synchronous gate orders hull
/// publication with departure. No object lock or synchronous guard crosses an await.
#[derive(Debug, Default)]
pub(crate) struct SlaveSummons {
    pub(super) diagnostics: super::audit::Diagnostics,
    reservations: Mutex<HashMap<SummonKey, Weak<tokio::sync::Mutex<()>>>>,
    pub(super) departing: Mutex<HashMap<u32, u32>>,
    /// Serializes complete synchronous hull publication/removal, including nested callbacks.
    pub(super) publication_gate: spawn_manager::ReentrantLock,
    pending_saves: Mutex<HashMap<u64, Arc<Vec<SlaveRow>>>>,
    #[cfg(test)]
    pub(crate) test_loader: Mutex<Option<TestSlaveLoader>>,
    #[cfg(test)]
    pub(crate) test_after_item_validation: Mutex<Option<TestPublicationHook>>,
}

#[cfg(test)]
type TestSlaveLoad =
    dyn Fn(u32, u64) -> futures::future::BoxFuture<'static, sqlx::Result<LoadedHull>> + Send + Sync;

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestSlaveLoader(pub Arc<TestSlaveLoad>);

#[cfg(test)]
impl std::fmt::Debug for TestSlaveLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestSlaveLoader")
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestPublicationHook(pub Arc<dyn Fn(&GameServer) + Send + Sync>);

#[cfg(test)]
impl std::fmt::Debug for TestPublicationHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestPublicationHook")
    }
}

impl SlaveSummons {
    /// Observe the actual reservation table; never acquire or create an authority reservation.
    pub(super) fn audit_identity(&self, owner: u32, item: u64) -> serde_json::Value {
        let reservations = self
            .reservations
            .lock()
            .expect("slave reservations poisoned");
        let mut rows = Vec::new();
        for (key, weak) in reservations.iter() {
            let matches = match key {
                SummonKey::Owner(id) => *id == owner,
                SummonKey::Item(id) => *id == item,
            };
            if !matches {
                continue;
            }
            if let Some(lock) = weak.upgrade() {
                rows.push(serde_json::json!({"key": match key { SummonKey::Owner(_) => "owner", SummonKey::Item(_) => "item" }, "held_sample": lock.try_lock().is_err()}));
            }
        }
        rows.sort_by_key(|r| r["key"].as_str().unwrap_or_default().to_owned());
        drop(reservations);
        let saves = self.pending_saves.lock().expect("slave saves poisoned");
        let pending: Vec<_> = saves.iter().filter(|(id, rows)| **id == item || rows.iter().any(|r| r.summoner == owner)).map(|(id, rows)| serde_json::json!({"item": id, "rows": rows.iter().map(|r| serde_json::json!({"db_id": r.id, "item": r.item_id, "summoner": r.summoner, "owner_id": r.owner_id, "owner_type": r.owner_type, "attach": r.attach_point, "template": r.template_id})).collect::<Vec<_>>()})).collect();
        serde_json::json!({"reservations_sample": rows,
            "departing_connection": self.departing.lock().expect("slave departures poisoned").get(&owner).copied(),
            "pending_hull_saves": pending})
    }

    pub(crate) fn publication_scope(&self) -> spawn_manager::ReentrantGuard<'_> {
        self.publication_gate.lock()
    }

    fn reservation(&self, key: SummonKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .reservations
            .lock()
            .expect("slave reservations poisoned");
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    fn publication<'a>(
        &'a self,
        server: &GameServer,
        player: &PlayerRef,
    ) -> Option<spawn_manager::ReentrantGuard<'a>> {
        let publication = self.publication_gate.lock();
        let state = self.departing.lock().expect("slave departures poisoned");
        if state.get(&player.character_id) == Some(&player.conn.id)
            || !current_session(server, player)
        {
            return None;
        }
        Some(publication)
    }

    pub(super) fn departure<'a>(
        &'a self,
        server: &GameServer,
        player: &PlayerRef,
    ) -> Option<spawn_manager::ReentrantGuard<'a>> {
        let publication = self.publication_gate.lock();
        let mut state = self.departing.lock().expect("slave departures poisoned");
        if !current_session(server, player)
            || state.get(&player.character_id) == Some(&player.conn.id)
        {
            return None;
        }
        state.insert(player.character_id, player.conn.id);
        Some(publication)
    }

    pub(super) fn reconnect(&self, server: &GameServer, player: &PlayerRef) {
        let _items = server.systems.economy.operation_scope();
        let _publication = self.publication_gate.lock();
        let mut state = self.departing.lock().expect("slave departures poisoned");
        if !current_session(server, player) {
            return;
        }
        state.remove(&player.character_id);
        drop(state);
        for (world, root) in owned_roots(server, player.character_id) {
            let (children, doodads) = {
                let mut d = root.data();
                d.leave_task = 0;
                d.summoner_obj_id = player.obj_id;
                d.owner_obj_id = player.obj_id;
                (d.attached_slaves.clone(), d.attached_doodads.clone())
            };
            for child in children {
                if let Some(child) = world.slave(child) {
                    child.data().summoner_obj_id = player.obj_id;
                }
            }
            for doodad in doodads {
                if let Some(doodad) = world.doodad(doodad) {
                    doodad.data().owner_obj_id = player.obj_id;
                }
            }
        }
    }

    fn queue_save(&self, server: &GameServer, slave: &SlaveRef) -> Arc<Vec<SlaveRow>> {
        // Capture and enqueue in one order, so an older snapshot cannot overwrite a newer one.
        let mut pending = self.pending_saves.lock().expect("slave saves poisoned");
        let rows = Arc::new(snapshot(server, slave));
        if let Some(row) = rows.first().filter(|row| row.item_id != 0) {
            pending.insert(row.item_id, rows.clone());
        }
        rows
    }
}

fn current_session(server: &GameServer, player: &PlayerRef) -> bool {
    server
        .world
        .character_by_obj_id(player.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, player))
}
// ---------------------------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------------------------

/// `World.GetAllSlaves()`.
pub fn all_slaves(world: &WorldInstance) -> Vec<SlaveRef> {
    world
        .all_of_kind(crate::world::ObjectKind::Slave)
        .into_iter()
        .filter_map(|o| o.as_slave().cloned())
        .collect()
}

/// `GetActiveSlaveByOwnerObjId(objId)`.
pub fn get_active_slave_by_owner_obj_id(world: &WorldInstance, obj_id: u32) -> Option<SlaveRef> {
    all_slaves(world).into_iter().find(|s| {
        let d = s.data();
        d.owner_type == BaseUnitType::Character
            && d.summoner_obj_id != 0
            && d.summoner_obj_id == obj_id
            && !d.unit.is_dead()
    })
}

/// Persistent owner identity, across instances. Cannon children share the summoner but are
/// owned by a slave, so they must never stand in for a summoned root.
pub(super) fn owned_roots(
    server: &GameServer,
    owner_id: u32,
) -> Vec<(Arc<WorldInstance>, SlaveRef)> {
    server
        .world
        .worlds()
        .into_iter()
        .flat_map(|world| {
            all_slaves(&world)
                .into_iter()
                .filter(|slave| {
                    let d = slave.data();
                    d.owner_type == BaseUnitType::Character
                        && d.summoner_id == owner_id
                        && !d.unit.is_dead()
                })
                .map(move |slave| (world.clone(), slave))
        })
        .collect()
}

pub(super) fn item_has_other_live_root(server: &GameServer, item_id: u64, owner_id: u32) -> bool {
    server.world.worlds().into_iter().any(|world| {
        all_slaves(&world).into_iter().any(|slave| {
            let d = slave.data();
            d.owner_type == BaseUnitType::Character
                && !d.unit.is_dead()
                && d.summoning_item_id == item_id
                && d.summoner_id != owner_id
        })
    })
}

/// `GetActiveSlavesByKinds(kinds, worldId)` (`u32::MAX` = every world template).
pub fn get_active_slaves_by_kinds(
    server: &GameServer,
    world: &WorldInstance,
    kinds: &[u32],
    world_id: u32,
) -> Vec<SlaveRef> {
    let data = &server.game_data.systems.vehicles;
    all_slaves(world)
        .into_iter()
        .filter(|s| {
            let (tpl, wid) = {
                let d = s.data();
                (d.unit.template_id, d.unit.transform.world_id)
            };
            data.slave_template(tpl)
                .is_some_and(|t| kinds.contains(&t.slave_kind))
                && (world_id == u32::MAX || wid == world_id)
        })
        .collect()
}

/// `GetSlaveByTlId(tlId)`.
pub fn get_slave_by_tl_id(world: &WorldInstance, tl_id: u32) -> Option<SlaveRef> {
    all_slaves(world)
        .into_iter()
        .find(|s| u32::from(s.data().unit.tl_id) == tl_id)
}

/// `GetSlaveByDbId(dbId)`.
pub fn get_slave_by_db_id(world: &WorldInstance, db_id: u32) -> Option<SlaveRef> {
    all_slaves(world).into_iter().find(|s| s.data().id == db_id)
}

/// `GetIsMounted(objId, out attachPoint)`.
pub fn get_is_mounted(world: &WorldInstance, obj_id: u32) -> Option<(SlaveRef, u8)> {
    for s in all_slaves(world) {
        let p = s
            .data()
            .attached_characters
            .iter()
            .find(|(_, c)| *c == obj_id)
            .map(|(p, _)| *p);
        if let Some(p) = p {
            return Some((s, p));
        }
    }
    None
}

fn player_world(server: &GameServer, p: &PlayerRef) -> Option<Arc<WorldInstance>> {
    world_of(server, &WorldObject::from(p))
}

/// `CSChangeSlaveNamePacket`: rename an owned summoned slave and persist its saved name.
pub fn rename_slave(server: &GameServer, owner: &PlayerRef, tl_id: u16, new_name: &str) -> bool {
    let Some(world) = player_world(server, owner) else {
        return false;
    };
    let Some(slave) = get_slave_by_tl_id(&world, u32::from(tl_id)) else {
        return false;
    };
    if slave.data().owner_obj_id != owner.obj_id {
        return false;
    }
    if new_name.trim().is_empty()
        || !server
            .systems
            .vehicles
            .mates
            .name_regex(server)
            .is_match(new_name)
    {
        return false;
    }
    let name = crate::managers::name::normalize_name(new_name);
    slave.data().unit.name = name.clone();
    broadcast(
        server,
        &WorldObject::from(&slave),
        &SCUnitNameChangedPacket {
            obj_id: slave.obj_id,
            name,
        },
        true,
    );
    save(server, &slave);
    true
}

/// `SlaveManager.GetIsMounted(character.ObjId) ?? GetActiveSlaveByOwnerObjId(character.ObjId)`.
pub fn character_slave(server: &GameServer, p: &PlayerRef) -> Option<WorldObject> {
    let world = player_world(server, p)?;
    get_is_mounted(&world, p.obj_id)
        .map(|(s, _)| s)
        .or_else(|| get_active_slave_by_owner_obj_id(&world, p.obj_id))
        .map(WorldObject::from)
}

/// `SlaveTemplate.SlaveKind` (0 when the template is not loaded).
pub fn slave_kind(server: &GameServer, slave: &SlaveRef) -> u32 {
    server
        .game_data
        .systems
        .vehicles
        .slave_template(slave.data().unit.template_id)
        .map_or(0, |t| t.slave_kind)
}

/// A cannon (slave kind 7 siege weapon / 8 slave equipment) has only its gunner seat at point
/// 1, never a passenger seat.
pub fn is_cannon_kind(kind: u32) -> bool {
    matches!(
        kind,
        super::data::slave_kind::SIEGE_WEAPON | super::data::slave_kind::SLAVE_EQUIPMENT
    )
}

/// `PositionAndRotation.AddDistanceToFront(distance)`.
pub fn add_distance_to_front(t: &mut Transform, distance: f32) {
    t.x += -distance * t.rot_z.sin();
    t.y += distance * t.rot_z.cos();
}

/// `PositionAndRotation.AddDistanceToRight(distance)`.
pub fn add_distance_to_right(t: &mut Transform, distance: f32) {
    t.x += distance * t.rot_z.cos();
    t.y += distance * t.rot_z.sin();
}

/// `SendMySlavePacketToAllOwners()` (`SendMySlaveTask`, every 5 s).
pub fn send_my_slave_packet_to_all_owners(server: &GameServer) {
    for world in server.world.worlds() {
        for slave in all_slaves(&world) {
            let obj = WorldObject::from(&slave);
            let (summoner, p) = {
                let d = slave.data();
                if d.owner_type != BaseUnitType::Character {
                    continue;
                }
                (
                    d.summoner_obj_id,
                    (
                        d.unit.tl_id,
                        d.unit.name.clone(),
                        d.unit.template_id,
                        d.unit.hp,
                        d.unit.max_hp,
                    ),
                )
            };
            if summoner == 0 {
                continue;
            }
            let Some(owner) = server.world.character_by_obj_id(summoner) else {
                continue;
            };
            let w = world.world_pos_rot(&obj);
            owner.send(SCMySlavePacket {
                obj_id: slave.obj_id,
                tl_id: p.0,
                name: p.1,
                template_id: p.2,
                hp: p.3,
                max_hp: p.4,
                x: w.x,
                y: w.y,
                z: w.z,
            });
        }
    }
}
