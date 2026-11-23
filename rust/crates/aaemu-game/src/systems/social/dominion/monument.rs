use super::{DominionState, GameServer};

/// Writes only the national monument columns of one territory (zeroes clear it). The full row
/// stays with the periodic financial save.
pub(crate) async fn persist_national_monument(
    server: &GameServer,
    zone_group_id: u32,
    db_id: i64,
    x: f32,
    y: f32,
    z: f32,
) -> sqlx::Result<()> {
    let _save = server.save.transaction_lock().await;
    crate::systems::housing::dominion::db::write_national_monument(
        &server.db,
        zone_group_id,
        db_id,
        x,
        y,
        z,
    )
    .await?;
    Ok(())
}

/// A territory changed owner (siege capture, re-claim) or was released: announce that the old
/// statue is gone, then zero the monument columns and delete the doodad row. The authority
/// already cleared the in-memory fields. The zero update precedes the doodad delete, so a
/// crash between them leaves an orphan doodad, not a dominion pointing at a missing one.
pub(crate) fn clear_national_monument_on_owner_change(
    server: &GameServer,
    previous: &DominionState,
    current: &DominionState,
) {
    let old_db_id = previous.national_monument_db_id;
    if old_db_id == 0 {
        return;
    }
    if let Ok(zone) = u16::try_from(current.zone_group_id) {
        server
            .world
            .broadcast_to_server(&super::packets::misc::SCNationalMonumentChangedPacket {
                id: zone,
                national_monument_db_id: 0,
                x: 0.0,
                y: 0.0,
                z: 0.0,
            });
    }
    let instance_id = current.tower.instance_id;
    let old_db_id = u32::try_from(old_db_id.max(0)).unwrap_or(0);
    let Some(server) = server.self_arc() else {
        // Unit tests have no shared handle: remove the visible doodad and let the DB writer
        // queue its row delete (the state is already cleared in memory).
        if let Some(world) = server.world.world(instance_id) {
            if let Some(doodad) = world.doodad_by_db_id(old_db_id) {
                crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
                return;
            }
        }
        crate::systems::npcs::doodad::db::delete_row(server, old_db_id);
        return;
    };
    let zone_group_id = current.zone_group_id;
    tokio::spawn(async move {
        if let Err(error) =
            persist_national_monument(&server, zone_group_id, 0, 0.0, 0.0, 0.0).await
        {
            tracing::error!(
                "Clearing the national monument of zone {zone_group_id} failed: {error}"
            );
            return;
        }
        if let Some(world) = server.world.world(instance_id) {
            if let Some(doodad) = world.doodad_by_db_id(old_db_id) {
                crate::systems::npcs::doodad::manager::delete(&server, &world, &doodad);
                return;
            }
        }
        crate::systems::npcs::doodad::db::delete_row(&server, old_db_id);
    });
}
