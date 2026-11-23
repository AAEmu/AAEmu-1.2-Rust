use super::tick::{
    batch_worker_count, maintenance_sweep_with, plan_chunks, tick_batch_with_workers,
    tick_candidates,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::data::GameData;
use crate::models::npc::Npc;
use crate::systems::ai::framework::BehaviorKind;
use crate::systems::ai::params::{AiCommandCategory, AiCommands};
use crate::systems::ai::path_handler::{AiPathPoint, AiPathPointAction};
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::skills::controllers::Vec3;
use crate::test_support::*;
use crate::world::{Entity, NpcRef, PlayerRef};

use super::*;

fn server() -> Arc<GameServer> {
    let server = Arc::new(test_server_with(GameData::empty()));
    server.set_self();
    server
}

/// Creates an AI-backed NPC and spawns it (without the spawner hooks: `GoToSpawn` stays
/// unqueued, so the behavior state is `None` and ticking only runs the aggro check).
fn spawn_npc(
    server: &GameServer,
    obj_id: u32,
    template: u32,
    x: f32,
    y: f32,
    kind: AiKind,
) -> NpcRef {
    let mut n = Npc::new(template);
    n.unit.hp = 100;
    n.unit.max_hp = 100;
    n.unit.transform.set_xyz(x, y, 100.0);
    n.unit.transform.zone_id = TEST_ZONE;
    let npc = Entity::new(obj_id, n);
    server.systems.ai.ais.add_ai(&npc, kind);
    let world = server.world.world(0).expect("world");
    world.spawn(&npc, &server.view_ctx());
    npc
}

fn spawn_player_at(server: &GameServer, obj_id: u32, x: f32, y: f32) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, obj_id, 1);
    let c = test_character(obj_id, "Tester", x, y);
    let p = spawn_player(server, &client, c, obj_id);
    (client, p)
}

fn idle(server: &GameServer, ticks: usize) {
    for _ in 0..ticks {
        tick_all(server, 100);
    }
}

/// Production-equivalent spawn (`NpcSpawnerNpc.Spawn`): the home / idle position and
/// `GoToSpawn()` are posted, so the behavior machine starts in `Spawning`.
fn spawn_initialized_npc(
    server: &GameServer,
    obj_id: u32,
    template: u32,
    x: f32,
    y: f32,
    kind: AiKind,
) -> NpcRef {
    let npc = spawn_npc(server, obj_id, template, x, y, kind);
    crate::systems::ai::hooks::spawn_positioned(server, &npc);
    npc
}

fn behavior(server: &GameServer, obj_id: u32) -> Option<BehaviorKind> {
    server
        .systems
        .ai
        .ais
        .inspect(obj_id, |ai| ai.current_behavior())
        .flatten()
}

/// Without players and without persistent state an AI leaves the candidate set once its
/// initialization grace is over: no lock, no visit.
#[tokio::test]
async fn dormant_ai_is_not_visited_without_player_activity() {
    let server = server();
    let local = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    let far = spawn_npc(&server, 0x220, 1, 1500.0, 1500.0, AiKind::Default);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 0, "nothing to tick: {stats:?}");
    assert_eq!(stats.ticked, 0);
    let local_visits = server.systems.ai.ais.visits(0x210).unwrap();
    let far_visits = server.systems.ai.ais.visits(0x220).unwrap();
    idle(&server, 5);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 0, "{stats:?}");
    assert_eq!(stats.ticked, 0);
    assert_eq!(server.systems.ai.ais.visits(0x210).unwrap(), local_visits);
    assert_eq!(server.systems.ai.ais.visits(0x220).unwrap(), far_visits);
    drop((local, far));
}

/// Production-equivalent spawn without players: the initialization grace actually runs the
/// behavior machine (`Spawning` -> `RunCommandSet` -> `Idle`) and the NPC then leaves the
/// candidate set. A distant unrelated idle NPC stays unvisited during and after the settle.
#[tokio::test]
async fn prewarmed_spawn_initialization_finishes_and_then_sleeps() {
    let server = server();
    let unrelated = spawn_npc(&server, 0x220, 1, 1500.0, 1500.0, AiKind::Default);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    assert_eq!(server.systems.ai.ais.tick_stats().candidates, 0);
    let unrelated_visits = server.systems.ai.ais.visits(0x220).unwrap();

    let npc = spawn_initialized_npc(&server, 0x210, 1, 100.0, 100.0, AiKind::Default);
    assert_eq!(behavior(&server, 0x210), Some(BehaviorKind::Spawning));

    // No player anywhere: the grace still advances the spawning steps to the settle state.
    let mut settled = false;
    for _ in 0..INITIALIZING_TICKS as usize {
        let stats = tick_all(&server, 100);
        assert_eq!(
            (stats.candidates, stats.ticked),
            (1, 1),
            "only the spawning NPC is a candidate: {stats:?}"
        );
        if behavior(&server, 0x210) == Some(BehaviorKind::Idle) {
            settled = true;
            break;
        }
    }
    assert!(
        settled,
        "the prewarmed NPC did not finish spawning: {:?}",
        behavior(&server, 0x210)
    );
    assert_eq!(
        server.systems.ai.ais.visits(0x220).unwrap(),
        unrelated_visits,
        "the unrelated distant idle NPC must not be visited"
    );

    // Past the grace the settled NPC is dormant again: no selection, no visits.
    idle(&server, INITIALIZING_TICKS as usize + 2);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 0, "{stats:?}");
    assert_eq!(stats.ticked, 0, "{stats:?}");
    let visits = server.systems.ai.ais.visits(0x210).unwrap();
    idle(&server, 5);
    assert_eq!(server.systems.ai.ais.visits(0x210).unwrap(), visits);
    assert_eq!(
        server.systems.ai.ais.visits(0x220).unwrap(),
        unrelated_visits
    );
    drop((npc, unrelated));
}

/// A displaced production-spawned NPC walks back to its home even after the player that kept
/// its region active leaves before the return completes.
#[tokio::test]
async fn displaced_return_finishes_after_region_activity_disappears() {
    let server = server();
    let npc = spawn_initialized_npc(&server, 0x210, 1, 1500.0, 1500.0, AiKind::HoldPosition);
    let (_client, player) = spawn_player_at(&server, 0x100, 1510.0, 1500.0);
    idle(&server, 3);

    // Displace the NPC 6 m from home and start the return.
    npc.data().unit.transform.set_xyz(1506.0, 1500.0, 100.0);
    post(&server, 0x210, |c| c.go_to_return());
    assert_eq!(behavior(&server, 0x210), Some(BehaviorKind::ReturnState));

    // The player leaves: no region activity marks the NPC local anymore.
    player.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    let world = server.world.world(0).expect("world");
    world.add_visible_object(&player, &server.view_ctx());
    let (candidates, local) = tick_candidates(&server, 0);
    assert_eq!(candidates.len(), 1, "the returning NPC stays selected");
    assert!(local.is_empty(), "no region activity marks it local");

    // It still walks home and settles, with no player anywhere near it.
    let mut home = false;
    for _ in 0..30 {
        tick_all(&server, 500);
        let (x, y) = {
            let t = npc.data().unit.transform;
            (t.x, t.y)
        };
        if behavior(&server, 0x210) == Some(BehaviorKind::Idle)
            && (x - 1500.0).abs() < 1.5
            && (y - 1500.0).abs() < 1.5
        {
            home = true;
            break;
        }
    }
    assert!(
        home,
        "the return did not finish without region activity: {:?}",
        behavior(&server, 0x210)
    );
    drop(npc);
}

/// A production-spawned NPC in combat keeps ticking after the player leaves its region: the
/// stale aggro is cleared and the NPC returns home even though no region activity selects it.
#[tokio::test]
async fn combat_cleanup_finishes_after_the_player_leaves_the_region() {
    let server = server();
    let npc = spawn_initialized_npc(&server, 0x210, 1, 1500.0, 1500.0, AiKind::HoldPosition);
    let (_client, player) = spawn_player_at(&server, 0x100, 1510.0, 1500.0);
    idle(&server, 3);

    // Enter combat with an aggro entry, then the player leaves the whole neighbourhood.
    let player_obj = WorldObject::from(&player);
    let npc_obj = WorldObject::from(&npc);
    super::super::combat::add_unit_aggro(
        &server,
        &npc_obj,
        crate::systems::skills::state::AggroKind::Damage,
        &player_obj,
        10,
    );
    post(&server, 0x210, |c| c.go_to_combat());
    assert_eq!(behavior(&server, 0x210), Some(BehaviorKind::Attack));
    player.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    let world = server.world.world(0).expect("world");
    world.add_visible_object(&player, &server.view_ctx());

    // No region activity selects it, but the combat cleanup must still run.
    let (candidates, local) = tick_candidates(&server, 0);
    assert_eq!(candidates.len(), 1, "the combat NPC stays selected");
    assert!(local.is_empty(), "no region activity marks it local");
    let mut cleared = false;
    for _ in 0..5 {
        tick_all(&server, 100);
        if behavior(&server, 0x210) == Some(BehaviorKind::Idle)
            && npc.data().unit.skill_state.aggro_table.is_empty()
        {
            cleared = true;
            break;
        }
    }
    assert!(
        cleared,
        "the stale combat state was not cleared: {:?}",
        behavior(&server, 0x210)
    );
    assert!(!npc.data().unit.is_in_battle);
    drop(npc);
}

/// An in-progress AI command keeps the NPC ticking after its queue entry was popped (the
/// queue is empty, only `CurrentCommand` remains): the wait still finishes and the NPC
/// settles to idle, with no player anywhere.
#[tokio::test]
async fn in_progress_command_with_empty_queue_finishes_without_players() {
    let server = server();
    let npc = spawn_initialized_npc(&server, 0x220, 1, 1500.0, 1500.0, AiKind::Default);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    assert_eq!(server.systems.ai.ais.tick_stats().candidates, 0);

    post(&server, 0x220, |c| {
        c.enqueue_ai_commands(
            &[AiCommands::new(0, 0, AiCommandCategory::Timeout, 250, "")],
            false,
        );
    });
    assert_eq!(behavior(&server, 0x220), Some(BehaviorKind::RunCommandSet));

    // One tick pops the queued command: only the in-progress `CurrentCommand` remains.
    tick_all(&server, 100);
    assert_eq!(
        server.systems.ai.ais.inspect(0x220, |ai| (
            ai.commands_queue.is_empty(),
            ai.current_command.is_some()
        )),
        Some((true, true)),
        "the queued command must be popped and in progress"
    );
    assert_eq!(
        server
            .systems
            .ai
            .ais
            .inspect(0x220, |ai| ai.has_persistent_ai()),
        Some(true),
        "the in-progress command must keep the NPC awake"
    );

    // The wait (250 ms, overshooting the 100 ms tick) still finishes: the command is
    // cleared and the behavior settles to Idle without any region activity.
    let mut finished = false;
    for _ in 0..10 {
        tick_all(&server, 100);
        if behavior(&server, 0x220) == Some(BehaviorKind::Idle) {
            finished = true;
            break;
        }
    }
    assert!(
        finished,
        "the in-progress command froze: {:?}",
        behavior(&server, 0x220)
    );
    assert_eq!(
        server
            .systems
            .ai
            .ais
            .inspect(0x220, |ai| ai.current_command.is_none()),
        Some(true)
    );
    drop(npc);
}

/// A player arriving wakes the dormant AI on the very next tick; leaving the whole
/// neighbourhood lets it rest again.
#[tokio::test]
async fn player_proximity_wakes_and_departure_sleeps_dormant_ai() {
    let server = server();
    let npc = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    assert_eq!(server.systems.ai.ais.tick_stats().candidates, 0);
    let before = server.systems.ai.ais.visits(0x210).unwrap();

    let (_client, player) = spawn_player_at(&server, 0x100, 115.0, 100.0);
    tick_all(&server, 100);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 1, "{stats:?}");
    assert_eq!(stats.ticked, 1);
    assert_eq!(server.systems.ai.ais.visits(0x210).unwrap(), before + 1);

    player.data().unit.transform.set_xyz(1500.0, 1500.0, 100.0);
    let world = server.world.world(0).expect("world");
    world.add_visible_object(&player, &server.view_ctx());
    tick_all(&server, 100);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 0, "{stats:?}");
    let visits = server.systems.ai.ais.visits(0x210).unwrap();
    idle(&server, 3);
    assert_eq!(server.systems.ai.ais.visits(0x210).unwrap(), visits);
    drop(npc);
}

/// Two players watching the same regions still yield one candidate and one tick.
#[tokio::test]
async fn overlapping_player_regions_tick_each_ai_once() {
    let server = server();
    let npc = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    let (_c1, _p1) = spawn_player_at(&server, 0x100, 115.0, 100.0);
    let (_c2, _p2) = spawn_player_at(&server, 0x101, 105.0, 100.0);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    let before = server.systems.ai.ais.visits(0x210).unwrap();
    tick_all(&server, 100);
    tick_all(&server, 100);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 1, "one AI, two players: {stats:?}");
    assert_eq!(stats.ticked, 1);
    assert_eq!(server.systems.ai.ais.visits(0x210).unwrap(), before + 2);
    drop(npc);
}

/// A queued path point (persistent AI) keeps a distant NPC ticking without players; the
/// same AI seen both locally and as woken is still ticked exactly once.
#[tokio::test]
async fn persistent_paths_keep_ticking_without_players() {
    let server = server();
    let npc = spawn_npc(&server, 0x220, 1, 1500.0, 1500.0, AiKind::Default);
    post(&server, 0x220, |c| {
        c.enqueue_path_point(AiPathPoint {
            position: Vec3::new(1500.0, 1500.0, 100.0),
            action: AiPathPointAction::None,
            param: String::new(),
        });
    });
    assert!(server
        .systems
        .ai
        .ais
        .inspect(0x220, |ai| ai.has_persistent_ai())
        .unwrap());
    idle(&server, INITIALIZING_TICKS as usize + 2);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 1, "persistent AI must stay: {stats:?}");
    let before = server.systems.ai.ais.visits(0x220).unwrap();
    idle(&server, 5);
    assert_eq!(server.systems.ai.ais.visits(0x220).unwrap(), before + 5);

    let (_client, _player) = spawn_player_at(&server, 0x100, 1510.0, 1500.0);
    let before = server.systems.ai.ais.visits(0x220).unwrap();
    tick_all(&server, 100);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 1, "{stats:?}");
    assert_eq!(server.systems.ai.ais.visits(0x220).unwrap(), before + 1);
    drop(npc);
}

/// `follow_unit` escorts and queued AI commands are persistent too: they keep ticking
/// without players, like the path-driven AIs.
#[tokio::test]
async fn escorts_and_queued_commands_keep_ticking_without_players() {
    let server = server();
    let escort = spawn_npc(&server, 0x220, 1, 1500.0, 1500.0, AiKind::Default);
    post(&server, 0x220, |c| {
        let target = WorldObject::from(&c.npc);
        c.ai.follow_unit = Some(target);
    });
    let queued = spawn_npc(&server, 0x221, 1, 1600.0, 1500.0, AiKind::Default);
    post(&server, 0x221, |c| {
        c.enqueue_ai_commands(
            &[AiCommands::new(0, 0, AiCommandCategory::UseSkill, 0, "")],
            true,
        );
    });
    idle(&server, INITIALIZING_TICKS as usize + 2);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 2, "{stats:?}");
    let escort_before = server.systems.ai.ais.visits(0x220).unwrap();
    let queued_before = server.systems.ai.ais.visits(0x221).unwrap();
    idle(&server, 4);
    assert_eq!(
        server.systems.ai.ais.visits(0x220).unwrap(),
        escort_before + 4
    );
    assert_eq!(
        server.systems.ai.ais.visits(0x221).unwrap(),
        queued_before + 4
    );
    drop((escort, queued));
}

/// A call queued while the AI is busy stays pending (and keeps the AI woken) and is run by
/// a later tick, even with no player anywhere near the NPC.
#[tokio::test]
async fn pending_calls_of_a_busy_ai_run_on_a_later_tick_without_players() {
    let server = server();
    let npc = spawn_npc(&server, 0x220, 1, 1500.0, 1500.0, AiKind::Default);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    assert_eq!(server.systems.ai.ais.tick_stats().candidates, 0);

    // Hold the AI's lock from a worker thread (a long external call; `inspect` never
    // drains the queue, so only a tick can run the call once the lock is released).
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let srv = server.clone();
    let holder = std::thread::spawn(move || {
        let _ = srv.systems.ai.ais.inspect(0x220, |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("AI lock not held");

    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    post(&server, 0x220, move |_| flag.store(true, Ordering::SeqCst));
    assert!(!ran.load(Ordering::SeqCst), "the busy AI ran the call");

    tick_all(&server, 100);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(
        stats.candidates, 1,
        "a pending call must wake the AI: {stats:?}"
    );
    assert_eq!(stats.busy, 1, "a busy AI is skipped, never waited for");
    assert!(!ran.load(Ordering::SeqCst));

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let mut fired = false;
    for _ in 0..5 {
        tick_all(&server, 100);
        if ran.load(Ordering::SeqCst) {
            fired = true;
            break;
        }
    }
    assert!(fired, "the queued call never ran through the tick");
    drop(npc);
}

/// Dropped owners are cleaned up promptly; a dormant AI of a directly deleted NPC is
/// caught by the maintenance sweep; reused ids stay a single candidate per tick.
#[tokio::test]
async fn stale_handles_are_swept_and_reused_ids_stay_single_tick() {
    let server = server();
    // Owner dropped before the first tick: the initialization grace still visits it.
    let orphan = Entity::new(0x230, Npc::new(1));
    server.systems.ai.ais.add_ai(&orphan, AiKind::Default);
    drop(orphan);
    tick_all(&server, 100);
    assert!(!has_ai(&server, 0x230));

    // `world.delete` without the npc_deleted hook: only the low-frequency sweep sees it.
    let npc = spawn_npc(&server, 0x220, 1, 1500.0, 1500.0, AiKind::Default);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    server.world.world(0).expect("world").delete(&npc);
    for _ in 0..(MAINTENANCE_SWEEP_TICKS as usize + 2) {
        if !has_ai(&server, 0x220) {
            break;
        }
        tick_all(&server, 100);
    }
    assert!(
        !has_ai(&server, 0x220),
        "the sweep did not drop the deleted AI"
    );
    drop(npc);

    // ID reuse: `add_ai` for an id that is already registered retires the replaced handle
    // and registers the new one - the candidate must still tick exactly once.
    let old = Entity::new(0x250, Npc::new(1));
    server.systems.ai.ais.add_ai(&old, AiKind::Default);
    let new = spawn_npc(&server, 0x250, 1, 1500.0, 1500.0, AiKind::Default);
    let (_client, _player) = spawn_player_at(&server, 0x100, 1510.0, 1500.0);
    let before = server.systems.ai.ais.visits(0x250).unwrap();
    tick_all(&server, 100);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 1, "{stats:?}");
    assert_eq!(server.systems.ai.ais.visits(0x250).unwrap(), before + 1);
    assert_eq!(
        server.systems.ai.ais.inspect(0x250, |ai| ai.kind),
        Some(AiKind::Default)
    );
    drop((old, new));
}

/// Only the wanted handles are collected, sorted by their monotonic `AddAi` ordinal: a
/// dormant distant AI is skipped and a woken one keeps its `AddAi` position between the
/// region-active NPCs (no registry scan).
#[tokio::test]
async fn selected_candidates_follow_addai_ordinals() {
    let server = server();
    let first = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    let far = spawn_npc(&server, 0x212, 1, 1500.0, 1500.0, AiKind::Default);
    post(&server, 0x212, |c| {
        c.enqueue_path_point(AiPathPoint {
            position: Vec3::new(1500.0, 1500.0, 100.0),
            action: AiPathPointAction::None,
            param: String::new(),
        });
    });
    let second = spawn_npc(&server, 0x211, 1, 112.0, 100.0, AiKind::Default);
    let dormant = spawn_npc(&server, 0x213, 1, 1600.0, 1500.0, AiKind::Default);
    let (_client, _player) = spawn_player_at(&server, 0x100, 115.0, 100.0);

    let ordinal = |id: u32| server.systems.ai.ais.get(id).unwrap().ordinal;
    assert!(ordinal(0x210) < ordinal(0x212));
    assert!(ordinal(0x212) < ordinal(0x211));
    assert!(ordinal(0x211) < ordinal(0x213));

    let (candidates, local) = tick_candidates(&server, INITIALIZING_TICKS + 10);
    let ids: Vec<u32> = candidates.iter().map(|h| h.obj_id).collect();
    assert_eq!(
        ids,
        vec![0x210, 0x212, 0x211],
        "AddAi order of the wanted handles only"
    );
    assert_eq!(local.len(), 2, "only the region-active NPCs are local");
    drop((first, far, second, dormant));
}

/// A recycled object id must never let the replaced (stale) handle touch the replacement:
/// no wake-state clearing, no registry removal; the replacement keeps its claims.
#[tokio::test]
async fn stale_handle_cannot_touch_the_replacement() {
    let server = server();
    let old_npc = Entity::new(0x260, Npc::new(1));
    let old = server.systems.ai.ais.add_ai(&old_npc, AiKind::Default);
    let new_npc = spawn_npc(&server, 0x260, 1, 1500.0, 1500.0, AiKind::Default);
    let new = server.systems.ai.ais.get(0x260).unwrap();
    assert!(!Arc::ptr_eq(&old, &new));
    assert!(
        old.ordinal < new.ordinal,
        "replacement gets a later ordinal"
    );
    assert!(
        old.removed.load(Ordering::Acquire),
        "replaced handle is retired"
    );

    // Wake the replacement (persistent AI) and refresh with the stale snapshot: the
    // replacement's wake state must survive.
    post(&server, 0x260, |c| {
        c.enqueue_path_point(AiPathPoint {
            position: Vec3::new(1500.0, 1500.0, 100.0),
            action: AiPathPointAction::None,
            param: String::new(),
        });
    });
    server.systems.ai.ais.refresh_woken(&old);
    let (candidates, _) = tick_candidates(&server, INITIALIZING_TICKS + 10);
    assert_eq!(candidates.len(), 1, "only the replacement is a candidate");
    assert!(Arc::ptr_eq(&candidates[0], &new), "replacement stays woken");

    // A stale removal must not drop the replacement's registry entry.
    assert!(!server.systems.ai.ais.remove_if_current(&old));
    assert!(server
        .systems
        .ai
        .ais
        .get(0x260)
        .is_some_and(|h| Arc::ptr_eq(&h, &new)));

    // The public removal drops the current handle and every claim it holds.
    server.systems.ai.ais.remove(0x260);
    assert!(server.systems.ai.ais.get(0x260).is_none());
    assert!(tick_candidates(&server, INITIALIZING_TICKS + 11)
        .0
        .is_empty());
    drop((old_npc, new_npc));
}

/// `NpcTemplate.ActivateAiAlways` keeps the AI ticking with no player in its region.
#[tokio::test]
async fn activate_ai_always_ticks_without_players() {
    let mut data = GameData::empty();
    data.systems.npcs.npc.templates.insert(
        5000,
        NpcTemplate {
            id: 5000,
            ai_file_id: 15,
            activate_ai_always: true,
            ..NpcTemplate::default()
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    let mut n = Npc::new(5000);
    n.unit.hp = 100;
    n.unit.max_hp = 100;
    n.unit.transform.set_xyz(1500.0, 1500.0, 100.0);
    n.unit.transform.zone_id = TEST_ZONE;
    let npc = Entity::new(0x240, n);
    let world = server.world.world(0).expect("world");
    world.spawn(&npc, &server.view_ctx());
    assert!(crate::systems::ai::hooks::create_ai(&server, &npc, 15));
    assert_eq!(
        server.systems.ai.ais.inspect(0x240, |ai| ai.kind),
        Some(AiKind::HoldPosition)
    );

    idle(&server, INITIALIZING_TICKS as usize + 2);
    let stats = server.systems.ai.ais.tick_stats();
    assert_eq!(stats.candidates, 1, "{stats:?}");
    let before = server.systems.ai.ais.visits(0x240).unwrap();
    idle(&server, 3);
    assert_eq!(server.systems.ai.ais.visits(0x240).unwrap(), before + 3);
    drop(npc);
}

/// Default configuration (`World.AiWorkerThreads = 1`): the asynchronous batch and the
/// synchronous `tick_all` both run the plan as one sequential pass in `AddAi` order.
#[tokio::test]
async fn default_worker_batch_ticks_candidates_in_addai_order() {
    let server = server();
    let first = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    let far = spawn_npc(&server, 0x212, 1, 1500.0, 1500.0, AiKind::Default);
    post(&server, 0x212, |c| {
        c.enqueue_path_point(AiPathPoint {
            position: Vec3::new(1500.0, 1500.0, 100.0),
            action: AiPathPointAction::None,
            param: String::new(),
        });
    });
    let second = spawn_npc(&server, 0x211, 1, 112.0, 100.0, AiKind::Default);
    let (_client, _player) = spawn_player_at(&server, 0x100, 115.0, 100.0);
    let ordinals: Vec<u64> = [0x210u32, 0x212, 0x211]
        .iter()
        .map(|id| server.systems.ai.ais.get(*id).unwrap().ordinal)
        .collect();

    assert_eq!(
        server.config.world.ai_worker_threads, 1,
        "default is one worker"
    );
    let diag = tick_batch(&server, 100).await;
    assert_eq!(diag.candidates, 3, "{diag:?}");
    assert_eq!(diag.ticked, 3, "{diag:?}");
    assert_eq!(
        *lock(&server.systems.ai.ais.tick_order),
        ordinals,
        "the default worker must keep the AddAi order"
    );

    lock(&server.systems.ai.ais.tick_order).clear();
    let diag = tick_all(&server, 100);
    assert_eq!(diag.candidates, 3, "{diag:?}");
    assert_eq!(
        *lock(&server.systems.ai.ais.tick_order),
        ordinals,
        "tick_all stays sequential"
    );
    drop((first, far, second));
}

/// A configured multi-worker batch visits every selected NPC exactly once, leaves the
/// dormant ones alone and aggregates its chunk diagnostics once.
#[tokio::test]
async fn parallel_batch_ticks_each_selected_npc_exactly_once() {
    let server = server();
    let npcs: Vec<NpcRef> = (0..6)
        .map(|i| {
            spawn_npc(
                &server,
                0x210 + i,
                1,
                104.0 + i as f32,
                100.0,
                AiKind::Default,
            )
        })
        .collect();
    let dormant = spawn_npc(&server, 0x230, 1, 1500.0, 1500.0, AiKind::Default);
    idle(&server, INITIALIZING_TICKS as usize + 2);
    let (_client, _player) = spawn_player_at(&server, 0x100, 110.0, 100.0);
    let before: Vec<u64> = npcs
        .iter()
        .map(|n| server.systems.ai.ais.visits(n.obj_id).unwrap())
        .collect();
    let dormant_before = server.systems.ai.ais.visits(0x230).unwrap();

    let diag = tick_batch_with_workers(&server, 100, 4).await;
    assert_eq!(diag.candidates, npcs.len(), "{diag:?}");
    assert_eq!(diag.ticked, npcs.len(), "{diag:?}");
    assert_eq!(diag.busy, 0, "{diag:?}");
    assert_eq!(diag.removed, 0, "{diag:?}");
    assert_eq!(
        server.systems.ai.ais.tick_stats(),
        diag,
        "the chunk diagnostics are aggregated once"
    );
    for (npc, before) in npcs.iter().zip(before) {
        assert_eq!(
            server.systems.ai.ais.visits(npc.obj_id).unwrap(),
            before + 1,
            "npc {:#x} must be visited exactly once",
            npc.obj_id
        );
    }
    assert_eq!(server.systems.ai.ais.visits(0x230).unwrap(), dormant_before);
    drop(dormant);
}

/// A multi-worker batch never waits for a busy AI: it counts it (try-lock), ticks the
/// others, and a later batch runs the queued call once the holder is gone.
#[tokio::test]
async fn parallel_batch_skips_a_busy_ai_and_runs_its_queued_call_later() {
    let server = server();
    let held = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    let free = spawn_npc(&server, 0x211, 1, 112.0, 100.0, AiKind::Default);
    let (_client, _player) = spawn_player_at(&server, 0x100, 115.0, 100.0);

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let srv = server.clone();
    let holder = std::thread::spawn(move || {
        let _ = srv.systems.ai.ais.inspect(0x210, |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("AI lock not held");

    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    post(&server, 0x210, move |_| flag.store(true, Ordering::SeqCst));
    assert!(!ran.load(Ordering::SeqCst), "the busy AI ran the call");

    let diag = tick_batch_with_workers(&server, 100, 2).await;
    assert_eq!(diag.candidates, 2, "{diag:?}");
    assert_eq!(diag.busy, 1, "{diag:?}");
    assert_eq!(diag.ticked, 1, "{diag:?}");
    assert!(!ran.load(Ordering::SeqCst));

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let mut fired = false;
    for _ in 0..5 {
        tick_batch_with_workers(&server, 100, 2).await;
        if ran.load(Ordering::SeqCst) {
            fired = true;
            break;
        }
    }
    assert!(fired, "the queued call never ran through a batch");
    drop((held, free));
}

/// `try_inspect` reads a free AI and returns `None` for an unknown, replaced or busy one
/// without ever waiting for the state lock.
#[tokio::test]
async fn try_inspect_reads_free_ai_and_skips_missing_replaced_and_busy_ones() {
    let server = server();
    let npc = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    assert_eq!(
        server.systems.ai.ais.try_inspect(0x210, |ai| ai.kind),
        Some(AiKind::Default)
    );
    assert_eq!(
        server.systems.ai.ais.try_inspect(0x2ff, |ai| ai.kind),
        None,
        "an unknown object id is not readable"
    );

    let replaced_npc = Entity::new(0x211, Npc::new(1));
    let replaced = server.systems.ai.ais.add_ai(&replaced_npc, AiKind::Default);
    let replacement = spawn_npc(&server, 0x211, 1, 112.0, 100.0, AiKind::HoldPosition);
    assert!(
        replaced.removed.load(Ordering::Acquire),
        "the replaced handle is retired"
    );
    assert_eq!(
        server.systems.ai.ais.try_inspect(0x211, |ai| ai.kind),
        Some(AiKind::HoldPosition),
        "the current handle is readable"
    );

    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let srv = server.clone();
    let holder = std::thread::spawn(move || {
        let _ = srv.systems.ai.ais.inspect(0x210, |_| {
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("AI lock not held");
    assert_eq!(
        server.systems.ai.ais.try_inspect(0x210, |ai| ai.kind),
        None,
        "a busy AI is skipped instead of waited for"
    );
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(
        server.systems.ai.ais.try_inspect(0x210, |ai| ai.kind),
        Some(AiKind::Default),
        "readable again after the holder released the lock"
    );
    drop((npc, replacement, replaced, replaced_npc));
}

/// A recycled object id is processed once through the current handle in a multi-worker
/// batch; the replaced snapshot never becomes a candidate.
#[tokio::test]
async fn parallel_batch_ticks_a_reused_id_once_and_keeps_the_replacement() {
    let server = server();
    let old_npc = Entity::new(0x250, Npc::new(1));
    let old = server.systems.ai.ais.add_ai(&old_npc, AiKind::Default);
    let new = spawn_npc(&server, 0x250, 1, 1500.0, 1500.0, AiKind::Default);
    let (_client, _player) = spawn_player_at(&server, 0x100, 1510.0, 1500.0);
    let before = server.systems.ai.ais.visits(0x250).unwrap();

    let diag = tick_batch_with_workers(&server, 100, 3).await;
    assert_eq!(diag.candidates, 1, "the stale snapshot is not a candidate");
    assert_eq!(diag.ticked, 1, "{diag:?}");
    assert_eq!(server.systems.ai.ais.visits(0x250).unwrap(), before + 1);
    assert_eq!(
        server.systems.ai.ais.inspect(0x250, |ai| ai.kind),
        Some(AiKind::Default)
    );
    drop((old, old_npc, new));
}

/// `World.AiWorkerThreads` is clamped to `1..=MAX_AI_WORKERS`; the default stays `1`.
#[tokio::test]
async fn worker_count_is_clamped_between_one_and_the_bound() {
    let server = server();
    for (configured, expected) in [(0u32, 1usize), (1, 1), (3, 3), (4, 4), (5, 4), (64, 4)] {
        server
            .config
            .update(|c| c.world.ai_worker_threads = configured);
        assert_eq!(
            batch_worker_count(&server),
            expected,
            "configured {configured} must clamp to {expected}"
        );
    }
}

/// The plan split keeps the `AddAi` order, partitions every candidate exactly once and
/// never creates more than `MAX_AI_WORKERS` jobs (or none for an empty plan).
#[test]
fn plan_chunks_bound_the_jobs_and_keep_the_plan_order() {
    let manager = AiManager::default();
    let npcs: Vec<NpcRef> = (0..10)
        .map(|i| Entity::new(0x300 + i, Npc::new(1)))
        .collect();
    let handles: Vec<Arc<AiHandle>> = npcs
        .iter()
        .map(|n| manager.add_ai(n, AiKind::Default))
        .collect();
    let ordinals: Vec<u64> = handles.iter().map(|h| h.ordinal).collect();

    assert!(
        plan_chunks(&[], 4).is_empty(),
        "an empty plan costs no jobs"
    );
    let single = plan_chunks(&handles, 1);
    assert_eq!(single.len(), 1, "the default is one sequential job");
    assert_eq!(
        single[0].iter().map(|h| h.ordinal).collect::<Vec<_>>(),
        ordinals
    );

    for workers in [2usize, 3, 4, 99] {
        let bound = workers.min(MAX_AI_WORKERS);
        let chunks = plan_chunks(&handles, workers);
        assert!(
            chunks.len() <= bound,
            "at most {bound} jobs for {workers} workers"
        );
        assert!(chunks.iter().all(|c| !c.is_empty()), "no empty jobs");
        let flat: Vec<u64> = chunks.iter().flatten().map(|h| h.ordinal).collect();
        assert_eq!(flat, ordinals, "each candidate exactly once, in plan order");
    }
    drop(npcs);
}

/// Two concurrent batches serialize on the manager lock: every selected NPC is visited
/// once per batch, with no busy-skips or lost visits.
#[tokio::test]
async fn concurrent_batches_serialize_instead_of_overlapping() {
    let server = server();
    let npcs: Vec<NpcRef> = (0..8)
        .map(|i| {
            spawn_npc(
                &server,
                0x210 + i,
                1,
                104.0 + i as f32,
                100.0,
                AiKind::Default,
            )
        })
        .collect();
    idle(&server, INITIALIZING_TICKS as usize + 2);
    let (_client, _player) = spawn_player_at(&server, 0x100, 110.0, 100.0);
    let before: Vec<u64> = npcs
        .iter()
        .map(|n| server.systems.ai.ais.visits(n.obj_id).unwrap())
        .collect();

    let (a, b) = tokio::join!(
        tick_batch_with_workers(&server, 100, 4),
        tick_batch_with_workers(&server, 100, 4),
    );
    assert_eq!(a.candidates, npcs.len(), "{a:?}");
    assert_eq!(b.candidates, npcs.len(), "{b:?}");
    assert_eq!(a.busy + b.busy, 0, "{a:?} {b:?}");
    assert_eq!(a.ticked + b.ticked, npcs.len() * 2, "{a:?} {b:?}");
    for (npc, before) in npcs.iter().zip(before) {
        assert_eq!(
            server.systems.ai.ais.visits(npc.obj_id).unwrap(),
            before + 2,
            "npc {:#x} must be visited once per batch",
            npc.obj_id
        );
    }
}

/// Cancelling a caller must not release the batch lock while the owned work is still in
/// flight: the detached supervisor keeps the serialized batch running, and the next batch
/// stays queued behind it until the work completes. Channels synchronize the steps.
#[tokio::test]
async fn cancelled_batch_caller_cannot_unlock_a_running_batch() {
    let server = server();
    let gated = spawn_npc(&server, 0x210, 1, 110.0, 100.0, AiKind::Default);
    let other = spawn_npc(&server, 0x211, 1, 112.0, 100.0, AiKind::Default);
    let (_client, _player) = spawn_player_at(&server, 0x100, 115.0, 100.0);

    // Queue a call on 0x210 while its state is held, then release the holder without
    // draining: the batch picks the call up and blocks inside it.
    let (held_tx, held_rx) = mpsc::channel();
    let (release_holder_tx, release_holder_rx) = mpsc::channel::<()>();
    let srv = server.clone();
    let holder = std::thread::spawn(move || {
        let _ = srv.systems.ai.ais.inspect(0x210, |_| {
            held_tx.send(()).unwrap();
            release_holder_rx.recv().unwrap();
        });
    });
    held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("AI lock not held");

    let (work_entered_tx, work_entered_rx) = tokio::sync::oneshot::channel::<()>();
    let (work_release_tx, work_release_rx) = mpsc::channel::<()>();
    post(&server, 0x210, move |_| {
        let _ = work_entered_tx.send(());
        work_release_rx.recv().unwrap();
    });
    release_holder_tx.send(()).unwrap();
    holder.join().unwrap();

    // First batch: its chunk ticks 0x210 and blocks in the queued call.
    let first = tokio::spawn({
        let srv = server.clone();
        async move { tick_batch_with_workers(&srv, 100, 2).await }
    });
    work_entered_rx
        .await
        .expect("the first batch must run the queued call");

    // Cancel the first caller: the supervisor must keep running and holding the lock.
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(
        server.systems.ai.ais.batch_lock.try_lock().is_err(),
        "cancelling the caller released the batch lock while its work was still running"
    );

    // The next batch arrives while the cancelled batch's work is still gated: it must queue
    // on the manager's batch lock, not run beside the in-flight jobs.
    let (b_started_tx, b_started_rx) = tokio::sync::oneshot::channel();
    let (b_done_tx, b_done_rx) = mpsc::channel::<AiTickDiagnostics>();
    let second = tokio::spawn({
        let srv = server.clone();
        async move {
            let _ = b_started_tx.send(());
            let diag = tick_batch_with_workers(&srv, 100, 2).await;
            let _ = b_done_tx.send(diag);
            diag
        }
    });
    b_started_rx.await.expect("the second batch must start");
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        b_done_rx.try_recv().is_err(),
        "the second batch completed while the cancelled batch's work was still running"
    );

    // Release the work: the cancelled batch finishes, then the second batch runs with every
    // AI free (no busy skips caused by an overlap).
    work_release_tx.send(()).unwrap();
    let diag = second.await.expect("second batch task must finish");
    assert_eq!(diag.candidates, 2, "{diag:?}");
    assert_eq!(diag.ticked, 2, "{diag:?}");
    assert_eq!(diag.busy, 0, "{diag:?}");

    drop((gated, other));
}

/// The claim cleanup verifies every claim against the **current** registry entry, not
/// against the sweep's registry snapshot: an AI registered while the sweep walks keeps its
/// initialization grace and `ActivateAiAlways` claim (and ticks without players), while the
/// late claims of the handle it replaced are still purged.
#[tokio::test]
async fn sweep_keeps_claims_of_an_ai_added_after_its_snapshot() {
    let server = server();
    let replaced_npc = Entity::new(0x270, Npc::new(1));
    let replaced = server.systems.ai.ais.add_ai(&replaced_npc, AiKind::Default);

    let srv = server.clone();
    let replaced_in_hook = replaced.clone();
    let mut diag = AiTickDiagnostics::default();
    maintenance_sweep_with(&server, &mut diag, move || {
        // Registered after the sweep's registry snapshot: the current handle of 0x270.
        let fresh = spawn_npc(&srv, 0x270, 1, 1500.0, 1500.0, AiKind::Default);
        srv.systems.ai.ais.mark_always_active(fresh.obj_id);
        // Late claims of the replaced handle (the safety net's race window): one per set.
        let ordinal = replaced_in_hook.ordinal;
        srv.systems
            .ai
            .ais
            .woken
            .insert(ordinal, replaced_in_hook.clone());
        srv.systems
            .ai
            .ais
            .always
            .insert(ordinal, replaced_in_hook.clone());
        srv.systems
            .ai
            .ais
            .initializing
            .insert(ordinal, (0, replaced_in_hook.clone()));
    });

    let manager = &server.systems.ai.ais;
    let current = manager.get(0x270).expect("the new handle is registered");
    assert!(!Arc::ptr_eq(&current, &replaced));
    assert!(replaced.removed.load(Ordering::Acquire));
    assert!(
        manager.initializing.contains_key(&current.ordinal),
        "the new handle lost its initialization grace"
    );
    assert!(
        manager.always.contains_key(&current.ordinal),
        "the new handle lost its ActivateAiAlways claim"
    );
    assert!(
        !manager.woken.contains_key(&replaced.ordinal)
            && !manager.always.contains_key(&replaced.ordinal)
            && !manager.initializing.contains_key(&replaced.ordinal),
        "the replaced handle's late claims must still be purged"
    );

    // No players anywhere: only the surviving always claim can tick the new handle, past the
    // initialization grace too.
    let ticks = INITIALIZING_TICKS as usize + 2;
    idle(&server, ticks);
    assert_eq!(manager.visits(0x270).unwrap(), ticks as u64);
    let stats = manager.tick_stats();
    assert_eq!(stats.candidates, 1, "{stats:?}");
    assert_eq!(stats.ticked, 1, "{stats:?}");
}
