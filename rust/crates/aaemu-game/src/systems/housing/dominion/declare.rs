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
        .settings(&server.config.housing, server.config.world.profile);
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
                    server
                        .systems
                        .housing
                        .headquarters
                        .bind_materialised(server, &state);
                    let _ = server
                        .systems
                        .housing
                        .headquarters
                        .spawn_defense_hq(server, &state);
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

/// The camp's spawn art template (`siege.md` 1.4: doodad 4307 "siege declaration").
pub const CAMP_DOODAD_TEMPLATE: u32 = 4307;
/// Skill 16918 "Declare siege" (`use_skill_id` of the six `siege_zones.declare_item_id` items).
pub const DECLARE_SKILL: u32 = 16_918;

fn refuse(player: &crate::world::PlayerRef, error: i16) -> Result<(), i16> {
    crate::systems::social::util::send_error(player, error);
    Err(error)
}

/// Validate an item cast of skill 16918 and record the declaration (WP-SD2b).
///
/// The skill's source item was already taken by the cast path; a refusal returns `Err` so
/// `spawn_doodad` cancels the effect, which makes the cast path refund the item. Checks and
/// the declaration run under the housing operation lock (the calendar tick takes it too), so
/// the declaration cannot race the period it was accepted in. The order of the refusals is
/// this server's policy, the ids are the client's (ui_texts).
///
/// Capture-pending (documented in PORTING.md): the placement thresholds behind 625 (near a
/// resurrection point), 633 (near a house), 661 (near another zone) and 741 (obstacle), and
/// the "siege too imminent" window behind 760, are UNKNOWN. They are not enforced here (no
/// refusal today); 459 is used for a missing SiegeMaster policy although 581 is the other
/// candidate text. The in-game cancel trigger is UNKNOWN; cancelling is a GM command.
pub fn handle_camp_use(
    server: &GameServer,
    args: &crate::systems::skills::effects::EffectArgs<'_>,
    pos: crate::models::transform::PositionAndRotation,
) -> Result<(), i16> {
    use crate::systems::economy::error_message as error;
    use crate::systems::skills::types::SkillCaster;

    let Some(player) = args
        .caster
        .as_ref()
        .and_then(crate::world::WorldObject::as_player)
    else {
        return Err(error::SIEGE_DECLARE_NO_DOMINION);
    };
    let SkillCaster::Item {
        item_template_id, ..
    } = args.caster_caster
    else {
        return Err(error::SIEGE_DECLARE_BAD_ZONE);
    };
    let _items = server.systems.economy.operation_scope();
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = server.systems.housing.calendar.now();
    let (online, alive, expedition, transform) = {
        let c = player.data();
        (
            c.is_online,
            c.unit.hp > 0 && !c.skill_char.skill_cancelled,
            c.unit.expedition_id,
            c.unit.transform,
        )
    };
    if !online || !alive {
        return Err(error::SIEGE_DECLARE_NO_DOMINION);
    }
    let Some(zone) = server
        .game_data
        .zones
        .zone_by_key(transform.zone_id)
        .map(|zone| zone.group_id)
    else {
        return refuse(player, error::SIEGE_DECLARE_NO_DOMINION);
    };
    let Some(state) = server.systems.housing.dominions.get(zone) else {
        return refuse(player, error::SIEGE_DECLARE_NO_DOMINION);
    };
    if state.owner_expedition_id == 0 {
        return refuse(player, error::SIEGE_DECLARE_NO_DOMINION);
    }
    if expedition == 0
        || expedition
            != server
                .expeditions
                .expedition_of_character(player.character_id)
    {
        return refuse(player, error::SIEGE_DECLARE_NO_DOMINION);
    }
    if expedition == state.owner_expedition_id {
        return refuse(player, error::SIEGE_DECLARE_OWN_ZONE);
    }
    if server
        .game_data
        .systems
        .housing
        .castles
        .siege_zones
        .get(&zone)
        .is_none_or(|schedule| schedule.declare_item_id != item_template_id)
    {
        return refuse(player, error::SIEGE_DECLARE_BAD_ZONE);
    }
    // The declared walk exists only with the weekly calendar (the feature ships behind it).
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    let calendar = SiegeCalendar::from_castles(&server.game_data.systems.housing.castles);
    let Some(view) = calendar.view_for_declaration(zone, now, &settings.timezone, None, 0) else {
        return refuse(player, error::SIEGE_DECLARE_BAD_PERIOD);
    };
    let Some(cycle) = view.cycle else {
        return refuse(player, error::SIEGE_DECLARE_BAD_PERIOD);
    };
    if view.period != calendar::PERIOD_DECLARE {
        return refuse(player, error::SIEGE_DECLARE_BAD_PERIOD);
    }
    // Range is the authored `radius_declare` around the live lodestone (250/450 m); the
    // horizontal distance matches the HQ rule (`headquarters.rs`).
    let Some(tower) = server.systems.housing.manager.by_id(state.tower.house_id) else {
        return refuse(player, error::SIEGE_DECLARE_NO_DOMINION);
    };
    let tower_object = crate::world::WorldObject::from(&tower);
    let Some(world) = server.world.world_of(&tower_object) else {
        return refuse(player, error::SIEGE_DECLARE_NO_DOMINION);
    };
    let origin = world.world_pos_rot(&tower_object);
    let radius = server
        .game_data
        .systems
        .housing
        .castles
        .guard_tower_settings
        .get(&state.tower.guard_tower_setting_id)
        .map(|setting| f64::from(setting.radius_declare))
        .unwrap_or(250.0);
    let distance2 = (f64::from(pos.x) - f64::from(origin.x)).powi(2)
        + (f64::from(pos.y) - f64::from(origin.y)).powi(2);
    if !distance2.is_finite() || distance2 < radius.powi(2) {
        return refuse(player, error::SIEGE_DECLARE_TOO_NEAR);
    }
    // SiegeMaster rank policy (ui label 2385). 459 is the chosen text; 581 is the other
    // candidate (capture-pending).
    let policy_allowed = match server.expeditions.get(expedition) {
        Some(guild) if !guild.is_disbanded => guild
            .member(player.character_id)
            .and_then(|member| guild.policy_by_role(member.role))
            .is_some_and(|policy| policy.siege_master),
        _ => false,
    };
    if !policy_allowed {
        return refuse(player, error::SIEGE_MASTER_ONLY);
    }
    let persisted_faction = crate::db_character::persisted_faction(&player.data());
    let pirate =
        server.expeditions.get(expedition).is_some_and(|guild| {
            guild.faction.mother_id == crate::data::world_data::factions::PIRATE
        }) || persisted_faction == crate::data::world_data::factions::PIRATE;
    if pirate {
        return refuse(player, error::SIEGE_DECLARE_PIRATE);
    }
    // A guild already fighting in another unfinished siege cannot declare (639).
    let participating = server.systems.housing.dominions.all().iter().any(|other| {
        other.siege.as_ref().is_some_and(|siege| {
            siege.status != crate::systems::housing::dominion::SiegeStatus::Finished
                && (siege.attacker_expedition_id == expedition
                    || siege.defender_expedition_id == expedition)
        })
    });
    if participating {
        return refuse(player, error::SIEGE_ALREADY_PARTICIPATING);
    }
    match server.systems.housing.dominions.declare(
        zone,
        expedition,
        cycle.auction_start,
        now,
        0,
        (pos.x, pos.y, pos.z),
    ) {
        Ok(_) => Ok(()),
        Err(crate::systems::housing::dominion::DominionError::AlreadyDeclared) => {
            refuse(player, error::SIEGE_DECLARED_ALREADY)
        }
        Err(crate::systems::housing::dominion::DominionError::DeclaredByOther) => {
            refuse(player, error::SIEGE_DECLARED_ALREADY_BY_OTHER)
        }
        Err(_) => refuse(player, error::SIEGE_ALREADY_PARTICIPATING),
    }
}

/// The camp spawned: store the real doodad id/position in the declaration and announce the
/// offense team. Called by the SpawnDoodad effect after the doodad exists.
pub fn attach_spawned_camp(server: &GameServer, zone: u32, doodad: &crate::world::DoodadRef) {
    let (obj_id, position) = {
        let d = doodad.data();
        (doodad.obj_id, d.transform)
    };
    if let Ok(Some(state)) = server.systems.housing.dominions.bind_declaration_hq(
        zone,
        obj_id,
        (position.x, position.y, position.z),
    ) {
        // The camp is the offense HQ from acceptance (WP-SD2c).
        if let Err(error) = server
            .systems
            .housing
            .headquarters
            .bind_declaration(server, zone, doodad)
        {
            tracing::warn!("Dominion {zone}: the camp cannot become the offense HQ: {error}");
        }
        crate::systems::social::dominion::broadcast_siege_declared(server, &state);
        crate::systems::social::dominion::broadcast_phase_change(server, &state);
    }
}

/// Cancel the declaration of `zone` (GM command; the in-game trigger is capture-pending).
pub fn cancel(
    server: &GameServer,
    zone: u32,
) -> Result<(), crate::systems::housing::dominion::DominionError> {
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let state = server
        .systems
        .housing
        .dominions
        .get(zone)
        .ok_or(crate::systems::housing::dominion::DominionError::Missing)?;
    let Some(declaration) = state.declaration else {
        return Err(crate::systems::housing::dominion::DominionError::NotDeclared);
    };
    let state = server
        .systems
        .housing
        .dominions
        .cancel_declaration(zone, declaration.attacker_expedition_id)?;
    crate::systems::social::dominion::broadcast_siege_declared(server, &state);
    crate::systems::social::dominion::broadcast_phase_change(server, &state);
    Ok(())
}
