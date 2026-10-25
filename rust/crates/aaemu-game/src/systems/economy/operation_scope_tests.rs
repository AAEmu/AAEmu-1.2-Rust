use super::*;
use crate::models::item_task::ItemFx;
use crate::test_support::{spawn_player, test_character, test_server, TestClient};
use crate::world::PlayerRef;
use std::sync::atomic::{AtomicUsize, Ordering};
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
