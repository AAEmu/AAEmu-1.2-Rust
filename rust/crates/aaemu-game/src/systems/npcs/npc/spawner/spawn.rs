use super::*;

impl NpcSpawner {
    // ----- spawning -----

    /// `DoSpawn()`: fills [`Self::target_population`] from the supported member templates.
    ///
    /// The C# iterates `SpawnableNpcs` exactly once and only caps the result afterwards, so a
    /// single-member template cannot replenish an authored `Count` / `min_population` > 1 while
    /// a member list larger than the cap overshoots it. Here the target is computed from the
    /// live shared population and the still-reserved respawn deadlines, full passes spawn every
    /// member, and a capped pass selects weighted members (later members stay eligible).
    pub fn do_spawn(self: &Arc<Self>, server: &GameServer) {
        let _ = self.do_spawn_bounded(server, i64::MAX);
    }

    /// Bounded variant of [`Self::do_spawn`]: creates at most `budget` NPCs and returns the
    /// number actually created. The activation pass budgets real creations across instances
    /// (`SpawnGroup` delegates to `SpawnNpc` and yields one NPC per member), and a partial fill
    /// simply leaves the rest of the target for a later pass. Everything else about the fill is
    /// unchanged: authored population caps / weights, management and schedule gates, the shared
    /// living population and the still-reserved respawn deadlines all still apply, and a failed
    /// pass leaves the spawner unscheduled so it is retried.
    pub fn do_spawn_bounded(self: &Arc<Self>, server: &GameServer, budget: i64) -> usize {
        let _items = server.systems.economy.operation_scope();
        if budget <= 0 || self.shared.saved_death() {
            return 0;
        }
        let Some(t) = self.template() else {
            tracing::error!(
                "[Spawn] Can't spawn npc {} from spawnerId {} - Template is null",
                self.unit_id,
                self.id
            );
            return 0;
        };
        tracing::debug!(
            "[Spawn] Starting spawn process for SpawnerId={}, UnitId={}, Template={}",
            self.spawner_id,
            self.unit_id,
            t.id
        );
        // Hold the reentrant spawn lock for the whole calculation and creation so direct calls
        // (`SpawnAll`, `Spawn`, `ForceSpawn`) serialize like `Update`.
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let target = self.target_population(server);
        // Direct read: the cached other-population count may still be a stale zero after
        // another clone spawned, despawned or died.
        let live = self.live_population_direct();
        // Respawn slots with their own future deadline are reserved for their own refill;
        // a lost member of an `enable_respawn` batch is reserved for the group timer instead.
        let reserved = self.future_pending_respawn_slots(server)
            + server.systems.npcs.group_batches.reserved_slots(self);
        let mut remaining = (target - live - reserved).min(budget);
        if remaining <= 0 {
            return 0;
        }
        let spawnable = self.spawnable_npcs();
        if spawnable.is_empty() {
            tracing::warn!(
                "[Spawn] No spawnable NPCs available for SpawnerId={}",
                self.spawner_id
            );
            return 0;
        }
        let mut spawned_npcs = Vec::new();
        while remaining > 0 {
            let mut pass_spawned = 0usize;
            if remaining >= spawnable.len() as i64 {
                // Full pass: every member once (the C# `DoSpawn` order); further passes reuse
                // them when the target is larger than the member list.
                for nsn in &spawnable {
                    if remaining <= 0 {
                        break;
                    }
                    pass_spawned +=
                        self.spawn_member_capped(server, nsn, &mut remaining, &mut spawned_npcs);
                }
            } else {
                // Capped pass: weighted sampling without replacement so the cap does not always
                // take the list prefix and later members stay eligible.
                let mut pool: Vec<&NpcSpawnerNpc> = spawnable.iter().collect();
                while remaining > 0 && !pool.is_empty() {
                    let Some(pick) = select_weighted_member(&pool) else {
                        break;
                    };
                    let idx = pool
                        .iter()
                        .position(|n| std::ptr::eq(*n, pick))
                        .expect("weighted pick comes from the pool");
                    let nsn = pool.remove(idx);
                    pass_spawned +=
                        self.spawn_member_capped(server, nsn, &mut remaining, &mut spawned_npcs);
                }
            }
            if pass_spawned == 0 {
                break;
            }
        }
        let created = spawned_npcs.len();
        if created == 0 {
            tracing::error!(
                "[Spawn] Failed to spawn any NPCs for {}:{}",
                self.unit_id,
                self.spawner_id
            );
            // No spawn at all: stay unscheduled so the next active tick retries.
            self.set_spawn_scheduled(false);
            return 0;
        }
        self.consume_spawn_credits(created, server);
        // The capacity check passed and the fill happened; only now is the spawner scheduled.
        self.set_spawn_scheduled(true);
        created
    }

    /// One member's batch, capped at the remaining slots: the survivors are tracked in
    /// `spawned_npcs`, a surplus batch is deleted again so the hard caps hold. Returns the
    /// number of NPCs added (0 when the member failed).
    fn spawn_member_capped(
        self: &Arc<Self>,
        server: &GameServer,
        nsn: &NpcSpawnerNpc,
        remaining: &mut i64,
        spawned_npcs: &mut Vec<NpcRef>,
    ) -> usize {
        match spawn_member(server, nsn, self, 0) {
            Ok(Some(spawned)) if !spawned.is_empty() => {
                // An `NpcGroup` is one authored encounter: every member spawns even when fewer
                // population slots remain (the C# pass spawned every member of the list too).
                // The batch consumes at most the remaining slots, so a group at a `max_population`
                // of 1 still spawns whole; an ordinary `Npc` batch keeps the hard per-unit cap.
                let keep = if nsn.member_type == "NpcGroup" {
                    spawned.len()
                } else {
                    spawned.len().min(*remaining as usize)
                };
                for npc in spawned.iter().take(keep) {
                    let key = self
                        .npc_spawner(server, npc)
                        .map_or(self.spawner_id, |s| s.spawner_id);
                    self.add_npc_to_spawned(key, npc.clone());
                    spawned_npcs.push(npc.clone());
                }
                for npc in spawned.iter().skip(keep) {
                    spawn_manager::delete_object(
                        server,
                        &WorldObject::from(npc),
                        self.parent_world(),
                    );
                    server.ids.object.release_id(npc.obj_id);
                    spawn_manager::forget_object(server, npc.obj_id);
                }
                *remaining = (*remaining - keep as i64).max(0);
                keep
            }
            Ok(_) => {
                tracing::warn!(
                    "[Spawn] Failed to spawn NPC from template {}:{} for SpawnerId={}",
                    nsn.npc_spawner_template_id,
                    nsn.member_id,
                    self.spawner_id
                );
                0
            }
            Err(e) => {
                tracing::error!(
                    "[Spawn] Failed to spawn NPC from template {}:{} for SpawnerId={}: {e}",
                    nsn.npc_spawner_template_id,
                    nsn.member_id,
                    self.spawner_id
                );
                0
            }
        }
    }

    /// `IsSpawningScheduleEnabled()`: the game schedule, else the spawn time window (no window =
    /// enabled). Only a successful [`Self::do_spawn`] sets `IsSpawnScheduled`, so a capacity
    /// check can never leave the clone marked scheduled while it spawned nothing.
    pub fn is_spawning_schedule_enabled(&self, server: &GameServer) -> bool {
        if self.template().is_none() {
            tracing::warn!(
                "[Spawn] Can't spawn NPC {}:{} (index={}) — template is null.",
                self.spawner_id,
                self.unit_id,
                self.id
            );
            return false;
        }
        match period_status_npc(server, self.spawner_id) {
            PeriodStatus::InProgress => {
                self.st().respawn_denied = true;
                return true;
            }
            PeriodStatus::NotFound => {}
            PeriodStatus::NotStarted | PeriodStatus::Ended => return false,
        }
        if self.is_within_spawn_time(server) {
            self.st().respawn_denied = true;
            return true;
        }
        false
    }

    /// `IsWithinSpawnTime()` (game clock; no window defined = true).
    fn is_within_spawn_time(&self, server: &GameServer) -> bool {
        let Some(t) = self.template() else {
            return true;
        };
        if t.start_time > 0.0 || t.end_time > 0.0 {
            return is_hour_between(server.time.get_time(), t.start_time, t.end_time);
        }
        true
    }

    /// `IsDespawningScheduleEnabled(spawnerId)`: true when an NPC of the spawner is outside its
    /// authored game-clock window or its event schedule is not active (`NotStarted` / `Ended`),
    /// so the ordinary tick removes it. An `InProgress` event keeps its NPCs, and ordinary NPCs
    /// without schedule rows (`NotFound`) are only governed by their time window. This corrects
    /// the mirrored C# check, which treated `InProgress` as despawnable and tore down live event
    /// NPCs on every tick.
    pub(super) fn is_despawning_schedule_enabled(
        &self,
        server: &GameServer,
        spawner_id: u32,
    ) -> bool {
        let Some(npcs) = self.spawned_of(spawner_id) else {
            return false;
        };
        for npc in &npcs {
            let spawner = self.npc_spawner(server, npc);
            let template = spawner.as_ref().and_then(|s| s.template());
            if is_within_despawn_time(server, template.as_deref()) {
                return true;
            }
            if let Some(t) = template {
                if matches!(
                    period_status_npc(server, t.id),
                    PeriodStatus::NotStarted | PeriodStatus::Ended
                ) {
                    return true;
                }
            }
        }
        false
    }

    /// `DoEventSpawn()`: spawns the template member matching `UnitId`.
    pub fn do_event_spawn(self: &Arc<Self>, server: &GameServer) {
        if !self.shared.management_enabled() {
            return;
        }
        let Some(t) = self.template() else {
            tracing::error!(
                "Can't spawn npc {} from spawnerId {}",
                self.unit_id,
                self.id
            );
            return;
        };
        let current = self.current_spawn_count() as i64;
        if current >= i64::from(t.max_population) {
            return;
        }
        if t.suspend_spawn_count > 0 && current > i64::from(t.suspend_spawn_count) {
            return;
        }
        let mut n = Vec::new();
        if let Some(nsn) = t.npcs.iter().find(|n| n.member_id == self.unit_id) {
            match spawn_member(server, nsn, self, 0) {
                Ok(Some(v)) => n = v,
                Ok(None) => {}
                Err(e) => tracing::error!("{e}"),
            }
        }
        for npc in &n {
            self.add_npc_to_spawned(self.spawner_id, npc.clone());
        }
        if n.is_empty() {
            tracing::error!("Can't spawn npc {} from spawnerId {}", self.unit_id, t.id);
            return;
        }
        self.decrement_count(n.len());
    }

    /// Consumes `_scheduledCount` for a normal fill, but only the credits that are not backed by
    /// one of this clone's own future respawn timers: due/free credits are consumed up to the
    /// number spawned, own future deadlines keep their reservations, and the count never drops
    /// below zero. Shared placement futures are capacity reservations, not this clone's credits,
    /// and [`Self::do_spawn`] already excluded them from the fill.
    fn consume_spawn_credits(self: &Arc<Self>, spawned: usize, server: &GameServer) {
        let own_future = self
            .own_pending_respawn_slots(server)
            .clamp(0, i64::from(i32::MAX)) as i32;
        let spawned = spawned.min(i32::MAX as usize) as i32;
        let mut st = self.st();
        let take = (st.scheduled_count - own_future).max(0).min(spawned);
        st.scheduled_count -= take;
        if st.scheduled_count < 0 {
            st.scheduled_count = 0;
        }
    }

    fn decrement_count(&self, n: usize) {
        let mut st = self.st();
        if st.scheduled_count > 0 {
            st.scheduled_count -= n as i32;
        }
    }

    pub(super) fn increment_count(&self, respawn: bool) {
        if respawn {
            let mut st = self.st();
            st.scheduled_count += 1;
            if st.scheduled_count < 0 {
                st.scheduled_count = 0;
            }
        }
    }

    /// `DoRandomSpawn(spawnerId, ownerId)`: one weighted-random member of a spawner template.
    /// Mirrors the C#: an extra NPC is created through `NpcManager.Create` and dropped (its
    /// object id leaks), and the spawned NPC is `Spawn()`ed a second time.
    pub fn do_random_spawn(
        self: &Arc<Self>,
        server: &GameServer,
        spawner_id: u32,
        owner_id: u32,
    ) -> Option<NpcRef> {
        let template = server
            .game_data
            .systems
            .npcs
            .npc_spawners
            .get_npc_spawner_template(spawner_id);
        let Some(template) = template.filter(|t| !t.npcs.is_empty()) else {
            tracing::warn!("No NPC templates available for spawner {spawner_id}.");
            return None;
        };
        let Some(nsn) = random_element_by_weight(&template.npcs, |n| n.weight) else {
            tracing::warn!(
                "Random template returned null on the NPC selection for spawner {spawner_id}."
            );
            return None;
        };
        // `NpcManager.Create` existence check (and its leaked probe NPC). A group row has no
        // NPC template of its own, so probe its member list instead; creating `MemberId` (the
        // group id) would make the wrong NPC whenever the ids collide.
        let exists = if nsn.member_type == "NpcGroup" {
            !server
                .game_data
                .systems
                .npcs
                .npc_groups
                .members(nsn.member_id)
                .is_empty()
        } else {
            backend::npc_create(server, 0, nsn.member_id).is_some()
        };
        if !exists {
            tracing::warn!(
                "Failed to create NPC from template {}:{}",
                nsn.npc_spawner_template_id,
                nsn.member_id
            );
            return None;
        }
        let spawned = match spawn_member(server, nsn, self, owner_id) {
            Ok(Some(v)) if !v.is_empty() => v,
            Ok(_) => {
                tracing::warn!(
                    "No NPCs spawned from template {}:{}",
                    nsn.npc_spawner_template_id,
                    nsn.member_id
                );
                return None;
            }
            Err(e) => {
                tracing::error!(
                    "Failed to spawn NPC from template {}:{}: {e}",
                    nsn.npc_spawner_template_id,
                    nsn.member_id
                );
                return None;
            }
        };
        let npc = spawned[0].clone();
        {
            let _lock = server.systems.npcs.spawns.spawn_lock();
            for spawned_npc in &spawned {
                let key = self
                    .npc_spawner(server, spawned_npc)
                    .map_or(self.spawner_id, |s| s.spawner_id);
                self.add_npc_to_spawned(key, spawned_npc.clone());
            }
        }
        if let Some(world) = self.parent_world().and_then(|id| server.world.world(id)) {
            spawn_into_world(server, &world, &npc);
        }
        Some(npc)
    }

    /// `DoSpawnEffect(spawnerId, effect, caster, target)` (`SpawnEffect`).
    pub fn do_spawn_effect(
        self: &Arc<Self>,
        server: &GameServer,
        spawner_id: u32,
        effect: &SpawnEffectParams,
        caster: Option<&WorldObject>,
        target: Option<&WorldObject>,
    ) {
        // Creation, event ownership and population bookkeeping must finish before an
        // event can end and delete the summoner's descendants.
        let _items = server.systems.economy.operation_scope();
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let Some(template) = server
            .game_data
            .systems
            .npcs
            .npc_spawners
            .get_npc_spawner_template(spawner_id)
        else {
            return;
        };
        let mut n = Vec::new();
        if let Some(nsn) = template.npcs.iter().find(|n| n.member_id == self.unit_id) {
            match spawn_member(server, nsn, self, 0) {
                Ok(Some(v)) => n = v,
                Ok(None) => return,
                Err(e) => tracing::error!("{e}"),
            }
        }
        if let Some(caster) = caster {
            crate::systems::instances::tower_def::on_summoned(server, caster, &n);
        }
        for npc in &n {
            if let Some(s) = self.npc_spawner(server, npc) {
                s.set_respawn_time(0);
            }
            if effect.use_summoner_faction {
                let source = match target {
                    Some(t) if t.as_npc().is_some() => Some(t),
                    _ => caster,
                };
                if let Some(faction) = source.map(|o| o.with_base_unit(|b| b.faction_id())) {
                    npc.data().unit.faction_id = faction;
                }
            }
            if effect.use_summoner_aggro_target && !effect.use_summoner_faction {
                // A self-targeted summon, such as the Cradle test subject's 18409,
                // should inherit its summoner's opponent. The plot's target is the
                // summoner or an offset from it, so attacking it loses friendly aggro.
                let summoner_target = caster.and_then(WorldObject::as_npc).and_then(|n| {
                    let d = n.data();
                    let id = d.current_aggro_target;
                    (id != 0).then_some(id)
                });
                let unit = match target {
                    Some(t)
                        if t.as_npc().is_some()
                            && caster.is_none_or(|c| t.obj_id() != c.obj_id()) =>
                    {
                        Some(t.clone())
                    }
                    _ => summoner_target
                        .and_then(|id| server.world.object(id))
                        .or_else(|| caster.cloned()),
                };
                if let (Some(ai), Some(unit)) = (server.systems.hooks.ai.get(), unit.as_ref()) {
                    ai.npc_engage(server, npc, unit);
                }
            }
            if effect.life_time > 0.0 {
                schedule_npc_do_despawn(server, npc, effect.life_time);
            }
            if effect.despawn_on_creator_death {
                if let Some(caster) = caster {
                    server
                        .systems
                        .npcs
                        .spawns
                        .add_creator_summon(caster.obj_id(), npc);
                }
            }
        }
        if n.is_empty() {
            tracing::error!(
                "Can't spawn npc {} from spawner {}",
                self.unit_id,
                template.id
            );
            return;
        }
        for npc in &n {
            self.add_npc_to_spawned(self.spawner_id, npc.clone());
        }
        let mut st = self.st();
        if st.scheduled_count > 0 {
            st.scheduled_count -= n.len() as i32;
        }
    }

    pub(super) fn add_npc_to_spawned(&self, key: u32, npc: NpcRef) {
        self.spawned_map().entry(key).or_default().push(npc);
        // The first-NPC anchor of `_playerCountCache` can change with every add.
        self.invalidate_membership_caches();
    }
}
