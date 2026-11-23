//! Original 1.2 dominion snapshots and the social edge of persistent castle authority.

use aaemu_commons::helpers::convert_coord_to_long;
use aaemu_commons::{Marshal, PacketStream};
use chrono::{DateTime, Utc};

use crate::models::container::{equip_slot, slot_type};
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::systems::economy::inventory::{Inv, ItemTxn};
use crate::systems::housing::dominion::calendar;
use crate::systems::housing::dominion::reinforce;
use crate::systems::housing::dominion::runtime::is_live_object;
use crate::systems::housing::dominion::runtime::{apply_tower_state, tower_identity};
use crate::systems::housing::dominion::state::{
    DominionError, DominionState, SiegeSide, SiegeStatus,
};
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster};
use crate::world::{PlayerRef, WorldObject};

use super::packets::misc::{
    SCDominionDataPacket, SCDominionOwnerChangedPacket, SCDominionTaxRatePacket,
    SCNationalTaxRatePacket, SCSiegeAlertPacket, SCSiegeDeclaredPacket, SCSiegeMemberPacket,
    SCSiegeStatePacket,
};

use super::{nation, packets, util};

mod claim;
mod monument;
mod snapshots;
mod taxation;
mod wire;

pub use self::claim::declare_effect;
pub(crate) use self::monument::{
    clear_national_monument_on_owner_change, persist_national_monument,
};
pub use self::snapshots::snapshot;
#[cfg(test)]
pub(crate) use self::taxation::persist_national_tax;
pub use self::taxation::{update_national_tax_rate, update_tax_rate};
pub use self::wire::{DominionData, DominionSiegeTimers, DominionTerritoryData, DominionUnkData};

const DECLARATION_SKILL: u32 = 13_661;
const PURIFYING_ARCHEUM: u32 = 16_493;

/// Original stock DominionGuardTowerStateNoticeKey indices, not fortification/model stages.
pub mod tower_alert {
    pub const FIRST_ATTACKED: u8 = 0;
    pub const BELOW_75_PERCENT: u8 = 1;
    pub const BELOW_50_PERCENT: u8 = 2;
    pub const ENGRAVABLE: u8 = 3;
    pub const ENGRAVING_STARTED: u8 = 4;
    pub const ENGRAVING_STOPPED: u8 = 5;
    pub const ENGRAVING_SUCCEEDED: u8 = 6;
}

/// Emit only actual roster differences, removing the previous expedition entry before adding
/// a replacement. Call while the housing operation serializes the old/new transition, before
/// the full state snapshot. Repeated registration returns old==new and emits no duplicates.
pub fn broadcast_participant_changes(
    server: &GameServer,
    old: &DominionState,
    new: &DominionState,
) {
    if old.zone_group_id != new.zone_group_id || old.tower != new.tower {
        return;
    }
    let Some(data) = snapshot(server, new, Utc::now().timestamp()) else {
        return;
    };
    let empty = std::collections::BTreeMap::new();
    let old_roster = old.siege.as_ref().map_or(&empty, |s| &s.participants);
    let new_roster = new.siege.as_ref().map_or(&empty, |s| &s.participants);
    for participant in old_roster
        .values()
        .filter(|p| new_roster.get(&p.character_id) != Some(*p))
    {
        server.world.broadcast_to_server(&SCSiegeMemberPacket {
            id: data.zone_id,
            expedition_id: participant.expedition_id,
            character_id: participant.character_id,
            added: false,
        });
    }
    for participant in new_roster
        .values()
        .filter(|p| old_roster.get(&p.character_id) != Some(*p))
    {
        server.world.broadcast_to_server(&SCSiegeMemberPacket {
            id: data.zone_id,
            expedition_id: participant.expedition_id,
            character_id: participant.character_id,
            added: true,
        });
    }
}

/// The offense declaration notification (a zero team means "nobody declared", the cancel
/// result the client understands).
pub fn broadcast_siege_declared(server: &GameServer, state: &DominionState) {
    let Some(data) = snapshot(server, state, Utc::now().timestamp()) else {
        return;
    };
    server.world.broadcast_to_server(&SCSiegeDeclaredPacket {
        id: data.zone_id,
        offense: data.siege_timers.unk2_data,
    });
}

/// Tower alerts omit their zone from the native Lua event. Limit delivery to online players
/// in this instance who belong to an involved expedition or are physically in this territory.
pub fn send_tower_alert(
    server: &GameServer,
    state: &DominionState,
    alert: u8,
    actor: Option<&PlayerRef>,
) {
    if alert > tower_alert::ENGRAVING_SUCCEEDED
        || snapshot(server, state, Utc::now().timestamp()).is_none()
    {
        return;
    }
    let (character_id, name) = actor
        .filter(|p| p.character_id != 0 && is_live_object(server, &WorldObject::from(*p)))
        .map(|p| {
            let name = p.data().unit.name.clone();
            let name = if name.is_empty() {
                server
                    .names
                    .get_character_name(p.character_id)
                    .unwrap_or_default()
            } else {
                name
            };
            (p.character_id, name)
        })
        .unwrap_or_default();
    let packet = SCSiegeAlertPacket {
        id: state.zone_group_id as u16,
        alert,
        character_id,
        name,
    };
    let mut expeditions = vec![state.owner_expedition_id];
    if let Some(siege) = &state.siege {
        expeditions.extend([siege.attacker_expedition_id, siege.defender_expedition_id]);
    }
    for player in server.world.all_characters() {
        let (online, transform) = {
            let c = player.data();
            (c.is_online, c.unit.transform)
        };
        if !online || transform.instance_id != state.tower.instance_id {
            continue;
        }
        let expedition = server
            .expeditions
            .expedition_of_character(player.character_id);
        let in_expedition = expedition != 0 && expeditions.contains(&expedition);
        let in_territory = server
            .game_data
            .zones
            .zone_by_key(transform.zone_id)
            .is_some_and(|z| z.group_id == state.zone_group_id);
        if in_expedition || in_territory {
            player.send(packet.clone());
        }
    }
}

pub fn broadcast_snapshot(server: &GameServer, state: &DominionState, newly_declared: bool) {
    if let Some(dominion) = snapshot(server, state, Utc::now().timestamp()) {
        server.world.broadcast_to_server(&SCDominionDataPacket {
            dominion,
            newly_declared,
            final_data_by_request: true,
        });
    }
}

/// Announce an actual phase transition before replacing the native cached snapshot.
/// OnSiegeState emits its phase event even for unchanged phases, so roster and tax updates
/// must use broadcast_snapshot instead.
pub fn broadcast_phase_change(server: &GameServer, state: &DominionState) {
    let Some(dominion) = snapshot(server, state, Utc::now().timestamp()) else {
        return;
    };
    tracing::debug!(
        "Dominion {} siege state: period {} started {} durations {:?}",
        dominion.zone_id,
        dominion.siege_timers.siege_period,
        dominion.siege_timers.started.timestamp(),
        dominion.siege_timers.durations
    );
    // Record exactly the period this snapshot carries: every phase announcement (in or out of
    // the tick) then agrees with the calendar's last announced period.
    server.systems.housing.calendar.record(
        u32::from(dominion.zone_id),
        dominion.siege_timers.siege_period,
    );
    broadcast_siege_state(server, &dominion, 0);
    server.world.broadcast_to_server(&SCDominionDataPacket {
        dominion,
        newly_declared: false,
        final_data_by_request: true,
    });
}

/// Applies a forced release (`DominionManager::release_expedition`) the way the GM cancel
/// command does: remove the old siege roster, refresh the tower runtime state, then announce
/// the finished phase so clients drop the cancelled siege.
pub fn apply_released(server: &GameServer, previous: &DominionState, released: &DominionState) {
    broadcast_participant_changes(server, previous, released);
    apply_tower_state(server, released);
    broadcast_phase_change(server, released);
    // The released territory has no owner any more: the client drops its cached record. A
    // release that only cancels a siege keeps the owner and its record.
    if previous.owner_expedition_id != 0 && released.owner_expedition_id == 0 {
        super::dominion_notifications::broadcast_deleted(server, released.zone_group_id);
    }
    clear_national_monument_on_owner_change(server, previous, released);
}

/// Ownership delta followed by the authoritative snapshot: the native delta resets national
/// tax to 500 independently, which must not replace the server's persisted value.
pub fn broadcast_owner_change(server: &GameServer, state: &DominionState) {
    broadcast_owner_change_with(server, state, false);
}

/// [`broadcast_owner_change`] with the client's announcement choice (`true` = bestowed,
/// `false` = declared/captured).
pub fn broadcast_owner_change_with(server: &GameServer, state: &DominionState, bestowed: bool) {
    let Some(dominion) = snapshot(server, state, Utc::now().timestamp()) else {
        return;
    };
    // Native OnSiegeState needs the preceding cached phase (4 at capture) and copies the
    // new timers before emitting DOMINION_SIEGE_PERIOD_CHANGED. A snapshot first loses it.
    server.systems.housing.calendar.record(
        u32::from(dominion.zone_id),
        dominion.siege_timers.siege_period,
    );
    broadcast_siege_state(server, &dominion, state.owner_expedition_id);
    server
        .world
        .broadcast_to_server(&SCDominionOwnerChangedPacket {
            id: dominion.zone_id,
            unk_id: state.owner_expedition_id,
            rst: state.reign_start_at as u64,
            bestowed,
        });
    server.world.broadcast_to_server(&SCDominionDataPacket {
        dominion,
        newly_declared: false,
        final_data_by_request: true,
    });
}

fn broadcast_siege_state(server: &GameServer, dominion: &DominionData, new_owner: u32) {
    server.world.broadcast_to_server(&SCSiegeStatePacket {
        id: dominion.zone_id,
        territory_data: dominion.territory_data,
        siege_timers: dominion.siege_timers.clone(),
        new_owner,
        reign_start_time: dominion.reign_start_time,
    });
}

/// 1.2 has no request-dominion-data opcode; resend the persistent cache at world entry.
pub fn send_snapshots(server: &GameServer, player: &PlayerRef) {
    let now = Utc::now().timestamp();
    let snapshots: Vec<_> = server
        .systems
        .housing
        .dominions
        .snapshots()
        .iter()
        .filter_map(|state| snapshot(server, state, now))
        .collect();
    let count = snapshots.len();
    for (index, dominion) in snapshots.into_iter().enumerate() {
        player.send(SCDominionDataPacket {
            dominion,
            newly_declared: false,
            final_data_by_request: index + 1 == count,
        });
    }
}

pub(crate) fn is_expedition_leader(
    server: &GameServer,
    player: &PlayerRef,
    expedition: u32,
) -> bool {
    expedition != 0
        && player.data().unit.expedition_id == expedition
        && server
            .expeditions
            .expedition_of_character(player.character_id)
            == expedition
        && server
            .expeditions
            .get(expedition)
            .is_some_and(|e| !e.is_disbanded && e.faction.owner_id == player.character_id)
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod authority_tests;
