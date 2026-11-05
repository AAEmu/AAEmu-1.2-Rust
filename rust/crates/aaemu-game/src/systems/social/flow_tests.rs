//! Social flow tests with several connections (no MySQL: saves fail fast and are logged).

use aaemu_commons::PacketStream;

use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::social::chat::chat_type;
use crate::systems::social::packets::body;
use crate::systems::social::packets::team::*;
use crate::systems::social::team::risky_action;
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
    let mut s = PacketStream::new();
    s.write_u32(team_id)
        .write_bool(true)
        .write_u32(owner)
        .write_bool(reject)
        .write_str("")
        .write_bool(false);
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
        Err(error::CHAT_NOT_JOINED_CHANNEL)
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
    let mut owner = PacketStream::new();
    owner.write_u32(3);
    a.send(&server, cs::CSFamilyChangeOwnerPacket, owner).await;
    assert!(a.ops().is_empty() && b.ops().is_empty() && c.ops().is_empty());
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
    // Adding twice does nothing.
    a.send(&server, cs::CSAddFriendPacket, name("Bob")).await;
    assert!(a.ops().is_empty());
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
    // Unknown (offline) names are ignored for blocks.
    a.send(&server, cs::CSAddBlockedUserPacket, name("Nobody"))
        .await;
    assert!(a.ops().is_empty());
    let _ = b.ops();
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
    assert_eq!(b.ops(), vec![sc::SCAskToJoinTeamPacket]);
    // 200 m away: outside the 100 m radius.
    assert!(far.ops().is_empty());
    // The inviter hears nothing (no "invite yourself" error for being around itself).
    assert!(a.ops().is_empty());
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

// ------------------------------------------------------------------------------------------
// Guild / party fixes (see PORTING.md "Guild and party fixes (2026-11-04)").
// ------------------------------------------------------------------------------------------

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

pub(super) fn name_body(n: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_str(n);
    s
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
    assert_eq!(b.frames(), vec![error_frame(error::TEAM_INVITOR_OFFLINE)]);
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
