//! Bounty placement / arrest flow tests: several connections, C->S packets through
//! `handle_frame`, the real wallet and the real `combat::do_die` arrest path.

use aaemu_commons::PacketStream;

use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_data::SystemFaction;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::net::packet::{encode_raw, LEVEL_PLAIN};
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::skills::combat::do_die;
use crate::systems::skills::types::KillReason;
use crate::systems::social::bounty::BountyManager;
use crate::systems::social::config::{CourtRoomConfig, JailConfig, JusticeConfig, SpawnPosition};
use crate::systems::social::crime::buff as crime_buff;
use crate::systems::social::util;
use crate::test_support::*;
use crate::world::{Entity, PlayerRef, WorldObject};

const NUIA: u32 = 148;

fn spawn_pos(x: f32, y: f32, z: f32) -> SpawnPosition {
    SpawnPosition {
        world_id: 0,
        zone_id: TEST_ZONE,
        x,
        y,
        z,
        yaw: 0.0,
    }
}

/// A test server with the wanted buff and its 894 tag, which `combat` and the bounty placement
/// both test with `check_buff_tag`. With `justice`, a Nuian court room and jail exist so
/// `arrest_criminal` really starts a case; without them it answers `None`.
fn bounty_server_with_justice(justice: bool) -> GameServer {
    let mut data = GameData::empty();
    let mut template = BuffTemplate::simple(crime_buff::WANTED, BuffKind::Bad);
    template.duration = 3_600_000;
    data.skills.buffs.insert(crime_buff::WANTED, template);
    data.systems
        .skills
        .buff_tags
        .insert(crime_buff::WANTED, vec![crime_buff::TAG_WANTED]);
    data.systems
        .skills
        .tagged_buffs
        .insert(crime_buff::TAG_WANTED, vec![crime_buff::WANTED]);
    for (id, mother) in [(101, NUIA), (NUIA, 0)] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..SystemFaction::default()
        });
    }
    let server = test_server_with(data);
    if justice {
        server.config.update(|c| {
            c.justice = JusticeConfig {
                court_rooms: vec![CourtRoomConfig {
                    id: 1,
                    name: "Nuian Main Court".into(),
                    faction: NUIA,
                    defendant: spawn_pos(150.0, 160.0, 70.0),
                    holding_cell: spawn_pos(200.0, 210.0, 60.0),
                }],
                jails: vec![JailConfig {
                    id: 1,
                    name: "Nuian Jail".into(),
                    faction: NUIA,
                    pos: spawn_pos(300.0, 310.0, 50.0),
                }],
                ..JusticeConfig::default()
            };
        });
        server.systems.social.trials.load(&server);
    }
    server
}

fn bounty_server() -> GameServer {
    bounty_server_with_justice(true)
}

struct Pc {
    client: TestClient,
    p: PlayerRef,
}

fn player(server: &GameServer, id: u32, name: &str, wanted: bool, money: i64) -> Pc {
    let mut client = TestClient::new(server, id, 1000 + id);
    let mut c = test_character(id, name, 100.0 + id as f32, 100.0);
    c.is_online = true;
    c.money = money;
    if wanted {
        crate::systems::social::crime::add_own_buff(
            server,
            &mut c,
            crime_buff::WANTED,
            3_600_000,
            util::now_ms(),
        );
    }
    let p = spawn_player(server, &client, c, 0x100 + id);
    client.conn.active_char = Some(p.clone());
    server.names.add_character(id, name, 1000 + id);
    client.drain();
    Pc { client, p }
}

async fn send(server: &GameServer, pc: &mut Pc, opcode: u16, f: impl FnOnce(&mut PacketStream)) {
    let mut s = PacketStream::new();
    f(&mut s);
    server
        .handle_frame(
            &mut pc.client.conn,
            &encode_raw(LEVEL_PLAIN, opcode, &s.into_bytes()),
        )
        .await
        .expect("handled");
}

fn find(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|(o, _)| *o == op)
        .map(|(_, b)| b.clone())
        .collect()
}

fn permitted(body: &[u8]) -> (u32, String, i64, bool) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_u32().unwrap(),
        s.read_string().unwrap(),
        s.read_i64().unwrap(),
        s.read_bool().unwrap(),
    )
}

fn done(body: &[u8]) -> (u32, String, i64, i16) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_u32().unwrap(),
        s.read_string().unwrap(),
        s.read_i64().unwrap(),
        s.read_i16().unwrap(),
    )
}

fn paid(body: &[u8]) -> (String, String, i64) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_string().unwrap(),
        s.read_string().unwrap(),
        s.read_i64().unwrap(),
    )
}

/// `(task type, money change)` of the single money task in an `SCItemTaskSuccess` body.
fn money_task(body: &[u8]) -> (u8, i32) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let action = s.read_u8().unwrap();
    assert_eq!(s.read_u8().unwrap(), 1, "one task");
    assert_eq!(s.read_u8().unwrap(), 1, "MoneyChange");
    (action, s.read_i32().unwrap())
}

async fn request(server: &GameServer, pc: &mut Pc, name: &str) {
    send(server, pc, cs::CSRequestSetBountyMoneyPacket, |s| {
        s.write_str(name);
    })
    .await;
}

async fn update(server: &GameServer, pc: &mut Pc, target_id: u32, name: &str, money: i32) {
    send(server, pc, cs::CSSendBountyUpdatePacket, |s| {
        s.write_u32(target_id).write_str(name).write_i32(money);
    })
    .await;
}

async fn place(server: &GameServer, pc: &mut Pc, target_id: u32, name: &str, money: i32) {
    request(server, pc, name).await;
    pc.client.drain();
    update(server, pc, target_id, name, money).await;
}

#[tokio::test]
async fn a_request_on_a_not_wanted_target_is_refused_with_permitted_false() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let b = player(&server, 2, "Plain", false, 0);

    request(&server, &mut a, "Plain").await;
    let frames = a.client.drain();
    let permitted_frames = find(&frames, sc::SCSetBountyPermittedPacket);
    assert_eq!(permitted_frames.len(), 1);
    assert_eq!(
        permitted(&permitted_frames[0]),
        (b.p.character_id, "Plain".into(), 0, false)
    );
    assert!(find(&frames, sc::SCSetBountyDonePacket).is_empty());
}

#[tokio::test]
async fn a_request_on_a_wanted_target_reports_the_current_total() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);
    let mut c = player(&server, 3, "Second", false, 1000);

    request(&server, &mut a, "Wanted").await;
    let frames = a.client.drain();
    assert_eq!(
        permitted(&find(&frames, sc::SCSetBountyPermittedPacket)[0]),
        (b.p.character_id, "Wanted".into(), 0, true)
    );

    place(&server, &mut c, b.p.character_id, "Wanted", 100).await;
    c.client.drain();

    request(&server, &mut a, "Wanted").await;
    let frames = a.client.drain();
    assert_eq!(
        permitted(&find(&frames, sc::SCSetBountyPermittedPacket)[0]),
        (b.p.character_id, "Wanted".into(), 100, true)
    );
}

#[tokio::test]
async fn a_placement_debits_the_purse_and_answers_the_new_total() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);

    place(&server, &mut a, b.p.character_id, "Wanted", 250).await;
    let frames = a.client.drain();
    let done_frames = find(&frames, sc::SCSetBountyDonePacket);
    assert_eq!(
        done(&done_frames[0]),
        (b.p.character_id, "Wanted".into(), 250, 0)
    );
    assert_eq!(a.p.data().money, 750);
    let tasks: Vec<(u8, i32)> = find(&frames, sc::SCItemTaskSuccessPacket)
        .iter()
        .map(|b| money_task(b))
        .collect();
    assert_eq!(tasks, vec![(87, -250)]);
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        250
    );
    // The `SCUnitBountyMoney` writer has not been located in the 1.2 client (BOUNTY-3).
    assert!(find(&frames, sc::SCUnitBountyMoneyPacket).is_empty());

    // A second update without a new request is refused and moves nothing.
    update(&server, &mut a, b.p.character_id, "Wanted", 50).await;
    let frames = a.client.drain();
    assert_eq!(
        done(&find(&frames, sc::SCSetBountyDonePacket)[0]),
        (b.p.character_id, "Wanted".into(), 250, 586)
    );
    assert_eq!(a.p.data().money, 750);
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        250
    );
}

#[tokio::test]
async fn a_placement_beyond_the_purse_is_refused_without_a_pledge() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 100);
    let b = player(&server, 2, "Wanted", true, 0);

    request(&server, &mut a, "Wanted").await;
    a.client.drain();
    for amount in [200, 0, -10] {
        update(&server, &mut a, b.p.character_id, "Wanted", amount).await;
        let frames = a.client.drain();
        assert_eq!(
            done(&find(&frames, sc::SCSetBountyDonePacket)[0]),
            (b.p.character_id, "Wanted".into(), 0, 586)
        );
        assert!(find(&frames, sc::SCItemTaskSuccessPacket).is_empty());
        assert_eq!(a.p.data().money, 100);
    }
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        0
    );
}

#[tokio::test]
async fn several_payers_stack_into_one_total() {
    let server = bounty_server();
    let mut a = player(&server, 1, "PayerA", false, 1000);
    let mut c = player(&server, 3, "PayerC", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);

    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    assert_eq!(
        done(&find(&a.client.drain(), sc::SCSetBountyDonePacket)[0]).2,
        100
    );
    place(&server, &mut c, b.p.character_id, "Wanted", 250).await;
    assert_eq!(
        done(&find(&c.client.drain(), sc::SCSetBountyDonePacket)[0]).2,
        350
    );
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        350
    );
    assert_eq!(a.p.data().money, 900);
    assert_eq!(c.p.data().money, 750);
}

#[tokio::test]
async fn arrest_pays_the_whole_total_once_and_resets_it() {
    let server = bounty_server();
    let mut a = player(&server, 1, "PayerA", false, 1000);
    let mut c = player(&server, 3, "PayerC", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);
    let mut d = player(&server, 4, "Arrestor", false, 0);

    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    a.client.drain();
    place(&server, &mut c, b.p.character_id, "Wanted", 250).await;
    c.client.drain();
    d.client.drain();

    do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&d.p)),
        KillReason::Damage,
    );

    let frames = d.client.drain();
    assert_eq!(
        paid(&find(&frames, sc::SCBountyPaidPacket)[0]),
        ("Arrestor".into(), "Wanted".into(), 350)
    );
    // The court room and jail made `arrest_criminal` start a real case.
    assert_eq!(b.p.data().arrest_count, 1);
    let tasks: Vec<(u8, i32)> = find(&frames, sc::SCItemTaskSuccessPacket)
        .iter()
        .map(|b| money_task(b))
        .collect();
    assert_eq!(tasks, vec![(88, 350)]);
    assert_eq!(d.p.data().money, 350);
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        0
    );

    // A repeated settlement (the replay of a death) pays nothing.
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .settle_arrest(&server, &b.p, &d.p),
        0
    );
    assert_eq!(d.p.data().money, 350);
}

#[tokio::test]
async fn a_kill_without_a_court_case_pays_nothing_and_keeps_the_pledge() {
    let server = bounty_server_with_justice(false);
    let mut a = player(&server, 1, "Payer", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);
    let mut d = player(&server, 4, "Killer", false, 0);

    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    a.client.drain();
    d.client.drain();

    do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&d.p)),
        KillReason::Damage,
    );

    let frames = d.client.drain();
    assert!(find(&frames, sc::SCBountyPaidPacket).is_empty());
    assert_eq!(d.p.data().money, 0);
    assert_eq!(b.p.data().arrest_count, 0);
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        100
    );
}

#[tokio::test]
async fn npc_kills_and_non_wanted_victims_never_pay() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);
    let mut d = player(&server, 4, "Killer", false, 0);

    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    a.client.drain();
    d.client.drain();

    // An NPC killer is not an arrestor.
    let npc = Entity::new(0x500, Npc::new(1));
    do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&npc)),
        KillReason::Damage,
    );
    assert_eq!(d.p.data().money, 0);
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        100
    );

    // The wanted state lapses: the stored pledge stays, but the next player kill is not an
    // arrest and pays nothing.
    let mut a2 = player(&server, 6, "Payer2", false, 1000);
    let b3 = player(&server, 7, "Wanted3", true, 0);
    place(&server, &mut a2, b3.p.character_id, "Wanted3", 40).await;
    a2.client.drain();
    crate::systems::skills::buffs::remove_buff(
        &server,
        &WorldObject::from(&b3.p),
        crime_buff::WANTED,
    );
    d.client.drain();
    do_die(
        &server,
        &WorldObject::from(&b3.p),
        Some(&WorldObject::from(&d.p)),
        KillReason::Damage,
    );
    assert_eq!(d.p.data().money, 0);
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b3.p.character_id),
        40
    );

    // A self settlement never pays.
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .settle_arrest(&server, &b3.p, &b3.p),
        0
    );
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b3.p.character_id),
        40
    );
}

#[tokio::test]
async fn the_board_still_answers_an_empty_list() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);
    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    a.client.drain();

    send(&server, &mut a, cs::CSNaviOpenBountyPacket, |s| {
        s.write_bc(0x123);
    })
    .await;
    let frames = a.client.drain();
    let lists = find(&frames, sc::SCBountyListPacket);
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0], vec![0, 0, 0, 0, 0]);
}

/// The pledges a live server wrote survive a restart: `save` + `load` on a temporary table.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only a temporary table"]
async fn bounty_pledges_round_trip_through_mysql() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    let ddl = include_str!("../../../../../../SQL/updates/2026-11-02_aaemu_game_bounty.sql")
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("CREATE TABLE IF NOT EXISTS", "CREATE TEMPORARY TABLE");
    for statement in ddl.split(';').filter(|s| !s.trim().is_empty()) {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }

    let server = bounty_server();
    let mut a = player(&server, 1, "PayerA", false, 1000);
    let mut c = player(&server, 3, "PayerC", false, 1000);
    let b = player(&server, 2, "Wanted", true, 0);
    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    place(&server, &mut c, b.p.character_id, "Wanted", 250).await;
    assert_eq!(
        server
            .systems
            .social
            .bounties
            .active_total(b.p.character_id),
        350
    );

    server.systems.social.bounties.prepare_save();
    let mut tx = pool.begin().await.unwrap();
    let written = server.systems.social.bounties.save(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    server.systems.social.bounties.commit_save();
    assert_eq!(written, 2);

    let restored = BountyManager::default();
    restored.load(&pool).await.unwrap();
    assert_eq!(restored.active_total(b.p.character_id), 350);
    assert_eq!(restored.active_total(a.p.character_id), 0);
}
