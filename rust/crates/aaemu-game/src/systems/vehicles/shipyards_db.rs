//! Persistence of the shipyards (drydocks): the `shipyards` table.
//!
//! The C# server kept shipyards in memory only, so a restart lost every drydock with its build
//! progress (and the materials the owner had paid). Every shipyard is written by the regular
//! save (`System::save`, also the shutdown save), its row is deleted when it is removed
//! (finished ship, death / decay), and `System::start` restores the rows with a fresh object id
//! (loading never consumes items or money).

use sqlx::Row;

use crate::models::shipyard::{Shipyard, ShipyardData};
use crate::server::GameServer;
use crate::world::{Entity, ShipyardRef};

use super::shipyards::{build_shipyard, set_current_step, spawn_shipyard, update_shipyard_info};

/// One `shipyards` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ShipyardRow {
    pub id: u32,
    pub template_id: u32,
    pub owner_id: u32,
    pub owner_name: String,
    pub faction: u32,
    pub instance_id: u32,
    pub zone_id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub rot_x: f32,
    pub rot_y: f32,
    pub z_rot: f32,
    pub money_amount: i32,
    pub actions: i32,
    pub step: i32,
    pub current_step: i32,
    pub num_action: i32,
    pub hp: i32,
    /// `ShipyardData.Hp`.
    pub data_hp: i32,
    /// Unix seconds UTC (drives tax protection / decay).
    pub spawned: i64,
}

/// The row of a live shipyard.
pub fn snapshot(shipyard: &ShipyardRef) -> ShipyardRow {
    let s = shipyard.data();
    let t = &s.unit.transform;
    ShipyardRow {
        id: s.data.id as u32,
        template_id: s.data.template_id,
        owner_id: s.data.kind2,
        owner_name: s.data.owner_name.clone(),
        faction: s.data.kind3,
        instance_id: t.instance_id,
        zone_id: t.zone_id,
        x: s.data.x,
        y: s.data.y,
        z: s.data.z,
        rot_x: t.rot_x,
        rot_y: t.rot_y,
        z_rot: s.data.z_rot,
        money_amount: s.data.money_amount,
        actions: s.data.actions,
        step: s.data.step,
        current_step: s.current_step,
        num_action: s.num_action,
        hp: s.unit.hp,
        data_hp: s.data.hp,
        spawned: s.data.spawned,
    }
}

/// Rebuilds the shipyard of a row (fresh object id, no items / money are touched). `None` when
/// the template or its step is gone from the game data.
pub fn restore(server: &GameServer, row: &ShipyardRow) -> Option<ShipyardRef> {
    let template = server
        .game_data
        .systems
        .vehicles
        .shipyard_templates
        .get(&row.template_id)?
        .clone();
    let step_tpl = template.step(row.current_step.max(0)).copied()?;
    let obj_id = server.ids.object.get_next_id().ok()?;
    let data = ShipyardData {
        id: u64::from(row.id),
        template_id: row.template_id,
        x: row.x,
        y: row.y,
        z: row.z,
        z_rot: row.z_rot,
        money_amount: row.money_amount,
        actions: row.actions,
        kind: template.origin_item_id,
        kind2: row.owner_id,
        owner_name: row.owner_name.clone(),
        kind3: row.faction,
        spawned: row.spawned,
        obj_id,
        hp: row.data_hp,
        step: row.step,
    };
    let mut sy: Shipyard = build_shipyard(
        server,
        &template,
        data,
        &step_tpl,
        (row.instance_id, row.zone_id),
        (row.rot_x, row.rot_y),
    );
    set_current_step(server, &mut sy, row.current_step);
    sy.num_action = row.num_action;
    sy.is_dirty = false;
    sy.unit.hp = row.hp.min(sy.unit.max_hp);
    Some(Entity::new(obj_id, sy))
}

const COLUMNS: &str =
    "`id`,`template_id`,`owner_id`,`owner_name`,`faction`,`instance_id`,`zone_id`,\
`x`,`y`,`z`,`rot_x`,`rot_y`,`z_rot`,`money_amount`,`actions`,`step`,`current_step`,`num_action`,\
`hp`,`data_hp`,`spawned`";

/// `REPLACE INTO shipyards ...`.
pub async fn write_row(conn: &mut sqlx::MySqlConnection, r: &ShipyardRow) -> sqlx::Result<()> {
    sqlx::query(&format!(
        "REPLACE INTO shipyards({COLUMNS}) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
    ))
    .bind(r.id)
    .bind(r.template_id)
    .bind(r.owner_id)
    .bind(&r.owner_name)
    .bind(r.faction)
    .bind(r.instance_id)
    .bind(r.zone_id)
    .bind(r.x)
    .bind(r.y)
    .bind(r.z)
    .bind(r.rot_x)
    .bind(r.rot_y)
    .bind(r.z_rot)
    .bind(r.money_amount)
    .bind(r.actions)
    .bind(r.step)
    .bind(r.current_step)
    .bind(r.num_action)
    .bind(r.hp)
    .bind(r.data_hp)
    .bind(r.spawned)
    .execute(conn)
    .await?;
    Ok(())
}

/// All rows of the table.
pub async fn read_rows(conn: &mut sqlx::MySqlConnection) -> sqlx::Result<Vec<ShipyardRow>> {
    let rows = sqlx::query(
        "SELECT CAST(id AS UNSIGNED) AS id, CAST(template_id AS UNSIGNED) AS template_id, \
         CAST(owner_id AS UNSIGNED) AS owner_id, owner_name, CAST(faction AS UNSIGNED) AS faction, \
         CAST(instance_id AS UNSIGNED) AS instance_id, CAST(zone_id AS UNSIGNED) AS zone_id, \
         x, y, z, rot_x, rot_y, z_rot, money_amount, actions, step, current_step, num_action, \
         hp, data_hp, spawned FROM shipyards ORDER BY id",
    )
    .fetch_all(conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ShipyardRow {
            id: r.try_get::<u64, _>("id")? as u32,
            template_id: r.try_get::<u64, _>("template_id")? as u32,
            owner_id: r.try_get::<u64, _>("owner_id")? as u32,
            owner_name: r.try_get("owner_name")?,
            faction: r.try_get::<u64, _>("faction")? as u32,
            instance_id: r.try_get::<u64, _>("instance_id")? as u32,
            zone_id: r.try_get::<u64, _>("zone_id")? as u32,
            x: r.try_get("x")?,
            y: r.try_get("y")?,
            z: r.try_get("z")?,
            rot_x: r.try_get("rot_x")?,
            rot_y: r.try_get("rot_y")?,
            z_rot: r.try_get("z_rot")?,
            money_amount: r.try_get("money_amount")?,
            actions: r.try_get("actions")?,
            step: r.try_get("step")?,
            current_step: r.try_get("current_step")?,
            num_action: r.try_get("num_action")?,
            hp: r.try_get("hp")?,
            data_hp: r.try_get("data_hp")?,
            spawned: r.try_get("spawned")?,
        });
    }
    Ok(out)
}

/// Spawns the shipyards of `rows`; returns the number restored. Rows that cannot be restored
/// (template gone, world instance missing) are skipped and stay in the table.
pub fn restore_all(server: &GameServer, rows: &[ShipyardRow]) -> usize {
    let mut n = 0;
    for row in rows {
        if row.current_step == -1 {
            // All steps built: the ship item was granted before the ceremony removal.
            tracing::info!("Shipyard {} is finished, deleting its row", row.id);
            server.systems.vehicles.shipyards.queue_delete(row.id);
            delete_now(server, row.id);
            server.ids.shipyard.release_id(row.id);
            continue;
        }
        if server.world.world(row.instance_id).is_none() {
            tracing::warn!(
                "Shipyard {} not restored: world instance {} missing",
                row.id,
                row.instance_id
            );
            continue;
        }
        let Some(shipyard) = restore(server, row) else {
            tracing::warn!(
                "Shipyard {} not restored: template {} / step {} missing",
                row.id,
                row.template_id,
                row.current_step
            );
            continue;
        };
        spawn_shipyard(server, row.id, &shipyard);
        // Protection / decay buffs as the tick would give them.
        update_shipyard_info(server, &shipyard);
        n += 1;
    }
    n
}

/// Startup: recreates and spawns every stored shipyard. Returns the number restored.
pub async fn load_all(server: &GameServer) -> sqlx::Result<usize> {
    let mut conn = server.db.acquire().await?;
    let rows = read_rows(&mut conn).await?;
    drop(conn);
    Ok(restore_all(server, &rows))
}

/// `SaveManager.DoSave` part: every live shipyard is written, then the rows of removed ones are
/// deleted. Returns the number of rows written.
pub async fn save_all(
    server: &GameServer,
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
) -> crate::Result<usize> {
    let m = &server.systems.vehicles.shipyards;
    for id in m.take_pending_delete() {
        if let Err(e) = sqlx::query("DELETE FROM shipyards WHERE `id` = ?")
            .bind(id)
            .execute(&mut **tx)
            .await
        {
            tracing::error!("Shipyard.Delete {id}: {e}");
        }
    }
    let mut n = 0;
    for shipyard in m.all() {
        let row = snapshot(&shipyard);
        match write_row(tx, &row).await {
            Ok(()) => {
                shipyard.data().is_dirty = false;
                n += 1;
            }
            Err(e) => tracing::error!("Shipyard.Save {}: {e}", row.id),
        }
    }
    Ok(n)
}

/// Deletes the row of a removed shipyard right away (fire and forget), so a crash before the
/// next save cannot bring a finished drydock back.
pub fn delete_now(server: &GameServer, id: u32) {
    let db = server.db.clone();
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            if let Err(e) = sqlx::query("DELETE FROM shipyards WHERE `id` = ?")
                .bind(id)
                .execute(&db)
                .await
            {
                tracing::error!("Shipyard.Delete {id}: {e}");
            }
        });
    }
}
