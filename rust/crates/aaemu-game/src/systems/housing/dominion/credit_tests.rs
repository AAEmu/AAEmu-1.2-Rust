//! The kinds 57-60 credit pass: capture and hold credit the two rosters, a cancel credits
//! nobody, and the persisted marker makes a second pass (restart) silent.

use std::sync::Arc;

use super::super::credit::credit_siege_outcome;
use super::super::state::{SiegeOutcome, SiegeSide, SiegeStatus};
use super::{
    tick_once, tower_identity, ENGRAVABLE_BUFF, ENGRAVING_BUFF, ENGRAVING_PLOT, ENGRAVING_SKILL,
};
use crate::data::world_data::{SystemFaction, Zone};
use crate::data::GameData;
use crate::models::house::House;
use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate};
use crate::systems::skills::data::{BuffTemplate, SkillTemplate};
use crate::systems::skills::effects::EffectSource;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::plots::PlotState;
use crate::systems::skills::skill::CastCtx;
use crate::systems::skills::state::{PlotStateRef, SkillRun};
use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
use crate::systems::social::achievement::{
    char_record_kind, AchievementGameData, AchievementObjectives, Achievements, CharRecords,
};
use crate::systems::social::achievement_hooks::LiveAchievementHooks;
use crate::systems::social::expedition::{member_from_character, Expedition};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, WorldObject};

const ZONE: u32 = 33;
const DEFENDER_EXPEDITION: u32 = 10;
const ATTACKER_EXPEDITION: u32 = 20;

/// The four authored zone-33 records (compact ids 525/533/541/549, `value1` = zone group 33).
const RECORD_WIN_ATTACK: u32 = 525;
const RECORD_WIN_DEFENCE: u32 = 533;
const RECORD_LOSE_ATTACK: u32 = 541;
const RECORD_LOSE_DEFENCE: u32 = 549;

const ATTACKERS: [u32; 3] = [1, 2, 3];
const DEFENDERS: [u32; 2] = [4, 5];

struct Fixture {
    server: crate::server::GameServer,
    players: Vec<PlayerRef>,
    house: crate::world::HouseRef,
}

fn achievements() -> AchievementGameData {
    let mut data = AchievementGameData::default();
    for (record_id, kind_id) in [
        (RECORD_WIN_ATTACK, char_record_kind::WIN_ATTACK_SIEGE),
        (RECORD_WIN_DEFENCE, char_record_kind::WIN_DEFENCE_SIEGE),
        (RECORD_LOSE_ATTACK, char_record_kind::LOSE_ATTACK_SIEGE),
        (RECORD_LOSE_DEFENCE, char_record_kind::LOSE_DEFENCE_SIEGE),
    ] {
        data.char_records.insert(
            record_id,
            CharRecords {
                id: record_id,
                kind_id,
                value1: ZONE,
                value2: 0,
            },
        );
        let objective = AchievementObjectives {
            id: record_id,
            achievement_id: kind_id,
            record_id,
            ..Default::default()
        };
        data.achievements.insert(
            kind_id,
            Achievements {
                id: kind_id,
                complete_num: 1,
                complete_or: true,
                is_active: true,
                achievement_objectives: vec![objective],
                ..Default::default()
            },
        );
        data.achievement_objectives.insert(kind_id, vec![objective]);
    }
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
    data.systems.housing.add_template(HousingTemplate {
        id: 139,
        guard_tower_setting_id: 6,
        hp: 1000,
        ..Default::default()
    });
    data.systems.housing.castles.guard_tower_settings.insert(
        6,
        GuardTowerSetting {
            id: 6,
            ..Default::default()
        },
    );
    data.systems.skills.skills.insert(
        ENGRAVING_SKILL,
        Arc::new(SkillTemplate {
            id: ENGRAVING_SKILL,
            plot_id: Some(ENGRAVING_PLOT),
            max_range: 30,
            ..Default::default()
        }),
    );
    for id in [ENGRAVABLE_BUFF, ENGRAVING_BUFF] {
        data.systems.skills.add_buff(BuffTemplate {
            id,
            max_stack: 1,
            ..Default::default()
        });
    }
    data.systems.social.achievements = achievements();
    data
}

/// Five online participants (3 attackers, 2 defenders) on one sieged territory. `ends_at` in
/// the past produces the deadline expiry (defenders held); in the future the engraving path can
/// capture.
fn fixture(ends_in_past: bool) -> Fixture {
    let server = test_server_with(game_data());
    server
        .systems
        .hooks
        .achievements
        .install(Arc::new(LiveAchievementHooks::default()));
    let mut players = Vec::new();
    for id in ATTACKERS.into_iter().chain(DEFENDERS) {
        let client = TestClient::new(&server, id, 1000 + id);
        let mut character = test_character(id, &format!("P{id}"), 100.0, 100.0);
        character.is_online = true;
        character.unit.hp = 500;
        character.unit.max_hp = 500;
        character.unit.expedition_id = if ATTACKERS.contains(&id) {
            ATTACKER_EXPEDITION
        } else {
            DEFENDER_EXPEDITION
        };
        let player = spawn_player(&server, &client, character, 1000 + id);
        players.push(player);
    }
    let member = member_from_character(&server, ATTACKER_EXPEDITION, &players[0], false);
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: ATTACKER_EXPEDITION,
            owner_id: 2,
            ..Default::default()
        },
        members: vec![member],
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });

    let mut house = House::new(61, 139);
    house.current_step = -1;
    house.unit.hp = 0;
    house.unit.max_hp = 1000;
    house.unit.transform.zone_id = TEST_ZONE;
    house
        .unit
        .transform
        .set_xyz(105.0, 100.0, players[0].data().unit.transform.z);
    let house = Entity::new(2001, house);
    server.systems.housing.manager.insert(&house);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&house, &server.view_ctx());
    let tower = WorldObject::from(&house);
    crate::systems::skills::buffs::add_buff_by_id(&server, &tower, ENGRAVABLE_BUFF, &tower, 0);

    let now = chrono::Utc::now().timestamp();
    let (_, identity) = tower_identity(&server, &house).unwrap();
    let manager = &server.systems.housing.dominions;
    manager
        .declare_unowned_with(ZONE, identity, DEFENDER_EXPEDITION, now - 1000, || true)
        .unwrap();
    let ends_at = if ends_in_past { now - 1 } else { now + 1000 };
    manager
        .schedule_siege(ZONE, ATTACKER_EXPEDITION, now - 500, ends_at, now - 600)
        .unwrap();
    for player in &players {
        let (expedition, side) = if ATTACKERS.contains(&player.character_id) {
            (ATTACKER_EXPEDITION, SiegeSide::Attacker)
        } else {
            (DEFENDER_EXPEDITION, SiegeSide::Defender)
        };
        manager
            .register_participant(ZONE, player.character_id, expedition, side, now - 500)
            .unwrap();
    }
    Fixture {
        server,
        players,
        house,
    }
}

fn value(f: &Fixture, index: usize, record_id: u32) -> i64 {
    f.players[index]
        .data()
        .achievement_state
        .record_value(record_id)
}

fn complete_capture(f: &Fixture) {
    let player = f.players[0].clone();
    let now = chrono::Utc::now().timestamp();
    let (_, identity) = tower_identity(&f.server, &f.house).unwrap();
    f.server
        .systems
        .housing
        .dominions
        .mark_tower_destroyed(ZONE, identity, now)
        .unwrap();
    let handle = f
        .server
        .systems
        .housing
        .dominions
        .begin_engraving(
            ZONE,
            identity,
            1,
            ATTACKER_EXPEDITION,
            crate::world::now_ms() - 300_001,
        )
        .unwrap();
    let skill = SkillRun::new(
        f.server
            .game_data
            .systems
            .skills
            .skill(ENGRAVING_SKILL)
            .unwrap()
            .clone(),
        1,
    );
    skill.state().tl_id = 99;
    let caster = WorldObject::from(&player);
    let plot = PlotState::new(
        skill.clone(),
        CastCtx {
            caster: caster.clone(),
            caster_caster: SkillCaster::Unit(player.obj_id),
            target: Target::Obj(WorldObject::from(&f.house)),
            target_caster: SkillCastTarget::Unit(f.house.obj_id),
            skill_object: SkillObject::None,
        },
    );
    skill.state().active_plot = Some(plot.clone());
    player.data().unit.skill_state.active_plot = Some(PlotStateRef(plot));
    let origin = f.server.world.world(0).unwrap().world_pos_rot(&caster);
    f.server
        .systems
        .housing
        .engraving_runtime
        .active
        .lock()
        .unwrap()
        .insert(
            player.character_id,
            super::ActiveEngraving {
                handle,
                skill: skill.clone(),
                generation: 0,
                caster,
                tower: f.house.clone(),
                origin,
            },
        );
    let object = SkillObject::None;
    super::complete_effect(
        &f.server,
        &crate::systems::skills::effects::EffectArgs {
            caster: Some(WorldObject::from(&player)),
            caster_caster: SkillCaster::Unit(player.obj_id),
            target: Target::Obj(WorldObject::from(&f.house)),
            target_caster: SkillCastTarget::Unit(f.house.obj_id),
            cast: CastAction::Plot {
                plot_id: ENGRAVING_PLOT,
                event_id: 5552,
                skill_id: ENGRAVING_SKILL,
                tl_id: 99,
            },
            source: EffectSource::from_skill(skill),
            skill_object: &object,
        },
    );
}

#[tokio::test]
async fn capture_credits_the_attackers_won_and_the_defenders_lost_once() {
    let f = fixture(false);
    complete_capture(&f);

    for (index, _) in ATTACKERS.iter().enumerate() {
        assert_eq!(value(&f, index, RECORD_WIN_ATTACK), 1, "attacker {index}");
        assert_eq!(value(&f, index, RECORD_LOSE_DEFENCE), 0, "attacker {index}");
        assert_eq!(value(&f, index, RECORD_WIN_DEFENCE), 0, "attacker {index}");
        assert_eq!(value(&f, index, RECORD_LOSE_ATTACK), 0, "attacker {index}");
    }
    for (offset, _) in DEFENDERS.iter().enumerate() {
        let index = ATTACKERS.len() + offset;
        assert_eq!(value(&f, index, RECORD_LOSE_DEFENCE), 1, "defender {index}");
        assert_eq!(value(&f, index, RECORD_WIN_ATTACK), 0, "defender {index}");
        assert_eq!(value(&f, index, RECORD_WIN_DEFENCE), 0, "defender {index}");
        assert_eq!(value(&f, index, RECORD_LOSE_ATTACK), 0, "defender {index}");
    }
    assert!(f.players.iter().all(|p| p
        .data()
        .achievement_state
        .is_complete(char_record_kind::WIN_ATTACK_SIEGE)
        || p.data()
            .achievement_state
            .is_complete(char_record_kind::LOSE_DEFENCE_SIEGE)));

    // A replay of the same finished state is a no-op (the marker is the check-and-set).
    let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
    assert!(!credit_siege_outcome(&f.server, &state));
    for (index, _) in ATTACKERS.iter().enumerate() {
        assert_eq!(value(&f, index, RECORD_WIN_ATTACK), 1, "attacker {index}");
    }
}

#[tokio::test]
async fn deadline_expiry_credits_the_defenders_won_and_the_attackers_lost() {
    let f = fixture(true);
    tick_once(&f.server);

    let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
    assert_eq!(
        state.siege.as_ref().unwrap().outcome,
        SiegeOutcome::DefenderHeld
    );
    for (index, _) in ATTACKERS.iter().enumerate() {
        assert_eq!(value(&f, index, RECORD_LOSE_ATTACK), 1, "attacker {index}");
        assert_eq!(value(&f, index, RECORD_WIN_ATTACK), 0, "attacker {index}");
    }
    for (offset, _) in DEFENDERS.iter().enumerate() {
        let index = ATTACKERS.len() + offset;
        assert_eq!(value(&f, index, RECORD_WIN_DEFENCE), 1, "defender {index}");
        assert_eq!(value(&f, index, RECORD_LOSE_DEFENCE), 0, "defender {index}");
    }
}

#[tokio::test]
async fn a_cancelled_siege_credits_nobody() {
    let f = fixture(true);
    let now = chrono::Utc::now().timestamp();
    let state = f
        .server
        .systems
        .housing
        .dominions
        .cancel_siege(ZONE, now)
        .unwrap();
    assert_eq!(
        state.siege.as_ref().unwrap().outcome,
        SiegeOutcome::Cancelled
    );

    tick_once(&f.server);
    assert!(!credit_siege_outcome(&f.server, &state));
    for index in 0..f.players.len() {
        for record_id in [
            RECORD_WIN_ATTACK,
            RECORD_WIN_DEFENCE,
            RECORD_LOSE_ATTACK,
            RECORD_LOSE_DEFENCE,
        ] {
            assert_eq!(
                value(&f, index, record_id),
                0,
                "player {index} record {record_id}"
            );
        }
    }
}

#[tokio::test]
async fn a_restart_keeps_the_credited_marker_and_does_not_credit_again() {
    let f = fixture(true);
    tick_once(&f.server);
    let state = f.server.systems.housing.dominions.get(ZONE).unwrap();
    assert!(state.siege.as_ref().unwrap().outcome_credited);

    // Restart: the authority is restored from the persisted snapshots (the marker included).
    let siege_id = state.siege.as_ref().unwrap().siege_id;
    let restarted = super::super::state::DominionManager::default();
    restarted
        .restore(f.server.systems.housing.dominions.snapshots())
        .unwrap();
    let restored = restarted.get(ZONE).unwrap();
    assert!(restored.siege.as_ref().unwrap().outcome_credited);
    assert_eq!(
        restored.siege.as_ref().unwrap().status,
        SiegeStatus::Finished
    );
    // The restored marker refuses a second check-and-set for the same siege id.
    assert_eq!(restarted.mark_outcome_credited(ZONE, siege_id), Ok(None));
    for index in 0..ATTACKERS.len() {
        assert_eq!(value(&f, index, RECORD_LOSE_ATTACK), 1, "attacker {index}");
    }
    for offset in 0..DEFENDERS.len() {
        assert_eq!(
            value(&f, ATTACKERS.len() + offset, RECORD_WIN_DEFENCE),
            1,
            "defender {offset}"
        );
    }
}

#[tokio::test]
async fn an_offline_participant_targets_the_authored_zone_record() {
    let f = fixture(true);
    let matcher = crate::systems::social::achievement_hooks::RecordMatch::Value1(ZONE);
    let ids = crate::systems::social::achievement_hooks::increment_record_ids(
        &f.server,
        char_record_kind::LOSE_ATTACK_SIEGE,
        matcher,
    );
    assert_eq!(ids, [RECORD_LOSE_ATTACK]);
}
