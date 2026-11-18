//! MySQL access for the lobby flow (runtime `sqlx::query`, no compile-time macros).
//!
//! Queries mirror the C# ones: `AccountManager.GetAccountDetailsInternal` / `UpdateLoginTime` and
//! `CharacterManager.LoadCharacters`; character persistence lives in [`crate::db_character`].
//!
//! Integer columns are selected through `CAST(... AS SIGNED)` because sqlx type-checks MySQL
//! integer widths strictly, while the C# reader converts (`GetInt16` on a MEDIUMINT etc.).

use chrono::{NaiveDateTime, Utc};
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::{MySql, Row, Transaction};

use crate::config::AppConfig;
use crate::error::Result;
use crate::models::item::UnixTime;

/// `DateTime.MinValue` (0001-01-01 00:00:00), the DB default for unset DATETIME columns.
pub fn min_value() -> NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(1, 1, 1)
        .expect("valid date")
        .and_hms_opt(0, 0, 0)
        .expect("valid time")
}

/// `t > DateTime.MinValue`.
pub fn is_set(t: Option<NaiveDateTime>) -> bool {
    t.is_some_and(|t| t > min_value())
}

/// `Helpers.UnixTime(DateTime)`: `<= MinValue` or before the epoch -> 0.
pub fn unix(dt: Option<NaiveDateTime>) -> UnixTime {
    dt.map_or(0, |d| d.and_utc().timestamp().max(0))
}

/// Unix seconds back to a DATETIME for writes (`0` = `DateTime.MinValue`).
pub fn from_unix(t: UnixTime) -> NaiveDateTime {
    if t <= 0 {
        return min_value();
    }
    chrono::DateTime::from_timestamp(t, 0).map_or_else(min_value, |d| d.naive_utc())
}

/// Reads a DATETIME column; NULL / zero dates (which sqlx cannot decode) become `None`
/// (`DateTime.MinValue` in C#).
pub fn dt(row: &MySqlRow, col: &str) -> Option<NaiveDateTime> {
    row.try_get::<Option<NaiveDateTime>, _>(col).ok().flatten()
}

pub fn int(row: &MySqlRow, col: &str) -> Result<i64> {
    Ok(row.try_get::<i64, _>(col)?)
}

/// `AccountDetails`.
#[derive(Debug, Clone, Default)]
pub struct AccountDetails {
    pub account_id: i32,
    pub access_level: i32,
    pub labor: i16,
    pub credits: i32,
    pub loyalty: i32,
    pub last_updated: Option<NaiveDateTime>,
    pub last_login: Option<NaiveDateTime>,
    pub last_labor_tick: Option<NaiveDateTime>,
    pub last_credits_tick: Option<NaiveDateTime>,
    pub last_loyalty_tick: Option<NaiveDateTime>,
    /// UTC midnight of the last daily login reward (the idempotency claim).
    pub last_daily_reward: Option<NaiveDateTime>,
    /// UTC day number (`unix seconds / 86400`) of the hourly Loyalty counters.
    pub loyalty_hour_day: i32,
    /// In-world seconds counted in [`Self::loyalty_hour_day`].
    pub loyalty_hour_seconds: i32,
    /// Hourly Loyalty tokens granted in [`Self::loyalty_hour_day`].
    pub loyalty_hour_taken: i32,
    /// Last online sample (UTC) of the hourly Loyalty counters.
    pub loyalty_hour_updated: Option<NaiveDateTime>,
    /// `accounts.pay_method` (`1` Premium, `3` Demo, `5` None, `0` = unset: use
    /// `Account:DefaultPayment`).
    pub pay_method: i32,
    pub pay_location: i32,
    /// Unix seconds (`SCAccountInfo.payStart`).
    pub pay_start: i64,
    /// Unix seconds (`SCAccountInfo.payEnd`).
    pub pay_end: i64,
    /// `accounts.premium_point` (grade = `premium_grades` lookup, client `FUN_397a7d10`).
    pub premium_point: i32,
}

impl AccountDetails {
    /// The hourly Loyalty counters as one value (see [`LoyaltyHourState`]).
    pub fn loyalty_hour_state(&self) -> LoyaltyHourState {
        LoyaltyHourState {
            day: self.loyalty_hour_day,
            seconds: i64::from(self.loyalty_hour_seconds),
            taken: self.loyalty_hour_taken,
            updated: self.loyalty_hour_updated,
        }
    }
}

/// The persisted hourly Loyalty counters of one account (`accounts.loyalty_hour_*`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoyaltyHourState {
    /// UTC day number (`unix seconds / 86400`) the counters belong to (`0` = never sampled).
    pub day: i32,
    /// In-world seconds counted in `day`.
    pub seconds: i64,
    /// Hourly tokens granted in `day`.
    pub taken: i32,
    /// Last online sample (UTC); `None` = the account has never been sampled.
    pub updated: Option<NaiveDateTime>,
}

/// `AccountManager.GetAccountDetailsInternal`, including the create-on-first-use insert.
/// Errors are logged and yield default details, as in C#.
pub async fn get_account_details(
    pool: &MySqlPool,
    config: &AppConfig,
    account_id: u32,
) -> AccountDetails {
    match try_get_account_details(pool, config, account_id).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("{e}");
            AccountDetails::default()
        }
    }
}

pub(crate) async fn try_get_account_details(
    pool: &MySqlPool,
    config: &AppConfig,
    account_id: u32,
) -> Result<AccountDetails> {
    let row = sqlx::query(
        "SELECT CAST(account_id AS SIGNED) AS account_id, CAST(access_level AS SIGNED) AS access_level, \
         CAST(labor AS SIGNED) AS labor, CAST(credits AS SIGNED) AS credits, CAST(loyalty AS SIGNED) AS loyalty, \
         last_updated, last_login, last_labor_tick, last_credits_tick, last_loyalty_tick, last_daily_reward, \
         CAST(loyalty_hour_day AS SIGNED) AS loyalty_hour_day, \
         CAST(loyalty_hour_seconds AS SIGNED) AS loyalty_hour_seconds, \
         CAST(loyalty_hour_taken AS SIGNED) AS loyalty_hour_taken, loyalty_hour_updated, \
         CAST(pay_method AS SIGNED) AS pay_method, CAST(pay_location AS SIGNED) AS pay_location, \
         CAST(pay_start AS SIGNED) AS pay_start, CAST(pay_end AS SIGNED) AS pay_end, \
         CAST(premium_point AS SIGNED) AS premium_point \
         FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = row {
        return Ok(AccountDetails {
            account_id: int(&row, "account_id")? as i32,
            access_level: int(&row, "access_level")? as i32,
            labor: int(&row, "labor")? as i16,
            credits: int(&row, "credits")? as i32,
            loyalty: int(&row, "loyalty")? as i32,
            last_updated: dt(&row, "last_updated"),
            last_login: dt(&row, "last_login"),
            last_labor_tick: dt(&row, "last_labor_tick"),
            last_credits_tick: dt(&row, "last_credits_tick"),
            last_loyalty_tick: dt(&row, "last_loyalty_tick"),
            last_daily_reward: dt(&row, "last_daily_reward"),
            loyalty_hour_day: int(&row, "loyalty_hour_day")? as i32,
            loyalty_hour_seconds: int(&row, "loyalty_hour_seconds")? as i32,
            loyalty_hour_taken: int(&row, "loyalty_hour_taken")? as i32,
            loyalty_hour_updated: dt(&row, "loyalty_hour_updated"),
            pay_method: int(&row, "pay_method")? as i32,
            pay_location: int(&row, "pay_location")? as i32,
            pay_start: int(&row, "pay_start")?,
            pay_end: int(&row, "pay_end")?,
            premium_point: int(&row, "premium_point")? as i32,
        });
    }

    // Account didn't exist, check if it's our first
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(pool)
        .await?;
    let access_level = if count <= 0 {
        config.account.access_level_first_account
    } else {
        0
    };
    let now = Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO accounts (account_id, access_level, labor, credits, loyalty, last_login, last_labor_tick, last_credits_tick, last_loyalty_tick) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(account_id)
    .bind(access_level)
    .bind(config.labor.default)
    .bind(config.credits.default)
    .bind(config.loyalty.default)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    // C# returns `res` with only the timestamps set (AccountId = LastInsertedId, which is 0 for a
    // non auto-increment key); Labor/Loyalty stay 0 for this first call.
    Ok(AccountDetails {
        last_login: Some(now),
        last_updated: Some(now),
        last_labor_tick: Some(now),
        last_credits_tick: Some(now),
        last_loyalty_tick: Some(now),
        ..AccountDetails::default()
    })
}

/// `AccountManager.UpdateLoginTime`: returns the previous `last_login`.
pub async fn update_login_time(
    pool: &MySqlPool,
    config: &AppConfig,
    account_id: u32,
    new_time: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let res = get_account_details(pool, config, account_id).await;
    if let Err(e) = sqlx::query("UPDATE accounts SET last_login = ? WHERE account_id = ?")
        .bind(new_time)
        .bind(account_id)
        .execute(pool)
        .await
    {
        tracing::error!("{e}");
        return Some(Utc::now().naive_utc());
    }
    res.last_login
}

/// `accounts.pay_method` / `pay_location` / `pay_start` / `pay_end` / `premium_point` in one
/// upsert (a GM grant / removal). The GM may target an account that has no game row yet (only a
/// login account exists), so the row is inserted when missing instead of silently affecting
/// zero rows. Callers hold the account lock (`AccountManager::lock_of`) and report the error to
/// the GM; `set_payment` does not apply the in-memory change when this fails.
pub async fn update_account_payment(
    pool: &MySqlPool,
    account_id: u32,
    pay_method: i32,
    pay_location: i32,
    pay_start: i64,
    pay_end: i64,
    premium_point: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO accounts (account_id, pay_method, pay_location, pay_start, pay_end, premium_point) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE pay_method = VALUES(pay_method), pay_location = VALUES(pay_location), \
         pay_start = VALUES(pay_start), pay_end = VALUES(pay_end), premium_point = VALUES(premium_point)",
    )
    .bind(account_id)
    .bind(pay_method)
    .bind(pay_location)
    .bind(pay_start)
    .bind(pay_end)
    .bind(premium_point)
    .execute(pool)
    .await
    .map_err(|e| {
        tracing::error!("update_account_payment({account_id}): {e}");
        crate::GameError::from(e)
    })?;
    Ok(())
}

/// Writes only `accounts.premium_point` (the connect / expiry / disconnect transitions keep the
/// stored payment method untouched). Callers hold the account lock.
pub async fn update_premium_point(pool: &MySqlPool, account_id: u32, premium_point: i32) {
    if let Err(e) = sqlx::query("UPDATE accounts SET premium_point = ? WHERE account_id = ?")
        .bind(premium_point)
        .bind(account_id)
        .execute(pool)
        .await
    {
        tracing::error!("update_premium_point({account_id}): {e}");
    }
}

/// `AccountManager.AddCredits` / `AddLoyalty`: `INSERT ... ON DUPLICATE KEY UPDATE col = col +
/// amount`; `true` when a row was affected (errors are logged, as in C#).
pub async fn add_account_points(
    pool: &MySqlPool,
    column: AccountPointColumn,
    account_id: u32,
    amount: i32,
) -> bool {
    let col = match column {
        AccountPointColumn::Credits => "credits",
        AccountPointColumn::Loyalty => "loyalty",
    };
    let sql = format!(
        "INSERT INTO accounts (account_id, {col}) VALUES(?, ?) ON DUPLICATE KEY UPDATE {col} = {col} + ?"
    );
    match sqlx::query(&sql)
        .bind(account_id)
        .bind(amount)
        .bind(amount)
        .execute(pool)
        .await
    {
        Ok(r) => r.rows_affected() > 0,
        Err(e) => {
            tracing::error!("{e}");
            false
        }
    }
}

/// The account point columns of [`add_account_points`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountPointColumn {
    Credits,
    Loyalty,
}

/// Claims the daily login reward for `today` (a UTC midnight) and credits the configured daily
/// amounts, all in one transaction. Returns `(credits, loyalty)` after the grant, or `None` when
/// the account was already paid for `today` (or the statements fail; the transaction rolls back).
///
/// The claim is the atomic `last_daily_reward < today` guard on the row, so the login path and
/// the midnight quest reset of an online account cannot both pay for the same day. It also
/// resets the legacy divine clock, which previously happened on every daily-login call.
pub async fn claim_daily_reward(
    pool: &MySqlPool,
    account_id: u32,
    today: NaiveDateTime,
    credits: i32,
    loyalty: i32,
) -> Option<(i32, i32)> {
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("claim_daily_reward({account_id}): {e}");
            return None;
        }
    };
    // The account row normally exists (created by `get_account_details`); create it for the
    // first-login race and for callers that reach the midnight hook without one.
    if let Err(e) = sqlx::query(
        "INSERT INTO accounts (account_id) VALUES (?) ON DUPLICATE KEY UPDATE account_id = account_id",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    {
        tracing::error!("claim_daily_reward({account_id}): {e}");
        let _ = tx.rollback().await;
        return None;
    }
    let claimed = sqlx::query(
        "UPDATE accounts SET last_daily_reward = ? WHERE account_id = ? \
         AND (last_daily_reward IS NULL OR last_daily_reward < ?)",
    )
    .bind(today)
    .bind(account_id)
    .bind(today)
    .execute(&mut *tx)
    .await;
    match claimed {
        Ok(r) if r.rows_affected() > 0 => {}
        Ok(_) => {
            let _ = tx.rollback().await;
            return None;
        }
        Err(e) => {
            tracing::error!("claim_daily_reward({account_id}): {e}");
            let _ = tx.rollback().await;
            return None;
        }
    }
    for (column, amount) in [("credits", credits), ("loyalty", loyalty)] {
        if amount <= 0 {
            continue;
        }
        let sql = format!("UPDATE accounts SET {column} = {column} + ? WHERE account_id = ?");
        if let Err(e) = sqlx::query(&sql)
            .bind(amount)
            .bind(account_id)
            .execute(&mut *tx)
            .await
        {
            tracing::error!("claim_daily_reward({account_id}): {e}");
            let _ = tx.rollback().await;
            return None;
        }
    }
    if let Err(e) = sqlx::query(
        "UPDATE accounts SET divine_clock_time = 0, divine_clock_taken = 0 WHERE account_id = ?",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    {
        tracing::error!("claim_daily_reward({account_id}): {e}");
        let _ = tx.rollback().await;
        return None;
    }
    let row = match sqlx::query(
        "SELECT CAST(credits AS SIGNED) AS credits, CAST(loyalty AS SIGNED) AS loyalty \
         FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            tracing::error!("claim_daily_reward({account_id}): {e}");
            let _ = tx.rollback().await;
            return None;
        }
    };
    let result = (
        int(&row, "credits").ok()? as i32,
        int(&row, "loyalty").ok()? as i32,
    );
    if let Err(e) = tx.commit().await {
        tracing::error!("claim_daily_reward({account_id}): {e}");
        return None;
    }
    Some(result)
}

/// Adds `amount` to `accounts.loyalty` and returns the new total (one transaction).
pub async fn add_loyalty_returning(
    pool: &MySqlPool,
    account_id: u32,
    amount: i32,
) -> Result<Option<i32>> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO accounts (account_id, loyalty) VALUES (?, ?) \
         ON DUPLICATE KEY UPDATE loyalty = loyalty + ?",
    )
    .bind(account_id)
    .bind(amount)
    .bind(amount)
    .execute(&mut *tx)
    .await?;
    let total = sqlx::query_scalar::<_, i64>(
        "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?
    .map(|v| v as i32);
    tx.commit().await?;
    Ok(total)
}

/// Adds `amount` (when positive) to `accounts.loyalty` and stores the account's new hourly
/// Loyalty counters in the same transaction. The counter write only applies while the row still
/// holds the snapshot it was computed from ([`store_loyalty_hour_state_tx`]), so two ticking
/// tasks that read the same state credit once. Returns the new total, or `None` when the
/// snapshot was stale or the transaction failed (nothing was credited).
pub async fn credit_hourly_loyalty(
    pool: &MySqlPool,
    account_id: u32,
    amount: i32,
    previous: LoyaltyHourState,
    next: LoyaltyHourState,
) -> Result<Option<i32>> {
    let mut tx = pool.begin().await?;
    if !store_loyalty_hour_state_tx(&mut tx, account_id, previous, next).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    if amount > 0 {
        sqlx::query(
            "INSERT INTO accounts (account_id, loyalty) VALUES (?, ?) \
             ON DUPLICATE KEY UPDATE loyalty = loyalty + ?",
        )
        .bind(account_id)
        .bind(amount)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    }
    let total = if amount > 0 {
        sqlx::query_scalar::<_, i64>(
            "SELECT CAST(loyalty AS SIGNED) FROM accounts WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|v| v as i32)
    } else {
        None
    };
    tx.commit().await?;
    Ok(total)
}

/// Stores the account's hourly Loyalty counters without granting a token. Returns whether the
/// row still held the snapshot that produced `next`.
pub async fn store_loyalty_hour_state(
    pool: &MySqlPool,
    account_id: u32,
    previous: LoyaltyHourState,
    next: LoyaltyHourState,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let changed = store_loyalty_hour_state_tx(&mut tx, account_id, previous, next).await?;
    tx.commit().await?;
    Ok(changed)
}

/// Conditional counter write: applies `next` only while the row still holds `previous`
/// (`loyalty_hour_updated` is NULL-safe compared). A concurrent tick that already wrote its own
/// sample makes this a no-op, so the same interval can never be paid twice.
async fn store_loyalty_hour_state_tx(
    tx: &mut Transaction<'_, MySql>,
    account_id: u32,
    previous: LoyaltyHourState,
    next: LoyaltyHourState,
) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE accounts SET loyalty_hour_day = ?, loyalty_hour_seconds = ?, \
         loyalty_hour_taken = ?, loyalty_hour_updated = ? \
         WHERE account_id = ? AND loyalty_hour_day = ? AND loyalty_hour_seconds = ? \
         AND loyalty_hour_taken = ? AND loyalty_hour_updated <=> ?",
    )
    .bind(next.day)
    .bind(i32::try_from(next.seconds.clamp(0, i64::from(i32::MAX))).unwrap_or(i32::MAX))
    .bind(next.taken.max(0))
    .bind(next.updated)
    .bind(account_id)
    .bind(previous.day)
    .bind(previous.seconds as i32)
    .bind(previous.taken)
    .bind(previous.updated)
    .execute(&mut **tx)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// `accounts.extra_character_slots` (0 when the account row does not exist).
pub async fn extra_character_slots(pool: &MySqlPool, account_id: u32) -> u8 {
    let slots: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(extra_character_slots AS SIGNED) FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!("extra_character_slots({account_id}): {e}");
        None
    });
    slots.unwrap_or(0).clamp(0, i64::from(u8::MAX)) as u8
}

/// Adds one `accounts.extra_character_slots` (saturating at 255) and returns the new count.
pub async fn add_extra_character_slot(pool: &MySqlPool, account_id: u32) -> Option<u8> {
    if let Err(e) = sqlx::query(
        "INSERT INTO accounts (account_id, extra_character_slots) VALUES (?, 1) \
         ON DUPLICATE KEY UPDATE extra_character_slots = LEAST(255, extra_character_slots + 1)",
    )
    .bind(account_id)
    .execute(pool)
    .await
    {
        tracing::error!("add_extra_character_slot({account_id}): {e}");
        return None;
    }
    Some(extra_character_slots(pool, account_id).await)
}

/// `AccountManager.UpdateLabor`.
pub async fn update_labor(pool: &MySqlPool, account_id: u32, labor: i16) {
    if let Err(e) = sqlx::query("UPDATE accounts SET labor = ? WHERE account_id = ?")
        .bind(labor)
        .bind(account_id)
        .execute(pool)
        .await
    {
        tracing::error!("{e}");
    }
}

/// Offline labor: atomically adds `add` to `accounts.labor` while the row is below `max`
/// (`LEAST` clamps the result, the `labor < max` guard makes an at-cap row a no-op). Returns the
/// affected rows; errors are returned, not swallowed.
pub async fn add_labor_capped(
    pool: &MySqlPool,
    account_id: u32,
    add: i32,
    max: i16,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE accounts SET labor = LEAST(?, labor + ?) WHERE account_id = ? AND labor < ?",
    )
    .bind(max)
    .bind(add)
    .bind(account_id)
    .bind(max)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// `AccountManager.UpdateTickTimes`: the column of the last set flag (labor, credits, loyalty
/// in that order of precedence); no flag writes the column `error` (fails and logs, as C#).
pub async fn update_tick_times(
    pool: &MySqlPool,
    account_id: u32,
    new_time: NaiveDateTime,
    update_labor: bool,
    update_credits: bool,
    update_loyalty: bool,
) {
    let mut field = "error";
    if update_labor {
        field = "last_labor_tick";
    }
    if update_credits {
        field = "last_credits_tick";
    }
    if update_loyalty {
        field = "last_loyalty_tick";
    }
    let sql = format!("UPDATE accounts SET {field} = ? WHERE account_id = ?");
    if let Err(e) = sqlx::query(&sql)
        .bind(new_time)
        .bind(account_id)
        .execute(pool)
        .await
    {
        tracing::error!("{e}");
    }
}

/// `AccountManager.UpdateDivineClock`.
pub async fn update_divine_clock(
    pool: &MySqlPool,
    account_id: u32,
    time_elapsed: u32,
    times_taken: u32,
) {
    if let Err(e) = sqlx::query(
        "UPDATE accounts SET divine_clock_time = ? , divine_clock_taken = ? WHERE account_id = ?",
    )
    .bind(time_elapsed)
    .bind(times_taken)
    .bind(account_id)
    .execute(pool)
    .await
    {
        tracing::error!("{e}");
    }
}

/// MySQL error numbers that roll the failed statement (or transaction) back, so re-running it
/// is safe: `1213` `ER_LOCK_DEADLOCK` and `1205` `ER_LOCK_WAIT_TIMEOUT`.
pub(crate) fn is_retryable_transaction_error(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = error else {
        return false;
    };
    if let Some(mysql) = db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
        return matches!(mysql.number(), 1213 | 1205);
    }
    // Test doubles and non-MySQL drivers carry the number or the deadlock SQLSTATE as `code`.
    db.code()
        .is_some_and(|code| matches!(&*code, "1213" | "1205" | "40001"))
}

/// Runs `op` again while MySQL rolls it back with a deadlock / lock wait timeout, up to
/// `max_attempts` calls in total. Both errors roll the whole statement back, so `op` merely
/// repeats the same write; a short pause lets the other transaction finish.
///
/// Only for **autocommit single statements** on the pool: inside an open transaction 1213 has
/// rolled back the whole transaction and 1205 only the last statement, so repeating one
/// statement there would silently lose the earlier ones.
async fn retry_transaction<T, F, Fut>(max_attempts: u32, mut op: F) -> sqlx::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = sqlx::Result<T>>,
{
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < max_attempts && is_retryable_transaction_error(&error) => {
                tracing::warn!(
                    "Database write rolled back ({error}); retrying ({attempt}/{max_attempts})"
                );
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Attempts per `add_divine_clock_time` statement (the first call plus two retries). A retried
/// statement does not need to outlast the periodic save: it blocks inside MySQL on the rows the
/// save still holds until the save commits (up to `innodb_lock_wait_timeout`, then 1205 and the
/// next attempt). The retry only recovers the statement from a rollback.
const ADD_DIVINE_CLOCK_ATTEMPTS: u32 = 3;

/// Adds `delta` seconds to the divine clock of every account in `account_ids` with one atomic
/// statement. `divine_clock_time` is `INT UNSIGNED`: the sum saturates at `u32::MAX` instead of
/// wrapping like the C# `uint` (a strict-mode overflow would otherwise fail the whole batch).
/// Returns `false` (after the retries) when the statement did not commit, so the caller can
/// keep the sample for the next save.
///
/// InnoDB locks the rows of `WHERE account_id IN (...)` in primary-key order whatever the bind
/// order, so sorting the ids here only makes the statement deterministic (and the dedup removes
/// a duplicate id that would make the per-account lock in `services/accounts.rs` wait on
/// itself). The lock-order fix proper is in `SaveManager`: the periodic save flushes one
/// `accounts.labor` row per character, now ordered by account id, so the save and this
/// online-tracking statement take the rows in the same order. Before that they could take them
/// in opposite orders and deadlock (MySQL 1213), which rolled these clock updates back on the
/// cloud bot server. A deadlock whose victim is this statement is retried, so the sample is not
/// lost.
pub async fn add_divine_clock_time(pool: &MySqlPool, account_ids: &[u32], delta: u32) -> bool {
    if account_ids.is_empty() || delta == 0 {
        return true;
    }
    let mut ids = account_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let marks = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "UPDATE accounts SET divine_clock_time = LEAST(CAST(divine_clock_time AS UNSIGNED) + ?, 4294967295) WHERE account_id IN ({marks})"
    );
    let result = retry_transaction(ADD_DIVINE_CLOCK_ATTEMPTS, || {
        let mut q = sqlx::query(&sql).bind(u64::from(delta));
        for id in &ids {
            q = q.bind(*id);
        }
        q.execute(pool)
    })
    .await;
    if let Err(e) = &result {
        tracing::error!(
            "add_divine_clock_time({} account(s), delta {delta}): {e}",
            ids.len()
        );
    }
    result.is_ok()
}

/// `AccountManager.GetDivineClock`: `(divine_clock_time, divine_clock_taken)`, `(0, 0)` when the
/// account does not exist or the query fails.
pub async fn get_divine_clock(pool: &MySqlPool, account_id: u32) -> (u32, u32) {
    let row = sqlx::query(
        "SELECT account_id, CAST(divine_clock_time AS SIGNED) AS t, CAST(divine_clock_taken AS SIGNED) AS n          FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some(r)) => (
            int(&r, "t").unwrap_or(0) as u32,
            int(&r, "n").unwrap_or(0) as u32,
        ),
        Ok(None) => (0, 0),
        Err(e) => {
            tracing::error!("{e}");
            (0, 0)
        }
    }
}

/// `LoginCharacterInfo` (AAEmu.Commons.Models).
#[derive(Debug, Clone, PartialEq)]
pub struct LoginCharacterInfo {
    pub id: u32,
    pub account_id: u32,
    pub name: String,
    pub race: u8,
    pub gender: u8,
}

/// `CharacterManager.LoadCharacters(accountId)` (for `LGRequestInfoPacket`).
pub async fn load_login_characters(
    pool: &MySqlPool,
    account_id: u32,
) -> Result<Vec<LoginCharacterInfo>> {
    let rows = sqlx::query(
        "SELECT CAST(`id` AS SIGNED) AS id, `name`, CAST(`race` AS SIGNED) AS race, CAST(`gender` AS SIGNED) AS gender, `delete_time` \
         FROM characters WHERE `account_id`=? and `deleted`=0",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    let now = Utc::now().naive_utc();
    let mut out = Vec::new();
    for r in rows {
        // Skip this char in the list if it's ready to be deleted
        let delete_time = dt(&r, "delete_time");
        if is_set(delete_time) && delete_time.is_some_and(|t| t < now) {
            continue;
        }
        out.push(LoginCharacterInfo {
            id: int(&r, "id")? as u32,
            account_id,
            name: r.try_get("name")?,
            race: int(&r, "race")? as u8,
            gender: int(&r, "gender")? as u8,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestDatabaseError(&'static str);

    impl std::fmt::Display for TestDatabaseError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "test database error {}", self.0)
        }
    }

    impl std::error::Error for TestDatabaseError {}

    impl sqlx::error::DatabaseError for TestDatabaseError {
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.0.into())
        }

        fn constraint(&self) -> Option<&str> {
            None
        }

        fn message(&self) -> &str {
            "test database error"
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    fn database_error(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(TestDatabaseError(code)))
    }

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// The MySQL error number of a database error (`1213` deadlock, `1205` lock wait timeout).
    fn mysql_error_number(error: &sqlx::Error) -> Option<u16> {
        let sqlx::Error::Database(db) = error else {
            return None;
        };
        db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .map(|mysql| mysql.number())
    }

    #[tokio::test]
    async fn retry_repeats_a_deadlocked_statement_until_it_succeeds() {
        let attempts = AtomicU32::new(0);
        let result = retry_transaction(3, || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if attempt < 3 {
                    Err(database_error("1213"))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 3);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_repeats_a_lock_wait_timeout() {
        let attempts = AtomicU32::new(0);
        let result = retry_transaction(3, || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if attempt < 2 {
                    Err(database_error("1205"))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_gives_up_after_the_attempt_bound() {
        let attempts = AtomicU32::new(0);
        let result: sqlx::Result<()> = retry_transaction(3, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(database_error("1213")) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_does_not_repeat_other_database_errors() {
        let attempts = AtomicU32::new(0);
        let result: sqlx::Result<()> = retry_transaction(3, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(database_error("1062")) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    /// Reproduces the save / online-tracking lock-order deadlock on the private MySQL: the
    /// "save" transaction locks row 5 (an account row of a character it saves) and a block of
    /// other rows, the retrying "tracking" statement then takes `id IN (1..=5)` in index order
    /// and blocks on row 5, and when the save asks for row 3 InnoDB detects the cycle and rolls
    /// back the smaller statement (the tracking one). The retry rewrites it after the save
    /// commits, so no row loses its update.
    ///
    /// The test waits until the tracking statement is actually in InnoDB's `LOCK WAIT` state
    /// before the save takes row 3, so a tracking statement that starts late cannot finish
    /// before the deadlock window. It also records every tracking attempt and asserts that the
    /// first one was rolled back with error 1213 and that the retry ran.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; creates and drops a scratch table"]
    async fn private_mysql_retry_recovers_a_divine_clock_deadlock() {
        use std::str::FromStr;
        use std::time::Duration;

        use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};

        let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
            return;
        };
        let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
        assert_eq!(
            options.get_port(),
            3317,
            "Only private MySQL3317 is allowed"
        );
        assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
        let pool = MySqlPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .unwrap();
        // A third connection observes `performance_schema.data_lock_waits` while the save and
        // the tracking statement hold the two pool connections.
        let monitor = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let table = format!(
            "divine_clock_deadlock_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        sqlx::query(&format!(
            "CREATE TABLE `{table}` (id INT NOT NULL PRIMARY KEY, \
             save_step INT NOT NULL DEFAULT 0, divine_clock_time INT UNSIGNED NOT NULL DEFAULT 0) \
             ENGINE=InnoDB"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "INSERT INTO `{table}` (id) \
             WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 120) \
             SELECT n FROM s"
        ))
        .execute(&pool)
        .await
        .unwrap();

        // The save, in the character order the snapshot had before the fix: row 5 first, then
        // the rest of its rows.
        let mut save = pool.begin().await.unwrap();
        for sql in [
            format!("UPDATE `{table}` SET save_step = save_step + 1 WHERE id = 5"),
            format!("UPDATE `{table}` SET save_step = save_step + 1 WHERE id BETWEEN 101 AND 120"),
        ] {
            sqlx::query(&sql).execute(&mut *save).await.unwrap();
        }

        // The tracking statement: blocks on row 5 after locking rows 1-4. Every attempt and
        // every deadlock (error 1213) it hits is recorded so the retry path is asserted.
        let sql = format!(
            "UPDATE `{table}` SET divine_clock_time = \
             LEAST(CAST(divine_clock_time AS UNSIGNED) + 1, 4294967295) WHERE id IN (1,2,3,4,5)"
        );
        let attempts = Arc::new(AtomicU32::new(0));
        let deadlocks = Arc::new(AtomicU32::new(0));
        let first_error = Arc::new(AtomicU32::new(0));
        let tracking = tokio::spawn({
            let retry_pool = pool.clone();
            let attempts = Arc::clone(&attempts);
            let deadlocks = Arc::clone(&deadlocks);
            let first_error = Arc::clone(&first_error);
            async move {
                retry_transaction(ADD_DIVINE_CLOCK_ATTEMPTS, move || {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    let retry_pool = retry_pool.clone();
                    let sql = sql.clone();
                    let deadlocks = Arc::clone(&deadlocks);
                    let first_error = Arc::clone(&first_error);
                    async move {
                        let result = sqlx::query(&sql).execute(&retry_pool).await;
                        if let Err(error) = &result {
                            let number = mysql_error_number(error);
                            if number == Some(1213) {
                                deadlocks.fetch_add(1, Ordering::SeqCst);
                            }
                            if attempt == 1 {
                                first_error.store(u32::from(number.unwrap_or(0)), Ordering::SeqCst);
                            }
                        }
                        result
                    }
                })
                .await
            }
        });

        // Wait until the tracking statement really is blocked on row 5. A fixed sleep could let
        // it start after the save took row 3, in which case the tracking succeeds on its first
        // attempt and the test would no longer prove the deadlock recovery. A row lock wait is
        // visible in `performance_schema.data_lock_waits` (InnoDB can keep `innodb_trx.trx_state`
        // at RUNNING while the statement waits).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let waiting: Option<String> = sqlx::query_scalar(
                "SELECT t.PROCESSLIST_INFO FROM performance_schema.data_lock_waits w \
                 JOIN performance_schema.threads t ON t.THREAD_ID = w.REQUESTING_THREAD_ID \
                 WHERE t.PROCESSLIST_INFO LIKE ? LIMIT 1",
            )
            .bind(format!("%{table}%"))
            .fetch_optional(&monitor)
            .await
            .unwrap();
            if waiting.is_some() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the tracking statement never reached LOCK WAIT on row 5"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The save asks for row 3, held by the tracking statement: deadlock. InnoDB rolls back
        // the smaller statement (the tracking one); the save update and its commit succeed.
        sqlx::query(&format!(
            "UPDATE `{table}` SET save_step = save_step + 1 WHERE id = 3"
        ))
        .execute(&mut *save)
        .await
        .unwrap();
        save.commit().await.unwrap();

        tracking
            .await
            .unwrap()
            .expect("the retried tracking write succeeds");
        assert_eq!(
            first_error.load(Ordering::SeqCst),
            1213,
            "the first tracking attempt must be rolled back by the deadlock"
        );
        assert_eq!(
            deadlocks.load(Ordering::SeqCst),
            1,
            "the tracking statement must hit exactly one deadlock"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "the tracking statement must be retried exactly once"
        );

        let rows: Vec<(i64, i64)> = sqlx::query_as(&format!(
            "SELECT CAST(save_step AS SIGNED), CAST(divine_clock_time AS SIGNED) \
             FROM `{table}` WHERE id <= 5 ORDER BY id"
        ))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [(0, 1), (0, 1), (1, 1), (0, 1), (1, 1)],
            "the save steps and the retried clock update all survive"
        );
        let high: i64 = sqlx::query_scalar(&format!(
            "SELECT CAST(SUM(save_step) AS SIGNED) FROM `{table}` WHERE id BETWEEN 101 AND 120"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(high, 20);

        sqlx::query(&format!("DROP TABLE `{table}`"))
            .execute(&pool)
            .await
            .unwrap();
    }

    /// Test-server accounts get their starting Credits/Loyalty from the `Credits.Default` /
    /// `Loyalty.Default` configuration (the shipped values are 0). The game creates the account
    /// row on first login, so the grant happens exactly once per account; a later login reads
    /// the stored values and never adds them again. Gated by `AAEMU_TEST_MYSQL` and using a
    /// connection-local temporary `accounts` table, so no persisted data is touched.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses a connection-local accounts table"]
    async fn private_mysql_first_login_grants_starting_credits_once() {
        use std::str::FromStr;

        use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};

        let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
            return;
        };
        let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
        assert_eq!(
            options.get_port(),
            3317,
            "Only private MySQL3317 is allowed"
        );
        assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TEMPORARY TABLE accounts ( \
                 account_id INT NOT NULL PRIMARY KEY, \
                 access_level INT NOT NULL DEFAULT 0, \
                 labor SMALLINT NOT NULL DEFAULT 0, \
                 credits INT NOT NULL DEFAULT 0, \
                 loyalty INT NOT NULL DEFAULT 0, \
                 last_updated DATETIME NULL, last_login DATETIME NULL, \
                 last_labor_tick DATETIME NULL, last_credits_tick DATETIME NULL, \
                 last_loyalty_tick DATETIME NULL, last_daily_reward DATETIME NULL, \
                 loyalty_hour_day INT NOT NULL DEFAULT 0, \
                 loyalty_hour_seconds INT NOT NULL DEFAULT 0, \
                 loyalty_hour_taken INT NOT NULL DEFAULT 0, \
                 loyalty_hour_updated DATETIME NULL, \
                 pay_method INT NOT NULL DEFAULT 0, pay_location INT NOT NULL DEFAULT 0, \
                 pay_start BIGINT NOT NULL DEFAULT 0, pay_end BIGINT NOT NULL DEFAULT 0, \
                 premium_point INT NOT NULL DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut config = AppConfig::default();
        config.credits.default = 1_000_000;
        config.loyalty.default = 5_000;

        // First login: the insert runs and stores the configured starting values. The C# return
        // value of this call keeps the all-zero defaults, so read the row back.
        let first = try_get_account_details(&pool, &config, 4242).await.unwrap();
        assert_eq!(first.credits, 0);
        let (credits, loyalty): (i32, i32) =
            sqlx::query_as("SELECT credits, loyalty FROM accounts WHERE account_id = 4242")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((credits, loyalty), (1_000_000, 5_000));

        // Later logins must not grant them again.
        for _ in 0..2 {
            let again = try_get_account_details(&pool, &config, 4242).await.unwrap();
            assert_eq!(again.credits, 1_000_000);
            assert_eq!(again.loyalty, 5_000);
        }
    }
}
