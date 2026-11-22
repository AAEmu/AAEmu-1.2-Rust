//! Kroloal Cradle scripted events: the Metamoze (Arkanis) cut scene and the scripted squads.
//!
//! A child module of `cradle_dungeon_tests`, so it shares that file's real-data fixture.

use super::*;

/// [`place_player`] that keeps the player's client, so a test can read what was sent to it.
fn place_viewer(
    server: &Arc<GameServer>,
    world: &Arc<WorldInstance>,
    pid: u32,
    x: f32,
    y: f32,
    z: f32,
) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, pid, pid);
    let mut ch = test_character(pid, &format!("P{pid}"), x, y);
    ch.unit.hp = 10_000;
    ch.unit.max_hp = 10_000;
    ch.is_online = true;
    ch.unit.transform.instance_id = world.id;
    ch.unit.transform.zone_id = ZONE;
    ch.unit.transform.set_xyz(x, y, z);
    ch.obj_id = server.ids.object.get_next_id().unwrap();
    let p = Player::new(ch, client.conn.handle());
    server.world.try_add_character(&p);
    world.spawn(&p, &server.view_ctx());
    (client, p)
}

fn sequences(client: &mut TestClient) -> Vec<Vec<u8>> {
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCPlaySequencePacket)
        .map(|(_, body)| body)
        .collect()
}

/// Metamoze's room: sphere 1969 (enter, `trigger_condition_id` 3 / 3000 ms) casts 19565, whose
/// `NpcControlEffect` (tag 830) runs command set 207 on the invisible NPC 12157: skill 19566
/// gives it the 0.5 s buff 4526, whose Timeout trigger casts 19562; its `CinemaEffect` 13 is
/// cinema 52 `id_264_01`. Then 19567 removes the invisible NPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_metamoze_sphere_plays_the_boss_room_cinema() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    // The live server installs every subsystem; the cinema effect is the quest bridge.
    crate::systems::quests::bridges::install(&server);
    let _ = load_cradle(&server, &conn_of(&db));
    let areas: Vec<crate::systems::quests::spheres::SphereArea> =
        crate::systems::quests::spheres::load_sphere_areas(&app_root(), |id| id == 1969)
            .into_iter()
            .filter(|a| a.world == WORLD)
            .collect();
    assert_eq!(areas.len(), 1, "the shipped sphere 1969 fill row");
    server.systems.quests.spheres.set_areas(areas);

    // One player steps into the sphere, a party member waits at the room's entrance, a third
    // is in the Sprigg room on the other side of the dungeon.
    let (mut inside, _p1) = place_viewer(&server, &world, 1, 650.331, 435.433, 113.433);
    let (mut entrance, _p2) = place_viewer(&server, &world, 2, 650.9, 460.0, 113.4);
    let (mut away, _p3) = place_viewer(&server, &world, 3, 176.1, 723.5, 182.4);
    for _ in 0..8 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let trigger = npcs_of(&world, 12157);
    assert_eq!(trigger.len(), 1, "the plain trigger row places 12157");
    for client in [&mut inside, &mut entrance, &mut away] {
        client.drain();
    }

    server.systems.quests.spheres.tick_at(&server, 0);
    server.systems.quests.spheres.tick_at(&server, 3_000);
    let expected = b"\x09\x00id_264_01\x00\x00\x00".to_vec();
    let mut seen_inside = Vec::new();
    let mut seen_entrance = Vec::new();
    for _ in 0..60 {
        tick_ai(&server, 1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        seen_inside.extend(sequences(&mut inside));
        seen_entrance.extend(sequences(&mut entrance));
        if !seen_inside.is_empty() && npcs_of(&world, 12157).is_empty() {
            break;
        }
    }
    assert_eq!(seen_inside, vec![expected.clone()], "the sphere's player");
    assert_eq!(seen_entrance, vec![expected], "a party member in the room");
    assert!(sequences(&mut away).is_empty(), "a player in another room");
    assert!(
        npcs_of(&world, 12157).is_empty(),
        "19567 removes the invisible cut-scene NPC"
    );
}

/// The `SCOneUnitMovementPacket` steps sent to `client`: object, position, actor flags and
/// forward delta.
fn unit_steps(client: &mut TestClient) -> Vec<(u32, f32, f32, u8, i8)> {
    use crate::models::movement::{MoveKind, MoveType};
    use aaemu_commons::PacketStream;
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCOneUnitMovementPacket)
        .filter_map(|(_, bytes)| {
            let mut stream = PacketStream::from_bytes(bytes);
            let obj_id = stream.read_bc().ok()?;
            let kind = stream.read_u8().ok()?;
            match MoveType::read(kind, &mut stream).ok()?.kind {
                MoveKind::Unit(step) => Some((
                    obj_id,
                    step.x,
                    step.y,
                    step.actor_flags.0,
                    step.delta_movement[1],
                )),
                _ => None,
            }
        })
        .collect()
}

/// Metamoze's room after the kill: Arkanis's death skill 19236 spawns Kuwon (12185) and two
/// Royal Stormers (12186/12187), whose `OnSpawn` skills run command sets 234-236. Their route
/// files have no `Speed` point; they walk at the human walk (model 18: 1.8 m/s, walk flags),
/// not the C# default of 1 m/s, and Kuwon holds for his authored 10 + 2 + 12.5 s of lines at
/// the end of `metamoze_commander_move1` before `metamoze_commander_move2`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_cradle_metamoze_squad_walks_its_scene_at_its_own_pace() {
    use crate::systems::skills::types::skill_result;
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_server(&db);
    crate::systems::quests::bridges::install(&server);
    server
        .systems
        .ai
        .set_paths_folder(app_root().join("Data").join("Path"));
    let _ = load_cradle(&server, &conn_of(&db));
    server
        .systems
        .npcs
        .spawns
        .get(INSTANCE_ID)
        .unwrap()
        .spawn_doodads(&server);
    let (mut viewer, player) = place_viewer(&server, &world, 1, 650.9, 393.2, 113.4);
    for _ in 0..8 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let arkanis = npcs_of(&world, 11171);
    assert!(!arkanis.is_empty());
    player.data().unit.transform.set_xyz(650.9, 420.0, 113.4);
    let arkanis = WorldObject::from(&arkanis[0]);
    arkanis.with_unit(|u| u.unit_mut().hp = 0);
    assert_eq!(
        crate::systems::skills::skill::unit_use_skill(&server, &arkanis, 19236, &arkanis),
        skill_result::SUCCESS
    );
    viewer.drain();
    tick_ai(&server, 1);
    let squad: Vec<u32> = [12185, 12186, 12187]
        .into_iter()
        .map(|unit| {
            let npcs = npcs_of(&world, unit);
            assert_eq!(npcs.len(), 1, "Royal Stormer {unit}");
            npcs[0].obj_id
        })
        .collect();
    let commander = npcs_of(&world, 12185).remove(0);

    // `metamoze_commander_move1` is 34.8 m: about 19.3 s at 1.8 m/s (35 s at 1 m/s).
    let end = (649.776_f32, 357.056_f32);
    let mut steps = Vec::new();
    let mut arrived_tick = None;
    for tick in 0..260 {
        tick_ai(&server, 1);
        tokio::task::yield_now().await;
        steps.extend(unit_steps(&mut viewer));
        let t = commander.data().unit.transform;
        if (t.x - end.0).hypot(t.y - end.1) <= 1.0 {
            arrived_tick = Some(tick);
            break;
        }
    }
    let arrived_tick = arrived_tick.expect("Kuwon reaches the end of his first route");
    assert!(
        (170..=215).contains(&arrived_tick),
        "34.8 m at the 1.8 m/s walk takes about 193 ticks, took {arrived_tick}"
    );
    for obj_id in &squad {
        // A zero actor-flag update (the spawn facing) is not a step.
        let mine: Vec<_> = steps
            .iter()
            .filter(|s| s.0 == *obj_id && s.3 != 0)
            .collect();
        assert!(
            mine.len() > 100,
            "{obj_id} walks its route: {} steps",
            mine.len()
        );
        assert!(
            mine.iter().all(|s| s.3 == 5 && s.4 == 63),
            "{obj_id} uses the client walk flags and forward delta"
        );
        let longest = mine
            .windows(2)
            .map(|w| (w[1].1 - w[0].1).hypot(w[1].2 - w[0].2))
            .fold(0.0_f32, f32::max);
        assert!(
            (0.17..=0.19).contains(&longest),
            "{obj_id} steps 0.18 m per 100 ms tick, longest {longest}"
        );
    }

    // The authored pauses are seconds: Kuwon stays for his lines (10 + 2 + 12.5 s).
    let held = commander.data().unit.transform;
    for _ in 0..200 {
        tick_ai(&server, 1);
        tokio::task::yield_now().await;
    }
    let t = commander.data().unit.transform;
    assert!(
        (t.x - held.x).hypot(t.y - held.y) < 0.5,
        "Kuwon holds his place for his lines: ({}, {}) -> ({}, {})",
        held.x,
        held.y,
        t.x,
        t.y
    );
}
