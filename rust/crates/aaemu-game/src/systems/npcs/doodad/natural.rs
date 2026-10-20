//! Growth state of world-spawned ("natural") doodads: trees, ore, crops and the like.
//!
//! Deviation from the C# (see PORTING.md): `DoodadSpawner.Spawn` recreated every world doodad in
//! its Start phase with the full growth timer on each server start, so a server restarted more
//! often than the longest growth chain (up to ~100 h) never grew a mature resource. Here the
//! phase of every doodad whose template has a long timed phase chain is kept in
//! `doodad_spawn_states` and restored at the next start, with the same mechanism the player
//! doodad load uses (`OverridePhaseTime`): the phase timer resumes with its remaining delay, and
//! a phase that expired while the server was down fires right away.
//!
//! * Identity: [`spawn_key`] = the spawn entry's authored template id + position (0.1 m), not the
//!   doodad's current template (a `DoodadFuncRatioRespawn` swaps it) and not the JSON `Id`.
//! * Tracked: only templates with a phase func that waits at least [`MIN_TRACKED_DELAY_MS`]
//!   (`Timer` with a next phase, `Growth`, `Final` with `After`); shorter chains restart in
//!   seconds and static props have no state to keep.
//! * Writes are coalesced per key and flushed in batches by a background task, by the periodic
//!   save and on shutdown (`doodad::save`); the row is deleted when the doodad is deleted
//!   (harvest, despawn), so the respawn starts fresh as before.
//! * Only the main world is tracked and only after [`NaturalStates::load`] succeeded, so flow tests
//!   and servers without the table are unaffected.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use sqlx::{MySql, MySqlPool, QueryBuilder, Row};

use super::data::DoodadData;
use super::phase;
use super::templates::FuncTemplate;
use crate::server::GameServer;
use crate::world::{now_ms, DoodadRef};

/// Shortest wait of a phase func that makes its template worth persisting (5 minutes).
pub const MIN_TRACKED_DELAY_MS: i32 = 300_000;
/// How long a state change may wait in memory before it is written.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// Rows per multi-row statement.
const BATCH: usize = 250;

/// One `doodad_spawn_states` row.
#[derive(Debug, Clone, PartialEq)]
pub struct StateRow {
    pub world_id: u32,
    pub spawn_key: String,
    /// Template of the doodad at save time (differs from the spawn entry's after a ratio respawn).
    pub template_id: u32,
    pub func_group_id: u32,
    /// Unix seconds.
    pub plant_time: i64,
    /// Unix ms.
    pub growth_time_ms: i64,
    /// Unix ms.
    pub phase_time_ms: i64,
    pub data: i32,
    pub interaction_counts: String,
    /// Unix seconds of the snapshot.
    pub updated: i64,
}

#[derive(Debug, Clone)]
enum Op {
    Save(StateRow),
    Delete,
}

type Key = (u32, String);

#[derive(Debug, Default)]
struct Shared {
    /// Latest unwritten operation per key.
    pending: Mutex<HashMap<Key, Op>>,
    /// Rows loaded at startup, consumed by the initial spawn.
    restore: Mutex<HashMap<Key, StateRow>>,
    /// Spawner id that owns a key (two entries of one key: only the first is tracked).
    claims: Mutex<HashMap<Key, u32>>,
}

/// The natural doodad state store (`Persistence::natural`).
#[derive(Debug)]
pub struct NaturalStates {
    /// World instance whose doodads are tracked (`NO_WORLD` = tracking off).
    world: AtomicI64,
    shared: Arc<Shared>,
    flusher: Mutex<bool>,
    restored: AtomicUsize,
    invalid: AtomicUsize,
}

const NO_WORLD: i64 = -1;

impl Default for NaturalStates {
    fn default() -> Self {
        Self {
            world: AtomicI64::new(NO_WORLD),
            shared: Arc::default(),
            flusher: Mutex::default(),
            restored: AtomicUsize::default(),
            invalid: AtomicUsize::default(),
        }
    }
}

/// Stable identity of a doodad spawn entry.
pub fn spawn_key(unit_id: u32, x: f32, y: f32, z: f32) -> String {
    let r = |v: f32| (v * 10.0).round() as i64;
    format!("{unit_id}:{}:{}:{}", r(x), r(y), r(z))
}

/// Whether the template waits long enough in some phase for its state to be worth keeping.
pub fn tracks_state(data: &DoodadData, template_id: u32) -> bool {
    data.doodad_func_groups(template_id).iter().any(|g| {
        data.phase_funcs(g.id).iter().any(|pf| {
            match data.phase_func_template(pf.func_id, &pf.func_type) {
                Some(FuncTemplate::Timer(f)) => f.next_phase > 0 && f.delay >= MIN_TRACKED_DELAY_MS,
                Some(FuncTemplate::Growth(f)) => f.delay >= MIN_TRACKED_DELAY_MS,
                Some(FuncTemplate::Final(f)) => f.after >= MIN_TRACKED_DELAY_MS,
                _ => false,
            }
        })
    })
}

impl NaturalStates {
    /// Tracking is on (for one world).
    pub fn enabled(&self) -> bool {
        self.world.load(Ordering::Acquire) != NO_WORLD
    }

    /// Tracking is on for the doodads of `world_id`.
    pub fn enabled_for(&self, world_id: u32) -> bool {
        self.world.load(Ordering::Acquire) == i64::from(world_id)
    }

    /// Enables tracking for tests (no database).
    #[cfg(test)]
    pub(crate) fn enable_for_test(&self, rows: Vec<StateRow>) {
        self.world.store(0, Ordering::Release);
        let mut restore = self.shared.restore.lock().expect("natural poisoned");
        for r in rows {
            restore.insert((r.world_id, r.spawn_key.clone()), r);
        }
    }

    /// The unwritten operations (tests): `(key, Some(row) = save | None = delete)`.
    #[cfg(test)]
    pub(crate) fn pending_for_test(&self) -> Vec<(Key, Option<StateRow>)> {
        self.shared
            .pending
            .lock()
            .expect("natural poisoned")
            .iter()
            .map(|(k, op)| {
                (
                    k.clone(),
                    match op {
                        Op::Save(r) => Some(r.clone()),
                        Op::Delete => None,
                    },
                )
            })
            .collect()
    }

    /// Loads the rows of `world_id` for the initial spawn and turns tracking on. An error (e.g.
    /// the table is missing) leaves tracking off.
    pub async fn load(&self, pool: &MySqlPool, world_id: u32) -> sqlx::Result<usize> {
        let rows = sqlx::query(
            "SELECT spawn_key, CAST(template_id AS SIGNED) AS template_id, \
             CAST(func_group_id AS SIGNED) AS func_group_id, plant_time, growth_time, phase_time, \
             data, interaction_counts, updated FROM doodad_spawn_states WHERE world_id = ?",
        )
        .bind(world_id)
        .fetch_all(pool)
        .await?;
        let mut map = HashMap::with_capacity(rows.len());
        for r in &rows {
            let row = StateRow {
                world_id,
                spawn_key: r.try_get("spawn_key")?,
                template_id: r.try_get::<i64, _>("template_id")? as u32,
                func_group_id: r.try_get::<i64, _>("func_group_id")? as u32,
                plant_time: r.try_get("plant_time")?,
                growth_time_ms: r.try_get("growth_time")?,
                phase_time_ms: r.try_get("phase_time")?,
                data: r.try_get("data")?,
                interaction_counts: r
                    .try_get::<Option<String>, _>("interaction_counts")?
                    .unwrap_or_default(),
                updated: r.try_get("updated")?,
            };
            map.insert((world_id, row.spawn_key.clone()), row);
        }
        let n = map.len();
        *self.shared.restore.lock().expect("natural poisoned") = map;
        self.shared.claims.lock().expect("natural poisoned").clear();
        self.restored.store(0, Ordering::Relaxed);
        self.invalid.store(0, Ordering::Relaxed);
        self.world.store(i64::from(world_id), Ordering::Release);
        Ok(n)
    }

    /// First spawner of a key owns it; a second entry of the same key is not tracked.
    fn claim(&self, key: &Key, spawner_id: u32) -> bool {
        let mut claims = self.shared.claims.lock().expect("natural poisoned");
        *claims.entry(key.clone()).or_insert(spawner_id) == spawner_id
    }

    fn take_restore(&self, key: &Key) -> Option<StateRow> {
        self.shared
            .restore
            .lock()
            .expect("natural poisoned")
            .remove(key)
    }

    fn queue(&self, server: &GameServer, key: Key, op: Op) {
        self.shared
            .pending
            .lock()
            .expect("natural poisoned")
            .insert(key, op);
        self.ensure_flusher(server);
    }

    fn ensure_flusher(&self, server: &GameServer) {
        let mut started = self.flusher.lock().expect("natural poisoned");
        if *started {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        *started = true;
        let shared: Weak<Shared> = Arc::downgrade(&self.shared);
        let pool = server.db.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(FLUSH_INTERVAL).await;
                let Some(shared) = shared.upgrade() else {
                    break;
                };
                flush_shared(&shared, &pool).await;
            }
        });
    }

    /// Writes everything pending (the periodic save and the shutdown save).
    pub async fn flush(&self, pool: &MySqlPool) {
        flush_shared(&self.shared, pool).await;
    }

    /// Rows nobody claimed (their spawn entry no longer exists): queue their deletion and log the
    /// outcome of the initial spawn. Returns `(restored, discarded)`.
    pub fn finish_initial_spawn(
        &self,
        server: &GameServer,
        world_id: u32,
        world_name: &str,
    ) -> (usize, usize) {
        if !self.enabled_for(world_id) {
            return (0, 0);
        }
        let left: Vec<Key> = {
            let mut restore = self.shared.restore.lock().expect("natural poisoned");
            restore.drain().map(|(k, _)| k).collect()
        };
        let discarded = left.len();
        for k in left {
            self.queue(server, k, Op::Delete);
        }
        let restored = self.restored.load(Ordering::Relaxed);
        let invalid = self.invalid.load(Ordering::Relaxed);
        tracing::info!("Restored {restored} natural doodad growth states for {world_name}");
        if discarded + invalid > 0 {
            tracing::warn!(
                "Natural doodad growth states for {world_name}: ignored {invalid} invalid and \
                 {discarded} without a spawn entry"
            );
        }
        (restored, discarded)
    }
}

async fn flush_shared(shared: &Shared, pool: &MySqlPool) {
    let ops: Vec<(Key, Op)> = {
        let mut pending = shared.pending.lock().expect("natural poisoned");
        if pending.is_empty() {
            return;
        }
        pending.drain().collect()
    };
    let mut saves: Vec<&StateRow> = Vec::new();
    let mut deletes: Vec<&Key> = Vec::new();
    for (k, op) in &ops {
        match op {
            Op::Save(r) => saves.push(r),
            Op::Delete => deletes.push(k),
        }
    }
    // One transaction: a single commit (disk sync) for the whole batch.
    let mut failed: Vec<(Key, Op)> = Vec::new();
    if let Err(e) = write_all(pool, &saves, &deletes).await {
        tracing::warn!("Natural doodad state write failed: {e}");
        failed.extend(ops.iter().map(|(k, op)| (k.clone(), op.clone())));
    }
    if !failed.is_empty() {
        // Retry later unless a newer operation arrived meanwhile.
        let mut pending = shared.pending.lock().expect("natural poisoned");
        for (k, op) in failed {
            pending.entry(k).or_insert(op);
        }
    }
}

async fn write_all(pool: &MySqlPool, saves: &[&StateRow], deletes: &[&Key]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for chunk in saves.chunks(BATCH) {
        write_saves(&mut *tx, chunk).await?;
    }
    for chunk in deletes.chunks(BATCH) {
        write_deletes(&mut *tx, chunk).await?;
    }
    tx.commit().await
}

async fn write_saves(
    conn: impl sqlx::Executor<'_, Database = MySql>,
    rows: &[&StateRow],
) -> sqlx::Result<()> {
    let mut q: QueryBuilder<MySql> = QueryBuilder::new(
        "REPLACE INTO doodad_spawn_states (world_id, spawn_key, template_id, func_group_id, \
         plant_time, growth_time, phase_time, data, interaction_counts, updated) ",
    );
    q.push_values(rows, |mut b, r| {
        b.push_bind(r.world_id)
            .push_bind(&r.spawn_key)
            .push_bind(r.template_id)
            .push_bind(r.func_group_id)
            .push_bind(r.plant_time)
            .push_bind(r.growth_time_ms)
            .push_bind(r.phase_time_ms)
            .push_bind(r.data)
            .push_bind(&r.interaction_counts)
            .push_bind(r.updated);
    });
    q.build().execute(conn).await?;
    Ok(())
}

async fn write_deletes(
    conn: impl sqlx::Executor<'_, Database = MySql>,
    keys: &[&Key],
) -> sqlx::Result<()> {
    let mut q: QueryBuilder<MySql> =
        QueryBuilder::new("DELETE FROM doodad_spawn_states WHERE (world_id, spawn_key) IN (");
    let mut list = q.separated(", ");
    for (world, key) in keys {
        list.push("(")
            .push_bind_unseparated(*world)
            .push_unseparated(", ")
            .push_bind_unseparated(key.as_str())
            .push_unseparated(")");
    }
    q.push(")");
    q.build().execute(conn).await?;
    Ok(())
}

/// The state of a tracked doodad (`None` when untracked, deleted or the counters cannot be
/// serialized).
pub fn snapshot(doodad: &DoodadRef) -> Option<StateRow> {
    let d = doodad.data();
    let (world_id, spawn_key) = d.natural_key.clone()?;
    if d.phase.deleted {
        return None;
    }
    Some(StateRow {
        world_id,
        spawn_key,
        template_id: d.template_id,
        func_group_id: d.func_group_id,
        plant_time: d.plant_time,
        growth_time_ms: d.growth_time_ms,
        phase_time_ms: d.phase_time_ms,
        data: d.data,
        interaction_counts: serde_json::to_string(&d.phase.interaction_counts).ok()?,
        updated: now_ms() / 1000,
    })
}

/// Queues the current state of a tracked doodad (a phase change; no-op for untracked doodads).
pub fn save(server: &GameServer, doodad: &DoodadRef) {
    let natural = &server.systems.npcs.doodads.persistence.natural;
    if !natural.enabled() {
        return;
    }
    // Snapshot and enqueue under the queue lock so a delete cannot be overtaken.
    let mut pending = natural.shared.pending.lock().expect("natural poisoned");
    if let Some(row) = snapshot(doodad) {
        pending.insert((row.world_id, row.spawn_key.clone()), Op::Save(row));
        drop(pending);
        natural.ensure_flusher(server);
    }
}

/// The doodad is gone (harvested, despawned, deleted): drop its row so a respawn starts fresh.
pub fn forget(server: &GameServer, doodad: &DoodadRef) {
    let key = doodad.data().natural_key.take();
    if let Some(key) = key {
        server
            .systems
            .npcs
            .doodads
            .persistence
            .natural
            .queue(server, key, Op::Delete);
    }
}

/// What the spawner needs before it creates the doodad of `spawner_id`'s entry: the saved state
/// of the entry, when it exists, is valid for the current data and the entry owns its key.
pub fn take_restore(
    server: &GameServer,
    world_id: u32,
    key: &str,
    spawner_id: u32,
    allow_restore: bool,
) -> (bool, Option<StateRow>) {
    let natural = &server.systems.npcs.doodads.persistence.natural;
    if !natural.enabled_for(world_id) {
        return (false, None);
    }
    let k = (world_id, key.to_string());
    if !natural.claim(&k, spawner_id) {
        return (false, None);
    }
    if !allow_restore {
        return (true, None);
    }
    let Some(row) = natural.take_restore(&k) else {
        return (true, None);
    };
    let data = &server.game_data.systems.npcs.doodad;
    let valid = data.exist(row.template_id)
        && tracks_state(data, row.template_id)
        && data
            .doodad_func_groups(row.template_id)
            .iter()
            .any(|g| g.id == row.func_group_id);
    if valid {
        (true, Some(row))
    } else {
        natural.invalid.fetch_add(1, Ordering::Relaxed);
        natural.queue(server, k, Op::Delete);
        (true, None)
    }
}

/// After the spawner created and positioned `doodad`: restores the saved state (the same way the
/// player doodad load does) and starts tracking when the template qualifies. `owned` is the first
/// result of [`take_restore`].
pub fn attach(
    server: &GameServer,
    doodad: &DoodadRef,
    world_id: u32,
    key: &str,
    owned: bool,
    restored: Option<StateRow>,
) {
    if !owned {
        return;
    }
    let natural = &server.systems.npcs.doodads.persistence.natural;
    if let Some(row) = restored {
        {
            let mut d = doodad.data();
            d.plant_time = row.plant_time;
            d.growth_time_ms = row.growth_time_ms;
            d.data = row.data;
        }
        // `FuncGroupId = saved` (setter: PhaseTime = now, phase caches), then the saved times.
        phase::set_func_group_id(server, doodad, row.func_group_id);
        let counts = serde_json::from_str(&row.interaction_counts).unwrap_or_default();
        {
            let mut d = doodad.data();
            d.phase.override_phase_time_ms = row.phase_time_ms;
            d.phase_time_ms = row.phase_time_ms;
            d.phase.interaction_counts = counts;
            d.update_final_phase_end();
        }
        natural.restored.fetch_add(1, Ordering::Relaxed);
    }
    let template_id = doodad.data().template_id;
    if tracks_state(&server.game_data.systems.npcs.doodad, template_id) {
        doodad.data().natural_key = Some((world_id, key.to_string()));
    }
}

#[cfg(test)]
#[path = "natural_tests.rs"]
mod tests;
