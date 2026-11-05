//! Premium service shop (WP-C): list packet bytes, the atomic purchase, concurrency and the
//! refusals, with an in-memory store; the last test drives the real MySQL transaction when
//! `AAEMU_TEST_MYSQL` is set (private MySQL 3317 only).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aaemu_commons::PacketStream;
use futures::future::BoxFuture;

use crate::config::DefaultPayment;
use crate::db;
use crate::net::connection::{AccountPayment, PaymentMethodType};
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::cash_shop::currency;
use crate::systems::economy::error_message;
use crate::systems::economy::premium_service::*;
use crate::systems::economy::tests::{run, test_data, world, world_with, World};
use crate::systems::services::premium;

fn service(c_id: i32, hours: i32, price_type: u8, price: i32, is_sell: bool) -> PremiumService {
    PremiumService {
        c_id,
        name: format!("Service {c_id}"),
        premium_id: 1,
        hours,
        price_type,
        price,
        grade: 0,
        is_sell,
        is_hidden: false,
        buy_max: 0,
    }
}

fn non_premium() -> AccountPayment {
    AccountPayment {
        method: PaymentMethodType::None,
        location: 1,
        start_time: 0,
        end_time: 0,
    }
}

fn premium_end(end_time: i64) -> AccountPayment {
    AccountPayment {
        method: PaymentMethodType::Premium,
        location: 1,
        start_time: 100,
        end_time,
    }
}

#[derive(Clone, Default)]
struct MemAccount {
    credits: i32,
    loyalty: i32,
    /// `None` = `pay_method = 0`: use `PurchaseRequest::default_payment`.
    payment: Option<AccountPayment>,
    premium_point: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemSale {
    account_id: u32,
    c_id: i32,
    cost: i32,
    currency: u8,
}

/// The production transaction's semantics (check-and-take under one lock) without MySQL.
#[derive(Default)]
struct MemStore {
    accounts: Mutex<HashMap<u32, MemAccount>>,
    sales: Mutex<Vec<MemSale>>,
    calls: Mutex<u32>,
}

impl PremiumServiceStore for MemStore {
    fn purchase<'a>(
        &'a self,
        _server: &'a GameServer,
        request: PurchaseRequest,
    ) -> BoxFuture<'a, PurchaseOutcome> {
        Box::pin(async move {
            *self.calls.lock().unwrap() += 1;
            let mut accounts = self.accounts.lock().unwrap();
            let account = accounts.entry(request.account_id).or_default();
            let current = account
                .payment
                .clone()
                .unwrap_or_else(|| premium::default_payment(request.default_payment));
            // A permanent premium has no end to extend; refuse before any debit.
            if current.premium_state(request.now) && current.end_time == 0 {
                return PurchaseOutcome::PermanentPremium;
            }
            let currency = request.service.price_type;
            let balance = match currency {
                currency::CREDITS => account.credits,
                currency::LOYALTY => account.loyalty,
                _ => return PurchaseOutcome::Failed,
            };
            if balance < request.service.price {
                return PurchaseOutcome::InsufficientFunds;
            }
            match currency {
                currency::CREDITS => account.credits -= request.service.price,
                currency::LOYALTY => account.loyalty -= request.service.price,
                _ => return PurchaseOutcome::Failed,
            }
            let payment = extend_payment(&current, request.service.hours, request.now);
            let premium_point = account
                .premium_point
                .max(request.connect_point)
                .clamp(0, request.max_point.max(0));
            account.payment = Some(payment.clone());
            account.premium_point = premium_point;
            let credits = (currency == currency::CREDITS).then_some(account.credits);
            let loyalty = (currency == currency::LOYALTY).then_some(account.loyalty);
            drop(accounts);
            self.sales.lock().unwrap().push(MemSale {
                account_id: request.account_id,
                c_id: request.service.c_id,
                cost: request.service.price,
                currency,
            });
            PurchaseOutcome::Purchased {
                payment,
                premium_point,
                credits,
                loyalty,
            }
        })
    }
}

fn shop_with_world(w: World, services: Vec<PremiumService>) -> (World, Arc<MemStore>) {
    let store = Arc::new(MemStore::default());
    let premium_service = &w.server.systems.economy.premium_service;
    premium_service.set_services(services);
    premium_service.db.install(store.clone());
    (w, store)
}

fn shop_with(services: Vec<PremiumService>) -> (World, Arc<MemStore>) {
    shop_with_world(world(), services)
}

fn error_body(error: i16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&error.to_le_bytes());
    body.extend_from_slice(&error.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.push(1);
    body
}

#[tokio::test]
async fn list_packets_carry_the_total_row_count() {
    let first = PremiumService {
        c_id: 0x0102_0304,
        name: "Ab".into(),
        premium_id: 9,
        hours: 720,
        price_type: currency::CREDITS,
        price: 300,
        grade: 2,
        is_sell: true,
        is_hidden: false,
        buy_max: 3,
    };
    let second = service(7, 24, currency::LOYALTY, 5, true);
    let third = service(8, 24, currency::CREDITS, 5, true);
    let (mut w, _store) = shop_with(vec![first, second, third]);
    run::<CSPremiumServiceListPacket>(&mut w, 0, &[]);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 3, "one detail per packet (FUN_391e25e0)");
    assert_eq!(frames[0].0, sc::SCPremiumServiceListPacket);
    // Independent literals: isEnd, size (the TOTAL rows), cId, cname, pId, isSell, isHidden,
    // ptime, ptype, price, id, bcount, exchangeRatio.
    let want = vec![
        0, 3, // isEnd false, size 3
        0x04, 0x03, 0x02, 0x01, // cId 0x01020304
        0x02, 0x00, b'A', b'b', // cname "Ab"
        0x09, 0x00, // pId 9
        1, 0, // isSell, isHidden
        0xD0, 0x02, 0x00, 0x00, // ptime 720
        0,    // ptype credits
        0x2C, 0x01, 0x00, 0x00, // price 300
        0x02, 0x00, 0x00, 0x00, // id = grade 2
        0x03, 0x00, 0x00, 0x00, // bcount 3
        0x00, 0x00, 0x00, 0x00, // exchangeRatio
    ];
    assert_eq!(frames[0].1, want);
    assert!(frames.iter().all(|f| f.1[1] == 3), "every packet says 3");
    assert_eq!(frames[1].1[..2], [0, 3]);
    assert_eq!(frames[2].1[..2], [1, 3], "the last detail carries isEnd");
    assert!(w.b.drain().is_empty());
}

#[tokio::test]
async fn empty_catalogue_marks_the_list_ready() {
    let (mut w, _store) = shop_with(Vec::new());
    run::<CSPremiumServiceListPacket>(&mut w, 0, &[]);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    let mut want = vec![1u8, 0u8]; // isEnd true, size 0 (list ready, no detail)
    want.extend_from_slice(&[0u8; 31]); // default detail (27 bytes) + exchangeRatio
    assert_eq!(frames[0].1, want);
}

#[test]
fn buy_packet_decodes_the_evidenced_layout() {
    let body = vec![0x04, 0x03, 0x02, 0x01];
    let packet =
        CSPremiumServiceBuyPacket::read(&mut PacketStream::from_bytes(body)).expect("read");
    assert_eq!(packet.c_id, 0x0102_0304);
    let packet =
        CSPremiumServiceMsgPacket::read(&mut PacketStream::from_bytes(2i32.to_le_bytes().to_vec()))
            .expect("read");
    assert_eq!(packet.stage, 2);
    assert_eq!(
        CSPremiumServiceListPacket::read(&mut PacketStream::from_bytes(Vec::new())).expect("read"),
        CSPremiumServiceListPacket
    );
}

#[tokio::test]
async fn msg_stage_is_a_trigger_and_has_no_reply() {
    let (mut w, _store) = shop_with(Vec::new());
    run::<CSPremiumServiceMsgPacket>(&mut w, 0, &1i32.to_le_bytes());
    run::<CSPremiumServiceMsgPacket>(&mut w, 0, &2i32.to_le_bytes());
    assert!(w.a.drain().is_empty());
    assert!(w.b.drain().is_empty());
}

#[test]
fn extend_payment_rules() {
    let now = 1_800_000_000;
    let day = 24 * 3600;
    // A non-premium account starts at now.
    let payment = extend_payment(&non_premium(), 24, now);
    assert_eq!((payment.start_time, payment.end_time), (now, now + day));
    // An active premium extends its own end.
    let payment = extend_payment(&premium_end(now + 1000), 24, now);
    assert_eq!(
        (payment.start_time, payment.end_time),
        (100, now + 1000 + day)
    );
    // An expired one restarts now instead of extending the stale end.
    let payment = extend_payment(&premium_end(now - 1), 24, now);
    assert_eq!((payment.start_time, payment.end_time), (now, now + day));
    // A future GM window is treated as not premium yet and replaced by now..now+hours.
    let future = AccountPayment {
        method: PaymentMethodType::Premium,
        location: 1,
        start_time: now + 1000,
        end_time: now + 2000,
    };
    let payment = extend_payment(&future, 24, now);
    assert_eq!((payment.start_time, payment.end_time), (now, now + day));
    // The owner default is permanent; this branch is defensive (the buy is refused first).
    let payment = extend_payment(
        &AccountPayment {
            method: PaymentMethodType::Premium,
            location: 1,
            start_time: 0,
            end_time: 0,
        },
        24,
        now,
    );
    assert_eq!((payment.start_time, payment.end_time), (0, 0));
    assert_eq!(payment.method, PaymentMethodType::Premium);
}

#[tokio::test]
async fn buy_debits_once_and_grants_once() {
    let (mut w, store) = shop_with(vec![service(8_000_001, 720, currency::CREDITS, 300, true)]);
    store.accounts.lock().unwrap().insert(
        1001,
        MemAccount {
            credits: 1000,
            payment: Some(non_premium()),
            ..MemAccount::default()
        },
    );
    let buyer = w.pa.clone();
    let before = chrono::Utc::now().timestamp();
    let outcome = execute_buy(&w.server, &buyer, 8_000_001).await;
    assert_eq!(outcome, BuyOutcome::Purchased);
    let account = store.accounts.lock().unwrap()[&1001].clone();
    assert_eq!(account.credits, 700, "debited exactly once");
    let payment = account.payment.expect("payment");
    assert_eq!(payment.method, PaymentMethodType::Premium);
    let seconds = 720 * 3600;
    assert!(payment.end_time >= before + seconds);
    assert!(payment.end_time <= chrono::Utc::now().timestamp() + seconds);
    assert_eq!(
        account.premium_point,
        premium::data(&w.server).config.connect_point
    );
    assert_eq!(
        store.sales.lock().unwrap().len(),
        1,
        "granted / audited once"
    );
    let frames = w.a.drain();
    assert_eq!(
        frames.iter().map(|f| f.0).collect::<Vec<_>>(),
        vec![
            sc::SCAccountInfoPacket,
            sc::SCUpdatePremiumPointPacket,
            sc::SCICSCashPointPacket,
            sc::SCErrorMsgPacket,
        ]
    );
    assert_eq!(frames[2].1, 700i32.to_le_bytes().to_vec());
    assert_eq!(
        frames[3].1,
        error_body(error_message::PREMIUM_SERVICE_BUY_SUCCESS)
    );
}

#[tokio::test]
async fn a_higher_stored_premium_point_is_kept() {
    let mut data = test_data();
    data.systems.services.premium.config.max_point = 10;
    data.systems.services.premium.config.connect_point = 1;
    let (w, store) = shop_with_world(
        world_with(data),
        vec![service(9, 24, currency::CREDITS, 300, true)],
    );
    store.accounts.lock().unwrap().insert(
        1001,
        MemAccount {
            credits: 300,
            payment: Some(non_premium()),
            premium_point: 5,
            ..MemAccount::default()
        },
    );
    let buyer = w.pa.clone();
    assert_eq!(
        execute_buy(&w.server, &buyer, 9).await,
        BuyOutcome::Purchased
    );
    let account = store.accounts.lock().unwrap()[&1001].clone();
    assert_eq!(account.premium_point, 5, "the stored point is not lowered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_buys_cannot_double_spend() {
    let (mut w, store) = shop_with(vec![service(9, 24, currency::CREDITS, 300, true)]);
    store.accounts.lock().unwrap().insert(
        1001,
        MemAccount {
            credits: 300,
            payment: Some(non_premium()),
            ..MemAccount::default()
        },
    );
    let buyer = w.pa.clone();
    let server = w.server.clone();
    let outcomes = futures::future::join_all((0..4).map(|_| {
        let server = server.clone();
        let buyer = buyer.clone();
        async move { execute_buy(&server, &buyer, 9).await }
    }))
    .await;
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == BuyOutcome::Purchased)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == BuyOutcome::InsufficientFunds)
            .count(),
        3
    );
    let account = store.accounts.lock().unwrap()[&1001].clone();
    assert_eq!(account.credits, 0);
    assert_eq!(store.sales.lock().unwrap().len(), 1);
    let day = 24 * 3600;
    let payment = account.payment.expect("payment");
    assert!(payment.end_time <= chrono::Utc::now().timestamp() + day + 2);
    let frames = w.a.drain();
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.1 == error_body(error_message::PREMIUM_SERVICE_BUY_SUCCESS))
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.1 == error_body(error_message::PREMIUM_SERVICE_NOT_ENOUGH_AA_CASH))
            .count(),
        3
    );
}

#[tokio::test]
async fn insufficient_funds_and_unknown_or_unsellable_services_change_nothing() {
    // Insufficient credits: 774, no debit, no state, no sale.
    let (mut w, store) = shop_with(vec![service(1, 24, currency::CREDITS, 300, true)]);
    store.accounts.lock().unwrap().insert(
        1001,
        MemAccount {
            credits: 50,
            payment: Some(non_premium()),
            ..MemAccount::default()
        },
    );
    let buyer = w.pa.clone();
    assert_eq!(
        execute_buy(&w.server, &buyer, 1).await,
        BuyOutcome::InsufficientFunds
    );
    assert_eq!(store.accounts.lock().unwrap()[&1001].credits, 50);
    let payment = store.accounts.lock().unwrap()[&1001]
        .payment
        .clone()
        .expect("payment");
    assert_eq!(payment.method, PaymentMethodType::None);
    assert!(store.sales.lock().unwrap().is_empty());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::PREMIUM_SERVICE_NOT_ENOUGH_AA_CASH)
        )]
    );

    // Unknown id, not for sale, AA points and an unsupported currency: refused before the store.
    let (mut w, store) = shop_with(vec![
        service(1, 24, currency::CREDITS, 300, false),
        service(2, 24, currency::AA_POINTS, 300, true),
        service(3, 24, currency::COINS, 300, true),
        service(4, 0, currency::CREDITS, 300, true),
        service(5, 24, currency::CREDITS, -1, true),
    ]);
    let buyer = w.pa.clone();
    for (c_id, error) in [
        (999, error_message::PREMIUM_SERVICE_BUY_FAIL),
        (1, error_message::PREMIUM_SERVICE_BUY_FAIL),
        (2, error_message::PREMIUM_SERVICE_NOT_ENOUGH_AA_POINT),
        (3, error_message::PREMIUM_SERVICE_BUY_FAIL),
        (4, error_message::PREMIUM_SERVICE_BUY_FAIL),
        (5, error_message::PREMIUM_SERVICE_BUY_FAIL),
    ] {
        assert_eq!(
            execute_buy(&w.server, &buyer, c_id).await,
            BuyOutcome::Refused
        );
        assert_eq!(
            w.a.drain(),
            vec![(sc::SCErrorMsgPacket, error_body(error))],
            "cId {c_id}"
        );
    }
    assert_eq!(*store.calls.lock().unwrap(), 0, "no store call on refusals");
}

#[tokio::test]
async fn permanent_premium_is_refused_without_a_debit() {
    // Explicit permanent premium.
    let (mut w, store) = shop_with(vec![service(9, 24, currency::CREDITS, 300, true)]);
    store.accounts.lock().unwrap().insert(
        1001,
        MemAccount {
            credits: 300,
            payment: Some(premium_end(0)),
            ..MemAccount::default()
        },
    );
    let buyer = w.pa.clone();
    assert_eq!(execute_buy(&w.server, &buyer, 9).await, BuyOutcome::Refused);
    let account = store.accounts.lock().unwrap()[&1001].clone();
    assert_eq!(account.credits, 300, "nothing debited");
    assert_eq!(account.payment.unwrap().end_time, 0, "window unchanged");
    assert!(store.sales.lock().unwrap().is_empty());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::PREMIUM_SERVICE_BUY_FAIL)
        )]
    );

    // The configured default is permanent premium too (`pay_method = 0`).
    let (w, store) = shop_with(vec![service(9, 24, currency::CREDITS, 300, true)]);
    store.accounts.lock().unwrap().insert(
        1001,
        MemAccount {
            credits: 300,
            ..MemAccount::default()
        },
    );
    let buyer = w.pa.clone();
    assert_eq!(execute_buy(&w.server, &buyer, 9).await, BuyOutcome::Refused);
    assert_eq!(store.accounts.lock().unwrap()[&1001].credits, 300);
    assert!(store.sales.lock().unwrap().is_empty());
}

#[tokio::test]
async fn loyalty_price_is_debited_and_pushes_the_bm_point() {
    let (mut w, store) = shop_with(vec![service(9, 24, currency::LOYALTY, 5, true)]);
    store.accounts.lock().unwrap().insert(
        1001,
        MemAccount {
            loyalty: 7,
            payment: Some(non_premium()),
            ..MemAccount::default()
        },
    );
    let buyer = w.pa.clone();
    assert_eq!(
        execute_buy(&w.server, &buyer, 9).await,
        BuyOutcome::Purchased
    );
    let account = store.accounts.lock().unwrap()[&1001].clone();
    assert_eq!(account.loyalty, 2);
    assert_eq!(buyer.data().bm_point, 2);
    assert_eq!(store.sales.lock().unwrap()[0].currency, currency::LOYALTY);
    // Loyalty pushes SCBmPointPacket (not the cash point packet).
    let frames = w.a.drain();
    assert!(!frames.iter().any(|f| f.0 == sc::SCICSCashPointPacket));
    let bm = frames
        .iter()
        .find(|f| f.0 == sc::SCBmPointPacket)
        .expect("SCBmPointPacket");
    assert_eq!(bm.1, 2i64.to_le_bytes().to_vec());
    assert_eq!(
        frames.last().unwrap().1,
        error_body(error_message::PREMIUM_SERVICE_BUY_SUCCESS)
    );
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses lane-local test tables"]
async fn private_mysql_purchase_is_atomic_and_persistent() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();
    let accounts = "premium_service_test_accounts";
    let sales = "premium_service_test_sales";
    let sales_fail = "premium_service_test_sales_fail";
    for table in [accounts, sales, sales_fail] {
        sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    // Production schema: `accounts` columns as in SQL/aaemu_game.sql and the `audit_ics_sales`
    // columns with their UNSIGNED widths.
    sqlx::query(&format!(
        "CREATE TABLE {accounts} (account_id INT NOT NULL, credits INT NOT NULL DEFAULT 0, \
         loyalty INT NOT NULL DEFAULT 0, pay_method TINYINT NOT NULL DEFAULT 0, \
         pay_location INT NOT NULL DEFAULT 1, pay_start BIGINT NOT NULL DEFAULT 0, \
         pay_end BIGINT NOT NULL DEFAULT 0, premium_point INT NOT NULL DEFAULT 0, \
         PRIMARY KEY (account_id))"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let sales_schema = "id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, \
         buyer_account INT UNSIGNED NOT NULL DEFAULT 0, buyer_char INT UNSIGNED NOT NULL DEFAULT 0, \
         target_account INT UNSIGNED NOT NULL DEFAULT 0, \
         target_char INT UNSIGNED NOT NULL DEFAULT 0, \
         sale_date DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, \
         shop_item_id INT UNSIGNED NOT NULL DEFAULT 0, sku INT UNSIGNED NOT NULL DEFAULT 0, \
         sale_cost INT NOT NULL DEFAULT 0, sale_currency TINYINT UNSIGNED NOT NULL DEFAULT 0, \
         description TEXT NOT NULL, PRIMARY KEY (id)";
    sqlx::query(&format!("CREATE TABLE {sales} ({sales_schema})"))
        .execute(&pool)
        .await
        .unwrap();
    // The failure table passes every check except the audit insert: the CHECK constraint fails
    // after the debit, so the transaction must roll it back.
    sqlx::query(&format!(
        "CREATE TABLE {sales_fail} ({sales_schema}, CONSTRAINT sale_cost_must_be_negative \
         CHECK (sale_cost < 0))"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {accounts} (account_id, credits) VALUES (1, 300)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let request = || PurchaseRequest {
        account_id: 1,
        character_id: 2,
        service: service(9, 24, currency::CREDITS, 300, true),
        now: 1_800_000_000,
        default_payment: DefaultPayment::None,
        connect_point: 1,
        max_point: 1,
    };
    let (r1, r2, r3, r4) = (request(), request(), request(), request());
    let (a, b, c, d) = tokio::join!(
        purchase_sql_with_tables(&pool, &r1, accounts, sales),
        purchase_sql_with_tables(&pool, &r2, accounts, sales),
        purchase_sql_with_tables(&pool, &r3, accounts, sales),
        purchase_sql_with_tables(&pool, &r4, accounts, sales)
    );
    let outcomes = [a.unwrap(), b.unwrap(), c.unwrap(), d.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, PurchaseOutcome::Purchased { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, PurchaseOutcome::InsufficientFunds))
            .count(),
        3
    );
    let row = sqlx::query(&format!(
        "SELECT CAST(credits AS SIGNED) AS credits, CAST(pay_method AS SIGNED) AS pay_method, \
         CAST(pay_start AS SIGNED) AS pay_start, CAST(pay_end AS SIGNED) AS pay_end, \
         CAST(pay_location AS SIGNED) AS pay_location, \
         CAST(premium_point AS SIGNED) AS premium_point \
         FROM {accounts} WHERE account_id = 1"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(db::int(&row, "credits").unwrap(), 0);
    // The row came from Account:DefaultPayment, so pay_method stays 0 and the window is stored.
    assert_eq!(db::int(&row, "pay_method").unwrap(), 0);
    assert_eq!(db::int(&row, "pay_start").unwrap(), 1_800_000_000);
    assert_eq!(db::int(&row, "pay_end").unwrap(), 1_800_000_000 + 24 * 3600);
    assert_eq!(db::int(&row, "premium_point").unwrap(), 1);
    // The stored window resolves as an explicit premium, not the configured default.
    let details = db::AccountDetails {
        pay_method: 0,
        pay_location: db::int(&row, "pay_location").unwrap() as i32,
        pay_start: db::int(&row, "pay_start").unwrap(),
        pay_end: db::int(&row, "pay_end").unwrap(),
        ..db::AccountDetails::default()
    };
    let resolved = premium::resolve_payment(DefaultPayment::None, &details);
    assert_eq!(resolved.method, PaymentMethodType::Premium);
    assert!(resolved.premium_state(1_800_000_000 + 5));
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {sales}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "one debit, one grant, one sale row");

    // Persistence: the next purchase reads the committed row back and extends its end.
    sqlx::query(&format!(
        "UPDATE {accounts} SET credits = 100 WHERE account_id = 1"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let mut next = request();
    next.service.hours = 1;
    next.service.price = 50;
    next.now = 1_800_000_000 + 10;
    assert!(matches!(
        purchase_sql_with_tables(&pool, &next, accounts, sales)
            .await
            .unwrap(),
        PurchaseOutcome::Purchased { .. }
    ));
    let row = sqlx::query(&format!(
        "SELECT CAST(pay_end AS SIGNED) AS pay_end FROM {accounts} WHERE account_id = 1"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        db::int(&row, "pay_end").unwrap(),
        1_800_000_000 + 24 * 3600 + 3600
    );

    // Insufficient funds leave the stored row untouched.
    let mut poor = request();
    poor.service.price = 10_000;
    poor.now = 1_800_000_000 + 20;
    assert!(matches!(
        purchase_sql_with_tables(&pool, &poor, accounts, sales)
            .await
            .unwrap(),
        PurchaseOutcome::InsufficientFunds
    ));
    let row = sqlx::query(&format!(
        "SELECT CAST(credits AS SIGNED) AS credits, CAST(pay_end AS SIGNED) AS pay_end \
         FROM {accounts} WHERE account_id = 1"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(db::int(&row, "credits").unwrap(), 50);
    assert_eq!(
        db::int(&row, "pay_end").unwrap(),
        1_800_000_000 + 24 * 3600 + 3600
    );
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {sales}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);

    // Loyalty buys debit the loyalty column, not credits, through the same transaction.
    sqlx::query(&format!(
        "UPDATE {accounts} SET loyalty = 60 WHERE account_id = 1"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let mut loyalty = request();
    loyalty.service.price_type = currency::LOYALTY;
    loyalty.service.price = 60;
    loyalty.service.hours = 1;
    loyalty.now = 1_800_000_000 + 30;
    assert!(matches!(
        purchase_sql_with_tables(&pool, &loyalty, accounts, sales)
            .await
            .unwrap(),
        PurchaseOutcome::Purchased {
            credits: None,
            loyalty: Some(0),
            ..
        }
    ));
    let row = sqlx::query(&format!(
        "SELECT CAST(credits AS SIGNED) AS credits, CAST(loyalty AS SIGNED) AS loyalty, \
         CAST(pay_end AS SIGNED) AS pay_end FROM {accounts} WHERE account_id = 1"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(db::int(&row, "credits").unwrap(), 50);
    assert_eq!(db::int(&row, "loyalty").unwrap(), 0);
    assert_eq!(
        db::int(&row, "pay_end").unwrap(),
        1_800_000_000 + 24 * 3600 + 3600 + 3600
    );
    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {sales} WHERE sale_currency = 2"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "the loyalty sale is audited with its currency");

    // A higher stored premium point is only raised to the connect point, never lowered.
    sqlx::query(&format!(
        "UPDATE {accounts} SET premium_point = 5 WHERE account_id = 1"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let mut point = request();
    point.service.price = 1;
    point.service.hours = 1;
    point.now = 1_800_000_000 + 40;
    point.connect_point = 1;
    point.max_point = 10;
    assert!(matches!(
        purchase_sql_with_tables(&pool, &point, accounts, sales)
            .await
            .unwrap(),
        PurchaseOutcome::Purchased {
            credits: Some(49),
            premium_point: 5,
            ..
        }
    ));
    let row = sqlx::query(&format!(
        "SELECT CAST(premium_point AS SIGNED) AS premium_point \
         FROM {accounts} WHERE account_id = 1"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(db::int(&row, "premium_point").unwrap(), 5);

    // Permanent premium: refused before the debit, nothing written.
    sqlx::query(&format!(
        "INSERT INTO {accounts} (account_id, credits) VALUES (4, 100)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let mut permanent = request();
    permanent.account_id = 4;
    permanent.default_payment = DefaultPayment::Premium;
    permanent.now = 1_800_000_000 + 50;
    assert!(matches!(
        purchase_sql_with_tables(&pool, &permanent, accounts, sales)
            .await
            .unwrap(),
        PurchaseOutcome::PermanentPremium
    ));
    let row = sqlx::query(&format!(
        "SELECT CAST(credits AS SIGNED) AS credits, CAST(pay_method AS SIGNED) AS pay_method, \
         CAST(pay_start AS SIGNED) AS pay_start, CAST(pay_end AS SIGNED) AS pay_end \
         FROM {accounts} WHERE account_id = 4"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(db::int(&row, "credits").unwrap(), 100, "nothing debited");
    assert_eq!(db::int(&row, "pay_method").unwrap(), 0);
    assert_eq!(db::int(&row, "pay_start").unwrap(), 0);
    assert_eq!(db::int(&row, "pay_end").unwrap(), 0);

    // The audit insert fails after the debit (CHECK); the whole transaction rolls back.
    sqlx::query(&format!(
        "INSERT INTO {accounts} (account_id, credits) VALUES (5, 100)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let mut failing = request();
    failing.account_id = 5;
    failing.service.price = 50;
    failing.now = 1_800_000_000 + 60;
    assert!(
        purchase_sql_with_tables(&pool, &failing, accounts, sales_fail)
            .await
            .is_err(),
        "the failing audit insert must surface as an error"
    );
    let row = sqlx::query(&format!(
        "SELECT CAST(credits AS SIGNED) AS credits, CAST(pay_method AS SIGNED) AS pay_method, \
         CAST(pay_start AS SIGNED) AS pay_start, CAST(pay_end AS SIGNED) AS pay_end, \
         CAST(premium_point AS SIGNED) AS premium_point \
         FROM {accounts} WHERE account_id = 5"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(db::int(&row, "credits").unwrap(), 100, "debit rolled back");
    assert_eq!(db::int(&row, "pay_method").unwrap(), 0);
    assert_eq!(db::int(&row, "pay_start").unwrap(), 0);
    assert_eq!(db::int(&row, "pay_end").unwrap(), 0);
    assert_eq!(db::int(&row, "premium_point").unwrap(), 0);

    for table in [sales_fail, sales, accounts] {
        sqlx::query(&format!("DROP TABLE {table}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}
