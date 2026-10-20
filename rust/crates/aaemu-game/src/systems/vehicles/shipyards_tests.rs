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
use crate::systems::vehicles::data::ShipyardTemplate;
use crate::test_support::test_server_with;

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
