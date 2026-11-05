//! Guild rename flow: the 1.2 client opens the dialog only for `SCExpeditionShowRenameUI`, sends
//! `CSRenameExpeditionPacket` from it, and every client updates its faction cache from
//! `SCFactionRenamedPacket`. Invalid or taken names answer with `SCFactionRetryRenamePacket`
//! (dialog re-opened, nothing changed); the nation half (`is_expedition = false`) is not ours.

use aaemu_commons::PacketStream;

use crate::data::world_data::SystemFaction;
use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::social::expedition::{self, Expedition, ExpeditionMember};
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

fn pc(server: &GameServer, id: u32, name: &str, x: f32) -> Pc {
    let c = TestClient::new(server, id, 1000 + id);
    let p = spawn_player(server, &c, test_character(id, name, x, 100.0), 0x100 + id);
    p.data().is_online = true;
    let mut c = c;
    c.conn.active_char = Some(p.clone());
    c.drain();
    Pc { c, p }
}

fn settle(pcs: &mut [&mut Pc]) {
    for p in pcs.iter_mut() {
        p.c.drain();
    }
}

fn wire_member(server: &GameServer, id: u32, p: &PlayerRef, owner: bool) -> ExpeditionMember {
    expedition::member_from_character(server, id, p, owner)
}

fn guild(
    id: u32,
    name: &str,
    owner_id: u32,
    owner_name: &str,
    members: Vec<ExpeditionMember>,
) -> Expedition {
    Expedition {
        faction: SystemFaction {
            id,
            name: name.into(),
            owner_id,
            owner_name: owner_name.into(),
            ..SystemFaction::default()
        },
        members,
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    }
}

fn rename_body(id: u32, name: &str, is_expedition: bool) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(id).write_str(name).write_bool(is_expedition);
    s
}

fn decode_renamed(body: &[u8]) -> (u32, String, bool) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_u32().expect("id"),
        s.read_string().expect("name"),
        s.read_bool().expect("byGm"),
    )
}

fn decode_retry(body: &[u8]) -> (bool, i16, String) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_bool().expect("xpdt"),
        s.read_i16().expect("error"),
        s.read_string().expect("name"),
    )
}

fn find(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<&[u8]> {
    frames
        .iter()
        .filter(|(o, _)| *o == op)
        .map(|(_, b)| b.as_slice())
        .collect()
}

#[tokio::test]
async fn owner_rename_is_broadcast_to_everyone_with_the_exact_wire_body() {
    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let mut member = pc(&server, 2, "Member", 101.0);
    let mut outsider = pc(&server, 9, "Outsider", 102.0);
    let id = 7;
    owner.p.data().unit.expedition_id = id;
    member.p.data().unit.expedition_id = id;
    let members = vec![
        wire_member(&server, id, &owner.p, true),
        wire_member(&server, id, &member.p, false),
    ];
    server
        .expeditions
        .add(guild(id, "Knights", 1, "Owner", members));
    settle(&mut [&mut owner, &mut member, &mut outsider]);

    // Only the server can open the 1.2 rename dialog: an empty SCExpeditionShowRenameUI (the
    // offer also grants the right, so the faction record is sent again as well).
    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");
    let frames = owner.frames();
    let ui = find(&frames, sc::SCExpeditionShowRenameUIPacket);
    assert_eq!(ui.len(), 1);
    assert!(ui[0].is_empty());
    assert!(!find(&frames, sc::SCFactionListPacket).is_empty());
    assert!(server.expeditions.has_rename_offer(1, id));

    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Renamed", true),
        )
        .await;

    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Renamed");
    assert!(!server.expeditions.has_rename_offer(1, id));
    for x in [&mut owner, &mut member, &mut outsider] {
        let frames = x.frames();
        let renamed = find(&frames, sc::SCFactionRenamedPacket);
        assert_eq!(
            renamed.len(),
            1,
            "every online client must learn the new name"
        );
        assert_eq!(decode_renamed(renamed[0]), (id, "Renamed".into(), false));
        // Literal layout: u32 id, 2-byte length + name, bool byGm.
        let mut expected = vec![7, 0, 0, 0, 7, 0];
        expected.extend_from_slice(b"Renamed");
        expected.push(0);
        assert_eq!(renamed[0], expected.as_slice());
    }
}

#[tokio::test]
async fn rename_requires_the_owner_and_a_show_rename_ui_offer() {
    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let mut member = pc(&server, 2, "Member", 101.0);
    let id = 7;
    owner.p.data().unit.expedition_id = id;
    member.p.data().unit.expedition_id = id;
    let members = vec![
        wire_member(&server, id, &owner.p, true),
        wire_member(&server, id, &member.p, false),
    ];
    server
        .expeditions
        .add(guild(id, "Knights", 1, "Owner", members));
    settle(&mut [&mut owner, &mut member]);

    // A forged CS without the dialog that the server opened changes nothing, even when the
    // right itself is pending.
    server
        .expeditions
        .with_mut(id, |e| e.faction.allow_change_name = 1);
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Forged", true),
        )
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Knights");
    let frames = owner.frames();
    assert_eq!(find(&frames, sc::SCFactionRenamedPacket).len(), 0);
    let errors = find(&frames, sc::SCErrorMsgPacket);
    assert_eq!(errors.len(), 1);
    assert_eq!(i16::from_le_bytes([errors[0][0], errors[0][1]]), 307);

    // A member never gets the offer from the service.
    assert_eq!(
        expedition::offer_expedition_rename_ui(&server, &member.p),
        Err(807)
    );
    // A (forged) member request with the offer forced on is refused by the owner check.
    server.expeditions.offer_rename(2, id);
    member
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Mutiny", true),
        )
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Knights");
    let frames = member.frames();
    assert_eq!(find(&frames, sc::SCFactionRenamedPacket).len(), 0);
    let errors = find(&frames, sc::SCErrorMsgPacket);
    assert_eq!(i16::from_le_bytes([errors[0][0], errors[0][1]]), 807);
    // A refused request does not spend the offer; only a successful rename consumes it.
    assert!(server.expeditions.has_rename_offer(2, id));

    // The nation half (`is_expedition = false`) belongs to the nation rename service
    // (RENAME-7). Without a nation rename offer it is refused with 748 and does not touch
    // either the guild name or the outstanding guild offer.
    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");
    let frames = owner.frames();
    assert_eq!(find(&frames, sc::SCExpeditionShowRenameUIPacket).len(), 1);
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Nation Name", false),
        )
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Knights");
    // The nation half is now the nation rename service (RENAME-7): without a nation rename
    // offer it is refused with 748 and does not touch the guild name or its offer.
    let frames = owner.frames();
    assert_eq!(find(&frames, sc::SCFactionRenamedPacket).len(), 0);
    let errors = find(&frames, sc::SCErrorMsgPacket);
    assert_eq!(errors.len(), 1);
    assert_eq!(
        i16::from_le_bytes([errors[0][0], errors[0][1]]),
        crate::systems::economy::error_message::ONLY_NATION_OWNER
    );
    assert!(server.expeditions.has_rename_offer(1, id));
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Renamed", true),
        )
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Renamed");
}

#[tokio::test]
async fn invalid_and_taken_names_retry_without_changing_and_keep_the_offer() {
    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let id = 7;
    owner.p.data().unit.expedition_id = id;
    let members = vec![wire_member(&server, id, &owner.p, true)];
    server
        .expeditions
        .add(guild(id, "Knights", 1, "Owner", members));
    server.expeditions.add(guild(
        8,
        "Taken",
        2,
        "Other",
        vec![ExpeditionMember {
            expedition_id: 8,
            character_id: 2,
            in_party: false,
            is_online: false,
            last_world_leave_time: 0,
            name: "Other".into(),
            level: 1,
            zone_id: 0,
            id3: 0,
            abilities: [0; 3],
            role: 255,
            position: (0.0, 0.0, 0.0),
            memo: String::new(),
            transfer_requested_time: 0,
        }],
    ));
    settle(&mut [&mut owner]);

    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");

    for (name, code) in [
        ("x".to_string(), 296),     // name regex (too short)
        ("A".repeat(33), 297),      // 33 UTF-16 units
        ("Taken".to_string(), 299), // another guild has it
    ] {
        owner
            .send(
                &server,
                cs::CSRenameExpeditionPacket,
                rename_body(id, &name, true),
            )
            .await;
        let frames = owner.frames();
        let retry = find(&frames, sc::SCFactionRetryRenamePacket);
        assert_eq!(retry.len(), 1, "{name:?} must answer with a retry dialog");
        assert_eq!(decode_retry(retry[0]), (true, code, name.to_string()));
        assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Knights");
        assert!(server.expeditions.has_rename_offer(1, id));
    }

    // The retry dialog is the same offer: correcting the name now succeeds.
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Fine", true),
        )
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Fine");
    assert!(!server.expeditions.has_rename_offer(1, id));
}

#[test]
fn retry_packet_body_is_byte_exact() {
    use crate::systems::social::packets::body;
    use crate::systems::social::packets::faction::SCFactionRetryRenamePacket;

    let mut expected = vec![1];
    expected.extend_from_slice(&299i16.to_le_bytes());
    expected.extend_from_slice(&[5, 0]);
    expected.extend_from_slice(b"Taken");
    assert_eq!(
        body(&SCFactionRetryRenamePacket {
            xpdt: true,
            error_message: 299,
            name: "Taken".into(),
        }),
        expected
    );
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary expedition tables"]
async fn private_mysql_guild_rename_is_persisted() {
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
    for table in ["expeditions", "expedition_members"] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(&ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1))
            .execute(&pool)
            .await
            .unwrap();
    }

    let mut server = test_server();
    server.db = pool.clone();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let id = 77;
    owner.p.data().unit.expedition_id = id;
    let members = vec![wire_member(&server, id, &owner.p, true)];
    server
        .expeditions
        .add(guild(id, "Knights", 1, "Owner", members));
    settle(&mut [&mut owner]);

    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Renamed", true),
        )
        .await;

    let saved: String = sqlx::query_scalar("SELECT name FROM expeditions WHERE id=77")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(saved, "Renamed");
}

#[tokio::test]
async fn logout_clears_the_rename_offer() {
    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let id = 7;
    owner.p.data().unit.expedition_id = id;
    let members = vec![wire_member(&server, id, &owner.p, true)];
    server
        .expeditions
        .add(guild(id, "Knights", 1, "Owner", members));
    settle(&mut [&mut owner]);

    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");
    assert!(server.expeditions.has_rename_offer(1, id));

    // The real leave-world path (world::session), not a helper only tests call.
    crate::world::session::leave_world_task(&server, None, 0, Some(owner.p.clone())).await;
    assert!(
        !server.expeditions.has_rename_offer(1, id),
        "a departed character must not keep a rename dialog offer"
    );
}

#[tokio::test]
async fn disband_clears_the_offer_and_a_new_guild_needs_a_new_one() {
    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let id = 7;
    owner.p.data().unit.expedition_id = id;
    let members = vec![wire_member(&server, id, &owner.p, true)];
    server
        .expeditions
        .add(guild(id, "Knights", 1, "Owner", members));
    settle(&mut [&mut owner]);

    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");
    assert!(server.expeditions.has_rename_offer(1, id));
    assert!(expedition::disband(&server, &owner.p).await);
    assert!(
        !server.expeditions.has_rename_offer(1, id),
        "the disbanded guild's offer must be dropped"
    );
    owner.frames();

    // A guild created afterwards must not inherit the old offer.
    let new_id = 8;
    owner.p.data().unit.expedition_id = new_id;
    let members = vec![wire_member(&server, new_id, &owner.p, true)];
    server
        .expeditions
        .add(guild(new_id, "New Guild", 1, "Owner", members));
    server
        .expeditions
        .with_mut(new_id, |e| e.faction.allow_change_name = 1);
    settle(&mut [&mut owner]);
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(new_id, "Inherited", true),
        )
        .await;
    assert_eq!(
        server.expeditions.get(new_id).unwrap().faction.name,
        "New Guild"
    );
    let frames = owner.frames();
    let errors = find(&frames, sc::SCErrorMsgPacket);
    assert_eq!(i16::from_le_bytes([errors[0][0], errors[0][1]]), 307);
    assert!(find(&frames, sc::SCFactionRenamedPacket).is_empty());
}

#[tokio::test]
async fn owner_change_clears_the_rename_offer() {
    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let mut member = pc(&server, 2, "Member", 101.0);
    let id = 7;
    owner.p.data().unit.expedition_id = id;
    member.p.data().unit.expedition_id = id;
    let members = vec![
        wire_member(&server, id, &owner.p, true),
        wire_member(&server, id, &member.p, false),
    ];
    server
        .expeditions
        .add(guild(id, "Knights", 1, "Owner", members));
    settle(&mut [&mut owner, &mut member]);

    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");
    assert!(server.expeditions.has_rename_offer(1, id));
    expedition::change_owner(&server, &owner.p, 2).await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.owner_id, 2);
    assert!(
        !server.expeditions.has_rename_offer(1, id),
        "the outgoing owner's offer must not survive the hand-over"
    );
    owner.frames();

    // The outgoing owner is no longer the owner: the owner check refuses it.
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "ExOwner", true),
        )
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Knights");
    let frames = owner.frames();
    let errors = find(&frames, sc::SCErrorMsgPacket);
    assert_eq!(i16::from_le_bytes([errors[0][0], errors[0][1]]), 807);
    assert!(find(&frames, sc::SCFactionRenamedPacket).is_empty());

    // The new owner still holds the granted right, but the hand-over dropped the dialog
    // offer: a forged request is refused with the bad-status code.
    member
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "NewOwner", true),
        )
        .await;
    assert_eq!(server.expeditions.get(id).unwrap().faction.name, "Knights");
    let frames = member.frames();
    let errors = find(&frames, sc::SCErrorMsgPacket);
    assert_eq!(i16::from_le_bytes([errors[0][0], errors[0][1]]), 307);
    assert!(find(&frames, sc::SCFactionRenamedPacket).is_empty());
}

#[tokio::test]
async fn an_offer_is_bound_to_its_guild() {
    let server = test_server();
    let mut owner = pc(&server, 1, "Owner", 100.0);
    let id = 7;
    let other = 8;
    owner.p.data().unit.expedition_id = id;
    server.expeditions.add(guild(
        id,
        "Knights",
        1,
        "Owner",
        vec![wire_member(&server, id, &owner.p, true)],
    ));
    server.expeditions.add(guild(
        other,
        "Other Guild",
        1,
        "Owner",
        vec![wire_member(&server, other, &owner.p, true)],
    ));
    settle(&mut [&mut owner]);

    expedition::offer_expedition_rename_ui(&server, &owner.p).expect("offer");
    assert!(server.expeditions.has_rename_offer(1, id));
    assert!(!server.expeditions.has_rename_offer(1, other));

    // The offer was made for guild 7. Move the character into guild 8 (it owns that one too in
    // this fixture) and grant guild 8 the right without a dialog: the old offer for 7 must not
    // authorize the request.
    owner.p.data().unit.expedition_id = other;
    server
        .expeditions
        .with_mut(other, |e| e.faction.allow_change_name = 1);
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(other, "Stolen Offer", true),
        )
        .await;
    assert_eq!(
        server.expeditions.get(other).unwrap().faction.name,
        "Other Guild"
    );
    let frames = owner.frames();
    let errors = find(&frames, sc::SCErrorMsgPacket);
    assert_eq!(i16::from_le_bytes([errors[0][0], errors[0][1]]), 307);

    // Back in guild 7 the offer it was made for still works.
    owner.p.data().unit.expedition_id = id;
    owner
        .send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(id, "Knights Renamed", true),
        )
        .await;
    assert_eq!(
        server.expeditions.get(id).unwrap().faction.name,
        "Knights Renamed"
    );
    assert!(!server.expeditions.has_rename_offer(1, id));
}
