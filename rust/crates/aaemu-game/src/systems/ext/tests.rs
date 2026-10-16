//! Wire format, handshake and dispatch tests of the client extension channel.

use serde_json::{json, Value};

use super::packets::{encode_body, parse_body, valid_channel, ExtParseError};
use super::*;
use crate::net::packet::{encode, encode_raw};
use crate::net::registry::ClientPacket;
use crate::test_support::*;
use aaemu_commons::PacketStream;

/// A level-1 C->S frame carrying `body` as `CSAAEmuExtPacket`.
fn cs_frame(body: &[u8]) -> Vec<u8> {
    encode_raw(LEVEL_PLAIN, CS_AAEMU_EXT, body)
}

fn msg(channel: &str, payload: &Value) -> Vec<u8> {
    cs_frame(&encode_body(channel, payload.to_string().as_bytes()).unwrap())
}

async fn send_frame(server: &GameServer, client: &mut TestClient, frame: Vec<u8>) {
    server
        .handle_frame(&mut client.conn, &frame)
        .await
        .expect("an extension frame never closes the connection");
}

/// The decoded ext messages among the drained frames: `(sc opcode, channel, JSON)`.
fn drain_ext(client: &mut TestClient) -> Vec<(u16, String, Value)> {
    client
        .drain()
        .into_iter()
        .map(|(op, body)| {
            let m = parse_body(&body).expect("well-formed ext body");
            (op, m.channel, serde_json::from_slice(&m.payload).unwrap())
        })
        .collect()
}

async fn hello(server: &GameServer, client: &mut TestClient, v: Value) {
    send_frame(server, client, msg("ext.hello", &v)).await;
}

#[test]
fn opcodes_are_not_in_the_generated_1_2_tables() {
    assert!(crate::opcodes::cs::NAMES
        .iter()
        .all(|(op, _)| *op != CS_AAEMU_EXT));
    assert!(crate::opcodes::sc::NAMES
        .iter()
        .all(|(op, _)| *op != SC_AAEMU_EXT));
    assert_eq!((CS_AAEMU_EXT, SC_AAEMU_EXT), (0x0C0, 0x1F2));
    let reg = crate::packets::build_registry();
    assert_eq!(
        reg.get(LEVEL_PLAIN, CS_AAEMU_EXT).map(|r| r.name),
        Some("CSAAEmuExtPacket")
    );
}

#[test]
fn channel_names() {
    assert!(valid_channel("ext.hello"));
    assert!(valid_channel("house.rebuild.info"));
    assert!(valid_channel("a_b.c9"));
    assert!(valid_channel(&"a".repeat(64)));
    assert!(!valid_channel(""));
    assert!(!valid_channel(&"a".repeat(65)));
    assert!(!valid_channel("House.Rebuild"));
    assert!(!valid_channel("house rebuild"));
    assert!(!valid_channel("house-rebuild"));
    assert!(!valid_channel("h\u{e4}user"));
}

#[test]
fn cs_packet_decodes_a_literal_body() {
    let body = [
        1u8, // proto
        4, 0, // channel_len
        b'a', b'.', b'b', b'c', // channel
        7, 0, 0, 0, // payload_len
        b'{', b'"', b'a', b'"', b':', b'1', b'}', // payload
    ];
    let mut s = PacketStream::from_bytes(body.to_vec());
    let p = CSAAEmuExtPacket::read(&mut s).unwrap();
    assert_eq!(
        p.msg,
        Ok(ExtMessage {
            proto: 1,
            channel: "a.bc".into(),
            payload: br#"{"a":1}"#.to_vec(),
        })
    );
    // An empty payload is legal.
    let body = [1u8, 1, 0, b'x', 0, 0, 0, 0];
    assert_eq!(parse_body(&body).unwrap().payload, Vec::<u8>::new());
}

#[test]
fn malformed_bodies_are_rejected_without_error() {
    use ExtParseError::*;
    assert_eq!(parse_body(&[]), Err(Truncated));
    assert_eq!(
        parse_body(&[2, 1, 0, b'x', 0, 0, 0, 0]),
        Err(UnsupportedProto(2))
    );
    assert_eq!(parse_body(&[1, 0]), Err(Truncated));
    assert_eq!(parse_body(&[1, 0, 0, 0, 0, 0, 0]), Err(BadChannel)); // empty channel
    assert_eq!(parse_body(&[1, 65, 0]), Err(BadChannel));
    assert_eq!(parse_body(&[1, 3, 0, b'a', b'b']), Err(Truncated));
    assert_eq!(parse_body(&[1, 1, 0, b'X', 0, 0, 0, 0]), Err(BadChannel));
    assert_eq!(parse_body(&[1, 1, 0, b'x', 0, 0]), Err(Truncated));
    assert_eq!(
        parse_body(&[1, 1, 0, b'x', 5, 0, 0, 0, b'a']),
        Err(Truncated)
    );
    assert_eq!(
        parse_body(&[1, 1, 0, b'x', 0, 0, 0, 0, 9]),
        Err(TrailingBytes(1))
    );
    // payload_len above the limit, whatever follows
    let mut big = vec![1u8, 1, 0, b'x'];
    big.extend((MAX_PAYLOAD_LEN as u32 + 1).to_le_bytes());
    assert_eq!(parse_body(&big), Err(PayloadTooLarge(MAX_PAYLOAD_LEN + 1)));
    // exactly the limit is fine
    let ok = encode_body("x", &vec![b' '; MAX_PAYLOAD_LEN]).unwrap();
    assert_eq!(parse_body(&ok).unwrap().payload.len(), MAX_PAYLOAD_LEN);
    assert_eq!(
        encode_body("x", &vec![b' '; MAX_PAYLOAD_LEN + 1]),
        Err(PayloadTooLarge(MAX_PAYLOAD_LEN + 1))
    );
    assert_eq!(encode_body("X", b"{}"), Err(BadChannel));
}

#[test]
fn sc_packet_is_byte_exact() {
    let p = SCAAEmuExtPacket {
        opcode: SC_AAEMU_EXT,
        channel: "ext.welcome".into(),
        payload: br#"{"proto":1}"#.to_vec(),
    };
    let mut expected = vec![0xDD, 0x01, 0x00, 0x00, 0xF2, 0x01]; // level-1 header + opcode
    expected.push(1); // proto
    expected.extend(11u16.to_le_bytes());
    expected.extend(b"ext.welcome");
    expected.extend(11u32.to_le_bytes());
    expected.extend(br#"{"proto":1}"#);
    assert_eq!(encode(&p), expected);

    // The negotiated opcode is used as given.
    let p = SCAAEmuExtPacket {
        opcode: 0x159,
        channel: "x".into(),
        payload: Vec::new(),
    };
    assert_eq!(
        encode(&p),
        [0xDD, 0x01, 0, 0, 0x59, 0x01, 1, 1, 0, b'x', 0, 0, 0, 0]
    );
    // C->S and S->C share one body layout.
    let cs = encode_body("x", b"").unwrap();
    assert_eq!(cs, [1, 1, 0, b'x', 0, 0, 0, 0]);
}

#[tokio::test]
async fn handshake_negotiates_features_and_opcode() {
    let server = test_server();
    let mut c = TestClient::new(&server, 1, 1001);
    assert!(c.conn.handle().ext_state().is_none());

    hello(
        &server,
        &mut c,
        json!({
            "proto": 1, "driver": "aadriver 1.2.3", "sc_opcode": 0x159,
            "features": ["house.rebuild", "unknown.feature"],
        }),
    )
    .await;
    let out = drain_ext(&mut c);
    assert_eq!(out.len(), 1);
    // The welcome already uses the announced opcode and lists the intersection.
    assert_eq!(out[0].0, 0x159);
    assert_eq!(out[0].1, "ext.welcome");
    assert_eq!(
        out[0].2,
        json!({ "proto": 1, "features": ["house.rebuild"] })
    );
    let state = c.conn.handle().ext_state().unwrap();
    assert_eq!(
        state,
        ExtState {
            proto: 1,
            driver: "aadriver 1.2.3".into(),
            sc_opcode: 0x159,
            features: vec!["house.rebuild".into()],
        }
    );
    assert!(has_feature(&c.conn.handle(), "house.rebuild"));
    assert!(!has_feature(&c.conn.handle(), "unknown.feature"));

    // A second hello replaces the state (reconnect of the driver on the same connection).
    hello(&server, &mut c, json!({ "proto": 1, "features": [] })).await;
    let out = drain_ext(&mut c);
    assert_eq!(out[0].0, SC_AAEMU_EXT); // sc_opcode defaults to 0x1F2
    assert_eq!(out[0].2["features"], json!([]));
    assert_eq!(c.conn.handle().ext_state().unwrap().sc_opcode, SC_AAEMU_EXT);
}

#[tokio::test]
async fn state_is_per_connection_and_shared_with_the_table_handle() {
    let server = test_server();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    hello(&server, &mut a, json!({ "proto": 1, "features": [] })).await;
    drain_ext(&mut a);
    assert!(a.conn.handle().ext_state().is_some());
    assert!(b.conn.handle().ext_state().is_none());
    // The table entry of the connection shares the state of the connection task.
    assert!(server.connections.get(1).unwrap().ext_state().is_some());
    assert!(server.connections.get(2).unwrap().ext_state().is_none());
    // Nothing is sent to a connection without state.
    assert!(!send(&b.conn.handle(), "ext.welcome", &json!({})));
    assert!(b.drain().is_empty());
}

#[tokio::test]
async fn bad_hellos_are_ignored() {
    let server = test_server();
    let mut c = TestClient::new(&server, 1, 1001);
    for v in [
        json!({ "proto": 2 }),
        json!({}),
        json!({ "proto": 1, "sc_opcode": 0 }),
        json!({ "proto": 1, "sc_opcode": 0x218 }),
        json!({ "proto": 1, "sc_opcode": "x" }),
    ] {
        hello(&server, &mut c, v).await;
        assert!(c.drain().is_empty());
        assert!(c.conn.handle().ext_state().is_none());
    }
    // Not JSON at all.
    send_frame(
        &server,
        &mut c,
        cs_frame(&encode_body("ext.hello", b"{nope").unwrap()),
    )
    .await;
    assert!(c.drain().is_empty());
    assert!(c.conn.handle().ext_state().is_none());
}

#[tokio::test]
async fn unknown_channel_and_bad_payloads_answer_ext_error() {
    let server = test_server();
    let mut c = TestClient::new(&server, 1, 1001);
    // Before the handshake nothing is answered (the reply opcode is unknown).
    send_frame(&server, &mut c, msg("no.such.channel", &json!({}))).await;
    assert!(c.drain().is_empty());

    hello(&server, &mut c, json!({ "proto": 1, "sc_opcode": 0x1D5 })).await;
    drain_ext(&mut c);

    send_frame(&server, &mut c, msg("no.such.channel", &json!({ "a": 1 }))).await;
    let out = drain_ext(&mut c);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, 0x1D5);
    assert_eq!(out[0].1, "ext.error");
    assert_eq!(out[0].2["ok"], json!(false));
    assert_eq!(out[0].2["error"], json!("unknown_channel"));
    assert_eq!(out[0].2["channel"], json!("no.such.channel"));

    // Invalid JSON on a known channel.
    send_frame(
        &server,
        &mut c,
        cs_frame(&encode_body("house.rebuild.info", b"{broken").unwrap()),
    )
    .await;
    let out = drain_ext(&mut c);
    assert_eq!(out[0].1, "ext.error");
    assert_eq!(out[0].2["error"], json!("bad_payload"));

    // Valid JSON that is not an object, and an object of the wrong shape.
    for payload in [json!([1, 2]), json!(5), json!({ "house": [1] })] {
        send_frame(&server, &mut c, msg("house.rebuild.info", &payload)).await;
        let out = drain_ext(&mut c);
        assert_eq!(out.len(), 1, "{payload}");
        assert_eq!(out[0].2["error"], json!("bad_payload"), "{payload}");
    }

    // An empty payload is an empty object: it reaches the handler (which needs a character in
    // the world and replies on its own channel, not with ext.error).
    send_frame(
        &server,
        &mut c,
        cs_frame(&encode_body("house.rebuild.info", b"").unwrap()),
    )
    .await;
    let out = drain_ext(&mut c);
    assert_eq!(out[0].1, "house.rebuild.info");
}

#[tokio::test]
async fn malformed_frames_never_close_the_connection() {
    let server = test_server();
    let mut c = TestClient::new(&server, 1, 1001);
    hello(&server, &mut c, json!({ "proto": 1 })).await;
    drain_ext(&mut c);
    for body in [
        vec![],
        vec![1],
        vec![9, 1, 0, b'x', 0, 0, 0, 0],
        vec![1, 2, 0, b'A', b'B', 0, 0, 0, 0],
        vec![1, 1, 0, b'x', 0xFF, 0xFF, 0xFF, 0xFF],
    ] {
        send_frame(&server, &mut c, cs_frame(&body)).await;
        assert!(c.drain().is_empty());
    }
    assert!(!c.conn.is_closing());
}

#[tokio::test]
async fn send_helpers_validate_and_respect_the_negotiated_opcode() {
    let server = test_server();
    let mut c = TestClient::new(&server, 1, 1001);
    hello(&server, &mut c, json!({ "proto": 1, "sc_opcode": 0x10B })).await;
    drain_ext(&mut c);

    assert!(reply(&c.conn, "feature.msg", &json!({ "n": 1 })));
    let out = drain_ext(&mut c);
    assert_eq!(
        out,
        vec![(0x10B, "feature.msg".to_string(), json!({ "n": 1 }))]
    );

    // Invalid channel, oversized payload: dropped, not sent.
    assert!(!send(&c.conn.handle(), "Bad Channel", &json!({})));
    assert!(!send(
        &c.conn.handle(),
        "feature.msg",
        &json!({ "blob": "x".repeat(MAX_PAYLOAD_LEN) })
    ));
    assert!(c.drain().is_empty());
}
