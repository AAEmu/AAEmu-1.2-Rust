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

const DECLARATION_SKILL: u32 = 13_661;
const PURIFYING_ARCHEUM: u32 = 16_493;

/// The configured weekly calendar period of a territory, when the calendar is enabled.
///
/// Only this calendar view uses the GM test clock. The explicit (persisted) siege branch of
/// [`snapshot`] always receives the real clock from its caller, and the WP-S6 claim window
/// uses the real wall clock in [`declare_effect`] (the GM clock affects the calendar only).
/// A claim younger than
/// `world_var_defaults.var_delay_siege_x_days_after_dominion_declare` gets no cycle: its view
/// stays peace until the first rotation cycle after the protection ends (INFERRED; the 1.2
/// value is 30 days).
fn calendar_view(server: &GameServer, state: &DominionState) -> Option<calendar::PeriodView> {
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    if !settings.enabled {
        return None;
    }
    let delay = server
        .game_data
        .systems
        .housing
        .castles
        .siege_delay_after_claim_days;
    let siege_not_before = if state.claimed_at > 0 && delay > 0 {
        state
            .claimed_at
            .saturating_add(delay.saturating_mul(86_400))
    } else {
        0
    };
    calendar::SiegeCalendar::from_castles(&server.game_data.systems.housing.castles)
        .view_for_declaration(
            state.zone_group_id,
            server.systems.housing.calendar.now(),
            &settings.timezone,
            state.declared_cycle(),
            siege_not_before,
        )
}

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

/// Build only snapshots backed by the same live lodestone and authored 1.2 setting.
pub fn snapshot(server: &GameServer, state: &DominionState, now: i64) -> Option<DominionData> {
    state.validate().ok()?;
    let house = server.systems.housing.manager.by_id(state.tower.house_id)?;
    let (zone, identity) = tower_identity(server, &house)?;
    let obj = WorldObject::from(&house);
    if zone != state.zone_group_id || identity != state.tower || !is_live_object(server, &obj) {
        return None;
    }
    let setting = server
        .game_data
        .systems
        .housing
        .castles
        .guard_tower_settings
        .get(&identity.guard_tower_setting_id)?;
    let pos = server.world.world_of(&obj)?.world_pos_rot(&obj);
    if !pos.x.is_finite() || !pos.y.is_finite() || !pos.z.is_finite() {
        return None;
    }
    let epoch = DateTime::from_timestamp(0, 0)?;
    let mut timers = DominionSiegeTimers {
        started: epoch,
        fixed: epoch,
        // Native 1.2: 0=no_dominion, 1=auction, 2=declare, 3=warmup, 4=siege, 5=peace.
        siege_period: if state.owner_expedition_id == 0 { 0 } else { 5 },
        ..Default::default()
    };
    let calendar_authoritative = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile)
        .enabled;
    if let Some(siege) = state.siege.as_ref().filter(|s| {
        s.status != SiegeStatus::Finished
            && now < s.ends_at
            // A calendar declaration keeps describing its cycle; only a GM schedule owns
            // its own timers (WP-SD2a).
            && !(s.materialised_from_declaration && calendar_authoritative)
    }) {
        // An explicit GM schedule overrides the calendar for this territory. The five native
        // duration slots are auction, declare, warmup, siege and peace, in seconds.
        timers.durations = [
            0,
            0,
            i32::try_from(siege.starts_at.checked_sub(siege.scheduled_at)?).ok()?,
            i32::try_from(siege.ends_at.checked_sub(siege.starts_at)?).ok()?,
            0,
        ];
        timers.started = DateTime::from_timestamp(siege.scheduled_at, 0)?;
        // `bdm` is the authored reinforcement delay; the client computes the wave arrival as
        // start(4) + bdm, i.e. `siege.starts_at + this delay` (`FUN_398390a0`).
        // An out-of-range authored delay clamps instead of dropping the whole snapshot.
        timers.bdm = i32::try_from(reinforce::reinforcement_delay(server, state.zone_group_id))
            .unwrap_or(i32::MAX);
        timers.siege_period = if siege.active_at(now) { 4 } else { 3 };
        // The native owner setter and team getters identify these records by expedition.
        timers.unk_data.id = siege.defender_expedition_id;
        timers.unk2_data.id = siege.attacker_expedition_id;
        for (side, team) in [
            (SiegeSide::Defender, &mut timers.unk_data),
            (SiegeSide::Attacker, &mut timers.unk2_data),
        ] {
            if let Some(hq) = server
                .systems
                .housing
                .headquarters
                .snapshot(server, state, side)
            {
                team.obj_id = hq.obj_id;
                team.x = hq.position.x;
                team.y = hq.position.y;
                team.z = hq.position.z;
            }
        }
        for participant in siege.participants.values() {
            let team = match participant.side {
                SiegeSide::Defender => &mut timers.unk_data,
                SiegeSide::Attacker => &mut timers.unk2_data,
            };
            team.unk_ids.push(participant.character_id);
        }
        // Roster entitlements frozen at scheduling (WP-S4). The defender starts at `ni` and
        // the client shows the reinforcement countdown until 0x0eb raises `limit` to `nr`;
        // with no reinforcements `ni == nr` and no wave is pending. The attacker cap is
        // UNKNOWN, so it uses the same evidenced total (see `reinforce`). A mid-siege change
        // in the defender's castle count cannot move these values.
        let pending = siege.defender_nr > siege.defender_ni && !siege.reinforced;
        timers.unk_data.ni = siege.defender_ni;
        timers.unk_data.nr = siege.defender_nr;
        timers.unk_data.limit = if pending {
            siege.defender_ni
        } else {
            siege.defender_nr
        };
        timers.unk2_data.ni = siege.defender_nr;
        timers.unk2_data.nr = siege.defender_nr;
        timers.unk2_data.limit = siege.defender_nr;
        if timers.unk_data.unk_ids.len() > 100 || timers.unk2_data.unk_ids.len() > 100 {
            return None;
        }
    } else if state.owner_expedition_id != 0 {
        // The weekly calendar (`FUN_3956fc70`, `FUN_39838ff0`/`FUN_3946be90`): real period and
        // timers in every phase; in peace `started` is the next cycle's auction start.
        if let Some(view) = calendar_view(server, state) {
            timers.durations = view.durations;
            timers.started = DateTime::from_timestamp(view.started, 0)?;
            // An out-of-range authored delay clamps instead of dropping the whole snapshot.
            timers.bdm = i32::try_from(reinforce::reinforcement_delay(server, state.zone_group_id))
                .unwrap_or(i32::MAX);
            timers.siege_period = view.period;
            // The defender owns the territory; the attacker team stays zero until a
            // declaration exists (a zero attacker is the client's "nobody declared" state).
            timers.unk_data.id = state.owner_expedition_id;
            if let Some(hq) =
                server
                    .systems
                    .housing
                    .headquarters
                    .snapshot(server, state, SiegeSide::Defender)
            {
                timers.unk_data.obj_id = hq.obj_id;
                timers.unk_data.x = hq.position.x;
                timers.unk_data.y = hq.position.y;
                timers.unk_data.z = hq.position.z;
            }
            // An accepted declaration of this cycle is the offense team (WP-SD2a). Once the
            // warmup materialised it, the record carries the same teams and roster.
            let declaration = state.declaration.filter(|d| {
                view.cycle
                    .is_some_and(|c| c.auction_start == d.cycle_auction_start)
            });
            let calendar_siege = state.siege.as_ref().filter(|siege| {
                siege.materialised_from_declaration
                    && siege.status != SiegeStatus::Finished
                    && now < siege.ends_at
            });
            if let Some(declaration) = declaration {
                timers.unk2_data.id = declaration.attacker_expedition_id;
                timers.unk2_data.obj_id = declaration.hq_obj_id;
                timers.unk2_data.x = declaration.hq_x;
                timers.unk2_data.y = declaration.hq_y;
                timers.unk2_data.z = declaration.hq_z;
                // The attacker cap is UNKNOWN (siege.md 1.3); the only evidenced total is the
                // defender row's `num_defenders + num_reinforcements`, used for both teams.
                let limit = reinforce::limits(server, state.owner_expedition_id)
                    .unwrap_or(crate::systems::housing::dominion::SiegeLimits::NATIVE)
                    .nr;
                timers.unk2_data.ni = limit;
                timers.unk2_data.nr = limit;
                timers.unk2_data.limit = limit;
            } else if let Some(siege) = calendar_siege {
                timers.unk2_data.id = siege.attacker_expedition_id;
                if let Some(hq) =
                    server
                        .systems
                        .housing
                        .headquarters
                        .snapshot(server, state, SiegeSide::Attacker)
                {
                    timers.unk2_data.obj_id = hq.obj_id;
                    timers.unk2_data.x = hq.position.x;
                    timers.unk2_data.y = hq.position.y;
                    timers.unk2_data.z = hq.position.z;
                }
                let pending = siege.defender_nr > siege.defender_ni && !siege.reinforced;
                timers.unk_data.ni = siege.defender_ni;
                timers.unk_data.nr = siege.defender_nr;
                timers.unk_data.limit = if pending {
                    siege.defender_ni
                } else {
                    siege.defender_nr
                };
                timers.unk2_data.ni = siege.defender_nr;
                timers.unk2_data.nr = siege.defender_nr;
                timers.unk2_data.limit = siege.defender_nr;
                for participant in siege.participants.values() {
                    let team = match participant.side {
                        SiegeSide::Defender => &mut timers.unk_data,
                        SiegeSide::Attacker => &mut timers.unk2_data,
                    };
                    team.unk_ids.push(participant.character_id);
                }
                if timers.unk_data.unk_ids.len() > 100 || timers.unk2_data.unk_ids.len() > 100 {
                    return None;
                }
            }
        }
    }
    Some(DominionData {
        zone_id: u16::try_from(zone).ok()?,
        expedition_id: state.owner_expedition_id,
        house: identity.house_id,
        tax_rate: state.tax_rate,
        x: pos.x,
        y: pos.y,
        z: pos.z,
        cur_house_tax_money: state.treasury.cur_house_tax_money,
        cur_hunt_tax_money: state.treasury.cur_hunt_tax_money,
        peace_tax_money: state.treasury.peace_tax_money,
        cur_house_tax_aa_point: state.treasury.cur_house_tax_aa_point,
        peace_tax_aa_point: state.treasury.peace_tax_aa_point,
        last_paid_time: DateTime::from_timestamp(state.last_paid_at, 0)?,
        last_siege_end_time: DateTime::from_timestamp(state.last_siege_end_at, 0)?,
        reign_start_time: DateTime::from_timestamp(state.reign_start_at, 0)?,
        last_tax_rate_changed_time: DateTime::from_timestamp(state.last_tax_rate_changed_at, 0)?,
        last_national_tax_rate_changed_time: DateTime::from_timestamp(
            state.last_national_tax_rate_changed_at,
            0,
        )?,
        national_tax_rate: state.national_tax_rate,
        national_monument_db_id: state.national_monument_db_id,
        national_monument_x: state.national_monument_x,
        national_monument_y: state.national_monument_y,
        national_monument_z: state.national_monument_z,
        obj_id: house.obj_id,
        territory_data: DominionTerritoryData {
            id: setting.id,
            id2: setting.initial_buff_id,
            max_gates: u8::try_from(setting.max_gates).ok()?,
            max_walls: u8::try_from(setting.max_walls).ok()?,
            radius_declare: i16::try_from(setting.radius_declare).ok()?,
            radius_dominion: u16::try_from(setting.radius_dominion).ok()?,
            radius_offense_hq: i16::try_from(setting.radius_offense_hq).ok()?,
            radius_siege: i16::try_from(setting.radius_siege).ok()?,
        },
        siege_timers: timers,
        non_pvp_start: DateTime::from_timestamp(state.non_pvp_start_at, 0)?,
        // Native DominionData stores the window end as `start + nonPvPDuration` with no
        // scale factor (its writer sends `end - start`), so this field is SECONDS while the
        // 0x024 packet carries the authored MINUTES. A value that cannot fit the u16 seconds
        // field drops the whole snapshot instead of truncating the end.
        non_pvp_duration: u16::try_from(i64::from(state.non_pvp_duration_min) * 60).ok()?,
    })
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

/// Match the original native IsDominionOwner gate and its inclusive 0..50 slider.
pub fn update_tax_rate(server: &GameServer, player: &PlayerRef, zone: u16, tax_rate: i32) {
    let _items = server.systems.economy.operation_scope();
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
        return;
    };
    if !(0..=50).contains(&tax_rate)
        || !is_expedition_leader(server, player, state.owner_expedition_id)
    {
        return;
    }
    let prev = state.tax_rate;
    let state = match server.systems.housing.dominions.set_tax_rate(
        u32::from(zone),
        state.owner_expedition_id,
        tax_rate,
        Utc::now().timestamp(),
    ) {
        Ok(state) => state,
        Err(DominionError::TaxRateChangeLater) => {
            super::util::send_error(
                player,
                crate::systems::economy::error_message::TAXRATE_CHANGE_LATER,
            );
            return;
        }
        Err(_) => return,
    };
    server
        .world
        .broadcast_to_server(&SCDominionTaxRatePacket { id: zone, tax_rate });
    crate::systems::housing::dominion::tax::refresh_bills(server, u32::from(zone));
    broadcast_snapshot(server, &state, false);
    drop(_operation);
    if prev != state.tax_rate {
        crate::systems::economy::mail_system::send_tax_rate_changed_mails(
            server,
            zone,
            prev,
            state.tax_rate,
        );
    }
}

/// `CSUpdateNationalTaxRatePacket` (0x013): the nation king sets the national tax rate of a
/// dominion owned by an expedition inside the kingdom. The checks mirror the 1.2 client's
/// confirm validator (`FUN_392f4820`): `rate > 1000` -> 684; an unknown / unowned dominion or
/// a caller who is not the king of the dominion's top-level faction -> 685; the same rate is
/// ignored (`equal_tax_rate_warning`); a change within 86400 s -> 725. Success broadcasts
/// `SCNationalTaxRate` (0x021; handler `FUN_392f39b0` and notice ui_texts 5675) to every
/// client, because the dominion record is server-wide, and leaves the row dirty for the
/// periodic dominion save. The rate is permille/10 (0-1000); applying it to collected tax and
/// the weekly receipt mail (type 24) are capture-pending (RENAME-8).
pub fn update_national_tax_rate(server: &GameServer, player: &PlayerRef, zone: u16, rate: u32) {
    if !server.config.player_nations_enabled() {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    }
    let _items = server.systems.economy.operation_scope();
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if rate > 1000 {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_INVALID_RATE,
        );
        return;
    }
    let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    };
    let Some(nation) = server.systems.social.nations.owned_by(player.character_id) else {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    };
    if state.owner_expedition_id == 0
        || super::nation::expedition_top_level_faction(server, state.owner_expedition_id)
            != Some(nation.id)
    {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION,
        );
        return;
    }
    if state.national_tax_rate == rate as u16 {
        return;
    }
    let prev = state.national_tax_rate;
    let now = Utc::now().timestamp();
    let state = match server.systems.housing.dominions.set_national_tax_rate(
        u32::from(zone),
        state.owner_expedition_id,
        rate as u16,
        now,
    ) {
        Ok(state) => state,
        Err(DominionError::NationalTaxRateChangeLater) => {
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_LATER,
            );
            return;
        }
        Err(DominionError::InvalidNationalTaxRate) => {
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_INVALID_RATE,
            );
            return;
        }
        Err(_) => return,
    };
    server.world.broadcast_to_server(&SCNationalTaxRatePacket {
        id: zone,
        tax_rate: i32::from(state.national_tax_rate),
        prev_tax_rate: i32::from(prev),
        changed_time: DateTime::from_timestamp(state.last_national_tax_rate_changed_at, 0)
            .unwrap_or(DateTime::<Utc>::MIN_UTC),
    });
    schedule_persist_national_tax(
        server,
        u32::from(zone),
        state.national_tax_rate,
        state.last_national_tax_rate_changed_at,
    );
    drop(_operation);
    crate::systems::economy::mail_system::send_nation_tax_rate_mails(
        server,
        nation.id,
        zone,
        state.national_tax_rate,
        prev,
        state.last_national_tax_rate_changed_at,
    );
}

/// Writes only the national tax columns of one territory. Treasury / siege / other columns
/// stay with the periodic financial save, which is the only path allowed to commit treasury
/// changes with houses / mail / wallets (`managers/save.rs`). The in-memory row remains dirty,
/// so the next financial save writes the same values again.
pub(crate) async fn persist_national_tax(
    server: &GameServer,
    zone_group_id: u32,
    rate: u16,
    changed_at: i64,
) -> sqlx::Result<()> {
    let _save = server.save.transaction_lock().await;
    crate::systems::housing::dominion::db::write_national_tax_rate(
        &server.db,
        zone_group_id,
        rate,
        changed_at,
    )
    .await?;
    Ok(())
}

/// Spawns [`persist_national_tax`] (no-op in unit tests without a server handle).
pub(crate) fn schedule_persist_national_tax(
    server: &GameServer,
    zone_group_id: u32,
    rate: u16,
    changed_at: i64,
) {
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = persist_national_tax(&server, zone_group_id, rate, changed_at).await {
            tracing::error!("Persisting the national tax of zone {zone_group_id} failed: {error}");
        }
    });
}

/// Writes only the national monument columns of one territory (zeroes clear it). The full row
/// stays with the periodic financial save.
pub(crate) async fn persist_national_monument(
    server: &GameServer,
    zone_group_id: u32,
    db_id: i64,
    x: f32,
    y: f32,
    z: f32,
) -> sqlx::Result<()> {
    let _save = server.save.transaction_lock().await;
    crate::systems::housing::dominion::db::write_national_monument(
        &server.db,
        zone_group_id,
        db_id,
        x,
        y,
        z,
    )
    .await?;
    Ok(())
}

/// A territory changed owner (siege capture, re-claim) or was released: announce that the old
/// statue is gone, then zero the monument columns and delete the doodad row. The authority
/// already cleared the in-memory fields. The zero update precedes the doodad delete, so a
/// crash between them leaves an orphan doodad, not a dominion pointing at a missing one.
pub(crate) fn clear_national_monument_on_owner_change(
    server: &GameServer,
    previous: &DominionState,
    current: &DominionState,
) {
    let old_db_id = previous.national_monument_db_id;
    if old_db_id == 0 {
        return;
    }
    if let Ok(zone) = u16::try_from(current.zone_group_id) {
        server
            .world
            .broadcast_to_server(&super::packets::misc::SCNationalMonumentChangedPacket {
                id: zone,
                national_monument_db_id: 0,
                x: 0.0,
                y: 0.0,
                z: 0.0,
            });
    }
    let instance_id = current.tower.instance_id;
    let old_db_id = u32::try_from(old_db_id.max(0)).unwrap_or(0);
    let Some(server) = server.self_arc() else {
        // Unit tests have no shared handle: remove the visible doodad and let the DB writer
        // queue its row delete (the state is already cleared in memory).
        if let Some(world) = server.world.world(instance_id) {
            if let Some(doodad) = world.doodad_by_db_id(old_db_id) {
                crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
                return;
            }
        }
        crate::systems::npcs::doodad::db::delete_row(server, old_db_id);
        return;
    };
    let zone_group_id = current.zone_group_id;
    tokio::spawn(async move {
        if let Err(error) =
            persist_national_monument(&server, zone_group_id, 0, 0.0, 0.0, 0.0).await
        {
            tracing::error!(
                "Clearing the national monument of zone {zone_group_id} failed: {error}"
            );
            return;
        }
        if let Some(world) = server.world.world(instance_id) {
            if let Some(doodad) = world.doodad_by_db_id(old_db_id) {
                crate::systems::npcs::doodad::manager::delete(&server, &world, &doodad);
                return;
            }
        }
        crate::systems::npcs::doodad::db::delete_row(&server, old_db_id);
    });
}

/// Initial unowned claim, only from the accepted original 20-second skill and exact carried
/// Purifying Archeum. Ownership assignment and pack consumption share one authority operation.
pub fn declare_effect(server: &GameServer, args: &EffectArgs<'_>) {
    let (Some(caster), Some(source), Some(tower)) = (
        args.caster.as_ref(),
        args.source.skill.as_ref(),
        args.target.as_obj().and_then(WorldObject::as_house),
    ) else {
        return;
    };
    let Some(player) = caster.as_player() else {
        return;
    };
    let Some(canonical) = server.game_data.systems.skills.skill(DECLARATION_SKILL) else {
        return;
    };
    let (accepted, target_id) = {
        let state = source.state();
        (
            state.generation != 0 && !state.cancelled,
            state.initial_target,
        )
    };
    if !std::sync::Arc::ptr_eq(canonical, &source.template)
        || source.id != DECLARATION_SKILL
        || args.source.buff.is_some()
        || args.source.is_trigger
        || !accepted
        || target_id != tower.obj_id
        || !matches!(args.cast, CastAction::Skill { skill_id: DECLARATION_SKILL, tl_id } if tl_id == source.tl_id())
        || !matches!(args.caster_caster, SkillCaster::Unit(id) if id == player.obj_id)
        || !matches!(args.target_caster, SkillCastTarget::Unit(id) if id == tower.obj_id)
    {
        return;
    }
    let _items = server.systems.economy.operation_scope();
    let operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .expect("housing poisoned");
    let obj = WorldObject::from(tower);
    let Some((zone, identity)) = tower_identity(server, tower) else {
        return;
    };
    // Original 1.2 lodestones have a single authored claim action at build step 0
    // (housing139/step395/model893/skill13661), before becoming their main model.
    let Some(template) = server
        .game_data
        .systems
        .housing
        .template(tower.data().unit.template_id)
    else {
        return;
    };
    let initial_claim = template.build_steps.len() == 1
        && template.build_steps.get(&0).is_some_and(|step| {
            step.step == 0 && step.skill_id == DECLARATION_SKILL && step.num_actions == 1
        });
    let at_initial_action = {
        let h = tower.data();
        h.current_step == 0 && h.base_action == 0 && h.num_action == 0 && h.unit.owner_id == 0
    };
    if !initial_claim || !at_initial_action {
        return;
    }
    if !server
        .systems
        .housing
        .manager
        .by_id(identity.house_id)
        .is_some_and(|h| std::sync::Arc::ptr_eq(&h, tower))
        || !is_live_object(server, caster)
        || !is_live_object(server, &obj)
        || caster.transform().instance_id != identity.instance_id
        || tower.data().unit.hp <= 0
        || !server
            .game_data
            .systems
            .housing
            .castles
            .guard_tower_settings
            .contains_key(&identity.guard_tower_setting_id)
    {
        return;
    }
    let (expedition, alive) = {
        let c = player.data();
        (
            c.unit.expedition_id,
            c.unit.hp > 0 && !c.skill_char.skill_cancelled,
        )
    };
    if !alive || !is_expedition_leader(server, player, expedition) {
        return;
    }
    let Some(world) = server.world.world_of(caster) else {
        return;
    };
    let c = world.world_pos_rot(caster);
    let t = world.world_pos_rot(&obj);
    let distance2 = (c.x - t.x).powi(2) + (c.y - t.y).powi(2) + (c.z - t.z).powi(2);
    let range = canonical.max_range as f32;
    if !distance2.is_finite() || !range.is_finite() || range <= 0.0 || distance2 > range * range {
        return;
    }
    // Auroria claims follow the era profile (`World.Profile`), independently of
    // `Housing.SiegeCalendar`: the Trion launch era has no northern continent, while a
    // calendar-off Auroria server still allows claims for GM-scheduled sieges. 662 is the
    // existing "not yet a region where a territory can be proclaimed" refusal.
    if !server.config.auroria_claims_enabled() {
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_CLOSED,
        );
        return;
    }
    // WP-S6 claim gates. The 1.2 client has no claim validator; it only renders the server's
    // error id, so the ids below are its error table (FUN_3956b860 in x2game.dll.c) and the
    // order is this server policy. The authored `open_*` window uses the real wall clock: the
    // GM test clock is restricted to the calendar view.
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    let window = calendar::SiegeCalendar::from_castles(&server.game_data.systems.housing.castles)
        .claim_window_at(zone, Utc::now().timestamp(), &settings.timezone);
    match window {
        calendar::ClaimWindow::Closed => {
            // 662 DOMINION_CLOSED: "not yet a region where a territory can be proclaimed".
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_CLOSED,
            );
            return;
        }
        calendar::ClaimWindow::NotOpen => {
            // 582 DOMINION_NOT_DECLARE_TIME: the window text is parameterized by open_*.
            super::util::send_error(
                player,
                crate::systems::economy::error_message::DOMINION_NOT_DECLARE_TIME,
            );
            return;
        }
        calendar::ClaimWindow::Open => {}
    }
    let persisted_faction = crate::db_character::persisted_faction(&player.data());
    let pirate = server
        .expeditions
        .get(expedition)
        .is_some_and(|e| e.faction.mother_id == crate::data::world_data::factions::PIRATE)
        || persisted_faction == crate::data::world_data::factions::PIRATE;
    if pirate {
        // 351 DOMINION_NOT_AVAILABLE_FOR_PIRATES: "Pirates cannot proclaim a territory".
        super::util::send_error(
            player,
            crate::systems::economy::error_message::DOMINION_NOT_AVAILABLE_FOR_PIRATES,
        );
        return;
    }
    let claim_limit = server.config.housing.dominion_claim_limit;
    let mut effects = None;
    let previous = server.systems.housing.dominions.get(zone);
    let claim = server.systems.housing.dominions.declare_unowned_with_limit(
        zone,
        identity,
        expedition,
        Utc::now().timestamp(),
        claim_limit,
        || {
            let mut txn = ItemTxn::begin(server);
            let inv = Inv::of(player);
            let Some(equipment) = inv.get(slot_type::EQUIPMENT) else {
                return false;
            };
            let mut equipment = equipment.lock().expect("equipment poisoned");
            let pack = equipment
                .get_item_by_slot(equip_slot::BACKPACK)
                .filter(|item| {
                    item.template_id == PURIFYING_ARCHEUM
                        && item.count == 1
                        && item.owner_id == u64::from(player.character_id)
                        && equipment.owner_id == player.character_id
                })
                .map(|item| item.id);
            let consumed = pack.is_some_and(|id| {
                equipment.consume_item(
                    task_type::SKILL_REAGENTS,
                    PURIFYING_ARCHEUM,
                    1,
                    Some(id),
                    &mut txn.env,
                ) == 1
            });
            drop(equipment);
            effects = Some(txn.into_fx());
            consumed
        },
    );
    // Special50 has no separate CraftEffect. Advance only after the authority and exact
    // pack consumption succeeded, while other housing operations remain serialized.
    if claim.is_ok() {
        crate::systems::housing::manager::add_build_action(server, tower);
    }
    drop(operation);
    if let Err(DominionError::TooManyOwned) = &claim {
        // 441 TOO_MANY_OWNED_DOMINIONS: the pack was not consumed (checked under the
        // authority lock before the callback).
        super::util::send_error(
            player,
            crate::systems::economy::error_message::TOO_MANY_OWNED_DOMINIONS,
        );
    }
    if let Some(effects) = effects {
        crate::systems::economy::fx::flush(server, effects);
    }
    if let Ok(state) = claim {
        let progress = {
            let h = tower.data();
            crate::systems::housing::packets::SCHouseBuildProgressPacket {
                tl: h.unit.tl_id,
                model_id: h.unit.model_id,
                all_step: h.all_action,
                cur_step: h.all_action,
            }
        };
        // Native 1.2 clamps UnitPoints against its current construction model's HP cap.
        // Install the finished model before sending the tower's completed health.
        crate::world::broadcast(&world, tower, &progress, true);
        apply_tower_state(server, &state);
        for doodad in server.systems.housing.manager.attached(tower.obj_id) {
            crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
        }
        broadcast_snapshot(server, &state, true);
        if let Some(previous) = previous {
            clear_national_monument_on_owner_change(server, &previous, &state);
        }
    }
}

/// `DominionData`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DominionData {
    pub zone_id: u16,
    pub expedition_id: u32,
    /// `House` (TODO in C#: an id?).
    pub house: u32,
    pub tax_rate: i32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub cur_house_tax_money: i32,
    pub cur_hunt_tax_money: i32,
    pub peace_tax_money: i32,
    pub cur_house_tax_aa_point: i32,
    pub peace_tax_aa_point: i32,
    pub last_paid_time: DateTime<Utc>,
    pub last_siege_end_time: DateTime<Utc>,
    pub reign_start_time: DateTime<Utc>,
    pub last_tax_rate_changed_time: DateTime<Utc>,
    /// `LastNationalTaxRateChagedTime` (sic).
    pub last_national_tax_rate_changed_time: DateTime<Utc>,
    pub national_tax_rate: u16,
    pub national_monument_db_id: i64,
    pub national_monument_x: f32,
    pub national_monument_y: f32,
    pub national_monument_z: f32,
    pub obj_id: u32,
    pub territory_data: DominionTerritoryData,
    pub siege_timers: DominionSiegeTimers,
    pub non_pvp_start: DateTime<Utc>,
    pub non_pvp_duration: u16,
}

impl Marshal for DominionData {
    fn write(&self, s: &mut PacketStream) {
        s.write_u16(self.zone_id)
            .write_u32(self.expedition_id)
            .write_u32(self.house)
            .write_i32(self.tax_rate)
            .write_i64(convert_coord_to_long(self.x))
            .write_i64(convert_coord_to_long(self.y))
            .write_f32(self.z)
            .write_i32(self.cur_house_tax_money)
            .write_i32(self.cur_hunt_tax_money)
            .write_i32(self.peace_tax_money)
            .write_i32(self.cur_house_tax_aa_point)
            .write_i32(self.peace_tax_aa_point)
            .write_date_time(self.last_paid_time)
            .write_date_time(self.last_siege_end_time)
            .write_date_time(self.reign_start_time)
            .write_date_time(self.last_tax_rate_changed_time)
            .write_date_time(self.last_national_tax_rate_changed_time)
            .write_u16(self.national_tax_rate)
            .write_i64(self.national_monument_db_id)
            .write_i64(convert_coord_to_long(self.national_monument_x))
            .write_i64(convert_coord_to_long(self.national_monument_y))
            .write_f32(self.national_monument_z)
            .write_bc(self.obj_id)
            .write(&self.territory_data)
            .write(&self.siege_timers)
            .write_date_time(self.non_pvp_start)
            .write_u16(self.non_pvp_duration);
    }
}

/// `DominionTerritoryData`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DominionTerritoryData {
    pub id: u32,
    pub id2: u32,
    pub max_gates: u8,
    pub max_walls: u8,
    pub radius_declare: i16,
    pub radius_dominion: u16,
    pub radius_offense_hq: i16,
    pub radius_siege: i16,
}

impl Marshal for DominionTerritoryData {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id)
            .write_u32(self.id2)
            .write_u8(self.max_gates)
            .write_u8(self.max_walls)
            .write_i16(self.radius_declare)
            .write_u16(self.radius_dominion)
            .write_i16(self.radius_offense_hq)
            .write_i16(self.radius_siege);
    }
}

/// `DominionSiegeTimers` (TODO in C#: the names may be wrong).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DominionSiegeTimers {
    pub durations: [i32; 5],
    pub started: DateTime<Utc>,
    pub fixed: DateTime<Utc>,
    pub bdm: i32,
    pub siege_period: u8,
    pub unk_data: DominionUnkData,
    pub unk2_data: DominionUnkData,
}

impl Marshal for DominionSiegeTimers {
    fn write(&self, s: &mut PacketStream) {
        for d in self.durations {
            s.write_i32(d);
        }
        s.write_date_time(self.started)
            .write_date_time(self.fixed)
            .write_i32(self.bdm)
            .write_u8(self.siege_period)
            .write(&self.unk_data)
            .write(&self.unk2_data);
    }
}

/// `DominionUnkData`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DominionUnkData {
    /// `Id` (TODO in C#: the expedition id).
    pub id: u32,
    pub obj_id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub ni: u8,
    pub nr: u8,
    pub limit: u8,
    /// `UnkIds` (the count is written as a byte).
    pub unk_ids: Vec<u32>,
}

impl Marshal for DominionUnkData {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id)
            .write_bc(self.obj_id)
            .write_i64(convert_coord_to_long(self.x))
            .write_i64(convert_coord_to_long(self.y))
            .write_f32(self.z)
            .write_u8(self.ni)
            .write_u8(self.nr)
            .write_u8(self.limit)
            .write_u8(self.unk_ids.len() as u8);
        for id in &self.unk_ids {
            s.write_u32(*id);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn long(v: f32) -> i64 {
        ((v * 4096.0) as i64) << 32
    }

    pub(crate) fn sample() -> DominionData {
        DominionData {
            zone_id: 0x0102,
            expedition_id: 3,
            house: 4,
            tax_rate: 5,
            x: 100.5,
            y: 200.25,
            z: 30.0,
            cur_house_tax_money: 6,
            cur_hunt_tax_money: 7,
            peace_tax_money: 8,
            cur_house_tax_aa_point: 9,
            peace_tax_aa_point: 10,
            last_paid_time: t(11),
            last_siege_end_time: t(12),
            reign_start_time: t(13),
            last_tax_rate_changed_time: t(14),
            last_national_tax_rate_changed_time: t(15),
            national_tax_rate: 16,
            national_monument_db_id: 17,
            national_monument_x: 18.0,
            national_monument_y: 19.0,
            national_monument_z: 20.0,
            obj_id: 0x0A0B0C,
            territory_data: DominionTerritoryData {
                id: 21,
                id2: 22,
                max_gates: 23,
                max_walls: 24,
                radius_declare: -25,
                radius_dominion: 26,
                radius_offense_hq: 27,
                radius_siege: 28,
            },
            siege_timers: DominionSiegeTimers {
                durations: [1, 2, 3, 4, 5],
                started: t(29),
                fixed: t(30),
                bdm: 31,
                siege_period: 32,
                unk_data: DominionUnkData {
                    id: 33,
                    obj_id: 34,
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                    ni: 4,
                    nr: 5,
                    limit: 6,
                    unk_ids: vec![7, 8],
                },
                unk2_data: DominionUnkData::default(),
            },
            non_pvp_start: t(35),
            non_pvp_duration: 36,
        }
    }

    /// The expected `Write` of [`sample`], spelled out with the C# primitives.
    pub(crate) fn sample_bytes() -> Vec<u8> {
        let mut e = PacketStream::new();
        e.write_u16(0x0102)
            .write_u32(3)
            .write_u32(4)
            .write_i32(5)
            .write_i64(long(100.5))
            .write_i64(long(200.25))
            .write_f32(30.0);
        for v in 6..=10 {
            e.write_i32(v);
        }
        for v in 11..=15 {
            e.write_i64(v);
        }
        e.write_u16(16)
            .write_i64(17)
            .write_i64(long(18.0))
            .write_i64(long(19.0))
            .write_f32(20.0)
            .write_bc(0x0A0B0C)
            // territory
            .write_u32(21)
            .write_u32(22)
            .write_u8(23)
            .write_u8(24)
            .write_i16(-25)
            .write_u16(26)
            .write_i16(27)
            .write_i16(28);
        // siege timers
        for v in 1..=5 {
            e.write_i32(v);
        }
        e.write_i64(29).write_i64(30).write_i32(31).write_u8(32);
        e.write_u32(33)
            .write_bc(34)
            .write_i64(long(1.0))
            .write_i64(long(2.0))
            .write_f32(3.0)
            .write_u8(4)
            .write_u8(5)
            .write_u8(6)
            .write_u8(2)
            .write_u32(7)
            .write_u32(8);
        e.write_u32(0)
            .write_bc(0)
            .write_i64(0)
            .write_i64(0)
            .write_f32(0.0)
            .write_raw(&[0, 0, 0, 0]);
        e.write_i64(35).write_u16(36);
        e.into_bytes()
    }

    #[test]
    fn dominion_data_write_is_byte_exact() {
        let mut s = PacketStream::new();
        sample().write(&mut s);
        assert_eq!(s.into_bytes(), sample_bytes());
    }
}
#[cfg(test)]
mod authority_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use chrono::{Datelike, TimeZone};

    use super::*;
    use crate::data::items::{ItemTemplate, TemplateKind};
    use crate::data::world_data::{SystemFaction, Zone};
    use crate::data::GameData;
    use crate::data::WorldTemplate;
    use crate::models::house::House;
    use crate::models::item::Item;
    use crate::net::registry::ClientPacket;
    use crate::opcodes::sc;
    use crate::systems::housing::data::{
        GuardTowerSetting, HousingBuildStep, HousingTemplate, SiegePlan, SiegeSettings, SiegeZone,
    };
    use crate::systems::housing::dominion::state::{
        SiegeLimits, SiegeOutcome, SiegeParticipant, SiegeState,
    };
    use crate::systems::skills::data::SkillTemplate;
    use crate::systems::skills::effects::EffectSource;
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::state::{SkillRef, SkillRun};
    use crate::systems::skills::types::SkillObject;
    use crate::systems::social::expedition::{self, Expedition};
    use crate::systems::social::packets::misc::CSUpdateDominionTaxRatePacket;
    use crate::test_support::{
        spawn_player, test_character, test_server_with_world, TestClient, TEST_ZONE,
    };
    use crate::world::{Entity, HouseRef};

    const ZONE: u32 = 25;
    const EXPEDITION: u32 = 77;
    const SETTING: u32 = 7;
    const DESIGN: u32 = 139;
    const WRONG_PACK: u32 = 26_672;

    struct Fixture {
        server: GameServer,
        client: TestClient,
        member_client: TestClient,
        leader: PlayerRef,
        member: PlayerRef,
        house: HouseRef,
        skill: SkillRef,
    }

    fn fixture() -> Fixture {
        let mut data = GameData::empty();
        data.zones.zones.insert(
            TEST_ZONE,
            Zone {
                id: 1,
                zone_key: TEST_ZONE,
                group_id: ZONE,
                ..Default::default()
            },
        );
        data.zones.zones.insert(
            TEST_ZONE + 1,
            Zone {
                id: 2,
                zone_key: TEST_ZONE + 1,
                group_id: ZONE + 1,
                ..Default::default()
            },
        );
        data.systems.housing.add_template(HousingTemplate {
            id: DESIGN,
            main_model_id: 894,
            hp: 1000,
            guard_tower_setting_id: SETTING,
            build_steps: BTreeMap::from([(
                0,
                HousingBuildStep {
                    id: 395,
                    housing_id: DESIGN,
                    step: 0,
                    model_id: 893,
                    skill_id: DECLARATION_SKILL,
                    num_actions: 1,
                },
            )]),
            ..Default::default()
        });
        data.systems.housing.castles.guard_tower_settings.insert(
            SETTING,
            GuardTowerSetting {
                id: SETTING,
                initial_buff_id: 4771,
                max_gates: 2,
                max_walls: 100,
                radius_declare: 450,
                radius_dominion: 200,
                radius_siege: 450,
                radius_offense_hq: 120,
                ..Default::default()
            },
        );
        // The shipped zone schedule: auction Wed 22:30, declare Fri 22:30, warmup Sun
        // 20:30, siege Sun 21:00 for 90 minutes. The plan rotation lists the zone in the
        // week of 2014-01-05 and three weeks later.
        data.systems.housing.castles.siege_zones.insert(
            ZONE,
            SiegeZone {
                id: 4,
                zone_group_id: ZONE,
                start_siege_weekday: 0,
                start_siege_hour: 21,
                start_siege_min: 0,
                siege_days: 0,
                siege_hours: 1,
                siege_mins: 30,
                pay_weekday: 0,
                pay_hour: 23,
                pay_min: 55,
                start_auction_weekday: 3,
                start_auction_hour: 22,
                start_auction_min: 30,
                start_declare_weekday: 5,
                start_declare_hour: 22,
                start_declare_min: 30,
                start_warmup_weekday: 0,
                start_warmup_hour: 20,
                start_warmup_min: 30,
                // The shipped claim window: Sunday 00:00 for 168 h, i.e. always open.
                open_weekday: 0,
                open_hour: 0,
                open_duration_hours: 168,
                reinforce_defense_delay_mins: 20,
                ..Default::default()
            },
        );
        data.systems.housing.castles.siege_delay_after_claim_days = 30;
        for (id, week) in [(1, "2014-01-05"), (2, "2014-01-26")] {
            data.systems
                .housing
                .castles
                .siege_plans
                .entry(ZONE)
                .or_default()
                .push(SiegePlan {
                    id,
                    zone_group_id: ZONE,
                    week_start: chrono::NaiveDateTime::parse_from_str(
                        &format!("{week} 00:00:00"),
                        "%Y-%m-%d %H:%M:%S",
                    )
                    .unwrap(),
                });
        }
        data.systems.skills.add_skill(SkillTemplate {
            id: DECLARATION_SKILL,
            max_range: 10,
            casting_time: 20_000,
            ..Default::default()
        });
        for id in [PURIFYING_ARCHEUM, WRONG_PACK] {
            data.items.templates.insert(
                id,
                ItemTemplate {
                    kind: TemplateKind::Backpack { backpack_type: 1 },
                    ..ItemTemplate::plain(id, 1)
                },
            );
        }
        let mut world = WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE);
        world.zone_keys.push(TEST_ZONE + 1);
        assert!(world.set_zone_key_at(1, 0, TEST_ZONE + 1));
        let server = test_server_with_world(data, world);
        server
            .config
            .update(|c| c.housing.siege_calendar_timezone = "+00:00".into());
        let mut client = TestClient::new(&server, 1, 1001);
        let leader = spawn_player(
            &server,
            &client,
            test_character(1, "Lord", 100.0, 100.0),
            0x100,
        );
        let mut member_client = TestClient::new(&server, 2, 1002);
        let member = spawn_player(
            &server,
            &member_client,
            test_character(2, "Member", 100.0, 100.0),
            0x101,
        );
        for p in [&leader, &member] {
            p.data().unit.hp = 100;
            p.data().unit.expedition_id = EXPEDITION;
        }
        server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: EXPEDITION,
                owner_id: 1,
                name: "Castle".into(),
                ..Default::default()
            },
            members: vec![
                expedition::member_from_character(&server, EXPEDITION, &leader, true),
                expedition::member_from_character(&server, EXPEDITION, &member, false),
            ],
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
        let mut h = House::new(13, DESIGN);
        h.current_step = 0;
        h.build_step_count = 1;
        h.all_action = 1;
        h.unit.model_id = 893;
        h.unit.hp = 1000;
        h.unit.max_hp = 1000;
        h.unit.tl_id = 12;
        h.unit.transform.set_xyz(105.0, 100.0, 100.0);
        h.unit.transform.zone_id = TEST_ZONE;
        let house = Entity::new(0x200, h);
        assert!(server.systems.housing.manager.insert(&house));
        crate::systems::housing::manager::spawn_house(&server, &house);
        let (_, identity) = tower_identity(&server, &house).unwrap();
        server
            .systems
            .housing
            .dominions
            .register_tower(ZONE, identity)
            .unwrap();
        let skill = SkillRun::new(
            server
                .game_data
                .systems
                .skills
                .skill(DECLARATION_SKILL)
                .unwrap()
                .clone(),
            1,
        );
        skill.state().generation = 1;
        skill.state().initial_target = house.obj_id;
        skill.state().tl_id = 5;
        client.conn.active_char = Some(leader.clone());
        client.drain();
        member_client.drain();
        Fixture {
            server,
            client,
            member_client,
            leader,
            member,
            house,
            skill,
        }
    }

    fn args<'a>(f: &Fixture, object: &'a SkillObject) -> EffectArgs<'a> {
        args_for(f, object, &f.house, &f.skill)
    }

    fn args_for<'a>(
        f: &Fixture,
        object: &'a SkillObject,
        tower: &HouseRef,
        skill: &SkillRef,
    ) -> EffectArgs<'a> {
        EffectArgs {
            caster: Some(WorldObject::from(&f.leader)),
            caster_caster: SkillCaster::Unit(f.leader.obj_id),
            target: Target::Obj(WorldObject::from(tower)),
            target_caster: SkillCastTarget::Unit(tower.obj_id),
            cast: CastAction::Skill {
                skill_id: DECLARATION_SKILL,
                tl_id: 5,
            },
            source: EffectSource::from_skill(skill.clone()),
            skill_object: object,
        }
    }

    /// A second unowned lodestone in its own zone group, for the owned-territory limit.
    fn second_tower(f: &mut Fixture) -> (HouseRef, SkillRef) {
        let f = &mut *f;
        data_mut(f).systems.housing.castles.siege_zones.insert(
            ZONE + 1,
            SiegeZone {
                id: 5,
                zone_group_id: ZONE + 1,
                open_weekday: 0,
                open_hour: 0,
                open_duration_hours: 168,
                ..Default::default()
            },
        );
        let mut h = House::new(14, DESIGN);
        h.current_step = 0;
        h.build_step_count = 1;
        h.all_action = 1;
        h.unit.model_id = 893;
        h.unit.hp = 1000;
        h.unit.max_hp = 1000;
        h.unit.tl_id = 13;
        h.unit.transform.set_xyz(100.0, 32.0, 100.0);
        h.unit.transform.zone_id = TEST_ZONE + 1;
        let house = Entity::new(0x201, h);
        assert!(f.server.systems.housing.manager.insert(&house));
        crate::systems::housing::manager::spawn_house(&f.server, &house);
        let (_, identity) = tower_identity(&f.server, &house).unwrap();
        f.server
            .systems
            .housing
            .dominions
            .register_tower(ZONE + 1, identity)
            .unwrap();
        let skill = SkillRun::new(
            f.server
                .game_data
                .systems
                .skills
                .skill(DECLARATION_SKILL)
                .unwrap()
                .clone(),
            1,
        );
        skill.state().generation = 1;
        skill.state().initial_target = house.obj_id;
        skill.state().tl_id = 5;
        (house, skill)
    }

    fn data_mut(f: &mut Fixture) -> &mut GameData {
        Arc::get_mut(&mut f.server.game_data).expect("game data is uniquely owned")
    }

    fn errors(client: &mut TestClient) -> Vec<i16> {
        client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
            .map(|(_, body)| i16::from_le_bytes([body[0], body[1]]))
            .collect()
    }

    fn set_clock(f: &Fixture, now: i64) {
        f.server.systems.housing.calendar.set_clock(Some(now));
        f.server
            .config
            .update(|c| c.housing.siege_calendar_timezone = "+00:00".into());
    }

    const CAMP_ITEM: u32 = 21_130;
    const CAMP_ITEM_OTHER: u32 = 21_134;
    const DECLARE_SKILL_ID: u32 = 16_918;

    fn add_siege_master(f: &Fixture, expedition: u32, policy_siege_master: bool) {
        f.server.expeditions.with_mut(expedition, |guild| {
            guild.policies.push(expedition::ExpeditionRolePolicy {
                expedition_id: expedition,
                role: 255,
                name: "master".into(),
                dominion_declare: true,
                invite: true,
                expel: true,
                promote: true,
                dismiss: true,
                chat: true,
                manager_chat: true,
                siege_master: policy_siege_master,
                join_siege: true,
            });
        });
    }

    fn add_camp_use_data(f: &mut Fixture) {
        let data = data_mut(f);
        data.systems
            .housing
            .castles
            .siege_zones
            .get_mut(&ZONE)
            .unwrap()
            .declare_item_id = CAMP_ITEM;
        for id in [CAMP_ITEM, CAMP_ITEM_OTHER] {
            data.items.templates.insert(
                id,
                ItemTemplate {
                    use_skill_id: DECLARE_SKILL_ID,
                    ..ItemTemplate::plain(id, 1)
                },
            );
        }
        data.systems.npcs.doodad.templates.insert(
            4307,
            crate::systems::npcs::doodad::templates::DoodadTemplate {
                id: 4307,
                ..Default::default()
            },
        );
        data.systems.skills.add_effect(
            crate::systems::skills::data::EffectKind::SpecialEffect,
            crate::systems::skills::data::EffectTemplate::Special(
                crate::systems::skills::data::effects::SpecialEffect {
                    id: 5564,
                    special_effect_type_id: 15,
                    value1: 4307,
                    value2: 0,
                    value3: 0,
                    value4: 0,
                },
            ),
            5564,
        );
        let mut effect = crate::systems::skills::tests::skill_effect(
            1,
            crate::systems::skills::data::EffectKind::SpecialEffect,
            5564,
        );
        effect.consume_source_item = true;
        effect.consume_item_count = 1;
        data.systems.skills.add_skill(SkillTemplate {
            id: DECLARE_SKILL_ID,
            target_type: 0,
            target_selection: 1,
            max_range: 4,
            effects: vec![effect],
            ..Default::default()
        });
        // WP-SD3 classes: the tank mine (only inside the siege circle during the siege) and an
        // escape scroll (everything except the siege/HQ areas).
        data.systems.skills.add_skill(SkillTemplate {
            id: 20_000,
            target_type: 0,
            target_selection: 1,
            max_range: 4,
            ..Default::default()
        });
        for id in [26_397u32, 34_018] {
            data.items.templates.insert(
                id,
                ItemTemplate {
                    use_skill_id: 20_000,
                    ..ItemTemplate::plain(id, 1)
                },
            );
        }
        use crate::systems::housing::dominion::siege_items::SiegeItem;
        data.systems.housing.castles.siege_items.insert(
            26_397,
            SiegeItem {
                item_id: 26_397,
                siege_circle_during_siege: true,
                ..Default::default()
            },
        );
        data.systems.housing.castles.siege_items.insert(
            34_018,
            SiegeItem {
                item_id: 34_018,
                outside_siege_zone: true,
                during_no_dominion: true,
                during_declare: true,
                during_peace: true,
                outside_siege_area_during_warmup: true,
                outside_siege_area_during_siege: true,
                ..Default::default()
            },
        );
    }

    const CAMP_ATTACKER: u32 = 88;

    fn add_camp_attacker(f: &mut Fixture) {
        f.server.expeditions.with_mut(EXPEDITION, |guild| {
            guild
                .members
                .retain(|member| member.character_id != f.member.character_id);
        });
        f.server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: CAMP_ATTACKER,
                owner_id: f.member.character_id,
                name: "Camp".into(),
                ..Default::default()
            },
            members: vec![expedition::member_from_character(
                &f.server,
                CAMP_ATTACKER,
                &f.member,
                true,
            )],
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
        add_siege_master(f, CAMP_ATTACKER, true);
        f.member.data().unit.expedition_id = CAMP_ATTACKER;
        f.member.data().is_online = true;
        f.member.data().unit.transform.set_xyz(600.0, 100.0, 100.0);
        f.member_client.conn.active_char = Some(f.member.clone());
    }

    fn camp_fixture() -> Fixture {
        let mut f = fixture();
        crate::systems::economy::bridges::install(&f.server);
        crate::systems::npcs::install_hooks(&f.server);
        add_camp_use_data(&mut f);
        enable_calendar(&f);
        owned(&f);
        add_siege_master(&f, EXPEDITION, true);
        add_camp_attacker(&mut f);
        f.server
            .systems
            .housing
            .calendar
            .set_clock(Some(kst(2014, 1, 10, 22, 31)));
        f.leader.data().is_online = true;
        f.leader.data().unit.transform.set_xyz(600.0, 100.0, 100.0);
        f
    }

    fn give_camp_item(f: &Fixture, player: &PlayerRef, template: u32) -> u64 {
        let id = u64::from(f.server.ids.item.get_next_id().unwrap());
        let mut item = Item::create(
            id,
            f.server.game_data.items.template(template).unwrap(),
            1,
            0,
            1,
            &f.server.game_data.items,
            0,
        );
        item.owner_id = u64::from(player.character_id);
        item.slot_type = slot_type::INVENTORY;
        let container = Inv::of(player).get(slot_type::INVENTORY).unwrap();
        assert!(
            container
                .lock()
                .unwrap()
                .add_new_item(item, 0, &f.server.game_data.items)
                .unwrap(),
            "camp item placed in the bag"
        );
        f.server.items.add_container(container);
        id
    }

    fn item_count(f: &Fixture, player: &PlayerRef, template: u32) -> i32 {
        f.server
            .systems
            .skills
            .ext
            .items()
            .item_count(&f.server, player, template)
    }

    fn cast_camp(
        server: &GameServer,
        client: &mut TestClient,
        player: &PlayerRef,
        item_id: u64,
        template: u32,
    ) {
        cast_item(server, client, player, item_id, template, DECLARE_SKILL_ID);
    }

    fn cast_item(
        server: &GameServer,
        client: &mut TestClient,
        player: &PlayerRef,
        item_id: u64,
        template: u32,
        skill_id: u32,
    ) {
        player.data().unit.skill_last_used_ms = 0;
        player.data().unit.global_cooldown_ms = 0;
        crate::systems::skills::handlers::start_skill(
            server,
            &mut client.conn,
            crate::systems::skills::handlers::CSStartSkillPacket {
                skill_id,
                caster: SkillCaster::Item {
                    obj_id: player.obj_id,
                    item_id,
                    item_template_id: template,
                    type1: 0,
                    type2: 0,
                },
                target: SkillCastTarget::Unit(player.obj_id),
                flag: 0,
                skill_object: crate::systems::skills::types::SkillObject::None,
            },
        );
    }

    fn give_pack(f: &Fixture, id: u64, template_id: u32) {
        give_pack_for(f, &f.leader, id, template_id);
    }

    fn give_pack_for(f: &Fixture, player: &PlayerRef, id: u64, template_id: u32) {
        let item = Item::create(
            id,
            f.server.game_data.items.template(template_id).unwrap(),
            1,
            0,
            1,
            &f.server.game_data.items,
            0,
        );
        let equipment = Inv::of(player).equipment();
        equipment
            .lock()
            .unwrap()
            .add_new_item(item, equip_slot::BACKPACK, &f.server.game_data.items)
            .unwrap();
        f.server.items.add_container(equipment);
    }

    fn owned(f: &Fixture) -> DominionState {
        let (_, identity) = tower_identity(&f.server, &f.house).unwrap();
        f.server
            .systems
            .housing
            .dominions
            .declare_unowned_with(ZONE, identity, EXPEDITION, 90, || true)
            .unwrap()
    }

    #[tokio::test]
    async fn snapshot_uses_live_tower_authored_limits_and_native_timer_seconds() {
        let f = fixture();
        // This test pins the explicit-timer view; the calendar must not supply a cycle.
        f.server
            .config
            .update(|c| c.housing.siege_calendar = Some(false));
        let mut state = owned(&f);
        state.national_monument_db_id = 77;
        state.national_monument_x = 10.5;
        state.national_monument_y = 20.0;
        state.national_monument_z = 30.0;
        let data = snapshot(&f.server, &state, 100).unwrap();
        assert_eq!(
            (data.zone_id, data.expedition_id, data.house, data.obj_id),
            (25, 77, 13, 0x200)
        );
        assert_eq!(
            (
                data.national_monument_db_id,
                data.national_monument_x,
                data.national_monument_y,
                data.national_monument_z
            ),
            (77, 10.5, 20.0, 30.0)
        );
        assert_eq!((data.x, data.y, data.z), (105.0, 100.0, 100.0));
        assert_eq!(
            data.territory_data,
            DominionTerritoryData {
                id: SETTING,
                id2: 4771,
                max_gates: 2,
                max_walls: 100,
                radius_declare: 450,
                radius_dominion: 200,
                radius_offense_hq: 120,
                radius_siege: 450,
            }
        );
        assert_eq!(
            (
                data.cur_house_tax_money,
                data.cur_hunt_tax_money,
                data.peace_tax_money,
                data.cur_house_tax_aa_point,
                data.peace_tax_aa_point,
                data.national_tax_rate
            ),
            (0, 0, 0, 0, 0, 0)
        );
        assert_eq!(data.reign_start_time.timestamp(), 90);
        assert_eq!(data.siege_timers.siege_period, 5);
        state.last_siege_id = 1;
        state.siege = Some(SiegeState {
            siege_id: 1,
            attacker_expedition_id: 88,
            defender_expedition_id: EXPEDITION,
            ownership_generation: state.ownership_generation,
            status: SiegeStatus::Scheduled,
            outcome: SiegeOutcome::Pending,
            scheduled_at: 100,
            starts_at: 120,
            ends_at: 420,
            ended_at: 0,
            reinforced: false,
            defender_ni: 100,
            defender_nr: 100,
            outcome_credited: false,
            materialised_from_declaration: false,
            calendar_cycle: 0,
            participants: BTreeMap::from([
                (
                    1,
                    SiegeParticipant {
                        character_id: 1,
                        expedition_id: 77,
                        side: SiegeSide::Defender,
                    },
                ),
                (
                    2,
                    SiegeParticipant {
                        character_id: 2,
                        expedition_id: 88,
                        side: SiegeSide::Attacker,
                    },
                ),
            ]),
        });
        let warmup = snapshot(&f.server, &state, 119).unwrap().siege_timers;
        assert_eq!(warmup.siege_period, 3);
        assert_eq!(warmup.durations, [0, 0, 20, 300, 0]);
        assert_eq!(warmup.started.timestamp(), 100);
        assert_eq!((warmup.unk_data.id, warmup.unk2_data.id), (77, 88));
        assert_eq!((warmup.unk_data.limit, warmup.unk2_data.limit), (100, 100));
        assert_eq!((warmup.unk_data.ni, warmup.unk_data.nr), (100, 100));
        assert_eq!((warmup.unk2_data.ni, warmup.unk2_data.nr), (100, 100));
        assert_eq!(warmup.unk_data.unk_ids, vec![1]);
        assert_eq!(warmup.unk2_data.unk_ids, vec![2]);
        assert_eq!(
            snapshot(&f.server, &state, 120)
                .unwrap()
                .siege_timers
                .siege_period,
            4
        );
        assert_eq!(
            snapshot(&f.server, &state, 420)
                .unwrap()
                .siege_timers
                .siege_period,
            5
        );
        state.tower.instance_id = 99;
        assert!(snapshot(&f.server, &state, 200).is_none());
    }

    #[tokio::test]
    async fn snapshot_carries_data_driven_team_limits_and_reinforcement_delay() {
        let mut f = fixture();
        owned(&f);
        data_mut(&mut f)
            .systems
            .housing
            .castles
            .siege_settings
            .insert(
                1,
                SiegeSettings {
                    total_castles: 1,
                    num_defenders: 50,
                    num_reinforcements: 20,
                },
            );
        let now = Utc::now().timestamp();
        let mut state = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege_with_limits(
                ZONE,
                88,
                now + 300,
                now + 900,
                now,
                SiegeLimits { ni: 50, nr: 70 },
            )
            .unwrap();
        let timers = snapshot(&f.server, &state, now).unwrap().siege_timers;
        // The fixture's `siege_zones` row authors a 20-minute reinforcement delay.
        assert_eq!(timers.bdm, 1_200);
        assert_eq!(
            (
                timers.unk_data.ni,
                timers.unk_data.nr,
                timers.unk_data.limit
            ),
            (50, 70, 50)
        );
        assert_eq!(
            (
                timers.unk2_data.ni,
                timers.unk2_data.nr,
                timers.unk2_data.limit
            ),
            (70, 70, 70)
        );

        state.siege.as_mut().unwrap().reinforced = true;
        let timers = snapshot(&f.server, &state, now).unwrap().siege_timers;
        assert_eq!(timers.unk_data.limit, 70);
    }

    #[tokio::test]
    async fn siege_state_body_carries_bdm_and_team_limits_at_native_offsets() {
        let mut f = fixture();
        owned(&f);
        data_mut(&mut f)
            .systems
            .housing
            .castles
            .siege_settings
            .insert(
                1,
                SiegeSettings {
                    total_castles: 1,
                    num_defenders: 50,
                    num_reinforcements: 20,
                },
            );
        let now = Utc::now().timestamp();
        let state = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege_with_limits(
                ZONE,
                88,
                now,
                now + 900,
                now,
                SiegeLimits { ni: 50, nr: 70 },
            )
            .unwrap();
        let data = snapshot(&f.server, &state, now).unwrap();
        let bytes = crate::systems::social::packets::body(
            &crate::systems::social::packets::misc::SCSiegeStatePacket {
                id: data.zone_id,
                territory_data: data.territory_data,
                siege_timers: data.siege_timers.clone(),
                new_owner: 0,
                reign_start_time: data.reign_start_time,
            },
        );
        // Offsets from native Deserialize 0x397B4840 with empty team id lists: bdm at 56,
        // the defender's ni/nr/limit at 88..91 and the attacker's at 119..122. Each team
        // record is 4 id + 3 obj_id + 8 x + 8 y + 4 z then ni/nr/limit/count (31 bytes plus
        // 4 per id).
        assert_eq!(&bytes[56..60], &1_200i32.to_le_bytes());
        assert_eq!(&bytes[88..91], &[50, 70, 50]);
        assert_eq!(&bytes[119..122], &[70, 70, 70]);
        assert_eq!(bytes.len(), 127);
    }

    #[tokio::test]
    async fn out_of_range_reinforcement_delay_is_clamped_instead_of_dropping_the_snapshot() {
        let mut f = fixture();
        owned(&f);
        data_mut(&mut f)
            .systems
            .housing
            .castles
            .siege_zones
            .get_mut(&ZONE)
            .unwrap()
            .reinforce_defense_delay_mins = u32::MAX;
        let now = Utc::now().timestamp();
        let state = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege_with_limits(
                ZONE,
                88,
                now,
                now + 900,
                now,
                SiegeLimits { ni: 50, nr: 70 },
            )
            .unwrap();
        // The authored minutes do not fit the signed wire field; the territory's snapshot is
        // still valid and carries the saturated value.
        let timers = snapshot(&f.server, &state, now).unwrap().siege_timers;
        assert_eq!(timers.bdm, i32::MAX);
    }

    #[tokio::test]
    async fn reinforcement_wave_is_announced_once_at_start_plus_bdm_through_the_tick() {
        let mut f = fixture();
        owned(&f);
        data_mut(&mut f)
            .systems
            .housing
            .castles
            .siege_settings
            .insert(
                1,
                SiegeSettings {
                    total_castles: 1,
                    num_defenders: 50,
                    num_reinforcements: 20,
                },
            );
        let now = Utc::now().timestamp();
        // `starts_at == now`: the wave deadline (start + 20 min) is still in the future.
        f.server
            .systems
            .housing
            .dominions
            .schedule_siege_with_limits(
                ZONE,
                88,
                now,
                now + 3_600,
                now,
                SiegeLimits { ni: 50, nr: 70 },
            )
            .unwrap();
        f.client.drain();
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(!f.client.drain_ops().contains(&sc::SCSiegeReinforcePacket));
        assert!(
            !f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .siege
                .as_ref()
                .unwrap()
                .reinforced
        );

        // A siege whose start is 21 minutes in the past is already past the wave deadline.
        let mut due = f.server.systems.housing.dominions.get(ZONE).unwrap();
        let siege = due.siege.as_mut().unwrap();
        siege.starts_at = now - 1_260;
        siege.scheduled_at = now - 1_300;
        siege.status = SiegeStatus::Active;
        due.validate().unwrap();
        f.server
            .systems
            .housing
            .dominions
            .restore(vec![due])
            .unwrap();
        f.client.drain();
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        let reinforce: Vec<_> = f
            .client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCSiegeReinforcePacket)
            .collect();
        assert_eq!(reinforce.len(), 1);
        assert_eq!(reinforce[0].1, (ZONE as u16).to_le_bytes().to_vec());
        assert!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .siege
                .as_ref()
                .unwrap()
                .reinforced
        );

        // Exactly once: a repeated tick sends no second packet. The persisted flag itself is
        // covered by the MySQL round-trip test (`private_mysql_dominion_...`).
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(!f.client.drain_ops().contains(&sc::SCSiegeReinforcePacket));
    }

    #[tokio::test]
    async fn peace_starts_the_authored_minutes_and_a_second_cast_is_refused() {
        let mut f = fixture();
        owned(&f);
        let object = SkillObject::None;
        let args = args(&f, &object);
        let before = Utc::now().timestamp();
        crate::systems::housing::dominion::peace::start_effect(&f.server, &args, 120);
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        assert_eq!(state.non_pvp_duration_min, 120);
        assert!((before..=before + 5).contains(&state.non_pvp_start_at));
        let data = snapshot(&f.server, &state, state.non_pvp_start_at + 1).unwrap();
        assert_eq!(data.non_pvp_start.timestamp(), state.non_pvp_start_at);
        // Native DominionData stores the window end as start + nonPvPDuration with no scale
        // factor, so the snapshot field is seconds while the 0x024 packet carries minutes.
        assert_eq!(data.non_pvp_duration, 7200);
        let frames = f.client.drain();
        let body = frames
            .iter()
            .find(|(op, _)| *op == crate::opcodes::sc::SCDominionDataPacket)
            .map(|(_, body)| body)
            .expect("the peace cast broadcasts a snapshot");
        let tail = &body[body.len() - 12..body.len() - 2];
        assert_eq!(
            i64::from_le_bytes(tail[..8].try_into().unwrap()),
            state.non_pvp_start_at
        );
        assert_eq!(u16::from_le_bytes([tail[8], tail[9]]), 7200);

        crate::systems::housing::dominion::peace::start_effect(&f.server, &args, 120);
        assert_eq!(f.server.systems.housing.dominions.get(ZONE).unwrap(), state);
        let errors: Vec<i16> = f
            .client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
            .map(|(_, body)| i16::from_le_bytes([body[0], body[1]]))
            .collect();
        assert_eq!(
            errors,
            vec![crate::systems::economy::error_message::ALREADY_IN_NON_PVP_DURATION]
        );
    }

    #[tokio::test]
    async fn peace_cast_emits_one_native_non_pvp_started_notification() {
        let mut f = fixture();
        owned(&f);
        f.client.drain();
        let object = SkillObject::None;
        crate::systems::housing::dominion::peace::start_effect(&f.server, &args(&f, &object), 120);
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        let started: Vec<_> = f
            .client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::CDominionStartUnkPacket)
            .collect();
        assert_eq!(started.len(), 1);
        // Native Deserialize 0x397B7BB0: u16 zone group, DateTime start, u16 minutes.
        let mut expected = PacketStream::new();
        expected
            .write_u16(ZONE as u16)
            .write_i64(state.non_pvp_start_at)
            .write_u16(120);
        assert_eq!(started[0].1, expected.into_bytes());
        crate::systems::housing::dominion::peace::start_effect(&f.server, &args(&f, &object), 120);
        assert!(!f.client.drain_ops().contains(&sc::CDominionStartUnkPacket));
    }

    #[tokio::test]
    async fn forced_release_emits_one_deleted_notification_only_when_the_owner_is_cleared() {
        let mut f = fixture();
        owned(&f);
        f.server
            .systems
            .housing
            .dominions
            .start_non_pvp(ZONE, EXPEDITION, 120, Utc::now().timestamp() - 10)
            .unwrap();
        f.client.drain();
        let released = f
            .server
            .systems
            .housing
            .dominions
            .release_expedition(EXPEDITION, Utc::now().timestamp());
        assert_eq!(released.len(), 1);
        let (previous, state) = &released[0];
        assert_eq!(
            (previous.owner_expedition_id, state.owner_expedition_id),
            (EXPEDITION, 0)
        );
        // The released record is dropped on the client, so no 0x025 is sent; a later owner
        // must not inherit the old owner's peace window.
        assert!(previous.non_pvp_start_at != 0);
        assert_eq!((state.non_pvp_start_at, state.non_pvp_duration_min), (0, 0));
        apply_released(&f.server, previous, state);
        let deleted: Vec<_> = f
            .client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCDominionDeletedPacket)
            .collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].1, (ZONE as u16).to_le_bytes().to_vec());

        // Releasing an expedition that is only the attacker of another guild's siege cancels
        // that siege but keeps the owner's cached record.
        owned(&f);
        let now = Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, 88, now + 100, now + 200, now)
            .unwrap();
        f.client.drain();
        let released = f
            .server
            .systems
            .housing
            .dominions
            .release_expedition(88, now);
        assert_eq!(released.len(), 1);
        let (previous, state) = &released[0];
        assert_eq!(
            (previous.owner_expedition_id, state.owner_expedition_id),
            (EXPEDITION, EXPEDITION)
        );
        apply_released(&f.server, previous, state);
        assert!(!f.client.drain_ops().contains(&sc::SCDominionDeletedPacket));
    }

    #[tokio::test]
    async fn natural_non_pvp_expiry_emits_one_stopped_notification_and_is_idempotent() {
        let mut f = fixture();
        owned(&f);
        let end = Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .start_non_pvp(ZONE, EXPEDITION, 120, end - 120 * 60)
            .unwrap();
        f.client.drain();
        crate::systems::social::dominion_notifications::expire_non_pvp(&f.server, end);
        let stopped: Vec<_> = f
            .client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCDominionEndUnkPacket)
            .collect();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].1, (ZONE as u16).to_le_bytes().to_vec());
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        assert_eq!((state.non_pvp_start_at, state.non_pvp_duration_min), (0, 0));
        crate::systems::social::dominion_notifications::expire_non_pvp(&f.server, end);
        assert!(!f.client.drain_ops().contains(&sc::SCDominionEndUnkPacket));
    }

    #[tokio::test]
    async fn dominion_tick_once_expires_the_non_pvp_window() {
        let mut f = fixture();
        owned(&f);
        let now = Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .start_non_pvp(ZONE, EXPEDITION, 120, now - 120 * 60 - 1)
            .unwrap();
        f.client.drain();
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        let stopped: Vec<_> = f
            .client
            .drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCDominionEndUnkPacket)
            .collect();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].1, (ZONE as u16).to_le_bytes().to_vec());
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        assert_eq!((state.non_pvp_start_at, state.non_pvp_duration_min), (0, 0));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(!f.client.drain_ops().contains(&sc::SCDominionEndUnkPacket));
    }

    #[tokio::test]
    async fn declaration_consumes_only_exact_equipped_pack_once_and_restores_snapshot() {
        let mut f = fixture();
        let object = SkillObject::None;
        f.house.data().unit.hp = 500;
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        declare_effect(&f.server, &args(&f, &object));
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        assert_eq!(state.owner_expedition_id, EXPEDITION);
        assert!(Inv::of(&f.leader).equipped(equip_slot::BACKPACK).is_none());
        assert_eq!(f.house.data().unit.expedition_id, EXPEDITION);
        assert_eq!(f.house.data().current_step, -1);
        assert_eq!(f.house.data().unit.model_id, 894);
        assert_eq!(f.house.data().unit.owner_id, 0);
        assert_eq!(f.house.data().unit.hp, 1000);
        let packets = f.client.drain();
        let progress = packets
            .iter()
            .position(|(op, _)| *op == sc::SCHouseBuildProgressPacket)
            .unwrap();
        let points = packets
            .iter()
            .position(|(op, body)| *op == sc::SCUnitPointsPacket && body.starts_with(&[0, 2, 0]))
            .unwrap();
        let snapshot = packets
            .iter()
            .position(|(op, _)| *op == sc::SCDominionDataPacket)
            .unwrap();
        assert!(progress < points && points < snapshot);
        assert_eq!(
            u32::from_le_bytes(packets[progress].1[2..6].try_into().unwrap()),
            894
        );
        assert_eq!(
            i32::from_le_bytes(packets[points].1[3..7].try_into().unwrap()),
            100_000
        );
        let snapshots: Vec<_> = packets
            .into_iter()
            .filter(|(op, _)| *op == sc::SCDominionDataPacket)
            .collect();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(&snapshots[0].1[snapshots[0].1.len() - 2..], &[1, 1]);
        give_pack(&f, 101, PURIFYING_ARCHEUM);
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            Inv::of(&f.leader)
                .equipped(equip_slot::BACKPACK)
                .unwrap()
                .id,
            101
        );
        assert_eq!(f.server.systems.housing.dominions.get(ZONE).unwrap(), state);
        // Reload the authority cache, then world entry must send the same persistent owner.
        f.server
            .systems
            .housing
            .dominions
            .restore(vec![state.clone()])
            .unwrap();
        send_snapshots(&f.server, &f.leader);
        let packets = f.client.drain();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, sc::SCDominionDataPacket);
        assert_eq!(&packets[0].1[packets[0].1.len() - 2..], &[0, 1]);
        assert_eq!(
            u32::from_le_bytes(packets[0].1[2..6].try_into().unwrap()),
            EXPEDITION
        );
    }

    #[tokio::test]
    async fn declaration_rejects_completed_or_mismatched_authored_claim_action() {
        let object = SkillObject::None;
        for (step, base_action, num_action, owner) in [
            (-1, 0, 0, 0),
            (1, 0, 0, 0),
            (0, 1, 0, 0),
            (0, 0, 1, 0),
            (0, 0, 0, 1),
        ] {
            let mut f = fixture();
            give_pack(&f, 100, PURIFYING_ARCHEUM);
            {
                let mut h = f.house.data();
                h.current_step = step;
                h.base_action = base_action;
                h.num_action = num_action;
                h.unit.owner_id = owner;
            }
            declare_effect(&f.server, &args(&f, &object));
            assert_eq!(
                Inv::of(&f.leader)
                    .equipped(equip_slot::BACKPACK)
                    .unwrap()
                    .id,
                100
            );
            assert_eq!(
                f.server
                    .systems
                    .housing
                    .dominions
                    .get(ZONE)
                    .unwrap()
                    .owner_expedition_id,
                0
            );
            assert_eq!(f.house.data().current_step, step);
            assert_eq!(f.house.data().unit.model_id, 893);
            assert!(f.client.drain().is_empty());
        }
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        let mut template = f
            .server
            .game_data
            .systems
            .housing
            .template(DESIGN)
            .unwrap()
            .clone();
        template.build_steps.get_mut(&0).unwrap().skill_id = 13_660;
        Arc::get_mut(&mut f.server.game_data)
            .unwrap()
            .systems
            .housing
            .add_template(template);
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            Inv::of(&f.leader)
                .equipped(equip_slot::BACKPACK)
                .unwrap()
                .id,
            100
        );
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            0
        );
        assert_eq!(f.house.data().current_step, 0);
        assert!(f.client.drain().is_empty());
    }

    #[tokio::test]
    async fn rejected_declarations_keep_pack_and_owner() {
        let f = fixture();
        let object = SkillObject::None;
        give_pack(&f, 100, WRONG_PACK);
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            Inv::of(&f.leader)
                .equipped(equip_slot::BACKPACK)
                .unwrap()
                .template_id,
            WRONG_PACK
        );
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            0
        );

        let f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        give_pack_for(&f, &f.member, 200, PURIFYING_ARCHEUM);
        let original = args(&f, &object);
        crate::systems::skills::ext::SocialSkillHooks::declare_dominion(
            &crate::systems::social::bridges::SocialForSkills,
            &f.server,
            &WorldObject::from(&f.leader),
            EXPEDITION,
            &WorldObject::from(&f.house),
        );
        let mut rejected = original.clone();
        rejected.caster = Some(WorldObject::from(&f.member));
        rejected.caster_caster = SkillCaster::Unit(f.member.obj_id);
        declare_effect(&f.server, &rejected);
        assert_eq!(
            Inv::of(&f.member)
                .equipped(equip_slot::BACKPACK)
                .unwrap()
                .id,
            200
        );
        rejected = original.clone();
        rejected.source.is_trigger = true;
        declare_effect(&f.server, &rejected);
        rejected = original.clone();
        let forged = SkillRun::new(Arc::new(f.skill.template.as_ref().clone()), 1);
        forged.state().generation = 1;
        forged.state().initial_target = f.house.obj_id;
        forged.state().tl_id = 5;
        rejected.source.skill = Some(forged);
        declare_effect(&f.server, &rejected);
        rejected = original.clone();
        rejected.target_caster = SkillCastTarget::Unit(0x201);
        declare_effect(&f.server, &rejected);
        f.skill.state().initial_target = 0x201;
        declare_effect(&f.server, &original);
        f.skill.state().initial_target = f.house.obj_id;
        f.house.data().unit.transform.set_xyz(111.0, 100.0, 100.0);
        declare_effect(&f.server, &original);
        f.house.data().unit.transform.set_xyz(105.0, 100.0, 100.0);
        f.skill.state().cancelled = true;
        declare_effect(&f.server, &original);
        assert_eq!(
            Inv::of(&f.leader)
                .equipped(equip_slot::BACKPACK)
                .unwrap()
                .id,
            100
        );
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            0
        );
    }

    fn pack_id(player: &PlayerRef) -> Option<u64> {
        Inv::of(player)
            .equipped(equip_slot::BACKPACK)
            .map(|item| item.id)
    }

    #[tokio::test]
    async fn a_claim_outside_the_authored_open_window_is_refused() {
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        // A one-hour window three days from the real weekday is closed now; the claim gate
        // uses the real wall clock (the GM clock is calendar-only).
        let closed_weekday = (Utc::now().weekday().num_days_from_sunday() + 3) % 7;
        {
            let zone = data_mut(&mut f)
                .systems
                .housing
                .castles
                .siege_zones
                .get_mut(&ZONE)
                .unwrap();
            zone.open_weekday = closed_weekday as u8;
            zone.open_hour = 0;
            zone.open_duration_hours = 1;
        }
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_NOT_DECLARE_TIME]
        );
        assert_eq!(pack_id(&f.leader), Some(100));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            0
        );
        assert_eq!(f.house.data().current_step, 0);
    }

    #[tokio::test]
    async fn the_gm_calendar_clock_does_not_move_the_claim_window() {
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        let now = Utc::now();
        let target = (now + chrono::Duration::days(3)).date_naive();
        let closed_weekday = target.weekday().num_days_from_sunday();
        {
            let zone = data_mut(&mut f)
                .systems
                .housing
                .castles
                .siege_zones
                .get_mut(&ZONE)
                .unwrap();
            zone.open_weekday = closed_weekday as u8;
            zone.open_hour = 0;
            zone.open_duration_hours = 1;
        }
        // The GM calendar clock is inside that window, but the claim uses the real clock.
        set_clock(
            &f,
            target.and_hms_opt(0, 30, 0).unwrap().and_utc().timestamp(),
        );
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_NOT_DECLARE_TIME]
        );
        assert_eq!(pack_id(&f.leader), Some(100));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            0
        );
    }

    #[tokio::test]
    async fn a_claim_in_a_zone_whose_window_never_opens_is_refused_as_closed() {
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        data_mut(&mut f)
            .systems
            .housing
            .castles
            .siege_zones
            .get_mut(&ZONE)
            .unwrap()
            .open_duration_hours = 0;
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_CLOSED]
        );
        assert_eq!(pack_id(&f.leader), Some(100));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            0
        );
    }

    #[tokio::test]
    async fn a_pirate_expedition_cannot_claim_a_dominion() {
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        f.server
            .expeditions
            .with_mut(EXPEDITION, |e| {
                e.faction.mother_id = crate::data::world_data::factions::PIRATE;
            })
            .unwrap();
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_NOT_AVAILABLE_FOR_PIRATES]
        );
        assert_eq!(pack_id(&f.leader), Some(100));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            0
        );
    }

    #[tokio::test]
    async fn a_pirate_leader_cannot_claim_a_dominion() {
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        f.leader.data().unit.faction_id = crate::data::world_data::factions::PIRATE;
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_NOT_AVAILABLE_FOR_PIRATES]
        );
        assert_eq!(pack_id(&f.leader), Some(100));
    }

    #[tokio::test]
    async fn a_temporary_duel_team_faction_does_not_make_the_leader_a_pirate() {
        let f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        {
            let mut c = f.leader.data();
            c.unit.faction_id = crate::data::world_data::factions::PIRATE;
            c.unit.origin_faction_id = 101;
            c.unit.faction_temporary = true;
        }
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            EXPEDITION
        );
        assert_eq!(pack_id(&f.leader), None);
    }

    #[tokio::test]
    async fn a_temporary_team_on_a_saved_pirate_leader_is_still_refused() {
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        {
            let mut c = f.leader.data();
            c.unit.faction_id = 101;
            c.unit.origin_faction_id = crate::data::world_data::factions::PIRATE;
            c.unit.faction_temporary = true;
        }
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_NOT_AVAILABLE_FOR_PIRATES]
        );
        assert_eq!(pack_id(&f.leader), Some(100));
    }

    #[tokio::test]
    async fn auroria_claims_follow_the_profile_not_the_siege_calendar() {
        use crate::config::ServerProfile;
        let object = SkillObject::None;

        // The launch era has no Auroria claims: even an explicitly enabled calendar (the GM
        // schedule switch) does not enable them.
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        f.server.config.update(|c| {
            c.world.profile = ServerProfile::EuLaunch;
            c.housing.siege_calendar = Some(true);
        });
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_CLOSED]
        );
        assert_eq!(pack_id(&f.leader), Some(100));
        assert!(f
            .server
            .systems
            .housing
            .dominions
            .get(ZONE)
            .is_none_or(|state| state.owner_expedition_id == 0));

        // Auroria with the weekly calendar off (GM-scheduled sieges only) still claims.
        let f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        f.server.config.update(|c| {
            c.world.profile = ServerProfile::EuAuroria;
            c.housing.siege_calendar = Some(false);
        });
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            EXPEDITION
        );
        assert_eq!(pack_id(&f.leader), None);
    }

    #[tokio::test]
    async fn ephemeral_seeded_dominions_do_not_count_toward_the_claim_limit() {
        let mut f = fixture();
        f.server
            .config
            .update(|c| c.housing.dominion_claim_limit = 1);
        let (second, _) = second_tower(&mut f);
        let (_, identity) = tower_identity(&f.server, &second).unwrap();
        f.server
            .systems
            .housing
            .dominions
            .declare_unowned_with(ZONE + 1, identity, EXPEDITION, 90, || true)
            .unwrap();
        f.server.systems.housing.dominions.mark_ephemeral(ZONE + 1);
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        let object = SkillObject::None;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            EXPEDITION
        );
        assert_eq!(pack_id(&f.leader), None);
    }

    #[tokio::test]
    async fn a_claim_over_the_owned_dominion_limit_is_refused() {
        let mut f = fixture();
        f.server
            .config
            .update(|c| c.housing.dominion_claim_limit = 1);
        let object = SkillObject::None;
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .owner_expedition_id,
            EXPEDITION
        );
        f.client.drain();
        let (second, skill) = second_tower(&mut f);
        f.leader.data().unit.transform.set_xyz(95.0, 32.0, 100.0);
        give_pack(&f, 101, PURIFYING_ARCHEUM);
        declare_effect(&f.server, &args_for(&f, &object, &second, &skill));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::TOO_MANY_OWNED_DOMINIONS]
        );
        assert_eq!(pack_id(&f.leader), Some(101));
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE + 1)
                .unwrap()
                .owner_expedition_id,
            0
        );
        assert_eq!(second.data().current_step, 0);
    }

    #[tokio::test]
    async fn claim_gate_errors_follow_the_documented_order() {
        let object = SkillObject::None;
        // A closed zone wins over the pirate and limit refusals.
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        f.server
            .config
            .update(|c| c.housing.dominion_claim_limit = 0);
        data_mut(&mut f)
            .systems
            .housing
            .castles
            .siege_zones
            .get_mut(&ZONE)
            .unwrap()
            .open_duration_hours = 0;
        f.leader.data().unit.faction_id = crate::data::world_data::factions::PIRATE;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_CLOSED]
        );

        // An open window still refuses a pirate before the owned-territory limit.
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        f.server
            .config
            .update(|c| c.housing.dominion_claim_limit = 0);
        f.leader.data().unit.faction_id = crate::data::world_data::factions::PIRATE;
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::DOMINION_NOT_AVAILABLE_FOR_PIRATES]
        );

        // A non-pirate in an open window reaches the limit refusal.
        let mut f = fixture();
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        f.server
            .config
            .update(|c| c.housing.dominion_claim_limit = 0);
        declare_effect(&f.server, &args(&f, &object));
        assert_eq!(
            errors(&mut f.client),
            vec![crate::systems::economy::error_message::TOO_MANY_OWNED_DOMINIONS]
        );
    }

    #[tokio::test]
    async fn a_fresh_claim_has_no_siege_cycle_during_the_authored_delay() {
        let f = fixture();
        f.server
            .config
            .update(|c| c.housing.siege_calendar = Some(true));
        let object = SkillObject::None;
        give_pack(&f, 100, PURIFYING_ARCHEUM);
        declare_effect(&f.server, &args(&f, &object));
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        let claimed = state.claimed_at;
        assert!(claimed > 0);

        // One hour after the claim the weekly cycle would be running for an older owner.
        set_clock(&f, claimed + 3_600);
        let protected = snapshot(&f.server, &state, claimed + 3_600).unwrap();
        let protected_started = protected.siege_timers.started.timestamp();
        assert_eq!(protected.siege_timers.siege_period, 5);
        assert!(protected_started >= claimed + 30 * 86_400);
        assert!(protected_started > claimed + 3_600);

        // The protected view names the first cycle after the delay; that cycle starts.
        set_clock(&f, protected_started);
        let started = snapshot(&f.server, &state, protected_started).unwrap();
        assert_eq!(started.siege_timers.siege_period, 1);
        assert_eq!(started.siege_timers.started.timestamp(), protected_started);
    }

    #[tokio::test]
    async fn a_captured_territory_does_not_get_the_fresh_claim_protection() {
        let f = fixture();
        f.server
            .config
            .update(|c| c.housing.siege_calendar = Some(true));
        let auction = Utc
            .with_ymd_and_hms(2014, 1, 8, 22, 30, 0)
            .single()
            .unwrap()
            .timestamp();
        let (_, identity) = tower_identity(&f.server, &f.house).unwrap();
        let mut captured = f
            .server
            .systems
            .housing
            .dominions
            .declare_unowned_with(ZONE, identity, EXPEDITION, auction, || true)
            .unwrap();
        // A siege capture changes owner and reign time but not claimed_at.
        captured.owner_expedition_id = EXPEDITION + 1;
        captured.ownership_generation += 1;
        captured.reign_start_at = auction;
        captured.claimed_at = 0;
        f.server
            .systems
            .housing
            .dominions
            .restore(vec![captured.clone()])
            .unwrap();
        set_clock(&f, auction + 3_600);
        let view = snapshot(&f.server, &captured, auction + 3_600).unwrap();
        assert_eq!(view.siege_timers.siege_period, 1);
    }

    #[tokio::test]
    async fn tax_packet_requires_current_owner_leader_and_native_range() {
        let mut f = fixture();
        owned(&f);
        f.server.names.add_character(1, "Lord", 1001);
        f.server.names.add_character(2, "Member", 1002);
        // An ordinary property owned by the member, so the tax-rate notice has a resident.
        let mut ordinary = House::new(14, DESIGN);
        ordinary.current_step = -1;
        ordinary.unit.owner_id = 2;
        ordinary.unit.tl_id = 13;
        ordinary.unit.hp = 1000;
        ordinary.unit.max_hp = 1000;
        ordinary.unit.transform.set_xyz(106.0, 100.0, 100.0);
        ordinary.unit.transform.zone_id = TEST_ZONE;
        ordinary.place_date = Utc::now().timestamp() - 86_400;
        ordinary.protection_end_date = Utc::now().timestamp() + 30 * 86_400;
        let ordinary = Entity::new(0x201, ordinary);
        assert!(f.server.systems.housing.manager.insert(&ordinary));
        let now = Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, 88, now + 300, now + 900, now)
            .unwrap();
        f.client.conn.active_char = Some(f.member.clone());
        CSUpdateDominionTaxRatePacket {
            id: ZONE as u16,
            tax_rate: 25,
        }
        .handle(&f.server, &mut f.client.conn)
        .await
        .unwrap();
        f.client.conn.active_char = Some(f.leader.clone());
        for rate in [-1, 51, i32::MAX] {
            CSUpdateDominionTaxRatePacket {
                id: ZONE as u16,
                tax_rate: rate,
            }
            .handle(&f.server, &mut f.client.conn)
            .await
            .unwrap();
        }
        assert_eq!(
            f.server
                .systems
                .housing
                .dominions
                .get(ZONE)
                .unwrap()
                .tax_rate,
            0
        );
        assert!(f.client.drain().is_empty());
        CSUpdateDominionTaxRatePacket {
            id: ZONE as u16,
            tax_rate: 50,
        }
        .handle(&f.server, &mut f.client.conn)
        .await
        .unwrap();
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        assert_eq!(state.tax_rate, 50);
        assert!(state.last_tax_rate_changed_at > 0);
        let packets = f.client.drain();
        assert_eq!(
            packets.iter().map(|p| p.0).collect::<Vec<_>>(),
            vec![sc::SCDominionTaxRatePacket, sc::SCDominionDataPacket]
        );
        assert_eq!(packets[0].1, vec![25, 0, 50, 0, 0, 0]);
        let notice = f
            .server
            .systems
            .economy
            .mail
            .state()
            .all
            .iter()
            .find(|(_, m)| {
                m.header.receiver_id == 2
                    && m.mail_type == crate::systems::economy::mail::mail_type::TAXRATE_CHANGED
            })
            .map(|(_, m)| m.clone())
            .expect("the property owner gets the tax-rate notice");
        assert_eq!(notice.title, "title(25)");
        assert_eq!(notice.body.text, "body('0','50')");
        assert_eq!(
            notice.header.sender_name,
            crate::systems::economy::mail_system::TAX_RATE_CHANGED_SENDER
        );
        CSUpdateDominionTaxRatePacket {
            id: ZONE as u16,
            tax_rate: 25,
        }
        .handle(&f.server, &mut f.client.conn)
        .await
        .unwrap();
        assert_eq!(f.server.systems.housing.dominions.get(ZONE).unwrap(), state);
        assert_eq!(
            f.client.drain(),
            vec![(
                sc::SCErrorMsgPacket,
                crate::systems::social::packets::body(
                    &crate::packets::world::SCErrorMsgPacket::new(
                        crate::systems::economy::error_message::TAXRATE_CHANGE_LATER,
                        0,
                        true,
                    )
                ),
            )]
        );
        CSUpdateDominionTaxRatePacket {
            id: ZONE as u16,
            tax_rate: 50,
        }
        .handle(&f.server, &mut f.client.conn)
        .await
        .unwrap();
        assert_eq!(f.server.systems.housing.dominions.get(ZONE).unwrap(), state);
        let repeated = f.client.drain();
        assert_eq!(
            repeated.iter().map(|p| p.0).collect::<Vec<_>>(),
            vec![sc::SCDominionTaxRatePacket, sc::SCDominionDataPacket]
        );
        f.leader.data().unit.expedition_id = 0;
        CSUpdateDominionTaxRatePacket {
            id: ZONE as u16,
            tax_rate: 0,
        }
        .handle(&f.server, &mut f.client.conn)
        .await
        .unwrap();
        assert_eq!(f.server.systems.housing.dominions.get(ZONE).unwrap(), state);
    }

    #[tokio::test]
    async fn participant_notifications_diff_rosters_remove_before_add_and_skip_repeated_rpc() {
        let mut f = fixture();
        owned(&f);
        let now = Utc::now().timestamp();
        let old = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, 88, now + 300, now + 900, now)
            .unwrap();
        let next = f
            .server
            .systems
            .housing
            .dominions
            .register_participant(ZONE, 1, 77, SiegeSide::Defender, now)
            .unwrap();
        broadcast_participant_changes(&f.server, &old, &next);
        assert_eq!(
            f.client.drain(),
            vec![(
                sc::SCSiegeMemberPacket,
                vec![25, 0, 77, 0, 0, 0, 1, 0, 0, 0, 1]
            )]
        );
        let repeated = f
            .server
            .systems
            .housing
            .dominions
            .register_participant(ZONE, 1, 77, SiegeSide::Defender, now)
            .unwrap();
        broadcast_participant_changes(&f.server, &next, &repeated);
        assert!(f.client.drain().is_empty());
        broadcast_snapshot(&f.server, &repeated, false);
        let packets = f.client.drain();
        assert_eq!(
            packets.iter().map(|p| p.0).collect::<Vec<_>>(),
            vec![sc::SCDominionDataPacket]
        );
        assert_eq!(&packets[0].1[packets[0].1.len() - 2..], &[0, 1]);
        let changed = f
            .server
            .systems
            .housing
            .dominions
            .register_participant(ZONE, 1, 88, SiegeSide::Attacker, now)
            .unwrap();
        broadcast_participant_changes(&f.server, &repeated, &changed);
        assert_eq!(
            f.client.drain(),
            vec![
                (
                    sc::SCSiegeMemberPacket,
                    vec![25, 0, 77, 0, 0, 0, 1, 0, 0, 0, 0]
                ),
                (
                    sc::SCSiegeMemberPacket,
                    vec![25, 0, 88, 0, 0, 0, 1, 0, 0, 0, 1]
                ),
            ]
        );
        assert!(f
            .server
            .systems
            .housing
            .dominions
            .unregister_participant(ZONE, 1)
            .unwrap());
        let removed = f.server.systems.housing.dominions.get(ZONE).unwrap();
        broadcast_participant_changes(&f.server, &changed, &removed);
        assert_eq!(
            f.client.drain(),
            vec![(
                sc::SCSiegeMemberPacket,
                vec![25, 0, 88, 0, 0, 0, 1, 0, 0, 0, 0]
            )]
        );
        broadcast_siege_declared(&f.server, &removed);
        let packets = f.client.drain();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, sc::SCSiegeDeclaredPacket);
        assert_eq!(&packets[0].1[..6], &[25, 0, 88, 0, 0, 0]);
        assert_eq!(&packets[0].1[29..33], &[100, 100, 100, 0]);
    }

    #[tokio::test]
    async fn tower_alerts_target_involved_guild_or_local_territory_in_same_instance() {
        let mut f = fixture();
        let state = owned(&f);
        f.leader.data().is_online = true;
        f.leader.data().unit.transform.zone_id = TEST_ZONE + 1; // Owning guild still receives it.
        f.member.data().is_online = true;
        f.member.data().unit.transform.instance_id = 99; // Owning guild in another instance does not.
        let mut clients = Vec::new();
        let mut players = Vec::new();
        for id in 3..=5 {
            let mut client = TestClient::new(&f.server, id, 1000 + id);
            let player = spawn_player(
                &f.server,
                &client,
                test_character(id, &format!("Observer{id}"), 120.0 + id as f32, 100.0),
                0x100 + id,
            );
            player.data().is_online = id != 5;
            client.conn.active_char = Some(player.clone());
            clients.push(client);
            players.push(player);
        }
        players[1].data().unit.transform.zone_id = TEST_ZONE + 1;
        f.client.drain();
        f.member_client.drain();
        for client in &mut clients {
            client.drain();
        }
        send_tower_alert(
            &f.server,
            &state,
            tower_alert::FIRST_ATTACKED,
            Some(&f.leader),
        );
        let expected = (
            sc::SCSiegeAlertPacket,
            vec![25, 0, 0, 1, 0, 0, 0, 4, 0, b'L', b'o', b'r', b'd'],
        );
        assert_eq!(f.client.drain(), vec![expected.clone()]);
        assert_eq!(clients[0].drain(), vec![expected]);
        assert!(f.member_client.drain().is_empty());
        assert!(clients[1].drain().is_empty());
        assert!(clients[2].drain().is_empty());
        send_tower_alert(&f.server, &state, 7, Some(&f.leader));
        assert!(f.client.drain().is_empty());
        send_tower_alert(&f.server, &state, tower_alert::ENGRAVABLE, None);
        assert_eq!(
            f.client.drain(),
            vec![(sc::SCSiegeAlertPacket, vec![25, 0, 3, 0, 0, 0, 0])]
        );
    }

    #[tokio::test]
    async fn scheduled_phase_notice_precedes_snapshot_without_a_reign_field() {
        let mut f = fixture();
        owned(&f);
        let now = Utc::now().timestamp();
        let state = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, 88, now + 300, now + 900, now)
            .unwrap();
        broadcast_phase_change(&f.server, &state);
        let packets = f.client.drain();
        assert_eq!(
            packets.iter().map(|p| p.0).collect::<Vec<_>>(),
            vec![sc::SCSiegeStatePacket, sc::SCDominionDataPacket]
        );
        assert_eq!(packets[0].1.len(), 127);
        assert_eq!(packets[0].1[60], 3);
        assert_eq!(&packets[0].1[123..127], &[0, 0, 0, 0]);
        assert_eq!(&packets[1].1[packets[1].1.len() - 2..], &[0, 1]);
    }

    #[tokio::test]
    async fn owner_delta_precedes_snapshot_that_restores_authoritative_national_tax() {
        let mut f = fixture();
        let state = owned(&f);
        broadcast_owner_change(&f.server, &state);
        let packets = f.client.drain();
        assert_eq!(
            packets.iter().map(|p| p.0).collect::<Vec<_>>(),
            vec![
                sc::SCSiegeStatePacket,
                sc::SCDominionOwnerChangedPacket,
                sc::SCDominionDataPacket
            ]
        );
        let mut expected = PacketStream::new();
        expected
            .write_u16(25)
            .write_u32(77)
            .write_u64(90)
            .write_bool(false);
        assert_eq!(packets[1].1, expected.into_bytes());
        let data = snapshot(&f.server, &state, 100).unwrap();
        assert_eq!(data.national_tax_rate, 0);
        let mut expected = PacketStream::new();
        expected.write(&data).write_bool(false).write_bool(true);
        assert_eq!(packets[2].1, expected.into_bytes());
    }

    fn kst(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        use chrono::TimeZone as _;
        chrono::FixedOffset::east_opt(9 * 3600)
            .unwrap()
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
            .timestamp()
    }

    fn enable_calendar(f: &Fixture) {
        f.server.config.update(|config| {
            config.housing.siege_calendar = Some(true);
            config.housing.siege_calendar_timezone = "+09:00".into();
        });
    }

    /// The exact `SCSiegeState` body for the calendar snapshot of this fixture.
    fn expected_state_bytes(period: u8, started: i64, durations: [i32; 5]) -> Vec<u8> {
        let mut expected = PacketStream::new();
        expected
            .write_u16(ZONE as u16)
            .write_u32(SETTING)
            .write_u32(4771)
            .write_u8(2)
            .write_u8(100)
            .write_i16(450)
            .write_u16(200)
            .write_i16(120)
            .write_i16(450);
        for duration in durations {
            expected.write_i32(duration);
        }
        expected
            .write_i64(started)
            .write_i64(0)
            // `siege_zones.reinforce_defense_delay_mins` = 20 in the fixture (WP-S4).
            .write_i32(1_200)
            .write_u8(period);
        expected
            .write_u32(EXPEDITION)
            .write_bc(0)
            .write_i64(0)
            .write_i64(0)
            .write_f32(0.0)
            .write_raw(&[0, 0, 0, 0]);
        expected
            .write_u32(0)
            .write_bc(0)
            .write_i64(0)
            .write_i64(0)
            .write_f32(0.0)
            .write_raw(&[0, 0, 0, 0])
            .write_u32(0);
        expected.into_bytes()
    }

    #[tokio::test]
    async fn calendar_snapshot_has_real_timers_and_exact_bytes_in_auction_declare_peace() {
        let f = fixture();
        enable_calendar(&f);
        let state = owned(&f);
        let calendar = &f.server.systems.housing.calendar;
        let auction = kst(2014, 1, 8, 22, 30);
        let declare = kst(2014, 1, 10, 22, 30);
        let warmup = kst(2014, 1, 12, 20, 30);
        let siege_end = kst(2014, 1, 12, 22, 30);
        let next_auction = kst(2014, 1, 29, 22, 30);
        let durations = [
            48 * 3600,
            46 * 3600,
            30 * 60,
            90 * 60,
            i32::try_from(next_auction - siege_end).unwrap(),
        ];
        assert_eq!(
            durations.iter().map(|d| i64::from(*d)).sum::<i64>(),
            21 * 86_400
        );
        for (now, period, started) in [
            (auction + 60, 1, auction),
            (declare + 60, 2, auction),
            // No declaration: the declare period ends in peace with the next cycle's timers.
            (warmup, 5, next_auction),
            (siege_end + 60, 5, next_auction),
        ] {
            // Only the calendar branch reads the GM test clock; the explicit branch keeps the
            // real clock passed by the caller.
            calendar.set_clock(Some(now));
            let data = snapshot(&f.server, &state, Utc::now().timestamp()).unwrap();
            let timers = &data.siege_timers;
            assert_eq!(timers.siege_period, period, "period at {now}");
            assert_eq!(timers.started.timestamp(), started);
            assert_eq!(timers.durations, durations);
            assert_eq!(timers.unk_data.id, EXPEDITION);
            assert_eq!(timers.unk2_data.id, 0);
            let packet = crate::systems::social::packets::misc::SCSiegeStatePacket {
                id: data.zone_id,
                territory_data: data.territory_data,
                siege_timers: timers.clone(),
                new_owner: 0,
                reign_start_time: data.reign_start_time,
            };
            let bytes = crate::systems::social::packets::body(&packet);
            assert_eq!(bytes, expected_state_bytes(period, started, durations));
        }
    }

    #[tokio::test]
    async fn calendar_tick_announces_declare_to_peace_once_and_skips_warmup_and_siege() {
        let mut f = fixture();
        enable_calendar(&f);
        owned(&f);
        f.client.drain();
        let declare = kst(2014, 1, 10, 22, 30);
        let warmup = kst(2014, 1, 12, 20, 30);
        let next_auction = kst(2014, 1, 29, 22, 30);
        let calendar = &f.server.systems.housing.calendar;

        // The first observation is silent; repeating it must not emit anything either.
        calendar.set_clock(Some(declare + 60));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert_eq!(calendar.last_period(ZONE), Some(2));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(f.client.drain().is_empty());

        // Declare ends with no attacker: one SCSiegeState (period 5) and one snapshot.
        calendar.set_clock(Some(warmup));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        let packets = f.client.drain();
        assert_eq!(
            packets.iter().map(|packet| packet.0).collect::<Vec<_>>(),
            vec![sc::SCSiegeStatePacket, sc::SCDominionDataPacket]
        );
        let state_bytes = &packets[0].1;
        assert_eq!(state_bytes[60], 5);
        assert_eq!(
            i64::from_le_bytes(state_bytes[40..48].try_into().unwrap()),
            next_auction
        );
        assert_eq!(calendar.last_period(ZONE), Some(5));

        calendar.set_clock(Some(warmup + 1));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(f.client.drain().is_empty());
    }

    #[tokio::test]
    async fn calendar_snapshot_carries_a_matching_declaration_and_zero_team_after_cancel() {
        let mut f = fixture();
        enable_calendar(&f);
        owned(&f);
        let auction = kst(2014, 1, 8, 22, 30);
        let declare = kst(2014, 1, 10, 22, 30);
        let state = f
            .server
            .systems
            .housing
            .dominions
            .declare(ZONE, 88, auction, declare + 60, 0x4321, (10.0, 20.0, 30.0))
            .unwrap();
        f.server
            .systems
            .housing
            .calendar
            .set_clock(Some(declare + 60));
        let data = snapshot(&f.server, &state, Utc::now().timestamp()).unwrap();
        assert_eq!(data.siege_timers.siege_period, 2);
        let offense = &data.siege_timers.unk2_data;
        assert_eq!(
            (offense.id, offense.obj_id),
            (88, 0x4321),
            "the declaration is the offense team"
        );
        assert_eq!((offense.x, offense.y, offense.z), (10.0, 20.0, 30.0));
        assert_eq!((offense.ni, offense.nr, offense.limit), (100, 100, 100));

        broadcast_siege_declared(&f.server, &state);
        let packets = f.client.drain();
        let mut expected = PacketStream::new();
        expected.write_u16(ZONE as u16);
        expected.write(&DominionUnkData {
            id: 88,
            obj_id: 0x4321,
            x: 10.0,
            y: 20.0,
            z: 30.0,
            ni: 100,
            nr: 100,
            limit: 100,
            unk_ids: Vec::new(),
        });
        assert_eq!(packets[0].0, sc::SCSiegeDeclaredPacket);
        assert_eq!(packets[0].1, expected.into_bytes());

        // Cancelling announces the zero team the client reads as "nobody declared".
        let cancelled = f
            .server
            .systems
            .housing
            .dominions
            .cancel_declaration(ZONE, 88)
            .unwrap();
        assert!(cancelled.declaration.is_none());
        broadcast_siege_declared(&f.server, &cancelled);
        let packets = f.client.drain();
        assert_eq!(packets[0].0, sc::SCSiegeDeclaredPacket);
        assert_eq!(
            &packets[0].1[..7],
            &[ZONE as u8, 0, 0, 0, 0, 0, 0],
            "a canceled declaration has a zero offense team"
        );
    }

    #[tokio::test]
    async fn declaration_materialises_at_warmup_and_the_calendar_walks_periods_three_to_five() {
        let mut f = fixture();
        enable_calendar(&f);
        owned(&f);
        let auction = kst(2014, 1, 8, 22, 30);
        let declare = kst(2014, 1, 10, 22, 30);
        let warmup = kst(2014, 1, 12, 20, 30);
        let siege = kst(2014, 1, 12, 21, 0);
        let siege_end = kst(2014, 1, 12, 22, 30);
        f.server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: 88,
                ..Default::default()
            },
            members: Vec::new(),
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
        f.server
            .systems
            .housing
            .dominions
            .declare(ZONE, 88, auction, declare + 60, 0x4321, (10.0, 20.0, 30.0))
            .unwrap();
        let calendar = &f.server.systems.housing.calendar;
        calendar.set_clock(Some(declare + 60));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        f.client.drain();

        calendar.set_clock(Some(warmup));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        let materialised = f.server.systems.housing.dominions.get(ZONE).unwrap();
        let record = materialised.siege.as_ref().expect("siege record");
        assert_eq!((record.starts_at, record.ends_at), (siege, siege_end));
        assert!(record.materialised_from_declaration);
        assert_eq!(record.calendar_cycle, auction);
        assert_eq!(
            (record.attacker_expedition_id, record.defender_expedition_id),
            (88, EXPEDITION)
        );
        assert!(materialised.declaration.is_none());
        let packets = f.client.drain();
        assert!(packets.iter().any(|p| p.0 == sc::SCSiegeDeclaredPacket));
        let states: Vec<_> = packets
            .iter()
            .filter(|p| p.0 == sc::SCSiegeStatePacket)
            .collect();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1[60], 3);

        for (now, period) in [(siege, 4), (siege_end, 5)] {
            calendar.set_clock(Some(now));
            crate::systems::housing::dominion::runtime::tick_once(&f.server);
            let packets = f.client.drain();
            let states: Vec<_> = packets
                .iter()
                .filter(|p| p.0 == sc::SCSiegeStatePacket)
                .collect();
            assert_eq!(states.len(), 1, "one SCSiegeState at period {period}");
            assert_eq!(states[0].1[60], period);
        }
    }

    #[tokio::test]
    async fn camp_use_records_a_declaration_consumes_the_item_and_announces_the_team() {
        let mut f = camp_fixture();
        f.member_client.drain();
        let item = give_camp_item(&f, &f.member, CAMP_ITEM);
        assert_eq!(item_count(&f, &f.member, CAMP_ITEM), 1, "item given");
        cast_camp(&f.server, &mut f.member_client, &f.member, item, CAMP_ITEM);
        let packets_debug = f.member_client.drain();
        let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
        let declaration = state.declaration.unwrap_or_else(|| {
            panic!(
                "declaration accepted; packets={packets_debug:?} items={}",
                item_count(&f, &f.member, CAMP_ITEM)
            )
        });
        assert_eq!(declaration.attacker_expedition_id, CAMP_ATTACKER);
        assert_eq!(item_count(&f, &f.member, CAMP_ITEM), 0);
        assert_ne!(declaration.hq_obj_id, 0, "the spawned camp is bound");
        let mut packets = f.member_client.drain();
        packets.extend(f.client.drain());
        let declared = packets
            .iter()
            .find(|packet| packet.0 == sc::SCSiegeDeclaredPacket)
            .expect("SCSiegeDeclared");
        assert_eq!(&declared.1[2..6], &CAMP_ATTACKER.to_le_bytes());
        assert!(packets
            .iter()
            .any(|packet| packet.0 == sc::SCSiegeStatePacket));

        // Cancel (the shipped GM route) announces the zero offense team.
        f.client.drain();
        f.member_client.drain();
        crate::systems::housing::dominion::declare::cancel(&f.server, ZONE).unwrap();
        let mut packets = f.client.drain();
        packets.extend(f.member_client.drain());
        let declared = packets
            .iter()
            .find(|packet| packet.0 == sc::SCSiegeDeclaredPacket)
            .expect("cancel announced");
        assert_eq!(&declared.1[2..6], &0u32.to_le_bytes());
    }

    #[tokio::test]
    async fn camp_use_refusals_use_the_client_ids_and_keep_the_item() {
        /// Casts once with the prepared fixture and asserts the refusal and the kept item.
        fn expect_refusal(f: &mut Fixture, template: u32, expected: i16) {
            f.member_client.drain();
            let item = give_camp_item(f, &f.member, template);
            cast_camp(&f.server, &mut f.member_client, &f.member, item, template);
            assert!(
                errors(&mut f.member_client).contains(&expected),
                "expected error {expected}"
            );
            assert_eq!(
                item_count(f, &f.member, template),
                1,
                "refusal keeps the item"
            );
        }

        // 342: no dominion in the zone the caster stands in.
        let mut f = camp_fixture();
        f.member.data().unit.transform.zone_id = TEST_ZONE + 1;
        expect_refusal(&mut f, CAMP_ITEM, 342);

        // 343: own zone (the caster is in the owning expedition).
        let mut f = camp_fixture();
        f.client.drain();
        let item = give_camp_item(&f, &f.leader, CAMP_ITEM);
        cast_camp(&f.server, &mut f.client, &f.leader, item, CAMP_ITEM);
        assert!(errors(&mut f.client).contains(&343));
        assert_eq!(item_count(&f, &f.leader, CAMP_ITEM), 1);

        // 344: the camp item belongs to another zone.
        let f = camp_fixture();
        let mut f = f;
        expect_refusal(&mut f, CAMP_ITEM_OTHER, 344);

        // 386: not the declaration period.
        let mut f = camp_fixture();
        f.server
            .systems
            .housing
            .calendar
            .set_clock(Some(kst(2014, 1, 8, 23, 0)));
        expect_refusal(&mut f, CAMP_ITEM, 386);

        // 345: inside the authored radius_declare of the lodestone.
        let mut f = camp_fixture();
        f.member.data().unit.transform.set_xyz(105.0, 100.0, 100.0);
        expect_refusal(&mut f, CAMP_ITEM, 345);

        // 459: the caster's rank lacks the SiegeMaster policy (459 vs 581 is UNKNOWN).
        let mut f = camp_fixture();
        f.server.expeditions.with_mut(CAMP_ATTACKER, |guild| {
            guild.policies[0].siege_master = false
        });
        expect_refusal(&mut f, CAMP_ITEM, 459);

        // 444: pirate expedition.
        let mut f = camp_fixture();
        f.server.expeditions.with_mut(CAMP_ATTACKER, |guild| {
            guild.faction.mother_id = crate::data::world_data::factions::PIRATE;
        });
        expect_refusal(&mut f, CAMP_ITEM, 444);

        // 639: the expedition already has an unfinished siege.
        let mut f = camp_fixture();
        let now = Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, CAMP_ATTACKER, now + 300, now + 900, now)
            .unwrap();
        expect_refusal(&mut f, CAMP_ITEM, 639);

        // 442: the same expedition already declared this cycle.
        let mut f = camp_fixture();
        let first = give_camp_item(&f, &f.member, CAMP_ITEM);
        cast_camp(&f.server, &mut f.member_client, &f.member, first, CAMP_ITEM);
        f.member_client.drain();
        expect_refusal(&mut f, CAMP_ITEM, 442);

        // 443: another expedition already declared this cycle.
        let mut f = camp_fixture();
        let first = give_camp_item(&f, &f.member, CAMP_ITEM);
        cast_camp(&f.server, &mut f.member_client, &f.member, first, CAMP_ITEM);
        f.client.drain();
        f.server.expeditions.with_mut(EXPEDITION, |guild| {
            guild.members.retain(|m| m.character_id != 1)
        });
        f.server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: 99,
                owner_id: f.leader.character_id,
                name: "Other".into(),
                ..Default::default()
            },
            members: vec![expedition::member_from_character(
                &f.server, 99, &f.leader, true,
            )],
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
        {
            use crate::systems::social::expedition::ExpeditionRolePolicy;
            f.server.expeditions.with_mut(99, |guild| {
                guild.policies.push(ExpeditionRolePolicy {
                    expedition_id: 99,
                    role: 255,
                    name: "master".into(),
                    dominion_declare: true,
                    invite: true,
                    expel: true,
                    promote: true,
                    dismiss: true,
                    chat: true,
                    manager_chat: true,
                    siege_master: true,
                    join_siege: true,
                });
            });
        }
        f.leader.data().unit.expedition_id = 99;
        f.leader.data().unit.transform.set_xyz(600.0, 100.0, 100.0);
        f.client.conn.active_char = Some(f.leader.clone());
        f.client.drain();
        let item = give_camp_item(&f, &f.leader, CAMP_ITEM);
        cast_camp(&f.server, &mut f.client, &f.leader, item, CAMP_ITEM);
        assert!(errors(&mut f.client).contains(&443));
        assert_eq!(item_count(&f, &f.leader, CAMP_ITEM), 1);
    }

    /// Adds a finished house with a `garden_radius` plot at `(x, y)` in instance 0.
    fn add_house_plot(
        f: &mut Fixture,
        template_id: u32,
        category_id: u32,
        x: f32,
        y: f32,
        garden_radius: f32,
    ) -> HouseRef {
        let data = data_mut(f);
        data.systems.housing.add_template(HousingTemplate {
            id: template_id,
            category_id,
            garden_radius,
            ..Default::default()
        });
        let mut h = House::new(500 + template_id, template_id);
        h.current_step = -1;
        h.unit.hp = 1_000;
        h.unit.max_hp = 1_000;
        h.unit.tl_id = (500 + template_id) as u16;
        h.unit.transform.set_xyz(x, y, 100.0);
        h.unit.transform.zone_id = TEST_ZONE;
        let house = Entity::new(f.server.ids.object.get_next_id().unwrap(), h);
        assert!(f.server.systems.housing.manager.insert(&house));
        crate::systems::housing::manager::spawn_house(&f.server, &house);
        house
    }

    /// Adds a `Data/Portal/respawns.json` resurrection point (`PortalTables::respawns`) at
    /// `(x, y)` in the synthetic `main_world`.
    fn add_respawn_at(f: &mut Fixture, id: u32, x: f32, y: f32) {
        let data = data_mut(f);
        let mut respawns = data.systems.npcs.portal.respawns.portals.clone();
        respawns.push(crate::data::world_data::Portal {
            id,
            name: format!("Respawn {id}"),
            x,
            y,
            z: 100.0,
            sub_zone_id: id,
            ..Default::default()
        });
        data.systems.npcs.portal = crate::systems::npcs::portal::PortalTables::from_parts(
            respawns,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
    }

    /// Adds a `DoodadFuncHouseFarm` phase func to the doodad's (growth) func group, the 1.2
    /// data's marker of a crop / livestock farm product.
    fn add_farm_doodad_template(f: &mut Fixture, template_id: u32) {
        let doodads = &mut data_mut(f).systems.npcs.doodad;
        let group_id = 50_000 + template_id;
        let group = crate::systems::npcs::doodad::templates::DoodadFuncGroups {
            id: group_id,
            almighty: template_id,
            group_kind_id: crate::systems::npcs::doodad::templates::doodad_func_group_kind::NORMAL,
            ..Default::default()
        };
        doodads.templates.insert(
            template_id,
            crate::systems::npcs::doodad::templates::DoodadTemplate {
                id: template_id,
                func_groups: vec![group],
                ..Default::default()
            },
        );
        doodads.phase_funcs.insert(
            group_id,
            vec![crate::systems::npcs::doodad::templates::DoodadPhaseFunc {
                group_id,
                func_id: 60_000 + template_id,
                func_type: "DoodadFuncHouseFarm".into(),
            }],
        );
    }

    /// Spawns a doodad of `template_id` at `(x, y)` in instance 0.
    fn add_doodad_at(f: &Fixture, template_id: u32, x: f32, y: f32) -> crate::world::DoodadRef {
        let mut d = crate::models::doodad::Doodad::new(template_id);
        d.transform.set_xyz(x, y, 100.0);
        d.transform.zone_id = TEST_ZONE;
        // The fixture's players use fixed obj ids (0x100/0x101) the id manager does not know,
        // so `get_next_id` can hand out 0x100 again and `add_object` would keep the player.
        let doodad = Entity::new(0xa000 + template_id, d);
        let world = f.server.world.world(0).unwrap();
        world.spawn(&doodad, &f.server.view_ctx());
        doodad
    }

    /// Marks a square sub zone (id `sub_zone_id`) of the synthetic `main_world` as a public
    /// farm area (`PublicFarmManager._farmZones`).
    fn add_public_farm_area(f: &mut Fixture, sub_zone_id: u32, x0: f32, y0: f32, x1: f32, y1: f32) {
        let data = data_mut(f);
        data.zones.zones.entry(TEST_ZONE).or_insert(Zone {
            id: 1,
            zone_key: TEST_ZONE,
            group_id: ZONE,
            ..Default::default()
        });
        let zone_id = data.zones.zones.get(&TEST_ZONE).unwrap().id;
        data.systems
            .npcs
            .subzone
            .sub_zones
            .entry(0)
            .or_default()
            .insert(
                zone_id,
                vec![crate::systems::npcs::subzone::Area {
                    id: sub_zone_id,
                    name: format!("public farm {sub_zone_id}"),
                    points: vec![[x0, y0, 0.0], [x1, y0, 0.0], [x1, y1, 0.0], [x0, y1, 0.0]],
                }],
            );
        data.systems
            .npcs
            .public_farm
            .farm_zones
            .entry(sub_zone_id)
            .or_insert(crate::systems::npcs::public_farm::farm_group_kind::FARM);
    }

    fn set_declare_radius(f: &mut Fixture, radius: u32) {
        data_mut(f)
            .systems
            .housing
            .castles
            .guard_tower_settings
            .get_mut(&SETTING)
            .unwrap()
            .radius_declare = radius;
    }

    /// A living physics collider at `(x, y, z)`, as the house-overlap tests install.
    fn install_obstacle(f: &Fixture, obj_id: u32, x: f32, y: f32, z: f32) {
        use std::collections::HashMap;
        use std::sync::Arc;
        let _ = f
            .server
            .systems
            .ai
            .collision
            .test_files
            .set(Arc::new(HashMap::new()));
        let _ = f.server.systems.ai.collision.test_terrain.set(Arc::new(
            crate::systems::ai::collision::terrain::tests::Plane {
                a: 100.0,
                b: 0.0,
                c: 0.0,
                n: 1024,
            },
        ));
        let physics =
            crate::systems::ai::collision::phys_world(&f.server, 0).expect("physics world");
        let cell = ((x / 1024.0).floor() as i32, (y / 1024.0).floor() as i32);
        physics
            .lock()
            .insert_cell(crate::systems::ai::collision::world::CellContent {
                cell,
                ..Default::default()
            });
        assert!(physics
            .lock()
            .sync_living(
                obj_id,
                &crate::systems::ai::collision::data::LivingParams::default(),
                aaemu_physics::math::Vec3::new(x, y, z),
            )
            .is_some());
    }

    /// Casts the camp once and answers the error ids the client received.
    fn cast_camp_errors(f: &mut Fixture) -> (Vec<i16>, bool) {
        f.member_client.drain();
        let item = give_camp_item(f, &f.member, CAMP_ITEM);
        cast_camp(&f.server, &mut f.member_client, &f.member, item, CAMP_ITEM);
        let codes = errors(&mut f.member_client);
        let kept = item_count(f, &f.member, CAMP_ITEM) == 1;
        (codes, kept)
    }

    fn declared(f: &Fixture) -> bool {
        f.server
            .systems
            .housing
            .dominions
            .get(ZONE)
            .is_some_and(|state| state.declaration.is_some())
    }

    #[tokio::test]
    async fn camp_placement_refuses_a_house_plot_and_honours_the_house_margin() {
        // 633: the camp spot (one metre in front of the caster) is inside a plot.
        let mut f = camp_fixture();
        add_house_plot(&mut f, 71, 1, 600.0, 101.0, 8.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&633), "house plot refusal: {codes:?}");
        assert!(kept, "a refused declaration keeps the camp");
        assert!(!declared(&f));

        // GameMeca's "about 10 m": a plot edge 9 m from the spot refuses.
        let mut f = camp_fixture();
        add_house_plot(&mut f, 71, 1, 600.0, 118.0, 8.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&633), "house plot 9 m away: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));

        // 11 m away is outside the default margin: allowed.
        let mut f = camp_fixture();
        add_house_plot(&mut f, 71, 1, 600.0, 120.0, 8.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "house plot 11 m away: {codes:?}");
        assert!(!kept, "the accepted declaration consumes the camp");
        assert!(declared(&f));

        // Config override: a 40 m margin refuses the same 11 m plot.
        let mut f = camp_fixture();
        f.server
            .config
            .update(|c| c.housing.siege_declare_house_margin = 40.0);
        add_house_plot(&mut f, 71, 1, 600.0, 120.0, 8.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&633), "40 m house margin: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));
    }

    #[tokio::test]
    async fn camp_placement_refuses_farm_plots_farm_doodads_and_public_farm_areas() {
        // A farm plot (category 18 "thatched farm") is a house with a `garden_radius` plot:
        // 9 m from its edge refuses, 11 m is allowed.
        let mut f = camp_fixture();
        add_house_plot(&mut f, 72, 18, 600.0, 118.0, 8.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&633), "farm plot 9 m away: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));

        let mut f = camp_fixture();
        add_house_plot(&mut f, 72, 18, 600.0, 120.0, 8.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "farm plot 11 m away: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        // A planted private farm doodad (`DoodadFuncHouseFarm`) 9 m away refuses.
        const FARM_DOODAD: u32 = 9_001;
        let mut f = camp_fixture();
        add_farm_doodad_template(&mut f, FARM_DOODAD);
        add_doodad_at(&f, FARM_DOODAD, 600.0, 110.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&633), "farm doodad 9 m away: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));

        // The same doodad 11 m away is allowed.
        let mut f = camp_fixture();
        add_farm_doodad_template(&mut f, FARM_DOODAD);
        add_doodad_at(&f, FARM_DOODAD, 600.0, 112.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "farm doodad 11 m away: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        // A public farm area (sub zone 966, a `_farmZones` farm) 9 m away refuses.
        let mut f = camp_fixture();
        add_public_farm_area(&mut f, 966, 590.0, 110.0, 620.0, 125.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&633), "public farm 9 m away: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));

        // A public farm edge 11 m away is allowed.
        let mut f = camp_fixture();
        add_public_farm_area(&mut f, 966, 590.0, 112.0, 620.0, 125.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "public farm 11 m away: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        // Config override: a 15 m farm margin refuses both 11 m cases.
        let mut f = camp_fixture();
        f.server
            .config
            .update(|c| c.housing.siege_declare_farm_margin = 15.0);
        add_farm_doodad_template(&mut f, FARM_DOODAD);
        add_doodad_at(&f, FARM_DOODAD, 600.0, 112.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&633), "15 m farm margin (doodad): {codes:?}");
        assert!(kept);
        assert!(!declared(&f));

        let mut f = camp_fixture();
        f.server
            .config
            .update(|c| c.housing.siege_declare_farm_margin = 15.0);
        add_public_farm_area(&mut f, 966, 590.0, 112.0, 620.0, 125.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(
            codes.contains(&633),
            "15 m farm margin (public farm): {codes:?}"
        );
        assert!(kept);
        assert!(!declared(&f));
    }

    #[tokio::test]
    async fn camp_placement_refuses_another_zone_group_and_honours_the_zone_margin() {
        // 661: region (1,0) is zone key `TEST_ZONE + 1` (group 26), the caster's dominion
        // record is group 25; the authored radius is lowered so 345 cannot answer first.
        let mut f = camp_fixture();
        set_declare_radius(&mut f, 10);
        f.member.data().unit.transform.set_xyz(70.0, 32.0, 100.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&661), "other zone group: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));

        // Default margin 0: a spot in group 25, 2 m from the group-26 border, is allowed.
        let mut f = camp_fixture();
        set_declare_radius(&mut f, 10);
        f.member.data().unit.transform.set_xyz(100.0, 65.0, 100.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "inside the besieged group: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        // Config override: a 5 m margin sees the neighbouring group and refuses.
        let mut f = camp_fixture();
        set_declare_radius(&mut f, 10);
        f.server
            .config
            .update(|c| c.housing.siege_declare_zone_margin = 5.0);
        f.member.data().unit.transform.set_xyz(100.0, 65.0, 100.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&661), "5 m zone margin: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));
    }

    #[tokio::test]
    async fn camp_placement_refuses_resurrection_points_and_honours_the_margin() {
        // GameMeca's "about 100 m": a resurrection point 99 m from the camp spot refuses.
        let mut f = camp_fixture();
        add_respawn_at(&mut f, 86, 600.0, 200.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(
            codes.contains(&625),
            "resurrection point 99 m away: {codes:?}"
        );
        assert!(kept, "a refused declaration keeps the camp");
        assert!(!declared(&f));

        // 101 m away is outside the default margin: allowed.
        let mut f = camp_fixture();
        add_respawn_at(&mut f, 86, 600.0, 202.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "resurrection point 101 m away: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        // Config switch off: the same 99 m point is ignored.
        let mut f = camp_fixture();
        f.server
            .config
            .update(|c| c.housing.siege_declare_resurrect_check = false);
        add_respawn_at(&mut f, 86, 600.0, 200.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "resurrection check off: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        // Config margin override: 50 m allows the 99 m point and refuses a 49 m one.
        let mut f = camp_fixture();
        f.server
            .config
            .update(|c| c.housing.siege_declare_resurrect_margin = 50.0);
        add_respawn_at(&mut f, 86, 600.0, 200.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "50 m resurrect margin, 99 m: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        let mut f = camp_fixture();
        f.server
            .config
            .update(|c| c.housing.siege_declare_resurrect_margin = 50.0);
        add_respawn_at(&mut f, 86, 600.0, 150.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&625), "50 m margin, 49 m away: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));
    }

    #[tokio::test]
    async fn camp_placement_refuses_obstacles_and_honours_the_radius() {
        // Default 0.35 m: an obstacle at the camp spot refuses.
        let mut f = camp_fixture();
        install_obstacle(&f, 0x900, 600.0, 101.0, 100.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&741), "obstacle at the spot: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));

        // The same obstacle 2 m away is outside the default radius: allowed.
        let mut f = camp_fixture();
        install_obstacle(&f, 0x900, 602.0, 101.0, 100.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.is_empty(), "obstacle outside the radius: {codes:?}");
        assert!(!kept);
        assert!(declared(&f));

        // Config override: a 3 m radius refuses the 2 m obstacle.
        let mut f = camp_fixture();
        f.server
            .config
            .update(|c| c.housing.siege_declare_obstacle_radius = 3.0);
        install_obstacle(&f, 0x900, 602.0, 101.0, 100.0);
        let (codes, kept) = cast_camp_errors(&mut f);
        assert!(codes.contains(&741), "3 m obstacle radius: {codes:?}");
        assert!(kept);
        assert!(!declared(&f));
    }

    #[tokio::test]
    async fn siege_items_are_refused_outside_their_authored_situations_and_kept() {
        const MINE: u32 = 26_397;
        const SCROLL: u32 = 34_018;
        let mut f = camp_fixture();
        // Decline period: the mine is not usable, the escape scroll is.
        f.member_client.drain();
        let mine = give_camp_item(&f, &f.member, MINE);
        cast_item(
            &f.server,
            &mut f.member_client,
            &f.member,
            mine,
            MINE,
            20_000,
        );
        assert!(errors(&mut f.member_client).contains(&431));
        assert_eq!(item_count(&f, &f.member, MINE), 1, "refusal keeps the item");
        let scroll = give_camp_item(&f, &f.member, SCROLL);
        cast_item(
            &f.server,
            &mut f.member_client,
            &f.member,
            scroll,
            SCROLL,
            20_000,
        );
        assert!(errors(&mut f.member_client).is_empty());

        // Siege: an explicit schedule owns the real-clock period 4.
        let now = Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, CAMP_ATTACKER, now - 60, now + 600, now - 60)
            .unwrap();
        // Outside the siege circle: the mine is refused, the scroll is allowed.
        let mine = give_camp_item(&f, &f.member, MINE);
        let kept = item_count(&f, &f.member, MINE);
        cast_item(
            &f.server,
            &mut f.member_client,
            &f.member,
            mine,
            MINE,
            20_000,
        );
        assert!(errors(&mut f.member_client).contains(&431));
        assert_eq!(item_count(&f, &f.member, MINE), kept);
        let scroll = give_camp_item(&f, &f.member, SCROLL);
        cast_item(
            &f.server,
            &mut f.member_client,
            &f.member,
            scroll,
            SCROLL,
            20_000,
        );
        assert!(errors(&mut f.member_client).is_empty());

        // Inside the circle: the mine is allowed, the scroll is refused.
        f.member.data().unit.transform.set_xyz(110.0, 100.0, 100.0);
        let mine = give_camp_item(&f, &f.member, MINE);
        cast_item(
            &f.server,
            &mut f.member_client,
            &f.member,
            mine,
            MINE,
            20_000,
        );
        assert!(errors(&mut f.member_client).is_empty());
        let scroll = give_camp_item(&f, &f.member, SCROLL);
        let kept = item_count(&f, &f.member, SCROLL);
        cast_item(
            &f.server,
            &mut f.member_client,
            &f.member,
            scroll,
            SCROLL,
            20_000,
        );
        assert!(errors(&mut f.member_client).contains(&431));
        assert_eq!(item_count(&f, &f.member, SCROLL), kept);
    }

    #[tokio::test]
    async fn calendar_enabled_keeps_an_explicit_gm_schedule_as_the_override() {
        let mut f = fixture();
        enable_calendar(&f);
        owned(&f);
        let real = Utc::now().timestamp();
        let state = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, 88, real + 300, real + 900, real)
            .unwrap();
        let data = snapshot(&f.server, &state, real).unwrap();
        assert_eq!(data.siege_timers.siege_period, 3);
        assert_eq!(data.siege_timers.started.timestamp(), real);
        assert_eq!(data.siege_timers.durations, [0, 0, 300, 600, 0]);
        assert_eq!(data.siege_timers.unk_data.id, EXPEDITION);
        assert_eq!(data.siege_timers.unk2_data.id, 88);
        // The explicit schedule also wins over the calendar for the tick's transition memory.
        let calendar = &f.server.systems.housing.calendar;
        calendar.set_clock(Some(kst(2014, 1, 9, 12, 0)));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert_eq!(calendar.last_period(ZONE), Some(3));

        // Giving the calendar a clock decades ahead must not finish the real siege.
        calendar.set_clock(Some(2_000_000_000));
        let before = f.server.systems.housing.dominions.get(ZONE).unwrap();
        f.client.drain();
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert_eq!(
            f.server.systems.housing.dominions.get(ZONE).unwrap(),
            before
        );
        assert!(f.client.drain().iter().all(
            |packet| packet.0 != sc::SCSiegeStatePacket && packet.0 != sc::SCDominionDataPacket
        ));

        // The explicit end announces itself once; the next calendar tick must not repeat it.
        let changed = f.server.systems.housing.dominions.tick(real + 901).unwrap();
        assert_eq!(changed.len(), 1);
        for state in &changed {
            broadcast_phase_change(&f.server, state);
        }
        let packets = f.client.drain();
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.0 == sc::SCSiegeStatePacket)
                .count(),
            1
        );
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.0 == sc::SCDominionDataPacket)
                .count(),
            1
        );
        assert!(changed[0].siege.as_ref().unwrap().status == SiegeStatus::Finished);
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(!f
            .client
            .drain()
            .iter()
            .any(|packet| packet.0 == sc::SCSiegeStatePacket));
    }

    #[tokio::test]
    async fn future_test_clock_never_touches_an_explicit_siege() {
        let mut f = fixture();
        enable_calendar(&f);
        owned(&f);
        let real = Utc::now().timestamp();
        let before = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, 88, real + 300, real + 900, real)
            .unwrap();
        assert_eq!(
            before.siege.as_ref().unwrap().status,
            SiegeStatus::Scheduled
        );
        f.server
            .systems
            .housing
            .calendar
            .set_clock(Some(2_000_000_000));
        f.client.drain();
        crate::systems::housing::dominion::runtime::tick_once(&f.server);

        assert_eq!(
            f.server.systems.housing.dominions.get(ZONE).unwrap(),
            before
        );
        let batch = f.server.systems.housing.dominions.prepare_save();
        let row = batch
            .rows
            .iter()
            .find(|row| row.state.zone_group_id == ZONE)
            .expect("dirty siege row");
        assert_eq!(row.state, before);
        assert!(!f.client.drain().iter().any(
            |packet| packet.0 == sc::SCSiegeStatePacket || packet.0 == sc::SCDominionDataPacket
        ));

        // The explicit snapshot still uses the real clock and shows the scheduled warmup.
        let data = snapshot(&f.server, &before, real).unwrap();
        assert_eq!(data.siege_timers.siege_period, 3);
    }

    #[tokio::test]
    async fn out_of_tick_schedule_and_cancel_are_not_reannounced() {
        let mut f = fixture();
        enable_calendar(&f);
        owned(&f);
        let real = Utc::now().timestamp();
        let calendar = &f.server.systems.housing.calendar;
        calendar.set_clock(Some(kst(2014, 1, 9, 12, 0)));
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        f.client.drain();

        let scheduled = f
            .server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, 88, real + 300, real + 900, real)
            .unwrap();
        broadcast_siege_declared(&f.server, &scheduled);
        broadcast_phase_change(&f.server, &scheduled);
        let packets = f.client.drain();
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.0 == sc::SCSiegeStatePacket)
                .count(),
            1
        );
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(!f
            .client
            .drain()
            .iter()
            .any(|packet| packet.0 == sc::SCSiegeStatePacket));

        let cancelled = f
            .server
            .systems
            .housing
            .dominions
            .cancel_siege(ZONE, real)
            .unwrap();
        broadcast_phase_change(&f.server, &cancelled);
        let packets = f.client.drain();
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.0 == sc::SCSiegeStatePacket)
                .count(),
            1
        );
        crate::systems::housing::dominion::runtime::tick_once(&f.server);
        assert!(!f
            .client
            .drain()
            .iter()
            .any(|packet| packet.0 == sc::SCSiegeStatePacket));
    }
}
