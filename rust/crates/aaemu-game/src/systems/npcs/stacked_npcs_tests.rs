//! Tests of the spawn scatter, the stacked-NPC check and `separate_npc`.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::data::world_templates::{WorldSpawnPosition, WorldTemplate};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::systems::npcs::npc::group_data::{NpcGroup, NpcGroupData, NpcGroupMember};
use crate::systems::npcs::npc::spawner::NpcSpawnerInit;
use crate::systems::npcs::npc::spawner::{spawn_into_world, NpcSpawner, NpcSpawnerShared};
use crate::systems::npcs::npc::spawner_data::{NpcSpawnerData, NpcSpawnerNpc, NpcSpawnerTemplate};
use crate::systems::npcs::spawn_manager::backend::TestBackend;
use crate::test_support::{test_server_with, TEST_ZONE};
use crate::world::Entity;

const Z: f32 = 100.0;

struct Backend;

impl TestBackend for Backend {
    fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
        let id = if obj_id == 0 {
            server.ids.object.get_next_id().ok()?
        } else {
            obj_id
        };
        let mut n = Npc::new(template_id);
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.level = 10;
        let npc = Entity::new(id, n);
        // The real `NpcManager.Create` attaches the AI (the home position is checked below).
        crate::systems::ai::hooks::create_ai(server, &npc, 22);
        Some(npc)
    }

    fn doodad_create(
        &self,
        _server: &GameServer,
        _world: &Arc<WorldInstance>,
        _obj_id: u32,
        _template_id: u32,
    ) -> Option<crate::world::DoodadRef> {
        None
    }
}

fn server_with(data: GameData) -> Arc<GameServer> {
    let s = Arc::new(test_server_with(data));
    s.set_self();
    crate::systems::skills::bind(&s);
    crate::systems::ai::hooks::install(&s);
    *s.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    s
}

fn server() -> Arc<GameServer> {
    server_with(GameData::empty())
}

fn member(id: u32, spawner: u32, npc: u32) -> NpcSpawnerNpc {
    NpcSpawnerNpc {
        id,
        npc_spawner_template_id: spawner,
        member_id: npc,
        member_type: "Npc".into(),
        weight: 1.0,
        unit_id: 0,
    }
}

fn template(id: u32, npcs: Vec<NpcSpawnerNpc>, max_population: u32) -> NpcSpawnerTemplate {
    NpcSpawnerTemplate {
        id,
        spawn_delay_min: 10.0,
        spawn_delay_max: 10.0,
        max_population,
        npcs,
        ..NpcSpawnerTemplate::default()
    }
}

fn spawner_at(
    template: NpcSpawnerTemplate,
    unit_id: u32,
    count: u32,
    x: f32,
    y: f32,
) -> Arc<NpcSpawner> {
    let spawner_id = template.id;
    let spawnable_npcs = template.npcs.clone();
    let position = WorldSpawnPosition {
        world_id: 0,
        zone_id: TEST_ZONE,
        x,
        y,
        z: Z,
        ..WorldSpawnPosition::default()
    };
    Arc::new(NpcSpawner::new(
        NpcSpawnerInit {
            id: spawner_id,
            spawner_id,
            unit_id,
            follow_path: String::new(),
            follow_npc: 0,
            respawn_time: 10,
            despawn_time: 20,
            special_link: String::new(),
            count,
            npc_spawner_ids: vec![spawner_id],
            parent_world: Some(0),
            template: Some(Arc::new(template)),
            spawnable_npcs,
        },
        NpcSpawnerShared::new(position),
    ))
}

fn pos_of(npc: &NpcRef) -> (f32, f32, f32) {
    let t = npc.data().unit.transform;
    (t.x, t.y, t.z)
}

fn home_of(server: &GameServer, npc: &NpcRef) -> (f32, f32, f32) {
    server
        .systems
        .ai
        .ais
        .inspect(npc.obj_id, |ai| {
            (ai.home_position.x, ai.home_position.y, ai.home_position.z)
        })
        .expect("the NPC has an AI")
}

fn all_pos(server: &GameServer, instance: u32) -> Vec<(u32, (f32, f32, f32))> {
    let mut v: Vec<_> = server
        .world
        .world(instance)
        .unwrap()
        .all_npcs()
        .iter()
        .map(|n| (n.obj_id, pos_of(n)))
        .collect();
    v.sort_by_key(|(id, _)| *id);
    v
}

fn min_distance(points: &[(u32, (f32, f32, f32))]) -> f32 {
    let mut min = f32::INFINITY;
    for (i, a) in points.iter().enumerate() {
        for b in &points[i + 1..] {
            min = min.min(dist3(a.1, b.1));
        }
    }
    min
}

/// An NPC of `template_id` standing at `(x, y, Z)` in `instance`.
fn put_npc(server: &GameServer, instance: u32, template_id: u32, x: f32, y: f32) -> NpcRef {
    let npc = Backend
        .npc_create(server, 0, template_id)
        .expect("backend creates");
    {
        let mut d = npc.data();
        d.unit.transform.set_xyz(x, y, Z);
        d.unit.transform.instance_id = instance;
    }
    let world = server.world.world(instance).expect("instance");
    spawn_into_world(server, &world, &npc);
    npc
}

#[tokio::test]
async fn count_three_spawner_scatters_and_keeps_the_home_position() {
    let server = server();
    let spawner = spawner_at(
        template(10, vec![member(100, 10, 500)], 5),
        500,
        3,
        100.0,
        100.0,
    );
    spawner.do_spawn(&server);

    let world = server.world.world(0).unwrap();
    let npcs = world.all_npcs();
    assert_eq!(npcs.len(), 3);
    let points = all_pos(&server, 0);
    assert!(
        min_distance(&points) >= 1.5,
        "the three NPCs are at least 1.5 m apart: {points:?}"
    );
    let mut at_origin = 0;
    for npc in &npcs {
        let p = pos_of(npc);
        if dist3(p, (100.0, 100.0, Z)) < 0.01 {
            at_origin += 1;
        } else {
            let d = dist3(p, (100.0, 100.0, Z));
            assert!((1.5 - 0.01..=3.0 + 0.01).contains(&d), "ring distance {d}");
        }
        assert_eq!(
            home_of(&server, npc),
            p,
            "home position = the scattered spot"
        );
    }
    assert_eq!(at_origin, 1, "the first NPC keeps the authored spot");
    let sp = spawner.position();
    assert_eq!(
        (sp.x, sp.y, sp.z),
        (100.0, 100.0, Z),
        "spawner position unchanged"
    );
}

#[tokio::test]
async fn second_spawner_on_the_same_coordinates_is_scattered() {
    let server = server();
    let a = spawner_at(
        template(10, vec![member(100, 10, 500)], 1),
        500,
        1,
        100.0,
        100.0,
    );
    let b = spawner_at(
        template(11, vec![member(101, 11, 501)], 1),
        501,
        1,
        100.0,
        100.0,
    );
    a.do_spawn(&server);
    b.do_spawn(&server);
    let points = all_pos(&server, 0);
    assert_eq!(points.len(), 2);
    assert!(min_distance(&points) >= 1.5, "{points:?}");
    let first = server
        .world
        .world(0)
        .unwrap()
        .all_npcs()
        .into_iter()
        .find(|n| n.data().unit.template_id == 500)
        .unwrap();
    assert_eq!(pos_of(&first), (100.0, 100.0, Z), "the first spawn stays");
}

#[tokio::test]
async fn scatter_is_off_when_max_is_zero() {
    let server = server();
    server
        .config
        .update(|cfg| cfg.world.npc_spawn_scatter_max = 0.0);
    let spawner = spawner_at(
        template(10, vec![member(100, 10, 500)], 5),
        500,
        3,
        100.0,
        100.0,
    );
    spawner.do_spawn(&server);
    let points = all_pos(&server, 0);
    assert_eq!(points.len(), 3);
    assert!(
        points.iter().all(|(_, p)| *p == (100.0, 100.0, Z)),
        "all three stay on the authored coordinates: {points:?}"
    );
}

#[tokio::test]
async fn a_spawn_clear_of_other_npcs_is_not_moved() {
    let server = server();
    let a = spawner_at(
        template(10, vec![member(100, 10, 500)], 1),
        500,
        1,
        100.0,
        100.0,
    );
    let b = spawner_at(
        template(11, vec![member(101, 11, 501)], 1),
        501,
        1,
        110.0,
        100.0,
    );
    a.do_spawn(&server);
    b.do_spawn(&server);
    let b_npc = server
        .world
        .world(0)
        .unwrap()
        .all_npcs()
        .into_iter()
        .find(|n| n.data().unit.template_id == 501)
        .unwrap();
    assert_eq!(pos_of(&b_npc), (110.0, 100.0, Z));
}

#[tokio::test]
async fn npc_group_members_keep_their_authored_offsets() {
    let mut data = GameData::empty();
    data.systems.npcs.npc_groups = NpcGroupData::from_parts(
        vec![NpcGroup {
            id: 50,
            name: "pair".into(),
            aggro_rule_id: 1,
            enable_respawn: false,
        }],
        (0..2)
            .map(|i| NpcGroupMember {
                id: 190 + i,
                npc_group_id: 50,
                npc_id: 8564 + i,
                is_leader: i == 0,
                is_move_leader: i == 0,
                formation_offset_x: 0.0,
                formation_offset_y: 0.0,
                formation_offset_z: 0.0,
                formation_tension: 1.0,
            })
            .collect(),
    );
    let row = NpcSpawnerNpc {
        member_type: "NpcGroup".into(),
        ..member(300, 20, 50)
    };
    data.systems.npcs.npc_spawners =
        NpcSpawnerData::from_parts(vec![template(20, Vec::new(), 1)], vec![row.clone()]).unwrap();
    let server = server_with(data);
    let spawner = spawner_at(template(20, vec![row], 1), 8564, 1, 100.0, 100.0);
    spawner.do_spawn(&server);
    let points = all_pos(&server, 0);
    assert_eq!(points.len(), 2);
    assert!(
        points.iter().all(|(_, p)| *p == (100.0, 100.0, Z)),
        "formation members are authored on one spot and are not scattered: {points:?}"
    );
}

#[tokio::test]
async fn parented_npcs_neither_trigger_nor_move() {
    let server = server();
    // A parented NPC (local coordinates) on the spawn spot does not make the spot a stack.
    let rider = Backend.npc_create(&server, 0, 600).unwrap();
    {
        let mut d = rider.data();
        d.unit.transform.set_xyz(100.0, 100.0, Z);
        d.unit.transform.parent_obj_id = 0x7777;
    }
    let world = server.world.world(0).unwrap();
    spawn_into_world(&server, &world, &rider);
    let spawner = spawner_at(
        template(10, vec![member(100, 10, 500)], 1),
        500,
        1,
        100.0,
        100.0,
    );
    spawner.do_spawn(&server);
    let spawned = world
        .all_npcs()
        .into_iter()
        .find(|n| n.data().unit.template_id == 500)
        .unwrap();
    assert_eq!(pos_of(&spawned), (100.0, 100.0, Z));
    // ... and it is never separated, nor reported as stacked.
    let params = ScatterParams::for_fix(&server.config.world, 0.5);
    assert_eq!(separate_npc(&server, &world, &rider, params), None);
    assert_eq!(pos_of(&rider), (100.0, 100.0, Z));
    assert!(find_stacks(&world, 0.5).is_empty());
}

fn tick(server: &GameServer, base: Instant, secs: u64) -> usize {
    scan_once(server, base + Duration::from_secs(secs))
}

#[tokio::test]
async fn stack_is_logged_once_after_the_dwell_time() {
    let server = server();
    let a = put_npc(&server, 0, 500, 100.0, 100.0);
    let b = put_npc(&server, 0, 501, 100.0, 100.2);
    let base = spawn_manager::now();
    assert_eq!(tick(&server, base, 0), 0, "first sight starts the dwell");
    assert_eq!(tick(&server, base, 60), 0, "not before the dwell time");
    assert_eq!(tick(&server, base, 119), 0);
    assert_eq!(
        tick(&server, base, 120),
        1,
        "logged once the dwell time passed"
    );
    assert_eq!(tick(&server, base, 180), 0, "once per stack");
    assert_eq!(tick(&server, base, 240), 0);
    // A third NPC joining changes the member set: logged again.
    let _c = put_npc(&server, 0, 502, 100.1, 100.0);
    assert_eq!(tick(&server, base, 300), 1, "member set changed");
    assert_eq!(tick(&server, base, 360), 0);
    // After an hour it is repeated.
    assert_eq!(tick(&server, base, 300 + 3600), 1, "hourly repeat");
    // Separated: the stack is forgotten; a new stack starts its dwell again.
    b.data().unit.transform.set_xyz(120.0, 100.0, Z);
    _c.data().unit.transform.set_xyz(140.0, 100.0, Z);
    assert_eq!(tick(&server, base, 300 + 3660), 0);
    a.data().unit.transform.set_xyz(120.0, 100.2, Z);
    assert_eq!(tick(&server, base, 300 + 3720), 0, "dwell restarts");
}

#[tokio::test]
async fn dead_and_fighting_npcs_are_not_stacked() {
    let server = server();
    let a = put_npc(&server, 0, 500, 100.0, 100.0);
    let dead = put_npc(&server, 0, 501, 100.0, 100.0);
    dead.data().unit.hp = 0;
    let world = server.world.world(0).unwrap();
    assert!(
        find_stacks(&world, 0.5).is_empty(),
        "a corpse is not a stack member"
    );
    dead.data().unit.hp = 100;
    assert_eq!(find_stacks(&world, 0.5).len(), 1);
    dead.data().current_aggro_target = 42;
    assert!(
        find_stacks(&world, 0.5).is_empty(),
        "an NPC with an aggro target is skipped"
    );
    dead.data().current_aggro_target = 0;
    a.data().unit.is_in_battle = true;
    assert!(find_stacks(&world, 0.5).is_empty(), "so is one in battle");
}

#[tokio::test]
async fn stacks_are_not_found_across_instances() {
    let server = server();
    server.world.create_world_instance(
        Arc::new(WorldTemplate::synthetic(5, "dungeon", 2, 2, TEST_ZONE)),
        5,
    );
    put_npc(&server, 0, 500, 100.0, 100.0);
    put_npc(&server, 5, 501, 100.0, 100.0);
    let base = spawn_manager::now();
    for secs in [0, 120, 240] {
        assert_eq!(
            tick(&server, base, secs),
            0,
            "same coordinates, two instances"
        );
    }
    assert!(find_stacks(&server.world.world(0).unwrap(), 0.5).is_empty());
    assert!(find_stacks(&server.world.world(5).unwrap(), 0.5).is_empty());
}

#[tokio::test]
async fn separate_npc_moves_the_npc_and_its_home() {
    let server = server();
    server
        .config
        .update(|cfg| cfg.world.npc_spawn_scatter_max = 0.0);
    let a = put_npc(&server, 0, 500, 100.0, 100.0);
    let b = put_npc(&server, 0, 501, 100.0, 100.0);
    let world = server.world.world(0).unwrap();
    let stacks = find_stacks(&world, 0.5);
    assert_eq!(stacks.len(), 1);
    assert_eq!(
        stacks[0].iter().map(|m| m.obj_id).collect::<Vec<_>>(),
        vec![a.obj_id, b.obj_id]
    );
    // Scattering is off for spawns, but the fix uses the default ring.
    let params = ScatterParams::for_fix(&server.config.world, 0.5);
    let spot = separate_npc(&server, &world, &b, params).expect("moved");
    assert!(dist3(spot, (100.0, 100.0, Z)) >= 1.5);
    assert_eq!(pos_of(&b), spot);
    assert_eq!(home_of(&server, &b), spot, "the AI returns to the new spot");
    assert_eq!(pos_of(&a), (100.0, 100.0, Z));
    assert!(find_stacks(&world, 0.5).is_empty());
}
