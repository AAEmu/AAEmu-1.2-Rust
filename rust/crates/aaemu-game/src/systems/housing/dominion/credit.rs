//! Siege result achievements (kinds 57-60): one credit pass per finished siege.
//!
//! The mapping is EVIDENCED from the authored `char_records` and the Korean achievement texts
//! (research `siege-social-2.md` 1.4): 57 attack won, 58 defence won, 59 attack lost, 60 defence
//! lost, each with `value1 = siege_zones.zone_group_id` and `value2 = 0`. The roster at outcome
//! time (`dominion_siege_participants`) receives the credit: an online character through the
//! achievement facade, an offline one through its `character_records` row, which the next entry
//! snapshot loads and evaluates (INFERRED: the counter is cumulative and the entry path re-runs
//! every authored record).
//!
//! `SiegeOutcome::Cancelled` credits nobody. [`DominionManager::mark_outcome_credited`] is the
//! persisted check-and-set marker, so a repeated tick, a capture racing the tick or a restart
//! cannot credit one siege id twice. The marker is written with the dominion row, and an online
//! character's record values are written by the same character save transaction, so a save
//! failure keeps both sides uncredited.
//!
//! [`DominionManager::mark_outcome_credited`]: super::state::DominionManager::mark_outcome_credited

use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};

use super::state::{DominionState, SiegeOutcome, SiegeSide};

/// The attacker and defender record kinds for an outcome (`None` = credit nobody).
fn outcome_kinds(outcome: SiegeOutcome) -> Option<(u32, u32)> {
    match outcome {
        SiegeOutcome::AttackerCaptured => Some((
            char_record_kind::WIN_ATTACK_SIEGE,
            char_record_kind::LOSE_DEFENCE_SIEGE,
        )),
        SiegeOutcome::DefenderHeld => Some((
            char_record_kind::LOSE_ATTACK_SIEGE,
            char_record_kind::WIN_DEFENCE_SIEGE,
        )),
        SiegeOutcome::Pending | SiegeOutcome::Cancelled => None,
    }
}

/// Credits the finished siege's roster once. Returns whether this call applied the credit.
pub fn credit_siege_outcome(server: &GameServer, state: &DominionState) -> bool {
    let Some(siege) = state.siege.as_ref() else {
        return false;
    };
    let Some((attacker_kind, defender_kind)) = outcome_kinds(siege.outcome) else {
        return false;
    };
    let Ok(Some(credited)) = server
        .systems
        .housing
        .dominions
        .mark_outcome_credited(state.zone_group_id, siege.siege_id)
    else {
        return false;
    };
    let Some(siege) = credited.siege.as_ref() else {
        return false;
    };
    let matcher = RecordMatch::Value1(credited.zone_group_id);
    for participant in siege.participants.values() {
        let kind_id = match participant.side {
            SiegeSide::Attacker => attacker_kind,
            SiegeSide::Defender => defender_kind,
        };
        let input = AchievementInput::Increment {
            kind_id,
            amount: 1,
            matcher,
        };
        if let Some(player) = server.world.character_by_id(participant.character_id) {
            crate::systems::report_achievement(server, &player, input);
        } else {
            let record_ids = crate::systems::social::achievement_hooks::increment_record_ids(
                server, kind_id, matcher,
            );
            persist_offline_credit(server, participant.character_id, record_ids);
        }
    }
    true
}

/// Persists an offline participant's increment for the next entry snapshot. Without a server
/// handle (unit tests) nothing is written; the online paths are unaffected.
fn persist_offline_credit(server: &GameServer, character_id: u32, record_ids: Vec<u32>) {
    if record_ids.is_empty() {
        return;
    }
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = write_offline_credit(&server, character_id, &record_ids).await {
            tracing::error!(
                "Persisting the siege achievements of character {character_id} failed: {error}"
            );
        }
    });
}

/// The entry snapshot reads `character_records`, so the offline credit is an increment there.
/// The save lock keeps it out of a character save that is rewriting the same rows.
async fn write_offline_credit(
    server: &GameServer,
    character_id: u32,
    record_ids: &[u32],
) -> sqlx::Result<()> {
    let _save = server.save.transaction_lock().await;
    for record_id in record_ids {
        sqlx::query(
            "INSERT INTO `character_records` (`owner`, `record_id`, `value`) VALUES (?, ?, 1) \
             ON DUPLICATE KEY UPDATE `value` = `value` + 1",
        )
        .bind(character_id)
        .bind(*record_id)
        .execute(&server.db)
        .await?;
    }
    Ok(())
}
