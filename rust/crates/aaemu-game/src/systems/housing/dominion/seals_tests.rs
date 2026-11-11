//! Participant seals: the side's seal item is handed to an accepted roster entry and taken
//! back when the siege ends or the entry leaves. Mutation check: removing the grant / the
//! reconcile removal makes the counts below stay 0 / 1.

use super::runtime;
use super::seals::{grant_for_participant, grant_roster, reconcile, seal_count};
use super::state::SiegeSide;
use crate::data::items::ItemTemplate;
use crate::data::world_data::Zone;
use crate::data::GameData;
use crate::models::house::House;
use crate::server::GameServer;
use crate::systems::housing::data::{HousingTemplate, SiegeZone};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef};

const GROUP: u32 = 25;
const ATTACKER_EXPEDITION: u32 = 20;
const DEFENDER_EXPEDITION: u32 = 10;
const DEFENSE_SEAL: u32 = 21313;
const OFFENSE_SEAL: u32 = 21317;
const INSIDE: (f32, f32) = (100.0, 100.0);

struct Fixture {
    server: GameServer,
    attacker: PlayerRef,
    defender: PlayerRef,
    attacker_client: TestClient,
}

fn fixture() -> Fixture {
    fixture_at(true)
}

fn fixture_at(active: bool) -> Fixture {
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
    data.systems.housing.castles.siege_zones.insert(
        GROUP,
        SiegeZone {
            id: 1,
            zone_group_id: GROUP,
            defense_ticket_id: DEFENSE_SEAL,
            offense_ticket_id: OFFENSE_SEAL,
            ..Default::default()
        },
    );
    for id in [DEFENSE_SEAL, OFFENSE_SEAL] {
        data.items.templates.insert(id, ItemTemplate::plain(id, 1));
    }
    let server = test_server_with(data);
    crate::systems::skills::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    let mut attacker_client = TestClient::new(&server, 1, 1001);
    let mut defender_client = TestClient::new(&server, 2, 1002);
    let make_player = |id, name, expedition, client: &TestClient| {
        let mut c = test_character(id, name, INSIDE.0, INSIDE.1);
        c.unit.expedition_id = expedition;
        c.is_online = true;
        spawn_player(&server, client, c, 0x100 + id)
    };
    let attacker = make_player(1, "Attacker", ATTACKER_EXPEDITION, &attacker_client);
    let defender = make_player(2, "Defender", DEFENDER_EXPEDITION, &defender_client);
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
    attacker_client.drain();
    defender_client.drain();
    drop(defender_client);
    Fixture {
        server,
        attacker,
        defender,
        attacker_client,
    }
}

fn state(f: &Fixture) -> super::state::DominionState {
    f.server.systems.housing.dominions.get(GROUP).unwrap()
}

fn register(f: &Fixture, character_id: u32, expedition: u32, side: SiegeSide) {
    f.server
        .systems
        .housing
        .dominions
        .register_participant(
            GROUP,
            character_id,
            expedition,
            side,
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
}

#[tokio::test]
async fn a_selected_participant_receives_the_side_seal_once() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    register(&f, 2, DEFENDER_EXPEDITION, SiegeSide::Defender);
    assert!(grant_for_participant(
        &f.server,
        &state(&f),
        1,
        SiegeSide::Attacker
    ));
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    assert_eq!(seal_count(&f.attacker, DEFENSE_SEAL), 0);
    // A repeated join does not hand a second seal.
    assert!(!grant_for_participant(
        &f.server,
        &state(&f),
        1,
        SiegeSide::Attacker
    ));
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    assert!(grant_for_participant(
        &f.server,
        &state(&f),
        2,
        SiegeSide::Defender
    ));
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
    assert_eq!(seal_count(&f.defender, OFFENSE_SEAL), 0);
}

#[tokio::test]
async fn the_roster_sweep_hands_the_seal_to_every_online_member() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    register(&f, 2, DEFENDER_EXPEDITION, SiegeSide::Defender);
    grant_roster(&f.server, &state(&f));
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
}

#[tokio::test]
async fn the_seal_is_taken_back_after_the_siege_ends_or_the_member_leaves() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    grant_for_participant(&f.server, &state(&f), 1, SiegeSide::Attacker);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    f.server
        .systems
        .housing
        .dominions
        .cancel_siege(GROUP, chrono::Utc::now().timestamp())
        .unwrap();
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);

    // Leaving the roster also ends the entitlement while the siege is still running.
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    grant_for_participant(&f.server, &state(&f), 1, SiegeSide::Attacker);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    f.server
        .systems
        .housing
        .dominions
        .unregister_participant(GROUP, 1)
        .unwrap();
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
}

#[tokio::test]
async fn a_scheduled_roster_keeps_the_seal_until_the_siege_ends() {
    let f = fixture_at(false);
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    grant_for_participant(&f.server, &state(&f), 1, SiegeSide::Attacker);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    // The record exists (scheduled; the calendar warmup has the same shape) but the siege
    // has not ended: the seal must not be taken back.
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    f.server
        .systems
        .housing
        .dominions
        .cancel_siege(GROUP, chrono::Utc::now().timestamp())
        .unwrap();
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
}

#[tokio::test]
async fn reconcile_hands_a_missing_seal_back_to_an_online_member() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    grant_for_participant(&f.server, &state(&f), 1, SiegeSide::Attacker);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    // Simulate the 240-minute expiry / a destructive action during the siege.
    f.server.systems.skills.ext.items().consume_item_template(
        &f.server,
        &f.attacker,
        OFFENSE_SEAL,
        1,
    );
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
}

#[tokio::test]
async fn reconcile_grants_to_a_member_who_was_offline_at_registration() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    f.attacker.data().is_online = false;
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
    f.attacker.data().is_online = true;
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
}

#[tokio::test]
async fn a_rejoin_before_the_removal_keeps_the_seal() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    grant_for_participant(&f.server, &state(&f), 1, SiegeSide::Attacker);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    f.server
        .systems
        .housing
        .dominions
        .unregister_participant(GROUP, 1)
        .unwrap();
    // The join path inserts the roster before its grant; the reconcile removal re-reads the
    // roster under the seal-action lock, so this rejoin is visible and the seal is kept.
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
}

#[tokio::test]
async fn an_already_held_reinforcement_seal_is_not_duplicated_or_revoked() {
    let f = fixture();
    // Skill 16975 (defence reinforcements) grants the same defence seal outside the roster.
    f.server.systems.skills.ext.items().try_add_new_item(
        &f.server,
        &f.defender,
        crate::models::item_task::task_type::SKILL_EFFECT_GAIN_ITEM,
        DEFENSE_SEAL,
        1,
        0,
    );
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
    register(&f, 2, DEFENDER_EXPEDITION, SiegeSide::Defender);
    assert!(!grant_for_participant(
        &f.server,
        &state(&f),
        2,
        SiegeSide::Defender
    ));
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
    f.server
        .systems
        .housing
        .dominions
        .cancel_siege(GROUP, chrono::Utc::now().timestamp())
        .unwrap();
    reconcile(&f.server);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
}

#[tokio::test]
async fn an_offline_member_keeps_the_item_for_its_authored_lifetime_backstop() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    grant_for_participant(&f.server, &state(&f), 1, SiegeSide::Attacker);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    f.attacker.data().is_online = false;
    f.server
        .systems
        .housing
        .dominions
        .cancel_siege(GROUP, chrono::Utc::now().timestamp())
        .unwrap();
    reconcile(&f.server);
    // The 240-minute item lifetime is the only cleanup for an offline character.
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
}

#[tokio::test]
async fn the_grant_is_client_visible_as_an_item_task() {
    let mut f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    grant_for_participant(&f.server, &state(&f), 1, SiegeSide::Attacker);
    let frames = f.attacker_client.drain();
    assert!(
        frames
            .iter()
            .any(|(opcode, _)| *opcode == crate::opcodes::sc::SCItemTaskSuccessPacket),
        "the client receives the acquired seal"
    );
}
