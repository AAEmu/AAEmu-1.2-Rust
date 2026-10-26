use super::*;
use crate::data::items::ItemTemplate;
use crate::data::GameData;
use crate::models::character::Inventory;
use crate::models::container::slot_type;
use crate::models::house::House;
use crate::models::item_task::ItemFx;
use crate::systems::housing;
use crate::test_support::{
    spawn_player, test_character, test_server, test_server_with, TestClient,
};
use crate::world::{Entity, HouseRef, PlayerRef};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn direct_ops_lock_waits_for_the_outer_scope() {
    let economy = Arc::new(System::default());
    let scope = economy.operation_scope();
    let worker_economy = economy.clone();
    let (done, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ops = worker_economy.ops_lock();
        done.send(()).unwrap();
    });
    economy.wait_until_operation_blocked();
    assert!(received.try_recv().is_err());
    drop(scope);
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
}

#[test]
fn failure_and_panic_release_every_scope_depth() {
    let economy = Arc::new(System::default());
    let fail = || -> Option<()> {
        let _outer = economy.operation_scope();
        let _inner = economy.operation_scope();
        None
    };
    assert!(fail().is_none());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _outer = economy.operation_scope();
        let _ops = economy.ops_lock();
        panic!("deliberate publication failure");
    }));
    assert!(result.is_err());
    assert!(!System::ops_held_on_this_thread());
    let worker_economy = economy.clone();
    let (done, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ops = worker_economy.ops_lock();
        done.send(()).unwrap();
    });
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
}

#[tokio::test]
async fn scopes_of_different_game_servers_are_independent() {
    let first = Arc::new(test_server());
    let second = Arc::new(test_server());
    let _first_scope = first.systems.economy.operation_scope();
    let (done, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ops = second.systems.economy.ops_lock();
        done.send(()).unwrap();
    });
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
}

struct Reenter(AtomicUsize);

impl hooks::QuestItemHooks for Reenter {
    fn on_items_acquired(&self, server: &GameServer, _player: &PlayerRef, _id: u32, _count: i32) {
        assert!(!System::ops_held_on_this_thread());
        let _callback = inventory::ItemTxn::begin(server);
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn item_callbacks_reenter_the_outer_scope_after_releasing_ops() {
    let server = Arc::new(test_server());
    server.set_self();
    let client = TestClient::new(&server, 1, 1);
    spawn_player(
        &server,
        &client,
        test_character(1, "Owner", 100.0, 100.0),
        0x100,
    );
    let callback = Arc::new(Reenter(AtomicUsize::new(0)));
    server
        .systems
        .economy
        .hooks
        .quests
        .install(callback.clone());
    let _publication = server.systems.economy.operation_scope();
    let mut txn = inventory::ItemTxn::begin(&server);
    txn.env.fx.push(ItemFx::Acquired {
        owner: 1,
        template_id: 1,
        count: 1,
    });
    drop(txn);
    assert_eq!(callback.0.load(Ordering::SeqCst), 1);
    assert!(!System::ops_held_on_this_thread());
}

/// A publication callback reaches the same managers as financial/property operations,
/// then starts an ordinary item operation. try_lock makes an ordering regression fail
/// immediately instead of deadlocking the test's publication thread.
struct PublicationCallback {
    active: AtomicBool,
    calls: AtomicUsize,
}

impl hooks::QuestItemHooks for PublicationCallback {
    fn on_items_acquired(&self, server: &GameServer, _player: &PlayerRef, _id: u32, _count: i32) {
        if !self.active.load(Ordering::SeqCst) {
            return;
        }
        assert!(!System::ops_held_on_this_thread());
        {
            let _auction = server
                .systems
                .economy
                .auction
                .try_operation_lock()
                .expect("waiting operation must not hold auction before the outer scope");
            let _housing = server
                .systems
                .housing
                .operation_lock
                .try_lock()
                .expect("waiting operation must not hold housing before the outer scope");
        }
        let _callback = inventory::ItemTxn::begin(server);
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

fn contend_with_publication(
    server: &Arc<GameServer>,
    operation: impl FnOnce(&GameServer) + Send + 'static,
) {
    let callback = Arc::new(PublicationCallback {
        active: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    server
        .systems
        .economy
        .hooks
        .quests
        .install(callback.clone());
    let publication = server.systems.economy.operation_scope();
    let worker_server = server.clone();
    let runtime = tokio::runtime::Handle::current();
    let (done, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _runtime = runtime.enter();
        operation(&worker_server);
        done.send(()).unwrap();
    });
    // This observes the real gate's waiter count, not a pre-entry barrier. The worker
    // cannot finish until publication releases its scope, and holds no manager yet.
    server.systems.economy.wait_until_operation_blocked();
    assert!(received.try_recv().is_err());
    let mut txn = inventory::ItemTxn::begin(server);
    txn.env.fx.push(ItemFx::Acquired {
        owner: 1,
        template_id: 1,
        count: 1,
    });
    drop(txn);
    assert_eq!(callback.calls.load(Ordering::SeqCst), 1);
    callback.active.store(false, Ordering::SeqCst);
    drop(publication);
    received
        .recv_timeout(Duration::from_secs(5))
        .expect("operation must finish after publication releases the scope");
    worker.join().unwrap();
}

const LISTED_ITEM: u32 = 5001;
const HOUSE: u32 = 300;

fn property_server() -> (Arc<GameServer>, PlayerRef) {
    let mut data = GameData::empty();
    for id in [LISTED_ITEM, housing::manager::APPRAISAL_CERTIFICATE] {
        data.items
            .templates
            .insert(id, ItemTemplate::plain(id, 100));
    }
    data.systems
        .housing
        .add_template(housing::data::HousingTemplate {
            id: HOUSE,
            is_sellable: true,
            ..Default::default()
        });
    let server = Arc::new(test_server_with(data));
    server.set_self();
    server
        .systems
        .housing
        .hooks
        .install(Arc::new(bridges::EconomyForHousing));
    let client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Owner", 100.0, 100.0);
    character.inventory = Inventory::new(1, 50, 50, &server.items, &server.ids.container);
    let player = spawn_player(&server, &client, character, 0x100);
    player.data().money = 100_000;
    server.names.add_character(1, "Owner", 1001);
    (server, player)
}

fn give_property_item(server: &GameServer, player: &PlayerRef, template: u32, count: i32) -> u64 {
    let item = server
        .items
        .create(
            template,
            count,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .unwrap();
    let id = item.id;
    inventory::Inv::of(player)
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, -1, &server.game_data.items)
        .unwrap();
    id
}

fn sale_house(server: &GameServer, player: &PlayerRef) -> HouseRef {
    let house = Entity::new(
        server.ids.object.get_next_id().unwrap(),
        House::new(1, HOUSE),
    );
    {
        let mut h = house.data();
        h.unit.owner_id = player.character_id;
        h.protection_end_date = housing::manager::now() + 3600;
        h.current_step = -1;
    }
    house
}

#[tokio::test]
async fn financial_capture_waits_before_auction_and_housing_callback_locks() {
    let (server, _) = property_server();
    contend_with_publication(&server, |server| {
        let snapshot = crate::managers::save::capture_financial_snapshot(server);
        assert_eq!(snapshot.characters.len(), 1);
    });
}

#[tokio::test]
async fn auction_list_and_bid_wait_before_publication_callback_locks() {
    let (server, seller) = property_server();
    let item = give_property_item(&server, &seller, LISTED_ITEM, 1);
    let listing_seller = seller.clone();
    contend_with_publication(&server, move |server| {
        server.systems.economy.auction.post_lot_on_auction(
            server,
            &listing_seller,
            item,
            10,
            1000,
            auction::auction_duration::HOURS_6,
        );
    });
    let lot = server.systems.economy.auction.all_lots().pop().unwrap();
    assert_eq!(lot.item_id(), item);
    assert_eq!(
        server.items.item_by_id(item).unwrap().slot_type,
        slot_type::AUCTION
    );
    let client = TestClient::new(&server, 2, 1002);
    let buyer = spawn_player(
        &server,
        &client,
        test_character(2, "Buyer", 101.0, 100.0),
        0x101,
    );
    buyer.data().money = 100;
    let bidder = buyer.clone();
    contend_with_publication(&server, move |server| {
        server.systems.economy.auction.bid_on_auction_lot(
            server,
            &bidder,
            lot.id,
            auction::AuctionBid {
                lot_id: lot.id,
                money: 20,
                ..Default::default()
            },
        );
    });
    assert_eq!(buyer.data().money, 80);
    assert_eq!(server.systems.economy.auction.all_lots()[0].bidder_id, 2);
}

#[tokio::test]
async fn housing_sale_waits_before_callback_locks_then_uses_real_economy_bridge() {
    let (server, seller) = property_server();
    let certificate =
        give_property_item(&server, &seller, housing::manager::APPRAISAL_CERTIFICATE, 2);
    let house = sale_house(&server, &seller);
    let listed_house = house.clone();
    contend_with_publication(&server, move |server| {
        assert!(housing::ops::set_for_sale(
            server,
            &listed_house,
            100_000,
            0,
            Some(&seller)
        ));
    });
    assert_eq!(house.data().sell_price, 100_000);
    assert_eq!(server.items.item_by_id(certificate).unwrap().count, 1);
}

#[tokio::test]
async fn housing_build_refusal_waits_before_callback_locks() {
    let (server, player) = property_server();
    let item = give_property_item(&server, &player, LISTED_ITEM, 1);
    contend_with_publication(&server, move |server| {
        // Owned item but no design mapping: the normal construction refusal must still
        // enter in the same order and leave its item and wallet untouched.
        housing::ops::build(
            server,
            &player,
            1001,
            &housing::packets::CSCreateHousePacket {
                design_id: HOUSE,
                x: 100.0,
                y: 100.0,
                z: 100.0,
                z_rot: 0.0,
                item_id: item,
                money_amount: 0,
                ht: 0,
                auto_use_aa_point: false,
            },
        );
        assert_eq!(player.data().money, 100_000);
    });
    assert_eq!(
        server.items.item_by_id(item).unwrap().slot_type,
        slot_type::INVENTORY
    );
    assert!(server.systems.housing.manager.all().is_empty());
}
