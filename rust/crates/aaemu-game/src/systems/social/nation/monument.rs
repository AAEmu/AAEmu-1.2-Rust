use super::{
    error_message, expedition_top_level_faction, GameServer, PlayerRef, PoisonError, WorldObject,
};

// ---- National monument (stage 4) ----

/// The 1.2 territory-statue skills 23681..23688 / 23682 carry `max_range` 20 (compact
/// `skills`); the placement skill's exact server rule is capture-pending (RENAME-9).
pub const NATIONAL_MONUMENT_RANGE: f32 = 20.0;

fn monument_distance(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn monument_error(error: crate::systems::housing::dominion::state::DominionError) -> i16 {
    use crate::systems::housing::dominion::state::DominionError;
    match error {
        DominionError::NoMoreNationalMonument => error_message::NO_MORE_NATIONAL_MONUMENT,
        DominionError::NationalMonumentDuringSiege => {
            error_message::CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD
        }
        _ => error_message::ONLY_NATION_OWNER,
    }
}

fn broadcast_monument(
    server: &GameServer,
    state: &crate::systems::housing::dominion::state::DominionState,
) {
    let Ok(zone) = u16::try_from(state.zone_group_id) else {
        return;
    };
    server
        .world
        .broadcast_to_server(&super::packets::misc::SCNationalMonumentChangedPacket {
            id: zone,
            national_monument_db_id: state.national_monument_db_id,
            x: state.national_monument_x,
            y: state.national_monument_y,
            z: state.national_monument_z,
        });
}

/// The king of a dominion's nation places its national monument: the territory statue doodad
/// named by `siege_zones.monument_doodad_id` (7229..7234 for the six 1.2 castle zone groups;
/// the doodad names match the territory-statue skills). The gates use the client's own error
/// ids: 748 not a nation king, 757 not the king of this dominion or not at the point, 750 the
/// dominion already has one / has no statue template, 751 while a siege is scheduled or
/// active. The placement trigger (skills 23681..23688) and its cost are capture-pending, so
/// the GM hook and tests call this service (RENAME-9).
pub async fn place_national_monument(
    server: &GameServer,
    king: &PlayerRef,
    zone: u16,
    x: f32,
    y: f32,
    z: f32,
) -> std::result::Result<(), i16> {
    if !server.config.player_nations_enabled() {
        return Err(error_message::ONLY_NATION_OWNER);
    }
    if ![x, y, z].iter().all(|v| v.is_finite()) {
        return Err(error_message::ONLY_NATION_OWNER);
    }
    // Phase 1: validate and spawn the statue while holding the housing operation lock.
    let (state, world, doodad) = {
        let _items = server.systems.economy.operation_scope();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        if state.owner_expedition_id == 0 {
            return Err(error_message::ONLY_NATION_OWNER);
        }
        if expedition_top_level_faction(server, state.owner_expedition_id) != Some(nation.id) {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        if state.national_monument_db_id != 0 {
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        }
        if state.siege.as_ref().is_some_and(|siege| {
            siege.status != crate::systems::housing::dominion::state::SiegeStatus::Finished
        }) {
            return Err(error_message::CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD);
        }
        let Some(world) = server.world.world(state.tower.instance_id) else {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        };
        let player = WorldObject::from(king);
        if server.world.world_of(&player).map(|w| w.id) != Some(world.id) {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let here = world.world_pos_rot(&player);
        if monument_distance((here.x, here.y, here.z), (x, y, z)) > NATIONAL_MONUMENT_RANGE {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let zone_key = world.zone_id_at(x, y);
        if server
            .game_data
            .zones
            .zone_by_key(zone_key)
            .map(|zone| zone.group_id)
            != Some(state.zone_group_id)
        {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let Some(siege_zone) = server
            .game_data
            .systems
            .housing
            .castles
            .siege_zones
            .get(&u32::from(zone))
        else {
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        };
        if siege_zone.monument_doodad_id == 0 {
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        }
        let Some(doodad) = crate::systems::npcs::doodad::manager::create(
            server,
            &world,
            0,
            siege_zone.monument_doodad_id,
            None,
            false,
        ) else {
            return Err(error_message::FACTION_CREATE);
        };
        {
            let mut data = doodad.data();
            data.transform.set_position(x, y, z, 0.0, 0.0, 0.0);
            data.transform.zone_id = zone_key;
            data.is_persistent = true;
        }
        crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
        (state, world, doodad)
    };
    // Phase 2: write the statue row before anything can point at it (RENAME-9). The dominion
    // row is written later, so a crash leaves an orphan doodad, never a dangling db id. Unit
    // tests have no database handle (`self_arc` is None); the in-memory writer's fire-and-
    // forget form still allocates the db id there.
    let doodad_save = if server.self_arc().is_some() {
        crate::systems::npcs::doodad::db::save_now(server, &doodad).await
    } else {
        crate::systems::npcs::doodad::db::save(server, &doodad);
        Ok(())
    };
    if let Err(error) = doodad_save {
        tracing::error!("Nation monument doodad save failed: {error}");
        crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
        return Err(error_message::FACTION_CREATE);
    }
    let db_id = doodad.data().db_id;
    if db_id == 0 {
        crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
        return Err(error_message::FACTION_CREATE);
    }
    // Phase 3: re-validate and commit the reference under the same operation lock the claim /
    // capture paths use.
    let state = {
        let _items = server.systems.economy.operation_scope();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(current) = server.systems.housing.dominions.get(u32::from(zone)) else {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::ONLY_NATION_OWNER);
        };
        if current.tower != state.tower || current.owner_expedition_id != state.owner_expedition_id
        {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        if current.national_monument_db_id != 0 {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        }
        if current.siege.as_ref().is_some_and(|siege| {
            siege.status != crate::systems::housing::dominion::state::SiegeStatus::Finished
        }) {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD);
        }
        let state = match server.systems.housing.dominions.place_national_monument(
            u32::from(zone),
            current.owner_expedition_id,
            db_id,
            x,
            y,
            z,
        ) {
            Ok(state) => state,
            Err(error) => {
                crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
                return Err(monument_error(error));
            }
        };
        broadcast_monument(server, &state);
        state
    };
    // The in-memory row stays dirty for the periodic financial save; this targeted write only
    // makes the reference durable now (the doodad row exists already).
    if server.self_arc().is_some() {
        if let Err(error) = super::dominion::persist_national_monument(
            server,
            state.zone_group_id,
            i64::from(db_id),
            x,
            y,
            z,
        )
        .await
        {
            tracing::error!("Persisting the national monument of zone {zone} failed: {error}");
        }
    }
    Ok(())
}

/// Removes the dominion's national monument (skill 23682 "statue removal"): the kingdom's king
/// must stand at the statue; the stored fields are cleared, every client receives
/// `SCNationalMonumentChanged` with db id 0, the dominion row is zeroed, and only then is the
/// doodad row deleted (a crash may leave an orphan statue, never a dangling reference).
/// Removing the same monument twice is a no-op. Siege state does not block removal (only
/// placement has 751).
pub async fn remove_national_monument(
    server: &GameServer,
    king: &PlayerRef,
    zone: u16,
) -> std::result::Result<(), i16> {
    if !server.config.player_nations_enabled() {
        return Err(error_message::ONLY_NATION_OWNER);
    }
    let (state, world, old_db_id) = {
        let _items = server.systems.economy.operation_scope();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        if state.owner_expedition_id == 0
            || expedition_top_level_faction(server, state.owner_expedition_id) != Some(nation.id)
        {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        if state.national_monument_db_id == 0 {
            return Ok(());
        }
        let Some(world) = server.world.world(state.tower.instance_id) else {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        };
        let player = WorldObject::from(king);
        if server.world.world_of(&player).map(|w| w.id) != Some(world.id) {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let here = world.world_pos_rot(&player);
        if monument_distance(
            (here.x, here.y, here.z),
            (
                state.national_monument_x,
                state.national_monument_y,
                state.national_monument_z,
            ),
        ) > NATIONAL_MONUMENT_RANGE
        {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let old_db_id = u32::try_from(state.national_monument_db_id.max(0)).unwrap_or(0);
        let state = match server
            .systems
            .housing
            .dominions
            .clear_national_monument(u32::from(zone), state.owner_expedition_id)
        {
            Ok(state) => state,
            Err(error) => return Err(monument_error(error)),
        };
        broadcast_monument(server, &state);
        (state, world, old_db_id)
    };
    if server.self_arc().is_some() {
        if let Err(error) = super::dominion::persist_national_monument(
            server,
            state.zone_group_id,
            0,
            0.0,
            0.0,
            0.0,
        )
        .await
        {
            tracing::error!("Clearing the national monument of zone {zone} failed: {error}");
        }
    }
    if let Some(doodad) = world.doodad_by_db_id(old_db_id) {
        crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
    } else {
        crate::systems::npcs::doodad::db::delete_row(server, old_db_id);
    }
    Ok(())
}
