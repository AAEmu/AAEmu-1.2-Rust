//! Hooked fish use authored controllers and broadcast their server-controlled motion.

use aaemu_commons::PacketStream;

use super::*;
use crate::data::world_templates::WorldTemplate;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::systems::ai::npc::tags;
use crate::systems::skills::geometry::pos_unit_from;
use crate::test_support::{
    spawn_player, test_character, test_server_with_world, TestClient, TEST_ZONE,
};
use crate::world::Entity;

fn hooked_fish() -> (GameServer, TestClient, WorldObject, WorldObject) {
    const FISH_BUFF: u32 = 5357;
    // Keep the skills timer unbound so tests can drive the real controller ticks.
    let mut data = GameData::empty();
    data.systems.skills.add_buff(BuffTemplate {
        id: FISH_BUFF,
        immune_except_creator: true,
        ..Default::default()
    });
    data.systems
        .skills
        .tagged_buffs
        .insert(tags::FISH, vec![FISH_BUFF]);
    data.systems.npcs.npc.templates.insert(
        12835,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: 12835,
            buffs: vec![FISH_BUFF],
            ..Default::default()
        },
    );
    let mut template = WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE);
    template.ocean_level = 100.0;
    let server = test_server_with_world(data, template);
    let client = TestClient::new(&server, 1, 1);
    let angler = spawn_player(
        &server,
        &client,
        test_character(1, "Angler", 100.0, 100.0),
        0x400,
    );
    let world = server.world.world(0).unwrap();
    let mut n = Npc::new(12835);
    n.unit.owner_id = angler.character_id;
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit
        .transform
        .set_position(100.0, 125.0, 100.0, 0.0, 0.0, 2.0);
    n.spawner_z = Some(100.0);
    let fish = Entity::new(0x401, n);
    world.spawn(&fish, &server.view_ctx());
    (
        server,
        client,
        WorldObject::from(fish),
        WorldObject::from(angler),
    )
}

#[tokio::test]
async fn owned_fish_swims_in_the_anglers_frame_and_reels_toward_the_surface() {
    let (server, _, obj, angler) = hooked_fish();
    let rod = Target::Obj(angler);
    let end = |id, kind, target: &Target, offset| {
        let template = SkillControllerTemplate {
            id,
            kind_id: kind,
            value: [0, 0, 1500, offset, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ..Default::default()
        };
        let controller = create(&server, &template, &obj, target, 0.0, 0.0, 0.0, 0.0)
            .expect("authored fish controller");
        let Behavior::FishSwim(swim) = controller.behavior() else {
            panic!("expected a swim");
        };
        swim.end_position
    };
    let cue_target = pos_unit_from(&server, &Target::Obj(obj.clone()));
    let mut cue_target = cue_target;
    cue_target.pos.x -= 15.0;
    let cue_target = Target::Pos(cue_target);
    let origin = Vec3::new(100.0, 100.0, 100.0);
    for (right, left) in [(7008, 7009), (7127, 7128)] {
        let right = end(right, 2, &cue_target, 0);
        let left = end(left, 2, &cue_target, 0);
        assert!(right.x > 100.0 && left.x < 100.0);
        assert_eq!(right.z, 98.0);
        assert_eq!(left.z, 98.0);
        assert!(calculate_distance(origin, right, false) <= 30.001);
    }
    let first_dive = end(7010, 2, &cue_target, 0);
    assert_eq!(first_dive.z, 92.0);
    obj.with_transform(|t| t.z = first_dive.z);
    assert_eq!(end(7010, 2, &cue_target, 0).z, first_dive.z);
    let slack = end(7129, 2, &Target::Obj(obj.clone()), -7000);
    assert!(calculate_distance(origin, slack, false) > 25.0);
    let reeled = end(7064, 4, &rod, -8000);
    assert_eq!(reeled.y, 117.0);
    assert!((reeled.z - 99.8).abs() < 0.001);
}

#[tokio::test]
async fn outward_fish_jump_near_the_line_limit_runs_to_the_surface() {
    let (server, _, fish, _) = hooked_fish();
    fish.with_transform(|t| t.set_position(100.0, 129.8, 92.0, 0.0, 0.0, 0.0));
    // 1.2 Leap controller used by the Jump cue.
    let template = SkillControllerTemplate {
        id: 7040,
        kind_id: 2,
        value: [80, 4, 1000, 4000, 0, 0, 0, 1, 1, 1, 0, 1, 0, 0, 0],
        ..Default::default()
    };
    let controller = create(
        &server,
        &template,
        &fish,
        &Target::Obj(fish.clone()),
        0.0,
        0.0,
        0.0,
        0.0,
    )
    .unwrap();
    assert!(matches!(controller.behavior(), Behavior::BallisticLeap(_)));
    activate(&server, &controller);
    controller.tick(&server, Duration::from_millis(100));
    let first = fish.transform();
    assert!(first.y < 129.8 && first.z > 92.0);
    assert_eq!(controller.state(), ScState::Running);

    // Measured ticks need not divide the arc duration evenly.
    for _ in 0..8 {
        if controller.state() == ScState::Ended {
            break;
        }
        controller.tick(&server, Duration::from_millis(125));
        assert!(fish.transform().y <= 130.0);
    }
    assert_eq!(controller.state(), ScState::Ended);
    assert!(active(&fish).is_none());
    let end = fish.transform();
    assert!((end.y - 125.8).abs() < 0.001);
    assert!((end.z - 99.8).abs() < 0.001);
}

#[tokio::test]
async fn reel_swim_sends_motion_and_stops_at_its_endpoint() {
    let (server, mut client, fish, angler) = hooked_fish();
    fish.with_transform(|t| t.z = 92.0);
    let template = SkillControllerTemplate {
        id: 7064,
        kind_id: 4,
        value: [600, 0, 2000, -8000, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 0],
        ..Default::default()
    };
    let controller = create(
        &server,
        &template,
        &fish,
        &Target::Obj(angler),
        0.0,
        0.0,
        0.0,
        0.0,
    )
    .unwrap();
    assert!(matches!(controller.behavior(), Behavior::FishSwim(_)));
    client.drain();
    activate(&server, &controller);
    assert!(active(&fish).is_some());
    // Run both an intermediate step and one large enough to reach the endpoint.
    for delta in [250, 1800] {
        controller.tick(&server, Duration::from_millis(delta));
    }

    assert_eq!(controller.state(), ScState::Ended);
    assert!(active(&fish).is_none());
    let end = fish.transform();
    assert!((end.y - 117.0).abs() < 0.001);
    assert!((end.z - 99.8).abs() < 0.001);
    let moves: Vec<_> = client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCOneUnitMovementPacket)
        .map(|(_, body)| {
            let mut stream = PacketStream::from_bytes(body);
            assert_eq!(stream.read_bc().unwrap(), fish.obj_id());
            let kind = stream.read_u8().unwrap();
            let movement = MoveType::read(kind, &mut stream).unwrap();
            let MoveKind::Unit(unit) = movement.kind else {
                panic!("expected unit movement");
            };
            (movement.flags, unit)
        })
        .collect();
    assert_eq!(moves.len(), 3);
    for (flags, unit) in &moves[..2] {
        assert!(flags.contains(MoveTypeFlags::MOVING | MoveTypeFlags::IN_COMBAT));
        assert_eq!(unit.stance, GameStanceType::CO_SWIM);
        assert!(unit.vel_y < 0 && unit.vel_z > 0);
    }
    assert!((moves[0].1.y - 124.0).abs() < 0.01);
    assert!((moves[0].1.z - 92.975).abs() < 0.01);
    for (flags, unit) in &moves[1..] {
        assert!((unit.y - end.y).abs() < 0.01);
        assert!((unit.z - end.z).abs() < 0.01);
        assert_eq!(unit.stance, GameStanceType::CO_SWIM);
        if flags.contains(MoveTypeFlags::STOPPING) {
            assert_eq!((unit.vel_x, unit.vel_y, unit.vel_z), (0, 0, 0));
        }
    }
    assert!(moves[2].0.contains(MoveTypeFlags::STOPPING));
}
