//! `Core/Managers/AccountManager` remainder: `Add` (login time + timed rewards), the
//! dead-connection sweep (`Initialize` -> `RemoveDeadConnections` every 30 s) and the account
//! value updates (`AddCredits`, `RemoveCredits`, `AddLoyalty`, `UpdateLabor`,
//! `UpdateTickTimes`, `UpdateDivineClock`, `GetDivineClock`) under the per-account lock.

use std::time::Duration;

use chrono::{NaiveDateTime, Utc};

use crate::db::{self, AccountDetails, AccountPointColumn};
use crate::managers::account::AccountState;
use crate::net::connection::GameConnection;
use crate::server::GameServer;
use crate::systems::services::premium;

/// `RemoveDeadConnections` interval and ping timeout.
pub const DEAD_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// `AccountManager.Add(connection)`: registers the account, updates `last_login`, claims the
/// UTC day's daily login reward and grants the offline labor. Returns `false` when the account
/// is already registered (the C# `Add` then returns early); the caller refuses the world entry.
pub async fn add(server: &GameServer, conn: &mut GameConnection) -> bool {
    let account_id = conn.account_id();
    if !server.accounts.try_add(account_id, conn.id) {
        return false;
    }
    let now = Utc::now().naive_utc();
    let last_login = update_login_time(server, account_id, now).await;
    // Load the payment / premium state (creates the `accounts` row on the first login) and cache
    // it on the connection for the lobby burst.
    let (state, _) = refresh_state_with_details(server, account_id).await;
    conn.payment = state.payment.clone();
    // The claim on `accounts.last_daily_reward` decides whether this is the first login of the
    // UTC day (a fresh account's row is created with `last_login = now`, so the old
    // `last_login < today` test skipped its very first daily reward).
    super::timed_rewards::do_daily_account_login(server, account_id).await;
    super::timed_rewards::add_offline_labor(server, account_id, &state, last_login, conn.id).await;
    true
}

/// `AccountManager.Initialize`: `RemoveDeadConnections` on the tick manager every 30 s.
pub fn initialize(server: &std::sync::Arc<GameServer>) {
    let weak = std::sync::Arc::downgrade(server);
    server.systems.services.tick.on_tick.subscribe(
        move |_| {
            if let Some(server) = weak.upgrade() {
                remove_dead_connections(&server, Utc::now().naive_utc());
            }
        },
        DEAD_CONNECTION_TIMEOUT,
        false,
    );
}

/// `RemoveDeadConnections`: shuts down every registered account connection whose `LastPing`
/// is older than 30 s. A connection that never sent a `PingPacket` has `LastPing =
/// DateTime.MinValue` in C# and is shut down too. Returns the closed connection ids.
pub fn remove_dead_connections(server: &GameServer, now: NaiveDateTime) -> Vec<u32> {
    let timeout = chrono::Duration::from_std(DEAD_CONNECTION_TIMEOUT).expect("30 s");
    let mut closed = Vec::new();
    for (_, conn_id) in server.accounts.entries() {
        let Some(handle) = server.connections.get(conn_id) else {
            continue;
        };
        let alive = handle.last_ping().is_some_and(|p| p + timeout >= now);
        if alive {
            continue;
        }
        if let Some(p) = server.world.character_by_connection(conn_id) {
            tracing::trace!(
                "Disconnecting {} due to no network activity",
                p.data().unit.name
            );
        }
        handle.close();
        closed.push(conn_id);
    }
    closed
}

/// `GetAccountDetails(accountId)` under the account lock.
pub async fn get_account_details(server: &GameServer, account_id: u32) -> AccountDetails {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::get_account_details(&server.db, &server.config, account_id).await
}

/// `UpdateLoginTime(accountId, newTime)` under the account lock: the previous `last_login`.
pub async fn update_login_time(
    server: &GameServer,
    account_id: u32,
    new_time: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::update_login_time(&server.db, &server.config, account_id, new_time).await
}

/// `AddCredits(accountId, amount)`.
pub async fn add_credits(server: &GameServer, account_id: u32, amount: i32) -> bool {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::add_account_points(&server.db, AccountPointColumn::Credits, account_id, amount).await
}

/// `RemoveCredits(accountId, credits)`.
pub async fn remove_credits(server: &GameServer, account_id: u32, credits: i32) -> bool {
    add_credits(server, account_id, credits.wrapping_neg()).await
}

/// `AddLoyalty(accountId, amount)`.
pub async fn add_loyalty(server: &GameServer, account_id: u32, amount: i32) -> bool {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::add_account_points(&server.db, AccountPointColumn::Loyalty, account_id, amount).await
}

/// Adds `amount` to `accounts.loyalty` and returns the new total (`None` = the write failed).
pub async fn add_loyalty_returning(
    server: &GameServer,
    account_id: u32,
    amount: i32,
) -> Option<i32> {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    match db::add_loyalty_returning(&server.db, account_id, amount).await {
        Ok(total) => total,
        Err(e) => {
            tracing::error!("add_loyalty_returning({account_id}): {e}");
            None
        }
    }
}

/// Claims the daily login reward of `today` (UTC midnight) and credits `credits` / `loyalty` in
/// one transaction. `None` = already paid today (or the transaction failed).
pub async fn claim_daily_reward(
    server: &GameServer,
    account_id: u32,
    today: NaiveDateTime,
    credits: i32,
    loyalty: i32,
) -> Option<(i32, i32)> {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::claim_daily_reward(&server.db, account_id, today, credits, loyalty).await
}

/// Adds `amount` to `accounts.loyalty` and stores the hourly Loyalty counters in one
/// transaction, but only while the row still holds `previous`; returns the new total
/// (`None` = the snapshot was stale or nothing was written).
pub async fn credit_hourly_loyalty(
    server: &GameServer,
    account_id: u32,
    amount: i32,
    previous: db::LoyaltyHourState,
    next: db::LoyaltyHourState,
) -> Option<i32> {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    match db::credit_hourly_loyalty(&server.db, account_id, amount, previous, next).await {
        Ok(total) => total,
        Err(e) => {
            tracing::error!("credit_hourly_loyalty({account_id}): {e}");
            None
        }
    }
}

/// Stores the hourly Loyalty counters without granting a token, only while the row still holds
/// `previous`. Returns whether the write applied.
pub async fn store_loyalty_hour_state(
    server: &GameServer,
    account_id: u32,
    previous: db::LoyaltyHourState,
    next: db::LoyaltyHourState,
) -> bool {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    match db::store_loyalty_hour_state(&server.db, account_id, previous, next).await {
        Ok(changed) => changed,
        Err(e) => {
            tracing::error!("store_loyalty_hour_state({account_id}): {e}");
            false
        }
    }
}

/// Pushes a Loyalty grant to every online character of the account: the new account total
/// (`SCBmPoint` 0x46) on each and the 1.2 `msg_gain_bmmileage` chat line
/// (`ui_texts` id 5450, localised through the loaded `ui_texts`) on each. A character of the
/// same account that was already selected would otherwise keep a stale `bm_point`.
pub fn notify_loyalty_grant(server: &GameServer, account_id: u32, amount: i32, total: i32) {
    use crate::packets::world::SCChatMessagePacket;
    use crate::systems::skills::packets_special::SCBmPointPacket;
    use crate::systems::social::chat::chat_type;

    let message = crate::systems::economy::mail_data::localized_ui_text(
        server,
        "msg_gain_bmmileage",
        "Acquired $1 Loyalty Token(s). (New Total: $2)",
    )
    .replace("$1", &amount.to_string())
    .replace("$2", &total.to_string());
    for p in server.world.all_characters() {
        if p.data().account_id != account_id {
            continue;
        }
        p.data().bm_point = i64::from(total);
        p.send(SCBmPointPacket {
            bm_point: i64::from(total),
        });
        p.send(SCChatMessagePacket {
            chat_type: chat_type::SYSTEM,
            sender: None,
            message: message.clone(),
            ability: 0,
            language_type: 0,
        });
    }
}

/// `UpdateLabor(accountId, laborPower)`.
pub async fn update_labor(server: &GameServer, account_id: u32, labor: i16) {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::update_labor(&server.db, account_id, labor).await;
}

/// Offline labor: atomically adds `add` to the account's labor, clamped to `max`, under the
/// account lock. Returns `true` when the row was updated.
pub async fn add_labor_capped(server: &GameServer, account_id: u32, add: i32, max: i16) -> bool {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    match db::add_labor_capped(&server.db, account_id, add, max).await {
        Ok(affected) => affected > 0,
        Err(e) => {
            tracing::error!("AddLaborCapped for account {account_id}: {e}");
            false
        }
    }
}

/// Which tick time [`update_tick_times`] writes (C# passes three flags; the last set wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickKind {
    Labor,
    Credits,
    Loyalty,
}

/// `UpdateTickTimes(accountId, newTime, updateLabor, updateCredits, updateLoyalty)`.
pub async fn update_tick_times(
    server: &GameServer,
    account_id: u32,
    new_time: NaiveDateTime,
    kind: TickKind,
) {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::update_tick_times(
        &server.db,
        account_id,
        new_time,
        kind == TickKind::Labor,
        kind == TickKind::Credits,
        kind == TickKind::Loyalty,
    )
    .await;
}

/// `UpdateDivineClock(accountId, timeElapsed, timesTaken)`.
pub async fn update_divine_clock(server: &GameServer, account_id: u32, time: u32, taken: u32) {
    let lock = server.accounts.lock_of(account_id);
    let _g = lock.lock().await;
    db::update_divine_clock(&server.db, account_id, time, taken).await;
}

/// Batched online-tracking write: adds `delta` seconds to each account's divine clock, holding
/// the per-account locks in ascending order (so no lock-order cycle with another batch) so it
/// cannot interleave with `update_divine_clock`. The same ascending order is the `accounts`
/// row order the periodic save uses (`db::add_divine_clock_time`).
pub async fn add_divine_clock_time(server: &GameServer, account_ids: &[u32], delta: u32) {
    let mut ids = account_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let locks: Vec<_> = ids.iter().map(|id| server.accounts.lock_of(*id)).collect();
    let mut guards = Vec::with_capacity(locks.len());
    for l in &locks {
        guards.push(l.lock().await);
    }
    db::add_divine_clock_time(&server.db, &ids, delta).await;
}

/// The cached payment / premium state of an account. Callers that only have an account id
/// (the timed-reward task, skill requirement checks) use this; a state that was never loaded
/// falls back to `Account:DefaultPayment`.
pub fn state(server: &GameServer, account_id: u32) -> AccountState {
    server
        .accounts
        .state(account_id)
        .unwrap_or_else(|| premium::default_state(server))
}

/// Loads (or reloads) `aaemu_game.accounts` under the account lock, resolves the effective
/// payment / premium point and persists a changed point. Expiry is part of the resolution: an
/// account whose premium ended is deactivated to `premium_configs.deactivate_point`, and its
/// connection receives `SCAccountInfo` + `SCUpdatePremiumPoint` after the lock is released.
pub async fn refresh_state_with_details(
    server: &GameServer,
    account_id: u32,
) -> (AccountState, AccountDetails) {
    let lock = server.accounts.lock_of(account_id);
    let guard = lock.lock().await;
    let details = match db::try_get_account_details(&server.db, &server.config, account_id).await {
        Ok(details) => details,
        Err(e) => {
            // Keep the last loaded state (or the configured default) instead of silently
            // changing a non-premium account into the default one.
            tracing::error!("{e}");
            return (state(server, account_id), AccountDetails::default());
        }
    };
    let now = Utc::now().timestamp();
    let loaded = premium::state_from_details(server, &details, now);
    if loaded.changed {
        db::update_premium_point(&server.db, account_id, loaded.state.premium_point).await;
    }
    server.accounts.set_state(account_id, loaded.state.clone());
    drop(guard);
    if let Some(old_point) = loaded.deactivated_from {
        tracing::info!(
            "Account {account_id}: premium ended, premium point {old_point} -> {}",
            loaded.state.premium_point
        );
        let old_grade = premium::data(server).grade_for_point(old_point);
        let new_grade = premium::grade_of_state(server, &loaded.state, now);
        premium::notify_account(server, account_id, &loaded.state, old_grade, new_grade);
        premium::sync_grade(server, account_id, new_grade);
    }
    (loaded.state, details)
}

/// [`refresh_state_with_details`] without the account details.
pub async fn refresh_state(server: &GameServer, account_id: u32) -> AccountState {
    refresh_state_with_details(server, account_id).await.0
}

/// Lazy expiry check for the minute tick: a cached premium state whose end passed is
/// deactivated (point + `SCAccountInfo` + `SCUpdatePremiumPoint` + buff removal). Returns the
/// current state.
pub async fn expire_lazily(server: &GameServer, account_id: u32) -> AccountState {
    let now = Utc::now().timestamp();
    let current = state(server, account_id);
    let cfg = &premium::data(server).config;
    if current.payment.premium_state(now) || current.premium_point <= cfg.deactivate_point {
        return current;
    }
    let lock = server.accounts.lock_of(account_id);
    let guard = lock.lock().await;
    // Re-check under the lock: a concurrent refresh may already have deactivated it.
    let current = state(server, account_id);
    if current.payment.premium_state(now) || current.premium_point <= cfg.deactivate_point {
        return current;
    }
    let old_point = current.premium_point;
    let point = cfg.deactivate_point.clamp(0, cfg.max_point.max(0));
    db::update_premium_point(&server.db, account_id, point).await;
    let next = AccountState {
        premium_point: point,
        ..current
    };
    server.accounts.set_state(account_id, next.clone());
    drop(guard);
    tracing::info!("Account {account_id}: premium expired, premium point {old_point} -> {point}");
    let old_grade = premium::data(server).grade_for_point(old_point);
    let new_grade = premium::data(server).grade_for_point(point);
    premium::notify_account(server, account_id, &next, old_grade, new_grade);
    premium::sync_grade(server, account_id, new_grade);
    next
}

/// The end of an account session (`AccountManager.Remove`): the premium point drops to
/// `premium_configs.disconnect_point`. The account lock is taken **before** the registry removal
/// so this transition serialises with `refresh_state` / `expire_lazily` like every other account
/// write.
pub async fn end_session(server: &GameServer, account_id: u32, connection_id: u32) {
    if account_id == 0 {
        return;
    }
    let lock = server.accounts.lock_of(account_id);
    let guard = lock.lock().await;
    let state = server.accounts.state(account_id);
    if !server.accounts.remove_if_owned(account_id, connection_id) {
        return;
    }
    let Some(state) = state else { return };
    let now = Utc::now().timestamp();
    if !state.payment.premium_state(now) {
        return;
    }
    let cfg = &premium::data(server).config;
    let point = cfg.disconnect_point.clamp(0, cfg.max_point.max(0));
    if point == state.premium_point {
        return;
    }
    db::update_premium_point(&server.db, account_id, point).await;
    drop(guard);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DefaultPayment};
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::GameData;
    use crate::net::connection::{AccountPayment, PaymentMethodType};
    use crate::systems::services::premium::{
        PcBangBuff, PremiumBenefit, PremiumConfig, PremiumGrade,
    };
    use crate::test_support::{test_server, test_server_with_config, TestClient};

    /// `premium_grades` (grade 2 at point 1) and the two benefit rows.
    fn premium_data() -> GameData {
        let mut data = GameData::empty();
        data.systems.services.premium.grades = vec![
            PremiumGrade {
                id: 1,
                grade_id: 1,
                point: 0,
            },
            PremiumGrade {
                id: 2,
                grade_id: 2,
                point: 1,
            },
        ];
        data.systems.services.premium.benefits = vec![
            PremiumBenefit {
                id: 1,
                online_labor: 5,
                offline_labor: 0,
                max_labor: 2000,
                icon_id: 0,
                grade_id: 1,
            },
            PremiumBenefit {
                id: 2,
                online_labor: 10,
                offline_labor: 5,
                max_labor: 5000,
                icon_id: 0,
                grade_id: 2,
            },
        ];
        data.systems.services.premium.config = PremiumConfig::default();
        data.systems.services.premium.pcbang_buffs = vec![PcBangBuff {
            id: 8_000_001,
            buff_id: 8_000_011,
            active: true,
            kind_id: 3,
            premium_grade_id: 2,
        }];
        data.skills
            .buffs
            .insert(8_000_011, BuffTemplate::simple(8_000_011, BuffKind::Good));
        let full = crate::systems::skills::data::templates::BuffTemplate {
            id: 8_000_011,
            kind: 1,
            ..Default::default()
        };
        data.systems
            .skills
            .buffs
            .insert(8_000_011, std::sync::Arc::new(full));
        data
    }

    #[tokio::test]
    async fn default_payment_setting_decides_the_effective_premium_state() {
        let mut config = AppConfig::default();
        config.account.default_payment = DefaultPayment::None;
        let normal = test_server_with_config(premium_data(), config);
        let effective = state(&normal, 42);
        assert_eq!(effective.payment.method, PaymentMethodType::None);
        assert_eq!(effective.premium_point, 0);
        assert_eq!(premium::max_labor(&normal, 42), 2000);

        let premium_server = test_server_with_config(premium_data(), AppConfig::default());
        let effective = state(&premium_server, 42);
        assert_eq!(effective.payment.method, PaymentMethodType::Premium);
        assert_eq!(effective.premium_point, 1);
        assert_eq!(premium::max_labor(&premium_server, 42), 5000);
    }

    #[tokio::test]
    async fn premium_expiry_deactivates_the_point_and_pushes_the_packets() {
        use crate::opcodes::sc;

        let server = test_server_with_config(premium_data(), AppConfig::default());
        let mut client = TestClient::new(&server, 1, 1001);
        let character = crate::test_support::test_character(1, "Expired", 100.0, 100.0);
        let player = crate::test_support::spawn_player(&server, &client, character, 0x101);
        crate::systems::services::premium::apply_grade_buffs(&server, &player, 2);
        assert!(player.data().unit.buffs.check_buff(8_000_011));
        let now = Utc::now().timestamp();
        server.accounts.set_state(
            1001,
            AccountState {
                payment: AccountPayment {
                    method: PaymentMethodType::Premium,
                    location: 1,
                    start_time: now - 100,
                    end_time: now - 1,
                },
                premium_point: 1,
            },
        );
        let state = expire_lazily(&server, 1001).await;
        assert_eq!(state.premium_point, 0);
        assert_eq!(state.payment.method, PaymentMethodType::Premium);
        assert!(!state.payment.premium_state(now));
        let frames = client.drain();
        let update = frames
            .iter()
            .find(|(op, _)| *op == sc::SCUpdatePremiumPointPacket)
            .expect("SCUpdatePremiumPoint");
        // [point i32 = 0][oldPg u8 = 2][pg u8 = 1]
        assert_eq!(update.1, [0, 0, 0, 0, 2, 1]);
        assert!(frames.iter().any(|(op, _)| *op == sc::SCAccountInfoPacket));
        assert!(
            !player.data().unit.buffs.check_buff(8_000_011),
            "expiry must remove the grade buff"
        );
        // A second pass is a no-op (the point already sits at `deactivate_point`).
        let again = expire_lazily(&server, 1001).await;
        assert_eq!(again.premium_point, 0);
        assert!(!client
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCUpdatePremiumPointPacket));
    }

    #[tokio::test]
    async fn a_stored_point_above_max_is_capped_on_load() {
        let server = test_server_with_config(premium_data(), AppConfig::default());
        let details = crate::db::AccountDetails {
            pay_method: 1,
            pay_location: 1,
            pay_start: 0,
            pay_end: 1_893_456_000,
            premium_point: 99,
            ..crate::db::AccountDetails::default()
        };
        let loaded = premium::state_from_details(&server, &details, Utc::now().timestamp());
        assert_eq!(loaded.state.premium_point, 1);
        assert!(loaded.changed);
        assert_eq!(loaded.deactivated_from, None);
    }

    /// A pinned private-MySQL pool with a temporary shadow `accounts` table (all writes vanish
    /// when it closes).
    async fn private_accounts_pool() -> Option<sqlx::MySqlPool> {
        use std::str::FromStr;

        let url = std::env::var("AAEMU_TEST_MYSQL").ok()?;
        let options =
            sqlx::mysql::MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
        assert_eq!(
            options.get_port(),
            3317,
            "only private MySQL3317 is allowed"
        );
        assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let (_, definition): (String, String) = sqlx::query_as("SHOW CREATE TABLE `accounts`")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            &crate::systems::social::achievement_persistence::temporary_shadow_definition(
                &definition,
            ),
        )
        .execute(&pool)
        .await
        .unwrap();
        Some(pool)
    }

    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
    async fn private_mysql_account_payment_round_trip() {
        let Some(pool) = private_accounts_pool().await else {
            return;
        };
        let config = AppConfig::default();
        sqlx::query(
            "INSERT INTO accounts (account_id, pay_method, pay_location, pay_start, pay_end, premium_point)              VALUES (777, 1, 2, 100, 200, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let details = crate::db::try_get_account_details(&pool, &config, 777)
            .await
            .expect("details");
        assert_eq!(
            (
                details.pay_method,
                details.pay_location,
                details.pay_start,
                details.pay_end,
                details.premium_point
            ),
            (1, 2, 100, 200, 1)
        );

        crate::db::update_account_payment(&pool, 777, 1, 1, 1_700_000_000, 1_900_000_000, 1)
            .await
            .unwrap();
        let mut server = test_server_with_config(premium_data(), AppConfig::default());
        server.db = pool.clone();
        let state = refresh_state(&server, 777).await;
        assert_eq!(state.payment.method, PaymentMethodType::Premium);
        assert_eq!(state.payment.start_time, 1_700_000_000);
        assert_eq!(state.payment.end_time, 1_900_000_000);
        assert_eq!(state.premium_point, 1);

        crate::db::update_premium_point(&pool, 777, 0).await;
        let details = crate::db::try_get_account_details(&pool, &config, 777)
            .await
            .expect("details");
        assert_eq!(details.premium_point, 0);

        // A GM grant may target an account without a game row: the upsert inserts it.
        crate::db::update_account_payment(&pool, 888, 1, 3, 1_700_000_000, 1_900_000_000, 1)
            .await
            .expect("insert a missing account row");
        let details = crate::db::try_get_account_details(&pool, &config, 888)
            .await
            .expect("details");
        assert_eq!(
            (
                details.pay_method,
                details.pay_location,
                details.pay_start,
                details.pay_end,
                details.premium_point
            ),
            (1, 3, 1_700_000_000, 1_900_000_000, 1)
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn dead_connection_sweep_closes_silent_connections() {
        let server = test_server();
        let quiet = TestClient::new(&server, 1, 10);
        let mut pinging = TestClient::new(&server, 2, 11);
        assert!(server.accounts.try_add(10, quiet.conn.id));
        assert!(server.accounts.try_add(11, pinging.conn.id));
        pinging.conn.touch_ping();
        let now = Utc::now().naive_utc();
        // Never pinged (`DateTime.MinValue`) -> closed; pinged just now -> kept.
        assert_eq!(remove_dead_connections(&server, now), vec![quiet.conn.id]);
        // 31 s later the pinging one is dead too.
        let later = now + chrono::Duration::seconds(31);
        let mut closed = remove_dead_connections(&server, later);
        closed.sort_unstable();
        let mut expected = vec![quiet.conn.id, pinging.conn.id];
        expected.sort_unstable();
        assert_eq!(closed, expected);
    }
}
