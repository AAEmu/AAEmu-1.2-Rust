//! Friend / blocked-user refusal flow tests: every failed add / remove / block / unblock
//! answers with the client's packet and error id (WP-SL2), and the success frames are the
//! unchanged ones.

use aaemu_commons::PacketStream;

use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::social::packets::body;
use crate::systems::social::packets::friend::{
    SCAddBlockedUserPacket, SCAddFriendPacket, SCDeleteBlockedUserPacket, SCDeleteFriendPacket,
};
use crate::test_support::{spawn_player, test_character, test_server, TestClient};
use crate::world::PlayerRef;

struct Pc {
    c: TestClient,
    p: PlayerRef,
}

impl Pc {
    fn frames(&mut self) -> Vec<(u16, Vec<u8>)> {
        self.c.drain()
    }

    async fn send(&mut self, server: &GameServer, opcode: u16, body: PacketStream) {
        server
            .handle_frame(&mut self.c.conn, &encode_raw(1, opcode, body.as_bytes()))
            .await
            .expect("handler");
    }
}

fn pc(server: &GameServer, id: u32, name: &str) -> Pc {
    let c = TestClient::new(server, id, 1000 + id);
    let p = spawn_player(
        server,
        &c,
        test_character(id, name, 100.0 + id as f32, 100.0),
        0x100 + id,
    );
    p.data().is_online = true;
    let mut c = c;
    c.conn.active_char = Some(p.clone());
    c.drain();
    Pc { c, p }
}

fn name_body(name: &str) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_str(name);
    s
}

#[tokio::test]
async fn add_friend_refusals_answer_unknown_self_and_duplicate() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al");
    let b = pc(&server, 2, "Bob");
    a.frames();

    // 551: the client prints "$1 does not exist." from `FriendInfo.name`.
    a.send(&server, cs::CSAddFriendPacket, name_body("Nobody"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddFriendPacket,
            body(&SCAddFriendPacket::failure(551, "Nobody"))
        )]
    );
    assert!(a.p.data().friends.is_empty());

    // 501: self-add is refused (deviation from the C# `AddFriend`, recorded in PORTING.md).
    a.send(&server, cs::CSAddFriendPacket, name_body("Al"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddFriendPacket,
            body(&SCAddFriendPacket::failure(501, "Al"))
        )]
    );
    assert!(a.p.data().friends.is_empty());

    // Success is unchanged, and a second add is refused with 502 (INFERRED id).
    a.send(&server, cs::CSAddFriendPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(sc::SCAddFriendPacket, {
            let mut s = PacketStream::new();
            let c = b.p.data();
            let info = crate::systems::social::friend::format_friend(&c, c.in_party);
            drop(c);
            use aaemu_commons::Marshal;
            info.write(&mut s);
            s.write_bool(true).write_i16(0);
            s.into_bytes()
        })]
    );
    assert_eq!(a.p.data().friends.len(), 1);
    assert_eq!(server.friends.owners_of(2), vec![1]);

    a.send(&server, cs::CSAddFriendPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddFriendPacket,
            body(&SCAddFriendPacket::failure(502, "Bob"))
        )]
    );
    assert_eq!(a.p.data().friends.len(), 1);
}

#[tokio::test]
async fn add_friend_of_a_character_that_blocked_the_owner_answers_unknown_user() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al");
    let b = pc(&server, 2, "Bob");
    b.p.data().blocked.push((2, 1));
    a.frames();

    a.send(&server, cs::CSAddFriendPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddFriendPacket,
            body(&SCAddFriendPacket::failure(551, "Bob"))
        )]
    );
    assert!(a.p.data().friends.is_empty());
}

#[tokio::test]
async fn remove_friend_refusals_answer_unknown_and_not_on_the_list() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al");
    let _b = pc(&server, 2, "Bob");
    a.frames();

    a.send(&server, cs::CSDeleteFriendPacket, name_body("Nobody"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCDeleteFriendPacket,
            body(&SCDeleteFriendPacket::failure(551, 0, "Nobody"))
        )]
    );

    // A known character that is not on the list: 556 with its id.
    a.send(&server, cs::CSDeleteFriendPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCDeleteFriendPacket,
            body(&SCDeleteFriendPacket::failure(556, 2, "Bob"))
        )]
    );

    // The success frame is unchanged.
    a.send(&server, cs::CSAddFriendPacket, name_body("Bob"))
        .await;
    a.frames();
    a.send(&server, cs::CSDeleteFriendPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCDeleteFriendPacket,
            body(&SCDeleteFriendPacket {
                character_id: 2,
                success: true,
                friend_name: "Bob".into(),
                error_message: 0,
            })
        )]
    );
    assert!(a.p.data().friends.is_empty());
    assert_eq!(a.p.data().removed_friends, vec![2]);
}

#[tokio::test]
async fn block_refusals_answer_self_unknown_and_already_blocked() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al");
    let _b = pc(&server, 2, "Bob");
    a.frames();

    a.send(&server, cs::CSAddBlockedUserPacket, name_body("Al"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(&SCAddBlockedUserPacket::failure(352, 1, "Al"))
        )]
    );
    assert!(a.p.data().blocked.is_empty());

    a.send(&server, cs::CSAddBlockedUserPacket, name_body("Nobody"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(&SCAddBlockedUserPacket::failure(551, 0, "Nobody"))
        )]
    );

    // The success frame is unchanged; adding again is 353 (INFERRED id).
    a.send(&server, cs::CSAddBlockedUserPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(&SCAddBlockedUserPacket {
                character_id: 2,
                character_name: "Bob".into(),
                success: true,
                error_message: 0,
            })
        )]
    );
    assert_eq!(a.p.data().blocked, vec![(1, 2)]);

    a.send(&server, cs::CSAddBlockedUserPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCAddBlockedUserPacket,
            body(&SCAddBlockedUserPacket::failure(353, 2, "Bob"))
        )]
    );
    assert_eq!(a.p.data().blocked, vec![(1, 2)]);
}

#[tokio::test]
async fn unblock_refusals_answer_unknown_and_not_blocked() {
    let server = test_server();
    let mut a = pc(&server, 1, "Al");
    let _b = pc(&server, 2, "Bob");
    a.frames();

    a.send(&server, cs::CSDeleteBlockedUserPacket, name_body("Nobody"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCDeleteBlockedUserPacket,
            body(&SCDeleteBlockedUserPacket::failure(551, 0, "Nobody"))
        )]
    );

    a.send(&server, cs::CSDeleteBlockedUserPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCDeleteBlockedUserPacket,
            body(&SCDeleteBlockedUserPacket::failure(354, 2, "Bob"))
        )]
    );

    // The success frame is unchanged.
    a.send(&server, cs::CSAddBlockedUserPacket, name_body("Bob"))
        .await;
    a.frames();
    a.send(&server, cs::CSDeleteBlockedUserPacket, name_body("Bob"))
        .await;
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCDeleteBlockedUserPacket,
            body(&SCDeleteBlockedUserPacket {
                character_id: 2,
                success: true,
                blocked_name: "Bob".into(),
                error_message: 0,
            })
        )]
    );
    assert!(a.p.data().blocked.is_empty());
    assert_eq!(a.p.data().removed_blocked, vec![2]);
}
