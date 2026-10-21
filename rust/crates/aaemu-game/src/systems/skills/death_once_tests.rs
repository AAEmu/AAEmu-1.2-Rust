//! The alive -> dead transition is once-only under concurrent lethal hits: skill hits, DoTs /
//! regen damage, fall damage, GM commands and NPC deaths all funnel through
//! `skills::combat::reduce_current_hp`, which decides the transition under the unit's data
//! lock (`try_claim_death`). These tests race two hits on a multi-threaded runtime, many times,
//! and check that exactly one death packet, one loot generation, one quest kill credit and one
//! exp loss are produced.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use crate::data::GameData;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::npcs::hooks::{EconomyBridge, QuestsBridge};
use crate::systems::skills::types::KillReason;
use crate::test_support::{
    spawn_player, test_character, test_experience, test_server_with, TestClient,
};
use crate::world::{NpcRef, PlayerRef, WorldObject};

/// Counts the NPC death side effects that must be once-only.
#[derive(Default)]
struct KillRecorder {
    loot: AtomicUsize,
    quest_kills: AtomicUsize,
}

impl EconomyBridge for KillRecorder {
    fn on_npc_loot(
        &self,
        _server: &GameServer,
        _npc: &NpcRef,
        _killer: Option<&PlayerRef>,
    ) -> bool {
        self.loot.fetch_add(1, Ordering::SeqCst);
        false
    }
}

impl QuestsBridge for KillRecorder {
    fn on_npc_killed(&self, _server: &GameServer, _player: &PlayerRef, _npc: &NpcRef) {
        self.quest_kills.fetch_add(1, Ordering::SeqCst);
    }
}

/// Starts two hits on separate OS threads at a barrier, so both pass the unlocked HP check of
/// `reduce_current_hp` before either applies its damage - the race under test.
fn race_two_lethal_hits(
    server: &Arc<GameServer>,
    target: &WorldObject,
    attacker: &WorldObject,
    damage: i32,
) {
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let server = server.clone();
            let target = target.clone();
            let attacker = attacker.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                crate::systems::skills::combat::reduce_current_hp(
                    &server,
                    &target,
                    Some(&attacker),
                    damage,
                    KillReason::Damage,
                );
            });
        }
    });
}

fn npc_data() -> GameData {
    let mut gd = crate::systems::npcs::npc::manager::tests::npc_game_data();
    gd.experience = test_experience(55);
    gd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_lethal_hits_kill_an_npc_once() {
    const ITERATIONS: usize = 64;
    let server = Arc::new(test_server_with(npc_data()));
    server.set_self();
    crate::systems::npcs::install_hooks(&server);
    let recorder = Arc::new(KillRecorder::default());
    server.systems.npcs.hooks.economy.install(recorder.clone());
    server.systems.npcs.hooks.quests.install(recorder.clone());

    let mut client = TestClient::new(&server, 1, 1001);
    let mut killer = test_character(1, "Killer", 100.0, 100.0);
    killer.unit.hp = 100;
    killer.unit.max_hp = 100;
    let killer = spawn_player(&server, &client, killer, 0x100);
    let killer = WorldObject::from(&killer);
    client.drain();

    for (iteration, obj_id) in (0..ITERATIONS).zip(0x1000u32..) {
        let npc =
            crate::systems::npcs::npc::manager::create(&server, obj_id, 200).expect("test npc");
        {
            let mut n = npc.data();
            n.unit.faction_id = 0;
            n.unit.hp = 100;
            n.unit.max_hp = 100;
            n.unit.transform.set_xyz(105.0, 100.0, 100.0);
        }
        let world = server.world.world(0).expect("instance 0");
        world.spawn(&npc, &server.view_ctx());
        client.drain();
        recorder.loot.store(0, Ordering::SeqCst);
        recorder.quest_kills.store(0, Ordering::SeqCst);

        let npc_obj = WorldObject::from(&npc);
        race_two_lethal_hits(&server, &npc_obj, &killer, 60);

        let ops = client.drain_ops();
        assert_eq!(
            ops.iter()
                .filter(|op| **op == sc::SCUnitDeathPacket)
                .count(),
            1,
            "iteration {iteration}: death packets {ops:x?}"
        );
        assert_eq!(
            recorder.loot.load(Ordering::SeqCst),
            1,
            "iteration {iteration}: loot generated more than once"
        );
        assert_eq!(
            recorder.quest_kills.load(Ordering::SeqCst),
            1,
            "iteration {iteration}: quest kill credited more than once"
        );
        assert!(npc.data().unit.is_dead());
        assert_eq!(npc.data().unit.hp, 0);

        world.delete(&npc);
        client.drain();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_lethal_hits_kill_a_player_once() {
    const ITERATIONS: usize = 64;
    let server = Arc::new(test_server_with(npc_data()));
    server.set_self();

    let mut client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Victim", 100.0, 100.0);
    character.unit.level = 20;
    character.unit.hp = 100;
    character.unit.max_hp = 100;
    character.unit.mp = 50;
    character.unit.max_mp = 50;
    // Start of level 20 in `test_experience(55)`: each level is 1 000 exp.
    character.experience = 20_500;
    let victim = spawn_player(&server, &client, character, 0x100);
    let victim_obj = WorldObject::from(&victim);

    let npc = crate::systems::npcs::npc::manager::create(&server, 0x200, 200).expect("test npc");
    npc.data().unit.transform.set_xyz(102.0, 100.0, 100.0);
    let world = server.world.world(0).expect("instance 0");
    world.spawn(&npc, &server.view_ctx());
    let attacker = WorldObject::from(&npc);
    client.drain();

    for iteration in 0..ITERATIONS {
        {
            let mut c = victim.data();
            c.unit.hp = 100;
            c.unit.mp = 50;
            c.experience = 20_500;
            c.recoverable_exp = 0;
        }
        // A restored HP pool starts a new life (the resurrection / repair path).
        crate::systems::skills::combat::post_update_current_hp(
            &server,
            &victim_obj,
            Some(&victim_obj),
            0,
            100,
            KillReason::Unknown,
        );
        client.drain();

        race_two_lethal_hits(&server, &victim_obj, &attacker, 60);

        let ops = client.drain_ops();
        assert_eq!(
            ops.iter()
                .filter(|op| **op == sc::SCUnitDeathPacket)
                .count(),
            1,
            "iteration {iteration}: death packets {ops:x?}"
        );
        assert_eq!(
            ops.iter()
                .filter(|op| **op == sc::SCExpChangedPacket)
                .count(),
            1,
            "iteration {iteration}: exp was lost more than once"
        );
        assert_eq!(
            ops.iter()
                .filter(|op| **op == sc::SCRecoverableExpPacket)
                .count(),
            1,
            "iteration {iteration}: recoverable exp reported more than once"
        );
        let c = victim.data();
        // GetExpLoss(20, 0.05) = floor(1 000 * 0.05) = 50, 80 % recoverable.
        assert_eq!(c.experience, 20_450, "iteration {iteration}");
        assert_eq!(c.recoverable_exp, 40, "iteration {iteration}");
        assert_eq!(c.unit.hp, 0, "iteration {iteration}");
    }
}
