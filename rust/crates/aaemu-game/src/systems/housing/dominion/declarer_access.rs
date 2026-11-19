//! `Housing.OpenCastleZonesForDeclarers`: the one narrow closed-zone exemption for a guild
//! captain who is declaring a siege (skill 16918, the 10 s "Siege Camp" cast).
//!
//! The six castle zone groups are authored `closed`, so `world::zone_access` evicts every
//! non-GM from them after five warnings (10 s) -- exactly the length of the camp cast, which
//! the declaration's own placement rules (`declare::handle_camp_use`: outside the lodestone's
//! `radius_declare`, clear of houses, farms, other zone groups and resurrection points) force
//! to happen inside the castle zone. `OpenCastleZonesForParticipants` cannot help: it needs an
//! active siege, and none exists while the declaration is being made. This module answers the
//! one question `zone_access` asks for that case: does `player` hold a declarer permit for
//! `zone` right now?
//!
//! A permit exists only while **all** of these hold (cheapest first, so nothing scans an
//! inventory for a character that cannot qualify):
//!
//! * the switch is on, the zone's group has an owned dominion, and the world instance is that
//!   dominion's own tower instance (so a same-template copy elsewhere never qualifies);
//! * the character belongs to a live guild that is not the owner, is not pirate, and the
//!   character's *current* rank carries the siege-master policy -- the same three checks
//!   `declare::handle_camp_use` runs ([`declare::has_siege_master_rank`],
//!   [`declare::is_pirate_declarer`]);
//! * the weekly calendar is on and the cycle's authored **declare** period is running (an
//!   accepted declaration keeps describing its own cycle, the way the calendar tick does);
//! * the registered tower is a live object and still the dominion's tower;
//! * one of two bases holds:
//!   * [`PermitBasis::CampItem`]: no declaration, no unfinished siege on the territory, the
//!     guild fights in no other unfinished siege (`declare::guild_in_unfinished_siege`), and the
//!     character owns the unexpired, unlocked camp item of *this zone group*
//!     (`siege_zones.declare_item_id`, an item whose template's use skill is 16918) in its bag
//!     or equipment -- the places the cast can take it from;
//!   * [`PermitBasis::AcceptedDeclaration`]: the territory's accepted declaration is this
//!     character's guild's, for the cycle now running, and the character was already admitted
//!     on a camp item in that very cycle (the caller's `admitted` mark). The cast takes the
//!     item when it completes, so without this the captain would be evicted the moment the
//!     declaration is accepted. It is "stay" only: it is never used to enter.
//!
//! Everything the declaration itself enforces (placement, distance, window, cost, rank, one
//! claim per territory) stays in `declare::handle_camp_use` and is unchanged; this only decides
//! who may stand in the zone. The permit ends, and the existing once-a-second participant
//! sweep (`zone_access::recheck_siege_participants`) starts the ordinary eviction timer, when
//! the declare period ends (the declaration materialises into the warmup siege or is
//! dropped), the declaration is cancelled, the item is used up, sold or expires without a
//! declaration, the rank or the guild membership changes, or the territory changes hands.

use crate::models::container::slot_type;
use crate::models::item::ITEM_FLAG_SECURE;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::item_timers::expired_at;
use crate::world::{PlayerRef, WorldObject};

use super::calendar::{self, SiegeCalendar};
use super::declare;
use super::runtime;
use super::state::SiegeStatus;

/// Why a permit exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermitBasis {
    CampItem,
    AcceptedDeclaration,
}

/// A held permit: the territory and the calendar cycle (`Cycle::auction_start`) it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permit {
    pub zone_group_id: u32,
    pub cycle: i64,
    pub basis: PermitBasis,
}

/// Whether `player` holds a declarer permit for `zone` in world `instance`.
///
/// `admitted` is the `(zone group, cycle)` the character was earlier admitted for on a camp
/// item; pass `None` when the question is about entering (portal, teleport) so only the camp
/// item can open the zone, and the stored mark when the character already stands there.
pub fn permit(
    server: &GameServer,
    player: &PlayerRef,
    instance: u32,
    zone: u32,
    admitted: Option<(u32, i64)>,
) -> Option<Permit> {
    if !server.config.housing.open_castle_zones_for_declarers {
        return None;
    }
    let group = server.game_data.zones.zone_by_key(zone)?.group_id;
    if group == 0 {
        return None;
    }
    let state = server.systems.housing.dominions.get(group)?;
    if state.owner_expedition_id == 0 || state.tower.instance_id != instance {
        return None;
    }
    let (character_id, expedition) = {
        let c = player.data();
        (player.character_id, c.unit.expedition_id)
    };
    if expedition == 0
        || expedition == state.owner_expedition_id
        || server.expeditions.expedition_of_character(character_id) != expedition
        || !declare::has_siege_master_rank(server, expedition, character_id)
        || declare::is_pirate_declarer(server, player, expedition)
    {
        return None;
    }
    // The role is real; now the calendar. Unlike a participant, a declarer is admitted before
    // any siege record exists, so the authored declare period is the gate.
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    if !settings.enabled {
        return None;
    }
    let calendar = SiegeCalendar::from_castles(&server.game_data.systems.housing.castles);
    let declaration = state.declaration;
    let view = calendar.view_for_declaration(
        group,
        server.systems.housing.calendar.now(),
        &settings.timezone,
        declaration.map(|declaration| declaration.cycle_auction_start),
        0,
    )?;
    let cycle = view.cycle?.auction_start;
    if view.period != calendar::PERIOD_DECLARE {
        return None;
    }
    // A rival claim or a siege that is not finished closes the window for everyone else.
    let basis = match declaration {
        Some(declaration) => {
            if declaration.attacker_expedition_id != expedition
                || declaration.cycle_auction_start != cycle
                || admitted != Some((group, cycle))
            {
                return None;
            }
            PermitBasis::AcceptedDeclaration
        }
        None => {
            if state
                .siege
                .as_ref()
                .is_some_and(|siege| siege.status != SiegeStatus::Finished)
                || declare::guild_in_unfinished_siege(server, expedition)
            {
                return None;
            }
            PermitBasis::CampItem
        }
    };
    let tower = server.systems.housing.manager.by_id(state.tower.house_id)?;
    if !runtime::is_live_object(server, &WorldObject::from(&tower))
        || runtime::tower_identity(server, &tower) != Some((group, state.tower))
    {
        return None;
    }
    if basis == PermitBasis::CampItem {
        let item = server
            .game_data
            .systems
            .housing
            .castles
            .siege_zones
            .get(&group)?
            .declare_item_id;
        if !holds_camp_item(server, player, item) {
            return None;
        }
    }
    Some(Permit {
        zone_group_id: group,
        cycle,
        basis,
    })
}

/// The character owns an unexpired, unlocked stack of the camp item in its bag or equipment
/// (the containers `take_cast_source_item` takes a cast item from). The template must really
/// carry the declare skill, so a mislabelled item row opens nothing.
fn holds_camp_item(server: &GameServer, player: &PlayerRef, template_id: u32) -> bool {
    if template_id == 0
        || server
            .game_data
            .items
            .template(template_id)
            .is_none_or(|template| template.use_skill_id != declare::DECLARE_SKILL)
    {
        return false;
    }
    let now = chrono::Utc::now().timestamp();
    let inv = Inv::of(player);
    [slot_type::INVENTORY, slot_type::EQUIPMENT]
        .into_iter()
        .filter_map(|slot| inv.get(slot))
        .any(|container| {
            container
                .lock()
                .expect("container poisoned")
                .items
                .iter()
                .any(|item| {
                    item.template_id == template_id
                        && item.count > 0
                        && item.owner_id == u64::from(player.character_id)
                        && !item.has_flag(ITEM_FLAG_SECURE)
                        && !expired_at(item, now)
                })
        })
}
