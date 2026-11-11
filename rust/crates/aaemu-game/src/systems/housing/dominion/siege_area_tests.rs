//! Siege-area eviction: a non-participant inside the circle of a running siege is moved to
//! the nearest safe resurrection point outside the area, after the client's
//! `SIEGE_NOT_PARTICIPANT` (446) error. Mutation check: without `evict_non_participants` (or
//! with the participant test removed) the outsider receives no teleport and the assertions
//! fail.

use super::runtime;
use super::siege_area::evict_non_participants;
use super::siege_fixtures::{join_expedition, leave_expedition};
use super::state::SiegeSide;
use crate::data::world_data::{Portal, Zone};
use crate::data::GameData;
use crate::models::house::House;
use crate::models::slave::Slave;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate};
use crate::systems::social::chat::GM_ACCESS_LEVEL;
use crate::systems::vehicles::data::attach_point;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, WorldObject};

const GROUP: u32 = 25;
const ATTACKER_EXPEDITION: u32 = 20;
const DEFENDER_EXPEDITION: u32 = 10;
const INSIDE: (f32, f32) = (100.0, 100.0);
const OUTSIDE: (f32, f32) = (600.0, 100.0);
const RETURN_POINT: (f32, f32, f32) = (1_000.0, 1_000.0, 100.0);
const ERROR_SIEGE_NOT_PARTICIPANT: i16 = 446;
const SLAVE_TEMPLATE: u32 = 300;

struct Fixture {
    server: GameServer,
    attacker: PlayerRef,
    outsider: PlayerRef,
    attacker_client: TestClient,
    outsider_client: TestClient,
}

fn fixture(active: bool) -> Fixture {
    fixture_with(active, &[(86, RETURN_POINT)])
}

/// The fixture with the given respawn points `(portal id, (x, y, z))` in `TEST_ZONE`.
fn fixture_with(active: bool, return_points: &[(u32, (f32, f32, f32))]) -> Fixture {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            zone_key: TEST_ZONE,
            group_id: GROUP,
            ..Default::default()
        },
    );
    data.systems.housing.add_template(HousingTemplate {
        id: 139,
        guard_tower_setting_id: 6,
        ..Default::default()
    });
    data.systems.housing.castles.guard_tower_settings.insert(
        6,
        GuardTowerSetting {
            id: 6,
            radius_siege: 250,
            radius_offense_hq: 100,
            ..Default::default()
        },
    );
    for &(id, (x, y, z)) in return_points {
        data.systems.npcs.portal.respawns.portals.push(Portal {
            id,
            name: format!("test return point {id}"),
            zone_id: TEST_ZONE,
            x,
            y,
            z,
            ..Default::default()
        });
    }
    let server = test_server_with(data);
    crate::systems::skills::install_hooks(&server);
    let mut attacker_client = TestClient::new(&server, 1, 1001);
    let mut outsider_client = TestClient::new(&server, 2, 1002);
    let make_player = |id, name, expedition, x, y, client: &TestClient| {
        let mut c = test_character(id, name, x, y);
        c.unit.hp = 1_000;
        c.unit.max_hp = 1_000;
        c.is_online = true;
        let player = spawn_player(&server, client, c, 0x100 + id);
        if expedition != 0 {
            join_expedition(&server, expedition, &player);
        }
        player
    };
    let attacker = make_player(
        1,
        "Attacker",
        ATTACKER_EXPEDITION,
        INSIDE.0,
        INSIDE.1,
        &attacker_client,
    );
    let outsider = make_player(2, "Outsider", 0, INSIDE.0, INSIDE.1, &outsider_client);
    let mut house = House::new(61, 139);
    house.current_step = -1;
    house.unit.transform.zone_id = TEST_ZONE;
    house.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let tower = Entity::new(0x200, house);
    server.systems.housing.manager.insert(&tower);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&tower, &server.view_ctx());
    let (_, identity) = runtime::tower_identity(&server, &tower).unwrap();
    let now = chrono::Utc::now().timestamp();
    server
        .systems
        .housing
        .dominions
        .declare_unowned_with(GROUP, identity, DEFENDER_EXPEDITION, now - 1000, || true)
        .unwrap();
    let (starts_at, ends_at) = if active {
        (now - 10, now + 3600)
    } else {
        (now + 600, now + 3600)
    };
    server
        .systems
        .housing
        .dominions
        .schedule_siege(GROUP, ATTACKER_EXPEDITION, starts_at, ends_at, now - 20)
        .unwrap();
    server
        .systems
        .housing
        .dominions
        .register_participant(GROUP, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker, now - 10)
        .unwrap();
    super::siege_area::refresh(&server);
    attacker_client.drain();
    outsider_client.drain();
    Fixture {
        server,
        attacker,
        outsider,
        attacker_client,
        outsider_client,
    }
}

/// `(error message, x, y, z)` of an `SCTeleportUnitPacket` body.
fn decode_teleport(body: &[u8]) -> (i16, f32, f32, f32) {
    let error = i16::from_le_bytes([body[1], body[2]]);
    let position: [u8; 9] = body[3..12].try_into().expect("position");
    let (x, y, z) = aaemu_commons::helpers::decode_position(&position);
    (error, x, y, z)
}

/// The forced move `(error, x, y, z)` lands on the given spot (decoded floats: a tolerance).
fn assert_at(moved: &(i16, f32, f32, f32), x: f32, y: f32) {
    assert!(
        (moved.1 - x).abs() < 0.01 && (moved.2 - y).abs() < 0.01,
        "expected ({x}, {y}), got {moved:?}"
    );
}

fn teleports(client: &mut TestClient) -> Vec<(i16, f32, f32, f32)> {
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCTeleportUnitPacket)
        .map(|(_, body)| decode_teleport(&body))
        .collect()
}

/// A calendar-materialised siege: its start/end follow the calendar / GM test clock, like
/// `DominionManager::tick_dual`.
fn materialised_fixture(clock: i64) -> Fixture {
    let f = fixture(true);
    let now = clock - 100;
    f.server
        .systems
        .housing
        .dominions
        .cancel_siege(GROUP, chrono::Utc::now().timestamp())
        .unwrap();
    let cycle = 1_000i64;
    f.server
        .systems
        .housing
        .dominions
        .declare(GROUP, ATTACKER_EXPEDITION, cycle, now, 0, (0.0, 0.0, 0.0))
        .unwrap();
    f.server
        .systems
        .housing
        .dominions
        .materialise_declaration(
            GROUP,
            cycle,
            clock - 10,
            clock + 3_600,
            now,
            crate::systems::housing::dominion::SiegeLimits::NATIVE,
        )
        .unwrap();
    f.server
        .systems
        .housing
        .dominions
        .register_participant(GROUP, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker, now)
        .unwrap();
    super::siege_area::refresh(&f.server);
    f
}

fn move_to(fixture: &Fixture, player: &PlayerRef, (x, y): (f32, f32)) {
    let object = WorldObject::from(player);
    player.data().unit.transform.set_xyz(x, y, 100.0);
    fixture
        .server
        .world
        .world(0)
        .unwrap()
        .add_visible_object(&object, &fixture.server.view_ctx());
}

/// The opcode of every frame the client received, in order.
fn opcodes(client: &mut TestClient) -> Vec<u16> {
    client.drain().into_iter().map(|(op, _)| op).collect()
}

#[tokio::test]
async fn a_non_participant_inside_the_circle_is_moved_to_the_return_point() {
    let mut f = fixture(true);
    evict_non_participants(&f.server);
    let frames = f.outsider_client.drain();
    let error_at = frames.iter().position(|(op, body)| {
        *op == sc::SCErrorMsgPacket
            && i16::from_le_bytes([body[0], body[1]]) == ERROR_SIEGE_NOT_PARTICIPANT
    });
    let teleport_at = frames
        .iter()
        .position(|(op, _)| *op == sc::SCTeleportUnitPacket);
    assert!(
        error_at.is_some() && teleport_at.is_some() && error_at < teleport_at,
        "the client's 'Not a siege participant' text comes right before the forced move"
    );
    let moves: Vec<_> = frames
        .iter()
        .filter(|(op, _)| *op == sc::SCTeleportUnitPacket)
        .map(|(_, body)| decode_teleport(body))
        .collect();
    assert_eq!(moves.len(), 1, "one forced move per eviction");
    // The `SCTeleportUnit` error field stays 0, like at every other sender.
    assert_eq!(moves[0].0, 0);
    assert_at(&moves[0], RETURN_POINT.0, RETURN_POINT.1);
    // The registered attacker is kept and told nothing.
    assert!(opcodes(&mut f.attacker_client).is_empty());
}

#[tokio::test]
async fn the_nearest_return_point_inside_the_area_gives_way_to_the_next_one() {
    // 87 is the closest respawn point to the outsider but lies inside the circle (the tower is
    // at 105, 100, the radius 250): the next closest one outside it, 86, is used.
    let mut f = fixture_with(true, &[(86, RETURN_POINT), (87, (110.0, 100.0, 100.0))]);
    evict_non_participants(&f.server);
    let moves = teleports(&mut f.outsider_client);
    assert_eq!(moves.len(), 1);
    assert_at(&moves[0], RETURN_POINT.0, RETURN_POINT.1);
    // A closer point outside the circle wins over a farther one.
    let mut f = fixture_with(true, &[(86, RETURN_POINT), (88, (400.0, 100.0, 100.0))]);
    evict_non_participants(&f.server);
    let moves = teleports(&mut f.outsider_client);
    assert_eq!(moves.len(), 1);
    assert_at(&moves[0], 400.0, 100.0);
}

#[tokio::test]
async fn without_a_return_point_outside_the_area_the_character_stays_where_it_is() {
    // Every respawn point lies inside the circle (or there is none): nobody is moved, and the
    // saved main-world position (the spot of the last instance entry) is no fallback.
    for points in [&[(87, (110.0, 100.0, 100.0))][..], &[][..]] {
        let mut f = fixture_with(true, points);
        let mut stale = crate::models::transform::Transform::default();
        stale.set_xyz(5_000.0, 5_000.0, 100.0);
        f.outsider.data().main_world_position = Some(stale);
        evict_non_participants(&f.server);
        assert!(
            opcodes(&mut f.outsider_client).is_empty(),
            "no error text and no move without a safe point"
        );
        assert!(!f.outsider.data().disabled_set_position);
    }
}

#[tokio::test]
async fn a_character_without_a_safe_point_is_warned_about_once_until_it_is_free_again() {
    let f = fixture_with(true, &[]);
    let cache = &f.server.systems.housing.siege_areas;
    let id = f.outsider.character_id;
    evict_non_participants(&f.server);
    // The sweep recorded its warning: the next report of the same character is a repeat ...
    assert!(!cache.first_warning(id));
    evict_non_participants(&f.server);
    assert!(!cache.first_warning(id), "still the same stay");
    // ... until the character is out of the area: the set forgets it, a later stay warns again.
    move_to(&f, &f.outsider, OUTSIDE);
    evict_non_participants(&f.server);
    assert!(
        cache.first_warning(id),
        "re-armed once the character was free"
    );
}

#[tokio::test]
async fn a_non_participant_outside_the_circle_or_outside_the_running_siege_is_kept() {
    let f = fixture(true);
    move_to(&f, &f.outsider, OUTSIDE);
    evict_non_participants(&f.server);
    let mut f = f;
    assert!(teleports(&mut f.outsider_client).is_empty());

    let mut warmup = fixture(false);
    evict_non_participants(&warmup.server);
    assert!(teleports(&mut warmup.outsider_client).is_empty());
}

#[tokio::test]
async fn an_unregistered_defender_expedition_member_is_still_evicted() {
    let mut f = fixture(true);
    // A member of the defending guild who never joined the roster is not a participant.
    join_expedition(&f.server, DEFENDER_EXPEDITION, &f.outsider);
    evict_non_participants(&f.server);
    assert_eq!(teleports(&mut f.outsider_client).len(), 1);
}

#[tokio::test]
async fn an_ex_member_with_a_roster_entry_is_moved_like_any_outsider() {
    // The snapshot still lists the attacker (it was taken before the leave): the live
    // expedition of the character decides at once.
    let mut f = fixture(true);
    leave_expedition(&f.server, ATTACKER_EXPEDITION, &f.attacker);
    evict_non_participants(&f.server);
    assert_eq!(teleports(&mut f.attacker_client).len(), 1);

    // After the next snapshot the shared participant rule leaves them out as well.
    let mut f = fixture(true);
    leave_expedition(&f.server, ATTACKER_EXPEDITION, &f.attacker);
    super::siege_area::refresh(&f.server);
    let roster = f.server.systems.housing.dominions.get(GROUP).unwrap().siege;
    assert!(roster.unwrap().participants.contains_key(&1));
    evict_non_participants(&f.server);
    assert_eq!(teleports(&mut f.attacker_client).len(), 1);
}

#[tokio::test]
async fn a_member_who_joined_after_the_snapshot_is_not_moved() {
    let mut f = fixture(true);
    // The outsider joins the attacking guild and the roster after the snapshot of `fixture`.
    join_expedition(&f.server, ATTACKER_EXPEDITION, &f.outsider);
    f.server
        .systems
        .housing
        .dominions
        .register_participant(
            GROUP,
            f.outsider.character_id,
            ATTACKER_EXPEDITION,
            SiegeSide::Attacker,
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
    evict_non_participants(&f.server);
    assert!(
        teleports(&mut f.outsider_client).is_empty(),
        "the authority is asked again before anyone is moved"
    );
    // The next snapshot agrees.
    super::siege_area::refresh(&f.server);
    evict_non_participants(&f.server);
    assert!(teleports(&mut f.outsider_client).is_empty());
}

#[tokio::test]
async fn a_gm_inside_the_circle_is_not_moved() {
    // One level below the GM threshold is an ordinary outsider ...
    let mut f = fixture(true);
    f.outsider.data().access_level = GM_ACCESS_LEVEL - 1;
    evict_non_participants(&f.server);
    assert_eq!(teleports(&mut f.outsider_client).len(), 1);

    // ... a GM inspects the lodestone and the camps undisturbed.
    let mut f = fixture(true);
    f.outsider.data().access_level = GM_ACCESS_LEVEL;
    evict_non_participants(&f.server);
    assert!(teleports(&mut f.outsider_client).is_empty());
}

#[tokio::test]
async fn the_switch_off_evicts_nobody() {
    let mut f = fixture(true);
    f.server
        .config
        .update(|config| config.housing.siege_evict_non_participants = false);
    evict_non_participants(&f.server);
    assert!(teleports(&mut f.outsider_client).is_empty());
    // Back on (the default), the same outsider is moved.
    f.server
        .config
        .update(|config| config.housing.siege_evict_non_participants = true);
    evict_non_participants(&f.server);
    assert_eq!(teleports(&mut f.outsider_client).len(), 1);
}

#[tokio::test]
async fn the_calendar_clock_moves_the_siege_area_window() {
    let clock = chrono::Utc::now().timestamp() + 100_000;
    let mut f = materialised_fixture(clock);
    f.server
        .systems
        .housing
        .calendar
        .set_clock(Some(clock - 100));
    evict_non_participants(&f.server);
    assert!(
        teleports(&mut f.outsider_client).is_empty(),
        "before the calendar start"
    );
    f.server
        .systems
        .housing
        .calendar
        .set_clock(Some(clock + 10));
    evict_non_participants(&f.server);
    assert_eq!(
        teleports(&mut f.outsider_client).len(),
        1,
        "inside the calendar window"
    );
    f.server.systems.housing.calendar.set_clock(None);
}

#[tokio::test]
async fn a_mounted_non_participant_is_dismounted_before_the_move() {
    let mut f = fixture(true);
    // A slave passenger is an "attached" character (the mount / vehicle case the forced move
    // must not carry along): one seat on a live slave, the character seated in it.
    let mut slave = Slave::new(0x300, SLAVE_TEMPLATE);
    slave.unit.tl_id = 77;
    slave.unit.transform.zone_id = TEST_ZONE;
    slave.unit.transform.set_xyz(100.0, 100.0, 100.0);
    slave
        .attached_characters
        .push((attach_point::PASSENGER0, f.outsider.obj_id));
    let slave = Entity::new(0x300, slave);
    f.server
        .world
        .world(0)
        .unwrap()
        .spawn(&slave, &f.server.view_ctx());
    {
        let mut c = f.outsider.data();
        c.is_riding = true;
        c.attached_point = attach_point::PASSENGER0;
    }
    evict_non_participants(&f.server);
    assert!(!f.outsider.data().is_riding, "the passenger left the seat");
    assert!(slave
        .data()
        .attached_characters
        .iter()
        .all(|(_, c)| *c != f.outsider.obj_id));
    // The order the test names: the client is told that the rider left the vehicle before it
    // is told to move.
    let ops = opcodes(&mut f.outsider_client);
    let detached = ops
        .iter()
        .position(|op| *op == sc::SCUnitDetachedPacket)
        .expect("the rider is detached");
    let moved = ops
        .iter()
        .position(|op| *op == sc::SCTeleportUnitPacket)
        .expect("the rider is moved");
    assert!(detached < moved, "dismount first, then the move: {ops:?}");
}

#[tokio::test]
async fn a_pending_teleport_or_a_dead_character_is_not_moved_again() {
    let mut f = fixture(true);
    f.outsider.data().set_disabled_set_position(true);
    evict_non_participants(&f.server);
    assert!(teleports(&mut f.outsider_client).is_empty());
    f.outsider.data().set_disabled_set_position(false);
    f.outsider.data().unit.hp = 0;
    evict_non_participants(&f.server);
    assert!(teleports(&mut f.outsider_client).is_empty());
}
