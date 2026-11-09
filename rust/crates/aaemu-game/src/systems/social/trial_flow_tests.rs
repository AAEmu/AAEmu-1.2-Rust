//! Crime / trial flow tests (several connections, C->S packets through `handle_frame`).

use std::sync::{Arc, Mutex};

use aaemu_commons::PacketStream;

use crate::data::character_templates::CharacterTemplate;
use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_data::SystemFaction;
use crate::data::GameData;
use crate::models::doodad::{doodad_owner_type, Doodad};
use crate::net::packet::{encode_raw, LEVEL_PLAIN};
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::social::config::{CourtRoomConfig, JailConfig, JusticeConfig, SpawnPosition};
use crate::systems::social::crime::{buff, crime_kind, CrimeEvent};
use crate::systems::social::hooks::{EvidenceFunc, SocialHooks};
use crate::systems::social::packets::body;
use crate::systems::social::packets::trial::*;
use crate::systems::social::trial::{self, sentence_result, trial_step};
use crate::systems::social::util;
use crate::test_support::*;
use crate::world::{DoodadRef, Entity, PlayerRef};

const NUIA: u32 = 148;
const HARANYA: u32 = 149;
const JAIL_POS: SpawnPosition = SpawnPosition {
    world_id: 0,
    zone_id: TEST_ZONE,
    x: 300.0,
    y: 310.0,
    z: 50.0,
    yaw: 90.0,
};
const HOLDING_CELL: SpawnPosition = SpawnPosition {
    world_id: 0,
    zone_id: TEST_ZONE,
    x: 200.0,
    y: 210.0,
    z: 60.0,
    yaw: 180.0,
};
const DEFENDANT_POS: SpawnPosition = SpawnPosition {
    world_id: 0,
    zone_id: TEST_ZONE,
    x: 150.0,
    y: 160.0,
    z: 70.0,
    yaw: -28.0,
};

/// A test server with the trial buffs, the Nuia / Haranya factions, one Nuian court room
/// (id 1) and one Nuian jail.
fn justice_server() -> Arc<GameServer> {
    justice_server_with(|_| {})
}

fn justice_server_with(configure: impl FnOnce(&mut JusticeConfig)) -> Arc<GameServer> {
    let mut data = GameData::empty();
    for id in [
        buff::PRISONER_NUIAN,
        buff::PRISONER_HARANYAN,
        buff::FORCIBLY_AWAITING_TRIAL,
        buff::JURY,
        buff::TRIAL_DEFENDANT,
        buff::WANTED,
        4832,
        buff::SUSPECTED_USER,
        buff::TRANSFORMING_INTO_PRIME_SUSPECT,
        buff::COURT_HOUSE,
        buff::CANNOT_ESCAPE,
    ] {
        let mut t = BuffTemplate::simple(id, BuffKind::Good);
        t.duration = 1000;
        data.skills.buffs.insert(id, t);
    }
    for (id, mother) in [
        (101, NUIA),
        (102, HARANYA),
        (NUIA, 0),
        (HARANYA, 0),
        (161, 0),
    ] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..SystemFaction::default()
        });
    }
    data.characters.templates.insert(
        0x11,
        CharacterTemplate {
            race: 1,
            gender: 1,
            creatable: true,
            faction_id: 101,
            ..Default::default()
        },
    );
    let s = test_server_with(data);
    let mut justice = JusticeConfig {
        court_rooms: vec![CourtRoomConfig {
            id: 1,
            name: "Nuian Main Court".into(),
            faction: NUIA,
            defendant: DEFENDANT_POS,
            holding_cell: HOLDING_CELL,
        }],
        jails: vec![JailConfig {
            id: 1,
            name: "Nuian Jail".into(),
            faction: NUIA,
            pos: JAIL_POS,
        }],
        ..JusticeConfig::default()
    };
    configure(&mut justice);
    s.config.update(|c| c.justice = justice);
    let s = Arc::new(s);
    s.set_self();
    s.systems.social.trials.load(&s);
    s
}

struct Pc {
    client: TestClient,
    p: PlayerRef,
}

fn player(
    server: &GameServer,
    id: u32,
    name: &str,
    f: impl FnOnce(&mut crate::models::character::Character),
) -> Pc {
    let mut client = TestClient::new(server, id, 1000 + id);
    let mut c = test_character(id, name, 100.0 + id as f32, 100.0);
    c.is_online = true;
    f(&mut c);
    let p = spawn_player(server, &client, c, 0x100 + id);
    client.conn.active_char = Some(p.clone());
    server.names.add_character(id, name, 1000 + id);
    client.drain();
    Pc { client, p }
}

async fn send(server: &GameServer, pc: &mut Pc, opcode: u16, f: impl FnOnce(&mut PacketStream)) {
    let mut s = PacketStream::new();
    f(&mut s);
    server
        .handle_frame(
            &mut pc.client.conn,
            &encode_raw(LEVEL_PLAIN, opcode, &s.into_bytes()),
        )
        .await
        .expect("handled");
}

fn ops(frames: &[(u16, Vec<u8>)]) -> Vec<u16> {
    frames.iter().map(|f| f.0).collect()
}

// Justice teleports also replicate entering/leaving regions. Check the trial protocol here;
// server position and region membership have dedicated regressions below.
fn trial_frames(client: &mut TestClient) -> Vec<(u16, Vec<u8>)> {
    let mut arrest_points = false;
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| {
            if *op == sc::SCCharacterResurrectedPacket {
                arrest_points = true;
            }
            if *op == sc::SCUnitPointsPacket {
                // Arrest explicitly sends points after resurrecting. Region replication
                // additionally sends nearby characters' points when a teleport arrives.
                return std::mem::take(&mut arrest_points);
            }
            !matches!(
                *op,
                sc::SCUnitsRemovedPacket
                    | sc::SCUnitStatePacket
                    | sc::SCCharacterStatePacket
                    | sc::SCTargetChangedPacket
                    | sc::SCDoodadsCreatedPacket
                    | sc::SCDoodadsRemovedPacket
                    | sc::SCGimmicksCreatedPacket
                    | sc::SCGimmicksRemovedPacket
            )
        })
        .collect()
}

/// Waits for a spawned faction-change side effect (the guild removal of
/// `set_faction_persistent` runs on the runtime; a test must let it finish before checking).
async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

fn find(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|f| f.0 == op)
        .map(|f| f.1.clone())
        .collect()
}

fn seat(id: u32, x: f32) -> DoodadRef {
    let mut d = Doodad::new(4937);
    d.transform.set_position(x, 500.0, 20.0, 0.0, 0.0, 1.25);
    Entity::new(id, d)
}

fn murder(id: u32, criminal: u32, victim: u32) -> CrimeEvent {
    CrimeEvent {
        id,
        criminal,
        victim,
        reporter: victim,
        crime_kind: crime_kind::MURDER,
        position: (10.0, 20.0, 30.0),
        report_time: 1_700_000_000,
        msg: "bloodstain".into(),
        ..CrimeEvent::default()
    }
}

fn state_bytes(trial_id: u32, state: u8, cur_jury: i32) -> Vec<u8> {
    // The remaining time is checked separately.
    let mut b = body(&SCChangeTrialStatePacket {
        trial_id,
        state,
        cur_jury,
        remain_time: 0,
    });
    b.truncate(9);
    b
}

fn states(frames: &[(u16, Vec<u8>)]) -> Vec<(u8, i32)> {
    find(frames, sc::SCChangeTrialStatePacket)
        .iter()
        .map(|b| (b[4], i32::from_le_bytes([b[5], b[6], b[7], b[8]])))
        .collect()
}

/// Everything `CSJurySummoned` must leave untouched.
type SeatState = (
    u8,
    i64,
    Option<u32>,
    u32,
    Vec<(i32, Option<u32>, bool, i32)>,
);

fn trial_seat_state(server: &GameServer, tid: u32) -> SeatState {
    let t = server.systems.social.trials.get_trial(tid).expect("trial");
    (
        t.step,
        t.current_step_end_ms,
        t.court_room,
        t.defendant_id,
        t.jury
            .iter()
            .map(|(seat, j)| {
                (
                    *seat,
                    j.jury_member.as_ref().map(|m| m.character_id),
                    j.confirm_testimony,
                    j.selected_sentence,
                )
            })
            .collect(),
    )
}

#[tokio::test]
async fn jury_trial_flow_to_guilty_verdict_and_cleanup() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    let (s1, s2) = (seat(0x900, 400.0), seat(0x901, 402.0));
    trials.initialize_doodads(&[
        ("justice.1.seat.1".into(), s1.clone()),
        ("justice.1.seat.2".into(), s2.clone()),
        ("justice.2.seat.1".into(), seat(0x902, 0.0)),
    ]);
    trials.initialize_npc(&[("justice.1.judge".into(), 55)]);
    assert_eq!(trials.court_rooms()[0].jury_seats.len(), 2);
    assert_eq!(trials.court_rooms()[0].judge_spawner, Some(55));

    let mut d = player(&server, 1, "Crook", |c| {
        c.crime_point = 60;
        c.infamy_point = 60;
    });
    let mut j1 = player(&server, 2, "Juror1", |c| c.jury_point = 1);
    let mut j2 = player(&server, 3, "Juror2", |c| c.jury_point = 1);
    let mut arrestor = player(&server, 4, "Guard", |_| {});
    for pc in [&mut d, &mut j1, &mut j2, &mut arrestor] {
        trial_frames(&mut pc.client);
    }
    // Murder of a level-10 victim: 20 * 10 minutes.
    assert!(server.systems.social.crimes.insert(murder(50, 1, 2)));

    // Jury queue: eligible players of the mother faction (the criminal and the guard are not).
    // (`GetAllCharacters` order is unspecified, as in C#.)
    trials.update_jury_queues(&server);
    let mut queue = trials.jury_queue(NUIA);
    let pos = queue.iter().position(|x| *x == 3).unwrap() as u8 + 1;
    queue.sort_unstable();
    assert_eq!(queue, vec![2, 3]);
    send(
        &server,
        &mut j2,
        cs::CSRequestJuryWaitingNumberPacket,
        |_| {},
    )
    .await;
    assert_eq!(
        find(&trial_frames(&mut j2.client), sc::SCJuryWaitingNumberPacket),
        vec![vec![pos, 0, 0, 0]]
    );

    // Arrest.
    let tid = trials
        .arrest_criminal(&server, &d.p, Some(&arrestor.p))
        .expect("trial");
    let f = trial_frames(&mut d.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCCriminalArrestedPacket,
            sc::SCCharacterResurrectedPacket,
            sc::SCUnitPointsPacket,
            sc::SCTeleportUnitPacket,
            sc::SCAskImprisonOrTrialPacket,
            sc::SCTrialWaitStatusPacket,
        ]
    );
    assert_eq!(
        f[0].1,
        body(&SCCriminalArrestedPacket {
            criminal_obj_id: 0x101,
            criminal: "Crook".into(),
            arrestor: "Guard".into()
        })
    );
    assert_eq!(
        f[3].1,
        body(&SCTeleportUnitPacket {
            reason: teleport_reason::LOCKUP,
            error_message: 0,
            x: 200.0,
            y: 210.0,
            z: 60.0,
            z2: std::f32::consts::PI / 180.0 * 180.0,
        })
    );
    assert_eq!(
        f[4].1,
        body(&SCAskImprisonOrTrialPacket {
            crime_value: 60,
            jail_minutes: 200
        })
    );
    assert_eq!(
        f[5].1,
        body(&SCTrialWaitStatusPacket {
            order: 0,
            sentence_time_ms: 200 * 60_000
        })
    );
    // Neighbours see the arrest.
    assert_eq!(
        find(
            &trial_frames(&mut arrestor.client),
            sc::SCCriminalArrestedPacket
        )
        .len(),
        1
    );
    {
        let c = d.p.data();
        assert_eq!(c.arrest_count, 1);
        // CreateTrialCase runs CalculateJailTime before EnterCourtJail: region = Nuian (0).
        assert_eq!((c.offline_guilty_time, c.offline_guilty_region), (200, 0));
    }
    assert!(trials.is_player_in_court(1));

    // The defendant asks for a trial; the next tick moves the case into the court room.
    send(&server, &mut d, cs::CSReplyImprisonOrTrialPacket, |s| {
        s.write_bool(true);
    })
    .await;
    assert_eq!(d.p.data().accept_trial_count, 1);
    let t0 = util::now_ms() + 1;
    trials.update_tick(&server, t0).await;
    let f = trial_frames(&mut d.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCTeleportUnitPacket,
            sc::SCSummonDefendantPacket,
            sc::SCJoinedChatChannelPacket,
            sc::SCJuryWaitStatusPacket,
            sc::SCChangeTrialStatePacket,
            sc::SCTrialInfoPacket,
            sc::SCCrimeRecordsPacket,
            sc::SCCrimeDataPacket,
            sc::SCChangeTrialStatePacket,
            sc::SCForceAttackSetPacket,
        ]
    );
    assert_eq!(
        f[3].1,
        body(&SCJuryWaitStatusPacket {
            count: 0,
            total: 2,
            sentence_time_ms: 200 * 60_000
        })
    );
    assert_eq!(
        f[4].1,
        body(&SCChangeTrialStatePacket {
            trial_id: tid,
            state: trial_step::VERIFY_CRIMINAL_RECORD,
            cur_jury: 0,
            remain_time: 10_000
        })
    );
    assert_eq!(
        f[8].1,
        body(&SCChangeTrialStatePacket {
            trial_id: tid,
            state: trial_step::AWAITING_JURY_SUMMONS,
            cur_jury: 0,
            remain_time: 120_000
        })
    );
    // The crime record carries the online victim's values and the reporter's name.
    let entry = server
        .systems
        .social
        .crimes
        .record_entry(&server, &murder(50, 1, 2));
    assert_eq!(entry.victim_name, "Juror1");
    assert_eq!(entry.victim_faction, 101);
    assert_eq!(entry.reporter_name.as_deref(), Some("Juror1"));
    assert_eq!(
        f[6].1,
        body(&SCCrimeRecordsPacket {
            trial_id: tid,
            unknown1: 0,
            total_count: 1,
            this_count: 1,
            entries: vec![entry]
        })
    );
    // The judge spawner is registered but no npcs hook resolves it: obj id 0.
    assert_eq!(
        f[7].1,
        body(&SCCrimeDataPacket {
            defendant_id: 1,
            defendant_name: "Crook".into(),
            defendant_race: 1,
            defendant_faction: 101,
            trial_id: tid,
            sentence_time_ms: 200 * 60_000,
            judge_id: 0
        })
    );
    assert!(d.p.data().unit.buffs.check_buff(buff::TRIAL_DEFENDANT));
    assert!(d.p.data().unit.buffs.check_buff(buff::CANNOT_ESCAPE));
    let invite = body(&SCInviteJuryPacket {
        defendant_name: "Crook".into(),
        trial: tid,
    });
    assert_eq!(
        find(&trial_frames(&mut j1.client), sc::SCInviteJuryPacket),
        vec![invite.clone()]
    );
    assert_eq!(
        find(&trial_frames(&mut j2.client), sc::SCInviteJuryPacket),
        vec![invite]
    );
    assert!(trial_frames(&mut arrestor.client).is_empty());
    assert_eq!(
        trial::trial_chat(&server, &d.p).map(|c| c.internal_name()),
        Some("Nuian Court".into())
    );

    // Juror 1 accepts.
    send(&server, &mut j1, cs::CSReplyInviteJuryPacket, |s| {
        s.write_bool(true).write_u32(tid);
    })
    .await;
    let f = trial_frames(&mut j1.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCSummonJuryPacket,
            sc::SCTeleportUnitPacket,
            sc::SCJuryBeSeatedPacket,
            sc::SCJoinedChatChannelPacket,
            sc::SCTrialInfoPacket,
            sc::SCCrimeRecordsPacket,
            sc::SCCrimeDataPacket,
            sc::SCJuryWaitStatusPacket,
        ]
    );
    assert_eq!(
        f[0].1,
        body(&SCSummonJuryPacket {
            trial: tid,
            court: 1,
            jury_number: 1
        })
    );
    assert_eq!(
        f[1].1,
        body(&SCTeleportUnitPacket {
            reason: teleport_reason::JURY,
            error_message: 0,
            x: 400.0,
            y: 500.0,
            z: 20.0,
            z2: 1.25
        })
    );
    assert_eq!(
        f[2].1,
        body(&SCJuryBeSeatedPacket {
            is_west: false,
            trial: tid,
            court: 1,
            jury_number: 1
        })
    );
    assert_eq!(
        f[7].1,
        body(&SCJuryWaitStatusPacket {
            count: 1,
            total: 2,
            sentence_time_ms: 200 * 60_000
        })
    );
    {
        let c = j1.p.data();
        let m = c.main_world_position.expect("main world position");
        assert_eq!((m.zone_id, m.x, m.y, m.z), (TEST_ZONE, 102.0, 100.0, 100.0));
        assert_eq!(
            (c.unit.transform.x, c.unit.transform.y, c.unit.transform.z),
            (400.0, 500.0, 20.0)
        );
        assert!(c
            .unit
            .buffs
            .check_buffs(&[buff::COURT_HOUSE, buff::JURY, buff::CANNOT_ESCAPE]));
    }
    assert_eq!(
        find(&trial_frames(&mut d.client), sc::SCJuryWaitStatusPacket),
        vec![body(&SCJuryWaitStatusPacket {
            count: 1,
            total: 2,
            sentence_time_ms: 200 * 60_000
        })]
    );
    assert_eq!(trials.jury_queue(NUIA), vec![3]);
    // A second reply is refused: already a jury member.
    send(&server, &mut j1, cs::CSReplyInviteJuryPacket, |s| {
        s.write_bool(true).write_u32(tid);
    })
    .await;
    assert_eq!(
        ops(&trial_frames(&mut j1.client)),
        vec![sc::SCErrorMsgPacket]
    );

    // Juror 2 accepts: all seats taken -> the summons step ends now.
    send(&server, &mut j2, cs::CSReplyInviteJuryPacket, |s| {
        s.write_bool(true).write_u32(tid);
    })
    .await;
    assert_eq!(
        find(&trial_frames(&mut j2.client), sc::SCJuryBeSeatedPacket),
        vec![body(&SCJuryBeSeatedPacket {
            is_west: false,
            trial: tid,
            court: 1,
            jury_number: 2
        })]
    );
    for pc in [&mut d, &mut j1] {
        assert_eq!(
            find(&trial_frames(&mut pc.client), sc::SCJuryWaitStatusPacket),
            vec![body(&SCJuryWaitStatusPacket {
                count: 2,
                total: 2,
                sentence_time_ms: 200 * 60_000
            })]
        );
    }
    assert!(trials.get_trial(tid).unwrap().current_step_end_ms <= util::now_ms());

    // Tick: ConfirmCriminalRecord (judge message + state to everyone).
    let t1 = util::now_ms() + 1;
    trials.update_tick(&server, t1).await;
    for pc in [&mut d, &mut j1, &mut j2] {
        let f = trial_frames(&mut pc.client);
        assert_eq!(
            ops(&f),
            vec![sc::SCChatMessagePacket, sc::SCChangeTrialStatePacket]
        );
        assert_eq!(
            &f[1].1[..9],
            state_bytes(tid, trial_step::CONFIRM_CRIMINAL_RECORD, 2)
        );
        assert_eq!(
            u32::from_le_bytes([f[1].1[9], f[1].1[10], f[1].1[11], f[1].1[12]]),
            300_000
        );
    }
    // Jury members are no longer queued (they take part in a trial).
    assert!(trials.jury_queue(NUIA).is_empty());

    // Both jurors confirm the records.
    send(&server, &mut j1, cs::CSJuryEndTestimonyPacket, |s| {
        s.write_u32(tid).write_i32(1);
    })
    .await;
    send(&server, &mut j2, cs::CSJuryEndTestimonyPacket, |s| {
        s.write_u32(tid).write_i32(2);
    })
    .await;
    assert_eq!(
        find(&trial_frames(&mut d.client), sc::SCChangeJuryOkCountPacket),
        vec![
            body(&SCChangeJuryOkCountPacket { count: 1, total: 2 }),
            body(&SCChangeJuryOkCountPacket { count: 2, total: 2 })
        ]
    );
    trial_frames(&mut j1.client);
    trial_frames(&mut j2.client);

    // Tick: ClosingStatement; the defendant skips it.
    trials.update_tick(&server, util::now_ms() + 1).await;
    assert_eq!(
        states(&trial_frames(&mut d.client)),
        vec![(trial_step::CLOSING_STATEMENT, 2)]
    );
    send(&server, &mut d, cs::CSSkipFinalStatementPacket, |s| {
        s.write_u32(tid);
    })
    .await;

    // Tick: JuryVerdict (+ the default ruling panel for the defendant).
    trials.update_tick(&server, util::now_ms() + 1).await;
    let f = trial_frames(&mut d.client);
    assert_eq!(
        ops(&f),
        vec![sc::SCChangeTrialStatePacket, sc::SCRulingStatusPacket]
    );
    assert_eq!(&f[0].1[..9], state_bytes(tid, trial_step::JURY_VERDICT, 2));
    assert_eq!(
        f[1].1,
        body(&SCRulingStatusPacket {
            count: 0,
            total: 2,
            sentence_type: 0,
            sentence_time: 0
        })
    );
    trial_frames(&mut j1.client);
    trial_frames(&mut j2.client);

    // Votes: guilty (5 = Guilty4, 100% of the jail time).
    send(&server, &mut j1, cs::CSJuryVerdictPacket, |s| {
        s.write_u32(tid).write_i32(1).write_u8(5);
    })
    .await;
    // Seat 2 voted by the wrong player is ignored.
    send(&server, &mut j1, cs::CSJuryVerdictPacket, |s| {
        s.write_u32(tid).write_i32(2).write_u8(0);
    })
    .await;
    assert_eq!(
        find(&trial_frames(&mut j2.client), sc::SCRulingStatusPacket),
        Vec::<Vec<u8>>::new()
    );
    send(&server, &mut j2, cs::CSJuryVerdictPacket, |s| {
        s.write_u32(tid).write_i32(2).write_u8(5);
    })
    .await;
    let ruling = |count| {
        body(&SCRulingStatusPacket {
            count,
            total: 2,
            sentence_type: 0,
            sentence_time: 0,
        })
    };
    assert_eq!(
        find(&trial_frames(&mut d.client), sc::SCRulingStatusPacket),
        vec![ruling(1), ruling(2)]
    );
    assert_eq!(
        find(&trial_frames(&mut j1.client), sc::SCRulingStatusPacket),
        vec![ruling(1), ruling(2)]
    );
    assert_eq!(
        find(&trial_frames(&mut j2.client), sc::SCRulingStatusPacket),
        vec![ruling(2)]
    );

    // Tick: EndTrial, verdict and the guilty result.
    let t_end = util::now_ms() + 1;
    trials.update_tick(&server, t_end).await;
    let f = trial_frames(&mut d.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCRulingStatusPacket,
            sc::SCChangeTrialStatePacket,
            sc::SCLeavedChatChannelPacket,
            sc::SCCrimeChangedPacket,
            sc::SCTeleportUnitPacket,
        ]
    );
    assert_eq!(
        f[0].1,
        body(&SCRulingStatusPacket {
            count: 2,
            total: 2,
            sentence_type: sentence_result::GUILTY4,
            sentence_time: 200 * 60_000
        })
    );
    assert_eq!(
        f[1].1,
        body(&SCChangeTrialStatePacket {
            trial_id: tid,
            state: trial_step::END_TRIAL,
            cur_jury: 2,
            remain_time: 30_000
        })
    );
    // Wanted is gone after the crime points were cleared: crime state 0.
    assert_eq!(
        f[3].1,
        body(&SCCrimeChangedPacket {
            points: 60,
            crime_points: 0,
            infamy_points: 60,
            crime_state: 0
        })
    );
    // Justice teleports consistently use radians.
    assert_eq!(
        f[4].1,
        body(&SCTeleportUnitPacket {
            reason: teleport_reason::JAIL,
            error_message: 0,
            x: 300.0,
            y: 310.0,
            z: 50.0,
            z2: std::f32::consts::FRAC_PI_2
        })
    );
    {
        let c = d.p.data();
        assert_eq!((c.guilty_count, c.crime_point), (1, 0));
        assert_eq!((c.offline_guilty_time, c.offline_guilty_region), (0, 255));
        assert!(c.disabled_set_position);
        let prisoner = c
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .expect("prisoner buff");
        assert_eq!(prisoner.duration, 200 * 60_000);
        assert!(!c.unit.buffs.check_buff(buff::TRIAL_DEFENDANT));
    }
    // The evidence is archived (KeepHistory).
    assert!(server
        .systems
        .social
        .crimes
        .crimes_of_player(1, false)
        .is_empty());
    assert!(server.systems.social.crimes.get(50).unwrap().judgement_time > 0);
    for pc in [&mut j1, &mut j2] {
        let f = trial_frames(&mut pc.client);
        assert_eq!(
            ops(&f),
            vec![sc::SCRulingStatusPacket, sc::SCChangeTrialStatePacket]
        );
        assert!(!pc.p.data().unit.buffs.check_buff(buff::JURY));
    }

    // 30 s later: ruling closed, the jury returns home, the trial is removed.
    trials.update_tick(&server, t_end + 31_000).await;
    assert_eq!(
        ops(&trial_frames(&mut d.client)),
        vec![sc::SCRulingClosedPacket]
    );
    let f = trial_frames(&mut j1.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCRulingClosedPacket,
            sc::SCTeleportUnitPacket,
            sc::SCJuryPointChangedPacket,
            sc::SCLeavedChatChannelPacket,
        ]
    );
    assert_eq!(
        f[1].1,
        body(&SCTeleportUnitPacket {
            reason: teleport_reason::JURY,
            error_message: 0,
            x: 102.0,
            y: 100.0,
            z: 100.0,
            z2: 0.0
        })
    );
    assert_eq!(f[2].1, body(&SCJuryPointChangedPacket { jury_point: 2 }));
    {
        let c = j1.p.data();
        assert_eq!(c.main_world_position, None);
        assert!(c.disabled_set_position);
        assert!(!c.unit.buffs.check_buff(buff::COURT_HOUSE));
    }
    assert!(trials.trial_ids().is_empty());
    assert!(trials.court_rooms()[0].current_trial.is_none());
    assert!(!trials.is_player_in_court(1));
    assert!(!trials.is_player_in_court(2));
    // A late reply is refused.
    send(&server, &mut j2, cs::CSReplyInviteJuryPacket, |s| {
        s.write_bool(true).write_u32(tid);
    })
    .await;
    assert_eq!(
        find(&trial_frames(&mut j2.client), sc::SCErrorMsgPacket).len(),
        1
    );
}

#[tokio::test]
async fn plead_guilty_path() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    let mut d = player(&server, 1, "Crook", |c| {
        c.crime_point = 55;
        c.infamy_point = 100;
    });
    assert!(server.systems.social.crimes.insert(CrimeEvent {
        crime_kind: crime_kind::THEFT,
        ..murder(7, 1, 99)
    }));
    // No arrestor: the region comes from OfflineGuiltyRegion (0 = Nuian).
    let tid = trials.arrest_criminal(&server, &d.p, None).expect("trial");
    let f = trial_frames(&mut d.client);
    assert_eq!(f[0].0, sc::SCCharacterResurrectedPacket);
    // Theft of an offline victim: 8 minutes.
    assert_eq!(
        find(&f, sc::SCAskImprisonOrTrialPacket),
        vec![body(&SCAskImprisonOrTrialPacket {
            crime_value: 55,
            jail_minutes: 8
        })]
    );
    assert_eq!(d.p.data().arrest_count, 0);

    send(&server, &mut d, cs::CSReplyImprisonOrTrialPacket, |s| {
        s.write_bool(false);
    })
    .await;
    let f = trial_frames(&mut d.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCChangeTrialStatePacket,
            sc::SCCrimeChangedPacket,
            sc::SCTeleportUnitPacket
        ]
    );
    assert_eq!(&f[0].1[..9], state_bytes(tid, trial_step::PLEAD_GUILTY, 0));
    assert_eq!(
        f[1].1,
        body(&SCCrimeChangedPacket {
            points: 55,
            crime_points: 0,
            infamy_points: 100,
            crime_state: 0
        })
    );
    {
        let c = d.p.data();
        assert_eq!(c.accept_guilty_count, 1);
        assert_eq!(
            c.unit
                .buffs
                .effect_from_buff_id(buff::PRISONER_NUIAN)
                .unwrap()
                .duration,
            8 * 60_000
        );
    }
    trials.update_tick(&server, util::now_ms()).await;
    assert_eq!(
        ops(&trial_frames(&mut d.client)),
        vec![sc::SCRulingClosedPacket]
    );
    assert!(trials.trial_ids().is_empty());
    // CSCancelTrial on a finished trial: the C# NRE is logged.
    send(&server, &mut d, cs::CSCancelTrialPacket, |s| {
        s.write_u32(tid);
    })
    .await;
    assert!(trial_frames(&mut d.client).is_empty());
}

#[tokio::test]
async fn not_guilty_verdict_with_one_juror() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    trials.initialize_doodads(&[("justice.1.seat.1".into(), seat(0x900, 400.0))]);
    let mut d = player(&server, 1, "Crook", |c| {
        c.crime_point = 70;
        c.infamy_point = 500;
    });
    let mut j = player(&server, 2, "Juror", |c| c.jury_point = 3);
    trials.update_jury_queues(&server);
    let tid = trials.arrest_criminal(&server, &d.p, None).unwrap();
    trials.reply_imprison_or_trial(&server, &d.p, true, util::now_ms());
    trials.update_tick(&server, util::now_ms() + 1).await;
    assert!(trials.process_trial_invite_reply(&server, &j.p, true, tid, util::now_ms()));
    trials.update_tick(&server, util::now_ms() + 1).await; // ConfirmCriminalRecord
    trials.jury_end_testimony(&j.p, tid, 1, util::now_ms());
    trials.update_tick(&server, util::now_ms() + 1).await; // ClosingStatement
                                                           // Not skipped: the closing statement runs its minute.
    trials.update_tick(&server, util::now_ms() + 61_000).await; // JuryVerdict
    trial_frames(&mut d.client);
    trial_frames(&mut j.client);
    trials.jury_verdict(&j.p, tid, 1, 1, util::now_ms());
    let t = util::now_ms() + 1;
    trials.update_tick(&server, t).await;
    let f = trial_frames(&mut d.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCRulingStatusPacket, // the juror's vote count
            sc::SCRulingStatusPacket,
            sc::SCChangeTrialStatePacket,
            sc::SCCrimeChangedPacket,
        ]
    );
    assert_eq!(
        f[1].1,
        body(&SCRulingStatusPacket {
            count: 1,
            total: 1,
            sentence_type: sentence_result::NOT_GUILTY,
            sentence_time: 0
        })
    );
    assert_eq!(
        f[3].1,
        body(&SCCrimeChangedPacket {
            points: 70,
            crime_points: 0,
            infamy_points: 430,
            crime_state: 0
        })
    );
    assert_eq!(d.p.data().not_guilty_count, 1);
    assert!(!d.p.data().unit.buffs.check_buff(buff::TRIAL_DEFENDANT));
    // The defendant stays in the court chat until the cleanup.
    trials.update_tick(&server, t + 31_000).await;
    assert_eq!(
        ops(&trial_frames(&mut d.client)),
        vec![sc::SCRulingClosedPacket, sc::SCLeavedChatChannelPacket]
    );
    assert_eq!(j.p.data().jury_point, 4);
    assert!(trials.trial_ids().is_empty());
}

#[tokio::test]
async fn trial_without_jury_is_cancelled_with_the_default_sentence() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    trials.initialize_doodads(&[("justice.1.seat.1".into(), seat(0x900, 400.0))]);
    let mut d = player(&server, 1, "Crook", |c| c.crime_point = 50);
    let tid = trials.arrest_criminal(&server, &d.p, None).unwrap();
    trials.reply_imprison_or_trial(&server, &d.p, true, util::now_ms());
    // Nobody queued: the summons wait is skipped in the same tick.
    trials.update_tick(&server, util::now_ms() + 1).await;
    let f = trial_frames(&mut d.client);
    assert_eq!(
        states(&f),
        vec![
            (trial_step::VERIFY_CRIMINAL_RECORD, 0),
            (trial_step::AWAITING_JURY_SUMMONS, 0),
            (trial_step::CONFIRM_CRIMINAL_RECORD, 0)
        ]
    );
    // Next tick: no active jury -> cancelled -> guilty with the default sentence.
    trials.update_tick(&server, util::now_ms() + 1).await;
    let f = trial_frames(&mut d.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCTrialCanceledPacket,
            sc::SCChangeTrialStatePacket,
            sc::SCLeavedChatChannelPacket,
            sc::SCCrimeChangedPacket,
            sc::SCTeleportUnitPacket,
            sc::SCRulingClosedPacket,
        ]
    );
    assert_eq!(f[0].1, body(&SCTrialCanceledPacket { trial_id: tid }));
    assert_eq!(&f[1].1[..9], state_bytes(tid, trial_step::END_TRIAL, 0));
    // No evidence: 0 minutes -> the 10 s minimum.
    assert_eq!(
        d.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .unwrap()
            .duration,
        10_000
    );
    assert!(trials.trial_ids().is_empty());
}

#[tokio::test]
async fn defendant_cancel_sends_trial_canceled_before_ruling_closed() {
    let (server, mut d, mut jury, tid) = summoned_trial(1).await;
    send(&server, &mut d, cs::CSCancelTrialPacket, |s| {
        s.write_u32(tid);
    })
    .await;
    let canceled = body(&SCTrialCanceledPacket { trial_id: tid });
    let mut defendant_frames = trial_frames(&mut d.client);
    assert_eq!(defendant_frames[0].0, sc::SCTrialCanceledPacket);
    assert_eq!(defendant_frames[0].1, canceled);
    assert_eq!(
        server.systems.social.trials.get_trial(tid).unwrap().step,
        trial_step::PLEAD_GUILTY
    );
    let mut juror_frames = trial_frames(&mut jury[0].client);
    assert_eq!(
        find(&juror_frames, sc::SCTrialCanceledPacket),
        vec![canceled.clone()]
    );

    // The ruling closes on the next tick, after the cancellation.
    server
        .systems
        .social
        .trials
        .update_tick(&server, util::now_ms() + 1)
        .await;
    defendant_frames.extend(trial_frames(&mut d.client));
    juror_frames.extend(trial_frames(&mut jury[0].client));
    for frames in [&defendant_frames, &juror_frames] {
        let canceled_at = frames
            .iter()
            .position(|f| f.0 == sc::SCTrialCanceledPacket)
            .expect("SCTrialCanceled");
        let closed_at = frames
            .iter()
            .position(|f| f.0 == sc::SCRulingClosedPacket)
            .expect("SCRulingClosed");
        assert!(canceled_at < closed_at);
    }
}

#[tokio::test]
async fn verdict_count_asks_only_the_jurors_who_still_have_to_vote() {
    let (server, mut d, mut jury, tid) = summoned_trial(2).await;
    advance_to_verdict(&server, &mut d, &mut jury, tid).await;

    // The verdict step starts with no votes: every seated juror gets 0 of 2.
    let start = body(&SCChangeJuryVerdictCountPacket { count: 0, total: 2 });
    for juror in &mut jury {
        assert_eq!(
            find(
                &trial_frames(&mut juror.client),
                sc::SCChangeJuryVerdictCountPacket
            ),
            vec![start.clone()]
        );
    }
    assert!(find(
        &trial_frames(&mut d.client),
        sc::SCChangeJuryVerdictCountPacket
    )
    .is_empty());

    // The first vote updates the second juror only; the voter keeps getting SCRulingStatus.
    send(&server, &mut jury[0], cs::CSJuryVerdictPacket, |s| {
        s.write_u32(tid).write_i32(1).write_u8(5);
    })
    .await;
    let voted = trial_frames(&mut jury[0].client);
    assert!(find(&voted, sc::SCChangeJuryVerdictCountPacket).is_empty());
    assert_eq!(
        find(&voted, sc::SCRulingStatusPacket),
        vec![body(&SCRulingStatusPacket {
            count: 1,
            total: 2,
            sentence_type: 0,
            sentence_time: 0,
        })]
    );
    let waiting = trial_frames(&mut jury[1].client);
    assert_eq!(
        find(&waiting, sc::SCChangeJuryVerdictCountPacket),
        vec![body(&SCChangeJuryVerdictCountPacket { count: 1, total: 2 })]
    );
    assert!(find(&waiting, sc::SCRulingStatusPacket).is_empty());
    assert!(find(
        &trial_frames(&mut d.client),
        sc::SCChangeJuryVerdictCountPacket
    )
    .is_empty());

    // With every seated juror voted there is nobody left to ask.
    send(&server, &mut jury[1], cs::CSJuryVerdictPacket, |s| {
        s.write_u32(tid).write_i32(2).write_u8(5);
    })
    .await;
    for juror in &mut jury {
        assert!(find(
            &trial_frames(&mut juror.client),
            sc::SCChangeJuryVerdictCountPacket
        )
        .is_empty());
    }
    assert!(find(
        &trial_frames(&mut d.client),
        sc::SCChangeJuryVerdictCountPacket
    )
    .is_empty());
}

#[tokio::test]
async fn jury_summoned_requires_the_seat_holder_and_changes_nothing() {
    let (server, mut d, mut jury, tid) = summoned_trial(2).await;
    let trials = &server.systems.social.trials;
    assert!(trials.jury_summoned(&jury[0].p, tid, 1));
    assert!(trials.jury_summoned(&jury[1].p, tid, 2));
    assert!(!trials.jury_summoned(&jury[1].p, tid, 1));
    assert!(!trials.jury_summoned(&jury[0].p, tid, 2));
    assert!(!trials.jury_summoned(&jury[0].p, tid + 100, 1));
    assert!(!trials.jury_summoned(&d.p, tid, 1));

    let before = trial_seat_state(&server, tid);
    // A non-seat-holder, a juror claiming the other seat and the real seat holder: none
    // changes the trial and none sends a packet.
    send(&server, &mut d, cs::CSJurySummonedPacket, |s| {
        s.write_u32(tid).write_i32(1).write_i32(1);
    })
    .await;
    send(&server, &mut jury[0], cs::CSJurySummonedPacket, |s| {
        s.write_u32(tid).write_i32(1).write_i32(2);
    })
    .await;
    send(&server, &mut jury[0], cs::CSJurySummonedPacket, |s| {
        s.write_u32(tid).write_i32(1).write_i32(1);
    })
    .await;
    assert_eq!(trial_seat_state(&server, tid), before);
    assert!(trial_frames(&mut d.client).is_empty());
    for juror in &mut jury {
        assert!(trial_frames(&mut juror.client).is_empty());
    }
}

#[tokio::test]
async fn login_with_offline_guilty_time_goes_to_jail() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    let d = player(&server, 1, "Escapee", |c| {
        c.jury_point = 1;
        c.offline_guilty_time = 12;
        c.offline_guilty_region = 0;
    });
    let mut d = d;
    trial::handle_player_login(&server, &d.p);
    assert!(trials.jury_queue(NUIA).is_empty());
    let f = trial_frames(&mut d.client);
    assert!(ops(&f).contains(&sc::SCTeleportUnitPacket));
    assert_eq!(d.p.data().offline_guilty_time, 0);
    assert_eq!(d.p.data().guilty_count, 1);
    assert!(d.p.data().unit.buffs.check_buff(buff::PRISONER_NUIAN));

    // Jail enforcement also applies without jury eligibility.
    let mut e = player(&server, 2, "Other", |c| c.offline_guilty_time = 12);
    trial::handle_player_login(&server, &e.p);
    assert!(ops(&trial_frames(&mut e.client)).contains(&sc::SCTeleportUnitPacket));
    assert_eq!(e.p.data().offline_guilty_time, 0);
    assert_eq!(
        e.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .unwrap()
            .duration,
        12 * 60_000
    );
}

async fn summoned_trial(count: u32) -> (Arc<GameServer>, Pc, Vec<Pc>, u32) {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    let links: Vec<_> = (1..=count)
        .map(|i| {
            (
                format!("justice.1.seat.{i}"),
                seat(0x900 + i, 400.0 + i as f32),
            )
        })
        .collect();
    trials.initialize_doodads(&links);
    let mut defendant = player(&server, 1, "Defendant", |c| {
        c.crime_point = 50;
        c.infamy_point = 50;
    });
    let mut jurors: Vec<_> = (0..count)
        .map(|i| player(&server, i + 2, &format!("Juror{i}"), |c| c.jury_point = 1))
        .collect();
    server.systems.social.crimes.insert(murder(50, 1, 99)); // offline victim: 20 minutes
    trials.update_jury_queues(&server);
    let tid = trials.arrest_criminal(&server, &defendant.p, None).unwrap();
    send(
        &server,
        &mut defendant,
        cs::CSReplyImprisonOrTrialPacket,
        |s| {
            s.write_bool(true);
        },
    )
    .await;
    trials.update_tick(&server, util::now_ms() + 1).await;
    for juror in &mut jurors {
        send(&server, juror, cs::CSReplyInviteJuryPacket, |s| {
            s.write_bool(true).write_u32(tid);
        })
        .await;
    }
    trials.update_tick(&server, util::now_ms() + 1).await;
    assert_eq!(
        trials.get_trial(tid).unwrap().step,
        trial_step::CONFIRM_CRIMINAL_RECORD
    );
    defendant.client.drain();
    for juror in &mut jurors {
        juror.client.drain();
    }
    (server, defendant, jurors, tid)
}

async fn advance_to_verdict(server: &GameServer, defendant: &mut Pc, jurors: &mut [Pc], tid: u32) {
    for (i, juror) in jurors.iter_mut().enumerate() {
        send(server, juror, cs::CSJuryEndTestimonyPacket, |s| {
            s.write_u32(tid).write_i32(i as i32 + 1);
        })
        .await;
    }
    server
        .systems
        .social
        .trials
        .update_tick(server, util::now_ms() + 1)
        .await;
    send(server, defendant, cs::CSSkipFinalStatementPacket, |s| {
        s.write_u32(tid);
    })
    .await;
    // Drop the confirm/closing frames so the verdict-step frames are the only ones left.
    defendant.client.drain();
    for juror in jurors.iter_mut() {
        juror.client.drain();
    }
    server
        .systems
        .social
        .trials
        .update_tick(server, util::now_ms() + 1)
        .await;
    assert_eq!(
        server.systems.social.trials.get_trial(tid).unwrap().step,
        trial_step::JURY_VERDICT
    );
}

async fn open_verdict(server: &GameServer, defendant: &mut Pc, jurors: &mut [Pc], tid: u32) {
    advance_to_verdict(server, defendant, jurors, tid).await;
    defendant.client.drain();
    for juror in jurors {
        juror.client.drain();
    }
}

#[tokio::test]
async fn jury_can_only_confirm_its_own_seat_once() {
    let (server, mut d, mut jury, tid) = summoned_trial(2).await;
    for caller in [&mut d, &mut jury[0]] {
        send(&server, caller, cs::CSJuryEndTestimonyPacket, |s| {
            s.write_u32(tid).write_i32(2);
        })
        .await;
    }
    assert!(!server.systems.social.trials.get_trial(tid).unwrap().jury[&2].confirm_testimony);
    send(&server, &mut jury[0], cs::CSJuryEndTestimonyPacket, |s| {
        s.write_u32(tid).write_i32(1);
    })
    .await;
    assert_eq!(
        find(&d.client.drain(), sc::SCChangeJuryOkCountPacket),
        vec![body(&SCChangeJuryOkCountPacket { count: 1, total: 2 })]
    );
    send(&server, &mut jury[0], cs::CSJuryEndTestimonyPacket, |s| {
        s.write_u32(tid).write_i32(1);
    })
    .await;
    assert!(find(&d.client.drain(), sc::SCChangeJuryOkCountPacket).is_empty());
}

#[tokio::test]
async fn invalid_spoofed_and_repeated_votes_cannot_change_the_verdict() {
    let (server, mut d, mut jury, tid) = summoned_trial(1).await;
    open_verdict(&server, &mut d, &mut jury, tid).await;
    for sentence in [0, 7, 255] {
        send(&server, &mut jury[0], cs::CSJuryVerdictPacket, |s| {
            s.write_u32(tid).write_i32(1).write_u8(sentence);
        })
        .await;
    }
    send(&server, &mut d, cs::CSJuryVerdictPacket, |s| {
        s.write_u32(tid).write_i32(1).write_u8(1);
    })
    .await;
    assert_eq!(
        server.systems.social.trials.get_trial(tid).unwrap().jury[&1].selected_sentence,
        -1
    );
    for sentence in [5, 1] {
        send(&server, &mut jury[0], cs::CSJuryVerdictPacket, |s| {
            s.write_u32(tid).write_i32(1).write_u8(sentence);
        })
        .await;
    }
    server
        .systems
        .social
        .trials
        .update_tick(&server, util::now_ms() + 1)
        .await;
    assert_eq!(d.p.data().guilty_count, 1);
    assert_eq!(
        d.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .unwrap()
            .duration,
        20 * 60_000
    );
    for op in [cs::CSReplyImprisonOrTrialPacket, cs::CSCancelTrialPacket] {
        send(&server, &mut d, op, |s| {
            if op == cs::CSCancelTrialPacket {
                s.write_u32(tid);
            } else {
                s.write_bool(false);
            }
        })
        .await;
    }
    assert_eq!(d.p.data().accept_guilty_count, 0);
    assert_eq!(d.p.data().guilty_count, 1);
}

#[tokio::test]
async fn all_six_jury_choices_apply_the_expected_sentence() {
    for (sentence, minutes) in [(1, 0), (2, 4), (3, 10), (4, 16), (5, 20), (6, 24)] {
        let (server, mut d, mut jury, tid) = summoned_trial(1).await;
        open_verdict(&server, &mut d, &mut jury, tid).await;
        send(&server, &mut jury[0], cs::CSJuryVerdictPacket, |s| {
            s.write_u32(tid).write_i32(1).write_u8(sentence);
        })
        .await;
        let end = util::now_ms() + 1;
        server.systems.social.trials.update_tick(&server, end).await;
        assert_eq!(
            find(&d.client.drain(), sc::SCRulingStatusPacket).last(),
            Some(&body(&SCRulingStatusPacket {
                count: 1,
                total: 1,
                sentence_type: sentence,
                sentence_time: minutes * 60_000,
            }))
        );
        {
            let c = d.p.data();
            if minutes == 0 {
                assert_eq!(c.not_guilty_count, 1);
                assert!(!c.unit.buffs.check_buff(buff::PRISONER_NUIAN));
                assert!(!c.unit.buffs.check_buff(buff::CANNOT_ESCAPE));
            } else {
                assert_eq!(
                    c.unit
                        .buffs
                        .effect_from_buff_id(buff::PRISONER_NUIAN)
                        .unwrap()
                        .duration,
                    minutes * 60_000
                );
                assert_eq!(
                    (c.unit.transform.x, c.unit.transform.y, c.unit.transform.z),
                    (JAIL_POS.x, JAIL_POS.y, JAIL_POS.z)
                );
            }
        }
        server
            .systems
            .social
            .trials
            .update_tick(&server, end + 31_000)
            .await;
        assert!(server.systems.social.trials.trial_ids().is_empty());
        assert_eq!(jury[0].p.data().main_world_position, None);
        assert!(!jury[0].p.data().unit.buffs.check_buff(buff::CANNOT_ESCAPE));
    }
}

#[tokio::test]
async fn the_most_common_jury_sentence_wins() {
    let (server, mut d, mut jury, tid) = summoned_trial(3).await;
    open_verdict(&server, &mut d, &mut jury, tid).await;
    for (i, sentence) in [1, 5, 5].into_iter().enumerate() {
        send(&server, &mut jury[i], cs::CSJuryVerdictPacket, |s| {
            s.write_u32(tid).write_i32(i as i32 + 1).write_u8(sentence);
        })
        .await;
    }
    server
        .systems
        .social
        .trials
        .update_tick(&server, util::now_ms() + 1)
        .await;
    assert_eq!(d.p.data().guilty_count, 1);
    assert_eq!(
        server
            .systems
            .social
            .trials
            .get_trial(tid)
            .unwrap()
            .jail_time,
        20
    );
}

#[tokio::test]
async fn a_guilty_plea_in_a_holding_cell_cannot_free_an_occupied_court() {
    let (server, d, mut jury, tid) = summoned_trial(1).await;
    let trials = &server.systems.social.trials;
    let mut other = player(&server, 10, "Other", |c| c.crime_point = 50);
    let other_tid = trials.arrest_criminal(&server, &other.p, None).unwrap();
    send(&server, &mut other, cs::CSReplyImprisonOrTrialPacket, |s| {
        s.write_bool(false);
    })
    .await;
    trials.update_tick(&server, util::now_ms() + 1).await;
    assert!(trials.get_trial(other_tid).is_none());
    assert_eq!(trials.court_rooms()[0].current_trial, Some(tid));
    trials.cancel_trial(&server, &d.p, tid, util::now_ms());
    trials.update_tick(&server, util::now_ms() + 1).await;
    let c = jury[0].p.data();
    assert!(c.main_world_position.is_none());
    assert!(!c
        .unit
        .buffs
        .check_buffs(&[buff::COURT_HOUSE, buff::JURY, buff::CANNOT_ESCAPE]));
    assert_eq!(c.unit.transform.x, 102.0);
    assert_eq!(c.jury_point, 1);
    drop(c);
    assert!(ops(&jury[0].client.drain()).contains(&sc::SCRulingClosedPacket));
}

#[tokio::test]
async fn disconnecting_defendant_closes_the_case_and_enforces_the_saved_sentence() {
    let (server, d, jury, tid) = summoned_trial(1).await;
    d.p.data().is_online = false;
    super::session::on_online_changed(&server, &d.p, false);
    assert!(server.systems.social.trials.get_trial(tid).is_none());
    assert_eq!(d.p.data().offline_guilty_time, 20);
    assert_eq!(jury[0].p.data().unit.transform.x, 102.0);
    assert!(jury[0].p.data().main_world_position.is_none());
    assert_eq!(jury[0].p.data().jury_point, 1);
    assert!(server.systems.social.trials.court_rooms()[0]
        .current_trial
        .is_none());
    // NotifyInGame after reconnect must enforce the pending sentence even without jury points.
    d.p.data().is_online = true;
    trial::handle_player_login(&server, &d.p);
    assert_eq!(d.p.data().guilty_count, 1);
    assert_eq!(
        d.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .unwrap()
            .duration,
        20 * 60_000
    );
    trial::handle_player_login(&server, &d.p);
    assert_eq!(d.p.data().guilty_count, 1);
}

#[tokio::test]
async fn disconnecting_a_juror_vacates_the_seat_without_awarding_attendance() {
    let (server, _d, jury, tid) = summoned_trial(2).await;
    jury[0].p.data().is_online = false;
    super::session::on_online_changed(&server, &jury[0].p, false);
    let t = server.systems.social.trials.get_trial(tid).unwrap();
    assert_eq!(t.active_jury_count(), 1);
    assert!(t.jury[&1].jury_member.is_none());
    assert_eq!(jury[0].p.data().unit.transform.x, 102.0);
    assert_eq!(jury[0].p.data().jury_point, 1);
    assert!(!jury[0].p.data().unit.buffs.check_buff(buff::CANNOT_ESCAPE));
}

#[tokio::test]
async fn restored_prisoner_keeps_remaining_online_time_and_can_finish_the_sentence() {
    let server = justice_server();
    crate::systems::skills::install_hooks(&server);
    let mut prisoner = player(&server, 1, "Prisoner", |_| {});
    let now = util::now_ms();
    let row = crate::db_character::SavedBuffRow {
        buff_id: buff::PRISONER_NUIAN,
        ab_level: 1,
        time_left: 5_000,
        charge: 0,
        real_time: false,
        saved_at_ms: now - 86_400_000,
    };
    crate::db_character::apply_active_buffs(&server, &mut prisoner.p.data(), &[row], now);
    crate::systems::dispatch_buff_events(&server, &crate::world::WorldObject::from(&prisoner.p));
    trial::handle_player_login(&server, &prisoner.p);
    let buff = prisoner
        .p
        .data()
        .unit
        .buffs
        .effect_from_buff_id(buff::PRISONER_NUIAN)
        .unwrap()
        .clone();
    assert_eq!(buff.duration, 5_000);
    assert_eq!(prisoner.p.data().guilty_count, 0);
    assert_eq!(prisoner.p.data().unit.transform.x, JAIL_POS.x);
    assert!(ops(&prisoner.client.drain()).contains(&sc::SCTeleportUnitPacket));
    // Run the scheduled completion without waiting five seconds in the test.
    crate::systems::skills::buffs::dispel_task(
        &server,
        prisoner.p.obj_id,
        buff.index,
        buff.task_gen,
    );
    assert!(!prisoner
        .p
        .data()
        .unit
        .buffs
        .check_buff(buff::PRISONER_NUIAN));
}

#[tokio::test]
async fn arrest_is_idempotent_and_justice_teleports_update_server_regions() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    let d = player(&server, 1, "Wanted", |c| c.crime_point = 50);
    let guard = player(&server, 2, "Guard", |_| {});
    let tid = trials
        .arrest_criminal(&server, &d.p, Some(&guard.p))
        .unwrap();
    assert_eq!(
        trials.arrest_criminal(&server, &d.p, Some(&guard.p)),
        Some(tid)
    );
    assert_eq!(trials.trial_ids(), vec![tid]);
    assert_eq!(d.p.data().arrest_count, 1);
    assert!(d
        .p
        .data()
        .unit
        .buffs
        .check_buff(buff::FORCIBLY_AWAITING_TRIAL));
    assert_eq!(d.p.data().unit.transform.x, HOLDING_CELL.x);
    assert_eq!(d.p.region(), Some((0, 3, 3)));
    trials.reply_imprison_or_trial(&server, &d.p, false, util::now_ms());
    assert_eq!(d.p.data().unit.transform.x, JAIL_POS.x);
    assert_eq!(d.p.region(), Some((0, 4, 4)));
    assert!(!d
        .p
        .data()
        .unit
        .buffs
        .check_buff(buff::FORCIBLY_AWAITING_TRIAL));
    trials.reply_imprison_or_trial(&server, &d.p, false, util::now_ms());
    assert_eq!(d.p.data().accept_guilty_count, 1);
}

#[tokio::test]
async fn jury_invites_require_an_invitation_the_right_faction_and_current_eligibility() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    trials.initialize_doodads(&[("justice.1.seat.1".into(), seat(0x900, 400.0))]);
    let d = player(&server, 1, "Defendant", |c| c.crime_point = 50);
    let invited = player(&server, 2, "Invited", |c| c.jury_point = 1);
    let foreign = player(&server, 3, "Foreign", |c| {
        c.jury_point = 1;
        c.unit.faction_id = 102;
    });
    trials.update_jury_queues(&server);
    let tid = trials.arrest_criminal(&server, &d.p, None).unwrap();
    trials.reply_imprison_or_trial(&server, &d.p, true, util::now_ms());
    trials.update_tick(&server, util::now_ms() + 1).await;
    let late = player(&server, 4, "Uninvited", |c| c.jury_point = 1);
    trials.update_jury_queues(&server);
    for p in [&foreign.p, &late.p] {
        assert!(!trials.process_trial_invite_reply(&server, p, true, tid, util::now_ms()));
    }
    invited.p.data().crime_point = 50; // queue is stale until the next tick
    assert!(!trials.process_trial_invite_reply(&server, &invited.p, true, tid, util::now_ms()));
    assert_eq!(trials.get_trial(tid).unwrap().active_jury_count(), 0);
}

#[tokio::test]
async fn sparse_jury_seats_are_usable() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    trials.initialize_doodads(&[("justice.1.seat.3".into(), seat(0x900, 400.0))]);
    let d = player(&server, 1, "Defendant", |c| c.crime_point = 50);
    let j = player(&server, 2, "Juror", |c| c.jury_point = 1);
    trials.update_jury_queues(&server);
    let tid = trials.arrest_criminal(&server, &d.p, None).unwrap();
    trials.reply_imprison_or_trial(&server, &d.p, true, util::now_ms());
    trials.update_tick(&server, util::now_ms() + 1).await;
    assert!(trials.process_trial_invite_reply(&server, &j.p, true, tid, util::now_ms()));
    assert!(trials.get_trial(tid).unwrap().jury[&3]
        .jury_member
        .is_some());
}

#[tokio::test]
async fn undecided_jury_times_out_to_the_default_sentence() {
    let (server, mut d, mut jury, tid) = summoned_trial(1).await;
    open_verdict(&server, &mut d, &mut jury, tid).await;
    server
        .systems
        .social
        .trials
        .update_tick(&server, util::now_ms() + 61_000)
        .await;
    assert_eq!(
        d.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .unwrap()
            .duration,
        10 * 60_000
    );
}

#[tokio::test]
async fn haranyan_sentences_keep_their_region_including_for_pirates_on_reconnect() {
    let server = justice_server_with(|j| {
        j.court_rooms[0].id = 3;
        j.court_rooms[0].faction = HARANYA;
        j.jails[0].faction = HARANYA;
    });
    let trials = &server.systems.social.trials;
    let d = player(&server, 1, "Defendant", |c| {
        c.unit.faction_id = 102;
        c.crime_point = 50;
    });
    server.systems.social.crimes.insert(murder(50, 1, 99));
    trials.arrest_criminal(&server, &d.p, None).unwrap();
    assert_eq!(d.p.data().offline_guilty_region, 1);
    d.p.data().is_online = false;
    super::session::on_online_changed(&server, &d.p, false);
    {
        let mut c = d.p.data();
        c.unit.faction_id = 161;
        c.is_online = true;
    }
    trial::handle_player_login(&server, &d.p);
    assert_eq!(
        d.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_HARANYAN)
            .unwrap()
            .duration,
        20 * 60_000
    );
    assert!(!d.p.data().unit.buffs.check_buff(buff::PRISONER_NUIAN));
}

#[tokio::test]
async fn concurrent_reports_only_count_a_piece_of_evidence_once() {
    let server = justice_server();
    let hooks = Arc::new(FakeHooks::default());
    server.systems.social.hooks.install(hooks.clone());
    let criminal = player(&server, 1, "Criminal", |_| {});
    let a = player(&server, 2, "WitnessA", |_| {});
    let b = player(&server, 3, "WitnessB", |_| {});
    let mut doodad = Doodad::new(878);
    doodad.owner_type = doodad_owner_type::CHARACTER;
    doodad.owner_id = 1;
    doodad.data = 2;
    let evidence = Entity::new(0x900, doodad);
    let crimes = &server.systems.social.crimes;
    let (a, b) = tokio::join!(
        crimes.report_crime(&server, &a.p, &evidence, 1, 2, 3, "first"),
        crimes.report_crime(&server, &b.p, &evidence, 1, 2, 3, "second"),
    );
    assert_ne!(a.is_some(), b.is_some());
    assert_eq!(crimes.crimes_of_player(1, false).len(), 1);
    assert_eq!(criminal.p.data().crime_point, 25);
    assert_eq!(criminal.p.data().evidence_reported_count, 1);
    assert_eq!(hooks.phases.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn arrest_from_an_instance_loads_the_main_world_and_moves_membership() {
    let server = justice_server();
    let d = player(&server, 1, "Defendant", |c| c.crime_point = 50);
    let main = server.world.world(0).unwrap();
    let instance = server.world.create_world_instance(
        Arc::new(crate::data::world_templates::WorldTemplate::synthetic(
            1,
            "test_instance",
            2,
            2,
            TEST_ZONE,
        )),
        42,
    );
    main.delete(&d.p);
    {
        let mut c = d.p.data();
        c.unit.transform.instance_id = 42;
        c.unit.transform.world_id = 1;
    }
    instance.spawn(&d.p, &server.view_ctx());
    let mut d = d;
    d.client.drain();
    server
        .systems
        .social
        .trials
        .arrest_criminal(&server, &d.p, None)
        .unwrap();
    let frames = d.client.drain();
    let load = frames
        .iter()
        .position(|f| f.0 == sc::SCLoadInstancePacket)
        .unwrap();
    let teleport = frames
        .iter()
        .position(|f| f.0 == sc::SCTeleportUnitPacket)
        .unwrap();
    assert!(load < teleport);
    assert_eq!(d.p.data().unit.transform.instance_id, 0);
    assert!(instance.character_by_obj_id(d.p.obj_id).is_none());
    assert!(main.character_by_obj_id(d.p.obj_id).is_some());
    assert_eq!(d.p.region(), Some((0, 3, 3)));
}

#[tokio::test]
async fn sentence_duration_cannot_overflow_the_prison_buff_or_wire() {
    let (server, mut d, mut jury, tid) = summoned_trial(1).await;
    d.p.data().infamy_point = i32::MAX;
    server
        .systems
        .social
        .trials
        .add_new_evidence(&server, &murder(51, 1, 99));
    open_verdict(&server, &mut d, &mut jury, tid).await;
    send(&server, &mut jury[0], cs::CSJuryVerdictPacket, |s| {
        s.write_u32(tid).write_i32(1).write_u8(6);
    })
    .await;
    server
        .systems
        .social
        .trials
        .update_tick(&server, util::now_ms() + 1)
        .await;
    let max_minutes = i32::MAX / 60_000;
    assert_eq!(
        d.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .unwrap()
            .duration,
        max_minutes * 60_000
    );
    let ruling = find(&d.client.drain(), sc::SCRulingStatusPacket);
    assert_eq!(
        ruling.last(),
        Some(&body(&SCRulingStatusPacket {
            count: 1,
            total: 1,
            sentence_type: sentence_result::GUILTY5,
            sentence_time: max_minutes * 60_000,
        }))
    );
}

#[tokio::test]
async fn even_a_zero_minute_pending_case_survives_disconnect() {
    let server = justice_server();
    let d = player(&server, 1, "Defendant", |c| c.crime_point = 50);
    server
        .systems
        .social
        .trials
        .arrest_criminal(&server, &d.p, None)
        .unwrap();
    d.p.data().is_online = false;
    super::session::on_online_changed(&server, &d.p, false);
    assert_eq!(d.p.data().offline_guilty_time, 1);
    d.p.data().is_online = true;
    trial::handle_player_login(&server, &d.p);
    assert_eq!(
        d.p.data()
            .unit
            .buffs
            .effect_from_buff_id(buff::PRISONER_NUIAN)
            .unwrap()
            .duration,
        60_000
    );
}

#[test]
#[ignore = "requires AAEMU_COMPACT_DB"]
fn real_justice_buffs_preserve_prison_sentences() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = crate::data::skills::SkillData::load(&conn).unwrap();
    for id in [buff::PRISONER_NUIAN, buff::PRISONER_HARANYAN] {
        let template = data.buff(id).unwrap();
        assert_eq!(template.kind, BuffKind::Bad);
        assert!(!template.real_time);
        let mut b = crate::models::buff::Buff::new(template.clone(), Default::default(), None, 0);
        b.duration = 10_000;
        assert!(b.should_persist(9_000));
        assert!(!b.should_persist(10_000));
    }
    for id in [
        buff::FORCIBLY_AWAITING_TRIAL,
        buff::TRIAL_DEFENDANT,
        buff::JURY,
    ] {
        assert_eq!(data.buff(id).unwrap().duration, 0);
    }
}

#[tokio::test]
async fn trial_chat_of_a_player_without_court() {
    let server = justice_server();
    let pirate = player(&server, 1, "Pirate", |c| c.unit.faction_id = 161);
    // The C# NullReferenceException: logged, no channel.
    assert!(trial::trial_chat(&server, &pirate.p).is_none());
    let h = player(&server, 2, "H", |c| c.unit.faction_id = 102);
    assert_eq!(
        trial::trial_chat(&server, &h.p).map(|c| c.internal_name()),
        Some("Haranyan Court".into())
    );
}

#[tokio::test]
async fn audience_join_and_leave() {
    let server = justice_server();
    let trials = &server.systems.social.trials;
    let mut bench = Doodad::new(5137);
    bench.transform.set_xyz(104.0, 100.0, 100.0);
    trials.initialize_doodads(&[("justice.1.spectator".into(), Entity::new(0x950, bench))]);
    let mut a = player(&server, 1, "Watcher", |_| {});
    let mut far = player(&server, 20, "Far", |_| {});
    trial_frames(&mut a.client);
    // Too far away (> 5 m): refused.
    send(&server, &mut far, cs::CSJoinTrialAudiencePacket, |s| {
        s.write_u32(5137);
    })
    .await;
    assert!(trial_frames(&mut far.client).is_empty());
    send(&server, &mut a, cs::CSJoinTrialAudiencePacket, |s| {
        s.write_u32(5137);
    })
    .await;
    let f = trial_frames(&mut a.client);
    assert_eq!(
        ops(&f),
        vec![
            sc::SCTrialAudienceJoinedPacket,
            sc::SCJoinedChatChannelPacket
        ]
    );
    assert_eq!(
        f[0].1,
        body(&SCTrialAudienceJoinedPacket {
            trial_id: 0,
            bc: 0x101,
            audience_name: "Watcher".into()
        })
    );
    assert_eq!(
        find(
            &trial_frames(&mut far.client),
            sc::SCTrialAudienceJoinedPacket
        )
        .len(),
        1
    );
    send(&server, &mut a, cs::CSLeaveTrialAudiencePacket, |_| {}).await;
    assert_eq!(
        ops(&trial_frames(&mut a.client)),
        vec![sc::SCTrialAudienceLeftPacket, sc::SCLeavedChatChannelPacket]
    );
    send(&server, &mut a, cs::CSNaviOpenBountyPacket, |s| {
        s.write_bc(0x77);
    })
    .await;
    assert_eq!(
        trial_frames(&mut a.client),
        vec![(sc::SCBountyListPacket, vec![0, 0, 0, 0, 0])]
    );
}

/// (owner, template, position, item template, custom data) of a created evidence doodad.
type CreatedDoodad = (u32, u32, (f32, f32, f32), u32, i32);

/// Fake npcs / skills services for the crime report path.
#[derive(Default)]
struct FakeHooks {
    phases: Mutex<Vec<(u32, u32, i32)>>,
    created: Mutex<Vec<CreatedDoodad>>,
}

impl SocialHooks for FakeHooks {
    fn evidence_func(&self, _server: &GameServer, _doodad: &DoodadRef) -> Option<EvidenceFunc> {
        Some(EvidenceFunc {
            crime_kind: crime_kind::MURDER,
            crime_value: 25,
            next_phase: 7,
        })
    }

    fn doodad_change_phase(
        &self,
        _server: &GameServer,
        doodad: &DoodadRef,
        caster: &PlayerRef,
        next_phase: i32,
    ) {
        self.phases
            .lock()
            .unwrap()
            .push((doodad.obj_id, caster.character_id, next_phase));
    }

    fn create_player_doodad(
        &self,
        _server: &GameServer,
        owner: &PlayerRef,
        template_id: u32,
        pos: (f32, f32, f32),
        _rot_z: f32,
        item_template_id: u32,
        custom_data: i32,
    ) -> Option<DoodadRef> {
        self.created.lock().unwrap().push((
            owner.character_id,
            template_id,
            pos,
            item_template_id,
            custom_data,
        ));
        None
    }
}

#[tokio::test]
async fn crime_report_through_hooks() {
    let server = justice_server();
    let hooks = Arc::new(FakeHooks::default());
    server.systems.social.hooks.install(hooks.clone());
    let mut criminal = player(&server, 1, "Crook", |_| {});
    let mut reporter = player(&server, 2, "Witness", |_| {});
    let victim = player(&server, 3, "Victim", |_| {});

    // Evidence created on a kill (large bloodstain at the victim).
    server.systems.social.crimes.generate_evidence_from_kill(
        &server,
        &crate::world::WorldObject::from(&criminal.p),
        Some(&criminal.p),
        Some(&victim.p),
    );
    assert_eq!(
        hooks.created.lock().unwrap().clone(),
        vec![(1, 878, (103.0, 100.0, 100.0), 0, 3)]
    );

    // A bloodstain owned by the criminal, victim 3, in the world.
    let mut d = Doodad::new(878);
    d.owner_type = doodad_owner_type::CHARACTER;
    d.owner_id = 1;
    d.data = 3;
    d.plant_time = 1_600_000_000;
    d.transform.set_xyz(110.0, 105.0, 100.0);
    let stain = Entity::new(0x800, d);
    let world = server.world.world(0).unwrap();
    world.spawn(&stain, &server.view_ctx());
    trial_frames(&mut criminal.client);
    trial_frames(&mut reporter.client);

    // The criminal cannot report its own crime.
    send(&server, &mut criminal, cs::CSReportCrimePacket, |s| {
        s.write_bc(0x800)
            .write_u32(1)
            .write_i32(2)
            .write_u32(3)
            .write_str("me");
    })
    .await;
    assert!(trial_frames(&mut criminal.client).is_empty());
    assert!(hooks.phases.lock().unwrap().is_empty());

    // An arrested criminal also receives the new record.
    let tid = server
        .systems
        .social
        .trials
        .arrest_criminal(&server, &criminal.p, None)
        .unwrap();
    trial_frames(&mut criminal.client);

    send(&server, &mut reporter, cs::CSReportCrimePacket, |s| {
        s.write_bc(0x800)
            .write_u32(20001)
            .write_i32(4)
            .write_u32(55)
            .write_str("I saw it");
    })
    .await;
    let crimes = server.systems.social.crimes.crimes_of_player(1, false);
    assert_eq!(crimes.len(), 1);
    let e = &crimes[0];
    assert_eq!(
        (
            e.criminal,
            e.victim,
            e.reporter,
            e.crime_kind,
            e.crime_time,
            e.used_skill_id,
            e.doodad_next_func_group,
            e.doodad_func_id,
            e.msg.as_str()
        ),
        (
            1,
            3,
            2,
            crime_kind::MURDER,
            1_600_000_000,
            20001,
            4,
            55,
            "I saw it"
        )
    );
    assert_eq!(e.position, (110.0, 105.0, 100.0));
    let f = trial_frames(&mut criminal.client);
    assert_eq!(
        ops(&f),
        vec![sc::SCCrimeChangedPacket, sc::SCCrimeRecordsPacket]
    );
    assert_eq!(
        f[0].1,
        body(&SCCrimeChangedPacket {
            points: 25,
            crime_points: 25,
            infamy_points: 25,
            crime_state: 2
        })
    );
    let entry = server.systems.social.crimes.record_entry(&server, e);
    assert_eq!(entry.victim_name, "Victim");
    assert_eq!(entry.reporter_name.as_deref(), Some("Witness"));
    assert_eq!(
        f[1].1,
        body(&SCCrimeRecordsPacket {
            trial_id: tid,
            unknown1: 0,
            total_count: 1,
            this_count: 1,
            entries: vec![entry]
        })
    );
    assert_eq!(criminal.p.data().evidence_reported_count, 1);
    // Jail time recalculated: one murder of a level-10 victim.
    assert_eq!(
        server
            .systems
            .social
            .trials
            .get_trial(tid)
            .unwrap()
            .jail_time,
        200
    );
    assert_eq!(hooks.phases.lock().unwrap().clone(), vec![(0x800, 2, 7)]);
    assert_eq!(server.systems.social.crimes.pending_ids().0, vec![e.id]);

    // Bot reports.
    let crimes = &server.systems.social.crimes;
    assert!(crimes.report_bot(&server, &criminal.p, &reporter.p, "bot", 0));
    assert!(!crimes.report_bot(&server, &criminal.p, &reporter.p, "bot", 0));
    assert!(criminal
        .p
        .data()
        .unit
        .buffs
        .check_buff(buff::SUSPECTED_USER));
    assert_eq!(criminal.p.data().reported_as_bot_count, 1);
    assert_eq!(reporter.p.data().bot_reported_count, 1);
    // No tag data: the expiry finds no prime suspect buff.
    assert!(!crimes.report_bot_expired(&server, &criminal.p));
    assert!(crimes.report_bot_expired(&server, &reporter.p));
}

/// `CheckWantedThreshold` -> `Character.SetFaction(Pirate)` through the real crime report path:
/// the nearby witness receives `SCUnitFactionChanged` exactly once (the 1.2 client turns it into
/// "$1 became pirate.", x2game `sub_396A1C20`), and the house / guild / party effects of
/// `Character.SetFaction` run: an ordinary guild member (no `Expel` policy, unlike the C#
/// `ExpeditionManager.Kick` path) leaves the guild. A repeated `SetFaction(Pirate)` is then a
/// no-op.
#[tokio::test]
async fn infamy_threshold_sets_the_pirate_faction_with_its_side_effects() {
    use crate::systems::social::expedition::{self, Expedition};
    use crate::systems::social::packets::expedition::SCUnitExpeditionChangedPacket;
    use crate::systems::social::packets::faction::SCUnitFactionChangedPacket;

    let server = justice_server();
    server
        .systems
        .social
        .hooks
        .install(Arc::new(FakeHooks::default()));
    let mut criminal = player(&server, 1, "Crook", |c| c.infamy_point = 2_990);
    let mut reporter = player(&server, 2, "Witness", |_| {});
    let victim = player(&server, 3, "Victim", |_| {});

    // The criminal owns the party; the witness joins it.
    let teams = &server.systems.social.teams;
    teams.ask_to_join(&server, &criminal.p, "", 0, true, Some(&reporter.p));
    teams.reply_to_join_team(
        &server,
        &reporter.p,
        0,
        true,
        criminal.p.character_id,
        false,
        "",
        false,
    );
    assert!(criminal.p.data().in_party && reporter.p.data().in_party);

    // The witness owns a Nuian guild; the criminal is an ordinary member, whose role policy has
    // no Expel (the C# `Kick` path would leave such a member in the guild).
    let members = [(&criminal.p, 0u8), (&reporter.p, 255u8)]
        .into_iter()
        .map(|(p, role)| {
            p.data().unit.expedition_id = 77;
            let mut m = expedition::member_from_character(&server, 77, p, false);
            m.role = role;
            m
        })
        .collect();
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: reporter.p.character_id,
            owner_name: "Witness".into(),
            mother_id: NUIA,
            ..Default::default()
        },
        members,
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    criminal.client.drain();
    reporter.client.drain();

    // A murder report by the witness: +25 infamy crosses 3000.
    let mut d = Doodad::new(878);
    d.owner_type = doodad_owner_type::CHARACTER;
    d.owner_id = criminal.p.character_id;
    d.data = victim.p.character_id as i32;
    let evidence = Entity::new(0x900, d);
    let event = server
        .systems
        .social
        .crimes
        .report_crime(&server, &reporter.p, &evidence, 1, 0, 0, "seen")
        .await;
    assert!(event.is_some());
    wait_for("the guild removal", || {
        criminal.p.data().unit.expedition_id == 0
    })
    .await;

    let expected = SCUnitFactionChangedPacket {
        obj_id: criminal.p.obj_id,
        unit_name: "Crook".into(),
        id: 101,
        id2: 161,
        temp: false,
    };
    let frames = reporter.client.drain();
    assert_eq!(
        find(&frames, sc::SCUnitFactionChangedPacket),
        vec![body(&expected)],
        "the nearby witness sees exactly one faction change"
    );
    let criminal_frames = criminal.client.drain();
    assert_eq!(
        find(&criminal_frames, sc::SCUnitFactionChangedPacket),
        vec![body(&expected)],
        "the new pirate sees it too"
    );
    assert_eq!(criminal.p.data().unit.faction_id, 161);
    assert!(!criminal.p.data().in_party, "removed from the party");
    assert_eq!(
        criminal.p.data().unit.expedition_id,
        0,
        "removed from the guild"
    );
    // The guild keeps its owner and spells the ordinary member's removal as a normal leave.
    let guild = server.expeditions.get(77).expect("guild survives");
    assert!(!guild.is_disbanded);
    assert_eq!(guild.faction.owner_id, reporter.p.character_id);
    assert!(guild.member(criminal.p.character_id).is_none());
    assert!(guild.member(reporter.p.character_id).is_some());
    let leave = SCUnitExpeditionChangedPacket {
        obj_id: criminal.p.obj_id,
        character_id: criminal.p.character_id,
        kicker: String::new(),
        unit_name: "Crook".into(),
        id: 77,
        expedition_id: 0,
        expel: false,
    };
    assert!(
        find(&frames, sc::SCUnitExpeditionChangedPacket).contains(&body(&leave)),
        "the guild sees the member leave"
    );
    assert!(find(&criminal_frames, sc::SCUnitExpeditionChangedPacket).contains(&body(&leave)));

    // A later SetFaction(Pirate) is consistent: old == new, so no second broadcast.
    crate::systems::social::faction::set_faction(&server, &criminal.p, 161).await;
    assert!(find(&reporter.client.drain(), sc::SCUnitFactionChangedPacket).is_empty());
    assert_eq!(criminal.p.data().unit.faction_id, 161);
}

/// A dominion owned by a guild, with a scheduled siege (attacker 88, defender 77).
fn guild_dominion() -> crate::systems::housing::dominion::state::DominionState {
    use crate::systems::housing::dominion::state::{
        DominionState, SiegeOutcome, SiegeState, SiegeStatus, TowerIdentity,
    };
    DominionState {
        zone_group_id: 25,
        tower: TowerIdentity {
            house_id: 61,
            instance_id: 0,
            guard_tower_setting_id: 7,
        },
        owner_expedition_id: 77,
        ownership_generation: 1,
        reign_start_at: 100,
        claimed_at: 0,
        last_siege_end_at: 0,
        tax_rate: 0,
        national_tax_rate: 0,
        last_paid_at: 0,
        last_tax_rate_changed_at: 0,
        last_national_tax_rate_changed_at: 0,
        national_monument_db_id: 0,
        national_monument_x: 0.0,
        national_monument_y: 0.0,
        national_monument_z: 0.0,
        ephemeral: false,
        non_pvp_start_at: 0,
        non_pvp_duration_min: 0,
        treasury: Default::default(),
        tower_destroyed: false,
        tower_hp: None,
        last_siege_id: 1,
        declaration: None,
        auction_posted_cycle: 0,
        siege: Some(SiegeState {
            siege_id: 1,
            attacker_expedition_id: 88,
            defender_expedition_id: 77,
            ownership_generation: 1,
            status: SiegeStatus::Scheduled,
            outcome: SiegeOutcome::Pending,
            scheduled_at: 100,
            starts_at: 200,
            ends_at: 300,
            ended_at: 0,
            reinforced: false,
            defender_ni: 100,
            defender_nr: 100,
            outcome_credited: false,
            materialised_from_declaration: false,
            calendar_cycle: 0,
            participants: Default::default(),
        }),
    }
}

/// A nation founded by expedition 77, whose king is character 1.
fn guild_nation() -> crate::systems::social::nation::Nation {
    use crate::systems::social::nation::Nation;
    Nation {
        id: 170,
        expedition_id: 77,
        owner_character_id: 1,
        owner_name: "Owner".into(),
        name: "Test Nation".into(),
        created_at: 0,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    }
}

/// PIRATE-1 (owner decision 2026-11-03): a guild owner who becomes a pirate is removed like any
/// member. The leadership first passes to the remaining member with the highest role, then the
/// longest membership stand-in (the lowest character id), through the normal `ChangeOwner`
/// packets; the guild keeps its territory, siege registration and nation, and the nation's king
/// moves to the new owner.
#[tokio::test]
async fn a_pirate_guild_owner_hands_the_guild_to_the_next_member() {
    use crate::systems::social::expedition::{self, Expedition};
    use crate::systems::social::packets::expedition::{
        SCExpeditionOwnerChangedPacket, SCExpeditionRoleChangedPacket,
        SCUnitExpeditionChangedPacket,
    };
    use crate::systems::social::packets::faction::{
        SCFactionOwnerChangedPacket, SCUnitFactionChangedPacket,
    };

    let server = justice_server();
    server
        .systems
        .social
        .hooks
        .install(Arc::new(FakeHooks::default()));
    let mut owner = player(&server, 1, "Owner", |c| c.infamy_point = 2_990);
    let mut cmdr_old = player(&server, 2, "CmdrOld", |_| {});
    let mut cmdr_new = player(&server, 3, "CmdrNew", |_| {});
    let veteran = player(&server, 4, "Veteran", |_| {});
    let victim = player(&server, 5, "Victim", |_| {});

    // Highest role wins first: CmdrOld (character 2) is the lower id commander and must beat
    // CmdrNew (3); the lowest id overall (Owner, role 255) is excluded and the role-0 Veteran
    // must not win over a commander. `last_world_leave_time` is deliberately reversed so the
    // test proves the stable character-id stand-in is used, not the login-refreshed activity
    // timestamp.
    let members = [
        (&owner.p, 255u8, 0i64),
        (&cmdr_old.p, 3, 50),
        (&cmdr_new.p, 3, 10),
        (&veteran.p, 0, 0),
    ]
    .into_iter()
    .map(|(p, role, last_leave)| {
        p.data().unit.expedition_id = 77;
        let mut m = expedition::member_from_character(&server, 77, p, false);
        m.role = role;
        m.last_world_leave_time = last_leave;
        m
    })
    .collect();
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: owner.p.character_id,
            owner_name: "Owner".into(),
            // Guild mother = the creator's faction (101, as `create_expedition` stores it).
            mother_id: 101,
            ..Default::default()
        },
        members,
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    server
        .systems
        .housing
        .dominions
        .restore(vec![guild_dominion()])
        .expect("test dominion");
    server.systems.social.nations.add(guild_nation());
    owner.client.drain();
    cmdr_old.client.drain();
    cmdr_new.client.drain();

    let mut d = Doodad::new(878);
    d.owner_type = doodad_owner_type::CHARACTER;
    d.owner_id = owner.p.character_id;
    d.data = victim.p.character_id as i32;
    let evidence = Entity::new(0x900, d);
    let event = server
        .systems
        .social
        .crimes
        .report_crime(&server, &cmdr_old.p, &evidence, 1, 0, 0, "seen")
        .await;
    assert!(event.is_some());
    wait_for("the owner removal", || {
        owner.p.data().unit.expedition_id == 0
    })
    .await;

    // The owner is kicked like any member and the leadership moves to CmdrOld.
    assert_eq!(owner.p.data().unit.faction_id, 161);
    assert_eq!(owner.p.data().unit.expedition_id, 0);
    let guild = server.expeditions.get(77).expect("the guild survives");
    assert!(!guild.is_disbanded);
    assert_eq!(guild.faction.owner_id, cmdr_old.p.character_id);
    assert_eq!(guild.faction.owner_name, "CmdrOld");
    assert!(guild.member(owner.p.character_id).is_none());
    assert_eq!(guild.member(cmdr_old.p.character_id).unwrap().role, 255);
    assert_eq!(guild.member(cmdr_new.p.character_id).unwrap().role, 3);
    assert_eq!(guild.member(veteran.p.character_id).unwrap().role, 0);
    assert_eq!(
        cmdr_old.p.data().unit.expedition_id,
        77,
        "the new owner keeps the guild"
    );
    assert_eq!(cmdr_new.p.data().unit.expedition_id, 77);
    assert_eq!(veteran.p.data().unit.expedition_id, 77);

    // Territory, siege registration and the nation stay with the guild; the king follows.
    let dominion = server
        .systems
        .housing
        .dominions
        .get(25)
        .expect("territory kept");
    assert_eq!(dominion.owner_expedition_id, 77);
    assert_eq!(
        dominion.siege.as_ref().unwrap().status,
        crate::systems::housing::dominion::state::SiegeStatus::Scheduled
    );
    let nation = server.systems.social.nations.get(170).expect("nation kept");
    assert_eq!(nation.expedition_id, 77);
    assert_eq!(nation.owner_character_id, cmdr_old.p.character_id);
    assert_eq!(nation.owner_name, "CmdrOld");

    // The normal ChangeOwner transition is broadcast to the members, then the owner's leave.
    let frames = cmdr_old.client.drain();
    assert!(
        find(&frames, sc::SCExpeditionOwnerChangedPacket).contains(&body(
            &SCExpeditionOwnerChangedPacket {
                id: owner.p.character_id,
                id2: cmdr_old.p.character_id,
                char_name: "CmdrOld".into(),
            }
        )),
        "the members see the owner change"
    );
    assert!(
        find(&frames, sc::SCExpeditionRoleChangedPacket).contains(&body(
            &SCExpeditionRoleChangedPacket {
                id: owner.p.character_id,
                role: 0,
                char_name: "Owner".into(),
            }
        ))
    );
    assert!(
        find(&frames, sc::SCExpeditionRoleChangedPacket).contains(&body(
            &SCExpeditionRoleChangedPacket {
                id: cmdr_old.p.character_id,
                role: 255,
                char_name: "CmdrOld".into(),
            }
        ))
    );
    let leave = SCUnitExpeditionChangedPacket {
        obj_id: owner.p.obj_id,
        character_id: owner.p.character_id,
        kicker: String::new(),
        unit_name: "Owner".into(),
        id: 77,
        expedition_id: 0,
        expel: false,
    };
    assert!(
        find(&frames, sc::SCUnitExpeditionChangedPacket).contains(&body(&leave)),
        "the members see the old owner leave"
    );
    assert!(
        find(&frames, sc::SCUnitFactionChangedPacket).contains(&body(
            &SCUnitFactionChangedPacket {
                obj_id: owner.p.obj_id,
                unit_name: "Owner".into(),
                id: 101,
                id2: 161,
                temp: false,
            }
        ))
    );
    assert!(
        find(&frames, sc::SCFactionOwnerChangedPacket).contains(&body(
            &SCFactionOwnerChangedPacket {
                id: 170,
                id2: cmdr_old.p.character_id,
                new_owner_name: "CmdrOld".into(),
            }
        )),
        "the nation's new king is broadcast with the runtime faction-owner packet"
    );
    let owner_frames = owner.client.drain();
    assert!(find(&owner_frames, sc::SCUnitExpeditionChangedPacket).contains(&body(&leave)));
    assert!(
        find(&owner_frames, sc::SCExpeditionOwnerChangedPacket).contains(&body(
            &SCExpeditionOwnerChangedPacket {
                id: owner.p.character_id,
                id2: cmdr_old.p.character_id,
                char_name: "CmdrOld".into(),
            }
        )),
        "the old owner sees who inherited the guild"
    );
}

/// The owner and the successor turn pirate together. The hand-over and the old owner's removal
/// share one expedition write section, and a member whose live faction is not the guild mother
/// cannot inherit, so the guild keeps a live member as owner instead of ending with an absent
/// owner (the old two-step transfer handed it back to the already-pirate owner, whose own leave
/// then removed it from the member list).
#[tokio::test]
async fn simultaneous_owner_and_successor_pirate_changes_leave_a_consistent_guild() {
    use crate::systems::social::expedition::{self, Expedition};

    let server = justice_server();
    let owner = player(&server, 1, "Owner", |c| c.unit.faction_id = 101);
    let heir = player(&server, 2, "Heir", |c| c.unit.faction_id = 101);
    let member = player(&server, 3, "Member", |c| c.unit.faction_id = 101);
    let members = [(&owner.p, 255u8), (&heir.p, 0u8), (&member.p, 0u8)]
        .into_iter()
        .map(|(p, role)| {
            p.data().unit.expedition_id = 77;
            let mut m = expedition::member_from_character(&server, 77, p, false);
            m.role = role;
            m
        })
        .collect();
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: owner.p.character_id,
            owner_name: "Owner".into(),
            mother_id: 101,
            ..Default::default()
        },
        members,
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });

    let _ = tokio::join!(
        crate::systems::social::faction::set_faction(&server, &owner.p, 161),
        crate::systems::social::faction::set_faction(&server, &heir.p, 161),
    );

    assert_eq!(owner.p.data().unit.faction_id, 161);
    assert_eq!(heir.p.data().unit.faction_id, 161);
    assert_eq!(owner.p.data().unit.expedition_id, 0);
    assert_eq!(heir.p.data().unit.expedition_id, 0);
    let guild = server.expeditions.get(77).expect("guild row");
    assert!(!guild.is_disbanded, "a valid member remained");
    let new_owner = guild.faction.owner_id;
    let owner_member = guild
        .member(new_owner)
        .expect("the guild owner is still a member");
    assert_eq!(owner_member.role, 255);
    assert_eq!(
        new_owner, member.p.character_id,
        "the non-pirate member inherits, not the old pirate owner"
    );
    assert_eq!(member.p.data().unit.faction_id, 101);
    assert_eq!(member.p.data().unit.expedition_id, 77);
    assert!(guild.member(owner.p.character_id).is_none());
    assert!(guild.member(heir.p.character_id).is_none());
}

/// Only an owner who was the last member dismisses the guild. The voluntary disband guard (301 /
/// 469) cannot apply: no member would be left to hold the territory, so the forced path releases
/// the dominion and cancels its siege under the same `housing.operation_lock`, then dismisses.
#[tokio::test]
async fn a_pirate_guild_owner_who_was_the_last_member_dismisses_and_releases_the_dominion() {
    use crate::systems::social::expedition::{self, Expedition};
    use crate::systems::social::packets::expedition::SCExpeditionDismissedPacket;
    use crate::systems::social::packets::faction::SCFactionOwnerChangedPacket;

    let server = justice_server();
    server
        .systems
        .social
        .hooks
        .install(Arc::new(FakeHooks::default()));
    let mut owner = player(&server, 1, "Crook", |c| c.infamy_point = 2_990);
    let reporter = player(&server, 2, "Witness", |_| {});
    let victim = player(&server, 3, "Victim", |_| {});

    // The owner is the only member.
    owner.p.data().unit.expedition_id = 77;
    let mut member = expedition::member_from_character(&server, 77, &owner.p, true);
    member.role = 255;
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Knights".into(),
            owner_id: owner.p.character_id,
            owner_name: "Crook".into(),
            mother_id: NUIA,
            ..Default::default()
        },
        members: vec![member],
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    server
        .systems
        .housing
        .dominions
        .restore(vec![guild_dominion()])
        .expect("test dominion");
    server.systems.social.nations.add(guild_nation());
    owner.client.drain();

    // The voluntary disband refuses a territory / siege holder.
    assert!(
        !expedition::disband(&server, &owner.p).await,
        "the voluntary guard still refuses"
    );
    assert!(!server.expeditions.get(77).unwrap().is_disbanded);
    owner.client.drain();

    let mut d = Doodad::new(878);
    d.owner_type = doodad_owner_type::CHARACTER;
    d.owner_id = owner.p.character_id;
    d.data = victim.p.character_id as i32;
    let evidence = Entity::new(0x900, d);
    let event = server
        .systems
        .social
        .crimes
        .report_crime(&server, &reporter.p, &evidence, 1, 0, 0, "seen")
        .await;
    assert!(event.is_some());
    wait_for("the guild dismissal", || {
        server.expeditions.get(77).is_some_and(|g| g.is_disbanded)
    })
    .await;

    let dismissed = SCExpeditionDismissedPacket {
        id: 77,
        success: true,
    };
    let frames = owner.client.drain();
    assert!(
        find(&frames, sc::SCExpeditionDismissedPacket).contains(&body(&dismissed)),
        "the last member is told the guild was dismissed"
    );
    assert!(
        find(&frames, sc::SCFactionOwnerChangedPacket).contains(&body(
            &SCFactionOwnerChangedPacket {
                id: 170,
                id2: 0,
                new_owner_name: String::new(),
            }
        )),
        "the client is told the nation lost its king"
    );
    assert_eq!(owner.p.data().unit.faction_id, 161);
    assert_eq!(owner.p.data().unit.expedition_id, 0);
    let guild = server.expeditions.get(77).expect("retired guild row");
    assert!(guild.is_disbanded);
    assert_eq!(guild.faction.owner_id, 0);
    assert_eq!(guild.faction.name, "$deleted-guild-77");
    assert!(guild.members.is_empty());

    // The disbanded guild's dominion is released and its unfinished siege cancelled.
    let dominion = server
        .systems
        .housing
        .dominions
        .get(25)
        .expect("territory kept as unowned");
    assert_eq!(dominion.owner_expedition_id, 0);
    assert_eq!(dominion.reign_start_at, 0);
    let siege = dominion.siege.expect("siege kept as history");
    assert_eq!(
        siege.status,
        crate::systems::housing::dominion::state::SiegeStatus::Finished
    );
    assert_eq!(
        siege.outcome,
        crate::systems::housing::dominion::state::SiegeOutcome::Cancelled
    );
    assert!(siege.ended_at >= siege.scheduled_at);
    let nation = server.systems.social.nations.get(170).unwrap();
    assert_eq!(
        nation.expedition_id, 0,
        "the nation becomes kingless as on the normal disband"
    );
    assert_eq!(
        nation.owner_character_id, 0,
        "the pirate owner is no longer the king"
    );
    assert!(nation.owner_name.is_empty());
    assert!(
        server
            .systems
            .social
            .nations
            .owned_by(owner.p.character_id)
            .is_none(),
        "a pirate must not keep the king's invite / kick rights"
    );
}

/// A pirate guild (its mother faction is the pirate faction) keeps its members: the faction
/// change only removes a member when the guild's mother differs from the new faction.
#[tokio::test]
async fn a_pirate_guild_keeps_a_member_who_becomes_a_pirate() {
    use crate::systems::social::expedition::{self, Expedition};

    let server = justice_server();
    server
        .systems
        .social
        .hooks
        .install(Arc::new(FakeHooks::default()));
    let mut criminal = player(&server, 1, "Crook", |c| {
        c.infamy_point = 2_990;
        c.unit.faction_id = 101; // about to become a pirate
    });
    let reporter = player(&server, 2, "Owner", |c| c.unit.faction_id = 161);
    let victim = player(&server, 3, "Victim", |_| {});

    let members = [(&criminal.p, 0u8), (&reporter.p, 255u8)]
        .into_iter()
        .map(|(p, role)| {
            p.data().unit.expedition_id = 77;
            let mut m = expedition::member_from_character(&server, 77, p, false);
            m.role = role;
            m
        })
        .collect();
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            name: "Pirate Guild".into(),
            owner_id: reporter.p.character_id,
            owner_name: "Owner".into(),
            mother_id: 161,
            ..Default::default()
        },
        members,
        policies: expedition::default_policies(&server, 77),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    criminal.client.drain();

    let mut d = Doodad::new(878);
    d.owner_type = doodad_owner_type::CHARACTER;
    d.owner_id = criminal.p.character_id;
    d.data = victim.p.character_id as i32;
    let evidence = Entity::new(0x900, d);
    let event = server
        .systems
        .social
        .crimes
        .report_crime(&server, &reporter.p, &evidence, 1, 0, 0, "seen")
        .await;
    assert!(event.is_some());

    assert_eq!(criminal.p.data().unit.faction_id, 161);
    assert_eq!(
        criminal.p.data().unit.expedition_id,
        77,
        "a pirate guild keeps its pirate member"
    );
    let guild = server.expeditions.get(77).expect("pirate guild survives");
    assert!(!guild.is_disbanded);
    assert!(guild.member(criminal.p.character_id).is_some());
    assert_eq!(guild.faction.owner_id, reporter.p.character_id);
    assert_eq!(guild.member(reporter.p.character_id).unwrap().role, 255);
}

/// A pirate who is duelling shows the temporary Red / Blue team faction. The infamy threshold
/// must use the saved faction: the old live-faction check re-fired `SetFaction(Pirate)`, which
/// ended the temporary faction (breaking the duel) and removed the pirate from the party.
#[tokio::test]
async fn a_duelling_pirates_crime_report_keeps_the_duel_and_the_party() {
    let server = justice_server();
    server
        .systems
        .social
        .hooks
        .install(Arc::new(FakeHooks::default()));
    let mut pirate = player(&server, 1, "Pirate", |c| {
        c.infamy_point = 2_990;
        c.unit.faction_id = 161;
        c.unit.hp = 1_000;
    });
    let mut rival = player(&server, 2, "Rival", |c| {
        c.unit.faction_id = 161;
        c.unit.hp = 1_000;
    });
    let victim = player(&server, 3, "Victim", |_| {});

    let teams = &server.systems.social.teams;
    teams.ask_to_join(&server, &pirate.p, "", 0, true, Some(&rival.p));
    teams.reply_to_join_team(
        &server,
        &rival.p,
        0,
        true,
        pirate.p.character_id,
        false,
        "",
        false,
    );
    assert!(pirate.p.data().in_party && rival.p.data().in_party);

    let duels = &server.systems.social.duels;
    duels.duel_request(&server, &pirate.p, rival.p.character_id);
    duels
        .duel_accepted(&server, &rival.p, pirate.p.character_id)
        .await;
    assert!(pirate.p.data().unit.faction_temporary);
    let duel_faction = pirate.p.data().unit.faction_id;
    assert_ne!(duel_faction, 161);
    pirate.client.drain();
    rival.client.drain();

    // The rival reports the pirate's crime; +25 infamy crosses 3000 while the saved faction is
    // already Pirate.
    let mut d = Doodad::new(878);
    d.owner_type = doodad_owner_type::CHARACTER;
    d.owner_id = pirate.p.character_id;
    d.data = victim.p.character_id as i32;
    let evidence = Entity::new(0x900, d);
    let event = server
        .systems
        .social
        .crimes
        .report_crime(&server, &rival.p, &evidence, 1, 0, 0, "seen")
        .await;
    assert!(event.is_some());

    assert_eq!(
        pirate.p.data().unit.faction_id,
        duel_faction,
        "the duel's temporary faction is untouched"
    );
    assert!(pirate.p.data().unit.faction_temporary);
    assert!(pirate.p.data().in_party, "the pirate stays in the party");
    assert!(
        duels.duel_of(pirate.p.character_id).is_some(),
        "the duel is still running"
    );
    assert!(
        find(&rival.client.drain(), sc::SCUnitFactionChangedPacket).is_empty(),
        "no second SetFaction(Pirate) broadcast"
    );
}

/// The guilty verdict runs while the trial mutex is held; its faction change is collected as an
/// `Effect` and runs after the lock is released (trial.rs module rule), through the same shared
/// helper as every other crime threshold. The public call returns once that effect has run.
#[tokio::test]
async fn the_guilty_verdict_sets_the_pirate_faction_after_the_trial_lock() {
    let (server, d, _jury, tid) = summoned_trial(1).await;
    d.p.data().infamy_point = crate::systems::social::crime::PIRATE_CRIME_POINT_THRESHOLD;
    server
        .systems
        .social
        .trials
        .result_is_guilty(&server, &d.p, tid, true, util::now_ms());
    assert_eq!(
        d.p.data().unit.faction_id,
        161,
        "the effect ran after the trial lock was released"
    );
}

/// A pirate not-guilty verdict whose infamy reaches 0 returns to the race/gender template
/// faction through the same shared helper as every other infamy decrease (the old trial-only
/// special case is gone). The 1.2 client prints ui_text 2297 from this faction change.
#[tokio::test]
async fn not_guilty_verdict_pardons_a_pirate_at_zero_infamy() {
    use crate::systems::social::packets::faction::SCUnitFactionChangedPacket;

    let server = justice_server();
    let trials = &server.systems.social.trials;
    trials.initialize_doodads(&[("justice.1.seat.1".into(), seat(0x900, 400.0))]);
    let mut d = player(&server, 1, "Pirate", |c| {
        c.unit.faction_id = 161;
        c.crime_point = 70;
        c.infamy_point = 70;
    });
    let mut j = player(&server, 2, "Juror", |c| c.jury_point = 3);
    trials.update_jury_queues(&server);
    let tid = trials.arrest_criminal(&server, &d.p, None).unwrap();
    trials.reply_imprison_or_trial(&server, &d.p, true, util::now_ms());
    trials.update_tick(&server, util::now_ms() + 1).await;
    assert!(trials.process_trial_invite_reply(&server, &j.p, true, tid, util::now_ms()));
    trials.update_tick(&server, util::now_ms() + 1).await; // ConfirmCriminalRecord
    trials.jury_end_testimony(&j.p, tid, 1, util::now_ms());
    trials.update_tick(&server, util::now_ms() + 1).await; // ClosingStatement
    trials.update_tick(&server, util::now_ms() + 61_000).await; // JuryVerdict
    trial_frames(&mut d.client);
    trial_frames(&mut j.client);
    trials.jury_verdict(&j.p, tid, 1, 1, util::now_ms());
    trial_frames(&mut d.client);
    trials.update_tick(&server, util::now_ms() + 1).await;

    assert_eq!(d.p.data().infamy_point, 0);
    assert_eq!(d.p.data().unit.faction_id, 101);
    assert_eq!(crate::db_character::persisted_faction(&d.p.data()), 101);
    let frames = trial_frames(&mut d.client);
    let expected = crate::systems::social::packets::body(&SCUnitFactionChangedPacket {
        obj_id: 0x101,
        unit_name: "Pirate".into(),
        id: 161,
        id2: 101,
        temp: false,
    });
    assert!(
        frames
            .iter()
            .any(|(op, body)| *op == sc::SCUnitFactionChangedPacket && *body == expected),
        "the trial pardon must broadcast the faction change: {frames:x?}"
    );
}
