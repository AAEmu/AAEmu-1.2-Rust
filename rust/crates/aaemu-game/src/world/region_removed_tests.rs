//! `Player::note_region_removed_npc` / `take_region_removed_npcs`: the per-player log of the NPC
//! units the region visibility path took off the client, drained by the quest-map range sync.
//! The flow tests (a streamed marker is streamed again after a region removal) are in
//! `systems/quests/quest_map.rs`; this file pins the log itself.

use super::REGION_REMOVED_NPC_LOG;
use crate::test_support::{spawn_player, test_character, test_server, TestClient};

/// Every player gets the log, and only a player who selected a quest-map range ever drains it:
/// one who never did must not accumulate more than the cap, whatever walking around removes.
#[tokio::test]
async fn the_log_of_a_player_nobody_drains_keeps_only_the_newest_entries() {
    let server = test_server();
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Alpha", 100.0, 100.0),
        0x100,
    );

    let total = REGION_REMOVED_NPC_LOG as u32 + 5;
    for id in 0..total {
        player.note_region_removed_npc(id);
    }

    let logged = player.take_region_removed_npcs();
    assert_eq!(logged.len(), REGION_REMOVED_NPC_LOG);
    assert_eq!(logged.first(), Some(&5), "the oldest entries are dropped");
    assert_eq!(logged.last(), Some(&(total - 1)), "the newest is kept");
    assert!(
        player.take_region_removed_npcs().is_empty(),
        "taking the log drains it"
    );
}
