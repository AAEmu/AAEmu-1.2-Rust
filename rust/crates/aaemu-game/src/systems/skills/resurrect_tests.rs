//! `CSResurrectCharacterPacket`: an in-place resurrection needs a pending offer.

use crate::net::registry::ClientPacket;
use crate::test_support::{spawn_player, test_character, test_server, TestClient};

use super::resurrect::{offer_resurrection, CSResurrectCharacterPacket};

fn dead_player(server: &crate::server::GameServer) -> (TestClient, crate::world::PlayerRef) {
    let mut client = TestClient::new(server, 1, 1001);
    let player = spawn_player(server, &client, test_character(1, "Al", 10.0, 10.0), 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();
    {
        let mut c = player.data();
        c.unit.max_hp = 1000;
        c.unit.max_mp = 1000;
        c.unit.hp = 0;
        c.unit.mp = 0;
    }
    (client, player)
}

#[tokio::test]
async fn in_place_resurrection_without_an_offer_is_refused() {
    let server = test_server();
    let (mut client, player) = dead_player(&server);
    CSResurrectCharacterPacket { in_place: true }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert_eq!(
        player.data().unit.hp,
        0,
        "the character was revived in place without a pending offer"
    );
    assert!(client.drain_ops().is_empty());
}

#[tokio::test]
async fn in_place_resurrection_consumes_the_pending_offer() {
    let server = test_server();
    let (mut client, player) = dead_player(&server);
    offer_resurrection(&player);
    CSResurrectCharacterPacket { in_place: true }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    {
        let c = player.data();
        assert_eq!(c.unit.hp, 10, "1 % of 1000 HP by default");
        assert!(!c.skill_char.resurrect_offer, "the offer is consumed");
    }
    assert_eq!(
        client.drain_ops(),
        vec![
            crate::opcodes::sc::SCCharacterResurrectedPacket,
            crate::opcodes::sc::SCUnitPointsPacket
        ]
    );
    // The same offer cannot revive a second time (the character is alive now).
    player.data().unit.hp = 0;
    CSResurrectCharacterPacket { in_place: true }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert_eq!(player.data().unit.hp, 0);
}

#[tokio::test]
async fn a_new_death_clears_a_stale_offer_above_the_free_resurrection_level() {
    let server = test_server();
    let (_client, player) = dead_player(&server);
    offer_resurrection(&player);
    player.data().unit.level = 20;
    super::combat::character_death_losses(&server, &player);
    assert!(!player.data().skill_char.resurrect_offer);
}
