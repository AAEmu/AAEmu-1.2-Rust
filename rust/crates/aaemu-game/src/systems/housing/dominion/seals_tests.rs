//! Participant seals: the sweep (`seals::reconcile`, the only writer) hands the side's seal to
//! a roster entry and takes it back when the siege ends or the entry leaves. Mutation check:
//! removing the grant / the removal half of `reconcile` makes the counts below stay 0 / 1.

use super::runtime;
use super::seals::{reconcile, seal_count};
use super::siege_fixtures::{disband_expedition, join_expedition, leave_expedition};
use super::state::SiegeSide;
use crate::data::items::ItemTemplate;
use crate::data::world_data::Zone;
use crate::data::GameData;
use crate::models::house::House;
use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::housing::data::{HousingTemplate, SiegeZone};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef};

const GROUP: u32 = 25;
const ATTACKER_EXPEDITION: u32 = 20;
const DEFENDER_EXPEDITION: u32 = 10;
const DEFENSE_SEAL: u32 = 21313;
const OFFENSE_SEAL: u32 = 21317;
/// A non-stacking filler item for the full-bag test.
const FILLER: u32 = 999;
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
    for id in [DEFENSE_SEAL, OFFENSE_SEAL, FILLER] {
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
        c.is_online = true;
        let player = spawn_player(&server, client, c, 0x100 + id);
        join_expedition(&server, expedition, &player);
        player
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

fn unregister(f: &Fixture, character_id: u32) {
    f.server
        .systems
        .housing
        .dominions
        .unregister_participant(GROUP, character_id)
        .unwrap();
}

fn cancel(f: &Fixture) {
    f.server
        .systems
        .housing
        .dominions
        .cancel_siege(GROUP, chrono::Utc::now().timestamp())
        .unwrap();
}

#[tokio::test]
async fn a_participant_receives_the_side_seal_from_the_sweep_once() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    register(&f, 2, DEFENDER_EXPEDITION, SiegeSide::Defender);
    // A join hands nothing by itself: the sweep is the only writer.
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    assert_eq!(seal_count(&f.attacker, DEFENSE_SEAL), 0);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
    assert_eq!(seal_count(&f.defender, OFFENSE_SEAL), 0);
    // A repeated sweep hands no second seal.
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
}

#[tokio::test]
async fn the_seal_is_taken_back_after_the_siege_ends_or_the_member_leaves() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    cancel(&f);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);

    // Leaving the roster also ends the entitlement while the siege is still running.
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    unregister(&f, 1);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
}

#[tokio::test]
async fn a_scheduled_roster_keeps_the_seal_until_the_siege_ends() {
    let f = fixture_at(false);
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    // The record exists (scheduled; the calendar warmup has the same shape) but the siege
    // has not ended: the seal must not be taken back.
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    cancel(&f);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
}

#[tokio::test]
async fn reconcile_hands_a_missing_seal_back_to_an_online_member() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
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
async fn a_full_bag_gets_the_seal_on_the_first_sweep_after_a_slot_frees_up() {
    let f = fixture();
    let items = f.server.systems.skills.ext.items();
    let bag = Inv::of(&f.attacker).bag();
    let size = bag.lock().unwrap().container_size();
    for _ in 0..size {
        assert!(items.try_add_new_item(
            &f.server,
            &f.attacker,
            task_type::SKILL_EFFECT_GAIN_ITEM,
            FILLER,
            1,
            0
        ));
    }
    assert_eq!(bag.lock().unwrap().free_slot_count(), 0, "the bag is full");
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0, "no room yet");
    // The failed grant is not tracked as handed: a second sweep fails the same way and the
    // end of the siege takes nothing.
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
    items.consume_item_template(&f.server, &f.attacker, FILLER, 1);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    assert_eq!(seal_count(&f.attacker, FILLER), size - 1);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1, "no second seal");
}

#[tokio::test]
async fn a_member_who_leaves_and_rejoins_ends_up_with_one_seal() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    // A leave seen by a sweep takes the seal; the rejoin is handed a fresh one by the next
    // sweep, so the lock-free sweep self-heals instead of needing a lock between its halves.
    unregister(&f, 1);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    // A leave and a rejoin between two sweeps changes nothing at all.
    unregister(&f, 1);
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
}

#[tokio::test]
async fn an_ex_member_with_a_roster_entry_loses_the_seal_and_gets_none() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    // Left (or kicked): nothing removes the roster entry, but the member is no participant.
    leave_expedition(&f.server, ATTACKER_EXPEDITION, &f.attacker);
    let roster = f.server.systems.housing.dominions.get(GROUP).unwrap().siege;
    assert!(roster.unwrap().participants.contains_key(&1));
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0, "not handed again");
    // Back in the guild they are a participant again and get a seal.
    join_expedition(&f.server, ATTACKER_EXPEDITION, &f.attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);

    // A member who left before the first sweep is never handed one.
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    leave_expedition(&f.server, ATTACKER_EXPEDITION, &f.attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 0);
}

#[tokio::test]
async fn a_disbanded_expedition_has_no_participants() {
    let f = fixture();
    register(&f, 2, DEFENDER_EXPEDITION, SiegeSide::Defender);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
    disband_expedition(&f.server, DEFENDER_EXPEDITION);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 0);
}

#[tokio::test]
async fn an_already_held_reinforcement_seal_is_not_duplicated_or_revoked() {
    let f = fixture();
    // Skill 16975 (defence reinforcements) grants the same defence seal outside the roster.
    f.server.systems.skills.ext.items().try_add_new_item(
        &f.server,
        &f.defender,
        task_type::SKILL_EFFECT_GAIN_ITEM,
        DEFENSE_SEAL,
        1,
        0,
    );
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
    register(&f, 2, DEFENDER_EXPEDITION, SiegeSide::Defender);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
    cancel(&f);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.defender, DEFENSE_SEAL), 1);
}

#[tokio::test]
async fn an_offline_member_keeps_the_item_for_its_authored_lifetime_backstop() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    f.attacker.data().is_online = false;
    cancel(&f);
    reconcile(&f.server);
    // The 240-minute item lifetime is the only cleanup for an offline character.
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
}

#[tokio::test]
async fn the_grant_is_client_visible_as_an_item_task() {
    let mut f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    reconcile(&f.server);
    let frames = f.attacker_client.drain();
    assert!(
        frames
            .iter()
            .any(|(opcode, _)| *opcode == crate::opcodes::sc::SCItemTaskSuccessPacket),
        "the client receives the acquired seal"
    );
}

#[tokio::test]
async fn the_participant_sweep_runs_the_seal_sweep_and_rebuilds_the_snapshot_itself() {
    let f = fixture();
    register(&f, 1, ATTACKER_EXPEDITION, SiegeSide::Attacker);
    assert!(f.server.systems.housing.siege_areas.get(GROUP).is_none());
    runtime::participant_sweep(&f.server);
    assert_eq!(seal_count(&f.attacker, OFFENSE_SEAL), 1);
    // The hostility rule and the eviction read the snapshot, so it must not depend on the
    // seal module calling `refresh`.
    assert!(f.server.systems.housing.siege_areas.get(GROUP).is_some());
}
