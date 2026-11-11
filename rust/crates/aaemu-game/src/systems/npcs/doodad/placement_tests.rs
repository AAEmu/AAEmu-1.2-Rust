use std::path::{Path, PathBuf};
use std::sync::Condvar;
use std::time::{Duration, Instant};

use super::*;
use crate::data::client_files::ClientFileManager;
use crate::data::pak::test_util::temp_path;
use crate::data::world_data::{SystemFaction, Zone};
use crate::data::{items::ItemTemplate, GameData};
use crate::models::house::House;
use crate::systems::ai::collision::flow_tests::box_cgf;
use crate::systems::ai::collision::terrain::tests::Plane;
use crate::systems::economy::inventory::Inv;
use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate};
use crate::systems::npcs::doodad::templates::{
    doodad_func_group_kind, DoodadFuncGroups, DoodadTemplate,
};
use crate::systems::npcs::heightmap::{
    HeightmapConfig, HeightmapWorlds, WorldHeightmap, CELL_HMAP_RESOLUTION,
};
use crate::systems::social::expedition::{member_from_character, Expedition};
use crate::test_support::*;
use crate::world::Entity;

/// Above the synthetic world's ocean level (100 m): a spot there is dry. The 1.2 client's water
/// rule (`placement::refusal`) uses `WorldInstance.IsWater`, which counts the ocean below it.
const DRY_Z: f32 = 110.0;

fn fixture() -> (GameServer, PlayerRef, u64) {
    let (server, player, item_id, _client) = fixture_with_client();
    (server, player, item_id)
}

fn fixture_with_client() -> (GameServer, PlayerRef, u64, TestClient) {
    let mut data = GameData::empty();
    for id in [1000, 1001, 2000, 1002, 1003, 1004, 1005] {
        data.items.templates.insert(id, ItemTemplate::plain(id, 20));
    }
    // 4000: a plain, non-round template (the original fixture).
    data.items.item_doodads.insert(4000, vec![1000, 1001]);
    data.systems.npcs.doodad.templates.insert(
        4000,
        DoodadTemplate {
            id: 4000,
            ..Default::default()
        },
    );
    // 4001: a round crop (group 12, sim_radius 500 = 0.5 m), like rice (2246).
    data.items.item_doodads.insert(4001, vec![1002]);
    data.systems.npcs.doodad.templates.insert(
        4001,
        DoodadTemplate {
            id: 4001,
            group_id: 12,
            sim_radius: 500,
            ..Default::default()
        },
    );
    // 4002: a marine crop (group 40, sim_radius 1000 = 1.0 m), like seaweed (411).
    data.items.item_doodads.insert(4002, vec![1003]);
    data.systems.npcs.doodad.templates.insert(
        4002,
        DoodadTemplate {
            id: 4002,
            group_id: 40,
            sim_radius: 1000,
            ..Default::default()
        },
    );
    // 4003: restricted to zone 54, not the synthetic TEST_ZONE (100), like the gem-bush seed (F11).
    data.items.item_doodads.insert(4003, vec![1004]);
    data.systems.npcs.doodad.templates.insert(
        4003,
        DoodadTemplate {
            id: 4003,
            restrict_zone_id: 54,
            ..Default::default()
        },
    );
    // 4005: a non-round placeable (group 24, sim_radius 500).
    data.items.item_doodads.insert(4005, vec![1005]);
    data.systems.npcs.doodad.templates.insert(
        4005,
        DoodadTemplate {
            id: 4005,
            group_id: 24,
            sim_radius: 500,
            ..Default::default()
        },
    );
    let server = test_server_with(data);
    crate::systems::economy::bridges::install(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player_obj = server.ids.object.get_next_id().unwrap();
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Planter", 100.0, 100.0),
        player_obj,
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

fn give(server: &GameServer, player: &PlayerRef, template_id: u32, count: i32) -> u64 {
    let item = server
        .items
        .create(
            template_id,
            count,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .unwrap();
    let id = item.id;
    Inv::of(player)
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, -1, &server.game_data.items)
        .unwrap();
    id
}

fn send_placement(
    client: &mut TestClient,
    server: &GameServer,
    player: &PlayerRef,
    doodad: u32,
    item_id: u64,
    pos: (f32, f32, f32),
) -> Vec<i16> {
    client.drain();
    handle_create_doodad(
        server,
        player,
        &CSCreateDoodadPacket {
            id: doodad,
            x: pos.0,
            y: pos.1,
            z: pos.2,
            z_rot: 0.0,
            scale: 1.0,
            item_id,
        },
    );
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|(_, body)| i16::from_le_bytes([body[0], body[1]]))
        .collect()
}

#[tokio::test]
async fn placement_consumes_only_actual_source_and_charges_after_success() {
    let (server, player, id) = fixture();
    let doodad = create_player_doodad_with_labor(
        &server,
        &player,
        4000,
        (101.0, 100.0, DRY_Z),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        5,
        true,
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
        (101.0, 100.0, DRY_Z),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        21,
        true
    )
    .is_none());
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 2);
    assert_eq!(player.data().labor_power, 20);
    assert!(server.world.world(0).unwrap().all_doodads().is_empty());
    assert!(create_player_doodad_with_labor(
        &server,
        &player,
        9999,
        (101.0, 100.0, DRY_Z),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        5,
        true
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
        (101.0, 100.0, DRY_Z),
        0.0,
        1.0,
        id,
        0,
        0,
        0,
        true,
        5,
        true,
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
    let (server, player, id, mut client) = fixture_with_client();
    let other = give(&server, &player, 2000, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        other,
        (101.0, 100.0, DRY_Z),
    );
    assert_eq!(
        codes,
        vec![error_message::FAILED_TO_USE_ITEM],
        "an item that is not a seed of the doodad answers 207"
    );
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        u64::MAX,
        (101.0, 100.0, DRY_Z),
    );
    assert_eq!(
        codes,
        vec![error_message::FAILED_TO_USE_ITEM],
        "an item id that is in no bag answers 207"
    );
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        id,
        (1000.0, 100.0, DRY_Z),
    );
    assert_eq!(codes, vec![error_message::TOO_FAR_AWAY]);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        id,
        (f32::NAN, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "a malformed position is dropped silently");
    assert!(server.world.world(0).unwrap().all_doodads().is_empty());
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 2);
    assert!(Inv::of(&player).item_by_id(other).is_some());
    assert_eq!(player.data().labor_power, 20);
}

#[tokio::test]
async fn placement_range_is_30_metres_in_3d_inclusive() {
    // `FUN_393a8fa0` refuses only `900.0 < d²` (~761319), so exactly 30 m is allowed. F1's
    // screen-projected sweep cannot pin the boundary.
    let (server, player, id, mut client) = fixture_with_client();
    player.data().unit.transform.set_xyz(100.0, 100.0, DRY_Z);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        id,
        (130.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "exactly 30 m (d² = 900) is allowed");
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 1);

    let (server, player, id, mut client) = fixture_with_client();
    player.data().unit.transform.set_xyz(100.0, 100.0, DRY_Z);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        id,
        (130.01, 100.0, DRY_Z),
    );
    assert_eq!(
        codes,
        vec![error_message::TOO_FAR_AWAY],
        "30.01 m (d² > 900) is refused"
    );
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 2);

    let (server, player, id, mut client) = fixture_with_client();
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        id,
        (129.9, 100.0, 100.5),
    );
    assert!(codes.is_empty(), "29.9 m flat, 0.5 m up = 29.904 m");

    let (server, player, id, mut client) = fixture_with_client();
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        id,
        (100.0, 129.0, 108.0),
    );
    assert_eq!(
        codes,
        vec![error_message::TOO_FAR_AWAY],
        "29 m flat, 8 m up = 30.08 m"
    );
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 2);
}

#[tokio::test]
async fn planting_refuses_water_unless_the_doodad_is_a_marine_crop() {
    // F1/research step 1: group 40 needs water, every other group refuses it, error 276.
    let (server, player, id, mut client) = fixture_with_client();
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4000,
        id,
        (101.0, 100.0, 90.0),
    );
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 2);

    let marine = give(&server, &player, 1003, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4002,
        marine,
        (101.0, 100.0, 90.0),
    );
    assert!(codes.is_empty(), "a marine crop may stand in water");

    let marine = give(&server, &player, 1003, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4002,
        marine,
        (102.0, 100.0, DRY_Z),
    );
    assert_eq!(
        codes,
        vec![error_message::DOODAD_SIM_AREA],
        "a marine crop refuses dry land"
    );
}

#[tokio::test]
async fn round_doodads_refuse_a_spot_inside_the_sum_of_their_radii() {
    // Research step 5 / client PR: sphere test `dist3D < r_new + r_other`, r = sim_radius/1000.
    let (server, player, _, mut client) = fixture_with_client();
    let first = give(&server, &player, 1002, 1);
    assert!(send_placement(
        &mut client,
        &server,
        &player,
        4001,
        first,
        (102.0, 100.0, DRY_Z)
    )
    .is_empty());
    let second = give(&server, &player, 1002, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4001,
        second,
        (102.9, 100.0, DRY_Z),
    );
    assert_eq!(
        codes,
        vec![error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD],
        "0.9 m < 0.5 + 0.5"
    );
    let third = give(&server, &player, 1002, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4001,
        third,
        (103.1, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "1.1 m > 0.5 + 0.5");
    // 0.9 m across and 0.6 m up is 1.08 m in 3D: outside the spheres (a flat test would refuse).
    let fourth = give(&server, &player, 1002, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4001,
        fourth,
        (102.0, 100.9, DRY_Z + 0.6),
    );
    assert!(codes.is_empty(), "3D overlap, not flat");
}

thread_local! {
    /// Set on the two threads of [`two_plantings_at_the_same_spot_race_and_one_wins`], so the
    /// shared pause hook ignores plantings of every other test in this process.
    static RACING_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Greptile P1 on the planting validation: the overlap refusal was a snapshot check, so two
/// players planting at the same empty spot could both pass it, both consume their seed and
/// both insert an overlapping round doodad. The fix holds the world's placement guard from the
/// final check through the doodad's spawn.
///
/// The test hook pauses the first planting just after its check. With the guard, the second
/// planting waits for it and then sees the first doodad (277, seed kept); without the guard it
/// enters the same window, passes its own check and inserts too, so both win (mutation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_plantings_at_the_same_spot_race_and_one_wins() {
    struct RacePause {
        arrived: Mutex<usize>,
        cv: Condvar,
    }
    struct ClearPause;
    impl Drop for ClearPause {
        fn drop(&mut self) {
            set_placement_pause(None);
        }
    }

    let (server, player_a, _, mut client_a) = fixture_with_client();
    let seed_a = give(&server, &player_a, 1002, 1);
    let server = Arc::new(server);
    let client_b = TestClient::new(&server, 2, 1002);
    let player_b = spawn_player(
        &server,
        &client_b,
        test_character(2, "Planter Two", 100.0, 100.0),
        server.ids.object.get_next_id().unwrap(),
    );
    player_b.data().labor_power = 20;
    for (_, container) in player_b.data().inventory.containers.clone() {
        server.items.add_container(container);
    }
    let seed_b = give(&server, &player_b, 1002, 1);
    let mut client_b = client_b;

    let pause = Arc::new(RacePause {
        arrived: Mutex::new(0),
        cv: Condvar::new(),
    });
    set_placement_pause(Some({
        let pause = pause.clone();
        Arc::new(move || {
            if !RACING_THREAD.with(|racing| racing.get()) {
                return;
            }
            let mut arrived = pause.arrived.lock().unwrap();
            *arrived += 1;
            pause.cv.notify_all();
            if *arrived > 1 {
                return;
            }
            // With the guard the second planting waits on it and never arrives here; let the
            // first go after a short wait. Without the guard the second arrives and both go.
            let deadline = Instant::now() + Duration::from_millis(250);
            while *arrived < 2 {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (guard, _) = pause.cv.wait_timeout(arrived, deadline - now).unwrap();
                arrived = guard;
            }
        })
    }));
    let _clear_pause = ClearPause;

    let handle = tokio::runtime::Handle::current();
    let plant =
        |server: Arc<GameServer>, player: PlayerRef, seed: u64, handle: tokio::runtime::Handle| {
            std::thread::spawn(move || {
                let _runtime = handle.enter();
                RACING_THREAD.with(|racing| racing.set(true));
                handle_create_doodad(
                    &server,
                    &player,
                    &CSCreateDoodadPacket {
                        id: 4001,
                        x: 110.0,
                        y: 110.0,
                        z: DRY_Z,
                        z_rot: 0.0,
                        scale: 1.0,
                        item_id: seed,
                    },
                );
            })
        };
    let a = plant(server.clone(), player_a.clone(), seed_a, handle.clone());
    let b = plant(server.clone(), player_b.clone(), seed_b, handle.clone());
    a.join().unwrap();
    b.join().unwrap();

    assert_eq!(
        server.world.world(0).unwrap().all_doodads().len(),
        1,
        "exactly one doodad stands on the spot"
    );
    let codes_a: Vec<i16> = client_a
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|(_, body)| i16::from_le_bytes([body[0], body[1]]))
        .collect();
    let codes_b: Vec<i16> = client_b
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|(_, body)| i16::from_le_bytes([body[0], body[1]]))
        .collect();
    let kept_a = Inv::of(&player_a).item_by_id(seed_a).is_some();
    let kept_b = Inv::of(&player_b).item_by_id(seed_b).is_some();
    assert_ne!(kept_a, kept_b, "exactly one seed is consumed");
    let (winner_codes, loser_codes) = if kept_a {
        (&codes_b, &codes_a)
    } else {
        (&codes_a, &codes_b)
    };
    assert!(
        winner_codes.is_empty(),
        "the winner gets no refusal: {winner_codes:?}"
    );
    assert_eq!(
        loser_codes,
        &vec![error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD],
        "the loser is told the spot is taken"
    );
}

#[tokio::test]
async fn non_round_doodads_skip_the_undecoded_box_overlap() {
    // The client tests oriented boxes / physics colliders for non-round doodads; that shape is
    // not decoded, so the server is permissive (never stricter than the client).
    let (server, player, _, mut client) = fixture_with_client();
    let first = give(&server, &player, 1005, 1);
    assert!(send_placement(
        &mut client,
        &server,
        &player,
        4005,
        first,
        (102.0, 100.0, DRY_Z)
    )
    .is_empty());
    let second = give(&server, &player, 1005, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4005,
        second,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "a non-round pair is not tested");
    assert_eq!(
        server.world.world(0).unwrap().all_doodads().len(),
        2,
        "both non-round doodads stay"
    );

    // The same for a unit next to a non-round doodad (the client uses a physics collider).
    let other_client = TestClient::new(&server, 2, 1002);
    let other = spawn_player(
        &server,
        &other_client,
        test_character(2, "Bystander", 100.0, 100.0),
        0x500,
    );
    other.data().unit.hp = 100;
    other.data().unit.max_hp = 100;
    let third = give(&server, &player, 1005, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4005,
        third,
        (100.3, 100.0, 100.4),
    );
    assert!(codes.is_empty(), "a non-round doodad skips the unit test");
}

#[tokio::test]
async fn units_block_a_plant_with_doodad_sim_too_close_to_other_unit() {
    let (server, player, _, mut client) = fixture_with_client();
    let other_client = TestClient::new(&server, 2, 1002);
    let other_obj = server.ids.object.get_next_id().unwrap();
    let other = spawn_player(
        &server,
        &other_client,
        test_character(2, "Bystander", 100.0, 100.0),
        other_obj,
    );
    assert_eq!(other.data().unit.transform.z, 100.0);
    // `test_character` leaves hp at 0 (dead); the client's unit overlap skips dead units.
    other.data().unit.hp = 100;
    other.data().unit.max_hp = 100;

    let item = give(&server, &player, 1002, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4001,
        item,
        (100.2, 100.0, 100.4),
    );
    assert_eq!(
        codes,
        vec![error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_UNIT]
    );

    let item = give(&server, &player, 1002, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4001,
        item,
        (103.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "3 m away from the bystander");
}

#[tokio::test]
async fn a_restricted_zone_answers_doodad_sim_area() {
    // F11: the original client refuses the gem-bush seed with 276 before it starts a builder.
    let (server, player, _, mut client) = fixture_with_client();
    let item = give(&server, &player, 1004, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4003,
        item,
        (101.0, 100.0, DRY_Z),
    );
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);
    assert!(Inv::of(&player).item_by_id(item).is_some());
}

#[tokio::test]
async fn scripted_create_player_doodad_uses_the_same_placement_rules() {
    // The GM / script path `CreatePlayerDoodad` goes through the same validation as the packet.
    let (server, player, _, mut client) = fixture_with_client();
    client.drain();
    assert!(create_player_doodad(
        &server,
        &player,
        4000,
        (101.0, 100.0, 90.0),
        0.0,
        1.0,
        0,
        0,
        0,
        0,
        true
    )
    .is_none());
    let codes: Vec<i16> = client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|(_, body)| i16::from_le_bytes([body[0], body[1]]))
        .collect();
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);
    assert!(server.world.world(0).unwrap().all_doodads().is_empty());

    assert!(create_player_doodad(
        &server,
        &player,
        4000,
        (101.0, 100.0, DRY_Z),
        0.0,
        1.0,
        0,
        0,
        0,
        0,
        true
    )
    .is_some());
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
            (101.0, 100.0, DRY_Z),
            0.0,
            1.0,
            id,
            0,
            0,
            0,
            true,
            5,
            true,
        )
    });
    assert!(doodad.is_some());
    assert_eq!(player.data().labor_power, 15);
    assert_eq!(Inv::of(&player).item_by_id(id).unwrap().count, 1);
}

/// The real seed / sapling templates and their radii; `AAEMU_COMPACT_DB` points at the 1.2
/// `compact.sqlite3` (`AAEMU_COMPACT_DB=... cargo test -p aaemu-game real_seed -- --ignored`).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_seed_and_sapling_templates_use_the_1_2_placement_rules() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let items = crate::data::ItemData::load(&conn).unwrap();
    let doodads = crate::systems::npcs::doodad::data::DoodadData::load_from(&conn).unwrap();

    // Rice Seed 15646 -> Rice Plant 2246 (group 12, sim_radius 500).
    let rice = doodads.template(2246).expect("rice template 2246");
    assert!(items.item_ids_from_doodad(2246).contains(&15646));
    assert_eq!(rice.group_id, 12);
    assert_eq!(placement::radius(rice), 0.5);
    assert!(placement::is_round(rice));

    // Fig Sapling 13725 -> Fig Tree 380 (group 2, sim_radius 1000).
    let fig = doodads.template(380).expect("fig template 380");
    assert!(items.item_ids_from_doodad(380).contains(&13725));
    assert_eq!(fig.group_id, 2);
    assert_eq!(placement::radius(fig), 1.0);
    assert!(placement::is_round(fig));

    // Brown seaweed 4867 -> 411 (group 40, sim_radius 1000): the marine rule.
    let marine = doodads.template(411).expect("seaweed template 411");
    assert!(items.item_ids_from_doodad(411).contains(&4867));
    assert_eq!(marine.group_id, placement::MARINE_CROPS);
    assert_eq!(placement::radius(marine), 1.0);

    // Non-round plantables keep their real radius but are not overlap-tested (the client's
    // oriented boxes / physics colliders are not decoded): groups 72 (3 m) and 69 (2 m).
    for (id, radius) in [
        (5287, 3.0),
        (5288, 3.0),
        (5289, 3.0),
        (5290, 3.0),
        (6591, 3.0),
        (3596, 2.0),
    ] {
        let template = doodads.template(id).expect("non-round plantable");
        assert!(!placement::is_round(template), "template {id}");
        assert!(
            (placement::radius(template) - radius).abs() < 1e-6,
            "template {id}: radius {} != {radius}",
            placement::radius(template)
        );
        let same = placement::Shape::of(template, (0.0, 0.0, 110.0));
        assert!(
            !placement::doodad_overlaps(same, same),
            "template {id} must not be refused against a same-spot doodad"
        );
        assert!(
            !placement::unit_overlaps(same, (0.0, 0.0, 110.0)),
            "template {id} must not be refused against a unit"
        );
    }

    // A rice and a fig at 1.2 m overlap (0.5 + 1.0); at 1.6 m they do not.
    let rice_shape = placement::Shape::of(rice, (0.0, 0.0, 110.0));
    let fig_shape = placement::Shape::of(fig, (1.2, 0.0, 110.0));
    assert!(placement::doodad_overlaps(rice_shape, fig_shape));
    let fig_shape = placement::Shape::of(fig, (1.6, 0.0, 110.0));
    assert!(!placement::doodad_overlaps(rice_shape, fig_shape));
    // The 30 m rule is inclusive (`> 900` refuses) and uses z: run the production check.
    let player = (0.0, 0.0, 110.0);
    assert!(placement::in_place_range(player, (29.0, 0.0, 110.0)));
    assert!(placement::in_place_range(player, (30.0, 0.0, 110.0)));
    assert!(!placement::in_place_range(player, (30.01, 0.0, 110.0)));
    assert!(placement::in_place_range(player, (0.0, 0.0, 139.0)));
    assert!(placement::in_place_range(player, (0.0, 0.0, 140.0)));
    assert!(!placement::in_place_range(player, (0.0, 0.0, 140.01)));
}

/// The real rice seed and fig sapling driven through `CSCreateDoodadPacket`'s handler: the real
/// templates (2246 / 380), their real radii and the 1.2 refusals. Same gate as above.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_seed_and_sapling_placements_drive_the_handler() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut data = GameData::empty();
    data.items = crate::data::ItemData::load(&conn).unwrap();
    data.systems.npcs.doodad =
        crate::systems::npcs::doodad::data::DoodadData::load_from(&conn).unwrap();
    let server = test_server_with(data);
    crate::systems::economy::bridges::install(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player_obj = server.ids.object.get_next_id().unwrap();
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Planter", 100.0, 100.0),
        player_obj,
    );
    let containers = player.data().inventory.containers.clone();
    for (_, container) in containers {
        server.items.add_container(container);
    }
    let mut client = client;
    client.drain();

    // Rice Seed 15646 -> 2246 at 2 m: accepted.
    let rice = give(&server, &player, 15646, 2);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        2246,
        rice,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "rice seed placement: {codes:?}");

    // Fig Sapling 13725 -> 380 at 1.2 m from the rice: 1.2 < 0.5 + 1.0, refused.
    let fig = give(&server, &player, 13725, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        380,
        fig,
        (103.2, 100.0, DRY_Z),
    );
    assert_eq!(
        codes,
        vec![error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD]
    );

    // At 1.6 m it is outside the two spheres.
    let fig = give(&server, &player, 13725, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        380,
        fig,
        (103.6, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "fig sapling at 1.6 m: {codes:?}");

    // A group 72 plantable (item 4868 -> 5287, 3 m radius, non-round) on top of the rice: the
    // client's box test is not decoded, so the server must not refuse it.
    let group72 = give(&server, &player, 4868, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        5287,
        group72,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "group 72 on the rice: {codes:?}");

    // Brown seaweed 4867 -> 411 is a marine crop: water is required.
    let marine = give(&server, &player, 4867, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        411,
        marine,
        (101.0, 100.0, 90.0),
    );
    assert!(codes.is_empty(), "seaweed in water: {codes:?}");
    let rice = give(&server, &player, 15646, 1);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        2246,
        rice,
        (100.5, 100.0, 90.0),
    );
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);
}

// ---------------------------------------------------------------------------------------------
// WP-D1a: the Territory Statue blueprint's dominion gate (`FUN_393a8fa0` step 7, error 749).
// ---------------------------------------------------------------------------------------------

struct StatueFixture {
    server: GameServer,
    player: PlayerRef,
    client: TestClient,
}

/// Zone [`TEST_ZONE`] in `group` (no zone record at all when `None`), a live tower house at
/// (100, 100) with `radius_dominion` 100, the caster in expedition 1000 and, when
/// `owner_expedition != 0`, a dominion claim by that expedition.
fn statue_fixture(group: Option<u32>, owner_expedition: u32) -> StatueFixture {
    let mut data = GameData::empty();
    if let Some(group) = group {
        data.zones.zones.insert(
            TEST_ZONE,
            Zone {
                zone_key: TEST_ZONE,
                group_id: group,
                ..Default::default()
            },
        );
    }
    // Item 29800 "Blueprint: Territory Statue" spawns doodad 7229 (group 80, sim_radius 5000).
    data.items
        .templates
        .insert(29800, ItemTemplate::plain(29800, 1));
    data.items
        .templates
        .insert(1000, ItemTemplate::plain(1000, 20));
    data.items.item_doodads.insert(7229, vec![29800]);
    data.items.item_doodads.insert(4000, vec![1000]);
    data.systems.npcs.doodad.templates.insert(
        7229,
        DoodadTemplate {
            id: 7229,
            group_id: 80,
            sim_radius: 5000,
            ..Default::default()
        },
    );
    data.systems.npcs.doodad.templates.insert(
        4000,
        DoodadTemplate {
            id: 4000,
            ..Default::default()
        },
    );
    data.systems.housing.add_template(HousingTemplate {
        id: 139,
        guard_tower_setting_id: 6,
        hp: 1000,
        ..Default::default()
    });
    data.systems.housing.castles.guard_tower_settings.insert(
        6,
        GuardTowerSetting {
            id: 6,
            radius_dominion: 100,
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
        server.ids.object.get_next_id().unwrap(),
    );
    player.data().labor_power = 20;
    let containers = player.data().inventory.containers.clone();
    for (_, container) in containers {
        server.items.add_container(container);
    }
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 1000,
            owner_id: 1,
            ..Default::default()
        },
        members: vec![member_from_character(&server, 1000, &player, true)],
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    if let Some(group) = group {
        let mut house = House::new(500, 139);
        house.current_step = -1;
        house.unit.hp = 1000;
        house.unit.max_hp = 1000;
        house.unit.transform.zone_id = TEST_ZONE;
        house.unit.transform.set_xyz(100.0, 100.0, DRY_Z);
        let tower = Entity::new(server.ids.object.get_next_id().unwrap(), house);
        server.systems.housing.manager.insert(&tower);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&tower, &server.view_ctx());
        if owner_expedition != 0 {
            let (_, identity) =
                crate::systems::housing::dominion::runtime::tower_identity(&server, &tower)
                    .unwrap();
            server
                .systems
                .housing
                .dominions
                .declare_unowned_with(group, identity, owner_expedition, 1_000_000, || true)
                .unwrap();
        }
    }
    StatueFixture {
        server,
        player,
        client,
    }
}

fn statue_count(server: &GameServer) -> usize {
    server
        .world
        .world(0)
        .unwrap()
        .all_doodads()
        .into_iter()
        .filter(|doodad| doodad.data().template_id == 7229)
        .count()
}

#[tokio::test]
async fn territory_statue_places_inside_the_casters_own_dominion() {
    let mut f = statue_fixture(Some(25), 1000);
    let item = give(&f.server, &f.player, 29800, 1);
    let codes = send_placement(
        &mut f.client,
        &f.server,
        &f.player,
        7229,
        item,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "own dominion inside radius: {codes:?}");
    assert_eq!(statue_count(&f.server), 1);
    assert!(
        Inv::of(&f.player).item_by_id(item).is_none(),
        "a successful placement consumes the blueprint"
    );
}

#[tokio::test]
async fn territory_statue_refuses_outside_the_dominion_radius_and_keeps_the_item() {
    let mut f = statue_fixture(Some(25), 1000);
    // Still within the 30 m placement range, but 100 m from the tower (radius² is exclusive).
    f.player.data().unit.transform.set_xyz(200.0, 100.0, DRY_Z);
    let item = give(&f.server, &f.player, 29800, 1);
    let codes = send_placement(
        &mut f.client,
        &f.server,
        &f.player,
        7229,
        item,
        (200.0, 100.0, DRY_Z),
    );
    assert_eq!(codes, vec![error_message::LOCATE_INVALID_DOMINION_AREA]);
    assert_eq!(statue_count(&f.server), 0);
    assert_eq!(
        Inv::of(&f.player).item_by_id(item).map(|i| i.count),
        Some(1),
        "a refused placement keeps the blueprint"
    );
}

#[tokio::test]
async fn territory_statue_refuses_another_guilds_dominion() {
    let mut f = statue_fixture(Some(25), 2000);
    let item = give(&f.server, &f.player, 29800, 1);
    let codes = send_placement(
        &mut f.client,
        &f.server,
        &f.player,
        7229,
        item,
        (102.0, 100.0, DRY_Z),
    );
    assert_eq!(codes, vec![error_message::LOCATE_INVALID_DOMINION_AREA]);
    assert_eq!(statue_count(&f.server), 0);
    assert_eq!(
        Inv::of(&f.player).item_by_id(item).map(|i| i.count),
        Some(1)
    );
}

#[tokio::test]
async fn territory_statue_refuses_a_spot_without_an_owned_zone_group() {
    // No zone record: `FUN_397a6c60` answers no group.
    let mut f = statue_fixture(None, 0);
    let item = give(&f.server, &f.player, 29800, 1);
    let codes = send_placement(
        &mut f.client,
        &f.server,
        &f.player,
        7229,
        item,
        (102.0, 100.0, DRY_Z),
    );
    assert_eq!(codes, vec![error_message::LOCATE_INVALID_DOMINION_AREA]);
    assert_eq!(statue_count(&f.server), 0);

    // Zone group 25 with a tower but no dominion record (unowned): `FUN_3983a730` fails.
    let mut f = statue_fixture(Some(25), 0);
    let item = give(&f.server, &f.player, 29800, 1);
    let codes = send_placement(
        &mut f.client,
        &f.server,
        &f.player,
        7229,
        item,
        (102.0, 100.0, DRY_Z),
    );
    assert_eq!(codes, vec![error_message::LOCATE_INVALID_DOMINION_AREA]);
    assert_eq!(statue_count(&f.server), 0);
    assert_eq!(
        Inv::of(&f.player).item_by_id(item).map(|i| i.count),
        Some(1)
    );
}

#[tokio::test]
async fn other_items_place_inside_a_foreign_dominion_unchanged() {
    let mut f = statue_fixture(Some(25), 2000);
    let item = give(&f.server, &f.player, 1000, 1);
    let codes = send_placement(
        &mut f.client,
        &f.server,
        &f.player,
        4000,
        item,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "the gate is item 29800 only: {codes:?}");
    assert_eq!(
        f.server
            .world
            .world(0)
            .unwrap()
            .all_doodads()
            .into_iter()
            .filter(|doodad| doodad.data().template_id == 4000)
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------------------------
// D1d / W1: the place-skin terrain material rule (`Housing.PlacementStrictSurface`).
// ---------------------------------------------------------------------------------------------

/// Writes a `main_world` `surface.xml` with three materials and its detail `.mtl` files:
/// id 10 `mat_grass` (kind 2), id 20 `mat_rock` (no kind), id 30 `mat_sand_dry` (permissive 4).
fn surface_files() -> PathBuf {
    let root = temp_path("place_surface");
    let _ = std::fs::remove_dir_all(&root);
    let world = root.join("game/worlds/main_world");
    let materials = root.join("game/materials");
    std::fs::create_dir_all(&world).unwrap();
    std::fs::create_dir_all(&materials).unwrap();
    std::fs::write(
        world.join("surface.xml"),
        r#"<Surface><SurfaceTypes>
 <SurfaceType Name="grass" DetailMaterial="materials/grass" Id="10"/>
 <SurfaceType Name="rock" DetailMaterial="materials/rock" Id="20"/>
 <SurfaceType Name="sand" DetailMaterial="materials/sand" Id="30"/>
</SurfaceTypes></Surface>"#,
    )
    .unwrap();
    for (name, surface_type) in [
        ("grass", "mat_grass"),
        ("rock", "mat_rock"),
        ("sand", "mat_sand_dry"),
    ] {
        std::fs::write(
            materials.join(format!("{name}.mtl")),
            format!("<Material Shader=\"Terrain.Layer\" SurfaceType=\"{surface_type}\"/>"),
        )
        .unwrap();
    }
    root
}

/// Installs a 2 x 2 cell surface map on the synthetic world. The lower-left 2 m sample's local id
/// depends on the world Y band: 0..60 -> 0 (grass), 60..80 -> 1 (rock), 80..100 -> 2 (sand),
/// 100..120 -> 31 (hole), 120..140 -> 3 (table id not in `surface.xml`), else 0.
fn install_surface_heightmap(server: &GameServer, root: Option<&Path>) {
    let files = root.map(|root| {
        Arc::new(ClientFileManager::from_sources(
            &[root.to_string_lossy().into_owned()],
            root,
        ))
    });
    let files = files.unwrap_or_else(|| Arc::new(ClientFileManager::new()));
    let template = server.world.world(0).unwrap().template.clone();
    let map = WorldHeightmap::with_size_surface(
        template.id,
        &template.name,
        template.cell_x,
        template.cell_y,
        template.height_max_coefficient,
        true,
    );
    let points = (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize;
    let mut ids = vec![0u8; points];
    for x in 0..CELL_HMAP_RESOLUTION {
        for y in 0..CELL_HMAP_RESOLUTION {
            let local = match y {
                60..=79 => 1,
                80..=99 => 2,
                100..=119 => 31,
                120..=139 => 3,
                _ => 0,
            };
            ids[(x * CELL_HMAP_RESOLUTION + y) as usize] = local;
        }
    }
    let mut table = [0u32; 32];
    table[0] = 10;
    table[1] = 20;
    table[2] = 30;
    table[3] = 999;
    assert!(map.insert_cell_with_surface(0, 0, vec![0u16; points], table, ids));
    let worlds = HeightmapWorlds::from_parts(
        files,
        vec![map],
        std::collections::HashMap::new(),
        HeightmapConfig {
            heightmaps_enable: true,
            placement_strict_surface: true,
            ..Default::default()
        },
    );
    server.systems.npcs.heightmaps.install(Arc::new(worlds));
}

struct SurfaceFixture {
    server: GameServer,
    player: PlayerRef,
    client: TestClient,
    root: PathBuf,
}

impl Drop for SurfaceFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Templates: 4010 grass-only (kind 2), 4011 sand/mud-only (kind 4), 4012 no place skins,
/// 4013 group 37, 1218/1219 the client skip ids.
fn surface_fixture(with_surface_xml: bool) -> SurfaceFixture {
    let mut data = GameData::empty();
    for id in [1010, 1011, 1012, 1013, 1018, 1019] {
        data.items.templates.insert(id, ItemTemplate::plain(id, 20));
    }
    let templates = [
        (4010, 1010, 24, vec![2]),
        (4011, 1011, 24, vec![4]),
        (4012, 1012, 24, Vec::new()),
        (4013, 1013, 37, vec![2]),
        (1218, 1018, 24, Vec::new()),
        (1219, 1019, 24, Vec::new()),
    ];
    for (id, item, group_id, skins) in templates {
        data.items.item_doodads.insert(id, vec![item]);
        data.systems.npcs.doodad.templates.insert(
            id,
            DoodadTemplate {
                id,
                group_id,
                ..Default::default()
            },
        );
        if !skins.is_empty() {
            data.systems.npcs.doodad.place_skins.insert(id, skins);
        }
    }
    data.systems.housing.add_template(HousingTemplate {
        id: 139,
        garden_radius: 20.0,
        hp: 1000,
        ..Default::default()
    });
    let server = test_server_with(data);
    crate::systems::economy::bridges::install(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Planter", 100.0, 100.0),
        server.ids.object.get_next_id().unwrap(),
    );
    player.data().labor_power = 20;
    let containers = player.data().inventory.containers.clone();
    for (_, container) in containers {
        server.items.add_container(container);
    }
    let root = if with_surface_xml {
        surface_files()
    } else {
        let root = temp_path("place_no_surface");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    };
    install_surface_heightmap(&server, Some(&root));
    SurfaceFixture {
        server,
        player,
        client,
        root,
    }
}

fn place_at(f: &mut SurfaceFixture, doodad: u32, item: u64, x: f32, y: f32) -> Vec<i16> {
    f.player.data().unit.transform.set_xyz(x, y, DRY_Z);
    send_placement(
        &mut f.client,
        &f.server,
        &f.player,
        doodad,
        item,
        (x, y, DRY_Z),
    )
}

fn doodad_count(server: &GameServer, template_id: u32) -> usize {
    server
        .world
        .world(0)
        .unwrap()
        .all_doodads()
        .into_iter()
        .filter(|doodad| doodad.data().template_id == template_id)
        .count()
}

#[tokio::test]
async fn strict_surface_rule_refuses_the_wrong_material_and_keeps_the_item() {
    let mut f = surface_fixture(true);
    f.server
        .config
        .update(|config| config.housing.placement_strict_surface = true);

    let item = give(&f.server, &f.player, 1010, 1);
    let codes = place_at(&mut f, 4010, item, 100.0, 100.0);
    assert!(codes.is_empty(), "grass doodad on grass: {codes:?}");
    assert_eq!(doodad_count(&f.server, 4010), 1);

    let item = give(&f.server, &f.player, 1010, 1);
    let codes = place_at(&mut f, 4010, item, 100.0, 140.0);
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);
    assert_eq!(doodad_count(&f.server, 4010), 1);
    assert_eq!(
        Inv::of(&f.player).item_by_id(item).map(|item| item.count),
        Some(1),
        "a refused placement keeps the seed"
    );

    let item = give(&f.server, &f.player, 1011, 1);
    let codes = place_at(&mut f, 4011, item, 100.0, 180.0);
    assert!(
        codes.is_empty(),
        "permissive: mat_sand_dry is kind 4: {codes:?}"
    );
    let item = give(&f.server, &f.player, 1011, 1);
    let codes = place_at(&mut f, 4011, item, 100.0, 100.0);
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);

    // A material whose surface type is not one of the four names is refused for every template.
    let item = give(&f.server, &f.player, 1011, 1);
    let codes = place_at(&mut f, 4011, item, 100.0, 140.0);
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);

    // A template without place-skin rows is refused on open terrain.
    let item = give(&f.server, &f.player, 1012, 1);
    let codes = place_at(&mut f, 4012, item, 100.0, 100.0);
    assert_eq!(codes, vec![error_message::DOODAD_SIM_AREA]);

    // Unknown data (hole, surface id not in surface.xml) is permissive.
    let item = give(&f.server, &f.player, 1012, 1);
    let codes = place_at(&mut f, 4012, item, 100.0, 220.0);
    assert!(codes.is_empty(), "a hole has no material: {codes:?}");
    let item = give(&f.server, &f.player, 1012, 1);
    let codes = place_at(&mut f, 4012, item, 100.0, 260.0);
    assert!(
        codes.is_empty(),
        "an id not in surface.xml is unknown: {codes:?}"
    );
}

#[tokio::test]
async fn strict_surface_rule_is_off_by_default() {
    let mut f = surface_fixture(true);
    assert!(!f.server.config.housing.placement_strict_surface);
    let item = give(&f.server, &f.player, 1010, 1);
    let codes = place_at(&mut f, 4010, item, 100.0, 140.0);
    assert!(
        codes.is_empty(),
        "without the flag a grass doodad may stand on rock: {codes:?}"
    );
    assert_eq!(doodad_count(&f.server, 4010), 1);
}

#[tokio::test]
async fn strict_surface_rule_honours_the_house_and_group_skips() {
    let mut f = surface_fixture(true);
    f.server
        .config
        .update(|config| config.housing.placement_strict_surface = true);

    for (doodad, item_template) in [(1218, 1018), (1219, 1019)] {
        let item = give(&f.server, &f.player, item_template, 1);
        let codes = place_at(&mut f, doodad, item, 100.0, 140.0);
        assert!(codes.is_empty(), "template {doodad} is skipped: {codes:?}");
    }
    let item = give(&f.server, &f.player, 1013, 1);
    let codes = place_at(&mut f, 4013, item, 100.0, 140.0);
    assert!(codes.is_empty(), "group 37 is skipped: {codes:?}");

    // A house box the player may build on skips the rule too (no place-skin rows on rock).
    let mut house = House::new(500, 139);
    house.current_step = 0;
    house.unit.transform.zone_id = TEST_ZONE;
    house.unit.transform.set_xyz(100.0, 140.0, DRY_Z);
    let tower = Entity::new(f.server.ids.object.get_next_id().unwrap(), house);
    f.server.systems.housing.manager.insert(&tower);
    f.server
        .world
        .world(0)
        .unwrap()
        .spawn(&tower, &f.server.view_ctx());
    let item = give(&f.server, &f.player, 1012, 1);
    let codes = place_at(&mut f, 4012, item, 100.0, 140.0);
    assert!(
        codes.is_empty(),
        "a permitted house box skips the rule: {codes:?}"
    );
    assert_eq!(doodad_count(&f.server, 4012), 1);
}

#[tokio::test]
async fn strict_surface_rule_does_nothing_without_surface_data() {
    let mut f = surface_fixture(false);
    f.server
        .config
        .update(|config| config.housing.placement_strict_surface = true);
    let item = give(&f.server, &f.player, 1010, 1);
    let codes = place_at(&mut f, 4010, item, 100.0, 140.0);
    assert!(codes.is_empty(), "no surface.xml -> no rule: {codes:?}");
    assert_eq!(doodad_count(&f.server, 4010), 1);
}

/// Planting is routine (the bots plant hundreds of crops a day): its success path must not log
/// at `WARN`, nor must a doodad the server places itself (a blood stain on a death).
#[tokio::test]
async fn successful_planting_logs_nothing_at_warn() {
    use tracing::Level;
    let (server, player, id, mut client) = fixture_with_client();
    let (codes, events) = crate::test_support::capture_events(|| {
        send_placement(
            &mut client,
            &server,
            &player,
            4000,
            id,
            (101.0, 100.0, DRY_Z),
        )
    });
    assert!(codes.is_empty(), "{codes:?}");
    assert_eq!(server.world.world(0).unwrap().all_doodads().len(), 1);
    let loud: Vec<_> = events.iter().filter(|(l, _)| *l <= Level::WARN).collect();
    assert!(loud.is_empty(), "{loud:?}");
    // Still traceable at debug level.
    for what in ["CreateDoodad, Id: 4000", "is placing a doodad 4000"] {
        assert!(
            events
                .iter()
                .any(|(l, m)| *l == Level::DEBUG && m.contains(what)),
            "{what}: {events:?}"
        );
    }

    // `Character.DoDie` places the blood stain (878 in the data) for the dead character.
    let (stain, events) = crate::test_support::capture_events(|| {
        create_player_doodad_server_spawned(
            &server,
            &player,
            4000,
            (100.0, 100.0, DRY_Z),
            0.0,
            1.0,
            0,
            0,
            0,
            0,
            true,
        )
    });
    assert!(stain.is_some());
    let loud: Vec<_> = events.iter().filter(|(l, _)| *l <= Level::WARN).collect();
    assert!(loud.is_empty(), "{loud:?}");
}

/// A refused planting is a real problem (the client checks the rules itself first): it stays
/// at `WARN`.
#[tokio::test]
async fn refused_planting_is_still_logged_at_warn() {
    use tracing::Level;
    let (server, player, id, mut client) = fixture_with_client();
    let (codes, events) = crate::test_support::capture_events(|| {
        // The wrong doodad for this seed.
        send_placement(
            &mut client,
            &server,
            &player,
            4005,
            id,
            (101.0, 100.0, DRY_Z),
        )
    });
    assert!(!codes.is_empty());
    assert!(
        events
            .iter()
            .any(|(l, m)| *l == Level::WARN && m.contains("CreateDoodad")),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// `FUN_393a7720` step 2: the footprint terrain spread (doodad seed builder, error 279).
// ---------------------------------------------------------------------------------------------

const BOX_MODEL: &str = "cgf://objects/test/box.cgf";

/// A planting fixture whose doodad 4020 has a START func group; `model` maps that group to a
/// collision model (`None` leaves the template without one: the no-bounds case). The collision
/// service answers with a plane `z = 110 + b·x`, so the 2 m box's footprint spans `2·b` m.
fn ground_fixture(
    group_id: u32,
    model: Option<&str>,
    b: f32,
) -> (GameServer, PlayerRef, u64, TestClient) {
    let mut data = GameData::empty();
    data.items
        .templates
        .insert(1020, ItemTemplate::plain(1020, 20));
    data.items.item_doodads.insert(4020, vec![1020]);
    let mut template = DoodadTemplate {
        id: 4020,
        group_id,
        ..Default::default()
    };
    if let Some(model) = model {
        template.func_groups.push(DoodadFuncGroups {
            id: 5020,
            almighty: 4020,
            group_kind_id: doodad_func_group_kind::START,
            ..Default::default()
        });
        data.systems
            .ai
            .collision
            .func_group_models
            .insert(5020, model.to_string());
    }
    data.systems.npcs.doodad.templates.insert(4020, template);
    let server = test_server_with(data);
    crate::systems::economy::bridges::install(&server);
    let mut files = HashMap::new();
    files.insert("game/objects/test/box.cgf".to_string(), box_cgf());
    server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(files))
        .unwrap();
    server
        .systems
        .ai
        .collision
        .test_terrain
        .set(Arc::new(Plane {
            a: DRY_Z,
            b,
            c: 0.0,
            n: 16384,
        }))
        .unwrap_or_else(|_| panic!("terrain"));
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Planter", 100.0, 100.0),
        server.ids.object.get_next_id().unwrap(),
    );
    player.data().labor_power = 20;
    for (_, container) in player.data().inventory.containers.clone() {
        server.items.add_container(container);
    }
    let item = server
        .items
        .create(1020, 2, 0, 1, &server.game_data.items, &server.ids.item, 0)
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
async fn ground_rule_passes_a_model_on_flat_terrain() {
    let (server, player, id, mut client) = ground_fixture(24, Some(BOX_MODEL), 0.0);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4020,
        id,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "flat ground: {codes:?}");
    assert_eq!(server.world.world(0).unwrap().all_doodads().len(), 1);
}

#[tokio::test]
async fn ground_rule_refuses_a_corner_seven_metres_lower_with_279() {
    // `z = 110 + 3.5·x` across the box's 2 m footprint: the corners differ by exactly 7 m.
    let (server, player, id, mut client) = ground_fixture(24, Some(BOX_MODEL), 3.5);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4020,
        id,
        (102.0, 100.0, DRY_Z),
    );
    assert_eq!(codes, vec![error_message::DOODAD_SIM_LOW_TERRAIN]);
    assert_eq!(
        Inv::of(&player).item_by_id(id).map(|item| item.count),
        Some(2),
        "a refused placement keeps the seed"
    );
    assert!(server.world.world(0).unwrap().all_doodads().is_empty());

    // Exactly 6 m is allowed (`max - min > 6.0` refuses, strictly).
    let (server, player, id, mut client) = ground_fixture(24, Some(BOX_MODEL), 3.0);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4020,
        id,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "exactly 6 m is allowed: {codes:?}");
}

#[tokio::test]
async fn ground_rule_is_skipped_for_the_client_groups() {
    // `FUN_393a7720`: groups 2, 3, 5, 12, 37, 40 and 80 never run the footprint spread.
    // Group 40 (marine crops) still needs water, so its spot is below the 100 m ocean level.
    for group in placement::FOOTPRINT_SKIP_GROUPS {
        let z = if group == placement::MARINE_CROPS {
            90.0
        } else {
            DRY_Z
        };
        let (server, player, id, mut client) = ground_fixture(group, Some(BOX_MODEL), 3.5);
        let codes = send_placement(&mut client, &server, &player, 4020, id, (102.0, 100.0, z));
        assert!(codes.is_empty(), "group {group} is skipped: {codes:?}");
        assert_eq!(server.world.world(0).unwrap().all_doodads().len(), 1);
    }
}

#[tokio::test]
async fn ground_rule_is_skipped_without_model_bounds() {
    // The render OBB stands in for the model bounds; a model without physics geometry skips the
    // check instead of refusing on a slope the server cannot measure.
    let (server, player, id, mut client) = ground_fixture(24, None, 3.5);
    let codes = send_placement(
        &mut client,
        &server,
        &player,
        4020,
        id,
        (102.0, 100.0, DRY_Z),
    );
    assert!(codes.is_empty(), "no bounds: {codes:?}");
    assert_eq!(server.world.world(0).unwrap().all_doodads().len(), 1);
}
