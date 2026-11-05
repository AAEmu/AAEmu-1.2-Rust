//! Port of AAEmu.UnitTests/Game/Core/Managers/ShipyardManagerTests.cs
//!
//! `ShipyardManager.Initialize()` schedules exactly one `ShipyardTickTask` (start delay one
//! minute, repeated every minute). The Rust manager runs it as one Tokio ticker started by
//! `systems::vehicles::System::start` (`spawn_ticker(TICK_INTERVAL, TICK_START_DELAY,
//! shipyard_tick)`); the schedule is checked on the constants and the ticker with a shortened
//! delay, observed through its effect (`UpdateShipyardInfo` gives a fresh shipyard the tax
//! protection buff).

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::data::GameData;
use crate::models::shipyard::ShipyardData;
use crate::systems::skills::data::BuffTemplate;
use crate::systems::vehicles::data::{ShipyardStep, ShipyardTemplate};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

#[tokio::test]
async fn initialize_schedules_tick() {
    let mut d = GameData::empty();
    d.systems.skills.add_buff(BuffTemplate {
        id: buff::TAX_PROTECTION,
        ..Default::default()
    });
    d.systems.vehicles.shipyard_templates.insert(
        3,
        ShipyardTemplate {
            id: 3,
            tax_duration: 3_600_000,
            ..ShipyardTemplate::default()
        },
    );
    let server = Arc::new(test_server_with(d));
    server.set_self();
    let obj_id = 0x00F0_0100;
    let shipyard = Entity::new(
        obj_id,
        Shipyard::new(ShipyardData {
            id: 1,
            template_id: 3,
            spawned: chrono::Utc::now().timestamp(),
            obj_id,
            ..ShipyardData::default()
        }),
    );
    server
        .systems
        .vehicles
        .shipyards
        .shipyards
        .lock()
        .unwrap()
        .push((1, shipyard.clone()));
    let has_protection = || shipyard.data().unit.buffs.check_buff(buff::TAX_PROTECTION);
    assert_eq!(TICK_START_DELAY, Duration::from_secs(60));
    assert_eq!(TICK_INTERVAL, Duration::from_secs(60));

    let step = Duration::from_millis(400);
    super::super::spawn_ticker(&server, step, step, shipyard_tick);
    tokio::time::sleep(step / 4).await;
    assert!(!has_protection(), "no shipyard tick before the start delay");

    // Poll instead of one fixed sleep: a loaded test machine can delay the ticker task.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !has_protection() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(step / 4).await;
    }
    assert!(
        has_protection(),
        "the shipyard tick ran after the start delay"
    );
}

#[tokio::test]
async fn shipyard_radius_counts_in_skill_distance() {
    use crate::models::doodad::Doodad;
    use crate::models::transform::PositionAndRotation;
    use crate::systems::skills::geometry::{self, PosUnit, Target};

    let mut d = GameData::empty();
    d.systems.vehicles.shipyard_templates.insert(
        3,
        ShipyardTemplate {
            id: 3,
            build_radius: 30,
            ..ShipyardTemplate::default()
        },
    );
    let server = test_server_with(d);
    let mut sy = Shipyard::new(ShipyardData {
        id: 1,
        template_id: 3,
        obj_id: 0x00F0_0100,
        ..ShipyardData::default()
    });
    sy.unit.transform.x = 100.0;
    sy.unit.transform.y = 100.0;
    let shipyard = WorldObject::from(&Entity::new(0x00F0_0100, sy));
    assert_eq!(shipyard_radius(&server, &shipyard), Some(30.0));

    let doodad = WorldObject::from(&Entity::new(0x00F0_0101, Doodad::new(1)));
    assert_eq!(shipyard_radius(&server, &doodad), None);

    let caster_at = |y: f32| {
        Target::Pos(PosUnit {
            instance_id: 0,
            zone_id: 0,
            pos: PositionAndRotation {
                x: 100.0,
                y,
                ..PositionAndRotation::default()
            },
        })
    };
    let target = Target::Obj(shipyard);
    // 50 m from the center: 20 m from the footprint edge.
    assert_eq!(
        geometry::distance_to(&server, &caster_at(150.0), &target, true),
        20.0
    );
    // Inside the footprint the distance clamps at 0.
    assert_eq!(
        geometry::distance_to(&server, &caster_at(110.0), &target, true),
        0.0
    );
}

#[tokio::test]
#[ignore = "requires AAEMU_TEST_MYSQL; uses a connection-local temporary table"]
async fn shipyard_rows_round_trip_through_mysql() {
    use super::super::shipyards_db::{read_rows, write_row, ShipyardRow};
    let url = std::env::var("AAEMU_TEST_MYSQL").expect("private MySQL URL");
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&url)
        .await
        .unwrap();
    let ddl = include_str!("../../../../../../SQL/updates/2026-10-20_aaemu_game_shipyards.sql");
    let ddl = ddl[ddl.find("CREATE TABLE").unwrap()..].replacen(
        "CREATE TABLE IF NOT EXISTS",
        "CREATE TEMPORARY TABLE",
        1,
    );
    sqlx::query(&ddl).execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let row = ShipyardRow {
        id: 4,
        template_id: 3,
        owner_id: 77,
        owner_name: "Owner".into(),
        faction: 101,
        instance_id: 0,
        zone_id: 12,
        x: 120.5,
        y: 130.25,
        z: 100.0,
        rot_x: 0.25,
        rot_y: -0.5,
        z_rot: 0.5,
        money_amount: 0,
        actions: 3,
        step: 1,
        current_step: 1,
        num_action: 1,
        hp: 950,
        data_hp: 1000,
        spawned: 1_700_000_000,
    };
    write_row(&mut conn, &row).await.unwrap();
    assert_eq!(read_rows(&mut conn).await.unwrap(), vec![row]);
}

// ---------------------------------------------------------------------------------------------
// WP3: shipyard design item (`item_shipyards`) and `shipyard_rewards`.
// ---------------------------------------------------------------------------------------------

const SHIPYARD_10: u32 = 10;
const SHIPYARD_11: u32 = 11;
/// The design item object ids the tests put in the bag (not item template ids).
const DESIGN_10: u64 = 0x0000_0000_0000_9101;
const DESIGN_11: u64 = 0x0000_0000_0000_9102;

/// The two shipyards whose `origin_item_id` is 0 in the 1.2 data: only `item_shipyards`
/// (18124 -> 10, 18149 -> 11) makes them buildable.
fn shipyard_test_data() -> GameData {
    use crate::data::items::ItemTemplate;
    use crate::systems::npcs::doodad::templates::DoodadTemplate;
    use crate::systems::vehicles::data::ShipyardReward;

    let mut d = GameData::empty();
    for (id, design_item, main_model_id) in [(SHIPYARD_10, 18124, 966), (SHIPYARD_11, 18149, 680)] {
        d.systems.vehicles.shipyard_templates.insert(
            id,
            ShipyardTemplate {
                id,
                name: format!("Shipyard {id}"),
                main_model_id,
                item_id: 15816,
                origin_item_id: 0,
                taxation_id: 1,
                steps: vec![(
                    0,
                    ShipyardStep {
                        id,
                        shipyard_id: id,
                        step: 0,
                        model_id: main_model_id,
                        skill_id: 0,
                        num_actions: 1,
                        max_hp: 10,
                    },
                )],
                ..ShipyardTemplate::default()
            },
        );
        d.systems.vehicles.item_shipyards.insert(design_item, id);
        d.items
            .templates
            .insert(design_item, ItemTemplate::plain(design_item, 1));
    }
    d.systems.vehicles.taxations.insert(1, 0);
    // The 1.2 debris row: 600-700 s lifetime, no func group in the fixture (the phase machine
    // is not what this test checks).
    d.systems.npcs.doodad.templates.insert(
        1330,
        DoodadTemplate {
            id: 1330,
            min_time: 600_000,
            max_time: 700_000,
            ..DoodadTemplate::default()
        },
    );
    d.systems.vehicles.shipyard_rewards.insert(
        SHIPYARD_10,
        vec![ShipyardReward {
            id: 1,
            shipyard_id: SHIPYARD_10,
            doodad_id: 1330,
            on_water: false,
            radius: 12.0,
            count: 4,
        }],
    );
    d
}

fn shipyard_test_server() -> Arc<GameServer> {
    let s = Arc::new(test_server_with(shipyard_test_data()));
    s.set_self();
    // The real item hooks (`UseSkillId`, the bag consume), not the no-op test defaults.
    s.systems
        .skills
        .ext
        .items
        .install(Arc::new(crate::systems::economy::bridges::EconomyForSkills));
    s
}

fn give_design_item(server: &GameServer, player: &PlayerRef, item_id: u64, template_id: u32) {
    let template = server
        .game_data
        .items
        .template(template_id)
        .unwrap()
        .clone();
    let mut item =
        crate::models::item::Item::create(item_id, &template, 1, 0, 1, &server.game_data.items, 0);
    item.owner_id = u64::from(player.data().id);
    item.slot_type = crate::models::container::slot_type::INVENTORY;
    let bag = player
        .data()
        .inventory
        .get(crate::models::container::slot_type::INVENTORY)
        .cloned()
        .unwrap();
    bag.lock().unwrap().items.push(item);
}

fn design_item_still_owned(server: &GameServer, player: &PlayerRef, item_id: u64) -> bool {
    super::super::mates::owned_item(server, player, item_id).is_some()
}

fn shipyard_packet(id: u32, design_item: u64) -> super::super::packets::CSCreateShipyardPacket {
    super::super::packets::CSCreateShipyardPacket {
        id,
        x: 120.0,
        y: 120.0,
        z: 100.0,
        z_rot: 0.5,
        design_item,
        aabb: [0.0; 6],
        auto_use_aa_point: false,
    }
}

#[tokio::test]
async fn shipyards_10_and_11_build_from_their_item_and_consume_it_exactly_once() {
    use crate::net::registry::ClientPacket;

    let server = shipyard_test_server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Owner", 100.0, 100.0),
        0x00F0_1001,
    );
    client.conn.active_char = Some(player.clone());
    player.data().money = 0;
    give_design_item(&server, &player, DESIGN_10, 18124);
    give_design_item(&server, &player, DESIGN_11, 18149);
    client.drain();

    // Build 10 through the packet handler (the real wiring), with the item the client names.
    shipyard_packet(SHIPYARD_10, DESIGN_10)
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert_eq!(server.systems.vehicles.shipyards.all().len(), 1);
    assert!(!design_item_still_owned(&server, &player, DESIGN_10));
    assert!(design_item_still_owned(&server, &player, DESIGN_11));

    // The same design item cannot be consumed twice: the second create is refused and the
    // first shipyard stays.
    assert!(super::create(
        &server,
        &player,
        SHIPYARD_10,
        120.0,
        121.0,
        100.0,
        0.5,
        DESIGN_10,
        0
    )
    .is_none());
    assert_eq!(server.systems.vehicles.shipyards.all().len(), 1);

    // Build 11 from its own design item.
    super::create(
        &server,
        &player,
        SHIPYARD_11,
        130.0,
        130.0,
        100.0,
        0.0,
        DESIGN_11,
        0,
    )
    .expect("shipyard 11");
    assert_eq!(server.systems.vehicles.shipyards.all().len(), 2);
    assert!(!design_item_still_owned(&server, &player, DESIGN_11));
}

#[tokio::test]
async fn wrong_missing_or_foreign_design_item_is_refused_and_kept() {
    let server = shipyard_test_server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Owner", 100.0, 100.0),
        0x00F0_1001,
    );
    client.conn.active_char = Some(player.clone());
    player.data().money = 0;
    give_design_item(&server, &player, DESIGN_11, 18149);
    client.drain();

    // An item that maps to shipyard 11 cannot build 10.
    assert!(super::create(
        &server,
        &player,
        SHIPYARD_10,
        120.0,
        120.0,
        100.0,
        0.0,
        DESIGN_11,
        0
    )
    .is_none());
    assert!(design_item_still_owned(&server, &player, DESIGN_11));
    assert!(server.systems.vehicles.shipyards.all().is_empty());

    // A zero / unknown item id is refused too.
    for design_item in [0, 0x0000_0000_0000_DEAD] {
        assert!(super::create(
            &server,
            &player,
            SHIPYARD_10,
            120.0,
            120.0,
            100.0,
            0.0,
            design_item,
            0
        )
        .is_none());
        assert!(design_item_still_owned(&server, &player, DESIGN_11));
        assert!(server.systems.vehicles.shipyards.all().is_empty());
    }

    // The refusal reached the client as the evidenced `NotEnoughItem` (27).
    let codes: Vec<i16> = client
        .drain()
        .iter()
        .filter(|f| f.0 == crate::opcodes::sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect();
    assert!(
        !codes.is_empty() && codes.iter().all(|c| *c == 27),
        "{codes:?}"
    );
}

#[tokio::test]
async fn destroyed_shipyard_spawns_the_reward_doodads() {
    let server = shipyard_test_server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Owner", 100.0, 100.0),
        0x00F0_1001,
    );
    client.conn.active_char = Some(player.clone());
    player.data().money = 0;
    give_design_item(&server, &player, DESIGN_10, 18124);
    let shipyard = super::create(
        &server,
        &player,
        SHIPYARD_10,
        120.0,
        120.0,
        100.0,
        0.0,
        DESIGN_10,
        0,
    )
    .expect("shipyard");
    let world = server.world.world(0).unwrap();
    assert!(
        world.all_doodads().is_empty(),
        "create does not spawn reward debris"
    );

    // Only destruction spawns the `shipyard_rewards` wreckage (INFERRED trigger).
    super::on_death(&server, &shipyard);
    let debris = world.all_doodads();
    assert_eq!(debris.len(), 4);
    for doodad in &debris {
        let d = doodad.data();
        assert_eq!(d.template_id, 1330);
        let dx = d.transform.x - 120.0;
        let dy = d.transform.y - 120.0;
        assert!(
            (dx * dx + dy * dy).sqrt() <= 12.0 + 0.001,
            "reward outside the 12 m radius: ({}, {})",
            d.transform.x,
            d.transform.y
        );
    }
    assert!(server.systems.vehicles.shipyards.all().is_empty());
}

#[test]
fn reward_points_are_inside_the_radius() {
    use rand::SeedableRng;

    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let points = super::reward_points(&mut rng, (10.0, 20.0), 15.0, 64);
    assert_eq!(points.len(), 64);
    for (x, y) in points {
        assert!(
            ((x - 10.0).powi(2) + (y - 20.0).powi(2)).sqrt() <= 15.0001,
            "point ({x}, {y}) outside the radius"
        );
    }
    // A zero / negative count or radius yields no points.
    assert!(super::reward_points(&mut rng, (0.0, 0.0), 5.0, 0).is_empty());
    assert!(super::reward_points(&mut rng, (0.0, 0.0), 5.0, -1).is_empty());
    let centered = super::reward_points(&mut rng, (3.0, 4.0), 0.0, 4);
    assert_eq!(centered, vec![(3.0, 4.0); 4]);
}
