use super::{
    catch_unwind, lock, profile, try_lock, AiCtx, AiHandle, AiTickDiagnostics, Arc,
    AssertUnwindSafe, BTreeMap, GameServer, HashSet, Instant, Ordering, WorldObject,
    INITIALIZING_TICKS, MAINTENANCE_SWEEP_TICKS, MAX_AI_WORKERS, PROFILE_TOP_SLOW_NPCS,
    PROFILE_WINDOW_BATCHES, SLOW_NPC_TICK,
};

/// Collects the handles wanted for one tick: the NPCs whose regions have player activity
/// (`Region.HasPlayerActivity()`), plus the woken / initializing / always-active sets, keyed by
/// handle ordinal. Only wanted handles are touched (the registry is never scanned here); the
/// monotonic `AddAi` ordinal gives the order and the deduplication. The returned set holds the
/// ordinals of the locally active handles (their tick gets `region_activity = true`).
pub(super) fn tick_candidates(server: &GameServer, seq: u64) -> (Vec<Arc<AiHandle>>, HashSet<u64>) {
    let manager = &server.systems.ai.ais;
    let mut local = HashSet::new();
    let mut wanted: BTreeMap<u64, Arc<AiHandle>> = BTreeMap::new();
    let mut want = |h: Arc<AiHandle>, is_local: bool| {
        if h.removed.load(Ordering::Acquire) {
            return;
        }
        if is_local {
            local.insert(h.ordinal);
        }
        wanted.entry(h.ordinal).or_insert(h);
    };
    for world in server.world.worlds() {
        let mut sectors = HashSet::new();
        for p in world.all_characters() {
            let Some((inst, x, y)) = p.region() else {
                continue;
            };
            if inst != world.id {
                continue;
            }
            sectors.extend(world.neighbors((x, y)));
        }
        for sector in sectors {
            for obj in world.objects_in_region(sector) {
                if let Some(npc) = obj.as_npc() {
                    if let Some(h) = manager.get(npc.obj_id) {
                        want(h, true);
                    }
                }
            }
        }
    }
    for e in manager.woken.iter() {
        want(e.value().clone(), false);
    }
    for e in manager.initializing.iter() {
        let (born, h) = e.value();
        if seq.saturating_sub(*born) <= INITIALIZING_TICKS {
            want(h.clone(), false);
        }
    }
    for e in manager.always.iter() {
        want(e.value().clone(), false);
    }
    (wanted.into_values().collect(), local)
}

/// Processes one selected handle: the shared body of the sequential pass and the batch chunk
/// jobs (so the two paths cannot drift apart). Try-lock discipline unchanged: a busy AI is left
/// for the next tick, a replaced / gone one is detached through its own handle only.
fn tick_handle(
    server: &GameServer,
    h: &Arc<AiHandle>,
    local: &HashSet<u64>,
    delta_ms: i64,
    diag: &mut AiTickDiagnostics,
) {
    let manager = &server.systems.ai.ais;
    if h.removed.load(Ordering::Acquire) {
        return; // replaced after collection: `add_ai` already dropped its claims
    }
    let id = h.obj_id;
    let mut gone = false;
    {
        // External skill/aggro calls also run under this lock. Leave a busy
        // NPC for the next tick instead of stopping every other NPC's AI.
        let Some(mut st) = try_lock(&h.state) else {
            diag.busy += 1;
            return;
        };
        if h.removed.load(Ordering::Acquire) {
            // Replaced while waiting for the lock: retire the snapshot, not the replacement.
            h.detach_locked(&mut st);
            manager.drop_handle_sets(h.ordinal);
            return;
        }
        match st.owner() {
            None => gone = true,
            Some(npc) => {
                let region = npc.region();
                if region.is_some() {
                    st.was_in_world = true;
                } else if st.was_in_world && server.world.object(npc.obj_id).is_none() {
                    // Npc.Delete(): the AI is detached
                    gone = true;
                }
                if !gone {
                    let obj = WorldObject::from(&npc);
                    let activity =
                        local.contains(&h.ordinal) || manager.always.contains_key(&h.ordinal);
                    let behavior = st.current_behavior();
                    let mut ctx = AiCtx {
                        server,
                        npc,
                        obj,
                        ai: &mut st,
                    };
                    h.visits.fetch_add(1, Ordering::Relaxed);
                    diag.ticked += 1;
                    #[cfg(test)]
                    lock(&manager.tick_order).push(h.ordinal);
                    let npc_started = Instant::now();
                    if catch_unwind(AssertUnwindSafe(|| ctx.tick(delta_ms, activity))).is_err() {
                        tracing::error!("AI tick of npc {id} panicked");
                    }
                    let npc_took = npc_started.elapsed();
                    if profile::enabled() {
                        let ns = npc_took.as_nanos() as u64;
                        if let Some(behavior) = behavior {
                            profile::behavior_tick(behavior.index(), ns);
                        }
                        profile::record_slow_npc(
                            ns / 1_000,
                            id,
                            behavior.map(super::framework::BehaviorKind::index),
                        );
                    }
                    if npc_took > SLOW_NPC_TICK {
                        tracing::warn!(
                            "AI tick of npc {id} (template {}, {:?}, behavior {:?}) took {}ms",
                            ctx.npc.data().unit.template_id,
                            ctx.ai.kind,
                            ctx.ai.current_behavior(),
                            npc_took.as_millis()
                        );
                    }
                }
            }
        }
        if !gone {
            h.drain_locked(server, &mut st);
        }
    }
    if gone {
        if manager.remove_if_current(h) {
            diag.removed += 1;
        }
    } else {
        h.try_drain(server);
        let _profile = profile::step(profile::Step::WokenRefresh);
        manager.refresh_woken(h);
    }
}

/// Runs a slice of the candidate plan sequentially (shared by [`tick_all`] and every chunk job).
/// A panic on one handle is contained: the remaining handles still run and the panicking
/// handle's lock / selection state is released by unwinding through the existing guards.
fn process_plan(
    server: &GameServer,
    candidates: &[Arc<AiHandle>],
    local: &HashSet<u64>,
    delta_ms: i64,
    diag: &mut AiTickDiagnostics,
) {
    // One region/position view per pass: the perception scans of all candidates then read each
    // region once instead of re-reading it for every NPC (see `tick_view`).
    let _view = super::tick_view::begin();
    for h in candidates {
        if catch_unwind(AssertUnwindSafe(|| {
            tick_handle(server, h, local, delta_ms, diag)
        }))
        .is_err()
        {
            tracing::error!("AI tick of npc {} panicked", h.obj_id);
        }
    }
}

/// One chunk job of a multi-worker batch: its own partial diagnostics, no shared mutable state
/// between chunks.
fn process_chunk(
    server: &GameServer,
    chunk: &[Arc<AiHandle>],
    local: &HashSet<u64>,
    delta_ms: i64,
) -> AiTickDiagnostics {
    let mut diag = AiTickDiagnostics::default();
    process_plan(server, chunk, local, delta_ms, &mut diag);
    diag
}

/// Splits the ordinal-sorted plan into at most `workers` contiguous chunks: a bounded number of
/// jobs per batch (never one task per NPC) that keeps the `AddAi` order inside every chunk. An
/// empty plan costs no jobs.
pub(super) fn plan_chunks(candidates: &[Arc<AiHandle>], workers: usize) -> Vec<Vec<Arc<AiHandle>>> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let workers = workers.clamp(1, MAX_AI_WORKERS);
    if workers == 1 {
        return vec![candidates.to_vec()];
    }
    let chunk_size = candidates.len().div_ceil(workers).max(1);
    candidates.chunks(chunk_size).map(|c| c.to_vec()).collect()
}

/// Shared tail of one pass: the low-frequency maintenance sweep (once per batch) and the
/// diagnostics snapshot. `maintenance_ms` is the sweep's own wall time; a pass without a sweep
/// keeps the zeroed default.
fn finish_pass(server: &GameServer, seq: u64, diag: &mut AiTickDiagnostics) {
    if seq.is_multiple_of(MAINTENANCE_SWEEP_TICKS) {
        let started = Instant::now();
        maintenance_sweep(server, diag);
        diag.maintenance_ms = started.elapsed().as_millis() as u64;
    }
    *lock(&server.systems.ai.ais.last_tick) = *diag;
}

/// `AIManager.Tick(delta)`: the candidate AIs tick (`NpcAi.Tick`), in `AddAi` order. The
/// synchronous, always-sequential entry kept for existing callers / tests; production uses
/// [`tick_batch`].
///
/// Candidates are the NPCs in regions with player activity plus the woken / initializing /
/// always-active AIs, collected by handle (deduplicated by ordinal, sorted by ordinal). A busy
/// AI is left for the next tick; a gone / replaced one is detached through its own handle.
/// Dormant AIs outside every player neighbourhood are neither locked nor visited.
pub fn tick_all(server: &GameServer, delta_ms: i64) -> AiTickDiagnostics {
    let manager = &server.systems.ai.ais;
    let seq = manager.tick_seq.fetch_add(1, Ordering::Relaxed) + 1;
    let plan_started = Instant::now();
    let (candidates, local) = tick_candidates(server, seq);
    let plan_ms = plan_started.elapsed().as_millis() as u64;
    let mut diag = AiTickDiagnostics {
        candidates: candidates.len(),
        plan_ms,
        ..AiTickDiagnostics::default()
    };
    let tick_started = Instant::now();
    process_plan(server, &candidates, &local, delta_ms, &mut diag);
    diag.tick_ms = tick_started.elapsed().as_millis() as u64;
    finish_pass(server, seq, &mut diag);
    diag
}

/// Concurrent chunk jobs for one asynchronous batch: `World.AiWorkerThreads` clamped to
/// `1..=MAX_AI_WORKERS`. Read once per batch (the configuration is not re-read while the
/// batch runs); the default `1` keeps the strict `AddAi` execution order of [`tick_all`].
pub(super) fn batch_worker_count(server: &GameServer) -> usize {
    (server.config.world.ai_worker_threads as usize).clamp(1, MAX_AI_WORKERS)
}

/// One asynchronous tick batch on the existing Tokio blocking pool. The candidate plan is built
/// once; the default worker count runs it as the single sequential pass of [`tick_all`], a
/// configured count splits it into at most that many contiguous chunk jobs (never one task per
/// NPC). Every job is awaited before the diagnostics are aggregated once and the maintenance
/// sweep runs once per batch. Concurrent calls serialize on the manager's batch lock; a
/// panicking job is logged and cannot hold up or overlap the next batch. The batch runs in an
/// owned supervisor (see `tick_batch_with_workers`), so cancelling the caller cannot release
/// the lock while its jobs are still running.
pub async fn tick_batch(server: &Arc<GameServer>, delta_ms: i64) -> AiTickDiagnostics {
    let started = Instant::now();
    let diag = tick_batch_with_workers(server, delta_ms, batch_worker_count(server)).await;
    let total_ms = started.elapsed().as_millis() as u64;
    // Opt-in per-batch profile (`RUST_LOG` at debug). `total_ms` is the whole awaited
    // batch as seen here: supervisor spawn, batch-lock serialization and blocking-pool dispatch
    // are included; `plan_ms`/`maintenance_ms` are measured inside their jobs and the parallel
    // `tick_ms` spans the whole chunk phase (see `AiTickDiagnostics`). Any remaining gap can
    // include supervisor, batch-lock and finalization overhead; it does not identify one cause.
    tracing::debug!(
        total_ms,
        plan_ms = diag.plan_ms,
        tick_ms = diag.tick_ms,
        maintenance_ms = diag.maintenance_ms,
        candidates = diag.candidates,
        ticked = diag.ticked,
        busy = diag.busy,
        removed = diag.removed,
        "AI tick batch profile"
    );
    if profile::enabled() {
        log_profile_window(server, &diag, total_ms);
    }
    diag
}

/// `p`-th percentile of `values` (0..=100), 0 for an empty slice.
fn percentile(values: &mut [u64], p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = (values.len() * p).div_ceil(100).max(1) - 1;
    values[rank.min(values.len() - 1)]
}

/// Appends one batch to the `AAEMU_AI_PROFILE` window and, every [`PROFILE_WINDOW_BATCHES`],
/// logs the window's tick-time distribution, the profile step / behavior deltas and the slowest
/// NPC ticks. The counters are process-wide, so the window is the delta against the previous
/// one (the first window is the delta against zero, i.e. since process start).
fn log_profile_window(server: &GameServer, diag: &AiTickDiagnostics, total_ms: u64) {
    let manager = &server.systems.ai.ais;
    let mut w = lock(&manager.profile_window);
    w.batches += 1;
    w.total_ms.push(total_ms);
    w.plan_ms.push(diag.plan_ms);
    w.tick_ms.push(diag.tick_ms);
    w.candidates.push(diag.candidates as u64);
    if !w.batches.is_multiple_of(PROFILE_WINDOW_BATCHES) {
        return;
    }
    let now = profile::snapshot();
    let window = match w.last.replace(now.clone()) {
        Some(prev) => profile::delta(&now, &prev),
        None => now,
    };
    let mut total = std::mem::take(&mut w.total_ms);
    let plan = std::mem::take(&mut w.plan_ms);
    let tick = std::mem::take(&mut w.tick_ms);
    let candidates = std::mem::take(&mut w.candidates);
    let candidates_avg = candidates.iter().sum::<u64>() / candidates.len().max(1) as u64;
    tracing::info!(
        batches = PROFILE_WINDOW_BATCHES,
        candidates_avg,
        total_ms_p50 = percentile(&mut total, 50),
        total_ms_p99 = percentile(&mut total, 99),
        total_ms_max = total.iter().max().copied().unwrap_or(0),
        plan_ms_avg = plan.iter().sum::<u64>() / plan.len().max(1) as u64,
        tick_ms_avg = tick.iter().sum::<u64>() / tick.len().max(1) as u64,
        "AI tick profile window"
    );
    let mut steps = String::new();
    for step in profile::Step::ALL {
        let i = step as usize;
        if window.step_calls[i] == 0 {
            continue;
        }
        if !steps.is_empty() {
            steps.push_str(", ");
        }
        steps.push_str(&format!(
            "{}={:.1}ms/{}",
            profile::step_name(step),
            window.step_ns[i] as f64 / 1e6,
            window.step_calls[i]
        ));
    }
    tracing::info!("AI tick profile steps (cumulative, nests): {steps}");
    let mut behaviors = String::new();
    for (i, kind) in super::framework::BehaviorKind::ALL.iter().enumerate() {
        if window.behavior_calls[i] == 0 {
            continue;
        }
        if !behaviors.is_empty() {
            behaviors.push_str(", ");
        }
        behaviors.push_str(&format!(
            "{:?}={:.1}ms/{}",
            kind,
            window.behavior_ns[i] as f64 / 1e6,
            window.behavior_calls[i]
        ));
    }
    tracing::info!("AI tick profile behaviors: {behaviors}");
    let slow: Vec<String> = profile::take_slow_npcs(PROFILE_TOP_SLOW_NPCS)
        .iter()
        .map(|(us, obj_id, behavior)| {
            let behavior = behavior
                .and_then(|i| super::framework::BehaviorKind::ALL.get(i))
                .map_or_else(|| "none".to_string(), |b| format!("{b:?}"));
            format!("npc {obj_id} ({behavior}) {:.1}ms", *us as f64 / 1e3)
        })
        .collect();
    if !slow.is_empty() {
        tracing::info!("AI tick profile slowest NPCs: {}", slow.join(", "));
    }
}

/// [`tick_batch`] with an explicit worker count (clamped; tests): spawns the owning supervisor
/// and awaits its result. The supervisor gets its own `Arc<GameServer>` and runs the complete
/// serialized batch in `run_batch`; cancelling this future (dropping the caller's await) only
/// abandons the `JoinHandle` await, while the detached supervisor keeps working and releases the
/// batch lock after the last job. The next batch can therefore never overlap the in-flight work.
pub(super) async fn tick_batch_with_workers(
    server: &Arc<GameServer>,
    delta_ms: i64,
    workers: usize,
) -> AiTickDiagnostics {
    let srv = server.clone();
    let supervisor = tokio::spawn(run_batch(srv, delta_ms, workers));
    match supervisor.await {
        Ok(diag) => diag,
        Err(e) => {
            tracing::error!("AI tick batch failed: {e}");
            *lock(&server.systems.ai.ais.last_tick)
        }
    }
}

/// The in-flight batch owned by the [`tick_batch_with_workers`] supervisor: acquires the batch
/// lock and holds it over the whole serialized pass (plan, chunk jobs, aggregation, sweep). Only
/// that supervisor calls it, so a cancelled caller can neither drop a half-finished batch nor
/// release the lock early.
async fn run_batch(server: Arc<GameServer>, delta_ms: i64, workers: usize) -> AiTickDiagnostics {
    let manager = &server.systems.ai.ais;
    let _batch = manager.batch_lock.lock().await;
    let workers = workers.clamp(1, MAX_AI_WORKERS);

    if workers == 1 {
        // Default: exactly the sequential pass, on one blocking thread.
        let srv = server.clone();
        return match tokio::task::spawn_blocking(move || tick_all(&srv, delta_ms)).await {
            Ok(diag) => diag,
            Err(e) => {
                tracing::error!("AI tick task failed: {e}");
                *lock(&manager.last_tick)
            }
        };
    }

    // Build the deduplicated plan once, on the blocking pool as well (it walks the player
    // regions): a failure leaves the candidate sets untouched for the next batch. `plan_ms` is
    // measured inside the job (candidate construction only; queue time excluded).
    let srv = server.clone();
    let plan = tokio::task::spawn_blocking(move || {
        let seq = srv.systems.ai.ais.tick_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let started = Instant::now();
        let (candidates, local) = tick_candidates(&srv, seq);
        let plan_ms = started.elapsed().as_millis() as u64;
        (seq, candidates, local, plan_ms)
    })
    .await;
    let (seq, candidates, local, plan_ms) = match plan {
        Ok(plan) => plan,
        Err(e) => {
            tracing::error!("AI tick planning failed: {e}");
            return *lock(&manager.last_tick);
        }
    };

    let local = Arc::new(local);
    let mut jobs = Vec::new();
    // One wall-clock span over the whole parallel processing phase: it starts just before the
    // chunk split/dispatch and ends when every chunk await has completed. It therefore includes
    // chunk dispatch, blocking-pool queueing and await overhead in addition to the workers'
    // execution; `plan_ms` and `maintenance_ms` are measured inside their jobs. The difference
    // between the batch total and these stages is not attributed to any single cause.
    let tick_started = Instant::now();
    for chunk in plan_chunks(&candidates, workers) {
        let srv = server.clone();
        let local = Arc::clone(&local);
        jobs.push(tokio::task::spawn_blocking(move || {
            process_chunk(&srv, &chunk, &local, delta_ms)
        }));
    }
    let mut diag = AiTickDiagnostics {
        candidates: candidates.len(),
        plan_ms,
        ..AiTickDiagnostics::default()
    };
    for job in jobs {
        match job.await {
            Ok(part) => {
                diag.ticked += part.ticked;
                diag.busy += part.busy;
                diag.removed += part.removed;
            }
            Err(e) => tracing::error!("AI tick chunk failed: {e}"),
        }
    }
    diag.tick_ms = tick_started.elapsed().as_millis() as u64;

    // The registry sweep and the diagnostics snapshot stay on the blocking pool, once.
    let base = diag;
    let srv = server.clone();
    match tokio::task::spawn_blocking(move || {
        let mut diag = base;
        finish_pass(&srv, seq, &mut diag);
        diag
    })
    .await
    {
        Ok(diag) => diag,
        Err(e) => {
            tracing::error!("AI tick finalize failed: {e}");
            *lock(&manager.last_tick)
        }
    }
}

/// Low-frequency sweep over every registered AI: drop the ones whose owner / NPC object is gone
/// (normal deletion detaches through `npc_deleted`; this covers direct `world.delete` and
/// dropped owners) and re-wake the ones with persistent state or pending calls. Runs with the
/// same try-lock discipline as the tick, so a busy AI is left to its holder; a stale snapshot
/// only removes the registry entry while it still is the current handle, and the claim cleanup
/// verifies every claim against the current registry, never against the snapshot.
fn maintenance_sweep(server: &GameServer, diag: &mut AiTickDiagnostics) {
    maintenance_sweep_with(server, diag, || {});
}

/// [`maintenance_sweep`] with a callback between the registry walk and the claim cleanup: the
/// production entry passes a no-op, the regression test registers an AI exactly inside the
/// window. The callback runs while no registry lock is held.
pub(super) fn maintenance_sweep_with(
    server: &GameServer,
    diag: &mut AiTickDiagnostics,
    after_walk: impl FnOnce(),
) {
    let manager = &server.systems.ai.ais;
    // Snapshot the registry first: never hold a registry lock while locking an AI.
    let handles: Vec<Arc<AiHandle>> = manager.ais.iter().map(|e| e.value().clone()).collect();
    let mut stale = Vec::new();
    let mut wake = Vec::new();
    for h in handles {
        if h.removed.load(Ordering::Acquire) {
            continue;
        }
        let Some(mut st) = try_lock(&h.state) else {
            continue;
        };
        match st.owner() {
            None => stale.push(h.clone()),
            Some(npc) => {
                let region = npc.region();
                if region.is_some() {
                    st.was_in_world = true;
                } else if st.was_in_world && server.world.object(npc.obj_id).is_none() {
                    stale.push(h.clone());
                    continue;
                }
                if st.has_persistent_ai() || !lock(&h.pending).is_empty() {
                    wake.push(h.clone());
                }
            }
        }
    }
    for h in stale {
        if manager.remove_if_current(&h) {
            diag.removed += 1;
        }
    }
    for h in wake {
        manager.set_woken(&h, true);
    }
    after_walk();
    // Safety net for claims of handles that are no longer current (replaced / removed), and
    // age-out of the initialization grace so the set stays proportional to recent adds. The
    // verification consults the current registry per claim, so a handle registered after the
    // snapshot above keeps its claims.
    manager.prune_claims();
}
