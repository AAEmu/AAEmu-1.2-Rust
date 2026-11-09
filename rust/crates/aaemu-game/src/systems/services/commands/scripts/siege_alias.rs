//! Retail siege / dominion GM command aliases (WP-SL6a).
//!
//! The 1.2 client knows these command names (its command string table; EVIDENCED) and the GM
//! console builds `ScheduleSiege(%d,%d,%d,%d,%d,%d)` and `DeleteDominion(%s, %d)` with those
//! argument shapes (`hud/gm_console_view.alb`). Every other argument shape here is assumed.
//!
//! `ScheduleSiege`'s six integers are read as zone group + year, month, day, hour, minute
//! (INFERRED; the console labels next to the shape are the five period minute fields, so the
//! retail reading of the remaining integers is UNKNOWN). The attacker is the territory's
//! accepted declaration: the six arguments carry no expedition, and the declaration chain is
//! the only other authority that names one. `DeleteDominion`'s string is read as the name of a
//! character whose expedition currently owns the territory (the console passes a selected
//! player for the other name-shaped commands).
//!
//! `declare_dominion_always` is deliberately not registered: it would skip the claim gates,
//! and no existing switch already carries that meaning (the claim window, pirate refusal and
//! `Housing.DominionClaimLimit` are separate checks). Its retail semantics are INFERRED only.

use chrono::TimeZone;

use super::prelude::*;
use crate::systems::housing::dominion::calendar::CalendarTimeZone;
use crate::systems::housing::dominion::state::DominionError;
use crate::systems::housing::dominion::{reinforce, runtime, SiegeLimits};

/// `ScheduleSiege(%d,%d,%d,%d,%d,%d)`, INFERRED as zone group + wall-clock date/time.
pub struct ScheduleSiege;
/// `stop_siege` / `off_siege` (both names are EVIDENCED; the zone argument is assumed).
pub struct StopSiege;
/// `DeleteDominion(%s, %d)`: owning character name + zone group.
pub struct DeleteDominion;
/// `change_dominion_owner <zoneGroupId> <expeditionId>` (shape assumed).
pub struct ChangeDominionOwner;
/// `bestow_dominion <zoneGroupId> <expeditionId>` (shape assumed).
pub struct BestowDominion;
/// `set_dominion_non_pvp <zoneGroupId> [minutes]` (shape assumed; 120 is the authored row).
pub struct SetDominionNonPvp;
/// `set_dominion_limit <limit>` (shape assumed; updates `Housing.DominionClaimLimit`).
pub struct SetDominionLimit;

impl ScheduleSiege {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl StopSiege {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl DeleteDominion {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl ChangeDominionOwner {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl BestowDominion {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl SetDominionNonPvp {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl SetDominionLimit {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

/// The configured calendar timezone's Unix seconds for a local wall-clock time.
fn local_timestamp(
    server: &GameServer,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<i64> {
    let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing);
    match settings.timezone {
        CalendarTimeZone::Fixed(offset) => offset
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.timestamp()),
        CalendarTimeZone::Local => chrono::Local
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.timestamp()),
    }
}

fn arg_error(command: &dyn Command, out: &mut dyn MessageOutput, text: &str) {
    CommandManager::send_default_help_text(command, out);
    CommandManager::send_error_text(command, out, text);
}

impl Command for ScheduleSiege {
    fn command_names(&self) -> Vec<String> {
        names(&["schedule_siege"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.len() != 6 {
            arg_error(
                self,
                out,
                "Expected <zoneGroupId> <year> <month> <day> <hour> <minute>",
            );
            return Ok(());
        }
        let (Some(zone), Some(year), Some(month), Some(day), Some(hour), Some(minute)) = (
            try_parse::<u32>(&args[0]),
            try_parse::<i32>(&args[1]),
            try_parse::<u32>(&args[2]),
            try_parse::<u32>(&args[3]),
            try_parse::<u32>(&args[4]),
            try_parse::<u32>(&args[5]),
        ) else {
            arg_error(self, out, "The arguments must be integers");
            return Ok(());
        };
        if !(1970..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
        {
            arg_error(self, out, "The date or time is out of range");
            return Ok(());
        }
        let server = ctx.server()?;
        let Some(start) = local_timestamp(server, year, month, day, hour, minute) else {
            CommandManager::send_error_text(self, out, "Invalid siege date/time");
            return Ok(());
        };
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let manager = &server.systems.housing.dominions;
        let Some(previous) = manager.get(zone) else {
            CommandManager::send_error_text(self, out, "This territory has no dominion");
            return Ok(());
        };
        let Some(attacker) = previous
            .declaration
            .map(|declaration| declaration.attacker_expedition_id)
            .filter(|expedition| *expedition != 0)
        else {
            CommandManager::send_error_text(
                self,
                out,
                "This territory has no accepted siege declaration",
            );
            return Ok(());
        };
        if server.expeditions.get(attacker).is_none() {
            CommandManager::send_error_text(self, out, "The declared attacker no longer exists");
            return Ok(());
        }
        let Some(template) = server
            .game_data
            .systems
            .housing
            .castles
            .siege_zones
            .get(&zone)
        else {
            CommandManager::send_error_text(self, out, "Zone has no original siege timing data");
            return Ok(());
        };
        let duration = i64::from(template.siege_days) * 86_400
            + i64::from(template.siege_hours) * 3_600
            + i64::from(template.siege_mins) * 60;
        let now = chrono::Utc::now().timestamp();
        let Some(end) = start.checked_add(duration) else {
            CommandManager::send_error_text(self, out, "The siege deadline overflows");
            return Ok(());
        };
        if start < now {
            CommandManager::send_error_text(self, out, "The siege cannot start in the past");
            return Ok(());
        }
        let limits =
            reinforce::limits(server, previous.owner_expedition_id).unwrap_or(SiegeLimits::NATIVE);
        match manager.schedule_siege_with_limits(zone, attacker, start, end, now, limits) {
            Ok(state) => {
                crate::systems::social::dominion::broadcast_siege_declared(server, &state);
                crate::systems::social::dominion::broadcast_participant_changes(
                    server, &previous, &state,
                );
                runtime::apply_tower_state(server, &state);
                crate::systems::social::dominion::broadcast_phase_change(server, &state);
                CommandManager::send_normal_text(
                    self,
                    out,
                    &format!(
                        "Scheduled the siege of territory {zone} for {start} (attacker expedition {attacker})"
                    ),
                );
            }
            Err(error) => CommandManager::send_error_text(self, out, &error.to_string()),
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<zoneGroupId> <year> <month> <day> <hour> <minute>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Schedule the accepted declaration's siege at a wall-clock time in Housing.SiegeCalendarTimezone (UTC by default) using the zone's authored duration.".into()
    }
}

impl Command for StopSiege {
    fn command_names(&self) -> Vec<String> {
        names(&["stop_siege", "off_siege"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.len() != 1 {
            arg_error(self, out, "Expected <zoneGroupId>");
            return Ok(());
        }
        let Some(zone) = try_parse::<u32>(&args[0]) else {
            arg_error(self, out, "The zone group must be an integer");
            return Ok(());
        };
        let server = ctx.server()?;
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let manager = &server.systems.housing.dominions;
        let previous = manager.get(zone);
        let now = chrono::Utc::now().timestamp();
        match manager.cancel_siege(zone, now) {
            Ok(state) => {
                if let Some(previous) = &previous {
                    crate::systems::social::dominion::broadcast_participant_changes(
                        server, previous, &state,
                    );
                }
                runtime::apply_tower_state(server, &state);
                crate::systems::social::dominion::broadcast_phase_change(server, &state);
                CommandManager::send_normal_text(
                    self,
                    out,
                    &format!("Cancelled the siege of territory {zone}"),
                );
            }
            Err(error) => CommandManager::send_error_text(self, out, &error.to_string()),
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<zoneGroupId>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Cancel a scheduled or running siege (same path as /dominion cancel).".into()
    }
}

impl Command for DeleteDominion {
    fn command_names(&self) -> Vec<String> {
        names(&["delete_dominion"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.len() != 2 {
            arg_error(self, out, "Expected <character> <zoneGroupId>");
            return Ok(());
        }
        let Some(zone) = try_parse::<u32>(&args[1]) else {
            arg_error(self, out, "The zone group must be an integer");
            return Ok(());
        };
        let server = ctx.server()?;
        let Some(character) = super::retail::character_id(server, &args[0]) else {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Character '{}' was not found", args[0]),
            );
            return Ok(());
        };
        let expedition = server.expeditions.expedition_of_character(character);
        if expedition == 0 {
            CommandManager::send_error_text(
                self,
                out,
                &format!("'{}' is not in an expedition", args[0]),
            );
            return Ok(());
        }
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let manager = &server.systems.housing.dominions;
        let Some(state) = manager.get(zone) else {
            CommandManager::send_error_text(self, out, "This territory has no dominion");
            return Ok(());
        };
        if state.owner_expedition_id != expedition {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Territory {zone} is not owned by '{}'s expedition", args[0]),
            );
            return Ok(());
        }
        if state.siege.as_ref().is_some_and(|siege| {
            siege.status != crate::systems::housing::dominion::SiegeStatus::Finished
        }) {
            CommandManager::send_error_text(
                self,
                out,
                "A siege is in progress; cancel it before deleting the territory",
            );
            return Ok(());
        }
        match manager.remove_tower(zone, state.tower) {
            Ok(removed) => {
                crate::systems::social::dominion_notifications::broadcast_deleted(
                    server,
                    removed.zone_group_id,
                );
                CommandManager::send_normal_text(
                    self,
                    out,
                    &format!("Deleted the dominion of territory {zone}"),
                );
            }
            Err(error) => CommandManager::send_error_text(self, out, &error.to_string()),
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<character> <zoneGroupId>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Delete the dominion record of the territory owned by <character>'s expedition.".into()
    }
}

fn transfer(
    command: &dyn Command,
    ctx: &CommandContext<'_>,
    args: &[String],
    out: &mut dyn MessageOutput,
    bestowed: bool,
) -> anyhow::Result<()> {
    if args.len() != 2 {
        arg_error(command, out, "Expected <zoneGroupId> <expeditionId>");
        return Ok(());
    }
    let (Some(zone), Some(expedition)) = (try_parse::<u32>(&args[0]), try_parse::<u32>(&args[1]))
    else {
        arg_error(command, out, "The arguments must be integers");
        return Ok(());
    };
    let server = ctx.server()?;
    match server.expeditions.get(expedition) {
        Some(expedition_info) if !expedition_info.is_disbanded => {}
        _ => {
            CommandManager::send_error_text(
                command,
                out,
                "The expedition does not exist or is disbanded",
            );
            return Ok(());
        }
    }
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let manager = &server.systems.housing.dominions;
    let Some(previous) = manager.get(zone) else {
        CommandManager::send_error_text(command, out, "This territory has no dominion");
        return Ok(());
    };
    match manager.transfer_owner(
        zone,
        expedition,
        chrono::Utc::now().timestamp(),
        server.config.housing.dominion_claim_limit,
    ) {
        Ok(state) => {
            runtime::apply_tower_state(server, &state);
            crate::systems::social::dominion::clear_national_monument_on_owner_change(
                server, &previous, &state,
            );
            crate::systems::social::dominion::broadcast_owner_change_with(server, &state, bestowed);
            CommandManager::send_normal_text(
                command,
                out,
                &format!("Territory {zone} owner changed to expedition {expedition}"),
            );
        }
        Err(DominionError::TooManyOwned) => CommandManager::send_error_text(
            command,
            out,
            "The expedition has reached the configured owned-dominion limit",
        ),
        Err(DominionError::SiegeInProgress) => CommandManager::send_error_text(
            command,
            out,
            "A siege is in progress; cancel it before changing the owner",
        ),
        Err(error) => CommandManager::send_error_text(command, out, &error.to_string()),
    }
    Ok(())
}

impl Command for ChangeDominionOwner {
    fn command_names(&self) -> Vec<String> {
        names(&["change_dominion_owner"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        transfer(self, ctx, args, out, false)
    }

    fn get_command_line_help(&self) -> String {
        "<zoneGroupId> <expeditionId>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Change a territory's owner to an existing expedition (refused during an unfinished siege)."
            .into()
    }
}

impl Command for BestowDominion {
    fn command_names(&self) -> Vec<String> {
        names(&["bestow_dominion"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        transfer(self, ctx, args, out, true)
    }

    fn get_command_line_help(&self) -> String {
        "<zoneGroupId> <expeditionId>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Bestow a territory on an existing expedition (the client announces it as bestowed).".into()
    }
}

impl Command for SetDominionNonPvp {
    fn command_names(&self) -> Vec<String> {
        names(&["set_dominion_non_pvp"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.is_empty() || args.len() > 2 {
            arg_error(self, out, "Expected <zoneGroupId> [minutes]");
            return Ok(());
        }
        let Some(zone) = try_parse::<u32>(&args[0]) else {
            arg_error(self, out, "The zone group must be an integer");
            return Ok(());
        };
        let minutes = match args.get(1) {
            Some(value) => match try_parse::<u16>(value) {
                Some(minutes) if minutes > 0 => minutes,
                _ => {
                    arg_error(self, out, "The minutes must be between 1 and 65535");
                    return Ok(());
                }
            },
            None => 120,
        };
        let server = ctx.server()?;
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let manager = &server.systems.housing.dominions;
        let Some(state) = manager.get(zone) else {
            CommandManager::send_error_text(self, out, "This territory has no dominion");
            return Ok(());
        };
        if state.owner_expedition_id == 0 {
            CommandManager::send_error_text(self, out, "This territory has no owner");
            return Ok(());
        }
        match manager.start_non_pvp(
            zone,
            state.owner_expedition_id,
            minutes,
            chrono::Utc::now().timestamp(),
        ) {
            Ok(state) => {
                crate::systems::social::dominion_notifications::broadcast_non_pvp_started(
                    server, &state,
                );
                crate::systems::social::dominion::broadcast_snapshot(server, &state, false);
                CommandManager::send_normal_text(
                    self,
                    out,
                    &format!("Territory {zone} starts a {minutes}-minute non-PvP window"),
                );
            }
            Err(error) => CommandManager::send_error_text(self, out, &error.to_string()),
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<zoneGroupId> [minutes]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Start the territory's non-PvP peace window (default 120 minutes, the authored row).".into()
    }
}

impl Command for SetDominionLimit {
    fn command_names(&self) -> Vec<String> {
        names(&["set_dominion_limit"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.len() != 1 {
            arg_error(self, out, "Expected <limit>");
            return Ok(());
        }
        let Some(limit) = try_parse::<u32>(&args[0]) else {
            arg_error(self, out, "The limit must be a non-negative integer");
            return Ok(());
        };
        let server = ctx.server()?;
        server
            .config
            .update(|config| config.housing.dominion_claim_limit = limit);
        CommandManager::send_normal_text(
            self,
            out,
            &format!("Dominion claim limit set to {limit}"),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<limit>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Set Housing.DominionClaimLimit for new territory claims at runtime.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness::{self, Pc};
    use super::*;
    use crate::data::world_data::{SystemFaction, Zone};
    use crate::data::GameData;
    use crate::models::house::House;
    use crate::opcodes::sc;
    use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate, SiegeSettings};
    use crate::systems::housing::dominion::DominionState;
    use crate::systems::social::expedition::{
        member_from_character, Expedition, ExpeditionRolePolicy,
    };
    use crate::test_support::TEST_ZONE;
    use crate::world::Entity;
    use aaemu_commons::PacketStream;

    const ZONE: u32 = 25;
    const OWNER: u32 = 10;
    const ATTACKER: u32 = 20;
    const RECIPIENT: u32 = 30;

    struct Fixture {
        server: Arc<GameServer>,
        owner: Pc,
        attacker: Pc,
    }

    fn fixture() -> Fixture {
        let mut data = GameData::empty();
        data.zones.zones.insert(
            TEST_ZONE,
            Zone {
                zone_key: TEST_ZONE,
                group_id: ZONE,
                ..Default::default()
            },
        );
        data.systems.housing.add_template(HousingTemplate {
            id: 139,
            guard_tower_setting_id: 6,
            ..Default::default()
        });
        data.systems.housing.castles.guard_tower_settings.insert(
            6,
            GuardTowerSetting {
                id: 6,
                radius_dominion: 200,
                ..Default::default()
            },
        );
        data.systems.housing.castles.siege_zones.insert(
            ZONE,
            crate::systems::housing::data::SiegeZone {
                id: 4,
                zone_group_id: ZONE,
                siege_hours: 1,
                ..Default::default()
            },
        );
        data.systems.housing.castles.siege_settings.insert(
            1,
            SiegeSettings {
                total_castles: 1,
                num_defenders: 50,
                num_reinforcements: 20,
            },
        );
        for id in [3733, 3734] {
            data.systems
                .skills
                .add_buff(crate::systems::skills::data::BuffTemplate {
                    id,
                    max_stack: 1,
                    system: true,
                    dead_applicable: true,
                    ..Default::default()
                });
        }
        let server = harness::server_with(data);
        // The GM `schedule_siege` time fields are wall-clock times in the configured calendar
        // timezone (UTC by default). Pin it so the test does not depend on the host's zone.
        server
            .config
            .update(|c| c.housing.siege_calendar_timezone = "+00:00".into());
        server.systems.services.commands.access_levels.load(&[
            ("joinsiege".into(), 0),
            ("leavesiege".into(), 0),
            ("dominion".into(), 100),
        ]);
        let mut owner = harness::pc(&server, 1, "Owner", 100.0, 100.0);
        let mut attacker = harness::pc(&server, 2, "Attacker", 100.0, 100.0);
        let mut outsider = harness::pc(&server, 3, "Outsider", 100.0, 100.0);
        for (id, pc) in [
            (OWNER, &owner),
            (ATTACKER, &attacker),
            (RECIPIENT, &outsider),
        ] {
            pc.p.data().access_level = 0;
            pc.p.data().unit.expedition_id = id;
            server.expeditions.add(Expedition {
                faction: SystemFaction {
                    id,
                    owner_id: 90 + id,
                    ..Default::default()
                },
                members: vec![member_from_character(&server, id, &pc.p, false)],
                policies: vec![ExpeditionRolePolicy {
                    expedition_id: id,
                    role: 0,
                    name: "Member".into(),
                    dominion_declare: false,
                    invite: false,
                    expel: false,
                    promote: false,
                    dismiss: false,
                    chat: true,
                    manager_chat: false,
                    siege_master: false,
                    join_siege: true,
                }],
                removed_members: Vec::new(),
                is_disbanded: false,
            });
        }
        let mut house = House::new(61, 139);
        house.current_step = -1;
        house.unit.transform.zone_id = TEST_ZONE;
        house.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let tower = Entity::new(0x200, house);
        server.systems.housing.manager.insert(&tower);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&tower, &server.view_ctx());
        let (_, identity) = runtime::tower_identity(&server, &tower).unwrap();
        let now = chrono::Utc::now().timestamp();
        server
            .systems
            .housing
            .dominions
            .declare_unowned_with(ZONE, identity, OWNER, now - 1_000, || true)
            .unwrap();
        owner.frames();
        attacker.frames();
        outsider.frames();
        Fixture {
            server,
            owner,
            attacker,
        }
    }

    fn state(f: &Fixture) -> DominionState {
        f.server.systems.housing.dominions.get(ZONE).unwrap()
    }

    fn register_declaration(f: &Fixture) {
        let now = chrono::Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .declare(ZONE, ATTACKER, now.max(1), now, 0, (100.0, 100.0, 100.0))
            .unwrap();
    }

    /// The fixture's `Housing.SiegeCalendarTimezone` is `+00:00`, so these are the wall-clock
    /// fields the GM `schedule_siege` command expects, independent of the host's local zone.
    fn calendar_fields(timestamp: i64) -> (i32, u32, u32, u32, u32) {
        use chrono::{Datelike, FixedOffset, TimeZone as _, Timelike};
        let timezone = FixedOffset::east_opt(0).unwrap();
        let local = timezone.timestamp_opt(timestamp, 0).single().unwrap();
        (
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
        )
    }

    #[tokio::test]
    async fn schedule_siege_uses_the_declaration_attacker_and_the_authored_duration() {
        let f = fixture();
        register_declaration(&f);
        let start = (chrono::Utc::now().timestamp() + 3_600) / 60 * 60;
        let (year, month, day, hour, minute) = calendar_fields(start);
        let (_, out) = harness::run(
            &f.server,
            &f.attacker.p,
            &format!("schedule_siege {ZONE} {year} {month} {day} {hour} {minute}"),
        )
        .await;
        assert_eq!(
            out,
            [harness::normal(
                "schedule_siege",
                &format!("Scheduled the siege of territory {ZONE} for {start} (attacker expedition {ATTACKER})")
            )]
        );
        let siege = state(&f).siege.unwrap();
        assert_eq!(siege.attacker_expedition_id, ATTACKER);
        assert_eq!(siege.defender_expedition_id, OWNER);
        assert_eq!((siege.starts_at, siege.ends_at), (start, start + 3_600));
    }

    /// The GM time fields are the configured calendar timezone's wall clock, not the host's
    /// local zone (the UTC default moved the claim window too; this command must follow it).
    #[tokio::test]
    async fn schedule_siege_parses_the_wall_clock_in_the_configured_timezone() {
        use chrono::{Datelike, FixedOffset, TimeZone as _, Timelike};
        let f = fixture();
        register_declaration(&f);
        f.server
            .config
            .update(|c| c.housing.siege_calendar_timezone = "+02:00".into());
        let start = (chrono::Utc::now().timestamp() + 3 * 3_600) / 60 * 60;
        let timezone = FixedOffset::east_opt(2 * 3_600).unwrap();
        let local = timezone.timestamp_opt(start, 0).single().unwrap();
        let (year, month, day, hour, minute) = (
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
        );
        let (_, out) = harness::run(
            &f.server,
            &f.attacker.p,
            &format!("schedule_siege {ZONE} {year} {month} {day} {hour} {minute}"),
        )
        .await;
        assert_eq!(
            out,
            [harness::normal(
                "schedule_siege",
                &format!("Scheduled the siege of territory {ZONE} for {start} (attacker expedition {ATTACKER})")
            )]
        );
        assert_eq!(state(&f).siege.unwrap().starts_at, start);
    }

    #[tokio::test]
    async fn schedule_siege_rejects_arguments_missing_declarations_and_past_times() {
        let f = fixture();
        let empty = state(&f);
        for command in [
            "schedule_siege 25 2026 1 1 0",
            "schedule_siege 25 2026 1 1 0 0 extra",
            "schedule_siege 25 x 1 1 0 0",
            "schedule_siege 25 2026 13 1 0 0",
            "schedule_siege 25 2026 1 1 24 0",
        ] {
            let (_, out) = harness::run(&f.server, &f.attacker.p, command).await;
            assert!(!out.is_empty(), "{command}");
            assert_eq!(state(&f), empty, "{command}");
        }
        let (_, out) =
            harness::run(&f.server, &f.attacker.p, "schedule_siege 25 2030 1 1 0 0").await;
        assert_eq!(
            out,
            [harness::error(
                "schedule_siege",
                "This territory has no accepted siege declaration"
            )]
        );
        register_declaration(&f);
        let past = chrono::Utc::now().timestamp() - 3_600;
        let (year, month, day, hour, minute) = calendar_fields(past);
        let (_, out) = harness::run(
            &f.server,
            &f.attacker.p,
            &format!("schedule_siege {ZONE} {year} {month} {day} {hour} {minute}"),
        )
        .await;
        assert_eq!(
            out,
            [harness::error(
                "schedule_siege",
                "The siege cannot start in the past"
            )]
        );
    }

    #[tokio::test]
    async fn stop_and_off_siege_cancel_and_are_idempotent_errors() {
        for command in ["stop_siege", "off_siege"] {
            let f = fixture();
            let now = chrono::Utc::now().timestamp();
            f.server
                .systems
                .housing
                .dominions
                .schedule_siege(ZONE, ATTACKER, now, now + 3_600, now)
                .unwrap();
            let (_, out) =
                harness::run(&f.server, &f.attacker.p, &format!("{command} {ZONE}")).await;
            assert_eq!(
                out,
                [harness::normal(
                    "stop_siege",
                    &format!("Cancelled the siege of territory {ZONE}")
                )]
            );
            assert_eq!(
                state(&f).siege.as_ref().unwrap().outcome,
                crate::systems::housing::dominion::SiegeOutcome::Cancelled
            );
            let (_, out) =
                harness::run(&f.server, &f.attacker.p, &format!("{command} {ZONE}")).await;
            assert_eq!(out, [harness::error("stop_siege", "siege is not active")]);
            let (_, out) = harness::run(&f.server, &f.attacker.p, command).await;
            assert!(!out.is_empty());
        }
    }

    #[tokio::test]
    async fn delete_dominion_requires_the_owning_expedition_and_removes_the_record() {
        let f = fixture();
        let (_, out) = harness::run(&f.server, &f.owner.p, "delete_dominion Outsider 25").await;
        assert_eq!(
            out,
            [harness::error(
                "delete_dominion",
                "Territory 25 is not owned by 'Outsider's expedition"
            )]
        );
        assert!(f.server.systems.housing.dominions.get(ZONE).is_some());
        let (_, out) = harness::run(&f.server, &f.owner.p, "delete_dominion Owner 25").await;
        assert_eq!(
            out,
            [harness::normal(
                "delete_dominion",
                "Deleted the dominion of territory 25"
            )]
        );
        assert!(f.server.systems.housing.dominions.get(ZONE).is_none());
        let (_, out) = harness::run(&f.server, &f.owner.p, "delete_dominion Owner 25").await;
        assert_eq!(
            out,
            [harness::error(
                "delete_dominion",
                "This territory has no dominion"
            )]
        );
        let (_, out) = harness::run(&f.server, &f.owner.p, "delete_dominion Nobody 25").await;
        assert!(out[0].contains("was not found"));
        let (_, out) = harness::run(&f.server, &f.owner.p, "delete_dominion Owner x").await;
        assert!(!out.is_empty());
    }

    #[tokio::test]
    async fn change_and_bestow_transfer_the_owner_and_announce_the_choice() {
        let mut f = fixture();
        let (_, out) = harness::run(
            &f.server,
            &f.owner.p,
            &format!("change_dominion_owner {ZONE} {RECIPIENT}"),
        )
        .await;
        assert_eq!(
            out,
            [harness::normal(
                "change_dominion_owner",
                &format!("Territory {ZONE} owner changed to expedition {RECIPIENT}")
            )]
        );
        let changed = state(&f);
        assert_eq!(changed.owner_expedition_id, RECIPIENT);
        assert_eq!(changed.claimed_at, 0);
        let packets = f.owner.packets();
        let owner_changed = packets
            .iter()
            .find(|(op, _)| *op == sc::SCDominionOwnerChangedPacket)
            .unwrap();
        let mut expected = PacketStream::new();
        expected
            .write_u16(ZONE as u16)
            .write_u32(RECIPIENT)
            .write_u64(changed.reign_start_at as u64)
            .write_bool(false);
        assert_eq!(owner_changed.1, expected.into_bytes());

        let (_, out) = harness::run(
            &f.server,
            &f.owner.p,
            &format!("bestow_dominion {ZONE} {ATTACKER}"),
        )
        .await;
        assert_eq!(
            out,
            [harness::normal(
                "bestow_dominion",
                &format!("Territory {ZONE} owner changed to expedition {ATTACKER}")
            )]
        );
        let bestowed = state(&f);
        assert_eq!(bestowed.owner_expedition_id, ATTACKER);
        let packets = f.owner.packets();
        let owner_changed = packets
            .iter()
            .find(|(op, _)| *op == sc::SCDominionOwnerChangedPacket)
            .unwrap();
        let mut expected = PacketStream::new();
        expected
            .write_u16(ZONE as u16)
            .write_u32(ATTACKER)
            .write_u64(bestowed.reign_start_at as u64)
            .write_bool(true);
        assert_eq!(owner_changed.1, expected.into_bytes());
    }

    #[tokio::test]
    async fn transfer_obeys_the_owned_limit_and_refuses_a_running_siege() {
        let f = fixture();
        f.server
            .config
            .update(|config| config.housing.dominion_claim_limit = 0);
        let (_, out) = harness::run(
            &f.server,
            &f.owner.p,
            &format!("bestow_dominion {ZONE} {RECIPIENT}"),
        )
        .await;
        assert!(out[0].contains("owned-dominion limit"));
        assert_eq!(state(&f).owner_expedition_id, OWNER);

        f.server
            .config
            .update(|config| config.housing.dominion_claim_limit = 10);
        let now = chrono::Utc::now().timestamp();
        f.server
            .systems
            .housing
            .dominions
            .schedule_siege(ZONE, ATTACKER, now, now + 3_600, now)
            .unwrap();
        let (_, out) = harness::run(
            &f.server,
            &f.owner.p,
            &format!("change_dominion_owner {ZONE} {RECIPIENT}"),
        )
        .await;
        assert_eq!(
            out,
            [harness::error(
                "change_dominion_owner",
                "A siege is in progress; cancel it before changing the owner"
            )]
        );
        assert_eq!(state(&f).owner_expedition_id, OWNER);
        let (_, out) = harness::run(
            &f.server,
            &f.owner.p,
            &format!("change_dominion_owner {ZONE} 99"),
        )
        .await;
        assert!(out[0].contains("does not exist"));
    }

    #[tokio::test]
    async fn set_dominion_non_pvp_starts_the_window_and_rejects_a_second_call() {
        let f = fixture();
        let (_, out) = harness::run(
            &f.server,
            &f.owner.p,
            &format!("set_dominion_non_pvp {ZONE}"),
        )
        .await;
        assert_eq!(
            out,
            [harness::normal(
                "set_dominion_non_pvp",
                "Territory 25 starts a 120-minute non-PvP window"
            )]
        );
        let state = state(&f);
        assert!(state.non_pvp_start_at > 0);
        assert_eq!(state.non_pvp_duration_min, 120);
        let (_, out) = harness::run(
            &f.server,
            &f.owner.p,
            &format!("set_dominion_non_pvp {ZONE} 60"),
        )
        .await;
        assert!(out[0].contains("already in its non-PvP"));
        let (_, out) = harness::run(&f.server, &f.owner.p, "set_dominion_non_pvp 25 x").await;
        assert!(!out.is_empty());
        let (_, out) = harness::run(&f.server, &f.owner.p, "set_dominion_non_pvp 99").await;
        assert_eq!(
            out,
            [harness::error(
                "set_dominion_non_pvp",
                "This territory has no dominion"
            )]
        );
    }

    #[tokio::test]
    async fn set_dominion_limit_updates_the_claim_cap_used_by_claims() {
        let f = fixture();
        let (_, out) = harness::run(&f.server, &f.owner.p, "set_dominion_limit 3").await;
        assert_eq!(
            out,
            [harness::normal(
                "set_dominion_limit",
                "Dominion claim limit set to 3"
            )]
        );
        assert_eq!(f.server.config.housing.dominion_claim_limit, 3);
        let (_, out) = harness::run(&f.server, &f.owner.p, "set_dominion_limit -1").await;
        assert!(!out.is_empty());
        assert_eq!(f.server.config.housing.dominion_claim_limit, 3);
    }

    #[tokio::test]
    async fn every_alias_requires_admin_access() {
        let f = fixture();
        let commands = [
            ("schedule_siege", "schedule_siege 25 2030 1 1 0 0"),
            ("stop_siege", "stop_siege 25"),
            ("off_siege", "off_siege 25"),
            ("delete_dominion", "delete_dominion Owner 25"),
            ("change_dominion_owner", "change_dominion_owner 25 30"),
            ("bestow_dominion", "bestow_dominion 25 30"),
            ("set_dominion_non_pvp", "set_dominion_non_pvp 25"),
            ("set_dominion_limit", "set_dominion_limit 5"),
        ];
        for (name, text) in commands {
            let (handled, out) = harness::run_at(&f.server, &f.attacker.p, text, 0).await;
            assert!(handled, "{name}");
            assert_eq!(out, ["|cFFFF0000Insufficient privileges.|r"], "{name}");
        }
        assert!(f
            .server
            .systems
            .services
            .commands
            .get_command_interface_by_name("declare_dominion_always")
            .is_none());
    }
}
