use super::*;
use crate::data::{items::ItemTemplate, GameData};
use crate::systems::economy::inventory::Inv;
use crate::systems::npcs::doodad::templates::DoodadTemplate;
use crate::test_support::*;

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
