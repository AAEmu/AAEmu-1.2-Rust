//! Siege participant seals: the Guardian Seal (`siege_zones.defense_ticket_id`) and the
//! Marching Seal (`offense_ticket_id`).
//!
//! Evidence and policy: the 1.2 data gives every castle zone one defense and one offense
//! seal item (`items` 21313-21320 / 28170-28173, category 120, not sellable, `price` 0,
//! `honor_price` 0, 240-minute absolute lifetime). Nothing in the 1.2 data or in a 1.2 server
//! source says that a seal marks a siege participant: that purpose is **inferred** from a
//! period source (IGN, 2014-09-26, "Castles"; Total ArcheAge, 2014-11-20, grade C), which says
//! the game "selects from the eligible participants for Attackers and Defenders up to the
//! allotted limits". There is no 1.2 CS packet for a join (siege.md 0: the client has no
//! siege-specific CS), so this server hands the seal to a roster entry it accepted and keeps it
//! for the siege record.
//!
//! [`reconcile`] is the **only** writer. It runs once a second from the participant tick
//! (`runtime::participant_tick`) and holds no lock of its own. No command and no housing tick
//! hands a seal: those run under the economy operation scope and the housing operation lock,
//! and an item grant takes that scope, so a grant from there racing the sweep was a lock-order
//! inversion. A join therefore shows its seal at most one second later. The sweep
//!
//! * (a) hands a missing seal to every online participant of an unfinished siege (a member who
//!   was offline at registration or at the start, a GM registration of an offline character, a
//!   full bag whose first grant failed, the authored 240-minute expiry during a long siege), and
//! * (b) takes the seal back once the siege record is finished or the character is no longer a
//!   participant of it.
//!
//! "Participant" is [`participant_side`], the one rule the participant buffs, the hostility
//! rule and the eviction use as well: a roster entry alone is not enough, a member who left or
//! was kicked from the side's expedition mid-siege keeps the entry but loses the seal.
//!
//! The grant is idempotent (`holds`), and a seal taken while a member left is handed again by
//! (a) on the next sweep if they rejoin, so no lock between the two halves is needed. The
//! runtime grant set is deliberately not persisted; after a restart the item lifetime is the
//! only cleanup for a character who is offline. The authored 240-minute lifetime is also the
//! backstop for a character who is offline when the siege ends; a seal granted by the
//! defence-reinforcement skill 16975 (`GetSiegeTicket`) is never removed by this module (it is
//! not in the runtime set).

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::systems::housing::dominion::participants::participant_side;
use crate::systems::housing::dominion::state::{SiegeSide, SiegeStatus};
use crate::world::PlayerRef;

/// Runtime-only record of the seals this server handed to a roster entry.
///
/// Keyed by zone group and character id; the side is kept so [`reconcile`] can compare it with
/// the current roster entry without re-reading the item template.
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

/// Hand the side's seal to one online roster entry. A character that already holds the item
/// (the defence-reinforcement skill 16975 grants the same defence seal) gets no duplicate and
/// is not tracked.
fn grant(server: &GameServer, zone_group_id: u32, player: &PlayerRef, side: SiegeSide) -> bool {
    let Some(template_id) = seal_item(server, zone_group_id, side) else {
        return false;
    };
    if holds(server, player, template_id) {
        return false;
    }
    let acquired = server.systems.skills.ext.items().try_add_new_item(
        server,
        player,
        task_type::SKILL_EFFECT_GAIN_ITEM,
        template_id,
        1,
        0,
    );
    if !acquired {
        // The sweep retries every second (a full bag is the common cause).
        tracing::debug!(
            "Dominion {zone_group_id}: could not hand seal {template_id} to character {}",
            player.character_id
        );
        return false;
    }
    server
        .systems
        .housing
        .siege_seals
        .track(zone_group_id, player.character_id, side);
    true
}

fn take_one(server: &GameServer, player: &PlayerRef, template_id: u32) {
    server
        .systems
        .skills
        .ext
        .items()
        .consume_item_template(server, player, template_id, 1);
}

/// Whether `character_id` is still a participant of the unfinished siege of `zone_group_id`
/// on `side`, read fresh from the authority. `current_expedition` is the character's current
/// expedition (the unit's while online, the expedition manager's while offline).
fn still_entitled(
    server: &GameServer,
    zone_group_id: u32,
    character_id: u32,
    side: SiegeSide,
    current_expedition: u32,
) -> bool {
    server
        .systems
        .housing
        .dominions
        .get(zone_group_id)
        .is_some_and(|state| {
            state.siege.as_ref().is_some_and(|siege| {
                siege.status != SiegeStatus::Finished
                    && participant_side(server, siege, character_id, current_expedition)
                        == Some(side)
            })
        })
}

/// (Re)grant missing seals and remove ended ones. Runs once a second from the participant
/// sweep (`runtime::participant_sweep`); it reads the authority, not the siege-area snapshot.
pub fn reconcile(server: &GameServer) {
    // (a) Every online participant of an unfinished siege holds its side's seal; `holds` keeps
    // the repeated sweep idempotent. "Participant" is `participants::participant_side`: a roster
    // entry alone is not enough, a member who left the expedition since is none.
    for state in server.systems.housing.dominions.all() {
        let Some(siege) = state
            .siege
            .as_ref()
            .filter(|siege| siege.status != SiegeStatus::Finished)
        else {
            continue;
        };
        for participant in siege.participants.values() {
            let Some(player) = online_player(server, participant.character_id) else {
                continue;
            };
            let expedition = player.data().unit.expedition_id;
            if let Some(side) =
                participant_side(server, siege, participant.character_id, expedition)
            {
                grant(server, state.zone_group_id, &player, side);
            }
        }
    }
    // (b) Remove seals whose entitlement ended. The entitlement is read fresh from the
    // authority, so a member who left and rejoined since (a) keeps theirs; one who rejoins
    // after the take is handed a new seal by (a) on the next sweep.
    for ((zone_group_id, character_id), side) in server.systems.housing.siege_seals.tracked() {
        let player = online_player(server, character_id);
        let expedition = match &player {
            Some(player) => player.data().unit.expedition_id,
            None => server.expeditions.expedition_of_character(character_id),
        };
        if still_entitled(server, zone_group_id, character_id, side, expedition) {
            continue;
        }
        // Nothing to take from an offline character: the authored lifetime ends the item.
        if let Some(player) = &player {
            if let Some(template_id) = seal_item(server, zone_group_id, side) {
                if holds(server, player, template_id) {
                    take_one(server, player, template_id);
                }
            }
        }
        server
            .systems
            .housing
            .siege_seals
            .untrack(zone_group_id, character_id);
    }
}

/// The item count of a seal, for tests.
#[cfg(test)]
pub fn seal_count(player: &PlayerRef, template_id: u32) -> i32 {
    crate::systems::economy::inventory::Inv::of(player).items_count_unlocked(template_id, -1)
}
