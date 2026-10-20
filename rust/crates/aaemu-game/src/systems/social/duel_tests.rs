//! Duel flow tests: two connections and a fake doodad subsystem that spawns the combat flag. The
//! timers are set to one hour so the tests drive the task steps directly; one test runs the real
//! timers with short delays.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use super::*;
use crate::models::doodad::Doodad;
use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::systems::social::hooks::SocialHooks;
use crate::test_support::{spawn_player, test_character, test_server, TestClient, TEST_ZONE};
use crate::world::Entity;

/// Spawns a plain combat flag doodad; the terrain is flat at Z = 90.
struct FlagHooks;

impl SocialHooks for FlagHooks {
    fn spawn_doodad(
        &self,
        server: &GameServer,
        instance_id: u32,
        template_id: u32,
        pos: (f32, f32, f32),
        rot_z: f32,
    ) -> Option<DoodadRef> {
        let mut d = Doodad::new(template_id);
        d.transform.set_xyz(pos.0, pos.1, pos.2);
        d.transform.rot_z = rot_z;
        d.transform.instance_id = instance_id;
        d.transform.zone_id = TEST_ZONE;
        d.func_group_id = 77;
        let e = Entity::new(0x5000, d);
        let w = server.world.world(instance_id)?;
        w.spawn(&e, &server.view_ctx());
        Some(e)
    }

    fn terrain_height(&self, _: &GameServer, _: u32, _: f32, _: f32) -> Option<f32> {
        Some(90.0)
    }
}

struct Fixture {
    server: Arc<GameServer>,
    a: TestClient,
    b: TestClient,
    pa: PlayerRef,
    pb: PlayerRef,
}

impl Fixture {
    fn new(with_flag: bool) -> Self {
        Self::with_delays(
            with_flag,
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        )
    }

    fn with_delays(with_flag: bool, start: Duration, check: Duration) -> Self {
        let server = Arc::new(test_server());
        server.set_self();
        server.systems.social.duels.set_delays(DuelDelays {
            start,
            request: Duration::from_secs(3600),
            check,
            duration: Duration::from_secs(3600),
        });
        if with_flag {
            server.systems.social.hooks.install(Arc::new(FlagHooks));
        }
        let mut a = TestClient::new(&server, 1, 1);
        let mut b = TestClient::new(&server, 2, 2);
        let pa = spawn_player(
            &server,
            &a,
            test_character(1, "Alice", 1000.0, 1000.0),
            0x100,
        );
        let pb = spawn_player(&server, &b, test_character(2, "Bob", 1010.0, 1000.0), 0x101);
        for p in [&pa, &pb] {
            p.data().unit.hp = 1000;
        }
        a.conn.active_char = Some(pa.clone());
        b.conn.active_char = Some(pb.clone());
        a.drain();
        b.drain();
        Self {
            server,
            a,
            b,
            pa,
            pb,
        }
    }

    fn duels(&self) -> &DuelManager {
        &self.server.systems.social.duels
    }

    async fn challenge(&mut self) {
        let body = 2u32.to_le_bytes();
        self.server
            .handle_frame(
                &mut self.a.conn,
                &encode_raw(1, cs::CSChallengeDuelPacket, &body),
            )
            .await
            .unwrap();
    }

    async fn answer(&mut self, error: i16) {
        let mut s = PacketStream::new();
        s.write_u32(1).write_i16(error);
        self.server
            .handle_frame(
                &mut self.b.conn,
                &encode_raw(1, cs::CSStartDuelPacket, s.as_bytes()),
            )
            .await
            .unwrap();
    }

    /// Challenge + accept + `DuelStartTask`.
    async fn start(&mut self) {
        self.challenge().await;
        self.answer(0).await;
        self.duels().duel_start(&self.server, 1);
        self.a.drain();
        self.b.drain();
    }

    fn duel(&self) -> DuelRef {
        self.duels().get(1).unwrap()
    }
}

/// Waits (real time, up to 5 s) until `client` received a frame with `op`.
async fn wait_for(client: &mut TestClient, op: u16) -> Vec<(u16, Vec<u8>)> {
    let mut frames = Vec::new();
    for _ in 0..500 {
        frames.extend(client.drain());
        if frames.iter().any(|f| f.0 == op) {
            return frames;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no frame 0x{op:03x}");
}

fn ended(challenger_id: u32, challenged_id: u32, a_obj: u32, b_obj: u32, det: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(challenger_id)
        .write_u32(challenged_id)
        .write_bc(a_obj)
        .write_bc(b_obj)
        .write_u8(det);
    s.into_bytes()
}

/// The error ids of the `SCErrorMsgPacket` frames.
fn err_ids(frames: &[(u16, Vec<u8>)]) -> Vec<i16> {
    find(frames, sc::SCErrorMsgPacket)
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

fn ops(frames: &[(u16, Vec<u8>)]) -> Vec<u16> {
    frames.iter().map(|f| f.0).collect()
}

fn find(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|f| f.0 == op)
        .map(|f| f.1.clone())
        .collect()
}

#[tokio::test]
async fn challenge_accept_start_and_hp_win() {
    let mut f = Fixture::new(true);
    f.challenge().await;
    assert!(f.a.drain().is_empty());
    assert_eq!(
        f.b.drain(),
        vec![(sc::SCDuelChallengedPacket, vec![1, 0, 0, 0])]
    );
    assert!(f.duels().is_in_duel_with(1, 2));
    assert!(!f.duels().duel_of(2).unwrap().started);

    // Accept: flag spawned between both, Red / Blue team factions.
    f.answer(0).await;
    let info = f.duels().duel_of(1).unwrap();
    assert!(info.started && info.flag_obj_id != 0);
    let flag = f.duels().get(1).unwrap().state().flag.clone().unwrap();
    {
        let d = flag.data();
        assert_eq!(
            (d.template_id, d.transform.x, d.transform.y, d.transform.z),
            (COMBAT_FLAG_TEMPLATE_ID, 1005.0, 1000.0, 90.0)
        );
    }
    assert_eq!(f.pa.data().unit.faction_id, FACTION_RED_TEAM);
    assert_eq!(f.pb.data().unit.faction_id, FACTION_BLUE_TEAM);
    assert!(f.pa.data().unit.is_in_duel && f.pb.data().unit.is_in_duel);
    assert_eq!(f.duels().saved_faction(1), Some(101));
    assert_eq!(f.duels().saved_faction(2), Some(101));
    let fa = f.a.drain();
    assert!(f
        .server
        .world
        .world(0)
        .unwrap()
        .doodad(flag.obj_id)
        .is_some());
    assert_eq!(find(&fa, sc::SCUnitFactionChangedPacket).len(), 2);
    // The countdown starts at the acceptance (10.x order), before the start task fires.
    assert_eq!(
        find(&fa, sc::SCDuelStartCountdownPacket),
        vec![Vec::<u8>::new()]
    );
    let fb = f.b.drain();
    assert_eq!(find(&fb, sc::SCUnitFactionChangedPacket).len(), 2);
    assert_eq!(find(&fb, sc::SCDuelStartCountdownPacket).len(), 1);
    assert!(find(&fa, sc::SCDuelStartedPacket).is_empty());

    // A second acceptance is ignored.
    f.answer(0).await;
    assert!(f.a.drain().is_empty());

    // DuelStartTask.
    assert!(f.duel().state().start_task.is_some());
    f.duels().duel_start(&f.server, 1);
    let fa = f.a.drain();
    let fb = f.b.drain();
    use crate::opcodes::sc::*;
    assert_eq!(
        ops(&fa),
        vec![
            SCDuelStartedPacket,
            SCAreaChatBubblePacket,
            SCDuelStatePacket,
            SCDuelStatePacket,
            SCDoodadPhaseChangedPacket,
            SCCombatEngagedPacket,
            SCCombatEngagedPacket,
        ]
    );
    let mut expected_b = ops(&fa);
    expected_b.remove(4);
    assert_eq!(ops(&fb), expected_b);
    let fo = flag.obj_id;
    assert_eq!(fa[0].1, vec![0, 1, 0, 1, 1, 0]);
    assert_eq!(fa[1].1, vec![1, 0, 1, 0, 0x1F, 2, 0, 0]);
    let state = |obj: u32, flag: u32| {
        let mut s = PacketStream::new();
        s.write_bc(obj).write_bc(flag);
        s.into_bytes()
    };
    assert_eq!(fa[2].1, state(0x100, fo));
    assert_eq!(fa[3].1, state(0x101, fo));
    let mut phase = PacketStream::new();
    phase
        .write_bc(fo)
        .write_u32(77)
        .write_u32(0)
        .write_i32(-1)
        .write_u32(0);
    assert_eq!(fa[4].1, phase.into_bytes());
    assert_eq!(fa[5].1, vec![0, 1, 0]);
    assert_eq!(fa[6].1, vec![1, 1, 0]);
    {
        let d = f.duels().get(1).unwrap();
        let st = d.state();
        assert!(st.end_timer_task.is_some());
        assert!(st.distance_check_task.is_some());
        assert!(st.result_check_task.is_some());
        assert!(st.start_task.is_some()); // only cleared by DuelCleanUp
    }

    // Checks while both are fine: nothing is sent.
    let duel = f.duel();
    f.duels().result_check_task_execute(&f.server, &duel).await;
    f.duels()
        .distance_check_task_execute(&f.server, &duel)
        .await;
    assert!(f.a.drain().is_empty());
    assert!(duel.state().result_check_task.is_some());

    // Bob drops to 1 HP: Alice wins at the next check.
    f.pb.data().unit.hp = 1;
    f.duels().result_check_task_execute(&f.server, &duel).await;
    let fa = f.a.drain();
    let fb = f.b.drain();
    for frames in [&fa, &fb] {
        assert_eq!(
            find(frames, SCDuelEndedPacket),
            vec![ended(1, 2, 0x100, 0x101, duel_det_type::WIN)]
        );
        assert_eq!(
            find(frames, SCDuelStatePacket),
            vec![state(0x101, 0), state(0x100, 0)]
        );
        let mut removed = PacketStream::new();
        removed.write_bc(fo).write_bool(false);
        assert!(find(frames, SCDoodadRemovedPacket).contains(&removed.into_bytes()));
    }
    assert_eq!(fa[0].0, SCDuelEndedPacket);
    assert_eq!(f.pa.data().unit.faction_id, 101);
    assert_eq!(f.pb.data().unit.faction_id, 101);
    assert!(!f.pa.data().unit.is_in_duel && !f.pb.data().unit.is_in_duel);
    assert!(f.duels().is_empty());
    assert_eq!(f.duels().saved_faction(1), None);
    assert!(f.server.world.world(0).unwrap().doodad(fo).is_none());

    // The stale distance check task fires once more and only logs.
    assert!(duel.state().distance_check_task.is_some());
    f.duels()
        .distance_check_task_execute(&f.server, &duel)
        .await;
    assert!(f.a.drain().is_empty());
}

#[tokio::test]
async fn running_away_from_the_flag_surrenders() {
    let mut f = Fixture::new(true);
    f.start().await;
    // Bob runs 80 m away from the flag at (1005, 1000, 90).
    f.pb.data().unit.transform.set_xyz(1085.0, 1000.0, 90.0);
    let duel = f.duel();
    f.duels()
        .distance_check_task_execute(&f.server, &duel)
        .await;
    for frames in [f.a.drain(), f.b.drain()] {
        assert_eq!(
            find(&frames, sc::SCDuelEndedPacket),
            vec![ended(1, 2, 0x100, 0x101, duel_det_type::SURRENDER)]
        );
    }
    assert!(f.duels().is_empty());

    // Alice running away: Bob wins.
    let mut f = Fixture::new(true);
    f.start().await;
    f.pa.data().unit.transform.set_xyz(1005.0, 1000.0, 170.0); // 3D distance
    let duel = f.duel();
    f.duels()
        .distance_check_task_execute(&f.server, &duel)
        .await;
    assert_eq!(
        find(&f.b.drain(), sc::SCDuelEndedPacket),
        vec![ended(2, 1, 0x101, 0x100, duel_det_type::SURRENDER)]
    );
}

#[tokio::test]
async fn end_timer_draw_and_hp_win() {
    let mut f = Fixture::new(true);
    f.start().await;
    let duel = f.duel();
    f.duels().end_timer_task_execute(&f.server, &duel, 1).await;
    // Draw: each side gets itself as the "challenged" side.
    assert_eq!(
        find(&f.b.drain(), sc::SCDuelEndedPacket),
        vec![ended(1, 2, 0x100, 0x101, duel_det_type::DRAW)]
    );
    assert_eq!(
        find(&f.a.drain(), sc::SCDuelEndedPacket),
        vec![ended(2, 1, 0x101, 0x100, duel_det_type::DRAW)]
    );
    assert!(f.duels().is_empty());
    // The end timer was taken: a second execution does nothing.
    f.duels().end_timer_task_execute(&f.server, &duel, 1).await;
    assert!(f.a.drain().is_empty());

    // The side with more HP wins.
    let mut f = Fixture::new(true);
    f.start().await;
    f.pa.data().unit.hp = 500;
    f.pb.data().unit.hp = 600;
    let duel = f.duel();
    f.duels().end_timer_task_execute(&f.server, &duel, 1).await;
    assert_eq!(
        find(&f.a.drain(), sc::SCDuelEndedPacket),
        vec![ended(2, 1, 0x101, 0x100, duel_det_type::WIN)]
    );
    assert!(f.duels().is_empty());
}

#[tokio::test]
async fn refusal_sends_the_error_to_the_challenger() {
    let mut f = Fixture::new(true);
    f.challenge().await;
    f.b.drain();
    f.answer(util::error::TARGET_REJECTED_DUEL).await;
    let fa = f.a.drain();
    assert_eq!(ops(&fa), vec![sc::SCErrorMsgPacket]);
    assert_eq!(
        fa[0].1,
        crate::systems::social::packets::body(&crate::packets::world::SCErrorMsgPacket::new(
            507, 0, true
        ))
    );
    assert!(f.b.drain().is_empty());
    assert!(f.duels().is_empty());
    assert!(!f.pa.data().unit.is_in_duel);
    // Cancelling an unknown duel only logs.
    f.answer(507).await;
    assert!(f.a.drain().is_empty());
}

#[tokio::test]
async fn without_doodads_the_duel_runs_without_a_flag() {
    let mut f = Fixture::new(false);
    f.challenge().await;
    f.answer(0).await;
    f.a.drain();
    f.b.drain();
    f.duels().duel_start(&f.server, 1);
    let fa = f.a.drain();
    assert!(!ops(&fa).contains(&sc::SCDoodadPhaseChangedPacket));
    assert_eq!(find(&fa, sc::SCDuelStatePacket)[0], vec![0, 1, 0, 0, 0, 0]);
    // The distance check uses the challenger's position at the acceptance.
    f.pb.data().unit.transform.set_xyz(1074.0, 1000.0, 100.0);
    assert_eq!(
        f.duels().duel_distance_check(&f.server, 1),
        DuelDistance::Near
    );
    f.pb.data().unit.transform.set_xyz(1075.0, 1000.0, 100.0);
    assert_eq!(
        f.duels().duel_distance_check(&f.server, 1),
        DuelDistance::ChallengedFar
    );
    assert!(f
        .duels()
        .get(1)
        .unwrap()
        .state()
        .distance_check_task
        .is_none());
    // A second "far" check without a task still reports the side.
    assert_eq!(
        f.duels().duel_distance_check(&f.server, 1),
        DuelDistance::ChallengedFar
    );
    assert_eq!(
        f.duels().duel_distance_check(&f.server, 9),
        DuelDistance::Error
    );
}

#[tokio::test]
async fn result_check_quirks() {
    let mut f = Fixture::new(true);
    f.challenge().await;
    f.answer(0).await;
    // A side at <= 1 HP before the first check: the C# throws (no task yet) -> false.
    f.pb.data().unit.hp = 1;
    assert!(!f.duels().duel_result_check(&f.server, 1));
    f.pb.data().unit.hp = 1000;
    assert!(!f.duels().duel_result_check(&f.server, 1));
    f.pb.data().unit.hp = 0;
    assert!(f.duels().duel_result_check(&f.server, 1));
    assert!(f
        .duels()
        .get(1)
        .unwrap()
        .state()
        .result_check_task
        .is_none());
    assert!(!f.duels().duel_result_check(&f.server, 42));
}

#[tokio::test]
async fn unknown_challenged_character_is_ignored() {
    let mut f = Fixture::new(false);
    let body = 99u32.to_le_bytes();
    f.server
        .handle_frame(
            &mut f.a.conn,
            &encode_raw(1, cs::CSChallengeDuelPacket, &body),
        )
        .await
        .unwrap();
    assert!(f.duels().is_empty());
    // The challenger is told the target is invalid (10.x `BadDuelTarget`).
    assert_eq!(err_ids(&f.a.drain()), vec![util::error::BAD_DUEL_TARGET]);
}

/// RPA-09: the Red / Blue team factions are temporary. They carry `temp = true`, do not touch
/// the guild (a guild of another mother faction used to expel its own member) or the party, and
/// the origin faction is restored when the duel ends.
#[tokio::test]
async fn temporary_team_factions_keep_guild_and_restore() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::{Expedition, ExpeditionMember, ExpeditionRolePolicy};
    use crate::systems::social::packets::body;
    use crate::systems::social::packets::faction::SCUnitFactionChangedPacket;

    let mut f = Fixture::new(true);
    // Nuia guild: RedTeam (159) is not its mother faction, so the old SetFaction path would
    // expel both members through their officer roles.
    let member = |id: u32, name: &str, role: u8| ExpeditionMember {
        expedition_id: 77,
        character_id: id,
        in_party: false,
        is_online: true,
        last_world_leave_time: 0,
        name: name.into(),
        level: 10,
        zone_id: 0,
        id3: 0,
        abilities: [0; 3],
        role,
        position: (0.0, 0.0, 0.0),
        memo: String::new(),
        transfer_requested_time: 0,
    };
    let policy = |role: u8| ExpeditionRolePolicy {
        expedition_id: 77,
        role,
        name: format!("role {role}"),
        dominion_declare: false,
        invite: true,
        expel: true,
        promote: true,
        dismiss: true,
        chat: true,
        manager_chat: false,
        siege_master: false,
        join_siege: false,
    };
    f.server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 77,
            mother_id: 148,
            name: "Knights".into(),
            owner_id: 1,
            owner_name: "Alice".into(),
            ..SystemFaction::default()
        },
        members: vec![member(1, "Alice", 255), member(2, "Bob", 2)],
        policies: vec![policy(255), policy(2)],
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    f.pa.data().unit.expedition_id = 77;
    f.pb.data().unit.expedition_id = 77;

    f.challenge().await;
    f.b.drain();
    f.answer(0).await;
    let fa = f.a.drain();
    assert!(find(&fa, sc::SCUnitFactionChangedPacket).contains(&body(
        &SCUnitFactionChangedPacket {
            obj_id: 0x100,
            unit_name: "Alice".into(),
            id: 101,
            id2: FACTION_RED_TEAM,
            temp: true,
        }
    )));
    assert!(
        find(&f.b.drain(), sc::SCUnitFactionChangedPacket).contains(&body(
            &SCUnitFactionChangedPacket {
                obj_id: 0x101,
                unit_name: "Bob".into(),
                id: 101,
                id2: FACTION_BLUE_TEAM,
                temp: true,
            }
        ))
    );
    // The guild and its roles are untouched.
    let e = f.server.expeditions.get(77).expect("guild");
    assert_eq!(e.member_ids(), vec![1, 2]);
    assert_eq!(e.member(2).unwrap().role, 2);
    assert!(e.removed_members.is_empty());
    assert_eq!(f.pa.data().unit.expedition_id, 77);
    assert_eq!(f.pb.data().unit.expedition_id, 77);
    assert!(f.pa.data().unit.faction_temporary && f.pb.data().unit.faction_temporary);

    // Duel end restores both factions and clears the temporary marker.
    f.duels()
        .duel_stop(&f.server, 1, duel_det_type::DRAW, 2)
        .await;
    for p in [&f.pa, &f.pb] {
        assert_eq!(p.data().unit.faction_id, 101);
        assert!(!p.data().unit.faction_temporary);
    }
    assert_eq!(
        f.server.expeditions.get(77).unwrap().member_ids(),
        vec![1, 2]
    );
    assert!(f
        .server
        .expeditions
        .get(77)
        .unwrap()
        .removed_members
        .is_empty());
}

/// RPA-09: a disconnect during the duel restores the faction before the departure save, so a
/// relog / crash recovery never sees the Red / Blue team faction.
#[tokio::test]
async fn disconnect_during_a_duel_restores_the_origin_faction() {
    let mut f = Fixture::new(true);
    f.challenge().await;
    f.b.drain();
    f.answer(0).await;
    f.a.drain();
    f.b.drain();
    assert_eq!(f.pa.data().unit.faction_id, FACTION_RED_TEAM);

    crate::world::session::on_disconnect(&f.server, &mut f.a.conn).await;
    assert_eq!(f.pa.data().unit.faction_id, 101);
    assert!(!f.pa.data().unit.faction_temporary);
    // The save path also restores, so a clean logout (leave world) behaves the same.
    f.pb.data().unit.faction_id = FACTION_BLUE_TEAM;
    f.pb.data().unit.faction_temporary = true;
    f.pb.data().unit.origin_faction_id = 101;
    crate::world::session::save_and_remove_from_world(&f.server, Some(&f.pb)).await;
    assert_eq!(f.pb.data().unit.faction_id, 101);
    assert!(!f.pb.data().unit.faction_temporary);
}

/// A participant leaving mid-duel ends the duel for both sides: the opponent gets the
/// surrender `SCDuelEndedPacket`, is restored to its faction and the duel is unregistered.
#[tokio::test]
async fn disconnect_mid_duel_ends_the_duel_for_the_opponent() {
    for challenger_leaves in [true, false] {
        let mut f = Fixture::new(true);
        f.start().await;
        assert_eq!(f.pb.data().unit.faction_id, FACTION_BLUE_TEAM);
        if challenger_leaves {
            crate::world::session::on_disconnect(&f.server, &mut f.a.conn).await;
            assert_eq!(
                find(&f.b.drain(), sc::SCDuelEndedPacket),
                vec![ended(2, 1, 0x101, 0x100, duel_det_type::SURRENDER)]
            );
        } else {
            crate::world::session::on_disconnect(&f.server, &mut f.b.conn).await;
            assert_eq!(
                find(&f.a.drain(), sc::SCDuelEndedPacket),
                vec![ended(1, 2, 0x100, 0x101, duel_det_type::SURRENDER)]
            );
        }
        for p in [&f.pa, &f.pb] {
            assert_eq!(p.data().unit.faction_id, 101);
            assert!(!p.data().unit.faction_temporary);
            assert!(!p.data().unit.is_in_duel);
        }
        assert!(f.duels().is_empty());
    }
}

/// A pending (unanswered) challenge is dropped when a participant leaves.
#[tokio::test]
async fn disconnect_with_a_pending_challenge_drops_it() {
    let mut f = Fixture::new(true);
    f.challenge().await;
    assert!(!f.duels().is_empty());
    crate::world::session::on_disconnect(&f.server, &mut f.b.conn).await;
    assert!(f.duels().is_empty());
}

/// A duel inside a battlefield: the duel end returns to the corps faction, not to the origin
/// faction, and the battlefield leave then restores the origin.
#[tokio::test]
async fn duel_inside_a_temporary_faction_returns_to_it() {
    let mut f = Fixture::new(true);
    {
        let mut c = f.pa.data();
        c.unit.faction_id = 160;
        c.unit.origin_faction_id = 101;
        c.unit.faction_temporary = true;
    }
    f.challenge().await;
    f.b.drain();
    f.answer(0).await;
    assert_eq!(f.pa.data().unit.faction_id, FACTION_RED_TEAM);
    assert_eq!(f.pa.data().unit.origin_faction_id, 101);
    f.duels()
        .duel_stop(&f.server, 1, duel_det_type::DRAW, 2)
        .await;
    assert_eq!(f.pa.data().unit.faction_id, 160);
    assert!(f.pa.data().unit.faction_temporary);
    crate::systems::social::faction::restore_temporary_faction(&f.server, &f.pa);
    assert_eq!(f.pa.data().unit.faction_id, 101);
    assert!(!f.pa.data().unit.faction_temporary);
}

#[tokio::test]
async fn real_timers_start_and_end_the_duel() {
    let mut f = Fixture::with_delays(true, Duration::from_millis(20), Duration::from_millis(20));
    f.challenge().await;
    f.answer(0).await;
    let fa = wait_for(&mut f.a, sc::SCCombatEngagedPacket).await;
    assert!(ops(&fa).contains(&sc::SCDuelStartedPacket));
    f.pa.data().unit.hp = 1;
    let fb = wait_for(&mut f.b, sc::SCDuelEndedPacket).await;
    assert_eq!(
        find(&fb, sc::SCDuelEndedPacket),
        vec![ended(2, 1, 0x101, 0x100, duel_det_type::WIN)]
    );
    for _ in 0..500 {
        if f.duels().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(f.duels().is_empty());
    assert_eq!(f.pa.data().unit.faction_id, 101);
}

/// Fix 1: SCDuelStartCountdown at the acceptance, SCDuelStarted `start` later (real timers).
#[tokio::test]
async fn countdown_comes_first_and_started_follows_after_the_delay() {
    let mut f = Fixture::with_delays(true, Duration::from_millis(150), Duration::from_secs(3600));
    f.challenge().await;
    f.b.drain();
    f.answer(0).await;
    let fa = f.a.drain();
    assert_eq!(find(&fa, sc::SCDuelStartCountdownPacket).len(), 1);
    assert!(find(&fa, sc::SCDuelStartedPacket).is_empty());
    let t0 = std::time::Instant::now();
    let fa = wait_for(&mut f.a, sc::SCDuelStartedPacket).await;
    assert!(t0.elapsed() >= Duration::from_millis(100));
    // The countdown is not repeated by the start.
    assert!(find(&fa, sc::SCDuelStartCountdownPacket).is_empty());
}

/// Fix 2: a refuse that arrives after the duel started is ignored (no half state); the duel
/// then ends normally and restores factions and flag.
#[tokio::test]
async fn refuse_after_start_is_ignored() {
    let mut f = Fixture::new(true);
    f.start().await;
    let flag = f.duel().state().flag.clone().unwrap();
    for (who, error) in [(1, 507i16), (2, 507), (2, 0x20)] {
        let mut s = PacketStream::new();
        s.write_u32(1).write_i16(error);
        let conn = if who == 1 {
            &mut f.a.conn
        } else {
            &mut f.b.conn
        };
        f.server
            .handle_frame(conn, &encode_raw(1, cs::CSStartDuelPacket, s.as_bytes()))
            .await
            .unwrap();
    }
    assert!(f.a.drain().is_empty() && f.b.drain().is_empty());
    assert!(f.duels().duel_of(1).unwrap().started);
    assert_eq!(f.pa.data().unit.faction_id, FACTION_RED_TEAM);
    assert_eq!(f.pb.data().unit.faction_id, FACTION_BLUE_TEAM);
    assert!(f.pa.data().unit.is_in_duel);
    assert!(f
        .server
        .world
        .world(0)
        .unwrap()
        .doodad(flag.obj_id)
        .is_some());
    // The normal end path still works and cleans everything up.
    f.duels()
        .duel_stop(&f.server, 1, duel_det_type::DRAW, 2)
        .await;
    assert_eq!(f.pa.data().unit.faction_id, 101);
    assert_eq!(f.pb.data().unit.faction_id, 101);
    assert!(f
        .server
        .world
        .world(0)
        .unwrap()
        .doodad(flag.obj_id)
        .is_none());
    assert!(f.duels().is_empty());
}

/// A third party can neither refuse nor accept somebody else's pending challenge.
#[tokio::test]
async fn third_party_cannot_answer_a_pending_challenge() {
    let mut f = Fixture::new(true);
    f.challenge().await;
    f.b.drain();
    // Alice (the challenger) accepting her own challenge is refused with bad_duel_target.
    let mut s = PacketStream::new();
    s.write_u32(1).write_i16(0);
    f.server
        .handle_frame(
            &mut f.a.conn,
            &encode_raw(1, cs::CSStartDuelPacket, s.as_bytes()),
        )
        .await
        .unwrap();
    assert_eq!(err_ids(&f.a.drain()), vec![util::error::BAD_DUEL_TARGET]);
    assert!(!f.duels().duel_of(1).unwrap().started);
    // Accepting an unknown challenge reports bad_duel_target as well.
    let mut s = PacketStream::new();
    s.write_u32(77).write_i16(0);
    f.server
        .handle_frame(
            &mut f.b.conn,
            &encode_raw(1, cs::CSStartDuelPacket, s.as_bytes()),
        )
        .await
        .unwrap();
    assert_eq!(err_ids(&f.b.drain()), vec![util::error::BAD_DUEL_TARGET]);
}

/// Fix 3: challenge validation.
#[tokio::test]
async fn challenge_validation_errors() {
    let mut f = Fixture::new(true);
    let challenge = |id: u32| encode_raw(1, cs::CSChallengeDuelPacket, &id.to_le_bytes());

    // Self.
    f.server
        .handle_frame(&mut f.a.conn, &challenge(1))
        .await
        .unwrap();
    assert_eq!(err_ids(&f.a.drain()), vec![util::error::BAD_DUEL_SELF]);
    assert!(f.duels().is_empty());

    // Not online.
    f.server
        .handle_frame(&mut f.a.conn, &challenge(99))
        .await
        .unwrap();
    assert_eq!(err_ids(&f.a.drain()), vec![util::error::BAD_DUEL_TARGET]);

    // Dead target.
    f.pb.data().unit.hp = 0;
    f.server
        .handle_frame(&mut f.a.conn, &challenge(2))
        .await
        .unwrap();
    assert_eq!(err_ids(&f.a.drain()), vec![util::error::BAD_DUEL_TARGET]);
    assert!(f.b.drain().is_empty() && f.duels().is_empty());
    f.pb.data().unit.hp = 1000;

    // A valid challenge, then a second one: the challenger is "already in a duel" ...
    f.challenge().await;
    assert_eq!(f.b.drain().len(), 1);
    f.server
        .handle_frame(&mut f.a.conn, &challenge(2))
        .await
        .unwrap();
    assert_eq!(err_ids(&f.a.drain()), vec![util::error::ALREADY_IN_DUEL]);
    // ... the target (busy with the pending challenge) is "other already in duel" for a third
    // character.
    let mut c = TestClient::new(&f.server, 3, 3);
    let pc = spawn_player(
        &f.server,
        &c,
        test_character(3, "Carol", 1020.0, 1000.0),
        0x102,
    );
    pc.data().unit.hp = 1000;
    c.conn.active_char = Some(pc.clone());
    c.drain();
    f.server
        .handle_frame(&mut c.conn, &challenge(2))
        .await
        .unwrap();
    assert_eq!(
        err_ids(&c.drain()),
        vec![util::error::OTHER_ALREADY_IN_DUEL]
    );
    // The first challenge is untouched and still answerable.
    assert!(find(&f.b.drain(), sc::SCDuelChallengedPacket).is_empty());
    assert!(f.duels().is_in_duel_with(1, 2) && f.duels().len() == 2);
    f.answer(0).await;
    assert!(f.duels().duel_of(1).unwrap().started);
}

/// An unanswered challenge expires and releases both players (10.x `DuelRequestExpired`).
#[tokio::test]
async fn unanswered_challenge_expires() {
    let mut f = Fixture::new(true);
    f.server.systems.social.duels.set_delays(DuelDelays {
        request: Duration::from_millis(50),
        ..f.duels().delays()
    });
    f.challenge().await;
    f.b.drain();
    let fa = wait_for(&mut f.a, sc::SCErrorMsgPacket).await;
    assert_eq!(err_ids(&fa), vec![util::error::TARGET_REJECTED_DUEL]);
    assert!(f.duels().is_empty());
    // Both can challenge again.
    f.challenge().await;
    assert!(f.duels().is_in_duel_with(1, 2));
}
