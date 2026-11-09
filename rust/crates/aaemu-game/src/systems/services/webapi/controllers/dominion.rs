//! `GET /dominions`: read-only territory and siege snapshot for tooling (owner-approved
//! WP-SL7). Every value comes from the dominion authority's snapshots and the housing
//! calendar; no lock is held across an `await` because the handler does no I/O.

use super::super::http::{ok_json, HttpResponse};
use super::super::json::{DateKind, JValue};
use super::super::route_mapper::{handler, Controller, ControllerCtx, Method, RouteSpec};
use crate::server::GameServer;
use crate::systems::housing::dominion::calendar::SiegeCalendar;
use crate::systems::housing::dominion::period;
use crate::systems::housing::dominion::state::{DominionState, SiegeStatus};

/// The name of an expedition, or `None` for 0 / an unknown id.
fn expedition_name(server: &GameServer, id: u32) -> Option<String> {
    (id != 0)
        .then(|| server.expeditions.get(id))
        .flatten()
        .map(|e| e.faction.name)
}

/// A Unix-second instant as a `DateTimeKind.Utc` ISO date.
fn unix_date(unix: i64) -> JValue {
    JValue::Date(crate::db::from_unix(unix), DateKind::Utc)
}

/// The next siege window of a territory:
///
/// * an unfinished explicit siege record owns its own real-clock timers;
/// * otherwise an accepted declaration for the running calendar cycle uses that cycle's
///   authored siege window (a stale declaration describes no cycle and reports nothing).
fn next_siege(server: &GameServer, state: &DominionState) -> Option<(i64, i64)> {
    if let Some(siege) = state
        .siege
        .as_ref()
        .filter(|siege| siege.status != SiegeStatus::Finished)
    {
        return Some((siege.starts_at, siege.ends_at));
    }
    let declared_cycle = state.declared_cycle()?;
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    if !settings.enabled {
        return None;
    }
    let calendar = SiegeCalendar::from_castles(&server.game_data.systems.housing.castles);
    if calendar.is_empty() {
        return None;
    }
    let now = server.systems.housing.calendar.now();
    let view = calendar.view_for_declaration(
        state.zone_group_id,
        now,
        &settings.timezone,
        Some(declared_cycle),
        0,
    )?;
    let cycle = view.cycle?;
    (cycle.auction_start == declared_cycle).then_some((cycle.siege_start, cycle.siege_end))
}

/// A declaration attacker (`{ "ExpeditionId", "Name" }`) or JSON `null`.
fn declaration_attacker(server: &GameServer, state: &DominionState) -> JValue {
    let Some(declaration) = state.declaration else {
        return JValue::Null;
    };
    JValue::object([
        (
            "ExpeditionId",
            JValue::from(declaration.attacker_expedition_id),
        ),
        (
            "Name",
            JValue::opt_str(expedition_name(server, declaration.attacker_expedition_id)),
        ),
    ])
}

/// `DominionController`.
pub struct DominionController;

impl Controller for DominionController {
    fn routes() -> Vec<RouteSpec> {
        vec![RouteSpec {
            method: Method::Get,
            path: "/dominions",
            name: "GetDominions",
            handler: handler(|ctx| async move { get_dominions(&ctx) }),
        }]
    }
}

/// `GetDominions`: one object per persisted dominion zone group, ordered by zone group id.
fn get_dominions(ctx: &ControllerCtx) -> anyhow::Result<HttpResponse> {
    let server = ctx.server()?;
    let list: Vec<JValue> = server
        .systems
        .housing
        .dominions
        .snapshots()
        .iter()
        .filter(|state| !state.ephemeral)
        .map(|state| {
            let owner_id = state.owner_expedition_id;
            let siege = next_siege(server, state);
            JValue::object([
                ("ZoneGroupId", JValue::from(state.zone_group_id)),
                (
                    "ZoneGroupName",
                    JValue::opt_str(
                        server
                            .game_data
                            .zones
                            .group(state.zone_group_id)
                            .map(|group| group.name.clone()),
                    ),
                ),
                (
                    "OwnerExpeditionId",
                    if owner_id == 0 {
                        JValue::Null
                    } else {
                        JValue::from(owner_id)
                    },
                ),
                (
                    "OwnerExpeditionName",
                    JValue::opt_str(expedition_name(server, owner_id)),
                ),
                (
                    "SiegePeriod",
                    period::current_period(server, state.zone_group_id)
                        .map_or(JValue::Null, JValue::from),
                ),
                (
                    "NextSiegeStart",
                    siege.map_or(JValue::Null, |(start, _)| unix_date(start)),
                ),
                (
                    "NextSiegeEnd",
                    siege.map_or(JValue::Null, |(_, end)| unix_date(end)),
                ),
                ("DeclarationAttacker", declaration_attacker(server, state)),
            ])
        })
        .collect();
    Ok(ok_json(&JValue::Array(list)))
}
