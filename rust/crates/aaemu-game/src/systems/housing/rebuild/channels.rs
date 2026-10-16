//! The `house.rebuild.*` client-extension channels (feature `house.rebuild`).
//!
//! | channel | direction | payload |
//! |---|---|---|
//! | `house.rebuild.info` | C->S | `{ house? }` (db id as string or number; omitted = the nearest own house within 30 m) |
//! | `house.rebuild.info` | S->C | `{ ok, house, template, name, tax, blocked?, blocked_message?, options: [{ rebuilding, target, name, skill, skill_name, labor, materials: [{ item, name, count, have }], tax, can, reason, reason_message }] }` (`skill_name` is empty when the data has no text; `reason`/`reason_message` are empty when `can`) |
//! | `house.rebuild.preview` | C->S | `{ house?, rebuilding }` |
//! | `house.rebuild.preview_end` | C->S | `{ house? }` |
//! | `house.rebuild.preview` | S->C | `{ ok, active, error?, message?, reason? }` |
//! | `house.rebuild.start` | C->S | `{ house?, rebuilding }` |
//! | `house.rebuild.result` | S->C | `{ ok, house, target, error?, message? }` |
//!
//! Ids that can exceed 16,777,216 travel as strings (the client's Lua numbers are floats); small
//! template ids and counts are numbers. Incoming ids may be either.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::net::connection::GameConnection;
use crate::server::GameServer;
use crate::systems::ext::{self, ExtRegistry};
use crate::world::{HouseRef, PlayerRef};

use super::ops::{self, HouseView, RebuildError};
use super::{preview, FEATURE};

/// A JSON id: a string of digits or a number.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum JsonId {
    Num(f64),
    Str(String),
}

impl JsonId {
    pub fn value(&self) -> Option<u32> {
        match self {
            Self::Num(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= f64::from(u32::MAX) => {
                Some(*n as u32)
            }
            Self::Str(s) => s.trim().parse().ok(),
            Self::Num(_) => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct HouseReq {
    #[serde(default)]
    house: Option<JsonId>,
}

#[derive(Debug, Deserialize)]
struct RebuildReq {
    #[serde(default)]
    house: Option<JsonId>,
    rebuilding: JsonId,
}

/// Registers the channels and the connection-closed hook.
pub fn register_ext(reg: &mut ExtRegistry) {
    reg.feature(FEATURE);
    reg.on_typed("house.rebuild.info", info);
    reg.on_typed("house.rebuild.preview", preview_start);
    reg.on_typed("house.rebuild.preview_end", preview_end);
    reg.on_typed("house.rebuild.start", start);
    reg.on_disconnect(preview::on_disconnect);
}

fn failure(error: RebuildError) -> Value {
    json!({ "ok": false, "error": error.code(), "message": error.message() })
}

/// The player of a connection that negotiated the feature.
fn player_of(conn: &GameConnection) -> Option<PlayerRef> {
    if !ext::has_feature(&conn.handle(), FEATURE) {
        return None;
    }
    conn.active_char.clone()
}

/// The house a request names (by `housings.id`), or the nearest own house.
fn house_of(
    server: &GameServer,
    player: &PlayerRef,
    wanted: Option<&JsonId>,
) -> Result<HouseRef, RebuildError> {
    match wanted {
        Some(id) => {
            let id = id.value().ok_or(RebuildError::NoHouse)?;
            server
                .systems
                .housing
                .manager
                .by_id(id)
                .ok_or(RebuildError::NoHouse)
        }
        None => ops::resolve_house(server, player).ok_or(RebuildError::NoHouse),
    }
}

/// The `house.rebuild.info` reply body.
pub fn info_json(view: &HouseView) -> Value {
    let options: Vec<Value> = view
        .options
        .iter()
        .map(|o| {
            json!({
                "rebuilding": o.rebuilding,
                "target": o.target,
                "name": o.name,
                "skill": o.skill,
                "skill_name": o.skill_name,
                "labor": o.labor,
                "materials": o.materials.iter().map(|m| json!({
                    "item": m.item, "name": m.name, "count": m.count, "have": m.have,
                })).collect::<Vec<_>>(),
                "tax": o.tax,
                "can": o.can,
                "reason": o.reason.map_or("", RebuildError::code),
                "reason_message": o.reason.map_or("", RebuildError::message),
            })
        })
        .collect();
    let mut v = json!({
        "ok": true,
        "house": view.house_id.to_string(),
        "template": view.template,
        "name": view.name,
        "tax": view.tax,
        "options": options,
    });
    if let Some(blocked) = view.blocked {
        v["blocked"] = json!(blocked.code());
        v["blocked_message"] = json!(blocked.message());
    }
    v
}

fn info(server: &GameServer, conn: &mut GameConnection, req: HouseReq) {
    let Some(player) = player_of(conn) else {
        ext::reply(
            conn,
            "house.rebuild.info",
            &json!({ "ok": false, "error": "not_available" }),
        );
        return;
    };
    let body = house_of(server, &player, req.house.as_ref())
        .and_then(|house| ops::view(server, &player, &house))
        .map_or_else(failure, |view| info_json(&view));
    ext::reply(conn, "house.rebuild.info", &body);
}

fn preview_start(server: &GameServer, conn: &mut GameConnection, req: RebuildReq) {
    let Some(player) = player_of(conn) else {
        ext::reply(
            conn,
            "house.rebuild.preview",
            &json!({ "ok": false, "active": false, "error": "not_available" }),
        );
        return;
    };
    let result = house_of(server, &player, req.house.as_ref()).and_then(|house| {
        let rebuilding = req
            .rebuilding
            .value()
            .ok_or(RebuildError::UnknownRebuilding)?;
        preview::start(server, &player, &house, rebuilding)
    });
    // Success is reported by `preview::start` itself.
    if let Err(e) = result {
        let mut v = failure(e);
        v["active"] = json!(preview::active_for(server, conn.id));
        ext::reply(conn, "house.rebuild.preview", &v);
    }
}

fn preview_end(server: &GameServer, conn: &mut GameConnection, _req: HouseReq) {
    if player_of(conn).is_none() {
        return;
    }
    // `end` notifies when a preview was active; answer anyway so the UI always gets a reply.
    if !preview::end(server, conn.id, "ended") {
        ext::reply(
            conn,
            "house.rebuild.preview",
            &json!({ "ok": true, "active": false }),
        );
    }
}

fn start(server: &GameServer, conn: &mut GameConnection, req: RebuildReq) {
    let Some(player) = player_of(conn) else {
        ext::reply(
            conn,
            "house.rebuild.result",
            &json!({ "ok": false, "error": "not_available" }),
        );
        return;
    };
    let house = house_of(server, &player, req.house.as_ref());
    let house_id = house.as_ref().map_or(0, |h| h.data().id);
    let result = house.and_then(|house| {
        let rebuilding = req
            .rebuilding
            .value()
            .ok_or(RebuildError::UnknownRebuilding)?;
        ops::rebuild(server, &player, &house, rebuilding)
    });
    let body = match result {
        Ok(target) => json!({ "ok": true, "house": house_id.to_string(), "target": target }),
        Err(e) => {
            let mut v = failure(e);
            v["house"] = json!(house_id.to_string());
            v
        }
    };
    ext::reply(conn, "house.rebuild.result", &body);
}
