//! Social flow tests with several connections (no MySQL: saves fail fast and are logged).

use aaemu_commons::PacketStream;

use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::social::chat::chat_type;
use crate::systems::social::packets::body;
use crate::systems::social::packets::misc::invitation_canceled_kind;
use crate::systems::social::packets::team::*;
use crate::systems::social::team::{risky_action, INVITATION_TIMEOUT_MS};
use crate::test_support::{spawn_player, test_character, test_server, TestClient};
use crate::world::PlayerRef;

pub(super) struct Pc {
    pub(super) c: TestClient,
    pub(super) p: PlayerRef,
}

impl Pc {
    pub(super) fn ops(&mut self) -> Vec<u16> {
        self.c.drain_ops()
    }

    pub(super) fn frames(&mut self) -> Vec<(u16, Vec<u8>)> {
        self.c.drain()
    }

    pub(super) async fn send(&mut self, server: &GameServer, opcode: u16, body: PacketStream) {
        server
            .handle_frame(&mut self.c.conn, &encode_raw(1, opcode, body.as_bytes()))
            .await
            .expect("handler");
    }
}

pub(super) fn pc(server: &GameServer, id: u32, name: &str, x: f32, y: f32) -> Pc {
    let c = TestClient::new(server, id, 1000 + id);
    let p = spawn_player(server, &c, test_character(id, name, x, y), 0x100 + id);
    p.data().is_online = true;
    // Alive (a guild invitation to a dead character is refused).
    p.data().unit.hp = 100;
    p.data().unit.max_hp = 100;
    let mut c = c;
    c.conn.active_char = Some(p.clone());
    c.drain();
    Pc { c, p }
}

/// Drops the frames produced by spawning (players seeing each other).
pub(super) fn settle(pcs: &mut [&mut Pc]) {
    for p in pcs.iter_mut() {
        p.c.drain();
    }
}

pub(super) fn find(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|(o, _)| *o == op)
        .map(|(_, b)| b.clone())
        .collect()
}

fn invite(team_id: u32, name: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_bool(true).write_str(name);
    s
}

fn reply(team_id: u32, owner: u32, reject: bool) -> PacketStream {
    reply_full(team_id, owner, reject, false)
}

/// `CSReplyToJoinTeamPacket` with the `isArea` flag set (the answer to
/// `SCAskToJoinTeamArea`).
fn reply_area(team_id: u32, owner: u32, reject: bool) -> PacketStream {
    reply_full(team_id, owner, reject, true)
}

fn reply_full(team_id: u32, owner: u32, reject: bool, is_area: bool) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(team_id)
        .write_bool(true)
        .write_u32(owner)
        .write_bool(reject)
        .write_str("")
        .write_bool(is_area);
    s
}

fn risky(team_id: u32, target: u32, action: u8) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_u32(target).write_u8(action);
    s
}

fn chat(t: i16, msg: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_i16(t)
        .write_i16(0)
        .write_i32(0)
        .write_str("")
        .write_str(msg)
        .write_u8(0)
        .write_i32(0);
    s
}

/// Captures the formatted WARN lines written while the guard is alive (current thread).
#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn warn_capture() -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    (capture, tracing::subscriber::set_default(subscriber))
}

fn whisper(target: &str, msg: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_i16(chat_type::WHISPER)
        .write_i16(0)
        .write_i32(0)
        .write_str(target)
        .write_str(msg)
        .write_u8(0)
        .write_i32(0);
    s
}

pub(super) fn name_body(name: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_str(name);
    s
}

fn user_join(name: &str, password: &str, create: bool) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_str(name).write_str(password).write_bool(create);
    s
}

fn user_leave(identity: i64) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_i64(identity);
    s
}

fn user_chat(identity: i64, message: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_i64(identity)
        .write_str("")
        .write_str(message)
        .write_u8(0)
        .write_i32(7);
    s
}

fn direct_leave(team_id: u32) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(team_id);
    s
}

fn direct_kick(team_id: u32, member_id: u32) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_u32(member_id);
    s
}

fn direct_dismiss(team_id: u32) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(team_id);
    s
}

fn expect_chat_error(p: &mut Pc, error: i16) {
    use crate::packets::world::SCErrorMsgPacket;
    assert_eq!(
        p.frames(),
        vec![(
            sc::SCErrorMsgPacket,
            body(&SCErrorMsgPacket::new(error, 0, true))
        )]
    );
}

/// The full expected `SCChatMessagePacket` body: literal identity bytes, then the sender block
/// `(obj id, character id, language, race, faction, name)`, message and ability. Built from
/// `PacketStream` primitives, never from the chat encoder.
fn chat_body(
    identity: [u8; 8],
    obj_id: u32,
    character_id: u32,
    faction: u32,
    name: &str,
    msg: &str,
) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_raw(&identity)
        .write_bc(obj_id)
        .write_u32(character_id)
        .write_u8(0)
        .write_u8(1)
        .write_u32(faction)
        .write_str(name)
        .write_str(msg)
        .write_i32(0)
        .write_i32(0);
    s.into_bytes()
}

#[tokio::test]
async fn user_chat_input_budgets_count_utf8_bytes_and_session_membership() {
    use crate::systems::social::chat::user_channel_identity;
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    let maximum_name = "é".repeat(24); //48 UTF-8 bytes
    let maximum_password = "λ".repeat(32); //64 UTF-8 bytes
    let requests = vec![
        user_join("", "", true),
        user_join(&"a".repeat(49), "", true),
        user_join(&"é".repeat(25), "", true),
        user_join("Valid", &"x".repeat(65), true),
        user_join("Valid", &"λ".repeat(33), true),
        user_join("bad\nname", "", true),
        user_join("bad\0name", "", true),
        user_join("Valid", "bad\tpassword", true),
        user_join("Valid", "bad\u{0085}password", true),
    ];
    for request in requests {
        a.send(&server, cs::CSJoinUserChatChannelPacket, request)
            .await;
        expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
        assert!(!server
            .chat
            .list_all_channels()
            .iter()
            .any(|c| c.chat_type == chat_type::USER));
    }
    // Invalid UTF-8 expands during the shared decoder:17 raw bytes become51 decoded bytes.
    let mut malformed_name = PacketStream::new();
    malformed_name
        .write_i16(17)
        .write_raw(&[0xff; 17])
        .write_str("")
        .write_bool(true);
    a.send(&server, cs::CSJoinUserChatChannelPacket, malformed_name)
        .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
    let mut malformed_password = PacketStream::new();
    malformed_password
        .write_str("Valid")
        .write_i16(22)
        .write_raw(&[0xff; 22])
        .write_bool(true);
    a.send(&server, cs::CSJoinUserChatChannelPacket, malformed_password)
        .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);

    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join(&maximum_name, &maximum_password, true),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCJoinedChatChannelPacket]);
    for index in 0..7 {
        a.send(
            &server,
            cs::CSJoinUserChatChannelPacket,
            user_join(&format!("Other{index}"), "", true),
        )
        .await;
        assert_eq!(a.ops(), vec![sc::SCJoinedChatChannelPacket]);
    }
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Ninth", "", true),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join(&maximum_name, &maximum_password, false),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_ALREADY_JOINED_CHANNEL);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join(&maximum_name, &maximum_password, true),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_CHANNEL_ALREADY_EXISTS);
    b.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Peer", "", true),
    )
    .await;
    assert_eq!(b.ops(), vec![sc::SCJoinedChatChannelPacket]);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Peer", "", false),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
    let channel = server
        .chat
        .list_all_channels()
        .into_iter()
        .find(|c| c.chat_type == chat_type::USER && c.internal_name() == "Other0")
        .unwrap();
    a.send(
        &server,
        cs::CSLeaveChatChannelPacket,
        user_leave(user_channel_identity(&channel)),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCLeavedChatChannelPacket]);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Peer", "", false),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCJoinedChatChannelPacket]);
    let peer = server
        .chat
        .list_all_channels()
        .into_iter()
        .find(|c| c.chat_type == chat_type::USER && c.internal_name() == "Peer")
        .unwrap();
    // Message decoding has the same expansion risk: the response must never wrap its string
    // length or exceed the transport's u16 frame size.
    let mut malformed_message = PacketStream::new();
    malformed_message
        .write_i64(user_channel_identity(&peer))
        .write_str("")
        .write_i16(10923)
        .write_raw(&vec![0xff; 10923])
        .write_u8(0)
        .write_i32(0);
    a.send(&server, cs::CSSendChatMessagePacket, malformed_message)
        .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
    assert!(b.frames().is_empty());
    server.chat.leave_user_channels(&a.p, &server.ids.chat);
    a.frames();
    assert_eq!(peer.members().len(), 1);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Again", "", true),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCJoinedChatChannelPacket]);
}

#[tokio::test]
async fn user_chat_create_join_password_message_and_leave_flow() {
    use crate::packets::world::{SCChatMessagePacket, SCLeavedChatChannelPacket};
    use crate::systems::social::chat::user_channel_identity;
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut outsider = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut outsider]);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Friends", "secret", true),
    )
    .await;
    let channel = server
        .chat
        .list_all_channels()
        .into_iter()
        .find(|c| c.chat_type == chat_type::USER)
        .unwrap();
    assert_eq!(channel.internal_name(), "Friends");
    let identity = user_channel_identity(&channel);
    let mut joined = PacketStream::new();
    joined
        .write_i16(15)
        .write_i16(0)
        .write_u32(channel.internal_id)
        .write_str("Friends");
    assert_eq!(
        a.frames(),
        vec![(sc::SCJoinedChatChannelPacket, joined.as_bytes().to_vec())]
    );
    assert!(channel.contains(&a.p));

    for (request, expected) in [
        (
            user_join("Friends", "secret", true),
            error::CHAT_CHANNEL_ALREADY_EXISTS,
        ),
        (
            user_join("Friends", "secret", false),
            error::CHAT_ALREADY_JOINED_CHANNEL,
        ),
    ] {
        a.send(&server, cs::CSJoinUserChatChannelPacket, request)
            .await;
        expect_chat_error(&mut a, expected);
    }
    for (request, expected) in [
        (user_join("Missing", "", false), error::CHAT_NO_CHANNEL),
        (user_join("Friends", "", false), error::CHAT_PRIVATE_CHANNEL),
        (
            user_join("Friends", "wrong", false),
            error::CHAT_WRONG_PASSWORD,
        ),
    ] {
        b.send(&server, cs::CSJoinUserChatChannelPacket, request)
            .await;
        expect_chat_error(&mut b, expected);
        assert!(!channel.contains(&b.p));
    }
    b.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Friends", "secret", false),
    )
    .await;
    assert_eq!(
        b.frames(),
        vec![(sc::SCJoinedChatChannelPacket, joined.as_bytes().to_vec())]
    );
    assert_eq!(channel.members().len(), 2);

    outsider
        .send(
            &server,
            cs::CSSendChatMessagePacket,
            user_chat(identity, "intrusion"),
        )
        .await;
    expect_chat_error(&mut outsider, error::CHAT_NOT_JOINED_CHANNEL);
    assert!(a.frames().is_empty() && b.frames().is_empty());
    outsider
        .send(&server, cs::CSLeaveChatChannelPacket, user_leave(identity))
        .await;
    expect_chat_error(&mut outsider, error::CHAT_NOT_JOINED_CHANNEL);
    assert_eq!(channel.members().len(), 2);

    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        user_chat(identity, "hello"),
    )
    .await;
    let sender = crate::world::session::chat_sender(&a.p.data(), a.p.obj_id);
    let mut expected = body(&SCChatMessagePacket {
        chat_type: chat_type::USER,
        sender: Some(sender),
        message: "hello".into(),
        ability: 7,
        language_type: 0,
    });
    expected[..8].copy_from_slice(&identity.to_le_bytes());
    assert_eq!(
        a.frames(),
        vec![(sc::SCChatMessagePacket, expected.clone())]
    );
    assert_eq!(b.frames(), vec![(sc::SCChatMessagePacket, expected)]);
    assert!(outsider.frames().is_empty());

    let wrong_subtype = identity | (1i64 << 16);
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        user_chat(wrong_subtype, "wrong channel"),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
    a.send(
        &server,
        cs::CSLeaveChatChannelPacket,
        user_leave(wrong_subtype),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
    let normal = server.chat.faction_chat(148);
    normal.join(&a.p);
    a.frames();
    a.send(
        &server,
        cs::CSLeaveChatChannelPacket,
        user_leave(user_channel_identity(&normal)),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_NO_CHANNEL);
    assert!(normal.contains(&a.p));

    a.send(&server, cs::CSLeaveChatChannelPacket, user_leave(identity))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCLeavedChatChannelPacket,
            body(&SCLeavedChatChannelPacket {
                chat_type: 15,
                sub_type: 0,
                faction_id: channel.internal_id
            })
        )]
    );
    assert!(!channel.contains(&a.p) && channel.contains(&b.p));
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        user_chat(identity, "after leave"),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_NOT_JOINED_CHANNEL);
    b.send(&server, cs::CSLeaveChatChannelPacket, user_leave(identity))
        .await;
    assert_eq!(b.frames().len(), 1);
    assert!(!server
        .chat
        .list_all_channels()
        .iter()
        .any(|c| c.chat_type == chat_type::USER));
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Friends", "", true),
    )
    .await;
    assert_eq!(
        a.frames(),
        vec![(sc::SCJoinedChatChannelPacket, joined.as_bytes().to_vec())]
    );
}

#[tokio::test]
async fn user_chat_requires_active_session_and_cleans_departing_members() {
    use crate::systems::social::chat::user_channel_identity;
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Public", "", true),
    )
    .await;
    b.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Public", "", false),
    )
    .await;
    settle(&mut [&mut a, &mut b]);
    let channel = server
        .chat
        .list_all_channels()
        .into_iter()
        .find(|c| c.chat_type == chat_type::USER)
        .unwrap();
    let identity = user_channel_identity(&channel);

    let mut unauthenticated = TestClient::new(&server, 9, 1009);
    for (op, request) in [
        (
            cs::CSJoinUserChatChannelPacket,
            user_join("Nobody", "", true),
        ),
        (cs::CSLeaveChatChannelPacket, user_leave(identity)),
        (
            cs::CSSendChatMessagePacket,
            user_chat(identity, "unauthenticated"),
        ),
    ] {
        server
            .handle_frame(
                &mut unauthenticated.conn,
                &encode_raw(1, op, request.as_bytes()),
            )
            .await
            .unwrap();
        assert!(unauthenticated.drain().is_empty());
    }
    assert_eq!(channel.members().len(), 2);
    crate::world::session::set_online(&server, &a.p, false);
    assert!(!channel.contains(&a.p) && channel.contains(&b.p));
    assert_eq!(a.ops(), vec![sc::SCLeavedChatChannelPacket]);
    let stale = a.p.clone();
    server.world.try_remove_character(a.p.obj_id);
    let new_session = pc(&server, 1, "Al", 100.0, 100.0);
    assert_eq!(
        server.chat.send_user_message(
            &new_session.p,
            0,
            channel.internal_id,
            &crate::packets::world::SCChatMessagePacket {
                chat_type: 15,
                sender: None,
                message: "new session".into(),
                ability: 0,
                language_type: 0
            }
        ),
        Err(crate::systems::social::util::ChatFailure::new(
            error::CHAT_NOT_JOINED_CHANNEL,
            channel.internal_name()
        ))
    );
    assert!(!channel.contains(&stale));
    // The disconnect edge must also clean an already-offline active character, for which
    // on_disconnect skips the ordinary set_online(false) hook.
    b.p.data().is_online = false;
    crate::world::session::on_disconnect(&server, &mut b.c.conn).await;
    assert!(!channel.contains(&b.p));
    assert!(b.ops().contains(&sc::SCLeavedChatChannelPacket));
    assert!(!server
        .chat
        .list_all_channels()
        .iter()
        .any(|c| c.chat_type == chat_type::USER));
    assert_eq!(server.chat.clean_up_channels(&server.ids.chat), 0);
}

#[tokio::test]
async fn faction_and_zone_chat_identity_and_membership() {
    use crate::data::world_data::SystemFaction;
    use crate::data::GameData;
    use crate::test_support::test_server_with;
    let mut data = GameData::empty();
    for (id, mother) in [(101, 148), (102, 148), (107, 149)] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..Default::default()
        });
    }
    let server = test_server_with(data);
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    b.p.data().unit.faction_id = 102;
    c.p.data().unit.faction_id = 107;
    settle(&mut [&mut a, &mut b, &mut c]);
    for x in [&mut a, &mut b, &mut c] {
        x.send(&server, cs::CSNotifyInGamePacket, PacketStream::new())
            .await;
    }
    settle(&mut [&mut a, &mut b, &mut c]);

    // ALLY (14): the faction channel of mother 148. The sender and the other ally faction
    // receive; the hostile mother 149 is excluded.
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::ALLY, "ally"),
    )
    .await;
    let ally = chat_body(
        [14, 0, 0, 0, 148, 0, 0, 0],
        a.p.obj_id,
        a.p.character_id,
        101,
        "Al",
        "ally",
    );
    assert_eq!(a.frames(), vec![(sc::SCChatMessagePacket, ally.clone())]);
    assert_eq!(b.frames(), vec![(sc::SCChatMessagePacket, ally)]);
    assert!(c.frames().is_empty());

    // Zone Shout / Trade / LFG (synthetic group 0): the identity is `(type, 0, 0)`, never the
    // sender faction; every zone member (including the hostile faction) receives them.
    for (t, identity, msg) in [
        (chat_type::SHOUT, [1, 0, 0, 0, 0, 0, 0, 0], "shout"),
        (chat_type::TRADE, [2, 0, 0, 0, 0, 0, 0, 0], "trade"),
        (chat_type::GROUP_FIND, [3, 0, 0, 0, 0, 0, 0, 0], "lfg"),
    ] {
        a.send(&server, cs::CSSendChatMessagePacket, chat(t, msg))
            .await;
        let want = chat_body(identity, a.p.obj_id, a.p.character_id, 101, "Al", msg);
        assert_eq!(a.frames(), vec![(sc::SCChatMessagePacket, want.clone())]);
        assert_eq!(b.frames(), vec![(sc::SCChatMessagePacket, want.clone())]);
        assert_eq!(c.frames(), vec![(sc::SCChatMessagePacket, want)]);
    }
}

/// The 1.2 `chat_commands` menu (row 7/8/9: "외침" 1, "거래" 2, "파티찾기" 3) sends Shout,
/// Trade and LFG to the zone-group channel; a player in another zone group hears none of them.
/// Row 14 ("연주", `chat_type_id` 13) is sendable but has no client display branch
/// (`chat_msg_event.alb` proto 20 logs "Unknown chat channel"), so the server refuses it.
#[tokio::test]
async fn trade_and_group_find_reach_only_their_zone_group_and_performance_chat_is_dropped() {
    use crate::data::world_data::{Zone, ZoneGroup};
    use crate::data::GameData;
    use crate::test_support::test_server_with;
    let mut data = GameData::empty();
    for (id, name) in [(1, "Group One"), (2, "Group Two")] {
        data.zones.groups.insert(
            id,
            ZoneGroup {
                id,
                name: name.into(),
                ..Default::default()
            },
        );
    }
    for (key, group) in [(1000, 1), (2000, 2)] {
        data.zones.zones.insert(
            key,
            Zone {
                id: key,
                name: format!("Zone {key}"),
                zone_key: key,
                group_id: group,
                ..Default::default()
            },
        );
    }
    let server = test_server_with(data);
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    for (p, zone) in [(&a.p, 1000u32), (&b.p, 1000), (&c.p, 2000)] {
        p.data().unit.transform.zone_id = zone;
        crate::world::session::on_zone_change(&server, p, 0, zone);
    }
    settle(&mut [&mut a, &mut b, &mut c]);
    for (t, identity, msg) in [
        (chat_type::SHOUT, [1, 0, 1, 0, 0, 0, 0, 0], "shout"),
        (chat_type::TRADE, [2, 0, 1, 0, 0, 0, 0, 0], "trade"),
        (chat_type::GROUP_FIND, [3, 0, 1, 0, 0, 0, 0, 0], "lfg"),
    ] {
        a.send(&server, cs::CSSendChatMessagePacket, chat(t, msg))
            .await;
        let want = chat_body(identity, a.p.obj_id, a.p.character_id, 101, "Al", msg);
        assert_eq!(a.frames(), vec![(sc::SCChatMessagePacket, want.clone())]);
        assert_eq!(b.frames(), vec![(sc::SCChatMessagePacket, want)]);
        assert!(c.frames().is_empty());
    }
    // Performance chat (13) is dropped with a warning: nobody receives it, not even the sender.
    let (capture, guard) = warn_capture();
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PERFORM, "perform"),
    )
    .await;
    drop(guard);
    assert!(a.frames().is_empty() && b.frames().is_empty() && c.frames().is_empty());
    let log = capture.0.lock().unwrap();
    let log = String::from_utf8_lossy(&log);
    assert!(
        log.lines().any(|l| l.contains("chat type 13")),
        "expected a warning that names chat type 13, got: {log}"
    );
}

/// A forms a party with B (invite + accept). Returns the team id.
async fn form_party(server: &GameServer, a: &mut Pc, b: &mut Pc) -> u32 {
    a.send(server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    let ask = find(&b.frames(), sc::SCAskToJoinTeamPacket);
    assert_eq!(ask.len(), 1);
    assert_eq!(
        ask[0],
        body(&SCAskToJoinTeamPacket {
            team_id: 0,
            sender_id: a.p.character_id,
            sender_name: "Al".into(),
            is_party: true,
        })
    );
    b.send(
        server,
        cs::CSReplyToJoinTeamPacket,
        reply(0, a.p.character_id, false),
    )
    .await;
    let team = server
        .systems
        .social
        .teams
        .active_team_by_unit(a.p.character_id)
        .expect("team created");
    team.id
}

#[tokio::test]
async fn risky_team_actions_require_member_owner_and_known_action() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    settle(&mut [&mut a, &mut b, &mut c]);
    for (actor, target, action) in [
        (3, 0, risky_action::DISMISS),
        (3, 3, risky_action::LEAVE),
        (3, 2, risky_action::KICK),
        (2, 0, risky_action::DISMISS),
        (2, 1, risky_action::KICK),
        (2, 1, risky_action::LEAVE),
        (1, 0, u8::MAX),
        (1, 99, risky_action::KICK),
    ] {
        let actor = match actor {
            1 => &mut a,
            2 => &mut b,
            _ => &mut c,
        };
        actor
            .send(
                &server,
                cs::CSAskRiskyTeamActionPacket,
                risky(team_id, target, action),
            )
            .await;
        assert!(a.ops().is_empty() && b.ops().is_empty() && c.ops().is_empty());
        let team = server
            .systems
            .social
            .teams
            .active_team(team_id)
            .expect("team preserved");
        assert_eq!(team.owner_id, 1);
        assert_eq!(team.members_count(), 2);
        assert!(a.p.data().in_party && b.p.data().in_party);
        assert!(!c.p.data().in_party);
    }
    // Rejected requests also preserve both party channel memberships.
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "still here"),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(b.ops(), vec![sc::SCChatMessagePacket]);
    assert!(c.ops().is_empty());
    // The authenticated owner can still dismiss the party.
    a.send(
        &server,
        cs::CSAskRiskyTeamActionPacket,
        risky(team_id, 0, risky_action::DISMISS),
    )
    .await;
    for member in [&mut a, &mut b] {
        let frames = member.frames();
        assert_eq!(
            find(&frames, sc::SCTeamDismissedPacket),
            vec![body(&SCTeamDismissedPacket { team_id })]
        );
        assert_eq!(
            find(&frames, sc::SCLeavedTeamPacket),
            vec![body(&SCLeavedTeamPacket {
                team_id,
                kicked: false,
                dismissed: true
            })]
        );
        assert!(!member.p.data().in_party);
    }
    assert!(server.systems.social.teams.active_team(team_id).is_none());
}

#[tokio::test]
async fn family_changes_reject_foreign_members_and_save_authorized_snapshots() {
    use crate::systems::social::family::{Family, FamilyMember};
    use crate::systems::social::packets::family::*;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    let member = |p: &PlayerRef, role| FamilyMember {
        character: Some(p.clone()),
        id: p.character_id,
        name: p.data().unit.name.clone(),
        role,
        title: "Old".into(),
    };
    server.systems.social.families.insert(Family {
        id: 11,
        members: vec![member(&a.p, 1), member(&b.p, 0)],
        removed_members: vec![901],
    });
    server.systems.social.families.insert(Family {
        id: 22,
        members: vec![member(&c.p, 1)],
        removed_members: vec![902],
    });
    a.p.data().family = 11;
    b.p.data().family = 11;
    c.p.data().family = 22;
    let mut title = PacketStream::new();
    title.write_u32(3).write_str("Forged");
    a.send(&server, cs::CSFamilyChangeTitlePacket, title).await;
    // The target is in another family: the C# `_familyMembers[memberId]` lookup was unchecked.
    assert_eq!(
        a.frames(),
        vec![error_frame(
            crate::systems::social::util::error::FAMILY_OTHER_FAMILY
        )]
    );
    let mut owner = PacketStream::new();
    owner.write_u32(3);
    a.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    assert_eq!(
        a.frames(),
        vec![error_frame(
            crate::systems::social::util::error::FAMILY_OTHER_FAMILY
        )]
    );
    assert!(b.ops().is_empty() && c.ops().is_empty());
    let own = server.systems.social.families.family(11).unwrap();
    let foreign = server.systems.social.families.family(22).unwrap();
    assert_eq!(own.member(1).unwrap().role, 1);
    assert_eq!(own.removed_members, vec![901]); // Invalid requests never request a save snapshot.
    assert_eq!(foreign.member(3).unwrap().title, "Old");
    assert_eq!(foreign.member(3).unwrap().role, 1);
    assert_eq!(foreign.removed_members, vec![902]);
    let mut title = PacketStream::new();
    title.write_u32(2).write_str("Brother");
    a.send(&server, cs::CSFamilyChangeTitlePacket, title).await;
    for member in [&mut a, &mut b] {
        assert_eq!(
            member.frames(),
            vec![(
                sc::SCFamilyTitleChangedPacket,
                body(&SCFamilyTitleChangedPacket {
                    family_id: 11,
                    member_id: 2,
                    title: "Brother".into()
                })
            )]
        );
    }
    let mut updated = server.systems.social.families.family(11).unwrap();
    assert_eq!(updated.member(2).unwrap().title, "Brother");
    assert!(updated.removed_members.is_empty()); // Existing save_family took its updated snapshot.
    updated.removed_members.push(903);
    server.systems.social.families.insert(updated);
    let mut owner = PacketStream::new();
    owner.write_u32(2);
    a.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    let updated = server.systems.social.families.family(11).unwrap();
    assert_eq!(updated.member(1).unwrap().role, 0);
    assert_eq!(updated.member(2).unwrap().role, 1);
    assert_eq!(updated.member(2).unwrap().title, "Brother");
    assert!(updated.removed_members.is_empty());
    for member in [&mut a, &mut b] {
        assert_eq!(
            member.frames(),
            vec![
                (
                    sc::SCFamilyOwnerChangedPacket,
                    body(&SCFamilyOwnerChangedPacket {
                        family_id: 11,
                        member_id: 2
                    })
                ),
                (
                    sc::SCFamilyDescPacket,
                    body(&SCFamilyDescPacket {
                        family: updated.info()
                    })
                ),
            ]
        );
    }
    assert!(c.ops().is_empty());
    assert_eq!(
        server
            .systems
            .social
            .families
            .family(22)
            .unwrap()
            .removed_members,
        vec![902]
    );
}

#[tokio::test]
async fn team_invite_accept_chat_owner_kick_leave() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 1900.0, 1900.0);
    settle(&mut [&mut a, &mut b, &mut c]);

    let team_id = form_party(&server, &mut a, &mut b).await;
    let fa = a.frames();
    let fb = b.frames();
    // Both get SCJoinedTeamPacket, the ping position and join their party chat channel.
    for f in [&fa, &fb] {
        let ops: Vec<u16> = f.iter().map(|x| x.0).collect();
        assert_eq!(
            ops,
            vec![
                sc::SCJoinedTeamPacket,
                sc::SCTeamPingPosPacket,
                sc::SCJoinedChatChannelPacket
            ],
            "{ops:x?}"
        );
    }
    let joined = &find(&fa, sc::SCJoinedTeamPacket)[0];
    let mut s = PacketStream::from_bytes(joined.clone());
    assert_eq!(s.read_u32().unwrap(), team_id);
    assert_eq!(s.read_u32().unwrap(), 1); // owner
    assert!(s.read_bool().unwrap()); // party
    assert_eq!(s.read_u8().unwrap(), 2); // party 0 count
    assert!(a.p.data().in_party && b.p.data().in_party);
    let party_chan = find(&fa, sc::SCJoinedChatChannelPacket)[0].clone();
    assert_eq!(
        &party_chan[..4],
        &[
            4,
            0,
            ((team_id << 6) & 0xFF) as u8,
            ((team_id << 6) >> 8) as u8
        ]
    );

    // Party chat reaches both members; the identity is `(party, team_id << 6, 0)`.
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "hi"),
    )
    .await;
    let mut party_identity = [0u8; 8];
    party_identity[..2].copy_from_slice(&chat_type::PARTY.to_le_bytes());
    party_identity[2..4].copy_from_slice(&((team_id << 6) as i16).to_le_bytes());
    let party_body = chat_body(
        party_identity,
        a.p.obj_id,
        a.p.character_id,
        101,
        "Al",
        "hi",
    );
    assert_eq!(
        a.frames(),
        vec![(sc::SCChatMessagePacket, party_body.clone())]
    );
    assert_eq!(b.frames(), vec![(sc::SCChatMessagePacket, party_body)]);
    // C is not in a party -> error.
    c.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "x"),
    )
    .await;
    assert_eq!(c.ops(), vec![sc::SCErrorMsgPacket]);
    // Raid chat in a party (team found, raid channel empty) and raid leader by a non-owner.
    b.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::RAID_LEADER, "x"),
    )
    .await;
    assert_eq!(b.ops(), vec![sc::SCErrorMsgPacket]);

    // C joins (far away: the ObjIds are refreshed both ways).
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    assert_eq!(c.ops(), vec![sc::SCAskToJoinTeamPacket]);
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(team_id, a.p.character_id, false),
    )
    .await;
    // The late joiner joins the party chat channel too (the C# forgot to).
    assert_eq!(
        c.ops(),
        vec![
            sc::SCJoinedTeamPacket,
            sc::SCTeamPingPosPacket,
            sc::SCJoinedChatChannelPacket,
            sc::SCRefreshTeamMemberPacket,
            sc::SCRefreshTeamMemberPacket
        ]
    );
    assert_eq!(
        a.ops(),
        vec![sc::SCTeamMemberJoinedPacket, sc::SCRefreshTeamMemberPacket]
    );
    assert_eq!(
        b.ops(),
        vec![sc::SCTeamMemberJoinedPacket, sc::SCRefreshTeamMemberPacket]
    );
    // ... so it hears the party as well as talks into it.
    c.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "x"),
    )
    .await;
    assert_eq!(c.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(b.ops(), vec![sc::SCChatMessagePacket]);
    b.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "y"),
    )
    .await;
    assert_eq!(c.ops(), vec![sc::SCChatMessagePacket]);
    a.ops();
    b.ops();

    // Owner change A -> B.
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_u32(2);
    a.send(&server, cs::CSMakeTeamOwnerPacket, s).await;
    let owner_changed = body(&SCTeamOwnerChangedPacket { team_id, id: 2 });
    for x in [&mut a, &mut b, &mut c] {
        assert_eq!(
            x.frames(),
            vec![(sc::SCTeamOwnerChangedPacket, owner_changed.clone())]
        );
    }

    // A (no longer owner) cannot kick; B kicks C.
    let c_channel_left = body(&crate::packets::world::SCLeavedChatChannelPacket {
        chat_type: chat_type::PARTY,
        sub_type: (team_id << 6) as i16,
        faction_id: 0,
    });
    a.send(
        &server,
        cs::CSAskRiskyTeamActionPacket,
        risky(team_id, 3, risky_action::KICK),
    )
    .await;
    assert!(a.ops().is_empty() && c.ops().is_empty());
    b.send(
        &server,
        cs::CSAskRiskyTeamActionPacket,
        risky(team_id, 3, risky_action::KICK),
    )
    .await;
    // The kicked member leaves its party chat channel (the C# made the kicker leave its own
    // instead), is removed from the team first, so it only gets SCLeavedTeamPacket after that.
    assert_eq!(
        c.frames(),
        vec![
            (sc::SCLeavedChatChannelPacket, c_channel_left.clone(),),
            (
                sc::SCLeavedTeamPacket,
                body(&SCLeavedTeamPacket {
                    team_id,
                    kicked: true,
                    dismissed: false
                })
            )
        ]
    );
    // The kicker (B) keeps its party chat channel.
    assert_eq!(
        b.ops(),
        vec![sc::SCTeamMemberLeavedPacket, sc::SCTeamAckRiskyActionPacket]
    );
    assert_eq!(
        a.ops(),
        vec![sc::SCTeamMemberLeavedPacket, sc::SCTeamAckRiskyActionPacket]
    );
    assert!(!c.p.data().in_party);

    // A leaves: a party of one is disbanded.
    a.send(
        &server,
        cs::CSAskRiskyTeamActionPacket,
        risky(team_id, 1, risky_action::LEAVE),
    )
    .await;
    let fa = a.frames();
    let fb = b.frames();
    assert_eq!(
        fa.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![sc::SCLeavedChatChannelPacket, sc::SCLeavedTeamPacket]
    );
    assert_eq!(
        fb.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![
            sc::SCTeamMemberLeavedPacket,
            sc::SCTeamAckRiskyActionPacket,
            sc::SCTeamDismissedPacket,
            sc::SCLeavedChatChannelPacket,
            sc::SCLeavedTeamPacket
        ]
    );
    assert_eq!(
        find(&fb, sc::SCLeavedTeamPacket)[0],
        body(&SCLeavedTeamPacket {
            team_id,
            kicked: false,
            dismissed: true
        })
    );
    assert!(server.systems.social.teams.active_team(team_id).is_none());
    assert!(!a.p.data().in_party && !b.p.data().in_party);
}

#[tokio::test]
async fn team_rejection_raid_conversion_offline_and_reconnect() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    // Rejection: the owner gets SCRejectedTeamPacket with the target's name.
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    b.ops();
    b.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, true))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCRejectedTeamPacket,
            body(&SCRejectedTeamPacket {
                name: "Bob".into(),
                party: true
            })
        )]
    );
    // A second invitation while one is pending is rejected at once.
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCRejectedTeamPacket]);
    b.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, false))
        .await;
    let team_id = server
        .systems
        .social
        .teams
        .active_team_by_unit(1)
        .expect("team")
        .id;
    a.ops();
    b.ops();

    // Convert to raid: both join the raid channel; raid chat works for every member.
    let mut s = PacketStream::new();
    s.write_u32(team_id);
    a.send(&server, cs::CSConvertToRaidTeamPacket, s).await;
    for x in [&mut a, &mut b] {
        assert_eq!(
            x.ops(),
            vec![
                sc::SCTeamBecameRaidTeamPacket,
                sc::SCJoinedChatChannelPacket
            ]
        );
    }
    b.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::RAID, "r"),
    )
    .await;
    let mut raid_identity = [0u8; 8];
    raid_identity[..2].copy_from_slice(&chat_type::RAID.to_le_bytes());
    raid_identity[2..4].copy_from_slice(&(team_id as i16).to_le_bytes());
    let raid_body = chat_body(raid_identity, b.p.obj_id, b.p.character_id, 101, "Bob", "r");
    assert_eq!(
        a.frames(),
        vec![(sc::SCChatMessagePacket, raid_body.clone())]
    );
    assert_eq!(b.frames(), vec![(sc::SCChatMessagePacket, raid_body)]);

    // Role / marker / looting rule / ping.
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_u32(2).write_u8(2);
    b.send(&server, cs::CSSetTeamMemberRolePacket, s).await;
    assert_eq!(a.ops(), vec![sc::SCTeamMemberRoleChangedPacket]);
    b.ops();
    let mut s = PacketStream::new();
    s.write_u32(team_id)
        .write_i32(3)
        .write_u8(2)
        .write_bc(0x102);
    a.send(&server, cs::CSSetOverHeadMarkerPacket, s).await;
    let marker = body(&SCOverHeadMarkerSetPacket {
        team_id,
        index: 3,
        is_obj_id: true,
        id: 0x102,
    });
    assert_eq!(b.frames(), vec![(sc::SCOverHeadMarkerSetPacket, marker)]);
    a.ops();
    let mut s = PacketStream::new();
    s.write_u32(team_id)
        .write_u8(2)
        .write_u8(4)
        .write_u32(2)
        .write_bool(false)
        .write_u8(0x05);
    a.send(&server, cs::CSChangeLootingRulePacket, s).await;
    let rule = server
        .systems
        .social
        .teams
        .active_team(team_id)
        .unwrap()
        .looting_rule;
    // Flags 1 | 4: method and loot master (kept: the method is LootMaster); grade unchanged.
    assert_eq!(
        (rule.loot_method, rule.minimum_grade, rule.loot_master),
        (2, 2, 2)
    );
    assert_eq!(b.ops(), vec![sc::SCTeamLootingRuleChangedPacket]);
    a.ops();

    // B goes offline: A sees the disconnect (obj id 0 in WritePerson); the team stays.
    crate::world::session::set_online(&server, &b.p, false);
    let fa = a.frames();
    assert_eq!(fa.len(), 1);
    assert_eq!(fa[0].0, sc::SCTeamMemberDisconnectedPacket);
    assert_eq!(&fa[0].1[8 + 38..8 + 41], &[0, 0, 0]);
    assert!(server.systems.social.teams.active_team(team_id).is_some());

    // B reconnects (UpdateAtLogin): full team + the other members' snapshots; A sees the
    // member rejoin, its snapshot and the forced ObjId refresh. (B never left its chat
    // channels: only LeaveWorldTask leaves them, so the re-joins send nothing.)
    crate::world::session::set_online(&server, &b.p, true);
    server.systems.social.teams.update_at_login(&server, &b.p);
    assert_eq!(
        b.ops(),
        vec![
            sc::SCJoinedTeamPacket,
            sc::SCTeamRemoteMembersExPacket,
            sc::SCRefreshTeamMemberPacket,
        ]
    );
    assert_eq!(
        a.ops(),
        vec![
            sc::SCTeamMemberJoinedPacket,
            sc::SCTeamRemoteMembersExPacket,
            sc::SCRefreshTeamMemberPacket
        ]
    );

    // Everybody offline -> the zombie team is disbanded after the grace period.
    crate::world::session::set_online(&server, &a.p, false);
    crate::world::session::set_online(&server, &b.p, false);
    let now = crate::systems::social::util::now_ms();
    server.systems.social.teams.update_all_teams(&server, now);
    assert!(server.systems.social.teams.active_team(team_id).is_some());
    server.systems.social.teams.update_all_teams(
        &server,
        now + crate::systems::social::team::OFFLINE_TEAM_DISBAND_GRACE_MS,
    );
    assert!(server.systems.social.teams.active_team(team_id).is_none());
    assert!(!a.p.data().in_party && !b.p.data().in_party);
}

#[tokio::test]
async fn remote_member_sync_is_delta_cached() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 1900.0, 1900.0);
    settle(&mut [&mut a, &mut b]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    a.ops();
    b.ops();
    let teams = &server.systems.social.teams;
    teams.update_all_teams(&server, 1_000);
    assert_eq!(a.ops(), vec![sc::SCTeamRemoteMembersExPacket]);
    assert_eq!(b.ops(), vec![sc::SCTeamRemoteMembersExPacket]);
    // Nothing changed: no traffic until the forced resync (5 s).
    teams.update_all_teams(&server, 1_500);
    assert!(a.ops().is_empty() && b.ops().is_empty());
    b.p.data().unit.hp -= 10;
    teams.update_all_teams(&server, 2_000);
    assert_eq!(a.ops(), vec![sc::SCTeamRemoteMembersExPacket]);
    assert!(b.ops().is_empty());
    // Forced resync 5 s after each direction's last send.
    teams.update_all_teams(&server, 6_000);
    assert!(a.ops().is_empty());
    assert_eq!(b.ops(), vec![sc::SCTeamRemoteMembersExPacket]);
    teams.update_all_teams(&server, 7_000);
    assert_eq!(a.ops(), vec![sc::SCTeamRemoteMembersExPacket]);
    assert!(b.ops().is_empty());
    // In render distance -> no remote sync.
    b.p.data().unit.transform.set_xyz(120.0, 100.0, 100.0);
    let world = server.world.world(0).unwrap();
    world.add_visible_object(&b.p, &server.view_ctx());
    a.ops();
    b.ops();
    teams.update_all_teams(&server, 20_000);
    assert!(a.ops().is_empty() && b.ops().is_empty());
    let _ = team_id;
}

/// Five members in a party, A has the money: the guild is created for everybody.
#[tokio::test]
async fn guild_create_invite_kick_roles_and_disband() {
    let server = test_server();
    let mut pcs: Vec<Pc> = (1..=5)
        .map(|i| pc(&server, i, &format!("M{i}"), 100.0 + i as f32, 100.0))
        .collect();
    let mut outsider = pc(&server, 9, "Out", 120.0, 100.0);
    for x in pcs.iter_mut() {
        x.c.drain();
    }
    outsider.c.drain();
    {
        let teams = &server.systems.social.teams;
        for i in 1..5 {
            let name = format!("M{}", i + 1);
            let (lead, rest) = pcs.split_at_mut(i);
            teams.ask_to_join(&server, &lead[0].p, &name, 0, true, None);
            teams.reply_to_join_team(&server, &rest[0].p, 0, true, 1, false, "", false);
        }
        assert_eq!(teams.active_team_by_unit(1).unwrap().members_count(), 5);
    }
    pcs[0].p.data().money = 20_000;
    for x in pcs.iter_mut() {
        x.ops();
    }
    outsider.ops();

    // Bad names first.
    let create = |name: &str| {
        let mut s = PacketStream::new();
        s.write_str(name).write_u32(0);
        s
    };
    pcs[0]
        .send(&server, cs::CSCreateExpeditionPacket, create("x1"))
        .await;
    assert_eq!(pcs[0].ops(), vec![sc::SCErrorMsgPacket]);

    pcs[0]
        .send(&server, cs::CSCreateExpeditionPacket, create("Knights"))
        .await;
    let id = pcs[0].p.data().unit.expedition_id;
    assert_ne!(id, 0);
    assert_eq!(pcs[0].p.data().money, 10_000);
    let owner_ops = pcs[0].ops();
    assert_eq!(owner_ops[0], sc::SCItemTaskSuccessPacket);
    assert_eq!(owner_ops[1], sc::SCFactionCreatedPacket);
    assert_eq!(owner_ops[2], sc::SCFactionListPacket);
    assert!(owner_ops.contains(&sc::SCUnitExpeditionChangedPacket));
    assert!(owner_ops.contains(&sc::SCExpeditionMemberListPacket));
    assert!(owner_ops.contains(&sc::SCExpeditionMemberStatusChangedPacket));
    let e = server.expeditions.get(id).unwrap();
    assert_eq!(e.members.len(), 5);
    assert_eq!(e.members[0].role, 255);
    assert_eq!(e.policies.len(), 5);
    for x in pcs.iter_mut().skip(1) {
        assert_eq!(x.p.data().unit.expedition_id, id);
        let ops = x.ops();
        assert!(ops.contains(&sc::SCExpeditionRolePolicyListPacket));
        assert!(ops.contains(&sc::SCJoinedChatChannelPacket));
    }
    // Everybody on the server learns the new faction.
    assert!(outsider.ops().contains(&sc::SCFactionListPacket));

    // Guild chat.
    pcs[1]
        .send(
            &server,
            cs::CSSendChatMessagePacket,
            chat(chat_type::CLAN, "gg"),
        )
        .await;
    for x in pcs.iter_mut() {
        assert_eq!(x.ops(), vec![sc::SCChatMessagePacket]);
    }

    // Invite the outsider (owner has the Invite policy) and accept.
    let mut s = PacketStream::new();
    s.write_str("Out");
    pcs[0]
        .send(&server, cs::CSInviteToExpeditionPacket, s)
        .await;
    let inv = find(&outsider.frames(), sc::SCExpeditionInvitationPacket);
    assert_eq!(inv.len(), 1);
    let mut s = PacketStream::new();
    s.write_u32(id).write_u32(1).write_bool(true);
    outsider
        .send(&server, cs::CSReplyExpeditionInvitationPacket, s)
        .await;
    assert_eq!(outsider.p.data().unit.expedition_id, id);
    assert_eq!(server.expeditions.get(id).unwrap().members.len(), 6);
    for x in pcs.iter_mut() {
        x.ops();
    }
    outsider.ops();

    // A member (role 0) cannot kick; the owner can.
    let mut s = PacketStream::new();
    s.write_u32(9);
    pcs[1]
        .send(&server, cs::CSKickFromExpeditionPacket, s)
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().members.len(), 6);
    let mut s = PacketStream::new();
    s.write_u32(9);
    pcs[0]
        .send(&server, cs::CSKickFromExpeditionPacket, s)
        .await;
    assert_eq!(outsider.p.data().unit.expedition_id, 0);
    let fo = outsider.frames();
    assert_eq!(fo[0].0, sc::SCLeavedChatChannelPacket);
    assert_eq!(fo[1].0, sc::SCUnitExpeditionChangedPacket);
    let mut s = PacketStream::from_bytes(fo[1].1.clone());
    s.read_bc().unwrap();
    assert_eq!(s.read_u32().unwrap(), 9);
    assert_eq!(s.read_string().unwrap(), "M1");
    assert_eq!(s.read_string().unwrap(), "Out");
    assert_eq!(s.read_u32().unwrap(), id);
    assert_eq!(s.read_u32().unwrap(), 0);
    assert!(s.read_bool().unwrap());
    for x in pcs.iter_mut() {
        x.ops();
    }

    // Promote M2 to officer (role 2), then hand the guild over to M2.
    let mut s = PacketStream::new();
    s.write_u8(2).write_u32(2);
    pcs[0]
        .send(&server, cs::CSChangeExpeditionMemberRolePacket, s)
        .await;
    assert_eq!(
        server.expeditions.get(id).unwrap().member(2).unwrap().role,
        2
    );
    for x in pcs.iter_mut() {
        assert_eq!(x.ops(), vec![sc::SCExpeditionRoleChangedPacket]);
    }
    let before_transfer = server.expeditions.get(id).unwrap();
    // An outsider, an ordinary member, and a self-transfer cannot change the owner snapshot
    // or downgrade the current owner's role. Invalid requests send no transition packets, only
    // the caller's error.
    for (actor, target, refused) in [
        (
            9,
            2,
            crate::systems::social::util::error::ONLY_EXPEDITION_MEMBER,
        ),
        (3, 2, crate::systems::social::util::error::FACTION_RIGHTS),
        (
            1,
            1,
            crate::systems::social::util::error::EXPEDITION_CHANGE_OWNER_SELF,
        ),
    ] {
        let mut request = PacketStream::new();
        request.write_u32(target);
        if actor == 9 {
            outsider
                .send(&server, cs::CSChangeExpeditionOwnerPacket, request)
                .await;
        } else {
            pcs[(actor - 1) as usize]
                .send(&server, cs::CSChangeExpeditionOwnerPacket, request)
                .await;
        }
        assert_eq!(server.expeditions.get(id).unwrap(), before_transfer);
        if actor == 9 {
            assert_eq!(outsider.frames(), vec![error_frame(refused)]);
        }
        for (i, member) in pcs.iter_mut().enumerate() {
            if i + 1 == actor as usize {
                assert_eq!(member.frames(), vec![error_frame(refused)]);
            } else {
                assert!(member.ops().is_empty());
            }
        }
        outsider.ops();
    }
    let mut s = PacketStream::new();
    s.write_u32(2);
    pcs[0]
        .send(&server, cs::CSChangeExpeditionOwnerPacket, s)
        .await;
    let e = server.expeditions.get(id).unwrap();
    assert_eq!(e.faction.owner_id, 2);
    assert_eq!(e.faction.owner_name, "M2"); // The existing save binds this snapshot field.
    assert_eq!(
        (e.member(1).unwrap().role, e.member(2).unwrap().role),
        (0, 255)
    );
    for x in pcs.iter_mut() {
        assert_eq!(
            x.ops(),
            vec![
                sc::SCExpeditionOwnerChangedPacket,
                sc::SCExpeditionRoleChangedPacket,
                sc::SCExpeditionRoleChangedPacket
            ]
        );
    }

    // M3 leaves.
    pcs[2]
        .send(&server, cs::CSLeaveExpeditionPacket, PacketStream::new())
        .await;
    assert_eq!(pcs[2].p.data().unit.expedition_id, 0);
    assert_eq!(server.expeditions.get(id).unwrap().members.len(), 4);
    for x in pcs.iter_mut() {
        x.ops();
    }

    // Only the owner can disband.
    pcs[0]
        .send(&server, cs::CSDismissExpeditionPacket, PacketStream::new())
        .await;
    assert_eq!(pcs[0].ops(), vec![sc::SCErrorMsgPacket]);
    pcs[1]
        .send(&server, cs::CSDismissExpeditionPacket, PacketStream::new())
        .await;
    let e = server.expeditions.get(id).unwrap();
    assert!(e.is_disbanded && e.members.is_empty());
    assert_eq!(e.faction.name, format!("$deleted-guild-{id}"));
    assert_eq!(
        pcs[1].ops(),
        vec![
            sc::SCExpeditionDismissedPacket,
            sc::SCLeavedChatChannelPacket
        ]
    );
    assert_eq!(pcs[0].p.data().unit.expedition_id, 0);
}

/// The role-policy packet carries nine permission flags. The 1.2 client only lets the owner
/// (role 255) edit a lower role (`SET_POLICY = role == 255` in its role-policy tab), so a
/// Commander's `Expel` right must not help. The dialog edits the name and the invite / expel /
/// promote / siege-join flags only; the other four keep their stored value, and the stored
/// policy is what the members receive.
#[tokio::test]
async fn guild_role_policy_change_matches_the_dialog_fields() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::{Expedition, ExpeditionMember, ExpeditionRolePolicy};
    use crate::systems::social::util::error;

    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0, 100.0);
    let mut officer = pc(&server, 2, "Officer", 101.0, 100.0);
    let id = 7;
    let member = |character_id: u32, name: &str, role: u8| ExpeditionMember {
        expedition_id: id,
        character_id,
        in_party: false,
        is_online: true,
        last_world_leave_time: 0,
        name: name.into(),
        level: 10,
        zone_id: 0,
        id3: 0,
        abilities: [0; 3],
        role,
        position: (0.0, 0.0, 0.0),
        memo: String::new(),
        transfer_requested_time: 0,
    };
    let policy = |role: u8, name: &str, flags: [bool; 9]| ExpeditionRolePolicy {
        expedition_id: id,
        role,
        name: name.into(),
        dominion_declare: flags[0],
        invite: flags[1],
        expel: flags[2],
        promote: flags[3],
        dismiss: flags[4],
        chat: flags[5],
        manager_chat: flags[6],
        siege_master: flags[7],
        join_siege: flags[8],
    };
    owner.p.data().unit.expedition_id = id;
    officer.p.data().unit.expedition_id = id;
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id,
            name: "Knights".into(),
            owner_id: 1,
            owner_name: "Owner".into(),
            ..SystemFaction::default()
        },
        members: vec![member(1, "Owner", 255), member(2, "Officer", 3)],
        policies: vec![
            policy(0, "Apprentice", [false; 9]),
            policy(3, "Commander", [true; 9]),
            policy(255, "General", [true; 9]),
        ],
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    settle(&mut [&mut owner, &mut officer]);

    let request = |role: u8, name: &str, flag: bool| {
        let mut s = PacketStream::new();
        s.write_u32(id)
            .write_u8(role)
            .write_str(name)
            .write_bool(flag)
            .write_bool(flag)
            .write_bool(flag)
            .write_bool(flag)
            .write_bool(flag)
            .write_bool(flag)
            .write_bool(flag)
            .write_bool(flag)
            .write_bool(flag);
        s
    };

    // Role 3 carries the C# Expel right, but the 1.2 client only enables the check boxes for
    // the owner: the change must not be stored, and the officer is told why.
    officer
        .send(
            &server,
            cs::CSChangeExpeditionRolePolicyPacket,
            request(0, "Hacked", true),
        )
        .await;
    assert_eq!(
        server
            .expeditions
            .get(id)
            .unwrap()
            .policy_by_role(0)
            .unwrap()
            .name,
        "Apprentice"
    );
    let frames = officer.frames();
    assert_eq!(
        find(&frames, sc::SCErrorMsgPacket),
        vec![body(&crate::packets::world::SCErrorMsgPacket::new(
            error::FACTION_CHANGE_MEMBER_ROLE_RIGHTS,
            0,
            true
        ))]
    );

    // The owner cannot edit their own (role 255) policy either.
    owner
        .send(
            &server,
            cs::CSChangeExpeditionRolePolicyPacket,
            request(255, "Me", false),
        )
        .await;
    assert_eq!(
        server
            .expeditions
            .get(id)
            .unwrap()
            .policy_by_role(255)
            .unwrap()
            .name,
        "General"
    );
    let frames = owner.frames();
    assert_eq!(
        find(&frames, sc::SCErrorMsgPacket),
        vec![body(&crate::packets::world::SCErrorMsgPacket::new(
            error::FACTION_CHANGE_MEMBER_ROLE_RIGHTS,
            0,
            true
        ))]
    );

    // The owner edits role 0 with every flag set: the name and the five dialog fields must be
    // kept in the stored policy and in the packet broadcast to both members; the four flags
    // the dialog does not edit keep their stored value.
    owner
        .send(
            &server,
            cs::CSChangeExpeditionRolePolicyPacket,
            request(0, "Recruit", true),
        )
        .await;
    let expected = policy(
        0,
        "Recruit",
        [false, true, true, true, false, false, false, false, true],
    );
    assert_eq!(
        server
            .expeditions
            .get(id)
            .unwrap()
            .policy_by_role(0)
            .unwrap(),
        &expected
    );
    for x in [&mut owner, &mut officer] {
        let frames = x.frames();
        let changed = find(&frames, sc::SCExpeditionRolePolicyChangedPacket);
        assert_eq!(changed.len(), 1);
        let mut s = PacketStream::from_bytes(changed[0].clone());
        assert_eq!(
            ExpeditionRolePolicy::read(&mut s).expect("policy"),
            expected
        );
        assert!(s.read_bool().expect("success"));
    }
}

#[tokio::test]
async fn guild_creation_needs_a_full_party() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    let mut s = PacketStream::new();
    s.write_str("Knights").write_u32(0);
    a.send(&server, cs::CSCreateExpeditionPacket, s.clone())
        .await;
    assert_eq!(a.ops(), vec![sc::SCErrorMsgPacket]); // no party
    form_party(&server, &mut a, &mut b).await;
    a.ops();
    a.send(&server, cs::CSCreateExpeditionPacket, s).await;
    let f = a.frames();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].1[..2], 294i16.to_le_bytes()); // ExpeditionCreateMember
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary expedition tables"]
async fn private_mysql_expedition_owner_transition_persists_consistent_snapshot() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::{self, Expedition};
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    // Single-connection temporary tables shadow the application tables for this test only.
    let mut schemas = Vec::new();
    for table in ["expeditions", "expedition_members"] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ddl.starts_with("CREATE TABLE"));
        schemas.push(ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1));
    }
    for ddl in schemas {
        sqlx::query(&ddl).execute(&pool).await.unwrap();
    }
    let mut server = test_server();
    server.db = pool.clone();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    a.p.data().unit.expedition_id = 77;
    b.p.data().unit.expedition_id = 77;
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: 1,
            owner_name: "Al".into(),
            mother_id: 101,
            ..Default::default()
        },
        members: vec![
            expedition::member_from_character(&server, 77, &a.p, true),
            expedition::member_from_character(&server, 77, &b.p, false),
        ],
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    let mut request = PacketStream::new();
    request.write_u32(2);
    a.send(&server, cs::CSChangeExpeditionOwnerPacket, request)
        .await;
    let saved: (i64, String) =
        sqlx::query_as("SELECT CAST(owner AS SIGNED),owner_name FROM expeditions WHERE id=77")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved, (2, "Bob".into()));
    let roles: Vec<(i64, i64)> =
        sqlx::query_as("SELECT CAST(character_id AS SIGNED),CAST(role AS SIGNED) FROM expedition_members ORDER BY character_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(roles, vec![(1, 0), (2, 255)]);
    let snapshot = server.expeditions.get(77).unwrap();
    assert_eq!(
        (
            snapshot.faction.owner_id,
            snapshot.faction.owner_name.as_str()
        ),
        (2, "Bob")
    );
    assert_eq!(
        a.ops(),
        vec![
            sc::SCExpeditionOwnerChangedPacket,
            sc::SCExpeditionRoleChangedPacket,
            sc::SCExpeditionRoleChangedPacket
        ]
    );
    assert_eq!(
        b.ops(),
        vec![
            sc::SCExpeditionOwnerChangedPacket,
            sc::SCExpeditionRoleChangedPacket,
            sc::SCExpeditionRoleChangedPacket
        ]
    );
    let mut self_transfer = PacketStream::new();
    self_transfer.write_u32(2);
    b.send(&server, cs::CSChangeExpeditionOwnerPacket, self_transfer)
        .await;
    assert_eq!(server.expeditions.get(77).unwrap(), snapshot);
    assert!(a.ops().is_empty() && b.ops().is_empty());
    let saved_after: (i64, String) =
        sqlx::query_as("SELECT CAST(owner AS SIGNED),owner_name FROM expeditions WHERE id=77")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved_after, saved);
    for table in ["expedition_members", "expeditions"] {
        sqlx::query(&format!("DROP TEMPORARY TABLE {table}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary expedition tables"]
async fn private_mysql_expedition_role_policy_persists_the_dialog_fields() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::{self, Expedition, ExpeditionRolePolicy};
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    // Single-connection temporary tables shadow the application tables for this test only.
    let mut schemas = Vec::new();
    for table in [
        "expeditions",
        "expedition_members",
        "expedition_role_policies",
    ] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ddl.starts_with("CREATE TABLE"));
        schemas.push(ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1));
    }
    for ddl in schemas {
        sqlx::query(&ddl).execute(&pool).await.unwrap();
    }
    let mut server = test_server();
    server.db = pool.clone();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    settle(&mut [&mut a]);
    a.p.data().unit.expedition_id = 77;
    let policy = |role: u8, name: &str, flags: [bool; 9]| ExpeditionRolePolicy {
        expedition_id: 77,
        role,
        name: name.into(),
        dominion_declare: flags[0],
        invite: flags[1],
        expel: flags[2],
        promote: flags[3],
        dismiss: flags[4],
        chat: flags[5],
        manager_chat: flags[6],
        siege_master: flags[7],
        join_siege: flags[8],
    };
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: 1,
            owner_name: "Al".into(),
            mother_id: 148,
            ..Default::default()
        },
        members: vec![expedition::member_from_character(&server, 77, &a.p, true)],
        policies: vec![policy(0, "Apprentice", [false; 9])],
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    let mut request = PacketStream::new();
    request
        .write_u32(77)
        .write_u8(0)
        .write_str("Recruit")
        .write_bool(true)
        .write_bool(true)
        .write_bool(true)
        .write_bool(true)
        .write_bool(true)
        .write_bool(true)
        .write_bool(true)
        .write_bool(true)
        .write_bool(true);
    a.send(&server, cs::CSChangeExpeditionRolePolicyPacket, request)
        .await;
    let saved: (i64, i64, i64, i64, i64, i64, i64, i64, i64, String) = sqlx::query_as(
        "SELECT CAST(dominion_declare AS SIGNED), CAST(invite AS SIGNED), CAST(expel AS SIGNED), \
         CAST(promote AS SIGNED), CAST(dismiss AS SIGNED), CAST(chat AS SIGNED), \
         CAST(manager_chat AS SIGNED), CAST(siege_master AS SIGNED), CAST(join_siege AS SIGNED), name \
         FROM expedition_role_policies WHERE expedition_id = 77 AND role = 0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(saved, (0, 1, 1, 1, 0, 0, 0, 0, 1, "Recruit".into()));
    // Reload from the temporary tables: the stored policy round-trips.
    server.expeditions.load(&pool).await.unwrap();
    assert_eq!(
        server
            .expeditions
            .get(77)
            .unwrap()
            .policy_by_role(0)
            .unwrap(),
        &policy(
            0,
            "Recruit",
            [false, true, true, true, false, false, false, false, true],
        )
    );
    for table in [
        "expedition_role_policies",
        "expedition_members",
        "expeditions",
    ] {
        sqlx::query(&format!("DROP TEMPORARY TABLE {table}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

#[tokio::test]
async fn family_create_join_leave_disband() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);

    let invite = |name: &str| {
        let mut s = PacketStream::new();
        s.write_str(name).write_str("Kid");
        s
    };
    let reply = |invitor: u32| {
        let mut s = PacketStream::new();
        s.write_u32(invitor).write_bool(true).write_str("Kid");
        s
    };
    // Bob blocked Al: the family invitation is dropped silently until he unblocks.
    b.send(&server, cs::CSAddBlockedUserPacket, name_body("Al"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAddBlockedUserPacket]);
    a.send(&server, cs::CSFamilyInviteMemberPacket, invite("Bob"))
        .await;
    assert!(a.ops().is_empty() && b.ops().is_empty());
    b.send(&server, cs::CSDeleteBlockedUserPacket, name_body("Al"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCDeleteBlockedUserPacket]);

    a.send(&server, cs::CSFamilyInviteMemberPacket, invite("Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCFamilyInvitationPacket]);
    b.send(&server, cs::CSFamilyReplyInvitationPacket, reply(1))
        .await;
    let fid = a.p.data().family;
    assert_ne!(fid, 0);
    assert_eq!(b.p.data().family, fid);
    // Both join the family chat; the created packet goes to both members.
    assert_eq!(
        a.ops(),
        vec![sc::SCJoinedChatChannelPacket, sc::SCFamilyCreatedPacket]
    );
    assert_eq!(
        b.ops(),
        vec![sc::SCJoinedChatChannelPacket, sc::SCFamilyCreatedPacket]
    );
    let f = server.systems.social.families.family(fid).unwrap();
    assert_eq!(
        f.info().members,
        vec![
            (1, "Al".into(), 1, true, "".into()),
            (2, "Bob".into(), 0, true, "Kid".into())
        ]
    );

    // Family chat.
    b.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::FAMILY, "hey"),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(b.ops(), vec![sc::SCChatMessagePacket]);

    // C joins the existing family.
    a.send(&server, cs::CSFamilyInviteMemberPacket, invite("Cy"))
        .await;
    c.ops();
    c.send(&server, cs::CSFamilyReplyInvitationPacket, reply(1))
        .await;
    assert_eq!(c.p.data().family, fid);
    assert_eq!(a.ops(), vec![sc::SCFamilyMemberAddedPacket]);
    assert_eq!(
        c.ops(),
        vec![sc::SCJoinedChatChannelPacket, sc::SCFamilyMemberAddedPacket]
    );
    b.ops();

    // Title / owner changes by the steward.
    let mut s = PacketStream::new();
    s.write_u32(3).write_str("Baby");
    a.send(&server, cs::CSFamilyChangeTitlePacket, s).await;
    assert_eq!(b.ops(), vec![sc::SCFamilyTitleChangedPacket]);
    let mut s = PacketStream::new();
    s.write_u32(2);
    a.send(&server, cs::CSFamilyChangeOwnerPacket, s).await;
    assert_eq!(
        c.ops(),
        vec![
            sc::SCFamilyTitleChangedPacket,
            sc::SCFamilyOwnerChangedPacket,
            sc::SCFamilyDescPacket
        ]
    );
    a.ops();
    b.ops();

    // Logout / login status.
    crate::systems::social::family::on_character_logout(&server, &c.p);
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCFamilyMemberOnlinePacket,
            body(
                &crate::systems::social::packets::family::SCFamilyMemberOnlinePacket {
                    family_id: fid,
                    member_id: 3,
                    online: false
                }
            )
        )]
    );
    assert_eq!(c.ops(), vec![sc::SCLeavedChatChannelPacket]);
    crate::systems::social::family::on_character_login(&server, &c.p);
    assert_eq!(
        c.ops(),
        vec![
            sc::SCJoinedChatChannelPacket,
            sc::SCFamilyDescPacket,
            sc::SCFamilyMemberOnlinePacket
        ]
    );
    a.ops();
    b.ops();

    // B (now steward) kicks C; then A leaves -> fewer than two members: disbanded.
    let mut s = PacketStream::new();
    s.write_u32(3);
    b.send(&server, cs::CSFamilyKickPacket, s).await;
    assert_eq!(c.p.data().family, 0);
    assert_eq!(
        c.ops(),
        vec![sc::SCLeavedChatChannelPacket, sc::SCFamilyRemovedPacket]
    );
    assert_eq!(a.ops(), vec![sc::SCFamilyMemberRemovedPacket]);
    b.ops();
    a.send(&server, cs::CSFamilyLeavePacket, PacketStream::new())
        .await;
    assert_eq!(
        a.ops(),
        vec![sc::SCFamilyRemovedPacket, sc::SCLeavedChatChannelPacket]
    );
    assert_eq!(
        b.ops(),
        vec![
            sc::SCFamilyMemberRemovedPacket,
            sc::SCLeavedChatChannelPacket,
            sc::SCFamilyRemovedPacket
        ]
    );
    assert_eq!((a.p.data().family, b.p.data().family), (0, 0));
    assert!(server.systems.social.families.family(fid).is_none());
}

fn family_invite_body(name: &str, title: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_str(name).write_str(title);
    s
}

fn family_reply_body(invitor: u32, join: bool, title: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(invitor).write_bool(join).write_str(title);
    s
}

#[tokio::test]
async fn family_full_refuses_invites_and_accepts() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    let fid = a.p.data().family;
    assert_ne!(fid, 0);
    assert_eq!(b.p.data().family, fid);
    a.ops();
    b.ops();
    c.ops();

    // Two members is the configured cap: the invite itself is refused.
    server
        .config
        .update(|cfg| cfg.social.family_max_members = 2);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Cy", "Kid"),
    )
    .await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_MAXIMUM)]);
    assert!(c.ops().is_empty());

    // Room opens, the invitation goes out; the family fills before the reply arrives.
    server
        .config
        .update(|cfg| cfg.social.family_max_members = 3);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Cy", "Kid"),
    )
    .await;
    assert_eq!(c.ops(), vec![sc::SCFamilyInvitationPacket]);
    server
        .config
        .update(|cfg| cfg.social.family_max_members = 2);
    c.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    assert_eq!(c.frames(), vec![error_frame(error::FAMILY_MAXIMUM)]);
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_MAXIMUM)]);
    assert_eq!(c.p.data().family, 0);
    assert_eq!(
        server
            .systems
            .social
            .families
            .family(fid)
            .unwrap()
            .members
            .len(),
        2
    );
}

#[tokio::test]
async fn family_member_cap_defaults_to_8_and_never_exceeds_the_client_limit() {
    use crate::systems::social::family::{max_members, Family, FamilyMember};
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    // The default is the client's limit: its family reader clamps the member count to 8.
    assert_eq!(max_members(&server), 8);
    // Below 2 a family could never form; above 8 the client would not list the extra members.
    for (configured, effective) in [
        (0, 2),
        (1, 2),
        (2, 2),
        (5, 5),
        (8, 8),
        (9, 8),
        (usize::MAX, 8),
    ] {
        server
            .config
            .update(|cfg| cfg.social.family_max_members = configured);
        assert_eq!(max_members(&server), effective, "configured {configured}");
    }
    // A family of 8 refuses a ninth member even though the config would allow more.
    let mut members = vec![FamilyMember {
        character: Some(a.p.clone()),
        id: 1,
        name: "Al".into(),
        role: 1,
        title: String::new(),
    }];
    members.extend((10..17).map(|id| FamilyMember {
        character: None,
        id,
        name: format!("Member{id}"),
        role: 0,
        title: "Kid".into(),
    }));
    assert_eq!(members.len(), 8);
    server.systems.social.families.insert(Family {
        id: 11,
        members,
        removed_members: Vec::new(),
    });
    a.p.data().family = 11;
    server
        .config
        .update(|cfg| cfg.social.family_max_members = 100);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_MAXIMUM)]);
    assert!(b.ops().is_empty());
}

#[tokio::test]
async fn family_titles_are_validated_like_the_client() {
    use crate::data::character_templates::BlockedName;
    use crate::systems::social::util::error;
    use crate::test_support::test_server_with;
    let mut data = crate::data::GameData::empty();
    data.characters.blocked_names = vec![BlockedName {
        text: "badword".into(),
        partial_match: true,
    }];
    let server = test_server_with(data);
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    // `VNT_FAMILY_TITLE` is `NamePolicy` type 3 (type 5 is `VNT_PORTAL`, the permissive row):
    // 1-12 letters, so no spaces, digits or punctuation (the retail ui_texts 2324 reads "Enter 1
    // to 12 characters for the title (excluding special characters)."). A character outside the
    // BMP cannot be stored in the utf8mb3 `family_members.title`, so it is refused as well, even
    // a letter: the three math letters below are only 6 UTF-16 units.
    let refused = [
        ("", error::FAMILY_TITLE),
        (" ", error::FAMILY_TITLE),
        (".Name", error::FAMILY_TITLE),
        (" Name", error::FAMILY_TITLE),
        ("Name ", error::FAMILY_TITLE),
        ("\tName", error::FAMILY_TITLE),
        ("Big Bro", error::FAMILY_TITLE),
        ("A  B!", error::FAMILY_TITLE),
        ("Big Bro 99!", error::FAMILY_TITLE),
        ("Kid99", error::FAMILY_TITLE),
        ("Kid!", error::FAMILY_TITLE),
        ("Kid-Kid", error::FAMILY_TITLE),
        ("Kid\u{1F600}", error::FAMILY_TITLE),
        ("\u{1D400}\u{1D401}\u{1D402}", error::FAMILY_TITLE),
        ("AAAAAAAAAAAAA", error::FAMILY_TITLE),
        // The word list applies to a well-formed title only.
        ("badword", error::FAMILY_TITLE_BAD),
        ("xBadWordx", error::FAMILY_TITLE_BAD),
        ("badword kid", error::FAMILY_TITLE),
    ];
    for (title, code) in refused {
        a.send(
            &server,
            cs::CSFamilyInviteMemberPacket,
            family_invite_body("Bob", title),
        )
        .await;
        assert_eq!(a.frames(), vec![error_frame(code)], "title {title:?}");
        assert!(b.ops().is_empty(), "title {title:?} reached the client");
    }
    // A 12-letter title passes (and is stored as sent).
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "ABCDEFGHIJKL"),
    )
    .await;
    assert_eq!(b.ops(), vec![sc::SCFamilyInvitationPacket]);
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "ABCDEFGHIJKL"),
    )
    .await;
    let fid = a.p.data().family;
    assert_ne!(fid, 0);
    let title_of_bob = || {
        server
            .systems
            .social
            .families
            .family(fid)
            .unwrap()
            .member(2)
            .unwrap()
            .title
            .clone()
    };
    assert_eq!(title_of_bob(), "ABCDEFGHIJKL");
    a.ops();
    b.ops();
    // The same rule applies to a title change by the steward.
    for (title, code) in refused {
        let mut body = PacketStream::new();
        body.write_u32(2).write_str(title);
        a.send(&server, cs::CSFamilyChangeTitlePacket, body).await;
        assert_eq!(a.frames(), vec![error_frame(code)], "title {title:?}");
        assert!(b.ops().is_empty(), "title {title:?} reached the client");
        assert_eq!(title_of_bob(), "ABCDEFGHIJKL", "title {title:?} was stored");
    }
    // One letter, mixed case and letters of other scripts (all in the BMP) pass.
    for title in [
        "A",
        "Bro",
        "bRo",
        "\u{411}\u{440}\u{430}\u{442}",
        "\u{B3D9}\u{C0DD}",
    ] {
        let mut body = PacketStream::new();
        body.write_u32(2).write_str(title);
        a.send(&server, cs::CSFamilyChangeTitlePacket, body).await;
        assert_eq!(
            b.ops(),
            vec![sc::SCFamilyTitleChangedPacket],
            "title {title:?}"
        );
        a.ops();
        assert_eq!(title_of_bob(), title);
    }
}

#[tokio::test]
async fn family_reply_uses_the_invitation_title() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    // The invitation was validated with "Kid"; the empty reply title is invalid on its own but
    // must not consume or kill the invitation.
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, ""),
    )
    .await;
    let fid = a.p.data().family;
    assert_ne!(fid, 0);
    assert_eq!(b.p.data().family, fid);
    assert_eq!(
        server
            .systems
            .social
            .families
            .family(fid)
            .unwrap()
            .member(2)
            .unwrap()
            .title,
        "Kid"
    );
    // The same for a different (valid) reply title on the existing-family path.
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Cy", "Son"),
    )
    .await;
    c.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Wrong"),
    )
    .await;
    assert_eq!(c.p.data().family, fid);
    assert_eq!(
        server
            .systems
            .social
            .families
            .family(fid)
            .unwrap()
            .member(3)
            .unwrap()
            .title,
        "Son"
    );
}

#[tokio::test]
async fn family_reply_without_an_invitation_answers_only_an_accept() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    // A decline of an invitation that is not (or no longer) pending needs no reply.
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, false, "Kid"),
    )
    .await;
    assert!(b.frames().is_empty() && a.frames().is_empty());
    // An accept of one is answered: the inviter is gone from the invited character's view.
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    assert_eq!(b.frames(), vec![error_frame(error::FAMILY_NO_INVITOR)]);
    assert!(a.frames().is_empty());
    assert_eq!(b.p.data().family, 0);
}

#[tokio::test]
async fn family_reply_after_the_inviter_lost_the_stewardship_tells_only_the_inviter() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    // Al and Bob found a family; Al invites Cy and hands the family to Bob before Cy answers.
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    let fid = a.p.data().family;
    assert_ne!(fid, 0);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Cy", "Kid"),
    )
    .await;
    assert_eq!(c.ops(), vec![sc::SCFamilyInvitationPacket]);
    let mut owner = PacketStream::new();
    owner.write_u32(2);
    a.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    a.ops();
    b.ops();
    c.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    // Al is no longer the steward and is told so. Cy gets no reply: "That player has
    // disconnected." (`FAMILY_NO_INVITOR`) would be wrong, the inviter is online.
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_NOT_OWNER)]);
    assert!(c.frames().is_empty() && b.frames().is_empty());
    assert_eq!(c.p.data().family, 0);
    assert_eq!(
        server
            .systems
            .social
            .families
            .family(fid)
            .unwrap()
            .members
            .len(),
        2
    );
}

#[tokio::test]
async fn family_creation_failure_answers_family_create() {
    use crate::managers::id::next_prime;
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    // Every family id is taken (the id manager starts with a `next_prime(100_000)` bit set).
    let taken: Vec<u32> = (1..=next_prime(100_000) as u32).collect();
    server.ids.family.initialize_with(&taken);
    assert!(
        server.ids.family.get_next_id().is_err(),
        "the test setup must exhaust the family ids"
    );
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    assert_eq!(b.ops(), vec![sc::SCFamilyInvitationPacket]);
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    // "Failed to create a family." (not "family does not exist"), and no family was left behind.
    assert_eq!(b.frames(), vec![error_frame(error::FAMILY_CREATE)]);
    assert!(a.frames().is_empty());
    assert_eq!((a.p.data().family, b.p.data().family), (0, 0));
    assert_eq!(server.systems.social.families.count(), 0);
}

#[tokio::test]
async fn family_invite_errors_name_the_reason() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Nobody", "Kid"),
    )
    .await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_NO_TARGET)]);
    // A registered but offline character is not in the world (the lobby name manager knows it).
    server.names.add_character(99, "Offline", 1099);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Offline", "Kid"),
    )
    .await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_INVITE_OFFLINE)]);
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Al", "Kid"),
    )
    .await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_SELF)]);

    // A family: the target already belongs to one, and a plain member cannot invite.
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    b.ops();
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    a.ops();
    b.ops();
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_OTHER_FAMILY)]);
    b.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Cy", "Kid"),
    )
    .await;
    assert_eq!(b.frames(), vec![error_frame(error::FAMILY_NOT_OWNER)]);
    assert!(c.ops().is_empty());

    // A decline ends the invitation and tells the inviter.
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Cy", "Kid"),
    )
    .await;
    assert_eq!(c.ops(), vec![sc::SCFamilyInvitationPacket]);
    c.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, false, "Kid"),
    )
    .await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_REJECT)]);
    assert!(c.ops().is_empty());
    assert_eq!(c.p.data().family, 0);
}

#[tokio::test]
async fn family_invite_trims_the_typed_name() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    // Surrounding blanks are not part of the name: the online Bob is found, not reported as
    // offline.
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body(" Bob ", "Kid"),
    )
    .await;
    assert_eq!(b.ops(), vec![sc::SCFamilyInvitationPacket]);
    assert!(a.ops().is_empty());
}

#[tokio::test]
async fn family_action_failures_answer_the_client() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    // Every action without a family answers `FAMILY_NOT_EXIST`.
    c.send(&server, cs::CSFamilyLeavePacket, PacketStream::new())
        .await;
    assert_eq!(c.frames(), vec![error_frame(error::FAMILY_NOT_EXIST)]);
    let mut kick = PacketStream::new();
    kick.write_u32(1);
    c.send(&server, cs::CSFamilyKickPacket, kick).await;
    assert_eq!(c.frames(), vec![error_frame(error::FAMILY_NOT_EXIST)]);
    let mut title = PacketStream::new();
    title.write_u32(1).write_str("Kid");
    c.send(&server, cs::CSFamilyChangeTitlePacket, title).await;
    assert_eq!(c.frames(), vec![error_frame(error::FAMILY_NOT_EXIST)]);
    let mut owner = PacketStream::new();
    owner.write_u32(1);
    c.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    assert_eq!(c.frames(), vec![error_frame(error::FAMILY_NOT_EXIST)]);

    // A two-member family: only the steward may kick, title or hand over the family.
    a.send(
        &server,
        cs::CSFamilyInviteMemberPacket,
        family_invite_body("Bob", "Kid"),
    )
    .await;
    b.ops();
    b.send(
        &server,
        cs::CSFamilyReplyInvitationPacket,
        family_reply_body(1, true, "Kid"),
    )
    .await;
    a.ops();
    b.ops();
    let mut kick = PacketStream::new();
    kick.write_u32(1);
    b.send(&server, cs::CSFamilyKickPacket, kick).await;
    assert_eq!(b.frames(), vec![error_frame(error::FAMILY_NOT_OWNER)]);
    let mut title = PacketStream::new();
    title.write_u32(1).write_str("Kid");
    b.send(&server, cs::CSFamilyChangeTitlePacket, title).await;
    assert_eq!(b.frames(), vec![error_frame(error::FAMILY_NOT_OWNER)]);
    let mut owner = PacketStream::new();
    owner.write_u32(1);
    b.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    assert_eq!(b.frames(), vec![error_frame(error::FAMILY_NOT_OWNER)]);

    let mut kick = PacketStream::new();
    kick.write_u32(1);
    a.send(&server, cs::CSFamilyKickPacket, kick).await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_KICK_SELF)]);
    let mut title = PacketStream::new();
    title.write_u32(1).write_str("Boss");
    a.send(&server, cs::CSFamilyChangeTitlePacket, title).await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_OWNER_TITLE)]);
    let mut owner = PacketStream::new();
    owner.write_u32(1);
    a.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_ALREADY_OWNER)]);
    let mut title = PacketStream::new();
    title.write_u32(999).write_str("Kid");
    a.send(&server, cs::CSFamilyChangeTitlePacket, title).await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_NO_TARGET)]);
    let mut owner = PacketStream::new();
    owner.write_u32(999);
    a.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_NO_TARGET)]);
    let mut kick = PacketStream::new();
    kick.write_u32(999);
    a.send(&server, cs::CSFamilyKickPacket, kick).await;
    assert_eq!(a.frames(), vec![error_frame(error::FAMILY_NO_TARGET)]);
}

#[tokio::test]
async fn family_kick_removes_a_member_whose_character_is_gone() {
    use crate::systems::social::family::{Family, FamilyMember};
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    let member = |character: Option<PlayerRef>, id: u32, name: &str, role: u8| FamilyMember {
        character,
        id,
        name: name.into(),
        role,
        title: "Kid".into(),
    };
    // Member 99 is not in the world and has no character row. The test server's pool has no
    // MySQL behind it, so a `Character.Load` lookup could not find it: a member must not need
    // that lookup, or it would hold a cap slot forever.
    server.systems.social.families.insert(Family {
        id: 11,
        members: vec![
            member(Some(a.p.clone()), 1, "Al", 1),
            member(Some(b.p.clone()), 2, "Bob", 0),
            member(None, 99, "Ghost", 0),
        ],
        removed_members: Vec::new(),
    });
    a.p.data().family = 11;
    b.p.data().family = 11;
    let mut kick = PacketStream::new();
    kick.write_u32(99);
    a.send(&server, cs::CSFamilyKickPacket, kick).await;
    assert_eq!(a.ops(), vec![sc::SCFamilyMemberRemovedPacket]);
    assert_eq!(b.ops(), vec![sc::SCFamilyMemberRemovedPacket]);
    let family = server.systems.social.families.family(11).unwrap();
    assert_eq!(family.members.len(), 2);
    assert!(family.member(99).is_none());
}

#[tokio::test]
async fn friends_and_blocked_users() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    let name = |n: &str| {
        let mut s = PacketStream::new();
        s.write_str(n);
        s
    };
    a.send(&server, cs::CSAddFriendPacket, name("Bob")).await;
    let f = a.frames();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, sc::SCAddFriendPacket);
    assert_eq!(a.p.data().friends.len(), 1);
    assert_eq!(server.friends.owners_of(2), vec![1]);
    // Adding twice is refused with 502 (INFERRED id: the client has no dedicated text).
    a.send(&server, cs::CSAddFriendPacket, name("Bob")).await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddFriendPacket,
            body(&crate::systems::social::packets::friend::SCAddFriendPacket::failure(502, "Bob"))
        )]
    );
    assert_eq!(a.p.data().friends.len(), 1);
    // B's status changes reach A.
    crate::world::session::set_online(&server, &b.p, false);
    assert_eq!(a.ops(), vec![sc::SCFriendStatusChangedPacket]);
    crate::world::session::set_online(&server, &b.p, true);
    a.ops();
    a.send(&server, cs::CSDeleteFriendPacket, name("Bob")).await;
    assert_eq!(a.ops(), vec![sc::SCDeleteFriendPacket]);
    assert!(a.p.data().friends.is_empty());
    assert_eq!(a.p.data().removed_friends, vec![2]);
    assert!(server.friends.owners_of(2).is_empty());

    a.send(&server, cs::CSAddBlockedUserPacket, name("bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(
                &crate::systems::social::packets::friend::SCAddBlockedUserPacket {
                    character_id: 2,
                    character_name: "Bob".into(),
                    success: true,
                    error_message: 0
                }
            )
        )]
    );
    a.send(&server, cs::CSDeleteBlockedUserPacket, name("Bob"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCDeleteBlockedUserPacket]);
    assert_eq!(a.p.data().removed_blocked, vec![2]);
    // An unknown name is answered with 551.
    a.send(&server, cs::CSAddBlockedUserPacket, name("Nobody"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(
                &crate::systems::social::packets::friend::SCAddBlockedUserPacket::failure(
                    551, 0, "Nobody"
                )
            )
        )]
    );
    let _ = b.ops();
}

/// Every direct player-to-player delivery path checks the recipient's block list, and
/// unblocking restores delivery. The blocked side is silent: 1.2 has no "target blocked you"
/// error (capture-pending, case `BLOCK-LIST-1`).
#[tokio::test]
async fn blocked_sender_is_silently_dropped_on_every_direct_path() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    // Within the 5 m trade range the unblocked trade request needs (`TradeManager` range check).
    let mut b = pc(&server, 2, "Bob", 102.0, 100.0);
    // A dead target is refused before the block check (duel and trade alike).
    a.p.data().unit.hp = 100;
    b.p.data().unit.hp = 100;
    settle(&mut [&mut a, &mut b]);

    a.send(&server, cs::CSAddBlockedUserPacket, name_body("Bob"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCAddBlockedUserPacket]);

    // Whisper: no packet reaches either side.
    b.send(&server, cs::CSSendChatMessagePacket, whisper("Al", "hi"))
        .await;
    assert!(a.frames().is_empty() && b.frames().is_empty());

    // Party invite (named; area invites call the same path).
    b.send(&server, cs::CSInviteToTeamPacket, invite(0, "Al"))
        .await;
    assert!(a.frames().is_empty() && b.frames().is_empty());

    // Family invite.
    let family_invite = |name: &str| {
        let mut s = PacketStream::new();
        s.write_str(name).write_str("Kid");
        s
    };
    b.send(&server, cs::CSFamilyInviteMemberPacket, family_invite("Al"))
        .await;
    assert!(a.frames().is_empty() && b.frames().is_empty());

    // Friend add: the adder gets the unknown-user failure (551), the blocker nothing.
    b.send(&server, cs::CSAddFriendPacket, name_body("Al"))
        .await;
    assert!(a.frames().is_empty());
    assert_eq!(b.frames().len(), 1);
    assert!(b.p.data().friends.is_empty());

    // Trade request.
    let mut obj = PacketStream::new();
    obj.write_bc(a.p.obj_id);
    b.send(&server, cs::CSCanStartTradePacket, obj).await;
    assert!(a.frames().is_empty() && b.frames().is_empty());

    // Duel request (challenged character id, plain u32).
    let mut id = PacketStream::new();
    id.write_u32(a.p.character_id);
    b.send(&server, cs::CSChallengeDuelPacket, id).await;
    assert!(a.frames().is_empty() && b.frames().is_empty());

    // Unblock: every path delivers again.
    a.send(&server, cs::CSDeleteBlockedUserPacket, name_body("Bob"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCDeleteBlockedUserPacket]);

    b.send(&server, cs::CSSendChatMessagePacket, whisper("Al", "hi"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(b.ops(), vec![sc::SCChatMessagePacket]);

    b.send(&server, cs::CSInviteToTeamPacket, invite(0, "Al"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCAskToJoinTeamPacket]);

    b.send(&server, cs::CSFamilyInviteMemberPacket, family_invite("Al"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCFamilyInvitationPacket]);

    b.send(&server, cs::CSAddFriendPacket, name_body("Al"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAddFriendPacket]);
    assert_eq!(b.p.data().friends.len(), 1);

    let mut obj = PacketStream::new();
    obj.write_bc(a.p.obj_id);
    b.send(&server, cs::CSCanStartTradePacket, obj).await;
    assert_eq!(a.ops(), vec![sc::SCCanStartTradePacket]);

    let mut id = PacketStream::new();
    id.write_u32(a.p.character_id);
    b.send(&server, cs::CSChallengeDuelPacket, id).await;
    assert_eq!(a.ops(), vec![sc::SCDuelChallengedPacket]);
}

/// Blocking also resolves offline characters through `NameManager`, so the added reply and the
/// persisted list stay consistent with the login `SCBlockedUsers` snapshot.
#[tokio::test]
async fn blocking_an_offline_character_uses_the_name_manager() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    settle(&mut [&mut a]);
    server.names.add_character(9, "Offline", 1009);

    a.send(&server, cs::CSAddBlockedUserPacket, name_body("offline"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(
                &crate::systems::social::packets::friend::SCAddBlockedUserPacket {
                    character_id: 9,
                    character_name: "Offline".into(),
                    success: true,
                    error_message: 0
                }
            )
        )]
    );
    assert_eq!(a.p.data().blocked, vec![(1, 9)]);

    a.send(&server, cs::CSDeleteBlockedUserPacket, name_body("OFFLINE"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCDeleteBlockedUserPacket]);
    assert!(a.p.data().blocked.is_empty());
    assert_eq!(a.p.data().removed_blocked, vec![9]);

    // An unregistered name is answered with 551.
    a.send(&server, cs::CSAddBlockedUserPacket, name_body("Nobody"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(
                &crate::systems::social::packets::friend::SCAddBlockedUserPacket::failure(
                    551, 0, "Nobody"
                )
            )
        )]
    );
}

#[tokio::test]
async fn team_area_invite_sends_the_area_packet_to_eligible_players_only() {
    use crate::data::world_data::SystemFaction;
    use crate::data::GameData;
    use crate::systems::social::faction::{self, relation_state, FactionRelations};
    use crate::test_support::test_server_with;

    // Faction 101 (mother 148) vs 102 (mother 149) are hostile; the default test faction 101
    // is friendly to itself.
    let mut data = GameData::empty();
    for (id, mother) in [(101, 148), (102, 149), (148, 0), (149, 0)] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..Default::default()
        });
    }
    data.systems.social.relations =
        FactionRelations::from_rows(&[(148, 149, 1)]).expect("relations");
    let server = test_server_with(data);
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 150.0, 100.0);
    let mut d = pc(&server, 4, "Di", 90.0, 90.0);
    let mut far = pc(&server, 5, "Far", 300.0, 100.0);
    let mut hostile = pc(&server, 6, "Hos", 120.0, 100.0);
    let mut blocked = pc(&server, 7, "Blk", 130.0, 100.0);
    hostile.p.data().unit.faction_id = 102;
    assert_eq!(
        faction::player_relation(&server, &a.p, &hostile.p),
        relation_state::HOSTILE
    );
    blocked.p.data().blocked.push((7, 1));
    settle(&mut [
        &mut a,
        &mut b,
        &mut c,
        &mut d,
        &mut far,
        &mut hostile,
        &mut blocked,
    ]);

    // A and B form a party; B is already in a team and cannot be area-invited.
    let team_id = form_party(&server, &mut a, &mut b).await;
    settle(&mut [
        &mut a,
        &mut b,
        &mut c,
        &mut d,
        &mut far,
        &mut hostile,
        &mut blocked,
    ]);

    let mut s = PacketStream::new();
    s.write_u32(team_id).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;

    let area = body(&SCAskToJoinTeamAreaPacket {
        team_id,
        sender_id: 1,
        sender_name: "Al".into(),
        is_party: true,
    });
    for x in [&mut c, &mut d] {
        assert_eq!(
            x.frames(),
            vec![(sc::SCAskToJoinTeamAreaPacket, area.clone())]
        );
    }
    // In the party, 200 m away, hostile and blocked: no invitation.
    for x in [&mut b, &mut far, &mut hostile, &mut blocked] {
        assert!(x.ops().is_empty(), "{:?}", x.ops());
    }
    // The success reply carries r = 0 (the client ignores r when s is true).
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCTeamAreaInvitedPacket,
            body(&SCTeamAreaInvitedPacket { r: 0, s: true })
        )]
    );

    // Accepting an area invitation runs the normal join path.
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(team_id, 1, false),
    )
    .await;
    assert!(c.p.data().in_party);
    let team = server
        .systems
        .social
        .teams
        .active_team(team_id)
        .expect("team");
    assert_eq!(team.members_count(), 3);
    assert!(team.is_member(3));
    assert!(c.ops().contains(&sc::SCJoinedTeamPacket));
    a.ops();
    b.ops();

    // D declines; the owner sees the rejection with the target's name.
    d.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(team_id, 1, true),
    )
    .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCRejectedTeamPacket,
            body(&SCRejectedTeamPacket {
                name: "Di".into(),
                party: true
            })
        )]
    );
}

#[tokio::test]
async fn team_area_invite_in_a_raid_sends_the_raid_flag() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 150.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    let mut s = PacketStream::new();
    s.write_u32(team_id);
    a.send(&server, cs::CSConvertToRaidTeamPacket, s).await;
    settle(&mut [&mut a, &mut b, &mut c]);

    // The request says party, but the team is a raid: the invitation carries the team's flag.
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    assert_eq!(
        c.frames(),
        vec![(
            sc::SCAskToJoinTeamAreaPacket,
            body(&SCAskToJoinTeamAreaPacket {
                team_id,
                sender_id: 1,
                sender_name: "Al".into(),
                is_party: false,
            })
        )]
    );
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCTeamAreaInvitedPacket,
            body(&SCTeamAreaInvitedPacket { r: 0, s: true })
        )]
    );
}

/// The error codes of the invitation paths (research `area-invite.md`): 72 to the inviter,
/// 77 to the inviter for a target already in a team, 79 for a team that is gone, 78 for an
/// offline inviter, 82 to an invitee already in a team, and silence when no invitation exists.
#[tokio::test]
async fn team_invite_errors_follow_the_client_codes() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 100.0, 100.0);
    let mut d = pc(&server, 4, "Di", 120.0, 100.0);
    let mut e = pc(&server, 5, "Ed", 130.0, 100.0);
    let mut f = pc(&server, 6, "Fl", 140.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c, &mut d, &mut e, &mut f]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    settle(&mut [&mut a, &mut b, &mut c, &mut d, &mut e, &mut f]);

    // A non-owner party member may not invite: TEAM_NO_RIGHTS (72) to the inviter, direct and
    // area (the area permission is checked once).
    b.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    expect_chat_error(&mut b, error::TEAM_NO_RIGHTS);
    assert!(c.ops().is_empty());
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_bool(true);
    b.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    expect_chat_error(&mut b, error::TEAM_NO_RIGHTS);
    assert!(c.ops().is_empty());

    // An accept without a pending invitation reports TEAM_JOIN; a reject stays silent.
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(team_id, 1, false),
    )
    .await;
    expect_chat_error(&mut c, error::TEAM_JOIN);
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(team_id, 1, true),
    )
    .await;
    assert!(c.ops().is_empty());

    // E and F form their own party U; E invites C, then dismisses U: TEAM_INVITOR_MOVED (79).
    e.send(&server, cs::CSInviteToTeamPacket, invite(0, "Fl"))
        .await;
    f.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 5, false))
        .await;
    let u = server
        .systems
        .social
        .teams
        .active_team_by_unit(e.p.character_id)
        .expect("team")
        .id;
    e.send(&server, cs::CSInviteToTeamPacket, invite(u, "Cy"))
        .await;
    c.ops();
    e.send(
        &server,
        cs::CSAskRiskyTeamActionPacket,
        risky(u, 0, risky_action::DISMISS),
    )
    .await;
    settle(&mut [&mut a, &mut b, &mut c, &mut d, &mut e, &mut f]);
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(u, e.p.character_id, false),
    )
    .await;
    expect_chat_error(&mut c, error::TEAM_INVITOR_MOVED);

    // The inviter is offline at reply: TEAM_INVITOR_OFFLINE (78). A logout removes the pending
    // invitations; this state is a race and is forced directly.
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    c.ops();
    a.p.data().is_online = false;
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(team_id, 1, false),
    )
    .await;
    expect_chat_error(&mut c, error::TEAM_INVITOR_OFFLINE);
    a.p.data().is_online = true;

    // The invitee joins another team before answering: TEAM_JOIN_ALONE (82).
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    c.ops();
    server
        .systems
        .social
        .teams
        .create_solo_team(&server, &c.p, true);
    c.ops();
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(team_id, 1, false),
    )
    .await;
    expect_chat_error(&mut c, error::TEAM_JOIN_ALONE);
    expect_chat_error(&mut a, error::TEAM_INVITEE_IN_TEAM);

    // Inviting a target who is already in a team: TEAM_INVITEE_IN_TEAM (77) to the inviter.
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    expect_chat_error(&mut a, error::TEAM_INVITEE_IN_TEAM);

    // A solo team cannot be created on top of an existing one: TEAM_JOIN_ALONE (82).
    server
        .systems
        .social
        .teams
        .create_solo_team(&server, &c.p, true);
    expect_chat_error(&mut c, error::TEAM_JOIN_ALONE);
}

#[tokio::test]
async fn team_owner_logout_drops_pending_invitations() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);
    crate::world::session::set_online(&server, &a.p, false);
    // The logout drops the invitation, tells the invitee that the inviter is gone and closes
    // its popup; a late accept without a pending invitation gets TEAM_JOIN.
    assert_eq!(
        b.frames(),
        vec![
            error_frame(crate::systems::social::util::error::TEAM_INVITOR_OFFLINE),
            (
                sc::SCInvitationCanceledPacket,
                vec![invitation_canceled_kind::TEAM]
            )
        ]
    );
    b.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, false))
        .await;
    expect_chat_error(&mut b, crate::systems::social::util::error::TEAM_JOIN);
    crate::world::session::set_online(&server, &a.p, true);
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);
}

/// A stale popup of another inviter must not consume (or reject) the pending invitation.
#[tokio::test]
async fn stale_team_reply_does_not_consume_another_invitation() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 100.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);

    // A area-invites C, then B's named invitation replaces the pending area invitation.
    let mut s = PacketStream::new();
    s.write_u32(0).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    assert_eq!(c.ops(), vec![sc::SCAskToJoinTeamAreaPacket]);
    a.ops();
    b.send(&server, cs::CSInviteToTeamPacket, invite(0, "Cy"))
        .await;
    assert_eq!(c.ops(), vec![sc::SCAskToJoinTeamPacket]);

    // The old area popup answers with A's id (accept and reject): ignored, B's invite stays.
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(0, 1, false),
    )
    .await;
    assert!(c.ops().is_empty());
    c.send(&server, cs::CSReplyToJoinTeamPacket, reply_area(0, 1, true))
        .await;
    assert!(c.ops().is_empty());
    assert!(
        a.ops().is_empty(),
        "the stale reject must not reach B's inviter"
    );

    // B's own popup still joins B's new team.
    c.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 2, false))
        .await;
    assert!(c.p.data().in_party);
    let team = server
        .systems
        .social
        .teams
        .active_team_by_unit(b.p.character_id)
        .expect("team");
    assert_eq!(team.owner_id, b.p.character_id);
    assert!(team.is_member(c.p.character_id));
}

/// A reply must name the invitation's team (when it carries one) and the invitation's owner.
#[tokio::test]
async fn team_reply_must_match_the_invitation() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 100.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    settle(&mut [&mut a, &mut b, &mut c]);

    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    c.ops();
    // A different team id is ignored and leaves the invitation in place.
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(team_id + 1, 1, false),
    )
    .await;
    assert!(c.ops().is_empty());
    assert!(!c.p.data().in_party);
    // The matching reply joins.
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(team_id, 1, false),
    )
    .await;
    assert!(c.p.data().in_party);
    assert!(server
        .systems
        .social
        .teams
        .active_team(team_id)
        .is_some_and(|t| t.is_member(3)));
}

/// A crafted client team id must not invite into another team: the invitation belongs to the
/// inviter's own team (none here), so accepting creates C's new team.
#[tokio::test]
async fn team_client_team_id_cannot_invite_into_another_team() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 100.0, 100.0);
    let mut d = pc(&server, 4, "Di", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);

    c.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Di"))
        .await;
    assert_eq!(
        d.frames(),
        vec![(
            sc::SCAskToJoinTeamPacket,
            body(&SCAskToJoinTeamPacket {
                team_id: 0,
                sender_id: 3,
                sender_name: "Cy".into(),
                is_party: true,
            })
        )]
    );
    d.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 3, false))
        .await;
    let own = server
        .systems
        .social
        .teams
        .active_team_by_unit(c.p.character_id)
        .expect("C's team");
    assert_eq!(own.owner_id, c.p.character_id);
    assert!(own.is_member(d.p.character_id));
    let other = server
        .systems
        .social
        .teams
        .active_team(team_id)
        .expect("A's team");
    assert_eq!(other.members_count(), 2);
    assert!(!other.is_member(3) && !other.is_member(4));
}

/// The owner's rights are re-checked when the invitee answers.
#[tokio::test]
async fn team_accepted_invite_rechecks_the_owner_rights() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 100.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    settle(&mut [&mut a, &mut b, &mut c]);

    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    c.ops();
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_u32(2);
    a.send(&server, cs::CSMakeTeamOwnerPacket, s).await;
    settle(&mut [&mut a, &mut b, &mut c]);
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(team_id, 1, false),
    )
    .await;
    // A is neither the owner nor a marked raid member any more.
    expect_chat_error(&mut a, error::TEAM_NO_RIGHTS);
    assert!(!c.p.data().in_party);
    assert!(!server
        .systems
        .social
        .teams
        .active_team(team_id)
        .unwrap()
        .is_member(3));
}

#[tokio::test]
async fn team_area_invite_cooldown_replies_with_the_remaining_ms() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut c = pc(&server, 2, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut c]);

    let mut s = PacketStream::new();
    s.write_u32(0).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    assert_eq!(c.ops(), vec![sc::SCAskToJoinTeamAreaPacket]);
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCTeamAreaInvitedPacket,
            body(&SCTeamAreaInvitedPacket { r: 0, s: true })
        )]
    );
    // A second request inside the 30 s window: s = false and r = remaining milliseconds.
    let mut s = PacketStream::new();
    s.write_u32(0).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    let frames = a.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, sc::SCTeamAreaInvitedPacket);
    let mut p = PacketStream::from_bytes(frames[0].1.clone());
    let remaining = p.read_u32().unwrap();
    assert!(!p.read_bool().unwrap());
    assert!(
        remaining > 0 && remaining <= crate::systems::social::team::AREA_INVITE_COOLDOWN_MS as u32,
        "{remaining}"
    );
    // No further popup for C.
    assert!(c.ops().is_empty());
}

#[tokio::test]
async fn team_area_invite_replaces_a_pending_area_invitation_and_skips_named() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    let mut d = pc(&server, 4, "Di", 150.0, 100.0);
    // E is outside A's 100 m radius but within range of C and D.
    let mut e = pc(&server, 5, "Ed", 210.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c, &mut d, &mut e]);
    let _ = form_party(&server, &mut a, &mut b).await;
    settle(&mut [&mut a, &mut b, &mut c, &mut d, &mut e]);

    // A area-invites C and D.
    let mut s = PacketStream::new();
    s.write_u32(0).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    assert_eq!(c.ops(), vec![sc::SCAskToJoinTeamAreaPacket]);
    assert_eq!(d.ops(), vec![sc::SCAskToJoinTeamAreaPacket]);
    a.ops();

    // A named invitation replaces C's pending AREA invitation.
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Cy"))
        .await;
    assert_eq!(c.ops(), vec![sc::SCAskToJoinTeamPacket]);
    assert!(a.ops().is_empty(), "no empty-name rejection");

    // E (not in a team) area-invites: C's pending named invite is skipped silently, D's stale
    // area invite is replaced by E's.
    let mut s = PacketStream::new();
    s.write_u32(0).write_bool(true);
    e.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    assert_eq!(
        d.ops(),
        vec![sc::SCAskToJoinTeamAreaPacket],
        "E's area invite replaces A's"
    );
    assert!(c.ops().is_empty(), "pending named invite is skipped");
    assert_eq!(
        e.frames(),
        vec![(
            sc::SCTeamAreaInvitedPacket,
            body(&SCTeamAreaInvitedPacket { r: 0, s: true })
        )]
    );

    // C joins A through the named invitation.
    c.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, false))
        .await;
    assert!(c.p.data().in_party);
    assert!(server
        .systems
        .social
        .teams
        .active_team_by_unit(a.p.character_id)
        .is_some_and(|t| t.is_member(c.p.character_id)));

    // D joins E through the replacement area invitation.
    d.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply_area(0, e.p.character_id, false),
    )
    .await;
    assert!(d.p.data().in_party);
    assert!(server
        .systems
        .social
        .teams
        .active_team_by_unit(e.p.character_id)
        .is_some_and(|t| t.is_member(d.p.character_id)));
}

#[tokio::test]
async fn expired_team_invitations_are_swept() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    let teams = &server.systems.social.teams;

    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);

    // After 60 s the 500 ms tick drops the pending invitation, so a new one is accepted.
    let expired_at = crate::systems::social::util::now_ms() + INVITATION_TIMEOUT_MS + 1;
    teams.update_all_teams(&server, expired_at);
    // The sweep also closes the invitee's popup (`SCInvitationCanceled`).
    assert_eq!(
        b.frames(),
        vec![(
            sc::SCInvitationCanceledPacket,
            vec![invitation_canceled_kind::TEAM]
        )]
    );
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);

    // A reply once the invitation has been swept has no pending invitation any more, so the
    // accept gets TEAM_JOIN. The second invitation was issued after `expired_at` was computed,
    // so sweep past its own deadline.
    let expired_at = crate::systems::social::util::now_ms() + INVITATION_TIMEOUT_MS + 1;
    teams.update_all_teams(&server, expired_at);
    // The second sweep closes the second invitation's popup too.
    assert_eq!(
        b.frames(),
        vec![(
            sc::SCInvitationCanceledPacket,
            vec![invitation_canceled_kind::TEAM]
        )]
    );
    b.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, false))
        .await;
    expect_chat_error(&mut b, crate::systems::social::util::error::TEAM_JOIN);
}

/// Guild invitations must be pending and unexpired, a member cannot join a second guild, and
/// expel / role changes need a strictly higher rank than the target (deviations from C#).
#[tokio::test]
async fn guild_invite_and_rank_guards() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::{self, Expedition};

    let server = test_server();
    let mut leader = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut cmdr = pc(&server, 2, "Cmdr", 101.0, 100.0);
    let mut cmdr2 = pc(&server, 3, "Cmdr2", 102.0, 100.0);
    let mut off = pc(&server, 4, "Off", 103.0, 100.0);
    let mut out = pc(&server, 9, "Out", 104.0, 100.0);
    let mut other = pc(&server, 10, "Other", 105.0, 100.0);
    for p in [
        &mut leader,
        &mut cmdr,
        &mut cmdr2,
        &mut off,
        &mut out,
        &mut other,
    ] {
        p.c.drain();
    }
    let build = |id: u32, name: &str, members: Vec<(&Pc, u8)>| {
        let members = members
            .into_iter()
            .map(|(m, role)| {
                m.p.data().unit.expedition_id = id;
                let mut x = expedition::member_from_character(&server, id, &m.p, false);
                x.role = role;
                x
            })
            .collect();
        let mut policies = expedition::default_policies(&server, id);
        for p in policies.iter_mut().filter(|p| p.role == 3) {
            p.promote = true;
        }
        server.expeditions.add(Expedition {
            faction: SystemFaction {
                id,
                name: name.into(),
                owner_id: 1,
                owner_name: "Lead".into(),
                mother_id: 101,
                ..Default::default()
            },
            members,
            policies,
            removed_members: Vec::new(),
            is_disbanded: false,
        });
    };
    build(
        77,
        "Knights",
        vec![(&leader, 255), (&cmdr, 3), (&cmdr2, 3), (&off, 2)],
    );
    build(88, "Rivals", vec![(&other, 255)]);
    let reply = |id: u32| {
        let mut s = PacketStream::new();
        s.write_u32(id).write_u32(1).write_bool(true);
        s
    };

    // An invited player who blocked the leader gets no guild invitation; unblocking restores
    // delivery (capture-pending, case `BLOCK-LIST-1`).
    out.send(&server, cs::CSAddBlockedUserPacket, name_body("Lead"))
        .await;
    assert_eq!(out.ops(), vec![sc::SCAddBlockedUserPacket]);
    let mut invite_out = PacketStream::new();
    invite_out.write_str("Out");
    leader
        .send(&server, cs::CSInviteToExpeditionPacket, invite_out)
        .await;
    assert!(out.ops().is_empty() && leader.ops().is_empty());
    out.send(&server, cs::CSDeleteBlockedUserPacket, name_body("Lead"))
        .await;
    assert_eq!(out.ops(), vec![sc::SCDeleteBlockedUserPacket]);

    // No invitation at all: the reply joins nothing.
    out.send(&server, cs::CSReplyExpeditionInvitationPacket, reply(77))
        .await;
    assert_eq!(out.p.data().unit.expedition_id, 0);
    assert_eq!(server.expeditions.get(77).unwrap().members.len(), 4);

    // Invited to 77 but answering for 88: refused, and the invitation is consumed.
    let mut s = PacketStream::new();
    s.write_str("Out");
    leader
        .send(&server, cs::CSInviteToExpeditionPacket, s.clone())
        .await;
    out.send(&server, cs::CSReplyExpeditionInvitationPacket, reply(88))
        .await;
    assert_eq!(out.p.data().unit.expedition_id, 0);
    out.send(&server, cs::CSReplyExpeditionInvitationPacket, reply(77))
        .await;
    assert_eq!(out.p.data().unit.expedition_id, 0);

    // A member of another guild cannot be pulled in with a stale / forged reply.
    other
        .send(&server, cs::CSReplyExpeditionInvitationPacket, reply(77))
        .await;
    assert_eq!(other.p.data().unit.expedition_id, 88);
    assert_eq!(server.expeditions.get(88).unwrap().members.len(), 1);

    // An expired invitation is refused.
    leader
        .send(&server, cs::CSInviteToExpeditionPacket, s.clone())
        .await;
    server
        .expeditions
        .invitations
        .lock()
        .unwrap()
        .get_mut(&9)
        .unwrap()
        .time_ms -= 61_000;
    out.send(&server, cs::CSReplyExpeditionInvitationPacket, reply(77))
        .await;
    assert_eq!(out.p.data().unit.expedition_id, 0);

    // A legitimate invitation still works.
    leader
        .send(&server, cs::CSInviteToExpeditionPacket, s)
        .await;
    out.send(&server, cs::CSReplyExpeditionInvitationPacket, reply(77))
        .await;
    assert_eq!(out.p.data().unit.expedition_id, 77);
    assert_eq!(server.expeditions.get(77).unwrap().members.len(), 5);

    // Kick: the leader and an equal rank are protected, a lower rank is not.
    let kick = |id: u32| {
        let mut s = PacketStream::new();
        s.write_u32(id);
        s
    };
    cmdr.send(&server, cs::CSKickFromExpeditionPacket, kick(1))
        .await;
    cmdr.send(&server, cs::CSKickFromExpeditionPacket, kick(3))
        .await;
    assert_eq!(server.expeditions.get(77).unwrap().members.len(), 5);
    assert_eq!(leader.p.data().unit.expedition_id, 77);
    cmdr.send(&server, cs::CSKickFromExpeditionPacket, kick(4))
        .await;
    assert_eq!(off.p.data().unit.expedition_id, 0);
    assert_eq!(server.expeditions.get(77).unwrap().members.len(), 4);

    // The leader cannot kick itself through the client packet either: the strict-rank guard is
    // only lifted for the server-side faction change (`leave_forced`).
    leader
        .send(&server, cs::CSKickFromExpeditionPacket, kick(1))
        .await;
    assert_eq!(server.expeditions.get(77).unwrap().members.len(), 4);
    assert_eq!(leader.p.data().unit.expedition_id, 77);
    assert!(!server.expeditions.get(77).unwrap().is_disbanded);

    // Role changes: not on the leader or an equal rank; a lower rank is fine.
    let role = |new_role: u8, id: u32| {
        let mut s = PacketStream::new();
        s.write_u8(new_role).write_u32(id);
        s
    };
    cmdr.send(&server, cs::CSChangeExpeditionMemberRolePacket, role(0, 1))
        .await;
    cmdr.send(&server, cs::CSChangeExpeditionMemberRolePacket, role(0, 3))
        .await;
    let e = server.expeditions.get(77).unwrap();
    assert_eq!(
        (e.member(1).unwrap().role, e.member(3).unwrap().role),
        (255, 3)
    );
    cmdr.send(&server, cs::CSChangeExpeditionMemberRolePacket, role(1, 9))
        .await;
    assert_eq!(
        server.expeditions.get(77).unwrap().member(9).unwrap().role,
        1
    );
}

/// The loot master of a team must be a member of it.
#[tokio::test]
async fn loot_master_must_be_a_team_member() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    let rule = |master: u32| {
        let mut s = PacketStream::new();
        s.write_u32(team_id)
            .write_u8(2)
            .write_u8(4)
            .write_u32(master)
            .write_bool(false)
            .write_u8(0x05);
        s
    };
    a.send(&server, cs::CSChangeLootingRulePacket, rule(3))
        .await;
    let master = || {
        server
            .systems
            .social
            .teams
            .active_team(team_id)
            .unwrap()
            .looting_rule
            .loot_master
    };
    assert_eq!(master(), 0);
    a.send(&server, cs::CSChangeLootingRulePacket, rule(2))
        .await;
    assert_eq!(master(), 2);
}

/// PIRATE-1 persistence: the forced owner transfer writes the new owner / role, clears the old
/// owner's `characters.expedition_id` and writes the nation's new king in the same transaction
/// (no later nation save). One connection-local temporary table per touched table, so shared
/// MySQL3317 keeps no rows.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary tables"]
async fn private_mysql_pirate_owner_transfer_persists_the_new_owner_and_nation_king() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::achievement_persistence::temporary_shadow_definition;
    use crate::systems::social::expedition::{self, Expedition};
    use crate::systems::social::nation::Nation;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    for table in [
        "expeditions",
        "expedition_members",
        "expedition_role_policies",
        "characters",
        "nations",
    ] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(&temporary_shadow_definition(&ddl))
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut server = test_server();
    server.db = pool.clone();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    a.p.data().unit.expedition_id = 77;
    b.p.data().unit.expedition_id = 77;
    let members = [(&a.p, 255u8), (&b.p, 3u8)]
        .into_iter()
        .map(|(p, role)| {
            let mut m = expedition::member_from_character(&server, 77, p, false);
            m.role = role;
            m
        })
        .collect();
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: 1,
            owner_name: "Al".into(),
            mother_id: 101,
            ..Default::default()
        },
        members,
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    server.systems.social.nations.add(Nation {
        id: 170,
        expedition_id: 77,
        owner_character_id: 1,
        owner_name: "Al".into(),
        name: "Test Nation".into(),
        created_at: 0,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    });
    // The production row exists from the independence save; seed the shadow so the immediate
    // owner write has a row to update.
    sqlx::query(
        "REPLACE INTO nations(`id`,`expedition_id`,`owner_character_id`,`owner_name`,`name`,\
         `created_at`,`allow_change_name`) VALUES (170,77,1,'Al','Test Nation',0,0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    expedition::leave_forced(&server, &a.p).await;

    let saved: (i64, String) =
        sqlx::query_as("SELECT CAST(owner AS SIGNED),owner_name FROM expeditions WHERE id=77")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved, (2, "Bob".into()));
    let roles: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT CAST(character_id AS SIGNED),CAST(role AS SIGNED) FROM expedition_members ORDER BY character_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(roles, vec![(2, 255)]);
    let king: (i64, String) = sqlx::query_as(
        "SELECT CAST(owner_character_id AS SIGNED),owner_name FROM nations WHERE id=170",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(king, (2, "Bob".into()));

    for table in [
        "nations",
        "characters",
        "expedition_role_policies",
        "expedition_members",
        "expeditions",
    ] {
        sqlx::query(&format!("DROP TEMPORARY TABLE {table}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

/// PIRATE-1 persistence for the last member: the dismissal deletes the guild row, clears the
/// nation king and writes the released dominion (owner 0, cancelled siege) in the same
/// transaction. One connection-local temporary table per touched table.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary tables"]
async fn private_mysql_pirate_last_member_dismissal_persists_nation_and_dominion() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::housing::dominion::state::{
        DominionState, SiegeOutcome, SiegeState, SiegeStatus, TowerIdentity,
    };
    use crate::systems::social::achievement_persistence::temporary_shadow_definition;
    use crate::systems::social::expedition::{self, Expedition};
    use crate::systems::social::nation::Nation;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    for table in [
        "expeditions",
        "expedition_members",
        "expedition_role_policies",
        "characters",
        "nations",
        "dominions",
        "dominion_siege_participants",
    ] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(&temporary_shadow_definition(&ddl))
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut server = test_server();
    server.db = pool.clone();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    settle(&mut [&mut a]);
    a.p.data().unit.expedition_id = 77;
    let mut member = expedition::member_from_character(&server, 77, &a.p, false);
    member.role = 255;
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: 1,
            owner_name: "Al".into(),
            mother_id: 101,
            ..Default::default()
        },
        members: vec![member],
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    server.systems.social.nations.add(Nation {
        id: 170,
        expedition_id: 77,
        owner_character_id: 1,
        owner_name: "Al".into(),
        name: "Test Nation".into(),
        created_at: 0,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    });
    sqlx::query(
        "REPLACE INTO nations(`id`,`expedition_id`,`owner_character_id`,`owner_name`,`name`,\
         `created_at`,`allow_change_name`) VALUES (170,77,1,'Al','Test Nation',0,0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .systems
        .housing
        .dominions
        .restore(vec![DominionState {
            zone_group_id: 25,
            tower: TowerIdentity {
                house_id: 61,
                instance_id: 0,
                guard_tower_setting_id: 7,
            },
            owner_expedition_id: 77,
            ownership_generation: 1,
            reign_start_at: 100,
            claimed_at: 0,
            last_siege_end_at: 0,
            tax_rate: 0,
            national_tax_rate: 0,
            last_paid_at: 0,
            last_tax_rate_changed_at: 0,
            last_national_tax_rate_changed_at: 0,
            national_monument_db_id: 0,
            national_monument_x: 0.0,
            national_monument_y: 0.0,
            national_monument_z: 0.0,
            ephemeral: false,
            non_pvp_start_at: 0,
            non_pvp_duration_min: 0,
            treasury: Default::default(),
            tower_destroyed: false,
            tower_hp: None,
            last_siege_id: 1,
            declaration: None,
            auction_posted_cycle: 0,
            siege: Some(SiegeState {
                siege_id: 1,
                attacker_expedition_id: 88,
                defender_expedition_id: 77,
                ownership_generation: 1,
                status: SiegeStatus::Scheduled,
                outcome: SiegeOutcome::Pending,
                scheduled_at: 100,
                starts_at: 200,
                ends_at: 300,
                ended_at: 0,
                reinforced: false,
                defender_ni: 100,
                defender_nr: 100,
                outcome_credited: false,
                materialised_from_declaration: false,
                calendar_cycle: 0,
                participants: Default::default(),
            }),
        }])
        .expect("test dominion");

    expedition::leave_forced(&server, &a.p).await;

    let guilds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM expeditions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(guilds, 0, "the dismissed guild row is deleted");
    let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM expedition_members")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(members, 0);
    let king: (i64, String) = sqlx::query_as(
        "SELECT CAST(owner_character_id AS SIGNED),owner_name FROM nations WHERE id=170",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(king, (0, String::new()));
    let dominion: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT CAST(owner_expedition_id AS SIGNED),CAST(siege_status AS SIGNED),\
         CAST(siege_outcome AS SIGNED),CAST(tower_hp IS NULL AS SIGNED) FROM dominions \
         WHERE zone_group_id=25",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dominion, (0, 2, 3, 1));
    let participants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dominion_siege_participants")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(participants, 0);

    for table in [
        "dominion_siege_participants",
        "dominions",
        "nations",
        "characters",
        "expedition_role_policies",
        "expedition_members",
        "expeditions",
    ] {
        sqlx::query(&format!("DROP TEMPORARY TABLE {table}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

#[tokio::test]
async fn chat_spam_delay_refuses_messages_inside_the_channel_window() {
    use crate::packets::world::SCErrorMsgPacket;
    use crate::systems::social::chat::CHAT_SPAM_DELAY_ERROR;
    let server = test_server();
    // Say is limited to one message per minute; shout and trade keep their own groups.
    server.config.update(|c| {
        c.chat.chat_type_groups = (0..15).collect();
        let mut delays = vec![0u32; 15];
        delays[chat_type::WHITE as usize] = 60_000;
        c.chat.chat_group_delays_ms = delays;
    });
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    a.c.drain();

    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "one"),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);

    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "two"),
    )
    .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCErrorMsgPacket,
            crate::systems::social::packets::body(&SCErrorMsgPacket::new(
                CHAT_SPAM_DELAY_ERROR,
                0,
                true,
            ))
        )]
    );

    // Whisper (-3) is outside the client's limited `0..=14` range and still reaches its normal
    // error path.
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        whisper("Nobody", "hi"),
    )
    .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCErrorMsgPacket,
            crate::systems::social::packets::body(&SCErrorMsgPacket::new(
                crate::packets::world::error_message::WHISPER_NO_TARGET,
                0,
                true,
            ))
        )]
    );
}

#[tokio::test]
async fn invite_area_skips_party_members_and_hostiles() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 150.0, 100.0);
    let mut far = pc(&server, 3, "Far", 300.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut far]);
    let mut s = PacketStream::new();
    s.write_u32(0).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    // The merged area path sends `SCAskToJoinTeamArea` (not the named invitation) ...
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamAreaPacket]);
    // 200 m away: outside the 100 m radius.
    assert!(far.ops().is_empty());
    // ... and the inviter gets the success reply, no "invite yourself" error.
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCTeamAreaInvitedPacket,
            body(&SCTeamAreaInvitedPacket { r: 0, s: true })
        )]
    );
}

/// `SCErrorMsgPacket(error, 0, true)` as the frames of one connection carry it.
pub(super) fn error_frame(error: i16) -> (u16, Vec<u8>) {
    use crate::packets::world::SCErrorMsgPacket;
    (
        sc::SCErrorMsgPacket,
        body(&SCErrorMsgPacket::new(error, 0, true)),
    )
}

/// Registers the guild `id` with `members` (character, role); the characters are marked as
/// members and join the guild chat like a login would.
pub(super) fn add_guild(server: &GameServer, id: u32, name: &str, members: &[(&Pc, u8)]) {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::{self, Expedition};
    let owner = members
        .iter()
        .find(|(_, role)| *role == 255)
        .map(|(m, _)| m.p.character_id)
        .unwrap_or(0);
    let list = members
        .iter()
        .map(|(m, role)| {
            m.p.data().unit.expedition_id = id;
            let mut x = expedition::member_from_character(server, id, &m.p, false);
            x.role = *role;
            expedition::guild_chat(server, id, name).join(&m.p);
            x
        })
        .collect();
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id,
            name: name.into(),
            owner_id: owner,
            owner_name: "Lead".into(),
            mother_id: 101,
            ..Default::default()
        },
        members: list,
        policies: expedition::default_policies(server, id),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
}

/// `owner` invites the player called `name` into its (new or existing) team and they accept.
pub(super) async fn join_party(
    server: &GameServer,
    owner: &mut Pc,
    target: &mut Pc,
    name: &str,
) -> u32 {
    let team_id = server
        .systems
        .social
        .teams
        .active_team_by_unit(owner.p.character_id)
        .map_or(0, |t| t.id);
    owner
        .send(server, cs::CSInviteToTeamPacket, invite(team_id, name))
        .await;
    target
        .send(
            server,
            cs::CSReplyToJoinTeamPacket,
            reply(team_id, owner.p.character_id, false),
        )
        .await;
    server
        .systems
        .social
        .teams
        .active_team_by_unit(target.p.character_id)
        .expect("joined")
        .id
}

/// A hard disconnect (also a kick or a server-side drop) logs the character out of its guild,
/// family, chat channels, party and friends' lists exactly like the clean leave does: the old
/// disconnect left guild members seeing it online (and its `last_leave_time` at its login time).
#[tokio::test]
async fn hard_disconnect_runs_the_same_social_logout_as_a_clean_leave() {
    use crate::packets::world::SCExpeditionMemberStatusChangedPacket;
    use crate::systems::social::family::{Family, FamilyMember};
    use crate::systems::social::packets::family::SCFamilyMemberOnlinePacket;
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut dropped = pc(&server, 2, "Dropped", 101.0, 100.0);
    let mut left = pc(&server, 3, "Left", 102.0, 100.0);
    settle(&mut [&mut lead, &mut dropped, &mut left]);
    add_guild(
        &server,
        77,
        "Knights",
        &[(&lead, 255), (&dropped, 0), (&left, 0)],
    );
    server.expeditions.with_mut(77, |e| {
        for m in &mut e.members {
            m.last_world_leave_time = 1000; // the login time the bots saw staying forever
        }
    });
    let member = |p: &PlayerRef, role| FamilyMember {
        character: Some(p.clone()),
        id: p.character_id,
        name: p.data().unit.name.clone(),
        role,
        title: String::new(),
    };
    server.systems.social.families.insert(Family {
        id: 11,
        members: vec![member(&lead.p, 1), member(&dropped.p, 0)],
        removed_members: Vec::new(),
    });
    lead.p.data().family = 11;
    dropped.p.data().family = 11;
    // Party, friend.
    let teams = &server.systems.social.teams;
    teams.ask_to_join(&server, &lead.p, "Dropped", 0, true, None);
    teams.reply_to_join_team(&server, &dropped.p, 0, true, 1, false, "", false);
    let team_id = teams.active_team_by_unit(1).expect("party").id;
    lead.send(&server, cs::CSAddFriendPacket, name_body("Dropped"))
        .await;
    settle(&mut [&mut lead, &mut dropped, &mut left]);

    // The hard disconnect of one member ...
    crate::world::session::on_disconnect(&server, &mut dropped.c.conn).await;
    let e = server.expeditions.get(77).unwrap();
    let m = e.member(2).unwrap();
    assert!(!m.is_online);
    assert!(m.last_world_leave_time > 1000, "last_leave_time moved");
    assert!(dropped.p.data().leave_time > 0);
    let frames = lead.frames();
    assert_eq!(
        find(&frames, sc::SCExpeditionMemberStatusChangedPacket),
        vec![body(&SCExpeditionMemberStatusChangedPacket {
            member: m.clone(),
            flag: 0
        })]
    );
    // ... the family and the party (as before) and the friend list ...
    assert_eq!(
        find(&frames, sc::SCFamilyMemberOnlinePacket),
        vec![body(&SCFamilyMemberOnlinePacket {
            family_id: 11,
            member_id: 2,
            online: false
        })]
    );
    assert!(server
        .systems
        .social
        .families
        .family(11)
        .unwrap()
        .member(2)
        .unwrap()
        .character
        .is_none());
    assert_eq!(find(&frames, sc::SCTeamMemberDisconnectedPacket).len(), 1);
    assert_eq!(find(&frames, sc::SCFriendStatusChangedPacket).len(), 1);
    assert!(!teams
        .active_team(team_id)
        .unwrap()
        .members
        .iter()
        .flatten()
        .any(|m| m.character.character_id == 2 && m.character.data().is_online));
    // ... and no chat channel keeps the dead session (guild, party).
    assert!(server
        .chat
        .list_all_channels()
        .iter()
        .all(|c| !c.contains(&dropped.p)));

    // A clean leave of another member gives the same guild state and notification.
    crate::world::session::leave_world_task(
        &server,
        None,
        crate::world::session::leave_target::CHARACTER_SELECT,
        Some(left.p.clone()),
    )
    .await;
    let e = server.expeditions.get(77).unwrap();
    assert!(!e.member(3).unwrap().is_online);
    assert!(e.member(3).unwrap().last_world_leave_time > 1000);
    assert_eq!(
        find(&lead.frames(), sc::SCExpeditionMemberStatusChangedPacket),
        vec![body(&SCExpeditionMemberStatusChangedPacket {
            member: e.member(3).unwrap().clone(),
            flag: 0
        })]
    );
    assert!(e.member(1).unwrap().is_online, "the leader is untouched");
}

/// A connection keeps its `active_char` after a leave. Its later disconnect must not log the
/// character out of the guild again: the character may be online on a newer session by then.
#[tokio::test]
async fn disconnect_of_a_session_that_already_left_keeps_the_new_session_online() {
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut old = pc(&server, 2, "Old", 101.0, 100.0);
    settle(&mut [&mut lead, &mut old]);
    add_guild(&server, 77, "Knights", &[(&lead, 255), (&old, 0)]);
    // The old session left (it is no longer the live world entry) and the character is online
    // again on a new session: the guild record says online.
    old.p.data().is_online = false;
    server.world.try_remove_character(old.p.obj_id);
    settle(&mut [&mut lead, &mut old]);
    crate::world::session::on_disconnect(&server, &mut old.c.conn).await;
    let e = server.expeditions.get(77).unwrap();
    assert!(e.member(2).unwrap().is_online);
    assert!(lead.ops().is_empty());
}

/// The guild leader can neither leave nor be left leaderless: the 1.2 client's text says "you
/// can leave after delegating the leader position".
#[tokio::test]
async fn guild_leader_cannot_leave_without_handing_over() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut two = pc(&server, 2, "Two", 101.0, 100.0);
    settle(&mut [&mut lead, &mut two]);
    add_guild(&server, 77, "Knights", &[(&lead, 255), (&two, 0)]);
    settle(&mut [&mut lead, &mut two]);
    let leave = PacketStream::new;
    let owner_to = |id: u32| {
        let mut s = PacketStream::new();
        s.write_u32(id);
        s
    };

    lead.send(&server, cs::CSLeaveExpeditionPacket, leave())
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_OWNER_CANNOT_LEAVE)]
    );
    assert!(two.ops().is_empty());
    let e = server.expeditions.get(77).unwrap();
    assert_eq!(e.members.len(), 2);
    assert_eq!(e.member(1).unwrap().role, 255);
    assert_eq!(lead.p.data().unit.expedition_id, 77);

    // An ordinary member still can leave, and the leader stays (and still cannot leave).
    two.send(&server, cs::CSLeaveExpeditionPacket, leave())
        .await;
    assert_eq!(two.p.data().unit.expedition_id, 0);
    assert_eq!(server.expeditions.get(77).unwrap().members.len(), 1);
    lead.ops();
    two.ops();
    lead.send(&server, cs::CSLeaveExpeditionPacket, leave())
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_OWNER_CANNOT_LEAVE)]
    );

    // Invite the other one back, hand the guild over: now the old leader can leave and the guild
    // keeps a leader.
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Two"))
        .await;
    let mut accept = PacketStream::new();
    accept.write_u32(77).write_u32(1).write_bool(true);
    two.send(&server, cs::CSReplyExpeditionInvitationPacket, accept)
        .await;
    assert_eq!(two.p.data().unit.expedition_id, 77);
    lead.send(&server, cs::CSChangeExpeditionOwnerPacket, owner_to(2))
        .await;
    lead.ops();
    two.ops();
    lead.send(&server, cs::CSLeaveExpeditionPacket, leave())
        .await;
    assert_eq!(lead.p.data().unit.expedition_id, 0);
    let e = server.expeditions.get(77).unwrap();
    assert_eq!(e.members.len(), 1);
    assert_eq!(e.member(2).unwrap().role, 255);
    assert_eq!(e.faction.owner_id, 2);
}

/// Party invitations expire after a minute, die with either session, and a late answer is told
/// why it fails (the old invitation blocked every later invite of the target forever).
#[tokio::test]
async fn party_invitations_expire_and_die_with_either_session() {
    use crate::systems::social::util::{error, now_ms};
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    let teams = &server.systems.social.teams;

    // Pending: blocks, until the sweep expires it and tells the inviter.
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(a.ops(), vec![sc::SCRejectedTeamPacket]);
    teams.expire_invitations(now_ms() + 30_000);
    assert!(a.ops().is_empty() && teams.invitation_count() == 1);
    teams.expire_invitations(now_ms() + 61_000);
    assert_eq!(
        a.frames(),
        vec![error_frame(error::INVITATION_CANCELED_TIMEOUT)]
    );
    assert_eq!(
        b.frames(),
        vec![(
            sc::SCInvitationCanceledPacket,
            vec![invitation_canceled_kind::TEAM]
        )]
    );
    assert_eq!(teams.invitation_count(), 0);
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);

    // An expired invitation that was not swept yet no longer blocks a new one either ...
    teams.age_invitations(61_000);
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);
    assert!(a.ops().is_empty());
    // ... and answering it late fails: the inviter is told about the refusal, the invitee why.
    teams.age_invitations(61_000);
    b.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, false))
        .await;
    assert_eq!(a.ops(), vec![sc::SCRejectedTeamPacket]);
    assert_eq!(
        b.frames(),
        vec![error_frame(error::INVITATION_CANCELED_TIMEOUT)]
    );
    assert!(teams.active_team_by_unit(1).is_none());

    // The invitee logs out and in again (reconnect): the invitation is gone.
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    b.ops();
    crate::world::session::set_online(&server, &b.p, false);
    crate::world::session::set_online(&server, &b.p, true);
    assert_eq!(teams.invitation_count(), 0);
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);
    assert!(a.ops().is_empty());

    // The inviter goes away: the invitee is told, and accepting afterwards fails.
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    b.ops();
    crate::world::session::set_online(&server, &a.p, false);
    assert_eq!(
        b.frames(),
        vec![
            error_frame(error::TEAM_INVITOR_OFFLINE),
            (
                sc::SCInvitationCanceledPacket,
                vec![invitation_canceled_kind::TEAM]
            )
        ]
    );
    assert_eq!(teams.invitation_count(), 0);
    b.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, false))
        .await;
    assert_eq!(b.frames(), vec![error_frame(error::TEAM_JOIN)]);
    assert!(teams.active_team_by_unit(2).is_none());
    // A decline of something that no longer exists is silent.
    b.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, true))
        .await;
    assert!(b.ops().is_empty());

    // A hard disconnect of the invitee drops its invitation as well.
    crate::world::session::set_online(&server, &a.p, true);
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);
    assert_eq!(teams.invitation_count(), 1);
    crate::world::session::on_disconnect(&server, &mut b.c.conn).await;
    assert_eq!(teams.invitation_count(), 0);
}

/// A member that joins an existing party / raid joins its chat channels, and a member that
/// leaves or is kicked leaves them (the late joiner could talk but never hear before).
#[tokio::test]
async fn late_party_members_hear_the_team_and_leavers_stop_hearing_it() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    let mut d = pc(&server, 4, "Di", 130.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    assert_eq!(join_party(&server, &mut a, &mut c, "Cy").await, team_id);
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);

    // Everybody hears everybody, in both directions.
    c.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "from the late joiner"),
    )
    .await;
    for x in [&mut a, &mut b, &mut c] {
        assert_eq!(x.ops(), vec![sc::SCChatMessagePacket]);
    }
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "to the late joiner"),
    )
    .await;
    for x in [&mut a, &mut b, &mut c] {
        assert_eq!(x.ops(), vec![sc::SCChatMessagePacket]);
    }

    // Leaving stops it.
    c.send(
        &server,
        cs::CSAskRiskyTeamActionPacket,
        risky(team_id, 3, risky_action::LEAVE),
    )
    .await;
    assert!(c.ops().contains(&sc::SCLeavedChatChannelPacket));
    settle(&mut [&mut a, &mut b, &mut c]);
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "bye"),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(b.ops(), vec![sc::SCChatMessagePacket]);
    assert!(c.ops().is_empty());

    // A raid: the late joiner joins the raid channel as well as its party's.
    let mut s = PacketStream::new();
    s.write_u32(team_id);
    a.send(&server, cs::CSConvertToRaidTeamPacket, s).await;
    assert_eq!(join_party(&server, &mut a, &mut d, "Di").await, team_id);
    let joined = d.ops();
    assert_eq!(
        joined
            .iter()
            .filter(|op| **op == sc::SCJoinedChatChannelPacket)
            .count(),
        2,
        "raid and party channel: {joined:x?}"
    );
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::RAID, "raid"),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(b.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(d.ops(), vec![sc::SCChatMessagePacket]);
    // Kicked: the raid owner keeps both channels, the kicked member leaves both.
    a.send(
        &server,
        cs::CSAskRiskyTeamActionPacket,
        risky(team_id, 4, risky_action::KICK),
    )
    .await;
    let kicked = d.ops();
    assert_eq!(
        kicked
            .iter()
            .filter(|op| **op == sc::SCLeavedChatChannelPacket)
            .count(),
        2,
        "{kicked:x?}"
    );
    assert!(!a.ops().contains(&sc::SCLeavedChatChannelPacket));
    b.ops();
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::RAID, "raid again"),
    )
    .await;
    assert_eq!(a.ops(), vec![sc::SCChatMessagePacket]);
    assert_eq!(b.ops(), vec![sc::SCChatMessagePacket]);
    assert!(d.ops().is_empty());
}

/// Accepting an invitation while in another team is refused (a character ended up in two).
#[tokio::test]
async fn accepting_a_party_invitation_while_in_another_team_is_refused() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    let mut d = pc(&server, 4, "Di", 130.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);
    let teams = &server.systems.social.teams;
    let first = form_party(&server, &mut a, &mut b).await;
    // Al invites Cy, who does not answer yet but founds a party with Di meanwhile.
    a.send(&server, cs::CSInviteToTeamPacket, invite(first, "Cy"))
        .await;
    let second = join_party(&server, &mut c, &mut d, "Di").await;
    assert_ne!(first, second);
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(first, a.p.character_id, false),
    )
    .await;
    assert_eq!(c.frames(), vec![error_frame(error::TEAM_JOIN_ALONE)]);
    assert_eq!(a.frames(), vec![error_frame(error::TEAM_INVITEE_IN_TEAM)]);
    assert!(b.ops().is_empty() && d.ops().is_empty());
    assert_eq!(teams.active_team_by_unit(3).unwrap().id, second);
    assert_eq!(teams.active_team(first).unwrap().members_count(), 2);
    assert_eq!(teams.active_team(second).unwrap().members_count(), 2);
    assert_eq!(teams.invitation_count(), 0);
    assert!(c.p.data().in_party);
}

/// The inviter hears why a party invitation does not happen.
#[tokio::test]
async fn party_invitation_failures_are_reported() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    let mut d = pc(&server, 4, "Di", 130.0, 100.0);
    let mut e = pc(&server, 5, "Ed", 140.0, 100.0);
    let mut f = pc(&server, 6, "Flo", 150.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c, &mut d, &mut e, &mut f]);

    // An offline / unknown invitee, and yourself.
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Nobody"))
        .await;
    assert_eq!(a.frames(), vec![error_frame(error::TEAM_INVITEE_OFFLINE)]);
    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Al"))
        .await;
    assert_eq!(a.frames(), vec![error_frame(error::TEAM_YOURSELF)]);

    // Replying without an invitation says so; declining one is silent.
    c.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, false))
        .await;
    assert_eq!(c.frames(), vec![error_frame(error::TEAM_JOIN)]);
    c.send(&server, cs::CSReplyToJoinTeamPacket, reply(0, 1, true))
        .await;
    assert!(c.ops().is_empty());

    // A member of a team can be invited neither by its own team nor by another one.
    let team_id = form_party(&server, &mut a, &mut b).await;
    settle(&mut [&mut a, &mut b]);
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Bob"))
        .await;
    assert_eq!(a.frames(), vec![error_frame(error::TEAM_INVITEE_MEMBER)]);
    join_party(&server, &mut c, &mut d, "Di").await;
    settle(&mut [&mut a, &mut b, &mut c, &mut d]);
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    assert_eq!(a.frames(), vec![error_frame(error::TEAM_INVITEE_IN_TEAM)]);
    assert!(c.ops().is_empty());

    // Only the owner of a party invites, whatever team id the client names (0 included).
    for named in [0, team_id, 0x7777] {
        b.send(&server, cs::CSInviteToTeamPacket, invite(named, "Ed"))
            .await;
        assert_eq!(b.frames(), vec![error_frame(error::TEAM_NO_RIGHTS)]);
    }
    assert!(e.ops().is_empty());

    // A full party (5) refuses the invitation at once, not just at the answer.
    assert_eq!(join_party(&server, &mut a, &mut e, "Ed").await, team_id);
    assert_eq!(join_party(&server, &mut a, &mut f, "Flo").await, team_id);
    let teams = &server.systems.social.teams;
    assert_eq!(teams.active_team(team_id).unwrap().members_count(), 4);
    let mut g = pc(&server, 7, "Gus", 160.0, 100.0);
    let mut h = pc(&server, 8, "Hal", 170.0, 100.0);
    settle(&mut [&mut a, &mut g, &mut h]);
    assert_eq!(join_party(&server, &mut a, &mut g, "Gus").await, team_id);
    assert_eq!(teams.active_team(team_id).unwrap().members_count(), 5);
    a.ops();
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Hal"))
        .await;
    assert_eq!(a.frames(), vec![error_frame(error::TEAM_FULL)]);
    assert!(h.ops().is_empty());
    let mut s = PacketStream::new();
    s.write_u32(team_id).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    assert_eq!(a.frames(), vec![error_frame(error::TEAM_FULL)]);
    assert!(h.ops().is_empty());
}

/// The party that fills up between an invitation and its answer: both sides are told.
#[tokio::test]
async fn a_party_that_fills_up_before_the_answer_tells_both_sides() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut pcs: Vec<Pc> = (1..=6)
        .map(|i| pc(&server, i, &format!("M{i}"), 100.0 + i as f32, 100.0))
        .collect();
    for x in pcs.iter_mut() {
        x.c.drain();
    }
    let teams = &server.systems.social.teams;
    for i in 1..4 {
        let name = format!("M{}", i + 1);
        teams.ask_to_join(&server, &pcs[0].p, &name, 0, true, None);
        teams.reply_to_join_team(&server, &pcs[i].p, 0, true, 1, false, "", false);
    }
    assert_eq!(teams.active_team_by_unit(1).unwrap().members_count(), 4);
    // One seat, two invitations (M5 and M6).
    teams.ask_to_join(&server, &pcs[0].p, "M5", 0, true, None);
    teams.ask_to_join(&server, &pcs[0].p, "M6", 0, true, None);
    for x in pcs.iter_mut() {
        x.c.drain();
    }
    teams.reply_to_join_team(&server, &pcs[4].p, 0, true, 1, false, "", false);
    teams.reply_to_join_team(&server, &pcs[5].p, 0, true, 1, false, "", false);
    assert_eq!(teams.active_team_by_unit(1).unwrap().members_count(), 5);
    assert!(!pcs[5].p.data().in_party);
    assert_eq!(pcs[5].c.drain(), vec![error_frame(error::TEAM_FULL)]);
    let owner = pcs[0].c.drain();
    assert_eq!(
        owner
            .iter()
            .filter(|f| **f == error_frame(error::TEAM_FULL))
            .count(),
        1
    );
}

/// Failed guild invitations, answers and expulsions are reported to the character.
#[tokio::test]
async fn guild_invitation_reply_and_kick_failures_are_reported() {
    use crate::systems::social::util::error;
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut cmdr = pc(&server, 2, "Cmdr", 101.0, 100.0);
    let mut off = pc(&server, 3, "Off", 102.0, 100.0);
    let mut rookie = pc(&server, 4, "Rookie", 103.0, 100.0);
    let mut out = pc(&server, 9, "Out", 104.0, 100.0);
    let mut other = pc(&server, 10, "Other", 105.0, 100.0);
    settle(&mut [
        &mut lead,
        &mut cmdr,
        &mut off,
        &mut rookie,
        &mut out,
        &mut other,
    ]);
    add_guild(
        &server,
        77,
        "Knights",
        &[(&lead, 255), (&cmdr, 3), (&off, 2), (&rookie, 0)],
    );
    add_guild(&server, 88, "Rivals", &[(&other, 255)]);
    for x in [&mut lead, &mut cmdr, &mut off, &mut rookie, &mut other] {
        x.c.drain();
    }
    let reply_to = |id: u32, join: bool| {
        let mut s = PacketStream::new();
        s.write_u32(id).write_u32(1).write_bool(join);
        s
    };
    let kick = |id: u32| {
        let mut s = PacketStream::new();
        s.write_u32(id);
        s
    };

    // --- invite ---
    // Not in a guild.
    out.send(&server, cs::CSInviteToExpeditionPacket, name_body("Rookie"))
        .await;
    assert_eq!(
        out.frames(),
        vec![error_frame(error::ONLY_EXPEDITION_MEMBER)]
    );
    // No right to invite (role 0).
    rookie
        .send(&server, cs::CSInviteToExpeditionPacket, name_body("Out"))
        .await;
    assert_eq!(
        rookie.frames(),
        vec![error_frame(error::FACTION_INVITE_RIGHTS)]
    );
    // Unknown / offline name, yourself, a member of the same / another guild.
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Ghost"))
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::FACTION_INVITE_OFFLINE)]
    );
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Lead"))
        .await;
    assert_eq!(lead.frames(), vec![error_frame(error::FACTION_INVITE_SELF)]);
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Off"))
        .await;
    assert_eq!(lead.frames(), vec![error_frame(error::FACTION_INVITE_SAME)]);
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Other"))
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_OTHER_EXPEDITION)]
    );
    // Somebody who is deciding on another invitation cannot be invited again.
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Out"))
        .await;
    assert_eq!(out.ops(), vec![sc::SCExpeditionInvitationPacket]);
    off.send(&server, cs::CSInviteToExpeditionPacket, name_body("Out"))
        .await;
    assert_eq!(
        off.frames(),
        vec![error_frame(error::EXPEDITION_OTHER_INVITATION)]
    );
    assert!(out.ops().is_empty());

    // --- reply ---
    // A refusal reaches the inviter.
    out.send(
        &server,
        cs::CSReplyExpeditionInvitationPacket,
        reply_to(77, false),
    )
    .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::FACTION_INVITE_REJECTED)]
    );
    assert!(out.ops().is_empty());
    // No invitation / one for another guild.
    out.send(
        &server,
        cs::CSReplyExpeditionInvitationPacket,
        reply_to(77, true),
    )
    .await;
    assert_eq!(
        out.frames(),
        vec![error_frame(error::FACTION_JOIN_NO_INVITOR)]
    );
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Out"))
        .await;
    out.ops();
    out.send(
        &server,
        cs::CSReplyExpeditionInvitationPacket,
        reply_to(88, true),
    )
    .await;
    assert_eq!(
        out.frames(),
        vec![error_frame(error::FACTION_JOIN_NO_INVITOR)]
    );
    // Expired.
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Out"))
        .await;
    out.ops();
    server
        .expeditions
        .invitations
        .lock()
        .unwrap()
        .get_mut(&9)
        .unwrap()
        .time_ms -= 61_000;
    out.send(
        &server,
        cs::CSReplyExpeditionInvitationPacket,
        reply_to(77, true),
    )
    .await;
    assert_eq!(
        out.frames(),
        vec![error_frame(error::INVITATION_CANCELED_TIMEOUT)]
    );
    // Already in a guild (invited before it joined another one).
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Out"))
        .await;
    out.ops();
    out.p.data().unit.expedition_id = 88;
    out.send(
        &server,
        cs::CSReplyExpeditionInvitationPacket,
        reply_to(77, true),
    )
    .await;
    assert_eq!(
        out.frames(),
        vec![error_frame(error::EXPEDITION_ALREADY_MEMBER)]
    );
    out.p.data().unit.expedition_id = 0;
    // A guild that was dismissed meanwhile cannot be joined.
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Out"))
        .await;
    out.ops();
    server.expeditions.with_mut(77, |e| e.is_disbanded = true);
    out.send(
        &server,
        cs::CSReplyExpeditionInvitationPacket,
        reply_to(77, true),
    )
    .await;
    assert_eq!(out.frames(), vec![error_frame(error::FACTION_JOIN)]);
    assert_eq!(out.p.data().unit.expedition_id, 0);
    server.expeditions.with_mut(77, |e| e.is_disbanded = false);
    for x in [&mut lead, &mut cmdr, &mut off, &mut rookie, &mut other] {
        x.ops();
    }

    // --- kick ---
    // Not in a guild; no right to expel; unknown target; the leader.
    out.send(&server, cs::CSKickFromExpeditionPacket, kick(4))
        .await;
    assert_eq!(
        out.frames(),
        vec![error_frame(error::ONLY_EXPEDITION_MEMBER)]
    );
    off.send(&server, cs::CSKickFromExpeditionPacket, kick(4))
        .await;
    assert_eq!(off.frames(), vec![error_frame(error::FACTION_KICK_RIGHTS)]);
    cmdr.send(&server, cs::CSKickFromExpeditionPacket, kick(99))
        .await;
    assert_eq!(
        cmdr.frames(),
        vec![error_frame(error::EXPEDITION_NO_TARGET)]
    );
    cmdr.send(&server, cs::CSKickFromExpeditionPacket, kick(1))
        .await;
    assert_eq!(cmdr.frames(), vec![error_frame(error::FACTION_KICK_RIGHTS)]);
    // A real kick still works and sends no error.
    lead.send(&server, cs::CSKickFromExpeditionPacket, kick(4))
        .await;
    assert_eq!(rookie.p.data().unit.expedition_id, 0);
    assert!(!lead
        .frames()
        .contains(&error_frame(error::FACTION_KICK_RIGHTS)));
}

fn expect_chat_failed(p: &mut Pc, error: i16, channel: &str) {
    use crate::systems::social::packets::chat::SCChatFailedPacket;
    assert_eq!(
        p.frames(),
        vec![(
            sc::SCChatFailedPacket,
            body(&SCChatFailedPacket::new(error, channel))
        )]
    );
}

/// The `SCRestrictInfoPacket` a muted sender gets: `chname`, `index` 0, `code` 0 (chat),
/// `duration` = remaining seconds of the persisted mute, `end` = the persisted end and
/// `start` = `end - duration` (the notice time; the client ignores both dates).
fn expect_restrict_info(p: &mut Pc, chname: &str, until: i64) {
    let frames = p.frames();
    assert_eq!(frames.len(), 1, "one refusal, nothing else");
    assert_eq!(frames[0].0, sc::SCRestrictInfoPacket);
    let mut s = PacketStream::from_bytes(frames[0].1.clone());
    assert_eq!(s.read_string().unwrap(), chname);
    assert_eq!(s.read_i32().unwrap(), 0, "index");
    assert_eq!(s.read_u8().unwrap(), 0, "restrictCode (chat)");
    let duration = s.read_u32().unwrap();
    let start = s.read_i64().unwrap();
    let end = s.read_i64().unwrap();
    assert_eq!(end, until);
    assert_eq!(start, until - i64::from(duration));
    assert!((595..=600).contains(&duration), "{duration}");
    assert_eq!(s.remaining(), 0);
}

#[tokio::test]
async fn a_muted_sender_cannot_bypass_the_mute_in_a_user_channel() {
    use crate::systems::social::chat::user_channel_identity;

    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Friends", "", true),
    )
    .await;
    let channel = server
        .chat
        .list_all_channels()
        .into_iter()
        .find(|c| c.chat_type == chat_type::USER)
        .unwrap();
    let identity = user_channel_identity(&channel);
    b.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Friends", "", false),
    )
    .await;
    a.frames();
    b.frames();

    let until = chrono::Utc::now().timestamp() + 600;
    a.p.data().chat_restricted_until = until;
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        user_chat(identity, "hidden"),
    )
    .await;
    expect_restrict_info(&mut a, "Al", until);
    assert!(b.frames().is_empty(), "the user-channel copy was routed");
}

/// A second invitation to the same invitee is refused while one is pending; nothing closes the
/// pending popup until that invitation ends.
#[tokio::test]
async fn a_second_team_invitation_does_not_cancel_the_pending_one() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);

    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    b.ops();
    c.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(c.ops(), vec![sc::SCRejectedTeamPacket]);
    assert!(
        b.frames().is_empty(),
        "the second invitation must not touch the pending popup"
    );

    let expired_at = crate::systems::social::util::now_ms() + INVITATION_TIMEOUT_MS + 1;
    server
        .systems
        .social
        .teams
        .update_all_teams(&server, expired_at);
    assert_eq!(
        b.frames(),
        vec![(
            sc::SCInvitationCanceledPacket,
            vec![invitation_canceled_kind::TEAM]
        )],
        "exactly one cancellation when the last pending invitation ends"
    );
}

/// An accepted invitation consumes the pending one, so the later sweep sends nothing.
#[tokio::test]
async fn accepted_team_invitation_sends_no_cancellation() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    b.ops();
    b.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(0, a.p.character_id, false),
    )
    .await;
    assert!(find(&b.frames(), sc::SCInvitationCanceledPacket).is_empty());

    let expired_at = crate::systems::social::util::now_ms() + INVITATION_TIMEOUT_MS + 1;
    server
        .systems
        .social
        .teams
        .update_all_teams(&server, expired_at);
    assert!(find(&b.frames(), sc::SCInvitationCanceledPacket).is_empty());
}

#[tokio::test]
async fn channel_bound_chat_failures_use_scc_chat_failed_only_when_enabled() {
    use crate::packets::world::error_message;
    use crate::systems::social::chat::user_channel_identity;
    use crate::systems::social::util::error;
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    // Default (flag off): exactly the pre-flag toast.
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::PARTY, "x"),
    )
    .await;
    expect_chat_error(&mut a, error::CHAT_NOT_IN_PARTY);

    server.config.update(|c| c.chat.chat_failed_packet = true);

    // Party / raid / expedition / family: the channel label travels with the error.
    for (kind, code, channel) in [
        (chat_type::PARTY, error::CHAT_NOT_IN_PARTY, "Party"),
        (chat_type::RAID, error::CHAT_NOT_IN_RAID, "Raid"),
        (chat_type::CLAN, error::CHAT_NOT_IN_EXPEDITION, "Expedition"),
        (chat_type::FAMILY, error::CHAT_NOT_IN_FAMILY, "Family"),
    ] {
        a.send(&server, cs::CSSendChatMessagePacket, chat(kind, "x"))
            .await;
        expect_chat_failed(&mut a, code, channel);
    }

    // User channels: a non-member message and a leave answer with the channel's own name.
    b.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Public", "", true),
    )
    .await;
    assert_eq!(b.ops(), vec![sc::SCJoinedChatChannelPacket]);
    let channel = server
        .chat
        .list_all_channels()
        .into_iter()
        .find(|c| c.chat_type == chat_type::USER)
        .unwrap();
    let identity = user_channel_identity(&channel);
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        user_chat(identity, "x"),
    )
    .await;
    expect_chat_failed(&mut a, error::CHAT_NOT_JOINED_CHANNEL, "Public");
    a.send(&server, cs::CSLeaveChatChannelPacket, user_leave(identity))
        .await;
    expect_chat_failed(&mut a, error::CHAT_NOT_JOINED_CHANNEL, "Public");
    // A channel that does not exist has no name to show.
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        user_chat(identity | (1i64 << 16), "x"),
    )
    .await;
    expect_chat_failed(&mut a, error::CHAT_NO_CHANNEL, "");
    // Join failures answer with the requested name.
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Missing", "", false),
    )
    .await;
    expect_chat_failed(&mut a, error::CHAT_NO_CHANNEL, "Missing");
    a.send(
        &server,
        cs::CSJoinUserChatChannelPacket,
        user_join("Public", "", true),
    )
    .await;
    expect_chat_failed(&mut a, error::CHAT_CHANNEL_ALREADY_EXISTS, "Public");

    // A whisper to a missing player is not channel-bound: the toast stays.
    a.send(&server, cs::CSSendChatMessagePacket, whisper("Nobody", "x"))
        .await;
    expect_chat_error(&mut a, error_message::WHISPER_NO_TARGET);
}

#[tokio::test]
async fn chat_level_gate_refuses_below_the_configured_level_and_exempts_gms() {
    use crate::packets::world::SCErrorMsgPacket;
    use crate::systems::economy::error_message;

    let server = test_server();
    let mut a = pc(&server, 1, "Low", 100.0, 100.0);
    let mut b = pc(&server, 2, "High", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    // The default `Chat.MinLevelByChatType` is empty: no gate, exactly the previous behaviour.
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "open"),
    )
    .await;
    assert_eq!(find(&a.frames(), sc::SCChatMessagePacket).len(), 1);
    assert_eq!(find(&b.frames(), sc::SCChatMessagePacket).len(), 1);

    // Both test characters are level 10; Say now needs 20.
    server.config.update(|c| {
        c.chat.min_level_by_chat_type.insert(chat_type::WHITE, 20);
    });
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "blocked"),
    )
    .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCErrorMsgPacket,
            body(&SCErrorMsgPacket::new(
                error_message::CHAT_CANNOT_SEND_SINCE_LEVEL_LOW,
                0,
                true,
            ))
        )]
    );
    assert!(b.frames().is_empty(), "a below-level message was routed");

    // Reaching the level lets the same message through.
    a.p.data().unit.level = 20;
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "allowed"),
    )
    .await;
    assert_eq!(find(&a.frames(), sc::SCChatMessagePacket).len(), 1);
    assert_eq!(find(&b.frames(), sc::SCChatMessagePacket).len(), 1);

    // GM exemption: `access_level >= 100` bypasses the gate even below the level.
    a.p.data().unit.level = 10;
    a.p.data().access_level = crate::systems::social::chat::GM_ACCESS_LEVEL;
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "gm"),
    )
    .await;
    assert_eq!(find(&a.frames(), sc::SCChatMessagePacket).len(), 1);
    assert_eq!(find(&b.frames(), sc::SCChatMessagePacket).len(), 1);
}

#[tokio::test]
async fn chat_mute_refuses_the_message_and_expires_automatically() {
    let server = test_server();
    let mut a = pc(&server, 1, "Muted", 100.0, 100.0);
    let mut b = pc(&server, 2, "Friend", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    let until = chrono::Utc::now().timestamp() + 600;
    a.p.data().chat_restricted_until = until;
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "blocked"),
    )
    .await;
    expect_restrict_info(&mut a, "Muted", until);
    assert!(b.frames().is_empty(), "a muted message was routed");
    assert!(a.p.data().chat_restricted_until > 0, "mute kept");

    a.p.data().chat_restricted_until = chrono::Utc::now().timestamp() - 1;
    a.send(
        &server,
        cs::CSSendChatMessagePacket,
        chat(chat_type::WHITE, "after"),
    )
    .await;
    assert_eq!(a.p.data().chat_restricted_until, 0, "expired mute cleared");
    assert_eq!(find(&a.frames(), sc::SCChatMessagePacket).len(), 1);
    assert_eq!(find(&b.frames(), sc::SCChatMessagePacket).len(), 1);
}

/// The direct packets keep the risky path's permission checks: only the owner may kick or
/// dismiss, and a kick target must be a team member.
#[tokio::test]
async fn direct_team_kick_and_dismiss_require_owner() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    let team_id = form_party(&server, &mut a, &mut b).await;
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(team_id, a.p.character_id, false),
    )
    .await;
    settle(&mut [&mut a, &mut b, &mut c]);

    // B is not the owner: neither kick nor dismiss has any effect.
    b.send(
        &server,
        cs::CSKickTeamMemberPacket,
        direct_kick(team_id, c.p.character_id),
    )
    .await;
    b.send(&server, cs::CSDismissTeamPacket, direct_dismiss(team_id))
        .await;
    assert!(a.ops().is_empty() && b.ops().is_empty() && c.ops().is_empty());
    // The owner cannot kick a non-member.
    a.send(
        &server,
        cs::CSKickTeamMemberPacket,
        direct_kick(team_id, 99),
    )
    .await;
    assert!(a.ops().is_empty() && b.ops().is_empty() && c.ops().is_empty());
    let team = server
        .systems
        .social
        .teams
        .active_team(team_id)
        .expect("team preserved");
    assert_eq!(team.owner_id, a.p.character_id);
    assert_eq!(team.members_count(), 3);
    assert!(a.p.data().in_party && b.p.data().in_party && c.p.data().in_party);

    // The owner's direct kick still works.
    a.send(
        &server,
        cs::CSKickTeamMemberPacket,
        direct_kick(team_id, c.p.character_id),
    )
    .await;
    // The kicked member also leaves its party chat channel (the chat-leave frame).
    assert_eq!(
        find(&c.frames(), sc::SCLeavedTeamPacket),
        vec![body(&SCLeavedTeamPacket {
            team_id,
            kicked: true,
            dismissed: false
        })]
    );
    assert!(!c.p.data().in_party);
    assert_eq!(
        server
            .systems
            .social
            .teams
            .active_team(team_id)
            .expect("team")
            .members_count(),
        2
    );
}

/// The 1.2 client's confirmation callback sends `CSLeaveTeam` / `CSKickTeamMember` /
/// `CSDismissTeam` (`FUN_39309670` -> `FUN_393095c0`); each must perform the same action,
/// with the same frames, as the `CSAskRiskyTeamAction` it confirms.
#[tokio::test]
async fn direct_team_packets_match_risky_action_frames() {
    for action in [
        risky_action::LEAVE,
        risky_action::KICK,
        risky_action::DISMISS,
    ] {
        let direct = team_action_frames(action, true).await;
        let risky = team_action_frames(action, false).await;
        assert_eq!(direct, risky, "action {action}");
    }
}

/// An expired area invitation closes the same popup type with kind 2 (the 1-vs-2 split is
/// INFERRED; the client closes popup type 2 for both).
#[tokio::test]
async fn expired_area_team_invitation_sends_kind_two() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    let mut s = PacketStream::new();
    s.write_u32(0).write_bool(true);
    a.send(&server, cs::CSInviteAreaToTeamPacket, s).await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamAreaPacket]);
    a.ops();

    let expired_at = crate::systems::social::util::now_ms() + INVITATION_TIMEOUT_MS + 1;
    server
        .systems
        .social
        .teams
        .update_all_teams(&server, expired_at);
    assert_eq!(
        b.frames(),
        vec![(
            sc::SCInvitationCanceledPacket,
            vec![invitation_canceled_kind::TEAM_AREA]
        )]
    );
}

/// An unanswered guild invitation is swept after 60 s and closes the invitee's popup with
/// kind 4 (`SCExpeditionInvitation`, popup type 3).
#[tokio::test]
async fn expired_expedition_invitation_sends_kind_four() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::{self, Expedition};

    let server = test_server();
    let mut leader = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut invitee = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut leader, &mut invitee]);
    leader.p.data().unit.expedition_id = 77;
    let member = expedition::member_from_character(&server, 77, &leader.p, true);
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: leader.p.character_id,
            owner_name: "Lead".into(),
            // The empty test game data has no faction rows, so the alliance fallback is the
            // faction id itself: use the inviter's faction as the guild mother.
            mother_id: 101,
            ..Default::default()
        },
        members: vec![member],
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });

    let mut s = PacketStream::new();
    s.write_str("Bob");
    leader
        .send(&server, cs::CSInviteToExpeditionPacket, s)
        .await;
    assert_eq!(invitee.ops(), vec![sc::SCExpeditionInvitationPacket]);

    let expired_at = crate::systems::social::util::now_ms() + expedition::INVITATION_TIMEOUT_MS + 1;
    server.expeditions.expire_invitations(&server, expired_at);
    assert_eq!(
        invitee.frames(),
        vec![(
            sc::SCInvitationCanceledPacket,
            vec![invitation_canceled_kind::EXPEDITION]
        )]
    );
}

/// Runs one team action on a fresh party of three (A owner, B, C): the direct packet
/// (0x7d / 0x7e / 0x84) and the `CSAskRiskyTeamAction` it is the confirmation of. Returns
/// every member's frames so the two paths can be compared byte for byte.
async fn team_action_frames(action: u8, direct: bool) -> [Vec<(u16, Vec<u8>)>; 3] {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    let team_id = form_party(&server, &mut a, &mut b).await;
    a.send(&server, cs::CSInviteToTeamPacket, invite(team_id, "Cy"))
        .await;
    c.send(
        &server,
        cs::CSReplyToJoinTeamPacket,
        reply(team_id, a.p.character_id, false),
    )
    .await;
    settle(&mut [&mut a, &mut b, &mut c]);

    let target = match action {
        risky_action::LEAVE => b.p.character_id,
        risky_action::KICK => c.p.character_id,
        risky_action::DISMISS => a.p.character_id,
        _ => unreachable!(),
    };
    if direct {
        match action {
            risky_action::LEAVE => {
                b.send(&server, cs::CSLeaveTeamPacket, direct_leave(team_id))
                    .await
            }
            risky_action::KICK => {
                a.send(
                    &server,
                    cs::CSKickTeamMemberPacket,
                    direct_kick(team_id, target),
                )
                .await
            }
            _ => {
                a.send(&server, cs::CSDismissTeamPacket, direct_dismiss(team_id))
                    .await
            }
        }
    } else {
        let actor = if action == risky_action::LEAVE {
            &mut b
        } else {
            &mut a
        };
        actor
            .send(
                &server,
                cs::CSAskRiskyTeamActionPacket,
                risky(team_id, target, action),
            )
            .await;
    }
    [a.frames(), b.frames(), c.frames()]
}

/// An invitation issued by a character that goes offline is dropped and its invitee's popup is
/// closed.
#[tokio::test]
async fn team_invitation_is_canceled_when_the_inviter_goes_offline() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    settle(&mut [&mut a, &mut b]);

    a.send(&server, cs::CSInviteToTeamPacket, invite(0, "Bob"))
        .await;
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);

    crate::world::session::set_online(&server, &a.p, false);
    assert_eq!(
        b.frames(),
        vec![
            error_frame(crate::systems::social::util::error::TEAM_INVITOR_OFFLINE),
            (
                sc::SCInvitationCanceledPacket,
                vec![invitation_canceled_kind::TEAM]
            )
        ]
    );
}

#[tokio::test]
async fn family_reply_needs_the_invitation_it_names() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    let invite = |name: &str| {
        let mut s = PacketStream::new();
        s.write_str(name).write_str("Kid");
        s
    };
    let reply = |invitor: u32| {
        let mut s = PacketStream::new();
        s.write_u32(invitor).write_bool(true).write_str("Kid");
        s
    };
    // No invitation was sent: naming Al as the inviter joins nothing.
    b.send(&server, cs::CSFamilyReplyInvitationPacket, reply(1))
        .await;
    assert_eq!((a.p.data().family, b.p.data().family), (0, 0));
    assert_eq!(server.systems.social.families.count(), 0);
    // An invitation from Al is not an invitation from Cy.
    a.send(&server, cs::CSFamilyInviteMemberPacket, invite("Bob"))
        .await;
    b.send(&server, cs::CSFamilyReplyInvitationPacket, reply(3))
        .await;
    assert_eq!((b.p.data().family, c.p.data().family), (0, 0));
    // The matching reply works once; it is consumed.
    b.send(&server, cs::CSFamilyReplyInvitationPacket, reply(1))
        .await;
    let fid = a.p.data().family;
    assert_ne!(fid, 0);
    assert_eq!(b.p.data().family, fid);
    b.send(&server, cs::CSFamilyLeavePacket, PacketStream::new())
        .await;
    b.send(&server, cs::CSFamilyReplyInvitationPacket, reply(1))
        .await;
    assert_eq!(b.p.data().family, 0);
    // A member that is not the steward cannot invite.
    a.send(&server, cs::CSFamilyInviteMemberPacket, invite("Bob"))
        .await;
    b.send(&server, cs::CSFamilyReplyInvitationPacket, reply(1))
        .await;
    // (A two-member family disbands when one leaves, so this is a new family.)
    let fid = a.p.data().family;
    assert_ne!(fid, 0);
    assert_eq!(b.p.data().family, fid);
    c.ops();
    b.send(&server, cs::CSFamilyInviteMemberPacket, invite("Cy"))
        .await;
    assert!(c.ops().is_empty());
    c.send(&server, cs::CSFamilyReplyInvitationPacket, reply(2))
        .await;
    assert_eq!(c.p.data().family, 0);
}

#[tokio::test]
async fn over_head_marker_needs_team_membership() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut d = pc(&server, 4, "Dee", 130.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut d]);
    let team_id = form_party(&server, &mut a, &mut b).await;
    a.ops();
    b.ops();
    let mut s = PacketStream::new();
    s.write_u32(team_id)
        .write_i32(3)
        .write_u8(2)
        .write_bc(0x102);
    d.send(&server, cs::CSSetOverHeadMarkerPacket, s).await;
    assert!(a.ops().is_empty() && b.ops().is_empty() && d.ops().is_empty());
}
