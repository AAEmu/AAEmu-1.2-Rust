//! NPC group rules on the AI side: a follower walks to the move leader's formation point and
//! waits while the leader stands still, never roams on its own, falls back to its own idle
//! behaviour when the leader dies, rule 2 shares the aggro table with every living member of the
//! same spawn, and rule 1 members answer the group's link-help call instead.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aaemu_physics::geometry::primitives::BoxPrim;
use aaemu_physics::geometry::shapes::stream::{save_phys_geometry, PhysGeomHeader};
use aaemu_physics::geometry::shapes::BoxGeom;
use aaemu_physics::math::{Quat, Vec3 as PhysicsVec3};

use crate::data::client_files::ClientFileManager;
use crate::data::GameData;
use crate::models::movement::GameStanceType;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::ai::collision::objects::{apply_doodads, collision_flags, DoodadInfo};
use crate::systems::npcs::heightmap::{
    HeightmapConfig, HeightmapWorlds, WorldHeightmap, CELL_HMAP_RESOLUTION,
};
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::npcs::npc::group;
use crate::systems::npcs::npc::group_data::{NpcGroup, NpcGroupMember};
use crate::systems::skills::controllers::Vec3;
use crate::systems::skills::data::BuffTemplate;
use crate::systems::skills::state::AggroKind;
use crate::test_support::*;
use crate::world::{Entity, NpcRef, PlayerRef, WorldObject};

use super::data::{ActorModel, GameStance};
use super::framework::{AiCtx, AiKind, BehaviorKind as B, NpcAi};
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
            attack_start_range_scale: 1.0,
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

/// [`server`] whose group template declines the template-driven help path (not aggressive, and a
/// faction rule whose sight check is off), so only the rule-1 group link can make a mate join.
/// `accept_aggro_link` is the template's own flag: the group link replaces the relation test but
/// still honours it.
fn server_with_link_only_helpers(accept_aggro_link: bool) -> Arc<GameServer> {
    let mut data = data();
    let t = data
        .systems
        .npcs
        .npc
        .templates
        .get_mut(&NPC_TEMPLATE)
        .expect("fixture template");
    t.accept_aggro_link = accept_aggro_link;
    t.aggro_link_special_rule_id = 1;
    t.aggro_link_help_dist = 6.0;
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    super::hooks::install(&server);
    server
}

/// An `AiCtx` for `npc` without starting its managed AI (direct behavior calls in tests).
fn ctx<'a>(server: &'a GameServer, npc: &NpcRef, ai: &'a mut NpcAi) -> AiCtx<'a> {
    AiCtx {
        server,
        npc: npc.clone(),
        obj: WorldObject::from(npc),
        ai,
    }
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
    fixture_on(server(), rule, follower_kind)
}

fn fixture_on(server: Arc<GameServer>, rule: i32, follower_kind: AiKind) -> Fixture {
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
    group::attach_spawned_group(&server, &rows, &group_row(rule), None);

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

/// One real 100 ms AI tick. Idle / HoldPosition / Talk start with `Behavior.Throttle(0.1)`
/// (`behaviors::throttle`), which compares the WALL clock with the last accepted tick, so a
/// `tick_all` + `sleep(1ms)` loop gets about one effective tick per 50 iterations on Linux (and
/// one per timer period on Windows). Forget the stamps instead of waiting.
fn tick(server: &GameServer) {
    for id in [LEADER_OBJ, FOLLOWER_OBJ, FOLLOWER2_OBJ] {
        super::manager::run_now(server, id, |c| {
            for k in [B::Idle, B::HoldPosition, B::Talk] {
                // `st` would create a slot for a behavior the AI does not have.
                if c.ai.has_behavior(k) {
                    c.st(k).last_tick = None;
                }
            }
        });
    }
    tick_all(server, 100);
}

/// A moving leader: the follower walks to `leader position + formation_offset_*` and stops
/// within `formation_tension` of it.
#[tokio::test]
async fn follower_moves_to_the_leader_plus_offset_and_stops_within_tension() {
    let f = fixture(1, AiKind::HoldPosition);
    // The settle loop may already have taken a step or two towards the formation point; start
    // from its spawn so the walk below does not depend on how many ticks it accepted.
    f.follower.data().unit.transform.set_xyz(95.0, 100.0, 100.0);
    let start = npc_pos(&f.follower);
    let target = Vec3::new(102.0, 97.0, 100.0);
    let start_dist = xy_dist(start, target);
    assert!(
        start_dist > 5.0,
        "fixture: the follower starts away, {start_dist}"
    );

    // One step is 10 m/s x 0.1 s = 1 m, so the walk of about 8 m needs about 8 ticks.
    for _ in 0..30 {
        // The leader keeps reporting a movement step (`MoveTowards`).
        f.leader.data().last_move_ms = crate::world::now_ms();
        tick(&f.server);
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
        tick(&f.server);
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
    // It walked to its formation point during the spawn settlement instead of roaming around
    // its own spawn (a roaming follower could never get closer than the 6 m roam radius allows).
    let start = npc_pos(&f.follower);
    assert!(
        xy_dist(start, Vec3::new(102.0, 97.0, 100.0)) <= 1.5,
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
    for _ in 0..40 {
        tick(&f.server);
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

/// `aggro_rule_id == 2` (`aggro_share`): pulling one member puts the attacker on every living
/// member's table (10.x `NpcAggroTable::CollectSharedAggroMembers`).
#[tokio::test]
async fn aggro_rule_2_shares_aggro_with_the_whole_spawned_group() {
    let f = fixture(2, AiKind::HoldPosition);
    pull_member(&f, &f.follower).await;
    assert!(
        has_aggro_on_player(&f.follower),
        "the pulled member keeps its own aggro"
    );
    assert!(
        has_aggro_on_player(&f.leader),
        "the move leader joins the fight (rule 2)"
    );
    assert!(
        has_aggro_on_player(&f.follower2),
        "every living group member joins the fight (rule 2)"
    );
}

/// `aggro_rule_id == 0`: neither mirrors the table nor answers the help call by group.
#[tokio::test]
async fn a_zero_aggro_rule_does_not_share_aggro() {
    let f = fixture(0, AiKind::HoldPosition);
    pull_member(&f, &f.follower).await;
    assert!(has_aggro_on_player(&f.follower));
    assert!(
        !has_aggro_on_player(&f.leader) && !has_aggro_on_player(&f.follower2),
        "rule 0 groups fight alone"
    );
    let player = f.server.world.object(PLAYER_OBJ).expect("player");
    let mut ai = NpcAi::new(AiKind::HoldPosition, &f.follower);
    let mut c = ctx(&f.server, &f.follower, &mut ai);
    super::behaviors::update_aggro_help(&mut c, &player, 200.0);
    assert!(
        !has_aggro_on_player(&f.leader) && !has_aggro_on_player(&f.follower2),
        "rule 0 members do not answer the help call by group either"
    );
}

/// `aggro_rule_id == 1` (`aggro_link`): the attacker is NOT mirrored to the other members'
/// tables (10.x only sets the group instance on the aggro table for rule 2, Z:855825-855826).
#[tokio::test]
async fn aggro_rule_1_does_not_mirror_the_aggro_table() {
    let f = fixture(1, AiKind::HoldPosition);
    let player = f.server.world.object(PLAYER_OBJ).expect("player");
    crate::systems::skills::combat::add_unit_aggro(
        &f.server,
        &WorldObject::from(&f.follower),
        AggroKind::Damage,
        &player,
        10,
    );
    let mut ai = NpcAi::new(AiKind::HoldPosition, &f.follower);
    let mut c = ctx(&f.server, &f.follower, &mut ai);
    super::group::share_aggro_with_group(&mut c, &player);
    assert!(
        has_aggro_on_player(&f.follower),
        "the pulled member keeps its own aggro"
    );
    assert!(
        !has_aggro_on_player(&f.leader) && !has_aggro_on_player(&f.follower2),
        "rule 1 does not copy the aggro table to the other members"
    );
}

/// `aggro_rule_id == 1` (`aggro_link`): the group mates answer the attacked member's help call
/// even when their own template's relation test declines (not aggressive, a faction rule with
/// the sight check off), as in the 10.x link-help scan's same-group-instance accept branch
/// (Z:823640-823646 / 823849-823851). The control of the test below.
#[tokio::test]
async fn aggro_rule_1_members_answer_the_help_call_despite_a_failing_relation_test() {
    let f = fixture_on(server_with_link_only_helpers(true), 1, AiKind::HoldPosition);
    let player = f.server.world.object(PLAYER_OBJ).expect("player");
    let mut ai = NpcAi::new(AiKind::HoldPosition, &f.follower);
    let mut c = ctx(&f.server, &f.follower, &mut ai);
    super::behaviors::update_aggro_help(&mut c, &player, 200.0);
    assert!(
        has_aggro_on_player(&f.leader),
        "the move leader answers the group help call"
    );
    assert!(
        has_aggro_on_player(&f.follower2),
        "every near member of the rule-1 batch answers the help call"
    );
}

/// The group link replaces only the relation test: a rule-1 mate whose template declines aggro
/// links (`accept_aggro_link` false: 43 of the 376 rule-1 member rows of the 1.2 data) keeps
/// declining, as the 10.x scan still requires `accept_aggro_link` (Z:823774). Same geometry as
/// the test above, only the template flag differs.
#[tokio::test]
async fn aggro_rule_1_does_not_override_a_template_that_declines_aggro_links() {
    let f = fixture_on(
        server_with_link_only_helpers(false),
        1,
        AiKind::HoldPosition,
    );
    let player = f.server.world.object(PLAYER_OBJ).expect("player");
    let mut ai = NpcAi::new(AiKind::HoldPosition, &f.follower);
    let mut c = ctx(&f.server, &f.follower, &mut ai);
    super::behaviors::update_aggro_help(&mut c, &player, 200.0);
    assert!(
        !has_aggro_on_player(&f.leader) && !has_aggro_on_player(&f.follower2),
        "a mate that declines aggro links does not answer the group help call"
    );
}

/// Rule-1 batches spread past their 6 m `aggro_link_help_dist` still answer: the group link
/// falls back to the template's 10 m aggression scan radius (`attack_start_range_scale * 10`).
#[tokio::test]
async fn aggro_rule_1_help_uses_the_aggression_radius_beyond_the_helper_distance() {
    let f = fixture_on(server_with_link_only_helpers(true), 1, AiKind::HoldPosition);
    // The settle loop may already have walked the follower towards its formation point; pin it
    // to its spawn so the distances below do not depend on how far it got. `distance_to`
    // subtracts both 0.5 m model radii.
    f.follower.data().unit.transform.set_xyz(95.0, 100.0, 100.0);
    // The mate moves 8 m - 1 m = 7 m away from the attacked member: beyond the 6 m helper
    // distance, inside the 10 m aggression radius.
    f.follower2
        .data()
        .unit
        .transform
        .set_xyz(103.0, 100.0, 100.0);
    let player = f.server.world.object(PLAYER_OBJ).expect("player");
    let mut ai = NpcAi::new(AiKind::HoldPosition, &f.follower);
    let mut c = ctx(&f.server, &f.follower, &mut ai);
    super::behaviors::update_aggro_help(&mut c, &player, 200.0);
    assert!(
        has_aggro_on_player(&f.follower2),
        "a spread rule-1 mate answers inside the aggression radius"
    );
}

/// Outside the fallback radius the same-batch help still refuses.
#[tokio::test]
async fn aggro_rule_1_help_refuses_a_mate_beyond_the_aggression_radius() {
    let f = fixture_on(server_with_link_only_helpers(true), 1, AiKind::HoldPosition);
    // Pinned to its spawn like in the test above (`distance_to` subtracts both 0.5 m radii).
    f.follower.data().unit.transform.set_xyz(95.0, 100.0, 100.0);
    // 12 m - 1 m = 11 m away: beyond both the 6 m helper distance and the 10 m aggression
    // radius.
    f.follower2
        .data()
        .unit
        .transform
        .set_xyz(107.0, 100.0, 100.0);
    let player = f.server.world.object(PLAYER_OBJ).expect("player");
    let mut ai = NpcAi::new(AiKind::HoldPosition, &f.follower);
    let mut c = ctx(&f.server, &f.follower, &mut ai);
    super::behaviors::update_aggro_help(&mut c, &player, 200.0);
    assert!(
        !has_aggro_on_player(&f.follower2),
        "a mate outside the fallback radius does not answer"
    );
}

// ===========================================================================================
// Review follow-ups: height source, off-formation walking, moving-leader returns, batch ids,
// blocked formation points and the follower's idle activity.
// ===========================================================================================

/// `HeightMaxCoefficient` of the synthetic `main_world` (as in the heightmap tests).
const COEF: f64 = 65535.0 / 4096.0;
const CELL_POINTS: usize = (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize;

/// A flat heightmap at `surface` metres for the test zone's cell (0, 0).
fn heightmaps(surface: f32) -> Arc<HeightmapWorlds> {
    let t = WorldHeightmap::with_size(0, "main_world", 2, 2, COEF);
    let value = (f64::from(surface) * COEF) as u16;
    assert!(t.insert_cell_with_holes(0, 0, vec![value; CELL_POINTS], &[]));
    Arc::new(HeightmapWorlds::from_parts(
        Arc::new(ClientFileManager::new()),
        vec![t],
        HashMap::from([(TEST_ZONE, 0)]),
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    ))
}

/// A 2 × 2 × 2 m static box collider centred at `(x, y, z)` (a wall between follower and point).
fn add_wall(server: &GameServer, x: f32, y: f32, z: f32) {
    // Wide in X so the blocked point stays blocked from the whole leader side.
    let box_g = BoxGeom::new(&BoxPrim::aligned(
        PhysicsVec3::ZERO,
        PhysicsVec3::new(4.0, 1.0, 2.0),
    ));
    let header = PhysGeomHeader {
        inertia: PhysicsVec3::splat(1.0),
        q: Quat::IDENTITY,
        origin: PhysicsVec3::ZERO,
        volume: 8.0,
        unk52: 1,
        surface_idx: 0,
    };
    let mut blob = Vec::new();
    save_phys_geometry(&header, &box_g, &mut blob);
    let files = HashMap::from([(
        "game/objects/test/group_wall.cgf".into(),
        crate::systems::ai::collision::cgf::tests::synthetic_cgf(&blob),
    )]);
    assert!(server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(files))
        .is_ok());
    let pw = crate::systems::ai::collision::phys_world(server, 0).expect("physics world");
    let files = crate::systems::ai::collision::files(server).expect("files");
    let models = crate::systems::ai::collision::model_cache(server, &files);
    let wall = DoodadInfo {
        obj_id: 8,
        model: "cgf://objects/test/group_wall.cgf".into(),
        pos: PhysicsVec3::new(x, y, z),
        q: Quat::IDENTITY,
        scale: 1.0,
        flags: collision_flags(false, false, false),
    };
    let mut shapes = HashMap::new();
    assert_eq!(
        apply_doodads(&pw, &files, &models, &mut shapes, &[wall]),
        (1, 0)
    );
}

/// A HoldPosition follower's formation walk uses the ground height under it, not its spawner's
/// Z (`ReferenceHeightAi.hold_or_idle` must be false for formation movement).
#[tokio::test]
async fn formation_move_uses_the_ground_height_not_the_spawner_height() {
    let server = server();
    server.systems.npcs.heightmaps.install(heightmaps(100.0));
    let f = fixture_on(server, 1, AiKind::HoldPosition);
    f.follower.data().spawner_z = Some(250.0);

    for _ in 0..40 {
        f.leader.data().last_move_ms = crate::world::now_ms();
        tick(&f.server);
    }
    let pos = npc_pos(&f.follower);
    assert!(
        (pos.z - 100.0).abs() < 1.0,
        "the follower walks on the ground, not at the spawner Z: {pos:?}"
    );
    assert!(
        xy_dist(pos, Vec3::new(102.0, 97.0, 100.0)) <= 1.5,
        "it still reached its formation point: {pos:?}"
    );
}

/// A follower left away from its formation point walks back even while the leader stands still.
#[tokio::test]
async fn a_follower_off_its_formation_point_walks_back_while_the_leader_stands_still() {
    let f = fixture(1, AiKind::HoldPosition);
    f.follower.data().unit.transform.set_xyz(90.0, 100.0, 100.0);

    for _ in 0..40 {
        f.leader.data().last_move_ms = 0;
        tick(&f.server);
    }
    let pos = npc_pos(&f.follower);
    assert!(
        xy_dist(pos, Vec3::new(102.0, 97.0, 100.0)) <= 1.5,
        "the follower re-forms without waiting for the leader to move: {pos:?}"
    );
}

/// A returning follower recomputes the formation point from the leader's current position; it
/// must not freeze the spot the leader was at when the fight ended.
#[tokio::test]
async fn a_returning_follower_tracks_a_moving_leaders_current_position() {
    let f = fixture(1, AiKind::HoldPosition);
    f.follower.data().unit.transform.set_xyz(90.0, 100.0, 100.0);
    post(&f.server, FOLLOWER_OBJ, |c| c.go_to_return());
    for _ in 0..5 {
        tick(&f.server);
    }

    // The leader moves on while the follower is still on its way back: the point it returns to
    // moves with the leader.
    f.leader.data().unit.transform.set_xyz(110.0, 100.0, 100.0);
    for _ in 0..60 {
        tick(&f.server);
    }
    let pos = npc_pos(&f.follower);
    assert!(
        xy_dist(pos, Vec3::new(112.0, 97.0, 100.0)) <= 1.5,
        "it returned to the leader's current formation point: {pos:?}"
    );
    assert!(
        xy_dist(pos, Vec3::new(102.0, 97.0, 100.0)) > 5.0,
        "it did not return to the leader's old spot: {pos:?}"
    );
}

/// Spawn batch ids are monotonic, so an old survivor never follows a new NPC that reuses its
/// leader's object id.
#[tokio::test]
async fn old_survivors_do_not_follow_a_reused_leader_object_id() {
    let f = fixture(1, AiKind::HoldPosition);
    let start = npc_pos(&f.follower);
    crate::systems::npcs::spawn_manager::delete_object(
        &f.server,
        &WorldObject::from(&f.leader),
        None,
    );
    let new_leader = spawn_npc(&f.server, LEADER_OBJ, 85.0, 100.0, AiKind::HoldPosition);
    group::attach_spawned_group(
        &f.server,
        &[(member_row(9, true, 0.0, 0.0, 1.0), new_leader.clone())],
        &group_row(1),
        None,
    );
    for _ in 0..25 {
        tick(&f.server);
    }
    assert!(
        group::move_leader(&f.server, &f.follower).is_none(),
        "a reused object id must not match the old batch"
    );

    for _ in 0..40 {
        new_leader.data().last_move_ms = crate::world::now_ms();
        tick(&f.server);
    }
    assert!(
        xy_dist(npc_pos(&f.follower), start) < 0.05,
        "the old survivor stays with its own idle behaviour"
    );
}

/// A formation point behind a wall is not walked to; the follower moves towards the leader
/// instead.
#[tokio::test]
async fn a_blocked_formation_point_falls_back_to_the_leader_position() {
    let server = server();
    server.systems.npcs.heightmaps.install(heightmaps(100.0));
    // Between the follower (95, 100) and its formation point (102, 97), not on the leader line.
    add_wall(&server, 98.0, 98.5, 100.0);
    let blocked = crate::systems::ai::collision::walk::walk_blocked(
        &server,
        0,
        [95.0, 100.0, 100.0],
        [102.0, 97.0, 100.0],
    );
    assert_eq!(
        blocked,
        Some(true),
        "fixture: the formation point is behind a wall"
    );
    let f = fixture_on(server, 1, AiKind::HoldPosition);

    for _ in 0..40 {
        f.leader.data().last_move_ms = crate::world::now_ms();
        tick(&f.server);
    }
    let pos = npc_pos(&f.follower);
    assert!(
        xy_dist(pos, Vec3::new(100.0, 100.0, 100.0)) <= 3.0,
        "the follower moved towards the leader when the point was blocked: {pos:?}"
    );
    assert!(
        xy_dist(pos, Vec3::new(102.0, 97.0, 100.0)) > 3.0,
        "it did not walk through the wall to the formation point: {pos:?}"
    );
}

/// A waiting HoldPosition follower still runs its idle activity: `check_for_talk` sees a
/// player it has not greeted yet and enters Talk (the formation code only takes over movement).
#[tokio::test]
async fn a_waiting_hold_position_follower_still_checks_for_talk() {
    let f = fixture(1, AiKind::HoldPosition);
    // Let the spawn-time greeting of the first player finish (Talk throttles at 100 ms).
    for _ in 0..10 {
        tick(&f.server);
    }
    assert_eq!(behavior(&f.server, FOLLOWER_OBJ), Some(B::HoldPosition));

    // Put the player in front of the follower and forget the greeting: the waiting follower
    // must still run check_for_talk and record it (the HoldPosition slot's greeted map).
    f._player.data().unit.transform.set_xyz(104.0, 96.0, 100.0);
    super::manager::run_now(&f.server, FOLLOWER_OBJ, |c| {
        c.st(B::HoldPosition).greeted.clear();
    });
    for _ in 0..3 {
        tick(&f.server);
    }
    let greeted = super::manager::run_now(&f.server, FOLLOWER_OBJ, |c| {
        c.st(B::HoldPosition).greeted.clone()
    })
    .expect("follower AI");
    assert!(
        greeted.contains_key(&PLAYER_OBJ),
        "the waiting follower still ran check_for_talk: {greeted:?}"
    );
}

/// A fight that ended because the abuser died must not latch the group share: the next fight
/// against the same object id shares again.
#[tokio::test]
async fn a_second_fight_after_the_abuser_died_shares_again() {
    let f = fixture(2, AiKind::HoldPosition);
    pull_member(&f, &f.follower).await;
    assert!(has_aggro_on_player(&f.leader));

    f._player.data().unit.hp = 0;
    for _ in 0..60 {
        tick(&f.server);
        if f.leader.data().unit.skill_state.aggro_table.is_empty()
            && behavior(&f.server, FOLLOWER_OBJ) == Some(B::HoldPosition)
        {
            break;
        }
    }
    assert!(
        f.leader.data().unit.skill_state.aggro_table.is_empty(),
        "the dead abuser was cleared from the leader"
    );

    f._player.data().unit.hp = 1000;
    pull_member(&f, &f.follower).await;
    assert!(
        has_aggro_on_player(&f.leader),
        "the second fight against the same object id shares again"
    );
}
