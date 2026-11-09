//! Read-only current siege period of one territory (WP-S3).
//!
//! This mirrors the period the `SCSiegeState` / `SCDominionData` snapshot sends
//! (`social::dominion::snapshot`) without changing the calendar tick:
//!
//! * an unfinished, unexpired explicit schedule owns the territory (warmup before `starts_at`,
//!   siege inside it; the snapshot keeps explicit sieges on the real clock);
//! * otherwise an owned territory follows the weekly calendar when it is enabled (the GM test
//!   clock drives only this view);
//! * an owned territory without either source is peace (`5`, the snapshot's default) and an
//!   unowned one is `0` (no dominion).
//!
//! `None` means the period cannot be resolved. The calendar view uses the accepted
//! declaration of the running cycle (WP-SD2a), so a declared territory walks 2 -> 3 -> 4 -> 5;
//! without one the declare period ends directly in peace.

use chrono::Utc;

use super::calendar::{self, SiegeCalendar};
use super::state::SiegeStatus;
use crate::server::GameServer;

/// The current period of `zone_group_id`, or `None` when no source can describe it.
pub fn current_period(server: &GameServer, zone_group_id: u32) -> Option<u8> {
    let Some(state) = server.systems.housing.dominions.get(zone_group_id) else {
        return Some(calendar::PERIOD_NONE);
    };
    let real_now = Utc::now().timestamp();
    if let Some(siege) = state
        .siege
        .as_ref()
        .filter(|siege| siege.status != SiegeStatus::Finished && real_now < siege.ends_at)
    {
        return Some(if siege.active_at(real_now) {
            calendar::PERIOD_SIEGE
        } else {
            calendar::PERIOD_WARMUP
        });
    }
    if state.owner_expedition_id == 0 {
        return Some(calendar::PERIOD_NONE);
    }
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    if !settings.enabled {
        return Some(calendar::PERIOD_PEACE);
    }
    let calendar = SiegeCalendar::from_castles(&server.game_data.systems.housing.castles);
    if calendar.is_empty() {
        return Some(calendar::PERIOD_PEACE);
    }
    let now = server.systems.housing.calendar.now();
    Some(
        calendar
            .view_for_declaration(
                zone_group_id,
                now,
                &settings.timezone,
                state.declared_cycle(),
                0,
            )
            .map_or(calendar::PERIOD_PEACE, |view| view.period),
    )
}
