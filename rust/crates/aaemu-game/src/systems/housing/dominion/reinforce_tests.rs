use super::*;
use crate::data::GameData;
use crate::systems::housing::data::{SiegeSettings, SiegeZone};
use crate::systems::housing::dominion::state::{DominionState, SiegeLimits, TowerIdentity};
use crate::test_support::test_server_with;

fn settings_row(total: u32, defenders: u32, reinforcements: u32) -> SiegeSettings {
    SiegeSettings {
        total_castles: total,
        num_defenders: defenders,
        num_reinforcements: reinforcements,
    }
}

/// The shipped `siege_settings` rows (0 -> 70/0, 1 -> 50/20, 2 -> 45/25, 3..=10 -> 40/30).
fn server() -> GameServer {
    let mut data = GameData::empty();
    for (total, defenders, reinforcements) in [
        (0, 70, 0),
        (1, 50, 20),
        (2, 45, 25),
        (3, 40, 30),
        (4, 40, 30),
        (5, 40, 30),
        (6, 40, 30),
        (7, 40, 30),
        (8, 40, 30),
        (9, 40, 30),
        (10, 40, 30),
    ] {
        data.systems
            .housing
            .castles
            .siege_settings
            .insert(total, settings_row(total, defenders, reinforcements));
    }
    data.systems.housing.castles.siege_zones.insert(
        1,
        SiegeZone {
            zone_group_id: 1,
            reinforce_defense_delay_mins: 20,
            ..Default::default()
        },
    );
    test_server_with(data)
}

fn owned(zone: u32, expedition: u32) -> DominionState {
    let mut state = DominionState::unowned(
        zone,
        TowerIdentity {
            house_id: 1000 + zone,
            instance_id: 0,
            guard_tower_setting_id: 1,
        },
    );
    state.owner_expedition_id = expedition;
    state.ownership_generation = 1;
    state.reign_start_at = 1_000;
    state.validate().unwrap();
    state
}

#[tokio::test]
async fn held_castle_count_selects_the_authored_settings_row() {
    let server = server();
    for (held, expect) in [
        (0, SiegeLimits { ni: 70, nr: 70 }),
        (1, SiegeLimits { ni: 50, nr: 70 }),
        (2, SiegeLimits { ni: 45, nr: 70 }),
        (3, SiegeLimits { ni: 40, nr: 70 }),
        (10, SiegeLimits { ni: 40, nr: 70 }),
        // More castles than the table has rows clamp to the last row.
        (11, SiegeLimits { ni: 40, nr: 70 }),
    ] {
        let states = (1..=held).map(|zone| owned(zone, 20)).collect();
        server.systems.housing.dominions.restore(states).unwrap();
        assert_eq!(limits(&server, 20), Some(expect), "{held} held castles");
    }
    // An expedition without a dominion still has the no-castle row; zero is not a real
    // defender and has no entitlement.
    server
        .systems
        .housing
        .dominions
        .restore(Vec::new())
        .unwrap();
    assert_eq!(limits(&server, 20), Some(SiegeLimits { ni: 70, nr: 70 }));
    assert_eq!(limits(&server, 0), None);

    // The GM `nation dominion` test hook is ephemeral but still the defender's held castle,
    // so the live path selects its row (1 -> 50 + 20).
    server
        .systems
        .housing
        .dominions
        .restore(vec![owned(1, 20)])
        .unwrap();
    server.systems.housing.dominions.mark_ephemeral(1);
    assert_eq!(limits(&server, 20), Some(SiegeLimits { ni: 50, nr: 70 }));
}

#[tokio::test]
async fn missing_or_oversized_settings_do_not_invent_native_capacity() {
    let server = server();
    server
        .systems
        .housing
        .dominions
        .restore(Vec::new())
        .unwrap();
    assert_eq!(limits(&server, 77), Some(SiegeLimits { ni: 70, nr: 70 }));

    let mut data = GameData::empty();
    data.systems
        .housing
        .castles
        .siege_settings
        .insert(0, settings_row(0, u32::MAX, u32::MAX));
    let server = test_server_with(data);
    assert_eq!(limits(&server, 20), Some(SiegeLimits { ni: 100, nr: 100 }));

    let server = test_server_with(GameData::empty());
    assert_eq!(limits(&server, 20), None);
}

#[tokio::test]
async fn bdm_is_the_authored_reinforce_delay_in_seconds() {
    let server = server();
    assert_eq!(reinforcement_delay(&server, 1), 1_200);
    assert_eq!(reinforcement_delay(&server, 2), 0);
}

#[tokio::test]
async fn the_defender_cap_waits_for_the_wave_and_the_attacker_uses_the_total() {
    let server = server();
    server
        .systems
        .housing
        .dominions
        .restore(vec![owned(1, 20)])
        .unwrap();
    let scheduled = server
        .systems
        .housing
        .dominions
        .schedule_siege_with_limits(1, 30, 2_000, 3_000, 1_500, SiegeLimits { ni: 50, nr: 70 })
        .unwrap();
    assert_eq!(
        participant_cap(&scheduled, SiegeSide::Defender),
        ParticipantCap {
            limit: 50,
            awaiting_reinforcements: true,
        }
    );
    assert_eq!(
        participant_cap(&scheduled, SiegeSide::Attacker),
        ParticipantCap {
            limit: 70,
            awaiting_reinforcements: false,
        }
    );
    let siege_id = scheduled.siege.as_ref().unwrap().siege_id;
    let reinforced = server
        .systems
        .housing
        .dominions
        .mark_reinforced(1, siege_id, 2_000, 2_000)
        .unwrap()
        .expect("the wave transition");
    assert!(reinforced.siege.as_ref().unwrap().reinforced);
    assert_eq!(
        server
            .systems
            .housing
            .dominions
            .mark_reinforced(1, siege_id, 2_000, 2_500)
            .unwrap(),
        None
    );
    assert_eq!(
        participant_cap(&reinforced, SiegeSide::Defender),
        ParticipantCap {
            limit: 70,
            awaiting_reinforcements: false,
        }
    );
}

#[tokio::test]
async fn a_missing_settings_row_keeps_the_native_cap_for_every_side() {
    let server = test_server_with(GameData::empty());
    server
        .systems
        .housing
        .dominions
        .restore(vec![owned(1, 20)])
        .unwrap();
    let scheduled = server
        .systems
        .housing
        .dominions
        .schedule_siege(1, 30, 2_000, 3_000, 1_500)
        .unwrap();
    for side in [SiegeSide::Defender, SiegeSide::Attacker] {
        assert_eq!(participant_cap(&scheduled, side), ParticipantCap::NATIVE);
    }
}

#[tokio::test]
async fn frozen_siege_limits_do_not_follow_the_live_castle_count() {
    let server = server();
    server
        .systems
        .housing
        .dominions
        .restore(vec![owned(1, 20)])
        .unwrap();
    server
        .systems
        .housing
        .dominions
        .schedule_siege_with_limits(1, 30, 2_000, 3_000, 1_500, SiegeLimits { ni: 50, nr: 70 })
        .unwrap();
    assert_eq!(limits(&server, 20), Some(SiegeLimits { ni: 50, nr: 70 }));

    // The defender claims a second castle mid-siege: the authored row becomes 45/25 (total
    // 70), but the running siege keeps the limits frozen at scheduling.
    server
        .systems
        .housing
        .dominions
        .declare_unowned_with(
            2,
            TowerIdentity {
                house_id: 1002,
                instance_id: 0,
                guard_tower_setting_id: 1,
            },
            20,
            1_500,
            || true,
        )
        .unwrap();
    assert_eq!(limits(&server, 20), Some(SiegeLimits { ni: 45, nr: 70 }));
    let state = server.systems.housing.dominions.get(1).unwrap();
    assert_eq!(participant_cap(&state, SiegeSide::Defender).limit, 50usize);
    assert_eq!(participant_cap(&state, SiegeSide::Attacker).limit, 70usize);
    assert!(state
        .siege
        .as_ref()
        .is_some_and(|siege| { siege.defender_ni == 50 && siege.defender_nr == 70 }));
}
