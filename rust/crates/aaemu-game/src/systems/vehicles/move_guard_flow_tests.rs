//! The server-side movement guard (`world::move_guard`) through the ordinary session path:
//! `CSMoveUnit` of a player on foot, an owned mate and a driven land vehicle.

use super::*;
use crate::models::movement::{MoveTypeActorFlags, UnitMove};
use crate::models::slave::Slave;
use crate::world::Entity;

const FAR: f32 = 5000.0;

fn unit_move(x: f32, y: f32, z: f32) -> MoveType {
    let mut mt = MoveType::new(MoveKind::Unit(UnitMove {
        x,
        y,
        z,
        ..Default::default()
    }));
    mt.flags = crate::models::movement::MoveTypeFlags::MOVING;
    mt
}

/// A moving player A at (100, 100, 100) and an observer B next to it.
fn fixture() -> (
    Arc<GameServer>,
    TestClient,
    TestClient,
    PlayerRef,
    PlayerRef,
) {
    let server = server_with(GameData::empty());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Mover", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Watcher", 110.0, 110.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    a.drain();
    b.drain();
    (server, a, b, pa, pb)
}

fn pos(p: &PlayerRef) -> (f32, f32, f32) {
    let t = p.data().unit.transform;
    (t.x, t.y, t.z)
}

fn count(ops: &[u16], op: u16) -> usize {
    ops.iter().filter(|o| **o == op).count()
}

#[tokio::test]
async fn a_walk_of_small_steps_is_accepted_and_broadcast() {
    let (server, mut a, mut b, pa, _pb) = fixture();
    for i in 1..=20 {
        let x = 100.0 + i as f32 * 2.0;
        crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(x, 100.0, 100.0))
            .await;
    }
    assert_eq!(pos(&pa), (140.0, 100.0, 100.0));
    assert_eq!(count(&b.drain_ops(), sc::SCOneUnitMovementPacket), 20);
    assert!(!pa.data().disabled_set_position);
}

#[tokio::test]
async fn a_5_km_jump_is_refused_and_the_client_is_sent_back() {
    let (server, mut a, mut b, pa, _pb) = fixture();
    crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(FAR, FAR, 100.0)).await;
    assert_eq!(pos(&pa), (100.0, 100.0, 100.0));
    assert!(!b.drain_ops().contains(&sc::SCOneUnitMovementPacket));
    let ops = a.drain_ops();
    assert_eq!(count(&ops, sc::SCTeleportUnitPacket), 1, "{ops:?}");
    assert!(!ops.contains(&sc::SCOneUnitMovementPacket));
    // The correction is the usual teleport: locked until the client acknowledges, to the
    // position the server holds.
    assert!(pa.data().disabled_set_position);
    assert_eq!(pa.data().pending_teleport, Some((100.0, 100.0, 100.0)));
    // One audit entry, rate limited: a second refusal inside the window adds none.
    tokio::task::yield_now().await;
    let cheats = |server: &GameServer| {
        server
            .systems
            .services
            .sus
            .recent_activities()
            .iter()
            .filter(|(category, _, text)| category == "Cheat" && text.contains("Refused"))
            .count()
    };
    assert_eq!(cheats(&server), 1);
    // Moves are ignored while the correction is pending; after the acknowledgement the player
    // walks on from the old position.
    crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(101.0, 100.0, 100.0))
        .await;
    assert_eq!(pos(&pa), (100.0, 100.0, 100.0));
    crate::world::session::teleport_ended(&server, &pa, 100.0, 100.0, 100.0);
    crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(101.0, 100.0, 100.0))
        .await;
    assert_eq!(pos(&pa), (101.0, 100.0, 100.0));
}

#[tokio::test]
async fn a_non_finite_move_is_refused() {
    let (server, mut a, _b, pa, _pb) = fixture();
    for (x, z) in [(f32::NAN, 100.0), (101.0, f32::INFINITY)] {
        crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(x, 100.0, z)).await;
        assert_eq!(pos(&pa), (100.0, 100.0, 100.0));
        crate::world::session::teleport_ended(&server, &pa, 100.0, 100.0, 100.0);
    }
}

#[tokio::test]
async fn a_sustained_speed_hack_runs_out_of_budget() {
    let (server, mut a, _b, pa, _pb) = fixture();
    // 25 m steps back to back: three seconds of 30 m/s burst cover three of them.
    let mut accepted = 0;
    for i in 1..=10 {
        let before = pos(&pa);
        crate::world::session::move_unit(
            &server,
            &mut a.conn,
            PA,
            unit_move(100.0 + i as f32 * 25.0, 100.0, 100.0),
        )
        .await;
        if pos(&pa) != before {
            accepted += 1;
        } else {
            break;
        }
    }
    assert!((3..=4).contains(&accepted), "accepted {accepted}");
}

#[tokio::test]
async fn a_move_claiming_a_far_away_support_is_refused() {
    let (server, mut a, mut b, pa, _pb) = fixture();
    let world = server.world.world(0).unwrap();
    let mut hull = Slave::new(1, 1);
    hull.unit
        .transform
        .set_position(1500.0, 1500.0, 99.0, 0.0, 0.0, 0.0);
    let boat = Entity::new(0x300, hull);
    world.spawn(&boat, &server.view_ctx());
    a.drain();
    b.drain();
    let mut mt = unit_move(0.5, 0.5, 1.0);
    let MoveKind::Unit(u) = &mut mt.kind else {
        unreachable!()
    };
    u.actor_flags = MoveTypeActorFlags::STANDING_ON_OBJECT;
    u.gc_flags = 1;
    u.gc_id = boat.obj_id;
    // The world backup claims to be right where the player is.
    (u.x2, u.y2, u.z2) = (101.0, 100.0, 100.0);
    crate::world::session::move_unit(&server, &mut a.conn, PA, mt).await;
    assert_eq!(pos(&pa), (100.0, 100.0, 100.0));
    assert_eq!(pa.data().unit.transform.parent_obj_id, 0);
    assert!(!b.drain_ops().contains(&sc::SCOneUnitMovementPacket));
    assert!(a.drain_ops().contains(&sc::SCTeleportUnitPacket));
}

#[tokio::test]
async fn a_move_onto_a_near_support_is_accepted() {
    let (server, mut a, _b, pa, _pb) = fixture();
    let world = server.world.world(0).unwrap();
    let mut hull = Slave::new(1, 1);
    hull.unit
        .transform
        .set_position(102.0, 100.0, 99.0, 0.0, 0.0, 0.0);
    let boat = Entity::new(0x300, hull);
    world.spawn(&boat, &server.view_ctx());
    let mut mt = unit_move(-2.0, 0.0, 1.0);
    let MoveKind::Unit(u) = &mut mt.kind else {
        unreachable!()
    };
    u.actor_flags = MoveTypeActorFlags::STANDING_ON_OBJECT;
    u.gc_flags = 1;
    u.gc_id = boat.obj_id;
    crate::world::session::move_unit(&server, &mut a.conn, PA, mt).await;
    assert_eq!(pa.data().unit.transform.parent_obj_id, boat.obj_id);
    // Moving along the deck is measured in the deck's frame: a fast boat is not a speed hack.
    boat.data().unit.transform.x += 1000.0;
    let mut mt = unit_move(-1.0, 0.0, 1.0);
    let MoveKind::Unit(u) = &mut mt.kind else {
        unreachable!()
    };
    u.actor_flags = MoveTypeActorFlags::STANDING_ON_OBJECT;
    u.gc_flags = 1;
    u.gc_id = 1;
    crate::world::session::move_unit(&server, &mut a.conn, PA, mt).await;
    assert_eq!(pa.data().unit.transform.parent_obj_id, boat.obj_id);
    assert_eq!(pa.data().unit.transform.x, -1.0);
}

#[tokio::test]
async fn a_disabled_guard_or_an_exempt_access_level_keeps_the_old_behaviour() {
    // Disabled by configuration.
    let mut config = crate::config::AppConfig {
        id: 1,
        ..Default::default()
    };
    config.world.move_guard_enabled = false;
    let server = Arc::new(crate::test_support::test_server_with_config(
        GameData::empty(),
        config,
    ));
    server.set_self();
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Mover", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(1000.0, 1000.0, 100.0))
        .await;
    assert_eq!(pos(&pa), (1000.0, 1000.0, 100.0));

    // Exempt access level.
    let (server, mut a, _b, pa, _pb) = fixture();
    pa.data().access_level = server.config.world.move_guard_exempt_access_level;
    crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(1000.0, 1000.0, 100.0))
        .await;
    assert_eq!(pos(&pa), (1000.0, 1000.0, 100.0));
    // One below the exemption is checked.
    let (server, mut a, _b, pa, _pb) = fixture();
    pa.data().access_level = server.config.world.move_guard_exempt_access_level - 1;
    crate::world::session::move_unit(&server, &mut a.conn, PA, unit_move(1000.0, 1000.0, 100.0))
        .await;
    assert_eq!(pos(&pa), (100.0, 100.0, 100.0));
}

#[tokio::test]
async fn a_mate_jump_is_refused_and_the_rider_dismounted() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    a.conn.active_char = Some(pa.clone());
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .unwrap();
    let tl = mate.data().unit.tl_id;
    mates::mount_mate(&server, &pa, u32::from(tl), attach_point::DRIVER, 1);
    assert!(pa.data().is_riding);
    let before = {
        let t = mate.data().unit.transform;
        (t.x, t.y, t.z)
    };
    a.drain();
    // A short mate move is fine.
    crate::world::session::move_unit(
        &server,
        &mut a.conn,
        mate.obj_id,
        unit_move(before.0 + 3.0, before.1, before.2),
    )
    .await;
    assert_eq!(mate.data().unit.transform.x, before.0 + 3.0);
    // A jump is not.
    let at = mate.data().unit.transform;
    crate::world::session::move_unit(
        &server,
        &mut a.conn,
        mate.obj_id,
        unit_move(FAR, FAR, before.2),
    )
    .await;
    let t = mate.data().unit.transform;
    assert_eq!((t.x, t.y, t.z), (at.x, at.y, at.z));
    assert!(!pa.data().is_riding);
    assert_eq!(pa.data().pending_teleport, Some((at.x, at.y, at.z)));
}

#[tokio::test]
async fn a_driven_vehicle_jump_is_refused() {
    let (server, mut a, mut b, pa, _pb) = fixture();
    let world = server.world.world(0).unwrap();
    let mut car = Slave::new(1, 1);
    car.unit
        .transform
        .set_position(100.0, 105.0, 100.0, 0.0, 0.0, 0.0);
    let car = Entity::new(0x301, car);
    world.spawn(&car, &server.view_ctx());
    car.data().attached_characters = vec![(attach_point::DRIVER, PA)];
    pa.data().attached_point = attach_point::DRIVER;
    a.drain();
    b.drain();
    let drive = |x: f32, y: f32| {
        MoveType::new(MoveKind::Vehicle(VehicleMove {
            x,
            y,
            z: 100.0,
            ..VehicleMove::default()
        }))
    };
    crate::world::session::move_unit(&server, &mut a.conn, car.obj_id, drive(110.0, 105.0)).await;
    assert_eq!(car.data().unit.transform.x, 110.0);
    assert_eq!(count(&b.drain_ops(), sc::SCOneUnitMovementPacket), 1);
    crate::world::session::move_unit(&server, &mut a.conn, car.obj_id, drive(FAR, FAR)).await;
    assert_eq!(car.data().unit.transform.x, 110.0);
    assert!(!b.drain_ops().contains(&sc::SCOneUnitMovementPacket));
    assert!(a.drain_ops().contains(&sc::SCTeleportUnitPacket));
}
