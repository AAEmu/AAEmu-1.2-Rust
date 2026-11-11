//! Siege-area eviction: a non-participant inside the circle of a running siege is moved to
//! the nearest safe resurrection point with the client's `SIEGE_NOT_PARTICIPANT` (446).
//! Mutation check: without `evict_non_participants` (or with the participant test removed)
//! the outsider receives no teleport and the assertions fail.

use super::hostility::siege_can_attack;
use super::runtime;
use super::siege_area::evict_non_participants;
use super::state::SiegeSide;
use crate::data::world_data::{Portal, Zone};
use crate::data::GameData;
use crate::models::house::House;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate};
use crate::systems::skills::geometry::Target;
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

struct Fixture {
    server: GameServer,
    attacker: PlayerRef,
    outsider: PlayerRef,
    attacker_client: TestClient,
    outsider_client: TestClient,
}

fn fixture(active: bool) -> Fixture {
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
    data.systems.npcs.portal.respawns.portals.push(Portal {
        id: 86,
        name: "test return point".into(),
        zone_id: TEST_ZONE,
        x: RETURN_POINT.0,
        y: RETURN_POINT.1,
        z: RETURN_POINT.2,
        ..Default::default()
    });
    let server = test_server_with(data);
    crate::systems::skills::install_hooks(&server);
    let mut attacker_client = TestClient::new(&server, 1, 1001);
    let mut outsider_client = TestClient::new(&server, 2, 1002);
    let make_player = |id, name, expedition, x, y, client: &TestClient| {
        let mut c = test_character(id, name, x, y);
        c.unit.expedition_id = expedition;
        c.unit.hp = 1_000;
        c.unit.max_hp = 1_000;
        c.is_online = true;
        spawn_player(&server, client, c, 0x100 + id)
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

fn teleports(client: &mut TestClient) -> Vec<(i16, f32, f32, f32)> {
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCTeleportUnitPacket)
        .map(|(_, body)| {
            let error = i16::from_le_bytes([body[1], body[2]]);
            let position: [u8; 9] = body[3..12].try_into().expect("position");
            let (x, y, z) = aaemu_commons::helpers::decode_position(&position);
            (error, x, y, z)
        })
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

#[tokio::test]
async fn a_non_participant_inside_the_circle_is_moved_to_the_return_point() {
    let mut f = fixture(true);
    evict_non_participants(&f.server);
    let outsider = teleports(&mut f.outsider_client);
    assert_eq!(outsider.len(), 1, "one forced move per eviction");
    assert_eq!(outsider[0].0, ERROR_SIEGE_NOT_PARTICIPANT);
    assert!(
        (outsider[0].1 - RETURN_POINT.0).abs() < 0.01,
        "got {:?}",
        outsider[0]
    );
    assert!(
        (outsider[0].2 - RETURN_POINT.1).abs() < 0.01,
        "got {:?}",
        outsider[0]
    );
    // The registered attacker is kept.
    assert!(teleports(&mut f.attacker_client).is_empty());
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
    f.outsider.data().unit.expedition_id = DEFENDER_EXPEDITION;
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

#[tokio::test]
async fn the_area_rule_matches_the_hostility_gate() {
    let f = fixture(true);
    let outsider = Target::Obj(WorldObject::from(&f.outsider));
    let attacker = Target::Obj(WorldObject::from(&f.attacker));
    // Both are inside, but neither the outsider's party gate nor the same-party gate is
    // hostile: the eviction and the hostility rule share the same cached area.
    let area = f.server.systems.housing.siege_areas.get(GROUP).unwrap();
    assert!(area.inside(0, INSIDE.0, INSIDE.1));
    assert!(!area.inside(0, OUTSIDE.0, OUTSIDE.1));
    assert_eq!(siege_can_attack(&f.server, &attacker, &outsider), None);
}
