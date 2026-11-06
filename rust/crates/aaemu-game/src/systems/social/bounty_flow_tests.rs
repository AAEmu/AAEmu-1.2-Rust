//! Bounty placement / arrest flow tests: several connections, C->S packets through
//! `handle_frame`, the real wallet and the real `combat::do_die` arrest path.

use aaemu_commons::helpers::convert_long_to_coord;
use aaemu_commons::PacketStream;

use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_data::SystemFaction;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::models::transform::convert_long;
use crate::net::packet::{encode_raw, LEVEL_PLAIN};
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::skills::combat::do_die;
use crate::systems::skills::types::KillReason;
use crate::systems::social::bounty::BountyManager;
use crate::systems::social::config::{CourtRoomConfig, JailConfig, JusticeConfig, SpawnPosition};
use crate::systems::social::crime::buff as crime_buff;
use crate::systems::social::crime::BountyDescription;
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

/// A player entering the world through the real entry path (`world::session::spawn`), whose
/// spawn frames are kept for inspection.
fn enter(server: &GameServer, id: u32, name: &str, x: f32) -> Pc {
    let mut client = TestClient::new(server, id, 1000 + id);
    let mut c = test_character(id, name, x, 100.0);
    c.obj_id = 0x100 + id;
    c.is_online = true;
    let p = crate::world::Player::new(c, client.conn.handle());
    client.conn.active_char = Some(p.clone());
    server.names.add_character(id, name, 1000 + id);
    crate::world::session::spawn(server, &p);
    Pc { client, p }
}

/// Drops the frames produced by spawning (players seeing each other).
fn settle(pcs: &mut [&mut Pc]) {
    for p in pcs.iter_mut() {
        p.client.drain();
    }
}

/// A player whose zone load (`CSInstanceLoaded`) files it into the world; its frames are kept.
fn zone_load(server: &GameServer, id: u32, name: &str, x: f32) -> Pc {
    let mut client = TestClient::new(server, id, 1000 + id);
    let mut c = test_character(id, name, x, 100.0);
    c.obj_id = 0x100 + id;
    c.is_online = true;
    let p = crate::world::Player::new(c, client.conn.handle());
    client.conn.active_char = Some(p.clone());
    server.names.add_character(id, name, 1000 + id);
    crate::systems::instances::worlds::instance_loaded(server, &p);
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

fn bounty_money(body: &[u8]) -> (u32, i64) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (s.read_bc().unwrap(), s.read_i64().unwrap())
}

fn paid(body: &[u8]) -> (String, String, i64) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_string().unwrap(),
        s.read_string().unwrap(),
        s.read_i64().unwrap(),
    )
}

/// One `SCBountyList` row as the 1.2 client reads it (`FUN_397bebf0`: id, name, level, zone,
/// fixed-point x / y, float z, bounty, crime points, live flag, race, faction id, arrest / trial
/// counters, seconds; notGuilty before guilty).
fn read_board_row(s: &mut PacketStream) -> BountyDescription {
    BountyDescription {
        player_id: s.read_u32().unwrap(),
        player_name: s.read_string().unwrap(),
        player_level: s.read_u8().unwrap(),
        zone_id: s.read_u32().unwrap(),
        pos: (
            convert_long_to_coord(s.read_i64().unwrap()),
            convert_long_to_coord(s.read_i64().unwrap()),
            s.read_f32().unwrap(),
        ),
        bounty_amount: s.read_u64().unwrap(),
        crime_points: s.read_i16().unwrap(),
        is_live: s.read_bool().unwrap(),
        player_race: s.read_u8().unwrap(),
        faction_id: s.read_u32().unwrap(),
        arrest_count: s.read_i32().unwrap(),
        accept_guilty_count: s.read_i32().unwrap(),
        accept_trial_count: s.read_i32().unwrap(),
        not_guilty_count: s.read_i32().unwrap(),
        guilty_count: s.read_i32().unwrap(),
        seconds: s.read_i32().unwrap(),
    }
}

/// The bytes of one row for the synthetic test characters (`test_character`: level 10, race 1,
/// zone [`TEST_ZONE`], no crime points or trial counters).
fn board_row_bytes(id: u32, name: &str, x: f32, y: f32, z: f32, bounty: u64) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(id)
        .write_str(name)
        .write_u8(10)
        .write_u32(TEST_ZONE)
        .write_i64(convert_long(x))
        .write_i64(convert_long(y))
        .write_f32(z)
        .write_u64(bounty)
        .write_i16(0)
        .write_bool(true)
        .write_u8(1)
        .write_u32(101) // every `test_character` is faction 101 (Nuia)
        .write_i32(0)
        .write_i32(0)
        .write_i32(0)
        .write_i32(0)
        .write_i32(0)
        .write_i32(0);
    s.into_bytes()
}

fn board_entries(body: &[u8]) -> (i32, u8, Vec<BountyDescription>) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let total = s.read_i32().unwrap();
    let count = s.read_u8().unwrap();
    let rows = (0..count).map(|_| read_board_row(&mut s)).collect();
    (total, count, rows)
}

async fn open_board(server: &GameServer, pc: &mut Pc) -> Vec<u8> {
    send(server, pc, cs::CSNaviOpenBountyPacket, |s| {
        s.write_bc(0x123);
    })
    .await;
    let frames = pc.client.drain();
    let lists = find(&frames, sc::SCBountyListPacket);
    assert_eq!(lists.len(), 1, "one SCBountyList answer");
    lists.into_iter().next().unwrap()
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
    // The payer is a viewer of the target: the whole new total goes out (BOUNTY-3).
    let moneys: Vec<(u32, i64)> = find(&frames, sc::SCUnitBountyMoneyPacket)
        .iter()
        .map(|b| bounty_money(b))
        .collect();
    assert_eq!(moneys, vec![(b.p.obj_id, 250)]);

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
async fn a_pledge_broadcasts_the_whole_new_total_to_every_viewer() {
    let server = bounty_server();
    let mut a = player(&server, 1, "PayerA", false, 1000);
    let mut b = player(&server, 2, "Wanted", true, 0);
    let mut c = player(&server, 3, "PayerC", false, 1000);
    settle(&mut [&mut a, &mut b, &mut c]);

    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    let frames = a.client.drain();
    assert_eq!(
        find(&frames, sc::SCUnitBountyMoneyPacket)
            .iter()
            .map(|body| bounty_money(body))
            .collect::<Vec<_>>(),
        vec![(b.p.obj_id, 100)]
    );
    a.client.drain();
    b.client.drain();
    c.client.drain();

    // A second pledge sends the whole stacked total (150), never the 50 delta.
    place(&server, &mut c, b.p.character_id, "Wanted", 50).await;
    let frames = a.client.drain();
    assert_eq!(
        find(&frames, sc::SCUnitBountyMoneyPacket)
            .iter()
            .map(|body| bounty_money(body))
            .collect::<Vec<_>>(),
        vec![(b.p.obj_id, 150)]
    );
    let frames = c.client.drain();
    assert_eq!(
        find(&frames, sc::SCUnitBountyMoneyPacket)
            .iter()
            .map(|body| bounty_money(body))
            .collect::<Vec<_>>(),
        vec![(b.p.obj_id, 150)]
    );
    // The target itself is a viewer too.
    let frames = b.client.drain();
    assert_eq!(
        find(&frames, sc::SCUnitBountyMoneyPacket)
            .iter()
            .map(|body| bounty_money(body))
            .collect::<Vec<_>>(),
        vec![(b.p.obj_id, 150)]
    );
}

#[tokio::test]
async fn an_entering_viewer_receives_a_wanted_targets_total_once() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let mut b = player(&server, 2, "Wanted", true, 0);
    settle(&mut [&mut a, &mut b]);
    place(&server, &mut a, b.p.character_id, "Wanted", 175).await;
    a.client.drain();

    // A viewer entering the world later (after the pledge) gets the target's total once.
    let mut c = enter(&server, 3, "Viewer", 103.0);
    let frames = c.client.drain();
    assert_eq!(
        find(&frames, sc::SCUnitBountyMoneyPacket)
            .iter()
            .map(|body| bounty_money(body))
            .collect::<Vec<_>>(),
        vec![(b.p.obj_id, 175)]
    );
    let _ = b.client.drain();
}

#[tokio::test]
async fn a_zone_load_viewer_receives_a_wanted_targets_total_once() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let mut b = player(&server, 2, "Wanted", true, 0);
    settle(&mut [&mut a, &mut b]);
    place(&server, &mut a, b.p.character_id, "Wanted", 175).await;
    a.client.drain();

    // The second entry path (`CSInstanceLoaded`) sends the same frames after the exchange.
    let mut c = zone_load(&server, 3, "Viewer", 103.0);
    let frames = c.client.drain();
    assert_eq!(
        find(&frames, sc::SCUnitBountyMoneyPacket)
            .iter()
            .map(|body| bounty_money(body))
            .collect::<Vec<_>>(),
        vec![(b.p.obj_id, 175)]
    );
    let _ = b.client.drain();
}

#[tokio::test]
async fn an_entering_viewer_gets_no_bounty_frame_when_no_total_is_set() {
    let server = bounty_server();
    let mut a = player(&server, 1, "PlainA", false, 0);
    settle(&mut [&mut a]);

    let mut b = enter(&server, 2, "PlainB", 103.0);
    let frames = b.client.drain();
    assert!(find(&frames, sc::SCUnitBountyMoneyPacket).is_empty());
}

#[tokio::test]
async fn a_payout_broadcasts_zero_to_the_viewers() {
    let server = bounty_server();
    let mut a = player(&server, 1, "Payer", false, 1000);
    let mut b = player(&server, 2, "Wanted", true, 0);
    let mut d = player(&server, 4, "Arrestor", false, 0);
    settle(&mut [&mut a, &mut b, &mut d]);

    place(&server, &mut a, b.p.character_id, "Wanted", 100).await;
    a.client.drain();
    b.client.drain();
    d.client.drain();

    do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&d.p)),
        KillReason::Damage,
    );

    for viewer in [&mut a, &mut d] {
        let frames = viewer.client.drain();
        assert_eq!(
            find(&frames, sc::SCUnitBountyMoneyPacket)
                .iter()
                .map(|body| bounty_money(body))
                .collect::<Vec<_>>(),
            vec![(b.p.obj_id, 0)]
        );
    }
    assert!(find(&b.client.drain(), sc::SCUnitBountyMoneyPacket)
        .iter()
        .map(|body| bounty_money(body))
        .collect::<Vec<_>>()
        .iter()
        .all(|&(id, total)| id == b.p.obj_id && total == 0));
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
async fn bounty_board_flag_off_answers_todays_empty_list() {
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

#[tokio::test]
async fn bounty_board_flag_on_with_no_pledges_is_the_empty_list() {
    let server = bounty_server();
    server.config.update(|c| c.social.bounty_board = true);
    let mut a = player(&server, 1, "Viewer", false, 1000);
    a.client.drain();

    assert_eq!(open_board(&server, &mut a).await, vec![0, 0, 0, 0, 0]);
}

#[tokio::test]
async fn bounty_board_writes_two_rows_highest_total_first() {
    let server = bounty_server();
    server.config.update(|c| c.social.bounty_board = true);
    let mut a = player(&server, 1, "Payer", false, 1_000_000);
    player(&server, 2, "Bravo", true, 0);
    player(&server, 3, "Alpha", true, 0);
    place(&server, &mut a, 2, "Bravo", 500).await;
    place(&server, &mut a, 3, "Alpha", 900).await;
    a.client.drain();

    let body = open_board(&server, &mut a).await;
    let mut expected = 2i32.to_le_bytes().to_vec();
    expected.push(2);
    expected.extend(board_row_bytes(3, "Alpha", 103.0, 100.0, 100.0, 900));
    expected.extend(board_row_bytes(2, "Bravo", 102.0, 100.0, 100.0, 500));
    assert_eq!(body, expected);
    // The +0xb8 field is the live character's faction id, not the C# dummy 1.
    let (_, _, rows) = board_entries(&body);
    assert_eq!(
        rows.iter().map(|row| row.faction_id).collect::<Vec<_>>(),
        vec![101, 101]
    );
}

#[tokio::test]
async fn bounty_board_caps_rows_at_twenty_and_reports_the_full_total() {
    let server = bounty_server();
    server.config.update(|c| c.social.bounty_board = true);
    let mut payer = player(&server, 1, "Payer", false, 1_000_000);
    let mut targets = Vec::new();
    for id in 2..=22u32 {
        let name = format!("Wanted{id}");
        player(&server, id, &name, true, 0);
        targets.push((id, name));
    }
    for (id, name) in &targets {
        place(&server, &mut payer, *id, name, 100 + *id as i32).await;
    }
    payer.client.drain();

    let body = open_board(&server, &mut payer).await;
    let (total, count, rows) = board_entries(&body);
    assert_eq!(total, 21, "total is every character with a pledge");
    assert_eq!(count, 20, "the client cap");
    assert_eq!(rows.len(), 20);
    let ids: Vec<u32> = rows.iter().map(|row| row.player_id).collect();
    assert_eq!(ids, (3..=22u32).rev().collect::<Vec<_>>());
    assert_eq!(rows[0].bounty_amount, 122);
    assert_eq!(rows[19].bounty_amount, 103);
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
