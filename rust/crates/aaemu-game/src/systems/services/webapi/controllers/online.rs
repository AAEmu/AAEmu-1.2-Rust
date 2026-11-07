//! `GET /online`: read-only tooling view of the characters currently in the world
//! (owner-approved WP-SL7). The 1.2 client has no web API; the route uses the same
//! API-key authentication as every other non-`/status` route.

use super::super::http::{ok_json, HttpResponse};
use super::super::json::JValue;
use super::super::route_mapper::{handler, Controller, ControllerCtx, Method, RouteSpec};
use crate::server::GameServer;

/// The fields of one online character, copied under its own lock.
struct OnlineCharacter {
    id: u32,
    name: String,
    level: u8,
    race: u8,
    faction_id: u32,
    faction_name: String,
    expedition_id: u32,
    zone_id: u32,
}

/// Online characters in character-id order, taken from the world manager's own read path.
/// `is_online` is the `Character.IsOnline` flag the session sets on enter-world / clears on
/// disconnect; a spawned character that is not online yet is not listed.
fn online_characters(server: &GameServer) -> Vec<OnlineCharacter> {
    let mut players = server.world.all_characters();
    players.sort_by_key(|p| p.character_id);
    let mut list = Vec::new();
    for player in players {
        let c = player.data();
        if !c.is_online {
            continue;
        }
        list.push(OnlineCharacter {
            id: c.id,
            name: c.unit.name.clone(),
            level: c.unit.level,
            race: c.race,
            faction_id: c.unit.faction_id,
            faction_name: c.faction_name.clone(),
            expedition_id: c.unit.expedition_id,
            zone_id: c.unit.transform.zone_id,
        });
    }
    list
}

/// The name of an expedition, or `None` for 0 / an unknown id.
fn expedition_name(server: &GameServer, id: u32) -> Option<String> {
    (id != 0)
        .then(|| server.expeditions.get(id))
        .flatten()
        .map(|e| e.faction.name)
}

/// `OnlineController`.
pub struct OnlineController;

impl Controller for OnlineController {
    fn routes() -> Vec<RouteSpec> {
        vec![RouteSpec {
            method: Method::Get,
            path: "/online",
            name: "GetOnline",
            handler: handler(|ctx| async move { get_online(&ctx) }),
        }]
    }
}

/// `GetOnline`: `{ "Count": n, "Characters": [ ... ] }` without account data, addresses or
/// positions (only the zone id).
fn get_online(ctx: &ControllerCtx) -> anyhow::Result<HttpResponse> {
    let server = ctx.server()?;
    let characters = online_characters(server);
    let list: Vec<JValue> = characters
        .iter()
        .map(|c| {
            JValue::object([
                ("Id", JValue::from(c.id)),
                ("Name", JValue::from(c.name.as_str())),
                ("Level", JValue::from(c.level)),
                ("Race", JValue::from(c.race)),
                ("FactionId", JValue::from(c.faction_id)),
                ("FactionName", JValue::from(c.faction_name.as_str())),
                ("ExpeditionId", JValue::from(c.expedition_id)),
                (
                    "ExpeditionName",
                    JValue::opt_str(expedition_name(server, c.expedition_id)),
                ),
                ("ZoneId", JValue::from(c.zone_id)),
            ])
        })
        .collect();
    Ok(ok_json(&JValue::object([
        ("Count", JValue::from(list.len() as u32)),
        ("Characters", JValue::Array(list)),
    ])))
}
