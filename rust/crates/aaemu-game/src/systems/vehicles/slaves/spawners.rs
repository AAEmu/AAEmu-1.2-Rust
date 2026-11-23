use std::sync::{Arc, Mutex};

use dashmap::DashMap;

use crate::server::GameServer;
use crate::systems::npcs::doodad::spawner::SimpleSpawnerDef;
use crate::systems::npcs::spawn_manager;
use crate::world::{SlaveRef, WorldInstance, WorldObject};

use super::create_core;

// ---------------------------------------------------------------------------------------------
// SlaveSpawner
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SpawnerState {
    spawned: Vec<u32>,
    last_spawn: Option<u32>,
    scheduled_count: i32,
    spawn_count: i32,
}

/// `SlaveSpawner` (`Spawner<Slave>` + `World`).
#[derive(Debug)]
pub struct SlaveSpawner {
    pub id: u32,
    pub world_id: u32,
    pub def: SimpleSpawnerDef,
    state: Mutex<SpawnerState>,
}

impl SlaveSpawner {
    pub fn new(world_id: u32, def: SimpleSpawnerDef) -> Self {
        Self {
            id: def.base.id,
            world_id,
            def,
            state: Mutex::new(SpawnerState::default()),
        }
    }

    /// `Spawn(objId)` (`DoSpawn`): returns `_lastSpawn`.
    pub fn spawn(self: &Arc<Self>, server: &GameServer) -> Option<u32> {
        let world = server.world.world(self.world_id)?;
        match create_core(server, &world, None, Some(self), 0, None, false, None, None) {
            None => {
                tracing::warn!(
                    "Slave {}, from spawn not exist at db",
                    self.def.base.unit_id
                );
            }
            Some(slave) => {
                let at_origin = {
                    let t = slave.data().unit.transform;
                    t.x == 0.0 && t.y == 0.0 && t.z == 0.0
                };
                if at_origin {
                    tracing::error!(
                        "Can't spawn slave {} from spawn {}",
                        self.def.base.unit_id,
                        self.id
                    );
                } else {
                    let mut st = self.state.lock().expect("spawner poisoned");
                    st.last_spawn = Some(slave.obj_id);
                    // C#: `_spawned` / counts are not updated by DoSpawn.
                }
            }
        }
        self.state.lock().expect("spawner poisoned").last_spawn
    }

    /// `DecreaseCount(slave)`: schedules the respawn (when `RespawnTime > 0` and below `Count`)
    /// and the despawn of a slave.
    pub fn decrease_count(&self, server: &GameServer, slave: &SlaveRef) {
        let Some(sm) = spawn_manager_of(server, self.world_id) else {
            return;
        };
        let now = spawn_manager::now();
        let respawn = {
            let mut st = self.state.lock().expect("spawner poisoned");
            st.spawn_count -= 1;
            st.spawned.retain(|id| *id != slave.obj_id);
            let r = self.def.base.respawn_time > 0
                && st.spawn_count + st.scheduled_count < self.def.count as i32;
            if r {
                st.scheduled_count += 1;
            }
            r
        };
        let obj = WorldObject::from(slave);
        if respawn {
            sm.add_respawn(
                server,
                obj.clone(),
                Some(spawn_manager::add_secs(
                    now,
                    f64::from(self.def.base.respawn_time),
                )),
            );
        }
        sm.add_despawn(
            server,
            obj,
            Some(spawn_manager::add_secs(
                now,
                f64::from(self.def.base.despawn_time),
            )),
        );
    }

    /// `Despawn(slave)`.
    pub fn despawn(&self, server: &GameServer, slave: &SlaveRef) {
        if let Some(world) = server.world.world_of(&WorldObject::from(slave)) {
            world.delete(slave);
        }
        let mut st = self.state.lock().expect("spawner poisoned");
        if server
            .systems
            .npcs
            .spawns
            .respawn_field(slave.obj_id)
            .is_none()
        {
            st.spawned.retain(|id| *id != slave.obj_id);
            server.ids.object.release_id(slave.obj_id);
            st.spawn_count -= 1;
        }
        if st.last_spawn.is_none_or(|l| l == slave.obj_id) {
            st.last_spawn = st.spawned.last().copied();
        }
    }
}

/// The loaded `SlaveSpawners` of every instance (`spawner id -> spawner`).
#[derive(Debug, Default)]
pub struct SlaveSpawners {
    spawners: DashMap<u32, Arc<SlaveSpawner>>,
}

impl SlaveSpawners {
    pub fn get(&self, id: u32) -> Option<Arc<SlaveSpawner>> {
        self.spawners.get(&id).map(|s| s.clone())
    }

    pub fn insert(&self, s: Arc<SlaveSpawner>) {
        self.spawners.insert(s.id, s);
    }
}

/// The slave part of `SpawnManager.SpawnAll` for one instance: `spawner.World = World;
/// spawner.Spawn(0)` per loaded spawner. Returns the number of spawners processed.
pub fn spawn_all(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    defs: Vec<SimpleSpawnerDef>,
) -> usize {
    let mut count = 0;
    for def in defs {
        let s = Arc::new(SlaveSpawner::new(world.id, def));
        server.systems.vehicles.slave_spawners.insert(s.clone());
        s.spawn(server);
        count += 1;
        if count % 25 == 0 {
            tracing::debug!(
                "In world {} Slaves spawned: {count}...",
                world.template.name
            );
        }
    }
    tracing::info!("In world {} slaves spawned: {count}", world.template.name);
    count
}

pub(super) fn spawn_manager_of(
    server: &GameServer,
    world_id: u32,
) -> Option<Arc<spawn_manager::SpawnManager>> {
    server.systems.npcs.spawns.get(world_id)
}
