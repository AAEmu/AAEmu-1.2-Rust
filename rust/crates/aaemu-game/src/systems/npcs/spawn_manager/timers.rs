use super::*;

impl SpawnManager {
    // ----- respawn / despawn -----

    /// `Stop()`.
    pub fn stop(&self) {
        self.work.store(false, Ordering::SeqCst);
    }

    fn schedule(
        &self,
        server: &GameServer,
        set: &Mutex<HashMap<u32, Scheduled>>,
        fields: &DashMap<u32, Instant>,
        obj: WorldObject,
        at: Option<Instant>,
    ) {
        let id = obj.obj_id();
        if let Some(t) = at {
            fields.insert(id, t);
        }
        let at = fields.get(&id).map(|t| *t);
        let spawns = &server.systems.npcs.spawns;
        let link = match &obj {
            WorldObject::Npc(_) => spawns
                .npc_spawner_of(id)
                .map_or(SpawnerLink::None, SpawnerLink::Npc),
            WorldObject::Doodad(_) => spawns
                .doodad_spawner_of(id)
                .map_or(SpawnerLink::None, SpawnerLink::Doodad),
            _ => SpawnerLink::None,
        };
        let mut set = set.lock().expect("poisoned");
        match set.get_mut(&id) {
            Some(e) if e.obj.same(&obj) => {
                e.at = at;
                e.link = link;
            }
            _ => {
                let seq = self.seq.fetch_add(1, Ordering::SeqCst);
                set.insert(id, Scheduled { obj, link, at, seq });
            }
        }
    }

    /// `AddRespawn(obj)` with `obj.Respawn = at` (`None` keeps the object's current value).
    pub fn add_respawn(&self, server: &GameServer, obj: WorldObject, at: Option<Instant>) {
        let fields = &server.systems.npcs.spawns.respawn_fields;
        self.schedule(server, &self.respawns, fields, obj, at);
    }

    /// `AddDespawn(obj)` with `obj.Despawn = at` (`None` keeps the object's current value).
    pub fn add_despawn(&self, server: &GameServer, obj: WorldObject, at: Option<Instant>) {
        let fields = &server.systems.npcs.spawns.despawn_fields;
        self.schedule(server, &self.despawns, fields, obj, at);
    }

    /// Objects waiting for a respawn (obj id, time).
    pub fn pending_respawns(&self) -> Vec<(u32, Option<Instant>)> {
        let mut v: Vec<_> = self
            .respawns
            .lock()
            .expect("poisoned")
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|e| e.seq);
        v.into_iter().map(|e| (e.obj.obj_id(), e.at)).collect()
    }

    /// Objects waiting for a despawn (obj id, time).
    pub fn pending_despawns(&self) -> Vec<(u32, Option<Instant>)> {
        let mut v: Vec<_> = self
            .despawns
            .lock()
            .expect("poisoned")
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|e| e.seq);
        v.into_iter().map(|e| (e.obj.obj_id(), e.at)).collect()
    }

    /// Whether `obj_id` waits for a queued respawn or despawn in this instance. While it does,
    /// the timers own the object id: they release it once both are done (see
    /// [`Self::check_respawns_tick`]), so nobody else may release it earlier.
    pub fn has_pending_timers(&self, obj_id: u32) -> bool {
        self.respawns
            .lock()
            .expect("poisoned")
            .contains_key(&obj_id)
            || self
                .despawns
                .lock()
                .expect("poisoned")
                .contains_key(&obj_id)
    }

    /// Whether `obj_id` already waits for a queued corpse / immediate despawn in this instance.
    pub fn has_pending_despawn(&self, obj_id: u32) -> bool {
        self.despawns
            .lock()
            .expect("poisoned")
            .contains_key(&obj_id)
    }

    /// Whether the placement (`shared` entry of one JSON line, shared by its memberwise clones)
    /// still owns queued respawn / despawn deadlines in this instance. The activation sweep uses
    /// it to keep an emptied cold placement tracked until its timers drain (every deadline is
    /// processed by [`Self::check_respawns_tick`], so the set cannot grow forever).
    pub(super) fn placement_has_pending_timers(&self, shared: &Arc<NpcSpawnerShared>) -> bool {
        let linked = |e: &Scheduled| matches!(&e.link, SpawnerLink::Npc(s) if Arc::ptr_eq(s.shared(), shared));
        if self
            .respawns
            .lock()
            .expect("poisoned")
            .values()
            .any(&linked)
        {
            return true;
        }
        self.despawns
            .lock()
            .expect("poisoned")
            .values()
            .any(&linked)
    }

    fn ready(
        set: &Mutex<HashMap<u32, Scheduled>>,
        fields: &DashMap<u32, Instant>,
        now: Instant,
    ) -> Vec<Scheduled> {
        let mut v: Vec<Scheduled> = set
            .lock()
            .expect("poisoned")
            .values()
            .cloned()
            .map(|mut e| {
                // Looting updates obj.Despawn after scheduling the corpse. Read the
                // live field, like CheckRespawns in C#, rather than its old snapshot.
                if let Some(at) = fields.get(&e.obj.obj_id()) {
                    e.at = Some(*at);
                }
                e
            })
            .filter(|e| e.at.is_none_or(|t| t <= now))
            .collect();
        v.sort_by_key(|e| e.seq);
        v
    }

    fn remove_entry(set: &Mutex<HashMap<u32, Scheduled>>, e: &Scheduled) {
        let mut set = set.lock().expect("poisoned");
        if set.get(&e.obj.obj_id()).is_some_and(|x| x.seq == e.seq) {
            set.remove(&e.obj.obj_id());
        }
    }

    /// One iteration of `CheckRespawns` (the 1 s respawn / despawn thread).
    pub fn check_respawns_tick(&self, server: &GameServer) {
        let _items = server.systems.economy.operation_scope();
        let _serial = server.systems.npcs.spawns.spawn_lock();
        for e in Self::ready(
            &self.respawns,
            &server.systems.npcs.spawns.respawn_fields,
            now(),
        ) {
            if e.at.is_some_and(|t| t > now()) {
                continue;
            }
            match (&e.link, &e.obj) {
                // `npc.Spawner.Respawn(npc)` is disabled: Update() spawns again.
                (SpawnerLink::Npc(s), _) => s.set_spawn_scheduled(false),
                (SpawnerLink::Doodad(s), WorldObject::Doodad(d)) => s.respawn(server, d),
                // Transfer spawner respawns (vehicles). Gimmicks are never scheduled in the 1.2
                // C# (`GimmickSpawner.DecreaseCount` has no caller).
                (_, o) => crate::systems::vehicles::respawn_object(server, o),
            }
            Self::remove_entry(&self.respawns, &e);
            if matches!(e.link, SpawnerLink::Npc(_))
                && !self
                    .despawns
                    .lock()
                    .expect("poisoned")
                    .contains_key(&e.obj.obj_id())
                && self.world.object(e.obj.obj_id()).is_none()
            {
                // The looted corpse can disappear before its respawn timer. Reserve
                // its ID until both timers finish so another NPC cannot replace the
                // pending entry or supply a different live deadline under the same ID.
                server.ids.object.release_id(e.obj.obj_id());
                forget_object(server, e.obj.obj_id());
            }
        }

        for e in Self::ready(
            &self.despawns,
            &server.systems.npcs.spawns.despawn_fields,
            now(),
        ) {
            if e.at.is_some_and(|t| t > now()) {
                continue;
            }
            match (&e.link, &e.obj) {
                (SpawnerLink::Npc(s), WorldObject::Npc(n)) => s.despawn(server, n),
                (SpawnerLink::Doodad(s), WorldObject::Doodad(d)) => s.despawn(server, d),
                // Transfer spawner despawns (vehicles); `slave.Delete()` is the common delete.
                (_, o) => {
                    if !crate::systems::vehicles::despawn_object(server, o) {
                        delete_object(server, o, Some(self.world.id));
                    }
                }
            }
            // A stale corpse handle must not release the id (or drop the spawner link) of the
            // live NPC that reused it.
            let superseded = e
                .obj
                .as_npc()
                .is_some_and(|n| super::npc::spawner::npc_superseded(server, n));
            // DoodadSpawner owns its ID release and may retain it for a pending respawn.
            if !superseded
                && !matches!(
                    (&e.link, &e.obj),
                    (SpawnerLink::Doodad(_), WorldObject::Doodad(_))
                )
                && !self
                    .respawns
                    .lock()
                    .expect("poisoned")
                    .get(&e.obj.obj_id())
                    .is_some_and(|r| matches!(r.link, SpawnerLink::Npc(_)) && r.obj.same(&e.obj))
            {
                server.ids.object.release_id(e.obj.obj_id());
                forget_object(server, e.obj.obj_id());
            }
            Self::remove_entry(&self.despawns, &e);
        }

        // `NpcGroupSupporter` timer: lost members of an `enable_respawn` batch (10.x
        // `sub_39343460`). Runs under the same spawn lock as the respawn deadlines above.
        server
            .systems
            .npcs
            .group_batches
            .check_respawns(server, self.world.id);
        drop(_serial);
        if let Some(h) = server.systems.npcs.hooks.economy.get() {
            h.make_loot_public_tick(server, self.world.id);
        }
    }

    pub(super) fn start_respawn_tick(self: &Arc<Self>, server: &Arc<GameServer>) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("RespawnThread_{}: no runtime", self.world.id);
            return;
        };
        let me = self.clone();
        let weak = Arc::downgrade(server);
        rt.spawn(async move {
            while me.work.load(Ordering::SeqCst) {
                let Some(server) = weak.upgrade() else {
                    break;
                };
                me.check_respawns_tick(&server);
                drop(server);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Cancel corpse/respawn timers before deleting an event-owned NPC. Prevents a later
    /// timer from deleting a different object after its id has been reused.
    pub fn cancel_npc_timers(&self, npc: &NpcRef) {
        for set in [&self.respawns, &self.despawns] {
            Self::cancel_npc_timer(set, npc);
        }
    }

    /// Stop an event NPC from respawning while leaving its normal loot corpse timer intact.
    pub fn cancel_npc_respawn(&self, npc: &NpcRef) {
        Self::cancel_npc_timer(&self.respawns, npc);
    }

    /// `NpcSpawner.ForceSpawn` repopulates a placement immediately, so its queued respawn
    /// deadlines are obsolete. Cancels only the respawns of spawners sharing `shared` (one
    /// authored placement across its memberwise clones); corpse despawn timers, other placements
    /// and other instances stay intact. An id is released only when neither the corpse despawn
    /// entry nor a live world object still owns it, mirroring `check_respawns_tick`. Every
    /// cancelled clone's `_scheduledCount` credits are reconciled too (its future timers are
    /// gone), not just the `ForceSpawn` caller's. Returns the number of cancelled respawns.
    pub fn cancel_placement_npc_respawns(
        &self,
        server: &GameServer,
        shared: &Arc<NpcSpawnerShared>,
    ) -> usize {
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let entries: Vec<Scheduled> = {
            let set = self.respawns.lock().expect("poisoned");
            set.values()
                .filter(|e| match &e.link {
                    SpawnerLink::Npc(s) => Arc::ptr_eq(s.shared(), shared),
                    _ => false,
                })
                .cloned()
                .collect()
        };
        for e in &entries {
            Self::remove_entry(&self.respawns, e);
            let id = e.obj.obj_id();
            // Drop the queue locks before the world / object locks (as `check_respawns_tick`).
            let corpse_pending = self.despawns.lock().expect("poisoned").contains_key(&id);
            if !corpse_pending && self.world.object(id).is_none() {
                server.ids.object.release_id(id);
                forget_object(server, id);
            }
            // All same-placement deadlines are gone, so each linked clone's credits are stale.
            if let SpawnerLink::Npc(s) = &e.link {
                s.reconcile_scheduled_count(0);
            }
        }
        entries.len()
    }

    fn cancel_npc_timer(set: &Mutex<HashMap<u32, Scheduled>>, npc: &NpcRef) {
        let mut entries = set.lock().expect("poisoned");
        if entries
            .get(&npc.obj_id)
            .is_some_and(|e| e.obj.as_npc().is_some_and(|n| Arc::ptr_eq(n, npc)))
        {
            entries.remove(&npc.obj_id);
        }
    }
}
