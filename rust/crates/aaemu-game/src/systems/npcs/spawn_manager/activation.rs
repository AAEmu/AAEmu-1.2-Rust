use super::*;

/// Structured stats of one activation pass (tests / diagnostics; no per-NPC log lines).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivationStats {
    /// Candidate spawners examined this pass.
    pub candidates: usize,
    /// NPCs actually created this pass across all instances (never above
    /// `World.NpcSpawnsPerTick`).
    pub created: usize,
    /// Candidate spawners not examined this pass; the next rotated pass continues with them.
    pub queued: usize,
    /// Living NPCs of expired warm placements actually scheduled for reclamation this pass
    /// (never above `World.NpcSpawnsPerTick`; a large placement drains over several passes).
    pub reclaimed: usize,
}

/// A player's captured activation input: the owning region and the world position (Z included
/// for the far-authored 3D sphere).
type PlayerSnapshot = ((i32, i32), (f32, f32, f32));

/// A far-authored activation candidate: a placement whose authored `TestRadiusPc` /
/// `TestRadiusNpc` reaches beyond the configured prewarm ring. `radius` is the authored 3D
/// trigger in world units (`r * 50`, the rule of `NpcSpawner::is_player_in_spawn_radius`).
#[derive(Debug, Clone)]
struct AuthoredCandidate {
    spawner: Arc<NpcSpawner>,
    radius: f32,
}

/// One tracked warm placement of the activation registry.
#[derive(Debug, Clone)]
struct WarmEntry {
    spawner: Arc<NpcSpawner>,
    /// `true` only for a homogeneously resident placement: every authored member is an
    /// ordinary quest giver / reporter or service role **and** no live member is runtime-owned.
    /// Such a placement is kept after the first preparation instead of being distance-reclaimed;
    /// a mixed placement still expires its nonresident members per live NPC. Gated by
    /// `World.NpcResidentServiceNpcs` at booking and at reclamation time.
    resident: bool,
    /// Stable retention deadline, booked once when the placement dropped out of the warm set
    /// (`None` while it is warm or resident). Independent of later queue rebuilds / unchanged
    /// player revisions.
    expires_at: Option<Instant>,
}

/// One instance's activation index: every ordinary spawner filed under its authored 64 m region
/// (`WorldManager.REGION_SIZE`). Rebuilt only when the ordinary spawner membership changes
/// (load / add / delete-all), never on the fast tick itself.
#[derive(Debug, Default)]
pub(super) struct ActivationState {
    /// `NpcSpawners` membership generation the region index was built from.
    index_built: bool,
    index_generation: u64,
    index: HashMap<(i32, i32), Vec<Arc<NpcSpawner>>>,
    /// Far-authored candidates (authored radius beyond the client-visible neighbourhood),
    /// grouped by their required radius in regions (`ceil(radius / REGION_SIZE)`); each group
    /// buckets centers into `g * REGION_SIZE` cells, so a player probes only the nine cells
    /// around it before the exact 3D sphere test. Groups the configured ring already covers are
    /// skipped at lookup, never at build (the margin is runtime-configurable).
    authored_index: HashMap<u32, HashMap<(i32, i32), Vec<AuthoredCandidate>>>,
    /// Whether the candidate queues below were built at least once.
    built: bool,
    /// Activity revision / generation / margin the queues were built for.
    revision: u64,
    queue_generation: u64,
    margin: u32,
    /// Spawners whose client-visible neighbourhood contains a player, nearest first.
    visible: Vec<Arc<NpcSpawner>>,
    /// Spawners in the prewarm ring (but not a visible neighbourhood), nearest first.
    prewarm: Vec<Arc<NpcSpawner>>,
    /// Ids of every queued spawner (the currently warm set).
    warm_ids: HashSet<u32>,
    /// Rotation / continuation cursors into the two queues.
    visible_cursor: usize,
    prewarm_cursor: usize,
    /// Placements prepared at least once while warm, with their residency and stable retention
    /// booking. Only this registry is swept for maintenance; the full spawner table is never
    /// scanned per tick.
    warmed: HashMap<u32, WarmEntry>,
}

/// Quest-map streaming index: ordinary placements that may be prepared only because a player
/// selected a wider map range. Built lazily from the same spawner membership generation as the
/// activation index; keyed by authored 64 m region.
#[derive(Debug, Default)]
pub(super) struct QuestGiverIndex {
    built: bool,
    generation: u64,
    regions: HashMap<(i32, i32), Vec<Arc<NpcSpawner>>>,
}
impl SpawnManager {
    // ----- quest-map streaming index -----

    /// Rebuilds the quest-map candidate index when the ordinary spawner membership changed.
    fn ensure_quest_giver_index(&self, server: &GameServer) {
        let generation = self.activation_generation.load(Ordering::SeqCst);
        {
            let index = self
                .quest_giver_index
                .lock()
                .expect("quest giver index poisoned");
            if index.built && index.generation == generation {
                return;
            }
        }
        let residents = server.systems.npcs.spawns.resident_npcs(server);
        let mut regions: HashMap<(i32, i32), Vec<Arc<NpcSpawner>>> = HashMap::new();
        {
            let inner = self.inner();
            for spawners in inner.npc_spawners.values() {
                for spawner in spawners {
                    if !spawner_is_quest_map_candidate(server, residents, spawner) {
                        continue;
                    }
                    let p = spawner.position();
                    if let Some(region) = self.world.region_by_pos(p.x, p.y) {
                        regions.entry(region).or_default().push(spawner.clone());
                    }
                }
            }
        }
        let mut index = self
            .quest_giver_index
            .lock()
            .expect("quest giver index poisoned");
        index.regions = regions;
        index.generation = generation;
        index.built = true;
    }

    /// Tracks a placement that the quest-map range option prepared far from every player: the
    /// activation pass registers only the placements it examined itself, so without this entry
    /// the retention sweep never reclaims the NPCs the option created (they would leak until
    /// restart while `World.NpcResidentServiceNpcs` is off). A placement that is already tracked
    /// keeps its entry; while a player streams one of its NPCs the sweep keeps it warm
    /// (`activation_sweep`).
    pub fn note_prepared(&self, server: &GameServer, spawner: &Arc<NpcSpawner>) {
        if self
            .activation
            .lock()
            .expect("spawn manager poisoned")
            .warmed
            .contains_key(&spawner.id)
        {
            return;
        }
        // Classified like the activation pass does, before taking the activation guard again.
        let residents = server.systems.npcs.spawns.resident_npcs(server);
        let resident = server.config.world.npc_resident_service_npcs
            && spawner_is_resident(server, residents, spawner);
        self.activation
            .lock()
            .expect("spawn manager poisoned")
            .warmed
            .entry(spawner.id)
            .or_insert(WarmEntry {
                spawner: spawner.clone(),
                resident,
                expires_at: None,
            });
    }

    /// Ordinary placements that can provide a quest marker within `radius` metres of
    /// `(center_x, center_y)`, nearest first. `radius` may be infinite (the Unlimited option).
    ///
    /// Placements filed (by their authored position) in `skip_regions` are left out and do not
    /// count against `limit`: the caller passes the player's own neighbourhood, which the region
    /// path already streams, so it must neither be prepared for a marker nor use up the limit.
    ///
    /// Only placements whose members are all safe quest givers / reporters are returned, so a
    /// monster camp with one quest NPC is not spawned for a map marker. `limit` bounds the work
    /// (and therefore what the caller can stream); 0 means no bound.
    pub fn quest_giver_spawners_near(
        &self,
        server: &GameServer,
        center_x: f32,
        center_y: f32,
        radius: f32,
        skip_regions: &HashSet<(i32, i32)>,
        limit: usize,
    ) -> Vec<Arc<NpcSpawner>> {
        self.ensure_quest_giver_index(server);
        let index = self
            .quest_giver_index
            .lock()
            .expect("quest giver index poisoned");
        let radius2 = radius * radius;
        let center_region = self.world.region_by_pos(center_x, center_y);
        let region_radius = if radius.is_finite() && radius > 0.0 {
            (radius / REGION_SIZE as f32).ceil() as i32 + 1
        } else {
            0
        };
        let mut found: Vec<(f32, u32, Arc<NpcSpawner>)> = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let mut visit = |region: (i32, i32)| {
            if skip_regions.contains(&region) {
                return;
            }
            let Some(spawners) = index.regions.get(&region) else {
                return;
            };
            for spawner in spawners {
                if !seen.insert(spawner.id) {
                    continue;
                }
                let p = spawner.position();
                let dx = p.x - center_x;
                let dy = p.y - center_y;
                let d2 = dx * dx + dy * dy;
                if d2 <= radius2 {
                    found.push((d2, spawner.id, spawner.clone()));
                }
            }
        };
        match center_region {
            Some((rx, ry)) if region_radius > 0 => {
                for dx in -region_radius..=region_radius {
                    for dy in -region_radius..=region_radius {
                        visit((rx + dx, ry + dy));
                    }
                }
            }
            Some(_) | None => {
                // Unlimited: the index only holds quest giver placements, so a full scan is
                // still bounded by the quest content (about two thousand placements in 1.2).
                for &region in index.regions.keys() {
                    visit(region);
                }
            }
        }
        found.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        if limit > 0 {
            found.truncate(limit);
        }
        found.into_iter().map(|(_, _, s)| s).collect()
    }

    // ----- activation -----

    /// The ordinary `NpcSpawners` membership changed: the region index and the candidate queues
    /// are stale. Only the atomic generation is touched here, so callers may hold the inner lock.
    pub(super) fn invalidate_activation_index(&self) {
        self.activation_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Drops the activation index, queues and warmed registry (the spawner lists were replaced
    /// wholesale: load / reload / delete-all). The caller must not hold the inner lock.
    pub(super) fn reset_activation(&self) {
        let mut state = self.activation.lock().expect("spawn manager poisoned");
        state.index_built = false;
        state.index.clear();
        state.authored_index.clear();
        state.built = false;
        state.visible.clear();
        state.prewarm.clear();
        state.warm_ids.clear();
        state.visible_cursor = 0;
        state.prewarm_cursor = 0;
        state.warmed.clear();
    }

    /// Rebuilds the per-region index and the candidate queues when the world's player activity
    /// revision, the spawner membership generation or the prewarm margin changed. An unchanged
    /// revision/generation/margin keeps the cached queues (unfilled targets continue there).
    /// Candidates are the configured visible / prewarm rings plus placements whose authored 3D
    /// trigger sphere contains a player (far-authored, prewarm tier).
    fn refresh_activation(&self, margin: u32) {
        // Capture the revision *before* the player snapshot: a movement / arrival during the
        // collection bumps the revision afterwards, so the captured value stays behind and the
        // next pass rebuilds instead of stamping the stale snapshot with the new revision (which
        // would keep a false queue until the next activity change).
        let revision = self.world.player_activity_revision();
        // The client packet visibility radius is `WorldManager.REGION_NEIGHBORHOOD_SIZE` (2
        // regions) and stays unchanged; the prewarm ring adds the configured margin beyond it.
        const CLIENT_NEIGHBOURHOOD: i32 = 2;
        // Player regions / positions (with Z for the far-authored 3D sphere) are read before the
        // activation lock (object data locks are leaves, the activation lock is not held here
        // yet).
        let players: Vec<PlayerSnapshot> = self
            .world
            .all_characters()
            .into_iter()
            .filter_map(|p| {
                let (inst, rx, ry) = p.region()?;
                if inst != self.world.id {
                    return None;
                }
                let pr = self.world.world_pos_rot(&WorldObject::from(&p));
                Some(((rx, ry), (pr.x, pr.y, pr.z)))
            })
            .collect();
        let generation = self.activation_generation.load(Ordering::SeqCst);
        let mut state = self.activation.lock().expect("spawn manager poisoned");
        if !state.index_built || state.index_generation != generation {
            let mut index: HashMap<(i32, i32), Vec<Arc<NpcSpawner>>> = HashMap::new();
            let mut authored: HashMap<u32, HashMap<(i32, i32), Vec<AuthoredCandidate>>> =
                HashMap::new();
            for spawner in self.inner().npc_spawners.values().flatten() {
                let p = spawner.position();
                if let Some(region) = self.world.region_by_pos(p.x, p.y) {
                    index.entry(region).or_default().push(spawner.clone());
                }
                // An authored trigger never reachable through the visible ring is bucketed by
                // its required radius (the ring already covers everything up to radius 2).
                let radius = spawner.authored_spawn_radius();
                if radius.is_finite() && radius > 0.0 {
                    let g = (radius / REGION_SIZE as f32).ceil() as u32;
                    if g > CLIENT_NEIGHBOURHOOD as u32 {
                        let cell = g as f32 * REGION_SIZE as f32;
                        let key = ((p.x / cell).floor() as i32, (p.y / cell).floor() as i32);
                        authored.entry(g).or_default().entry(key).or_default().push(
                            AuthoredCandidate {
                                spawner: spawner.clone(),
                                radius,
                            },
                        );
                    }
                }
            }
            state.index = index;
            state.authored_index = authored;
            state.index_generation = generation;
            state.index_built = true;
            state.built = false;
        }
        if state.built
            && state.revision == revision
            && state.queue_generation == generation
            && state.margin == margin
        {
            return;
        }
        state.revision = revision;
        state.queue_generation = generation;
        state.margin = margin;
        state.built = true;
        state.visible.clear();
        state.prewarm.clear();
        state.warm_ids.clear();
        state.visible_cursor = 0;
        state.prewarm_cursor = 0;
        if players.is_empty() {
            // Keep `warmed`: the sweep still books the retention countdown of prepared
            // placements (and keeps residents) after the last player left.
            return;
        }
        let warm_radius = CLIENT_NEIGHBOURHOOD + margin as i32;
        // Deduplicate across players, ring overlaps and far-authored candidates: visible wins,
        // nearest horizontal distance wins.
        let mut found: HashMap<u32, (bool, f32, Arc<NpcSpawner>)> = HashMap::new();
        for &((rx, ry), (px, py, _pz)) in &players {
            for dx in -warm_radius..=warm_radius {
                for dy in -warm_radius..=warm_radius {
                    let region = (rx + dx, ry + dy);
                    if !self.world.template.valid_region(region.0, region.1) {
                        continue;
                    }
                    let Some(spawners) = state.index.get(&region) else {
                        continue;
                    };
                    let visible = dx.abs().max(dy.abs()) <= CLIENT_NEIGHBOURHOOD;
                    for spawner in spawners {
                        // A template-less spawner cannot ever spawn. An empty member list is not
                        // filtered here: `initialize_spawnable_npcs` may fill it later while this
                        // cached queue lives on, so the fill stays retryable.
                        if spawner.template().is_none() {
                            continue;
                        }
                        let p = spawner.position();
                        let d = (p.x - px) * (p.x - px) + (p.y - py) * (p.y - py);
                        found
                            .entry(spawner.id)
                            .and_modify(|e| {
                                e.0 |= visible;
                                if d < e.1 {
                                    e.1 = d;
                                }
                            })
                            .or_insert((visible, d, spawner.clone()));
                    }
                }
            }
        }
        // Far-authored eligibility beyond the configured ring: a player inside a placement's
        // authored 3D sphere queues it even when its region sits outside the visible / prewarm
        // rings. Each radius group probes only the nine buckets around the player (the cell size
        // equals the group's required radius), then the exact 3D sphere decides; a group the
        // configured ring already covers is skipped, as the queues above hold those placements.
        for &((_rx, _ry), (px, py, pz)) in &players {
            for (&g, cells) in &state.authored_index {
                if g <= warm_radius as u32 {
                    continue;
                }
                let cell = g as f32 * REGION_SIZE as f32;
                let (cx, cy) = ((px / cell).floor() as i32, (py / cell).floor() as i32);
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        let Some(bucket) = cells.get(&(cx + dx, cy + dy)) else {
                            continue;
                        };
                        for candidate in bucket {
                            // Same template gate as the ring queues; an empty member list stays
                            // queued and is retried once it is initialized later.
                            if candidate.spawner.template().is_none() {
                                continue;
                            }
                            let p = candidate.spawner.position();
                            let (ox, oy, oz) = (p.x - px, p.y - py, p.z - pz);
                            if ox * ox + oy * oy + oz * oz > candidate.radius * candidate.radius {
                                continue;
                            }
                            let d = ox * ox + oy * oy;
                            found
                                .entry(candidate.spawner.id)
                                .and_modify(|e| {
                                    if d < e.1 {
                                        e.1 = d;
                                    }
                                })
                                .or_insert((false, d, candidate.spawner.clone()));
                        }
                    }
                }
            }
        }
        let mut visible: Vec<(f32, Arc<NpcSpawner>)> = Vec::new();
        let mut prewarm: Vec<(f32, Arc<NpcSpawner>)> = Vec::new();
        for (is_visible, d, spawner) in found.into_values() {
            if is_visible {
                visible.push((d, spawner));
            } else {
                prewarm.push((d, spawner));
            }
        }
        visible.sort_by(|a, b| a.0.total_cmp(&b.0));
        prewarm.sort_by(|a, b| a.0.total_cmp(&b.0));
        state.visible = visible.into_iter().map(|(_, s)| s).collect();
        state.prewarm = prewarm.into_iter().map(|(_, s)| s).collect();
        let mut warm_ids = HashSet::new();
        for spawner in state.visible.iter().chain(state.prewarm.iter()) {
            warm_ids.insert(spawner.id);
        }
        state.warm_ids = warm_ids;
    }

    /// Processes one of this instance's candidate queues from its rotation cursor within the
    /// remaining global creation budget. Unvisited candidates are left `queued` for the next
    /// pass; a completed sweep wraps the cursor so the next pass starts at the queue head again.
    pub(super) fn activation_phase(
        &self,
        server: &GameServer,
        margin: u32,
        visible_phase: bool,
        budget: &mut i64,
        stats: &mut ActivationStats,
    ) {
        self.refresh_activation(margin);
        let (queue, mut cursor) = {
            let state = self.activation.lock().expect("spawn manager poisoned");
            if visible_phase {
                (state.visible.clone(), state.visible_cursor)
            } else {
                (state.prewarm.clone(), state.prewarm_cursor)
            }
        };
        let len = queue.len();
        if len == 0 {
            return;
        }
        let mut examined: Vec<Arc<NpcSpawner>> = Vec::new();
        let mut swept = false;
        if *budget > 0 {
            let mut i = cursor;
            while i < len && *budget > 0 {
                let spawner = &queue[i];
                let created = spawner.update_warm(server, true, *budget);
                i += 1;
                if created > 0 {
                    *budget -= created as i64;
                    stats.created += created;
                }
                examined.push(spawner.clone());
                stats.candidates += 1;
            }
            cursor = i;
            if cursor >= len {
                cursor = 0;
                swept = true;
            }
        }
        // The resident classification (static game data) and the live-population reads
        // (object-data leaves) happen before taking the activation guard; only the registry is
        // mutated under it. Every examined placement becomes tracked - also the currently empty
        // ones (e.g. corpse / respawn timers pending) so a later refill stays reclaimable.
        let residents = server.systems.npcs.spawns.resident_npcs(server);
        let resident_gate = server.config.world.npc_resident_service_npcs;
        let prepared: Vec<(Arc<NpcSpawner>, bool)> = examined
            .into_iter()
            .map(|spawner| {
                let resident = resident_gate && spawner_is_resident(server, residents, &spawner);
                (spawner, resident)
            })
            .collect();
        let mut state = self.activation.lock().expect("spawn manager poisoned");
        for (spawner, resident) in prepared {
            state.warmed.insert(
                spawner.id,
                WarmEntry {
                    spawner,
                    resident,
                    expires_at: None,
                },
            );
        }
        if visible_phase {
            state.visible_cursor = cursor;
        } else {
            state.prewarm_cursor = cursor;
        }
        if !swept {
            stats.queued += len - cursor;
        }
    }

    /// Maintenance pass over the warmed registry: clears the retention booking of placements
    /// inside the warm set, starts the stable countdown of the ones that dropped out, reclaims
    /// expired ordinary placements under the shared NPC budget and drops tracking once nothing
    /// (living NPCs or queued corpse / respawn deadlines) is left. Authored schedule /
    /// management despawns keep priority over residency and retention. Only the warmed registry
    /// is walked, never the full spawner table.
    pub(super) fn activation_sweep(
        &self,
        server: &GameServer,
        budget: &mut i64,
        stats: &mut ActivationStats,
    ) {
        let now = now();
        let retention = Duration::from_secs(server.config.world.npc_warm_retention_seconds);
        // Snapshot the two halves before taking the activation guard; the object-data /
        // population reads below happen without it.
        let mut cold: Vec<(Option<Instant>, u32, Arc<NpcSpawner>, bool)> = Vec::new();
        let mut warm: Vec<u32> = Vec::new();
        {
            let state = self.activation.lock().expect("spawn manager poisoned");
            for (id, entry) in state.warmed.iter() {
                if state.warm_ids.contains(id) {
                    warm.push(*id);
                } else {
                    cold.push((entry.expires_at, *id, entry.spawner.clone(), entry.resident));
                }
            }
        }
        if !warm.is_empty() {
            let mut state = self.activation.lock().expect("spawn manager poisoned");
            for id in warm {
                if let Some(entry) = state.warmed.get_mut(&id) {
                    entry.expires_at = None;
                }
            }
        }
        if cold.is_empty() {
            return;
        }
        // Oldest booking first: a placement that dropped out never loses its stable deadline,
        // and budget-starved passes continue with the same oldest entrants next time.
        cold.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        // A placement whose NPCs are streamed by a far quest-map viewer is still warm even when
        // it sits outside every player's warm ring; reclaiming it would only make the map
        // refresh rebuild it (cold-reclaim churn).
        let streamed = server.systems.quests.quest_map.streamed_npc_ids();
        let resident_gate = server.config.world.npc_resident_service_npcs;
        let residents = server.systems.npcs.spawns.resident_npcs(server);
        let mut bookings: Vec<(u32, Instant)> = Vec::new();
        let mut keep_warm: Vec<u32> = Vec::new();
        let mut drop_entries: Vec<u32> = Vec::new();
        for (expires_at, id, spawner, resident) in cold {
            if !streamed.is_empty()
                && spawner
                    .spawned_npcs()
                    .values()
                    .flatten()
                    .any(|npc| streamed.contains(&npc.obj_id))
            {
                // Clear the retention booking: if streaming stops the placement starts its
                // countdown from that pass instead of expiring immediately.
                keep_warm.push(id);
                continue;
            }
            // The homogeneous residency fast path is re-validated against the live members: a
            // placement whose classification is cached from an earlier pass must not stay
            // skipped once a runtime-owned (never resident) member appears.
            let resident_skip = resident
                && resident_gate
                && !spawner_has_nonresident_live_npc(server, residents, &spawner);
            if spawner.can_despawn_npcs(server) {
                // Authored schedule / management removal keeps its own lifecycle: it wins over
                // residency and retention.
                spawner.despawn_npcs(server);
            } else if !resident_skip {
                let deadline = match expires_at {
                    Some(t) => t,
                    None => {
                        let t = now + retention;
                        bookings.push((id, t));
                        t
                    }
                };
                if now >= deadline && *budget > 0 {
                    let removed =
                        spawner.reclaim_cold_npcs(server, *budget, residents, resident_gate);
                    *budget -= removed as i64;
                    stats.reclaimed += removed;
                }
            }
            // Stop tracking a placement with nothing left: no living NPCs and no queued
            // corpse / respawn deadlines (kept ones drain through the 1 s respawn tick, so the
            // registry cannot grow forever).
            if spawner.spawned_npcs().is_empty()
                && !self.placement_has_pending_timers(spawner.shared())
            {
                drop_entries.push(id);
            }
        }
        let mut state = self.activation.lock().expect("spawn manager poisoned");
        for (id, deadline) in bookings {
            if let Some(entry) = state.warmed.get_mut(&id) {
                entry.expires_at = Some(deadline);
            }
        }
        for id in keep_warm {
            if let Some(entry) = state.warmed.get_mut(&id) {
                entry.expires_at = None;
            }
        }
        for id in drop_entries {
            state.warmed.remove(&id);
        }
    }
}

/// Whether an ordinary placement is homogeneously resident: every authored member (or the
/// `UnitId` fallback without a member list) is a quest giver / reporter or service role of the
/// immutable [`ResidentNpcTemplates`] set. Only such a placement may skip distance reclamation
/// as a whole, and only while no live member diverges from that classification (runtime
/// ownership, see [`spawner_has_nonresident_live_npc`]); a mixed placement is reclaimed per live
/// NPC ([`npc_is_resident`]), so its nonresident members expire while its residents stay.
/// Hostile quest kill / accept-kill targets and ordinary neutral NPCs are not residents.
fn spawner_is_resident(
    server: &GameServer,
    residents: &ResidentNpcTemplates,
    spawner: &NpcSpawner,
) -> bool {
    let npcs = &server.game_data.systems.npcs.npc;
    let is_resident = |id: u32| npcs.template(id).is_some_and(|t| residents.is_resident(t));
    let members = spawner.spawnable_npcs();
    if members.is_empty() {
        is_resident(spawner.unit_id)
    } else {
        members.iter().all(|member| is_resident(member.member_id))
    }
}

/// Whether an ordinary placement may be prepared for the quest-map range option: every
/// authored member is a plain `Npc` row naming a safe quest giver / reporter (nonaggressive, not
/// creator-only) and at least one member carries a quest role. Mixed camps with hostile members
/// are excluded, and so are `NpcGroup` rows: their `member_id` is a group id, not an NPC
/// template id (the spawner spawns the whole group), even when the number is also an NPC id.
fn spawner_is_quest_map_candidate(
    server: &GameServer,
    residents: &ResidentNpcTemplates,
    spawner: &NpcSpawner,
) -> bool {
    let npcs = &server.game_data.systems.npcs.npc;
    let members = spawner.spawnable_npcs();
    let member_ids: Vec<u32> = if members.is_empty() {
        vec![spawner.unit_id]
    } else if members.iter().any(|m| m.member_type != "Npc") {
        return false;
    } else {
        members.iter().map(|m| m.member_id).collect()
    };
    if member_ids.is_empty() {
        return false;
    }
    let mut has_quest_npc = false;
    for id in member_ids {
        let Some(template) = npcs.template(id) else {
            return false;
        };
        if !residents.is_quest_map_candidate(template) {
            return false;
        }
        has_quest_npc = true;
    }
    has_quest_npc
}

/// Whether any live NPC of an otherwise resident placement is not resident per the
/// authoritative live decision ([`npc_is_resident`], which excludes runtime-owned NPCs). A
/// cached homogeneous classification must not skip reclamation while such a member exists - it
/// follows its owner's lifecycle like any nonresident.
fn spawner_has_nonresident_live_npc(
    server: &GameServer,
    residents: &ResidentNpcTemplates,
    spawner: &NpcSpawner,
) -> bool {
    spawner
        .spawned_npcs()
        .values()
        .flatten()
        .any(|npc| !npc_is_resident(server, residents, npc))
}
