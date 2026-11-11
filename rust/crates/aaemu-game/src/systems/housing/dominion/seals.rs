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
//! seal when the roster accepts the character and keeps it for the siege record.
//!
//! The 1-second [`reconcile`] sweep (a) re-grants a missing seal to every online roster member
//! of an unfinished siege — covering a participant who was offline at registration or at the
//! start, a GM registration of an offline character, a full bag whose first grant failed, and
//! the authored 240-minute expiry during a long siege — and (b) takes the seal back when the
//! record ends or the roster entry / side is gone. The grant is idempotent (`holds`), and one
//! seal-action lock covers the grant insert and the removal's participation/`holds` re-check,
//! so a leave and rejoin cannot race a removal into taking the rejoined seal. The runtime
//! grant set is deliberately not persisted; after a restart the item lifetime is the only
//! cleanup for a character who is offline.
//!
//! The authored 240-minute lifetime is the backstop for a character who is offline when the
//! siege ends; a seal granted by the defence-reinforcement skill 16975 (`GetSiegeTicket`) is
//! never removed by this module (it is not in the runtime set).

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
/// current roster entry without re-reading the item template. `action` serialises a grant with
/// a removal, so the removal's fresh participation + item check cannot race a rejoin's grant.
#[derive(Debug, Default)]
pub struct SealRuntime {
    granted: Mutex<BTreeMap<(u32, u32), SiegeSide>>,
    action: Mutex<()>,
}

impl SealRuntime {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(u32, u32), SiegeSide>> {
        self.granted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_action(&self) -> std::sync::MutexGuard<'_, ()> {
        self.action
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

/// Hand the side's seal to one accepted roster entry. The caller holds the seal-action lock.
/// A character that already holds the item (the defence-reinforcement skill 16975 grants the
/// same defence seal) gets no duplicate and is not tracked; an offline character gets none.
fn grant_locked(
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
        // The reconcile sweep retries every second (a full bag is the common cause).
        tracing::debug!(
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

/// Hand the side's seal to one accepted roster entry (the join-command path).
pub fn grant_for_participant(
    server: &GameServer,
    state: &DominionState,
    character_id: u32,
    side: SiegeSide,
) -> bool {
    let _action = server.systems.housing.siege_seals.lock_action();
    grant_locked(server, state, character_id, side)
}

/// Hand the seals to every online member of a just-started roster.
pub fn grant_roster(server: &GameServer, state: &DominionState) {
    let Some(siege) = state.siege.as_ref() else {
        return;
    };
    let _action = server.systems.housing.siege_seals.lock_action();
    for participant in siege.participants.values() {
        grant_locked(server, state, participant.character_id, participant.side);
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

fn still_entitled(
    server: &GameServer,
    zone_group_id: u32,
    character_id: u32,
    side: SiegeSide,
) -> bool {
    server
        .systems
        .housing
        .dominions
        .get(zone_group_id)
        .is_some_and(|state| {
            state.siege.as_ref().is_some_and(|siege| {
                siege.status != SiegeStatus::Finished
                    && siege
                        .participants
                        .get(&character_id)
                        .is_some_and(|participant| participant.side == side)
            })
        })
}

/// (Re)grant missing seals and remove ended ones. Runs once a second from the participant
/// tick; also rebuilds the per-zone siege-area cache the hostility rule reads.
pub fn reconcile(server: &GameServer) {
    super::siege_area::refresh(server);
    let _action = server.systems.housing.siege_seals.lock_action();
    // (a) Every online roster member of an unfinished siege holds its side's seal. A missing
    // item (offline at registration or at the start, a GM registration of an offline
    // character, a full bag whose first grant failed, or the 240-minute expiry during a long
    // siege) is handed again; `holds` keeps the repeated sweep idempotent.
    for state in server.systems.housing.dominions.all() {
        let Some(siege) = state
            .siege
            .as_ref()
            .filter(|siege| siege.status != SiegeStatus::Finished)
        else {
            continue;
        };
        for participant in siege.participants.values() {
            grant_locked(server, &state, participant.character_id, participant.side);
        }
    }
    // (b) Remove seals whose entitlement ended. The fresh participation re-read and the
    // `holds` check run under the same seal-action lock as a command's grant, so a leave and
    // rejoin between the sweep above and this take cannot lose the rejoined seal.
    for ((zone_group_id, character_id), side) in server.systems.housing.siege_seals.tracked() {
        let Some(player) = online_player(server, character_id) else {
            // Nothing to take from an offline character: the authored lifetime ends the item.
            if !still_entitled(server, zone_group_id, character_id, side) {
                server
                    .systems
                    .housing
                    .siege_seals
                    .untrack(zone_group_id, character_id);
            }
            continue;
        };
        if still_entitled(server, zone_group_id, character_id, side) {
            continue;
        }
        if let Some(template_id) = seal_item(server, zone_group_id, side) {
            if holds(server, &player, template_id) {
                take_one(server, &player, template_id);
            }
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
