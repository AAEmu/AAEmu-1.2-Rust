//! Dominion notifications the 1.2 client handles but the server did not send.
//!
//! Evidence (`D:/aa-re/client12/out-x2game/x2game.dll.c`): the registration
//! `FUN_391bb580`..`FUN_391bb6c0` maps 0x023..0x025 to `OnDominionTaxBalanced`,
//! `OnDominionNonPvPStarted` and `OnDominionNonPvPStopped`; their deserializers are
//! `FUN_397b7b60`, `FUN_397b7bb0` and `FUN_397b7c00`. Every handler updates the same cached
//! dominion record that `SCDominionData` (0x01d) populates, and none filters by owner or zone,
//! so the packets go to every online player. The client itself only refreshes the territory
//! window when the player is in that zone (`FUN_392f4d30`).

use chrono::DateTime;

use crate::server::GameServer;
use crate::systems::housing::dominion::state::DominionState;

use super::packets::misc::{
    SCDominionDeletedPacket, SCDominionNonPvPStartedPacket, SCDominionNonPvPStoppedPacket,
    SCDominionTaxBalancedPacket,
};

fn zone_id(zone_group_id: u32) -> Option<u16> {
    u16::try_from(zone_group_id).ok()
}

/// `OnDominionNonPvPStarted` for a window that `DominionManager::start_non_pvp` just opened.
pub fn broadcast_non_pvp_started(server: &GameServer, state: &DominionState) {
    let Some(id) = zone_id(state.zone_group_id) else {
        return;
    };
    if state.non_pvp_start_at <= 0 || state.non_pvp_duration_min == 0 {
        return;
    }
    let Some(start) = DateTime::from_timestamp(state.non_pvp_start_at, 0) else {
        return;
    };
    server
        .world
        .broadcast_to_server(&SCDominionNonPvPStartedPacket {
            id,
            start,
            minutes: state.non_pvp_duration_min,
        });
}

/// `OnDominionNonPvPStopped`: the client clears its cached window end, which its own
/// peace-skill precheck (`FUN_398391c0`, error 0x2f7) otherwise keeps refusing.
pub fn broadcast_non_pvp_stopped(server: &GameServer, zone_group_id: u32) {
    let Some(id) = zone_id(zone_group_id) else {
        return;
    };
    server
        .world
        .broadcast_to_server(&SCDominionNonPvPStoppedPacket { id });
}

/// `OnDominionDeleted`: the client drops its cached record for the zone.
pub fn broadcast_deleted(server: &GameServer, zone_group_id: u32) {
    let Some(id) = zone_id(zone_group_id) else {
        return;
    };
    server
        .world
        .broadcast_to_server(&SCDominionDeletedPacket { id });
}

/// `OnDominionTaxBalanced` carrying the current peace fund. Only actual revenue may move the
/// ledger; this sender does not accrue anything.
pub fn broadcast_tax_balanced(server: &GameServer, state: &DominionState) {
    let Some(id) = zone_id(state.zone_group_id) else {
        return;
    };
    server
        .world
        .broadcast_to_server(&SCDominionTaxBalancedPacket {
            id,
            peace_tax_money: state.treasury.peace_tax_money,
            peace_tax_aa_point: state.treasury.peace_tax_aa_point,
        });
}

/// Natural end of every expired non-PvP window, followed by its stop notification. Early
/// termination (for example by a siege declaration) is UNKNOWN in 1.2 and is not invented.
pub fn expire_non_pvp(server: &GameServer, now_seconds: i64) {
    let zones = match server.systems.housing.dominions.expire_non_pvp(now_seconds) {
        Ok(zones) => zones,
        Err(error) => {
            tracing::error!("Dominion non-PvP expiry: {error}");
            return;
        }
    };
    for zone in zones {
        broadcast_non_pvp_stopped(server, zone);
    }
}
