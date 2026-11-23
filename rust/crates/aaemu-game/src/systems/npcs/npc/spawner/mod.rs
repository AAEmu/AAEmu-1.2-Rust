//! `NpcSpawner` (`Models/Game/NPChar/NpcSpawner.cs`) and the spawning half of `NpcSpawnerNpc`
//! (`Models/Game/NPChar/NpcSpawnerNpc.cs`, see [`spawn_member`]).
//!
//! C# object graph and how it maps here:
//! * `SpawnManager.AddNpcSpawner` makes *memberwise clones* of one JSON entry, one per game-data
//!   spawner template id. A memberwise clone shares the reference-type members: `Position`,
//!   `SpawnedNpcs` (keyed by `SpawnerId`), the three 10-second caches and `NpcSpawnerIds`.
//!   They live in [`NpcSpawnerShared`] (one `Arc` per JSON entry); the value members
//!   (`_scheduledCount`, `IsSpawnScheduled`, `RespawnTime`, ...) are per clone.
//! * `npc.Spawner` = `server.systems.npcs.spawns.npc_spawner_of(obj_id)` (plus
//!   `Npc::spawner_id` = `Spawner.Id`).
//! * The static `_spawnLock` is the server-wide reentrant `SpawnManagers::spawn_lock` (C#
//!   `lock` is reentrant and the C# methods nest it freely).
//! * `DateTime.UtcNow` for caches / respawn times is `tokio::time::Instant` (mockable in tests);
//!   `npc.DeadTime` is the unix-ms `Npc::dead_time_ms`.
//!
//! Locking: the per-spawner `state` / shared mutexes are leaf locks, never held while locking an
//! NPC or calling into the world. Callers must not hold an object data lock.

mod despawn;
mod placement;
mod spawn;

pub(crate) use self::placement::spawn_ground_z;
use self::placement::spawn_npc_template;
pub use self::placement::{geo_data_height, spawn_member};

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::time::Instant;

use super::group;
use super::group_data;
use super::residency::ResidentNpcTemplates;
use super::spawner_data::{npc_spawner_category, NpcSpawnerNpc, NpcSpawnerTemplate};
use crate::data::world_templates::WorldSpawnPosition;
use crate::server::GameServer;
use crate::systems::ai::npc as ai_npc;
use crate::systems::npcs::schedule::PeriodStatus;
use crate::systems::npcs::spawn_manager::{self, backend};
use crate::systems::npcs::stacked_npcs::{self, ScatterParams};
use crate::systems::skills::controllers::calculate_distance;
use crate::world::{NpcRef, WorldInstance, WorldObject};

/// `LootingContainer.LootDespawnExtensionTime` (seconds).
pub const LOOT_DESPAWN_EXTENSION_TIME: f32 = 300.0;

/// Seconds the NPC-membership `_npcsInZoneCache` stays valid (it is invalidated eagerly on
/// membership changes).
const CACHE_SECONDS: f64 = 10.0;

/// The reference-type members shared by memberwise clones of one spawner entry.
#[derive(Debug, Default)]
pub struct NpcSpawnerShared {
    management_group: AtomicU32,
    management_enabled: AtomicBool,
    /// `Position`.
    position: Mutex<WorldSpawnPosition>,
    /// `SpawnedNpcs` (`<SpawnerId, List<Npc>>`).
    spawned: Mutex<BTreeMap<u32, Vec<NpcRef>>>,
    /// `_playerCountCache`: `(instance id, spawner id)` -> (count, instance activity revision).
    /// Stamped with [`WorldInstance::player_activity_revision`], so a player arrival, move
    /// (including within-region movement) or departure invalidates it at once; the C# 10 s
    /// staleness is deliberately not replicated (it reported stale proximity as fact).
    player_count_cache: Mutex<HashMap<(u32, u32), (i32, u64)>>,
    /// `_playerInRadiusCache`: `(instance id, spawner id)` -> (found, instance activity
    /// revision), invalidated the same way.
    player_in_radius_cache: Mutex<HashMap<(u32, u32), (bool, u64)>>,
    /// `_npcsInZoneCache` (count, areNpcsInZone); invalidated on membership changes.
    npcs_in_zone_cache: Mutex<HashMap<u32, (i32, bool, Instant)>>,
    /// One of this placement's NPCs died where the run keeps its dead (authored
    /// `npc_spawners.save_indun` inside a live `Dungeon`, or a spawn-file placement in a
    /// per-run dungeon), so the placement's population is fixed for the run (no respawn, no
    /// periodic refill, no distance reclaim of the survivors). A fresh run creates new
    /// spawners, which start unfrozen. The C# loaded `save_indun` but never read it.
    saved_death: AtomicBool,
}

impl NpcSpawnerShared {
    pub fn set_management_group(&self, group: u32) {
        self.management_group.store(group, Ordering::Relaxed);
    }
    /// Whether a member of this placement died under the one-run freeze.
    pub fn saved_death(&self) -> bool {
        self.saved_death.load(Ordering::Relaxed)
    }

    /// Freezes the placement after a member died where the run keeps its dead.
    pub fn set_saved_death(&self) {
        self.saved_death.store(true, Ordering::Relaxed);
    }
    pub fn management_group(&self) -> u32 {
        self.management_group.load(Ordering::Relaxed)
    }
    pub fn set_management_enabled(&self, enabled: bool) {
        self.management_enabled.store(enabled, Ordering::Relaxed);
    }
    pub fn management_enabled(&self) -> bool {
        self.management_group() == 0 || self.management_enabled.load(Ordering::Relaxed)
    }
    pub fn new(position: WorldSpawnPosition) -> Arc<Self> {
        Arc::new(Self {
            position: Mutex::new(position),
            ..Self::default()
        })
    }
}

/// The value members of a spawner (copied by a memberwise clone).
#[derive(Debug, Clone)]
pub struct NpcSpawnerInit {
    pub id: u32,
    pub spawner_id: u32,
    pub unit_id: u32,
    pub follow_path: String,
    pub follow_npc: u32,
    pub respawn_time: i32,
    pub despawn_time: i32,
    pub special_link: String,
    pub count: u32,
    pub npc_spawner_ids: Vec<u32>,
    /// `ParentWorld` (instance id).
    pub parent_world: Option<u32>,
    pub template: Option<Arc<NpcSpawnerTemplate>>,
    /// `SpawnableNpcs`.
    pub spawnable_npcs: Vec<NpcSpawnerNpc>,
}

#[derive(Debug, Clone)]
struct State {
    parent_world: Option<u32>,
    template: Option<Arc<NpcSpawnerTemplate>>,
    spawnable_npcs: Vec<NpcSpawnerNpc>,
    respawn_time: i32,
    /// `_scheduledCount`.
    scheduled_count: i32,
    is_spawn_scheduled: bool,
    is_despawn_scheduled: bool,
    /// `RespawnDenied` (written, never read in the C#).
    respawn_denied: bool,
}

/// `SpawnEffect` members `NpcSpawner.DoSpawnEffect` reads.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SpawnEffectParams {
    pub use_summoner_faction: bool,
    pub use_summoner_aggro_target: bool,
    /// `DespawnOnCreatorDeath`: the spawned NPC is removed when the caster dies.
    pub despawn_on_creator_death: bool,
    /// `LifeTime` (seconds; > 0 schedules `NpcSpawnerDoDespawnTask`).
    pub life_time: f32,
}

/// `NpcSpawner`.
#[derive(Debug)]
pub struct NpcSpawner {
    /// `Id` (index).
    pub id: u32,
    /// `SpawnerId` (spawner template id).
    pub spawner_id: u32,
    /// `UnitId` (NPC template id).
    pub unit_id: u32,
    pub follow_path: String,
    pub follow_npc: u32,
    pub despawn_time: i32,
    pub special_link: String,
    pub count: u32,
    pub npc_spawner_ids: Vec<u32>,
    shared: Arc<NpcSpawnerShared>,
    state: Mutex<State>,
    /// `true` for a spawner created by a spawn-file placement (`SpawnManager::NpcSpawners`);
    /// event / tower / summon spawners keep `false`. Set once when the placement is created, so
    /// the one-run death freeze is a flag read instead of a scan over every spawner.
    placement: AtomicBool,
}

/// `TimeSpan.FromHours(hours)` ticks (100 ns).
fn ticks_from_hours(hours: f32) -> i64 {
    (f64::from(hours) * 36_000_000_000.0) as i64
}

/// `NpcSpawner.IsTimeBetween(currentTime, startTime, endTime)` (TimeSpan ticks): inclusive,
/// wrapping over midnight when `start > end`.
pub fn is_time_between(current: i64, start: i64, end: i64) -> bool {
    if start <= end {
        current >= start && current <= end
    } else {
        current >= start || current <= end
    }
}

/// `IsTimeBetween` on game-clock hours (`TimeSpan.FromHours` of each value).
pub fn is_hour_between(current: f32, start: f32, end: f32) -> bool {
    is_time_between(
        ticks_from_hours(current),
        ticks_from_hours(start),
        ticks_from_hours(end),
    )
}

/// `Random.Shared.Next(float min, float max)` (`RandomExtensions`): `NextSingle() * (max - min)
/// + min`.
pub fn random_float(min: f32, max: f32) -> f32 {
    rand::random::<f32>() * (max - min) + min
}

/// `IEnumerableExtensions.RandomElementByWeight`.
pub fn random_element_by_weight<T>(items: &[T], weight: impl Fn(&T) -> f32) -> Option<&T> {
    let total: f32 = items.iter().map(&weight).sum();
    let target = rand::random::<f32>() * total;
    let mut current = 0.0f32;
    for item in items {
        current += weight(item);
        if current >= target {
            return Some(item);
        }
    }
    None
}

/// The authored weighted member selection for a capped spawn list: zero-weight members are
/// skipped while a positive-weight one exists, so a cap cannot pin the choice to the list
/// prefix and later members stay eligible. All-zero weights fall back to a uniform pick.
fn select_weighted_member<'a>(members: &[&'a NpcSpawnerNpc]) -> Option<&'a NpcSpawnerNpc> {
    if members.iter().any(|n| n.weight > 0.0) {
        let positive: Vec<&NpcSpawnerNpc> =
            members.iter().copied().filter(|n| n.weight > 0.0).collect();
        random_element_by_weight(&positive, |n| n.weight).copied()
    } else {
        random_element_by_weight(members, |_| 1.0).copied()
    }
}

fn period_status_npc(server: &GameServer, id: u32) -> PeriodStatus {
    server
        .game_data
        .systems
        .npcs
        .schedule
        .period_status_npc(id as i32, spawn_manager::unix_now())
}

/// `Transform.World.Position` of a player (composed with its parent when attached).
fn player_world_pos(server: &GameServer, p: &crate::world::PlayerRef) -> (f32, f32, f32) {
    let t = p.data().unit.transform;
    if !t.has_parent() {
        return (t.x, t.y, t.z);
    }
    let obj = WorldObject::from(p);
    match server.world.world_of(&obj) {
        Some(w) => {
            let pr = w.world_pos_rot(&obj);
            (pr.x, pr.y, pr.z)
        }
        None => (t.x, t.y, t.z),
    }
}

impl NpcSpawner {
    /// A spawner from its value members and the (possibly shared) reference members.
    pub fn new(init: NpcSpawnerInit, shared: Arc<NpcSpawnerShared>) -> Self {
        Self {
            id: init.id,
            spawner_id: init.spawner_id,
            unit_id: init.unit_id,
            follow_path: init.follow_path,
            follow_npc: init.follow_npc,
            despawn_time: init.despawn_time,
            special_link: init.special_link,
            count: init.count,
            npc_spawner_ids: init.npc_spawner_ids,
            shared,
            state: Mutex::new(State {
                parent_world: init.parent_world,
                template: init.template,
                spawnable_npcs: init.spawnable_npcs,
                respawn_time: init.respawn_time,
                scheduled_count: 0,
                is_spawn_scheduled: false,
                is_despawn_scheduled: false,
                respawn_denied: false,
            }),
            placement: AtomicBool::new(false),
        }
    }

    /// Marks a spawner created by a spawn-file placement (`SpawnManager::AddNpcSpawner`).
    pub fn mark_placement(&self) {
        self.placement.store(true, Ordering::Relaxed);
    }

    /// Whether this spawner is a spawn-file placement (see [`Self::mark_placement`]).
    pub fn is_placement(&self) -> bool {
        self.placement.load(Ordering::Relaxed)
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("npc spawner poisoned")
    }

    fn spawned_map(&self) -> MutexGuard<'_, BTreeMap<u32, Vec<NpcRef>>> {
        self.shared.spawned.lock().expect("npc spawner poisoned")
    }

    // ----- accessors -----

    pub fn shared(&self) -> &Arc<NpcSpawnerShared> {
        &self.shared
    }

    pub fn template(&self) -> Option<Arc<NpcSpawnerTemplate>> {
        self.st().template.clone()
    }

    pub fn set_template(&self, t: Option<Arc<NpcSpawnerTemplate>>) {
        self.st().template = t;
    }

    pub fn position(&self) -> WorldSpawnPosition {
        *self.shared.position.lock().expect("npc spawner poisoned")
    }

    /// Writes `Position` (shared by the clones of the entry; `SpawnEffect` moves it).
    pub fn set_position(&self, p: WorldSpawnPosition) {
        *self.shared.position.lock().expect("npc spawner poisoned") = p;
    }

    pub fn respawn_time(&self) -> i32 {
        self.st().respawn_time
    }

    pub fn set_respawn_time(&self, v: i32) {
        self.st().respawn_time = v;
    }

    /// Whether a member of this placement died under the one-run freeze.
    pub fn saved_death(&self) -> bool {
        self.shared.saved_death()
    }

    /// Whether the death of one of this spawner's NPCs freezes its placement for the run. Only a
    /// live `Dungeon` has a run: the open world and plain instances never freeze, whatever the
    /// authored `save_indun` ("save in dungeon") says. Inside a dungeon the authored
    /// `save_indun = 't'` freezes (system dungeon or not), and so does every spawn-file
    /// placement of a per-run dungeon (`Dungeon::is_system = false`, not the client-driven
    /// prologue): a 1.2 run keeps its trash and bosses dead. System / persistent dungeons
    /// (Mirage Isle, the Library wings) keep their respawn rules for the other rows, and
    /// event / tower / summon spawners keep theirs.
    fn freezes_dead_member(&self, server: &GameServer, save_indun: bool) -> bool {
        let Some(dungeon) = self
            .parent_world()
            .and_then(|id| server.systems.instances.dungeon_of_world(id))
        else {
            return false;
        };
        save_indun
            || (self.is_placement() && !dungeon.is_system && !dungeon.indun_zone.client_driven)
    }

    pub fn parent_world(&self) -> Option<u32> {
        self.st().parent_world
    }

    pub fn set_parent_world(&self, w: Option<u32>) {
        self.st().parent_world = w;
    }

    pub fn spawnable_npcs(&self) -> Vec<NpcSpawnerNpc> {
        self.st().spawnable_npcs.clone()
    }

    pub fn is_spawn_scheduled(&self) -> bool {
        self.st().is_spawn_scheduled
    }

    pub fn is_despawn_scheduled(&self) -> bool {
        self.st().is_despawn_scheduled
    }

    pub fn scheduled_count(&self) -> i32 {
        self.st().scheduled_count
    }

    pub fn respawn_denied(&self) -> bool {
        self.st().respawn_denied
    }

    /// `SpawnedNpcs` (a copy).
    pub fn spawned_npcs(&self) -> BTreeMap<u32, Vec<NpcRef>> {
        self.spawned_map().clone()
    }

    /// `SpawnedNpcs.Clear()`.
    /// `SpawnedNpcs.TryAdd(key, list)`: `false` (unchanged) when `key` is already present.
    pub fn try_add_spawned(&self, key: u32, npcs: Vec<NpcRef>) -> bool {
        let added = match self.spawned_map().entry(key) {
            std::collections::btree_map::Entry::Occupied(_) => false,
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(npcs);
                true
            }
        };
        if added {
            self.invalidate_membership_caches();
        }
        added
    }

    pub fn clear_spawned_npcs(&self) {
        self.spawned_map().clear();
        self.invalidate_membership_caches();
    }

    /// Drops the caches computed from the `SpawnedNpcs` membership: `_playerCountCache` anchors
    /// on the map's first NPC and `_npcsInZoneCache` counts the other keys, so a membership
    /// change makes both stale. The player-proximity caches are revision-stamped instead and
    /// need no invalidation here.
    fn invalidate_membership_caches(&self) {
        self.shared
            .player_count_cache
            .lock()
            .expect("npc spawner poisoned")
            .clear();
        self.shared
            .npcs_in_zone_cache
            .lock()
            .expect("npc spawner poisoned")
            .clear();
    }

    /// Living population. Native Npc::OnDeath (10.x x2game 0x393452D0 ->
    /// 0x39345220 -> 0x39344AA0) releases its population slot before corpse removal.
    /// Keep corpses in the list for event cleanup, but exclude them from spawn limits.
    pub fn current_spawn_count(&self) -> i32 {
        self.spawned_of(self.spawner_id).map_or(0, |l| {
            l.iter().filter(|n| !n.data().unit.is_dead()).count() as i32
        })
    }

    fn spawned_of(&self, key: u32) -> Option<Vec<NpcRef>> {
        self.spawned_map().get(&key).cloned()
    }

    /// The `Spawner.Template` of an NPC's spawner (this spawner when unknown).
    fn npc_spawner(&self, server: &GameServer, npc: &NpcRef) -> Option<Arc<NpcSpawner>> {
        server.systems.npcs.spawns.npc_spawner_of(npc.obj_id)
    }

    /// `InitializeSpawnableNpcs(template)`.
    pub fn initialize_spawnable_npcs(&self, template: Option<&NpcSpawnerTemplate>) {
        match template {
            Some(t) => self.st().spawnable_npcs = t.npcs.clone(),
            None => tracing::warn!(
                "Template or template.Npcs is null. SpawnableNpcs will not be initialized."
            ),
        }
    }

    /// The member update of `SpawnManager.GetNpcSpawner(spawnerId)` on an event spawner:
    /// `Template.Npcs[^1].MemberId = UnitId; .UnitId = UnitId; .MemberType = "Npc"`.
    pub fn retarget_last_template_npc(&self) {
        let mut st = self.st();
        if let Some(t) = st.template.as_mut() {
            let t = Arc::make_mut(t);
            if let Some(n) = t.npcs.last_mut() {
                n.member_id = self.unit_id;
                n.unit_id = self.unit_id;
                n.member_type = "Npc".into();
            }
        }
    }

    // ----- Update -----

    /// `Update()`: despawn (schedule / no player around) has priority over spawning.
    pub fn update(self: &Arc<Self>, server: &GameServer) {
        let _items = server.systems.economy.operation_scope();
        let _lock = server.systems.npcs.spawns.spawn_lock();
        let mut did_action = false;
        if self.can_despawn_npcs(server) {
            self.despawn_npcs(server);
            did_action = true;
        } else if !self.is_player_in_spawn_radius(server) && self.current_spawn_count() > 0 {
            self.despawn_npcs_now(server);
            did_action = true;
        }
        if !did_action && self.can_spawn_npcs(server) {
            self.do_spawn(server);
        }
    }

    /// The activation-path [`Self::update`] with an explicit warm presence: while a player is
    /// in the instance's prewarm ring (the client-visible neighbourhood or
    /// `World.NpcPrewarmRegionMargin` regions beyond it), NPCs prepared by the activation pass
    /// are not despawned merely for sitting outside the authored `TestRadiusPc` /
    /// `TestRadiusNpc`; without warm presence they follow the normal no-player despawn. The
    /// authored despawn schedules, management gates, population caps / weights and individual
    /// pending respawn deadlines are unchanged, and spawning only happens while warm. Returns
    /// the number of NPCs actually created (at most `budget`).
    pub fn update_warm(
        self: &Arc<Self>,
        server: &GameServer,
        warm_present: bool,
        budget: i64,
    ) -> usize {
        let _items = server.systems.economy.operation_scope();
        let _lock = server.systems.npcs.spawns.spawn_lock();
        let mut did_action = false;
        if self.can_despawn_npcs(server) {
            self.despawn_npcs(server);
            did_action = true;
        } else if !warm_present && self.current_spawn_count() > 0 {
            self.despawn_npcs_now(server);
            did_action = true;
        }
        if !did_action && warm_present && self.can_spawn_npcs(server) {
            return self.do_spawn_bounded(server, budget);
        }
        0
    }

    /// `CanDespawnNpcs()`.
    pub fn can_despawn_npcs(&self, server: &GameServer) -> bool {
        self.is_despawning_schedule_enabled(server, self.spawner_id)
    }

    pub(crate) fn despawn_npcs(self: &Arc<Self>, server: &GameServer) {
        if self.is_despawn_scheduled() {
            tracing::debug!(
                "[Despawn] is already at the stage of removal for SpawnerId={}.",
                self.spawner_id
            );
            return;
        }
        if let Some(npcs) = self.spawned_of(self.spawner_id) {
            if self.has_spawning_time() {
                // A clock change removes the living population on the next cleanup tick.
                // Repeated schedule checks must not restart a delayed despawn or touch loot.
                let living: Vec<_> = npcs
                    .into_iter()
                    .filter(|npc| !npc.data().unit.is_dead())
                    .collect();
                self.do_despawns_now(server, &living);
            } else {
                self.do_despawns(server, &npcs);
            }
        }
    }

    /// `DespawnNpcsNow()`.
    pub fn despawn_npcs_now(self: &Arc<Self>, server: &GameServer) {
        if self.is_despawn_scheduled() {
            tracing::debug!(
                "[Despawn] is already at the stage of removal for SpawnerId={}.",
                self.spawner_id
            );
            return;
        }
        if let Some(npcs) = self.spawned_of(self.spawner_id) {
            self.do_despawns_now(server, &npcs);
        }
    }

    /// Distance reclamation of a cold ordinary placement (no nearby players for
    /// `World.NpcWarmRetentionSeconds`): removes up to `budget` living NPCs of *this* clone in
    /// place and returns how many were removed. Removal is completed in this pass (never a
    /// synthesized respawn delay, never a queued `DoDespawnNow` that could delete a refilled /
    /// re-shown NPC on the next respawn tick) and keeps the same `_scheduledCount` reservation
    /// credit as `DespawnNpcsNow`. NPCs already waiting for a queued despawn, corpses with their
    /// own corpse / respawn timers, stale slots (reused object ids) and NPCs with ongoing
    /// activity ([`reclaimable_npc`]: casting / plots / controllers, persistent or authored
    /// always-active AI, actual combat against a live in-leash opponent) are left alone, as are
    /// live residents ([`npc_is_resident`] under the `World.NpcResidentServiceNpcs` gate), so a
    /// mixed service / hostile placement keeps its resident members and expires its nonresident
    /// ones. A frozen placement ([`Self::saved_death`]) is never reclaimed: nothing refills it
    /// (`check_spawn_count_can_spawn` / `do_spawn_bounded` refuse it), so reclaiming the
    /// survivors of a one-run placement would leave its room empty for the rest of the run.
    /// The caller holds the activation pass's serialization.
    pub fn reclaim_cold_npcs(
        self: &Arc<Self>,
        server: &GameServer,
        budget: i64,
        residents: &ResidentNpcTemplates,
        resident_gate: bool,
    ) -> usize {
        if budget <= 0 || self.shared.saved_death() {
            return 0;
        }
        let _lock = server.systems.npcs.spawns.spawn_lock();
        let Some(npcs) = self.spawned_of(self.spawner_id) else {
            return 0;
        };
        let mut reclaimed = 0usize;
        self.st().is_despawn_scheduled = true;
        for npc in npcs {
            if reclaimed as i64 >= budget {
                break;
            }
            let obj = WorldObject::from(&npc);
            // Exact current world identity: a stale slot entry (the id was reused) leaves the
            // list without touching the replacement or releasing an id it does not own.
            if !server
                .world
                .object(npc.obj_id)
                .and_then(|o| o.as_npc().cloned())
                .is_some_and(|current| Arc::ptr_eq(&current, &npc))
            {
                self.remove_spawned_npc_exact(self.spawner_id, &npc);
                continue;
            }
            if npc.data().unit.is_dead() {
                continue; // the corpse keeps its own despawn / respawn deadlines
            }
            // An NPC already scheduled for despawn stays scheduled; re-reclaiming it would not
            // drain the placement faster and would consume the budget twice.
            if spawn_manager::manager_of(server, &obj, self.parent_world())
                .is_some_and(|m| m.has_pending_despawn(npc.obj_id))
            {
                continue;
            }
            if !reclaimable_npc(server, &npc) {
                continue;
            }
            // Per-live-NPC residency: the placement may mix resident members (kept) with
            // hostile / nonresident members (removed), and a runtime-owned NPC follows its
            // owner's lifecycle instead of the residency of its template.
            if resident_gate && npc_is_resident(server, residents, &npc) {
                continue;
            }
            self.despawn_reclaimed_now(server, &npc);
            reclaimed += 1;
        }
        self.st().is_despawn_scheduled = false;
        reclaimed
    }

    fn can_spawn_npcs(&self, server: &GameServer) -> bool {
        if !self.can_spawn(server) {
            return false;
        }
        // IsSpawnDelayNotElapsed(): `_lastSpawnTime` is a readonly DateTime.MinValue in the C#,
        // so the spawn delay never blocks.
        true
    }

    fn can_spawn(&self, server: &GameServer) -> bool {
        if !self.shared.management_enabled() {
            return false;
        }
        if self.template().is_none() {
            tracing::warn!(
                "[Spawn [SpawnerId={}, UnitId={}] Template is null. Cannot determine if NPC can be spawned.",
                self.spawner_id,
                self.unit_id
            );
            return false;
        }
        if self.is_despawn_scheduled() {
            return false;
        }
        // `IsSpawnScheduled` is deliberately not a gate here: it used to be set for every
        // ordinary spawner by `IsSpawningScheduleEnabled`, which froze adaptive growth and left
        // clones blocked by the shared caps permanently unscheduled. The authoritative live +
        // future-slot check below guards `Update`; the flag is only set when a fill happens.
        if !self.is_optimal_spawner(server) {
            return false;
        }
        if !self.is_spawning_schedule_enabled(server) {
            return false;
        }
        self.check_spawn_count_can_spawn(server)
    }

    /// `IsOptimalSpawner()`.
    pub fn is_optimal_spawner(&self, server: &GameServer) -> bool {
        self.select_spawner_id(server) == Some(self.spawner_id)
    }

    /// `SelectSpawnerId()`.
    fn select_spawner_id(&self, server: &GameServer) -> Option<u32> {
        if self.npc_spawner_ids.len() == 1 {
            return Some(self.spawner_id);
        }
        let category = self
            .template()
            .map_or(npc_spawner_category::NORMAL, |t| t.npc_spawner_category_id);
        if category == npc_spawner_category::AUTOCREATED && !self.has_scheduled_spawner(server) {
            return Some(self.spawner_id);
        }
        if self.is_there_spawning_schedule(server) {
            return Some(self.spawner_id);
        }
        None
    }

    /// `IsThereSpawningSchedule()`.
    pub fn is_there_spawning_schedule(&self, server: &GameServer) -> bool {
        match period_status_npc(server, self.spawner_id) {
            PeriodStatus::NotFound => {}
            PeriodStatus::InProgress | PeriodStatus::NotStarted | PeriodStatus::Ended => {
                return true
            }
        }
        self.has_spawning_time()
    }

    fn has_spawning_time(&self) -> bool {
        self.template()
            .is_some_and(|t| t.start_time > 0.0 || t.end_time > 0.0)
    }

    /// `HasScheduledSpawner()`: whether any candidate spawner of the shared placement has game
    /// schedule rows (any status) or a defined time window. The schedule is looked up by each
    /// candidate's own template id — not through this clone's template — so an AUTOCREATED
    /// fallback next to a scheduled NORMAL sibling is suppressed before, during and after its
    /// event: an unscheduled automatic clone must never spawn year-round in its place.
    fn has_scheduled_spawner(&self, server: &GameServer) -> bool {
        if self.npc_spawner_ids.is_empty() {
            return true;
        }
        let data = &server.game_data.systems.npcs.npc_spawners;
        let mut result = false;
        for &spawner_id in &self.npc_spawner_ids {
            if spawner_id == 0 {
                continue;
            }
            let Some(t) = data.get_npc_spawner_template(spawner_id) else {
                continue;
            };
            let has_window = t.start_time > 0.0 && t.end_time > 0.0;
            if has_window || period_status_npc(server, spawner_id) != PeriodStatus::NotFound {
                if self.spawner_id != spawner_id {
                    result = true;
                } else {
                    return true;
                }
            }
        }
        result
    }

    /// The population this spawner keeps alive: at least the authored `Count` and
    /// `min_population` (default baseline 1), raised to the nearby player count when the players
    /// exceed the baseline, and clamped by `max_population` and a positive `suspend_spawn_count`
    /// (the hard living-population caps).
    fn target_population(&self, server: &GameServer) -> i64 {
        let Some(t) = self.template() else {
            return 0;
        };
        let baseline = i64::from(self.count).max(if t.min_population == 0 {
            1
        } else {
            i64::from(t.min_population)
        });
        let players = i64::from(self.get_number_of_player_in_spawn_radius(server, &t));
        let mut cap = i64::from(t.max_population);
        if t.suspend_spawn_count > 0 {
            cap = cap.min(i64::from(t.suspend_spawn_count));
        }
        baseline.max(players).min(cap)
    }

    /// Living NPCs across the shared `SpawnedNpcs` map (all memberwise clones), read directly
    /// without the 10 s `AreOtherNpcsInSpawnZone` cache. Handles are copied out first so no leaf
    /// lock is held while object data locks are taken.
    fn live_population_direct(&self) -> i64 {
        let lists: Vec<Vec<NpcRef>> = self.spawned_map().values().cloned().collect();
        lists
            .iter()
            .flatten()
            .filter(|n| !n.data().unit.is_dead())
            .count() as i64
    }

    /// Future respawns already scheduled for this placement (any clone sharing the
    /// `SpawnedNpcs` map): each keeps its own authored deadline, so one fill cannot consume
    /// another slot's reservation. Ids are matched through `npc_spawner_of` and the shared
    /// `Arc` (a spawner template id can be reused at different positions, the `Arc` cannot).
    fn future_pending_respawn_slots(&self, server: &GameServer) -> i64 {
        let Some(manager) = self
            .parent_world()
            .and_then(|id| server.systems.npcs.spawns.get(id))
        else {
            return 0;
        };
        let now = spawn_manager::now();
        manager
            .pending_respawns()
            .into_iter()
            .filter(|(_, at)| at.is_some_and(|t| t > now))
            .filter(|(obj_id, _)| {
                server
                    .systems
                    .npcs
                    .spawns
                    .npc_spawner_of(*obj_id)
                    .is_some_and(|spawner| Arc::ptr_eq(spawner.shared(), self.shared()))
            })
            .count() as i64
    }

    /// THIS clone's own future respawns (its exact `Arc`, not the whole shared placement):
    /// these are the timers its `_scheduledCount` credits back. Shared placement futures
    /// constrain capacity ([`Self::future_pending_respawn_slots`]); own futures do not constrain
    /// this clone's fill beyond their shared reservation, but a fill must never consume them.
    fn own_pending_respawn_slots(self: &Arc<Self>, server: &GameServer) -> i64 {
        let Some(manager) = self
            .parent_world()
            .and_then(|id| server.systems.npcs.spawns.get(id))
        else {
            return 0;
        };
        let now = spawn_manager::now();
        let me = Arc::clone(self);
        manager
            .pending_respawns()
            .into_iter()
            .filter(|(_, at)| at.is_some_and(|t| t > now))
            .filter(|(obj_id, _)| {
                server
                    .systems
                    .npcs
                    .spawns
                    .npc_spawner_of(*obj_id)
                    .is_some_and(|spawner| Arc::ptr_eq(&spawner, &me))
            })
            .count() as i64
    }

    /// `CheckSpawnCountCanSpawn()`. The C# caps the *live* population by a soft `maxPopulation`
    /// that it replaces with the nearby player count, which drops an authored group to one NPC
    /// as soon as a single player is near. The corrected check keeps the authored baseline
    /// ([`Self::target_population`]) and guards `Update` with the authoritative availability:
    /// the living NPCs of the whole shared map plus the slots still reserved by future respawn
    /// deadlines and by `enable_respawn` batches' partial respawns must stay below the target.
    fn check_spawn_count_can_spawn(&self, server: &GameServer) -> bool {
        if self.template().is_none() || self.shared.saved_death() {
            return false;
        }
        self.live_population_direct()
            + self.future_pending_respawn_slots(server)
            + server.systems.npcs.group_batches.reserved_slots(self)
            < self.target_population(server)
    }

    /// The authored activation radius of `template` in world units: `TestRadiusPc` when nonzero,
    /// else `TestRadiusNpc`, times 50.
    fn authored_radius(template: &NpcSpawnerTemplate) -> f32 {
        let r = if template.test_radius_pc == 0.0 {
            template.test_radius_npc
        } else {
            template.test_radius_pc
        };
        r * 50.0
    }

    /// The spawner's authored 3D activation radius in world units (`0` without a template).
    pub fn authored_spawn_radius(&self) -> f32 {
        self.template().map_or(0.0, |t| Self::authored_radius(&t))
    }

    /// `IsPlayerInSpawnRadius()`: any player *present in the spawner's owning instance* within
    /// `TestRadiusPc` (or `TestRadiusNpc`) * 50 of the spawn position (3D). The answer is cached
    /// per instance with the instance's [`WorldInstance::player_activity_revision`], so player
    /// arrival, movement (including within one region) and departure invalidate it immediately
    /// instead of the stale C# 10 s window.
    pub fn is_player_in_spawn_radius(&self, server: &GameServer) -> bool {
        let Some(t) = self.template() else {
            return false;
        };
        let test_radius = Self::authored_radius(&t);
        let test_radius = test_radius * test_radius;
        let Some(world) = spawn_manager::owning_world(server, self) else {
            return false;
        };
        let revision = world.player_activity_revision();
        let key = (world.id, self.spawner_id);
        {
            let cache = self
                .shared
                .player_in_radius_cache
                .lock()
                .expect("npc spawner poisoned");
            if let Some(&(v, at)) = cache.get(&key) {
                if at == revision {
                    return v;
                }
            }
        }
        let pos = self.position();
        let mut found = false;
        for p in world.all_characters() {
            // Only players actually present in this instance (a server-wide alias or another
            // instance's player at the same coordinates must not count).
            if !p.region().is_some_and(|(inst, _, _)| inst == world.id) {
                continue;
            }
            let (x, y, z) = player_world_pos(server, &p);
            let (dx, dy, dz) = (x - pos.x, y - pos.y, z - pos.z);
            if dx * dx + dy * dy + dz * dz <= test_radius {
                found = true;
                break;
            }
        }
        self.shared
            .player_in_radius_cache
            .lock()
            .expect("npc spawner poisoned")
            .insert(key, (found, revision));
        found
    }

    /// `GetNumberOfPlayerInSpawnRadius(template)`: players within `TestRadiusNpc * 50` (2D,
    /// `GetAround<Character>` in the owning instance) of the first NPC of the first
    /// `SpawnedNpcs` list (lowest key here; the C# takes the `ConcurrentDictionary` order),
    /// cached per instance with the instance's activity revision. Membership changes clear it
    /// (the anchor NPC may change).
    fn get_number_of_player_in_spawn_radius(
        &self,
        server: &GameServer,
        template: &NpcSpawnerTemplate,
    ) -> i32 {
        let Some(world) = spawn_manager::owning_world(server, self) else {
            return 0;
        };
        let revision = world.player_activity_revision();
        let key = (world.id, self.spawner_id);
        {
            let cache = self
                .shared
                .player_count_cache
                .lock()
                .expect("npc spawner poisoned");
            if let Some(&(v, at)) = cache.get(&key) {
                if at == revision {
                    return v;
                }
            }
        }
        if template.test_radius_npc <= 0.0 {
            return 0;
        }
        let mut player_count = 0;
        let first = self
            .spawned_map()
            .values()
            .next()
            .and_then(|l| l.first().cloned());
        if let Some(npc) = first {
            let tmp = count_players_around(server, &npc, template.test_radius_npc * 50.0);
            if player_count < tmp {
                player_count = tmp;
            }
        }
        self.shared
            .player_count_cache
            .lock()
            .expect("npc spawner poisoned")
            .insert(key, (player_count, revision));
        player_count
    }

    /// `AreOtherNpcsInSpawnZone()` (cached for 10 s): the living NPCs of the other spawner ids
    /// sharing this entry's `SpawnedNpcs` map. The C# adds *this* spawner's
    /// `CurrentSpawnCount + _scheduledCount` for every other non-empty list, so the shared-map
    /// port counts each alternate population's own living NPCs (corpses excluded).
    fn are_other_npcs_in_spawn_zone(&self) -> (bool, i32) {
        let now = spawn_manager::now();
        {
            let cache = self
                .shared
                .npcs_in_zone_cache
                .lock()
                .expect("npc spawner poisoned");
            if let Some(&(count, are, at)) = cache.get(&self.spawner_id) {
                if now.saturating_duration_since(at).as_secs_f64() < CACHE_SECONDS {
                    return (are, count);
                }
            }
        }
        let others: Vec<Vec<NpcRef>> = self
            .spawned_map()
            .iter()
            .filter(|(k, _)| **k != self.spawner_id)
            .map(|(_, l)| l.clone())
            .collect();
        let mut count = 0;
        for npcs in others {
            count += npcs.iter().filter(|n| !n.data().unit.is_dead()).count() as i32;
        }
        let are = count > 0;
        self.shared
            .npcs_in_zone_cache
            .lock()
            .expect("npc spawner poisoned")
            .insert(self.spawner_id, (count, are, now));
        (are, count)
    }

    // ----- spawn entry points -----

    /// `SpawnAll(beginning)`.
    pub fn spawn_all(self: &Arc<Self>, server: &GameServer, _beginning: bool) {
        if !self.shared.management_enabled() {
            return;
        }
        if self.is_spawning_schedule_enabled(server) {
            return;
        }
        self.do_spawn(server);
        // `if (IsSpawnScheduled) IsDespawningScheduleEnabled(SpawnerId);` has no side effect.
    }

    /// `Spawn(objId)`: `DoSpawn()` and the first NPC of this spawner (`None` where the C#
    /// throws `KeyNotFoundException`).
    pub fn spawn(self: &Arc<Self>, server: &GameServer, _obj_id: u32) -> Option<NpcRef> {
        self.do_spawn(server);
        self.spawned_of(self.spawner_id)
            .and_then(|l| l.into_iter().find(|n| !n.data().unit.is_dead()))
    }

    /// `ForceSpawn(objId)`: an explicit timing override that repopulates the placement now.
    /// Queued NPC respawns of the same shared placement (`SpawnedNpcs` / `Position` entry) are
    /// obsolete once the forced fill happens; they are cancelled first so their reservations
    /// cannot block or duplicate it. Corpse despawn timers, other placements and other instances
    /// stay intact; a cancelled id is released only when no world object or corpse timer owns it.
    pub fn force_spawn(self: &Arc<Self>, server: &GameServer, _obj_id: u32) -> Option<NpcRef> {
        let _items = server.systems.economy.operation_scope();
        let _serial = server.systems.npcs.spawns.spawn_lock();
        if self.spawned_map().is_empty() {
            let t = self.template();
            self.initialize_spawnable_npcs(t.as_deref());
        }
        if let Some(manager) = self
            .parent_world()
            .and_then(|id| server.systems.npcs.spawns.get(id))
        {
            manager.cancel_placement_npc_respawns(server, self.shared());
            // The cancelled deadlines no longer back these `_scheduledCount` credits.
            self.reconcile_scheduled_count(self.own_pending_respawn_slots(server));
        }
        self.do_spawn(server);
        self.spawned_of(self.spawner_id)
            .and_then(|l| l.into_iter().find(|n| !n.data().unit.is_dead()))
    }

    /// `NpcGroupSupporter::RespawnLostGroupMembers` step: recreates one lost member of an
    /// `enable_respawn` batch at the move leader's position plus the member's formation offset
    /// and registers it in `SpawnedNpcs` like a normal fill. The dead slot was reserved by the
    /// group timer, not by a respawn deadline, so no `_scheduledCount` credit is consumed. The
    /// caller re-attaches the batch membership fields and holds the spawn lock.
    pub(crate) fn respawn_group_member(
        self: &Arc<Self>,
        server: &GameServer,
        npc_id: u32,
        owner_id: u32,
        base: (f32, f32, f32, f32),
        offset: (f32, f32, f32),
    ) -> Option<NpcRef> {
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let npc =
            spawn_npc_template(server, npc_id, self, owner_id, Some(offset), Some(base)).ok()??;
        self.add_npc_to_spawned(self.spawner_id, npc.clone());
        Some(npc)
    }

    /// The spawner gates a group partial respawn must pass before a lost member is recreated
    /// (the `CanSpawn` checks of the spawner's own update): management, template, a queued
    /// despawn, the active clone and the time window / event period. The population gate
    /// allows the batch's whole authored size: groups spawn whole even above
    /// `max_population` (see `PORTING.md`), and the lost slot is already reserved, so the
    /// comparison is against `max(target_population, batch_size)`; a second batch filled by
    /// another clone still refuses it.
    pub(crate) fn allows_group_partial_respawn(
        &self,
        server: &GameServer,
        batch_size: i64,
    ) -> bool {
        if !self.shared.management_enabled()
            || self.shared.saved_death()
            || self.template().is_none()
            || self.is_despawn_scheduled()
            || !self.is_optimal_spawner(server)
            || !self.is_spawning_schedule_enabled(server)
        {
            return false;
        }
        let target = self.target_population(server).max(batch_size);
        let used = self.live_population_direct()
            + self.future_pending_respawn_slots(server)
            + server.systems.npcs.group_batches.reserved_slots(self);
        used <= target
    }

    /// Reconciles `_scheduledCount` with the pending respawn timers that remain after
    /// [`Self::force_spawn`] cancelled the obsolete ones; never below zero.
    pub fn reconcile_scheduled_count(&self, remaining: i64) {
        let mut st = self.st();
        st.scheduled_count = st
            .scheduled_count
            .min(remaining.clamp(0, i64::from(i32::MAX)) as i32)
            .max(0);
    }

    /// `SetSpawnScheduled(value)` (the respawn tick re-enables spawning with `false`).
    pub fn set_spawn_scheduled(&self, value: bool) {
        self.st().is_spawn_scheduled = value;
    }
}

/// Whether `npc`'s object id now belongs to a different live object (the handle is stale: the id
/// was released and reused). Mirrors the identity guard of `npc::manager::delete`.
pub(crate) fn npc_superseded(server: &GameServer, npc: &NpcRef) -> bool {
    server
        .world
        .object(npc.obj_id)
        .is_some_and(|o| !o.as_npc().is_some_and(|n| Arc::ptr_eq(n, npc)))
}

/// `IsWithinDespawnTime(npc)` on the NPC's spawner template: outside its time window.
fn is_within_despawn_time(server: &GameServer, template: Option<&NpcSpawnerTemplate>) -> bool {
    let Some(t) = template else {
        return false;
    };
    if t.start_time <= 0.0 && t.end_time <= 0.0 {
        return false;
    }
    !is_hour_between(server.time.get_time(), t.start_time, t.end_time)
}

/// Players within `radius` (2D, strict) of `npc` in its region neighbourhood
/// (`WorldManager.GetAround<Character>(npc, radius).Count`).
fn count_players_around(server: &GameServer, npc: &NpcRef, radius: f32) -> i32 {
    if radius <= 0.0 {
        return 0;
    }
    let obj = WorldObject::from(npc);
    let Some(world) = server.world.world_of(&obj) else {
        return 0;
    };
    if obj.region().is_none() {
        return 0;
    }
    let center = world.world_pos_rot(&obj);
    let sq = radius * radius;
    let players = world.players_around(obj);
    players
        .iter()
        .filter(|p| {
            let (x, y, _) = player_world_pos(server, p);
            let dx = (x - center.x) * (x - center.x);
            if dx > sq {
                return false;
            }
            let dy = (y - center.y) * (y - center.y);
            dx + dy < sq
        })
        .count() as i32
}

/// Whether a distance reclamation may remove `npc` now: no cast / plot / controller, no
/// persistent AI work (path / escort / follow / command queue), no authored
/// `NpcTemplate.ActivateAiAlways` AI and no actual combat (a live opponent inside the authored
/// leash). A busy or retired AI handle counts as active: the inspection never blocks, so the AI
/// tick may keep holding its state while it enters the spawn lifecycle without this check
/// deadlocking.
fn reclaimable_npc(server: &GameServer, npc: &NpcRef) -> bool {
    {
        let n = npc.data();
        let s = &n.unit.skill_state;
        if s.skill_task.is_some() || s.active_plot.is_some() || s.active_skill_controller.is_some()
        {
            return false;
        }
    }
    let ais = &server.systems.ai.ais;
    if ais.get(npc.obj_id).is_some() {
        // Authored always-active NPCs are important actors while their AI exists; deleting them
        // by distance would break the authored `ActivateAiAlways` behaviour.
        if ai_npc::template(server, npc).is_some_and(|t| t.activate_ai_always) {
            return false;
        }
        match ais.try_inspect(npc.obj_id, |ai| {
            ai.has_persistent_ai() || ai.current_command.is_some()
        }) {
            Some(false) => {}
            Some(true) | None => return false,
        }
    }
    !has_live_leash_opponent(server, npc)
}

/// Whether a live NPC counts as a resident actor for distance retention: its actual template
/// ([`ai_npc::template`]) is an ordinary quest giver / reporter or service role of the immutable
/// [`ResidentNpcTemplates`] set and it is not runtime-owned (spawned for a player / creator,
/// `Unit.OwnerId != 0`). A runtime-owned NPC follows its owner's lifecycle, never the residency
/// of its template. The caller applies the `World.NpcResidentServiceNpcs` gate.
pub(crate) fn npc_is_resident(
    server: &GameServer,
    residents: &ResidentNpcTemplates,
    npc: &NpcRef,
) -> bool {
    npc.data().unit.owner_id == 0
        && ai_npc::template(server, npc).is_some_and(|t| residents.is_resident(t))
}

/// Actual ongoing combat evidence: a live opponent (`CurrentTarget` / `AggroTable`) inside the
/// NPC's authored return distance. Mirrors `BaseCombatBehavior.ShouldReturn`: an opponent
/// outside the leash, or the NPC itself far beyond the absolute return distance, means the NPC
/// is returning / stale - a deleted or out-of-leash aggro target must not make a departed NPC
/// immortal.
fn has_live_leash_opponent(server: &GameServer, npc: &NpcRef) -> bool {
    let (engaged, target, aggro) = {
        let n = npc.data();
        let u = &n.unit;
        (
            u.is_in_battle || u.current_target != 0,
            u.current_target,
            u.skill_state
                .aggro_table
                .keys()
                .copied()
                .collect::<Vec<u32>>(),
        )
    };
    if !engaged && aggro.is_empty() {
        return false;
    }
    let (return_distance, absolute_return_distance) = ai_npc::template(server, npc)
        .map_or((0.0, 0.0), |t| {
            (t.return_distance, t.absolute_return_distance)
        });
    let return_distance = if return_distance > 0.0 {
        return_distance
    } else {
        50.0
    };
    let absolute_return_distance = if absolute_return_distance > 0.0 {
        absolute_return_distance
    } else {
        200.0
    };
    let me_obj = WorldObject::from(npc);
    let me = ai_npc::world_pos(server, &me_obj);
    // A busy AI already counts as active in `reclaimable_npc`; a free one tells the NPC's idle
    // position for the return-distance half of the leash check.
    let idle = server
        .systems
        .ai
        .ais
        .try_inspect(npc.obj_id, |ai| ai.idle_position);
    let mut opponents = aggro;
    if target != 0 && !opponents.contains(&target) {
        opponents.push(target);
    }
    for id in opponents {
        if id == 0 || id == npc.obj_id {
            continue;
        }
        let Some(opponent) = server.world.object(id) else {
            continue; // deleted / never materialized: stale aggro
        };
        if !opponent.is_unit() || ai_npc::is_dead(&opponent) {
            continue;
        }
        let opponent_pos = ai_npc::world_pos(server, &opponent);
        let mut should_return = calculate_distance(me, opponent_pos, true) > return_distance;
        if let Some(idle) = idle {
            let distance_to_idle = calculate_distance(me, idle, true);
            should_return |= distance_to_idle > return_distance;
            if should_return {
                should_return = distance_to_idle <= absolute_return_distance;
            }
        }
        if !should_return {
            return true;
        }
    }
    false
}

/// `GameObject.Spawn()` of an NPC + the zone-change dispatch.
pub fn spawn_into_world(server: &GameServer, world: &Arc<WorldInstance>, npc: &NpcRef) {
    let changes = world.spawn(npc, &server.view_ctx());
    crate::world::session::handle_zone_changes(server, changes);
}

/// `TaskManager.Schedule(new NpcSpawnerDoDespawnTask(npc), lifeTime)`:
/// `npc?.Spawner?.DoDespawn(npc)` after `secs` seconds.
pub(crate) fn schedule_npc_do_despawn(server: &GameServer, npc: &NpcRef, secs: f32) {
    let (Some(server), Ok(rt)) = (server.self_arc(), tokio::runtime::Handle::try_current()) else {
        tracing::warn!("NpcSpawnerDoDespawnTask: no runtime / server handle");
        return;
    };
    let npc = npc.clone();
    rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs_f32(secs.max(0.0))).await;
        let _serial = server.systems.npcs.spawns.spawn_lock();
        if !server
            .world
            .object(npc.obj_id)
            .and_then(|o| o.as_npc().cloned())
            .is_some_and(|n| Arc::ptr_eq(&n, &npc))
        {
            return;
        }
        if let Some(s) = server.systems.npcs.spawns.npc_spawner_of(npc.obj_id) {
            s.do_despawn(&server, &npc);
        }
    });
}

/// How far above a collision floor a spawn is still snapped onto it (m). The C# snapped only
/// within 1 m, which left spawns authored 1-5 m off the ground floating (or sunk).
pub(crate) const SPAWN_GROUND_SNAP: f32 = 5.0;

/// How far under solid terrain a spawn is still lifted onto the surface (m). The ground probe
/// found no floor below it, so it sits inside the terrain and the client draws it buried.
/// Deeper ones are left alone: likely real caves under solid terrain whose floor we lack.
pub(crate) const SPAWN_SINK_SNAP: f32 = 6.0;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod spawner_population_tests;

#[cfg(test)]
mod spawner_schedule_tests;

#[cfg(test)]
mod spawner_group_tests;
