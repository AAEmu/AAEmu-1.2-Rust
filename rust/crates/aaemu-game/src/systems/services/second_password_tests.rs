//! Flow tests for the 1.2 second password: the packet handlers run against an in-memory store
//! and a counting hasher, so the lockout state machine, the one-shot key tables and the "no
//! hashing while locked" rule are checked without MySQL.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaemu_commons::password::PasswordService;
use aaemu_commons::PacketStream;
use futures::future::BoxFuture;

use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::{test_server, TestClient};

use super::*;

#[derive(Debug, Default)]
struct MemoryStore {
    rows: Mutex<HashMap<u32, AccountSecondPassword>>,
}

impl MemoryStore {
    fn row(&self, account_id: u32) -> AccountSecondPassword {
        self.rows
            .lock()
            .unwrap()
            .get(&account_id)
            .cloned()
            .unwrap_or_default()
    }

    fn hash(&self, account_id: u32) -> Option<String> {
        self.row(account_id).hash
    }

    fn set_row(&self, account_id: u32, row: AccountSecondPassword) {
        self.rows.lock().unwrap().insert(account_id, row);
    }
}

impl SecondPasswordStore for MemoryStore {
    fn load<'a>(
        &'a self,
        _server: &'a GameServer,
        account_id: u32,
    ) -> BoxFuture<'a, Option<AccountSecondPassword>> {
        Box::pin(async move { Some(self.row(account_id)) })
    }

    fn set_hash<'a>(
        &'a self,
        _server: &'a GameServer,
        account_id: u32,
        hash: String,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut rows = self.rows.lock().unwrap();
            let row = rows.entry(account_id).or_default();
            row.hash = Some(hash);
            row.failed = 0;
            row.locked_until = 0;
            true
        })
    }

    fn set_failed<'a>(
        &'a self,
        _server: &'a GameServer,
        account_id: u32,
        failed: u8,
        locked_until: i64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut rows = self.rows.lock().unwrap();
            let row = rows.entry(account_id).or_default();
            row.failed = failed;
            row.locked_until = locked_until;
            true
        })
    }

    fn clear<'a>(&'a self, _server: &'a GameServer, account_id: u32) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.rows.lock().unwrap().remove(&account_id);
            true
        })
    }
}

/// The production hasher with a cheap iteration count plus call counters. Hashing still goes
/// through `aaemu_commons::password::PasswordService`.
#[derive(Debug)]
struct CountingHasher {
    service: PasswordService,
    hashes: AtomicUsize,
    verifies: AtomicUsize,
}

impl CountingHasher {
    fn new() -> Self {
        Self {
            service: PasswordService { iterations: 1_000 },
            hashes: AtomicUsize::new(0),
            verifies: AtomicUsize::new(0),
        }
    }

    fn hashes(&self) -> usize {
        self.hashes.load(Ordering::SeqCst)
    }

    fn verifies(&self) -> usize {
        self.verifies.load(Ordering::SeqCst)
    }
}

impl SecondPasswordHasher for CountingHasher {
    fn hash(&self, plaintext: &str) -> Option<String> {
        self.hashes.fetch_add(1, Ordering::SeqCst);
        self.service
            .hash_for_storage(&aaemu_commons::password::Password::from_plaintext(
                plaintext,
            ))
            .ok()
    }

    fn verify(&self, stored: &str, plaintext: &str) -> bool {
        self.verifies.fetch_add(1, Ordering::SeqCst);
        self.service.verify_password(
            stored,
            &aaemu_commons::password::Password::from_plaintext(plaintext),
        ) != aaemu_commons::password::PasswordVerificationResult::Failed
    }
}

fn server_with(store: Arc<dyn SecondPasswordStore>, hasher: Arc<CountingHasher>) -> GameServer {
    let server = test_server();
    server.config.update(|config| {
        config.second_password.enabled = true;
        config.second_password.max_fail_count = 5;
        config.second_password.lock_minutes = 10;
    });
    server.systems.services.second_password.store.install(store);
    server
        .systems
        .services
        .second_password
        .hasher
        .install(hasher);
    server
}

fn read_packet<T: ClientPacket>(body: Vec<u8>) -> T {
    T::read(&mut PacketStream::from_bytes(body)).expect("packet body")
}

async fn request_tables(server: &GameServer, client: &mut TestClient, wire_type: u8) -> KeyTables {
    let mut body = PacketStream::new();
    body.write_u8(wire_type);
    let packet: CSRequestSecondPasswordKeyTablesPacket = read_packet(body.into_bytes());
    packet
        .handle(server, &mut client.conn)
        .await
        .expect("key tables request");
    let frames = client.drain();
    assert_eq!(
        frames.len(),
        1,
        "one key tables packet expected: {frames:?}"
    );
    let (opcode, body) = &frames[0];
    assert_eq!(*opcode, sc::SCSecondPassKeyTablesPacket);
    let mut stream = PacketStream::from_bytes(body.clone());
    assert_eq!(stream.read_u8().unwrap(), wire_type);
    let key_id = stream.read_u32().unwrap();
    let tables = std::array::from_fn(|_| stream.read_string().unwrap());
    KeyTables { key_id, tables }
}

fn substitute(table: &str, plain: &str) -> String {
    let chars: Vec<char> = table.chars().collect();
    plain
        .chars()
        .map(|c| {
            let position = ALPHABET.find(c).expect("alphabet member");
            chars[position]
        })
        .collect()
}

async fn send_create(
    server: &GameServer,
    client: &mut TestClient,
    tables: &KeyTables,
    table_index: u8,
    plain: &str,
) {
    let sent = substitute(&tables.tables[usize::from(table_index)], plain);
    let mut body = PacketStream::new();
    body.write_u32(tables.key_id)
        .write_u8(table_index)
        .write_str(&sent);
    let packet: CSCreateSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(server, &mut client.conn)
        .await
        .expect("create");
}

async fn send_change(
    server: &GameServer,
    client: &mut TestClient,
    tables: &KeyTables,
    old_index: u8,
    old_plain: &str,
    new_index: u8,
    new_plain: &str,
) {
    let old_sent = substitute(&tables.tables[usize::from(old_index)], old_plain);
    let new_sent = substitute(&tables.tables[usize::from(new_index)], new_plain);
    let mut body = PacketStream::new();
    body.write_u32(tables.key_id)
        .write_u8(old_index)
        .write_u8(new_index)
        .write_str(&old_sent)
        .write_str(&new_sent);
    let packet: CSChangeSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(server, &mut client.conn)
        .await
        .expect("change");
}

async fn send_clear(
    server: &GameServer,
    client: &mut TestClient,
    tables: &KeyTables,
    table_index: u8,
    plain: &str,
) {
    let sent = substitute(&tables.tables[usize::from(table_index)], plain);
    let mut body = PacketStream::new();
    body.write_u32(tables.key_id)
        .write_u8(table_index)
        .write_str(&sent);
    let packet: CSClearSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(server, &mut client.conn)
        .await
        .expect("clear");
}

async fn send_check(
    server: &GameServer,
    client: &mut TestClient,
    tables: &KeyTables,
    table_index: u8,
    plain: &str,
) {
    let sent = substitute(&tables.tables[usize::from(table_index)], plain);
    let mut body = PacketStream::new();
    body.write_u32(tables.key_id)
        .write_u8(table_index)
        .write_str(&sent);
    let packet: CSCheckSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(server, &mut client.conn)
        .await
        .expect("check");
}

fn created(frames: &[(u16, Vec<u8>)]) -> Option<bool> {
    frames
        .iter()
        .find(|(opcode, _)| *opcode == sc::SCSecondPassCreatedPacket)
        .map(|(_, body)| body[0] != 0)
}

fn checked(frames: &[(u16, Vec<u8>)]) -> Option<(bool, u8)> {
    frames
        .iter()
        .find(|(opcode, _)| *opcode == sc::SCSecondPassCheckedPacket)
        .map(|(_, body)| (body[0] != 0, body[1]))
}

fn changed(frames: &[(u16, Vec<u8>)]) -> Option<(u8, u8)> {
    frames
        .iter()
        .find(|(opcode, _)| *opcode == sc::SCSecondPassChangedPacket)
        .map(|(_, body)| (body[0], body[1]))
}

fn cleared(frames: &[(u16, Vec<u8>)]) -> Option<(bool, u8)> {
    frames
        .iter()
        .find(|(opcode, _)| *opcode == sc::SCSecondPassClearedPacket)
        .map(|(_, body)| (body[0] != 0, body[1]))
}

fn lock_over_failed(frames: &[(u16, Vec<u8>)]) -> Option<i64> {
    frames
        .iter()
        .find(|(opcode, _)| *opcode == sc::SCSecondPassCheckOverFailedPacket)
        .map(|(_, body)| i64::from_le_bytes(body[..8].try_into().unwrap()))
}

fn account_locked(frames: &[(u16, Vec<u8>)]) -> Option<i64> {
    frames
        .iter()
        .find(|(opcode, _)| *opcode == sc::SCSecondPassAccountLockedPacket)
        .map(|(_, body)| i64::from_le_bytes(body[..8].try_into().unwrap()))
}

fn settled(frames: &[(u16, Vec<u8>)]) -> Option<(bool, i64, u8)> {
    frames
        .iter()
        .find(|(opcode, _)| *opcode == sc::SCSecondPassSettedPacket)
        .map(|(_, body)| {
            let mut stream = PacketStream::from_bytes(body.clone());
            (
                stream.read_bool().unwrap(),
                stream.read_i64().unwrap(),
                stream.read_u8().unwrap(),
            )
        })
}

async fn create_password(
    server: &GameServer,
    client: &mut TestClient,
    account_id: u32,
    plain: &str,
) {
    let tables = request_tables(server, client, 1).await;
    send_create(server, client, &tables, 0, plain).await;
    let frames = client.drain();
    assert_eq!(created(&frames), Some(true), "{frames:?}");
    let _ = account_id;
}

#[tokio::test]
async fn create_check_change_and_clear_round_trip() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(store.clone(), hasher.clone());
    let mut client = TestClient::new(&server, 1, 42);
    let plain = "1234abcd";

    let tables = request_tables(&server, &mut client, 1).await;
    send_create(&server, &mut client, &tables, 0, plain).await;
    let frames = client.drain();
    assert_eq!(created(&frames), Some(true), "{frames:?}");
    assert_eq!(settled(&frames), Some((true, 0, 0)), "{frames:?}");
    let hash = store.hash(42).expect("stored hash");
    assert_ne!(hash, plain);
    assert!(!hash.contains(plain));
    assert_eq!(hasher.hashes(), 1);
    assert!(client.conn.second_password.passed);

    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 2, plain).await;
    let frames = client.drain();
    assert_eq!(checked(&frames), Some((true, 0)), "{frames:?}");

    let tables = request_tables(&server, &mut client, 2).await;
    send_change(&server, &mut client, &tables, 0, plain, 3, "5678wxyz").await;
    let frames = client.drain();
    assert_eq!(
        changed(&frames),
        Some((changed_result::SUCCESSED, 0)),
        "{frames:?}"
    );
    assert_ne!(store.hash(42).unwrap(), hash);
    assert_eq!(hasher.hashes(), 2);

    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 1, plain).await;
    let frames = client.drain();
    assert_eq!(checked(&frames), Some((false, 1)), "{frames:?}");

    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 2, "5678wxyz").await;
    let frames = client.drain();
    assert_eq!(checked(&frames), Some((true, 0)), "{frames:?}");
    assert_eq!(
        store.row(42).failed,
        0,
        "a successful check resets the count"
    );

    let tables = request_tables(&server, &mut client, 3).await;
    send_clear(&server, &mut client, &tables, 0, "5678wxyz").await;
    let frames = client.drain();
    assert_eq!(cleared(&frames), Some((true, 0)), "{frames:?}");
    assert_eq!(store.hash(42), None);
    assert!(client.conn.second_password.passed);

    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "anything").await;
    let frames = client.drain();
    assert_eq!(
        checked(&frames),
        Some((true, 0)),
        "an account without a second password is passed"
    );
}

#[tokio::test]
async fn wrong_password_counts_and_locks_at_five_without_hashing_while_locked() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(store.clone(), hasher.clone());
    let mut client = TestClient::new(&server, 1, 42);
    create_password(&server, &mut client, 42, "1234abcd").await;

    for expected in 1..=4u8 {
        let tables = request_tables(&server, &mut client, 4).await;
        send_check(&server, &mut client, &tables, 0, "wrongpass").await;
        let frames = client.drain();
        assert_eq!(checked(&frames), Some((false, expected)), "{frames:?}");
        assert_eq!(store.row(42).failed, expected);
        assert_eq!(store.row(42).locked_until, 0);
    }

    let before = hasher.verifies();
    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "wrongpass").await;
    let frames = client.drain();
    let unlock = lock_over_failed(&frames).expect("lock packet");
    let now = chrono::Utc::now().timestamp();
    assert!(
        unlock >= now + 590 && unlock <= now + 610,
        "10 minute lock, got {unlock} at {now}"
    );
    assert_eq!(store.row(42).failed, 5);
    assert_eq!(store.row(42).locked_until, unlock);
    assert_eq!(hasher.verifies(), before + 1);

    // Locked: even the correct password is not hashed and the account unlocked time comes back.
    let before = hasher.verifies();
    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "1234abcd").await;
    let frames = client.drain();
    assert_eq!(account_locked(&frames), Some(unlock), "{frames:?}");
    assert_eq!(hasher.verifies(), before, "no hashing while locked");
    assert!(!client.conn.second_password.passed);
}

#[tokio::test]
async fn locked_account_clears_the_lock_after_it_expires() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(store.clone(), hasher.clone());
    let mut client = TestClient::new(&server, 1, 42);
    create_password(&server, &mut client, 42, "1234abcd").await;
    let expired = chrono::Utc::now().timestamp() - 1;
    store.set_row(
        42,
        AccountSecondPassword {
            hash: store.hash(42),
            failed: 5,
            locked_until: expired,
        },
    );

    let before = hasher.verifies();
    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "1234abcd").await;
    let frames = client.drain();
    assert_eq!(account_locked(&frames), Some(0), "{frames:?}");
    assert_eq!(store.row(42).failed, 0);
    assert_eq!(store.row(42).locked_until, 0);
    assert_eq!(
        hasher.verifies(),
        before,
        "the expired lock is reset before hashing"
    );

    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "1234abcd").await;
    let frames = client.drain();
    assert_eq!(checked(&frames), Some((true, 0)), "{frames:?}");
}

#[tokio::test]
async fn key_tables_are_one_shot_per_connection() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(store.clone(), hasher.clone());
    let mut client = TestClient::new(&server, 1, 42);

    let tables = request_tables(&server, &mut client, 1).await;
    send_create(&server, &mut client, &tables, 0, "1234abcd").await;
    assert_eq!(created(&client.drain()), Some(true));
    assert_eq!(hasher.hashes(), 1);

    // A check consumes its tables: the same key id cannot be used a second time.
    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "1234abcd").await;
    assert_eq!(checked(&client.drain()), Some((true, 0)));
    send_check(&server, &mut client, &tables, 0, "1234abcd").await;
    let frames = client.drain();
    assert_eq!(
        checked(&frames),
        Some((false, 0)),
        "a replayed check key id is refused: {frames:?}"
    );

    // A wrong key id also consumes the pending tables: the retry with the real id is refused.
    let tables = request_tables(&server, &mut client, 4).await;
    let mut wrong = tables.clone();
    wrong.key_id = wrong.key_id.wrapping_add(1);
    send_check(&server, &mut client, &wrong, 0, "1234abcd").await;
    assert_eq!(checked(&client.drain()), Some((false, 0)));
    send_check(&server, &mut client, &tables, 0, "1234abcd").await;
    assert_eq!(checked(&client.drain()), Some((false, 0)));
}

#[tokio::test]
async fn create_enforces_the_4_to_31_length_and_alphabet() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(store.clone(), hasher.clone());
    let mut client = TestClient::new(&server, 1, 42);

    for bad in ["abc", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
        let tables = request_tables(&server, &mut client, 1).await;
        send_create(&server, &mut client, &tables, 0, bad).await;
        assert_eq!(created(&client.drain()), Some(false));
    }
    // A character that is not in the sent table cannot be decoded.
    let tables = request_tables(&server, &mut client, 1).await;
    let mut body = PacketStream::new();
    body.write_u32(tables.key_id).write_u8(0).write_str("!!!!");
    let packet: CSCreateSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(&server, &mut client.conn)
        .await
        .expect("create");
    assert_eq!(created(&client.drain()), Some(false));
    assert_eq!(hasher.hashes(), 0, "invalid input never reaches the hasher");
    assert_eq!(store.hash(42), None);

    let tables = request_tables(&server, &mut client, 1).await;
    send_create(&server, &mut client, &tables, 2, "abcd").await;
    assert_eq!(created(&client.drain()), Some(true));
    assert_eq!(hasher.hashes(), 1);
}

fn parse_key_tables(body: &[u8]) -> KeyTables {
    let mut stream = PacketStream::from_bytes(body.to_vec());
    let _wire_type = stream.read_u8().unwrap();
    let key_id = stream.read_u32().unwrap();
    KeyTables {
        key_id,
        tables: std::array::from_fn(|_| stream.read_string().unwrap()),
    }
}

#[tokio::test]
async fn lobby_actions_are_refused_until_the_second_password_is_checked() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(store.clone(), hasher.clone());
    let mut first = TestClient::new(&server, 1, 42);
    create_password(&server, &mut first, 42, "1234abcd").await;
    first.drain();

    // A new connection has passed = false and loads the state on the first gated action.
    let mut client = TestClient::new(&server, 2, 42);
    let select = crate::packets::c2g_world::CSSelectCharacterPacket {
        character_id: 1,
        gm: false,
        unk: 0,
    };
    select
        .handle(&server, &mut client.conn)
        .await
        .expect("select");
    let frames = client.drain();
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].0, sc::SCSecondPassKeyTablesPacket);
    assert_eq!(frames[0].1[0], 4, "the server pushes check tables (type 4)");
    assert!(!client.conn.second_password.passed);
    let pushed = parse_key_tables(&frames[0].1);

    // Check with the pushed tables, then the gate opens.
    send_check(&server, &mut client, &pushed, 0, "1234abcd").await;
    let frames = client.drain();
    assert_eq!(checked(&frames), Some((true, 0)), "{frames:?}");
    assert!(client.conn.second_password.passed);
    assert!(allow_lobby_action(&server, &mut client.conn)
        .await
        .expect("gate"));

    // Another connection without a check is refused for character creation too.
    let mut other = TestClient::new(&server, 3, 42);
    let create = crate::packets::c2g_world::CSCreateCharacterPacket(
        crate::managers::character::CreateCharacterRequest {
            name: "Tester".into(),
            race: 1,
            gender: 1,
            body_items: [0; 7],
            model_params: Default::default(),
            ability1: 7,
            ability2: 11,
            ability3: 11,
            level: 1,
        },
    );
    create
        .handle(&server, &mut other.conn)
        .await
        .expect("create");
    let frames = other.drain();
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].0, sc::SCSecondPassKeyTablesPacket);
    assert!(!other.conn.second_password.passed);
}

#[test]
fn zero_max_fail_count_is_refused_at_config_load() {
    let error = crate::config::AppConfig::from_value(serde_json::json!({
        "SecondPassword": { "Enabled": true, "MaxFailCount": 0 }
    }))
    .expect_err("MaxFailCount 0 must be refused");
    assert!(error.to_string().contains("MaxFailCount"), "{error}");

    let config = crate::config::AppConfig::from_value(serde_json::json!({
        "SecondPassword": { "Enabled": true, "MaxFailCount": 7, "LockMinutes": 3 }
    }))
    .expect("valid config");
    assert!(config.second_password.enabled);
    assert_eq!(config.second_password.max_fail_count, 7);
    assert_eq!(config.second_password.lock_minutes, 3);

    let zero_lock = crate::config::AppConfig::from_value(serde_json::json!({
        "SecondPassword": { "Enabled": true, "MaxFailCount": 5, "LockMinutes": 0 }
    }))
    .expect_err("LockMinutes 0 must be refused");
    assert!(zero_lock.to_string().contains("LockMinutes"), "{zero_lock}");

    let defaults = crate::config::AppConfig::default();
    assert_eq!(defaults.second_password.max_fail_count, 5);
    assert_eq!(defaults.second_password.lock_minutes, 10);
    assert!(!defaults.second_password.enabled);
}

#[tokio::test]
async fn parallel_wrong_checks_cannot_exceed_the_max_fail_count() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(store.clone(), hasher.clone());
    let mut first = TestClient::new(&server, 1, 42);
    create_password(&server, &mut first, 42, "1234abcd").await;
    first.drain();

    let mut clients: Vec<TestClient> = (0..8)
        .map(|i| TestClient::new(&server, 100 + i, 42))
        .collect();
    let mut packets = Vec::new();
    for client in &mut clients {
        let tables = request_tables(&server, client, 4).await;
        let sent = substitute(&tables.tables[0], "wrongpass");
        let mut body = PacketStream::new();
        body.write_u32(tables.key_id).write_u8(0).write_str(&sent);
        packets.push(read_packet::<CSCheckSecondPasswordPacket>(
            body.into_bytes(),
        ));
    }
    let futures = packets
        .into_iter()
        .zip(clients.iter_mut())
        .map(|(packet, client)| packet.handle(&server, &mut client.conn));
    let results = futures::future::join_all(futures).await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    assert_eq!(store.row(42).failed, 5, "the lock caps the counter");
    assert!(store.row(42).locked_until > chrono::Utc::now().timestamp());
    let mut lock_over = 0;
    let mut locked = 0;
    let mut checked_replies = Vec::new();
    for client in &mut clients {
        let frames = client.drain();
        lock_over += frames
            .iter()
            .filter(|(opcode, _)| *opcode == sc::SCSecondPassCheckOverFailedPacket)
            .count();
        locked += frames
            .iter()
            .filter(|(opcode, _)| *opcode == sc::SCSecondPassAccountLockedPacket)
            .count();
        if let Some(reply) = checked(&frames) {
            checked_replies.push(reply);
        }
    }
    assert_eq!(lock_over, 1, "exactly one attempt reaches the lock");
    assert_eq!(locked, 3, "the later attempts answer AccountLocked");
    assert_eq!(checked_replies.len(), 4);
    assert!(checked_replies
        .iter()
        .all(|(ok, failed)| !ok && *failed <= 5));
}

async fn private_pool() -> Option<sqlx::MySqlPool> {
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
        &crate::systems::social::achievement_persistence::temporary_shadow_definition(&definition),
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_second_password_store_round_trip() {
    let Some(pool) = private_pool().await else {
        return;
    };
    let mut server = test_server();
    server.db = pool;
    let store = MySqlSecondPasswordStore;
    assert_eq!(
        store.load(&server, 777).await,
        Some(AccountSecondPassword::default())
    );
    assert!(
        store
            .set_hash(&server, 777, "AQAAAA-second-password-hash".into())
            .await
    );
    let row = store.load(&server, 777).await.expect("row");
    assert_eq!(row.hash.as_deref(), Some("AQAAAA-second-password-hash"));
    assert_eq!(row.failed, 0);
    assert!(store.set_failed(&server, 777, 3, 1_900_000_000).await);
    let row = store.load(&server, 777).await.expect("row");
    assert_eq!(row.failed, 3);
    assert_eq!(row.locked_until, 1_900_000_000);
    assert!(store.clear(&server, 777).await);
    assert_eq!(
        store.load(&server, 777).await,
        Some(AccountSecondPassword::default())
    );
}

#[derive(Debug)]
struct FailingLoadStore;

impl SecondPasswordStore for FailingLoadStore {
    fn load<'a>(
        &'a self,
        _server: &'a GameServer,
        _account_id: u32,
    ) -> BoxFuture<'a, Option<AccountSecondPassword>> {
        Box::pin(async { None })
    }

    fn set_hash<'a>(
        &'a self,
        _server: &'a GameServer,
        _account_id: u32,
        _hash: String,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn set_failed<'a>(
        &'a self,
        _server: &'a GameServer,
        _account_id: u32,
        _failed: u8,
        _locked_until: i64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn clear<'a>(&'a self, _server: &'a GameServer, _account_id: u32) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
}

#[derive(Debug)]
struct FailingWriteStore {
    inner: Arc<MemoryStore>,
}

impl SecondPasswordStore for FailingWriteStore {
    fn load<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
    ) -> BoxFuture<'a, Option<AccountSecondPassword>> {
        self.inner.load(server, account_id)
    }

    fn set_hash<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        hash: String,
    ) -> BoxFuture<'a, bool> {
        self.inner.set_hash(server, account_id, hash)
    }

    fn set_failed<'a>(
        &'a self,
        _server: &'a GameServer,
        _account_id: u32,
        _failed: u8,
        _locked_until: i64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn clear<'a>(&'a self, server: &'a GameServer, account_id: u32) -> BoxFuture<'a, bool> {
        self.inner.clear(server, account_id)
    }
}

#[tokio::test]
async fn a_failed_store_load_fails_closed() {
    let hasher = Arc::new(CountingHasher::new());
    let server = server_with(Arc::new(FailingLoadStore), hasher.clone());
    let mut client = TestClient::new(&server, 1, 42);
    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "1234abcd").await;
    let frames = client.drain();
    assert_eq!(checked(&frames), Some((false, 0)), "{frames:?}");
    assert_eq!(
        hasher.verifies(),
        0,
        "a failed load never reaches the hasher"
    );
    assert!(!client.conn.second_password.passed);
    assert!(!allow_lobby_action(&server, &mut client.conn)
        .await
        .expect("gate"));
}

#[tokio::test]
async fn a_failure_that_cannot_be_recorded_refuses_further_attempts() {
    let inner = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let hash = hasher.hash("1234abcd").expect("hash");
    inner.set_row(
        42,
        AccountSecondPassword {
            hash: Some(hash),
            failed: 0,
            locked_until: 0,
        },
    );
    let server = server_with(
        Arc::new(FailingWriteStore {
            inner: inner.clone(),
        }),
        hasher.clone(),
    );
    let mut client = TestClient::new(&server, 1, 42);

    let tables = request_tables(&server, &mut client, 4).await;
    send_check(&server, &mut client, &tables, 0, "wrongpass").await;
    let frames = client.drain();
    assert_eq!(
        checked(&frames),
        Some((false, 1)),
        "the failed write still answers the failure: {frames:?}"
    );
    assert!(client.conn.second_password.fail_closed);
    assert!(!client.conn.second_password.passed);
    assert_eq!(inner.row(42).failed, 0, "nothing was persisted");

    // Every later attempt on the connection fails without hashing or touching the store.
    let before = hasher.verifies();
    let mut body = PacketStream::new();
    body.write_u32(0).write_u8(0).write_str("1234abcd");
    let packet: CSCheckSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(&server, &mut client.conn)
        .await
        .expect("check");
    assert_eq!(checked(&client.drain()), Some((false, 0)));
    assert_eq!(hasher.verifies(), before);

    let key_request: CSRequestSecondPasswordKeyTablesPacket = read_packet(vec![4]);
    key_request
        .handle(&server, &mut client.conn)
        .await
        .expect("key tables");
    assert!(client.drain().is_empty(), "no tables after a store failure");

    let create = CSCreateSecondPasswordPacket {
        key_id: 1,
        table_index: 0,
        pass: "x".into(),
    };
    create
        .handle(&server, &mut client.conn)
        .await
        .expect("create");
    assert_eq!(created(&client.drain()), Some(false));

    assert!(!allow_lobby_action(&server, &mut client.conn)
        .await
        .expect("gate"));
}

#[test]
fn debug_output_redacts_tables_and_passwords() {
    let tables = KeyTables {
        key_id: 7,
        tables: [
            "table-secret-0".into(),
            "table-secret-1".into(),
            "table-secret-2".into(),
            "table-secret-3".into(),
        ],
    };
    let debug = format!("{tables:?}");
    assert!(!debug.contains("table-secret"), "{debug}");
    assert!(debug.contains("redacted"), "{debug}");

    let packet = SCSecondPassKeyTablesPacket {
        key_type: 1,
        key_id: 7,
        tables: tables.tables.clone(),
    };
    let debug = format!("{packet:?}");
    assert!(!debug.contains("table-secret"), "{debug}");
    assert!(debug.contains("redacted"), "{debug}");

    let pending = PendingKeyTables {
        action: SecondPasswordAction::Create,
        key_tables: tables.clone(),
    };
    let state = ConnectionState {
        pending: Some(pending),
        passed: true,
        has_password: true,
        loaded: true,
        fail_closed: false,
    };
    let debug = format!("{state:?}");
    assert!(!debug.contains("table-secret"), "{debug}");

    let pass = "hunter2-secret";
    let create = CSCreateSecondPasswordPacket {
        key_id: 1,
        table_index: 0,
        pass: pass.into(),
    };
    let change = CSChangeSecondPasswordPacket {
        key_id: 1,
        old_pass_table_index: 0,
        new_pass_table_index: 1,
        old_pass: pass.into(),
        new_pass: pass.into(),
    };
    let clear = CSClearSecondPasswordPacket {
        key_id: 1,
        table_index: 0,
        pass: pass.into(),
    };
    let check = CSCheckSecondPasswordPacket {
        key_id: 1,
        table_index: 0,
        pass: pass.into(),
    };
    for debug in [
        format!("{create:?}"),
        format!("{change:?}"),
        format!("{clear:?}"),
        format!("{check:?}"),
    ] {
        assert!(!debug.contains(pass), "{debug}");
        assert!(debug.contains("redacted"), "{debug}");
    }

    let row = AccountSecondPassword {
        hash: Some("stored-hash-secret".into()),
        failed: 5,
        locked_until: 1_900_000_000,
    };
    let debug = format!("{row:?}");
    assert!(!debug.contains("stored-hash-secret"), "{debug}");
    assert!(debug.contains("redacted"), "{debug}");
}

#[tokio::test]
async fn gm_command_clears_the_second_password() {
    use crate::config::AppConfig;
    use crate::systems::services::commands::scripts::harness;

    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    if !app.join("Config.json").exists() {
        return;
    }
    let args: Vec<String> = [
        "--LoginNetwork:Port=1234",
        "/Connections:MySQLProvider:Port",
        "3306",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let config = AppConfig::load_from(&app, None, Vec::new(), &args).unwrap();

    let store = Arc::new(MemoryStore::default());
    store.set_row(
        1001,
        AccountSecondPassword {
            hash: Some("stored-hash".into()),
            failed: 5,
            locked_until: 1_900_000_000,
        },
    );
    let server = harness::server();
    server
        .systems
        .services
        .second_password
        .store
        .install(store.clone());
    server
        .systems
        .services
        .commands
        .access_levels
        .load(&config.access_level);
    let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);

    let (handled, out) = harness::run_at(&server, &gm.p, "secondpassword 1001", 50).await;
    assert!(handled);
    assert_eq!(out, ["|cFFFF0000Insufficient privileges.|r"]);
    assert_eq!(store.row(1001).hash.as_deref(), Some("stored-hash"));

    let (handled, out) = harness::run_at(&server, &gm.p, "secondpassword 1001", 100).await;
    assert!(handled);
    assert_eq!(
        out,
        [harness::normal(
            "secondpassword",
            "Second password of account 1001 cleared"
        )]
    );
    assert_eq!(store.row(1001), AccountSecondPassword::default());
}

#[tokio::test]
async fn enter_world_login_resets_the_second_password_session() {
    let server = test_server();
    let mut client = TestClient::new(&server, 5, 0);
    client.conn.second_password.passed = true;
    client.conn.second_password.has_password = true;
    client.conn.second_password.loaded = true;
    client.conn.second_password.fail_closed = true;
    server.enter_world.add_account(42, 5, false);
    crate::packets::c2g::enter_world_login(&server, &mut client.conn, 42, 5)
        .await
        .expect("enter world");
    assert_eq!(client.conn.account_id(), 42);
    assert!(!client.conn.second_password.passed);
    assert!(!client.conn.second_password.has_password);
    assert!(!client.conn.second_password.loaded);
    assert!(!client.conn.second_password.fail_closed);
}

#[tokio::test]
async fn protocol_errors_report_the_stored_failed_count() {
    let store = Arc::new(MemoryStore::default());
    let hasher = Arc::new(CountingHasher::new());
    let hash = hasher.hash("1234abcd").expect("hash");
    store.set_row(
        42,
        AccountSecondPassword {
            hash: Some(hash),
            failed: 3,
            locked_until: 0,
        },
    );
    let server = server_with(store.clone(), hasher.clone());
    let mut client = TestClient::new(&server, 1, 42);

    let tables = request_tables(&server, &mut client, 4).await;
    let mut wrong = tables.clone();
    wrong.key_id = wrong.key_id.wrapping_add(1);
    send_check(&server, &mut client, &wrong, 0, "1234abcd").await;
    assert_eq!(checked(&client.drain()), Some((false, 3)));

    let tables = request_tables(&server, &mut client, 4).await;
    let mut body = PacketStream::new();
    body.write_u32(tables.key_id).write_u8(0).write_str("!!");
    let packet: CSCheckSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(&server, &mut client.conn)
        .await
        .expect("check");
    assert_eq!(checked(&client.drain()), Some((false, 3)));

    let tables = request_tables(&server, &mut client, 2).await;
    let mut wrong = tables.clone();
    wrong.key_id = wrong.key_id.wrapping_add(1);
    send_change(&server, &mut client, &wrong, 0, "1234abcd", 1, "5678wxyz").await;
    assert_eq!(changed(&client.drain()), Some((changed_result::FAILED, 3)));

    let tables = request_tables(&server, &mut client, 2).await;
    let mut body = PacketStream::new();
    body.write_u32(tables.key_id)
        .write_u8(0)
        .write_u8(1)
        .write_str("!!")
        .write_str("!!");
    let packet: CSChangeSecondPasswordPacket = read_packet(body.into_bytes());
    packet
        .handle(&server, &mut client.conn)
        .await
        .expect("change");
    assert_eq!(
        changed(&client.drain()),
        Some((changed_result::OLD_PASSWORD_FAILED, 3))
    );

    let tables = request_tables(&server, &mut client, 3).await;
    let mut wrong = tables.clone();
    wrong.key_id = wrong.key_id.wrapping_add(1);
    send_clear(&server, &mut client, &wrong, 0, "1234abcd").await;
    assert_eq!(cleared(&client.drain()), Some((false, 3)));

    assert_eq!(store.row(42).failed, 3, "protocol errors do not count");
    assert_eq!(hasher.verifies(), 0, "no hashing on protocol errors");
}
