//! Stream server tests: byte-exact S->C encodings, C->S decodings, the join/token handshake
//! (through the real X2EnterWorld path), doodad cell streaming, UCC upload/download and a
//! socket-level round trip. MySQL is not available (the pool points at a closed port), so the
//! UCC `Save` step fails exactly like an uncaught C# MySqlException would.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::helpers::encode_position;
use aaemu_commons::PacketStream;
use sqlx::mysql::MySqlPoolOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::c2s::*;
use super::connection::{StreamCommand, StreamConnection};
use super::models::{DotNetDateTime, Ucc, UccType};
use super::opcodes::{ct, tc};
use super::packet::{encode, StreamServerPacket};
use super::protocol::{handle_frame, REGISTERED};
use super::s2c::*;
use super::ucc::{crest_printer_cost, CrestPrinterCost, CREST_STAMP_SKILL};
use super::world::{
    ActiveCharacter, ItemUcc, StreamDoodad, StreamWorld, ITEM_CREST_BRAINSTORM, ITEM_CREST_INK,
};
use crate::config::AppConfig;
use crate::data::items::{ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::item_task::task_type;
use crate::net::connection::GameConnection;
use crate::opcodes::cs;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::templates::{DoodadFunc, DoodadFuncStampMaker, FuncTemplate};
use crate::GameServer;

// ---------------------------------------------------------------------------------------------
// helpers

/// `doodad_funcs` rows 6314 / 11693 / 19145 and their `doodad_func_stamp_makers` rows (the
/// 1.2 compact values: 3038 -> 50000 + 1x 11127, 1470 -> 10000).
fn install_crest_printers(doodad: &mut DoodadData) {
    let mut rows = HashMap::new();
    for (id, money, item, count) in [
        (2, 50_000, ITEM_CREST_BRAINSTORM, 1),
        (3, 10_000, 0, 1),
        (4, 50_000, ITEM_CREST_BRAINSTORM, 1),
    ] {
        rows.insert(
            id,
            FuncTemplate::StampMaker(DoodadFuncStampMaker {
                id,
                consume_money: money,
                item_id: ITEM_CREST_INK,
                consume_item_id: item,
                consume_count: count,
            }),
        );
    }
    doodad.func_templates.insert("DoodadFuncStampMaker", rows);
    let func = |group, key, func_id| DoodadFunc {
        group_id: group,
        func_id,
        func_key: key,
        func_type: "DoodadFuncStampMaker".into(),
        next_phase: -1,
        sound_id: 0,
        skill_id: CREST_STAMP_SKILL,
        perm_id: 0,
        count: 0,
    };
    doodad
        .funcs_by_groups
        .insert(6944, vec![func(6944, 6314, 2)]);
    doodad
        .funcs_by_groups
        .insert(2260, vec![func(2260, 11693, 3)]);
    doodad
        .funcs_by_groups
        .insert(22729, vec![func(22729, 19145, 4)]);
}

fn test_config() -> AppConfig {
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.stream_network.port = 1250;
    config.network.num_connections = 10;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1; // nothing listens here
    config
}

fn test_server() -> GameServer {
    let config = test_config();
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    test_server_with_db(config, db)
}

/// [`test_server`] with a caller-built pool (the MySQL-gated two-upload test).
fn test_server_with_db(config: AppConfig, db: sqlx::mysql::MySqlPool) -> GameServer {
    let mut data = GameData::empty();
    // `ConfirmDefaultUcc` creates a Crest Ink (the C# hack-fix template).
    data.items.templates.insert(
        ITEM_CREST_INK,
        ItemTemplate::new(ITEM_CREST_INK, TemplateKind::Ucc),
    );
    // The custom-upload reagent of the crest printer rows.
    data.items.templates.insert(
        ITEM_CREST_BRAINSTORM,
        ItemTemplate::new(ITEM_CREST_BRAINSTORM, TemplateKind::Item),
    );
    install_crest_printers(&mut data.systems.npcs.doodad);
    GameServer::new(config, db, data)
}

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

/// Expected frame payload `[u16 opcode][body]`.
fn frame(opcode: u16, body: &[u8]) -> Vec<u8> {
    let mut v = opcode.to_le_bytes().to_vec();
    v.extend_from_slice(body);
    v
}

fn bytes(f: impl FnOnce(&mut PacketStream)) -> Vec<u8> {
    let mut s = PacketStream::new();
    f(&mut s);
    s.into_bytes()
}

fn join_frame(account_id: u32, ticket: u32) -> Vec<u8> {
    frame(
        ct::CT_JOIN,
        &bytes(|s| {
            s.write_u32(account_id).write_u32(ticket);
        }),
    )
}

fn enc(p: &dyn StreamServerPacket) -> Vec<u8> {
    encode(p)
}

struct StreamClient {
    conn: StreamConnection,
    out: mpsc::UnboundedReceiver<Vec<u8>>,
    _cmd: mpsc::UnboundedReceiver<StreamCommand>,
}

fn stream_client(server: &GameServer, id: u32) -> StreamClient {
    let (out_tx, out) = mpsc::unbounded_channel();
    let (cmd_tx, cmd) = mpsc::unbounded_channel();
    let peer: SocketAddr = "127.0.0.1:50001".parse().unwrap();
    let (conn, handle) = StreamConnection::new(id, peer, out_tx, cmd_tx);
    server.streams.connections.add(handle);
    StreamClient {
        conn,
        out,
        _cmd: cmd,
    }
}

impl StreamClient {
    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        while let Ok(f) = self.out.try_recv() {
            v.push(f);
        }
        v
    }

    async fn recv(&mut self, server: &GameServer, payload: &[u8]) -> crate::Result<()> {
        handle_frame(server, &mut self.conn, payload).await
    }
}

/// A game connection registered in the table (as `GameProtocolHandler.OnConnect`). The
/// returned receivers must be kept alive for its sends to succeed.
type GameClient = (
    GameConnection,
    crate::net::outbox::OutboxReceiver,
    mpsc::UnboundedReceiver<crate::net::connection::ConnCommand>,
);

fn game_connection(server: &GameServer, id: u32) -> GameClient {
    let (out_tx, out) = crate::net::outbox::outbox();
    let (cmd_tx, cmd) = mpsc::unbounded_channel();
    let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let (conn, handle) = GameConnection::new(id, peer, out_tx, cmd_tx);
    server.connections.add(handle);
    (conn, out, cmd)
}

fn doodad(i: u32) -> StreamDoodad {
    StreamDoodad {
        obj_id: 0x1000 + i,
        template_id: i,
        x: 100.0,
        y: 200.0,
        z: 50.0,
        roll: 0,
        pitch: 0,
        yaw: i as i16,
        scale: 1.0,
        func_group_id: 0,
        time_left: 0,
        plant_time: 0,
    }
}

#[derive(Default)]
struct TestWorld {
    doodads: Vec<StreamDoodad>,
}

impl StreamWorld for TestWorld {
    fn doodads_in_cell(&self, instance_id: u32, _x: i32, _y: i32) -> Option<Vec<StreamDoodad>> {
        (instance_id == 1).then(|| self.doodads.clone())
    }
    fn doodad(&self, instance_id: u32, obj_id: u32) -> Option<StreamDoodad> {
        if instance_id == 0 {
            self.doodads.iter().find(|d| d.obj_id == obj_id).cloned()
        } else {
            None
        }
    }
    fn character_name(&self, character_id: u32) -> Option<String> {
        (character_id == 7).then(|| "Abc".to_string())
    }
    fn item_ucc(&self, item_id: u64) -> Option<ItemUcc> {
        (item_id < 100).then_some(ItemUcc {
            item_id,
            ucc_id: item_id * 10,
        })
    }
    fn active_character(&self, game_connection_id: u32) -> Option<ActiveCharacter> {
        (game_connection_id == 77).then(|| ActiveCharacter {
            id: 555,
            name: "Tester".into(),
        })
    }
}

fn custom_ucc(id: u32, len: usize) -> Ucc {
    let mut u = Ucc::new(UccType::Complex, 555);
    u.id = id;
    u.pattern1 = 1;
    u.pattern2 = 2;
    u.color1 = [3, 4, 5];
    u.color2 = [6, 7, 8];
    u.color3 = [9, 10, 11];
    u.modified = DotNetDateTime::from_binary(0x0102_0304_0506_0708).unwrap();
    u.data = (0..len).map(|i| i as u8).collect();
    u
}

/// `DefaultUcc.Write` of [`custom_ucc`].
fn appearance() -> Vec<u8> {
    hex(
        "01 00 00 00  02 00 00 00  03 00 00 00  04 00 00 00  05 00 00 00  06 00 00 00
         07 00 00 00  08 00 00 00  09 00 00 00  0A 00 00 00  0B 00 00 00
         08 07 06 05 04 03 02 01",
    )
}

/// Server with a joined stream connection (id 900) bound to game connection 77.
async fn joined(world: Arc<TestWorld>) -> (GameServer, StreamClient, GameClient) {
    joined_with(test_server(), world).await
}

/// [`joined`] on a caller-built server (the MySQL-gated upload test).
async fn joined_with(
    server: GameServer,
    world: Arc<TestWorld>,
) -> (GameServer, StreamClient, GameClient) {
    server.streams.set_world(world);
    let g = game_connection(&server, 77);
    // The ActiveChar of game connection 77 (character 555, as `TestWorld::active_character`).
    let mut character = crate::test_support::test_character(555, "Tester", 0.0, 0.0);
    character.money = 100;
    let p = crate::world::Player::new(character, g.0.handle());
    assert!(server.world.try_add_character(&p));
    assert!(server.streams.add_token_with_ticket(1000, 77, 77));
    let mut c = stream_client(&server, 900);
    c.recv(
        &server,
        &frame(ct::CT_JOIN, &hex("E8 03 00 00  4D 00 00 00")),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![hex("01 00 00")]);
    (server, c, g)
}

// ---------------------------------------------------------------------------------------------
// WP-U1 crest printer fixtures

/// The `bc` of the crest printer the upload tests stand at.
const PRINTER_OBJ: u32 = 0x2001;

fn printer(obj_id: u32, template_id: u32, func_group_id: u32) -> StreamDoodad {
    StreamDoodad {
        obj_id,
        template_id,
        x: 2.0,
        y: 0.0,
        z: 100.0,
        roll: 0,
        pitch: 0,
        yaw: 0,
        scale: 1.0,
        func_group_id,
        time_left: 0,
        plant_time: 0,
    }
}

/// Doodad 3038 ("Crest Printer"): `doodad_funcs` 6314 -> row 2 (50000 + 1x 11127).
fn printer_3038() -> StreamDoodad {
    printer(PRINTER_OBJ, 3038, 6944)
}

/// Doodad 1470 (test doodad): `doodad_funcs` 11693 -> row 3 (10000, no item).
fn printer_1470() -> StreamDoodad {
    printer(PRINTER_OBJ, 1470, 2260)
}

/// [`joined`] plus a synthetic world instance 0 with the live doodad `p` (at 2 m from the
/// player). The stream fake answers `doodad()` from the same snapshot.
async fn joined_printer(p: StreamDoodad) -> (GameServer, StreamClient, GameClient) {
    joined_printer_with(test_server(), p).await
}

/// [`joined_printer`] on a caller-built server (the MySQL-gated upload test).
async fn joined_printer_with(
    server: GameServer,
    p: StreamDoodad,
) -> (GameServer, StreamClient, GameClient) {
    let (server, c, g) = joined_with(
        server,
        Arc::new(TestWorld {
            doodads: vec![p.clone()],
        }),
    )
    .await;
    server.world.create_world_instance(
        Arc::new(crate::data::world_templates::WorldTemplate::synthetic(
            0,
            "main_world",
            2,
            2,
            crate::test_support::TEST_ZONE,
        )),
        0,
    );
    let mut d = crate::models::doodad::Doodad::new(p.template_id);
    d.func_group_id = p.func_group_id;
    d.transform.set_xyz(p.x, p.y, p.z);
    let d = crate::world::Entity::new(p.obj_id, d);
    server.world.world(0).unwrap().add_object(&d);
    (server, c, g)
}

/// `CTStartUploadEmblemStreamPacket` body: `bc ; uccId(-1) ; dataSize ; DefaultUcc`.
fn start_upload_body(bc: u32, data_size: i32) -> Vec<u8> {
    let mut body = vec![
        (bc & 0xFF) as u8,
        ((bc >> 8) & 0xFF) as u8,
        ((bc >> 16) & 0xFF) as u8,
    ];
    body.extend((-1i64).to_le_bytes());
    body.extend(data_size.to_le_bytes());
    body.extend(appearance());
    body
}

fn give_item(server: &GameServer, player: &crate::world::PlayerRef, template_id: u32, count: i32) {
    let id = u64::from(server.ids.item.get_next_id().unwrap());
    let item = crate::models::item::Item::create(
        id,
        server.game_data.items.template(template_id).unwrap(),
        count,
        0,
        1,
        &server.game_data.items,
        0,
    );
    let bag = crate::systems::economy::inventory::Inv::of(player).bag();
    bag.lock()
        .unwrap()
        .add_new_item(item, -1, &server.game_data.items)
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// S->C encodings

#[test]
fn tc_packets_are_byte_exact() {
    assert_eq!(enc(&TCJoinResponsePacket { response: 1 }), hex("01 00 01"));

    let d = StreamDoodad {
        obj_id: 0x123456,
        template_id: 0x1234,
        x: 10.0,
        y: 20.0,
        z: 30.0,
        roll: 1,
        pitch: -1,
        yaw: 0x100,
        scale: 1.0,
        func_group_id: 5,
        time_left: 6,
        plant_time: 7,
    };
    let mut want = hex("03 00 00 00  01 00 00 00  01 00 00 00  56 34 12  34 12 00 00");
    want.extend_from_slice(&encode_position(10.0, 20.0, 30.0));
    want.extend(hex(
        "01 00  FF FF  00 01  00 00 80 3F  05 00 00 00  06 00 00 00  07 00 00 00 00 00 00 00",
    ));
    assert_eq!(
        enc(&TCDoodadStreamPacket {
            id: 3,
            next: 1,
            doodads: vec![d],
        }),
        frame(tc::TC_DOODAD_STREAM, &want)
    );

    assert_eq!(
        enc(&TCDoodadIdsPacket {
            id: 1,
            next_id: 2,
            total: 3,
            obj_ids: vec![0x010203, 5],
        }),
        frame(
            tc::TC_DOODAD_IDS,
            &hex("01 00 00 00  02 00 00 00  03 00 00 00  02 00 00 00  03 02 01  05 00 00")
        )
    );
    assert_eq!(
        enc(&TCDownloadEmblemPacket),
        frame(tc::TC_DOWNLOAD_EMBLEM, &[0; 20])
    );
    assert_eq!(
        enc(&TCUccComplexPacket {
            id: 0x01020304,
            modified: 0x11,
        }),
        frame(
            tc::TC_UCC_COMPLEX,
            &hex(
                "04 03 02 01 00 00 00 00  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00
                 04 03 02 01 00 00 00 00  11 00 00 00 00 00 00 00"
            )
        )
    );
    assert_eq!(enc(&TCUccStringPacket), frame(tc::TC_UCC_STRING, &[0; 18]));
    assert_eq!(
        enc(&TCUccPositionPacket),
        frame(tc::TC_UCC_POSITION, &[0; 36])
    );
    assert_eq!(
        enc(&TCUccCharNamePacket {
            id: 7,
            name: "Ab".into(),
        }),
        hex("08 00  07 00 00 00  02 00 41 62")
    );
    assert_eq!(
        enc(&TCCharNameQueriedPacket {
            id: 7,
            name: "Ab".into(),
        }),
        hex("09 00  07 00 00 00  02 00 41 62")
    );
    assert_eq!(
        enc(&TCEmblemStreamRecvStatusPacket {
            status: EmblemStreamStatus::End,
        }),
        hex("0A 00  02  00 00 00 00")
    );

    let ucc = custom_ucc(9, 5);
    let mut want = hex("09 00 00 00 00 00 00 00  05 00 00 00");
    want.extend(appearance());
    want.push(1);
    assert_eq!(
        enc(&TCEmblemStreamSendStatusPacket::new(
            &ucc,
            EmblemStreamStatus::Start
        )),
        frame(tc::TC_EMBLEM_STREAM_SEND_STATUS, &want)
    );
    // A DefaultUcc sends total 0.
    let mut simple = custom_ucc(9, 5);
    simple.ty = UccType::Simple;
    let mut want = hex("09 00 00 00 00 00 00 00  00 00 00 00");
    want.extend(appearance());
    want.push(0);
    assert_eq!(
        enc(&TCEmblemStreamSendStatusPacket::new(
            &simple,
            EmblemStreamStatus::Continue
        )),
        frame(tc::TC_EMBLEM_STREAM_SEND_STATUS, &want)
    );

    assert_eq!(
        enc(&TCItemUccDataPacket {
            player_id: 5,
            items: vec![ItemUcc {
                item_id: 1,
                ucc_id: 2,
            }],
        }),
        hex("0D 00  05 00 00 00  01 00 00 00  01 00 00 00 00 00 00 00  02 00 00 00 00 00 00 00")
    );
    assert_eq!(enc(&TCHouseFarmPacket), hex("0E 00"));
    assert_eq!(
        enc(&TCUccComplexCheckValidPacket {
            id: 9,
            is_valid: false,
        }),
        hex("0F 00  09 00 00 00 00 00 00 00  00")
    );
}

#[test]
fn emblem_download_chunks() {
    let ucc = custom_ucc(9, 3073);
    // chunk 0: 3072 bytes
    let p = TCEmblemStreamDownloadPacket::new(&ucc, 0).unwrap();
    let mut want = hex("00 00 00 00  00 0C 00 00  00 0C");
    want.extend_from_slice(&ucc.data[..3072]);
    assert_eq!(enc(&p), frame(tc::TC_EMBLEM_STREAM_DOWNLOAD, &want));
    // chunk 1: the last byte
    let p = TCEmblemStreamDownloadPacket::new(&ucc, 1).unwrap();
    let mut want = hex("01 00 00 00  01 00 00 00  01 00");
    want.push(ucc.data[3072]);
    assert_eq!(enc(&p), frame(tc::TC_EMBLEM_STREAM_DOWNLOAD, &want));
    // chunk 2: beyond the end -> negative size (3073 - 6144 = -3071) written unchecked
    let p = TCEmblemStreamDownloadPacket::new(&ucc, 2).unwrap();
    assert_eq!(
        enc(&p),
        frame(
            tc::TC_EMBLEM_STREAM_DOWNLOAD,
            &hex("02 00 00 00  01 F4 FF FF  01 F4")
        )
    );
    // no data / not custom -> index, 0, 0
    let mut simple = custom_ucc(9, 0);
    assert_eq!(
        enc(&TCEmblemStreamDownloadPacket::new(&simple, 4).unwrap()),
        frame(
            tc::TC_EMBLEM_STREAM_DOWNLOAD,
            &hex("04 00 00 00  00 00 00 00  00 00")
        )
    );
    simple.ty = UccType::Simple;
    simple.data = vec![1, 2, 3];
    assert_eq!(
        enc(&TCEmblemStreamDownloadPacket::new(&simple, 0).unwrap()),
        frame(
            tc::TC_EMBLEM_STREAM_DOWNLOAD,
            &hex("00 00 00 00  00 00 00 00  00 00")
        )
    );
    // negative start offset with a positive size: List.GetRange throws in C#
    assert!(TCEmblemStreamDownloadPacket::new(&ucc, -1).is_err());
}

// ---------------------------------------------------------------------------------------------
// C->S decodings

fn read<P: StreamClientPacket>(body: &[u8]) -> P {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let p = P::read(&mut s).unwrap();
    assert_eq!(s.remaining(), 0, "{} left bytes unread", P::NAME);
    p
}

#[test]
fn ct_packets_decode() {
    assert_eq!(
        read::<CTJoinPacket>(&hex("E8 03 00 00  4D 00 00 00")),
        CTJoinPacket {
            account_id: 1000,
            cookie: 77,
        }
    );
    assert_eq!(
        read::<CTRequestCellPacket>(&hex("01 00 00 00  FF FF FF FF  02 00 00 00")),
        CTRequestCellPacket {
            instance_id: 1,
            x: -1,
            y: 2,
        }
    );
    assert_eq!(
        read::<CTRequestEmblemPacket>(&hex("09 00 00 00 00 00 00 01")),
        CTRequestEmblemPacket {
            ucc_id: 0x0100_0000_0000_0009,
        }
    );
    assert_eq!(
        read::<CTCancelCellPacket>(&hex("01 00 00 00  02 00 00 00  03 00 00 00")),
        CTCancelCellPacket { i: 1, x: 2, y: 3 }
    );
    assert_eq!(
        read::<CTContinuePacket>(&hex("05 00 00 00  1E 00 00 00")),
        CTContinuePacket { id: 5, next: 30 }
    );
    assert_eq!(
        read::<CTUccComplexPacket>(&hex("09 00 00 00 00 00 00 00")),
        CTUccComplexPacket { id: 9 }
    );
    assert_eq!(
        read::<CTUccStringPacket>(&hex("FF FF FF FF FF FF FF FF")),
        CTUccStringPacket { id: -1 }
    );
    assert_eq!(
        read::<CTUccPositionPacket>(&hex("02 00 00 00 00 00 00 00")),
        CTUccPositionPacket { id: 2 }
    );
    assert_eq!(
        read::<CTUccCharacterNamePacket>(&hex("07 00 00 00")),
        CTUccCharacterNamePacket { id: 7 }
    );
    assert_eq!(
        read::<CTQueryCharNamePacket>(&hex("07 00 00 00")),
        CTQueryCharNamePacket { id: 7 }
    );
    assert_eq!(
        read::<CTUploadEmblemStreamPacket>(&hex(
            "05 00 00 00  03 00 00 00  01 00 00 00  03 00  AA BB CC"
        )),
        CTUploadEmblemStreamPacket {
            total: 5,
            size: 3,
            index: 1,
            data: vec![0xAA, 0xBB, 0xCC],
        }
    );
    assert_eq!(
        read::<CTEmblemStreamUploadStatusPacket>(&hex("01")),
        CTEmblemStreamUploadStatusPacket { status: 1 }
    );

    let mut body = hex("03 02 01  FF FF FF FF FF FF FF FF  05 00 00 00");
    body.extend(appearance());
    let p = read::<CTStartUploadEmblemStreamPacket>(&body);
    assert_eq!((p.bc, p.ucc_id, p.data_size), (0x010203, -1, 5));
    let mut want = custom_ucc(0, 0);
    want.uploader_id = 0;
    assert_eq!(p.ucc, want);
    let mut body = hex("03 02 01  00 00 00 00 00 00 00 00  00 00 00 00");
    body.extend(appearance());
    assert_eq!(
        read::<CTStartUploadEmblemStreamPacket>(&body).ucc.ty,
        UccType::Simple
    );

    assert_eq!(
        read::<CTEmblemStreamDownloadStatusPacket>(&hex(
            "09 00 00 00 00 00 00 00  04  02 00 00 00"
        )),
        CTEmblemStreamDownloadStatusPacket {
            id: 9,
            status: 4,
            count: 2,
        }
    );
    assert_eq!(
        read::<CTItemUccPacket>(&hex(
            "05 00 00 00  02 00 00 00  01 00 00 00 00 00 00 00  02 00 00 00 00 00 00 00"
        )),
        CTItemUccPacket {
            player: 5,
            items: vec![1, 2],
        }
    );
    assert_eq!(
        read::<CTEmblemPartDownloadedPacket>(&hex("FF FF FF FF  00 0C 00 00")),
        CTEmblemPartDownloadedPacket {
            previous_index: -1,
            previous_size: 3072,
        }
    );
    assert_eq!(
        read::<CTUccComplexCheckValidPacket>(&hex("09 00 00 00 00 00 00 00")),
        CTUccComplexCheckValidPacket { id: 9 }
    );
}

#[test]
fn registered_opcodes_match_ct_offsets() {
    let ops: Vec<u16> = REGISTERED.iter().map(|&(op, _)| op).collect();
    assert_eq!(
        ops,
        vec![
            0x001, 0x002, 0x003, 0x004, 0x005, 0x006, 0x007, 0x008, 0x009, 0x00a, 0x00c, 0x00d,
            0x00e, 0x00f, 0x010, 0x011, 0x012
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Join / token handshake

#[tokio::test]
async fn join_with_token_from_enter_world() {
    let server = Arc::new(test_server());
    // login -> game: LGPlayerEnter(account 1000, connection 4242); the game issues the cookie
    let (login_tx, mut login_rx) = tokio::sync::mpsc::unbounded_channel();
    server.login.set(Some(login_tx));
    assert!(crate::login::handle(&server, &hex("01 00  E8 03 00 00  92 10 00 00")).await);
    let reply = login_rx.try_recv().unwrap();
    let cookie = reply[16..20].to_vec();
    // X2EnterWorld(account 1000, that cookie) on game connection 77 -> StreamManager.AddToken
    let mut g = game_connection(&server, 77);
    let mut x2 = vec![0xDD, 0x01, 0x00, 0x00];
    x2.extend_from_slice(&cs::X2EnterWorldPacket.to_le_bytes());
    x2.extend(hex("00 00 00 00  00 00 00 00  E8 03 00 00"));
    x2.extend(cookie);
    x2.extend(hex("00 00 00 00  00"));
    x2.extend_from_slice(&[0; 8]);
    server.handle_frame(&mut g.0, &x2).await.unwrap();
    // The join ticket is random, not the connection id.
    let ticket = server.streams.ticket_for(77).expect("ticket");
    assert_ne!(ticket, 0);
    assert_eq!(server.streams.account_for(ticket), Some(1000));
    assert_eq!(server.streams.account_for(77), None);

    // Stream join with the ticket -> accepted, linked to game conn 77.
    let mut s = stream_client(&server, 900);
    s.recv(&server, &join_frame(1000, ticket)).await.unwrap();
    assert_eq!(s.drain(), vec![hex("01 00 00")]);
    assert_eq!(s.conn.game_connection_id, Some(77));
    assert!(!s.conn.is_closing());

    // The ticket is not consumed: a second stream connection may join too.
    let mut s2 = stream_client(&server, 901);
    s2.recv(&server, &join_frame(1000, ticket)).await.unwrap();
    assert_eq!(s2.drain(), vec![hex("01 00 00")]);

    // The game connection id is not a ticket.
    let mut bad = stream_client(&server, 902);
    bad.recv(&server, &join_frame(1000, 77)).await.unwrap();
    assert_eq!(bad.drain(), vec![hex("01 00 01")]);
    assert_eq!(bad.conn.game_connection_id, None);
    assert!(!bad.conn.is_closing());

    // Known ticket, wrong account -> response 1 and the owner's ticket stays usable (the C#
    // removed the token, which let anyone break another player's stream session).
    bad.recv(&server, &join_frame(5, ticket)).await.unwrap();
    assert_eq!(bad.drain(), vec![hex("01 00 01")]);
    assert_eq!(server.streams.account_for(ticket), Some(1000));
    let mut s3 = stream_client(&server, 903);
    s3.recv(&server, &join_frame(1000, ticket)).await.unwrap();
    assert_eq!(s3.drain(), vec![hex("01 00 00")]);

    // The ticket dies with the game connection.
    server.streams.remove_token(77);
    let mut s4 = stream_client(&server, 904);
    s4.recv(&server, &join_frame(1000, ticket)).await.unwrap();
    assert_eq!(s4.drain(), vec![hex("01 00 01")]);
}

#[tokio::test]
async fn unknown_and_truncated_packets() {
    let server = test_server();
    let mut s = stream_client(&server, 900);
    // 0x00b is not registered in 1.2: logged and ignored (before a join it also closes the
    // connection, see `unknown_packet_before_join_closes_the_connection`).
    s.recv(&server, &frame(0x00b, &[1, 2])).await.unwrap();
    // Payload shorter than an opcode: opcode 0 (unknown).
    s.recv(&server, &[0x07]).await.unwrap();
    s.recv(&server, &[]).await.unwrap();
    assert!(s.drain().is_empty());
    // A truncated body is an error (closes the connection).
    assert!(s
        .recv(&server, &frame(ct::CT_JOIN, &[1, 2, 3]))
        .await
        .is_err());
}

#[tokio::test]
async fn unknown_packet_before_join_closes_the_connection() {
    let server = test_server();
    let mut s = stream_client(&server, 900);
    assert!(!s.conn.is_joined());
    // A scanner-style probe: unknown opcode with a body far larger than the log dump cap.
    s.recv(&server, &frame(0x00b, &[0xAB; 4000])).await.unwrap();
    assert!(s.conn.is_closing());
    assert!(s.drain().is_empty());

    // Frames shorter than an opcode (opcode 0) are unknown too.
    let mut s = stream_client(&server, 901);
    s.recv(&server, &[]).await.unwrap();
    assert!(s.conn.is_closing());

    // A failed join (bad token) does not make the connection trusted.
    let mut s = stream_client(&server, 902);
    s.recv(
        &server,
        &frame(ct::CT_JOIN, &hex("E8 03 00 00  63 00 00 00")),
    )
    .await
    .unwrap();
    assert_eq!(s.drain(), vec![hex("01 00 01")]);
    assert!(!s.conn.is_joined());
    s.recv(&server, &frame(0x00b, &[1])).await.unwrap();
    assert!(s.conn.is_closing());
}

#[tokio::test]
async fn unknown_packet_after_join_is_ignored() {
    let server = test_server();
    let _g = game_connection(&server, 77);
    server.streams.add_token_with_ticket(1000, 77, 77);
    let mut s = stream_client(&server, 900);
    s.recv(
        &server,
        &frame(ct::CT_JOIN, &hex("E8 03 00 00  4D 00 00 00")),
    )
    .await
    .unwrap();
    assert_eq!(s.drain(), vec![hex("01 00 00")]);
    assert!(s.conn.is_joined());

    // C# HandleUnknownPacket: logged and ignored, the connection keeps working.
    s.recv(&server, &frame(0x00b, &[0xAB; 4000])).await.unwrap();
    s.recv(&server, &[0x07]).await.unwrap();
    assert!(!s.conn.is_closing());
    assert!(s.drain().is_empty());
    s.recv(
        &server,
        &frame(ct::CT_JOIN, &hex("E8 03 00 00  63 00 00 00")),
    )
    .await
    .unwrap();
    assert_eq!(s.drain(), vec![hex("01 00 01")]);
}

#[tokio::test]
async fn no_game_connection_or_active_char_is_fatal_for_ucc_requests() {
    let server = test_server();
    server.streams.ucc.insert(custom_ucc(9, 10));
    // Not joined: only CTJoin is served, anything else closes the connection (the C# threw a
    // NullReferenceException on connection.GameConnection).
    let mut s = stream_client(&server, 899);
    s.recv(&server, &frame(ct::CT_REQUEST_EMBLEM, &9u64.to_le_bytes()))
        .await
        .unwrap();
    assert!(s.conn.is_closing());
    // Joined to a game connection that has no active character.
    let _g = game_connection(&server, 78);
    assert!(server.streams.add_token_with_ticket(1001, 78, 78));
    let mut s = stream_client(&server, 900);
    s.recv(&server, &join_frame(1001, 78)).await.unwrap();
    assert_eq!(s.drain(), vec![hex("01 00 00")]);
    assert!(s
        .recv(&server, &frame(ct::CT_REQUEST_EMBLEM, &9u64.to_le_bytes()))
        .await
        .is_err());
    let mut start = hex("03 02 01  00 00 00 00 00 00 00 00  00 00 00 00");
    start.extend(appearance());
    assert!(s
        .recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .is_err());
    // Packets not touching the game connection keep working.
    s.recv(&server, &frame(ct::CT_UCC_COMPLEX, &9u64.to_le_bytes()))
        .await
        .unwrap();
    assert_eq!(s.drain().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Doodad cells

#[tokio::test]
async fn cell_streaming_in_batches_of_30() {
    let world = Arc::new(TestWorld {
        doodads: (0..70).map(doodad).collect(),
    });
    let (server, mut c, _g) = joined(world).await;

    let want = |id: i32, next: i32, range: std::ops::Range<u32>| {
        enc(&TCDoodadStreamPacket {
            id,
            next,
            doodads: range.map(doodad).collect(),
        })
    };
    let cell = |instance: u32| {
        let mut b = instance.to_le_bytes().to_vec();
        b.extend(hex("02 00 00 00  03 00 00 00"));
        b
    };

    c.recv(&server, &frame(ct::CT_REQUEST_CELL, &cell(1)))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![want(0, 30, 0..30)]);

    // ContinueCell copies from next - 1 (C# off-by-one).
    c.recv(
        &server,
        &frame(ct::CT_CONTINUE, &hex("00 00 00 00  1E 00 00 00")),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![want(0, 60, 29..59)]);
    c.recv(
        &server,
        &frame(ct::CT_CONTINUE, &hex("00 00 00 00  3C 00 00 00")),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![want(0, 70, 59..69)]);
    // next == total: request dropped, an empty batch is still sent.
    c.recv(
        &server,
        &frame(ct::CT_CONTINUE, &hex("00 00 00 00  46 00 00 00")),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![want(0, 70, 0..0)]);
    // Unknown request: nothing.
    c.recv(
        &server,
        &frame(ct::CT_CONTINUE, &hex("00 00 00 00  46 00 00 00")),
    )
    .await
    .unwrap();
    assert!(c.drain().is_empty());

    // Second request gets id 1; next > total throws in C# (negative array size).
    c.recv(&server, &frame(ct::CT_REQUEST_CELL, &cell(1)))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![want(1, 30, 0..30)]);
    assert!(c
        .recv(
            &server,
            &frame(ct::CT_CONTINUE, &hex("01 00 00 00  50 00 00 00"))
        )
        .await
        .is_err());

    // Unknown world instance: null world in C# -> error.
    assert!(c
        .recv(&server, &frame(ct::CT_REQUEST_CELL, &cell(2)))
        .await
        .is_err());
    // Cancel is a no-op.
    c.recv(&server, &frame(ct::CT_CANCEL_CELL, &cell(1)))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
}

#[tokio::test]
async fn null_world_serves_empty_cells() {
    let server = test_server();
    let _g = game_connection(&server, 77);
    server.streams.add_token_with_ticket(1000, 77, 77);
    let mut c = stream_client(&server, 900);
    c.recv(&server, &join_frame(1000, 77)).await.unwrap();
    assert_eq!(c.drain(), vec![hex("01 00 00")]);
    c.recv(
        &server,
        &frame(
            ct::CT_REQUEST_CELL,
            &hex("00 00 00 00  02 00 00 00  03 00 00 00"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        c.drain(),
        vec![frame(
            tc::TC_DOODAD_STREAM,
            &hex("00 00 00 00  00 00 00 00  00 00 00 00")
        )]
    );
}

// ---------------------------------------------------------------------------------------------
// Names / items

#[tokio::test]
async fn names_and_item_uccs() {
    let (server, mut c, _g) = joined(Arc::new(TestWorld::default())).await;
    c.recv(&server, &frame(ct::CT_QUERY_CHAR_NAME, &hex("07 00 00 00")))
        .await
        .unwrap();
    c.recv(
        &server,
        &frame(ct::CT_UCC_CHARACTER_NAME, &hex("07 00 00 00")),
    )
    .await
    .unwrap();
    // Unknown name: no answer.
    c.recv(&server, &frame(ct::CT_QUERY_CHAR_NAME, &hex("08 00 00 00")))
        .await
        .unwrap();
    assert_eq!(
        c.drain(),
        vec![
            hex("09 00  07 00 00 00  03 00 41 62 63"),
            hex("08 00  07 00 00 00  03 00 41 62 63")
        ]
    );

    c.recv(
        &server,
        &frame(
            ct::CT_ITEM_UCC,
            &hex("05 00 00 00  02 00 00 00  01 00 00 00 00 00 00 00  02 00 00 00 00 00 00 00"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        c.drain(),
        vec![hex("0D 00  05 00 00 00  02 00 00 00
             01 00 00 00 00 00 00 00  0A 00 00 00 00 00 00 00
             02 00 00 00 00 00 00 00  14 00 00 00 00 00 00 00")]
    );
    // Unknown item: NullReferenceException while encoding in C#.
    assert!(c
        .recv(
            &server,
            &frame(
                ct::CT_ITEM_UCC,
                &hex("05 00 00 00  01 00 00 00  C8 00 00 00 00 00 00 00"),
            ),
        )
        .await
        .is_err());
}

// ---------------------------------------------------------------------------------------------
// UCC

#[tokio::test]
async fn ucc_queries() {
    let (server, mut c, _g) = joined(Arc::new(TestWorld::default())).await;
    server.streams.ucc.insert(custom_ucc(9, 10));
    for (op, id) in [
        (ct::CT_UCC_COMPLEX, 9u64),
        (ct::CT_UCC_COMPLEX, 8),
        (ct::CT_UCC_COMPLEX_CHECK_VALID, 9),
        (ct::CT_UCC_COMPLEX_CHECK_VALID, 8),
        (ct::CT_UCC_STRING, 9),
        (ct::CT_UCC_POSITION, 9),
    ] {
        c.recv(&server, &frame(op, &id.to_le_bytes()))
            .await
            .unwrap();
    }
    let modified = custom_ucc(9, 0).modified.unix_time();
    let mut complex = hex(
        "05 00  09 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00
         09 00 00 00 00 00 00 00",
    );
    complex.extend_from_slice(&modified.to_le_bytes());
    assert_eq!(
        c.drain(),
        vec![complex, hex("0F 00  09 00 00 00 00 00 00 00  00")]
    );
}

#[tokio::test]
async fn ucc_download_flow() {
    let (server, mut c, _g) = joined(Arc::new(TestWorld::default())).await;
    let ucc = custom_ucc(9, 3072 + 100);
    server.streams.ucc.insert(ucc.clone());
    let send_status = |status| enc(&TCEmblemStreamSendStatusPacket::new(&ucc, status));
    let chunk = |i| enc(&TCEmblemStreamDownloadPacket::new(&ucc, i).unwrap());

    // CTRequestEmblem -> SendStatus(Start) (custom with data)
    c.recv(&server, &frame(ct::CT_REQUEST_EMBLEM, &9u64.to_le_bytes()))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![send_status(EmblemStreamStatus::Start)]);
    // Same id again -> first block.
    c.recv(&server, &frame(ct::CT_REQUEST_EMBLEM, &9u64.to_le_bytes()))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![chunk(0)]);
    // Part 0 confirmed -> part 1; part 1 confirmed -> end status (Continue = 0) and dequeued.
    c.recv(
        &server,
        &frame(
            ct::CT_EMBLEM_PART_DOWNLOADED,
            &hex("00 00 00 00  00 0C 00 00"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![chunk(1)]);
    c.recv(
        &server,
        &frame(
            ct::CT_EMBLEM_PART_DOWNLOADED,
            &hex("01 00 00 00  64 00 00 00"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![send_status(EmblemStreamStatus::Continue)]);
    // Nothing queued any more: logged, no answer.
    c.recv(
        &server,
        &frame(
            ct::CT_EMBLEM_PART_DOWNLOADED,
            &hex("01 00 00 00  64 00 00 00"),
        ),
    )
    .await
    .unwrap();
    assert!(c.drain().is_empty());

    // DownloadStatus: 3 -> End, other -> Start, 4 with count 1 -> RequestUccPart(1) needs the
    // queue (empty now: nothing), 4 with count 2 -> RequestUcc.
    let status = |st: u8, count: i32| {
        let mut b = 9u64.to_le_bytes().to_vec();
        b.push(st);
        b.extend_from_slice(&count.to_le_bytes());
        frame(ct::CT_EMBLEM_STREAM_DOWNLOAD_STATUS, &b)
    };
    c.recv(&server, &status(3, 0)).await.unwrap();
    c.recv(&server, &status(1, 0)).await.unwrap();
    c.recv(&server, &status(4, 1)).await.unwrap();
    c.recv(&server, &status(4, 2)).await.unwrap();
    assert_eq!(
        c.drain(),
        vec![
            send_status(EmblemStreamStatus::End),
            send_status(EmblemStreamStatus::Start),
            send_status(EmblemStreamStatus::Start),
        ]
    );
    // Now queued: 4 with count 1 -> part index 2 is beyond the data -> end status.
    c.recv(&server, &status(4, 1)).await.unwrap();
    assert_eq!(c.drain(), vec![send_status(EmblemStreamStatus::Continue)]);
    // Unknown UCC: nothing.
    c.recv(&server, &frame(ct::CT_REQUEST_EMBLEM, &8u64.to_le_bytes()))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
}

#[tokio::test]
async fn ucc_simple_upload() {
    let (server, mut c, mut g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 60_000;
    let start = start_upload_body(PRINTER_OBJ, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  01  00 00 00 00")]);
    // A second start before confirming: Dictionary.Add throws in C#.
    assert!(c
        .recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .is_err());

    // Upload status -> ConfirmDefaultUcc: id 1, End, the row's 50000 taken (60000 -> 10000),
    // the Crest Ink is created, then the MySQL save fails (no database in tests), which closes
    // the connection like the uncaught C# exception: no ink in the bag.
    let res = c
        .recv(&server, &frame(ct::CT_EMBLEM_STREAM_UPLOAD_STATUS, &[1]))
        .await;
    assert!(res.is_err());
    assert_eq!(c.drain(), vec![hex("0A 00  02  00 00 00 00")]);
    // SCItemTaskSuccessPacket(DepositMoney = 83, [MoneyChange(-50000)], [])
    assert_eq!(
        game_frames(&mut g),
        vec![(
            crate::opcodes::sc::SCItemTaskSuccessPacket,
            hex("53  01  01 B0 3C FF FF  00  00 00 00 00")
        )]
    );
    assert_eq!(p.data().money, 60_000 - 50_000);
    assert!(bag_items(&p).is_empty());
    let stored = server.streams.ucc.get_ucc(1).unwrap();
    assert_eq!(stored.ty, UccType::Simple);
    assert_eq!(stored.uploader_id, 555);
    assert_eq!(stored.color3, [9, 10, 11]);
    // Nothing queued any more: ConfirmDefaultUcc throws KeyNotFoundException in C#.
    assert!(c
        .recv(&server, &frame(ct::CT_EMBLEM_STREAM_UPLOAD_STATUS, &[1]))
        .await
        .is_err());
}

#[tokio::test]
async fn ucc_custom_upload() {
    let (server, mut c, mut g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    give_item(&server, &p, ITEM_CREST_BRAINSTORM, 2);
    let start = start_upload_body(PRINTER_OBJ, 5);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);

    c.recv(
        &server,
        &frame(
            ct::CT_UPLOAD_EMBLEM_STREAM,
            &hex("05 00 00 00  03 00 00 00  00 00 00 00  03 00  AA BB CC"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
    // Last part completes the upload -> ConfirmDefaultUcc (save fails without MySQL).
    let res = c
        .recv(
            &server,
            &frame(
                ct::CT_UPLOAD_EMBLEM_STREAM,
                &hex("05 00 00 00  02 00 00 00  01 00 00 00  02 00  DD EE"),
            ),
        )
        .await;
    assert!(res.is_err());
    assert_eq!(c.drain(), vec![hex("0A 00  02  00 00 00 00")]);
    let stored = server
        .streams
        .ucc
        .get_ucc(1)
        .expect("custom UCC registered");
    assert_eq!(stored.ty, UccType::Complex);
    assert_eq!(stored.uploader_id, 555);
    assert_eq!(stored.data, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    // A custom image costs the reagent only: the row's 50000 is not charged on this path
    // (client `X2Ucc.GetMakeUccConsumeInfo` shows the item, not the money).
    let frames = game_frames(&mut g);
    assert_eq!(frames[0].0, crate::opcodes::sc::SCItemTaskSuccessPacket);
    assert_eq!(
        frames[0].1.first(),
        Some(&task_type::IMPRINT_UCC),
        "the reagent removal is an IMPRINT_UCC item task"
    );
    assert!(
        !frames.iter().any(
            |(op, body)| *op == crate::opcodes::sc::SCItemTaskSuccessPacket
                && body.first() == Some(&task_type::DEPOSIT_MONEY)
        ),
        "no money task on a custom upload"
    );
    // The save failed before the Crest Ink went to the bag, one reagent was spent (one is left
    // for the second upload) and the money is untouched.
    assert_eq!(p.data().money, 50_000);
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_BRAINSTORM,
            -1
        ),
        1
    );
    assert!(
        bag_items(&p)
            .iter()
            .all(|i| i.template_id == ITEM_CREST_BRAINSTORM),
        "no Crest Ink in the bag before the save succeeds"
    );

    // The upload finished: the per-connection upload state is gone, so a **second** custom
    // upload on the same stream connection starts instead of throwing (the C# never cleared
    // `_complexUploadParts` and closed the connection here).
    assert_eq!(server.streams.ucc.parts_len(), 0);
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_BRAINSTORM,
            -1
        ),
        1,
        "one reagent left for the second upload"
    );

    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
    c.recv(
        &server,
        &frame(
            ct::CT_UPLOAD_EMBLEM_STREAM,
            &hex("05 00 00 00  03 00 00 00  00 00 00 00  03 00  11 22 33"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
    // The second upload also completes and reaches its own ConfirmDefaultUcc / save step.
    let res = c
        .recv(
            &server,
            &frame(
                ct::CT_UPLOAD_EMBLEM_STREAM,
                &hex("05 00 00 00  02 00 00 00  01 00 00 00  02 00  44 55"),
            ),
        )
        .await;
    assert!(res.is_err());
    assert_eq!(c.drain(), vec![hex("0A 00  02  00 00 00 00")]);
    let stored = server
        .streams
        .ucc
        .get_ucc(2)
        .expect("second custom UCC registered");
    assert_eq!(stored.ty, UccType::Complex);
    assert_eq!(stored.uploader_id, 555);
    assert_eq!(stored.data, vec![0x11, 0x22, 0x33, 0x44, 0x55]);
    assert_eq!(server.streams.ucc.parts_len(), 0);
    assert_eq!(server.streams.ucc.queue_len(), 0);

    // One charge per upload is proven by the reagent balance (2 -> 1 -> 0, asserted above and
    // below), not by counting frames: the first upload's frame was drained earlier, and emptying
    // the stack in the second upload sends two IMPRINT_UCC tasks (the removed slot, then the empty
    // list `consume_item_filtered` always sends). No money task.
    let frames = game_frames(&mut g);
    assert!(
        !frames.iter().any(
            |(op, body)| *op == crate::opcodes::sc::SCItemTaskSuccessPacket
                && body.first() == Some(&task_type::DEPOSIT_MONEY)
        ),
        "no money task on a custom upload"
    );
    assert_eq!(p.data().money, 50_000);
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_BRAINSTORM,
            -1
        ),
        0,
        "one reagent per upload"
    );
}

/// `CTEmblemStreamUploadStatus` (`ConfirmDefaultUcc`) during a custom upload must not charge
/// the reagent and save an empty Complex UCC: the interleaved packet order is refused and the
/// in-progress upload is left untouched.
#[tokio::test]
async fn ucc_default_confirm_during_a_custom_upload_is_refused() {
    let (server, mut c, mut g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    give_item(&server, &p, ITEM_CREST_BRAINSTORM, 1);
    let start = start_upload_body(PRINTER_OBJ, 5);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
    c.recv(
        &server,
        &frame(
            ct::CT_UPLOAD_EMBLEM_STREAM,
            &hex("05 00 00 00  03 00 00 00  00 00 00 00  03 00  AA BB CC"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);

    let res = c
        .recv(&server, &frame(ct::CT_EMBLEM_STREAM_UPLOAD_STATUS, &[1]))
        .await;
    assert!(res.is_err(), "the confirm is refused");
    assert!(server.streams.ucc.is_empty(), "no UCC saved");
    assert_eq!(p.data().money, 50_000);
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_BRAINSTORM,
            -1
        ),
        1,
        "the reagent is not charged"
    );
    assert!(game_frames(&mut g).is_empty(), "no item task sent");
    assert_eq!(
        server.streams.ucc.parts_len(),
        1,
        "upload still in progress"
    );
    assert_eq!(server.streams.ucc.queue_len(), 1);
}

/// The MySQL half of [`ucc_custom_upload`]: with a real database both custom uploads on the
/// same stream connection save, so the client gets `End` twice and two `uccs` rows exist.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses a temporary uccs table"]
async fn ucc_two_custom_uploads_on_one_connection_are_saved() {
    use crate::systems::social::achievement_persistence::temporary_shadow_definition;
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
    let (_, definition): (String, String) = sqlx::query_as("SHOW CREATE TABLE `uccs`")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(&temporary_shadow_definition(&definition))
        .execute(&pool)
        .await
        .unwrap();

    let (server, mut c, mut g) = joined_printer_with(
        test_server_with_db(test_config(), pool.clone()),
        printer_3038(),
    )
    .await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    give_item(&server, &p, ITEM_CREST_BRAINSTORM, 2);
    let start = start_upload_body(PRINTER_OBJ, 5);
    for (first, last) in [
        (
            "05 00 00 00  03 00 00 00  00 00 00 00  03 00  AA BB CC",
            "05 00 00 00  02 00 00 00  01 00 00 00  02 00  DD EE",
        ),
        (
            "05 00 00 00  03 00 00 00  00 00 00 00  03 00  11 22 33",
            "05 00 00 00  02 00 00 00  01 00 00 00  02 00  44 55",
        ),
    ] {
        c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
            .await
            .expect("start upload");
        assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
        c.recv(&server, &frame(ct::CT_UPLOAD_EMBLEM_STREAM, &hex(first)))
            .await
            .expect("first part");
        assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
        c.recv(&server, &frame(ct::CT_UPLOAD_EMBLEM_STREAM, &hex(last)))
            .await
            .expect("last part saves");
        assert_eq!(c.drain(), vec![hex("0A 00  02  00 00 00 00")]);
    }
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), CAST(uploader_id AS SIGNED) FROM uccs ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], (1, 555));
    assert_eq!(rows[1], (2, 555));
    assert_eq!(
        server.streams.ucc.get_ucc(2).expect("second UCC").data,
        vec![0x11, 0x22, 0x33, 0x44, 0x55]
    );
    // Two saved uploads: one reagent and one Crest Ink each, no money.
    assert_eq!(p.data().money, 50_000);
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_BRAINSTORM,
            -1
        ),
        0
    );
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_INK,
            -1
        ),
        2,
        "one Crest Ink gained per upload"
    );
    let frames = game_frames(&mut g);
    assert_eq!(
        frames
            .iter()
            .filter(
                |(op, body)| *op == crate::opcodes::sc::SCItemTaskSuccessPacket
                    && body.first() == Some(&task_type::GAIN_ITEM_WITH_UCC)
            )
            .count(),
        2,
        "one Crest Ink item task per upload"
    );
    assert!(
        !frames.iter().any(
            |(op, body)| *op == crate::opcodes::sc::SCItemTaskSuccessPacket
                && body.first() == Some(&task_type::DEPOSIT_MONEY)
        ),
        "no money task on a custom upload"
    );
    pool.close().await;
}

/// The frames a game connection received, as (opcode, body) (level-1 frames only).
fn game_frames(g: &mut GameClient) -> Vec<(u16, Vec<u8>)> {
    let mut v = Vec::new();
    while let Ok(f) = g.1.try_recv() {
        assert_eq!(f[1], 1, "level-1 frame");
        v.push((u16::from_le_bytes([f[4], f[5]]), f[6..].to_vec()));
    }
    v
}

fn bag_items(p: &crate::world::PlayerRef) -> Vec<crate::models::item::Item> {
    let bag = crate::systems::economy::inventory::Inv::of(p).bag();
    let items = bag.lock().unwrap().items.clone();
    items
}

#[tokio::test]
async fn ucc_confirm_crest_ink_in_csharp_order() {
    use super::ucc::{add_crest_ink, charge_and_create_crest_ink};
    use crate::models::item_task::task_type;
    let world = Arc::new(TestWorld::default());
    let (server, _c, mut g) = joined(world).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    let cost = CrestPrinterCost {
        consume_money: 50_000,
        consume_item_id: ITEM_CREST_BRAINSTORM,
        consume_count: 1,
    };

    // Before Save(ucc): the money task is sent at once and the ink exists (not in the bag).
    let item =
        charge_and_create_crest_ink(&server, &p, 42, &cost, false).expect("crest ink created");
    assert_eq!(item.template_id, ITEM_CREST_INK);
    assert_eq!(item.ucc_id, 42);
    assert_eq!(item.count, 1);
    assert_ne!(item.id, 0);
    assert_eq!(
        game_frames(&mut g),
        vec![(
            crate::opcodes::sc::SCItemTaskSuccessPacket,
            hex("53  01  01 B0 3C FF FF  00  00 00 00 00")
        )]
    );
    assert!(bag_items(&p).is_empty());

    // After Save(ucc): Bag.AddOrMoveExistingItem(GainItemWithUcc, ink).
    let id = item.id;
    add_crest_ink(&server, &p, item);
    let bag = bag_items(&p);
    let ink = bag.iter().find(|i| i.id == id).expect("ink in the bag");
    assert_eq!(ink.ucc_id, 42);
    assert_eq!(ink.slot, 0);
    let frames = game_frames(&mut g);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, crate::opcodes::sc::SCItemTaskSuccessPacket);
    assert_eq!(frames[0].1[0], task_type::GAIN_ITEM_WITH_UCC);
    assert_eq!(frames[0].1[1], 1); // one ItemAdd task
}

// ---------------------------------------------------------------------------------------------
// WP-U1: crest printer upload validation (housing-farm-ucc#1)

/// (a) A live doodad with no `StampMaker` func in its current group is not a crest printer: the
/// packet is dropped with no reply, no queue entry and no charge.
#[tokio::test]
async fn crest_upload_refuses_a_plain_doodad() {
    let (server, mut c, _g) = joined_printer(printer(PRINTER_OBJ, 1470, 9999)).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    let start = start_upload_body(PRINTER_OBJ, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
    assert_eq!(p.data().money, 50_000);
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(server.streams.ucc.parts_len(), 0);
    assert!(!c.conn.is_closing());
}

/// (a) An unknown `bc` (no live doodad in the player's instance) is refused the same way.
#[tokio::test]
async fn crest_upload_refuses_an_unknown_doodad() {
    let (server, mut c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    let start = start_upload_body(0x00_9999, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
    assert_eq!(p.data().money, 50_000);
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(server.streams.ucc.parts_len(), 0);
}

/// Rule 2: the printer must be within the func-skill reach (14435 + 15 m). At 100 m it is not.
#[tokio::test]
async fn crest_upload_refuses_an_out_of_reach_printer() {
    let mut far = printer_3038();
    far.x = 102.0;
    let (server, mut c, _g) = joined_printer(far).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    let start = start_upload_body(PRINTER_OBJ, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
    assert_eq!(p.data().money, 50_000);
    assert_eq!(server.streams.ucc.queue_len(), 0);
}

/// (b) 3038 with less than the row's 50000: refused, nothing charged, no queue entry.
#[tokio::test]
async fn crest_upload_refuses_3038_without_the_money() {
    let (server, mut c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 49_999;
    let start = start_upload_body(PRINTER_OBJ, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
    assert_eq!(p.data().money, 49_999);
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(server.streams.ucc.parts_len(), 0);
}

/// (c) 3038 with the money: the row's 50000 (not the old constant) is charged and the UCC is
/// registered with the new id; the row's reagent is not touched on a default upload.
#[tokio::test]
async fn crest_upload_3038_charges_the_row_money() {
    let (server, mut c, mut g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    give_item(&server, &p, ITEM_CREST_BRAINSTORM, 1);
    let start = start_upload_body(PRINTER_OBJ, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  01  00 00 00 00")]);
    let res = c
        .recv(&server, &frame(ct::CT_EMBLEM_STREAM_UPLOAD_STATUS, &[1]))
        .await;
    assert!(res.is_err()); // the MySQL save fails in tests, as before
    assert_eq!(c.drain(), vec![hex("0A 00  02  00 00 00 00")]);
    assert_eq!(p.data().money, 0);
    assert_eq!(
        game_frames(&mut g),
        vec![(
            crate::opcodes::sc::SCItemTaskSuccessPacket,
            hex("53  01  01 B0 3C FF FF  00  00 00 00 00")
        )]
    );
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_BRAINSTORM,
            -1
        ),
        1,
        "a default pattern charges no reagent"
    );
    assert_eq!(server.streams.ucc.get_ucc(1).unwrap().ty, UccType::Simple);
}

/// (d) A custom upload without the row's reagent is refused before any queue entry.
#[tokio::test]
async fn crest_custom_upload_requires_the_reagent() {
    let (server, mut c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    let start = start_upload_body(PRINTER_OBJ, 5);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
    assert_eq!(p.data().money, 50_000);
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(server.streams.ucc.parts_len(), 0);
}

/// (d) With the reagent: it is consumed exactly once, the ink carries the new UCC id and the
/// custom path charges **no money** (it succeeds with an empty purse). A second call without
/// the reagent changes nothing.
#[tokio::test]
async fn crest_custom_reagent_is_consumed_once_with_the_ink() {
    use super::ucc::{add_crest_ink, charge_and_create_crest_ink};
    let (server, _c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 0;
    give_item(&server, &p, ITEM_CREST_BRAINSTORM, 1);
    let cost = CrestPrinterCost {
        consume_money: 50_000,
        consume_item_id: ITEM_CREST_BRAINSTORM,
        consume_count: 1,
    };
    let item = charge_and_create_crest_ink(&server, &p, 7, &cost, true).expect("crest ink");
    assert_eq!(p.data().money, 0, "a custom image never takes money");
    assert_eq!(
        crate::systems::economy::inventory::Inv::of(&p).items_count_in_unlocked(
            crate::models::container::slot_type::INVENTORY,
            ITEM_CREST_BRAINSTORM,
            -1
        ),
        0
    );
    assert_eq!(item.ucc_id, 7);
    add_crest_ink(&server, &p, item);
    let ink = bag_items(&p)
        .into_iter()
        .find(|i| i.ucc_id == 7)
        .expect("ink in the bag");
    assert_eq!(ink.template_id, ITEM_CREST_INK);

    // Without the reagent again: refused before any charge.
    let money = p.data().money;
    assert!(charge_and_create_crest_ink(&server, &p, 8, &cost, true).is_none());
    assert_eq!(p.data().money, money);
}

/// Rule 5: the confirm re-checks the mutable state. Money spent between the start and the
/// confirm drops the queued upload without a reply, registration or charge.
#[tokio::test]
async fn crest_confirm_rechecks_the_balance() {
    let (server, mut c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    let start = start_upload_body(PRINTER_OBJ, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  01  00 00 00 00")]);
    p.data().money = 0;
    let res = c
        .recv(&server, &frame(ct::CT_EMBLEM_STREAM_UPLOAD_STATUS, &[1]))
        .await;
    assert!(res.is_ok(), "a refusal is not a connection error");
    assert!(c.drain().is_empty(), "no End / failure status (UNKNOWN)");
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(server.streams.ucc.len(), 0, "the UCC is not registered");
    assert_eq!(p.data().money, 0);
}

/// (e) The 1470 printer row charges its own 10000, not 50000.
#[tokio::test]
async fn crest_upload_1470_charges_its_row_money() {
    let (server, mut c, mut g) = joined_printer(printer_1470()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 10_000;
    let start = start_upload_body(PRINTER_OBJ, 0);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  01  00 00 00 00")]);
    let res = c
        .recv(&server, &frame(ct::CT_EMBLEM_STREAM_UPLOAD_STATUS, &[1]))
        .await;
    assert!(res.is_err());
    assert_eq!(p.data().money, 0);
    // SCItemTaskSuccessPacket(DepositMoney = 83, [MoneyChange(-10000)], [])
    assert_eq!(
        game_frames(&mut g),
        vec![(
            crate::opcodes::sc::SCItemTaskSuccessPacket,
            hex("53  01  01 F0 D8 FF FF  00  00 00 00 00")
        )]
    );
}

/// (f) A full bag leaves the new ink in `_allItems` (the C# `Orphaned` path), unchanged.
#[tokio::test]
async fn crest_ink_is_orphaned_when_the_bag_is_full() {
    use super::ucc::{add_crest_ink, charge_and_create_crest_ink};
    let (server, _c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    for _ in 0..50 {
        give_item(&server, &p, ITEM_CREST_BRAINSTORM, 1);
    }
    assert_eq!(bag_items(&p).len(), 50);
    let cost = CrestPrinterCost {
        consume_money: 0,
        consume_item_id: 0,
        consume_count: 0,
    };
    let item = charge_and_create_crest_ink(&server, &p, 42, &cost, false).expect("crest ink");
    let id = item.id;
    add_crest_ink(&server, &p, item);
    assert_eq!(bag_items(&p).len(), 50, "the bag is still full");
    assert!(
        server.items.item_by_id(id).is_some(),
        "the orphaned ink stays in _allItems"
    );
}

/// The shipped rows resolve to the expected cost (3038 groups 6944 / 22729, 1470 group 2260);
/// the Insignia Drafting Table (group 5689) is a `StampMaker` but not a crest printer.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_stamp_maker_rows_resolve_to_expected_cost() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = DoodadData::load_from(&conn).unwrap();
    let crest = crest_printer_cost(&data, 6944).unwrap();
    assert_eq!(
        crest,
        CrestPrinterCost {
            consume_money: 50_000,
            consume_item_id: ITEM_CREST_BRAINSTORM,
            consume_count: 1,
        }
    );
    assert_eq!(crest_printer_cost(&data, 22729), Some(crest));
    assert_eq!(
        crest_printer_cost(&data, 2260),
        Some(CrestPrinterCost {
            consume_money: 10_000,
            consume_item_id: 0,
            consume_count: 1,
        })
    );
    // Item 16169 ("Insignia Stamp") is not Crest Ink.
    assert_eq!(crest_printer_cost(&data, 5689), None);
}

// ---------------------------------------------------------------------------------------------
// Socket level

#[tokio::test]
async fn socket_round_trip() {
    let server = Arc::new(test_server());
    let _g = game_connection(&server, 77);
    server.streams.add_token_with_ticket(1000, 77, 77);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(super::listener::serve_listener(server.clone(), listener));

    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    // CTJoin split across two writes (reassembly).
    let join = |cookie: u32| {
        bytes(|s| {
            s.write_u16(10)
                .write_u16(ct::CT_JOIN)
                .write_u32(1000)
                .write_u32(cookie);
        })
    };
    let good = join(77);
    sock.write_all(&good[..5]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    sock.write_all(&good[5..]).await.unwrap();

    let mut buf = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buf, [0x03, 0x00, 0x01, 0x00, 0x00]);
    assert_eq!(server.streams.connections.len(), 1);

    // An unknown packet after the join is ignored (C# HandleUnknownPacket).
    sock.write_all(&hex("03 00  0B 00  FF")).await.unwrap();

    // Bad token -> 1, connection kept.
    sock.write_all(&join(99)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buf, [0x03, 0x00, 0x01, 0x00, 0x01]);

    // A truncated body closes the connection and removes it from the table.
    sock.write_all(&hex("03 00  01 00  FF")).await.unwrap();
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0);
    for _ in 0..50 {
        if server.streams.connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server.streams.connections.is_empty());
    task.abort();
}

#[tokio::test]
async fn socket_unknown_packet_before_join_closes() {
    let server = Arc::new(test_server());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(super::listener::serve_listener(server.clone(), listener));

    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(&hex("03 00  0B 00  FF")).await.unwrap();
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0);
    for _ in 0..50 {
        if server.streams.connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server.streams.connections.is_empty());
    task.abort();
}

#[tokio::test]
async fn socket_closes_connections_that_never_join() {
    let server = Arc::new(test_server());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(super::listener::serve_listener_with(
        server.clone(),
        listener,
        Duration::from_millis(100),
    ));

    // A silent peer, and one stuck in the middle of a frame.
    let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut partial = tokio::net::TcpStream::connect(addr).await.unwrap();
    partial.write_all(&hex("2F 00  02 00")).await.unwrap();
    for sock in [&mut idle, &mut partial] {
        let mut rest = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
    }
    task.abort();
}

// ---------------------------------------------------------------------------------------------
// Hardening (Rust addition)

/// Before `CTJoin` only `CTJoin` is served: registered packets close the connection too.
#[tokio::test]
async fn registered_packets_before_join_close_the_connection() {
    let server = test_server();
    server.streams.ucc.insert(custom_ucc(9, 10));
    for (opcode, body) in [
        (
            ct::CT_REQUEST_CELL,
            hex("00 00 00 00  02 00 00 00  03 00 00 00"),
        ),
        (ct::CT_REQUEST_EMBLEM, 9u64.to_le_bytes().to_vec()),
        (ct::CT_QUERY_CHAR_NAME, hex("07 00 00 00")),
        (ct::CT_UCC_COMPLEX_CHECK_VALID, 9u64.to_le_bytes().to_vec()),
    ] {
        let mut s = stream_client(&server, 900);
        s.recv(&server, &frame(opcode, &body)).await.unwrap();
        assert!(s.conn.is_closing(), "opcode {opcode:#x}");
        assert!(s.drain().is_empty(), "opcode {opcode:#x}");
    }
}

/// A join attempt with the owner's account but a wrong ticket (or the other way round) neither
/// joins nor invalidates the real ticket.
#[tokio::test]
async fn a_foreign_join_leaves_the_owners_ticket_usable() {
    let server = test_server();
    let _g = game_connection(&server, 77);
    let ticket = server.streams.add_token(1000, 77).expect("ticket");
    let mut attacker = stream_client(&server, 901);
    for (account, guess) in [(5, ticket), (1000, ticket ^ 1), (1000, 77)] {
        attacker
            .recv(&server, &join_frame(account, guess))
            .await
            .unwrap();
        assert_eq!(attacker.drain(), vec![hex("01 00 01")]);
        assert!(!attacker.conn.is_joined());
    }
    let mut owner = stream_client(&server, 902);
    owner
        .recv(&server, &join_frame(1000, ticket))
        .await
        .unwrap();
    assert_eq!(owner.drain(), vec![hex("01 00 00")]);
    assert_eq!(owner.conn.game_connection_id, Some(77));
}

/// A custom upload declaring more than the cap is refused before anything is queued.
#[tokio::test]
async fn an_oversize_custom_upload_is_refused() {
    let (server, mut c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    give_item(&server, &p, ITEM_CREST_BRAINSTORM, 1);
    let start = start_upload_body(PRINTER_OBJ, crate::stream::models::MAX_UCC_UPLOAD_BYTES + 1);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert!(c.drain().is_empty());
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(server.streams.ucc.parts_len(), 0);
    // The cap itself is allowed.
    let start = start_upload_body(PRINTER_OBJ, crate::stream::models::MAX_UCC_UPLOAD_BYTES);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
    assert_eq!(server.streams.ucc.parts_len(), 1);
}

/// A part that carries the upload past its declared size drops the upload and the connection.
#[tokio::test]
async fn a_part_beyond_the_declared_size_drops_the_upload() {
    let (server, mut c, _g) = joined_printer(printer_3038()).await;
    let p = server.world.character_by_connection(77).unwrap();
    p.data().money = 50_000;
    give_item(&server, &p, ITEM_CREST_BRAINSTORM, 1);
    let start = start_upload_body(PRINTER_OBJ, 5);
    c.recv(&server, &frame(ct::CT_START_UPLOAD_EMBLEM_STREAM, &start))
        .await
        .unwrap();
    assert_eq!(c.drain(), vec![hex("0A 00  00 00 00 00 00")]);
    assert_eq!(server.streams.ucc.parts_len(), 1);
    let res = c
        .recv(
            &server,
            &frame(
                ct::CT_UPLOAD_EMBLEM_STREAM,
                &hex("05 00 00 00  06 00 00 00  00 00 00 00  06 00  01 02 03 04 05 06"),
            ),
        )
        .await;
    assert!(res.is_err());
    assert_eq!(server.streams.ucc.queue_len(), 0);
    assert_eq!(server.streams.ucc.parts_len(), 0);
}

/// Pending upload / download state does not outlive the stream connection.
#[tokio::test]
async fn ucc_state_is_dropped_when_the_stream_connection_closes() {
    use tokio::io::AsyncWriteExt;

    let (server, _c, _g) = joined(Arc::new(TestWorld::default())).await;
    server.streams.ucc.insert(custom_ucc(9, 10));
    let server = Arc::new(server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(super::listener::serve_listener(server.clone(), listener));

    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut out = bytes(|s| {
        s.write_u16(10)
            .write_u16(ct::CT_JOIN)
            .write_u32(1000)
            .write_u32(77);
    });
    out.extend(bytes(|s| {
        s.write_u16(10)
            .write_u16(ct::CT_REQUEST_EMBLEM)
            .write_u64(9);
    }));
    sock.write_all(&out).await.unwrap();
    for _ in 0..100 {
        if server.streams.ucc.download_queue_len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(server.streams.ucc.download_queue_len(), 1);

    drop(sock);
    for _ in 0..100 {
        if server.streams.ucc.download_queue_len() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(server.streams.ucc.download_queue_len(), 0);
    task.abort();
}

/// The live cell requests of one connection are capped; the oldest makes room.
#[tokio::test]
async fn live_cell_requests_are_capped_per_connection() {
    use crate::stream::connection::MAX_LIVE_CELL_REQUESTS;

    let server = test_server();
    let mut s = stream_client(&server, 900);
    let first = s.conn.next_request_id(vec![doodad(1)]);
    let mut last = first;
    for _ in 0..(MAX_LIVE_CELL_REQUESTS * 3) {
        last = s.conn.next_request_id(vec![doodad(1)]);
    }
    assert_eq!(s.conn.request_count(), MAX_LIVE_CELL_REQUESTS);
    assert!(s.conn.request(first).is_none());
    assert!(s.conn.request(last).is_some());
}
