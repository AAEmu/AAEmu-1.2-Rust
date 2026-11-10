//! NPC group rules on the AI side: a follower walks to the move leader's formation point and
//! waits while the leader stands still, never roams on its own, falls back to its own idle
//! behaviour when the leader dies, and `aggro_rule_id != 0` groups share aggro with every living
//! member of the same spawn.

use std::sync::Arc;
use std::time::Duration;

use crate::data::GameData;
use crate::models::movement::GameStanceType;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::npcs::npc::group;
use crate::systems::npcs::npc::group_data::{NpcGroup, NpcGroupMember};
use crate::systems::skills::controllers::Vec3;
use crate::systems::skills::data::BuffTemplate;
use crate::systems::skills::state::AggroKind;
use crate::test_support::*;
use crate::world::{Entity, NpcRef, PlayerRef};

use super::data::{ActorModel, GameStance};
use super::framework::{AiKind, BehaviorKind as B};
use super::manager::{post, tick_all};

const PLAYER_OBJ: u32 = 0x100;
const LEADER_OBJ: u32 = 0x200;
const FOLLOWER_OBJ: u32 = 0x201;
const FOLLOWER2_OBJ: u32 = 0x202;
const NPC_TEMPLATE: u32 = 7000;
const MODEL: u32 = 900;

fn stance(
    stance_id: GameStanceType,
    walk: f32,
    run: f32,
    sprint: f32,
) -> (GameStanceType, GameStance) {
    (
        stance_id,
        GameStance {
            id: u32::from(stance_id.0) + 1,
            stance_id,
            actor_model_id: 1,
            ai_move_speed_run: run,
            ai_move_speed_slow: walk,
            ai_move_speed_sprint: sprint,
            ai_move_speed_walk: walk,
            height_collider: 0.0,
            height_pivot: 0.0,
            normal_speed: run,
            max_speed: 20.0,
            size: [0.0; 3],
            model_offset: [0.0; 3],
            view_offset: [0.0; 3],
            use_capsule: true,
        },
    )
}

fn data() -> GameData {
    let mut data = GameData::empty();
    data.systems.npcs.npc.templates.insert(
        NPC_TEMPLATE,
        NpcTemplate {
            id: NPC_TEMPLATE,
            model_id: MODEL,
            level: 10,
            scale: 1.0,
            // Passive: the group rules must work without the NPCs noticing the player by
            // themselves.
            aggression: false,
            sight_range_scale: 0.0,
            ai_file_id: 13, // Roaming
            accept_aggro_link: true,
            aggro_link_help_dist: 30.0,
            ..NpcTemplate::default()
        },
    );
    data.systems.skills.model_radii.insert(MODEL, 0.5);
    data.systems.skills.add_buff(BuffTemplate {
        id: super::npc::BUFF_NPC_RETURN,
        kind: 1,
        duration: 0,
        stack_rule: 1,
        ..BuffTemplate::default()
    });
    data.systems.ai.ai.actor_models.insert(
        MODEL,
        Arc::new(ActorModel {
            id: 1,
            radius: 0.5,
            height: 2.0,
            movement_id: 0,
            underwater_creature: false,
            stances: vec![
                stance(GameStanceType::COMBAT, 10.0, 10.0, 10.0),
                stance(GameStanceType::RELAXED, 10.0, 10.0, 10.0),
            ],
        }),
    );
    data
}

fn server() -> Arc<GameServer> {
    let server = Arc::new(test_server_with(data()));
    server.set_self();
    crate::systems::skills::bind(&server);
    super::hooks::install(&server);
    server
}

/// Creates an NPC with an AI and spawns it like `NpcSpawnerNpc.Spawn` (home position, Spawning).
fn spawn_npc(server: &Arc<GameServer>, obj_id: u32, x: f32, y: f32, kind: AiKind) -> NpcRef {
    let mut n = Npc::new(NPC_TEMPLATE);
    n.unit.name = "Biyam".into();
    n.unit.level = 10;
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.mp = 100;
    n.unit.max_mp = 100;
    n.unit.model_id = MODEL;
    n.unit.scale = 1.0;
    n.unit.transform.set_xyz(x, y, 100.0);
    n.unit.transform.zone_id = TEST_ZONE;
    let npc = Entity::new(obj_id, n);
    server.systems.ai.ais.add_ai(&npc, kind);
    let world = server.world.world(0).expect("world");
    world.spawn(&npc, &server.view_ctx());
    super::hooks::spawn_positioned(server, &npc);
    npc
}

fn group_row(rule: i32) -> NpcGroup {
    NpcGroup {
        id: 44,
        name: "Biyam herd".into(),
        aggro_rule_id: rule,
        enable_respawn: false,
    }
}

fn member_row(id: u32, is_move_leader: bool, x: f32, y: f32, tension: f32) -> NpcGroupMember {
    NpcGroupMember {
        id,
        npc_group_id: 44,
        npc_id: NPC_TEMPLATE,
        is_leader: is_move_leader,
        is_move_leader,
        formation_offset_x: x,
        formation_offset_y: y,
        formation_offset_z: 0.0,
        formation_tension: tension,
    }
}

struct Fixture {
    server: Arc<GameServer>,
    _client: TestClient,
    _player: PlayerRef,
    leader: NpcRef,
    follower: NpcRef,
    follower2: NpcRef,
}

/// Three group members (one move leader + two followers), the follower 5.8 m from its formation
/// point, a player for region activity, and the IDs the tests need.
fn fixture(rule: i32, follower_kind: AiKind) -> Fixture {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let mut character = test_character(1, "Observer", 100.0, 100.0);
    character.unit.hp = 1000;
    character.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, character, PLAYER_OBJ);

    let leader = spawn_npc(&server, LEADER_OBJ, 100.0, 100.0, AiKind::HoldPosition);
    let follower = spawn_npc(&server, FOLLOWER_OBJ, 95.0, 100.0, follower_kind);
    let follower2 = spawn_npc(&server, FOLLOWER2_OBJ, 95.0, 103.0, AiKind::HoldPosition);
    let rows = vec![
        (member_row(1, true, 0.0, 0.0, 1.0), leader.clone()),
        (member_row(2, false, 2.0, -3.0, 1.0), follower.clone()),
        (member_row(3, false, 3.0, 3.0, 1.0), follower2.clone()),
    ];
    group::attach_spawned_group(&rows, &group_row(rule));

    // Settle Spawning -> RunCommandSet -> Idle.
    for _ in 0..25 {
        tick_all(&server, 100);
    }
    Fixture {
        server,
        _client: client,
        _player: player,
        leader,
        follower,
        follower2,
    }
}

fn npc_pos(npc: &NpcRef) -> Vec3 {
    let t = npc.data().unit.transform;
    Vec3::new(t.x, t.y, t.z)
}

fn xy_dist(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

fn behavior(server: &GameServer, id: u32) -> Option<B> {
    server
        .systems
        .ai
        .ais
        .inspect(id, |a| a.current_behavior())
        .flatten()
}

/// A moving leader: the follower walks to `leader position + formation_offset_*` and stops
/// within `formation_tension` of it.
#[tokio::test]
async fn follower_moves_to_the_leader_plus_offset_and_stops_within_tension() {
    let f = fixture(1, AiKind::HoldPosition);
    let start = npc_pos(&f.follower);
    let target = Vec3::new(102.0, 97.0, 100.0);
    let start_dist = xy_dist(start, target);
    assert!(
        start_dist > 5.0,
        "fixture: the follower starts away, {start_dist}"
    );

    for _ in 0..60 {
        // The leader keeps reporting a movement step (`MoveTowards`).
        f.leader.data().last_move_ms = crate::world::now_ms();
        tick_all(&f.server, 100);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let pos = npc_pos(&f.follower);
    assert!(
        xy_dist(pos, target) <= 1.05,
        "the follower stopped within the 1 m tension: {pos:?} from {target:?}"
    );
    assert!(
        xy_dist(start, target) - xy_dist(pos, target) > 4.0,
        "the follower walked towards the formation point"
    );

    // It does not keep walking once it is inside the tension.
    let settled = npc_pos(&f.follower);
    for _ in 0..10 {
        f.leader.data().last_move_ms = crate::world::now_ms();
        tick_all(&f.server, 100);
    }
    assert!(
        xy_dist(npc_pos(&f.follower), settled) < 0.01,
        "no movement while within the tension"
    );
}

/// A follower with a roaming default behavior stays at its leader while the leader stands still
/// (1.2 `idle.UpdateIdle` / `roaming.UpdateRoaming` send a follower to the formation pipe instead
/// of roaming).
#[tokio::test]
async fn follower_does_not_roam_on_its_own() {
    let f = fixture(1, AiKind::Roaming);
    assert_eq!(
        behavior(&f.server, FOLLOWER_OBJ),
        Some(B::Roaming),
        "a follower keeps its own idle behavior (here Roaming) so combat transitions stay"
    );
    // It did not leave its formation point while the leader stood still, not even during the
    // spawn settlement (an ordinary Roaming NPC would have started roaming there).
    let start = npc_pos(&f.follower);
    assert!(
        xy_dist(start, Vec3::new(95.0, 100.0, 100.0)) < 0.05,
        "the follower waited at its formation point during the spawn settlement: {start:?}"
    );
    for _ in 0..60 {
        f.leader.data().last_move_ms = 0;
        tick_all(&f.server, 100);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(behavior(&f.server, FOLLOWER_OBJ), Some(B::Roaming));
    assert!(
        xy_dist(npc_pos(&f.follower), start) < 0.05,
        "the follower must not roam while the leader stands still"
    );
}

/// When the move leader dies the follower falls back to its own idle behavior (here it starts
/// roaming like any ordinary NPC).
#[tokio::test]
async fn leader_death_returns_followers_to_their_own_idle_behaviour() {
    let f = fixture(1, AiKind::Roaming);
    assert_eq!(behavior(&f.server, FOLLOWER_OBJ), Some(B::Roaming));
    let start = npc_pos(&f.follower);
    for _ in 0..20 {
        tick_all(&f.server, 100);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        xy_dist(npc_pos(&f.follower), start) < 0.05,
        "while the leader lives the follower does not roam"
    );

    f.leader.data().unit.hp = 0;
    for _ in 0..40 {
        tick_all(&f.server, 100);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        behavior(&f.server, FOLLOWER_OBJ),
        Some(B::Roaming),
        "without a live leader the follower runs its own default behavior"
    );
    assert!(
        xy_dist(npc_pos(&f.follower), start) > 0.2,
        "the roaming follower moves again"
    );
}

/// After combat a follower returns to its formation point, not to its own spawn (the 1.2
/// behaviors re-anchor a group member's idle position on the leader).
#[tokio::test]
async fn a_returning_follower_goes_to_its_formation_point() {
    let f = fixture(1, AiKind::HoldPosition);
    // Both its own spawn (95, 100) and the walked-away position differ from the formation point
    // (102, 97).
    f.follower.data().unit.transform.set_xyz(90.0, 100.0, 100.0);
    post(&f.server, FOLLOWER_OBJ, |c| c.go_to_return());
    for _ in 0..80 {
        tick_all(&f.server, 100);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let target = Vec3::new(102.0, 97.0, 100.0);
    let pos = npc_pos(&f.follower);
    assert!(
        xy_dist(pos, target) <= 1.5,
        "the follower returned to its formation point: {pos:?}"
    );
    let spawn = Vec3::new(95.0, 100.0, 100.0);
    assert!(
        xy_dist(pos, spawn) > 3.0,
        "the follower did not return to its own spawn"
    );
}

/// Seeds combat on `member` through the real `AddUnitAggro` + `OnAggroTargetChanged` path, then
/// runs the AI so `UpdateTarget` / `UpdateAggroHelp` share the abuser with the group.
async fn pull_member(f: &Fixture, member: &NpcRef) {
    let player = f.server.world.object(PLAYER_OBJ).expect("player");
    crate::systems::skills::combat::add_unit_aggro(
        &f.server,
        &crate::world::WorldObject::from(member),
        AggroKind::Damage,
        &player,
        10,
    );
    post(&f.server, member.obj_id, |c| c.on_aggro_target_changed());
    for _ in 0..5 {
        tick_all(&f.server, 100);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn has_aggro_on_player(npc: &NpcRef) -> bool {
    npc.data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&PLAYER_OBJ)
}

/// `aggro_rule_id != 0`: pulling one member puts the attacker on every living member's table.
#[tokio::test]
async fn a_non_zero_aggro_rule_shares_aggro_with_the_whole_spawned_group() {
    let f = fixture(1, AiKind::HoldPosition);
    pull_member(&f, &f.follower).await;
    assert!(
        has_aggro_on_player(&f.follower),
        "the pulled member keeps its own aggro"
    );
    assert!(
        has_aggro_on_player(&f.leader),
        "the move leader joins the fight (rule 1)"
    );
    assert!(
        has_aggro_on_player(&f.follower2),
        "every living group member joins the fight (rule 1)"
    );
}

/// `aggro_rule_id == 0`: no sharing.
#[tokio::test]
async fn a_zero_aggro_rule_does_not_share_aggro() {
    let f = fixture(0, AiKind::HoldPosition);
    pull_member(&f, &f.follower).await;
    assert!(has_aggro_on_player(&f.follower));
    assert!(
        !has_aggro_on_player(&f.leader) && !has_aggro_on_player(&f.follower2),
        "rule 0 groups fight alone"
    );
}
