//! Tests for the automatic declaration-auction posting (WP-SD1c).

use chrono::{TimeZone, Utc};

use crate::data::items::ItemTemplate;
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::server::GameServer;
use crate::systems::economy::auction::{auction_duration, AuctionBid};
use crate::systems::economy::inventory::Inv;
use crate::systems::housing::data::{SiegePlan, SiegeZone};
use crate::systems::housing::dominion::auction_post;
use crate::systems::housing::dominion::state::TowerIdentity;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

const ZONE: u32 = 25;
const OTHER_ZONE: u32 = 26;
const LATER_ZONE: u32 = 43;
const EXPEDITION: u32 = 77;
const SETTING: u32 = 7;
const CAMP: u32 = 21_130;
const OTHER_CAMP: u32 = 21_134;
const LATER_CAMP: u32 = 21_135;
const PRICE: i32 = 5_000_000;
/// A claim well before the 2014 cycles, so the 30-day fresh-claim protection has expired.
const CLAIM_AT: i64 = 1_385_932_800; // 2013-12-01 00:00 UTC

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .unwrap()
        .timestamp()
}

/// The shipped `siege_zones` schedule (auction Wed 22:30 -> declare Fri 22:30 -> warmup Sun
/// 20:30 -> siege Sun 21:00 for 90 min) with the caller's camp item.
fn schedule(zone_group_id: u32, declare_item_id: u32) -> SiegeZone {
    SiegeZone {
        id: zone_group_id,
        zone_group_id,
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
        declare_item_id,
        open_weekday: 0,
        open_hour: 0,
        open_duration_hours: 168,
        ..Default::default()
    }
}

fn plan(zone_group_id: u32, id: u32, week: &str) -> SiegePlan {
    SiegePlan {
        id,
        zone_group_id,
        week_start: chrono::NaiveDateTime::parse_from_str(
            &format!("{week} 00:00:00"),
            "%Y-%m-%d %H:%M:%S",
        )
        .unwrap(),
    }
}

struct Fixture {
    server: GameServer,
}

/// Three zones: 25/26 run the week of 2014-01-05 (the shipped pair), 43 runs the week of
/// 2014-01-12. All are owned by the same expedition.
fn fixture() -> Fixture {
    let mut data = GameData::empty();
    for (zone, item) in [
        (ZONE, CAMP),
        (OTHER_ZONE, OTHER_CAMP),
        (LATER_ZONE, LATER_CAMP),
    ] {
        data.systems
            .housing
            .castles
            .siege_zones
            .insert(zone, schedule(zone, item));
        data.items.templates.insert(
            item,
            ItemTemplate {
                price: PRICE,
                auction_category_a: 11,
                ..ItemTemplate::plain(item, 1)
            },
        );
    }
    let mut plan_id = 1;
    for (zone, week) in [
        (ZONE, "2014-01-05"),
        (OTHER_ZONE, "2014-01-05"),
        (LATER_ZONE, "2014-01-12"),
    ] {
        for _ in 0..2 {
            data.systems
                .housing
                .castles
                .siege_plans
                .entry(zone)
                .or_default()
                .push(plan(zone, plan_id, week));
            plan_id += 1;
        }
    }
    let server = test_server_with(data);
    server.config.update(|c| c.housing.siege_calendar = true);
    for (zone, house_id) in [(ZONE, 61), (OTHER_ZONE, 62), (LATER_ZONE, 63)] {
        let identity = TowerIdentity {
            house_id,
            instance_id: 0,
            guard_tower_setting_id: SETTING,
        };
        let dominions = &server.systems.housing.dominions;
        dominions.register_tower(zone, identity).unwrap();
        dominions
            .declare_unowned_with(zone, identity, EXPEDITION, CLAIM_AT, || true)
            .unwrap();
    }
    Fixture { server }
}

fn lots(server: &GameServer) -> Vec<crate::systems::economy::auction::AuctionLot> {
    server.systems.economy.auction.all_lots()
}

#[tokio::test]
async fn auction_start_posts_one_system_lot_per_running_zone_from_the_authored_data() {
    let f = fixture();
    let auction = utc(2014, 1, 8, 22, 30);
    let declare = utc(2014, 1, 10, 22, 30);
    let warmup = utc(2014, 1, 12, 20, 30);
    auction_post::tick(&f.server, auction + 60);

    let mut posted = lots(&f.server);
    posted.sort_by_key(|lot| lot.item_template_id());
    // Two zones run this week's cycle; the third zone runs three weeks later.
    assert_eq!(posted.len(), 2);
    for (lot, item_template_id) in posted.iter().zip([CAMP, OTHER_CAMP]) {
        assert_eq!(lot.item_template_id(), item_template_id);
        assert!(lot.is_system_lot(), "the camp lot has no player seller");
        assert_eq!((lot.client_id, lot.client_name.as_str()), (0, ""));
        assert_eq!((lot.start_money, lot.direct_money), (PRICE, 0));
        assert_eq!(lot.duration, auction_duration::HOURS_48);
        assert_eq!(lot.end_time - lot.post_date, chrono::Duration::hours(48));
        // The lot covers the authored cycle window, not the tick instant.
        assert_eq!(lot.post_date.timestamp(), auction);
        assert_eq!(lot.end_time.timestamp(), declare);
        let item = f
            .server
            .items
            .item_by_id(lot.item_id())
            .expect("the camp item exists");
        assert_eq!(
            (item.template_id, item.count, item.owner_id, item.slot_type),
            (item_template_id, 1, 0, slot_type::AUCTION)
        );
        // The won camp expires when the declare period ends (the warmup start).
        assert_eq!(item.expiration_time, warmup);
    }
    assert!(!posted
        .iter()
        .any(|lot| lot.item_template_id() == LATER_CAMP));
}

#[tokio::test]
async fn the_dominion_tick_posts_the_camp_lots_at_the_auction_start() {
    let f = fixture();
    let auction = utc(2014, 1, 8, 22, 30);
    f.server
        .systems
        .housing
        .calendar
        .set_clock(Some(auction + 60));
    crate::systems::housing::dominion::runtime::tick_once(&f.server);
    assert_eq!(lots(&f.server).len(), 2);
}

#[tokio::test]
async fn posting_is_idempotent_on_a_repeated_tick_and_across_a_restart() {
    let f = fixture();
    let auction = utc(2014, 1, 8, 22, 30);
    auction_post::tick(&f.server, auction + 60);
    assert_eq!(lots(&f.server).len(), 2);
    assert_eq!(
        f.server
            .systems
            .housing
            .dominions
            .get(ZONE)
            .unwrap()
            .auction_posted_cycle,
        auction
    );

    // The same cycle's later ticks post nothing.
    auction_post::tick(&f.server, auction + 120);
    auction_post::tick(&f.server, auction + 3_600);
    assert_eq!(lots(&f.server).len(), 2);

    // A restart reloads the authority rows (the marker included) and the lots; the first tick
    // of the next process must not post a second time.
    let states = f.server.systems.housing.dominions.all();
    f.server.systems.housing.dominions.restore(states).unwrap();
    auction_post::tick(&f.server, auction + 180);
    assert_eq!(lots(&f.server).len(), 2);
    assert_eq!(
        f.server
            .systems
            .housing
            .dominions
            .get(ZONE)
            .unwrap()
            .auction_posted_cycle,
        auction
    );
}

#[tokio::test]
async fn a_later_rotation_week_posts_its_own_zones_lots() {
    let f = fixture();
    // Week of 2014-01-12: only zone 43 (its own cycle) posts.
    let later_auction = utc(2014, 1, 15, 22, 30);
    auction_post::tick(&f.server, later_auction + 60);
    let posted = lots(&f.server);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].item_template_id(), LATER_CAMP);

    // Week of 2014-01-26: zones 25/26 run again and get a new lot each.
    let second_auction = utc(2014, 1, 29, 22, 30);
    auction_post::tick(&f.server, second_auction + 60);
    let posted = lots(&f.server);
    assert_eq!(posted.len(), 3);
    assert_eq!(
        posted
            .iter()
            .filter(|lot| lot.item_template_id() == CAMP)
            .count(),
        1
    );
    assert_eq!(
        f.server
            .systems
            .housing
            .dominions
            .get(ZONE)
            .unwrap()
            .auction_posted_cycle,
        second_auction
    );
}

#[tokio::test]
async fn a_won_camp_item_expires_when_the_declare_period_ends() {
    let f = fixture();
    crate::systems::economy::wiring::install(&f.server);
    let auction = utc(2014, 1, 8, 22, 30);
    let warmup = utc(2014, 1, 12, 20, 30);
    auction_post::tick(&f.server, auction + 60);

    let lot = lots(&f.server)
        .into_iter()
        .find(|lot| lot.item_template_id() == CAMP)
        .expect("the camp lot");
    let item_id = lot.item_id();
    // The bid must happen while the auction is running; the fixture's cycle is historical.
    let mut running = lot.clone();
    running.end_time = Utc::now() + chrono::Duration::hours(1);
    f.server.systems.economy.auction.set_lot(running.clone());

    let client = TestClient::new(&f.server, 9, 1009);
    let mut character = test_character(9, "Bidder", 100.0, 100.0);
    character.money = i64::from(PRICE) * 2;
    let buyer = spawn_player(&f.server, &client, character, 0x300);
    for (_, container) in Inv::of(&buyer).containers {
        f.server.items.add_container(container);
    }
    f.server.names.add_character(9, "Bidder", 1009);

    f.server.systems.economy.auction.bid_on_auction_lot(
        &f.server,
        &buyer,
        running.id,
        AuctionBid {
            lot_id: running.id,
            world_id: 0,
            bidder_id: 9,
            bidder_name: "Bidder".into(),
            money: PRICE,
        },
    );
    assert_eq!(
        f.server
            .systems
            .economy
            .auction
            .lot(running.id)
            .unwrap()
            .bidder_id,
        9
    );

    // The auction ends: the winner is mailed the camp item, which keeps its declare-period
    // expiry.
    f.server
        .systems
        .economy
        .auction
        .update_auction_house(&f.server, running.end_time + chrono::Duration::seconds(1));
    assert!(f.server.systems.economy.auction.lot(running.id).is_none());
    let buyer_mail = f
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .find(|(_, mail)| {
            mail.mail_type == crate::systems::economy::mail::mail_type::AUC_BID_WIN
                && mail.header.receiver_id == 9
        })
        .map(|(_, mail)| mail.clone())
        .expect("the winner's mail");
    assert_eq!(buyer_mail.body.attachments, vec![item_id]);
    let delivered = f.server.items.item_by_id(item_id).expect("the item");
    assert_eq!(delivered.expiration_time, warmup);
}

#[tokio::test]
async fn a_disabled_calendar_posts_nothing() {
    let f = fixture();
    f.server.config.update(|c| c.housing.siege_calendar = false);
    auction_post::tick(&f.server, utc(2014, 1, 8, 22, 30) + 60);
    assert!(lots(&f.server).is_empty());
}

#[tokio::test]
async fn a_zone_without_a_camp_item_posts_nothing() {
    let mut f = fixture();
    std::sync::Arc::get_mut(&mut f.server.game_data)
        .unwrap()
        .systems
        .housing
        .castles
        .siege_zones
        .get_mut(&ZONE)
        .unwrap()
        .declare_item_id = 0;
    auction_post::tick(&f.server, utc(2014, 1, 8, 22, 30) + 60);
    let posted = lots(&f.server);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].item_template_id(), OTHER_CAMP);
}

/// The authored prices and the 48 h auction gap behind the automatic posting.
/// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game auction_post_real_compact_db -- --ignored`.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn auction_post_real_compact_db_prices_and_auction_gap() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let castles = crate::systems::housing::data::CastleReferenceData::load(&conn).unwrap();
    let mut items: Vec<u32> = castles
        .siege_zones
        .values()
        .map(|zone| zone.declare_item_id)
        .filter(|id| *id != 0)
        .collect();
    items.sort_unstable();
    items.dedup();
    assert_eq!(items.len(), 6, "the six Siege Camps");
    for item in items {
        let price: i64 = conn
            .query_row("SELECT price FROM items WHERE id=?", [item], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(price, i64::from(PRICE), "item {item}");
    }
    for zone in castles.siege_zones.values() {
        let auction = &zone;
        assert_eq!(auction.start_auction_weekday, 3);
        assert_eq!(
            (
                auction.start_declare_weekday,
                auction.start_declare_hour,
                auction.start_declare_min
            ),
            (5, 22, 30),
            "the declare start is 48 h after the auction start"
        );
    }
    // The rotation pairs the six zones two per week, so a running cycle owns two camp items:
    // that is the lot count the automatic posting takes from the data.
    let rotation = castles.siege_rotation();
    for week in ["2014-01-05", "2014-01-12", "2014-01-19"] {
        let date = chrono::NaiveDate::parse_from_str(week, "%Y-%m-%d").unwrap();
        assert_eq!(
            rotation.week(date).map(|zones| zones.len()),
            Some(2),
            "the week of {week}"
        );
    }
}
