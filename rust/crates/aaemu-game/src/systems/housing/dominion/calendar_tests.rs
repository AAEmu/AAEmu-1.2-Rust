use super::*;
use crate::systems::housing::data::{CastleReferenceData, SiegePlan, SiegeZone};
use chrono::Timelike;

const ZONE: u32 = 33;

fn timezone() -> CalendarTimeZone {
    CalendarTimeZone::Fixed(FixedOffset::east_opt(9 * 3600).unwrap())
}

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).unwrap()
}

fn epoch(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    timezone()
        .timestamp(date(year, month, day).and_hms_opt(hour, minute, 0).unwrap())
        .unwrap()
}

/// The shipped `siege_zones` row for zone group 33 (all six rows share these columns).
fn schedule() -> SiegeZone {
    SiegeZone {
        id: 4,
        zone_group_id: ZONE,
        start_siege_weekday: 0,
        start_siege_hour: 21,
        start_siege_min: 0,
        siege_days: 0,
        siege_hours: 1,
        siege_mins: 30,
        start_auction_weekday: 3,
        start_auction_hour: 22,
        start_auction_min: 30,
        start_declare_weekday: 5,
        start_declare_hour: 22,
        start_declare_min: 30,
        start_warmup_weekday: 0,
        start_warmup_hour: 20,
        start_warmup_min: 30,
        pay_weekday: 0,
        pay_hour: 23,
        pay_min: 55,
        reinforce_defense_delay_mins: 20,
        open_weekday: 0,
        open_hour: 0,
        open_duration_hours: 168,
        ..Default::default()
    }
}

fn calendar(weeks: &[NaiveDate]) -> SiegeCalendar {
    let mut castles = CastleReferenceData::default();
    castles.siege_zones.insert(ZONE, schedule());
    let rows = castles.siege_plans.entry(ZONE).or_default();
    for (id, week) in weeks.iter().enumerate() {
        // Duplicate rows are part of the shipped table shape.
        for copy in 0..2 {
            rows.push(SiegePlan {
                id: id as u32 * 10 + copy,
                zone_group_id: ZONE,
                week_start: week.and_hms_opt(0, 0, 0).unwrap(),
            });
        }
    }
    SiegeCalendar::from_castles(&castles)
}

/// A zone in all three rotation slots runs a cycle every week.
fn weekly_calendar() -> SiegeCalendar {
    calendar(&[date(2014, 1, 5), date(2014, 1, 12), date(2014, 1, 19)])
}

fn view(calendar: &SiegeCalendar, now: i64, declared: bool) -> PeriodView {
    calendar
        .view_at(ZONE, now, &timezone(), declared)
        .expect("period view")
}

#[test]
fn timezone_rejects_non_ascii_without_panicking() {
    for v in ["+é", "+1é", "+1éé", "+ééé", "-１２３４", "+0é:00"] {
        assert_eq!(CalendarTimeZone::parse(v), None, "{v}");
    }
}

#[test]
fn timezone_parses_offsets_and_defaults_to_utc() {
    let utc = CalendarTimeZone::Fixed(FixedOffset::east_opt(0).unwrap());
    assert_eq!(CalendarTimeZone::default(), utc);
    assert_eq!(CalendarTimeZone::parse(""), Some(utc));
    assert_eq!(CalendarTimeZone::parse("  "), Some(utc));
    assert_eq!(
        CalendarTimeZone::parse("local"),
        Some(CalendarTimeZone::Local)
    );
    assert_eq!(
        CalendarTimeZone::parse("UTC"),
        Some(CalendarTimeZone::Fixed(FixedOffset::east_opt(0).unwrap()))
    );
    assert_eq!(
        CalendarTimeZone::parse("+09:00"),
        Some(CalendarTimeZone::Fixed(
            FixedOffset::east_opt(9 * 3600).unwrap()
        ))
    );
    assert_eq!(
        CalendarTimeZone::parse("-0530"),
        Some(CalendarTimeZone::Fixed(
            FixedOffset::east_opt(-(5 * 3600 + 30 * 60)).unwrap()
        ))
    );
    assert_eq!(
        CalendarTimeZone::parse("+9"),
        Some(CalendarTimeZone::Fixed(
            FixedOffset::east_opt(9 * 3600).unwrap()
        ))
    );
    assert_eq!(
        CalendarTimeZone::parse("+9:30"),
        Some(CalendarTimeZone::Fixed(
            FixedOffset::east_opt(9 * 3600 + 30 * 60).unwrap()
        ))
    );
    assert_eq!(
        CalendarTimeZone::parse("-5:30"),
        Some(CalendarTimeZone::Fixed(
            FixedOffset::east_opt(-(5 * 3600 + 30 * 60)).unwrap()
        ))
    );
    assert_eq!(CalendarTimeZone::parse("Asia/Seoul"), None);
    assert_eq!(CalendarTimeZone::parse("+24:00"), None);
    assert_eq!(CalendarTimeZone::parse("+09:60"), None);
    assert_eq!(CalendarTimeZone::parse("nonsense"), None);
}

#[test]
fn timezone_display_and_the_config_source_are_loggable() {
    assert_eq!(CalendarTimeZone::default().to_string(), "UTC+00:00");
    assert_eq!(
        CalendarTimeZone::Fixed(FixedOffset::east_opt(9 * 3600).unwrap()).to_string(),
        "UTC+09:00"
    );
    assert_eq!(
        CalendarTimeZone::Fixed(FixedOffset::east_opt(-(5 * 3600 + 30 * 60)).unwrap()).to_string(),
        "UTC-05:30"
    );
    assert_eq!(CalendarTimeZone::Local.to_string(), "server local time");

    // Empty (the default) and unusable values fall back to UTC; an explicit value is reported
    // as coming from the configuration.
    assert!(!CalendarSettings::timezone_from_config(
        &HousingConfig::default()
    ));
    assert!(!CalendarSettings::timezone_from_config(&HousingConfig {
        siege_calendar_timezone: "local time".into(),
        ..Default::default()
    }));
    for value in ["UTC", "z", "+09:00", "-0530", "local"] {
        assert!(
            CalendarSettings::timezone_from_config(&HousingConfig {
                siege_calendar_timezone: value.into(),
                ..Default::default()
            }),
            "{value}"
        );
    }
}

#[test]
fn weekly_cycle_crosses_every_period_boundary_and_the_week_boundary() {
    let calendar = weekly_calendar();
    let auction = epoch(2014, 1, 8, 22, 30);
    let declare = epoch(2014, 1, 10, 22, 30);
    let warmup = epoch(2014, 1, 12, 20, 30);
    let siege = epoch(2014, 1, 12, 21, 0);
    let siege_end = epoch(2014, 1, 12, 22, 30);
    let next_auction = epoch(2014, 1, 15, 22, 30);
    let durations = [48 * 3600, 46 * 3600, 30 * 60, 90 * 60, 72 * 3600];

    let cycle = calendar
        .cycle_for_week(ZONE, date(2014, 1, 5), &timezone())
        .unwrap();
    assert_eq!(cycle.auction_start, auction);
    assert_eq!(cycle.declare_start, declare);
    assert_eq!(cycle.warmup_start, warmup);
    assert_eq!(cycle.siege_start, siege);
    assert_eq!(cycle.siege_end, siege_end);
    assert_eq!(cycle.peace_end, next_auction);
    assert_eq!(cycle.durations, durations);
    // The five durations add up to the weekly plan period.
    assert_eq!(durations.iter().sum::<i32>(), 7 * 86_400);

    // Peace of the previous cycle ends exactly at the auction start.
    let before = view(&calendar, auction - 1, false);
    assert_eq!((before.period, before.started), (PERIOD_PEACE, auction));
    let auction_view = view(&calendar, auction, false);
    assert_eq!((auction_view.period, auction_view.started), (1, auction));
    assert_eq!(auction_view.durations, durations);
    assert_eq!(view(&calendar, declare - 1, false).period, 1);
    assert_eq!(view(&calendar, declare, false).period, 2);
    // No declaration: the declare period ends directly in peace (client "unearned_win").
    let undeclared = view(&calendar, warmup, false);
    assert_eq!(undeclared.period, PERIOD_PEACE);
    assert_eq!(undeclared.started, next_auction);
    assert_eq!(undeclared.durations, durations);
    assert_eq!(view(&calendar, warmup - 1, true).period, 2);
    assert_eq!(view(&calendar, warmup, true).period, 3);
    assert_eq!(view(&calendar, siege - 1, true).period, 3);
    assert_eq!(view(&calendar, siege, true).period, 4);
    assert_eq!(view(&calendar, siege_end - 1, true).period, 4);
    // Peace carries the next cycle's auction start and durations.
    let peace = view(&calendar, siege_end, true);
    assert_eq!(peace.period, PERIOD_PEACE);
    assert_eq!(peace.started, next_auction);
    assert_eq!(peace.durations, durations);
    assert_eq!(
        view(&calendar, next_auction - 1, false).started,
        next_auction
    );
    let next = view(&calendar, next_auction, false);
    assert_eq!((next.period, next.started), (1, next_auction));
}

#[test]
fn a_declaration_of_the_running_cycle_keeps_the_warmup_and_siege_walk() {
    let calendar = weekly_calendar();
    let auction = epoch(2014, 1, 8, 22, 30);
    let warmup = epoch(2014, 1, 12, 20, 30);
    let siege = epoch(2014, 1, 12, 21, 0);
    let siege_end = epoch(2014, 1, 12, 22, 30);
    let declared = Some(auction);
    let view = |now: i64, cycle: Option<i64>| {
        calendar
            .view_for_declaration(ZONE, now, &timezone(), cycle, 0)
            .expect("period view")
    };
    assert_eq!(view(warmup, declared).period, PERIOD_WARMUP);
    assert_eq!(view(siege, declared).period, PERIOD_SIEGE);
    assert_eq!(view(siege_end, declared).period, PERIOD_PEACE);
    // A declaration from another cycle changes nothing: the undeclared walk applies.
    let stale = Some(auction - 7 * 86_400);
    assert_eq!(view(warmup, stale).period, PERIOD_PEACE);
    assert_eq!(view(warmup, None).period, PERIOD_PEACE);
}

#[test]
fn peace_waits_for_the_next_rotation_week_and_uses_its_timers() {
    // Zone 33 is listed in the first and fourth week only (the three-week rotation).
    let calendar = calendar(&[date(2014, 1, 5), date(2014, 1, 26)]);
    let first_end = epoch(2014, 1, 12, 22, 30);
    let next_auction = epoch(2014, 1, 29, 22, 30);
    let peace = view(&calendar, first_end + 60, false);
    assert_eq!(peace.period, PERIOD_PEACE);
    assert_eq!(peace.started, next_auction);
    // Three weeks of plan time: the next cycle's peace runs until the one after it.
    assert_eq!(
        peace.durations.iter().map(|d| i64::from(*d)).sum::<i64>(),
        21 * 86_400
    );
    // A week later this zone has no cycle of its own: still peace, same next start.
    let idle = view(&calendar, epoch(2014, 1, 19, 12, 0), false);
    assert_eq!(idle.period, PERIOD_PEACE);
    assert_eq!(idle.started, next_auction);
    assert_eq!(idle.durations, peace.durations);
    // The next rotation week starts a real cycle.
    let started = view(&calendar, next_auction, false);
    assert_eq!(
        (started.period, started.started),
        (PERIOD_AUCTION, next_auction)
    );
}

#[test]
fn plan_rotation_dedupes_duplicate_rows_and_wraps_after_the_table_ends() {
    let mut castles = CastleReferenceData::default();
    for zone in [33, 43, 54] {
        castles.siege_zones.insert(
            zone,
            SiegeZone {
                zone_group_id: zone,
                ..schedule()
            },
        );
    }
    let rows = [
        (33, date(2014, 1, 5)),
        (43, date(2014, 1, 12)),
        (54, date(2014, 1, 19)),
    ];
    let mut id = 1;
    for (zone, week) in rows {
        // Every listed week carries each zone twice in the shipped table.
        for _ in 0..2 {
            castles
                .siege_plans
                .entry(zone)
                .or_default()
                .push(SiegePlan {
                    id,
                    zone_group_id: zone,
                    week_start: week.and_hms_opt(0, 0, 0).unwrap(),
                });
            id += 1;
        }
    }
    let rotation = castles.siege_rotation();
    assert!(rotation.has_cycle(33, date(2014, 1, 5)));
    assert!(!rotation.has_cycle(43, date(2014, 1, 5)));
    assert!(rotation.has_cycle(43, date(2014, 1, 12)));
    assert!(rotation.has_cycle(54, date(2014, 1, 19)));
    assert!(rotation.has_cycle(33, date(2014, 1, 26)));
    assert_eq!(rotation.week(date(2014, 1, 5)).unwrap().len(), 1);
    // Week 52 wraps the three-week rotation past the 2014 table.
    assert!(rotation.has_cycle(33, date(2014, 12, 28)));
    assert!(rotation.has_cycle(43, date(2015, 1, 4)));
    assert!(rotation.has_cycle(54, date(2015, 1, 11)));
    assert!(rotation.has_cycle(33, date(2015, 1, 18)));
    assert!(!rotation.has_cycle(33, date(2014, 1, 12)));
}

#[test]
fn disabled_calendar_is_reported_by_the_settings_and_invalid_schedules_are_rejected() {
    let empty = SiegeCalendar::default();
    assert!(empty.is_empty());
    assert!(empty.view_at(ZONE, 0, &timezone(), false).is_none());

    let mut castles = CastleReferenceData::default();
    castles.siege_zones.insert(
        ZONE,
        SiegeZone {
            siege_days: 0,
            siege_hours: 0,
            siege_mins: 0,
            ..schedule()
        },
    );
    castles
        .siege_plans
        .entry(ZONE)
        .or_default()
        .push(SiegePlan {
            id: 1,
            zone_group_id: ZONE,
            week_start: date(2014, 1, 5).and_hms_opt(0, 0, 0).unwrap(),
        });
    let invalid = SiegeCalendar::from_castles(&castles);
    // A zero-length siege is malformed data, not a schedule to guess around.
    assert!(invalid
        .cycle_for_week(ZONE, date(2014, 1, 5), &timezone())
        .is_none());

    let settings = CalendarSettings::from_config(&HousingConfig {
        siege_calendar: true,
        siege_calendar_timezone: "+09:00".into(),
        ..Default::default()
    });
    assert!(settings.enabled);
    assert_eq!(settings.timezone, timezone());
    let default = CalendarSettings::from_config(&HousingConfig::default());
    assert!(!default.enabled);
    // The config default (empty string) is UTC, not the server's local offset.
    assert_eq!(
        default.timezone,
        CalendarTimeZone::Fixed(FixedOffset::east_opt(0).unwrap())
    );
    // An explicit `local` still selects the server's local offset.
    assert_eq!(
        CalendarSettings::from_config(&HousingConfig {
            siege_calendar_timezone: "local".into(),
            ..Default::default()
        })
        .timezone,
        CalendarTimeZone::Local
    );
}

#[test]
fn claim_window_uses_the_authored_open_weekday_hour_and_duration() {
    let weekly = weekly_calendar();
    // The shipped row is Sunday 00:00 for 168 h, so every instant is inside the window.
    for now in [
        epoch(2014, 1, 5, 0, 0),
        epoch(2014, 1, 8, 12, 0),
        epoch(2014, 1, 11, 23, 59),
    ] {
        assert_eq!(
            weekly.claim_window_at(ZONE, now, &timezone()),
            ClaimWindow::Open
        );
    }

    let mut castles = CastleReferenceData::default();
    castles.siege_zones.insert(
        ZONE,
        SiegeZone {
            open_weekday: 4,
            open_hour: 0,
            open_duration_hours: 1,
            ..schedule()
        },
    );
    let narrow = SiegeCalendar::from_castles(&castles);
    assert_eq!(
        narrow.claim_window_at(ZONE, epoch(2014, 1, 9, 0, 30), &timezone()),
        ClaimWindow::Open
    );
    assert_eq!(
        narrow.claim_window_at(ZONE, epoch(2014, 1, 8, 23, 59), &timezone()),
        ClaimWindow::NotOpen
    );
    assert_eq!(
        narrow.claim_window_at(ZONE, epoch(2014, 1, 9, 1, 0), &timezone()),
        ClaimWindow::NotOpen
    );
    // Exactly at the opening boundary is open; exactly at the closing boundary is not.
    assert_eq!(
        narrow.claim_window_at(ZONE, epoch(2014, 1, 9, 0, 0), &timezone()),
        ClaimWindow::Open
    );

    let mut castles = CastleReferenceData::default();
    castles.siege_zones.insert(
        ZONE,
        SiegeZone {
            open_duration_hours: 0,
            ..schedule()
        },
    );
    let closed = SiegeCalendar::from_castles(&castles);
    assert_eq!(
        closed.claim_window_at(ZONE, epoch(2014, 1, 5, 0, 0), &timezone()),
        ClaimWindow::Closed
    );
    assert_eq!(
        closed.claim_window_at(999, epoch(2014, 1, 5, 0, 0), &timezone()),
        ClaimWindow::Closed
    );
}

#[test]
fn fresh_claim_protection_refuses_cycles_until_the_delay_expires() {
    // Zone 33 runs every three weeks: auctions on Jan 8, Jan 29 and Feb 19 2014.
    let calendar = calendar(&[date(2014, 1, 5), date(2014, 1, 26)]);
    let claim = epoch(2014, 1, 8, 23, 0);
    let not_before = claim + 30 * 86_400;
    let first_after = epoch(2014, 2, 19, 22, 30);

    // Without protection this is the auction period of the current cycle.
    assert_eq!(
        calendar
            .view_at(ZONE, claim + 3_600, &timezone(), false)
            .unwrap()
            .period,
        PERIOD_AUCTION
    );

    // During the delay the territory stays in peace, and its next period start is the first
    // rotation auction at or after the delay.
    for now in [claim, claim + 3_600, not_before - 1] {
        let view = calendar
            .view_at_protected(ZONE, now, &timezone(), false, not_before)
            .unwrap();
        assert_eq!(view.period, PERIOD_PEACE);
        assert_eq!(view.started, first_after);
        assert_eq!(
            view.durations,
            calendar
                .view_at(ZONE, first_after, &timezone(), false)
                .unwrap()
                .durations
        );
    }

    // The delay ends exactly at `not_before`; this is still the Feb 19 auction.
    let ended = calendar
        .view_at_protected(ZONE, not_before, &timezone(), false, not_before)
        .unwrap();
    assert_eq!((ended.period, ended.started), (PERIOD_PEACE, first_after));
    let auction = calendar
        .view_at_protected(ZONE, first_after, &timezone(), false, not_before)
        .unwrap();
    assert_eq!(
        (auction.period, auction.started),
        (PERIOD_AUCTION, first_after)
    );

    // A delay longer than the bounded search drops the view instead of claiming a cycle.
    assert!(calendar
        .view_at_protected(ZONE, claim, &timezone(), false, claim + 400 * 86_400)
        .is_none());
}

#[test]
fn claim_window_uses_local_end_and_falls_forward_across_a_dst_change() {
    // Locate a spring-forward gap in the host's local zone; zones without DST skip the test.
    // `chrono::Local` may return a `Single` value for a skipped time (Windows), so validate by
    // round-tripping the conversion instead of looking for `LocalResult::None`.
    let valid_local = |naive: NaiveDateTime| -> bool {
        match Local.from_local_datetime(&naive) {
            LocalResult::Ambiguous(_, _) => true,
            LocalResult::None => false,
            LocalResult::Single(dt) => Local
                .timestamp_opt(dt.timestamp(), 0)
                .earliest()
                .is_some_and(|back| back.naive_local() == naive),
        }
    };
    let mut gap = None;
    'search: for month in 1..=12 {
        for day in 1..=31 {
            let Some(date) = NaiveDate::from_ymd_opt(2026, month, day) else {
                continue;
            };
            for hour in 0..6 {
                let Some(naive) = date.and_hms_opt(hour, 0, 0) else {
                    continue;
                };
                if !valid_local(naive) {
                    gap = Some(naive);
                    break 'search;
                }
            }
        }
    }
    let Some(gap) = gap else {
        eprintln!("claim_window DST test skipped: the host local zone has no spring-forward gap");
        return;
    };
    let local_ts = |naive: NaiveDateTime| -> i64 {
        let mut candidate = naive;
        for _ in 0..=180 {
            match Local.from_local_datetime(&candidate) {
                LocalResult::Ambiguous(dt, _) => return dt.timestamp(),
                LocalResult::Single(dt) => {
                    if Local
                        .timestamp_opt(dt.timestamp(), 0)
                        .earliest()
                        .is_some_and(|back| back.naive_local() == candidate)
                    {
                        return dt.timestamp();
                    }
                }
                LocalResult::None => {}
            }
            candidate += Duration::minutes(1);
        }
        panic!("no valid local instant after {naive}");
    };

    // 01:00 -> 05:00 local on the gap day is three real hours, not four. The authored local
    // end wins over "start timestamp + duration seconds".
    let day = gap.date();
    let weekday = day.weekday().num_days_from_sunday() as u8;
    let start = local_ts(day.and_hms_opt(1, 0, 0).unwrap());
    let end = local_ts(day.and_hms_opt(5, 0, 0).unwrap());
    assert_eq!(end - start, 3 * 3_600, "spring-forward day loses one hour");

    let mut castles = CastleReferenceData::default();
    castles.siege_zones.insert(
        ZONE,
        SiegeZone {
            open_weekday: weekday,
            open_hour: 1,
            open_duration_hours: 4,
            ..schedule()
        },
    );
    let calendar = SiegeCalendar::from_castles(&castles);
    assert_eq!(
        calendar.claim_window_at(ZONE, start, &CalendarTimeZone::Local),
        ClaimWindow::Open
    );
    assert_eq!(
        calendar.claim_window_at(ZONE, end - 1, &CalendarTimeZone::Local),
        ClaimWindow::Open
    );
    assert_eq!(
        calendar.claim_window_at(ZONE, end, &CalendarTimeZone::Local),
        ClaimWindow::NotOpen
    );

    // A window that starts inside the skipped hour falls forward to the first valid instant;
    // a two-hour local window then ends one real hour after that instant.
    let mut castles = CastleReferenceData::default();
    castles.siege_zones.insert(
        ZONE,
        SiegeZone {
            open_weekday: weekday,
            open_hour: gap.hour() as u8,
            open_duration_hours: 2,
            ..schedule()
        },
    );
    let calendar = SiegeCalendar::from_castles(&castles);
    let fall_start = local_ts(gap);
    let window_end = local_ts(gap + Duration::hours(2));
    assert_eq!(window_end - fall_start, 3_600);
    assert_eq!(
        calendar.claim_window_at(ZONE, fall_start, &CalendarTimeZone::Local),
        ClaimWindow::Open
    );
    assert_eq!(
        calendar.claim_window_at(ZONE, window_end - 1, &CalendarTimeZone::Local),
        ClaimWindow::Open
    );
    assert_eq!(
        calendar.claim_window_at(ZONE, window_end, &CalendarTimeZone::Local),
        ClaimWindow::NotOpen
    );
}
