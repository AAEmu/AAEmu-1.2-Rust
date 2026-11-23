use super::*;

impl NpcSpawner {
    // ----- despawn -----

    /// `Despawn(npc)`: removes the NPC from `SpawnedNpcs` and deletes it.
    pub fn despawn(&self, server: &GameServer, npc: &NpcRef) {
        let _lock = server.systems.npcs.spawns.spawn_lock();
        if npc_superseded(server, npc) {
            // Stale handle (e.g. a corpse timer that outlived a `despawn` GM command): its
            // object id belongs to another live NPC now, whose spawner link, membership entry
            // and world object are not this handle's to remove or delete.
            tracing::debug!(
                "Skipped despawn of stale NPC {}: its object id is reused by another NPC.",
                npc.obj_id
            );
            self.st().is_despawn_scheduled = false;
            return;
        }
        self.remove_npc_from_spawned_list(server, npc);
        // npc.UnregisterNpcEvents(): `Npc.Delete` drops the AI with its registrations.
        spawn_manager::delete_object(server, &WorldObject::from(npc), self.parent_world());
        self.st().is_despawn_scheduled = false;
    }

    fn remove_npc_from_spawned_list(&self, server: &GameServer, npc: &NpcRef) {
        let Some(spawner) = self.npc_spawner(server, npc) else {
            tracing::warn!(
                "NPC {} has no associated Spawner.",
                npc.data().unit.template_id
            );
            return;
        };
        let id = spawner.spawner_id;
        if !self.remove_spawned_npc_exact(id, npc) {
            if server.world.object(npc.obj_id).is_none() {
                // Deviation from the C# (which never removes on a direct `Npc.Delete`): the
                // object left the world before this despawn (a corpse deleted by a script / GM
                // command while its corpse timer was pending), and `reconcile_deleted_npc`
                // already dropped its entry. Nothing is inconsistent.
                tracing::debug!(
                    "NPC {} already removed from SpawnedNpcs for SpawnerId={id}.",
                    npc.obj_id
                );
            } else {
                tracing::warn!(
                    "NPC {} not found in SpawnedNpcs for SpawnerId={id}.",
                    npc.obj_id
                );
            }
        }
    }

    /// Reconciliation of a direct delete (`Npc.Delete`): the object is gone from the world, so
    /// its exact entry (`Arc` identity) leaves the shared `SpawnedNpcs` map and the population
    /// caches computed from the old membership are dropped. Without this, an alive deleted
    /// `NpcRef` keeps counting against the spawn caps and blocks every replacement. Quiet and
    /// idempotent: the despawn paths already removed the entry before deleting the object.
    /// Returns whether this exact NPC was in the list.
    pub fn reconcile_deleted_npc(&self, server: &GameServer, npc: &NpcRef) -> bool {
        let Some(spawner) = self.npc_spawner(server, npc) else {
            return false;
        };
        self.remove_spawned_npc_exact(spawner.spawner_id, npc)
    }

    /// Exact-identity removal from the shared map: only the `Arc` equal to `npc` is removed, so
    /// a reused object id or an identical member of another placement is untouched. An emptied
    /// list drops its key. A successful removal invalidates the population caches
    /// (`_playerCountCache` anchors on the map's first NPC, `_npcsInZoneCache` counts the other
    /// keys), which are leaf locks taken after the map guard is released.
    pub(super) fn remove_spawned_npc_exact(&self, key: u32, npc: &NpcRef) -> bool {
        let removed = {
            let mut map = self.spawned_map();
            match map.get_mut(&key) {
                Some(list) => {
                    let before = list.len();
                    list.retain(|n| !Arc::ptr_eq(n, npc));
                    let removed = list.len() < before;
                    if list.is_empty() {
                        map.remove(&key);
                    }
                    removed
                }
                None => false,
            }
        };
        if removed {
            self.invalidate_membership_caches();
        }
        removed
    }

    /// `DoDespawn(npc)` (`Npc.DoDie`, `NpcSpawnerDoDespawnTask`, `SpawnManager.DeSpawnAll`):
    /// schedules the respawn (when allowed) and the despawn of the corpse.
    pub fn do_despawn(&self, server: &GameServer, npc: &NpcRef) {
        let _lock = server.systems.npcs.spawns.spawn_lock();
        let Some(t) = self.template() else {
            tracing::error!(
                "Failed to process despawn for NPC {}:{}:{}.",
                self.unit_id,
                self.spawner_id,
                npc.obj_id
            );
            return;
        };
        let obj = WorldObject::from(npc);
        let Some(manager) = spawn_manager::manager_of(server, &obj, self.parent_world()) else {
            tracing::error!(
                "Failed to process despawn for NPC {}:{}:{}.",
                self.unit_id,
                self.spawner_id,
                npc.obj_id
            );
            return;
        };
        let respawn_time = self.respawn_time();
        let now = spawn_manager::now();
        // Authored `npc_spawners.save_indun`, or a spawn-file placement in a per-run dungeon
        // (see `freezes_dead_member`): a 1.2 run is one run long and the dead stay dead until
        // the run is recreated. The placement is frozen so neither the respawn deadline nor the
        // periodic refill (`do_spawn` / the activation tick) brings the member back. Only a
        // death freezes: a scheduled despawn of a living NPC is not one.
        let dead = npc.data().unit.is_dead();
        let freezes = dead && self.freezes_dead_member(server, t.save_indun);
        if freezes {
            self.shared.set_saved_death();
        }
        // `npc_groups.enable_respawn = 't'`: a dead member whose batch still has a living
        // member is respawned by the group's partial timer (`RespawnLostGroupMembers`), not by
        // a whole-batch spawner refill. The group reserves its dead slots in
        // `check_spawn_count_can_spawn`, so the spawner does not fill them in the meantime.
        // The last death of the batch keeps the normal spawner respawn below.
        let partial_group_respawn =
            npc.data().unit.is_dead() && server.systems.npcs.group_batches.on_member_died(npc);
        if !partial_group_respawn
            && !freezes
            && respawn_time > 0
            && i64::from(self.are_other_npcs_in_spawn_zone().1) + i64::from(self.scheduled_count())
                < i64::from(t.max_population)
        {
            self.increment_count(true);
            let dead_ms = npc.data().dead_time_ms;
            let since_death_ms = if dead_ms == 0 {
                0
            } else {
                crate::world::now_ms() - dead_ms
            };
            let at = spawn_manager::add_secs(
                now,
                f64::from(respawn_time) - since_death_ms as f64 / 1000.0,
            );
            manager.add_respawn(server, obj.clone(), Some(at));
        } else {
            self.increment_count(false);
        }
        let mut despawn_secs = f64::from(self.despawn_time);
        let has_loot = server
            .systems
            .npcs
            .hooks
            .economy
            .get()
            .is_some_and(|h| h.npc_has_loot_items(server, npc));
        if has_loot {
            despawn_secs += f64::from(LOOT_DESPAWN_EXTENSION_TIME);
        }
        manager.add_despawn(
            server,
            obj,
            Some(spawn_manager::add_secs(now, despawn_secs)),
        );
    }

    fn do_despawn_now(&self, server: &GameServer, npc: &NpcRef) {
        let _lock = server.systems.npcs.spawns.spawn_lock();
        let max = self.template().map_or(0, |t| i64::from(t.max_population));
        if i64::from(self.are_other_npcs_in_spawn_zone().1) + i64::from(self.scheduled_count())
            < max
        {
            self.increment_count(true);
            // Routine during a player departure / retention reclaim (the activation sweep
            // schedules whole cold neighbourhoods), so not an info event.
            tracing::debug!(
                "Immediate despawn scheduled for NPC {}:{}:{}.",
                self.unit_id,
                self.spawner_id,
                npc.obj_id
            );
        }
        let obj = WorldObject::from(npc);
        match spawn_manager::manager_of(server, &obj, self.parent_world()) {
            // The despawn time is whatever `npc.Despawn` holds (MinValue unless it died).
            Some(m) => m.add_despawn(server, obj, None),
            None => tracing::error!(
                "Failed to immediately despawn NPC {}:{}:{}.",
                self.unit_id,
                self.spawner_id,
                npc.obj_id
            ),
        }
    }

    /// Immediate healthy removal of one distance-reclaimed NPC, completing in the reclaim pass
    /// instead of a queued `DoDespawnNow`: the queued variant deletes only on the next 1 s
    /// respawn tick, so an NPC re-shown by a returning player in that window would still be
    /// removed. A living reclaimable NPC owns no corpse / respawn deadline (both are protected
    /// by [`reclaimable_npc`] and the dead check), so its exact slot entry, world object and
    /// object id are released here like the respawn tick does after a queued despawn;
    /// `_scheduledCount` keeps the same reservation credit as `DoDespawnNow`.
    pub(super) fn despawn_reclaimed_now(&self, server: &GameServer, npc: &NpcRef) {
        let _lock = server.systems.npcs.spawns.spawn_lock();
        let max = self.template().map_or(0, |t| i64::from(t.max_population));
        if i64::from(self.are_other_npcs_in_spawn_zone().1) + i64::from(self.scheduled_count())
            < max
        {
            self.increment_count(true);
        }
        self.remove_npc_from_spawned_list(server, npc);
        spawn_manager::delete_object(server, &WorldObject::from(npc), self.parent_world());
        server.ids.object.release_id(npc.obj_id);
        spawn_manager::forget_object(server, npc.obj_id);
        tracing::debug!(
            "Reclaimed cold NPC {}:{}:{}.",
            self.unit_id,
            self.spawner_id,
            npc.obj_id
        );
    }

    /// `DespawnWithRespawn(npc)` (`KillNpcWithoutCorpseEffect`, `NpcDeleteTask`, simulation).
    /// The delete, the reserved respawn slot and its `_scheduledCount` credit are one step for
    /// the activation tick (reentrant with callers that already hold the lock).
    pub fn despawn_with_respawn(&self, server: &GameServer, npc: &NpcRef) {
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let obj = WorldObject::from(npc);
        let manager = spawn_manager::manager_of(server, &obj, self.parent_world());
        spawn_manager::delete_object(server, &obj, self.parent_world());
        let respawn_time = self.respawn_time();
        // The one-run freeze needs a death or a claimed scripted removal
        // (`KillNpcWithoutCorpse`, which counts as a kill for the monster hunt), like
        // `do_despawn`; a living NPC that finishes a scripted walk and is removed
        // (`Simulation.StopMove`) keeps its respawn.
        let gone = {
            let d = npc.data();
            d.unit.is_dead() || d.unit.scripted_removed
        };
        let save_indun = self.template().is_some_and(|t| t.save_indun);
        let freezes = gone && self.freezes_dead_member(server, save_indun);
        if freezes {
            self.shared.set_saved_death();
        }
        let max = self.template().map_or(0, |t| i64::from(t.max_population));
        if !freezes && respawn_time > 0 && i64::from(self.are_other_npcs_in_spawn_zone().1) < max {
            let at = spawn_manager::add_secs(spawn_manager::now(), f64::from(respawn_time));
            if let Some(m) = manager {
                m.add_respawn(server, obj, Some(at));
            }
            let mut st = self.st();
            st.scheduled_count += 1;
            if st.scheduled_count < 0 {
                st.scheduled_count = 0;
            }
        }
    }

    /// `DoDespawns(npcs)`: `DoDespawn` for every NPC (in combat or not).
    pub fn do_despawns(&self, server: &GameServer, npcs: &[NpcRef]) {
        let _lock = server.systems.npcs.spawns.spawn_lock();
        self.st().is_despawn_scheduled = true;
        for npc in npcs {
            self.do_despawn(server, npc);
        }
        self.st().is_despawn_scheduled = false;
    }

    /// `DoDespawnsNow(npcs)`.
    pub fn do_despawns_now(&self, server: &GameServer, npcs: &[NpcRef]) {
        let _lock = server.systems.npcs.spawns.spawn_lock();
        self.st().is_despawn_scheduled = true;
        for npc in npcs {
            self.do_despawn_now(server, npc);
            tracing::debug!(
                "Despawned NPC {}:{}.",
                npc.data().unit.template_id,
                npc.obj_id
            );
        }
        self.st().is_despawn_scheduled = false;
    }
}
