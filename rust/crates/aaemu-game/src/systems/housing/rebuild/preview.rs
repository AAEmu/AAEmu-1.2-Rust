//! The 3D rebuild preview, driven by the server for one client only.
//!
//! For the requesting connection the real house (and everything parented to it) is removed from
//! the client's view with the packets visibility removal sends, and a *phantom* house is shown
//! instead: a fresh object id, the target template in its finished state, the real house's
//! position and rotation, the packets visibility-add sends. The phantom is never inserted into
//! the world, so nobody else sees it and no world code ever touches it.
//!
//! A preview ends on: `house.rebuild.preview_end`, another preview, the rebuild itself (or any
//! other rebuild of that house), [`super::PREVIEW_TIMEOUT_MS`], the player moving farther than
//! [`super::PREVIEW_MAX_DISTANCE`], the house disappearing or changing owner, and the connection
//! closing. At most
//! one preview exists per connection.
//!
//! Known limit: while a preview is active, a region change that makes the world re-send the real
//! house to the client shows it on top of the phantom until the preview ends (<= 2 minutes).
//! The phantom does not carry the target's bound doodads. It carries the target's default name
//! (a custom house name is kept).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::server::GameServer;
use crate::systems::ext;
use crate::world::{visibility, Entity, HouseRef, PlayerRef, WorldObject};

use super::super::manager::{template_of, world_of};
use super::ops::RebuildError;
use super::{FEATURE, PREVIEW_MAX_DISTANCE, PREVIEW_TIMEOUT_MS};

/// What ending a preview does to the client's view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Finish {
    /// Nothing is sent (the connection or the player is gone).
    Silent,
    /// The phantom disappears; the real house stays hidden (it no longer exists).
    PhantomOnly,
    /// The phantom disappears and the real house is shown again.
    Restore,
}

/// One active preview.
struct Preview {
    player: PlayerRef,
    house: HouseRef,
    /// `housings.id` of the real house.
    house_id: u32,
    phantom: HouseRef,
    rebuilding: u32,
    started_ms: i64,
}

/// The previews of all connections (`server.systems.housing.rebuild_previews`).
#[derive(Default)]
pub struct Previews(Mutex<HashMap<u32, Preview>>);

impl std::fmt::Debug for Previews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Previews({})", self.lock().len())
    }
}

impl Previews {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Preview>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `(housings.id, rebuilding id)` of a connection's active preview.
    pub fn active(&self, connection_id: u32) -> Option<(u32, u32)> {
        self.lock()
            .get(&connection_id)
            .map(|p| (p.house_id, p.rebuilding))
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

fn previews(server: &GameServer) -> &Previews {
    &server.systems.housing.rebuild_previews
}

/// The `house.rebuild.preview` message.
fn notify(player: &PlayerRef, active: bool, reason: Option<&str>) {
    let mut v = json!({ "ok": true, "active": active });
    if let Some(reason) = reason {
        v["reason"] = json!(reason);
    }
    ext::send_player(player, "house.rebuild.preview", &v);
}

/// True when the connection has a preview.
pub fn active_for(server: &GameServer, connection_id: u32) -> bool {
    previews(server).active(connection_id).is_some()
}

/// Starts (or replaces) the connection's preview of `rebuilding_id` on `house`.
pub fn start(
    server: &GameServer,
    player: &PlayerRef,
    house: &HouseRef,
    rebuilding_id: u32,
) -> Result<(), RebuildError> {
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if house.data().unit.owner_id != player.character_id {
        return Err(RebuildError::NotOwner);
    }
    let data = &server.game_data.systems.housing;
    let template_id = house.data().unit.template_id;
    let rebuilding = data
        .rebuild
        .option_for(template_id, rebuilding_id)
        .ok_or(RebuildError::UnknownRebuilding)?;
    let target = data
        .template(rebuilding.housing_id)
        .ok_or(RebuildError::NoTarget)?;
    let world = world_of(server, house).ok_or(RebuildError::NoHouse)?;
    let reach = PREVIEW_MAX_DISTANCE + template_of(server, house).map_or(0.0, |t| t.garden_radius);
    if distance(player, house).is_none_or(|d| d > reach) {
        return Err(RebuildError::TooFar);
    }
    let conn_id = player.conn.id;
    // Another preview of this connection ends first (no notification: the new one follows).
    end_connection(server, conn_id, Finish::Restore, None);

    let Ok(obj_id) = server.ids.object.get_next_id() else {
        return Err(RebuildError::NoTarget);
    };
    let phantom = {
        let mut h = house.data().clone();
        // The nameplate shows the target's name unless the owner named the house.
        if let Some(name) =
            super::ops::retargeted_name(data, h.unit.template_id, &h.unit.name, target.id)
        {
            h.unit.name = name;
        }
        h.unit.template_id = target.id;
        h.unit.model_id = target.main_model_id;
        h.unit.max_hp = target.hp;
        h.unit.hp = target.hp;
        // The finished building (no construction progress, no sale, no bound doodads).
        h.current_step = -1;
        h.num_action = 0;
        h.base_action = 0;
        h.all_action = target.all_actions();
        h.build_step_count = target.build_steps.len() as i32;
        h.tax = target.tax_or_zero() as i32;
        h.sell_price = 0;
        h.sell_to_player_id = 0;
        h.attached_doodads.clear();
        Entity::new(obj_id, h)
    };
    let ctx = server.view_ctx();
    visibility::remove_visible_object(&world, &WorldObject::from(house), player);
    visibility::add_visible_object(&world, &WorldObject::from(&phantom), player, &ctx);
    let house_id = house.data().id;
    previews(server).lock().insert(
        conn_id,
        Preview {
            player: player.clone(),
            house: house.clone(),
            house_id,
            phantom,
            rebuilding: rebuilding_id,
            started_ms: crate::world::now_ms(),
        },
    );
    notify(player, true, None);
    Ok(())
}

/// Ends the connection's preview: the phantom disappears and the real house is shown again.
/// Returns false when there was none. Sends `house.rebuild.preview {active:false}` when the
/// connection has the extension and `reason` is given.
pub fn end(server: &GameServer, connection_id: u32, reason: &str) -> bool {
    end_connection(server, connection_id, Finish::Restore, Some(reason))
}

fn end_connection(
    server: &GameServer,
    connection_id: u32,
    mode: Finish,
    reason: Option<&str>,
) -> bool {
    let Some(preview) = previews(server).lock().remove(&connection_id) else {
        return false;
    };
    finish(server, preview, mode, reason);
    true
}

/// Ends every preview of the house `house_id` (`housings.id`), e.g. because it is rebuilt.
pub fn end_for_house(server: &GameServer, house_id: u32) {
    let ended: Vec<Preview> = {
        let mut map = previews(server).lock();
        let ids: Vec<u32> = map
            .iter()
            .filter(|(_, p)| p.house_id == house_id)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().filter_map(|id| map.remove(&id)).collect()
    };
    for preview in ended {
        finish(server, preview, Finish::Restore, Some("rebuilt"));
    }
}

/// The connection closed: forget its preview (nothing to send, nobody to restore).
pub fn on_disconnect(server: &GameServer, connection_id: u32) {
    end_connection(server, connection_id, Finish::Silent, None);
}

/// Starts the preview watchdog (every 2 s; see [`tick`]).
pub fn start_task(server: &Arc<GameServer>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(server);
    rt.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let Some(server) = weak.upgrade() else {
                return;
            };
            if !previews(&server).is_empty() {
                tick(&server, crate::world::now_ms());
            }
        }
    });
}

/// Periodic check (every couple of seconds): timeout, distance, vanished player or house.
pub fn tick(server: &GameServer, now_ms: i64) {
    let mut ended = Vec::new();
    {
        let mut map = previews(server).lock();
        let mut remove = Vec::new();
        for (conn_id, p) in map.iter() {
            let online = server
                .world
                .character_by_obj_id(p.player.obj_id)
                .is_some_and(|c| Arc::ptr_eq(&c, &p.player));
            let house_alive = server
                .systems
                .housing
                .manager
                .by_id(p.house_id)
                .is_some_and(|h| Arc::ptr_eq(&h, &p.house));
            let decision = if !online {
                Some((Finish::Silent, None))
            } else if !house_alive {
                Some((Finish::PhantomOnly, Some("house_removed")))
            } else if p.house.data().unit.owner_id != p.player.character_id {
                // Sold, demolished or transferred meanwhile.
                Some((Finish::Restore, Some("not_owner")))
            } else if now_ms - p.started_ms >= PREVIEW_TIMEOUT_MS {
                Some((Finish::Restore, Some("timeout")))
            } else if distance(&p.player, &p.house).is_none_or(|d| d > PREVIEW_MAX_DISTANCE) {
                Some((Finish::Restore, Some("too_far")))
            } else {
                None
            };
            if let Some(d) = decision {
                remove.push((*conn_id, d));
            }
        }
        for (conn_id, (mode, reason)) in remove {
            if let Some(p) = map.remove(&conn_id) {
                ended.push((p, mode, reason));
            }
        }
    }
    for (preview, mode, reason) in ended {
        finish(server, preview, mode, reason);
    }
}

fn distance(player: &PlayerRef, house: &HouseRef) -> Option<f32> {
    let p = player.data().unit.transform;
    let h = house.data().unit.transform;
    (p.instance_id == h.instance_id).then(|| ((p.x - h.x).powi(2) + (p.y - h.y).powi(2)).sqrt())
}

/// Removes the phantom from the client's view, restores the real house when asked and still
/// valid, releases the phantom's object id and notifies the client.
fn finish(server: &GameServer, preview: Preview, mode: Finish, reason: Option<&str>) {
    let Preview {
        player,
        house,
        house_id,
        phantom,
        ..
    } = preview;
    if let Some(world) = world_of(server, &house) {
        if mode != Finish::Silent {
            visibility::remove_visible_object(&world, &WorldObject::from(&phantom), &player);
        }
        if mode == Finish::Restore {
            let alive = server
                .systems
                .housing
                .manager
                .by_id(house_id)
                .is_some_and(|h| Arc::ptr_eq(&h, &house));
            let obj = WorldObject::from(&house);
            if alive && house.core().is_visible() && near_in_view(&world, &player, &obj) {
                visibility::add_visible_object(&world, &obj, &player, &server.view_ctx());
            }
        }
    }
    server.ids.object.release_id(phantom.obj_id);
    if reason.is_some() && ext::has_feature(&player.conn, FEATURE) {
        notify(&player, false, reason);
    }
}

/// The house lies in the sectors the player currently receives objects from.
fn near_in_view(
    world: &crate::world::WorldInstance,
    player: &PlayerRef,
    house: &WorldObject,
) -> bool {
    let (Some(house_region), Some(player_region)) = (
        world.region_of(house),
        world.region_of(&WorldObject::from(player)),
    ) else {
        return false;
    };
    world.neighbors(player_region).contains(&house_region)
}
