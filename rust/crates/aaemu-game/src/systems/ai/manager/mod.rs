//! `Core/Managers/AIManager`: every NPC AI (in `AddAi` order), the 100 ms tick
//! (`TickManager.OnTick.Subscribe(Tick, 100 ms, useAsync: true)`), and the way other code
//! reaches an AI.
//!
//! Each AI lives behind its own mutex. Code outside the AI (skills, npcs, hooks) never blocks on
//! it: [`post`] queues the call and runs the queue when the AI is free; when the AI is busy
//! (another thread ticking it, or the call comes from inside the AI's own tick through a skill /
//! aggro hook) the current holder runs the queued calls before releasing it. This replaces the
//! C# unsynchronised re-entrant calls (`Ai.OnAggroTargetChanged()` from a damage effect while the
//! AI ticks) with "run right after the current AI step".
//!
//! The tick does **not** visit every registered AI. Its candidates are the AIs whose NPCs sit in
//! a region with player activity (gathered from the world regions) plus the explicitly maintained
//! sets: `woken` (persistent paths / follow targets / command queues / finite ongoing work /
//! calls still pending), `initializing` (newly added AIs) and `always`
//! (`NpcTemplate.ActivateAiAlways`). Every handle
//! carries its monotonic `AddAi` ordinal; only the wanted handles are collected (the sets are
//! keyed by ordinal, so an object-id reuse cannot mix a replaced handle into the selection),
//! deduplicated, sorted by ordinal and visited. A dormant AI far from every player is neither
//! locked nor visited (the full registry is only read by the low-frequency maintenance sweep,
//! `MAINTENANCE_SWEEP_TICKS`, which drops stale owners / NPCs deleted without the `npc_deleted`
//! hook). Pending calls always keep their AI woken. The plan is executed once per tick: the
//! default `World.AiWorkerThreads = 1` keeps the strict `AddAi` order, a configured value
//! (clamped to [`MAX_AI_WORKERS`]) splits it into a bounded number of contiguous chunk jobs on
//! the Tokio blocking pool.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::server::GameServer;
use crate::world::{NpcRef, WorldObject};

use super::framework::{self, AiCtx, AiKind, NpcAi};
use super::profile;

use super::tick_view;
mod tick;
pub use tick::{tick_all, tick_batch};

/// Ticks a newly added AI stays a candidate so its initialization (queued `GoToSpawn` /
/// home position / path, `NpcSpawnerNpc.Spawn`) is never starved by the region selection.
const INITIALIZING_TICKS: u64 = 20;
/// One NPC's tick longer than this is logged with the NPC, its AI kind and behavior, so a stalled
/// batch can be traced to its cause (a path search, a stuck lock, ...). Set above the 52-77 ms a
/// normal first navmesh cell load costs, so that expected lazy load does not warn.
const SLOW_NPC_TICK: Duration = Duration::from_millis(250);

/// Ticks between the dormant-handle maintenance sweeps (stale owners / NPCs deleted without the
/// `npc_deleted` hook). The sweep is the C# "visit every AI" behaviour at 1/50 the cost.
const MAINTENANCE_SWEEP_TICKS: u64 = 50;

/// Upper bound of concurrent chunk jobs in one tick batch (`World.AiWorkerThreads` is clamped
/// to `1..=MAX_AI_WORKERS`): a bounded number of jobs per batch instead of one task per NPC.
pub const MAX_AI_WORKERS: usize = 4;

/// A call queued for an AI.
pub type AiFn = Box<dyn FnOnce(&mut AiCtx<'_>) + Send>;

/// One NPC's AI.
pub struct AiHandle {
    pub obj_id: u32,
    /// Monotonic `AddAi` ordinal: total order of the registry, unique across object-id reuse.
    ordinal: u64,
    state: Mutex<NpcAi>,
    pending: Mutex<VecDeque<AiFn>>,
    removed: AtomicBool,
    /// `NpcAi::Tick` calls on this handle (tests / diagnostics).
    visits: AtomicU64,
}

impl std::fmt::Debug for AiHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiHandle")
            .field("obj_id", &self.obj_id)
            .field("ordinal", &self.ordinal)
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn try_lock<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

impl AiHandle {
    fn detach_locked(&self, st: &mut NpcAi) {
        st.owner = None;
        st.should_tick = false;
        lock(&self.pending).clear();
    }

    /// Runs the queued calls while holding the state (the holder's duty).
    fn drain_locked(&self, server: &GameServer, st: &mut NpcAi) {
        loop {
            if self.removed.load(Ordering::Acquire) {
                self.detach_locked(st);
                return;
            }
            let next = lock(&self.pending).pop_front();
            let Some(f) = next else { return };
            let Some(npc) = st.owner() else {
                continue; // Owner == null
            };
            let obj = WorldObject::from(&npc);
            let mut ctx = AiCtx {
                server,
                npc,
                obj,
                ai: st,
            };
            if catch_unwind(AssertUnwindSafe(|| f(&mut ctx))).is_err() {
                tracing::error!("AI call on npc {} panicked", self.obj_id);
            }
        }
    }

    /// Tries to become the holder and run the queue (repeats while calls keep arriving after a
    /// release, so nothing is left behind).
    fn try_drain(&self, server: &GameServer) {
        loop {
            {
                let Some(mut st) = try_lock(&self.state) else {
                    return; // the holder runs the queue
                };
                self.drain_locked(server, &mut st);
            }
            if lock(&self.pending).is_empty() {
                return;
            }
        }
    }
}

/// Counts and stage wall times of one [`tick_all`] / [`tick_batch`] pass (tests / diagnostics).
/// Counts are exact; the stage times are millisecond wall times (truncated) and are not
/// reproducible between passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AiTickDiagnostics {
    /// AIs selected for the tick after deduplication (active regions + woken sets).
    pub candidates: usize,
    /// AIs whose `NpcAi::Tick` ran (the rest were locked by another thread).
    pub ticked: usize,
    /// Candidates left alone because another thread held their AI lock (try-lock).
    pub busy: usize,
    /// Stale AIs removed during the tick (owner gone / NPC object deleted).
    pub removed: usize,
    /// Wall time of the candidate plan construction (`tick_candidates`: selecting,
    /// deduplicating and ordering the handles), ms. In a batch it is measured inside the plan
    /// job, so blocking-pool queue time is not included.
    pub plan_ms: u64,
    /// Wall time of behavior processing (`process_plan`: `NpcAi::Tick` over the plan plus the
    /// try-lock skips and drains), ms. Sequential passes measure `process_plan` directly; a
    /// multi-worker batch measures one span over its whole chunk phase (dispatch through all
    /// chunk awaits), so that span also includes chunk dispatch, blocking-pool queueing and
    /// await overhead.
    pub tick_ms: u64,
    /// Wall time of the maintenance sweep of this pass, ms; 0 when the pass had no sweep
    /// (`MAINTENANCE_SWEEP_TICKS` cadence).
    pub maintenance_ms: u64,
}

/// The opt-in profile window (`AAEMU_AI_PROFILE`): the counters to subtract, the batch wall
/// times and candidate counts of the current window.
#[derive(Debug, Default)]
struct ProfileWindow {
    last: Option<profile::Snapshot>,
    total_ms: Vec<u64>,
    plan_ms: Vec<u64>,
    tick_ms: Vec<u64>,
    candidates: Vec<u64>,
    batches: u64,
}

/// Batches per profile window (about one line per second at the 100 ms tick).
const PROFILE_WINDOW_BATCHES: u64 = 10;
/// Slow NPC ticks reported per profile window.
const PROFILE_TOP_SLOW_NPCS: usize = 5;

/// `AIManager`.
#[derive(Debug, Default)]
pub struct AiManager {
    ais: DashMap<u32, Arc<AiHandle>>,
    /// Monotonic `AddAi` ordinal source; the only ordering of the registry (no id list to scan).
    next_ordinal: AtomicU64,
    /// Explicit candidates outside player activity: persistent paths / follow targets / command
    /// queues / finite ongoing work (`NpcAi::has_persistent_ai`) and AIs with calls still
    /// pending, keyed by handle ordinal. Refreshed after every AI state change ([`post`],
    /// [`run_now`], the tick drains).
    woken: DashMap<u64, Arc<AiHandle>>,
    /// `ordinal -> (tick sequence of AddAi, handle)`: candidates for [`INITIALIZING_TICKS`] ticks
    /// even while no callback has reached them yet.
    initializing: DashMap<u64, (u64, Arc<AiHandle>)>,
    /// `NpcTemplate.ActivateAiAlways`: candidates on every tick, players or not, by ordinal.
    always: DashMap<u64, Arc<AiHandle>>,
    /// Tick counter of [`tick_all`] (initialization freshness + sweep cadence).
    tick_seq: AtomicU64,
    /// Counts of the last tick.
    last_tick: Mutex<AiTickDiagnostics>,
    /// Serializes asynchronous batches: [`tick_batch`] is public and may be called
    /// concurrently, and two batches must never overlap on one manager.
    batch_lock: tokio::sync::Mutex<()>,
    /// `AAEMU_AI_PROFILE` window: the last profile snapshot and the window's batch samples.
    profile_window: Mutex<ProfileWindow>,
    /// Test-only: ordinals of the handles whose `NpcAi::Tick` ran, in execution order.
    #[cfg(test)]
    tick_order: Mutex<Vec<u64>>,
    started: AtomicBool,
}

impl AiManager {
    /// `AIManager.AddAi(ai)` (+ `npc.Ai = ai`). An already registered object id (recycled id) is
    /// a replacement: the old handle is marked removed and its claims are dropped, so a stale
    /// snapshot can never tick again or touch the replacement's candidate state.
    pub fn add_ai(&self, npc: &NpcRef, kind: AiKind) -> Arc<AiHandle> {
        let h = Arc::new(AiHandle {
            obj_id: npc.obj_id,
            ordinal: self.next_ordinal.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(NpcAi::new(kind, npc)),
            pending: Mutex::new(VecDeque::new()),
            removed: AtomicBool::new(false),
            visits: AtomicU64::new(0),
        });
        if let Some(old) = self.ais.insert(npc.obj_id, h.clone()) {
            self.retire(&old);
        }
        self.initializing.insert(
            h.ordinal,
            (self.tick_seq.load(Ordering::Relaxed), h.clone()),
        );
        npc.data().has_ai = true;
        h
    }

    /// Marks an AI as always active (`NpcTemplate.ActivateAiAlways`): it stays a candidate
    /// even when no player is in its region.
    pub fn mark_always_active(&self, obj_id: u32) {
        if let Some(h) = self.get(obj_id) {
            if !h.removed.load(Ordering::Acquire) {
                self.always.insert(h.ordinal, h);
            }
        }
    }

    /// The AI of an NPC.
    pub fn get(&self, obj_id: u32) -> Option<Arc<AiHandle>> {
        self.ais.get(&obj_id).map(|h| h.clone())
    }

    /// `Npc.Delete()`: `Ai.Owner = null; Ai = null` (the AI stops ticking and is forgotten).
    pub fn remove(&self, obj_id: u32) {
        if let Some((_, h)) = self.ais.remove(&obj_id) {
            self.retire(&h);
        }
    }

    /// Removes the registry entry only while it still is the expected handle (a stale snapshot
    /// of a replaced handle must never drop the replacement).
    fn remove_if_current(&self, h: &Arc<AiHandle>) -> bool {
        let Some((_, current)) = self.ais.remove_if(&h.obj_id, |_, v| Arc::ptr_eq(v, h)) else {
            return false;
        };
        self.retire(&current);
        true
    }

    /// Whether `h` still is the current registry entry of its object id. Claims are keyed by the
    /// handle ordinal, so an object-id reuse replaces the registry entry with a different handle.
    fn is_current_handle(&self, h: &Arc<AiHandle>) -> bool {
        self.ais
            .get(&h.obj_id)
            .is_some_and(|cur| Arc::ptr_eq(cur.value(), h))
    }

    /// Stops an AI and drops every claim it holds (registry entry or sets). Idempotent; the
    /// state itself detaches when a busy holder drains.
    fn retire(&self, h: &AiHandle) {
        h.removed.store(true, Ordering::Release);
        lock(&h.pending).clear();
        if let Some(mut st) = try_lock(&h.state) {
            h.detach_locked(&mut st);
        }
        self.drop_handle_sets(h.ordinal);
    }

    /// Drops the candidate claims of one handle ordinal (replacement / removal clean-up). A
    /// stale handle can only ever touch its own ordinal, never a replacement's.
    fn drop_handle_sets(&self, ordinal: u64) {
        self.woken.remove(&ordinal);
        self.initializing.remove(&ordinal);
        self.always.remove(&ordinal);
    }

    /// Cleanup safety net of one sweep: drops the claims whose handle no longer is the current
    /// registry entry of its object id (replaced / removed) and the initialization claims past
    /// their grace period. Each claim is verified against the **current** registry, never
    /// against the sweep's snapshot: a handle registered while the sweep runs keeps its
    /// initialization grace and `ActivateAiAlways` claim. The claim handles are collected with
    /// the registry locks released before the registry is read, so no two registry locks are
    /// held at once; removals use the claim's own ordinal key, which is unique per handle.
    fn prune_claims(&self) {
        let seq = self.tick_seq.load(Ordering::Relaxed);
        let woken: Vec<(u64, Arc<AiHandle>)> = self
            .woken
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let always: Vec<(u64, Arc<AiHandle>)> = self
            .always
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let initializing: Vec<(u64, u64, Arc<AiHandle>)> = self
            .initializing
            .iter()
            .map(|e| {
                let (born, h) = e.value();
                (*e.key(), *born, h.clone())
            })
            .collect();
        for (ordinal, h) in woken {
            if !self.is_current_handle(&h) {
                self.woken.remove(&ordinal);
            }
        }
        for (ordinal, h) in always {
            if !self.is_current_handle(&h) {
                self.always.remove(&ordinal);
            }
        }
        for (ordinal, born, h) in initializing {
            if !self.is_current_handle(&h) || seq.saturating_sub(born) > INITIALIZING_TICKS {
                self.initializing.remove(&ordinal);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.ais.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ais.is_empty()
    }

    /// Reads an AI's state (tests / diagnostics; blocks while the AI runs).
    pub fn inspect<R>(&self, obj_id: u32, f: impl FnOnce(&NpcAi) -> R) -> Option<R> {
        let h = self.get(obj_id)?;
        let st = lock(&h.state);
        Some(f(&st))
    }

    /// Reads an AI's state only while it is actually readable (tests / diagnostics; never
    /// blocks). `None` when there is no such AI, it was replaced, or another thread currently
    /// holds its state (the tick's try-lock discipline).
    pub fn try_inspect<R>(&self, obj_id: u32, f: impl FnOnce(&NpcAi) -> R) -> Option<R> {
        let h = self.get(obj_id)?;
        if h.removed.load(Ordering::Acquire) {
            return None;
        }
        let st = try_lock(&h.state)?;
        if h.removed.load(Ordering::Acquire) {
            return None; // retired between the check above and acquiring the state
        }
        Some(f(&st))
    }

    /// Counts of the last [`tick_all`] pass.
    pub fn tick_stats(&self) -> AiTickDiagnostics {
        *lock(&self.last_tick)
    }

    /// How often `NpcAi::Tick` ran on an AI (`None` = no such AI).
    pub fn visits(&self, obj_id: u32) -> Option<u64> {
        self.ais
            .get(&obj_id)
            .map(|h| h.visits.load(Ordering::Relaxed))
    }

    /// Re-evaluates the explicit candidate set after an AI state change (a persistent path /
    /// follow target / queued command / finite ongoing work or calls still pending keep it
    /// woken; a dormant AI with none of those leaves the set). A replaced handle (`removed`) is
    /// a no-op: it must not clear the wake state of the handle that replaced it.
    fn refresh_woken(&self, h: &Arc<AiHandle>) {
        if h.removed.load(Ordering::Acquire) {
            return;
        }
        if let Some(st) = try_lock(&h.state) {
            let keep = st.has_persistent_ai() || !lock(&h.pending).is_empty();
            self.set_woken(h, keep);
        } else if !lock(&h.pending).is_empty() {
            // The current holder runs the queue; keep the AI in reach of the next tick.
            self.set_woken(h, true);
        }
    }

    fn set_woken(&self, h: &Arc<AiHandle>, woken: bool) {
        if h.removed.load(Ordering::Acquire) {
            return;
        }
        if woken {
            self.woken.insert(h.ordinal, h.clone());
        } else {
            self.woken.remove(&h.ordinal);
        }
    }
}

/// `npc.Ai != null`.
pub fn has_ai(server: &GameServer, obj_id: u32) -> bool {
    server.systems.ai.ais.get(obj_id).is_some()
}

/// Runs `f` on the NPC's AI now, or right after its current step (see the module docs).
/// Nothing happens for an NPC without an AI (`npc.Ai?.X()`).
pub fn post(server: &GameServer, obj_id: u32, f: impl FnOnce(&mut AiCtx<'_>) + Send + 'static) {
    let Some(h) = server.systems.ai.ais.get(obj_id) else {
        return;
    };
    lock(&h.pending).push_back(Box::new(f));
    h.try_drain(server);
    server.systems.ai.ais.refresh_woken(&h);
}

/// Runs `f` on the NPC's AI right away (waiting for its current step to finish) and returns
/// the result: a synchronous `npc.Ai.X()` call from outside the AI (GM commands). `None` for an
/// NPC without an AI (`npc.Ai == null`). Calls queued meanwhile run afterwards.
pub fn run_now<R>(
    server: &GameServer,
    obj_id: u32,
    f: impl FnOnce(&mut AiCtx<'_>) -> R,
) -> Option<R> {
    let h = server.systems.ai.ais.get(obj_id)?;
    let res = {
        let mut st = lock(&h.state);
        if h.removed.load(Ordering::Acquire) {
            return None;
        }
        let npc = st.owner()?;
        let obj = WorldObject::from(&npc);
        let r = {
            let mut ctx = AiCtx {
                server,
                npc,
                obj,
                ai: &mut st,
            };
            f(&mut ctx)
        };
        h.drain_locked(server, &mut st);
        r
    };
    h.try_drain(server);
    server.systems.ai.ais.refresh_woken(&h);
    Some(res)
}

/// `AIManager.Initialize()`: the 100 ms tick (one batch at a time, measured delta), on the
/// blocking pool so long ticks do not stall the runtime.
pub fn start_tick(server: &Arc<GameServer>) {
    let manager = &server.systems.ai.ais;
    if manager.started.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(server);
    tokio::spawn(async move {
        let mut last = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let delta = last.elapsed();
            if delta <= Duration::from_millis(100) {
                continue;
            }
            last = Instant::now();
            let Some(server) = weak.upgrade() else { return };
            let delta_ms = delta.as_millis() as i64;
            let started = Instant::now();
            let diag = tick_batch(&server, delta_ms).await;
            let took = started.elapsed();
            if took > Duration::from_millis(100) {
                tracing::warn!(
                    "AI tick took {}ms to finish (plan {}ms, tick {}ms, maintenance {}ms; candidates {}, ticked {}, busy {}, removed {})",
                    took.as_millis(),
                    diag.plan_ms,
                    diag.tick_ms,
                    diag.maintenance_ms,
                    diag.candidates,
                    diag.ticked,
                    diag.busy,
                    diag.removed
                );
            }
        }
    });
}

#[cfg(test)]
mod tests;
