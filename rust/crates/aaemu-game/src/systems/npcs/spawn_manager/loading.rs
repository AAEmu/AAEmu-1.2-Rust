use super::*;

/// What `Load` read (logs and tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadStats {
    pub npc_files: usize,
    pub npc_entries: usize,
    pub npc_duplicates: usize,
    pub npc_missing_templates: usize,
    /// Entries that produced spawner clones in `NpcSpawners`.
    pub npc_entries_with_spawners: usize,
    /// `NpcSpawner` clones in `NpcSpawners`.
    pub npc_spawners: usize,
    /// Generated ("fake") spawner templates (the generated spawner is stored too, unlike the C#).
    pub npc_fake_spawners: usize,
    pub npc_event_spawners: usize,
    /// Plain rows that skipped a spawner with `npc_spawners.activation_state = 'f'` in a dungeon
    /// instance (see [`spawner_skip`]).
    pub npc_event_only_spawners_skipped: usize,
    /// Plain rows that skipped a category-0 `연출` cutscene spawner (any world; see
    /// [`spawner_skip`]).
    pub npc_cutscene_spawners_skipped: usize,
    pub tower_def_spawners: usize,
    pub doodad_files: usize,
    pub doodad_entries: usize,
    pub doodad_duplicates: usize,
    pub doodad_missing_templates: usize,
    pub doodad_spawners: usize,
    pub transfer_entries: usize,
    pub transfer_spawners: usize,
    pub gimmick_entries: usize,
    pub gimmick_spawners: usize,
    pub slave_entries: usize,
    pub slave_spawners: usize,
}

/// `Exist(templateId)` of the template managers `Load` filters with.
pub struct TemplateExists<'a> {
    pub npc: &'a dyn Fn(u32) -> bool,
    pub doodad: &'a dyn Fn(u32) -> bool,
    pub transfer: &'a dyn Fn(u32) -> bool,
    pub gimmick: &'a dyn Fn(u32) -> bool,
    pub slave: &'a dyn Fn(u32) -> bool,
}

/// `Directory.GetFiles(dir, "<prefix>*.json")` (file-name order, case-insensitive match);
/// `None` when the directory cannot be read.
fn spawn_files(dir: &Path, prefix: &str) -> Option<Vec<PathBuf>> {
    let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            (name.starts_with(prefix) && name.ends_with(".json")).then(|| (name, e.path()))
        })
        .collect();
    files.sort();
    Some(files.into_iter().map(|(_, p)| p).collect())
}

/// Reads a spawn file (`FileManager.GetFileContents`); `None` = missing / empty.
fn read_spawn_file(world: &WorldInstance, path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() && s.trim() != "\u{feff}" => Some(s),
        Ok(_) => {
            tracing::warn!("File {} is empty.", path.display());
            None
        }
        Err(_) => {
            tracing::info!(
                "World {} is missing {}",
                world.template.name,
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            None
        }
    }
}

/// Outcome of one `AddNpcSpawner` entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct AddedSpawners {
    /// Spawner clones stored in `NpcSpawners`.
    pub(super) added: usize,
    /// Game-data spawners a plain row would have expanded into but that start inactive
    /// (`activation_state = 'f'` in a dungeon instance; see [`spawner_skip`]).
    skipped_inactive: usize,
    /// Category-0 `연출` cutscene spawners a plain row would have expanded into (see
    /// [`spawner_skip`]).
    skipped_cutscene: usize,
}

impl SpawnManager {
    // ----- AddNpcSpawner -----

    /// `AddNpcSpawner(npcSpawner)` (the JSON entry). Returns the number of spawners added to
    /// `NpcSpawners` (0 for event spawners). `Err` where the C# throws (missing spawner template
    /// 1, duplicate fake ids).
    pub fn add_npc_spawner(
        &self,
        server: &GameServer,
        def: NpcSpawnerDef,
    ) -> Result<usize, String> {
        Ok(self.add_npc_spawner_with(server, def, true)?.added)
    }

    /// [`Self::add_npc_spawner`]; `hold_group_members` keeps the entries of tower defense group
    /// members back for [`Self::resolve_tower_groups`] (false when that releases them).
    pub(super) fn add_npc_spawner_with(
        &self,
        server: &GameServer,
        def: NpcSpawnerDef,
        hold_group_members: bool,
    ) -> Result<AddedSpawners, String> {
        let data = &server.game_data.systems.npcs.npc_spawners;
        let spawns = &server.systems.npcs.spawns;
        let mut inner = self.inner();
        let mut ids = def.npc_spawner_ids.clone();
        if ids == [0] {
            ids.clear();
        }
        let b = &def.base;
        let new_shared = || {
            let shared = NpcSpawnerShared::new(b.position);
            shared.set_management_group(b.spawn_group_id);
            shared
        };
        let init =
            |id: u32, spawner_id: u32, ids: Vec<u32>, template: Option<Arc<NpcSpawnerTemplate>>| {
                NpcSpawnerInit {
                    id,
                    spawner_id,
                    unit_id: b.unit_id,
                    follow_path: b.follow_path.clone(),
                    follow_npc: b.follow_npc,
                    respawn_time: b.respawn_time,
                    despawn_time: b.despawn_time,
                    special_link: b.special_link.clone(),
                    count: def.count,
                    npc_spawner_ids: ids,
                    parent_world: Some(self.world.id),
                    spawnable_npcs: Vec::new(),
                    template,
                }
            };
        let tower_ids = server
            .game_data
            .systems
            .instances
            .tower_defs
            .npc_spawner_ids();
        // An explicit ordinary spawner disambiguates NPC templates also used in tower waves.
        let single_placement = (ids.is_empty() && b.spawner_id != 0)
            .then(|| data.get_npc_spawner_template(b.spawner_id))
            .flatten()
            .filter(|t| {
                t.npc_spawner_category_id == npc_spawner_category::AUTOCREATED
                    && !tower_ids.contains(&t.id)
                    && spawner_skip(&self.world.template.name, t).is_none()
                    && matches!(t.npcs.as_slice(), [member]
                        if member.member_type == "Npc" && member.member_id == b.unit_id)
            });
        let controller_ids: Vec<_> = data
            .get_spawner_ids(b.unit_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|id| tower_ids.contains(id))
            .filter(|id| {
                data.get_npc_spawner_template(*id).is_some_and(|t| {
                    t.npcs
                        .iter()
                        .any(|n| n.member_id == b.unit_id && n.member_type == "Npc")
                })
            })
            .filter(|id| single_placement.is_none() && (ids.is_empty() || ids.contains(id)))
            .collect();
        if !controller_ids.is_empty() {
            // The shipped JSON lists event controllers as ordinary NPCs. Retain their
            // authored positions/paths, but only the tower defense runtime may spawn them.
            for id in controller_ids {
                let mut template = NpcSpawnerTemplate::with_member(id, b.unit_id);
                template.spawn_delay_min = 0.0;
                template.spawn_delay_max = 0.0;
                let spawner = NpcSpawner::new(
                    init(id, id, vec![id], Some(Arc::new(template))),
                    new_shared(),
                );
                inner
                    .tower_def_spawners
                    .entry(id)
                    .or_default()
                    .push(Arc::new(spawner));
            }
            return Ok(AddedSpawners::default());
        }
        if hold_group_members
            && ids.is_empty()
            && single_placement.is_none()
            && server
                .game_data
                .systems
                .instances
                .tower_defs
                .group_npcs
                .contains(&b.unit_id)
        {
            // The shipped JSON lists the members of an event's NPC group one entry each at the
            // group's spawn site. Only the tower defense runtime may spawn them.
            inner.tower_group_entries.push(def.clone());
            return Ok(AddedSpawners::default());
        }
        // A world placement can name a group template explicitly. The ordinary UnitId
        // lookup intentionally selects single-NPC spawners, even for a group leader.
        let group_placement = if ids.is_empty() && b.spawner_id != 0 {
            data.get_npc_spawner_template(b.spawner_id).filter(|t| {
                matches!(t.npcs.as_slice(), [member] if member.member_type == "NpcGroup"
                    && server.game_data.systems.npcs.npc_groups.members(member.member_id)
                        .iter().any(|npc| npc.npc_id == b.unit_id && npc.is_leader))
            })
        } else {
            None
        };
        let placement_template = if def.placement_schedule_id != 0 {
            let schedule_id = def.placement_schedule_id;
            let schedule = data
                .get_npc_spawner_template(schedule_id)
                .filter(|t| {
                    (t.start_time > 0.0 || t.end_time > 0.0)
                        && t.npcs
                            .iter()
                            .any(|n| n.member_id == b.unit_id && n.member_type == "Npc")
                })
                .ok_or_else(|| {
                    format!(
                        "invalid placement schedule {schedule_id} for NPC {}",
                        b.unit_id
                    )
                })?;
            if !ids.is_empty() {
                return Err(format!(
                    "placement schedule {schedule_id} also lists event spawners"
                ));
            }
            // The area template describes the whole area. Each authored point gets its own
            // NPC and only inherits the area's clock window.
            let mut template = NpcSpawnerTemplate::with_member(schedule_id, b.unit_id);
            template.start_time = schedule.start_time;
            template.end_time = schedule.end_time;
            template.destroy_time = schedule.destroy_time;
            Some(template)
        } else {
            single_placement
                .or(group_placement)
                .map(|template| (*template).clone())
        };
        if let Some(mut template) = placement_template {
            // An area template can describe several packs. The placement count bounds
            // this point; each NpcGroup member expands to its full encounter.
            template.min_population = def.count;
            template.max_population = def.count;
            let template_id = template.id;
            let template = Arc::new(template);
            let spawner = NpcSpawner::new(
                init(
                    inner.next_id,
                    template_id,
                    vec![template_id],
                    Some(template.clone()),
                ),
                new_shared(),
            );
            spawner.mark_placement();
            spawner.initialize_spawnable_npcs(Some(template.as_ref()));
            inner.next_id += 1;
            let key = inner.next_id;
            inner
                .npc_spawners
                .entry(key)
                .or_insert_with(|| vec![Arc::new(spawner)]);
            self.invalidate_activation_index();
            return Ok(AddedSpawners {
                added: 1,
                ..AddedSpawners::default()
            });
        }
        if ids.is_empty() {
            let game_data_ids = data.get_spawner_ids(b.unit_id).unwrap_or_default();
            let mut spawners = Vec::new();
            let mut added = 0;
            let mut skipped_inactive = 0;
            let mut skipped_cutscene = 0;
            if game_data_ids.is_empty() {
                // No pre-defined spawner: generate one from template 1 ("Test Warrior").
                tracing::trace!("SpawnerIds for Npc={} doesn't exist", b.unit_id);
                let id = spawns.fake_spawner_id.load(Ordering::SeqCst);
                let Some(tmp) = data.get_npc_spawner_template(1) else {
                    return Err(format!(
                        "AddNpcSpawner: npc spawner template 1 is missing (Npc={})",
                        b.unit_id
                    ));
                };
                let mut template = (*tmp).clone();
                template.id = id;
                let tmp_npc = NpcSpawnerNpc {
                    id,
                    npc_spawner_template_id: id,
                    member_id: b.unit_id,
                    member_type: "Npc".into(),
                    weight: 1.0,
                    unit_id: 0,
                };
                template.npcs = vec![tmp_npc.clone()];
                if !data.add_npc_spawner_npc(tmp_npc.clone()) {
                    return Err(format!("AddNpcSpawnerNpc: duplicate id {id}"));
                }
                data.add_member_and_spawner_template_ids(&tmp_npc);
                let template = Arc::new(template);
                if !data.add_npc_spawner((*template).clone()) {
                    return Err(format!("AddNpcSpawner: duplicate id {id}"));
                }
                spawns.fake_spawner_id.fetch_add(1, Ordering::SeqCst);
                // Deviation from the C#: the generated spawner is kept for this entry too. The C#
                // dropped it (`spawners` stayed empty), losing the first placement of every NPC
                // without game-data spawners; later entries cloned the generated template.
                let spawner = NpcSpawner::new(
                    init(inner.next_id, id, vec![id], Some(template.clone())),
                    new_shared(),
                );
                spawner.mark_placement();
                spawner.initialize_spawnable_npcs(Some(template.as_ref()));
                spawners.push(Arc::new(spawner));
                inner.next_id += 1;
                added += 1;
            } else {
                // Memberwise clones sharing Position / SpawnedNpcs / caches / NpcSpawnerIds.
                let shared = new_shared();
                for &id in &game_data_ids {
                    let template = data.get_npc_spawner_template(id);
                    match template
                        .as_deref()
                        .and_then(|t| spawner_skip(&self.world.template.name, t))
                    {
                        Some(SpawnerSkip::InactiveAtStart) => {
                            // Event spawner of a dungeon: a plain row must not pre-spawn it.
                            skipped_inactive += 1;
                            continue;
                        }
                        Some(SpawnerSkip::Cutscene) => {
                            // Staging actor: it is not a world spawn.
                            skipped_cutscene += 1;
                            continue;
                        }
                        None => {}
                    }
                    let spawner = NpcSpawner::new(
                        init(inner.next_id, id, game_data_ids.clone(), template.clone()),
                        shared.clone(),
                    );
                    spawner.mark_placement();
                    spawner.initialize_spawnable_npcs(template.as_deref());
                    spawners.push(Arc::new(spawner));
                    inner.next_id += 1;
                    added += 1;
                }
            }
            if !spawners.is_empty() {
                let key = inner.next_id;
                inner.npc_spawners.entry(key).or_insert(spawners);
                self.invalidate_activation_index();
            }
            Ok(AddedSpawners {
                added,
                skipped_inactive,
                skipped_cutscene,
            })
        } else {
            // NPC spawns for events.
            let last = *ids.last().expect("non-empty");
            let mut template = NpcSpawnerTemplate::with_member(last, b.unit_id);
            template.max_population = def.count.max(1);
            let spawner = NpcSpawner::new(
                init(last, b.spawner_id, ids, Some(Arc::new(template))),
                new_shared(),
            );
            let key = inner.next_id;
            inner
                .npc_event_spawners
                .entry(key)
                .or_insert_with(|| vec![Arc::new(spawner)]);
            inner.next_id += 1;
            Ok(AddedSpawners::default())
        }
    }

    // ----- Load -----

    /// `Load()`: spawn files, persistent doodads (main world) and the respawn tick.
    pub async fn load(self: &Arc<Self>, server: &Arc<GameServer>) -> anyhow::Result<LoadStats> {
        if self.loaded.load(Ordering::SeqCst) {
            return Ok(LoadStats::default());
        }
        tracing::info!("Loading spawn data for {} ...", self.world.template.name);
        let world_path = server
            .config
            .app_path
            .join("Data")
            .join("Worlds")
            .join(&self.world.template.name);
        let gd = &server.game_data;
        let sd = &gd.systems.npcs.npc_spawners;
        let npc = |id| gd.systems.npcs.npc.exist(id);
        let doodad = |id| gd.systems.npcs.doodad.exist(id);
        let transfer = |id| sd.transfer_template_ids.contains(&id);
        let gimmick = |id| gd.systems.npcs.gimmick.exist(id);
        let slave = |id| sd.slave_template_ids.contains(&id);
        let exists = TemplateExists {
            npc: &npc,
            doodad: &doodad,
            transfer: &transfer,
            gimmick: &gimmick,
            slave: &slave,
        };
        let stats = self
            .load_spawn_files(server, &world_path, &exists)
            .map_err(anyhow::Error::msg)?;

        if self.world.template.id == gd.worlds.default_world_template_id() {
            // Load player housing data; houses need to be spawned before doodads.
            crate::systems::housing::load_for_world(server, &self.world).await?;
            tracing::info!(
                "Loading persistent doodads for {}",
                self.world.template.name
            );
            let mut spawned = 0;
            // Load furniture and bound doodads
            spawned += self
                .spawn_persistent_doodads(server, doodad_owner_type::HOUSING, -1, None, false)
                .await?;
            // Reconcile bound doodads: spawn any missing from DB, remove duplicates
            crate::systems::housing::after_housing_doodads_loaded(server);
            // Load plants/packs and everything else that was placed into the world by players
            for owner_type in [doodad_owner_type::SYSTEM, doodad_owner_type::CHARACTER] {
                spawned += self
                    .spawn_persistent_doodads(server, owner_type, -1, None, false)
                    .await?;
            }
            tracing::info!("{spawned} doodads loaded in {}.", self.world.template.name);
            // Saved growth state of the world-spawned doodads, consumed by the initial spawn.
            let natural = &server.systems.npcs.doodads.persistence.natural;
            match natural.load(&server.db, self.world.id).await {
                Ok(n) => tracing::info!(
                    "Loaded {n} natural doodad growth states for {}",
                    self.world.template.name
                ),
                Err(e) => tracing::warn!(
                    "Natural doodad growth states of {} are not persisted: {e}",
                    self.world.template.name
                ),
            }
        }

        self.start_respawn_tick(server);
        self.loaded.store(true, Ordering::SeqCst);
        Ok(stats)
    }

    /// The spawn-file part of `Load` (`LoadNpcSpawns` .. `LoadSlaveSpawns`) from `world_path`
    /// (clears every spawner list first). `Err` = the C# `GameException` / parse failure.
    pub fn load_spawn_files(
        &self,
        server: &GameServer,
        world_path: &Path,
        exists: &TemplateExists<'_>,
    ) -> Result<LoadStats, String> {
        // Serialize the replacement with the activation pass, creation and despawn paths (the
        // reentrant lock; this function is sync and never held across an await).
        let _serial = server.systems.npcs.spawns.spawn_lock();
        self.respawns.lock().expect("poisoned").clear();
        self.despawns.lock().expect("poisoned").clear();
        {
            let mut inner = self.inner();
            inner.npc_spawners.clear();
            inner.npc_event_spawners.clear();
            inner.tower_def_spawners.clear();
            inner.tower_group_entries.clear();
            inner.doodad_spawners.clear();
            inner.transfer_spawners.clear();
            inner.gimmick_spawners.clear();
            inner.slave_spawners.clear();
            inner.player_doodads.clear();
        }
        // Reload: the ordinary spawner table was emptied and is refilled below; both steps
        // invalidate the activation index/queues.
        self.invalidate_activation_index();
        self.reset_activation();
        let mut stats = LoadStats::default();
        self.load_npc_spawns(server, world_path, exists, &mut stats)?;
        self.resolve_tower_groups(server, &mut stats)?;
        self.load_doodad_spawns(server, world_path, exists, &mut stats)?;
        self.load_simple_spawns(
            server,
            world_path,
            "transfer_spawns",
            exists.transfer,
            &mut stats,
        )?;
        self.load_gimmick_spawns(server, world_path, exists, &mut stats)?;
        self.load_simple_spawns(server, world_path, "slave_spawns", exists.slave, &mut stats)?;
        stats.tower_def_spawners = self.inner().tower_def_spawners.values().map(Vec::len).sum();
        tracing::info!("Spawn data for {}: {stats:?}", self.world.template.name);
        Ok(stats)
    }

    /// World id, zone id and `DegToRad` of a loaded position.
    fn fix_position(&self, p: &mut crate::data::world_templates::WorldSpawnPosition, rotate: bool) {
        p.world_id = self.world.id;
        p.zone_id = self.world.zone_id_at(p.x, p.y);
        if rotate {
            p.yaw = deg_to_rad(p.yaw);
            p.pitch = deg_to_rad(p.pitch);
            p.roll = deg_to_rad(p.roll);
        }
    }

    fn load_npc_spawns(
        &self,
        server: &GameServer,
        world_path: &Path,
        exists: &TemplateExists<'_>,
        stats: &mut LoadStats,
    ) -> Result<(), String> {
        let Some(mut files) = spawn_files(world_path, "npc_spawns") else {
            return Ok(());
        };
        files.reverse(); // ReverseSpawnFiles
                         // Positions of the entries stored in NpcSpawners, by UnitId (the duplicate
                         // filter; Z is part of the position, so cave / building floors do not collide).
        let mut stored: HashMap<u32, Vec<(f32, f32, f32)>> = HashMap::new();
        for path in files {
            let Some(text) = read_spawn_file(&self.world, &path) else {
                continue;
            };
            stats.npc_files += 1;
            let defs = parse_list(&text, NpcSpawnerDef::from_json)
                .map_err(|e| format!("SpawnManager: Parse {} file: {e}", path.display()))?;
            for (entry, mut def) in defs.into_iter().enumerate() {
                stats.npc_entries += 1;
                let (unit_id, x, y, z) = (
                    def.base.unit_id,
                    def.base.position.x,
                    def.base.position.y,
                    def.base.position.z,
                );
                if stored.get(&unit_id).is_some_and(|v| {
                    v.iter().any(|&(sx, sy, sz)| {
                        (sx - x).abs() < 2.0 && (sy - y).abs() < 2.0 && (sz - z).abs() < 2.0
                    })
                }) {
                    tracing::trace!(
                        "Duplicate NPC spawner found in {} (UnitId: {unit_id})",
                        path.display()
                    );
                    stats.npc_duplicates += 1;
                    continue;
                }
                if !(exists.npc)(unit_id) {
                    tracing::trace!(
                        "Npc Template {unit_id} (file entry {}) doesn't exist - {}",
                        entry + 1,
                        path.display()
                    );
                    stats.npc_missing_templates += 1;
                    continue;
                }
                self.fix_position(&mut def.base.position, true);
                let event = !def.npc_spawner_ids.is_empty() && def.npc_spawner_ids != [0];
                let fake_before = server.systems.npcs.spawns.peek_fake_spawner_id();
                let result = self.add_npc_spawner_with(server, def, true)?;
                stats.npc_event_only_spawners_skipped += result.skipped_inactive;
                stats.npc_cutscene_spawners_skipped += result.skipped_cutscene;
                let added = result.added;
                if event {
                    stats.npc_event_spawners += 1;
                } else if server.systems.npcs.spawns.peek_fake_spawner_id() != fake_before {
                    stats.npc_fake_spawners += 1;
                }
                if added > 0 {
                    stats.npc_entries_with_spawners += 1;
                    stats.npc_spawners += added;
                    stored.entry(unit_id).or_default().push((x, y, z));
                }
            }
        }
        Ok(())
    }

    fn load_doodad_spawns(
        &self,
        _server: &GameServer,
        world_path: &Path,
        exists: &TemplateExists<'_>,
        stats: &mut LoadStats,
    ) -> Result<(), String> {
        let Some(mut files) = spawn_files(world_path, "doodad_spawns") else {
            return Ok(());
        };
        files.reverse(); // ReverseSpawnFiles
        let mut stored: HashMap<u32, Vec<(f32, f32, f32)>> = HashMap::new();
        for path in files {
            let Some(text) = read_spawn_file(&self.world, &path) else {
                continue;
            };
            stats.doodad_files += 1;
            let defs = parse_list(&text, DoodadSpawnerDef::from_json)
                .map_err(|e| format!("SpawnManager: Parse {} file: {e}", path.display()))?;
            for (entry, mut def) in defs.into_iter().enumerate() {
                stats.doodad_entries += 1;
                let p = def.base.position;
                if stored.get(&def.base.unit_id).is_some_and(|v| {
                    v.iter().any(|&(x, y, z)| {
                        (x - p.x).abs() < 0.01 && (y - p.y).abs() < 0.01 && (z - p.z).abs() < 0.01
                    })
                }) {
                    stats.doodad_duplicates += 1;
                    continue;
                }
                if !(exists.doodad)(def.base.unit_id) {
                    tracing::warn!(
                        "Doodad Template {} (file entry {}) doesn't exist - {}",
                        def.base.unit_id,
                        entry + 1,
                        path.display()
                    );
                    stats.doodad_missing_templates += 1;
                    continue;
                }
                let mut inner = self.inner();
                def.base.id = inner.next_id;
                self.fix_position(&mut def.base.position, true);
                let unit_id = def.base.unit_id;
                let key = inner.next_id;
                inner
                    .doodad_spawners
                    .insert(key, Arc::new(DoodadSpawner::new(def, Some(self.world.id))));
                inner.next_id += 1;
                stats.doodad_spawners += 1;
                stored.entry(unit_id).or_default().push((p.x, p.y, p.z));
            }
        }
        Ok(())
    }

    /// `LoadTransferSpawns` / `LoadSlaveSpawns` (not reversed, no duplicate filter).
    fn load_simple_spawns(
        &self,
        _server: &GameServer,
        world_path: &Path,
        prefix: &str,
        exists: &dyn Fn(u32) -> bool,
        stats: &mut LoadStats,
    ) -> Result<(), String> {
        let Some(files) = spawn_files(world_path, prefix) else {
            return Ok(());
        };
        let transfer = prefix.starts_with("transfer");
        for path in files {
            let Some(text) = read_spawn_file(&self.world, &path) else {
                continue;
            };
            let defs = parse_list(&text, SimpleSpawnerDef::from_json)
                .map_err(|e| format!("SpawnManager: Parse {} file: {e}", path.display()))?;
            for (entry, mut def) in defs.into_iter().enumerate() {
                if transfer {
                    stats.transfer_entries += 1;
                } else {
                    stats.slave_entries += 1;
                }
                if !exists(def.base.unit_id) {
                    tracing::warn!(
                        "{} Template {} (file entry {}) doesn't exist - {}",
                        if transfer { "Transfer" } else { "Slave" },
                        def.base.unit_id,
                        entry + 1,
                        path.display()
                    );
                    continue;
                }
                let mut inner = self.inner();
                def.base.id = inner.next_id;
                self.fix_position(&mut def.base.position, true);
                let key = inner.next_id;
                if transfer {
                    inner.transfer_spawners.insert(key, def);
                    stats.transfer_spawners += 1;
                } else {
                    inner.slave_spawners.insert(key, def);
                    stats.slave_spawners += 1;
                }
                inner.next_id += 1;
            }
        }
        Ok(())
    }

    /// `LoadGimmickSpawns` (no `DegToRad`; `UnitId == 0` is accepted).
    fn load_gimmick_spawns(
        &self,
        _server: &GameServer,
        world_path: &Path,
        exists: &TemplateExists<'_>,
        stats: &mut LoadStats,
    ) -> Result<(), String> {
        let Some(files) = spawn_files(world_path, "gimmick_spawns") else {
            return Ok(());
        };
        for path in files {
            let Some(text) = read_spawn_file(&self.world, &path) else {
                continue;
            };
            let defs = crate::systems::npcs::json::parse(&text)
                .map_err(|e| e.to_string())
                .and_then(|doc| parse_gimmick_spawners(&doc))
                .map_err(|e| format!("SpawnManager: Parse {} file: {e}", path.display()))?;
            for (entry, mut def) in defs.into_iter().enumerate() {
                stats.gimmick_entries += 1;
                if def.unit_id != 0 && !(exists.gimmick)(def.unit_id) {
                    tracing::error!(
                        "Gimmick Template {} (file entry {}) doesn't exist - {}",
                        def.unit_id,
                        entry + 1,
                        path.display()
                    );
                    continue;
                }
                let Some(position) = def.position.as_mut() else {
                    // C#: NullReferenceException on `spawner.Position`.
                    return Err(format!(
                        "SpawnManager: {} entry {}: Position is null",
                        path.display(),
                        entry + 1
                    ));
                };
                self.fix_position(position, false);
                let mut inner = self.inner();
                def.id = inner.next_id;
                let key = inner.next_id;
                inner.gimmick_spawners.insert(key, def);
                inner.next_id += 1;
                stats.gimmick_spawners += 1;
            }
        }
        Ok(())
    }
}
