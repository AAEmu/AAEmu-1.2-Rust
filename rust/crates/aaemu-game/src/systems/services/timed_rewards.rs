//! `Core/Managers/TimedRewardsManager` + `Models/Tasks/TimedRewards/TimedRewardsTask`: labor,
//! credits and loyalty ticks for online connections (every minute), the daily login reward and
//! the offline labor granted on login.

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{NaiveDateTime, Timelike, Utc};
use futures::future::BoxFuture;

use super::accounts::{self, TickKind};
use super::premium;
use super::tasks::{GameTask, TaskState};
use crate::db::{AccountDetails, LoyaltyHourState};
use crate::managers::account::AccountState;
use crate::server::GameServer;
use crate::systems::skills::packets_special::SCICSCashPointPacket;
use crate::systems::skills::progress::SCCharacterLaborPowerChangedPacket;

/// `MaxLabor`.
pub const MAX_LABOR: i16 = 2000;
/// `MaxLaborPremium`.
pub const MAX_LABOR_PREMIUM: i16 = 5000;

/// `GetMaxLabor(isPremium)`: the C# constants, now only the fallback when
/// `premium_benefits` is empty ([`premium::max_labor`] reads the table).
pub fn get_max_labor(is_premium: bool) -> i16 {
    if is_premium {
        MAX_LABOR_PREMIUM
    } else {
        MAX_LABOR
    }
}

/// `TimedRewardsTask`.
pub struct TimedRewardsTask {
    state: TaskState,
    server: Weak<GameServer>,
}

impl TimedRewardsTask {
    pub fn new(server: &Arc<GameServer>) -> Arc<Self> {
        Arc::new(Self {
            state: TaskState::new("TimedRewardsTask"),
            server: Arc::downgrade(server),
        })
    }
}

impl GameTask for TimedRewardsTask {
    fn state(&self) -> &TaskState {
        &self.state
    }

    fn execute(&self) {
        if let (Some(server), Ok(h)) =
            (self.server.upgrade(), tokio::runtime::Handle::try_current())
        {
            h.spawn(async move { do_tick(&server).await });
        }
    }

    fn execute_async(self: Arc<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(server) = self.server.upgrade() {
                do_tick(&server).await;
            }
        })
    }
}

/// `Initialize()`: `TimedRewardsTask` every minute (first run after one minute).
pub fn initialize(server: &Arc<GameServer>) {
    let minute = Duration::from_secs(60);
    server.systems.services.tasks.schedule(
        TimedRewardsTask::new(server),
        Some(minute),
        Some(minute),
        -1,
    );
}

fn now() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// `DoAddLabor(connection, currentLabor, addLabor)`.
///
/// Deviation from C#: labor is account-level (`accounts.labor`), but only the offline path writes
/// it directly (atomically, capped). The `transaction_lock` serializes with `select_character`
/// (which loads the character and registers it in the world under the lock), so a connection
/// between those two points can never be mistaken for an offline account. With a live character
/// the add is applied to the character's cached labor and the delta is flushed into
/// `accounts.labor` by the next `save_character`, so regeneration always builds on what was spent
/// in play. Without one, a departing sibling's unflushed labor is written first, so the cap
/// applies to the true account balance.
async fn do_add_labor(
    server: &GameServer,
    account_id: u32,
    max_labor: i16,
    connection_id: u32,
    add_labor: i32,
) {
    accounts::update_tick_times(server, account_id, now(), TickKind::Labor).await;
    let _transaction = server.save.transaction_lock().await;
    if let Some(p) = server.world.character_by_connection(connection_id) {
        let add = {
            let mut c = p.data();
            let max_labor_to_add = (i32::from(max_labor) - i32::from(c.labor_power)).max(0);
            let add = add_labor.min(max_labor_to_add);
            if add > 0 {
                c.change_labor_power(add as i16);
            }
            add
        };
        if add > 0 {
            p.send(SCCharacterLaborPowerChangedPacket {
                amount: add,
                action: 0,
                point: 0,
                step: 0,
            });
        }
    } else if add_labor > 0 {
        if !server
            .save
            .flush_account_labor_locked(server, account_id)
            .await
        {
            tracing::warn!(
                "Account {account_id}: pending character labor save, skipping offline labor"
            );
            return;
        }
        // Offline labor (login / character select): no character cache to update.
        accounts::add_labor_capped(server, account_id, add_labor, max_labor).await;
    }
}

/// A sample gap longer than this is not play time: the account was offline or the server was
/// stopped. The minute tick samples every 60 s, so two minutes leaves room for a late tick while
/// never crediting a relog or restart gap.
pub const LOYALTY_SAMPLE_GAP_SECONDS: i64 = 120;

/// Seconds in one UTC day (`loyalty_hour_day` is the unix day number).
const SECONDS_PER_UTC_DAY: i64 = 86_400;

/// The result of one hourly-Loyalty sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourlyLoyaltyUpdate {
    /// The counters to store.
    pub state: LoyaltyHourState,
    /// Tokens to grant now (`0` when the interval or the daily maximum is not reached).
    pub grant: i32,
}

/// The EU 1.2 hourly Loyalty rule (owner decision 2026-11-09): every full `HourlyMinutes` of
/// in-world play in the current UTC day grants `HourlyAmount` tokens, at most `HourlyDailyMax`
/// per account and day. Play time is per account, carries across relogs and is reset by the UTC
/// day boundary; a gap above [`LOYALTY_SAMPLE_GAP_SECONDS`] is not credited. The rule is
/// independent of Patron / premium state (no payment input at all).
pub fn hourly_loyalty_update(
    current: LoyaltyHourState,
    now: NaiveDateTime,
    config: &crate::config::CurrencyValuesConfig,
) -> HourlyLoyaltyUpdate {
    // `loyalty_hour_updated` is a DATETIME(0): sample on whole seconds so the stored value and
    // the elapsed time computed from it agree (a fractional sample lost ~0.5-1 s per tick).
    let now = now.with_nanosecond(0).unwrap_or(now);
    let interval_seconds = i64::from(config.hourly_minutes) * 60;
    if config.hourly_amount <= 0 || config.hourly_daily_max <= 0 || interval_seconds <= 0 {
        return HourlyLoyaltyUpdate {
            state: current,
            grant: 0,
        };
    }
    let day = now.and_utc().timestamp().div_euclid(SECONDS_PER_UTC_DAY) as i32;
    if current.day != day {
        // First sample of a new UTC day: yesterday's play time and tokens are gone.
        return HourlyLoyaltyUpdate {
            state: LoyaltyHourState {
                day,
                seconds: 0,
                taken: 0,
                updated: Some(now),
            },
            grant: 0,
        };
    }
    let elapsed = current.updated.map_or(0, |last| (now - last).num_seconds());
    let added = if (0..=LOYALTY_SAMPLE_GAP_SECONDS).contains(&elapsed) {
        elapsed
    } else {
        0
    };
    let seconds = current.seconds.max(0).saturating_add(added);
    let periods = seconds / interval_seconds;
    let due = periods
        .saturating_mul(i64::from(config.hourly_amount))
        .min(i64::from(config.hourly_daily_max)) as i32;
    let taken = current.taken.max(0);
    let grant = (due - taken).max(0);
    HourlyLoyaltyUpdate {
        state: LoyaltyHourState {
            day,
            seconds,
            taken: taken.saturating_add(grant),
            updated: Some(now),
        },
        grant,
    }
}

/// One hourly-Loyalty sample of an in-world account: stores the counters and, when a token is
/// due, credits it and pushes `SCBmPoint` (0x46) plus the `msg_gain_bmmileage` line to every
/// online character of the account. The counter write is conditional on the sampled snapshot, so
/// overlapping ticks credit once; a failed or stale write credits nothing and the next tick
/// retries from the state it then reads.
async fn tick_hourly_loyalty(
    server: &GameServer,
    account_id: u32,
    details: &AccountDetails,
    at: NaiveDateTime,
) {
    let current = details.loyalty_hour_state();
    let update = hourly_loyalty_update(current, at, &server.config.loyalty);
    if update.state == current {
        return;
    }
    if update.grant > 0 {
        if let Some(total) =
            accounts::credit_hourly_loyalty(server, account_id, update.grant, current, update.state)
                .await
        {
            accounts::notify_loyalty_grant(server, account_id, update.grant, total);
        }
    } else {
        accounts::store_loyalty_hour_state(server, account_id, current, update.state).await;
    }
}

/// `DoTick()`: labor / credits / loyalty for every connection.
///
/// Deviation: connections without an account (not entered yet) are skipped; the C#
/// `GetAccountDetails(0)` would create an `accounts` row with id 0 for them.
pub async fn do_tick(server: &GameServer) {
    do_tick_at(server, now()).await;
}

/// [`do_tick`] with an explicit sample time (tests pin it away from the UTC day boundary).
pub async fn do_tick_at(server: &GameServer, at: NaiveDateTime) {
    let config = &server.config;
    // The hourly Loyalty counters belong to the account, not the connection: an account with a
    // second connection (refused by `AccountManager.Add`, but possible in fixtures) is sampled
    // once so the same minute can never be counted twice.
    let mut loyalty_sampled: HashSet<u32> = HashSet::new();
    for handle in server.connections.all() {
        let account_id = handle.account_id();
        if account_id == 0 {
            continue;
        }
        // WP-B: the payment / premium state lives in the account registry (the task only has a
        // `ConnectionHandle`), and the exact max labor comes from `premium_benefits`.
        let state = accounts::expire_lazily(server, account_id).await;
        let premium = state.payment.premium_state(Utc::now().timestamp());
        let max_labor = premium::max_labor_for_state(server, &state);
        let active_char = server.world.character_by_connection(handle.id);
        let details = accounts::get_account_details(server, account_id).await;
        let due = |last: Option<NaiveDateTime>, minutes: i32| {
            last.unwrap_or_default_min() + chrono::Duration::minutes(i64::from(minutes)) <= at
        };

        // Distribute Labor if needed (only for online labor)
        if config.labor.tick_minutes > 0 && due(details.last_labor_tick, config.labor.tick_minutes)
        {
            let add = config.labor.get_tick_amount(premium);
            do_add_labor(server, account_id, max_labor, handle.id, add).await;
        }

        // Distribute Credits if needed
        if config.credits.tick_minutes > 0
            && due(details.last_credits_tick, config.credits.tick_minutes)
        {
            accounts::add_credits(server, account_id, config.credits.get_tick_amount(premium))
                .await;
            accounts::update_tick_times(server, account_id, at, TickKind::Credits).await;
            if let Some(p) = &active_char {
                let credits = accounts::get_account_details(server, account_id)
                    .await
                    .credits;
                p.send(SCICSCashPointPacket { point: credits });
            }
        }

        // Hourly Loyalty: in-world play time only, per account (see `hourly_loyalty_update`).
        if active_char.is_some() && loyalty_sampled.insert(account_id) {
            tick_hourly_loyalty(server, account_id, &details, at).await;
        }
    }
}

/// `DoDailyAccountLogin(accountId)`: the once-per-UTC-day credits / loyalty grant.
///
/// The grant is claimed atomically on `accounts.last_daily_reward`, so the first login of the
/// day and the midnight quest reset of an account that is already online cannot both pay: an
/// account online across 00:00 UTC is paid by the reset, and its next login sees the day as
/// already claimed.
pub async fn do_daily_account_login(server: &GameServer, account_id: u32) {
    do_daily_account_login_on(server, account_id, now().date()).await;
}

/// [`do_daily_account_login`] for an explicit UTC day. The midnight reset passes the day it was
/// scheduled for, so a wake a few milliseconds early (wall clock still on the previous day) still
/// pays the new day; the login path passes the current wall-clock day.
pub async fn do_daily_account_login_on(
    server: &GameServer,
    account_id: u32,
    day: chrono::NaiveDate,
) {
    let config = &server.config;
    let today = day.and_hms_opt(0, 0, 0).expect("midnight");
    let Some((_credits, loyalty)) = accounts::claim_daily_reward(
        server,
        account_id,
        today,
        config.credits.daily_login,
        config.loyalty.daily_login,
    )
    .await
    else {
        return;
    };
    if config.loyalty.daily_login > 0 {
        // No character is online on the login path; at the midnight reset this reaches every
        // online character of the account (stale `bm_point`, research §3).
        accounts::notify_loyalty_grant(server, account_id, config.loyalty.daily_login, loyalty);
    }
}

/// The number of `LaborOffline` ticks since `last_login` (`(int)Math.Floor(minutes /
/// TickMinutes)`, saturating like the .NET 9 conversion; `None` = `DateTime.MinValue`).
pub fn offline_ticks(
    last_login: Option<NaiveDateTime>,
    now: NaiveDateTime,
    tick_minutes: i32,
) -> i32 {
    let delta = now - last_login.unwrap_or_default_min();
    let minutes = delta.num_milliseconds() as f64 / 60_000.0;
    (minutes / f64::from(tick_minutes)).floor() as i32
}

/// `AddOfflineLabor(connection, lastLoginTime, currentLabor)` (the connection has no active
/// character yet when `AccountManager.Add` runs).
pub async fn add_offline_labor(
    server: &GameServer,
    account_id: u32,
    state: &AccountState,
    last_login: Option<NaiveDateTime>,
    connection_id: u32,
) {
    let config = &server.config.labor_offline;
    let ticks_to_add = offline_ticks(last_login, now(), config.tick_minutes);
    if ticks_to_add <= 0 {
        return;
    }
    let premium = state.payment.premium_state(Utc::now().timestamp());
    let add_labor = config.get_tick_amount(premium).wrapping_mul(ticks_to_add);
    let max_labor = premium::max_labor_for_state(server, state);
    // `connection.ActiveChar` is always null here (called from `X2EnterWorld`).
    do_add_labor(server, account_id, max_labor, connection_id, add_labor).await;
}

/// `DateTime.MinValue` for unset DATETIME columns.
trait OrMinValue {
    fn unwrap_or_default_min(self) -> NaiveDateTime;
}

impl OrMinValue for Option<NaiveDateTime> {
    fn unwrap_or_default_min(self) -> NaiveDateTime {
        self.unwrap_or_else(crate::db::min_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{spawn_player, test_character, test_server, TestClient};
    use crate::world::PlayerRef;

    fn at(h: u32, m: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 1, 2)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    #[test]
    fn max_labor() {
        assert_eq!(get_max_labor(true), 5000);
        assert_eq!(get_max_labor(false), 2000);
    }

    #[test]
    fn offline_tick_count() {
        assert_eq!(offline_ticks(Some(at(10, 0)), at(10, 4), 5), 0);
        assert_eq!(offline_ticks(Some(at(10, 0)), at(10, 5), 5), 1);
        assert_eq!(offline_ticks(Some(at(10, 0)), at(11, 2), 5), 12);
        // Never logged in: saturates.
        assert!(offline_ticks(None, at(10, 0), 5) > 200_000_000);
        // TickMinutes 0: +Infinity -> int.MaxValue.
        assert_eq!(offline_ticks(Some(at(10, 0)), at(10, 1), 0), i32::MAX);
    }

    #[test]
    fn default_payment_is_permanent_premium() {
        let p = crate::net::connection::AccountPayment::default();
        assert!(p.premium_state(1_800_000_000));
        assert!(p.premium_state(4_000_000_000), "no end date");
    }

    /// A spawned non-premium player with `labor` memory labor on account 1001 (the account the
    /// `do_add_labor` calls below address; its DB writes fail against the closed test pool).
    fn labor_player(server: &GameServer, labor: i16) -> (TestClient, PlayerRef) {
        let mut client = TestClient::new(server, 1, 1001);
        let mut c = test_character(1, "Laborer", 500.0, 500.0);
        c.labor_power = labor;
        c.is_online = true;
        let p = spawn_player(server, &client, c, 0x100 + 1);
        client.conn.active_char = Some(p.clone());
        client.drain();
        (client, p)
    }

    #[tokio::test]
    async fn regeneration_adds_to_spent_balance() {
        let server = test_server();
        let (_client, p) = labor_player(&server, 1000);
        crate::systems::skills::progress::change_labor(&server, &p, -100, 0);
        assert_eq!(p.data().labor_power, 900);
        // The database row still reads the pre-spend 1000 while the player is online.
        do_add_labor(&server, 1001, MAX_LABOR, p.conn.id, 10).await;
        assert_eq!(
            p.data().labor_power,
            910,
            "regenerated labor must be added to the spent balance"
        );
    }

    #[tokio::test]
    async fn regeneration_works_after_spending_from_cap() {
        let server = test_server();
        let (_client, p) = labor_player(&server, MAX_LABOR);
        crate::systems::skills::progress::change_labor(&server, &p, -100, 0);
        assert_eq!(p.data().labor_power, 1900);
        // The database row is still at the cap.
        do_add_labor(&server, 1001, MAX_LABOR, p.conn.id, 10).await;
        assert_eq!(
            p.data().labor_power,
            1910,
            "spending from the cap must make room for regeneration"
        );
    }

    #[tokio::test]
    async fn economy_labor_spend_also_keeps_regeneration() {
        let server = test_server();
        let (_client, p) = labor_player(&server, 1000);
        let mut out = crate::systems::economy::inventory::Out::Direct(&server);
        crate::systems::economy::inventory::change_labor(&mut out, &server, &p, -100, 0);
        assert_eq!(p.data().labor_power, 900);
        do_add_labor(&server, 1001, MAX_LABOR, p.conn.id, 10).await;
        assert_eq!(
            p.data().labor_power,
            910,
            "regenerated labor must be added to the spent balance"
        );
    }

    #[tokio::test]
    async fn labor_tick_uses_the_account_state_not_the_default() {
        use crate::managers::account::AccountState;
        use crate::net::connection::{AccountPayment, PaymentMethodType};

        let server = test_server();
        server.config.update(|c| {
            c.labor.tick_minutes = 5;
            c.labor.tick_amount = 5;
            c.labor.tick_amount_premium = 10;
            c.credits.tick_minutes = 0;
            c.loyalty.tick_minutes = 0;
        });
        let (_client, p) = labor_player(&server, 1998);
        server.accounts.set_state(
            1001,
            AccountState {
                payment: AccountPayment {
                    method: PaymentMethodType::None,
                    location: 1,
                    start_time: 0,
                    end_time: 0,
                },
                premium_point: 0,
            },
        );
        do_tick(&server).await;
        assert_eq!(
            p.data().labor_power,
            2000,
            "a normal account is capped at the non-premium max labor (1998 + 5 would be 2003)"
        );

        p.data().labor_power = 4995;
        server.accounts.set_state(
            1001,
            AccountState {
                payment: AccountPayment {
                    method: PaymentMethodType::Premium,
                    location: 1,
                    start_time: 0,
                    end_time: 1_893_456_000,
                },
                premium_point: 1,
            },
        );
        do_tick(&server).await;
        assert_eq!(
            p.data().labor_power,
            5000,
            "a premium account regenerates to the premium max labor"
        );
    }

    /// Regression: a tick running while `select_character` holds the transaction lock (after the
    /// character load, before the world registration) must not take the offline branch; it has to
    /// wait for the lock and then reach the freshly registered cache.
    #[tokio::test]
    async fn regeneration_during_character_selection_reaches_the_cache() {
        let server = test_server();
        let mut client = TestClient::new(&server, 1, 1001);
        let mut c = test_character(1, "Laborer", 500.0, 500.0);
        c.labor_power = 1000;
        c.is_online = true;
        // During `select_character` the lock spans the load and the world registration.
        let transaction = server.save.transaction_lock().await;
        let mut tick = Box::pin(do_add_labor(&server, 1001, MAX_LABOR, client.conn.id, 10));
        // The tick first fails `update_tick_times` against the closed test pool, which needs up
        // to one 200 ms acquire timeout (two on the unguarded mutant path); 1 s leaves room for
        // that before the tick can reach the lock and block on it for the rest of the window.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut tick)
                .await
                .is_err(),
            "the tick must block on the selection transaction lock"
        );
        let p = spawn_player(&server, &client, c, 0x100 + 1);
        client.conn.active_char = Some(p.clone());
        client.drain();
        drop(transaction);
        tick.await;
        assert_eq!(
            p.data().labor_power,
            1010,
            "labor regenerated during character selection must reach the loaded cache"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Hourly Loyalty (EU 1.2 owner rule, 2026-11-09)
    // -----------------------------------------------------------------------------------------

    fn day_key(now: NaiveDateTime) -> i32 {
        now.and_utc().timestamp().div_euclid(86_400) as i32
    }

    fn day_at(day_offset: i64, h: u32, m: u32) -> NaiveDateTime {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()
            + chrono::Duration::days(day_offset);
        day.and_hms_opt(h, m, 0).unwrap()
    }

    fn hourly_config(minutes: i32) -> crate::config::CurrencyValuesConfig {
        crate::config::CurrencyValuesConfig {
            tick_minutes: 0,
            hourly_amount: 1,
            hourly_minutes: minutes,
            hourly_daily_max: 3,
            ..Default::default()
        }
    }

    #[test]
    fn hourly_loyalty_grants_one_per_full_interval_and_stops_at_the_daily_max() {
        let config = hourly_config(1);
        let start = day_at(0, 10, 0);
        let mut state = LoyaltyHourState {
            day: day_key(start),
            seconds: 0,
            taken: 0,
            updated: Some(start),
        };
        let mut granted = 0;
        for minute in 1..=4 {
            let update =
                hourly_loyalty_update(state, start + chrono::Duration::minutes(minute), &config);
            state = update.state;
            granted += update.grant;
        }
        assert_eq!(granted, 3, "four samples grant three tokens and stop");
        assert_eq!(state.taken, 3);
        assert_eq!(state.seconds, 240, "play time keeps counting past the cap");
    }

    #[test]
    fn hourly_loyalty_samples_on_whole_seconds_like_the_datetime_column() {
        let config = hourly_config(60);
        let start = day_at(0, 10, 0);
        let state = LoyaltyHourState {
            day: day_key(start),
            seconds: 0,
            taken: 0,
            updated: Some(start),
        };
        let at = start + chrono::Duration::milliseconds(60_700);
        let update = hourly_loyalty_update(state, at, &config);
        assert_eq!(
            update.state.updated,
            Some(start + chrono::Duration::seconds(60))
        );
        assert_eq!(update.state.seconds, 60);
        // The next tick (another 60.7 s) is measured from the stored whole second.
        let next = hourly_loyalty_update(
            update.state,
            at + chrono::Duration::milliseconds(60_700),
            &config,
        );
        assert_eq!(next.state.seconds, 121);
        assert_eq!(
            next.state.updated,
            Some(start + chrono::Duration::seconds(121))
        );
    }

    #[test]
    fn hourly_loyalty_carries_play_time_across_a_relog_without_crediting_offline_time() {
        let config = hourly_config(60);
        let start = day_at(0, 10, 0);
        // 58 minutes 20 seconds already played today.
        let state = LoyaltyHourState {
            day: day_key(start),
            seconds: 3500,
            taken: 0,
            updated: Some(start),
        };
        // A normal tick: 60 more seconds, still under the hour.
        let update = hourly_loyalty_update(state, start + chrono::Duration::minutes(1), &config);
        assert_eq!(update.grant, 0);
        assert_eq!(update.state.seconds, 3560);

        // Relog four hours later: the gap is not play time, but the 3560 s persist.
        let after_relog = start + chrono::Duration::hours(4);
        let update = hourly_loyalty_update(update.state, after_relog, &config);
        assert_eq!(update.grant, 0);
        assert_eq!(update.state.seconds, 3560, "play time survives the relog");
        assert_eq!(update.state.updated, Some(after_relog));

        // 40 s of the next session complete the hour: one token.
        let update = hourly_loyalty_update(
            update.state,
            after_relog + chrono::Duration::seconds(40),
            &config,
        );
        assert_eq!(update.grant, 1);
        assert_eq!(update.state.taken, 1);
    }

    #[test]
    fn hourly_loyalty_resets_at_the_utc_day_boundary() {
        let config = hourly_config(1);
        let before_midnight = day_at(0, 23, 59);
        let state = LoyaltyHourState {
            day: day_key(before_midnight),
            seconds: 59,
            taken: 2,
            updated: Some(before_midnight),
        };
        let just_after = day_at(1, 0, 0) + chrono::Duration::seconds(30);
        let update = hourly_loyalty_update(state, just_after, &config);
        assert_eq!(update.grant, 0, "the boundary sample does not pay");
        assert_eq!(update.state.day, day_key(just_after));
        assert_eq!(update.state.seconds, 0);
        assert_eq!(update.state.taken, 0);

        // The new day starts counting from that sample.
        let update = hourly_loyalty_update(
            update.state,
            just_after + chrono::Duration::minutes(1),
            &config,
        );
        assert_eq!(update.grant, 1);
    }

    #[test]
    fn hourly_loyalty_is_off_when_the_interval_or_amount_is_zero() {
        let start = day_at(0, 10, 0);
        let state = LoyaltyHourState {
            day: day_key(start),
            seconds: 300,
            taken: 0,
            updated: Some(start),
        };
        for config in [
            crate::config::CurrencyValuesConfig {
                hourly_amount: 0,
                hourly_minutes: 60,
                hourly_daily_max: 3,
                ..Default::default()
            },
            crate::config::CurrencyValuesConfig {
                hourly_amount: 1,
                hourly_minutes: 0,
                hourly_daily_max: 3,
                ..Default::default()
            },
            crate::config::CurrencyValuesConfig {
                hourly_amount: 1,
                hourly_minutes: 60,
                hourly_daily_max: 0,
                ..Default::default()
            },
        ] {
            let update = hourly_loyalty_update(state, start + chrono::Duration::hours(2), &config);
            assert_eq!(update.grant, 0);
            assert_eq!(update.state, state, "a disabled rule writes nothing");
        }
    }

    /// The private MySQL pool the gated tests below use; the `accounts` table is a temporary
    /// table on the one pooled connection, so the lane database is never touched.
    async fn private_accounts_pool() -> Option<sqlx::MySqlPool> {
        use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
        use std::str::FromStr;

        let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
            return None;
        };
        let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
        assert_eq!(
            options.get_port(),
            3317,
            "Only private MySQL3317 is allowed"
        );
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("private MySQL");
        sqlx::query(
            "CREATE TEMPORARY TABLE accounts ( \
             account_id INT NOT NULL PRIMARY KEY, access_level INT NOT NULL DEFAULT 0, \
             labor INT NOT NULL DEFAULT 0, credits INT NOT NULL DEFAULT 0, \
             loyalty INT NOT NULL DEFAULT 0, \
             last_updated DATETIME NOT NULL DEFAULT '2026-01-01 00:00:00', \
             last_login DATETIME NOT NULL DEFAULT '2026-01-01 00:00:00', \
             last_labor_tick DATETIME NOT NULL DEFAULT '2026-01-01 00:00:00', \
             last_credits_tick DATETIME NOT NULL DEFAULT '2026-01-01 00:00:00', \
             last_loyalty_tick DATETIME NOT NULL DEFAULT '2026-01-01 00:00:00', \
             last_daily_reward DATETIME NULL DEFAULT NULL, \
             loyalty_hour_day INT NOT NULL DEFAULT 0, \
             loyalty_hour_seconds INT NOT NULL DEFAULT 0, \
             loyalty_hour_taken INT NOT NULL DEFAULT 0, \
             loyalty_hour_updated DATETIME NULL DEFAULT NULL, \
             divine_clock_time INT UNSIGNED NOT NULL DEFAULT 0, \
             divine_clock_taken INT UNSIGNED NOT NULL DEFAULT 0, \
             extra_character_slots TINYINT UNSIGNED NOT NULL DEFAULT 0, \
             pay_method TINYINT NOT NULL DEFAULT 0, pay_location INT NOT NULL DEFAULT 1, \
             pay_start BIGINT NOT NULL DEFAULT 0, pay_end BIGINT NOT NULL DEFAULT 0, \
             premium_point INT NOT NULL DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .expect("temporary accounts table");
        Some(pool)
    }

    fn loyalty_player(server: &GameServer) -> (TestClient, PlayerRef) {
        let mut client = TestClient::new(server, 1, 1001);
        let mut c = test_character(1, "Loyal", 500.0, 500.0);
        c.is_online = true;
        let p = spawn_player(server, &client, c, 0x100 + 1);
        client.conn.active_char = Some(p.clone());
        client.drain();
        (client, p)
    }

    fn non_premium(server: &GameServer) {
        use crate::managers::account::AccountState;
        use crate::net::connection::{AccountPayment, PaymentMethodType};
        server.accounts.set_state(
            1001,
            AccountState {
                payment: AccountPayment {
                    method: PaymentMethodType::None,
                    location: 1,
                    start_time: 0,
                    end_time: 0,
                },
                premium_point: 0,
            },
        );
    }

    /// The minute tick credits the hourly token for a **non-premium** account (no Patron rule)
    /// and stops at `HourlyDailyMax`. The sample time is pinned away from the UTC day boundary, so
    /// the test cannot flip days between two calls.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
    async fn hourly_loyalty_tick_grants_without_premium_and_stops_at_the_cap() {
        let Some(pool) = private_accounts_pool().await else {
            return;
        };
        let mut server = test_server();
        server.db = pool.clone();
        server.config.update(|c| {
            c.labor.tick_minutes = 0;
            c.credits.tick_minutes = 0;
            c.loyalty.hourly_amount = 1;
            c.loyalty.hourly_minutes = 1;
            c.loyalty.hourly_daily_max = 3;
        });
        let (mut client, p) = loyalty_player(&server);
        non_premium(&server);
        let pinned = day_at(0, 12, 0);
        sqlx::query(
            "INSERT INTO accounts (account_id, loyalty_hour_day, loyalty_hour_seconds, \
             loyalty_hour_taken, loyalty_hour_updated) \
             VALUES (1001, ?, 0, 0, ?)",
        )
        .bind(day_key(pinned))
        .bind(pinned - chrono::Duration::seconds(60))
        .execute(&pool)
        .await
        .unwrap();

        for expected in 1..=3 {
            do_tick_at(&server, pinned + chrono::Duration::minutes(expected - 1)).await;
            let total: i64 = sqlx::query_scalar(
                "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = 1001",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(total, expected, "grant {expected} of the hourly rule");
            assert_eq!(p.data().bm_point, expected);
        }

        // Fourth full interval: the daily maximum already paid three.
        do_tick_at(&server, pinned + chrono::Duration::minutes(3)).await;
        let total: i64 = sqlx::query_scalar(
            "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = 1001",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 3, "three is the daily maximum");

        let frames = client.drain();
        assert_eq!(
            frames
                .iter()
                .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCBmPointPacket)
                .count(),
            3,
            "one SCBmPoint per granted token"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCChatMessagePacket)
                .count(),
            3,
            "one msg_gain_bmmileage line per granted token"
        );
        assert!(frames
            .iter()
            .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCChatMessagePacket)
            .any(|(_, body)| String::from_utf8_lossy(body).contains("Loyalty Token")));
    }

    /// An account online across 00:00 UTC is paid by the midnight reset; its next login that day
    /// must not pay again (the `last_daily_reward` claim). The day is pinned, so the test cannot
    /// flip days between its two calls.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
    async fn daily_login_reward_is_granted_once_across_a_midnight() {
        let Some(pool) = private_accounts_pool().await else {
            return;
        };
        let mut server = test_server();
        server.db = pool.clone();
        server.config.update(|c| {
            c.credits.daily_login = 0;
            c.loyalty.daily_login = 5;
        });
        let (mut client, p) = loyalty_player(&server);
        let day = day_at(0, 12, 0).date();
        sqlx::query(
            "INSERT INTO accounts (account_id, loyalty, last_daily_reward) \
             VALUES (1001, 0, ?)",
        )
        .bind(day.and_hms_opt(0, 0, 0).unwrap() - chrono::Duration::days(1))
        .execute(&pool)
        .await
        .unwrap();

        // The midnight quest reset of the account that stayed online.
        do_daily_account_login_on(&server, 1001, day).await;
        let total: i64 = sqlx::query_scalar(
            "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = 1001",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 5);
        assert_eq!(p.data().bm_point, 5, "the online character is pushed");

        // Its next login on the same UTC day (the reported double grant).
        do_daily_account_login_on(&server, 1001, day).await;
        let total: i64 = sqlx::query_scalar(
            "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = 1001",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 5, "the daily 5 is paid once per UTC day");

        let frames = client.drain();
        assert_eq!(
            frames
                .iter()
                .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCBmPointPacket)
                .count(),
            1,
            "only the midnight grant reaches the online character"
        );
    }

    /// The midnight reset passes the day it was scheduled for: a wake before the wall clock
    /// reaches midnight still pays the new day, even though `today` is already claimed.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
    async fn daily_login_reward_uses_the_reset_target_day() {
        let Some(pool) = private_accounts_pool().await else {
            return;
        };
        let mut server = test_server();
        server.db = pool.clone();
        server.config.update(|c| {
            c.credits.daily_login = 0;
            c.loyalty.daily_login = 5;
        });
        let _ = loyalty_player(&server);
        // Wall-clock "today", captured once; the reset's target is the next day. The function
        // uses the passed day, so a midnight crossing cannot make this flaky.
        let today = chrono::Utc::now().date_naive();
        let tomorrow = today + chrono::Duration::days(1);
        sqlx::query(
            "INSERT INTO accounts (account_id, loyalty, last_daily_reward) VALUES (1001, 0, ?)",
        )
        .bind(today.and_hms_opt(0, 0, 0).unwrap())
        .execute(&pool)
        .await
        .unwrap();
        do_daily_account_login_on(&server, 1001, tomorrow).await;
        let total: i64 = sqlx::query_scalar(
            "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = 1001",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            total, 5,
            "the target day is claimed, not the wall-clock day"
        );
        let stored: Option<chrono::NaiveDateTime> =
            sqlx::query_scalar("SELECT last_daily_reward FROM accounts WHERE account_id = 1001")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, tomorrow.and_hms_opt(0, 0, 0));
    }

    /// Two ticks that computed a grant from the same snapshot must pay once: the counter write
    /// is conditional on that snapshot.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
    async fn hourly_loyalty_overlapping_ticks_pay_once() {
        let Some(pool) = private_accounts_pool().await else {
            return;
        };
        let pinned = day_at(0, 12, 0);
        let previous = LoyaltyHourState {
            day: day_key(pinned),
            seconds: 0,
            taken: 0,
            updated: Some(pinned),
        };
        let next = LoyaltyHourState {
            day: day_key(pinned),
            seconds: 60,
            taken: 1,
            updated: Some(pinned + chrono::Duration::minutes(1)),
        };
        sqlx::query(
            "INSERT INTO accounts (account_id, loyalty, loyalty_hour_day, \
             loyalty_hour_seconds, loyalty_hour_taken, loyalty_hour_updated) \
             VALUES (1001, 0, ?, 0, 0, ?)",
        )
        .bind(previous.day)
        .bind(previous.updated)
        .execute(&pool)
        .await
        .unwrap();

        let (a, b) = tokio::join!(
            crate::db::credit_hourly_loyalty(&pool, 1001, 1, previous, next),
            crate::db::credit_hourly_loyalty(&pool, 1001, 1, previous, next)
        );
        let paid = [a.unwrap(), b.unwrap()];
        assert_eq!(
            paid.iter().flatten().count(),
            1,
            "one of the two same-snapshot ticks credits"
        );
        let total: i64 = sqlx::query_scalar(
            "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = 1001",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 1, "the same hour is paid once");
        let state: (i64, i64) = sqlx::query_as(
            "SELECT CAST(loyalty_hour_seconds AS SIGNED), CAST(loyalty_hour_taken AS SIGNED) \
             FROM accounts WHERE account_id = 1001",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, (60, 1));
    }
}
