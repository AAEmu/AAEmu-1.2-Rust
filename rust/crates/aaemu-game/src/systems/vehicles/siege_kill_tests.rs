//! KILL_SLAVE (kind 26): destroying an enemy-side siege catapult during an active siege
//! credits the killer; its own side and a kill outside a siege credit nothing.

use std::sync::Arc;

use super::do_die;
use crate::data::world_data::Zone;
use crate::data::GameData;
use crate::models::slave::Slave;
use crate::systems::housing::dominion::state::{SiegeSide, TowerIdentity};
use crate::systems::skills::types::KillReason;
use crate::systems::social::achievement::{
    char_record_kind, AchievementGameData, AchievementObjectives, Achievements, CharRecords,
};
use crate::systems::social::achievement_hooks::LiveAchievementHooks;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, SlaveRef, WorldObject};

const ZONE: u32 = 33;
const ATTACKER_EXPEDITION: u32 = 20;
const DEFENDER_EXPEDITION: u32 = 10;
/// The authored KILL_SLAVE row (`value1` 2 = siege catapult, `value2` 1 = enemy side).
const RECORD_KILL_SLAVE: u32 = 559;
const ACHIEVEMENT_KILL_SLAVE: u32 = 347;
const SIEGE_CATAPULT: u32 = 2;
const ATTACKER: u32 = 1;
const DEFENDER: u32 = 2;
const ATTACKER_OBJ: u32 = 0x1001;
const DEFENDER_OBJ: u32 = 0x1002;

struct Fixture {
    server: crate::server::GameServer,
    attacker: PlayerRef,
    defender: PlayerRef,
    catapult: SlaveRef,
    _clients: Vec<TestClient>,
}

fn achievements() -> AchievementGameData {
    let mut data = AchievementGameData::default();
    data.char_records.insert(
        RECORD_KILL_SLAVE,
        CharRecords {
            id: RECORD_KILL_SLAVE,
            kind_id: char_record_kind::KILL_SLAVE,
            value1: SIEGE_CATAPULT,
            value2: 1,
        },
    );
    let objective = AchievementObjectives {
        id: RECORD_KILL_SLAVE,
        achievement_id: ACHIEVEMENT_KILL_SLAVE,
        record_id: RECORD_KILL_SLAVE,
        ..Default::default()
    };
    data.achievements.insert(
        ACHIEVEMENT_KILL_SLAVE,
        Achievements {
            id: ACHIEVEMENT_KILL_SLAVE,
            complete_num: 1,
            complete_or: true,
            is_active: true,
            achievement_objectives: vec![objective],
            ..Default::default()
        },
    );
    data.achievement_objectives
        .insert(ACHIEVEMENT_KILL_SLAVE, vec![objective]);
    data
}

fn game_data() -> GameData {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            zone_key: TEST_ZONE,
            group_id: ZONE,
            ..Default::default()
        },
    );
    data.systems.social.achievements = achievements();
    data
}

/// Two online roster members (attacker 1, defender 2) and a siege catapult owned by `owner`.
fn fixture(siege: bool, owner: u32) -> Fixture {
    let server = test_server_with(game_data());
    server
        .systems
        .hooks
        .achievements
        .install(Arc::new(LiveAchievementHooks::default()));
    let mut clients = Vec::new();
    let mut players = Vec::new();
    for (id, obj_id, expedition) in [
        (ATTACKER, ATTACKER_OBJ, ATTACKER_EXPEDITION),
        (DEFENDER, DEFENDER_OBJ, DEFENDER_EXPEDITION),
    ] {
        let client = TestClient::new(&server, id, 1000 + id);
        let mut character = test_character(id, &format!("P{id}"), 100.0, 100.0);
        character.is_online = true;
        character.unit.hp = 500;
        character.unit.max_hp = 500;
        character.unit.expedition_id = expedition;
        players.push(spawn_player(&server, &client, character, obj_id));
        clients.push(client);
    }
    let attacker = players[0].clone();
    let defender = players[1].clone();
    if siege {
        let now = chrono::Utc::now().timestamp();
        let tower = TowerIdentity {
            house_id: 61,
            instance_id: 0,
            guard_tower_setting_id: 6,
        };
        let manager = &server.systems.housing.dominions;
        manager
            .declare_unowned_with(ZONE, tower, DEFENDER_EXPEDITION, now - 1000, || true)
            .unwrap();
        manager
            .schedule_siege(ZONE, ATTACKER_EXPEDITION, now - 500, now + 1000, now - 600)
            .unwrap();
        manager
            .register_participant(
                ZONE,
                ATTACKER,
                ATTACKER_EXPEDITION,
                SiegeSide::Attacker,
                now - 500,
            )
            .unwrap();
        manager
            .register_participant(
                ZONE,
                DEFENDER,
                DEFENDER_EXPEDITION,
                SiegeSide::Defender,
                now - 500,
            )
            .unwrap();
    }

    let mut slave = Slave::new(0, SIEGE_CATAPULT);
    slave.unit.hp = 100;
    slave.unit.max_hp = 100;
    slave.unit.template_id = SIEGE_CATAPULT;
    slave.unit.owner_id = owner;
    slave.summoner_id = owner;
    slave.summoner_obj_id = if owner == ATTACKER {
        ATTACKER_OBJ
    } else {
        DEFENDER_OBJ
    };
    slave.unit.transform.zone_id = TEST_ZONE;
    slave.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let catapult = Entity::new(0x2001, slave);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&catapult, &server.view_ctx());
    Fixture {
        server,
        attacker,
        defender,
        catapult,
        _clients: clients,
    }
}

fn killed_record(player: &PlayerRef) -> i64 {
    player
        .data()
        .achievement_state
        .record_value(RECORD_KILL_SLAVE)
}

fn kill_catapult(f: &Fixture, killer: &PlayerRef) {
    do_die(
        &f.server,
        &f.catapult,
        Some(&WorldObject::from(killer)),
        KillReason::Damage,
    );
}

#[tokio::test]
async fn an_enemy_side_catapult_credits_the_killer() {
    let f = fixture(true, DEFENDER);
    kill_catapult(&f, &f.attacker);

    assert_eq!(killed_record(&f.attacker), 1);
    assert_eq!(killed_record(&f.defender), 0);
    assert!(f
        .attacker
        .data()
        .achievement_state
        .is_complete(ACHIEVEMENT_KILL_SLAVE));
    assert!(!f
        .defender
        .data()
        .achievement_state
        .is_complete(ACHIEVEMENT_KILL_SLAVE));
}

#[tokio::test]
async fn the_own_side_catapult_credits_nobody() {
    let f = fixture(true, ATTACKER);
    kill_catapult(&f, &f.attacker);

    assert_eq!(killed_record(&f.attacker), 0);
    assert_eq!(killed_record(&f.defender), 0);
}

#[tokio::test]
async fn a_catapult_destroyed_outside_a_siege_credits_nobody() {
    let f = fixture(false, DEFENDER);
    kill_catapult(&f, &f.attacker);

    assert_eq!(killed_record(&f.attacker), 0);
    assert_eq!(killed_record(&f.defender), 0);
}
