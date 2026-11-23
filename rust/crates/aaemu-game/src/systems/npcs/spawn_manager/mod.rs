//! `SpawnManager` (`Core/Managers/World/SpawnManager.cs`), one per world insance, kept in
//! [`SpawnManagers`] (`server.systems.npcs.spawns`), plus the spawner part of
//! `WorldManager.ActiveRegionTick` and the C# object-graph links the spawners need:
//!
//! * `npc.Spawner` / `doodad.Spawner` ([`SpawnManagers::npc_spawner_of`] /
//!   [`SpawnManagers::doodad_spawner_of`], by object id);
//! * `GameObject.Respawn` / `GameObject.Despawn` (the last time set through
//!   [`SpawnManager::add_respawn`] / [`SpawnManager::add_despawn`]; `None` = `MinValue`);
//! * the static `NpcSpawner._spawnLock` ([`SpawnManagers::spawn_lock`], reentrant like C# `lock`).
//!
//! The active-region tick is the bounded activation pass (`active_region_tick_stats`): ordinary
//! spawners are indexed per instance and 64 m region and queried only around present players, on
//! player activity / membership / config changes; each pass creates at most
//! `World.NpcSpawnsPerTick` NPCs (`visible neighbourhoods before prewarm rings, nearest first`,
//! rotating fairly across instances). A prepared placement is tracked in the warmed registry:
//! when it drops out of every player's warm set it keeps its NPCs for
//! `World.NpcWarmRetentionSeconds` and is then reclaimed gradually within the same per-tick
//! budget; ordinary quest givers / reporters and service roles stay resident per live member
//! after their first preparation (`World.NpcResidentServiceNpcs`), so a mixed placement still
//! expires its nonresident members. Event / tower / dynamic spawners keep their own
//! triggers and are not indexed.
//!
//! Object ids are reused after `ObjectIdManager.ReleaseId`, so the links / fields of an id are
//! dropped when this module releases it ([`forget_object`]) and when a spawner creates a new
//! object with it. Other modules that release spawned objects' ids call [`forget_object`].
//! (Loot containers are dropped by the object id manager's release observer, see
//! `LootManager::install_release_hook`, so no release path can miss them.)
//!
//! Times use `tokio::time::Instant` (`DateTime.UtcNow` in C#) so tests can drive them.

mod activation;
mod loading;
mod persistent_doodads;
mod timers;

pub use self::activation::ActivationStats;
use self::activation::{ActivationState, QuestGiverIndex};
pub use self::loading::{LoadStats, TemplateExists};
#[cfg(test)]
pub(crate) use self::persistent_doodads::PersistentDoodadLoad;
pub(crate) use self::persistent_doodads::PersistentDoodads;
use super::npc;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::ThreadId;
use std::time::Duration;

use dashmap::DashMap;
use tokio::time::Instant;

use crate::config::WorldConfig;
use crate::data::world_templates::REGION_SIZE;
use crate::models::doodad::doodad_owner_type;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::instances::encounters::registry as encounters;
use crate::systems::npcs::doodad::spawner::{
    parse_list, DoodadSpawner, DoodadSpawnerDef, NpcSpawnerDef, SimpleSpawnerDef,
};
use crate::systems::npcs::gimmick::spawner::{parse_gimmick_spawners, GimmickSpawner};
use crate::systems::npcs::npc::residency::ResidentNpcTemplates;
use crate::systems::npcs::npc::spawner::{
    npc_is_resident, NpcSpawner, NpcSpawnerInit, NpcSpawnerShared,
};
use crate::systems::npcs::npc::spawner_data::{
    npc_spawner_category, NpcSpawnerNpc, NpcSpawnerTemplate,
};
use crate::world::{DoodadRef, Entity, NpcRef, ObjectKind, WorldInstance, WorldObject};

/// `s_fakeSpawnerId` start value.
pub const FIRST_FAKE_SPAWNER_ID: u32 = 9_000_001;

/// Upper bound of `World.NpcPrewarmRegionMargin`: each extra ring adds regions (and candidate
/// spawners) quadratically, so a pathological configuration is clamped to a modest ring.
pub const MAX_PREWARM_REGION_MARGIN: u32 = 4;

/// The configured prewarm margin, clamped to [`MAX_PREWARM_REGION_MARGIN`].
pub fn prewarm_region_margin(config: &WorldConfig) -> u32 {
    config
        .npc_prewarm_region_margin
        .min(MAX_PREWARM_REGION_MARGIN)
}

/// The activation tick interval from `World.NpcSpawnTickMs`: `0` falls back to the 250 ms
/// default (never a busy spin) and extreme values are clamped to a safe range.
pub fn spawn_tick_interval(config: &WorldConfig) -> Duration {
    let ms = config.npc_spawn_tick_ms;
    let ms = if ms == 0 { 250 } else { ms.clamp(50, 60_000) };
    Duration::from_millis(ms)
}

// ---------------------------------------------------------------------------------------------
// time helpers
// ---------------------------------------------------------------------------------------------

/// `DateTime.UtcNow` for spawn timing (tests shift it with [`advance_test_clock`]).
pub fn now() -> Instant {
    #[cfg(test)]
    {
        Instant::now() + TEST_CLOCK_OFFSET.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        Instant::now()
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CLOCK_OFFSET: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

/// Moves this thread's spawn clock forward (tests; `#[tokio::test]` runs on one thread).
#[cfg(test)]
pub fn advance_test_clock(d: Duration) {
    TEST_CLOCK_OFFSET.with(|c| c.set(c.get() + d));
}

/// `t.AddSeconds(secs)` (negative values move back; clamped at the clock origin).
pub fn add_secs(t: Instant, secs: f64) -> Instant {
    if !secs.is_finite() {
        return t;
    }
    if secs >= 0.0 {
        t + Duration::from_secs_f64(secs)
    } else {
        t.checked_sub(Duration::from_secs_f64(-secs)).unwrap_or(t)
    }
}

/// Unix seconds (the `GameScheduleManager` queries).
pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `MathF.PI / 180f * val` (`NumericExtensions.DegToRad(float)`).
pub fn deg_to_rad(v: f32) -> f32 {
    std::f32::consts::PI / 180.0 * v
}

// ---------------------------------------------------------------------------------------------
// reentrant lock (C# `lock`)
// ---------------------------------------------------------------------------------------------

/// A reentrant mutex (C# `Monitor`) for the static `NpcSpawner._spawnLock`. Never held across an
/// `.await`.
#[derive(Debug, Default)]
pub struct ReentrantLock {
    owner: Mutex<(Option<ThreadId>, usize)>,
    cv: Condvar,
}

/// Guard of [`ReentrantLock`].
#[derive(Debug)]
pub struct ReentrantGuard<'a>(&'a ReentrantLock);

impl ReentrantLock {
    pub fn lock(&self) -> ReentrantGuard<'_> {
        let me = std::thread::current().id();
        let mut st = self.owner.lock().expect("spawn lock poisoned");
        loop {
            match st.0 {
                None => {
                    *st = (Some(me), 1);
                    break;
                }
                Some(o) if o == me => {
                    st.1 += 1;
                    break;
                }
                Some(_) => st = self.cv.wait(st).expect("spawn lock poisoned"),
            }
        }
        ReentrantGuard(self)
    }
}

impl Drop for ReentrantGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.0.owner.lock().expect("spawn lock poisoned");
        st.1 -= 1;
        if st.1 == 0 {
            st.0 = None;
            self.0.cv.notify_one();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// backend: the other helpers' managers (+ a test seam while they are stubs)
// ---------------------------------------------------------------------------------------------

/// Calls into `NpcManager` / `DoodadManager` (tests can install a [`backend::TestBackend`] per
/// server while those managers are stubs).
pub mod backend {
    use std::sync::Arc;

    use crate::server::GameServer;
    use crate::world::{DoodadRef, NpcRef, WorldInstance, WorldObject};

    /// Test replacement of the NPC / doodad managers' `Create` (spawn / delete then use the
    /// plain world API).
    #[cfg(test)]
    pub trait TestBackend: Send + Sync {
        fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef>;
        fn doodad_create(
            &self,
            server: &GameServer,
            world: &Arc<WorldInstance>,
            obj_id: u32,
            template_id: u32,
        ) -> Option<DoodadRef>;
    }

    #[cfg(test)]
    fn test_backend(server: &GameServer) -> Option<Arc<dyn TestBackend>> {
        server
            .systems
            .npcs
            .spawns
            .test_backend
            .lock()
            .expect("poisoned")
            .clone()
    }

    /// `NpcManager.Instance.Create(world, objId, templateId)`.
    pub fn npc_create(server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
        #[cfg(test)]
        if let Some(b) = test_backend(server) {
            return b.npc_create(server, obj_id, template_id);
        }
        crate::systems::npcs::npc::manager::create(server, obj_id, template_id)
    }

    /// `DoodadManager.Instance.Create(world, objId, templateId, owner, skipPhaseInit)`.
    pub fn doodad_create(
        server: &GameServer,
        world: &Arc<WorldInstance>,
        obj_id: u32,
        template_id: u32,
        owner: Option<&WorldObject>,
        skip_phase_init: bool,
    ) -> Option<DoodadRef> {
        #[cfg(test)]
        if let Some(b) = test_backend(server) {
            return b.doodad_create(server, world, obj_id, template_id);
        }
        crate::systems::npcs::doodad::manager::create(
            server,
            world,
            obj_id,
            template_id,
            owner,
            skip_phase_init,
        )
    }

    /// `Doodad.Spawn()`.
    pub fn doodad_spawn(server: &GameServer, world: &Arc<WorldInstance>, doodad: &DoodadRef) {
        #[cfg(test)]
        if test_backend(server).is_some() {
            world.spawn(doodad, &server.view_ctx());
            return;
        }
        crate::systems::npcs::doodad::manager::spawn(server, world, doodad)
    }

    /// `Doodad.Delete()`.
    pub fn doodad_delete(server: &GameServer, world: &Arc<WorldInstance>, doodad: &DoodadRef) {
        #[cfg(test)]
        if test_backend(server).is_some() {
            world.delete(doodad);
            return;
        }
        crate::systems::npcs::doodad::manager::delete(server, world, doodad)
    }
}

// ---------------------------------------------------------------------------------------------
// SpawnManagers
// ---------------------------------------------------------------------------------------------

/// `WorldInstance.SpawnManager` of every instance + the cross-instance spawner state.
pub struct SpawnManagers {
    managers: DashMap<u32, Arc<SpawnManager>>,
    /// `NpcSpawner._spawnLock` (static).
    spawn_lock: ReentrantLock,
    /// `npc.Spawner`.
    npc_spawner_of: DashMap<u32, Arc<NpcSpawner>>,
    /// `SpawnEffect` / `NpcSpawnerSpawnEffect` `DespawnOnCreatorDeath` summons: creator obj id
    /// -> the NPCs spawned by its effects. Weak handles: the world object registry owns the
    /// NPCs, so a despawned summon simply stops upgrading.
    creator_summons: Mutex<HashMap<u32, Vec<Weak<Entity<Npc>>>>>,
    /// `doodad.Spawner`.
    doodad_spawner_of: DashMap<u32, Arc<DoodadSpawner>>,
    /// `GameObject.Respawn` (absent = `DateTime.MinValue`).
    respawn_fields: DashMap<u32, Instant>,
    /// `GameObject.Despawn` (absent = `DateTime.MinValue`).
    despawn_fields: DashMap<u32, Instant>,
    /// `SpawnManager.s_fakeSpawnerId` (static, shared by all instances).
    fake_spawner_id: AtomicU32,
    /// Immutable quest-giver / reporter + service-role template snapshot, built once from the
    /// static game data (`ResidentNpcTemplates`; never rescanned per tick).
    resident_npcs: OnceLock<ResidentNpcTemplates>,
    active_tick_started: AtomicBool,
    /// Fairness cursor of the activation pass: the instance it starts with rotates every pass so
    /// a small `World.NpcSpawnsPerTick` budget cannot starve later instances.
    activation_rotation: AtomicU32,
    #[cfg(test)]
    pub(crate) test_backend: Mutex<Option<Arc<dyn backend::TestBackend>>>,
    #[cfg(test)]
    pub(crate) test_persistent_loader: Mutex<Option<Arc<PersistentDoodadLoad>>>,
}

impl std::fmt::Debug for SpawnManagers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnManagers")
            .field("instances", &self.managers.len())
            .field("npc_links", &self.npc_spawner_of.len())
            .field("doodad_links", &self.doodad_spawner_of.len())
            .finish()
    }
}

impl Default for SpawnManagers {
    fn default() -> Self {
        Self {
            managers: DashMap::new(),
            spawn_lock: ReentrantLock::default(),
            npc_spawner_of: DashMap::new(),
            creator_summons: Mutex::new(HashMap::new()),
            doodad_spawner_of: DashMap::new(),
            respawn_fields: DashMap::new(),
            despawn_fields: DashMap::new(),
            fake_spawner_id: AtomicU32::new(FIRST_FAKE_SPAWNER_ID),
            resident_npcs: OnceLock::new(),
            active_tick_started: AtomicBool::new(false),
            activation_rotation: AtomicU32::new(0),
            #[cfg(test)]
            test_backend: Mutex::new(None),
            #[cfg(test)]
            test_persistent_loader: Mutex::new(None),
        }
    }
}

impl SpawnManagers {
    /// `world.SpawnManager`.
    pub fn get(&self, world_id: u32) -> Option<Arc<SpawnManager>> {
        self.managers.get(&world_id).map(|m| m.clone())
    }

    /// `world.SpawnManager = new SpawnManager(world)` (existing manager kept).
    pub fn get_or_create(&self, world: &Arc<WorldInstance>) -> Arc<SpawnManager> {
        self.managers
            .entry(world.id)
            .or_insert_with(|| Arc::new(SpawnManager::new(world.clone())))
            .clone()
    }

    /// Whether the 1 s active-region ticker runs (`WorldManager.Initialize` subscription; tests).
    #[cfg(test)]
    pub(crate) fn active_tick_started(&self) -> bool {
        self.active_tick_started.load(Ordering::SeqCst)
    }

    /// Drops an instance's manager (`WorldInstance` removed; systems::instances).
    pub fn remove(&self, world_id: u32) -> Option<Arc<SpawnManager>> {
        self.managers.remove(&world_id).map(|(_, m)| m)
    }

    /// Every instance's manager.
    pub fn all(&self) -> Vec<Arc<SpawnManager>> {
        let mut v: Vec<_> = self.managers.iter().map(|m| m.clone()).collect();
        v.sort_by_key(|m| m.world.id);
        v
    }

    /// `lock (_spawnLock)`. Callback-bearing entries acquire the economy operation scope
    /// before this lock and before their own manager/object locks. Keep this primitive bare:
    /// pure bookkeeping does not enter the economy, and a caller under a character lock
    /// cannot safely acquire the item scope here. Quest NPC components remain deferred.
    pub fn spawn_lock(&self) -> ReentrantGuard<'_> {
        self.spawn_lock.lock()
    }

    /// `npc.Spawner`.
    pub fn npc_spawner_of(&self, obj_id: u32) -> Option<Arc<NpcSpawner>> {
        self.npc_spawner_of.get(&obj_id).map(|s| s.clone())
    }

    /// `npc.Spawner = spawner`.
    pub fn set_npc_spawner(&self, obj_id: u32, spawner: Arc<NpcSpawner>) {
        self.npc_spawner_of.insert(obj_id, spawner);
    }

    /// Records an NPC spawned by a `SpawnEffect` / `NpcSpawnerSpawnEffect` with
    /// `DespawnOnCreatorDeath` under the caster's obj id (`0` = no creator, ignored so a
    /// creator-less effect can never despawn on an unrelated unit death). A summon cannot be
    /// its own creator.
    pub fn add_creator_summon(&self, creator_obj_id: u32, npc: &NpcRef) {
        if creator_obj_id == 0 || creator_obj_id == npc.obj_id {
            return;
        }
        let mut map = self
            .creator_summons
            .lock()
            .expect("creator summons poisoned");
        let list = map.entry(creator_obj_id).or_default();
        // Expired entries (the summon despawned first) must not accumulate.
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(npc));
    }

    /// Takes the still-live summons registered to `creator_obj_id` (the entry is consumed: a
    /// dying creator is handled once).
    pub fn take_creator_summons(&self, creator_obj_id: u32) -> Vec<NpcRef> {
        self.creator_summons
            .lock()
            .expect("creator summons poisoned")
            .remove(&creator_obj_id)
            .map(|list| list.into_iter().filter_map(|w| w.upgrade()).collect())
            .unwrap_or_default()
    }

    /// `doodad.Spawner`.
    pub fn doodad_spawner_of(&self, obj_id: u32) -> Option<Arc<DoodadSpawner>> {
        self.doodad_spawner_of.get(&obj_id).map(|s| s.clone())
    }

    /// `doodad.Spawner = spawner`.
    pub fn set_doodad_spawner(&self, obj_id: u32, spawner: Arc<DoodadSpawner>) {
        self.doodad_spawner_of.insert(obj_id, spawner);
    }

    /// `obj.Respawn` (`None` = `MinValue`).
    pub fn respawn_field(&self, obj_id: u32) -> Option<Instant> {
        self.respawn_fields.get(&obj_id).map(|t| *t)
    }

    /// `obj.Despawn` (`None` = `MinValue`).
    pub fn despawn_field(&self, obj_id: u32) -> Option<Instant> {
        self.despawn_fields.get(&obj_id).map(|t| *t)
    }

    /// `obj.Despawn = at` (the field only; an already scheduled despawn reads it).
    pub fn set_despawn_field(&self, obj_id: u32, at: Instant) {
        self.despawn_fields.insert(obj_id, at);
    }

    /// Drops the spawner links, the creator-summon registrations and the `Respawn` / `Despawn`
    /// fields of a released object id.
    pub fn forget_object(&self, obj_id: u32) {
        self.npc_spawner_of.remove(&obj_id);
        self.creator_summons
            .lock()
            .expect("creator summons poisoned")
            .remove(&obj_id);
        self.doodad_spawner_of.remove(&obj_id);
        self.respawn_fields.remove(&obj_id);
        self.despawn_fields.remove(&obj_id);
    }

    /// The next `s_fakeSpawnerId` (not advanced).
    pub fn peek_fake_spawner_id(&self) -> u32 {
        self.fake_spawner_id.load(Ordering::SeqCst)
    }

    /// The immutable resident-template snapshot (ordinary quest givers / reporters and service
    /// roles), derived once from the static game data; the activation client code calls this
    /// instead of rescanning the quest acts.
    pub fn resident_npcs(&self, server: &GameServer) -> &ResidentNpcTemplates {
        self.resident_npcs
            .get_or_init(|| ResidentNpcTemplates::from_game_data(&server.game_data))
    }
}

/// [`SpawnManagers::forget_object`] on the server's managers.
pub fn forget_object(server: &GameServer, obj_id: u32) {
    server.systems.npcs.spawns.forget_object(obj_id);
}

/// Resets the `Respawn` / `Despawn` fields (and any stale looting container) of a freshly
/// created object (its id may be reused).
pub fn forget_object_fields(server: &GameServer, obj_id: u32) {
    let s = &server.systems.npcs.spawns;
    s.respawn_fields.remove(&obj_id);
    s.despawn_fields.remove(&obj_id);
    if let Some(h) = server.systems.npcs.hooks.economy.get() {
        h.forget_loot(server, obj_id);
    }
}

/// `obj.ParentWorld.SpawnManager` (by the object's world, else `fallback_world`).
pub fn manager_of(
    server: &GameServer,
    obj: &WorldObject,
    fallback_world: Option<u32>,
) -> Option<Arc<SpawnManager>> {
    let id = server
        .world
        .world_of(obj)
        .map(|w| w.id)
        .or(fallback_world)?;
    server.systems.npcs.spawns.get(id)
}

/// The instance a spawner belongs to (`ParentWorld`). Unlike [`crate::world::WorldManager::world`]
/// this stays quiet for a since-removed dungeon; `None` for a detached spawner.
pub fn owning_world(server: &GameServer, spawner: &NpcSpawner) -> Option<Arc<WorldInstance>> {
    let id = spawner.parent_world()?;
    if let Some(m) = server.systems.npcs.spawns.get(id) {
        return Some(m.world.clone());
    }
    server.world.worlds().into_iter().find(|w| w.id == id)
}

/// `GameObject.Delete()` in the object's world (or `fallback_world`). Doodads go through the
/// doodad manager.
pub fn delete_object(server: &GameServer, obj: &WorldObject, fallback_world: Option<u32>) {
    let world = server
        .world
        .world_of(obj)
        .or_else(|| fallback_world.and_then(|id| server.world.world(id)));
    let Some(world) = world else {
        return;
    };
    match obj {
        WorldObject::Doodad(d) => backend::doodad_delete(server, &world, d),
        // Npc.Delete detaches the AI; Slave / Mate have no Delete override.
        WorldObject::Npc(n) => crate::systems::npcs::npc::manager::delete(server, &world, n),
        o => world.delete(o.clone()),
    }
}

// ---------------------------------------------------------------------------------------------
// SpawnManager
// ---------------------------------------------------------------------------------------------

/// The spawner of a scheduled object, read when it was scheduled (`obj.Spawner`).
#[derive(Debug, Clone)]
pub enum SpawnerLink {
    Npc(Arc<NpcSpawner>),
    Doodad(Arc<DoodadSpawner>),
    None,
}

#[derive(Debug, Clone)]
struct Scheduled {
    obj: WorldObject,
    link: SpawnerLink,
    /// `obj.Respawn` / `obj.Despawn` (`None` = `MinValue`).
    at: Option<Instant>,
    /// `HashSet` enumeration order (insertion).
    seq: u64,
}

#[derive(Debug, Default)]
struct Inner {
    npc_spawners: BTreeMap<u32, Vec<Arc<NpcSpawner>>>,
    npc_event_spawners: BTreeMap<u32, Vec<Arc<NpcSpawner>>>,
    tower_def_spawners: BTreeMap<u32, Vec<Arc<NpcSpawner>>>,
    /// Entries of NPCs that are members of a tower defense `NpcGroup`, held back from the
    /// ordinary spawns until [`SpawnManager::resolve_tower_groups`] knows their group sites.
    tower_group_entries: Vec<NpcSpawnerDef>,
    doodad_spawners: BTreeMap<u32, Arc<DoodadSpawner>>,
    transfer_spawners: BTreeMap<u32, SimpleSpawnerDef>,
    gimmick_spawners: BTreeMap<u32, GimmickSpawner>,
    slave_spawners: BTreeMap<u32, SimpleSpawnerDef>,
    player_doodads: Vec<DoodadRef>,
    /// `_nextId`.
    next_id: u32,
}

/// `SpawnManager`.
#[derive(Debug)]
pub struct SpawnManager {
    /// `World` (the owning instance).
    pub world: Arc<WorldInstance>,
    inner: Mutex<Inner>,
    respawns: Mutex<HashMap<u32, Scheduled>>,
    despawns: Mutex<HashMap<u32, Scheduled>>,
    seq: AtomicU64,
    /// `_work`.
    work: AtomicBool,
    /// `_loaded`.
    loaded: AtomicBool,
    /// `_lockSpawner`.
    lock_spawner: Mutex<()>,
    /// `SpawnTasks`.
    spawn_tasks: Mutex<Vec<tokio::task::JoinHandle<usize>>>,
    /// The activation region index, candidate queues and warmed registry (see [`ActivationState`]).
    activation: Mutex<ActivationState>,
    /// Bumped whenever the ordinary `NpcSpawners` membership changes; invalidates the index and
    /// the candidate queues without rebuilding them per tick.
    activation_generation: AtomicU64,
    /// Quest-map streaming candidates (see [`QuestGiverIndex`]).
    quest_giver_index: Mutex<QuestGiverIndex>,
}

/// Why a plain (no `NpcSpawnerIds`) spawn-file row must not create a game-data spawner when a
/// world or dungeon instance starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnerSkip {
    /// Category-0 (`NORMAL`) spawner marked `연출` ("staging/cutscene") in its name or comment
    /// (e.g. `instance_cuttingwind_deadmine_alistair0` / `연출용 알리스테어0`): a cutscene actor
    /// placed for a scripted scene, not for world spawning. Skipped in every world.
    Cutscene,
    /// `npc_spawners.activation_state = 'f'` in a dungeon instance: the spawner starts inactive
    /// and appears only through its activating skill / indun event, whose row names the spawner
    /// id itself. Kept in open worlds, where 'f' does not mean "absent at start" (see
    /// [`skips_inactive_at_start`]).
    InactiveAtStart,
}

/// Whether a plain row may skip `activation_state = 'f'` spawners in this world.
///
/// Only dungeon instances: their spawners are placed for an indun event / `NpcSpawnerSpawnEffect`
/// that names the spawner id. In the open worlds 'f' does not mean "absent at start":
/// `arche_mall_world`'s vendors (Wardrobe Stylist, Fairmoon Clerks, Veroe Weapons Dealer, Premium
/// Potion Merchant, ...) and `racing_test_world`'s NPCs are all 'f', and so are `main_world`'s
/// quest givers (Liteia, Baron Alfonso, Bard Pagos, the Alliance Quartermasters, the five capital
/// Arena Managers).
fn skips_inactive_at_start(world_name: &str) -> bool {
    world_name.starts_with("instance_") && !encounters::keeps_inactive_spawners(world_name)
}

/// [`SpawnerSkip`] for a plain row in `world_name`, `None` when the spawner may start.
pub(crate) fn spawner_skip(world_name: &str, template: &NpcSpawnerTemplate) -> Option<SpawnerSkip> {
    let cutscene = template.npc_spawner_category_id == npc_spawner_category::NORMAL
        && (template.name.contains("연출") || template.comment.contains("연출"));
    if cutscene {
        return Some(SpawnerSkip::Cutscene);
    }
    if !template.activation_state && skips_inactive_at_start(world_name) {
        return Some(SpawnerSkip::InactiveAtStart);
    }
    None
}

impl SpawnManager {
    pub fn new(world: Arc<WorldInstance>) -> Self {
        Self {
            world,
            inner: Mutex::new(Inner {
                next_id: 1,
                ..Inner::default()
            }),
            respawns: Mutex::new(HashMap::new()),
            despawns: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            work: AtomicBool::new(true),
            loaded: AtomicBool::new(false),
            lock_spawner: Mutex::new(()),
            spawn_tasks: Mutex::new(Vec::new()),
            activation: Mutex::new(ActivationState::default()),
            activation_generation: AtomicU64::new(0),
            quest_giver_index: Mutex::new(QuestGiverIndex::default()),
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("spawn manager poisoned")
    }

    // ----- SpawnAll -----

    /// `SpawnAll()`: doodads, transfers, gimmicks, slaves and player doodads on background
    /// tasks (`SpawnTasks`). NPCs are not spawned here: the active-region tick does it.
    pub fn spawn_all(self: &Arc<Self>, server: &Arc<GameServer>) {
        tracing::info!("Spawning NPCs...");
        tracing::info!("Spawning Doodads...");
        let mut tasks = Vec::new();
        {
            let (me, server) = (self.clone(), server.clone());
            tasks.push(tokio::task::spawn_blocking(move || {
                me.spawn_doodads(&server)
            }));
        }
        {
            let (me, server) = (self.clone(), server.clone());
            tasks.push(tokio::task::spawn_blocking(move || {
                me.spawn_other_spawners(&server)
            }));
        }
        {
            let (me, server) = (self.clone(), server.clone());
            tasks.push(tokio::task::spawn_blocking(move || {
                me.spawn_player_doodads(&server)
            }));
        }
        self.spawn_tasks.lock().expect("poisoned").extend(tasks);
    }

    /// Waits for the `SpawnTasks` started so far; returns the spawned counts.
    pub async fn wait_spawn_tasks(&self) -> Vec<usize> {
        let tasks: Vec<_> = self
            .spawn_tasks
            .lock()
            .expect("poisoned")
            .drain(..)
            .collect();
        let mut out = Vec::new();
        for t in tasks {
            out.push(t.await.unwrap_or(0));
        }
        out
    }

    /// The doodad task of `SpawnAll`: `spawner.Spawn(0)` for every doodad spawner.
    pub fn spawn_doodads(&self, server: &GameServer) -> usize {
        let start = std::time::Instant::now();
        let spawners: Vec<_> = self.inner().doodad_spawners.values().cloned().collect();
        tracing::info!(
            "Spawning {} Doodads in world {}",
            spawners.len(),
            self.world.template.name
        );
        let mut count = 0;
        for s in spawners {
            s.spawn(server, 0);
            count += 1;
            if count % 5000 == 0 {
                tracing::debug!(
                    "In world {} Doodads spawned: {count}...",
                    self.world.template.name
                );
            }
        }
        tracing::info!(
            "In world {} Doodads spawned: {count} in {:?}",
            self.world.template.name,
            start.elapsed()
        );
        server
            .systems
            .npcs
            .doodads
            .persistence
            .natural
            .finish_initial_spawn(server, self.world.id, &self.world.template.name);
        // Link doodads seats to the trial system
        if self.world.id == crate::models::character::DEFAULT_INSTANCE_ID {
            self.link_trial_spawners(server);
        }
        count
    }

    /// `TrialManager.Load()` + `InitializeDoodads(World)` + `InitializeNpc(World)`: the jury /
    /// audience seats (`justice.<court>.seat.<n>` / `.spectator` doodad spawners, their last
    /// spawned doodad) and the judges (`justice.<court>.judge` NPC spawners).
    pub fn link_trial_spawners(&self, server: &GameServer) {
        let trials = &server.systems.social.trials;
        trials.load(server);
        let doodads: Vec<(String, DoodadRef)> = self
            .get_special_doodad_spawners("justice.")
            .into_iter()
            .filter_map(|s| s.last().map(|d| (s.special_link.clone(), d)))
            .collect();
        trials.initialize_doodads(&doodads);
        let npcs: Vec<(String, u32)> = self
            .get_special_npc_spawners("justice.")
            .into_iter()
            .map(|s| (s.special_link.clone(), s.id))
            .collect();
        trials.initialize_npc(&npcs);
    }

    /// The transfer / gimmick / slave tasks of `SpawnAll` (returns the gimmicks spawned).
    /// `TransferSpawner.SpawnAll` / `SlaveSpawner.Spawn(0)` are the vehicles subsystem's.
    pub fn spawn_other_spawners(&self, server: &GameServer) -> usize {
        let (transfer_defs, gimmicks, slave_defs) = {
            let inner = self.inner();
            (
                inner
                    .transfer_spawners
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
                inner.gimmick_spawners.values().cloned().collect::<Vec<_>>(),
                inner.slave_spawners.values().cloned().collect::<Vec<_>>(),
            )
        };
        let (transfers, slaves) = (transfer_defs.len(), slave_defs.len());
        let name = &self.world.template.name;
        tracing::info!("Spawning {transfers} Transfers in world {name}");
        crate::systems::vehicles::transfers::spawn_all(server, &self.world, transfer_defs);
        tracing::info!("Spawning {} Gimmicks in world {name}", gimmicks.len());
        let mut count = 0;
        for g in &gimmicks {
            // GimmickSpawner.Spawn(0)
            crate::systems::npcs::gimmick::spawn_from_spawner(server, &self.world, g);
            count += 1;
            if count % 25 == 0 {
                tracing::debug!("In world {name} Gimmicks spawned: {count}...");
            }
        }
        tracing::info!("In world {name} Gimmicks spawned: {count}");
        tracing::info!("Spawning {slaves} Slaves in world {name}");
        crate::systems::vehicles::slaves::spawn_all(server, &self.world, slave_defs);
        count
    }

    /// The player-doodad task of `SpawnAll`.
    pub fn spawn_player_doodads(&self, server: &GameServer) -> usize {
        let doodads = self.get_all_player_doodads();
        if !doodads.is_empty() {
            tracing::info!("Spawning {} Player Doodads", doodads.len());
        }
        let mut count = 0;
        for d in doodads {
            match server.systems.npcs.spawns.doodad_spawner_of(d.obj_id) {
                None => {
                    backend::doodad_spawn(server, &self.world, &d);
                    crate::systems::npcs::doodad::manager::init_doodad(server, &self.world, &d);
                    count += 1;
                }
                Some(s) => {
                    if s.spawn(server, d.obj_id).is_none() {
                        let (db_id, tpl) = {
                            let x = d.data();
                            (x.db_id, x.template_id)
                        };
                        tracing::error!(
                            "Failed to spawn player doodad DbId:{db_id}, TemplateId: {tpl}"
                        );
                    }
                }
            }
        }
        count
    }

    // ----- queries -----

    /// `GetAllSpawners()` (a copy).
    pub fn get_all_spawners(&self) -> BTreeMap<u32, Vec<Arc<NpcSpawner>>> {
        self.inner().npc_spawners.clone()
    }

    /// A doodad management group uses explicit authored membership, not spawner template IDs.
    pub fn set_management_group(
        &self,
        server: &GameServer,
        zone: u32,
        group: u32,
        enabled: bool,
    ) -> usize {
        if group == 0 {
            return 0;
        }
        let _items = server.systems.economy.operation_scope();
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let spawners: Vec<_> = {
            let inner = self.inner();
            inner
                .npc_spawners
                .values()
                .chain(inner.npc_event_spawners.values())
                .flatten()
                .filter(|s| s.shared().management_group() == group && s.position().zone_id == zone)
                .cloned()
                .collect()
        };
        for spawner in &spawners {
            spawner.shared().set_management_enabled(enabled);
            if enabled {
                spawner.do_event_spawn(server);
            } else {
                for npc in spawner.spawned_npcs().into_values().flatten() {
                    if !self
                        .world
                        .npc(npc.obj_id)
                        .is_some_and(|current| Arc::ptr_eq(&current, &npc))
                    {
                        continue;
                    }
                    if npc.data().dead_time_ms > 0 {
                        self.cancel_npc_respawn(&npc);
                        continue;
                    }
                    self.cancel_npc_timers(&npc);
                    spawner.despawn(server, &npc);
                    server.ids.object.release_id(npc.obj_id);
                    forget_object(server, npc.obj_id);
                }
            }
        }
        spawners.len()
    }

    /// Turns the held-back entries of tower defense group members into event spawners.
    ///
    /// The JSON has no group entry: every member of an `npc_groups` group is listed (one entry
    /// each, duplicates included) at the group's spawn site. A site of group `G` is therefore a
    /// position whose entries are exactly `G`'s members; every spawner template listing `G` as an
    /// `NpcGroup` member gets one event spawner per member of every site, standing at the
    /// site plus the member's formation offset. Entries that complete no group are released as
    /// ordinary spawns.
    fn resolve_tower_groups(
        &self,
        server: &GameServer,
        stats: &mut LoadStats,
    ) -> Result<(), String> {
        let entries = std::mem::take(&mut self.inner().tower_group_entries);
        if entries.is_empty() {
            return Ok(());
        }
        let td = &server.game_data.systems.instances.tower_defs;
        let key = |p: &crate::data::world_templates::WorldSpawnPosition| {
            (
                (p.x * 100.0).round() as i64,
                (p.y * 100.0).round() as i64,
                (p.z * 100.0).round() as i64,
            )
        };
        let mut sites: BTreeMap<(i64, i64, i64), Vec<usize>> = BTreeMap::new();
        for (i, e) in entries.iter().enumerate() {
            sites.entry(key(&e.base.position)).or_default().push(i);
        }
        let mut used = vec![false; entries.len()];
        let mut resolved: BTreeMap<u32, Vec<Arc<NpcSpawner>>> = BTreeMap::new();
        for (&spawner_id, group_ids) in &td.spawner_groups {
            for group_id in group_ids {
                let Some(members) = td.groups.get(group_id) else {
                    continue;
                };
                let mut need: BTreeMap<u32, usize> = BTreeMap::new();
                for m in members {
                    *need.entry(m.npc_id).or_default() += 1;
                }
                for indices in sites.values() {
                    let mut have: BTreeMap<u32, usize> = BTreeMap::new();
                    for &i in indices {
                        *have.entry(entries[i].base.unit_id).or_default() += 1;
                    }
                    if have != need {
                        continue;
                    }
                    for &i in indices {
                        used[i] = true;
                    }
                    let site = &entries[indices[0]].base;
                    for m in members {
                        let mut position = site.position;
                        let (sin, cos) = position.yaw.sin_cos();
                        position.x += m.offset[0] * cos - m.offset[1] * sin;
                        position.y += m.offset[0] * sin + m.offset[1] * cos;
                        position.z += m.offset[2];
                        let shared = NpcSpawnerShared::new(position);
                        shared.set_management_group(site.spawn_group_id);
                        let mut template = NpcSpawnerTemplate::with_member(spawner_id, m.npc_id);
                        template.spawn_delay_min = 0.0;
                        template.spawn_delay_max = 0.0;
                        resolved
                            .entry(spawner_id)
                            .or_default()
                            .push(Arc::new(NpcSpawner::new(
                                NpcSpawnerInit {
                                    id: spawner_id,
                                    spawner_id,
                                    unit_id: m.npc_id,
                                    follow_path: site.follow_path.clone(),
                                    follow_npc: site.follow_npc,
                                    respawn_time: site.respawn_time,
                                    despawn_time: site.despawn_time,
                                    special_link: site.special_link.clone(),
                                    count: 1,
                                    npc_spawner_ids: vec![spawner_id],
                                    parent_world: Some(self.world.id),
                                    spawnable_npcs: Vec::new(),
                                    template: Some(Arc::new(template)),
                                },
                                shared.clone(),
                            )));
                    }
                }
            }
        }
        {
            let mut inner = self.inner();
            for (id, list) in resolved {
                inner.tower_def_spawners.entry(id).or_default().extend(list);
            }
        }
        for (def, used) in entries.into_iter().zip(used) {
            if !used {
                // Not part of any complete group: an ordinary spawn after all.
                let added = self.add_npc_spawner_with(server, def, false)?.added;
                if added > 0 {
                    stats.npc_entries_with_spawners += 1;
                    stats.npc_spawners += added;
                }
            }
        }
        Ok(())
    }

    /// Authored controller positions for a tower defense spawner template.
    pub fn tower_def_spawners(&self, id: u32) -> Vec<Arc<NpcSpawner>> {
        self.inner()
            .tower_def_spawners
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    /// `GetNpcSpawner(spawnerId)`: the event spawners with that `Id` (retargets their last
    /// template member to `UnitId`, as the C# does).
    pub fn get_npc_spawner(&self, spawner_id: u32) -> Vec<Arc<NpcSpawner>> {
        let inner = self.inner();
        let mut ret = Vec::new();
        for spawners in inner.npc_event_spawners.values() {
            for s in spawners {
                if s.id != spawner_id {
                    continue;
                }
                s.retarget_last_template_npc();
                ret.push(s.clone());
            }
        }
        ret
    }

    /// `GetNpcSpawner(unitId, unit)`: a new spawner at the unit's position (`SpawnEffect`).
    /// Mirrors the C# rewrite of the shared game-data template (`Template.Npcs = [first member
    /// retargeted to unitId]`).
    pub fn get_npc_spawner_at(
        &self,
        server: &GameServer,
        unit_id: u32,
        unit: &WorldObject,
    ) -> Option<Arc<NpcSpawner>> {
        let _g = self.lock_spawner.lock().expect("poisoned");
        let (id, template) = Self::effect_template(server, unit_id)?;
        let world = self.world.world_pos_rot(unit);
        let t = unit.transform();
        let position = crate::data::world_templates::WorldSpawnPosition {
            world_id: t.world_id,
            zone_id: t.zone_id,
            x: world.x,
            y: world.y,
            z: world.z,
            yaw: world.rot_z,
            pitch: 0.0,
            roll: 0.0,
        };
        Some(self.effect_spawner(unit_id, id, template, position))
    }

    /// [`Self::get_npc_spawner_at`] for a position unit (a plot "Location" target, which has no
    /// world object): the spawner stands at `pos` in `zone_id` of `world_id`.
    pub fn get_npc_spawner_at_pos(
        &self,
        server: &GameServer,
        unit_id: u32,
        world_id: u32,
        zone_id: u32,
        pos: &crate::models::transform::PositionAndRotation,
    ) -> Option<Arc<NpcSpawner>> {
        let _g = self.lock_spawner.lock().expect("poisoned");
        let (id, template) = Self::effect_template(server, unit_id)?;
        let position = crate::data::world_templates::WorldSpawnPosition {
            world_id,
            zone_id,
            x: pos.x,
            y: pos.y,
            z: pos.z,
            yaw: pos.rot_z,
            pitch: 0.0,
            roll: 0.0,
        };
        Some(self.effect_spawner(unit_id, id, template, position))
    }

    /// The spawner template `GetNpcSpawner` rewrites for `unit_id`.
    fn effect_template(
        server: &GameServer,
        unit_id: u32,
    ) -> Option<(u32, Arc<NpcSpawnerTemplate>)> {
        let data = &server.game_data.systems.npcs.npc_spawners;
        // Only a template that lists this NPC as an ordinary member may be rewritten: an
        // `NpcGroup` row is keyed by its group id, and retargeting it would replace the group's
        // first member (and therefore its whole spawn) with the summoned unit. A member that is
        // only reachable through a group keeps the C# fallback: one generated single-NPC
        // spawner.
        let picked = data.get_spawner_ids(unit_id).and_then(|ids| {
            ids.into_iter().find_map(|id| {
                let template = data.get_npc_spawner_template(id)?;
                let nsn = template
                    .npcs
                    .iter()
                    .find(|n| n.member_type == "Npc" && n.member_id == unit_id)?
                    .clone();
                Some((id, nsn))
            })
        });
        match picked {
            None => {
                let id = server.ids.object.get_next_id().ok()?;
                let mut t = NpcSpawnerTemplate::with_spawner(id);
                t.npcs[0].member_id = unit_id;
                t.npcs[0].unit_id = unit_id;
                t.npcs[0].member_type = "Npc".into();
                Some((id, Arc::new(t)))
            }
            Some((id, mut nsn)) => {
                nsn.member_id = unit_id;
                nsn.unit_id = unit_id;
                data.replace_template_npcs(id, nsn);
                Some((id, data.get_npc_spawner_template(id)?))
            }
        }
    }

    fn effect_spawner(
        &self,
        unit_id: u32,
        id: u32,
        template: Arc<NpcSpawnerTemplate>,
        position: crate::data::world_templates::WorldSpawnPosition,
    ) -> Arc<NpcSpawner> {
        Arc::new(NpcSpawner::new(
            NpcSpawnerInit {
                id,
                spawner_id: 0,
                unit_id,
                follow_path: String::new(),
                follow_npc: 0,
                respawn_time: 15,
                despawn_time: 20,
                special_link: String::new(),
                count: 1,
                npc_spawner_ids: vec![id],
                parent_world: Some(self.world.id),
                template: Some(template),
                spawnable_npcs: Vec::new(),
            },
            NpcSpawnerShared::new(position),
        ))
    }

    /// `CloneNpcEventSpawners(from, to)`.
    pub fn clone_npc_event_spawners(&self, from: u8, to: u8) -> bool {
        let mut inner = self.inner();
        let value = inner
            .npc_event_spawners
            .get(&u32::from(from))
            .cloned()
            .unwrap_or_default();
        if inner.npc_event_spawners.contains_key(&u32::from(to)) {
            return false;
        }
        inner.npc_event_spawners.insert(u32::from(to), value);
        true
    }

    /// `RemoveNpcEventSpawners(from)`.
    pub fn remove_npc_event_spawners(&self, from: u8) -> bool {
        self.inner()
            .npc_event_spawners
            .remove(&u32::from(from))
            .is_some()
    }

    /// `GetTreasureChestDoodadSpawners()`: doodad spawners whose template (or respawn
    /// template) is a treasure chest.
    pub fn get_treasure_chest_doodad_spawners(
        &self,
        server: &GameServer,
    ) -> Vec<Arc<DoodadSpawner>> {
        let ids = server
            .game_data
            .systems
            .npcs
            .doodad
            .treasure_chest_template_ids();
        self.inner()
            .doodad_spawners
            .values()
            .filter(|d| ids.contains(&d.respawn_doodad_template_id()) || ids.contains(&d.unit_id))
            .cloned()
            .collect()
    }

    /// `GetDoodadSpawnersByUnitId(unitId)`.
    pub fn get_doodad_spawners_by_unit_id(&self, unit_id: u32) -> Vec<Arc<DoodadSpawner>> {
        let _g = self.lock_spawner.lock().expect("poisoned");
        self.inner()
            .doodad_spawners
            .values()
            .filter(|d| d.unit_id == unit_id)
            .cloned()
            .collect()
    }

    /// `GetNExtSpawnerId()`.
    pub fn get_next_spawner_id(&self) -> u32 {
        let mut inner = self.inner();
        let r = inner.next_id;
        inner.next_id += 1;
        r
    }

    /// `GetSpecialDoodadSpawners(header)`.
    pub fn get_special_doodad_spawners(&self, header: &str) -> Vec<Arc<DoodadSpawner>> {
        self.inner()
            .doodad_spawners
            .values()
            .filter(|d| !d.special_link.trim().is_empty() && d.special_link.starts_with(header))
            .cloned()
            .collect()
    }

    /// `GetSpecialNpcSpawners(header)`.
    pub fn get_special_npc_spawners(&self, header: &str) -> Vec<Arc<NpcSpawner>> {
        self.inner()
            .npc_spawners
            .values()
            .flatten()
            .filter(|s| !s.special_link.trim().is_empty() && s.special_link.starts_with(header))
            .cloned()
            .collect()
    }

    /// Loaded spawner counts (npc clones, event, doodad, transfer, gimmick, slave).
    pub fn counts(&self) -> [usize; 6] {
        let inner = self.inner();
        [
            inner.npc_spawners.values().map(Vec::len).sum(),
            inner.npc_event_spawners.values().map(Vec::len).sum(),
            inner.doodad_spawners.len(),
            inner.transfer_spawners.len(),
            inner.gimmick_spawners.len(),
            inner.slave_spawners.len(),
        ]
    }

    /// The loaded doodad spawners.
    pub fn doodad_spawners(&self) -> Vec<Arc<DoodadSpawner>> {
        self.inner().doodad_spawners.values().cloned().collect()
    }

    /// The loaded gimmick spawner entries (for the gimmick module).
    pub fn gimmick_spawners(&self) -> Vec<GimmickSpawner> {
        self.inner().gimmick_spawners.values().cloned().collect()
    }

    // ----- teardown -----

    /// `DeSpawnAll()`: every NPC / doodad / mate / slave of the instance; returns the count.
    /// Serialized with the activation pass; the warm registry is dropped afterwards (nothing of
    /// the instance is left to track, so a deliberately emptied instance cannot leave a growing
    /// set of placements whose 9 999 999 s respawn timers never drain).
    pub fn despawn_all(&self, server: &GameServer) -> usize {
        let _items = server.systems.economy.operation_scope();
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let spawns = &server.systems.npcs.spawns;
        let mut res = 0;
        for npc in self.world.all_npcs() {
            match spawns.npc_spawner_of(npc.obj_id) {
                Some(s) => {
                    s.set_respawn_time(9_999_999);
                    s.do_despawn(server, &npc);
                }
                None => crate::systems::npcs::npc::manager::delete(server, &self.world, &npc),
            }
            res += 1;
        }
        for doodad in self.world.all_doodads() {
            match spawns.doodad_spawner_of(doodad.obj_id) {
                Some(s) => {
                    s.set_respawn_time(9_999_999);
                    s.despawn(server, &doodad);
                }
                None => {
                    // Detach from the save system so the delete does not remove the DB row.
                    doodad.data().is_persistent = false;
                    backend::doodad_delete(server, &self.world, &doodad);
                }
            }
            res += 1;
        }
        // Mate / Slave: GameObject.Delete (no override in C#).
        for kind in [ObjectKind::Mate, ObjectKind::Slave] {
            for o in self.world.all_of_kind(kind) {
                self.world.delete(o);
                res += 1;
            }
        }
        self.reset_activation();
        res
    }

    /// `DeleteAllSpawners()`: removes every spawner and what they spawned.
    pub fn delete_all_spawners(&self, server: &GameServer) {
        let _items = server.systems.economy.operation_scope();
        // Serialized with the activation pass so a stale queue cannot create NPCs between
        // despawning a placement's old members and detaching / resetting it.
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let npc_spawners: Vec<_> = self
            .inner()
            .npc_spawners
            .values()
            .flatten()
            .cloned()
            .collect();
        for s in npc_spawners {
            for npc in s.spawned_npcs().into_values().flatten() {
                // npc.UnregisterNpcEvents(): the AI (and its event registrations) goes with the
                // deleted NPC.
                s.despawn(server, &npc);
            }
            s.clear_spawned_npcs();
            s.set_parent_world(None);
        }
        self.inner().npc_spawners.clear();
        self.invalidate_activation_index();
        self.reset_activation();
        let doodad_spawners: Vec<_> = self.inner().doodad_spawners.values().cloned().collect();
        for s in doodad_spawners {
            for d in s.spawned() {
                s.despawn(server, &d);
            }
            s.set_parent_world(None);
        }
        self.inner().doodad_spawners.clear();
        self.inner().gimmick_spawners.clear();
        // World.GimmickManager._activeGimmicks -> gimmick.Delete() (GameObject.Delete: the
        // active list itself is left as is, as in C#).
        for g in self.world.all_of_kind(ObjectKind::Gimmick) {
            self.world.delete(g);
        }
    }
}
// ---------------------------------------------------------------------------------------------
// startup, active-region tick, NPC death
// ---------------------------------------------------------------------------------------------

/// One bounded activation pass over every instance with a spawn manager (tests / diagnostics
/// call this; production goes through [`active_region_tick`]).
///
/// Each instance's candidate queues are refreshed only when its player activity revision, its
/// ordinary spawner membership or the prewarm margin changed, so an unchanged pass never scans
/// the full spawner table. Candidates fill within the global `World.NpcSpawnsPerTick` created-NPC
/// budget: client-visible neighbourhoods before prewarm rings, nearest first, and the starting
/// instance rotates every pass so small budgets cannot starve later instances. Afterwards the
/// warmed registry is swept: placements that dropped out of the warm set keep their NPCs for
/// `World.NpcWarmRetentionSeconds` (residents stay after their first preparation) and expired
/// ordinary ones are reclaimed within the same per-tick budget, counted in actual NPCs.
pub fn active_region_tick_stats(server: &GameServer) -> ActivationStats {
    let _items = server.systems.economy.operation_scope();
    // The whole pass holds the spawners' reentrant lock: creation, despawn, placement reload and
    // delete-all serialize against the queued candidates (sync fn, never held across an await).
    let _serial = server.systems.npcs.spawns.spawn_lock();
    let margin = prewarm_region_margin(&server.config.world);
    let mut budget = i64::from(server.config.world.npc_spawns_per_tick.max(1));
    let worlds = server.systems.npcs.spawns.all();
    let mut stats = ActivationStats::default();
    if worlds.is_empty() {
        return stats;
    }
    let n = worlds.len();
    let start = server
        .systems
        .npcs
        .spawns
        .activation_rotation
        .fetch_add(1, Ordering::Relaxed) as usize
        % n;
    for i in 0..n {
        worlds[(start + i) % n].activation_phase(server, margin, true, &mut budget, &mut stats);
    }
    for i in 0..n {
        worlds[(start + i) % n].activation_phase(server, margin, false, &mut budget, &mut stats);
    }
    let mut reclaim_budget = i64::from(server.config.world.npc_spawns_per_tick.max(1));
    for i in 0..n {
        worlds[(start + i) % n].activation_sweep(server, &mut reclaim_budget, &mut stats);
    }
    stats
}

/// `WorldManager.ActiveRegionTick` spawner part (the production entry point; the structured
/// result is [`active_region_tick_stats`]).
pub fn active_region_tick(server: &GameServer) {
    let _ = active_region_tick_stats(server);
}

/// Starts the active-region tick (once per server) at `World.NpcSpawnTickMs`.
pub fn start_active_region_tick(server: &Arc<GameServer>) {
    if server
        .systems
        .npcs
        .spawns
        .active_tick_started
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    // Build the immutable resident-template snapshot once, before the first pass, instead of
    // lazily inside it (it scans the static quest acts).
    let _ = server.systems.npcs.spawns.resident_npcs(server);
    let weak = Arc::downgrade(server);
    rt.spawn(async move {
        loop {
            // Re-read the interval every pass so a config reload applies without a restart;
            // `0` never spins (see [`spawn_tick_interval`]).
            let wait = match weak.upgrade() {
                Some(s) => spawn_tick_interval(&s.config.world),
                None => break,
            };
            tokio::time::sleep(wait).await;
            let Some(server) = weak.upgrade() else {
                break;
            };
            let start = std::time::Instant::now();
            crate::systems::run_blocking(&server, |s| {
                let _ = active_region_tick_stats(s);
            })
            .await;
            if start.elapsed() > Duration::from_millis(100) {
                tracing::warn!("ActiveRegionTick took {} ms", start.elapsed().as_millis());
            }
        }
    });
}

/// `WorldManager.CreateStaticInstances` spawn part: `SpawnManager.Load()` + `SpawnAll()` for
/// `main_world` (NPCs appear through the active-region tick).
pub async fn start(server: &Arc<GameServer>) -> anyhow::Result<()> {
    let Some(world) = server
        .world
        .world(crate::models::character::DEFAULT_INSTANCE_ID)
    else {
        return Ok(());
    };
    let manager = server.systems.npcs.spawns.get_or_create(&world);
    manager.load(server).await?;
    manager.spawn_all(server);
    start_active_region_tick(server);
    Ok(())
}

/// `Npc.DoDie` → `Spawner?.DoDespawn(this)`: schedules the despawn (corpse) and, when allowed,
/// the respawn of an NPC that died. No-op for NPCs without a spawner. The caller must not hold
/// any object lock.
pub fn on_npc_died(server: &GameServer, npc: &NpcRef) {
    // A far quest-map viewer only knows this NPC through the map option; the death broadcast
    // never reaches it, so its marker must be removed explicitly.
    server.systems.quests.quest_map.forget_npc(server, npc);
    if let Some(s) = server.systems.npcs.spawns.npc_spawner_of(npc.obj_id) {
        s.do_despawn(server, npc);
    }
}

/// `SpawnEffect` / `NpcSpawnerSpawnEffect` `DespawnOnCreatorDeath`: the summons registered to a
/// dying unit are despawned with it. Called from the unit death path
/// (`skills::combat::do_die`); the caller must not hold any object lock. The 1.2 C# only loaded
/// the flag (see `PORTING.md`); creator logout / non-death despawn leaves the summons alive, as
/// there.
pub fn despawn_creator_summons(server: &GameServer, creator: &WorldObject) {
    let summons = server
        .systems
        .npcs
        .spawns
        .take_creator_summons(creator.obj_id());
    for npc in summons {
        // A summon that is already dead keeps its corpse for the corpse timer (as the despawn
        // tick does); only live summons follow their creator.
        let dead = npc.data().dead_time_ms > 0;
        if dead {
            continue;
        }
        let obj = WorldObject::from(&npc);
        // A stale entry: the object id now belongs to another live NPC (its summon entry was
        // registered by a creator whose id was reused).
        if !server
            .world
            .object(npc.obj_id)
            .and_then(|o| o.as_npc().cloned())
            .is_some_and(|current| Arc::ptr_eq(&current, &npc))
        {
            continue;
        }
        let spawner = server.systems.npcs.spawns.npc_spawner_of(npc.obj_id);
        if let Some(manager) = manager_of(
            server,
            &obj,
            spawner.as_ref().and_then(|s| s.parent_world()),
        ) {
            manager.cancel_npc_timers(&npc);
        }
        match spawner {
            Some(s) => s.despawn(server, &npc),
            None => {
                tracing::warn!(
                    "DespawnOnCreatorDeath: summon {} has no spawner",
                    npc.obj_id
                );
                delete_object(server, &obj, None);
            }
        }
        // `NpcSpawner.Despawn` leaves the id to the caller (the despawn tick releases it).
        server.ids.object.release_id(npc.obj_id);
        forget_object(server, npc.obj_id);
        tracing::debug!(
            "DespawnOnCreatorDeath: deleted summon {} with its creator {}",
            npc.obj_id,
            creator.obj_id()
        );
    }
}
