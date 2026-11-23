use super::*;

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
