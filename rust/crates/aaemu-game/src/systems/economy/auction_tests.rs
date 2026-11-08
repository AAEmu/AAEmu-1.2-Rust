//! Auction house tests: byte-exact packets, C->S decoding, and the list -> search -> bid ->
//! buyout / cancel / expiry flows through the real handlers (no MySQL).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aaemu_commons::{Marshal, PacketStream};
use chrono::{Duration, TimeZone, Utc};

use crate::data::items::ItemTemplate;
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemTask};
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::auction::*;
use crate::systems::economy::auction_packets::*;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::tests::{
    give, run, spawn_service_npc, task_body, test_data, World, AUCTIONEER_TEMPLATE,
};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

const SWORD: u32 = 5001; // "Iron Sword", level 20, categories 1/2/3
const POTION: u32 = 5002; // "Healing Potion", level 5, categories 4/0/0
const SIEGE_CAMP: u32 = 5003; // "Siege Camp", category 11 (declare_siege)

#[derive(Default)]
struct MailLog(Mutex<Vec<AuctionMail>>);

impl AuctionMailHooks for MailLog {
    fn send_auction_mail(&self, _server: &GameServer, mail: AuctionMail) -> bool {
        self.0.lock().unwrap().push(mail);
        true
    }
}

impl MailLog {
    fn take(&self) -> Vec<AuctionMail> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// A mail hook that records every attempt and can refuse deliveries: nothing is delivered
/// unless `accept` is set, and `fail_buyer` fails `SaleBuyer` even then.
#[derive(Default)]
struct FlakyMailLog {
    attempts: Mutex<Vec<AuctionMail>>,
    delivered: Mutex<Vec<AuctionMail>>,
    accept: AtomicBool,
    fail_buyer: AtomicBool,
}

impl AuctionMailHooks for FlakyMailLog {
    fn send_auction_mail(&self, _server: &GameServer, mail: AuctionMail) -> bool {
        self.attempts.lock().unwrap().push(mail.clone());
        let fail = !self.accept.load(Ordering::SeqCst)
            || (self.fail_buyer.load(Ordering::SeqCst)
                && matches!(mail, AuctionMail::SaleBuyer { .. }));
        if fail {
            return false;
        }
        self.delivered.lock().unwrap().push(mail);
        true
    }
}

fn flaky_world() -> (World, Arc<FlakyMailLog>) {
    let (w, _) = auction_world();
    let log = Arc::new(FlakyMailLog::default());
    w.server.systems.economy.auction.mail.install(log.clone());
    (w, log)
}

fn auction_world() -> (World, Arc<MailLog>) {
    let mut d = test_data();
    d.items.templates.insert(
        SWORD,
        ItemTemplate {
            level: 20,
            auction_category_a: 1,
            auction_category_b: 2,
            auction_category_c: 3,
            ..ItemTemplate::plain(SWORD, 1)
        },
    );
    d.items.templates.insert(
        POTION,
        ItemTemplate {
            level: 5,
            auction_category_a: 4,
            ..ItemTemplate::plain(POTION, 20)
        },
    );
    d.items.templates.insert(
        SIEGE_CAMP,
        ItemTemplate {
            level: 1,
            auction_category_a: 11,
            ..ItemTemplate::plain(SIEGE_CAMP, 1)
        },
    );
    let mut en = HashMap::new();
    en.insert(i64::from(SWORD), "iron sword".to_string());
    en.insert(i64::from(POTION), "healing potion".to_string());
    let mut de = HashMap::new();
    de.insert(i64::from(SWORD), "eisenschwert".to_string());
    d.systems.economy.auction.item_name_cache = vec![("en_us".into(), en), ("de".into(), de)];
    let server = Arc::new(test_server_with(d));
    server.set_self();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    let pa = spawn_player(&server, &a, test_character(1, "Alice", 100.0, 100.0), 0x100);
    let pb = spawn_player(&server, &b, test_character(2, "Bob", 101.0, 100.0), 0x101);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    for p in [&pa, &pb] {
        // Persistent containers are known to the ItemManager (GetItemByItemId).
        for (_, c) in Inv::of(p).containers {
            server.items.add_container(c);
        }
        p.data().money = 100_000;
    }
    server.names.add_character(1, "Alice", 1001);
    server.names.add_character(2, "Bob", 1002);
    a.drain();
    b.drain();
    let log = Arc::new(MailLog::default());
    server.systems.economy.auction.mail.install(log.clone());
    let mut w = World {
        server,
        a,
        pa,
        b,
        pb,
    };
    // The packet bodies name the auctioneer NPC 0x200.
    spawn_service_npc(&mut w, 0x200, AUCTIONEER_TEMPLATE, 102.0);
    (w, log)
}

fn money(p: &PlayerRef) -> i64 {
    p.data().money
}

fn post_body(item_id: u64, start: i32, buyout: i32, duration: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(0x200)
        .write_bc(0x200)
        .write_u64(item_id)
        .write_i32(start)
        .write_i32(buyout)
        .write_u8(duration);
    s.into_bytes()
}

fn write_client_lot(s: &mut PacketStream, id: u64) {
    s.write_u64(id).write_u8(0);
    // Item.Write of an equipment item (detail type 1 -> 55 detail bytes).
    s.write_u32(SWORD)
        .write_u64(77)
        .write_u8(0)
        .write_u8(0)
        .write_i32(1)
        .write_u8(1)
        .write_raw(&[0xAB; 55])
        .write_i64(1)
        .write_i32(2)
        .write_u32(3)
        .write_u8(1)
        .write_i64(4)
        .write_i64(5);
    s.write_u8(1)
        .write_u32(1)
        .write_str("Alice")
        .write_i32(100)
        .write_i32(1000)
        .write_i64(123)
        .write_u8(255)
        .write_u32(0)
        .write_str("")
        .write_i32(0)
        .write_i32(0);
}

fn bid_body(lot_id: u64, money: i32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(0x200).write_bc(0x200);
    write_client_lot(&mut s, lot_id);
    s.write_u64(lot_id)
        .write_u8(1)
        .write_u32(0)
        .write_str("whoever")
        .write_i32(money);
    s.into_bytes()
}

/// `SCAuctionBidPacket` body: bid(lot, world, bidder, name, money), isBuyout, itemId.
fn bid_packet_body(
    lot_id: u64,
    world_id: u8,
    bidder_id: u32,
    name: &str,
    money: i32,
    is_buyout: bool,
    item_id: u32,
) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u64(lot_id)
        .write_u8(world_id)
        .write_u32(bidder_id)
        .write_str(name)
        .write_i32(money)
        .write_bool(is_buyout)
        .write_u32(item_id);
    s.into_bytes()
}

/// `SCAuctionMessagePacket` body: msgType, itemType, moneyAmount.
fn message_body(msg_type: u8, item_type: u32, money: i32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u8(msg_type).write_u32(item_type).write_i32(money);
    s.into_bytes()
}

fn cancel_body(lot_id: u64) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(0x200).write_bc(0x200);
    write_client_lot(&mut s, lot_id);
    s.into_bytes()
}

fn search_body(search: &AuctionSearch) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(0x200).write_bc(0x200);
    search.write(&mut s);
    s.into_bytes()
}

fn lot_bytes(lot: &AuctionLot) -> Vec<u8> {
    let mut s = PacketStream::new();
    lot.write(&mut s);
    s.into_bytes()
}

/// Posts `template` from Alice's bag slot `slot`; returns (item id, lot).
fn post(w: &mut World, template: u32, slot: i32, buyout: i32, duration: u8) -> (u64, AuctionLot) {
    let pa = w.pa.clone();
    let item_id = give(w, &pa, slot_type::INVENTORY, template, 1, slot);
    run::<CSAuctionPostPacket>(w, 0, &post_body(item_id, 0, buyout, duration));
    let lot = w
        .server
        .systems
        .economy
        .auction
        .all_lots()
        .into_iter()
        .find(|l| l.item_id() == item_id)
        .expect("lot");
    (item_id, lot)
}

// ---------------------------------------------------------------------------------------------
// Packets
// ---------------------------------------------------------------------------------------------

fn sample_lot() -> AuctionLot {
    let t = ItemTemplate::plain(SWORD, 1);
    let mut item = Item::create(0x55, &t, 3, 2, 1, &Default::default(), 1_000);
    item.flags = 0;
    AuctionLot {
        id: 0x0102_0304_0506_0708,
        duration: 2,
        item: Some(item),
        end_time: Utc.timestamp_opt(2_000, 0).unwrap(),
        world_id: 1,
        client_id: 7,
        client_name: "Seller".into(),
        start_money: 10,
        direct_money: 20,
        post_date: Utc.timestamp_opt(1_500, 0).unwrap(),
        bid_world_id: 255,
        bidder_id: 9,
        bidder_name: "Bidder".into(),
        bid_money: 15,
        extra: -1,
        dirty: false,
    }
}

fn expected_lot_bytes() -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u64(0x0102_0304_0506_0708).write_u8(2);
    // Item.Write (plain Item: DetailType Invalid, no details)
    s.write_u32(SWORD)
        .write_u64(0x55)
        .write_u8(2)
        .write_u8(0)
        .write_i32(3)
        .write_u8(0)
        .write_i64(1_000)
        .write_i32(0)
        .write_u32(0)
        .write_u8(1)
        .write_i64(0)
        .write_i64(0);
    s.write_u8(1)
        .write_u32(7)
        .write_str("Seller")
        .write_i32(10)
        .write_i32(20)
        .write_i64(1_500)
        .write_u8(255)
        .write_u32(9)
        .write_str("Bidder")
        .write_i32(15)
        .write_i32(-1);
    s.into_bytes()
}

fn body(p: &impl Marshal) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write(&mut s);
    s.into_bytes()
}

#[test]
fn sc_auction_posted_and_canceled_are_byte_exact() {
    let lot = sample_lot();
    assert_eq!(
        body(&SCAuctionPostedPacket { lot: lot.clone() }),
        expected_lot_bytes()
    );
    assert_eq!(body(&SCAuctionCanceledPacket { lot }), expected_lot_bytes());
    use crate::net::packet::PacketDef;
    assert_eq!(SCAuctionPostedPacket::OPCODE, 0x12e);
    assert_eq!(SCAuctionCanceledPacket::OPCODE, 0x132);
    assert_eq!(SCAuctionSearchedPacket::OPCODE, 0x12f);
    assert_eq!(SCAuctionLowestPricePacket::OPCODE, 0x130);
    assert_eq!(SCAuctionBidPacket::OPCODE, 0x131);
    assert_eq!(SCAuctionMessagePacket::OPCODE, 0x133);
}

/// Body layout from the 1.2 client reader `FUN_397b56f0`: u8 msgType, u32 itemType,
/// i32 moneyAmount (literals, not the writer).
#[test]
fn sc_auction_message_is_byte_exact() {
    let p = SCAuctionMessagePacket {
        msg_type: 0,
        item_type: 5001,     // 0x0000_1389
        money_amount: 12345, // 0x0000_3039
    };
    assert_eq!(
        body(&p),
        vec![0x00, 0x89, 0x13, 0x00, 0x00, 0x39, 0x30, 0x00, 0x00]
    );

    let p = SCAuctionMessagePacket {
        msg_type: 1,
        item_type: 0x0403_0201,
        money_amount: 0x0807_0605,
    };
    assert_eq!(
        body(&p),
        vec![0x01, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
    );
}

#[test]
fn sc_auction_searched_is_byte_exact() {
    let lot = sample_lot();
    let p = SCAuctionSearchedPacket {
        page: 2,
        count: 2,
        lots: vec![lot.clone(), lot],
        error_msg: 0,
        server_time: Utc.timestamp_opt(3_000, 0).unwrap(),
    };
    let mut want = PacketStream::new();
    want.write_i32(2).write_i32(2);
    want.write_raw(&expected_lot_bytes())
        .write_raw(&expected_lot_bytes());
    want.write_i16(0).write_i64(3_000);
    assert_eq!(body(&p), want.into_bytes());
}

#[test]
fn sc_auction_bid_and_lowest_price_are_byte_exact() {
    let p = SCAuctionBidPacket {
        bid: AuctionBid {
            lot_id: 5,
            world_id: 1,
            bidder_id: 2,
            bidder_name: "Bob".into(),
            money: 300,
        },
        is_buyout: false,
        item_id: SWORD,
    };
    let mut want = PacketStream::new();
    want.write_u64(5)
        .write_u8(1)
        .write_u32(2)
        .write_str("Bob")
        .write_i32(300)
        .write_bool(false)
        .write_u32(SWORD);
    assert_eq!(body(&p), want.into_bytes());

    let p = SCAuctionLowestPricePacket {
        item_template_id: SWORD,
        item_grade: 3,
        money_amount: 1234,
    };
    let mut want = PacketStream::new();
    want.write_u32(SWORD).write_u8(3).write_i32(1234);
    assert_eq!(body(&p), want.into_bytes());
}

#[test]
fn lot_without_item_writes_empty_item() {
    let mut lot = sample_lot();
    lot.item = None;
    let b = lot_bytes(&lot);
    assert_eq!(&b[9..13], &[0, 0, 0, 0]);
    assert_eq!(b[13], 1); // world id follows
}

// ---------------------------------------------------------------------------------------------
// C->S decoding
// ---------------------------------------------------------------------------------------------

fn decode<P: crate::net::registry::ClientPacket>(b: &[u8]) -> P {
    let mut s = PacketStream::from_bytes(b.to_vec());
    let p = P::read(&mut s).expect("read");
    assert_eq!(s.remaining(), 0, "unread bytes");
    p
}

#[test]
fn cs_packets_decode() {
    let p: CSAuctionPostPacket = decode(&post_body(99, 10, 20, 3));
    assert_eq!(
        p,
        CSAuctionPostPacket {
            auctioneer_id: 0x200,
            auctioneer_id2: 0x200,
            item_id: 99,
            start_price: 10,
            buyout_price: 20,
            duration: 3,
        }
    );

    let search = AuctionSearch {
        keyword: "iron".into(),
        exact_match: true,
        grade: 1,
        category_a: 2,
        category_b: 3,
        category_c: 4,
        page: 5,
        client_id: 6,
        filter: 7,
        world_id: 8,
        min_item_level: 9,
        max_item_level: 10,
        sort_kind: 6,
        sort_order: 1,
    };
    let p: CSAuctionSearchPacket = decode(&search_body(&search));
    assert_eq!(p.search, search);
    // Field order of AuctionSearch.Read: ..., categoryC, page, clientId, filter, ...
    let b = search_body(&search);
    let k = 6 + 2 + 4 + 1 + 1 + 3;
    assert_eq!(&b[k..k + 4], &5i32.to_le_bytes());

    let p: CSBidAuctionPacket = decode(&bid_body(42, 777));
    assert_eq!(p.lot.id, 42);
    assert_eq!(p.lot.item.template_id, SWORD);
    assert_eq!(p.lot.item.detail.len(), 55);
    assert_eq!(p.lot.item.unpack_time, 5);
    assert_eq!(p.lot.client_name, "Alice");
    assert_eq!(p.lot.post_date, 123);
    assert_eq!(p.bid.lot_id, 42);
    assert_eq!(p.bid.bidder_name, "whoever");
    assert_eq!(p.bid.money, 777);

    let p: CSCancelAuctionPacket = decode(&cancel_body(43));
    assert_eq!(p.lot.id, 43);
    assert_eq!(p.lot.extra, 0);

    let mut s = PacketStream::new();
    s.write_bc(1).write_bc(2).write_i32(3);
    let p: CSAuctionMyBidListPacket = decode(s.as_bytes());
    assert_eq!((p.auctioneer_id, p.auctioneer_id2, p.page), (1, 2, 3));

    let mut s = PacketStream::new();
    s.write_bc(1).write_bc(2).write_u32(SWORD).write_u8(4);
    let p: CSAuctionLowestPricePacket = decode(s.as_bytes());
    assert_eq!((p.item_template_id, p.item_grade), (SWORD, 4));
}

#[test]
fn wire_item_with_empty_template_reads_only_the_id() {
    let mut s = PacketStream::new();
    s.write_u32(0).write_u8(9);
    let mut r = PacketStream::from_bytes(s.into_bytes());
    let i = WireItem::read(&mut r).unwrap();
    assert_eq!(i.template_id, 0);
    assert_eq!(r.remaining(), 1);
}

// ---------------------------------------------------------------------------------------------
// Flows
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn post_moves_item_charges_fee_and_sends_lot() {
    let (mut w, _log) = auction_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    // fee = 1000 * .01 * (1 + 1) = 20
    assert_eq!(money(&w.pa), 100_000 - 20);
    assert!(Inv::of(&w.pa).item_at(slot_type::INVENTORY, 0).is_none());
    let auction = Inv::of(&w.pa).get(slot_type::AUCTION).unwrap();
    let placed = auction
        .lock()
        .unwrap()
        .get_item_by_id(item_id)
        .cloned()
        .unwrap();
    assert_eq!(placed.slot_type, slot_type::AUCTION);
    assert_eq!(lot.item.as_ref().unwrap(), &placed);
    assert_eq!(
        (lot.client_id, lot.client_name.as_str(), lot.world_id),
        (1, "Alice", 1)
    );
    assert_eq!(
        (lot.start_money, lot.direct_money, lot.duration),
        (0, 1000, 1)
    );
    assert_eq!((lot.bid_world_id, lot.bidder_id), (255, 0));
    assert_eq!(lot.end_time - lot.post_date, Duration::hours(12));
    assert!(lot.dirty);

    let frames = w.a.drain();
    let ops: Vec<u16> = frames.iter().map(|f| f.0).collect();
    assert_eq!(
        ops,
        vec![
            sc::SCItemTaskSuccessPacket,
            sc::SCItemTaskSuccessPacket,
            sc::SCItemTaskSuccessPacket,
            sc::SCAuctionPostedPacket
        ]
    );
    assert_eq!(
        frames[2].1,
        task_body(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-20)])
    );
    assert_eq!(
        frames[0].1,
        task_body(task_type::AUCTION, vec![ItemTask::add(&placed)])
    );
    assert_eq!(frames[3].1, lot_bytes(&lot));

    // 48 h listing of a huge buyout: fee is capped at MaxListingFee.
    w.pa.data().money = 5_000_000;
    let (_, lot) = post(&mut w, POTION, 1, 1_000_000_000, 3);
    assert_eq!(lot.end_time - lot.post_date, Duration::hours(48));
    assert_eq!(money(&w.pa), 5_000_000 - i64::from(MAX_LISTING_FEE));
    // Invalid duration leaves the item and wallet unchanged.
    let pa = w.pa.clone();
    let id = give(&w, &pa, slot_type::INVENTORY, POTION, 1, 2);
    let before = w.server.systems.economy.auction.all_lots().len();
    run::<CSAuctionPostPacket>(&mut w, 0, &post_body(id, 0, 1000, 9));
    assert_eq!(w.server.systems.economy.auction.all_lots().len(), before);
    assert!(Inv::of(&w.pa).item_at(slot_type::INVENTORY, 2).is_some());
}

#[tokio::test]
async fn post_of_unknown_item_does_nothing() {
    let (mut w, _log) = auction_world();
    run::<CSAuctionPostPacket>(&mut w, 0, &post_body(0xDEAD, 1, 2, 0));
    assert!(w.a.drain().is_empty());
    assert!(w.server.systems.economy.auction.all_lots().is_empty());
}

#[tokio::test]
async fn search_filters_sorts_and_pages() {
    let (mut w, _log) = auction_world();
    let (_, sword) = post(&mut w, SWORD, 0, 1000, 0);
    let (_, potion) = post(&mut w, POTION, 1, 50, 0);
    w.a.drain();

    let search = |w: &mut World, s: AuctionSearch| -> Vec<u8> {
        run::<CSAuctionSearchPacket>(w, 1, &search_body(&s));
        let frames = w.b.drain();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, sc::SCAuctionSearchedPacket);
        let b = frames[0].1.clone();
        b[..b.len() - 8].to_vec() // without the server time
    };
    let page = |p: i32, lots: &[&AuctionLot]| -> Vec<u8> {
        let mut s = PacketStream::new();
        s.write_i32(p).write_i32(lots.len() as i32);
        for l in lots {
            s.write_raw(&lot_bytes(l));
        }
        s.write_i16(0);
        s.into_bytes()
    };

    // Everything, id order.
    assert_eq!(
        search(&mut w, AuctionSearch::default()),
        page(0, &[&sword, &potion])
    );
    // Keyword (any locale, partial words), exact match.
    let s = AuctionSearch {
        keyword: "SWORD Iron".into(),
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[&sword]));
    let s = AuctionSearch {
        keyword: "eisen".into(),
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[&sword]));
    let s = AuctionSearch {
        keyword: "healing".into(),
        exact_match: true,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[]));
    // Categories / levels / client id.
    let s = AuctionSearch {
        category_a: 4,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[&potion]));
    let s = AuctionSearch {
        category_a: 1,
        category_c: 9,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[]));
    let s = AuctionSearch {
        min_item_level: 10,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[&sword]));
    let s = AuctionSearch {
        max_item_level: 10,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[&potion]));
    let s = AuctionSearch {
        client_id: 2,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[]));
    // Sorting.
    let s = AuctionSearch {
        sort_kind: sort_kind::DIRECT_PRICE,
        sort_order: sort_order::ASC,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[&potion, &sword]));
    let s = AuctionSearch {
        sort_kind: sort_kind::ITEM_LEVEL,
        sort_order: sort_order::DESC,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(0, &[&sword, &potion]));
    // Out-of-bounds page keeps the requested page number, no lots.
    let s = AuctionSearch {
        page: 1,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(1, &[]));
    let s = AuctionSearch {
        page: -1,
        ..Default::default()
    };
    assert_eq!(search(&mut w, s), page(-1, &[]));
}

#[tokio::test]
async fn declare_siege_filter_lists_only_the_siege_camps() {
    let (mut w, _log) = auction_world();
    let (_, camp_one) = post(&mut w, SIEGE_CAMP, 0, 100, 0);
    let (_, sword) = post(&mut w, SWORD, 1, 100, 0);
    let (_, camp_two) = post(&mut w, SIEGE_CAMP, 2, 100, 0);
    w.a.drain();

    let search = |w: &mut World, s: AuctionSearch| -> Vec<u8> {
        run::<CSAuctionSearchPacket>(w, 1, &search_body(&s));
        let frames = w.b.drain();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, sc::SCAuctionSearchedPacket);
        let b = frames[0].1.clone();
        b[..b.len() - 8].to_vec() // without the server time
    };
    let page = |p: i32, lots: &[&AuctionLot]| -> Vec<u8> {
        let mut s = PacketStream::new();
        s.write_i32(p).write_i32(lots.len() as i32);
        for l in lots {
            s.write_raw(&lot_bytes(l));
        }
        s.write_i16(0);
        s.into_bytes()
    };

    // The declare tab (`filter = 1`) keeps only the category-11 lots.
    let declare = AuctionSearch {
        filter: 1,
        ..Default::default()
    };
    assert_eq!(
        search(&mut w, declare),
        page(0, &[&camp_one, &camp_two]),
        "filter 1 must list the two camps and hide the sword"
    );
    // The normal tab is unchanged and still lists every lot.
    assert_eq!(
        search(&mut w, AuctionSearch::default()),
        page(0, &[&camp_one, &sword, &camp_two])
    );
    // An unknown filter behaves like the normal tab.
    let unknown = AuctionSearch {
        filter: 7,
        ..Default::default()
    };
    assert_eq!(
        search(&mut w, unknown),
        page(0, &[&camp_one, &sword, &camp_two])
    );
}

#[tokio::test]
async fn search_pages_hold_nine_lots() {
    let (mut w, _log) = auction_world();
    w.pa.data().money = 1_000_000;
    let mut lots = Vec::new();
    for slot in 0..11 {
        lots.push(post(&mut w, POTION, slot, 10, 0).1);
    }
    w.a.drain();
    let s = AuctionSearch {
        page: 1,
        ..Default::default()
    };
    run::<CSAuctionSearchPacket>(&mut w, 1, &search_body(&s));
    let frames = w.b.drain();
    let mut want = PacketStream::new();
    want.write_i32(1).write_i32(2);
    want.write_raw(&lot_bytes(&lots[9]))
        .write_raw(&lot_bytes(&lots[10]));
    want.write_i16(0);
    let b = &frames[0].1;
    assert_eq!(&b[..b.len() - 8], want.as_bytes());
}

#[tokio::test]
async fn expire_date_sort_uses_end_time_not_post_date() {
    let (mut w, _log) = auction_world();
    let (_, mut first) = post(&mut w, SWORD, 0, 100, 0);
    let (_, mut second) = post(&mut w, POTION, 1, 100, 0);
    // The first lot was posted first but expires last: a PostDate sort would keep this order.
    first.post_date = Utc.timestamp_opt(1_000, 0).unwrap();
    first.end_time = Utc.timestamp_opt(9_000, 0).unwrap();
    second.post_date = Utc.timestamp_opt(2_000, 0).unwrap();
    second.end_time = Utc.timestamp_opt(8_000, 0).unwrap();
    let ah = &w.server.systems.economy.auction;
    ah.set_lot(first.clone());
    ah.set_lot(second.clone());
    w.a.drain();
    w.b.drain();
    let s = AuctionSearch {
        sort_kind: sort_kind::EXPIRE_DATE,
        sort_order: sort_order::ASC,
        ..Default::default()
    };
    run::<CSAuctionSearchPacket>(&mut w, 1, &search_body(&s));
    let frames = w.b.drain();
    let mut want = PacketStream::new();
    want.write_i32(0).write_i32(2);
    want.write_raw(&lot_bytes(&second))
        .write_raw(&lot_bytes(&first));
    want.write_i16(0);
    let b = &frames[0].1;
    assert_eq!(&b[..b.len() - 8], want.as_bytes());
}

#[tokio::test]
async fn bid_outbid_buyout_moves_money_and_mails() {
    let (mut w, log) = auction_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    w.a.drain();

    // Bob bids 500.
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 500));
    assert_eq!(money(&w.pb), 100_000 - 500);
    let frames = w.b.drain();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0].1,
        task_body(task_type::AUCTION, vec![ItemTask::MoneyChange(-500)])
    );
    assert_eq!(frames[1].0, sc::SCAuctionBidPacket);
    let mut want = PacketStream::new();
    want.write_u64(lot.id)
        .write_u8(0)
        .write_u32(2)
        .write_str("Bob")
        .write_i32(500)
        .write_bool(false)
        .write_u32(SWORD);
    assert_eq!(frames[1].1, want.into_bytes());
    let l = w.server.systems.economy.auction.lot(lot.id).unwrap();
    assert_eq!(
        (
            l.bidder_id,
            l.bidder_name.as_str(),
            l.bid_money,
            l.bid_world_id
        ),
        (2, "Bob", 500, 0)
    );
    assert!(log.take().is_empty());
    // The online seller is told about the accepted bid (type 0).
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCAuctionMessagePacket,
            message_body(auction_message::BIDDEN, SWORD, 500)
        )]
    );

    // A lower bid is ignored.
    run::<CSBidAuctionPacket>(&mut w, 0, &bid_body(lot.id, 400));
    assert!(w.a.drain().is_empty());

    // My bid list.
    let mut s = PacketStream::new();
    s.write_bc(0x200).write_bc(0x200).write_i32(0);
    run::<CSAuctionMyBidListPacket>(&mut w, 1, s.as_bytes());
    let frames = w.b.drain();
    let mut want = PacketStream::new();
    want.write_i32(0).write_i32(1);
    want.write_raw(&lot_bytes(
        &w.server.systems.economy.auction.lot(lot.id).unwrap(),
    ));
    want.write_i16(0);
    assert_eq!(&frames[0].1[..frames[0].1.len() - 8], want.as_bytes());

    // Bob raises his bid; the previous escrow is refunded by mail.
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 600));
    assert_eq!(
        log.take(),
        vec![AuctionMail::BidFail {
            item_template_id: SWORD,
            seller_id: 1,
            buyout_price: 1000,
            listing_fee: 20,
            previous_buyer_id: 2,
            previous_bid: 500,
        }]
    );
    assert_eq!(money(&w.pa), 100_000 - 20);
    w.b.drain();
    w.a.drain();

    // Bob buys it out (any bid >= DirectMoney pays DirectMoney).
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 5000));
    assert_eq!(money(&w.pb), 100_000 - 500 - 600 - 1000);
    let frames = w.b.drain();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0].1,
        task_body(task_type::DEPOSIT_MONEY, vec![ItemTask::MoneyChange(-1000)])
    );
    // The buyer gets the buyout confirmation (the C# sent none).
    assert_eq!(
        frames[1],
        (
            sc::SCAuctionBidPacket,
            bid_packet_body(lot.id, 0, 2, "Bob", 1000, true, SWORD)
        )
    );
    let item = w.server.items.item_by_id(item_id).unwrap();
    assert_eq!(
        log.take(),
        vec![
            AuctionMail::BidFail {
                item_template_id: SWORD,
                seller_id: 1,
                buyout_price: 1000,
                listing_fee: 0,
                previous_buyer_id: 2,
                previous_bid: 600,
            },
            AuctionMail::SaleSeller {
                item: item.clone(),
                seller_id: 1,
                buyout_price: 1000,
                listing_fee: 20,
                seller_share: 900,
                trade_tax_fee: 100,
            },
            AuctionMail::SaleBuyer {
                item,
                seller_id: 1,
                buyout_price: 1000,
                listing_fee: 20,
                buyer_id: 2,
            },
        ]
    );
    let ah = &w.server.systems.economy.auction;
    assert!(ah.lot(lot.id).is_none());
    assert_eq!(ah.deleted_ids(), vec![lot.id]);
    // The item stays in the seller's auction container until the mail takes it.
    assert!(Inv::of(&w.pa)
        .get(slot_type::AUCTION)
        .unwrap()
        .lock()
        .unwrap()
        .get_item_by_id(item_id)
        .is_some());

    // Bidding on a removed lot does nothing.
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 5000));
    assert!(w.b.drain().is_empty());
    assert!(log.take().is_empty());
}

/// An online seller gets `SCAuctionMessage` type 0 when a bid is accepted and type 1 when the
/// lot is bought out (the sale path shared with a timed winner); the mail still carries the
/// result.
#[tokio::test]
async fn online_seller_gets_bid_and_sale_messages() {
    let (mut w, _log) = auction_world();
    let (_, lot) = post(&mut w, SWORD, 0, 1000, 1);
    w.a.drain();

    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 500));
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCAuctionMessagePacket,
            message_body(auction_message::BIDDEN, SWORD, 500)
        )]
    );

    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCAuctionMessagePacket,
            message_body(auction_message::BOUGHT_BY_SOMEONE, SWORD, 1000)
        )]
    );
    assert!(w.server.systems.economy.auction.lot(lot.id).is_none());
}

/// An offline seller gets no live message; the sale mail is delivered as before.
#[tokio::test]
async fn offline_seller_gets_no_auction_message_but_the_mails() {
    let (mut w, log) = auction_world();
    let (_, lot) = post(&mut w, SWORD, 0, 1000, 1);
    assert!(w.server.world.try_remove_character(w.pa.obj_id));
    w.a.drain();

    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 500));
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
    assert!(
        w.a.drain().is_empty(),
        "no live notice for an offline seller"
    );
    let mails = log.take();
    assert_eq!(mails.len(), 3, "outbid refund, sale seller, sale buyer");
}

#[tokio::test]
async fn buyout_without_previous_bid_and_no_buyout_price() {
    let (mut w, log) = auction_world();
    let (_, lot) = post(&mut w, SWORD, 0, 0, 0);
    // DirectMoney 0: never a buyout, any higher bid is a bid.
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 999_999));
    assert_eq!(money(&w.pb), 100_000);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 999));
    assert!(w.server.systems.economy.auction.lot(lot.id).is_some());
    assert_eq!(money(&w.pb), 100_000 - 999);
    assert!(log.take().is_empty());

    let (_, lot) = post(&mut w, POTION, 1, 300, 0);
    w.b.drain();
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 300));
    let frames = w.b.drain();
    assert_eq!(
        frames.last().unwrap(),
        &(
            sc::SCAuctionBidPacket,
            bid_packet_body(lot.id, 0, 2, "Bob", 300, true, POTION)
        )
    );
    let mails = log.take();
    assert_eq!(mails.len(), 2);
    assert!(matches!(
        mails[0],
        AuctionMail::SaleSeller {
            seller_share: 270,
            trade_tax_fee: 30,
            listing_fee: 3,
            ..
        }
    ));
    assert!(matches!(
        mails[1],
        AuctionMail::SaleBuyer { buyer_id: 2, .. }
    ));
}

#[tokio::test]
async fn cancel_returns_the_original_item_and_keeps_bid_lots() {
    let (mut w, log) = auction_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 2);
    w.a.drain();
    // A foreign client knows the public lot id but cannot cancel the seller's listing.
    let before_item = w.server.items.item_by_id(item_id).unwrap();
    let before_money = (money(&w.pa), money(&w.pb));
    run::<CSCancelAuctionPacket>(&mut w, 1, &cancel_body(lot.id));
    assert!(w.b.drain().is_empty());
    assert!(w.a.drain().is_empty());
    assert!(log.take().is_empty());
    assert_eq!(w.server.items.item_by_id(item_id).unwrap(), before_item);
    assert_eq!((money(&w.pa), money(&w.pb)), before_money);
    assert_eq!(w.server.systems.economy.auction.lot(lot.id).unwrap(), lot);
    run::<CSCancelAuctionPacket>(&mut w, 0, &cancel_body(lot.id));
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, sc::SCAuctionCanceledPacket);
    assert_eq!(frames[0].1, lot_bytes(&lot));
    let mails = log.take();
    assert_eq!(mails.len(), 1);
    let AuctionMail::Cancel {
        item,
        seller_id,
        buyout_price,
        listing_fee,
    } = &mails[0]
    else {
        panic!("{mails:?}")
    };
    assert_eq!((*seller_id, *buyout_price, *listing_fee), (1, 1000, 30));
    assert_eq!(item.id, item_id);
    assert_eq!((item.template_id, item.count), (SWORD, 1));
    // The new item is registered (loose), the listed one stays in the auction container.
    assert!(w.server.items.item_by_id(item.id).is_some());
    assert!(w.server.items.item_by_id(item_id).is_some());
    assert!(w.server.systems.economy.auction.lot(lot.id).is_none());

    // A lot with a bid cannot be cancelled.
    let (_, lot) = post(&mut w, POTION, 1, 1000, 0);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 10));
    w.a.drain();
    run::<CSCancelAuctionPacket>(&mut w, 0, &cancel_body(lot.id));
    assert!(w.a.drain().is_empty());
    assert!(w.server.systems.economy.auction.lot(lot.id).is_some());
    // Unknown lot.
    run::<CSCancelAuctionPacket>(&mut w, 0, &cancel_body(0xFFFF));
    assert!(w.a.drain().is_empty());
    assert!(log.take().is_empty());
}

#[tokio::test]
async fn expiry_returns_unsold_and_delivers_won_lots() {
    let (mut w, log) = auction_world();
    let (unsold_id, unsold) = post(&mut w, SWORD, 0, 1000, 0);
    let (won_id, won) = post(&mut w, POTION, 1, 1000, 0);
    let (_, running) = post(&mut w, POTION, 2, 1000, 0);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(won.id, 700));
    w.a.drain();
    w.b.drain();
    let ah = &w.server.systems.economy.auction;

    // Nothing expired yet.
    ah.update_auction_house(&w.server, Utc::now());
    assert_eq!(ah.all_lots().len(), 3);
    assert!(log.take().is_empty());

    let later = unsold.end_time + Duration::seconds(1);
    let mut r = running.clone();
    r.end_time = later + Duration::hours(1);
    ah.set_lot(r);
    ah.update_auction_house(&w.server, later);
    let unsold_item = w.server.items.item_by_id(unsold_id).unwrap();
    let won_item = w.server.items.item_by_id(won_id).unwrap();
    assert_eq!(
        log.take(),
        vec![
            AuctionMail::Fail {
                item: unsold_item,
                seller_id: 1,
                buyout_price: 1000,
                listing_fee: 10,
            },
            AuctionMail::SaleSeller {
                item: won_item.clone(),
                seller_id: 1,
                buyout_price: 700,
                listing_fee: 10,
                seller_share: 630,
                trade_tax_fee: 70,
            },
            AuctionMail::SaleBuyer {
                item: won_item,
                seller_id: 1,
                buyout_price: 700,
                listing_fee: 10,
                buyer_id: 2,
            },
        ]
    );
    let left: Vec<u64> = ah.all_lots().iter().map(|l| l.id).collect();
    assert_eq!(left, vec![running.id]);
    let mut deleted = ah.deleted_ids();
    deleted.sort_unstable();
    assert_eq!(deleted, vec![unsold.id, won.id]);
    // Auction ids are released for reuse. The seller only hears about the won lot (type 1),
    // not about the unsold return.
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCAuctionMessagePacket,
            message_body(auction_message::BOUGHT_BY_SOMEONE, POTION, 700)
        )]
    );
}

/// Posts a system lot (`client_id = 0`) the way the GM command does, outside the fixtures'
/// player-posted path.
fn post_system(w: &mut World, template: u32, start: i32) -> AuctionLot {
    let auction = &w.server.systems.economy.auction;
    let _items = w.server.systems.economy.operation_scope();
    let _op = auction.operation_lock();
    let lot = AuctionManager::create_system_lot(
        &w.server,
        template,
        start,
        0,
        auction_duration::HOURS_6,
        Utc::now(),
    )
    .expect("system lot");
    auction.add_auction_lot(lot.clone());
    lot
}

/// A system lot (`client_id = 0`) sold at expiry: the buyer gets the item by the normal auction
/// mail, the money is destroyed (there is no seller to pay or mail).
#[tokio::test]
async fn system_lot_expiry_mails_the_item_to_the_buyer_only() {
    let (mut w, _log) = auction_world();
    crate::systems::economy::wiring::install(&w.server);
    let lot = post_system(&mut w, SIEGE_CAMP, 100);
    let item_id = lot.item_id();
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 100));
    assert_eq!(money(&w.pb), 100_000 - 100);
    let ah = &w.server.systems.economy.auction;
    let later = lot.end_time + Duration::seconds(1);
    ah.update_auction_house(&w.server, later);

    assert!(ah.lot(lot.id).is_none());
    assert_eq!(ah.deleted_ids(), vec![lot.id]);
    let buyer = mails_to(&w.server, 2);
    assert_eq!(buyer.len(), 1, "the buyer's win mail");
    assert_eq!(
        buyer[0].mail_type,
        crate::systems::economy::mail::mail_type::AUC_BID_WIN
    );
    assert_eq!(buyer[0].body.attachments, vec![item_id]);
    assert_eq!(buyer[0].body.copper_coins, 0);
    assert_eq!(
        mails_to(&w.server, 1),
        vec![],
        "no seller 0 / Alice mail for a system lot"
    );
    assert_eq!(mails_to(&w.server, 0), Vec::new());
    let item = w.server.items.item_by_id(item_id).expect("the camp item");
    assert_eq!(
        (item.owner_id, item.slot_type),
        (2, slot_type::MAIL),
        "the item is attached to the buyer's mail"
    );
}

/// An unsold system lot is not returned to a seller (there is none): the item is deleted and
/// both the lot row and the item row are queued for deletion, leaving no orphan.
#[tokio::test]
async fn system_lot_unbid_expiry_deletes_the_item_and_leaves_no_orphan() {
    let (mut w, log) = auction_world();
    let lot = post_system(&mut w, SIEGE_CAMP, 100);
    let item_id = lot.item_id();
    let ah = &w.server.systems.economy.auction;
    let later = lot.end_time + Duration::seconds(1);
    ah.update_auction_house(&w.server, later);

    assert!(ah.lot(lot.id).is_none());
    assert_eq!(ah.deleted_ids(), vec![lot.id]);
    assert!(w.server.items.item_by_id(item_id).is_none());
    assert!(
        w.server.items.removed_items().contains(&item_id),
        "the item row is queued for deletion"
    );
    assert!(log.take().is_empty(), "no return mail for a system lot");
    assert!(mails_to(&w.server, 0).is_empty());
}

/// A failed sale mail must not drop the lot (the seller's money / buyer's item would be lost):
/// it stays listed and the next auction tick retries the mails exactly once.
#[tokio::test]
async fn failed_sale_mail_keeps_the_lot_and_settles_once_on_retry() {
    let (mut w, log) = flaky_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
    // The buyout itself happened: the money is gone and the buyer was confirmed.
    assert_eq!(money(&w.pb), 100_000 - 1000);
    let ah = &w.server.systems.economy.auction;
    assert!(ah.lot(lot.id).is_some());
    assert!(log.delivered.lock().unwrap().is_empty());
    w.b.drain();

    // Still failing: the tick keeps the lot and sends nothing twice.
    ah.update_auction_house(&w.server, Utc::now());
    assert!(ah.lot(lot.id).is_some());
    assert!(log.delivered.lock().unwrap().is_empty());

    // A later success delivers the missing mails and removes the lot.
    log.accept.store(true, Ordering::SeqCst);
    ah.update_auction_house(&w.server, Utc::now());
    assert!(ah.lot(lot.id).is_none());
    assert_eq!(ah.deleted_ids(), vec![lot.id]);
    let delivered = log.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert!(matches!(delivered[0], AuctionMail::SaleSeller { .. }));
    assert!(matches!(
        &delivered[1],
        AuctionMail::SaleBuyer { item, buyer_id: 2, .. } if item.id == item_id
    ));
    // Settled once: further ticks deliver nothing.
    ah.update_auction_house(&w.server, Utc::now());
    assert_eq!(log.delivered.lock().unwrap().len(), 2);
    assert_eq!(ah.deleted_ids(), vec![lot.id]);
}

/// A mail that can never be delivered (e.g. a deleted receiver) must not keep the lot alive
/// forever: after `MAX_SETTLEMENT_RETRIES` failed ticks the settlement is dropped and the lot
/// removed.
#[tokio::test]
async fn undeliverable_settlement_is_given_up_after_bounded_retries() {
    let (mut w, log) = flaky_world();
    let (_item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
    let ah = &w.server.systems.economy.auction;
    assert!(ah.lot(lot.id).is_some());

    for _ in 0..MAX_SETTLEMENT_RETRIES - 1 {
        ah.update_auction_house(&w.server, Utc::now());
        assert!(ah.lot(lot.id).is_some(), "still retrying");
    }
    ah.update_auction_house(&w.server, Utc::now());
    assert!(ah.lot(lot.id).is_none(), "the lot is removed when given up");
    assert_eq!(ah.deleted_ids(), vec![lot.id]);
    assert!(log.delivered.lock().unwrap().is_empty());

    // Nothing is retried afterwards.
    let attempts = log.attempts.lock().unwrap().len();
    ah.update_auction_house(&w.server, Utc::now());
    assert_eq!(log.attempts.lock().unwrap().len(), attempts);
}

/// A settlement that already delivered the seller's mail must only retry the buyer's.
#[tokio::test]
async fn sale_retry_sends_only_the_mail_that_failed() {
    let (mut w, log) = flaky_world();
    log.accept.store(true, Ordering::SeqCst);
    log.fail_buyer.store(true, Ordering::SeqCst);
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
    let ah = &w.server.systems.economy.auction;
    assert!(ah.lot(lot.id).is_some());
    {
        let delivered = log.delivered.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert!(matches!(delivered[0], AuctionMail::SaleSeller { .. }));
    }

    log.fail_buyer.store(false, Ordering::SeqCst);
    ah.update_auction_house(&w.server, Utc::now());
    assert!(ah.lot(lot.id).is_none());
    let delivered = log.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert!(matches!(
        &delivered[1],
        AuctionMail::SaleBuyer { item, .. } if item.id == item_id
    ));
    assert_eq!(
        delivered
            .iter()
            .filter(|m| matches!(m, AuctionMail::SaleSeller { .. }))
            .count(),
        1,
        "the seller's mail must not be delivered twice"
    );
}

/// The unsold ("failed auction") return mail is retried the same way.
#[tokio::test]
async fn failed_expiry_mail_keeps_the_lot_until_delivered() {
    let (mut w, log) = flaky_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 0);
    let ah = &w.server.systems.economy.auction;
    let later = lot.end_time + Duration::seconds(1);
    ah.update_auction_house(&w.server, later);
    assert!(ah.lot(lot.id).is_some());
    assert!(log.delivered.lock().unwrap().is_empty());

    log.accept.store(true, Ordering::SeqCst);
    ah.update_auction_house(&w.server, later);
    assert!(ah.lot(lot.id).is_none());
    let delivered = log.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert!(matches!(
        &delivered[0],
        AuctionMail::Fail { item, seller_id: 1, .. } if item.id == item_id
    ));
    ah.update_auction_house(&w.server, later);
    assert_eq!(log.delivered.lock().unwrap().len(), 1);
    assert_eq!(log.attempts.lock().unwrap().len(), 2);
}

/// A cancel whose refund mail fails keeps the lot; retrying settles it without a second mail.
#[tokio::test]
async fn failed_cancel_mail_keeps_the_lot_until_delivered() {
    let (mut w, log) = flaky_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    w.a.drain();
    run::<CSCancelAuctionPacket>(&mut w, 0, &cancel_body(lot.id));
    assert!(w.server.systems.economy.auction.lot(lot.id).is_some());
    assert!(
        w.a.drain().is_empty(),
        "no confirmation before the mail is out"
    );
    // Cancelling again while the refund is queued does nothing.
    run::<CSCancelAuctionPacket>(&mut w, 0, &cancel_body(lot.id));
    let ah = &w.server.systems.economy.auction;
    assert!(ah.lot(lot.id).is_some());
    assert_eq!(log.attempts.lock().unwrap().len(), 1);

    log.accept.store(true, Ordering::SeqCst);
    ah.update_auction_house(&w.server, Utc::now());
    assert!(ah.lot(lot.id).is_none());
    let delivered = log.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert!(matches!(
        &delivered[0],
        AuctionMail::Cancel { item, .. } if item.id == item_id
    ));
    ah.update_auction_house(&w.server, Utc::now());
    assert_eq!(log.delivered.lock().unwrap().len(), 1);
}

/// An outbid refund that fails is retried on the next tick; the lot keeps its new bidder.
#[tokio::test]
async fn failed_outbid_refund_is_retried() {
    let (mut w, log) = flaky_world();
    log.accept.store(true, Ordering::SeqCst);
    let (_, lot) = post(&mut w, SWORD, 0, 1000, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 500));
    log.accept.store(false, Ordering::SeqCst);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 600));
    let ah = &w.server.systems.economy.auction;
    assert_eq!(ah.lot(lot.id).unwrap().bid_money, 600);
    assert!(log.delivered.lock().unwrap().is_empty());

    log.accept.store(true, Ordering::SeqCst);
    ah.update_auction_house(&w.server, Utc::now());
    let delivered = log.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert!(matches!(
        &delivered[0],
        AuctionMail::BidFail {
            previous_buyer_id: 2,
            previous_bid: 500,
            ..
        }
    ));
    ah.update_auction_house(&w.server, Utc::now());
    assert_eq!(log.delivered.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn lowest_price_reports_cheapest_buyout() {
    let (mut w, _log) = auction_world();
    post(&mut w, POTION, 0, 500, 0);
    post(&mut w, POTION, 1, 200, 0);
    post(&mut w, SWORD, 2, 100, 0);
    let mut s = PacketStream::new();
    s.write_bc(0x200)
        .write_bc(0x200)
        .write_u32(POTION)
        .write_u8(3);
    run::<CSAuctionLowestPricePacket>(&mut w, 1, s.as_bytes());
    let frames = w.b.drain();
    assert_eq!(frames[0].0, sc::SCAuctionLowestPricePacket);
    let mut want = PacketStream::new();
    want.write_u32(POTION).write_u8(3).write_i32(200);
    assert_eq!(frames[0].1, want.into_bytes());

    let mut s = PacketStream::new();
    s.write_bc(0x200).write_bc(0x200).write_u32(999).write_u8(0);
    run::<CSAuctionLowestPricePacket>(&mut w, 1, s.as_bytes());
    let mut want = PacketStream::new();
    want.write_u32(999).write_u8(0).write_i32(0);
    assert_eq!(w.b.drain()[0].1, want.into_bytes());
}

#[tokio::test]
async fn empty_bid_list_sends_page_zero() {
    let (mut w, _log) = auction_world();
    let mut s = PacketStream::new();
    s.write_bc(0x200).write_bc(0x200).write_i32(4);
    run::<CSAuctionMyBidListPacket>(&mut w, 1, s.as_bytes());
    let frames = w.b.drain();
    let b = &frames[0].1;
    assert_eq!(&b[..10], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(b.len(), 18);
}

#[test]
fn listing_fee_matches_csharp_double_math() {
    assert_eq!(listing_fee(1000, 0) as i32, 10);
    assert_eq!(listing_fee(1000, 3) as i32, 40);
    assert_eq!(listing_fee(333, 2) as i32, 9); // 9.99 -> 9
    assert_eq!(listing_fee(i32::MAX, 3) as i32, MAX_LISTING_FEE);
    // (int)(soldAmount * .9) / (int)(soldAmount - soldAmount * .9)
    let sold = 333;
    let after = f64::from(sold) * 0.9;
    assert_eq!((after as i32, (f64::from(sold) - after) as i32), (299, 33));
}

// ---------------------------------------------------------------------------------------------
// Integration with the mail port (systems::economy::wiring)
// ---------------------------------------------------------------------------------------------

fn mails_to(server: &GameServer, receiver: u32) -> Vec<crate::systems::economy::mail::BaseMail> {
    server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .filter(|(_, m)| m.header.receiver_id == receiver)
        .map(|(_, m)| m.clone())
        .collect()
}

/// A lot whose escrow item expires before the auction ends is never delivered: the winner's
/// bid is refunded in full by the normal bid-fail mail and the item is deleted; the seller
/// gets neither the item nor a sale mail.
#[tokio::test]
async fn an_item_that_expires_in_escrow_refunds_the_winner_and_is_deleted() {
    let (mut w, log) = auction_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 0, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 700));
    assert_eq!(money(&w.pb), 100_000 - 700);
    log.take();
    w.server
        .items
        .update_item(item_id, |item| item.expiration_time = 1);
    let ah = &w.server.systems.economy.auction;
    ah.update_auction_house(&w.server, lot.end_time + Duration::seconds(1));
    assert!(ah.lot(lot.id).is_none());
    assert!(ah.deleted_ids().contains(&lot.id));
    assert!(w.server.items.item_by_id(item_id).is_none());
    assert_eq!(
        log.take(),
        vec![AuctionMail::BidFail {
            item_template_id: SWORD,
            seller_id: 1,
            buyout_price: 0,
            listing_fee: 0,
            previous_buyer_id: 2,
            previous_bid: 700,
        }],
        "the winner is refunded in full; no sale or seller mail is sent"
    );
}

/// A bid or buyout on a lot whose escrow item already expired is refused before the debit, so
/// the buyer's money never moves and no refund/duplicate can happen later.
#[tokio::test]
async fn an_expired_lot_refuses_bids_and_buyouts_before_any_money_moves() {
    let (mut w, _log) = auction_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    w.server
        .items
        .update_item(item_id, |item| item.expiration_time = 1);
    w.b.drain();
    let before = money(&w.pb);
    for bid in [10, 1000] {
        run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, bid));
        let frames = w.b.drain();
        assert_eq!(frames.len(), 1, "only the refusal frame");
        assert_eq!(frames[0].0, sc::SCErrorMsgPacket);
        assert_eq!(
            i16::from_le_bytes([frames[0].1[0], frames[0].1[1]]),
            crate::systems::economy::error_message::CAN_NOT_PUTUP_ITEM
        );
        assert_eq!(money(&w.pb), before, "no money moved for bid {bid}");
        let current = w.server.systems.economy.auction.lot(lot.id).unwrap();
        assert_eq!((current.bidder_id, current.bid_money), (0, 0));
    }
}

/// A failed refund mail at settlement changes nothing: the lot and its bidder survive for a
/// retry. Once the mail can be delivered the item is released exactly once and the lot is
/// removed, so a second tick cannot refund again.
#[tokio::test]
async fn a_failed_expired_lot_refund_keeps_the_lot_and_retries_once() {
    let (mut w, log) = flaky_world();
    let (item_id, lot) = post(&mut w, SWORD, 0, 0, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 700));
    assert_eq!(money(&w.pb), 100_000 - 700);
    w.server
        .items
        .update_item(item_id, |item| item.expiration_time = 1);

    let ah = &w.server.systems.economy.auction;
    log.accept.store(false, Ordering::SeqCst);
    ah.update_auction_house(&w.server, lot.end_time + Duration::seconds(1));
    assert!(
        ah.lot(lot.id).is_some(),
        "the lot waits for the refund mail"
    );
    let pending = ah.lot(lot.id).unwrap();
    assert_eq!((pending.bidder_id, pending.bid_money), (2, 700));
    assert!(w.server.items.item_by_id(item_id).is_some());
    assert!(log.delivered.lock().unwrap().is_empty());
    assert_eq!(log.attempts.lock().unwrap().len(), 1);

    log.accept.store(true, Ordering::SeqCst);
    ah.update_auction_house(&w.server, lot.end_time + Duration::seconds(2));
    assert!(ah.lot(lot.id).is_none());
    assert!(w.server.items.item_by_id(item_id).is_none());
    let refunds: Vec<AuctionMail> = log
        .delivered
        .lock()
        .unwrap()
        .iter()
        .filter(|mail| matches!(mail, AuctionMail::BidFail { .. }))
        .cloned()
        .collect();
    assert_eq!(
        refunds,
        vec![AuctionMail::BidFail {
            item_template_id: SWORD,
            seller_id: 1,
            buyout_price: 0,
            listing_fee: 0,
            previous_buyer_id: 2,
            previous_bid: 700,
        }],
        "exactly one full refund"
    );
    ah.update_auction_house(&w.server, lot.end_time + Duration::seconds(3));
    assert_eq!(
        log.delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|mail| matches!(mail, AuctionMail::BidFail { .. }))
            .count(),
        1,
        "the removed lot cannot refund again"
    );
}

/// An expired lot whose winner no longer resolves cannot be refunded; the item is released
/// once and the lot is removed instead of retrying a permanent failure forever.
#[tokio::test]
async fn an_unresolvable_expired_lot_winner_deletes_the_item_once_and_removes_the_lot() {
    let (mut w, _log) = auction_world();
    // The real mail port: a missing receiver name makes the refund mail fail at send.
    crate::systems::economy::wiring::install(&w.server);
    let (item_id, lot) = post(&mut w, SWORD, 0, 0, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 700));
    w.server
        .items
        .update_item(item_id, |item| item.expiration_time = 1);
    w.server.names.remove_character_id(2);

    let ah = &w.server.systems.economy.auction;
    ah.update_auction_house(&w.server, lot.end_time + Duration::seconds(1));
    assert!(ah.lot(lot.id).is_none());
    assert!(w.server.items.item_by_id(item_id).is_none());
    let deleted = |server: &GameServer| {
        server
            .items
            .removed_items()
            .iter()
            .filter(|id| **id == item_id)
            .count()
    };
    assert_eq!(deleted(&w.server), 1, "the item row is released once");
    ah.update_auction_house(&w.server, lot.end_time + Duration::seconds(2));
    assert_eq!(deleted(&w.server), 1, "no second release");
}

/// A delivery that expires before the buyer takes it is deleted instead of entering the bag.
/// This covers a mail persisted before the settlement guard and an item that expires while the
/// mail waits; the delivery mail carries no price, so no refund is minted here.
#[tokio::test]
async fn an_expired_auction_delivery_is_deleted_instead_of_taken() {
    let (mut w, _log) = auction_world();
    crate::systems::economy::wiring::install(&w.server);
    let (item_id, lot) = post(&mut w, SWORD, 0, 0, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 500));
    w.server
        .systems
        .economy
        .auction
        .update_auction_house(&w.server, lot.end_time + Duration::seconds(1));
    let winner = mails_to(&w.server, 2)
        .into_iter()
        .find(|mail| mail.mail_type == crate::systems::economy::mail::mail_type::AUC_BID_WIN)
        .expect("the winner mail");
    assert_eq!(winner.body.attachments, vec![item_id]);
    w.server
        .items
        .update_item(item_id, |item| item.expiration_time = 1);
    w.b.drain();
    run::<crate::systems::economy::mail_packets::CSTakeAttachmentSequentially>(
        &mut w,
        1,
        &winner.id.to_le_bytes(),
    );
    // The drop is reported the same way the item timer reports an expired bag item.
    let frames = w.b.drain();
    let lifespan = frames
        .iter()
        .find(|(op, _)| *op == sc::SCSyncItemLifespanPacket)
        .expect("the expired attachment is reported like a bag item");
    let mut expected = PacketStream::new();
    expected
        .write_bool(false)
        .write_u64(item_id)
        .write_u32(SWORD)
        .write_i64(0);
    assert_eq!(lifespan.1, expected.into_bytes());
    assert!(w.server.items.item_by_id(item_id).is_none());
    assert!(!Inv::of(&w.pb)
        .bag()
        .lock()
        .unwrap()
        .items
        .iter()
        .any(|item| item.id == item_id));
    let mail = w
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(winner.id)
        .expect("the mail");
    assert!(mail.body.attachments.is_empty());
    assert_eq!(mail.header.attachments, 0);
}

#[tokio::test]
async fn buyout_delivers_item_and_payment_by_mail() {
    let (mut w, _log) = auction_world();
    crate::systems::economy::wiring::install(&w.server);
    let (item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
    run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
    assert_eq!(money(&w.pb), 100_000 - 1000);
    assert!(w.server.systems.economy.auction.lot(lot.id).is_none());

    // Bob: the won item as attachment.
    let bob = mails_to(&w.server, 2);
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].body.attachments, vec![item_id]);
    w.b.drain();
    run::<crate::systems::economy::mail_packets::CSTakeAllAttachmentItemPacket>(
        &mut w,
        1,
        &bob[0].id.to_le_bytes(),
    );
    let bag_item = Inv::of(&w.pb)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id(item_id)
        .cloned();
    assert_eq!(
        bag_item.map(|i| (i.owner_id, i.slot_type)),
        Some((2, slot_type::INVENTORY))
    );

    // Alice: 90 % of the price (taking auction money costs 1 labor).
    w.pa.data().labor_power = 10;
    let alice = mails_to(&w.server, 1);
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].body.copper_coins, 900);
    let before = money(&w.pa);
    run::<crate::systems::economy::mail_packets::CSTakeAttachmentMoneyPacket>(
        &mut w,
        0,
        &alice[0].id.to_le_bytes(),
    );
    assert_eq!(money(&w.pa), before + 900);
}

/// Post -> cancel (refund mail) -> take-all-and-delete on the cancel mail, through the real mail
/// wiring. The Bevy client hit a server wedge here; the whole flow must finish.
#[tokio::test]
async fn cancel_refund_mail_can_be_taken_all_and_deleted() {
    let (mut w, _log) = auction_world();
    crate::systems::economy::wiring::install(&w.server);
    let ran = crate::test_support::finishes_in_time("cancel refund take-all", move || {
        let (_item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
        run::<CSCancelAuctionPacket>(&mut w, 0, &cancel_body(lot.id));
        let mails = mails_to(&w.server, 1);
        assert_eq!(mails.len(), 1, "the refund mail");
        let mail = mails[0].clone();
        w.a.drain();
        run::<crate::systems::economy::mail_packets::CSTakeAllAttachmentItemPacket>(
            &mut w,
            0,
            &mail.id.to_le_bytes(),
        );
        w.a.drain();
        // The player can still act afterwards: a second operation takes the ops lock again.
        let _ = w.server.systems.economy.ops_lock();
        mails_to(&w.server, 1).len()
    });
    assert_eq!(ran, 0, "take-all deleted the refund mail");
}

/// The seller's sale proceeds and the buyer's item, each taken with take-all (which deletes the
/// mail), through the real mail wiring: money, labor and delete must not wedge the server.
#[tokio::test]
async fn sale_and_win_mails_can_be_taken_all_and_deleted() {
    let (mut w, _log) = auction_world();
    crate::systems::economy::wiring::install(&w.server);
    let ran = crate::test_support::finishes_in_time("sale take-all", move || {
        let (_item_id, lot) = post(&mut w, SWORD, 0, 1000, 1);
        run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
        w.pa.data().labor_power = 10;
        for (which, receiver) in [(1u8, 2u32), (0u8, 1u32)] {
            let mail = mails_to(&w.server, receiver).remove(0);
            run::<crate::systems::economy::mail_packets::CSTakeAllAttachmentItemPacket>(
                &mut w,
                which,
                &mail.id.to_le_bytes(),
            );
            assert!(mails_to(&w.server, receiver).is_empty());
        }
        let _ = w.server.systems.economy.ops_lock();
        w.pa.data().money
    });
    // 20 listing fee for a 1 h lot, 90 % of the 1000 price received.
    assert_eq!(ran, 100_000 - 20 + 900);
}

/// The outbid refund (`AucBidFail` money mail) taken with take-all-and-delete: the exact
/// sequence the Bevy client's live auction test hung on (bid, buyout by someone else, claim the
/// refund).
#[tokio::test]
async fn outbid_refund_mail_can_be_taken_all_and_deleted() {
    use crate::net::registry::ClientPacket;
    let (w, _log) = auction_world();
    crate::systems::economy::wiring::install(&w.server);
    let ran = crate::test_support::finishes_in_time("outbid refund take-all", move || {
        let mut w = w;
        let mut c = TestClient::new(&w.server, 3, 1003);
        let pc = spawn_player(
            &w.server,
            &c,
            test_character(3, "Carol", 102.0, 100.0),
            0x102,
        );
        c.conn.active_char = Some(pc.clone());
        for (_, container) in Inv::of(&pc).containers {
            w.server.items.add_container(container);
        }
        w.server.names.add_character(3, "Carol", 1003);
        pc.data().money = 100_000;
        c.drain();

        let (_item_id, lot) = post(&mut w, SWORD, 0, 100, 1);
        // Bob bids below the buyout, Carol buys it out: Bob's bid comes back by mail.
        run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 10));
        assert_eq!(money(&w.pb), 100_000 - 10);
        let mut s = PacketStream::from_bytes(bid_body(lot.id, 100));
        let bid = CSBidAuctionPacket::read(&mut s).unwrap();
        futures::executor::block_on(bid.handle(&w.server, &mut c.conn)).unwrap();
        assert!(w.server.systems.economy.auction.lot(lot.id).is_none());

        let refund = mails_to(&w.server, 2)
            .into_iter()
            .find(|m| m.body.copper_coins == 10)
            .expect("Bob's refund mail");
        w.b.drain();
        run::<crate::systems::economy::mail_packets::CSTakeAllAttachmentItemPacket>(
            &mut w,
            1,
            &refund.id.to_le_bytes(),
        );
        w.b.drain();
        let _ = w.server.systems.economy.ops_lock();
        (
            money(&w.pb),
            mails_to(&w.server, 2).iter().any(|m| m.id == refund.id),
        )
    });
    assert_eq!(ran, (100_000, false), "refund paid back, mail deleted");
}

#[tokio::test]
async fn auction_requires_an_auctioneer_in_range() {
    let (mut w, _log) = auction_world();
    let pa = w.pa.clone();
    let item_id = give(&w, &pa, slot_type::INVENTORY, POTION, 1, 0);
    // Wrong NPC id (no such NPC): refused with the auctioneer-too-far error.
    let mut s = PacketStream::new();
    s.write_bc(0x999)
        .write_bc(0x999)
        .write_u64(item_id)
        .write_i32(0)
        .write_i32(1000)
        .write_u8(1);
    w.a.drain();
    run::<CSAuctionPostPacket>(&mut w, 0, s.as_bytes());
    assert!(w.server.systems.economy.auction.all_lots().is_empty());
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, sc::SCErrorMsgPacket);
    // The real auctioneer works; moved out of range it does not.
    run::<CSAuctionPostPacket>(&mut w, 0, &post_body(item_id, 0, 1000, 1));
    assert_eq!(w.server.systems.economy.auction.all_lots().len(), 1);
    pa.data().unit.transform.set_xyz(200.0, 100.0, 100.0);
    w.a.drain();
    run::<CSAuctionSearchPacket>(&mut w, 0, &search_body(&AuctionSearch::default()));
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, sc::SCErrorMsgPacket);
}

/// An already expired item cannot be listed: no container timer visits the auction container,
/// so it would otherwise be sold and delivered after its lifetime ended. The client's existing
/// invalid-item code (240, "cannot put up the item") is used and the item stays in the bag.
#[tokio::test]
async fn an_expired_item_cannot_be_posted_and_reports_the_invalid_item_code() {
    let (mut w, _log) = auction_world();
    let pa = w.pa.clone();
    let item_id = give(&w, &pa, slot_type::INVENTORY, SWORD, 1, 0);
    w.server
        .items
        .update_item(item_id, |item| item.expiration_time = 1);
    w.a.drain();
    run::<CSAuctionPostPacket>(&mut w, 0, &post_body(item_id, 0, 1000, 1));
    assert!(w.server.systems.economy.auction.all_lots().is_empty());
    assert_eq!(
        Inv::of(&pa).item_by_id(item_id).map(|item| item.slot_type),
        Some(slot_type::INVENTORY),
        "the expired item stays in the bag"
    );
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, sc::SCErrorMsgPacket);
    assert_eq!(
        i16::from_le_bytes([frames[0].1[0], frames[0].1[1]]),
        crate::systems::economy::error_message::CAN_NOT_PUTUP_ITEM
    );
}

/// Posting, searching, buying and cancelling are the auction house's everyday traffic: they must
/// not log at `WARN` (a rejected request still does).
#[tokio::test]
async fn auction_requests_are_not_logged_at_warn() {
    use tracing::Level;
    let (mut w, _log) = auction_world();
    let (_, events) = crate::test_support::capture_events(|| {
        let (_, lot) = post(&mut w, SWORD, 0, 1000, 2);
        run::<CSAuctionSearchPacket>(&mut w, 1, &search_body(&AuctionSearch::default()));
        // Bob pays the buyout price.
        run::<CSBidAuctionPacket>(&mut w, 1, &bid_body(lot.id, 1000));
        let (_, mine) = post(&mut w, POTION, 1, 1000, 0);
        run::<CSCancelAuctionPacket>(&mut w, 0, &cancel_body(mine.id));
    });
    assert!(w.server.systems.economy.auction.all_lots().is_empty());
    let loud: Vec<_> = events.iter().filter(|(l, _)| *l <= Level::WARN).collect();
    assert!(loud.is_empty(), "{loud:?}");
    for what in [
        "AuctionPost,",
        "AuctionSearch,",
        "AuctionBid,",
        "AuctionCancel,",
    ] {
        assert!(
            events
                .iter()
                .any(|(l, m)| *l == Level::DEBUG && m.starts_with(what)),
            "{what}: {events:?}"
        );
    }
}
