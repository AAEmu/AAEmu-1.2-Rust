//! The area the siege rules apply to, the per-zone snapshot behind them, and the eviction of
//! non-participants.
//!
//! Evidence: the common relationship engine's siege branch (`sub_39695460`) tests two circles
//! while the siege period runs — the lodestone's siege circle and the offense-headquarters
//! circle (`D:/ArcheAge Returns 10.0.2.13 .../decompiled/x2game-dev_dedicate.dll.c`, cited as
//! `Z:2790654` in `D:/aaemu-audits/gap-audit-2026-11-10/verify-claims-10x.md`, claim 2). The
//! 1.2 data names both radii: `guard_tower_settings.radius_siege` (250 m small / 450 m large
//! castles) and `radius_offense_hq` (100 / 120 m).
//!
//! The removal rule is the owner's source for the 1.2-era siege flow (IGN, 2014-09-26,
//! "Castles"; Total ArcheAge, 2014-11-20, grade C): "only designated participants can enter"
//! the perimeter and "those who do not qualify ... are transported out of the siege perimeter
//! when it begins". The 1.2 client has no separate warning text for it; the error table has
//! `SIEGE_NOT_PARTICIPANT` 446 (ui_texts 2505 "Not a siege participant"), which the forced
//! `SCTeleportUnitPacket` carries in its error-message field.
//!
//! The 10.x enum numbers the running siege 3; the 1.2 client numbers it 4 (siege.md 1.2).
//! Both callers gate on [`SiegeArea::active_at`], which picks the clock exactly like
//! `DominionManager::tick_dual` does (the calendar / GM test clock for a calendar-materialised
//! siege, the real clock for a GM schedule), not on a specific period number.
//!
//! [`SiegeAreaCache`] is rebuilt once a second by [`refresh`] (the participant tick) so the
//! per-attack hostility check does not clone the authority's `DominionState`: the cache holds
//! only the siege parties, the window, the two circle centres and the roster ids.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::housing::dominion::runtime;
use crate::systems::housing::dominion::state::{SiegeSide, SiegeStatus, TowerIdentity};
use crate::systems::skills::packets_special::{
    SCTeleportUnitPacket, TELEPORT_REASON_MOVE_TO_LOCATION,
};
use crate::world::session;
use crate::world::WorldObject;

/// Fallback when a data row has no `radius_siege` (the shipped rows have 250 / 450).
const DEFAULT_RADIUS_SIEGE: f64 = 250.0;
/// Fallback when a data row has no `radius_offense_hq` (the shipped rows have 100 / 120).
const DEFAULT_RADIUS_OFFENSE_HQ: f64 = 100.0;

fn horizontal_distance2(a: (f32, f32), b: (f32, f32)) -> f64 {
    (f64::from(a.0) - f64::from(b.0)).powi(2) + (f64::from(a.1) - f64::from(b.1)).powi(2)
}

/// A small per-zone snapshot of an unfinished siege, copied out of the authority by
/// [`refresh`]. `can_attack` and the eviction read this instead of cloning `DominionState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SiegeArea {
    pub zone_group_id: u32,
    pub tower: TowerIdentity,
    pub tower_x: f32,
    pub tower_y: f32,
    /// The bound offense HQ's centre, when one resolves for the record.
    pub offense_hq: Option<(f32, f32)>,
    pub radius_siege: f64,
    pub radius_offense_hq: f64,
    pub attacker_expedition_id: u32,
    pub defender_expedition_id: u32,
    pub starts_at: i64,
    pub ends_at: i64,
    pub finished: bool,
    pub materialised_from_declaration: bool,
}

impl SiegeArea {
    /// The clock choice of `DominionManager::tick_dual`: a calendar materialisation follows
    /// the calendar / GM test clock, a GM schedule the real clock.
    pub fn active_at(&self, real_now: i64, calendar_now: i64) -> bool {
        let now = if self.materialised_from_declaration {
            calendar_now
        } else {
            real_now
        };
        !self.finished && self.starts_at <= now && now < self.ends_at
    }

    /// Whether `(x, y)` lies in the lodestone's `radius_siege` circle or, when one is bound,
    /// the offense HQ's `radius_offense_hq` circle.
    pub fn inside(&self, instance_id: u32, x: f32, y: f32) -> bool {
        if instance_id != self.tower.instance_id || !x.is_finite() || !y.is_finite() {
            return false;
        }
        if self.radius_siege > 0.0
            && horizontal_distance2((x, y), (self.tower_x, self.tower_y))
                <= self.radius_siege.powi(2)
        {
            return true;
        }
        self.offense_hq.is_some_and(|(hx, hy)| {
            self.radius_offense_hq > 0.0
                && horizontal_distance2((x, y), (hx, hy)) <= self.radius_offense_hq.powi(2)
        })
    }
}

/// Rebuilt once a second from the authority: the unfinished sieges' areas and rosters.
#[derive(Debug, Default)]
pub struct SiegeAreaCache {
    areas: Mutex<BTreeMap<u32, SiegeArea>>,
    rosters: Mutex<BTreeMap<u32, BTreeSet<u32>>>,
}

impl SiegeAreaCache {
    pub fn get(&self, zone_group_id: u32) -> Option<SiegeArea> {
        self.areas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&zone_group_id)
            .copied()
    }

    pub fn is_participant(&self, zone_group_id: u32, character_id: u32) -> bool {
        self.rosters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&zone_group_id)
            .is_some_and(|ids| ids.contains(&character_id))
    }
}

fn area_snapshot(
    server: &GameServer,
    state: &crate::systems::housing::dominion::state::DominionState,
) -> Option<SiegeArea> {
    let siege = state.siege.as_ref()?;
    let tower = server.systems.housing.manager.by_id(state.tower.house_id)?;
    let tower_object = WorldObject::from(&tower);
    if !runtime::is_live_object(server, &tower_object) {
        return None;
    }
    let world = server.world.world(state.tower.instance_id)?;
    let position = world.world_pos_rot(&tower_object);
    if !position.x.is_finite() || !position.y.is_finite() {
        return None;
    }
    let setting = server
        .game_data
        .systems
        .housing
        .castles
        .guard_tower_settings
        .get(&state.tower.guard_tower_setting_id);
    let offense_hq = server
        .systems
        .housing
        .headquarters
        .snapshot(server, state, SiegeSide::Attacker)
        .map(|hq| (hq.position.x, hq.position.y));
    Some(SiegeArea {
        zone_group_id: state.zone_group_id,
        tower: state.tower,
        tower_x: position.x,
        tower_y: position.y,
        offense_hq,
        radius_siege: setting.map_or(DEFAULT_RADIUS_SIEGE, |s| f64::from(s.radius_siege)),
        radius_offense_hq: setting.map_or(DEFAULT_RADIUS_OFFENSE_HQ, |s| {
            f64::from(s.radius_offense_hq)
        }),
        attacker_expedition_id: siege.attacker_expedition_id,
        defender_expedition_id: siege.defender_expedition_id,
        starts_at: siege.starts_at,
        ends_at: siege.ends_at,
        finished: siege.status == SiegeStatus::Finished,
        materialised_from_declaration: siege.materialised_from_declaration,
    })
}

/// Rebuild the per-zone cache. Called once a second by the participant tick (it clones each
/// `DominionState` once then, never per attack), and by tests that drive the rules directly.
pub fn refresh(server: &GameServer) {
    let states = server.systems.housing.dominions.all();
    let mut areas = BTreeMap::new();
    let mut rosters = BTreeMap::new();
    for state in states {
        let Some(siege) = state
            .siege
            .as_ref()
            .filter(|siege| siege.status != SiegeStatus::Finished)
        else {
            continue;
        };
        let Some(area) = area_snapshot(server, &state) else {
            continue;
        };
        areas.insert(state.zone_group_id, area);
        rosters.insert(
            state.zone_group_id,
            siege.participants.keys().copied().collect(),
        );
    }
    let cache = &server.systems.housing.siege_areas;
    *cache
        .areas
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = areas;
    *cache
        .rosters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = rosters;
}

/// The nearest safe resurrection point (`PortalManager.GetClosestReturnPortal`, the same
/// fallback the ordinary resurrection uses), or the character's saved main-world position,
/// provided the destination is outside the siege area. `None` when the data has no safe spot.
fn safe_destination(
    server: &GameServer,
    player: &crate::world::PlayerRef,
    area: &SiegeArea,
    instance_id: u32,
) -> Option<SCTeleportUnitPacket> {
    let portal = crate::systems::npcs::portal::closest_return_portal(server, player);
    if portal.id != 0
        && [portal.x, portal.y, portal.z]
            .into_iter()
            .all(f32::is_finite)
        && !area.inside(instance_id, portal.x, portal.y)
    {
        let z = crate::systems::ai::collision::ground_height_or(
            server,
            instance_id,
            portal.x,
            portal.y,
            portal.z,
            || portal.z,
        );
        if z.is_finite() {
            return Some(SCTeleportUnitPacket {
                reason: TELEPORT_REASON_MOVE_TO_LOCATION,
                error_message: error_message::SIEGE_NOT_PARTICIPANT,
                x: portal.x,
                y: portal.y,
                z,
                z2: portal.z_rot.to_radians(),
            });
        }
    }
    let mwp = player.data().main_world_position?;
    if mwp.instance_id != instance_id
        || ![mwp.x, mwp.y, mwp.z].into_iter().all(f32::is_finite)
        || area.inside(instance_id, mwp.x, mwp.y)
    {
        return None;
    }
    Some(SCTeleportUnitPacket {
        reason: TELEPORT_REASON_MOVE_TO_LOCATION,
        error_message: error_message::SIEGE_NOT_PARTICIPANT,
        x: mwp.x,
        y: mwp.y,
        z: mwp.z,
        z2: mwp.rot_z,
    })
}

/// Move every online, living non-participant out of an active siege area. A teleport that is
/// already pending is skipped: the server position only follows the client's acknowledgement,
/// so re-sending would stack packets while the client loads.
///
/// `Character.ForceDismount` runs first (the same `PrefabChanged` call the move guard, the
/// jail move and the GM teleport use), so a rider / driver / transfer passenger is not moved
/// while attached to the mount or vehicle. Called once a second by the participant tick, and
/// by the tests.
pub fn evict_non_participants(server: &GameServer) {
    let real_now = chrono::Utc::now().timestamp();
    let calendar_now = server.systems.housing.calendar.now();
    for player in server.world.all_characters() {
        let (online, dead, teleporting, transform) = {
            let c = player.data();
            (
                c.is_online,
                c.unit.hp <= 0,
                c.disabled_set_position,
                c.unit.transform,
            )
        };
        if !online || dead || teleporting {
            continue;
        }
        let Some(zone) = server
            .game_data
            .zones
            .zone_by_key(transform.zone_id)
            .map(|zone| zone.group_id)
        else {
            continue;
        };
        let Some(area) = server.systems.housing.siege_areas.get(zone) else {
            continue;
        };
        if !area.active_at(real_now, calendar_now)
            || area.tower.instance_id != transform.instance_id
        {
            continue;
        }
        let object = WorldObject::from(&player);
        let Some(world) = server.world.world_of(&object) else {
            continue;
        };
        let position = world.world_pos_rot(&object);
        if !area.inside(transform.instance_id, position.x, position.y) {
            continue;
        }
        if server
            .systems
            .housing
            .siege_areas
            .is_participant(zone, player.character_id)
        {
            continue;
        }
        let Some(packet) = safe_destination(server, &player, &area, transform.instance_id) else {
            tracing::warn!(
                "Dominion {zone}: no safe point outside the siege area for '{}' ({}); not moved",
                player.data().unit.name,
                player.character_id
            );
            continue;
        };
        tracing::info!(
            "Dominion {zone}: moving '{}' ({}) out of the siege area: not a participant",
            player.data().unit.name,
            player.character_id
        );
        crate::systems::vehicles::force_dismount(
            server,
            &player,
            crate::systems::vehicles::slaves::attach_reason::PREFAB_CHANGED,
        );
        session::send_teleport_unit(&player, packet);
    }
}
