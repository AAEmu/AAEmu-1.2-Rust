//! Reproductions of the 2026-11-11 live freeze: the party-accept path held the manager
//! state mutex and then took the inviter's character data lock, while the achievement
//! refresh held a character data lock and then took the manager mutex. Each scenario runs
//! on its own thread so a regression fails the test on a timeout instead of hanging the
//! suite (the stuck threads are leaked, like the `astar_deadlock_tests`).
use std::sync::{mpsc, TryLockError};
use std::time::{Duration, Instant};

use super::*;
use crate::systems::social::achievement_hooks;
use crate::test_support::{
    finishes_in_time, spawn_player, test_character, test_server, TestClient,
};

fn spawn_pair(server: &GameServer) -> (TestClient, TestClient, PlayerRef, PlayerRef) {
    let ca = TestClient::new(server, 1, 1001);
    let cb = TestClient::new(server, 2, 1002);
    let a = spawn_player(server, &ca, test_character(1, "Owner", 100.0, 100.0), 0x100);
    let b = spawn_player(
        server,
        &cb,
        test_character(2, "Target", 101.0, 100.0),
        0x101,
    );
    a.data().is_online = true;
    b.data().is_online = true;
    (ca, cb, a, b)
}

/// Party accept + achievement refresh + AoE damage tagging, all at once: the cycle from
/// the freeze needs a thread that holds the team lock while the owner's character lock is
/// held, and the refresh as the second order.
#[test]
fn party_accept_refresh_and_aggro_tagging_do_not_deadlock() {
    let (tx, rx) = mpsc::channel();
    let scenario = std::thread::spawn(move || {
        // The lazily connected MySQL pool needs a Tokio context even though nothing here
        // reaches the database.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = rt.enter();
        let server = test_server();
        crate::systems::social::install_hooks(&server);
        let (_ca, _cb, a, b) = spawn_pair(&server);
        let teams = &server.systems.social.teams;
        teams.ask_to_join(&server, &a, "Target", 0, true, None);
        // Without the invitation the accept returns before it reaches the owner's lock and
        // the scenario would pass on the old code as well.
        assert_eq!(teams.invitation_count(), 1, "the invitation was issued");
        std::thread::scope(|scope| {
            let owner_lock = a.data();
            let accept = scope.spawn(|| {
                teams.reply_to_join_team(&server, &b, 0, true, 1, false, "", false);
            });
            // Let the accept path reach the manager lock first (the old order took it
            // before the owner's data lock); the fixed order waits for the data lock
            // before it ever takes the team lock.
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && teams.state.try_lock().is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
            // The achievement refresh used to hold the character lock and then take the
            // team lock. With the owner lock still held and the accept holding the team
            // lock, that is the two-thread cycle of the freeze.
            achievement_hooks::refresh_current(&server, &b, chrono::Utc::now());
            // AoE damage tagging on the same character takes the team lock through
            // `team_of`; it must not stay blocked behind the accept.
            let damaged = WorldObject::from(&b);
            for _ in 0..50 {
                crate::systems::skills::combat::add_damage_aggro(
                    &server, &damaged, &damaged, 10, 10,
                );
            }
            drop(owner_lock);
            accept.join().expect("accept thread");
        });
        // The accept went through (the owner lock is released): both are in the party.
        assert!(
            teams.active_team_by_unit(1).is_some_and(|t| t.is_member(2)),
            "the accept created the team"
        );
        assert!(
            a.data().in_party && b.data().in_party,
            "both members are marked InParty"
        );
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(()) => {}
        // The scenario panicked (a postcondition failed): show its own message.
        Err(mpsc::RecvTimeoutError::Disconnected) => match scenario.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => panic!("the scenario ended without reporting"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("party accept / achievement refresh / aggro tagging deadlocked")
        }
    }
}

/// Deterministic order probe for the manager side: while the character lock is held,
/// `CreateSoloTeam` must not take the team lock (the old code did, then waited for the
/// character lock).
#[test]
fn create_solo_team_reads_character_data_before_the_team_lock() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _guard = rt.enter();
    let server = test_server();
    let _c = TestClient::new(&server, 1, 1001);
    let p = spawn_player(&server, &_c, test_character(1, "Solo", 100.0, 100.0), 0x100);
    p.data().is_online = true;
    let teams = &server.systems.social.teams;
    std::thread::scope(|scope| {
        let character_lock = p.data();
        let worker = scope.spawn(|| teams.create_solo_team(&server, &p, true));
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            assert!(
                teams.state.try_lock().is_ok(),
                "CreateSoloTeam took the team lock while the character lock was held"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(character_lock);
        worker.join().expect("solo team thread");
    });
}

/// Deterministic order probe for the achievement side: while the team lock is held, the
/// refresh must not take the character lock (the old code did, then waited for the team
/// lock).
#[test]
fn achievement_refresh_takes_the_team_before_the_character_lock() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _guard = rt.enter();
    let server = test_server();
    let _c = TestClient::new(&server, 1, 1001);
    let p = spawn_player(
        &server,
        &_c,
        test_character(1, "Refresh", 100.0, 100.0),
        0x100,
    );
    p.data().is_online = true;
    let teams = &server.systems.social.teams;
    let team_lock = teams.lock();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            achievement_hooks::refresh_current(&server, &p, chrono::Utc::now());
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            assert!(
                p.data.try_lock().is_ok(),
                "refresh_current took the character lock while waiting for the team lock"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(team_lock);
        worker.join().expect("refresh thread");
    });
}

/// The same probe for the entry snapshot (`AchievementContent::initial_snapshot`, run by
/// `CSNotifyInGame`): the old code took the economy lock and the character lock first and
/// then waited for the team lock, which is the second cycle of the freeze.
#[test]
fn achievement_initial_snapshot_takes_the_team_before_the_character_lock() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _guard = rt.enter();
    let server = test_server();
    let _c = TestClient::new(&server, 1, 1001);
    let p = spawn_player(
        &server,
        &_c,
        test_character(1, "Snapshot", 100.0, 100.0),
        0x100,
    );
    p.data().is_online = true;
    let teams = &server.systems.social.teams;
    let team_lock = teams.lock();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            achievement_hooks::initial_snapshot(&server, &p, chrono::Utc::now());
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            assert!(
                p.data.try_lock().is_ok(),
                "initial_snapshot took the character lock while waiting for the team lock"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(team_lock);
        worker.join().expect("snapshot thread");
    });
}

/// The id of the fixture team (the id manager hands out small ids, so no collision).
const TEAM: u32 = 100;
/// How long a worker is watched, counted from its start.
const PROBE_WINDOW: Duration = Duration::from_millis(150);
/// How long the state mutex may stay unavailable before it counts as held across a wait. A
/// legitimate hold is a few microseconds of state-only work.
const PROBE_HELD: Duration = Duration::from_millis(50);

/// A (id 1, owner of [`TEAM`]), B (2) and C (3) are its members, D (4) is nobody's member.
/// A and D stand together, B and C far away from them (out of render distance).
fn lineup(server: &GameServer) -> (Vec<TestClient>, Vec<PlayerRef>) {
    let spots = [
        (100.0, 100.0),
        (1900.0, 1900.0),
        (1901.0, 1900.0),
        (101.0, 100.0),
    ];
    let mut clients = Vec::new();
    let mut players = Vec::new();
    for (i, (x, y)) in spots.into_iter().enumerate() {
        let id = i as u32 + 1;
        let client = TestClient::new(server, id, 1000 + id);
        let p = spawn_player(
            server,
            &client,
            test_character(id, &format!("P{id}"), x, y),
            0x100 + id,
        );
        p.data().is_online = true;
        clients.push(client);
        players.push(p);
    }
    let mut team = Team::new(TEAM, 1, true);
    for p in &players[..3] {
        team.add_member(p);
    }
    server.systems.social.teams.insert_team_for_test(team);
    (clients, players)
}

fn teams(server: &GameServer) -> &TeamManager {
    &server.systems.social.teams
}

fn nothing(_: &GameServer, _: &[PlayerRef]) {}

fn always(_: &GameServer, _: &[PlayerRef]) -> bool {
    true
}

/// Whether member `id` of [`TEAM`] is cached online / offline.
fn member_online(server: &GameServer, id: u32) -> Option<bool> {
    teams(server)
        .active_team(TEAM)?
        .members
        .iter()
        .flatten()
        .find(|m| m.character.character_id == id)
        .map(|m| m.online)
}

/// One public mutator of the manager under the generic lock-order probe.
struct Case {
    name: &'static str,
    /// The players (indices into [`lineup`]) whose data lock the test thread holds while
    /// the mutator runs. A mutator that reads a character before its first state section
    /// (the invite and reply paths) leaves that character free, or it would block before it
    /// ever reaches the guard: those cases hold the two bystander members (B, C).
    held: &'static [usize],
    /// Runs before any lock is held.
    setup: fn(&GameServer, &[PlayerRef]),
    /// The mutator.
    op: fn(&GameServer, &[PlayerRef]),
    /// Whether the mutator reached its effect (checked once the locks are released), so a
    /// case cannot pass because the mutator returned at its first check.
    done: fn(&GameServer, &[PlayerRef]) -> bool,
}

fn cases() -> Vec<Case> {
    const ALL: &[usize] = &[0, 1, 2, 3];
    const BYSTANDERS: &[usize] = &[1, 2];
    // The invitation of A to D (a pending invitation for the reply / expiry cases).
    fn invite_d(s: &GameServer, p: &[PlayerRef]) {
        teams(s).ask_to_join(s, &p[0], "", 0, true, Some(&p[3]));
    }
    vec![
        Case {
            name: "ask_to_join",
            held: BYSTANDERS,
            setup: nothing,
            op: |s, p| teams(s).ask_to_join(s, &p[0], "", 0, true, Some(&p[3])),
            done: |s, _| teams(s).invitation_count() == 1,
        },
        Case {
            name: "invite_area_to_team",
            held: BYSTANDERS,
            setup: nothing,
            op: |s, p| teams(s).invite_area_to_team(s, &p[0], TEAM, true),
            // The cooldown is claimed in the guarded section after every gate.
            done: |s, _| teams(s).lock().area_invite_until.contains_key(&1),
        },
        Case {
            name: "reply_to_join_team (late join)",
            held: BYSTANDERS,
            setup: invite_d,
            op: |s, p| teams(s).reply_to_join_team(s, &p[3], 0, true, 1, false, "", false),
            done: |s, _| teams(s).is_member_now(TEAM, 4),
        },
        Case {
            name: "create_solo_team",
            held: &[0, 1, 2],
            setup: nothing,
            op: |s, p| teams(s).create_solo_team(s, &p[3], true),
            done: |s, _| teams(s).active_team_by_unit(4).is_some(),
        },
        Case {
            name: "expire_invitations",
            held: ALL,
            setup: invite_d,
            op: |s, _| teams(s).expire_invitations(util::now_ms() + INVITATION_TIMEOUT_MS + 1),
            done: |s, _| teams(s).invitation_count() == 0,
        },
        Case {
            name: "make_team_owner",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).make_team_owner(&p[0], TEAM, 2),
            done: |s, _| teams(s).active_team(TEAM).is_some_and(|t| t.owner_id == 2),
        },
        Case {
            name: "set_team_member_role",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).set_team_member_role(&p[1], TEAM, 2, member_role::TANK),
            done: |s, _| {
                teams(s).active_team(TEAM).is_some_and(|t| {
                    t.members
                        .iter()
                        .flatten()
                        .any(|m| m.role == member_role::TANK)
                })
            },
        },
        Case {
            name: "set_over_head_marker",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).set_over_head_marker(&p[0], TEAM, 3, 2, 0x102),
            done: |s, _| {
                teams(s)
                    .active_team(TEAM)
                    .is_some_and(|t| t.marks[3] == (2, 0x102))
            },
        },
        Case {
            name: "change_looting_rule",
            held: ALL,
            setup: nothing,
            op: |s, p| {
                let rule = LootingRule {
                    loot_method: looting_method::FREE_FOR_ALL,
                    ..LootingRule::default()
                };
                teams(s).change_looting_rule(&p[0], TEAM, 0x01, rule)
            },
            done: |s, _| {
                teams(s)
                    .active_team(TEAM)
                    .is_some_and(|t| t.looting_rule.loot_method == looting_method::FREE_FOR_ALL)
            },
        },
        Case {
            name: "set_ping_pos",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).set_ping_pos(&p[0], TEAM, true, (1.0, 2.0, 3.0), 0),
            done: |s, _| {
                teams(s)
                    .active_team(TEAM)
                    .is_some_and(|t| t.ping_position.0 > 0.5)
            },
        },
        Case {
            name: "convert_to_raid",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).convert_to_raid(s, &p[0], TEAM),
            done: |s, _| teams(s).active_team(TEAM).is_some_and(|t| !t.is_party),
        },
        Case {
            name: "move_team_member",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).move_team_member(s, &p[0], TEAM, 2, 3, 1, 2),
            done: |s, _| {
                teams(s).active_team(TEAM).is_some_and(|t| {
                    t.members[1]
                        .as_ref()
                        .is_some_and(|m| m.character.character_id == 3)
                })
            },
        },
        Case {
            name: "ask_risky_team (leave)",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).ask_risky_team(s, &p[1], TEAM, 2, risky_action::LEAVE),
            done: |s, _| !teams(s).is_member_now(TEAM, 2),
        },
        Case {
            name: "ask_risky_team (kick)",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).ask_risky_team(s, &p[0], TEAM, 3, risky_action::KICK),
            done: |s, _| !teams(s).is_member_now(TEAM, 3),
        },
        Case {
            name: "ask_risky_team (dismiss)",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).ask_risky_team(s, &p[0], TEAM, 0, risky_action::DISMISS),
            done: |s, _| teams(s).active_team(TEAM).is_none(),
        },
        Case {
            name: "set_offline",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).set_offline(s, &p[1]),
            done: |s, _| member_online(s, 2) == Some(false),
        },
        Case {
            name: "member_remove_for_faction_change",
            held: ALL,
            setup: nothing,
            op: |s, p| teams(s).member_remove_for_faction_change(s, &p[1]),
            done: |s, _| !teams(s).is_member_now(TEAM, 2),
        },
        Case {
            name: "update_position",
            held: ALL,
            setup: nothing,
            op: |s, _| teams(s).update_position(s, 2),
            done: always,
        },
        Case {
            name: "update_at_login",
            held: ALL,
            // B logged out before (no data lock is held yet).
            setup: |s, p| teams(s).set_offline(s, &p[1]),
            op: |s, p| teams(s).update_at_login(s, &p[1]),
            done: |s, _| member_online(s, 2) == Some(true),
        },
        Case {
            name: "update_all_teams",
            held: ALL,
            setup: nothing,
            op: |s, _| teams(s).update_all_teams(s, util::now_ms()),
            // The delta claims run in the second state section, after the rows were built.
            done: |s, _| !teams(s).lock().last_sync.is_empty(),
        },
        Case {
            name: "next_loot_winner",
            held: ALL,
            setup: nothing,
            op: |s, _| {
                assert!(teams(s).next_loot_winner(TEAM, &[1, 2, 3]).is_some());
            },
            done: always,
        },
    ]
}

/// Runs one case: the test thread holds the data lock of every player in `case.held`, the
/// mutator runs on a worker, and the state mutex must not stay held while the worker waits
/// for one of those locks. The state is polled with `try_lock`; a few microseconds of
/// legitimate state-only work show up as a failed poll now and then, a lock held across a
/// wait for a character stays unavailable for good.
fn run_case(case: Case) -> Result<(), String> {
    let server = test_server();
    let (_clients, players) = lineup(&server);
    let state = &teams(&server).state;
    (case.setup)(&server, &players);
    let verdict = std::thread::scope(|scope| {
        let locks: Vec<_> = case.held.iter().map(|&i| players[i].data()).collect();
        let (started_tx, started_rx) = mpsc::channel();
        let (server_ref, players_ref, op) = (&server, &players, case.op);
        let worker = scope.spawn(move || {
            let _ = started_tx.send(());
            op(server_ref, players_ref);
        });
        started_rx.recv().expect("the worker started");
        let end = Instant::now() + PROBE_WINDOW;
        let mut held_since: Option<Instant> = None;
        let mut verdict = Ok(());
        while Instant::now() < end && !worker.is_finished() {
            match state.try_lock() {
                Ok(guard) => {
                    drop(guard);
                    held_since = None;
                }
                Err(TryLockError::WouldBlock) => {
                    let since = *held_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= PROBE_HELD {
                        verdict = Err(format!(
                            "{}: the team state mutex stayed held while the mutator waited \
                             for a character data lock (a character lock or another manager \
                             is taken under the guard)",
                            case.name
                        ));
                        break;
                    }
                }
                Err(TryLockError::Poisoned(_)) => {
                    verdict = Err(format!("{}: the team state is poisoned", case.name));
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // Let the mutator finish: it may wait for the locks, but never forever.
        drop(locks);
        if worker.join().is_err() && verdict.is_ok() {
            verdict = Err(format!("{}: the mutator panicked", case.name));
        }
        verdict
    });
    verdict?;
    if !(case.done)(&server, &players) {
        return Err(format!(
            "{}: the mutator never reached its effect",
            case.name
        ));
    }
    Ok(())
}

/// Generic lock-order probe: no public mutator of the manager holds its state mutex while
/// it waits for a character data lock. `member_info`, `set_in_party`, a chat channel or an
/// instance event back under a guard would block on the locks the test thread holds and
/// keep the mutex held, which is the cycle of the freeze.
#[tokio::test]
async fn no_public_mutator_holds_the_team_state_while_it_waits_for_a_character() {
    for case in cases() {
        let name = case.name;
        if let Err(why) = finishes_in_time(name, move || run_case(case)) {
            panic!("{why}");
        }
    }
}
