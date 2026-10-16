//! `Core/Managers/TimedRewardsManager` + `Models/Tasks/TimedRewards/TimedRewardsTask`: labor,
//! credits and loyalty ticks for online connections (every minute), the daily login reward and
//! the offline labor granted on login.

use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{NaiveDateTime, Utc};
use futures::future::BoxFuture;

use super::accounts::{self, TickKind};
use super::tasks::{GameTask, TaskState};
use crate::net::connection::AccountPayment;
use crate::server::GameServer;
use crate::systems::skills::packets_special::{SCBmPointPacket, SCICSCashPointPacket};
use crate::systems::skills::progress::SCCharacterLaborPowerChangedPacket;

/// `MaxLabor`.
pub const MAX_LABOR: i16 = 2000;
/// `MaxLaborPremium`.
pub const MAX_LABOR_PREMIUM: i16 = 5000;

/// `GetMaxLabor(isPremium)`.
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
    premium: bool,
    connection_id: u32,
    add_labor: i32,
) {
    accounts::update_tick_times(server, account_id, now(), TickKind::Labor).await;
    let _transaction = server.save.transaction_lock().await;
    if let Some(p) = server.world.character_by_connection(connection_id) {
        let add = {
            let mut c = p.data();
            let max_labor_to_add =
                (i32::from(get_max_labor(premium)) - i32::from(c.labor_power)).max(0);
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
        accounts::add_labor_capped(server, account_id, add_labor, get_max_labor(premium)).await;
    }
}

/// `DoTick()`: labor / credits / loyalty for every connection.
///
/// Deviation: connections without an account (not entered yet) are skipped; the C#
/// `GetAccountDetails(0)` would create an `accounts` row with id 0 for them.
pub async fn do_tick(server: &GameServer) {
    let config = &server.config;
    for handle in server.connections.all() {
        let account_id = handle.account_id();
        if account_id == 0 {
            continue;
        }
        let premium = accounts::connection_payment().premium_state(Utc::now().timestamp());
        let active_char = server.world.character_by_connection(handle.id);
        let details = accounts::get_account_details(server, account_id).await;
        let due = |last: Option<NaiveDateTime>, minutes: i32| {
            last.unwrap_or_default_min() + chrono::Duration::minutes(i64::from(minutes)) <= now()
        };

        // Distribute Labor if needed (only for online labor)
        if config.labor.tick_minutes > 0 && due(details.last_labor_tick, config.labor.tick_minutes)
        {
            let add = config.labor.get_tick_amount(premium);
            do_add_labor(server, account_id, premium, handle.id, add).await;
        }

        // Distribute Credits if needed
        if config.credits.tick_minutes > 0
            && due(details.last_credits_tick, config.credits.tick_minutes)
        {
            accounts::add_credits(server, account_id, config.credits.get_tick_amount(premium))
                .await;
            accounts::update_tick_times(server, account_id, now(), TickKind::Credits).await;
            if let Some(p) = &active_char {
                let credits = accounts::get_account_details(server, account_id)
                    .await
                    .credits;
                p.send(SCICSCashPointPacket { point: credits });
            }
        }

        // Distribute Loyalty if needed
        if config.loyalty.tick_minutes > 0
            && due(details.last_loyalty_tick, config.loyalty.tick_minutes)
        {
            accounts::add_loyalty(server, account_id, config.loyalty.get_tick_amount(premium))
                .await;
            accounts::update_tick_times(server, account_id, now(), TickKind::Loyalty).await;
            if let Some(p) = &active_char {
                let loyalty = accounts::get_account_details(server, account_id)
                    .await
                    .loyalty;
                p.send(SCBmPointPacket {
                    bm_point: i64::from(loyalty),
                });
            }
        }
    }
}

/// `DoDailyAccountLogin(accountId)`.
pub async fn do_daily_account_login(server: &GameServer, account_id: u32) {
    let config = &server.config;
    if config.credits.daily_login > 0 {
        accounts::add_credits(server, account_id, config.credits.daily_login).await;
    }
    if config.loyalty.daily_login > 0 {
        accounts::add_loyalty(server, account_id, config.loyalty.daily_login).await;
    }
    accounts::update_divine_clock(server, account_id, 0, 0).await;
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
    payment: &AccountPayment,
    last_login: Option<NaiveDateTime>,
    connection_id: u32,
) {
    let config = &server.config.labor_offline;
    let ticks_to_add = offline_ticks(last_login, now(), config.tick_minutes);
    if ticks_to_add <= 0 {
        return;
    }
    let premium = payment.premium_state(Utc::now().timestamp());
    let add_labor = config.get_tick_amount(premium).wrapping_mul(ticks_to_add);
    // `connection.ActiveChar` is always null here (called from `X2EnterWorld`).
    do_add_labor(server, account_id, premium, connection_id, add_labor).await;
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
    fn default_payment_is_premium_until_2030() {
        let p = AccountPayment::default();
        assert!(p.premium_state(1_800_000_000));
        assert!(!p.premium_state(1_900_000_000));
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
        do_add_labor(&server, 1001, false, p.conn.id, 10).await;
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
        do_add_labor(&server, 1001, false, p.conn.id, 10).await;
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
        let mut out = crate::systems::economy::inventory::Out::Direct;
        crate::systems::economy::inventory::change_labor(&mut out, &server, &p, -100, 0);
        assert_eq!(p.data().labor_power, 900);
        do_add_labor(&server, 1001, false, p.conn.id, 10).await;
        assert_eq!(
            p.data().labor_power,
            910,
            "regenerated labor must be added to the spent balance"
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
        let mut tick = Box::pin(do_add_labor(&server, 1001, false, client.conn.id, 10));
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
}
