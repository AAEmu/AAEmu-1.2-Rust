//! Premium service shop (WP-C): `CSPremiumServiceList` / `Buy` / `Msg`.
//!
//! # Evidence
//!
//! * `CSPremiumServiceList` 0x136 has an empty body (writer `FUN_391877d0`).
//! * `CSPremiumServiceBuy` 0x135 is `i32 cId` (client serializer `FUN_397b17b0`, writer
//!   `FUN_39187790`).
//! * `CSPremiumServiceMsg` 0x13c is `i32 stage` (serializer `FUN_397b17d0`, writer `FUN_39187870`).
//!   The client sends it only when `payMethod == 1` (premium), stage 1 at character-select init
//!   (`FUN_39426790`) and stage 2 at world entry (line ~876536). No 1.2 client code expects an
//!   answer to it, so the C# `SCAccountWarned("Premium ...")` is removed instead of guessed at.
//! * `SCPremiumServiceList` 0x1d8 is `bool isEnd, u8 size, detail, i32 exchangeRatio`
//!   (`FUN_397c49b0` -> `FUN_397bf920`); detail is `i32 cId, string cname (<= 400),
//!   u16 pId, u8 isSell, u8 isHidden, i32 ptime, u8 ptype, i32 price, u32 id, i32 bcount`.
//!   The client handler `FUN_391e25e0` consumes **one detail per packet**; `size` is the expected
//!   **total** row count, not the count in this packet: it passes the detail to `FUN_39055e50`,
//!   which appends only while `(end - begin) / 0x1b0 < size`. Every packet therefore carries
//!   `size = rows.len()` and `isEnd` is set only on the last one. A `size = 0` packet marks the
//!   list ready (empty catalogue); it does not clear a list. The catalogue is capped at 255 rows
//!   (the `u8` field, logged when truncated).
//! * Buy result: the 1.2 SC table has no dedicated premium-buy result packet. The client's error
//!   handler (`FUN_391e5070`, reached from `OnErrorMsg`) routes error ids 772..775 to
//!   `FUN_39055e80` / `FUN_39055a90`, which raise UI event 0x266 with `PSBFR_NONE / NORMAL /
//!   CASH / AA_POINT`; `premiumservice/premium_service.alb` shows the matching success / failure
//!   message (`ShowPremiumServiceBuyResult`). The buy result therefore is `SCErrorMsg` with
//!   `PREMIUM_SERVICE_BUY_SUCCESS` 772, `PREMIUM_SERVICE_BUY_FAIL` 773,
//!   `PREMIUM_SERVICE_NOT_ENOUGH_AA_CASH` 774 or `PREMIUM_SERVICE_NOT_ENOUGH_AA_POINT` 775
//!   (all already in `error_message`). A capture of one real purchase is still pending
//!   (**ACCOUNT-PREMIUM-BUY-1**).
//! * Catalogue: `compact.sqlite3.premium_points` is empty (0 rows), so the rows come from the
//!   operator table `aaemu_game.premium_services`. It is empty by default (no 1.2 retail row is
//!   evidenced); see the SQL update for the column meaning and an example insert.
//!
//! # Purchase
//!
//! The debit and the premium grant are one MySQL transaction: the account row is locked, the
//! guarded balance update is the debit (`... >= price`, so two concurrent buys cannot overdraw),
//! the payment fields are extended on the same row and the sale is audited in
//! `audit_ics_sales`, then the transaction commits. The whole path runs under
//! `AccountManager::lock_of` so the cached state and the packets of one account cannot interleave.
//! Only the account columns `credits` and `loyalty` can be debited atomically with the grant;
//! AA points have no balance carrier yet (WP-D) and are refused with error 775. A loyalty debit
//! also updates the buyer's `bm_point` and sends `SCBmPointPacket` (the ICS loyalty path). A
//! permanent premium (`pay_end = 0`, including the configured default) is refused with 773
//! before the debit: there is no end to extend. A row that still uses `Account:DefaultPayment`
//! keeps `pay_method = 0`; only its purchased window (`pay_start` / `pay_end`) is stored, which
//! [`premium::resolve_payment`] reads back as explicit premium. The stored premium point is
//! raised to `premium_configs.connect_point` at most and never lowered (`max(details.premium_point,
//! connect_point).clamp(0, max_point)`).
//!
//! # Known limits
//!
//! * An ambiguous MySQL commit error (e.g. the connection drops during `COMMIT`) answers 773;
//!   the transaction may have committed, but the cached state is only corrected by the next
//!   refresh / login.
//! * The state is published after the account lock is released. A concurrent GM `set_payment`
//!   can publish in between, so the client may briefly show the purchase state after the GM
//!   write until the next `SCAccountInfo`. The database row is the one that was written last.
//! * There is no rate limit on `CSPremiumServiceBuy`; every packet is one charge.
//! * A GM premium window that starts in the future is not premium yet and is replaced by
//!   `now..now + hours` by a purchase (not extended).

use std::sync::{Arc, Mutex};

use aaemu_commons::{Marshal, PacketStream};
use chrono::Utc;
use futures::future::BoxFuture;
use sqlx::mysql::MySqlPool;
use sqlx::Row;

use crate::config::DefaultPayment;
use crate::db::{self, AccountDetails};
use crate::error::Result;
use crate::managers::account::AccountState;
use crate::net::connection::{AccountPayment, GameConnection, PaymentMethodType};
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::{cs, sc};
use crate::packets::packet_def;
use crate::server::GameServer;
use crate::systems::services::premium;
use crate::systems::HookSlot;
use crate::world::PlayerRef;

use super::cash_shop::{self, SCBmPointPacket, SCICSCashPointPacket};
use super::error_message;
use super::inventory;
use super::item_handlers::{active, sync_client_packet};

/// `PremiumDetail` (the `SCPremiumServiceList` detail block).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PremiumDetail {
    pub c_id: i32,
    pub c_name: String,
    pub p_id: u16,
    pub is_sell: u8,
    pub is_hidden: u8,
    pub p_time: i32,
    pub p_type: u8,
    pub price: i32,
    pub id: u32,
    pub b_count: i32,
}

impl Marshal for PremiumDetail {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.c_id)
            .write_str(&self.c_name)
            .write_u16(self.p_id)
            .write_u8(self.is_sell)
            .write_u8(self.is_hidden)
            .write_i32(self.p_time)
            .write_u8(self.p_type)
            .write_i32(self.price)
            .write_u32(self.id)
            .write_i32(self.b_count);
    }
}

/// One `aaemu_game.premium_services` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PremiumService {
    pub c_id: i32,
    pub name: String,
    pub premium_id: u16,
    /// Hours of premium granted by a purchase (`ptime`).
    pub hours: i32,
    /// ICS currency (`cash_shop::currency`).
    pub price_type: u8,
    pub price: i32,
    /// Copied to the client detail's `id`; the client meaning is UNKNOWN (display only).
    pub grade: u8,
    pub is_sell: bool,
    pub is_hidden: bool,
    /// Copied to the client detail's `bcount`; not enforced (the 1.2 meaning is UNKNOWN).
    pub buy_max: i32,
}

impl PremiumService {
    pub fn detail(&self) -> PremiumDetail {
        PremiumDetail {
            c_id: self.c_id,
            c_name: self.name.clone(),
            p_id: self.premium_id,
            is_sell: u8::from(self.is_sell),
            is_hidden: u8::from(self.is_hidden),
            p_time: self.hours,
            p_type: self.price_type,
            price: self.price,
            id: u32::from(self.grade),
            b_count: self.buy_max,
        }
    }
}

/// `SCPremiumServiceListPacket(isEnd, size, detail, exchangeRatio)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCPremiumServiceListPacket {
    pub is_end: bool,
    pub size: u8,
    pub detail: PremiumDetail,
    pub exchange_ratio: i32,
}
packet_def!(
    SCPremiumServiceListPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCPremiumServiceListPacket
);
impl Marshal for SCPremiumServiceListPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.is_end).write_u8(self.size);
        self.detail.write(s);
        s.write_i32(self.exchange_ratio);
    }
}

/// `CSPremiumServiceBuyPacket` (`i32 cId`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSPremiumServiceBuyPacket {
    pub c_id: i32,
}

impl CSPremiumServiceBuyPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            c_id: s.read_i32()?,
        })
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let Some(buyer) = active(conn) else {
            tracing::warn!("PremiumServiceBuy: no active character");
            return;
        };
        let Some(server) = server.self_arc() else {
            tracing::error!("CSPremiumServiceBuyPacket: server handle not registered");
            return;
        };
        let c_id = self.c_id;
        tokio::spawn(async move {
            execute_buy(&server, &buyer, c_id).await;
        });
    }
}
sync_client_packet!(CSPremiumServiceBuyPacket);

/// `CSPremiumServiceListPacket` (empty body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSPremiumServiceListPacket;

impl CSPremiumServiceListPacket {
    fn read_body(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn run(self, server: &GameServer, conn: &mut GameConnection) {
        let services = manager(server).services();
        if services.is_empty() {
            conn.send(&SCPremiumServiceListPacket {
                is_end: true,
                size: 0,
                detail: PremiumDetail::default(),
                exchange_ratio: 0,
            });
            return;
        }
        let last = services.len() - 1;
        // `size` is the total row count expected by `FUN_391e25e0`, on every packet.
        let size = services.len().min(usize::from(u8::MAX)) as u8;
        for (index, service) in services.iter().enumerate() {
            conn.send(&SCPremiumServiceListPacket {
                is_end: index == last,
                size,
                detail: service.detail(),
                exchange_ratio: 0,
            });
        }
    }
}
sync_client_packet!(CSPremiumServiceListPacket);

/// `CSPremiumServiceMsgPacket` (`i32 stage`). The client uses the stage as a trigger (1 at
/// character-select init, 2 at world entry) and expects no reply; nothing is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSPremiumServiceMsgPacket {
    pub stage: i32,
}

impl CSPremiumServiceMsgPacket {
    fn read_body(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            stage: s.read_i32()?,
        })
    }

    fn run(self, _server: &GameServer, _conn: &mut GameConnection) {
        tracing::debug!("PremiumServiceMsg, stage {}", self.stage);
    }
}
sync_client_packet!(CSPremiumServiceMsgPacket);

/// Result of [`execute_buy`] (also used by the tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyOutcome {
    Purchased,
    InsufficientFunds,
    Refused,
}

/// One purchase request; the catalogue row is validated by [`execute_buy`] before the store.
#[derive(Debug, Clone)]
pub struct PurchaseRequest {
    pub account_id: u32,
    pub character_id: u32,
    pub service: PremiumService,
    /// Unix seconds, captured when the purchase starts (tests pin it; the store does not re-read
    /// the clock).
    pub now: i64,
    /// `Account:DefaultPayment` for a row whose `pay_method` is still 0.
    pub default_payment: DefaultPayment,
    pub connect_point: i32,
    pub max_point: i32,
}

/// What the store's transaction did.
#[derive(Debug, Clone)]
pub enum PurchaseOutcome {
    Purchased {
        payment: AccountPayment,
        premium_point: i32,
        /// New `accounts.credits` after a credits debit (`None` for other currencies).
        credits: Option<i32>,
        /// New `accounts.loyalty` after a loyalty debit (`None` for other currencies).
        loyalty: Option<i32>,
    },
    InsufficientFunds,
    /// The resolved payment is a permanent premium (`pay_end == 0`): there is nothing to
    /// extend, so the buy is refused and nothing is written.
    PermanentPremium,
    /// A validation / database failure: nothing was written.
    Failed,
}

/// The atomic debit + grant of one service purchase.
pub trait PremiumServiceStore: Send + Sync {
    fn purchase<'a>(
        &'a self,
        server: &'a GameServer,
        request: PurchaseRequest,
    ) -> BoxFuture<'a, PurchaseOutcome>;
}

/// The production store (one MySQL transaction).
#[derive(Debug, Default)]
pub struct MySqlPremiumServiceStore;

impl PremiumServiceStore for MySqlPremiumServiceStore {
    fn purchase<'a>(
        &'a self,
        server: &'a GameServer,
        request: PurchaseRequest,
    ) -> BoxFuture<'a, PurchaseOutcome> {
        Box::pin(async move {
            match purchase_sql(&server.db, &request).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    tracing::error!(
                        "Premium service {} for account {} failed: {e}",
                        request.service.c_id,
                        request.account_id
                    );
                    PurchaseOutcome::Failed
                }
            }
        })
    }
}

/// The loaded catalogue and the purchase store (`server.systems.economy.premium_service`).
#[derive(Debug, Default)]
pub struct PremiumServiceManager {
    services: Mutex<Vec<PremiumService>>,
    /// MySQL ([`MySqlPremiumServiceStore`]) when nothing is installed.
    pub db: HookSlot<dyn PremiumServiceStore>,
}

impl PremiumServiceManager {
    pub fn services(&self) -> Vec<PremiumService> {
        self.services
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replaces the catalogue (loader and tests).
    pub fn set_services(&self, services: Vec<PremiumService>) {
        *self.services.lock().unwrap_or_else(|e| e.into_inner()) = services;
    }

    pub fn service(&self, c_id: i32) -> Option<PremiumService> {
        self.services
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|s| s.c_id == c_id)
            .cloned()
    }

    fn store(&self) -> Arc<dyn PremiumServiceStore> {
        self.db
            .get()
            .unwrap_or_else(|| Arc::new(MySqlPremiumServiceStore))
    }
}

fn manager(server: &GameServer) -> &PremiumServiceManager {
    &server.systems.economy.premium_service
}

/// Registers the three premium service C->S packets (called by [`super::cash_shop::register`]).
pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSPremiumServiceBuyPacket>(LEVEL_PLAIN, cs::CSPremiumServiceBuyPacket);
    reg.register::<CSPremiumServiceListPacket>(LEVEL_PLAIN, cs::CSPremiumServiceListPacket);
    reg.register::<CSPremiumServiceMsgPacket>(LEVEL_PLAIN, cs::CSPremiumServiceMsgPacket);
}

/// The client string field is limited to 400 **bytes** (`FUN_397bf920`); truncate a longer
/// operator name on a UTF-8 boundary instead of letting the client cut a character in half.
fn clamp_name(name: &str) -> String {
    if name.len() <= 400 {
        return name.to_owned();
    }
    let mut end = 400;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    tracing::warn!("premium_services name longer than 400 bytes; truncated");
    name[..end].to_owned()
}

/// `SELECT * FROM premium_services ORDER BY c_id` (called from `economy::System::start`).
pub async fn load(server: &GameServer) -> Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(c_id AS SIGNED) AS c_id, name, CAST(premium_id AS SIGNED) AS premium_id, \
         CAST(hours AS SIGNED) AS hours, CAST(price_type AS SIGNED) AS price_type, \
         CAST(price AS SIGNED) AS price, CAST(grade AS SIGNED) AS grade, \
         CAST(is_sell AS SIGNED) AS is_sell, CAST(is_hidden AS SIGNED) AS is_hidden, \
         CAST(buy_max AS SIGNED) AS buy_max \
         FROM premium_services ORDER BY c_id",
    )
    .fetch_all(&server.db)
    .await?;
    let mut services = Vec::with_capacity(rows.len());
    for row in rows {
        let parsed = (|| -> Result<PremiumService> {
            let c_id = db::int(&row, "c_id")? as i32;
            if c_id == 0 {
                return Err(crate::GameError::Protocol(
                    "premium_services: c_id 0 is not a valid service id".into(),
                ));
            }
            Ok(PremiumService {
                c_id,
                name: clamp_name(&row.try_get::<String, _>("name").unwrap_or_default()),
                premium_id: db::int(&row, "premium_id")?.clamp(0, i64::from(u16::MAX)) as u16,
                hours: db::int(&row, "hours")? as i32,
                price_type: db::int(&row, "price_type")? as u8,
                price: db::int(&row, "price")? as i32,
                grade: db::int(&row, "grade")? as u8,
                is_sell: db::int(&row, "is_sell")? != 0,
                is_hidden: db::int(&row, "is_hidden")? != 0,
                buy_max: db::int(&row, "buy_max")? as i32,
            })
        })();
        match parsed {
            Ok(service) => services.push(service),
            // Skip a bad row instead of failing the whole catalogue.
            Err(e) => tracing::error!("premium_services row skipped: {e}"),
        }
    }
    let count = services.len();
    if count > usize::from(u8::MAX) {
        tracing::warn!(
            "premium_services: {count} rows loaded; only the first {} are served",
            u8::MAX
        );
        services.truncate(usize::from(u8::MAX));
    }
    manager(server).set_services(services);
    tracing::info!("Loaded {count} premium services");
    Ok(())
}

/// The payment a purchase produces: the C#/spec extension rule `pay_end = max(now, pay_end) +
/// hours * 3600`, `pay_method = Premium`. A permanent premium (`pay_end = 0`, the owner default)
/// stays permanent instead of being shortened. A non-premium or expired account starts now.
pub fn extend_payment(current: &AccountPayment, hours: i32, now: i64) -> AccountPayment {
    let seconds = i64::from(hours.max(0)).saturating_mul(3600);
    let premium = current.premium_state(now);
    let (start, end) = if premium && current.end_time == 0 {
        (current.start_time, 0)
    } else if premium {
        (
            current.start_time,
            current.end_time.max(now).saturating_add(seconds),
        )
    } else {
        (now, now.saturating_add(seconds))
    };
    AccountPayment {
        method: PaymentMethodType::Premium,
        location: 1,
        start_time: start,
        end_time: end,
    }
}

fn send_error(buyer: &PlayerRef, error: i16) {
    inventory::send_error_message(buyer, error, 0, true);
}

/// Validates the service, runs the atomic debit + grant under the account lock, publishes the new
/// state and answers with the evidenced `SCErrorMsg` result (see the module docs).
pub async fn execute_buy(server: &GameServer, buyer: &PlayerRef, c_id: i32) -> BuyOutcome {
    let (account_id, character_id) = {
        let c = buyer.data();
        (c.account_id, c.id)
    };
    let Some(service) = manager(server).service(c_id) else {
        tracing::warn!("PremiumServiceBuy: unknown cId {c_id} (account {account_id})");
        send_error(buyer, error_message::PREMIUM_SERVICE_BUY_FAIL);
        return BuyOutcome::Refused;
    };
    if !service.is_sell {
        tracing::warn!(
            "PremiumServiceBuy: service {} is not for sale (account {account_id})",
            service.c_id
        );
        send_error(buyer, error_message::PREMIUM_SERVICE_BUY_FAIL);
        return BuyOutcome::Refused;
    }
    if service.hours <= 0 || service.price < 0 {
        tracing::error!(
            "PremiumServiceBuy: service {} has invalid hours {} / price {}",
            service.c_id,
            service.hours,
            service.price
        );
        send_error(buyer, error_message::PREMIUM_SERVICE_BUY_FAIL);
        return BuyOutcome::Refused;
    }
    let currency = service.price_type;
    if currency == cash_shop::currency::AA_POINTS {
        send_error(buyer, error_message::PREMIUM_SERVICE_NOT_ENOUGH_AA_POINT);
        return BuyOutcome::Refused;
    }
    if currency != cash_shop::currency::CREDITS && currency != cash_shop::currency::LOYALTY {
        tracing::warn!(
            "PremiumServiceBuy: service {} uses unsupported price type {currency}",
            service.c_id
        );
        send_error(buyer, error_message::PREMIUM_SERVICE_BUY_FAIL);
        return BuyOutcome::Refused;
    }

    let now = Utc::now().timestamp();
    let cfg = premium::data(server).config;
    let request = PurchaseRequest {
        account_id,
        character_id,
        service: service.clone(),
        now,
        default_payment: server.config.account.default_payment,
        connect_point: cfg.connect_point,
        max_point: cfg.max_point,
    };
    let lock = server.accounts.lock_of(account_id);
    let guard = lock.lock().await;
    let old_state = crate::systems::services::accounts::state(server, account_id);
    let old_grade = premium::grade_of_state(server, &old_state, now);
    let outcome = manager(server).store().purchase(server, request).await;
    match outcome {
        PurchaseOutcome::Purchased {
            payment,
            premium_point,
            credits,
            loyalty,
        } => {
            let state = AccountState {
                payment,
                premium_point,
            };
            server.accounts.set_state(account_id, state.clone());
            drop(guard);
            premium::publish_state(server, account_id, &state, old_grade);
            if let Some(credits) = credits {
                buyer.send(SCICSCashPointPacket { point: credits });
            }
            if let Some(loyalty) = loyalty {
                {
                    let mut c = buyer.data();
                    c.bm_point = i64::from(loyalty);
                }
                buyer.send(SCBmPointPacket {
                    bm_point: i64::from(loyalty),
                });
            }
            tracing::info!(
                "Account {account_id} bought premium service {} ({})",
                service.c_id,
                service.name
            );
            send_error(buyer, error_message::PREMIUM_SERVICE_BUY_SUCCESS);
            BuyOutcome::Purchased
        }
        PurchaseOutcome::InsufficientFunds => {
            drop(guard);
            let error = if currency == cash_shop::currency::CREDITS {
                error_message::PREMIUM_SERVICE_NOT_ENOUGH_AA_CASH
            } else {
                error_message::PREMIUM_SERVICE_BUY_FAIL
            };
            send_error(buyer, error);
            BuyOutcome::InsufficientFunds
        }
        PurchaseOutcome::PermanentPremium => {
            drop(guard);
            tracing::info!(
                "Account {account_id}: premium service {} refused, the account is permanently                  premium",
                service.c_id
            );
            send_error(buyer, error_message::PREMIUM_SERVICE_BUY_FAIL);
            BuyOutcome::Refused
        }
        PurchaseOutcome::Failed => {
            drop(guard);
            send_error(buyer, error_message::PREMIUM_SERVICE_BUY_FAIL);
            BuyOutcome::Refused
        }
    }
}

/// The production transaction. `aaemu_game.accounts` and `audit_ics_sales` are the real tables;
/// the test entry point substitutes its own tables so it can run against the lane MySQL.
async fn purchase_sql(pool: &MySqlPool, request: &PurchaseRequest) -> Result<PurchaseOutcome> {
    purchase_sql_with_tables(pool, request, "accounts", "audit_ics_sales").await
}

pub(crate) async fn purchase_sql_with_tables(
    pool: &MySqlPool,
    request: &PurchaseRequest,
    accounts_table: &str,
    sales_table: &str,
) -> Result<PurchaseOutcome> {
    debug_assert!(accounts_table
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_'));
    debug_assert!(sales_table
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_'));
    let account_id = request.account_id;
    let mut tx = pool.begin().await?;
    // The login already creates the row. Lock it first: an unconditional `INSERT IGNORE` takes a
    // shared duplicate-check lock on an existing row, and four concurrent purchases deadlocked
    // while upgrading it to the row lock (MySQL 1213). Only a missing row is inserted, with an
    // exclusive `ON DUPLICATE KEY UPDATE` for the race against another first purchase.
    let select_sql = format!(
        "SELECT CAST(credits AS SIGNED) AS credits, CAST(loyalty AS SIGNED) AS loyalty, \
         CAST(pay_method AS SIGNED) AS pay_method, CAST(pay_location AS SIGNED) AS pay_location, \
         CAST(pay_start AS SIGNED) AS pay_start, CAST(pay_end AS SIGNED) AS pay_end, \
         CAST(premium_point AS SIGNED) AS premium_point \
         FROM {accounts_table} WHERE account_id = ? FOR UPDATE"
    );
    let row = match sqlx::query(&select_sql)
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await?
    {
        Some(row) => row,
        None => {
            sqlx::query(&format!(
                "INSERT INTO {accounts_table} (account_id) VALUES (?) \
                 ON DUPLICATE KEY UPDATE account_id = account_id"
            ))
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(&select_sql)
                .bind(account_id)
                .fetch_one(&mut *tx)
                .await?
        }
    };
    let details = AccountDetails {
        account_id: account_id as i32,
        credits: db::int(&row, "credits")? as i32,
        loyalty: db::int(&row, "loyalty")? as i32,
        pay_method: db::int(&row, "pay_method")? as i32,
        pay_location: db::int(&row, "pay_location")? as i32,
        pay_start: db::int(&row, "pay_start")?,
        pay_end: db::int(&row, "pay_end")?,
        premium_point: db::int(&row, "premium_point")? as i32,
        ..AccountDetails::default()
    };
    let (column, credits_balance, loyalty_balance) = match request.service.price_type {
        cash_shop::currency::CREDITS => ("credits", true, false),
        cash_shop::currency::LOYALTY => ("loyalty", false, true),
        _ => {
            tx.rollback().await?;
            return Ok(PurchaseOutcome::Failed);
        }
    };
    let current = premium::resolve_payment(request.default_payment, &details);
    // A permanent premium (pay_end = 0, including the owner-decided default) has no end to
    // extend. Refuse before the debit so nothing is charged.
    if current.premium_state(request.now) && current.end_time == 0 {
        tx.rollback().await?;
        return Ok(PurchaseOutcome::PermanentPremium);
    }
    // A row that still uses `Account:DefaultPayment` keeps pay_method 0; only the purchased
    // window (pay_start/pay_end) is stored, which `resolve_payment` reads back as explicit
    // premium. An explicit payment row becomes Premium.
    let from_default = details.pay_method == 0;
    let pay_method = if from_default {
        0
    } else {
        PaymentMethodType::Premium as i32
    };
    let pay_location = if from_default {
        details.pay_location
    } else {
        1
    };
    let price = request.service.price;
    // Check and take in the same statement: a concurrent second buy matches zero rows.
    let debit = sqlx::query(&format!(
        "UPDATE {accounts_table} SET {column} = {column} - ? \
         WHERE account_id = ? AND {column} >= ?"
    ))
    .bind(price)
    .bind(account_id)
    .bind(price)
    .execute(&mut *tx)
    .await?;
    if debit.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(PurchaseOutcome::InsufficientFunds);
    }
    let mut payment = extend_payment(&current, request.service.hours, request.now);
    payment.location = pay_location;
    // Never lower an existing premium point; the connect point is a floor, capped by max_point.
    let premium_point = details
        .premium_point
        .max(request.connect_point)
        .clamp(0, request.max_point.max(0));
    sqlx::query(&format!(
        "UPDATE {accounts_table} SET pay_method = ?, pay_location = ?, pay_start = ?, \
         pay_end = ?, premium_point = ? WHERE account_id = ?"
    ))
    .bind(pay_method)
    .bind(pay_location)
    .bind(payment.start_time)
    .bind(payment.end_time)
    .bind(premium_point)
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!(
        "INSERT INTO {sales_table} (buyer_account, buyer_char, target_account, target_char, \
         sale_date, shop_item_id, sku, sale_cost, sale_currency, description) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(account_id)
    .bind(request.character_id)
    .bind(account_id)
    .bind(request.character_id)
    .bind(Utc::now().naive_utc())
    .bind(request.service.c_id as u32)
    .bind(0u32)
    .bind(price)
    .bind(request.service.price_type)
    .bind(format!(
        "Premium service {} ({})",
        request.service.c_id, request.service.name
    ))
    .execute(&mut *tx)
    .await?;
    let credits = if credits_balance {
        Some(details.credits.saturating_sub(price))
    } else {
        None
    };
    let loyalty = if loyalty_balance {
        Some(details.loyalty.saturating_sub(price))
    } else {
        None
    };
    tx.commit().await?;
    Ok(PurchaseOutcome::Purchased {
        payment,
        premium_point,
        credits,
        loyalty,
    })
}
