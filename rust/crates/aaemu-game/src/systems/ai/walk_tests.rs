//! Wall checks of the straight AI walks (roaming, returning home): a synthetic wall in the
//! collision world, see `collision/walk.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use aaemu_physics::math::Vec3 as PVec3;

use crate::data::world_templates::WorldTemplate;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::skills::controllers::Vec3;
use crate::test_support::test_server_with;
use crate::world::{Entity, NpcRef, WorldObject};

use super::super::collision::flow_tests::{add_wall_boxes, enable_collision};
use super::super::framework::{AiCtx, AiKind, BehaviorKind, NpcAi};
use super::super::geodata::bai::test_files::net_mission;
use super::super::geodata::bai::UsedAreaNames;
use super::super::geodata::manager::AiGeoData;
use super::chase_tests::Files;

const HOME: (f32, f32) = (100.0, 100.0);

fn server(collision: bool) -> (crate::server::GameServer, NpcRef) {
    let mut data = GameData::empty();
    data.systems.npcs.npc.templates.insert(
        1,
        NpcTemplate {
            id: 1,
            ..NpcTemplate::default()
        },
    );
    let server = test_server_with(data);
    if collision {
        enable_collision(&server, 100.0);
    }
    let mut n = Npc::new(1);
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.transform.set_xyz(HOME.0, HOME.1, 100.0);
    n.spawner_z = Some(100.0);
    let npc = Entity::new(0x300, n);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    (server, npc)
}

fn ctx<'a>(server: &'a crate::server::GameServer, npc: &NpcRef, ai: &'a mut NpcAi) -> AiCtx<'a> {
    ai.idle_position = Vec3::new(HOME.0, HOME.1, 100.0);
    AiCtx {
        server,
        npc: npc.clone(),
        obj: WorldObject::from(npc),
        ai,
    }
}

/// Whether the horizontal segment `a -> b` touches the rectangle `(x0, x1, y0, y1)`.
fn crosses(a: Vec3, b: Vec3, (x0, x1, y0, y1): (f32, f32, f32, f32)) -> bool {
    (0..=200).any(|i| {
        let t = i as f32 / 200.0;
        let (x, y) = (a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
        x >= x0 && x <= x1 && y >= y0 && y <= y1
    })
}

#[tokio::test]
async fn roam_targets_are_never_behind_a_wall() {
    let (server, npc) = server(true);
    // a 6 m cube, x 103..109, y 97..103, east of the idle position
    add_wall_boxes(&server, &[(42, PVec3::new(106.0, 100.0, 103.0), 3.0)]);
    let mut ai = NpcAi::new(AiKind::Roaming, &npc);
    let c = ctx(&server, &npc, &mut ai);
    let from = Vec3::new(HOME.0, HOME.1, 100.0);
    let wall = (103.0, 109.0, 97.0, 103.0);
    let mut blocked_raw = 0;
    for _ in 0..300 {
        let raw = super::calc_next_roaming_position(&c);
        blocked_raw += usize::from(crosses(from, raw, wall));
        let p = super::choose_roaming_position(&c).expect("an open side exists");
        assert!(!crosses(from, p, wall), "roam target {p:?} behind the wall");
    }
    // the wall is in the way of a real share of the raw candidates
    assert!(blocked_raw > 20, "{blocked_raw}");
}

/// Four cubes around the NPC, their faces `gap` m from it.
fn cage(server: &crate::server::GameServer, gap: f32) {
    let c = gap + 3.0;
    let boxes: Vec<_> = [(c, 0.0), (-c, 0.0), (0.0, c), (0.0, -c)]
        .into_iter()
        .enumerate()
        .map(|(i, (dx, dy))| {
            (
                50 + i as u32,
                PVec3::new(HOME.0 + dx, HOME.1 + dy, 103.0),
                3.0,
            )
        })
        .collect();
    add_wall_boxes(server, &boxes);
}

#[tokio::test]
async fn walled_in_roamer_walks_short() {
    let (server, npc) = server(true);
    cage(&server, 2.5);
    let mut ai = NpcAi::new(AiKind::Roaming, &npc);
    let c = ctx(&server, &npc, &mut ai);
    let from = Vec3::new(HOME.0, HOME.1, 100.0);
    let mut short = 0;
    for _ in 0..100 {
        if let Some(p) = super::choose_roaming_position(&c) {
            assert!(
                (p.x - from.x).abs() < 2.6 && (p.y - from.y).abs() < 2.6,
                "left the cage: {p:?}"
            );
            short += 1;
        }
    }
    assert!(short > 0);
}

#[tokio::test]
async fn tightly_walled_in_roamer_skips_the_cycle() {
    let (server, npc) = server(true);
    cage(&server, 1.0);
    let mut ai = NpcAi::new(AiKind::Roaming, &npc);
    let c = ctx(&server, &npc, &mut ai);
    for _ in 0..100 {
        // a candidate inside the cage is free; every other one is cut below the minimum
        if let Some(p) = super::choose_roaming_position(&c) {
            assert!(
                (p.x - HOME.0).abs() < 1.0 && (p.y - HOME.1).abs() < 1.0,
                "{p:?}"
            );
        }
    }
}

#[tokio::test]
async fn without_the_collision_service_roaming_is_unchanged() {
    let (server, npc) = server(false);
    let mut ai = NpcAi::new(AiKind::Roaming, &npc);
    let c = ctx(&server, &npc, &mut ai);
    for _ in 0..100 {
        let p = super::choose_roaming_position(&c).expect("first candidate");
        assert!((p.x - HOME.0).abs() <= 6.0 && (p.y - HOME.1).abs() <= 6.0);
    }
    assert_eq!(
        super::wall_ahead(&c, Vec3::new(110.0, 100.0, 100.0)),
        None,
        "no service, no wall"
    );
}

/// An NPC 10 m west of its home with a wall (x 94..96, y 90..110) between; the navmesh has a
/// node around the wall's north end.
fn return_fixture(collision: bool) -> (crate::server::GameServer, NpcRef) {
    let (server, npc) = server(collision);
    server.config.update(|cfg| cfg.world.geo_data_mode = true);
    let files = Files(HashMap::from([(
        "game/worlds/main_world/paths/000_000/netmission0.bai".into(),
        net_mission(
            &[
                (1, [90.0, 100.0, 100.0]),
                (2, [95.0, 114.0, 100.0]),
                (3, [100.0, 100.0, 100.0]),
            ],
            &[(1, 2), (2, 1), (2, 3), (3, 2)],
        ),
    )]));
    server.systems.ai.geodata.install(Arc::new(AiGeoData::new(
        &WorldTemplate::synthetic(0, "main_world", 2, 2, 100),
        true,
        Arc::new(files),
        Arc::new(UsedAreaNames::default()),
        None,
    )));
    if collision {
        // a column of 2 m cubes
        let boxes: Vec<_> = (0..10)
            .map(|i| (70 + i, PVec3::new(95.0, 91.0 + 2.0 * i as f32, 101.0), 1.0))
            .collect();
        add_wall_boxes(&server, &boxes);
    }
    npc.data().unit.transform.set_xyz(90.0, 100.0, 100.0);
    (server, npc)
}

fn run_return(
    server: &crate::server::GameServer,
    npc: &NpcRef,
    steps: usize,
    detour: bool,
) -> Vec<Vec3> {
    let mut ai = NpcAi::new(AiKind::HoldPosition, npc);
    let mut c = ctx(server, npc, &mut ai);
    super::return_state_enter(&mut c, BehaviorKind::ReturnState);
    if detour {
        // The synthetic navmesh has no hole for the wall, so its route is the straight line;
        // a real one goes around. Check that a route was planned, then give it the detour.
        assert!(!c.ai.path_node.found_path.is_empty(), "no route planned");
        c.ai.path_node.found_path = [
            Vec3::new(95.0, 114.0, 100.0),
            Vec3::new(100.0, 100.0, 100.0),
        ]
        .into();
    }
    let mut trail = Vec::new();
    for _ in 0..steps {
        super::return_state_tick(&mut c, BehaviorKind::ReturnState, 100);
        let t = npc.data().unit.transform;
        trail.push(Vec3::new(t.x, t.y, t.z));
    }
    trail
}

#[tokio::test]
async fn return_home_walks_around_a_wall_when_the_navmesh_has_a_way() {
    let (server, npc) = return_fixture(true);
    // the wall blocks the straight way
    assert!(crate::systems::ai::collision::walk::walk_blocked(
        &server,
        0,
        [90.0, 100.0, 100.0],
        [100.0, 100.0, 100.0]
    )
    .unwrap());
    let trail = run_return(&server, &npc, 400, true);
    let in_wall = |p: &&Vec3| p.x > 93.7 && p.x < 96.3 && p.y > 90.0 && p.y < 110.0;
    assert!(
        trail.iter().find(in_wall).is_none(),
        "went through the wall: {:?}",
        trail.iter().find(in_wall)
    );
    let end = *trail.last().unwrap();
    assert!(
        super::calculate_distance(end, Vec3::new(100.0, 100.0, 100.0), false) < 1.5,
        "did not get home: {end:?}"
    );
}

#[tokio::test]
async fn return_home_without_the_service_walks_straight_as_before() {
    let (server, npc) = return_fixture(false);
    let trail = run_return(&server, &npc, 100, false);
    assert!(
        trail.iter().all(|p| (p.y - 100.0).abs() < 1e-3),
        "{trail:?}"
    );
}
