//! The automatic weekly siege calendar (WP-S1).
//!
//! The client side is fully decoded in the 1.2 decompile:
//!
//! * period ids `FUN_3956fc70`: 0 no_dominion, 1 auction, 2 declare, 3 warmup, 4 siege,
//!   5 peace; the order is `FUN_3946ba60` 1 -> 2 -> 3 -> 4 -> 5 -> 1.
//! * `FUN_39839740` reads the five wire durations into slots 1..=5, so they are the lengths of
//!   [auction, declare, warmup, siege, peace]; `FUN_39838ff0`/`FUN_39839040` derive a period's
//!   start/end as `started + sum(durations before/through it)`. `started` is the auction start
//!   of the described cycle.
//! * `FUN_3946be90` in period 5 returns `start(1)`, i.e. `started` itself; therefore a peace
//!   snapshot must carry the next cycle's auction start (and the next cycle's durations).
//! * `FUN_392f7db0` shows "unearned_win" when the new period is 5, the cached one was 1 or 2
//!   and the cached attacker team id is 0, which is exactly "declare ended with no declaration".
//!
//! This module is pure date arithmetic over the authored `siege_zones` columns and the
//! `siege_plans` rotation; it never touches the world. The timezone is a server setting; its
//! default is UTC because the Western 2014 schedule (Total ArcheAge, 2014-11-20/25, grade C)
//! names the same wall-clock times in GMT as the Korean data (`round6-siege.md` 1). Weekday
//! 0 = Sunday is INFERRED from the `siege_plans` week starts, which are all Sundays, and that
//! reading is corroborated by those Sunday siege times (grade C).

use chrono::{
    DateTime, Datelike, Days, Duration, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime,
    TimeZone,
};
use std::collections::BTreeMap;

use crate::config::ServerProfile;
use crate::systems::housing::config::HousingConfig;
use crate::systems::housing::data::{CastleReferenceData, SiegeRotation, SiegeZone};

pub const PERIOD_NONE: u8 = 0;
pub const PERIOD_AUCTION: u8 = 1;
pub const PERIOD_DECLARE: u8 = 2;
pub const PERIOD_WARMUP: u8 = 3;
pub const PERIOD_SIEGE: u8 = 4;
pub const PERIOD_PEACE: u8 = 5;

/// A cycle's next occurrence is one rotation period away; the shipped plan rotation is three
/// weeks. The bounded search keeps a malformed plan table from looping forever.
const MAX_SEARCH_WEEKS: i64 = 8;

/// Wall-clock timezone of the `siege_zones` columns.
///
/// The default is UTC: the Western launch-era schedule (Total ArcheAge, 2014-11-20/25, grade C)
/// lists the same wall-clock times the Korean data has, as GMT (`round6-siege.md` 1). The
/// retail zone of the data is not proven, so a server can still select `local` or an explicit
/// offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarTimeZone {
    Local,
    Fixed(FixedOffset),
}

impl Default for CalendarTimeZone {
    fn default() -> Self {
        Self::Fixed(FixedOffset::east_opt(0).expect("UTC is a valid fixed offset"))
    }
}

impl std::fmt::Display for CalendarTimeZone {
    /// `UTC+09:00` / `UTC-05:30` / `server local time`; used by the one-line startup notice so
    /// a server can see which offset actually applies (the claim window uses it even with the
    /// siege calendar off).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("server local time"),
            Self::Fixed(offset) => {
                let minutes = offset.local_minus_utc() / 60;
                let sign = if minutes < 0 { '-' } else { '+' };
                let hours = minutes.abs() / 60;
                let minutes = minutes.abs() % 60;
                write!(f, "UTC{sign}{hours:02}:{minutes:02}")
            }
        }
    }
}

impl CalendarTimeZone {
    /// `""`/`utc`/`z` = UTC (the default); `local` = server local; or an explicit `+HH:MM`,
    /// `+HHMM`, `+HH`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("utc") || value.eq_ignore_ascii_case("z")
        {
            return Some(Self::default());
        }
        if value.eq_ignore_ascii_case("local") {
            return Some(Self::Local);
        }
        if !value.is_ascii() {
            return None; // the offset body is sliced by byte index below
        }
        let sign = match value.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        let body = &value[1..];
        let (hours, minutes) = match body.split_once(':') {
            Some((hours, minutes)) => (hours.parse::<i32>().ok()?, minutes.parse::<i32>().ok()?),
            None => match body.len() {
                1 | 2 => (body.parse::<i32>().ok()?, 0),
                4 => (
                    body[..2].parse::<i32>().ok()?,
                    body[2..].parse::<i32>().ok()?,
                ),
                _ => return None,
            },
        };
        if hours > 23 || minutes > 59 {
            return None;
        }
        FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60)).map(Self::Fixed)
    }

    fn local_datetime(&self, timestamp: i64) -> Option<DateTime<FixedOffset>> {
        match self {
            Self::Local => Local
                .timestamp_opt(timestamp, 0)
                .single()
                .map(|dt| dt.fixed_offset()),
            Self::Fixed(offset) => offset.timestamp_opt(timestamp, 0).single(),
        }
    }

    fn timestamp(&self, naive: NaiveDateTime) -> Option<i64> {
        match self {
            Self::Local => match Local.from_local_datetime(&naive) {
                LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => Some(dt.timestamp()),
                LocalResult::None => None,
            },
            Self::Fixed(offset) => offset
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.timestamp()),
        }
    }

    /// Local wall-clock to Unix seconds for the claim window. A local time that does not exist
    /// (DST spring-forward gap) falls forward to the first valid minute instead of making the
    /// window unknown; an ambiguous fall-back time uses the earlier occurrence.
    fn claim_timestamp(&self, naive: NaiveDateTime) -> Option<i64> {
        match self {
            Self::Fixed(offset) => offset
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.timestamp()),
            Self::Local => {
                // `chrono::Local` returns a `Single` value even for a skipped wall-clock time
                // on Windows, so validate a `Single` by round-tripping: a valid local instant
                // converts back to itself. An `Ambiguous` fall-back time is valid and uses the
                // earlier occurrence. Advance minute by minute through a spring-forward gap to
                // the first valid instant.
                let mut candidate = naive;
                for _ in 0..=180 {
                    match Local.from_local_datetime(&candidate) {
                        LocalResult::Ambiguous(dt, _) => return Some(dt.timestamp()),
                        LocalResult::Single(dt) => {
                            let back = Local.timestamp_opt(dt.timestamp(), 0).earliest();
                            if back.is_some_and(|back| back.naive_local() == candidate) {
                                return Some(dt.timestamp());
                            }
                        }
                        LocalResult::None => {}
                    }
                    candidate = candidate.checked_add_signed(Duration::minutes(1))?;
                }
                None
            }
        }
    }

    fn week_start_of(&self, now: i64) -> Option<NaiveDate> {
        let local = self.local_datetime(now)?;
        let days = local.weekday().num_days_from_sunday();
        local
            .date_naive()
            .checked_sub_days(Days::new(u64::from(days)))
    }

    fn at(&self, week: NaiveDate, weekday: u8, hour: u8, minute: u8) -> Option<NaiveDateTime> {
        if weekday > 6 || hour > 23 || minute > 59 {
            return None;
        }
        let date = week.checked_add_days(Days::new(u64::from(weekday)))?;
        date.and_hms_opt(u32::from(hour), u32::from(minute), 0)
    }
}

/// The configuration switch that turns the inferred calendar on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CalendarSettings {
    pub enabled: bool,
    pub timezone: CalendarTimeZone,
}

impl CalendarSettings {
    /// The explicit `Housing.SiegeCalendar` value wins; a missing key follows the era profile.
    pub fn from_config(config: &HousingConfig, profile: ServerProfile) -> Self {
        Self {
            enabled: config.siege_calendar.unwrap_or_else(|| profile.sieges()),
            timezone: CalendarTimeZone::parse(&config.siege_calendar_timezone).unwrap_or_default(),
        }
    }

    /// Whether `Housing.SiegeCalendarTimezone` supplied the resolved offset. `false` means the
    /// setting is empty or unusable, so [`CalendarTimeZone::default`] (UTC) applies. Used by
    /// the one-time startup notice; the same resolved timezone also drives the territory claim
    /// window (`claim_window_at`), not only the weekly calendar.
    pub fn timezone_from_config(config: &HousingConfig) -> bool {
        !config.siege_calendar_timezone.trim().is_empty()
            && CalendarTimeZone::parse(&config.siege_calendar_timezone).is_some()
    }
}

/// One weekly cycle. All times are Unix seconds in the configured timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cycle {
    /// Local Sunday that starts the plan week.
    pub week: NaiveDate,
    pub auction_start: i64,
    pub declare_start: i64,
    pub warmup_start: i64,
    pub siege_start: i64,
    pub siege_end: i64,
    /// The next cycle's auction start; the peace period runs until then.
    pub peace_end: i64,
    /// The following rotation week (peace_end comes from its auction start).
    pub next_week: NaiveDate,
    /// Wire durations: [auction, declare, warmup, siege, peace].
    pub durations: [i32; 5],
}

/// The period a snapshot describes, with the timers the client derives its countdown from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodView {
    pub period: u8,
    /// `DominionSiegeTimers.started`: the described cycle's auction start (in peace the next
    /// cycle's).
    pub started: i64,
    pub durations: [i32; 5],
    /// The cycle whose current period this is; `None` while waiting for the next cycle.
    pub cycle: Option<Cycle>,
}

/// The authored `siege_zones.open_*` claim window at one point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimWindow {
    /// No schedule row, or `open_duration_hours == 0`: the territory never opens. 1.2 ships
    /// zero-hour rows for zone groups 54 and 56; `DOMINION_CLOSED` (662) describes them
    /// ("not yet a region where a territory can be proclaimed", ui_texts 5165).
    Closed,
    /// The recurring window exists but is not active at this instant. `DOMINION_NOT_DECLARE_TIME`
    /// (582) is the matching client text, parameterized by the window columns
    /// ("$1: you can proclaim a territory from $2 weekday $3 o'clock for $4 hours",
    /// ui_texts 4662).
    NotOpen,
    Open,
}

/// The loaded schedules plus the deduplicated plan rotation.
#[derive(Debug, Clone, Default)]
pub struct SiegeCalendar {
    zones: BTreeMap<u32, SiegeZone>,
    rotation: SiegeRotation,
    siege_delay_after_claim_days: i64,
}

impl SiegeCalendar {
    pub fn from_castles(castles: &CastleReferenceData) -> Self {
        Self {
            zones: castles
                .siege_zones
                .iter()
                .map(|(zone, schedule)| (*zone, *schedule))
                .collect(),
            rotation: castles.siege_rotation(),
            siege_delay_after_claim_days: castles.siege_delay_after_claim_days,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.zones.is_empty() || self.rotation.is_empty()
    }

    /// `world_var_defaults.var_delay_siege_x_days_after_dominion_declare` (0 = no protection).
    pub fn siege_delay_after_claim_days(&self) -> i64 {
        self.siege_delay_after_claim_days
    }

    /// Whether this zone has a cycle in the week starting on `week`.
    pub fn has_cycle(&self, zone_group_id: u32, week: NaiveDate) -> bool {
        self.rotation.has_cycle(zone_group_id, week)
    }

    /// Whether an unowned territory may be claimed at `now` according to
    /// `siege_zones.open_weekday/open_hour/open_duration_hours`.
    ///
    /// The 1.2 client has no claim validator; it only maps the server's error id to text, and
    /// the text for 582 is parameterized by exactly these columns (`FUN_3956b860` case
    /// `0x246` in `x2game.dll.c`), so the columns are the claim window. Weekday 0 = Sunday is
    /// the same INFERRED convention as the rest of the table (`siege_plans` weeks start on
    /// Sunday). The window is independent of [`CalendarSettings::enabled`]: the switch drives
    /// the inferred weekly siege cycle, not the authored claim-availability columns.
    ///
    /// `open_duration_hours == 0` never opens (`DOMINION_CLOSED`); 168 h or more covers the
    /// whole week and is always open. Shorter windows are local wall-clock intervals: the end
    /// is `start + N hours` in local time and then converted, so a DST change inside the
    /// window keeps the authored local end. A start that falls into a spring-forward gap moves
    /// to the first valid local instant.
    pub fn claim_window_at(
        &self,
        zone_group_id: u32,
        now: i64,
        timezone: &CalendarTimeZone,
    ) -> ClaimWindow {
        let Some(zone) = self.zones.get(&zone_group_id) else {
            return ClaimWindow::Closed;
        };
        let hours = i64::from(zone.open_duration_hours);
        if hours <= 0 {
            return ClaimWindow::Closed;
        }
        // A full week covers every instant; no need to scan weeks or touch DST.
        if hours >= 168 {
            return ClaimWindow::Open;
        }
        let Some(week) = timezone.week_start_of(now) else {
            return ClaimWindow::Closed;
        };
        // A window shorter than a week starts in the current or previous local week.
        for back in 0..=1 {
            let Ok(days) = u64::try_from(back * 7) else {
                continue;
            };
            let Some(candidate) = week.checked_sub_days(Days::new(days)) else {
                continue;
            };
            let Some(start_naive) = timezone.at(candidate, zone.open_weekday, zone.open_hour, 0)
            else {
                continue;
            };
            let Some(end_naive) = start_naive.checked_add_signed(Duration::hours(hours)) else {
                continue;
            };
            let Some(start) = timezone.claim_timestamp(start_naive) else {
                continue;
            };
            let Some(end) = timezone.claim_timestamp(end_naive) else {
                continue;
            };
            if now >= start && now < end {
                return ClaimWindow::Open;
            }
        }
        ClaimWindow::NotOpen
    }

    /// The cycle anchored on `week`, or `None` when the zone is not in that rotation week.
    pub fn cycle_for_week(
        &self,
        zone_group_id: u32,
        week: NaiveDate,
        timezone: &CalendarTimeZone,
    ) -> Option<Cycle> {
        let zone = self.zones.get(&zone_group_id)?;
        if !self.rotation.has_cycle(zone_group_id, week) {
            return None;
        }
        // INFERRED: the plan week starts on Sunday, and the authored warmup/siege weekday 0
        // is the Sunday that *ends* that plan week (a lower weekday rolls into the next week),
        // keeping the authored order auction Wed -> declare Fri -> warmup/siege Sun. The other
        // reading (same-day Sunday) would move every siege one week earlier and put warmup
        // before the auction; the shipped schedule only fits the roll-forward reading.
        let mut stamps = [
            timezone.at(
                week,
                zone.start_auction_weekday,
                zone.start_auction_hour,
                zone.start_auction_min,
            )?,
            timezone.at(
                week,
                zone.start_declare_weekday,
                zone.start_declare_hour,
                zone.start_declare_min,
            )?,
            timezone.at(
                week,
                zone.start_warmup_weekday,
                zone.start_warmup_hour,
                zone.start_warmup_min,
            )?,
            timezone.at(
                week,
                zone.start_siege_weekday,
                zone.start_siege_hour,
                zone.start_siege_min,
            )?,
        ];
        for index in 1..stamps.len() {
            while stamps[index] <= stamps[index - 1] {
                stamps[index] = stamps[index].checked_add_days(Days::new(7))?;
            }
        }
        let auction_start = timezone.timestamp(stamps[0])?;
        let declare_start = timezone.timestamp(stamps[1])?;
        let warmup_start = timezone.timestamp(stamps[2])?;
        let siege_start = timezone.timestamp(stamps[3])?;
        let siege_length = i64::from(zone.siege_days) * 86_400
            + i64::from(zone.siege_hours) * 3_600
            + i64::from(zone.siege_mins) * 60;
        let siege_end = siege_start.checked_add(siege_length)?;
        if !(auction_start < declare_start
            && declare_start < warmup_start
            && warmup_start < siege_start
            && siege_end > siege_start)
        {
            return None;
        }
        let next_week = self.next_cycle_week(zone_group_id, week)?;
        let next_auction = timezone.timestamp(timezone.at(
            next_week,
            zone.start_auction_weekday,
            zone.start_auction_hour,
            zone.start_auction_min,
        )?)?;
        let peace_end = next_auction;
        if peace_end <= siege_end {
            return None;
        }
        let durations = [
            i32::try_from(declare_start - auction_start).ok()?,
            i32::try_from(warmup_start - declare_start).ok()?,
            i32::try_from(siege_start - warmup_start).ok()?,
            i32::try_from(siege_end - siege_start).ok()?,
            i32::try_from(peace_end - siege_end).ok()?,
        ];
        Some(Cycle {
            week,
            auction_start,
            declare_start,
            warmup_start,
            siege_start,
            siege_end,
            peace_end,
            next_week,
            durations,
        })
    }

    fn next_cycle_week(&self, zone_group_id: u32, after: NaiveDate) -> Option<NaiveDate> {
        let mut week = after.checked_add_days(Days::new(7))?;
        for _ in 0..=MAX_SEARCH_WEEKS {
            if self.rotation.has_cycle(zone_group_id, week) {
                return Some(week);
            }
            week = week.checked_add_days(Days::new(7))?;
        }
        None
    }

    /// The period at `now`, or `None` when the zone has no schedule/rotation.
    ///
    /// `declared` is the existence of an accepted attacker for this cycle. Without one the
    /// declare period ends directly in peace (`FUN_392f7db0` "unearned_win"), skipping the
    /// warmup and siege periods; with one the authored order 2 -> 3 -> 4 -> 5 applies. The
    /// explicit GM/declaration siege record owns its own timers, so this module is called
    /// with `declared = false` and the caller's explicit siege path covers the other case.
    ///
    /// Known limits: the schedule is rebuilt from the data on every call (cache later); a
    /// returned `None` means the whole zone has no usable view (a missing next cycle in a
    /// rotation, or an authored time that does not exist in a DST gap — the shipped
    /// 20:30..22:30 times never hit one); and without a declared attacker the calendar never
    /// enters warmup/siege on its own.
    pub fn view_at(
        &self,
        zone_group_id: u32,
        now: i64,
        timezone: &CalendarTimeZone,
        declared: bool,
    ) -> Option<PeriodView> {
        self.view_at_protected(zone_group_id, now, timezone, declared, 0)
    }

    /// [`view_at`] with the fresh-claim siege delay
    /// (`world_var_defaults.var_delay_siege_x_days_after_dominion_declare`).
    ///
    /// While `now < siege_not_before` the territory gets no cycle at all: the view is peace and
    /// its next period start is the first rotation cycle whose auction starts at or after
    /// `siege_not_before` (INFERRED: a freshly claimed dominion cannot be sieged for 30 days).
    /// `0` disables the protection. The forward search is bounded like the normal next-cycle
    /// search, so a delay longer than the search window drops the view instead of lying.
    pub fn view_at_protected(
        &self,
        zone_group_id: u32,
        now: i64,
        timezone: &CalendarTimeZone,
        declared: bool,
        siege_not_before: i64,
    ) -> Option<PeriodView> {
        if siege_not_before > now {
            return self.protected_view(zone_group_id, now, timezone, siege_not_before);
        }
        let week = timezone.week_start_of(now)?;
        // The most recent cycle still running (its peace may span several rotation weeks).
        for back in 0..=MAX_SEARCH_WEEKS {
            let candidate = week.checked_sub_days(Days::new(u64::try_from(back * 7).ok()?))?;
            let Some(cycle) = self.cycle_for_week(zone_group_id, candidate, timezone) else {
                continue;
            };
            if now < cycle.auction_start || now >= cycle.peace_end {
                continue;
            }
            if !declared && now >= cycle.warmup_start {
                // Nobody declared: the declare period ends in peace, and the client shows the
                // next cycle's auction as the next period start.
                let next = self.cycle_for_week(zone_group_id, cycle.next_week, timezone)?;
                return Some(PeriodView {
                    period: PERIOD_PEACE,
                    started: next.auction_start,
                    durations: next.durations,
                    cycle: Some(cycle),
                });
            }
            let (period, started, durations) = if now < cycle.declare_start {
                (PERIOD_AUCTION, cycle.auction_start, cycle.durations)
            } else if now < cycle.warmup_start {
                (PERIOD_DECLARE, cycle.auction_start, cycle.durations)
            } else if now < cycle.siege_start {
                (PERIOD_WARMUP, cycle.auction_start, cycle.durations)
            } else if now < cycle.siege_end {
                (PERIOD_SIEGE, cycle.auction_start, cycle.durations)
            } else {
                // Peace describes the next cycle: its auction start is the next period start
                // (`FUN_3946be90` case 5 -> start(1)), and the durations are the next cycle's.
                let next = self.cycle_for_week(zone_group_id, cycle.next_week, timezone)?;
                (PERIOD_PEACE, next.auction_start, next.durations)
            };
            return Some(PeriodView {
                period,
                started,
                durations,
                cycle: Some(cycle),
            });
        }
        // No running cycle: peace waiting for the next rotation week.
        for forward in 0..=MAX_SEARCH_WEEKS {
            let candidate = week.checked_add_days(Days::new(u64::try_from(forward * 7).ok()?))?;
            let Some(cycle) = self.cycle_for_week(zone_group_id, candidate, timezone) else {
                continue;
            };
            if cycle.auction_start > now {
                return Some(PeriodView {
                    period: PERIOD_PEACE,
                    started: cycle.auction_start,
                    durations: cycle.durations,
                    cycle: None,
                });
            }
        }
        None
    }

    /// The calendar view for a territory that may carry a declaration (WP-SD2a).
    ///
    /// A declaration belongs to exactly one cycle. When it matches the running cycle the
    /// declared walk (2 -> 3 -> 4 -> 5) applies; a stale declaration from an earlier cycle
    /// changes nothing and the undeclared walk is used.
    pub fn view_for_declaration(
        &self,
        zone_group_id: u32,
        now: i64,
        timezone: &CalendarTimeZone,
        declared_cycle: Option<i64>,
        siege_not_before: i64,
    ) -> Option<PeriodView> {
        if let Some(cycle) = declared_cycle {
            if let Some(view) =
                self.view_at_protected(zone_group_id, now, timezone, true, siege_not_before)
            {
                if view.cycle.is_some_and(|view| view.auction_start == cycle) {
                    return Some(view);
                }
            }
        }
        self.view_at_protected(zone_group_id, now, timezone, false, siege_not_before)
    }

    /// Peace until the first rotation cycle at or after `siege_not_before`.
    fn protected_view(
        &self,
        zone_group_id: u32,
        now: i64,
        timezone: &CalendarTimeZone,
        siege_not_before: i64,
    ) -> Option<PeriodView> {
        let week = timezone.week_start_of(now)?;
        for forward in 0..=MAX_SEARCH_WEEKS {
            let candidate = week.checked_add_days(Days::new(u64::try_from(forward * 7).ok()?))?;
            let Some(cycle) = self.cycle_for_week(zone_group_id, candidate, timezone) else {
                continue;
            };
            if cycle.auction_start >= siege_not_before {
                return Some(PeriodView {
                    period: PERIOD_PEACE,
                    started: cycle.auction_start,
                    durations: cycle.durations,
                    cycle: None,
                });
            }
        }
        None
    }
}
#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
