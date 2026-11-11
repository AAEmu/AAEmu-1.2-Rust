//! Siege participant seals: the Guardian Seal (`siege_zones.defense_ticket_id`) and the
//! Advance Seal (`offense_ticket_id`).
//!
//! Evidence and policy: the 1.2 data gives every castle zone one defense and one offense
//! seal item (`items` 21313-21320 / 28170-28173, category 120, not sellable, `price` 0,
//! `honor_price` 0, 240-minute absolute lifetime). The owner's period source for the siege
//! flow (IGN, 2014-09-26, "Castles"; Total ArcheAge, 2014-11-20, grade C) says the game
//! "selects from the eligible participants for Attackers and Defenders up to the allotted
//! limits" — a selected participant is marked with their side's seal. There is no 1.2 CS
//! packet for it (siege.md 0: the client has no siege-specific CS), so this server hands the
//! seal when the roster accepts the character and takes it back when the siege ends.
//!
//! The 240-minute item lifetime is the retail backstop: a character who is offline when the
//! siege ends keeps the seal only until that timer expires. The runtime grant set below is
//! deliberately not persisted; after a restart the lifetime is the only owner of the cleanup,
//! and a seal granted by the defence-reinforcement skill 16975 (`GetSiegeTicket`) is never
//! removed by this module (it is not in the set).

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::housing::dominion::state::{DominionState, SiegeSide, SiegeStatus};
use crate::world::PlayerRef;

/// Runtime-only record of the seals this server handed to a roster entry.
///
/// Keyed by zone group and character id; the side is kept so reconcile can compare it with the
/// current roster entry without re-reading the item template.
#[derive(Debug, Default)]
pub struct SealRuntime {
    granted: Mutex<BTreeMap<(u32, u32), SiegeSide>>,
}

impl SealRuntime {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(u32, u32), SiegeSide>> {
        self.granted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn track(&self, zone_group_id: u32, character_id: u32, side: SiegeSide) {
        self.lock().insert((zone_group_id, character_id), side);
    }

    fn untrack(&self, zone_group_id: u32, character_id: u32) {
        self.lock().remove(&(zone_group_id, character_id));
    }

    fn tracked(&self) -> Vec<((u32, u32), SiegeSide)> {
        self.lock()
            .iter()
            .map(|(key, side)| (*key, *side))
            .collect()
    }
}

/// The zone's seal item for `side`, when the authored row names one.
pub fn seal_item(server: &GameServer, zone_group_id: u32, side: SiegeSide) -> Option<u32> {
    let zone = server
        .game_data
        .systems
        .housing
        .castles
        .siege_zones
        .get(&zone_group_id)?;
    let item = match side {
        SiegeSide::Attacker => zone.offense_ticket_id,
        SiegeSide::Defender => zone.defense_ticket_id,
    };
    (item != 0).then_some(item)
}

fn online_player(server: &GameServer, character_id: u32) -> Option<PlayerRef> {
    server
        .world
        .character_by_id(character_id)
        .filter(|player| player.data().is_online)
}

fn holds(server: &GameServer, player: &PlayerRef, template_id: u32) -> bool {
    server
        .systems
        .skills
        .ext
        .items()
        .item_count(server, player, template_id)
        > 0
}

/// Hand the side's seal to one accepted roster entry. A character that already holds the item
/// (the defence-reinforcement skill 16975 grants the same defence seal) gets no duplicate and
/// is not tracked; an offline character cannot be given one and gets none.
pub fn grant_for_participant(
    server: &GameServer,
    state: &DominionState,
    character_id: u32,
    side: SiegeSide,
) -> bool {
    let Some(template_id) = seal_item(server, state.zone_group_id, side) else {
        return false;
    };
    let Some(player) = online_player(server, character_id) else {
        return false;
    };
    if holds(server, &player, template_id) {
        return false;
    }
    let acquired = server.systems.skills.ext.items().try_add_new_item(
        server,
        &player,
        task_type::SKILL_EFFECT_GAIN_ITEM,
        template_id,
        1,
        0,
    );
    if !acquired {
        tracing::warn!(
            "Dominion {}: could not hand seal {template_id} to character {character_id}",
            state.zone_group_id
        );
        return false;
    }
    server
        .systems
        .housing
        .siege_seals
        .track(state.zone_group_id, character_id, side);
    true
}

/// Hand the seals to every online member of a just-started roster.
pub fn grant_roster(server: &GameServer, state: &DominionState) {
    let Some(siege) = state.siege.as_ref() else {
        return;
    };
    for participant in siege.participants.values() {
        grant_for_participant(server, state, participant.character_id, participant.side);
    }
}

fn take_one(server: &GameServer, player: &PlayerRef, template_id: u32) {
    server
        .systems
        .skills
        .ext
        .items()
        .consume_item_template(server, player, template_id, 1);
}

/// Remove the seals this server handed out whose roster entry is gone. Runs once a second from
/// the participant tick; the item's own 240-minute lifetime covers offline characters and
/// restarts. A tracked character that is offline while the siege still runs keeps the record.
pub fn reconcile(server: &GameServer) {
    super::siege_area::refresh(server);
    for ((zone_group_id, character_id), side) in server.systems.housing.siege_seals.tracked() {
        let state = server.systems.housing.dominions.get(zone_group_id);
        // A roster entry keeps its seal through the whole record, including the warmup (and a
        // GM-scheduled start still in the future); only the end of the siege (or leaving the
        // roster / a side change) ends the entitlement.
        let still_participating = state.as_ref().is_some_and(|state| {
            state.siege.as_ref().is_some_and(|siege| {
                siege.status != SiegeStatus::Finished
                    && siege
                        .participants
                        .get(&character_id)
                        .is_some_and(|participant| participant.side == side)
            })
        });
        if still_participating {
            continue;
        }
        let Some(player) = online_player(server, character_id) else {
            // Nothing to take from an offline character: the authored lifetime ends the item.
            if state.as_ref().is_none_or(|state| {
                state
                    .siege
                    .as_ref()
                    .is_none_or(|siege| siege.status == SiegeStatus::Finished)
            }) {
                server
                    .systems
                    .housing
                    .siege_seals
                    .untrack(zone_group_id, character_id);
            }
            continue;
        };
        if let Some(template_id) = seal_item(server, zone_group_id, side) {
            take_one(server, &player, template_id);
        }
        server
            .systems
            .housing
            .siege_seals
            .untrack(zone_group_id, character_id);
    }
}

/// The item count of a seal, for tests and diagnostics.
#[allow(dead_code)]
pub fn seal_count(player: &PlayerRef, template_id: u32) -> i32 {
    Inv::of(player).items_count_unlocked(template_id, -1)
}
