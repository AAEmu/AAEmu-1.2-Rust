//! Calendar side of the siege declaration chain (WP-SD2a).
//!
//! A declaration is accepted during the declare period (WP-SD2b); this module hands it over
//! to the explicit siege record at the cycle's warmup start, so the record exists before the
//! siege. The calendar keeps describing the cycle's periods until the siege ends; only a GM
//! `/dominion schedule` owns its own timers. A declaration whose guild no longer exists, or
//! that was never materialised before warmup, is dropped and announced with a zero offense
//! team (`SCSiegeDeclared`), the client's "nobody declared" state.

use crate::server::GameServer;

use super::calendar::{self, SiegeCalendar};
use super::reinforce;
use super::state::SiegeLimits;

fn expedition_alive(server: &GameServer, expedition_id: u32) -> bool {
    expedition_id != 0
        && server
            .expeditions
            .get(expedition_id)
            .is_some_and(|expedition| !expedition.is_disbanded)
}

/// Drop a declaration that can no longer become this cycle's siege. The caller holds the
/// housing operation lock.
fn drop_and_announce(server: &GameServer, zone_group_id: u32) {
    match server
        .systems
        .housing
        .dominions
        .drop_declaration(zone_group_id)
    {
        Ok(Some(state)) => {
            tracing::info!(
                "Dominion {zone_group_id}: a siege declaration can no longer be honoured"
            );
            crate::systems::social::dominion::broadcast_siege_declared(server, &state);
        }
        Ok(None) => {}
        Err(error) => tracing::warn!("Dominion {zone_group_id}: dropping declaration: {error}"),
    }
}

/// Materialise declarations at the warmup boundary and drop stale ones. Runs inside the
/// housing operation lock (called from `runtime::tick_once`), never holding an object lock.
pub fn tick(server: &GameServer, now: i64) {
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing);
    if !settings.enabled {
        return;
    }
    let calendar = SiegeCalendar::from_castles(&server.game_data.systems.housing.castles);
    if calendar.is_empty() {
        return;
    }
    for state in server.systems.housing.dominions.all() {
        if state.owner_expedition_id == 0 {
            continue;
        }
        let Some(cycle) = state.declared_cycle() else {
            continue;
        };
        let attacker = state
            .declaration
            .map(|declaration| declaration.attacker_expedition_id)
            .or_else(|| {
                state
                    .siege
                    .as_ref()
                    .filter(|siege| siege.materialised_from_declaration)
                    .map(|siege| siege.attacker_expedition_id)
            })
            .unwrap_or(0);
        if !expedition_alive(server, attacker) {
            drop_and_announce(server, state.zone_group_id);
            continue;
        }
        let Some(view) = calendar.view_for_declaration(
            state.zone_group_id,
            now,
            &settings.timezone,
            Some(cycle),
            0,
        ) else {
            continue;
        };
        if view.cycle.map(|cycle| cycle.auction_start) != Some(cycle) {
            // The declaration describes a cycle that is no longer running.
            drop_and_announce(server, state.zone_group_id);
            continue;
        }
        if view.period == calendar::PERIOD_WARMUP {
            let Some(running) = view.cycle else {
                continue;
            };
            let limits =
                reinforce::limits(server, state.owner_expedition_id).unwrap_or(SiegeLimits::NATIVE);
            match server.systems.housing.dominions.materialise_declaration(
                state.zone_group_id,
                running.auction_start,
                running.siege_start,
                running.siege_end,
                now,
                limits,
            ) {
                Ok(Some(state)) => {
                    tracing::info!(
                        "Dominion {}: declaration materialised into the cycle siege",
                        state.zone_group_id
                    );
                    crate::systems::social::dominion::broadcast_siege_declared(server, &state);
                    crate::systems::social::dominion::broadcast_phase_change(server, &state);
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    "Dominion {}: could not materialise the declaration: {error}",
                    state.zone_group_id
                ),
            }
        } else if matches!(view.period, calendar::PERIOD_SIEGE | calendar::PERIOD_PEACE)
            && state.declaration.is_some()
        {
            // Warmup passed while no record was created (e.g. the server was down): the
            // declaration cannot be honoured anymore.
            drop_and_announce(server, state.zone_group_id);
        }
    }
}
