use super::split_quoted;

#[test]
fn chat_and_trade_cancel_readers_decode_the_server_writers() {
    use crate::game::LEVEL_GAME;
    use aaemu_commons::{Marshal, PacketStream};
    use aaemu_game::packets::world::{ChatSender, SCChatMessagePacket};
    use aaemu_game::systems::economy::trade::SCTradeCanceledPacket;

    let mut body = PacketStream::new();
    SCChatMessagePacket {
        chat_type: 2,
        sender: Some(ChatSender {
            faction_id: 148,
            obj_id: 0x100,
            id: 7,
            race: 1,
            name: "Al".into(),
            gm: false,
        }),
        message: "trade".into(),
        ability: 3,
        language_type: 0,
    }
    .write(&mut body);
    let p = super::Packet {
        level: LEVEL_GAME,
        opcode: aaemu_game::opcodes::sc::SCChatMessagePacket,
        body: body.into_bytes(),
    };
    assert_eq!(
        super::chat_type_and_text(&p),
        Some((2, "trade".to_string()))
    );

    let mut body = PacketStream::new();
    SCTradeCanceledPacket {
        reason: 2,
        caused_by_me: false,
    }
    .write(&mut body);
    let p = super::Packet {
        level: LEVEL_GAME,
        opcode: aaemu_game::opcodes::sc::SCTradeCanceledPacket,
        body: body.into_bytes(),
    };
    assert_eq!(super::trade_cancel_reason(&p), Some((2, false)));
    assert_eq!(
        super::trade_cancel_reason(&super::Packet {
            level: LEVEL_GAME,
            opcode: aaemu_game::opcodes::sc::SCChatMessagePacket,
            body: Vec::new(),
        }),
        None
    );
}

#[test]
fn quoted_arguments_keep_spaces_and_escapes() {
    assert_eq!(
        split_quoted(" \"SELECT COUNT(*) FROM t WHERE name = 'a b'\" 1 ").unwrap(),
        vec!["SELECT COUNT(*) FROM t WHERE name = 'a b'", "1"]
    );
    assert_eq!(
        split_quoted(r#""a \"b\"" x"#).unwrap(),
        vec![r#"a "b""#, "x"]
    );
    assert_eq!(
        split_quoted(r#""object \d+ ignored""#).unwrap(),
        vec![r"object \d+ ignored"]
    );
    assert!(split_quoted("\"unterminated").is_err());
}

#[test]
fn complete_quest_body_writes_the_doodad_object_id() {
    use aaemu_commons::PacketStream;
    use aaemu_game::net::registry::ClientPacket;
    use aaemu_game::systems::quests::packets::CSCompleteQuestContextPacket;

    let body = super::complete_quest_body(2563, 0, 0x300, 1);
    assert_eq!(
        body,
        vec![0x03, 0x0a, 0, 0, 0, 0, 0, 0, 0x03, 0, 1, 0, 0, 0]
    );
    // The server's reader must see the doodad object id, not an NPC object id.
    let p = CSCompleteQuestContextPacket::read(&mut PacketStream::from_bytes(body)).unwrap();
    assert_eq!(
        p,
        CSCompleteQuestContextPacket {
            quest_context_id: 2563,
            npc_obj_id: 0,
            doodad_obj_id: 0x300,
            selected: 1,
        }
    );

    let body = super::complete_quest_body(100, 0x200, 0, -1);
    assert_eq!(
        body,
        vec![100, 0, 0, 0, 0x00, 0x02, 0x00, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]
    );
    let p = CSCompleteQuestContextPacket::read(&mut PacketStream::from_bytes(body)).unwrap();
    assert_eq!((p.npc_obj_id, p.doodad_obj_id, p.selected), (0x200, 0, -1));
}

#[test]
fn cexec_arguments_parse() {
    assert_eq!(
        super::parse_craft_args(&["1", "2", "3"]).unwrap(),
        (1, 2, 3)
    );
    assert!(super::parse_craft_args(&["1", "2"]).is_err());
    assert!(super::parse_craft_args(&["x", "2", "3"]).is_err());
}

#[test]
fn execute_craft_body_matches_the_client_writer() {
    // `FUN_397bbfc0`: u32 craftId, bc doodadObjId, i32 count.
    assert_eq!(
        super::execute_craft_body(987, 0x300, 1),
        vec![0xdb, 0x03, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]
    );
}

#[test]
fn read_mail_body_matches_the_client_writer() {
    // `FUN_397ba350`: bool isSent, i64 mailId.
    assert_eq!(
        super::read_mail_body(false, 0x0102_0304_0506_0708),
        vec![0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
    );
    assert_eq!(
        super::read_mail_body(true, -2),
        vec![0x01, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
    );
}

#[test]
fn auction_post_body_matches_the_client_writer() {
    // `FUN_397bae70`: bc, bc, u64 itemId, i32 startPrice, i32 buyoutPrice, u8 duration.
    assert_eq!(
        super::auction_post_body(0x300, 0, 0x0102_0304_0506_0708, 10, 20, 2),
        vec![
            0x00, 0x03, 0x00, // auctioneerId
            0x00, 0x00, 0x00, // auctioneerId2
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // itemId
            0x0a, 0x00, 0x00, 0x00, // startPrice
            0x14, 0x00, 0x00, 0x00, // buyoutPrice
            0x02, // duration
        ]
    );
}

#[test]
fn auction_bid_body_matches_the_client_writer() {
    // `FUN_397bb050` + `FUN_398a6de0` (AuctionDisplay) + `FUN_398a6880` (AuctionBid).
    assert_eq!(
        super::auction_bid_body(0x300, 0, 0x1122_3344_5566_7788, 0x0a, "Bob", 42),
        vec![
            0x00, 0x03, 0x00, // auctioneerId
            0x00, 0x00, 0x00, // auctioneerId2
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // display.id
            0x00, // display.duration
            0x00, 0x00, 0x00, 0x00, // display.item templateId 0
            0x00, // display.worldId
            0x00, 0x00, 0x00, 0x00, // display.clientId
            0x00, 0x00, // display.clientName ""
            0x00, 0x00, 0x00, 0x00, // display.startMoney
            0x00, 0x00, 0x00, 0x00, // display.directMoney
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // display.postDate
            0x00, // display.bidWorldId
            0x00, 0x00, 0x00, 0x00, // display.bidderId
            0x00, 0x00, // display.bidderName ""
            0x00, 0x00, 0x00, 0x00, // display.bidMoney
            0x00, 0x00, 0x00, 0x00, // display.extra
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // bid.lotId
            0x00, // bid.worldId
            0x0a, 0x00, 0x00, 0x00, // bid.bidderId
            0x03, 0x00, b'B', b'o', b'b', // bid.bidderName
            0x2a, 0x00, 0x00, 0x00, // bid.money
        ]
    );
}

#[test]
fn mail_header_decodes_the_client_reader_layout() {
    // `FUN_398a5a00`: i64, u8, u8, string, string, u8, string, i64, bool, i64.
    let bytes = vec![
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // mailId
        0x03, // type
        0x01, // status
        0x02, 0x00, b'h', b'i', // title
        0x02, 0x00, b'A', b'l', // senderName
        0x02, // attachments
        0x02, 0x00, b'B', b'o', // receiverName
        0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // openDate
        0x01, // returned
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // extra -1
    ];
    let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
    let h = super::decode_mail_header(&mut s).unwrap();
    assert_eq!(s.remaining(), 0);
    assert_eq!(
        h,
        super::MailHeaderView {
            mail_id: 0x0102_0304_0506_0708,
            mail_type: 3,
            status: 1,
            title: "hi".into(),
            sender_name: "Al".into(),
            attachments: 2,
            receiver_name: "Bo".into(),
            open_date: 0x11,
            returned: true,
            extra: -1,
        }
    );
}

#[test]
fn mail_body_decodes_the_client_reader_layout() {
    // `FUN_398a5e60`: i64, u8, string, string, string, i32 x3, i64 x3, ten Items.
    let mut bytes = vec![
        0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mailId
        0x01, // type
        0x02, 0x00, b'B', b'o', // receiverName
        0x01, 0x00, b'T', // title
        0x01, 0x00, b'x', // text
        0x01, 0x00, 0x00, 0x00, // copperCoins
        0x02, 0x00, 0x00, 0x00, // billingAmount
        0x03, 0x00, 0x00, 0x00, // moneyAmount2
        0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // sendDate
        0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // recvDate
        0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // openDate
        // attachment 0: `Item` (`FUN_3961f500`) with detail type 0.
        0xd2, 0x04, 0x00, 0x00, // templateId 1234
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // id
        0x05, // grade
        0x00, // flags
        0x07, 0x00, 0x00, 0x00, // stackSize
        0x00, // detailType
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // creationTime
        0x00, 0x00, 0x00, 0x00, // lifespanMins
        0x00, 0x00, 0x00, 0x00, // madeUnitId
        0x00, // worldId
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // unsecureDateTime
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // unpackDateTime
    ];
    // Attachments 1..9 are empty items (`templateId 0`, no body).
    bytes.extend(std::iter::repeat_n(0, 9 * 4));
    let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
    let b = super::decode_mail_body(&mut s).unwrap();
    assert_eq!(s.remaining(), 0);
    assert_eq!(
        b,
        super::MailBodyView {
            mail_id: 5,
            mail_type: 1,
            receiver_name: "Bo".into(),
            title: "T".into(),
            text: "x".into(),
            copper_coins: 1,
            billing_amount: 2,
            money_amount2: 3,
            send_date: 10,
            recv_date: 11,
            open_date: 12,
            items: {
                let mut items = vec![(1234, 0x0102_0304_0506_0708, 5, 7)];
                items.extend(std::iter::repeat_n((0, 0, 0, 0), 9));
                items
            },
        }
    );
}

#[test]
fn auction_lot_decodes_the_client_reader_layout() {
    // `FUN_398a6de0`: u64, u8, Item, u8, u32, string, i32 x2, i64, u8, u32, string,
    // i32 x2.
    let bytes = vec![
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // id
        0x02, // duration
        0x00, 0x00, 0x00, 0x00, // item templateId 0
        0x03, // worldId
        0x0d, 0x0c, 0x0b, 0x0a, // clientId
        0x01, 0x00, b'S', // clientName
        0x64, 0x00, 0x00, 0x00, // startMoney
        0xc8, 0x00, 0x00, 0x00, // directMoney
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // postDate
        0x04, // bidWorldId
        0x11, 0x00, 0x00, 0x00, // bidderId
        0x01, 0x00, b'B', // bidderName
        0x32, 0x00, 0x00, 0x00, // bidMoney
        0xfe, 0xff, 0xff, 0xff, // extra -2
    ];
    let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
    let lot = super::decode_auction_lot(&mut s).unwrap();
    assert_eq!(s.remaining(), 0);
    assert_eq!(
        lot,
        super::AuctionLotView {
            id: 0x1122_3344_5566_7788,
            duration: 2,
            item_template: 0,
            item_id: 0,
            world_id: 3,
            client_id: 0x0a0b_0c0d,
            client_name: "S".into(),
            start_money: 100,
            direct_money: 200,
            post_date: 0x0102_0304_0506_0708,
            bid_world_id: 4,
            bidder_id: 0x11,
            bidder_name: "B".into(),
            bid_money: 50,
            extra: -2,
        }
    );
}

#[test]
fn auction_bid_decodes_the_client_reader_layout() {
    // `FUN_398a6880`: u64 lotId, u8 worldId, u32 bidderId, string bidderName, i32 money.
    let bytes = vec![
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // lotId
        0x04, // worldId
        0x09, 0x00, 0x00, 0x00, // bidderId
        0x03, 0x00, b'B', b'i', b'd', // bidderName
        0x2a, 0x00, 0x00, 0x00, // money
    ];
    let mut s = aaemu_commons::PacketStream::from_bytes(bytes);
    let bid = super::decode_auction_bid(&mut s).unwrap();
    assert_eq!(s.remaining(), 0);
    assert_eq!(
        bid,
        super::AuctionBidView {
            lot_id: 0x1122_3344_5566_7788,
            world_id: 4,
            bidder_id: 9,
            bidder_name: "Bid".into(),
            money: 42,
        }
    );
}

#[test]
fn auction_message_decodes_the_client_reader_layout() {
    // `FUN_397b56f0`: u8 msgType, u32 itemType, i32 moneyAmount.
    let mut s = aaemu_commons::PacketStream::from_bytes(vec![
        0x01, 0xd2, 0x04, 0x00, 0x00, 0x2a, 0x00, 0x00, 0x00,
    ]);
    assert_eq!(
        super::decode_auction_message(&mut s).unwrap(),
        (1, 1234, 42)
    );
    assert_eq!(s.remaining(), 0);
}

#[test]
fn craft_unlock_and_failure_decode_the_client_reader_layouts() {
    // `FUN_397b4c60`: two u8s.
    let mut s = aaemu_commons::PacketStream::from_bytes(vec![0x01, 0x02]);
    assert_eq!(super::decode_craft_unlock(&mut s).unwrap(), (1, 2));
    assert_eq!(s.remaining(), 0);

    // `FUN_397b8110`: u32 id, i32 count, count u32 item template ids.
    let mut s = aaemu_commons::PacketStream::from_bytes(vec![
        0x07, 0x00, 0x00, 0x00, // id
        0x02, 0x00, 0x00, 0x00, // count
        0x64, 0x00, 0x00, 0x00, // item 100
        0xc8, 0x00, 0x00, 0x00, // item 200
    ]);
    assert_eq!(
        super::decode_craft_failed(&mut s).unwrap(),
        (7, 2, vec![100, 200])
    );
    assert_eq!(s.remaining(), 0);
}

#[test]
fn racing_result_decodes_the_client_reader_layout() {
    // `FUN_397b0340`: i32 count, then min(count, 10) rows of {u32 type, u32 record}.
    let mut s = aaemu_commons::PacketStream::from_bytes(vec![
        0x02, 0x00, 0x00, 0x00, // count
        0x02, 0x00, 0x00, 0x00, 0x88, 0x13, 0x00, 0x00, // type 2, record 5000
        0x07, 0x00, 0x00, 0x00, 0x60, 0xEA, 0x00, 0x00, // type 7, record 60000
    ]);
    assert_eq!(
        super::decode_racing_result(&mut s).unwrap(),
        vec![(2, 5000), (7, 60000)]
    );
    assert_eq!(s.remaining(), 0);
}

/// Runs `expect_hp_drop` on our own unit (obj 42) over an in-memory transport. `known_hp` is
/// the HP the client already knows (as after a `gm heal`); the server then sends one
/// `SCUnitPointsPacket` per entry of `server_hp`. A window of 0 s judges the state as it is,
/// a longer one gives the packets time to arrive (the command returns as soon as it can).
async fn hp_command(
    want_drop: bool,
    secs: u64,
    known_hp: Option<i32>,
    server_hp: &[i32],
) -> anyhow::Result<()> {
    use aaemu_commons::net::FrameCodec;
    use aaemu_game::net::packet::encode;
    use aaemu_game::packets::world::SCUnitPointsPacket;
    use futures::SinkExt;
    use tokio_util::codec::FramedWrite;

    let (io, peer) = tokio::io::duplex(64 * 1024);
    let mut game = crate::game::GameClient::from_io(io, crate::record::Recorder::new(false));
    game.state.self_obj = 42;
    if let Some(hp) = known_hp {
        game.state.unit_points.insert(42, (hp, 500));
    }
    let mut server = FramedWrite::new(peer, FrameCodec);
    for &hp in server_hp {
        let packet = SCUnitPointsPacket {
            obj_id: 42,
            hp,
            mp: 500,
        };
        server.send(encode(&packet)).await?;
    }
    super::expect_hp_drop(&mut game, Some("self"), secs, want_drop).await
}

#[tokio::test]
async fn expecthpdrop_passes_when_the_hp_falls() {
    hp_command(true, 5, Some(1000), &[900]).await.unwrap();
}

/// `expectnohpdrop` used to print "no HP drop" and succeed whatever the HP did.
#[tokio::test]
async fn expectnohpdrop_fails_when_the_hp_falls() {
    let err = hp_command(false, 5, Some(1000), &[900]).await.unwrap_err();
    assert!(err.to_string().contains("fell"), "{err}");
}

#[tokio::test]
async fn the_hp_commands_judge_an_unchanged_hp() {
    hp_command(false, 0, Some(1000), &[]).await.unwrap();
    let err = hp_command(true, 0, Some(1000), &[]).await.unwrap_err();
    assert!(err.to_string().contains("no HP drop"), "{err}");
}

#[tokio::test]
async fn the_hp_commands_need_a_known_baseline() {
    for want_drop in [false, true] {
        let err = hp_command(want_drop, 0, None, &[]).await.unwrap_err();
        assert!(err.to_string().contains("hp unknown"), "{err}");
    }
}
