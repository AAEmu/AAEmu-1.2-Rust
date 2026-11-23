use super::super::data::BuffTemplate;
use super::*;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::test_support::*;
use crate::world::{Entity, NpcRef, PlayerRef};

fn sc_template(kind: SkillControllerKind, values: &[(usize, i32)]) -> SkillControllerTemplate {
    let mut t = SkillControllerTemplate {
        id: 77,
        kind_id: kind as u32,
        ..SkillControllerTemplate::default()
    };
    for &(i, v) in values {
        t.value[i] = v;
    }
    t
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

#[test]
fn leap_end_position_pulls_in_front_of_target() {
    // offset -2 m: lands 2 m short of the target, on the owner -> target line.
    let t = sc_template(SkillControllerKind::Leap, &[(2, 1000), (3, -2000)]);
    let l = LinearMove::leap(&t, Vec3::new(0.0, 0.0, 0.0), Vec3::new(10.0, 0.0, 5.0));
    assert!(close(l.end_position.x, 8.0) && close(l.end_position.y, 0.0));
    assert_eq!(l.end_position.z, 5.0);
    let expected = (64.0f32 + 25.0).sqrt();
    assert!(
        close(l.calculated_speed, expected),
        "{}",
        l.calculated_speed
    );
    // no duration -> 0.5 s
    let t = sc_template(SkillControllerKind::Leap, &[(3, -2000)]);
    let l = LinearMove::leap(&t, Vec3::default(), Vec3::new(10.0, 0.0, 0.0));
    assert!(close(l.calculated_speed, 16.0));
}

#[test]
fn leap_direction_constraints() {
    let owner = Vec3::new(0.0, 0.0, 0.0);
    let target = Vec3::new(1.0, 0.0, 0.0);
    // ForwardOnly landing farther from the target than the owner -> stays put.
    let t = sc_template(SkillControllerKind::Leap, &[(3, 5000), (6, 1)]);
    let l = LinearMove::leap(&t, owner, target);
    assert_eq!(l.direction, MoveDirection::ForwardOnly);
    assert_eq!(l.end_position, owner);
    assert_eq!(l.calculated_speed, 0.0);
    // Both: unconstrained
    let t = sc_template(SkillControllerKind::Leap, &[(3, 5000)]);
    assert!(close(
        LinearMove::leap(&t, owner, target).end_position.x,
        6.0
    ));
    // BackwardOnly landing closer -> stays put; farther -> allowed.
    let end = Vec3::new(0.5, 0.0, 0.0);
    let far = Vec3::new(-3.0, 0.0, 0.0);
    assert_eq!(
        apply_direction_constraint(MoveDirection::BackwardOnly, owner, end, target),
        owner
    );
    assert_eq!(
        apply_direction_constraint(MoveDirection::BackwardOnly, owner, far, target),
        far
    );
    assert_eq!(
        apply_direction_constraint(MoveDirection::ForwardOnly, owner, end, target),
        end
    );
}

#[test]
fn dash_end_position_and_backward_flip() {
    let owner = Vec3::new(0.0, 0.0, 3.0);
    let target = Vec3::new(10.0, 0.0, 0.0);
    let t = sc_template(SkillControllerKind::Dash, &[(2, 500), (3, 4000)]);
    let d = LinearMove::dash(&t, owner, target);
    assert!(close(d.end_position.x, 4.0) && close(d.end_position.y, 0.0));
    assert_eq!(d.end_position.z, 3.0, "dash keeps the owner's height");
    assert!(close(d.calculated_speed, 8.0));
    let t = sc_template(SkillControllerKind::Dash, &[(2, 500), (3, 4000), (6, 2)]);
    let d = LinearMove::dash(&t, owner, target);
    assert!(close(d.end_position.x, -4.0) && d.end_position.y.abs() < 1e-3);
}

#[test]
fn floating_pull_and_lift_modes() {
    let f = FloatingMove::new(
        None,
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.0, 10.0, 4.0),
        0.0,
        0.0,
        0.0,
        0.0,
    );
    assert!(!f.is_lift_mode);
    assert_eq!(f.speed, 5.0);
    let pull = (100.0f32 + 16.0).sqrt() - 1.5;
    assert!(close(f.pull_distance, pull));
    assert!(close(f.end_position.y, pull) && f.end_position.x.abs() < 1e-4);
    assert_eq!(f.end_position.z, 0.0);
    // template speed (Value[0] mm/s) and psychokinesis speed override
    let t = sc_template(SkillControllerKind::Floating, &[(0, 8000)]);
    let f = FloatingMove::new(
        Some(&t),
        Vec3::default(),
        Vec3::new(5.0, 0.0, 0.0),
        0.0,
        0.0,
        0.0,
        0.0,
    );
    assert_eq!(f.speed, 8.0);
    let f = FloatingMove::new(
        Some(&t),
        Vec3::default(),
        Vec3::new(5.0, 0.0, 0.0),
        12.0,
        0.0,
        0.0,
        0.0,
    );
    assert_eq!(f.speed, 12.0);
    // too close: no pull
    let f = FloatingMove::new(
        None,
        Vec3::default(),
        Vec3::new(1.0, 0.0, 0.0),
        0.0,
        0.0,
        0.0,
        0.0,
    );
    assert_eq!(f.pull_distance, 0.0);
    assert_eq!(f.end_position, Vec3::default());
    // lift
    let f = FloatingMove::new(
        None,
        Vec3::default(),
        Vec3::new(5.0, 0.0, 0.0),
        0.0,
        5.0,
        0.0,
        2.0,
    );
    assert!(f.is_lift_mode);
    assert_eq!((f.lift_speed, f.speed, f.lift_duration), (3.0, 3.0, 2.0));
}

#[test]
fn heading_velocity_and_rotation() {
    // facing +x: velocity (4000, 0), yaw -90 deg
    let (yaw, vx, vy) = heading(Vec3::default(), Vec3::new(5.0, 0.0, 0.0));
    assert_eq!((vx, vy), (4000, 0));
    assert!(close(yaw, -std::f32::consts::FRAC_PI_2));
    let (_, _, rz) = roll_pitch_yaw_sbytes_movement(0.0, 0.0, yaw);
    assert_eq!(rz, convert_radian_to_direction(f64::from(yaw - TWO_PI)));
    // facing +y: velocity (0, 4000), yaw 0
    let (yaw, vx, vy) = heading(Vec3::default(), Vec3::new(0.0, 5.0, 0.0));
    assert_eq!((vx, vy), (0, 4000));
    assert!(yaw.abs() < 1e-6);
    assert_eq!(roll_pitch_yaw_sbytes_movement(0.0, 0.0, 0.0), (0, 0, 0));
}

#[test]
fn final_step_lands_on_end_and_keeps_heading_at_large_coordinates() {
    // x ~ 20000: the f32 ulp is ~0.002 m, bigger than the old fixed 1e-3 m tolerance.
    let cur = Vec3::new(20003.0, 100.0, 5.0);
    let end = Vec3::new(20000.0, 100.0, 5.0);
    let (face_end, vx_end, _) = heading(cur, end);
    // The final step lands exactly on the end point.
    assert_eq!(next_step_position(3.0, 3.0, cur, end), end);
    assert_eq!(next_step_position(4.0, 3.0, cur, end), end);
    // One ulp past the end (what the ratio interpolation can produce) would face backwards
    // with `heading(new, end)`; the step heading stays pointing at the end.
    let over = Vec3::new(f32::from_bits(end.x.to_bits() - 1), 100.0, 5.0);
    assert!(over.x < end.x);
    let flipped = heading(over, end);
    assert_ne!(flipped.1, vx_end, "the old check flips here");
    assert!(calculate_distance(over, end, false) > 1e-3 || over.x == end.x);
    let (yaw, vx, vy) = step_heading(cur, over, end);
    assert!(close(yaw, face_end) && vx == vx_end && vy == 0);
    let under = Vec3::new(f32::from_bits(end.x.to_bits() + 1), 100.0, 5.0);
    let (yaw, vx, _) = step_heading(cur, under, end);
    assert!(close(yaw, face_end) && vx == vx_end);
    // A genuinely distant step is unaffected.
    let mid = Vec3::new(20001.5, 100.0, 5.0);
    assert_eq!(step_heading(cur, mid, end), heading(mid, end));
    // Non-final steps interpolate as before.
    assert_eq!(
        next_step_position(1.5, 3.0, cur, end),
        add_distance_to_front_between(1.5, 3.0, cur, end)
    );
}

#[test]
fn kinds_without_controller() {
    assert_eq!(
        SkillControllerKind::from_u32(4),
        Some(SkillControllerKind::Dash)
    );
    assert_eq!(SkillControllerKind::from_u32(9), None);
    assert_eq!(MoveDirection::from_i32(7), MoveDirection::Both);
}

const PLAYER_OBJ: u32 = 0x100;
const NPC_OBJ: u32 = 0x200;

pub(super) fn setup() -> (Arc<GameServer>, TestClient, PlayerRef, NpcRef) {
    let server = Arc::new(test_server_with(GameData::empty()));
    super::super::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let c = test_character(1, "Viewer", 100.0, 100.0);
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let mut n = Npc::new(1234);
    n.unit.hp = 100;
    n.unit.max_hp = 100;
    n.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let npc = Entity::new(NPC_OBJ, n);
    server
        .world
        .world(0)
        .expect("world")
        .spawn(&npc, &server.view_ctx());
    (server, client, player, npc)
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn dash_moves_npc_and_broadcasts_movement() {
    let (server, mut client, player, npc) = setup();
    client.drain();
    let owner = WorldObject::from(&npc);
    // 3 m dash in 500 ms towards the player (at x = 100).
    let t = sc_template(SkillControllerKind::Dash, &[(2, 500), (3, 3000)]);
    let sc = create(
        &server,
        &t,
        &owner,
        &Target::Obj(WorldObject::from(&player)),
        0.0,
        0.0,
        0.0,
        0.0,
    )
    .expect("dash controller");
    let Behavior::Dash(d) = sc.behavior() else {
        panic!("not a dash");
    };
    assert!(close(d.end_position.x, 102.0) && close(d.end_position.y, 100.0));
    activate(&server, &sc);
    assert_eq!(sc.state(), ScState::Running);
    assert_eq!(active(&owner), Some(sc.clone()));
    wait_until(|| sc.state() == ScState::Ended).await;
    assert_eq!(sc.state(), ScState::Ended);
    let tr = npc.data().unit.transform;
    assert!(tr.x < 102.6 && tr.x >= 101.99, "x = {}", tr.x);
    assert!(close(tr.y, 100.0));
    // facing -x: yaw = 180 - 90 deg
    assert!(close(tr.rot_z, std::f32::consts::FRAC_PI_2));
    assert_eq!(active(&owner), None, "the controller cleared itself");
    let frames = client.drain();
    let moves: Vec<&Vec<u8>> = frames
        .iter()
        .filter(|(op, _)| *op == sc::SCOneUnitMovementPacket)
        .map(|(_, b)| b)
        .collect();
    assert!(moves.len() >= 2, "{} movement packets", moves.len());
    // body: bc obj id, type 1 (unit), time, flags Moving
    let body = moves[0];
    assert_eq!(&body[..3], &[0x00, 0x02, 0x00]);
    assert_eq!(body[3], 1);
    assert_eq!(body[8], MoveTypeFlags::MOVING.0);
}

/// S26: an NPC of the Howling Abyss (Normal zone 265, Greater 280) leaping at a victim 8 m
/// below keeps its level when no floor supports a step within the 2.5 m walker drop. With
/// no collision service here the leap's end point keeps the victim's height (z 100) and no
/// step finds a floor. Elsewhere the steps descend to the victim's height.
#[tokio::test]
async fn howling_leap_without_a_floor_keeps_its_level() {
    use crate::data::world_templates::WorldTemplate;

    for (zone, keeps_level) in [(265, true), (280, true), (100, false)] {
        let server = Arc::new(test_server_with_world(
            GameData::empty(),
            WorldTemplate::synthetic(0, "main_world", 2, 2, zone),
        ));
        super::super::bind(&server);
        let client = TestClient::new(&server, 1, 1);
        let mut c = test_character(1, "Victim", 510.0, 500.0);
        c.unit.transform.set_xyz(510.0, 500.0, 100.0);
        c.unit.transform.zone_id = zone;
        let player = spawn_player(&server, &client, c, PLAYER_OBJ);
        let mut n = Npc::new(1234);
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.transform.set_xyz(502.0, 500.0, 108.0);
        n.unit.transform.zone_id = zone;
        let npc = Entity::new(NPC_OBJ, n);
        server
            .world
            .world(0)
            .expect("world")
            .spawn(&npc, &server.view_ctx());
        let owner = WorldObject::from(&npc);
        let t = sc_template(SkillControllerKind::Leap, &[(2, 500), (3, -1000)]);
        let sc = create(
            &server,
            &t,
            &owner,
            &Target::Obj(WorldObject::from(&player)),
            0.0,
            0.0,
            0.0,
            0.0,
        )
        .expect("leap controller");
        let Behavior::Leap(l) = sc.behavior() else {
            panic!("not a linear leap");
        };
        assert!((l.end_position.z - 100.0).abs() < 0.01, "zone {zone}");
        activate(&server, &sc);
        // A leap that keeps its level never closes the 3D distance to its end point, so
        // wait for the horizontal arrival (end x 509) and end it.
        wait_until(|| sc.state() == ScState::Ended || npc.data().unit.transform.x > 508.5).await;
        let tr = npc.data().unit.transform;
        sc.end(&server, false);
        assert!(
            tr.x > 508.5,
            "zone {zone}: the leap moves the NPC: {}",
            tr.x
        );
        if keeps_level {
            assert!(
                (tr.z - 108.0).abs() < 0.01,
                "zone {zone}: the leap keeps its level: {}",
                tr.z
            );
        } else {
            assert!(
                tr.z < 101.0,
                "zone {zone}: the leap descends to the victim: {}",
                tr.z
            );
        }
    }
}

#[tokio::test]
async fn replacing_ends_the_previous_controller() {
    let (server, _client, player, npc) = setup();
    let owner = WorldObject::from(&npc);
    let target = Target::Obj(WorldObject::from(&player));
    let t = sc_template(SkillControllerKind::Dash, &[(2, 60_000), (3, 3000)]);
    let first = create(&server, &t, &owner, &target, 0.0, 0.0, 0.0, 0.0).expect("sc");
    activate(&server, &first);
    let second = create(&server, &t, &owner, &target, 0.0, 0.0, 0.0, 0.0).expect("sc");
    second.set_source_buff_id(5);
    activate(&server, &second);
    assert_eq!(first.state(), ScState::Ended);
    assert_eq!(second.state(), ScState::Running);
    assert_eq!(active(&owner), Some(second.clone()));
    second.end(&server, false);
    assert_eq!(active(&owner), None);
}

#[tokio::test(start_paused = true)]
async fn lift_then_fall_back_to_start_height() {
    let (server, _client, player, npc) = setup();
    let owner = WorldObject::from(&npc);
    let sc = new_floating(
        &server,
        None,
        &owner,
        &Target::Obj(WorldObject::from(&player)),
        0.0,
        1.0,
        20.0,
        0.0,
    );
    activate(&server, &sc);
    wait_until(|| npc.data().unit.transform.z >= 101.0).await;
    assert!(close(npc.data().unit.transform.z, 101.0));
    // dispel path: End() switches to the fall phase instead of ending.
    sc.end(&server, false);
    assert_eq!(sc.state(), ScState::Running);
    assert!(matches!(sc.behavior(), Behavior::Floating(f) if f.is_falling));
    assert_eq!(active(&owner), Some(sc.clone()));
    // Buff expiry and the controller's own lift deadline can both call End().
    sc.end(&server, false);
    assert_eq!(active(&owner), Some(sc.clone()));
    wait_until(|| sc.state() == ScState::Ended).await;
    assert_eq!(sc.state(), ScState::Ended);
    assert_eq!(npc.data().unit.transform.z, 100.0);
    assert_eq!(active(&owner), None);
}

#[tokio::test]
async fn psychokinesis_buff_starts_and_dispel_ends_the_pull() {
    const BUFF: u32 = 60_001;
    let mut data = GameData::empty();
    data.systems.skills.add_buff(BuffTemplate {
        id: BUFF,
        kind: 2,
        duration: 60_000,
        stack_rule: 1,
        psychokinesis: true,
        psychokinesis_speed: 2.0,
        ..BuffTemplate::default()
    });
    let server = Arc::new(test_server_with(data));
    super::super::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Puller", 100.0, 100.0),
        PLAYER_OBJ,
    );
    let mut n = Npc::new(1);
    n.unit.hp = 100;
    n.unit.max_hp = 100;
    n.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let npc = Entity::new(NPC_OBJ, n);
    server
        .world
        .world(0)
        .expect("world")
        .spawn(&npc, &server.view_ctx());
    let owner = WorldObject::from(&npc);
    buffs::add_buff_by_id(&server, &owner, BUFF, &WorldObject::from(&player), 0);
    let sc = active(&owner).expect("pull controller started by the buff");
    assert_eq!(sc.kind(), SkillControllerKind::Floating);
    assert_eq!(sc.source_buff_id(), BUFF);
    assert_eq!(sc.state(), ScState::Running);
    let Behavior::Floating(f) = sc.behavior() else {
        unreachable!()
    };
    assert!(close(f.end_position.x, 101.5) && close(f.speed, 2.0));
    wait_until(|| npc.data().unit.transform.x < 104.9).await;
    let index = npc.data().unit.buffs.effects[0].index;
    buffs::exit(&server, &owner, index, false);
    assert_eq!(sc.state(), ScState::Ended);
    assert_eq!(active(&owner), None);
}
