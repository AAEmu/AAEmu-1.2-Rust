//! `StartDominionNonPvpDuration` (special effect 110). The 1.2 row 14465 carries 120 minutes;
//! skill 23992 "Dominion peace" already handles its own 4-hour cooldown and the five reagent
//! crystals, so this handler only validates the live territory and starts the peace window.

use crate::server::GameServer;
use crate::systems::economy::error_message::ALREADY_IN_NON_PVP_DURATION;
use crate::systems::skills::effects::EffectArgs;
use crate::systems::social::dominion::{broadcast_snapshot, is_expedition_leader};
use crate::systems::social::util::send_error;
use crate::world::WorldObject;

use super::fort;
use super::state::DominionError;

/// Starts the peace window of the territory the caster is standing in.
pub fn start_effect(server: &GameServer, args: &EffectArgs<'_>, duration_min: i32) {
    let Some(player) = args
        .caster
        .as_ref()
        .and_then(WorldObject::as_player)
        .cloned()
    else {
        return;
    };
    let Ok(duration) = u16::try_from(duration_min) else {
        tracing::warn!(
            "Special effects: StartDominionNonPvpDuration has invalid minutes {duration_min}"
        );
        return;
    };
    if duration == 0 {
        tracing::warn!("Special effects: StartDominionNonPvpDuration has no duration");
        return;
    }
    let (instance_id, x, y) = {
        let t = player.data().unit.transform;
        (t.instance_id, t.x, t.y)
    };
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(state) = fort::state_at(server, instance_id, x, y) else {
        tracing::debug!(
            "Special effects: StartDominionNonPvpDuration has no owned live territory at the caster"
        );
        return;
    };
    if !is_expedition_leader(server, &player, state.owner_expedition_id) {
        tracing::warn!(
            "Special effects: StartDominionNonPvpDuration requires the current territory owner leader"
        );
        return;
    }
    match server.systems.housing.dominions.start_non_pvp(
        state.zone_group_id,
        state.owner_expedition_id,
        duration,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(state) => broadcast_snapshot(server, &state, false),
        Err(DominionError::NonPvpActive) => send_error(&player, ALREADY_IN_NON_PVP_DURATION),
        Err(error) => tracing::warn!(
            "Special effects: StartDominionNonPvpDuration failed for zone group {}: {error}",
            state.zone_group_id
        ),
    }
}
