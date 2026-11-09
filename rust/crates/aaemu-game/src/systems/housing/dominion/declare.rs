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

/// The zone group of a spot, `None` when it has no authored zone (or zone group 0, which the
/// 1.2 client's dominion check also refuses).
fn zone_group_at(
    server: &GameServer,
    world: &crate::world::WorldInstance,
    x: f32,
    y: f32,
) -> Option<u32> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let group = server
        .game_data
        .zones
        .zone_by_key(world.zone_id_at(x, y))?
        .group_id;
    (group != 0).then_some(group)
}

/// The spots the distance checks inspect: the declared point, plus rings at fractions of
/// `margin` in 16 directions. The 1.2 server's exact border query is UNKNOWN, so the ring
/// sampling is an INFERRED approximation of "within `margin` of another zone".
fn sample_points(x: f32, y: f32, margin: f32) -> Vec<(f32, f32)> {
    let mut points = vec![(x, y)];
    if margin.is_finite() && margin > 0.0 {
        for step in 1..=4 {
            let radius = margin * step as f32 / 4.0;
            for direction in 0..16 {
                let angle = std::f32::consts::TAU * direction as f32 / 16.0;
                points.push((x + radius * angle.cos(), y + radius * angle.sin()));
            }
        }
    }
    points
}

/// 633: the spot is inside a house plot, or within `margin` metres of its edge. The server's
/// house plot is the `garden_radius` square (`GetHouseAtLocation`); the client's shrunken
/// collision box needs render bounds the server does not have.
fn house_near(
    server: &GameServer,
    world: &crate::world::WorldInstance,
    x: f32,
    y: f32,
    margin: f32,
) -> bool {
    if !x.is_finite() || !y.is_finite() {
        return true;
    }
    if !margin.is_finite() || margin <= 0.0 {
        return crate::systems::housing::house_at_location_in_world(server, world.id, x, y)
            .is_some();
    }
    server.systems.housing.manager.all().iter().any(|house| {
        let Some(template) = crate::systems::housing::manager::template_of(server, house) else {
            return false;
        };
        let (hx, hy, instance) = {
            let h = house.data();
            (
                h.unit.transform.x,
                h.unit.transform.y,
                h.unit.transform.instance_id,
            )
        };
        if instance != world.id {
            return false;
        }
        let radius = template.garden_radius.max(0.0);
        let dx = ((x - hx).abs() - radius).max(0.0);
        let dy = ((y - hy).abs() - radius).max(0.0);
        dx * dx + dy * dy <= margin * margin
    })
}

/// 661: the spot is outside the besieged dominion's zone group, or another zone group is
/// within `margin` metres of it.
fn other_zone_group_within(
    server: &GameServer,
    world: &crate::world::WorldInstance,
    x: f32,
    y: f32,
    zone_group_id: u32,
    margin: f32,
) -> bool {
    if zone_group_at(server, world, x, y) != Some(zone_group_id) {
        return true;
    }
    if !margin.is_finite() || margin <= 0.0 {
        return false;
    }
    sample_points(x, y, margin).into_iter().any(|(sx, sy)| {
        zone_group_at(server, world, sx, sy).is_some_and(|group| group != zone_group_id)
    })
}

/// 633 farmland: the spot is inside a public farm area, or within `margin` metres of one.
/// The public farm areas are the `PublicFarmManager._farmZones` sub zone polygons, the data
/// the server already uses to decide whether a placement is a common-farm planting. The sub
/// zone test is a point-in-polygon test, so "within `margin`" is sampled like the 661 rule.
fn public_farm_near(
    server: &GameServer,
    world: &crate::world::WorldInstance,
    x: f32,
    y: f32,
    margin: f32,
) -> bool {
    if !x.is_finite() || !y.is_finite() {
        return true;
    }
    let farm_zones = &server.game_data.systems.npcs.public_farm.farm_zones;
    if farm_zones.is_empty() {
        return false;
    }
    let margin = if margin.is_finite() {
        margin.max(0.0)
    } else {
        0.0
    };
    sample_points(x, y, margin).into_iter().any(|(sx, sy)| {
        crate::systems::npcs::subzone::get_sub_zone_by_position(&server.game_data, world, sx, sy)
            .into_iter()
            .any(|sub_zone_id| farm_zones.contains_key(&sub_zone_id))
    })
}

/// Whether a doodad template is a farm product: one of its phase funcs is
/// `DoodadFuncHouseFarm` (the crop / livestock growth phase, 368 rows / 147 templates in the
/// 1.2 data). The same templates are also wild gathering nodes (clover, azalea, mushroom,
/// rubber tree, ...), so the caller must require a player-planted owner.
fn template_is_house_farm(server: &GameServer, template_id: u32) -> bool {
    let doodads = &server.game_data.systems.npcs.doodad;
    let Some(template) = doodads.template(template_id) else {
        return false;
    };
    template.func_groups.iter().any(|group| {
        doodads
            .phase_funcs
            .get(&group.id)
            .is_some_and(|funcs| funcs.iter().any(|f| f.func_type == "DoodadFuncHouseFarm"))
    })
}

/// 633 farmland: a player-planted farm doodad lies within `margin` metres of the spot. Only
/// doodads owned by a character or a house count: the crop / livestock templates are also
/// the wild gathering nodes (`DoodadFuncHouseFarm`), and those static spawns are not
/// farmland. The planted ones are the common-farm plantings (carrying a `farm_type`) and the
/// private house-garden plantings (owner type `HOUSING`).
///
/// Only the sectors within `margin` of the spot are scanned, and the distance is checked
/// before the template lookup.
fn farm_doodad_near(
    server: &GameServer,
    world: &crate::world::WorldInstance,
    x: f32,
    y: f32,
    margin: f32,
) -> bool {
    if !x.is_finite() || !y.is_finite() {
        return true;
    }
    let margin = if margin.is_finite() {
        margin.max(0.0)
    } else {
        0.0
    };
    let region = crate::data::world_templates::REGION_SIZE as f32;
    let (min_x, max_x) = (
        ((x - margin) / region).floor() as i32,
        ((x + margin) / region).floor() as i32,
    );
    let (min_y, max_y) = (
        ((y - margin) / region).floor() as i32,
        ((y + margin) / region).floor() as i32,
    );
    let margin2 = f64::from(margin) * f64::from(margin);
    for sx in min_x..=max_x {
        for sy in min_y..=max_y {
            if !world.template.valid_region(sx, sy) {
                continue;
            }
            for object in world.objects_in_region((sx, sy)) {
                let crate::world::WorldObject::Doodad(doodad) = object else {
                    continue;
                };
                let (owner_id, owner_type) = {
                    let d = doodad.data();
                    (d.owner_id, d.owner_type)
                };
                if owner_id == 0
                    || !matches!(
                        owner_type,
                        crate::models::doodad::doodad_owner_type::CHARACTER
                            | crate::models::doodad::doodad_owner_type::HOUSING
                    )
                {
                    continue;
                }
                let position =
                    world.world_pos_rot(&crate::world::WorldObject::Doodad(doodad.clone()));
                let dx = f64::from(x) - f64::from(position.x);
                let dy = f64::from(y) - f64::from(position.y);
                if dx * dx + dy * dy > margin2 {
                    continue;
                }
                let (template_id, farm_type) = {
                    let d = doodad.data();
                    (d.template_id, d.farm_type)
                };
                if farm_type != crate::systems::npcs::public_farm::farm_group_kind::INVALID
                    || template_is_house_farm(server, template_id)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// 625: a resurrection point lies within `margin` metres (horizontal) of the spot. The
/// server's resurrection points are the `Data/Portal/respawns.json` portals
/// (`PortalTables::respawns`, `PortalManager.Load`); their zone is always computed against
/// `main_world` (`portal::respawn_zone`), so the rule only applies there.
fn resurrection_near(
    server: &GameServer,
    world: &crate::world::WorldInstance,
    x: f32,
    y: f32,
    margin: f32,
) -> bool {
    if !x.is_finite() || !y.is_finite() {
        return true;
    }
    if world.template.id != server.game_data.worlds.default_world_template_id() {
        return false;
    }
    let margin = if margin.is_finite() {
        margin.max(0.0)
    } else {
        0.0
    };
    server
        .game_data
        .systems
        .npcs
        .portal
        .respawns
        .portals
        .iter()
        .any(|point| {
            let dx = f64::from(x) - f64::from(point.x);
            let dy = f64::from(y) - f64::from(point.y);
            dx * dx + dy * dy <= f64::from(margin) * f64::from(margin)
        })
}

/// A slave or mate the caster owns (the camp spot is one metre in front of the caster, so its
/// own mount must not refuse the placement).
fn owned_companion(world: &crate::world::WorldInstance, character_id: u32, obj_id: u32) -> bool {
    world.object(obj_id).is_some_and(|object| match object {
        crate::world::WorldObject::Slave(slave) => slave.data().unit.owner_id == character_id,
        crate::world::WorldObject::Mate(mate) => mate.data().unit.owner_id == character_id,
        _ => false,
    })
}

/// House placement's entity mask (`FUN_39335860`, `systems::housing::placement`).
const OBSTACLE_MASK: u32 = 0x1f;

/// 741: the collision query around the spot reports an obstacle. The query is the house
/// builder's (`box_obstacles`); houses are the 633 rule, and vegetation, brush, the caster and
/// its own mount are not obstacles. Terrain is reported only when the query cannot complete.
fn obstacle_near(
    server: &GameServer,
    world: &crate::world::WorldInstance,
    player: &crate::world::PlayerRef,
    pos: (f32, f32, f32),
    radius: f32,
) -> bool {
    if !radius.is_finite() || radius <= 0.0 {
        return false;
    }
    use crate::systems::ai::collision::world::{ObjKind, Owner};
    let Some(found) = crate::systems::ai::collision::box_obstacles(
        server,
        world.id,
        [pos.0, pos.1, pos.2],
        [radius, radius, radius],
        0.0,
        OBSTACLE_MASK,
    ) else {
        return false;
    };
    let character_id = player.character_id;
    found.iter().any(|(owner, _)| match owner {
        Owner::Terrain => true,
        Owner::Object {
            kind: ObjKind::House,
            ..
        } => false,
        Owner::Object { obj_id, .. } => {
            *obj_id != player.obj_id && !owned_companion(world, character_id, *obj_id)
        }
        Owner::Brush { .. } | Owner::Vegetation { .. } | Owner::Sentinel => false,
    })
}

/// Validate an item cast of skill 16918 and record the declaration (WP-SD2b).
///
/// The skill's source item was already taken by the cast path; a refusal returns `Err` so
/// `spawn_doodad` cancels the effect, which makes the cast path refund the item. Checks and
/// the declaration run under the housing operation lock (the calendar tick takes it too), so
/// the declaration cannot race the period it was accepted in. The order of the refusals is
/// this server's policy, the ids are the client's (ui_texts).
///
/// The placement refusals 633 (house or farmland), 661 (another zone group), 625
/// (resurrection point, behind a config switch that defaults on) and 741 (obstacles) run
/// after the authored `radius_declare` with the `Housing.SiegeDeclare*` thresholds. GameMeca
/// 2013-04-22 (grade C) gives "about 100 m" from a resurrection point and "about 10 m" from
/// houses (incl. farmland) for 625/633; 661 and 741 are INFERRED (PORTING.md). The "siege
/// too imminent" window behind 760 is UNKNOWN and not enforced; 459 is used for a missing
/// SiegeMaster policy although 581 is the other candidate text. The in-game cancel trigger
/// is UNKNOWN; cancelling is a GM command.
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
    // The placement rules act on the camp's real position, which is one metre in front of
    // the caster, in the caster's world.
    let Some(player_world) = server
        .world
        .world_of(&crate::world::WorldObject::from(player))
    else {
        return refuse(player, error::SIEGE_DECLARE_NO_DOMINION);
    };
    let housing = &server.config.housing;
    if house_near(
        server,
        &player_world,
        pos.x,
        pos.y,
        housing.siege_declare_house_margin,
    ) || public_farm_near(
        server,
        &player_world,
        pos.x,
        pos.y,
        housing.siege_declare_farm_margin,
    ) || farm_doodad_near(
        server,
        &player_world,
        pos.x,
        pos.y,
        housing.siege_declare_farm_margin,
    ) {
        return refuse(player, error::SIEGE_CANNOT_DECLARE_NEAR_HOUSE);
    }
    if other_zone_group_within(
        server,
        &player_world,
        pos.x,
        pos.y,
        zone,
        housing.siege_declare_zone_margin,
    ) {
        return refuse(player, error::SIEGE_CANNOT_DECLARE_NEAR_OTHER_ZONE);
    }
    if housing.siege_declare_resurrect_check
        && resurrection_near(
            server,
            &player_world,
            pos.x,
            pos.y,
            housing.siege_declare_resurrect_margin,
        )
    {
        return refuse(player, error::SIEGE_CANNOT_DECLARE_NEAR_RESURRECT_POS);
    }
    if obstacle_near(
        server,
        &player_world,
        player,
        (pos.x, pos.y, pos.z),
        housing.siege_declare_obstacle_radius,
    ) {
        return refuse(player, error::SIEGE_CANNOT_DECLARE_OBSTACLE);
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
