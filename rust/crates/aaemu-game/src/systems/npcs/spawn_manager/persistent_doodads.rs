use super::*;

/// Fully decoded persistent rows. Fetching/validating this batch allocates no world objects,
/// changes no parent links and queues no persistence writes.
#[derive(Debug, Default)]
pub(crate) struct PersistentDoodads {
    rows: Vec<PreparedDoodad>,
}

#[derive(Debug)]
struct PreparedDoodad {
    row: crate::systems::npcs::doodad::db::DoodadRow,
    interaction_counts: HashMap<u32, u32>,
    /// A cargo rack whose escrow reference was stale and was reset to the authored empty
    /// state by [`PersistentDoodads::validate_slave_cargo`]; publication persists the row.
    repaired: bool,
}

/// What the item manager currently holds under the id a saved cargo rack references.
enum CargoEscrow {
    /// Exactly the System-escrow pack the rack was saved with.
    Intact,
    /// No container holds the id any more (the pack was deleted and the id may be reused).
    Gone,
    /// A single holder exists but it is not the rack's escrow pack (moved, or the id was reused
    /// by an unrelated item).
    Mismatched,
    /// Several containers hold the id: a duplication risk that must never be repaired blindly.
    Ambiguous,
}

impl PersistentDoodads {
    pub(crate) fn prepare(
        server: &GameServer,
        rows: Vec<crate::systems::npcs::doodad::db::DoodadRow>,
    ) -> anyhow::Result<Self> {
        let mut prepared = Vec::with_capacity(rows.len());
        for row in rows {
            anyhow::ensure!(row.id != 0, "persistent doodad has no database id");
            let template = server
                .game_data
                .systems
                .npcs
                .doodad
                .template(row.template_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "persistent doodad {}: template {} does not exist",
                        row.id,
                        row.template_id
                    )
                })?;
            anyhow::ensure!(
                [row.x, row.y, row.z, row.roll, row.pitch, row.yaw, row.scale]
                    .iter()
                    .all(|value| value.is_finite()),
                "persistent doodad {}: non-finite transform",
                row.id
            );
            anyhow::ensure!(
                row.current_phase_id == 0
                    || template
                        .func_groups
                        .iter()
                        .any(|group| group.id == row.current_phase_id),
                "persistent doodad {}: phase {} does not belong to template {}",
                row.id,
                row.current_phase_id,
                row.template_id
            );
            let interaction_counts = serde_json::from_str(&row.interaction_counts)?;
            prepared.push(PreparedDoodad {
                row,
                interaction_counts,
                repaired: false,
            });
        }
        Ok(Self { rows: prepared })
    }

    /// Called inside the item/publication scopes, before replacing any hull. Compare the
    /// fetched receipt, not the before/after emptiness of the hull: a store/recover ABA can
    /// leave an empty live rack while SQL has captured its former loaded reference.
    ///
    /// A rack whose escrow pack is gone or is no longer the escrowed pack (left the System
    /// container, or its id was reused by an unrelated item, as happened to the cargo of ships
    /// despawned before original-reference cleanup retained rack items) can never be
    /// satisfied. Refusing the summon for it would brick the ship forever, so the prepared
    /// row is reset to the authored empty rack (no item is granted or moved, so nothing can be
    /// duplicated) and publication persists it. References that may duplicate cargo (several
    /// holders, an intact pack shared by racks, or a live receipt mismatch) still refuse.
    /// Returns the number of racks reset.
    pub(crate) fn validate_slave_cargo(
        &mut self,
        server: &GameServer,
        hull_id: u32,
        live_hull: Option<&crate::world::SlaveRef>,
    ) -> anyhow::Result<usize> {
        use crate::systems::npcs::doodad::funcs::items::cargo_empty_phase_for;

        let mut row_ids = HashSet::new();
        let mut packs = HashSet::new();
        let mut repaired = 0;
        anyhow::ensure!(
            live_hull.is_none_or(|h| h.data().id == hull_id),
            "cargo preflight has a different live hull identity"
        );
        for prepared in &mut self.rows {
            let row = &mut prepared.row;
            anyhow::ensure!(
                row.owner_type == doodad_owner_type::SLAVE
                    && row.house_id == hull_id
                    && row_ids.insert(row.id),
                "persistent doodad batch has a duplicate or different hull identity"
            );
            let cargo = server
                .game_data
                .systems
                .npcs
                .doodad
                .doodad_func_groups(row.template_id)
                .iter()
                .any(|group| {
                    cargo_empty_phase_for(server, row.owner_type, row.template_id, group.id)
                        .is_some()
                });
            if !cargo {
                continue;
            }
            if row.item_id == 0 {
                anyhow::ensure!(
                    row.item_template_id == 0
                        && cargo_empty_phase_for(
                            server,
                            row.owner_type,
                            row.template_id,
                            row.current_phase_id
                        )
                        .is_none(),
                    "saved empty cargo rack has loaded metadata"
                );
                continue;
            }
            let loaded_phase = cargo_empty_phase_for(
                server,
                row.owner_type,
                row.template_id,
                row.current_phase_id,
            );
            let escrow = Self::cargo_escrow(server, row);
            anyhow::ensure!(
                !matches!(escrow, CargoEscrow::Ambiguous),
                "saved cargo item has multiple holders"
            );
            // A live version of this exact persistent hull must agree with the fetched
            // loaded receipt. Other inactive summon items have no live rack to compare.
            if let Some(hull) = live_hull {
                let world = server
                    .world
                    .world_of(&WorldObject::from(hull))
                    .ok_or_else(|| anyhow::anyhow!("cargo hull is no longer live"))?;
                let matched = world.all_doodads().into_iter().any(|rack| {
                    let d = rack.data();
                    !d.phase.deleted
                        && d.db_id == row.id
                        && d.owner_type == row.owner_type
                        && d.owner_id == row.owner_id
                        && d.owner_db_id == hull_id
                        && d.template_id == row.template_id
                        && d.attach_point == row.attach_point
                        && d.parent_obj_id == hull.obj_id
                        && d.transform.parent_obj_id == hull.obj_id
                        && d.func_group_id == row.current_phase_id
                        && d.item_id == row.item_id
                        && d.item_template_id == row.item_template_id
                });
                anyhow::ensure!(matched, "saved cargo receipt changed during hull load");
            }
            let reason = match escrow {
                CargoEscrow::Intact | CargoEscrow::Ambiguous => None,
                CargoEscrow::Gone => Some("the pack no longer exists"),
                CargoEscrow::Mismatched => {
                    Some("the item with that id is not the escrowed pack (moved or id reused)")
                }
            };
            if let Some(reason) = reason {
                tracing::warn!(
                    rack_db_id = row.id,
                    item_id = row.item_id,
                    item_template_id = row.item_template_id,
                    hull_db_id = hull_id,
                    attach_point = row.attach_point,
                    reason,
                    "Saved cargo rack references a missing escrow pack; restoring the rack empty"
                );
                row.item_id = 0;
                row.item_template_id = 0;
                if let Some(empty) = loaded_phase {
                    row.current_phase_id = empty;
                }
                row.phase_time = chrono::Utc::now().naive_utc();
                prepared.repaired = true;
                repaired += 1;
                continue;
            }
            anyhow::ensure!(
                loaded_phase.is_some(),
                "saved cargo item is not in its loaded phase"
            );
            anyhow::ensure!(packs.insert(row.item_id), "aliased saved cargo item");
            // System escrow alone is insufficient: the same GUID could now be stored on
            // a different live hull. Include hidden/retiring registry objects in this scan.
            for world in server.world.worlds() {
                for rack in world.all_doodads() {
                    let d = rack.data();
                    if !d.phase.deleted && d.item_id == row.item_id {
                        anyhow::ensure!(
                            live_hull.is_some_and(|h| d.parent_obj_id == h.obj_id)
                                && d.db_id == row.id
                                && d.owner_db_id == hull_id,
                            "saved cargo item is referenced by another live rack"
                        );
                    }
                }
            }
        }
        Ok(repaired)
    }

    /// Classify the item a saved cargo rack references against the live item containers.
    fn cargo_escrow(
        server: &GameServer,
        row: &crate::systems::npcs::doodad::db::DoodadRow,
    ) -> CargoEscrow {
        use crate::models::container::slot_type;
        use crate::models::item::ItemClass;

        let occurrences: usize = server
            .items
            .all_containers()
            .iter()
            .map(|container| {
                container
                    .lock()
                    .expect("container poisoned")
                    .items
                    .iter()
                    .filter(|item| item.id == row.item_id)
                    .count()
            })
            .sum();
        if occurrences > 1 {
            return CargoEscrow::Ambiguous;
        }
        let Some(holder) = server.items.holding_container(row.item_id) else {
            return CargoEscrow::Gone;
        };
        let holder = holder.lock().expect("container poisoned");
        let intact = holder.slot_type == slot_type::SYSTEM
            && holder.get_item_by_id(row.item_id).is_some_and(|item| {
                item.slot_type == slot_type::SYSTEM
                    && item.owner_id == u64::from(holder.owner_id)
                    && item.template_id == row.item_template_id
                    && item.class == ItemClass::Backpack
                    && item.count == 1
            });
        if intact {
            CargoEscrow::Intact
        } else {
            CargoEscrow::Mismatched
        }
    }
}

#[cfg(test)]
pub(crate) type PersistentDoodadLoad = dyn Fn(u8, i64) -> futures::future::BoxFuture<'static, anyhow::Result<PersistentDoodads>>
    + Send
    + Sync;

impl SpawnManager {
    // ----- persistent doodads -----

    /// Compatibility entry point for startup/housing callers. Ships preload the same batch
    /// before touching their old hull and call the synchronous publication method later.
    pub async fn spawn_persistent_doodads(
        &self,
        server: &GameServer,
        owner_type_to_spawn: u8,
        owner_to_spawn_id: i64,
        use_parent_object: Option<&WorldObject>,
        do_spawn: bool,
    ) -> anyhow::Result<usize> {
        let prepared = self
            .fetch_persistent_doodads(server, owner_type_to_spawn, owner_to_spawn_id)
            .await?;
        let _items = use_parent_object
            .and_then(WorldObject::as_slave)
            .map(|_| server.systems.economy.operation_scope());
        let _parent_scope = use_parent_object
            .and_then(WorldObject::as_slave)
            .map(|_| server.systems.vehicles.slave_summons.publication_scope());
        self.publish_persistent_doodads(server, prepared, use_parent_object, do_spawn)
    }

    pub(crate) async fn fetch_persistent_doodads(
        &self,
        server: &GameServer,
        owner_type_to_spawn: u8,
        owner_to_spawn_id: i64,
    ) -> anyhow::Result<PersistentDoodads> {
        use crate::db::{int, min_value};
        use crate::systems::npcs::doodad::db::DoodadRow;
        use sqlx::Row;
        #[cfg(test)]
        {
            let loader = server
                .systems
                .npcs
                .spawns
                .test_persistent_loader
                .lock()
                .expect("test persistent loader poisoned")
                .clone();
            if let Some(loader) = loader {
                return loader(owner_type_to_spawn, owner_to_spawn_id).await;
            }
        }
        let mut sql = String::from(
            "SELECT CAST(id AS SIGNED) AS id, CAST(template_id AS SIGNED) AS template_id, \
             CAST(current_phase_id AS SIGNED) AS current_phase_id, x, y, z, roll, pitch, yaw, scale, \
             plant_time, growth_time, phase_time, CAST(owner_id AS SIGNED) AS owner_id, \
             CAST(owner_type AS SIGNED) AS owner_type, CAST(attach_point AS SIGNED) AS attach_point, \
             CAST(item_id AS UNSIGNED) AS item_id, CAST(house_id AS SIGNED) AS house_id, \
             CAST(parent_doodad AS SIGNED) AS parent_doodad, \
             CAST(item_template_id AS SIGNED) AS item_template_id, \
             CAST(item_container_id AS UNSIGNED) AS item_container_id, CAST(data AS SIGNED) AS data, \
             CAST(farm_type AS SIGNED) AS farm_type, interaction_counts FROM doodads WHERE owner_type = ?",
        );
        if owner_to_spawn_id >= 0 {
            sql.push_str(" AND house_id = ?");
        }
        sql.push_str(" ORDER BY `plant_time`");
        let mut q = sqlx::query(&sql).bind(owner_type_to_spawn);
        if owner_to_spawn_id >= 0 {
            q = q.bind(owner_to_spawn_id);
        }
        let rows = q.fetch_all(&server.db).await?;
        let rows = rows
            .into_iter()
            .map(|r| -> anyhow::Result<DoodadRow> {
                let float = |column: &str| -> sqlx::Result<f32> { r.try_get(column) };
                let time = |column: &str| -> sqlx::Result<chrono::NaiveDateTime> {
                    Ok(r.try_get::<Option<chrono::NaiveDateTime>, _>(column)?
                        .unwrap_or_else(min_value))
                };
                Ok(DoodadRow {
                    id: int(&r, "id")? as u32,
                    owner_id: int(&r, "owner_id")? as u32,
                    owner_type: int(&r, "owner_type")? as u8,
                    attach_point: int(&r, "attach_point")? as u8,
                    template_id: int(&r, "template_id")? as u32,
                    current_phase_id: int(&r, "current_phase_id")? as u32,
                    plant_time: time("plant_time")?,
                    growth_time: time("growth_time")?,
                    phase_time: time("phase_time")?,
                    x: float("x")?,
                    y: float("y")?,
                    z: float("z")?,
                    roll: float("roll")?,
                    pitch: float("pitch")?,
                    yaw: float("yaw")?,
                    scale: float("scale")?,
                    item_id: r.try_get("item_id")?,
                    house_id: int(&r, "house_id")? as u32,
                    parent_doodad: int(&r, "parent_doodad")? as u32,
                    item_template_id: int(&r, "item_template_id")? as u32,
                    item_container_id: r.try_get("item_container_id")?,
                    data: int(&r, "data")? as i32,
                    farm_type: int(&r, "farm_type")? as u8,
                    interaction_counts: r
                        .try_get::<Option<String>, _>("interaction_counts")?
                        .unwrap_or_else(|| "{}".into()),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        PersistentDoodads::prepare(server, rows)
    }

    /// Caller coordinates parent removal for the whole synchronous publication. The pointer
    /// check rejects batches fetched for an earlier generation of a reused parent object id.
    pub(crate) fn publish_persistent_doodads(
        &self,
        server: &GameServer,
        prepared: PersistentDoodads,
        use_parent_object: Option<&WorldObject>,
        do_spawn: bool,
    ) -> anyhow::Result<usize> {
        use crate::db::unix;
        use crate::systems::npcs::doodad::db::DoodadRow;
        if let Some(parent) = use_parent_object {
            anyhow::ensure!(
                self.world
                    .object(parent.obj_id())
                    .is_some_and(|live| std::ptr::eq(live.core(), parent.core())),
                "persistent doodad parent is no longer live"
            );
            if let Some(slave) = parent.as_slave() {
                let db_id = slave.data().id;
                anyhow::ensure!(
                    prepared
                        .rows
                        .iter()
                        .all(|d| d.row.owner_type == doodad_owner_type::SLAVE
                            && d.row.house_id == db_id),
                    "persistent doodad batch does not belong to this hull"
                );
            }
        }
        let mut count = 0;
        let mut new_coffers = Vec::new();
        for PreparedDoodad {
            row,
            interaction_counts,
            repaired,
        } in prepared.rows
        {
            let DoodadRow {
                id: db_id,
                owner_id,
                owner_type,
                attach_point,
                template_id,
                current_phase_id: phase_id,
                plant_time,
                growth_time,
                phase_time,
                x,
                y,
                z,
                roll,
                pitch,
                yaw,
                scale,
                item_id,
                house_id,
                parent_doodad,
                item_template_id,
                item_container_id,
                data,
                farm_type,
                interaction_counts: _,
            } = row;
            let Some(doodad) =
                backend::doodad_create(server, &self.world, 0, template_id, None, true)
            else {
                // C#: NullReferenceException on the null doodad.
                anyhow::bail!("persistent doodad {db_id}: template {template_id} does not exist");
            };
            forget_object_fields(server, doodad.obj_id);
            let (item_tpl, ucc) = server
                .systems
                .npcs
                .hooks
                .economy
                .get()
                .and_then(|h| h.item_template_and_ucc(server, item_id))
                .unwrap_or((item_template_id, 0));
            {
                let mut d = doodad.data();
                d.db_id = db_id;
                d.owner_id = owner_id;
                d.owner_type = owner_type;
                d.attach_point = attach_point;
                d.plant_time = unix(Some(plant_time));
                d.growth_time_ms = growth_time.and_utc().timestamp_millis().max(0);
                d.item_id = item_id;
                d.owner_db_id = house_id;
                d.scale = if scale != 0.0 { scale } else { 1.0 };
                d.item_template_id = item_tpl;
                d.ucc_id = ucc;
                d.data = data;
                d.farm_type = farm_type;
            }
            // `FuncGroupId = phaseId` (setter: PhaseTime = now, Save, phase caches), then
            // `OverridePhaseTime = PhaseTime = phase_time`.
            crate::systems::npcs::doodad::manager::set_func_group_id(server, &doodad, phase_id);
            {
                let pt = phase_time.and_utc().timestamp_millis().max(0);
                let mut d = doodad.data();
                d.phase.override_phase_time_ms = pt;
                d.phase_time_ms = pt;
                d.phase.interaction_counts = interaction_counts;
            }
            let doodad_obj = WorldObject::from(&doodad);
            let mut has_parent = false;
            if parent_doodad > 0 {
                let parent = self
                    .inner()
                    .player_doodads
                    .iter()
                    .find(|d| d.data().db_id == parent_doodad)
                    .cloned();
                match parent {
                    None => tracing::warn!(
                        "Unable to place doodad {db_id} can't find it's parent doodad {parent_doodad}"
                    ),
                    Some(p) => {
                        doodad.data().parent_obj_id = p.obj_id;
                        self.world.attach_child(&WorldObject::from(&p), &doodad_obj);
                        has_parent = true;
                    }
                }
            }
            // For SLAVE rows `house_id` is the vehicle hull id, not a house: they are attached to
            // the slave through `use_parent_object` below, never looked up in housing.
            if house_id > 0 && !has_parent && owner_type != doodad_owner_type::SLAVE {
                // HousingManager.GetHouseById(OwnerDbId): parent + bound doodad registration
                // (UsePersistentHouseDoodads).
                if !crate::systems::housing::attach_loaded_doodad(
                    server,
                    &self.world,
                    house_id,
                    &doodad,
                ) {
                    tracing::warn!(
                        "Unable to place doodad {db_id} can't find it's owning house {house_id}"
                    );
                }
            }
            if let Some(p) = use_parent_object {
                doodad.data().parent_obj_id = p.obj_id();
                self.world.attach_child(p, &doodad_obj);
            }
            doodad
                .data()
                .transform
                .set_position(x, y, z, roll, pitch, yaw);
            // Attach ItemContainer to coffer if needed
            if doodad.data().coffer.is_some() {
                if item_container_id > 0 {
                    let is_coffer_container = server
                        .items
                        .container_by_id(item_container_id)
                        .is_some_and(|c| {
                            c.lock().expect("container poisoned").kind
                                == crate::models::container::ContainerKind::Coffer
                        });
                    if is_coffer_container {
                        if let Some(c) = doodad.data().coffer.as_mut() {
                            c.container_id = item_container_id;
                        }
                    } else {
                        tracing::error!(
                            "Unable to attach ItemContainer {item_container_id} to DoodadCoffer, objId: {}, DbId: {db_id}",
                            doodad.obj_id
                        );
                    }
                } else {
                    tracing::warn!(
                        "DoodadCoffer has no persistent ItemContainer assigned to it, creating new one, objId: {}, DbId: {db_id}",
                        doodad.obj_id
                    );
                    if let Some(h) = server.systems.npcs.hooks.economy.get() {
                        h.initialize_coffer(server, &doodad, owner_id);
                    }
                    // Mark for saving again later when we're done with this loop
                    new_coffers.push(doodad.clone());
                }
            }
            // Slave AttachedDoodads
            if owner_type == doodad_owner_type::SLAVE {
                if let Some(slave) = use_parent_object.and_then(WorldObject::as_slave) {
                    let mut s = slave.data();
                    if !s.attached_doodads.contains(&doodad.obj_id) {
                        s.attached_doodads.push(doodad.obj_id);
                    }
                }
            }
            // Only enable saving after the phase, counters, parent and position are restored.
            doodad.data().is_persistent = true;
            if repaired {
                // A cargo rack reset by `validate_slave_cargo`: the cleared reference must
                // reach the database, or the stale row would be repaired on every summon.
                crate::systems::npcs::doodad::db::save(server, &doodad);
            }
            if do_spawn {
                backend::doodad_spawn(server, &self.world, &doodad);
                crate::systems::npcs::doodad::manager::init_doodad(server, &self.world, &doodad);
            }
            if !doodad.data().phase.deleted {
                self.inner().player_doodads.push(doodad.clone());
                count += 1;
            }
        }
        // Save Coffer Doodads that had a new ItemContainer created for them
        for coffer in new_coffers {
            crate::systems::npcs::doodad::db::save(server, &coffer);
        }
        Ok(count)
    }

    /// `GetPlayerDoodads(charId)`.
    pub fn get_player_doodads(&self, char_id: u32) -> Vec<DoodadRef> {
        self.inner()
            .player_doodads
            .iter()
            .filter(|d| d.data().owner_id == char_id)
            .cloned()
            .collect()
    }

    /// `GetAllPlayerDoodads()`.
    pub fn get_all_player_doodads(&self) -> Vec<DoodadRef> {
        self.inner().player_doodads.clone()
    }

    /// `RemovePlayerDoodad(doodad)`.
    pub fn remove_player_doodad(&self, doodad: &DoodadRef) {
        let mut inner = self.inner();
        if let Some(i) = inner
            .player_doodads
            .iter()
            .position(|d| Arc::ptr_eq(d, doodad))
        {
            inner.player_doodads.remove(i);
        }
    }

    /// `AddPlayerDoodad(doodad)`.
    pub fn add_player_doodad(&self, doodad: DoodadRef) {
        self.inner().player_doodads.push(doodad);
    }
}
