use super::*;
use crate::data::{items::ItemTemplate, GameData};
use crate::systems::economy::inventory::Inv;
use crate::systems::npcs::doodad::templates::DoodadTemplate;
use crate::test_support::*;

fn fixture() -> (GameServer, PlayerRef, u64) {
    let (server, player, item_id, _client) = fixture_with_client();
    (server, player, item_id)
}

fn fixture_with_client() -> (GameServer, PlayerRef, u64, TestClient) {
    let mut data = GameData::empty();
    for id in [1000, 1001, 2000] {
        data.items.templates.insert(id, ItemTemplate::plain(id, 20));
    }
    data.items.item_doodads.insert(4000, vec![1000, 1001]);
    data.systems.npcs.doodad.templates.insert(
        4000,
        DoodadTemplate {
            id: 4000,
            ..Default::default()
        },
    );
    let server = test_server_with(data);
    crate::systems::economy::bridges::install(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Planter", 100.0, 100.0),
        0x100,
    );
    player.data().labor_power = 20;
    let containers = player.data().inventory.containers.clone();
    for (_, container) in containers {
        server.items.add_container(container);
    }
    let item = server
        .items
        .create(1000, 2, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .unwrap();
    let item_id = item.id;
    Inv::of(&player)
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, 0, &server.game_data.items)
        .unwrap();
    (server, player, item_id, client)
}

#[tokio::test]
async fn placement_consumes_only_actual_source_and_charges_after_success() {
    let (server, player, id) = fixture();
    let doodad = create_player_doodad_with_labor(
        &server,
        &player,
        4000,
        (101.0, 100.0, 50.0),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        5,
    )
    .unwrap();
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 1);
    assert_eq!(player.data().labor_power, 15);
    assert_eq!(doodad.data().item_template_id, 1000);
    assert_eq!(doodad.data().item_id, 0);
    assert!(doodad.data().is_persistent);
}

#[tokio::test]
async fn failed_placement_keeps_labor_and_inventory_and_creates_no_object() {
    let (server, player, id) = fixture();
    assert!(create_player_doodad_with_labor(
        &server,
        &player,
        4000,
        (101.0, 100.0, 50.0),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        21
    )
    .is_none());
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 2);
    assert_eq!(player.data().labor_power, 20);
    assert!(server.world.world(0).unwrap().all_doodads().is_empty());
    assert!(create_player_doodad_with_labor(
        &server,
        &player,
        9999,
        (101.0, 100.0, 50.0),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        5
    )
    .is_none());
    assert_eq!(player.data().labor_power, 20);
}

#[tokio::test]
async fn single_placement_preserves_item_identity_in_system_container() {
    let (mut server, player, _) = fixture();
    Arc::get_mut(&mut server.game_data)
        .unwrap()
        .items
        .templates
        .get_mut(&1001)
        .unwrap()
        .max_count = 1;
    let item = server
        .items
        .create(1001, 1, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .unwrap();
    let id = item.id;
    Inv::of(&player)
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, 2, &server.game_data.items)
        .unwrap();
    let doodad = create_player_doodad_with_labor(
        &server,
        &player,
        4000,
        (101.0, 100.0, 100.0),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        5,
    )
    .unwrap();
    assert_eq!(doodad.data().item_id, id);
    assert!(Inv::of(&player)
        .bag()
        .lock()
        .unwrap()
        .get_item_by_id(id)
        .is_none());
    let saved = server.items.item_by_id(id).unwrap();
    assert_eq!(saved.slot_type, crate::models::container::slot_type::SYSTEM);
    assert_eq!(saved.template_id, 1001);
    assert_eq!(player.data().labor_power, 15);
}

#[tokio::test]
async fn unrelated_item_and_remote_or_nonfinite_client_positions_are_rejected() {
    let (server, player, id) = fixture();
    let other = server
        .items
        .create(2000, 1, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .unwrap();
    let other_id = other.id;
    Inv::of(&player)
        .bag()
        .lock()
        .unwrap()
        .add_new_item(other, 1, &server.game_data.items)
        .unwrap();
    for (item_id, x) in [(other_id, 101.0), (id, 1000.0), (id, f32::NAN)] {
        handle_create_doodad(
            &server,
            &player,
            &CSCreateDoodadPacket {
                id: 4000,
                x,
                y: 100.0,
                z: 50.0,
                z_rot: 0.0,
                scale: 1.0,
                item_id,
            },
        );
    }
    assert!(server.world.world(0).unwrap().all_doodads().is_empty());
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 2);
    assert!(Inv::of(&player).item_by_id(other_id).is_some());
    assert_eq!(player.data().labor_power, 20);
}

#[tokio::test]
async fn placement_range_is_a_horizontal_sanity_bound() {
    let (server, player, id, mut client) = fixture_with_client();
    let place = |x: f32, y: f32, z: f32| {
        handle_create_doodad(
            &server,
            &player,
            &CSCreateDoodadPacket {
                id: 4000,
                x,
                y,
                z,
                z_rot: 0.0,
                scale: 1.0,
                item_id: id,
            },
        );
    };
    client.drain();
    // 15 m away horizontally, 60 m above: the client sends such placements; accepted (the
    // source item is consumed).
    place(115.0, 100.0, 60.0);
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 1);
    client.drain();
    // 40 m away: refused with "too far away" and nothing consumed.
    place(140.0, 100.0, 50.0);
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 1);
    let (_, body) = client
        .drain()
        .into_iter()
        .find(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
        .expect("too far error");
    assert_eq!(
        i16::from_le_bytes([body[0], body[1]]),
        error_message::TOO_FAR_AWAY
    );
}

/// What the real `skills::progress::add_exp` does with a positive labor experience gain: it
/// ends in `quests::events::on_level_up`, a quest operation that takes the economy ops lock.
/// (The real hook needs the experience tables; this stub keeps the same lock behavior.)
struct LevelUpProgression;

impl crate::systems::economy::hooks::ProgressionHooks for LevelUpProgression {
    fn add_exp(&self, server: &GameServer, player: &PlayerRef, _exp: i32, _add_ability: bool) {
        crate::systems::quests::events::on_level_up(server, player);
    }
}

/// `ItemTxn` holds the economy ops lock while `change_labor` runs; the labor experience
/// (`add_exp` -> quest `on_level_up` -> `with_quests` -> ops lock) used to re-take that
/// non-reentrant lock on the same thread and hang the server (labor-costing doodad placement).
#[tokio::test]
async fn labor_costing_placement_does_not_deadlock_on_the_ops_lock() {
    let (server, player, id) = fixture();
    server
        .systems
        .economy
        .hooks
        .progression
        .install(Arc::new(LevelUpProgression));
    let server = Arc::new(server);
    let s = server.clone();
    let p = player.clone();
    let doodad = finishes_in_time("labor-costing doodad placement", move || {
        create_player_doodad_with_labor(
            &s,
            &p,
            4000,
            (101.0, 100.0, 50.0),
            0.0,
            1.0,
            id,
            0,
            0,
            0,
            true,
            5,
        )
    });
    assert!(doodad.is_some());
    assert_eq!(player.data().labor_power, 15);
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 1);
}
