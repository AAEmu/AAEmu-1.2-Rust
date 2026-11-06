//! Data-driven siege rosters and the defender reinforcement wave (WP-S4).
//!
//! Evidence (1.2 client decompile, `D:/aa-re/client12/out-x2game/x2game.dll.c`):
//!
//! * A team is `id, obj_id, x, y, z, ni, nr, limit, count, ids[]`
//!   (`FUN_39839c80` reads `ni`/`nr`, `FUN_39839c10` reads `limit`/`count`/ids).
//! * `IsReinforceArrived` (`FUN_3946c500`) is period 4 and `defender.limit == defender.nr`.
//! * `GetReinforceRemainDate` (`FUN_3946c3f0`) is 0 when `ni == nr`, `limit == nr` or now is
//!   past `start(siege) + bdm` (`FUN_398390a0`); otherwise it is that deadline minus now.
//! * `SCSiegeReinforce` (0xeb) sets the defender `limit = nr` (clamped to 100,
//!   `FUN_392f3670`/`FUN_39839320`).
//!
//! So while the wave is pending the defender is encoded as
//! `ni = limit = num_defenders`, `nr = num_defenders + num_reinforcements`; at
//! `starts_at + bdm` the server sends 0x0eb once and the snapshot raises `limit` to `nr`.
//! With no reinforcements `ni == nr` and no packet is needed. `siege_settings` is keyed by
//! `total_castles`, INFERRED to be the number of dominions the defending expedition holds.
//!
//! UNKNOWN (capture-pending, see PORTING.md WP-S4): the attacker cap (the same
//! `num_defenders + num_reinforcements` total is used, the only evidenced cap), and what the
//! tickets do (a wave raises the cap only; no member is enrolled automatically).

use crate::server::GameServer;
use crate::systems::housing::dominion::state::{
    DominionState, ParticipantCap, SiegeLimits, SiegeSide, SiegeStatus, MAX_PARTICIPANTS_PER_SIDE,
};
use crate::systems::social::packets::misc::SCSiegeReinforcePacket;

/// `siege_settings` rows cover 0..=10 held castles (INFERRED index meaning).
pub const MAX_SETTINGS_CASTLES: u32 = 10;

fn clamp_limit(value: u32) -> u8 {
    u8::try_from(value.min(u32::from(u8::MAX)))
        .unwrap_or(u8::MAX)
        .min(MAX_PARTICIPANTS_PER_SIDE as u8)
}

/// The authored limits for the defending expedition: the row for the number of dominions it
/// holds, clamped to the shipped 0..=10 range. `None` when the table or the row is missing
/// (the caller falls back to the native 100-character capacity). A test-only ephemeral
/// dominion still represents a held castle here; the flag only keeps it out of the database
/// and out of the WP-S6 claim gate.
pub fn limits(server: &GameServer, defender_expedition_id: u32) -> Option<SiegeLimits> {
    if defender_expedition_id == 0 {
        return None;
    }
    let held = server
        .systems
        .housing
        .dominions
        .all()
        .iter()
        .filter(|state| state.owner_expedition_id == defender_expedition_id)
        .count() as u32;
    let row = server
        .game_data
        .systems
        .housing
        .castles
        .siege_settings
        .get(&held.min(MAX_SETTINGS_CASTLES))?;
    let ni = clamp_limit(row.num_defenders);
    let nr = clamp_limit(row.num_defenders.saturating_add(row.num_reinforcements)).max(ni);
    Some(SiegeLimits { ni, nr })
}

/// `siege_zones.reinforce_defense_delay_mins` as `bdm` seconds (0 when the zone has no row).
pub fn reinforcement_delay(server: &GameServer, zone_group_id: u32) -> i64 {
    server
        .game_data
        .systems
        .housing
        .castles
        .siege_zones
        .get(&zone_group_id)
        .map_or(0, |zone| i64::from(zone.reinforce_defense_delay_mins) * 60)
}

/// The cap a roster join must check for one side at the current authoritative state.
///
/// Before the wave the defender is limited to `ni`; a full roster then reports
/// [`DominionError::ReinforceBlocked`](super::state::DominionError::ReinforceBlocked). The
/// attacker uses the total (`nr`) because its own cap is UNKNOWN.
pub fn participant_cap(state: &DominionState, side: SiegeSide) -> ParticipantCap {
    let Some(siege) = state.siege.as_ref() else {
        return ParticipantCap::NATIVE;
    };
    // The caps were frozen at scheduling; the live castle count cannot move them mid-siege.
    let limits = SiegeLimits {
        ni: siege.defender_ni,
        nr: siege.defender_nr,
    };
    match side {
        SiegeSide::Defender => {
            let pending = limits.pending() && !siege.reinforced;
            ParticipantCap {
                limit: usize::from(if pending { limits.ni } else { limits.nr }),
                awaiting_reinforcements: pending,
            }
        }
        SiegeSide::Attacker => ParticipantCap {
            limit: usize::from(limits.nr),
            awaiting_reinforcements: false,
        },
    }
}

/// Announce the defender wave at `start(siege) + bdm`, exactly once.
///
/// The due check and the persisted `reinforced` flag happen under the authority lock, and the
/// packet is sent only for the transition, so repeated ticks and restarts cannot repeat it.
pub fn tick(server: &GameServer, now: i64) {
    if now < 0 {
        return;
    }
    for state in server.systems.housing.dominions.all() {
        let Some(siege) = state
            .siege
            .as_ref()
            .filter(|siege| siege.status != SiegeStatus::Finished && !siege.reinforced)
        else {
            continue;
        };
        if siege.defender_nr <= siege.defender_ni {
            continue;
        }
        let delay = reinforcement_delay(server, state.zone_group_id);
        let Some(deadline) = siege.starts_at.checked_add(delay) else {
            continue;
        };
        if now < deadline {
            continue;
        }
        match server.systems.housing.dominions.mark_reinforced(
            state.zone_group_id,
            siege.siege_id,
            deadline,
            now,
        ) {
            Ok(Some(_)) => {
                if let Ok(id) = u16::try_from(state.zone_group_id) {
                    server
                        .world
                        .broadcast_to_server(&SCSiegeReinforcePacket { id });
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                "Dominion {} reinforcement wave: {error}",
                state.zone_group_id
            ),
        }
    }
}

#[cfg(test)]
#[path = "reinforce_tests.rs"]
mod tests;
