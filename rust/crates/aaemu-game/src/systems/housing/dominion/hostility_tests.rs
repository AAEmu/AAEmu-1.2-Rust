//! Siege hostility wiring: the rule runs through the real `skills::relation::can_attack`,
//! the real authority and a live lodestone. Each assertion was mutation-checked (removing the
//! `siege_can_attack` call makes the inside-circle assertions fail).

use super::hostility::{classify, Party, SiegeRelation};
use super::runtime;
use super::state::SiegeSide;
use crate::data::world_data::{conflict_type, FactionRelation, SystemFaction, Zone, ZoneConflict};
use crate::data::GameData;
use crate::models::house::House;
use crate::server::GameServer;
use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate};
use crate::systems::instances::data::IndunZone;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::relation::can_attack;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, WorldObject};

const GROUP: u32 = 25;
const ATTACKER_EXPEDITION: u32 = 20;
const DEFENDER_EXPEDITION: u32 = 10;
const TOWER_X: f32 = 105.0;
const TOWER_Y: f32 = 100.0;
const INSIDE_X: f32 = 100.0;
const INSIDE_Y: f32 = 100.0;
const OUTSIDE_X: f32 = 600.0;
const OUTSIDE_Y: f32 = 100.0;

struct Fixture {
    server: GameServer,
    attacker: PlayerRef,
    defender: PlayerRef,
    same_party: PlayerRef,
    outsider: PlayerRef,
}

fn fixture(active: bool, no_pvp: bool) -> Fixture {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            zone_key: TEST_ZONE,
            group_id: GROUP,
            ..Default::default()
        },
    );
    for id in [101, 109] {
        data.factions.add(SystemFaction {
            id,
            mother_id: 0,
            ..Default::default()
        });
    }
    // The same expedition can hold characters of two factions; the faction relation must not
    // decide a siege party pair.
    data.factions.relations.push(FactionRelation {
        id: 101,
        id2: 109,
        state: crate::systems::skills::types::RelationState::Hostile as u8,
        exp_time: 0,
    });
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
    if no_pvp {
        data.systems.instances.indun.zones.insert(
            GROUP,
            IndunZone {
                zone_group_id: GROUP,
                pvp: false,
                ..IndunZone::default()
            },
        );
    }
    let server = test_server_with(data);
    crate::systems::skills::install_hooks(&server);
    let mut attacker_client = TestClient::new(&server, 1, 1001);
    let mut defender_client = TestClient::new(&server, 2, 1002);
    let mut same_party_client = TestClient::new(&server, 3, 1003);
    let mut outsider_client = TestClient::new(&server, 4, 1004);
    let make_player = |id, name, expedition, faction, x, y, client: &TestClient| {
        let mut c = test_character(id, name, x, y);
        c.unit.expedition_id = expedition;
        c.unit.faction_id = faction;
        c.is_online = true;
        spawn_player(&server, client, c, 0x100 + id)
    };
    let attacker = make_player(
        1,
        "Attacker",
        ATTACKER_EXPEDITION,
        101,
        INSIDE_X,
        INSIDE_Y,
        &attacker_client,
    );
    let defender = make_player(
        2,
        "Defender",
        DEFENDER_EXPEDITION,
        101,
        INSIDE_X,
        INSIDE_Y,
        &defender_client,
    );
    let same_party = make_player(
        3,
        "SameParty",
        ATTACKER_EXPEDITION,
        109,
        INSIDE_X,
        INSIDE_Y,
        &same_party_client,
    );
    let outsider = make_player(4, "Outsider", 0, 101, INSIDE_X, INSIDE_Y, &outsider_client);
    let mut house = House::new(61, 139);
    house.current_step = -1;
    house.unit.transform.zone_id = TEST_ZONE;
    house.unit.transform.set_xyz(TOWER_X, TOWER_Y, 100.0);
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
    let (starts_at, ends_at, scheduled_at) = if active {
        (now - 10, now + 3600, now - 20)
    } else {
        (now + 600, now + 3600, now - 20)
    };
    server
        .systems
        .housing
        .dominions
        .schedule_siege(GROUP, ATTACKER_EXPEDITION, starts_at, ends_at, scheduled_at)
        .unwrap();
    super::siege_area::refresh(&server);
    attacker_client.drain();
    defender_client.drain();
    same_party_client.drain();
    outsider_client.drain();
    drop(attacker_client);
    drop(defender_client);
    drop(same_party_client);
    drop(outsider_client);
    Fixture {
        server,
        attacker,
        defender,
        same_party,
        outsider,
    }
}

/// A calendar-materialised siege: its window follows the calendar / GM test clock.
fn materialised_fixture(clock: i64) -> Fixture {
    let f = fixture(true, false);
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
    super::siege_area::refresh(&f.server);
    f
}

fn target(player: &PlayerRef) -> Target {
    Target::Obj(WorldObject::from(player))
}

fn move_to(fixture: &Fixture, player: &PlayerRef, x: f32, y: f32) {
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
async fn attackers_and_defenders_are_hostile_inside_the_running_siege_only() {
    let f = fixture(true, false);
    assert!(
        can_attack(&f.server, &target(&f.attacker), &target(&f.defender)),
        "the attacking and defending roster parties are hostile while the siege runs"
    );
    assert!(can_attack(
        &f.server,
        &target(&f.defender),
        &target(&f.attacker)
    ));
    // Same faction would be friendly without the siege rule.
    assert_eq!(
        crate::systems::skills::relation::faction_relation(&f.server, 101, 101),
        crate::systems::skills::types::RelationState::Friendly
    );

    // A scheduled (warmup) siege does not make them hostile: only the running window does.
    let warmup = fixture(false, false);
    assert!(!can_attack(
        &warmup.server,
        &target(&warmup.attacker),
        &target(&warmup.defender)
    ));
}

#[tokio::test]
async fn the_same_party_is_friendly_inside_and_the_circle_gate_preserves_faction_rules() {
    let f = fixture(true, false);
    // Two attacker-expedition characters of hostile factions: the siege party rule wins.
    assert_eq!(
        crate::systems::skills::relation::faction_relation(&f.server, 101, 109),
        crate::systems::skills::types::RelationState::Hostile
    );
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.same_party)
    ));
    // Outside the circle the siege rule does not apply; the normal hostile factions decide.
    move_to(&f, &f.same_party, OUTSIDE_X, OUTSIDE_Y);
    assert!(can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.same_party)
    ));
}

#[tokio::test]
async fn the_calendar_clock_arms_the_siege_relation() {
    let clock = chrono::Utc::now().timestamp() + 100_000;
    let f = materialised_fixture(clock);
    f.server
        .systems
        .housing
        .calendar
        .set_clock(Some(clock - 100));
    assert!(
        !can_attack(&f.server, &target(&f.attacker), &target(&f.defender)),
        "before the calendar start"
    );
    f.server
        .systems
        .housing
        .calendar
        .set_clock(Some(clock + 10));
    assert!(
        can_attack(&f.server, &target(&f.attacker), &target(&f.defender)),
        "inside the calendar window"
    );
    f.server.systems.housing.calendar.set_clock(None);
}

#[tokio::test]
async fn a_second_attacker_party_would_be_neutral() {
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            },
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 30
            }
        ),
        SiegeRelation::Neutral
    );
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            },
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            }
        ),
        SiegeRelation::Friendly
    );
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Defender,
                expedition_id: 10
            },
            Party {
                side: SiegeSide::Defender,
                expedition_id: 10
            }
        ),
        SiegeRelation::Friendly
    );
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Defender,
                expedition_id: 10
            },
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            }
        ),
        SiegeRelation::Hostile
    );
}

#[tokio::test]
async fn peace_and_no_pvp_protections_still_refuse_the_siege_attack() {
    let f = fixture(true, false);
    f.server.systems.social.conflicts.init(
        &[ZoneConflict {
            zone_group_id: GROUP as u16,
            current_state: conflict_type::PEACE,
            ..ZoneConflict::default()
        }],
        crate::world::now_ms(),
    );
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.defender)
    ));

    let no_pvp = fixture(true, true);
    assert!(!can_attack(
        &no_pvp.server,
        &target(&no_pvp.attacker),
        &target(&no_pvp.defender)
    ));
}

#[tokio::test]
async fn a_non_participant_is_not_made_hostile_by_the_siege() {
    let f = fixture(true, false);
    // The outsider shares the defender's faction and is in no siege party.
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.outsider)
    ));
    // A hostile-faction outsider keeps the normal faction relation (attackable), which proves
    // the siege rule did not neutralise it.
    let hostile_outsider = f.same_party.clone();
    hostile_outsider.data().unit.expedition_id = 0;
    assert!(can_attack(
        &f.server,
        &target(&f.defender),
        &target(&hostile_outsider)
    ));
}
