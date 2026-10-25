//! Authored 1.2 breaker relations. IDs/tags below are pinned again by the ignored compact
//! regression. No packet round trips: BuffRemoved's native reader is 397C6360, handler
//! 391D5E90 (x2game12.c:1763754-1763769 / 382144-382148): 3-byte owner, u32 buff index.
//! Relation loaders: native 1.2 395D17B0 (x2game12.c:1256462-1256496), 10.x 39B41DE0
//! (x2game-dev_dedicate.c:2323838-2323904). Those prove ownership/direction only. Traced
//! 10.x ZoneBuffMan::Create 39450740 and its tick 3944FC90 do not establish a breaker-set
//! consumer; immediate reconciliation below is an explicit implementation choice pending
//! native timing evidence, not a claim of native timing parity.

use std::sync::Arc;
use std::time::Duration;

use crate::data::GameData;
use crate::models::npc::Npc;
use crate::models::unit::{BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{now_ms, Entity, WorldObject};

use super::super::controllers::{self, ScState};
use super::super::data::effects::BuffEffect;
use super::super::data::{
    BuffTemplate, BuffTolerance, EffectKind, EffectTemplate, SkillTemplate, TickEffect,
};
use super::super::state::{CancelToken, SkillRun, SkillTaskKind, SkillTaskSlot, ToleranceCounter};
use super::super::types::SkillCaster;

const SONGS: [u32; 4] = [656, 662, 667, 778];
const GLIDER: u32 = 1029;
const STUN: u32 = 93;
const SLEEP: u32 = 84; // Leech: Sleep tag 97, despite its stun flag in the export.
const SILENCE: u32 = 245;
const IRRELEVANT: u32 = 60_001;
const IMMUNITY: u32 = 60_002;

fn data() -> GameData {
    let mut data = GameData::empty();
    let d = &mut data.systems.skills;
    for id in SONGS {
        d.add_buff(BuffTemplate {
            id,
            kind: 3,
            duration: 15_000,
            tick: 500.0,
            stack_rule: 1,
            max_stack: 1,
            bonuses: vec![BonusTemplate {
                attribute: UnitAttribute::Str,
                modifier_type: UnitModifierType::Value,
                value: 100,
                linear_level_bonus: 0,
            }],
            ..BuffTemplate::default()
        });
        d.buff_breakers
            .insert(id, vec![6, 12, 13, 97, 99, 107, 150, 346]);
    }
    d.add_buff(BuffTemplate {
        id: GLIDER,
        kind: 1,
        duration: 180_000,
        gliding: true,
        stack_rule: 1,
        max_stack: 1,
        ..BuffTemplate::default()
    });
    d.buff_breakers
        .insert(GLIDER, vec![6, 12, 13, 27, 97, 99, 107, 150, 346]);
    d.buff_tags.insert(GLIDER, vec![10, 294]);
    for (id, duration) in [(431, 15_000), (2170, 10_000)] {
        d.add_buff(BuffTemplate {
            id,
            kind: 1,
            duration,
            ..BuffTemplate::default()
        });
        d.buff_breakers.insert(id, vec![294]);
    }
    for (id, tag, duration, stun, silence) in [
        (STUN, 6, 4000, true, false),
        (SLEEP, 97, 5000, true, false),
        (SILENCE, 99, 3500, false, true),
        (IRRELEVANT, 60_000, 10_000, true, false),
    ] {
        d.add_buff(BuffTemplate {
            id,
            kind: 2,
            duration,
            stun,
            silence,
            stack_rule: 1,
            max_stack: 1,
            ..BuffTemplate::default()
        });
        d.buff_tags.insert(id, vec![tag]);
        d.tagged_buffs.insert(tag, vec![id]);
    }
    data
}

fn fixture(data: GameData) -> (Arc<GameServer>, TestClient, WorldObject) {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let owner = spawn_player(
        &server,
        &client,
        test_character(1, "Bard", 100.0, 100.0),
        0x300,
    );
    // Visibility serialization can add its synthetic state buffs. Start this fixture's
    // bookkeeping at index 2 so the native removal-body expectations remain literal.
    owner.data().unit.buffs = crate::models::buff::Buffs::default();
    (server, client, WorldObject::from(&owner))
}

fn add(server: &GameServer, owner: &WorldObject, id: u32) {
    super::add_buff_by_id(server, owner, id, owner, 0);
}

fn has(owner: &WorldObject, id: u32) -> bool {
    owner.with_base_unit(|u| u.buffs().check_buff(id))
}

fn removed(client: &mut TestClient) -> Vec<Vec<u8>> {
    client
        .drain()
        .into_iter()
        .filter_map(|(op, body)| (op == sc::SCBuffRemovedPacket).then_some(body))
        .collect()
}

#[tokio::test]
async fn buff_breakers_bard_songs_end_on_real_stun_sleep_and_silence_tags() {
    for cc in [STUN, SLEEP, SILENCE] {
        for song in SONGS {
            let (server, mut client, owner) = fixture(data());
            add(&server, &owner, song);
            let song_buff = owner.with_base_unit(|u| u.buffs().effects[0].clone());
            client.drain();
            add(&server, &owner, cc);
            assert!(!has(&owner, song), "song {song}, CC {cc}");
            assert!(has(&owner, cc));
            assert_eq!(removed(&mut client), vec![vec![0, 3, 0, 2, 0, 0, 0]]);
            assert_eq!(
                owner.with_unit(|u| u
                    .unit()
                    .bonuses
                    .calculate(0.0, UnitAttribute::Str, now_ms())),
                Some(0.0)
            );
            // A queued song timer may no longer tick, resurrect, or remove twice.
            super::dispel_task(&server, owner.obj_id(), song_buff.index, song_buff.task_gen);
            assert!(removed(&mut client).is_empty());
            assert!(!has(&owner, song));
        }
    }
}

#[tokio::test]
async fn buff_breakers_glider_controller_stops_through_normal_dispel() {
    let (server, _client, _player) = fixture(data());
    let mut npc = Npc::new(1);
    npc.unit.hp = 100;
    npc.unit.max_hp = 100;
    npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let npc = Entity::new(0x400, npc);
    let owner = WorldObject::from(&npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&owner, &server.view_ctx());
    add(&server, &owner, GLIDER);
    let controller = controllers::active(&owner).unwrap();
    assert_eq!(controller.source_buff_id(), GLIDER);
    assert_eq!(controller.state(), ScState::Running);
    add(&server, &owner, SILENCE);
    assert!(!has(&owner, GLIDER));
    assert_eq!(controller.state(), ScState::Ended);
    assert!(controllers::active(&owner).is_none());
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert_eq!(npc.data().unit.transform.z, 100.0);
}

#[tokio::test]
async fn buff_breakers_ignore_irrelevant_tags_and_rejected_incoming_buffs() {
    let mut d = data();
    d.systems.skills.add_buff(BuffTemplate {
        id: IMMUNITY,
        kind: 1,
        immune_buff_tag_id: 99,
        ..BuffTemplate::default()
    });
    let (server, mut client, owner) = fixture(d);
    add(&server, &owner, SONGS[0]);
    add(&server, &owner, IRRELEVANT);
    assert!(has(&owner, SONGS[0]), "CC flags alone are not breaker tags");
    add(&server, &owner, IMMUNITY);
    client.drain();
    let silence = server.game_data.systems.skills.buff(SILENCE).unwrap();
    super::apply_template(
        &server,
        silence,
        Some(&owner),
        &SkillCaster::Unit(owner.obj_id()),
        &owner,
        None,
    );
    assert!(has(&owner, SONGS[0]));
    assert!(!has(&owner, SILENCE));
    assert!(removed(&mut client).is_empty());

    let mut d = data();
    Arc::make_mut(d.systems.skills.buffs.get_mut(&SILENCE).unwrap()).require_buff_id = 60_004;
    let (server, mut client, owner) = fixture(d);
    add(&server, &owner, SONGS[0]);
    client.drain();
    let rejected = server.game_data.systems.skills.buff(SILENCE).unwrap();
    super::apply_template(
        &server,
        rejected,
        Some(&owner),
        &SkillCaster::Unit(owner.obj_id()),
        &owner,
        None,
    );
    assert!(has(&owner, SONGS[0]));
    assert!(!has(&owner, SILENCE));
    assert!(removed(&mut client).is_empty());
}

#[tokio::test]
async fn buff_breakers_tolerance_rejection_does_not_end_song() {
    let mut d = data();
    d.systems.skills.buff_game_data.tolerances.insert(
        6,
        BuffTolerance {
            id: 1,
            buff_tag_id: 6,
            step_duration: 60,
            final_step_buff_id: IMMUNITY,
            character_time_reduction: 0,
            steps: vec![(1, 100, 0)],
        },
    );
    let (server, mut client, owner) = fixture(d);
    owner.with_unit(|u| {
        u.unit_mut().skill_state.tolerance_counters.insert(
            1,
            ToleranceCounter {
                tolerance_id: 1,
                current_step: (1, 100, 0),
                last_step_ms: now_ms(),
            },
        );
    });
    add(&server, &owner, SONGS[0]);
    client.drain();
    add(&server, &owner, STUN); // No next tolerance step: existing rejection path.
    assert!(!has(&owner, STUN));
    assert!(has(&owner, SONGS[0]));
    assert!(removed(&mut client).is_empty());
}

#[tokio::test]
async fn buff_breakers_cancel_only_the_corresponding_channel_without_reentering_removal() {
    let mut d = data();
    // Snare tag 27 is authored for gliders, without the generic stun/silence interrupt.
    Arc::make_mut(d.systems.skills.buffs.get_mut(&IRRELEVANT).unwrap()).stun = false;
    d.systems.skills.buff_tags.insert(IRRELEVANT, vec![27]);
    let (server, mut client, owner) = fixture(d);
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            id: 60_010,
            channeling_buff_id: GLIDER,
            channeling_time: 10_000,
            ..SkillTemplate::default()
        }),
        1,
    );
    skill.state().tl_id = server
        .systems
        .skills
        .tl_ids
        .next_id(owner.obj_id(), skill.template.id);
    let cancel = CancelToken::new();
    owner.with_unit(|u| {
        u.unit_mut().skill_state.skill_task = Some(SkillTaskSlot {
            kind: SkillTaskKind::EndChanneling { channel_doodad: 0 },
            skill: skill.clone(),
            cancel: cancel.clone(),
        });
    });
    let template = server.game_data.systems.skills.buff(GLIDER).unwrap();
    let buff = super::new_buff(
        &server,
        template,
        Some(&owner),
        &SkillCaster::Unit(owner.obj_id()),
        Some(&skill),
        now_ms(),
    );
    super::add_buff(&server, &owner, buff, 0, 0);
    client.drain();
    add(&server, &owner, IRRELEVANT);
    assert!(!has(&owner, GLIDER));
    assert!(cancel.is_cancelled());
    assert!(skill.cancelled());
    assert!(owner
        .with_unit(|u| u.unit().skill_state.skill_task.is_none())
        .unwrap());
    assert_eq!(server.systems.skills.tl_ids.in_use(), 0);
    assert_eq!(removed(&mut client), vec![vec![0, 3, 0, 2, 0, 0, 0]]);
}

#[tokio::test]
async fn buff_breakers_timer_applied_cc_uses_the_same_acceptance_path() {
    let mut d = data();
    d.systems.skills.add_buff(BuffTemplate {
        id: 60_020,
        kind: 3,
        duration: 10_000,
        tick: 10.0,
        tick_effects: vec![TickEffect {
            effect_id: 60_021,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
        }],
        ..BuffTemplate::default()
    });
    d.systems.skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: 60_021,
            buff_id: Some(SILENCE),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        60_021,
    );
    let (server, mut client, owner) = fixture(d);
    add(&server, &owner, SONGS[0]);
    add(&server, &owner, 60_020);
    client.drain();
    for _ in 0..40 {
        if !has(&owner, SONGS[0]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(has(&owner, SILENCE));
    assert!(!has(&owner, SONGS[0]));
    assert_eq!(removed(&mut client), vec![vec![0, 3, 0, 2, 0, 0, 0]]);
}

#[tokio::test]
async fn buff_breakers_multiple_tags_remove_each_song_once_in_index_order() {
    let mut d = data();
    d.systems.skills.buff_tags.insert(STUN, vec![99, 6, 97, 6]);
    let (server, mut client, owner) = fixture(d);
    for song in SONGS {
        add(&server, &owner, song);
    }
    client.drain();
    add(&server, &owner, STUN);
    for song in SONGS {
        assert!(!has(&owner, song));
    }
    assert_eq!(
        removed(&mut client),
        vec![
            vec![0, 3, 0, 2, 0, 0, 0],
            vec![0, 3, 0, 3, 0, 0, 0],
            vec![0, 3, 0, 4, 0, 0, 0],
            vec![0, 3, 0, 5, 0, 0, 0],
        ]
    );
}

#[tokio::test]
async fn buff_breakers_mutual_batch_removal_does_not_start_a_stale_buff_event() {
    let mut d = data();
    for id in [SONGS[0], GLIDER] {
        d.systems.skills.buff_tags.insert(id, vec![6]);
    }
    let (server, mut client, owner) = fixture(d);
    for id in [SONGS[0], GLIDER] {
        let template = server.game_data.systems.skills.buff(id).unwrap();
        let mut buff = super::new_buff(
            &server,
            template,
            Some(&owner),
            &SkillCaster::Unit(owner.obj_id()),
            None,
            now_ms(),
        );
        buff.duration = template.duration;
        owner.with_base_unit(|u| u.buffs_mut().add_buff_with_duration(buff, 0, now_ms()));
    }
    client.drain();
    crate::systems::dispatch_buff_events(&server, &owner);
    assert!(!has(&owner, SONGS[0]));
    assert!(!has(&owner, GLIDER));
    assert_eq!(
        removed(&mut client),
        vec![vec![0, 3, 0, 2, 0, 0, 0], vec![0, 3, 0, 3, 0, 0, 0]]
    );
    assert!(controllers::active(&owner).is_none());
}

#[tokio::test]
async fn buff_breakers_existing_cc_prevents_restarting_a_song() {
    let (server, _client, owner) = fixture(data());
    add(&server, &owner, SILENCE);
    add(&server, &owner, SONGS[0]);
    assert!(has(&owner, SILENCE));
    assert!(!has(&owner, SONGS[0]));
    assert_eq!(
        owner.with_unit(|u| u
            .unit()
            .bonuses
            .calculate(0.0, UnitAttribute::Str, now_ms())),
        Some(0.0)
    );
}

#[tokio::test]
async fn buff_breakers_gliding_tag_ends_frigid_tracks_and_deadeye() {
    let (server, mut client, owner) = fixture(data());
    add(&server, &owner, 431);
    add(&server, &owner, 2170);
    client.drain();
    add(&server, &owner, GLIDER);
    assert!(!has(&owner, 431));
    assert!(!has(&owner, 2170));
    assert!(has(&owner, GLIDER));
    assert_eq!(
        removed(&mut client),
        vec![vec![0, 3, 0, 2, 0, 0, 0], vec![0, 3, 0, 3, 0, 0, 0]]
    );
}

#[test]
fn buff_breakers_loader_preserves_direction_and_deduplicates_tags() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE buff_breakers (buff_id, buff_tag_id); INSERT INTO buff_breakers VALUES (656,99),(656,6),(656,6),(1029,27);").unwrap();
    let breakers = super::super::data::load_buff_breakers(&conn).unwrap();
    assert_eq!(breakers[&656], vec![6, 99]);
    assert_eq!(breakers[&1029], vec![27]);
    assert!(!breakers.contains_key(&6));
}

#[test]
#[ignore = "requires AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn buff_breakers_real_compact_db_song_glider_and_cc_relations() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let breakers = super::super::data::load_buff_breakers(&conn).unwrap();
    assert_eq!(breakers.len(), 92);
    assert_eq!(breakers.values().map(Vec::len).sum::<usize>(), 661);
    for song in SONGS {
        assert_eq!(breakers[&song], vec![6, 12, 13, 97, 99, 107, 150, 346]);
    }
    assert_eq!(
        breakers[&GLIDER],
        vec![6, 12, 13, 27, 97, 99, 107, 150, 346]
    );
    for (id, tag) in [(STUN, 6), (SLEEP, 97), (SILENCE, 99)] {
        let count: i32 = conn
            .query_row(
                "SELECT count(*) FROM tagged_buffs WHERE buff_id=?1 AND tag_id=?2",
                [id, tag],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    // Frigid Tracks and Deadeye break on Gliding, rather than arbitrary CC flags.
    assert_eq!(breakers[&431], vec![294]);
    assert_eq!(breakers[&2170], vec![294]);
}
