//! End-to-end 1.2 lobby flow through the login link handler and `GameServer::handle_frame`,
//! without MySQL (the pool points at a closed port; DB-backed steps log errors exactly like the
//! C# catch blocks, or fail the packet like an uncaught C# exception). Every S->C frame is
//! compared byte for byte.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::{Marshal, PacketStream};
use sqlx::mysql::MySqlPoolOptions;
use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::data::{FeatureSet, GameData};
use crate::models::character::Character;
use crate::models::item::{Item, ItemRow};
use crate::net::connection::{ConnCommand, GameConnection};
use crate::net::packet::{PacketDef, LEVEL_PLAIN};
use crate::opcodes::{cs, pp};
use crate::packets::c2g::send_character_list;
use crate::GameServer;

fn test_server() -> GameServer {
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.stream_network.port = 1250;
    config.network.num_connections = 10;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1; // nothing listens here
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    GameServer::new(config, db, GameData::empty())
}

struct Client {
    conn: GameConnection,
    out: crate::net::outbox::OutboxReceiver,
    cmd: mpsc::UnboundedReceiver<ConnCommand>,
}

/// A connection registered in the server's connection table (like `GameProtocolHandler.OnConnect`).
fn client(server: &GameServer, id: u32) -> Client {
    let (out_tx, out) = crate::net::outbox::outbox();
    let (cmd_tx, cmd) = mpsc::unbounded_channel();
    let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let (mut conn, _) = GameConnection::new(id, peer, out_tx, cmd_tx);
    conn.set_reply_delay(server.reply_delay.clone());
    server.connections.add(conn.handle());
    Client { conn, out, cmd }
}

impl Client {
    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        while let Ok(f) = self.out.try_recv() {
            v.push(f.to_vec());
        }
        v
    }

    /// Runs queued `ConnCommand`s the way the connection task does.
    fn pump_commands(&mut self) {
        while let Ok(cmd) = self.cmd.try_recv() {
            match cmd {
                ConnCommand::Send(p) => self.conn.send(p.as_ref()),
                ConnCommand::Close => self.conn.shutdown(),
                ConnCommand::DetachAndClose => {
                    self.conn.active_char = None;
                    self.conn.shutdown();
                }
                ConnCommand::LeaveWorld(_) | ConnCommand::ReturnToWorldList => {}
            }
        }
    }
}

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

/// Expected S->C level-1 frame payload: `[dd 01 00 00][opcode][body]`.
fn l1(opcode: u16, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0xDD, 0x01, 0x00, 0x00];
    v.extend_from_slice(&opcode.to_le_bytes());
    v.extend_from_slice(body);
    v
}

/// Expected S->C level-2 frame payload: `[dd 02][opcode][body]`.
fn l2(opcode: u16, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0xDD, 0x02];
    v.extend_from_slice(&opcode.to_le_bytes());
    v.extend_from_slice(body);
    v
}

/// C->S level-1 frame with a client hash/count (ignored by the server).
fn c2s_l1(opcode: u16, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0xDD, 0x01, 0x5A, 0x03];
    v.extend_from_slice(&opcode.to_le_bytes());
    v.extend_from_slice(body);
    v
}

fn bytes(f: impl FnOnce(&mut PacketStream)) -> Vec<u8> {
    let mut s = PacketStream::new();
    f(&mut s);
    s.into_bytes()
}

fn enter_world_body(account_id: u32, cookie: u32) -> Vec<u8> {
    bytes(|s| {
        s.write_u32(0) // pFrom
            .write_u32(0) // pTo
            .write_u32(account_id)
            .write_u32(cookie)
            .write_i32(0) // zoneId
            .write_u8(0) // tb
            .write_u64(0); // revision
    })
}

/// The world cookie of a `GLPlayerEnterPacket` the game sent to the login server (header 2 bytes,
/// then connection id, gs id, result, online, sequence, cookie).
fn enter_reply_cookie(reply: &[u8]) -> u32 {
    u32::from_le_bytes(reply[16..20].try_into().unwrap())
}

/// SCAccountInfoPacket body for the default `AccountPayment` (Premium, 1, MinValue, no end date;
/// owner decision 2026-11-03: the configured default premium is permanent).
fn account_info_frame() -> Vec<u8> {
    l1(
        0x1C2,
        &hex("01 00 00 00  01 00 00 00  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00"),
    )
}

/// WP-J: the configured restriction flags (selected by the effective payment method) and the
/// idle kick time reach `SCInitialConfig` through the real `FinishState(0)` handler. The reader
/// (`FUN_397b2a00`) takes the seven booleans, then `secondPriceType`, `secondPasswordMaxFailCount`
/// and `idleKickTime` (`u16`, `FUN_39017bf0` masks with `0xffff`).
#[tokio::test]
async fn initial_config_uses_configured_restrictions_and_idle_kick_time() {
    use crate::config::{DefaultPayment, RestrictionFlags};

    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.stream_network.port = 1250;
    config.network.num_connections = 10;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1; // nothing listens here
    config.account.default_payment = DefaultPayment::Demo;
    config.account.restrictions.demo = RestrictionFlags {
        can_place_house: false,
        can_use_auction: false,
        can_trade: false,
        can_use_copper: false,
        ..RestrictionFlags::default()
    };
    config.account.idle_kick_time = 0x1234;
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, db, GameData::empty()));

    let cookie = server
        .enter_world
        .add_account(1000, 4242, false)
        .cookie()
        .unwrap();
    let mut c = client(&server, 77);
    server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(1000, cookie)),
        )
        .await
        .unwrap();
    c.drain();

    // ---- FinishState(0): the lobby configuration burst
    server
        .handle_frame(
            &mut c.conn,
            &[0xDD, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00],
        )
        .await
        .unwrap();
    let frames = c.drain();
    let init = frames
        .iter()
        .find(|f| f.len() > 6 && f[..6] == [0xDD, 0x01, 0x00, 0x00, 0x06, 0x00])
        .expect("SCInitialConfig");
    assert_eq!(
        &init[init.len() - 11..],
        &[0, 1, 0, 0, 1, 1, 0, 2, 0, 0x34, 0x12]
    );
}

#[tokio::test]
async fn lobby_flow_end_to_end() {
    let server = Arc::new(test_server());
    let (login_tx, mut login_rx) = mpsc::unbounded_channel();
    server.login.set(Some(login_tx));

    // ---- L2G: LGPlayerEnter(accountId 1000, connectionId 4242) -> GLPlayerEnter(4242, gs 1, 0,
    // online 1: the pending cookie occupies one slot; the two following u32 are the Rust queue
    // occupancy and report sequence, the last one the random world cookie).
    assert!(crate::login::handle(&server, &hex("01 00  E8 03 00 00  92 10 00 00")).await);
    let reply = login_rx.try_recv().unwrap();
    assert_eq!(
        reply[..16],
        hex("01 00  92 10 00 00  01  00  01 00 00 00  01 00 00 00")[..]
    );
    let first_cookie = enter_reply_cookie(&reply);
    assert_ne!(first_cookie, 0);
    assert_ne!(
        first_cookie, 4242,
        "the cookie is not the login connection id"
    );
    // A second LGPlayerEnter for the same connection id throws in C# (no reply). Here the stale
    // entry (a cookie that was never used) is replaced and answered.
    assert!(crate::login::handle(&server, &hex("01 00  E9 03 00 00  92 10 00 00")).await);
    let reply = login_rx.try_recv().unwrap();
    assert_eq!(
        reply[..16],
        hex("01 00  92 10 00 00  01  00  01 00 00 00  02 00 00 00")[..]
    );
    let second_cookie = enter_reply_cookie(&reply);
    // ... and back to account 1000 for the rest of the flow.
    assert!(crate::login::handle(&server, &hex("01 00  E8 03 00 00  92 10 00 00")).await);
    let reply = login_rx.try_recv().unwrap();
    assert_eq!(
        reply[..16],
        hex("01 00  92 10 00 00  01  00  01 00 00 00  03 00 00 00")[..]
    );
    let cookie = enter_reply_cookie(&reply);
    assert!(login_rx.try_recv().is_err());
    // The replaced entries' cookies no longer work; the connection id never did.
    for dead in [first_cookie, second_cookie, 4242] {
        let mut stale = client(&server, 76);
        server
            .handle_frame(
                &mut stale.conn,
                &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(1000, dead)),
            )
            .await
            .unwrap();
        assert!(stale.conn.is_closing());
        assert!(stale.drain().is_empty());
    }

    // ---- X2EnterWorld with the cookie -> X2EnterWorldResponse + ChangeState(0)
    let mut c = client(&server, 77);
    server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(1000, cookie)),
        )
        .await
        .unwrap();
    let frames = c.drain();
    assert_eq!(frames.len(), 2);
    let resp = &frames[0];
    // [dd 01 00 00][00 00] reason=0 gm=0 token=<stream ticket> port=1250 wf=<now> pubKeySize=0 pubKey=""
    assert_eq!(resp.len(), 6 + 2 + 1 + 4 + 2 + 8 + 2 + 2);
    assert_eq!(resp[..9], hex("DD 01 00 00  00 00  00 00  00")[..]);
    assert_eq!(resp[13..15], hex("E2 04")[..]);
    // The token is a random nonzero ticket, not the connection id.
    let ticket = u32::from_le_bytes(resp[9..13].try_into().unwrap());
    assert_ne!(ticket, 0);
    let wf = i64::from_le_bytes(resp[15..23].try_into().unwrap());
    assert!((wf - chrono::Utc::now().timestamp()).abs() < 30);
    assert_eq!(resp[23..], hex("00 00  00 00")[..]);
    assert_eq!(frames[1], l2(pp::ChangeStatePacket, &[0, 0, 0, 0]));
    assert_eq!(c.conn.account_id(), 1000);
    assert!(server.accounts.contains(1000));
    assert_eq!(server.streams.account_for(ticket), Some(1000));
    assert_eq!(server.streams.ticket_for(77), Some(ticket));

    // The cookie is consumed: a second connection with it is refused and shut down.
    let mut c2 = client(&server, 78);
    server
        .handle_frame(
            &mut c2.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(1000, cookie)),
        )
        .await
        .unwrap();
    assert!(c2.conn.is_closing());
    assert!(c2.drain().is_empty());

    // ---- FinishState(0): the 1.2 lobby configuration burst
    server
        .handle_frame(
            &mut c.conn,
            &[0xDD, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00],
        )
        .await
        .unwrap();
    let frames = c.drain();
    let mut want = vec![l2(pp::ChangeStatePacket, &[1, 0, 0, 0])];
    let mut sgt = hex("0D 00");
    sgt.extend_from_slice(b"w_hanuimaru_1");
    sgt.extend_from_slice(&[0; 8]);
    sgt.push(1);
    want.push(l2(pp::SetGameTypePacket, &sgt));
    let fset = FeatureSet::initialized(0, 0);
    let mut init = hex("0B 00");
    init.extend_from_slice(b"aaemu.local");
    init.extend_from_slice(&[11, 0]);
    init.extend_from_slice(fset.bytes());
    init.extend_from_slice(&hex(
        "00 00 00 00  32 00 00 00  01 01 01 01 01 01 01  02  00  00 00",
    ));
    want.push(l1(0x006, &init));
    let trion = bytes(|s| {
        s.write_u8(1)
            .write_str("http://localhost/aaemu/login")
            .write_str("http://localhost/aaemu/platform")
            .write_str("http://localhost/aaemu/shop");
    });
    want.push(l1(0x007, &trion));
    want.push(account_info_frame());
    want.push(l1(0x0D3, &[0; 1 + 15 + 60 + 2 + 2]));
    want.push(l1(0x215, &[0, 1]));
    want.push(l1(
        0x217,
        &hex("0A 0A 0A 0A 0A  00 0F 0F 0F 00 00 0F 00 00 00 00 00 00 00 0F"),
    ));
    assert_eq!(frames, want);

    // ---- FinishState 1..6 -> ChangeState(state + 1), 7 -> SCUpdatePremiumPoint(1, 1, 1)
    for state in 1..=6i32 {
        server
            .handle_frame(
                &mut c.conn,
                &l2(pp::FinishStatePacket, &state.to_le_bytes()),
            )
            .await
            .unwrap();
        assert_eq!(
            c.drain(),
            vec![l2(pp::ChangeStatePacket, &(state + 1).to_le_bytes())]
        );
    }
    server
        .handle_frame(&mut c.conn, &l2(pp::FinishStatePacket, &7i32.to_le_bytes()))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![l1(0x201, &hex("01 00 00 00 01 01"))]);

    // ---- CSListCharacter: SCGetSlotCount + SCAccountInfo, then LoadAccount needs MySQL, so the
    // packet fails there (an exception in C#, which shuts the connection down).
    let res = server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::CSListCharacterPacket, &hex("00 00 00 00  00 00")),
        )
        .await;
    assert!(res.is_err());
    assert_eq!(c.drain(), vec![l1(0x1E2, &[0]), account_info_frame()]);

    // ... and what follows LoadAccount for an account with three characters.
    let chars: Vec<Character> = (1..=3).map(sample_character).collect();
    send_character_list(&mut c.conn, &chars);
    let frames = c.drain();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0], l1(0x03F, &[0; 9]));
    let list = |last: bool, cs: &[Character]| {
        l1(
            0x03E,
            &bytes(|s| {
                s.write_bool(last).write_u8(cs.len() as u8);
                for c in cs {
                    s.write_raw(&expected_character_bytes(c));
                }
            }),
        )
    };
    assert_eq!(frames[1], list(false, &chars[..2]));
    assert_eq!(frames[2], list(true, &chars[2..]));

    // ---- CSLeaveWorld(ServerSelect) in the lobby -> GLPlayerReconnect(gs 1, account, token)
    server
        .handle_frame(&mut c.conn, &c2s_l1(cs::CSLeaveWorldPacket, &[2]))
        .await
        .unwrap();
    let reconnect = login_rx.try_recv().unwrap();
    assert_eq!(&reconnect[..7], &hex("02 00  01  E8 03 00 00")[..]);
    assert_eq!(reconnect.len(), 11);
    let token = u32::from_le_bytes(reconnect[7..11].try_into().unwrap());
    // QuitGame / CharacterSelect in the lobby: nothing happens.
    server
        .handle_frame(&mut c.conn, &c2s_l1(cs::CSLeaveWorldPacket, &[1]))
        .await
        .unwrap();
    assert!(login_rx.try_recv().is_err() && c.drain().is_empty());

    // ---- LGPlayerReconnect(token) -> SCReconnectAuth(token)
    let mut ack = vec![0x02, 0x00];
    ack.extend_from_slice(&token.to_le_bytes());
    assert!(crate::login::handle(&server, &ack).await);
    c.pump_commands();
    assert_eq!(c.drain(), vec![l1(0x001, &token.to_le_bytes())]);

    // ---- LGRequestInfo for account 0 -> GLRequestInfo with no characters (no DB access)
    let req = hex("03 00  05 00 00 00  06 00 00 00  00 00 00 00 00 00 00 00");
    assert!(crate::login::handle(&server, &req).await);
    assert_eq!(
        login_rx.try_recv().unwrap(),
        hex("03 00  05 00 00 00  06 00 00 00  00 00 00 00")
    );

    // ---- LGRegisterGameServer: 0 keeps the link, anything else closes it. The Rust-only tail
    // carries the login's pre-select period, which the game applies.
    assert!(crate::login::handle(&server, &[0x00, 0x00, 0x00]).await);
    assert!(!crate::login::handle(&server, &[0x00, 0x00, 0x01]).await);
    assert!(!server.config.characters.pre_select_period);
    assert!(crate::login::handle(&server, &[0x00, 0x00, 0x00, 0x01]).await);
    assert!(
        server.config.characters.pre_select_period,
        "the login's pre-select period is authoritative"
    );
}

/// A character with one equipped armor piece in slot 3 (chest).
fn sample_character(id: u32) -> Character {
    let details = bytes(|s| {
        s.write_u32(0).write_u8(100).write_i16(0).write_u32(0);
        s.write_i64(0).write_u32(0);
        for _ in 0..7 {
            s.write_u32(0);
        }
        s.write_u16(0).write_u16(0);
    });
    let item = Item::from_row(
        ItemRow {
            type_name: "AAEmu.Game.Models.Game.Items.Armor".into(),
            id: 0x1000 + u64::from(id),
            template_id: 18_000,
            slot: 3,
            count: 1,
            details,
            created_at: 1_600_000_000,
            ..ItemRow::default()
        },
        None,
        1,
    );
    let mut c = Character::new(id, 1000, Default::default());
    c.unit.name = format!("Char{id}");
    c.race = 1;
    c.gender = 2;
    c.unit.level = 50;
    c.unit.hp = 1000;
    c.unit.mp = 500;
    c.unit.transform.zone_id = 179;
    c.unit.faction_id = 101;
    c.ability1 = 1;
    c.ability2 = 2;
    c.ability3 = 11;
    c.unit.transform.x = 12345.5;
    c.unit.transform.y = 6789.25;
    c.unit.transform.z = 100.0;
    c.labor_power = 5000;
    c.labor_power_modified = 1_700_000_000;
    c.money = 123_456;
    c.crime_point = 7;
    c.infamy_point = 42;
    c.bm_point = 99;
    c.updated = 1_700_000_001;
    c.inventory.equipment().lock().unwrap().items.push(item);
    c
}

/// `Character.Write` spelled out field by field from the 1.2 C# (independent of
/// `impl Marshal for Character`, except for the nested item / model-params writers).
fn expected_character_bytes(c: &Character) -> Vec<u8> {
    bytes(|s| {
        s.write_u32(c.id);
        s.write_str(&c.unit.name);
        s.write_u8(c.race).write_u8(c.gender).write_u8(c.unit.level);
        s.write_i32(c.unit.hp).write_i32(c.unit.mp);
        s.write_u32(c.unit.transform.zone_id)
            .write_u32(c.unit.faction_id);
        s.write_str(&c.faction_name);
        s.write_u32(c.unit.expedition_id).write_u32(c.family);
        for slot in &c.inventory.equipment_slots() {
            match slot {
                None => {
                    s.write_i32(0);
                }
                Some(item) => {
                    // Armor: Item.Write + EquipItem.WriteDetails
                    s.write_u32(item.template_id)
                        .write_u64(item.id)
                        .write_u8(item.grade)
                        .write_u8(item.flags)
                        .write_i32(item.count)
                        .write_u8(1); // DetailType Equipment
                    s.write_u32(0).write_u8(100).write_i16(0).write_u32(0);
                    s.write_i64(item.charge_start_time).write_u32(0);
                    for _ in 0..7 {
                        s.write_u32(0);
                    }
                    s.write_u16(0).write_u16(0);
                    s.write_i64(item.create_time)
                        .write_i32(item.lifespan_mins)
                        .write_u32(item.crafter())
                        .write_u8(item.world_id)
                        .write_i64(item.unsecure_time)
                        .write_i64(item.unpack_time);
                }
            }
        }
        s.write_u8(c.ability1)
            .write_u8(c.ability2)
            .write_u8(c.ability3);
        s.write_i64(((c.unit.transform.x * 4096.0) as i64) << 32)
            .write_i64(((c.unit.transform.y * 4096.0) as i64) << 32)
            .write_f32(c.unit.transform.z);
        c.unit.model_params.write(s);
        s.write_i16(c.labor_power)
            .write_i64(c.labor_power_modified)
            .write_i16(c.dead_count)
            .write_i64(c.dead_time)
            .write_i32(c.rez_wait_duration)
            .write_i64(c.rez_time)
            .write_i32(c.rez_penalty_duration)
            .write_i64(c.leave_time)
            .write_i64(c.money)
            .write_i64(0)
            .write_i16(c.crime_point)
            .write_i32(c.infamy_point)
            .write_i16(0)
            .write_i64(c.delete_request_time)
            .write_i64(c.transfer_request_time)
            .write_i64(c.delete_time)
            .write_i32(c.consumed_labor_power)
            .write_i64(c.bm_point)
            .write_i64(c.money2)
            .write_i64(0)
            .write_bool(c.auto_use_aa_point)
            .write_i32(c.prev_point)
            .write_i32(c.point)
            .write_i32(c.gift)
            .write_i64(c.updated)
            .write_u8(0);
    })
}

#[tokio::test]
async fn dispatch_edge_cases() {
    let server = test_server();
    assert!(server.registry.get(1, cs::X2EnterWorldPacket).is_some());
    assert!(server.registry.get(2, pp::FinishStatePacket).is_some());
    assert!(server.registry.get(2, 0x003).is_none());
    assert_eq!(
        server
            .registry
            .get(1, cs::CSListCharacterPacket)
            .unwrap()
            .name,
        "CSListCharacterPacket"
    );

    let mut c = client(&server, 1);
    // Past `X2EnterWorld` (before it only that packet is dispatched).
    c.conn.set_account_id(1000);
    // Ping -> Pong echoes tm/when/local
    let body = bytes(|s| {
        s.write_i64(11).write_i64(22).write_u32(33);
    });
    server
        .handle_frame(&mut c.conn, &l2(pp::PingPacket, &body))
        .await
        .unwrap();
    let pong = &c.drain()[0];
    assert_eq!(pong[..4], l2(pp::PongPacket, &[])[..]);
    assert_eq!(
        pong[4..20],
        bytes(|s| {
            s.write_i64(11).write_i64(22);
        })[..]
    );
    assert_eq!(pong[20..28], [0; 8]); // elapsed
    assert_eq!(pong[36..40], 33u32.to_le_bytes());
    // FastPing -> FastPong
    server
        .handle_frame(&mut c.conn, &l2(pp::FastPingPacket, &[9, 0, 0, 0]))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![l2(pp::FastPongPacket, &[9, 0, 0, 0])]);

    // A registered packet whose handler only logs: the connection stays up.
    server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::CSCriminalLockedPacket, &[1, 0, 0, 0]),
        )
        .await
        .unwrap();
    assert!(!c.conn.is_closing());
    // `CSEditCharacterPacket` is a real reader now; its body must decode.
    assert!(server
        .handle_frame(&mut c.conn, &c2s_l1(cs::CSEditCharacterPacket, &[1, 2, 3]))
        .await
        .is_err());
    // Levels other than 1 and 2 have no table (C# KeyNotFoundException -> Shutdown).
    for level in [0u8, 3, 4, 5] {
        assert!(server
            .handle_frame(&mut c.conn, &[0xDD, level, 0, 0, 0, 0])
            .await
            .is_err());
    }
    // Truncated body: read error closes the connection.
    assert!(server
        .handle_frame(&mut c.conn, &l2(pp::FinishStatePacket, &[1]))
        .await
        .is_err());
    // Truncated level-1 header.
    assert!(server
        .handle_frame(&mut c.conn, &[0xDD, 0x01, 0x00])
        .await
        .is_err());

    // Packets whose opcode is the 0xFFF placeholder are dropped by GameConnection.SendPacket.
    #[derive(Debug)]
    struct NoOpcode;
    impl PacketDef for NoOpcode {
        const OPCODE: u16 = 0xFFF;
        const LEVEL: u8 = LEVEL_PLAIN;
        const NAME: &'static str = "NoOpcode";
    }
    impl Marshal for NoOpcode {
        fn write(&self, s: &mut PacketStream) {
            s.write_u8(1);
        }
    }
    c.conn.send(&NoOpcode);
    assert!(c.drain().is_empty());

    // Unknown token -> shutdown, nothing sent.
    server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(5, 6)),
        )
        .await
        .unwrap();
    assert!(c.conn.is_closing());
    assert!(c.drain().is_empty());

    // Token registered for another account -> shutdown, token kept for its owner.
    let cookie = server
        .enter_world
        .add_account(7, 8, false)
        .cookie()
        .unwrap();
    let mut c2 = client(&server, 2);
    server
        .handle_frame(
            &mut c2.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(9, cookie)),
        )
        .await
        .unwrap();
    assert!(c2.conn.is_closing());
    assert_eq!(
        server.enter_world.login(7, cookie),
        crate::managers::LoginDecision::Accepted
    );
}

#[tokio::test]
async fn a_second_cookie_for_a_connected_account_is_refused() {
    let server = Arc::new(test_server());
    // Two cookies can be granted before either client reaches the game server: the login server
    // only learns the account is connected from `GLPlayerEnter`, answered when the client sends
    // X2EnterWorld. The second `AccountManager.Add` must not be swallowed.
    let cookie_a = server
        .enter_world
        .add_account(1000, 11, false)
        .cookie()
        .unwrap();
    let cookie_b = server
        .enter_world
        .add_account(1000, 12, false)
        .cookie()
        .unwrap();
    let mut a = client(&server, 1);
    server
        .handle_frame(
            &mut a.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(1000, cookie_a)),
        )
        .await
        .unwrap();
    assert!(!a.conn.is_closing());
    assert_eq!(server.accounts.connection_of(1000), Some(1));
    assert!(!a.drain().is_empty());

    let mut b = client(&server, 2);
    server
        .handle_frame(
            &mut b.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(1000, cookie_b)),
        )
        .await
        .unwrap();
    // Refused like any other invalid world entry: no X2EnterWorldResponse, connection closed.
    assert!(b.conn.is_closing());
    assert!(b.drain().is_empty());
    assert_eq!(
        server.accounts.connection_of(1000),
        Some(1),
        "the existing session keeps the account"
    );
    assert_eq!(server.streams.ticket_for(2), None);
}

#[tokio::test]
async fn a_hard_disconnect_keeps_the_account_reserved_until_the_save_finishes() {
    use crate::test_support::{spawn_player, test_character, TestClient};

    let server = Arc::new(crate::test_support::test_server());
    server.set_self();
    let mut client = TestClient::new(&server, 1, 7);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Saver", 100.0, 100.0),
        0x100,
    );
    client.conn.active_char = Some(player);
    assert!(server.accounts.try_add(7, 1));
    assert!(server.enter_world.reserve_character(1, 1).is_ok());

    // Hold the save transaction: the disconnect waits for it and the account must stay
    // registered meanwhile, so a reconnect cannot load the character before this save commits.
    let transaction = server.save.transaction_lock().await;
    let disconnect = crate::net::listener::on_disconnect(&server, &mut client.conn);
    futures::pin_mut!(disconnect);
    assert!(futures::poll!(disconnect.as_mut()).is_pending());
    assert!(server.accounts.contains(7));
    assert_eq!(server.enter_world.character_owner(1), Some(1));
    drop(transaction);
    disconnect.await;

    assert!(!server.accounts.contains(7));
    assert_eq!(server.enter_world.character_owner(1), None);
    assert!(server.world.character_by_id(1).is_none());
}

#[tokio::test]
async fn tcp_session_roundtrip() {
    use aaemu_commons::net::FrameCodec;
    use futures::{SinkExt, StreamExt};
    use tokio_util::codec::Framed;

    let server = Arc::new(test_server());
    let cookie = server
        .enter_world
        .add_account(5, 9, false)
        .cookie()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = server.clone();
    tokio::spawn(async move {
        let (sock, peer) = listener.accept().await.unwrap();
        crate::net::listener::serve(srv, sock, peer).await;
    });

    let mut client = Framed::new(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        FrameCodec,
    );
    client
        .send(c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(5, cookie)))
        .await
        .unwrap();

    let first = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first[..6], hex("DD 01 00 00 00 00")[..]);
    let second = client.next().await.unwrap().unwrap();
    assert_eq!(second, l2(pp::ChangeStatePacket, &[0, 0, 0, 0]));
    assert_eq!(server.connections.len(), 1);
    assert!(server.accounts.contains(5));

    drop(client);
    for _ in 0..50 {
        if server.connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(server.connections.is_empty());
    assert!(!server.accounts.contains(5));
}

/// A connection that has not completed `X2EnterWorld` may only send `X2EnterWorld`: anything else
/// closes it without running a handler (no reply, no database access).
#[tokio::test]
async fn a_packet_before_x2_enter_world_closes_the_connection() {
    let server = test_server();
    for frame in [
        c2s_l1(cs::CSListCharacterPacket, &[0, 0, 0, 0, 0, 0]),
        c2s_l1(cs::CSLeaveWorldPacket, &[2]),
        l2(pp::PingPacket, &[0; 20]),
        l2(pp::FinishStatePacket, &[0, 0, 0, 0]),
        // An unregistered opcode is not exempt either.
        c2s_l1(0x7FF, &[]),
    ] {
        let mut c = client(&server, 90);
        server.handle_frame(&mut c.conn, &frame).await.unwrap();
        assert!(c.conn.is_closing(), "frame {frame:02x?}");
        assert!(c.drain().is_empty());
        assert_eq!(c.conn.account_id(), 0);
    }
    // An unknown frame level is still a protocol error.
    let mut c = client(&server, 91);
    assert!(server
        .handle_frame(&mut c.conn, &[0xDD, 0x07, 0, 0])
        .await
        .is_err());
}

/// A second `X2EnterWorld` on a connection that already has an account is refused and leaves the
/// first account's registration alone.
#[tokio::test]
async fn a_second_x2_enter_world_is_refused() {
    let server = test_server();
    let cookie5 = server
        .enter_world
        .add_account(5, 9, false)
        .cookie()
        .unwrap();
    let cookie6 = server
        .enter_world
        .add_account(6, 10, false)
        .cookie()
        .unwrap();
    let mut c = client(&server, 92);
    server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(5, cookie5)),
        )
        .await
        .unwrap();
    assert_eq!(c.conn.account_id(), 5);
    assert!(!c.conn.is_closing());
    c.drain();

    server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(6, cookie6)),
        )
        .await
        .unwrap();
    assert!(c.conn.is_closing());
    assert_eq!(c.conn.account_id(), 5);
    assert!(server.accounts.contains(5));
    assert!(!server.accounts.contains(6));
    assert!(c.drain().is_empty());
    // The second cookie was never consumed.
    assert_eq!(
        server.enter_world.login(6, cookie6),
        crate::managers::LoginDecision::Accepted
    );
}

/// After `Network.EnterWorldFailureLimit` failed attempts an IP is refused without the cookie
/// being checked, so a correct cookie cannot be reached by guessing.
#[tokio::test]
async fn repeated_bad_cookies_block_the_ip() {
    let server = test_server();
    let cookie = server
        .enter_world
        .add_account(5, 9, false)
        .cookie()
        .unwrap();
    let limit = server.config.network.enter_world_failure_limit;
    assert_eq!(limit, 5);
    for i in 0..limit {
        let mut c = client(&server, 100 + i);
        server
            .handle_frame(
                &mut c.conn,
                &c2s_l1(
                    cs::X2EnterWorldPacket,
                    &enter_world_body(5, cookie.wrapping_add(1 + i)),
                ),
            )
            .await
            .unwrap();
        assert!(c.conn.is_closing());
    }
    let mut c = client(&server, 200);
    server
        .handle_frame(
            &mut c.conn,
            &c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(5, cookie)),
        )
        .await
        .unwrap();
    assert!(c.conn.is_closing());
    assert_eq!(c.conn.account_id(), 0);
    assert!(!server.accounts.contains(5));
    // The right cookie was not even looked at.
    assert_eq!(
        server.enter_world.login(5, cookie),
        crate::managers::LoginDecision::Accepted
    );
}

type LoopbackClient =
    tokio_util::codec::Framed<tokio::net::TcpStream, aaemu_commons::net::FrameCodec>;

/// Reads frames until the server closes the socket (`true`) or `wait` passes (`false`).
async fn closed_within(client: &mut LoopbackClient, wait: Duration) -> bool {
    use futures::StreamExt;
    tokio::time::timeout(wait, async {
        while let Some(frame) = client.next().await {
            if frame.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}

fn limited(timeout_ms: Option<u64>, rate: Option<(f64, f64)>) -> crate::net::listener::NetLimits {
    crate::net::listener::NetLimits {
        pre_auth_timeout: timeout_ms.map(Duration::from_millis),
        frame_rate: rate,
    }
}

/// Starts `serve_with` on a loopback socket and returns the framed client side.
async fn serve_loopback(
    server: &Arc<GameServer>,
    limits: crate::net::listener::NetLimits,
) -> LoopbackClient {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = server.clone();
    tokio::spawn(async move {
        let (sock, peer) = listener.accept().await.unwrap();
        crate::net::listener::serve_with(srv, sock, peer, limits, None).await;
    });
    tokio_util::codec::Framed::new(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        aaemu_commons::net::FrameCodec,
    )
}

#[tokio::test]
async fn a_connection_that_never_enters_is_closed_after_the_pre_auth_timeout() {
    let server = Arc::new(test_server());
    let mut silent = serve_loopback(&server, limited(Some(200), None)).await;
    assert!(closed_within(&mut silent, Duration::from_secs(10)).await);
    for _ in 0..50 {
        if server.connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(server.connections.is_empty());
}

#[tokio::test]
async fn an_authenticated_connection_is_not_closed_by_the_pre_auth_timeout() {
    use futures::SinkExt;

    let server = Arc::new(test_server());
    let cookie = server
        .enter_world
        .add_account(5, 9, false)
        .cookie()
        .unwrap();
    let mut client = serve_loopback(&server, limited(Some(200), None)).await;
    client
        .send(c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(5, cookie)))
        .await
        .unwrap();
    assert!(!closed_within(&mut client, Duration::from_millis(700)).await);
    assert!(server.accounts.contains(5));
}

#[tokio::test]
async fn a_connection_over_the_frame_rate_is_closed() {
    use futures::SinkExt;

    let server = Arc::new(test_server());
    let cookie = server
        .enter_world
        .add_account(5, 9, false)
        .cookie()
        .unwrap();
    // 1 frame/s with a burst of 5: X2EnterWorld plus a few unknown frames pass, a flood does not.
    let mut client = serve_loopback(&server, limited(None, Some((1.0, 5.0)))).await;
    client
        .send(c2s_l1(cs::X2EnterWorldPacket, &enter_world_body(5, cookie)))
        .await
        .unwrap();
    for _ in 0..30 {
        if client.send(c2s_l1(0x7FF, &[])).await.is_err() {
            break;
        }
    }
    assert!(closed_within(&mut client, Duration::from_secs(10)).await);
}

/// Unknown or unreadable packets are logged at `error` only for the first ten of a 60 s window of
/// a connection; none of them closes it (the real client sends not-yet-ported opcodes).
#[tokio::test]
async fn unknown_packets_are_counted_per_connection_and_do_not_close_it() {
    let server = test_server();
    let mut c = client(&server, 93);
    c.conn.set_account_id(1000);
    for i in 1..=12 {
        server
            .handle_frame(&mut c.conn, &c2s_l1(0x7FF, &[]))
            .await
            .unwrap();
        assert!(!c.conn.is_closing(), "packet {i}");
    }
    // The 13th is the first one past the limit of ten.
    assert!(!c.conn.note_bad_packet());
    let mut fresh = client(&server, 94);
    assert!(fresh.conn.note_bad_packet());
}

/// The `error` budget is per 60 s window, not per connection lifetime: a new unported opcode in a
/// long session still shows up at `error`.
#[test]
fn bad_packet_error_logs_use_a_sliding_window() {
    use crate::net::connection::{BadPacketLog, BAD_PACKET_ERROR_LOG_LIMIT, BAD_PACKET_LOG_WINDOW};
    let mut log = BadPacketLog::default();
    let t0 = std::time::Instant::now();
    for i in 0..BAD_PACKET_ERROR_LOG_LIMIT {
        assert!(log.note(t0 + Duration::from_millis(u64::from(i)), 1), "{i}");
    }
    // The rest of the window is demoted ...
    for i in 0..50 {
        assert!(!log.note(t0 + Duration::from_secs(1 + i / 10), 1));
    }
    // ... and the next window starts with a fresh budget.
    let next = t0 + BAD_PACKET_LOG_WINDOW;
    for i in 0..BAD_PACKET_ERROR_LOG_LIMIT {
        assert!(
            log.note(next + Duration::from_millis(u64::from(i)), 1),
            "{i}"
        );
    }
    assert!(!log.note(next + Duration::from_secs(1), 1));
    // A quiet connection never overflows.
    let mut quiet = BadPacketLog::default();
    for i in 0..100u64 {
        assert!(quiet.note(t0 + Duration::from_secs(i * 61), 2));
    }
}
