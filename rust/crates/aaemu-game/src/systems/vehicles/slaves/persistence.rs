use std::sync::Arc;

use sqlx::Row;

use crate::models::doodad::doodad_owner_type;
use crate::models::item::Item;
use crate::server::GameServer;
use crate::systems::npcs::spawn_manager;
use crate::world::{PlayerRef, SlaveRef, WorldInstance, WorldObject};

use super::{all_slaves, delete, owned_roots, SummonKey};

/// The `slaves` row of a previously saved player vehicle (`load_saved_slave`).
#[derive(Debug, Clone, PartialEq)]
pub struct SavedSlave {
    pub id: u32,
    pub name: String,
    pub hp: i32,
    pub mp: i32,
}

/// A saved child slave row (`owner_type = 2`), by attach point.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedChildSlave {
    pub id: u32,
    pub template_id: u32,
    pub attach_point: u8,
    pub name: String,
    pub hp: i32,
    pub mp: i32,
}

#[derive(Debug, Default)]
pub(crate) struct LoadedHull {
    pub(crate) saved: Option<SavedSlave>,
    pub(crate) children: Vec<SavedChildSlave>,
    pub(crate) doodads: spawn_manager::PersistentDoodads,
}

pub(super) async fn load_hull(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    owner_id: u32,
    item: &Item,
    template_id: u32,
    flush_items: &[u64],
) -> anyhow::Result<LoadedHull> {
    #[cfg(test)]
    {
        let loader = server
            .systems
            .vehicles
            .slave_summons
            .test_loader
            .lock()
            .expect("test slave loader poisoned")
            .clone();
        if let Some(loader) = loader {
            let loaded = (loader.0)(owner_id, item.id).await?;
            return load_hull_doodads(server, world, loaded).await;
        }
    }
    let _transaction = server.save.transaction_lock().await;
    for item_id in flush_items.iter().copied().filter(|id| *id != 0) {
        flush_pending_save(server, item_id).await?;
    }
    server.systems.npcs.doodads.persistence.flush().await?;
    let saved = load_saved_slave(
        server,
        owner_id,
        item.id,
        item.details.slave_db_id,
        template_id,
    )
    .await?;
    let children = match &saved {
        Some(saved) => load_saved_children(server, saved.id, owner_id).await?,
        None => Vec::new(),
    };
    load_hull_doodads(
        server,
        world,
        LoadedHull {
            saved,
            children,
            ..Default::default()
        },
    )
    .await
}

async fn load_hull_doodads(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    mut loaded: LoadedHull,
) -> anyhow::Result<LoadedHull> {
    if let Some(saved) = &loaded.saved {
        loaded.doodads = server
            .systems
            .npcs
            .spawns
            .get_or_create(world)
            .fetch_persistent_doodads(server, doodad_owner_type::SLAVE, i64::from(saved.id))
            .await?;
    }
    Ok(loaded)
}

async fn load_saved_slave(
    server: &GameServer,
    owner_id: u32,
    item_id: u64,
    preferred_id: u32,
    template_id: u32,
) -> sqlx::Result<Option<SavedSlave>> {
    let r = sqlx::query(
        "SELECT CAST(id AS UNSIGNED) AS id, CAST(template_id AS UNSIGNED) AS template_id, name, CAST(hp AS SIGNED) AS hp, CAST(mp AS SIGNED) AS mp \
         FROM slaves WHERE (owner_type = 0) AND (owner_id = ?) AND (summoner = ?) AND (item_id = ?) \
         ORDER BY (id = ?) DESC, id LIMIT 1",
    )
    .bind(owner_id)
    .bind(owner_id)
    .bind(item_id)
    .bind(preferred_id)
    .fetch_optional(&server.db)
    .await?;
    r.map(|r| {
        if r.try_get::<Option<u64>, _>("template_id")? != Some(u64::from(template_id)) {
            return Err(sqlx::Error::Protocol(
                "saved hull does not match its summon item template".into(),
            ));
        }
        Ok(SavedSlave {
            id: r.try_get::<u64, _>("id")? as u32,
            name: r.try_get::<Option<String>, _>("name")?.unwrap_or_default(),
            hp: r.try_get::<Option<i64>, _>("hp")?.unwrap_or(0) as i32,
            mp: r.try_get::<Option<i64>, _>("mp")?.unwrap_or(0) as i32,
        })
    })
    .transpose()
}

async fn load_saved_children(
    server: &GameServer,
    parent_db_id: u32,
    summoner_id: u32,
) -> sqlx::Result<Vec<SavedChildSlave>> {
    let rows = sqlx::query(
        "SELECT CAST(id AS UNSIGNED) AS id, CAST(template_id AS UNSIGNED) AS template_id, \
         CAST(attach_point AS SIGNED) AS attach_point, name, CAST(hp AS SIGNED) AS hp, \
         CAST(mp AS SIGNED) AS mp FROM slaves WHERE (owner_type = 2) AND (owner_id = ?) AND (summoner = ?)",
    )
    .bind(parent_db_id)
    .bind(summoner_id)
    .fetch_all(&server.db)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(SavedChildSlave {
                id: r.try_get::<u64, _>("id")? as u32,
                template_id: r.try_get::<Option<u64>, _>("template_id")?.unwrap_or(0) as u32,
                attach_point: r.try_get::<Option<i64>, _>("attach_point")?.unwrap_or(0) as u8,
                name: r.try_get::<Option<String>, _>("name")?.unwrap_or_default(),
                hp: r.try_get::<Option<i64>, _>("hp")?.unwrap_or(0) as i32,
                mp: r.try_get::<Option<i64>, _>("mp")?.unwrap_or(0) as i32,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------------------------

/// The values of one `REPLACE INTO slaves` row.
#[derive(Debug, Clone, PartialEq)]
pub struct SlaveRow {
    pub id: u32,
    pub item_id: u64,
    pub template_id: u32,
    pub attach_point: i8,
    pub name: String,
    pub owner_type: u8,
    pub owner_id: u32,
    pub summoner: u32,
    pub hp: i32,
    pub mp: i32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// The row of a slave (`None` when `Id <= 0`), plus its children's rows.
pub fn snapshot(server: &GameServer, slave: &SlaveRef) -> Vec<SlaveRow> {
    let obj = WorldObject::from(slave);
    let pos = server
        .world
        .world_of(&obj)
        .map(|w| w.world_pos_rot(&obj))
        .unwrap_or_else(|| slave.data().unit.transform.local());
    let (row, children) = {
        let d = slave.data();
        if d.id == 0 {
            return Vec::new();
        }
        (
            SlaveRow {
                id: d.id,
                item_id: d.summoning_item_id,
                template_id: d.unit.template_id,
                attach_point: d.attach_point_id,
                name: d.unit.name.clone(),
                owner_type: d.owner_type as u8,
                owner_id: d.unit.owner_id,
                summoner: d.summoner_id,
                hp: d.unit.hp,
                mp: d.unit.mp,
                x: pos.x,
                y: pos.y,
                z: pos.z,
            },
            d.attached_slaves.clone(),
        )
    };
    let mut out = vec![row];
    if let Some(world) = server.world.world_of(&obj) {
        for c in children {
            if let Some(cs) = world.slave(c) {
                out.extend(snapshot(server, &cs));
            }
        }
    }
    out
}

/// `REPLACE INTO slaves ...`.
pub async fn write_row(conn: &mut sqlx::MySqlConnection, r: &SlaveRow) -> sqlx::Result<()> {
    sqlx::query(
        "REPLACE INTO slaves(`id`,`item_id`,`template_id`,`attach_point`,`name`,`owner_type`,`owner_id`,`summoner`,`updated_at`,`hp`,`mp`,`x`,`y`,`z`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(r.id)
    .bind(r.item_id)
    .bind(r.template_id)
    .bind(r.attach_point)
    .bind(&r.name)
    .bind(r.owner_type)
    .bind(r.owner_id)
    .bind(r.summoner)
    .bind(chrono::Utc::now().naive_utc())
    .bind(r.hp)
    .bind(r.mp)
    .bind(r.x)
    .bind(r.y)
    .bind(r.z)
    .execute(conn)
    .await?;
    Ok(())
}

/// `Slave.Save()` (new connection, fire and forget): nothing for `Id <= 0` or without a
/// summoning item.
pub fn save(server: &GameServer, slave: &SlaveRef) {
    if slave.data().summoning_item_id == 0 {
        return;
    }
    let rows = server
        .systems
        .vehicles
        .slave_summons
        .queue_save(server, slave);
    if rows.is_empty() {
        return;
    }
    let item_id = rows[0].item_id;
    let Some(server) = server.self_arc() else {
        return;
    };
    let fut = async move {
        let _transaction = server.save.transaction_lock().await;
        if let Err(e) = flush_pending_save(&server, item_id).await {
            tracing::error!("Slave.Save: {e}");
        }
    };
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(fut);
    }
}

/// Caller owns SaveManager's transaction lock. A failed write stays queued for retry; a
/// later summon must flush it before SELECT, never interpret an unfinished save as absence.
async fn flush_pending_save(server: &GameServer, item_id: u64) -> sqlx::Result<()> {
    let pending = &server.systems.vehicles.slave_summons.pending_saves;
    loop {
        let rows = pending
            .lock()
            .expect("slave saves poisoned")
            .get(&item_id)
            .cloned();
        let Some(rows) = rows else { return Ok(()) };
        let mut tx = server.db.begin().await?;
        for row in rows.iter() {
            write_row(&mut tx, row).await?;
        }
        tx.commit().await?;
        let flushed = {
            let mut pending = pending.lock().expect("slave saves poisoned");
            let current = pending
                .get(&item_id)
                .is_some_and(|current| Arc::ptr_eq(current, &rows));
            if current {
                pending.remove(&item_id);
            }
            current
        };
        if flushed {
            return Ok(());
        }
    }
}

/// The `SaveManager.DoSave` slave part: `slave.Save(connection, transaction)` for every slave
/// of every world (children are written again by their parent, as in C#). Returns the number
/// of slaves saved.
pub async fn save_all(
    server: &GameServer,
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
) -> crate::Result<usize> {
    let mut n = 0;
    for world in server.world.worlds() {
        for slave in all_slaves(&world) {
            let rows = server
                .systems
                .vehicles
                .slave_summons
                .queue_save(server, &slave);
            if rows.is_empty() {
                continue;
            }
            let mut ok = true;
            for (i, r) in rows.iter().enumerate() {
                if let Err(e) = write_row(tx, r).await {
                    tracing::error!("{e}");
                    if i == 0 {
                        ok = false;
                    }
                }
            }
            if ok {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// `OnDeleteSlaveItem(summonSlaveItem)` (called by the item code when a summon item is
/// destroyed): despawns the active slave and deletes its rows.
pub async fn on_delete_slave_item(
    server: &GameServer,
    owner: &PlayerRef,
    slave_db_id: u32,
) -> bool {
    if slave_db_id == 0 {
        return false;
    }
    let summons = &server.systems.vehicles.slave_summons;
    let _owner = summons
        .reservation(SummonKey::Owner(owner.character_id))
        .lock_owned()
        .await;
    for (world, slave) in owned_roots(server, owner.character_id) {
        if slave.data().id == slave_db_id {
            delete(server, Some(owner), world.id, slave.obj_id, true);
        }
    }
    // Queued fire-and-forget saves must not resurrect the row after item destruction.
    let _transaction = server.save.transaction_lock().await;
    summons
        .pending_saves
        .lock()
        .expect("slave saves poisoned")
        .retain(|_, rows| rows.first().is_none_or(|row| row.id != slave_db_id));
    let mut conn = match server.db.acquire().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return false;
        }
    };
    delete_slave_by_id(server, &mut conn, slave_db_id).await
}

/// `DeleteSlaveById(connection, transaction, dbId)`: the row, its child doodads and (recursive)
/// child slaves.
pub fn delete_slave_by_id<'a>(
    server: &'a GameServer,
    conn: &'a mut sqlx::MySqlConnection,
    db_id: u32,
) -> futures::future::BoxFuture<'a, bool> {
    Box::pin(async move {
        let delete_count = match sqlx::query("DELETE FROM slaves WHERE `id` = ?")
            .bind(db_id)
            .execute(&mut *conn)
            .await
        {
            Ok(r) => r.rows_affected(),
            Err(e) => {
                tracing::error!("{e}");
                0
            }
        };
        let doodads: Vec<u32> = sqlx::query(
            "SELECT CAST(id AS UNSIGNED) AS id FROM doodads WHERE (owner_type = 2) AND (house_id = ?)",
        )
        .bind(db_id)
        .fetch_all(&mut *conn)
        .await
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.try_get::<u64, _>("id").ok().map(|v| v as u32))
                .collect()
        })
        .unwrap_or_default();
        let children: Vec<u32> = sqlx::query(
            "SELECT CAST(id AS UNSIGNED) AS id FROM slaves WHERE (owner_type = 2) AND (owner_id = ?)",
        )
        .bind(db_id)
        .fetch_all(&mut *conn)
        .await
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.try_get::<u64, _>("id").ok().map(|v| v as u32))
                .collect()
        })
        .unwrap_or_default();
        for d in doodads {
            if let Err(e) =
                crate::systems::npcs::doodad::db::delete_doodad_by_id(server, &mut *conn, d).await
            {
                tracing::error!("{e}");
            }
        }
        for c in children {
            delete_slave_by_id(server, conn, c).await;
        }
        if delete_count == 0 {
            tracing::error!("Slave could not be deleted or did not exist, Id {db_id}");
            return false;
        }
        server.ids.character.release_id(db_id);
        true
    })
}
