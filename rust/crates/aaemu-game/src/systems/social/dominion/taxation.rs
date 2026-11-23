use super::{
    broadcast_snapshot, is_expedition_leader, DateTime, DominionError, GameServer, PlayerRef,
    SCDominionTaxRatePacket, SCNationalTaxRatePacket, Utc,
};

/// Match the original native IsDominionOwner gate and its inclusive 0..50 slider.
pub fn update_tax_rate(server: &GameServer, player: &PlayerRef, zone: u16, tax_rate: i32) {
    let _items = server.systems.economy.operation_scope();
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
        return;
    };
    if !(0..=50).contains(&tax_rate)
        || !is_expedition_leader(server, player, state.owner_expedition_id)
    {
        return;
    }
    let prev = state.tax_rate;
    let state = match server.systems.housing.dominions.set_tax_rate(
        u32::from(zone),
        state.owner_expedition_id,
        tax_rate,
        Utc::now().timestamp(),
    ) {
        Ok(state) => state,
        Err(DominionError::TaxRateChangeLater) => {
            super::util::send_error(
                player,
                crate::systems::economy::error_message::TAXRATE_CHANGE_LATER,
            );
            return;
        }
        Err(_) => return,
    };
    server
        .world
        .broadcast_to_server(&SCDominionTaxRatePacket { id: zone, tax_rate });
    crate::systems::housing::dominion::tax::refresh_bills(server, u32::from(zone));
    broadcast_snapshot(server, &state, false);
    drop(_operation);
    if prev != state.tax_rate {
        crate::systems::economy::mail_system::send_tax_rate_changed_mails(
            server,
            zone,
            prev,
            state.tax_rate,
        );
    }
}

/// `CSUpdateNationalTaxRatePacket` (0x013): the nation king sets the national tax rate of a
/// dominion owned by an expedition inside the kingdom. The checks mirror the 1.2 client's
/// confirm validator (`FUN_392f4820`): `rate > 1000` -> 684; an unknown / unowned dominion or
/// a caller who is not the king of the dominion's top-level faction -> 685; the same rate is
/// ignored (`equal_tax_rate_warning`); a change within 86400 s -> 725. Success broadcasts
/// `SCNationalTaxRate` (0x021; handler `FUN_392f39b0` and notice ui_texts 5675) to every
/// client, because the dominion record is server-wide, and leaves the row dirty for the
/// periodic dominion save. The rate is permille/10 (0-1000); applying it to collected tax and
/// the weekly receipt mail (type 24) are capture-pending (RENAME-8).
pub fn update_national_tax_rate(server: &GameServer, player: &PlayerRef, zone: u16, rate: u32) {
    if !server.config.player_nations_enabled() {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    }
    let _items = server.systems.economy.operation_scope();
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if rate > 1000 {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_INVALID_RATE,
        );
        return;
    }
    let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    };
    let Some(nation) = server.systems.social.nations.owned_by(player.character_id) else {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    };
    if state.owner_expedition_id == 0
        || super::nation::expedition_top_level_faction(server, state.owner_expedition_id)
            != Some(nation.id)
    {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    }
    if state.national_tax_rate == rate as u16 {
        return;
    }
    let prev = state.national_tax_rate;
    let now = Utc::now().timestamp();
    let state = match server.systems.housing.dominions.set_national_tax_rate(
        u32::from(zone),
        state.owner_expedition_id,
        rate as u16,
        now,
    ) {
        Ok(state) => state,
        Err(DominionError::NationalTaxRateChangeLater) => {
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_LATER,
            );
            return;
        }
        Err(DominionError::InvalidNationalTaxRate) => {
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_INVALID_RATE,
            );
            return;
        }
        Err(_) => return,
    };
    server.world.broadcast_to_server(&SCNationalTaxRatePacket {
        id: zone,
        tax_rate: i32::from(state.national_tax_rate),
        prev_tax_rate: i32::from(prev),
        changed_time: DateTime::from_timestamp(state.last_national_tax_rate_changed_at, 0)
            .unwrap_or(DateTime::<Utc>::MIN_UTC),
    });
    schedule_persist_national_tax(
        server,
        u32::from(zone),
        state.national_tax_rate,
        state.last_national_tax_rate_changed_at,
    );
    drop(_operation);
    crate::systems::economy::mail_system::send_nation_tax_rate_mails(
        server,
        nation.id,
        zone,
        state.national_tax_rate,
        prev,
        state.last_national_tax_rate_changed_at,
    );
}

/// Writes only the national tax columns of one territory. Treasury / siege / other columns
/// stay with the periodic financial save, which is the only path allowed to commit treasury
/// changes with houses / mail / wallets (`managers/save.rs`). The in-memory row remains dirty,
/// so the next financial save writes the same values again.
pub(crate) async fn persist_national_tax(
    server: &GameServer,
    zone_group_id: u32,
    rate: u16,
    changed_at: i64,
) -> sqlx::Result<()> {
    let _save = server.save.transaction_lock().await;
    crate::systems::housing::dominion::db::write_national_tax_rate(
        &server.db,
        zone_group_id,
        rate,
        changed_at,
    )
    .await?;
    Ok(())
}

/// Spawns [`persist_national_tax`] (no-op in unit tests without a server handle).
pub(crate) fn schedule_persist_national_tax(
    server: &GameServer,
    zone_group_id: u32,
    rate: u16,
    changed_at: i64,
) {
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = persist_national_tax(&server, zone_group_id, rate, changed_at).await {
            tracing::error!("Persisting the national tax of zone {zone_group_id} failed: {error}");
        }
    });
}
