//! `NpcSpawnerNpc` `"NpcGroup"` member expansion: the spawner spawns every NPC template of the
//! group at the spawner position plus its `formation_offset`, not the group id as an NPC.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::WorldSpawnPosition;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::npcs::doodad::spawner::{NpcSpawnerDef, SpawnerBase};
use crate::systems::npcs::heightmap::{
    HeightmapConfig, HeightmapWorlds, WorldHeightmap, CELL_HMAP_RESOLUTION,
};
use crate::systems::npcs::npc::group_data::{NpcGroup, NpcGroupData, NpcGroupMember};
use crate::systems::npcs::npc::spawner_data::{
    npc_spawner_category, NpcSpawnerData, NpcSpawnerNpc, NpcSpawnerTemplate,
};
use crate::systems::npcs::spawn_manager;
use crate::systems::npcs::spawn_manager::backend::TestBackend;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{DoodadRef, Entity, NpcRef, WorldInstance, WorldObject};

use super::{NpcSpawner, NpcSpawnerInit, NpcSpawnerShared};

struct Backend;

impl TestBackend for Backend {
    fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
        if template_id == 0 || template_id >= 90_000 {
            return None; // "not exist at db"
        }
        let id = if obj_id == 0 {
            server.ids.object.get_next_id().ok()?
        } else {
            obj_id
        };
        let mut n = Npc::new(template_id);
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.level = 10;
        Some(Entity::new(id, n))
    }

    fn doodad_create(
        &self,
        _server: &GameServer,
        _world: &Arc<WorldInstance>,
        _obj_id: u32,
        _template_id: u32,
    ) -> Option<DoodadRef> {
        None
    }
}

fn server() -> Arc<GameServer> {
    let s = Arc::new(test_server_with(GameData::empty()));
    s.set_self();
    *s.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    s
}

fn position(x: f32, y: f32) -> WorldSpawnPosition {
    WorldSpawnPosition {
        world_id: 0,
        zone_id: TEST_ZONE,
        x,
        y,
        z: 100.0,
        ..WorldSpawnPosition::default()
    }
}

fn group_member_row(id: u32, spawner: u32, group: u32) -> NpcSpawnerNpc {
    NpcSpawnerNpc {
        id,
        npc_spawner_template_id: spawner,
        member_id: group,
        member_type: "NpcGroup".into(),
        weight: 1.0,
        unit_id: 0,
    }
}

fn plain_member_row(id: u32, spawner: u32, npc: u32) -> NpcSpawnerNpc {
    NpcSpawnerNpc {
        id,
        npc_spawner_template_id: spawner,
        member_id: npc,
        member_type: "Npc".into(),
        weight: 1.0,
        unit_id: 0,
    }
}

fn group(id: u32) -> NpcGroup {
    NpcGroup {
        id,
        name: "sea worms".into(),
        aggro_rule_id: 1,
        enable_respawn: false,
    }
}

fn member(id: u32, group: u32, npc: u32, x: f32, y: f32, z: f32) -> NpcGroupMember {
    NpcGroupMember {
        id,
        npc_group_id: group,
        npc_id: npc,
        is_leader: x == 0.0 && y == 0.0,
        is_move_leader: x == 0.0 && y == 0.0,
        formation_offset_x: x,
        formation_offset_y: y,
        formation_offset_z: z,
        formation_tension: 1.0,
    }
}

fn template(id: u32, npcs: Vec<NpcSpawnerNpc>) -> NpcSpawnerTemplate {
    NpcSpawnerTemplate {
        id,
        spawn_delay_min: 10.0,
        spawn_delay_max: 10.0,
        npcs,
        ..NpcSpawnerTemplate::default()
    }
}

fn make_spawner(template: NpcSpawnerTemplate, unit_id: u32) -> Arc<NpcSpawner> {
    make_spawner_with_despawn(template, unit_id, 20)
}

fn make_spawner_with_despawn(
    template: NpcSpawnerTemplate,
    unit_id: u32,
    despawn_time: i32,
) -> Arc<NpcSpawner> {
    let spawner_id = template.id;
    let spawnable_npcs = template.npcs.clone();
    Arc::new(NpcSpawner::new(
        NpcSpawnerInit {
            id: spawner_id,
            spawner_id,
            unit_id,
            follow_path: String::new(),
            follow_npc: 0,
            respawn_time: 10,
            despawn_time,
            special_link: String::new(),
            count: 1,
            npc_spawner_ids: vec![spawner_id],
            parent_world: Some(0),
            template: Some(Arc::new(template)),
            spawnable_npcs,
        },
        NpcSpawnerShared::new(position(100.0, 100.0)),
    ))
}

fn sea_worm_members() -> Vec<NpcGroupMember> {
    vec![
        member(191, 50, 8564, 0.0, 0.0, 0.0),
        member(192, 50, 8566, 2.0, -3.0, 0.0),
        member(193, 50, 8566, -2.0, -3.0, 0.0),
    ]
}

fn sea_worm_group() -> NpcGroupData {
    NpcGroupData::from_parts(vec![group(50)], sea_worm_members())
}

fn respawn_sea_worm_group() -> NpcGroupData {
    let mut g = group(50);
    g.enable_respawn = true;
    NpcGroupData::from_parts(vec![g], sea_worm_members())
}

/// The real data shape: spawner 9571 (`max_population` 1) has one `NpcGroup` member row (group
/// 50) and its world placements name member NPC 8564. The group must spawn whole, at the
/// offsets, instead of template 50 (which collides with an unrelated NPC).
#[tokio::test]
async fn npc_group_member_spawns_every_member_at_its_offset() {
    let mut data = GameData::empty();
    data.systems.npcs.npc_groups = sea_worm_group();
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![template(9571, Vec::new())],
        vec![group_member_row(10075, 9571, 50)],
    )
    .unwrap();
    let server = Arc::new(test_server_with(data));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));

    let spawner = make_spawner(
        template(9571, vec![group_member_row(10075, 9571, 50)]),
        8564,
    );
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(&server, &client, test_character(1, "P", 100.0, 100.0), 5000);

    spawner.update(&server);

    let spawned = spawner
        .spawned_npcs()
        .get(&spawner.spawner_id)
        .cloned()
        .expect("group members registered on the spawner");
    let mut templates: Vec<u32> = spawned.iter().map(|n| n.data().unit.template_id).collect();
    templates.sort_unstable();
    assert_eq!(
        templates,
        vec![8564, 8566, 8566],
        "every group member spawns; the group id 50 is not an NPC template"
    );
    assert_eq!(
        server.world.world(0).unwrap().all_npcs().len(),
        3,
        "all members are in the world"
    );

    let mut places: Vec<(u32, f32, f32, f32)> = spawned
        .iter()
        .map(|n| {
            let t = n.data().unit.transform;
            (n.data().unit.template_id, t.x, t.y, t.z)
        })
        .collect();
    places.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    assert_eq!(
        places,
        vec![
            (8564, 100.0, 100.0, 100.0),
            (8566, 98.0, 97.0, 100.0),
            (8566, 102.0, 97.0, 100.0),
        ],
        "each member stands at the spawner position plus its formation offset"
    );

    // Runtime membership (`systems::ai::group`): the batch id, the move leader and each
    // member's own offsets / tension.
    let leader = spawned
        .iter()
        .find(|n| n.data().unit.template_id == 8564)
        .expect("leader");
    let instance = leader.data().npc_group_instance;
    assert_ne!(instance, 0, "a spawned batch has a monotonic id");
    assert_ne!(
        instance, leader.obj_id,
        "the batch id is not the leader's reused object id"
    );
    {
        let d = leader.data();
        assert_eq!(d.npc_group_instance, instance);
        assert_eq!(d.npc_group_id, 50);
        assert_eq!(d.npc_group_aggro_rule_id, 1);
        assert!(d.npc_group_is_move_leader);
        assert_eq!(d.npc_group_leader_obj_id, leader.obj_id);
        assert_eq!(d.npc_group_formation_tension, 1.0);
    }
    for npc in spawned.iter().filter(|n| n.obj_id != leader.obj_id) {
        let d = npc.data();
        assert_eq!(d.npc_group_instance, instance);
        assert_eq!(d.npc_group_id, 50);
        assert_eq!(d.npc_group_aggro_rule_id, 1);
        assert!(!d.npc_group_is_move_leader);
        assert_eq!(d.npc_group_leader_obj_id, leader.obj_id);
        assert_eq!(d.npc_group_formation_tension, 1.0);
    }
    let offsets: Vec<(f32, f32, f32)> = spawned
        .iter()
        .filter(|n| n.obj_id != leader.obj_id)
        .map(|n| n.data().npc_group_formation_offset)
        .collect();
    assert_eq!(offsets, vec![(2.0, -3.0, 0.0), (-2.0, -3.0, 0.0)]);
}

#[tokio::test]
async fn plain_members_of_a_mixed_spawner_keep_spawning() {
    let mut data = GameData::empty();
    data.systems.npcs.npc_groups = sea_worm_group();
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![template(9631, Vec::new())],
        vec![
            plain_member_row(100, 9631, 8563),
            group_member_row(101, 9631, 50),
        ],
    )
    .unwrap();
    let mut t = template(
        9631,
        vec![
            plain_member_row(100, 9631, 8563),
            group_member_row(101, 9631, 50),
        ],
    );
    t.max_population = 4;
    t.min_population = 4;
    let server = Arc::new(test_server_with(data));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));

    let spawner = make_spawner(t, 8563);
    spawner.do_spawn(&server);

    let mut templates: Vec<u32> = spawner
        .spawned_npcs()
        .values()
        .flatten()
        .map(|n| n.data().unit.template_id)
        .collect();
    templates.sort_unstable();
    assert_eq!(
        templates,
        vec![8563, 8564, 8566, 8566],
        "the ordinary member and the whole group fill the four slots"
    );
}

#[tokio::test]
async fn unknown_group_is_a_retryable_empty_pass_not_a_wrong_npc() {
    let server = server();
    let mut t = template(9571, vec![group_member_row(10075, 9571, 50)]);
    t.max_population = 1;
    let spawner = make_spawner(t, 8564);
    spawner.set_spawn_scheduled(true);

    spawner.do_spawn(&server);

    assert_eq!(spawner.current_spawn_count(), 0);
    assert!(!spawner.is_spawn_scheduled(), "the next tick retries");
    assert!(server.world.world(0).unwrap().all_npcs().is_empty());
}

/// The real data shape: every group member NPC also has an ordinary (mostly AUTOCREATED)
/// member row, and the world places the members individually. The group sibling must not change
/// which spawner a placement selects, even when the group spawner has a time window.
fn ghost_army_data(group_window: Option<(f32, f32)>) -> GameData {
    let mut data = GameData::empty();
    data.systems.npcs.npc_groups = NpcGroupData::from_parts(
        vec![NpcGroup {
            id: 142,
            name: "ghost army".into(),
            aggro_rule_id: 1,
            enable_respawn: false,
        }],
        vec![
            member(498, 142, 12728, 0.0, 0.0, 0.0),
            member(499, 142, 12729, 3.0, 0.0, 0.0),
        ],
    );
    let mut single = template(14135, Vec::new());
    single.npc_spawner_category_id = npc_spawner_category::AUTOCREATED;
    let mut group_spawner = template(14133, Vec::new());
    if let Some((start, end)) = group_window {
        group_spawner.start_time = start;
        group_spawner.end_time = end;
    }
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![single, group_spawner],
        vec![
            plain_member_row(15217, 14135, 12728),
            group_member_row(15345, 14133, 142),
        ],
    )
    .unwrap();
    data
}

#[tokio::test]
async fn placements_keep_selecting_the_autocreated_single_member() {
    for (label, window) in [
        ("without a group schedule", None),
        ("outside the group's time window", Some((0.1, 1.0))),
    ] {
        let server = Arc::new(test_server_with(ghost_army_data(window)));
        server.set_self();
        *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
        // 12:00 is outside the ghost army's authored 00:06-01:00 window.
        server.time.set(12.0);
        let world = server.world.world(0).unwrap();
        let manager = server.systems.npcs.spawns.get_or_create(&world);
        let mut def = NpcSpawnerDef {
            base: SpawnerBase {
                unit_id: 12728,
                ..SpawnerBase::default()
            },
            count: 1,
            npc_spawner_ids: Vec::new(),
        };
        def.base.position = position(100.0, 100.0);
        assert_eq!(manager.add_npc_spawner(&server, def), Ok(1), "{label}");
        let spawner = manager
            .get_all_spawners()
            .values()
            .flatten()
            .next()
            .unwrap()
            .clone();
        assert_eq!(
            spawner.spawner_id, 14135,
            "{label}: the AUTOCREATED single member spawner"
        );

        spawner.update(&server);

        let mut templates: Vec<u32> = spawner
            .spawned_npcs()
            .values()
            .flatten()
            .map(|n| n.data().unit.template_id)
            .collect();
        templates.sort_unstable();
        assert_eq!(
            templates,
            vec![12728],
            "{label}: exactly one member, not the whole group 142"
        );
    }
}

#[tokio::test]
async fn summon_effect_does_not_rewrite_a_group_template() {
    let mut data = GameData::empty();
    data.systems.npcs.npc_groups = sea_worm_group();
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![template(9571, Vec::new())],
        vec![
            plain_member_row(10074, 9571, 9000),
            group_member_row(10075, 9571, 50),
        ],
    )
    .unwrap();
    let server = Arc::new(test_server_with(data));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    let world = server.world.world(0).unwrap();
    let manager = server.systems.npcs.spawns.get_or_create(&world);
    let client = TestClient::new(&server, 1, 1);
    let player = spawn_player(&server, &client, test_character(1, "P", 100.0, 100.0), 5000);

    // The summoned unit id equals the group id of a template that also has a plain row: the
    // C# rewrite would replace the group template with the plain row retargeted to id 50.
    let spawner = manager
        .get_npc_spawner_at(&server, 50, &WorldObject::from(&player))
        .expect("summon spawner");
    let template = spawner.template().expect("template");
    assert_ne!(template.id, 9571, "the group template is not retargeted");
    assert_eq!(template.npcs.len(), 1);
    assert_eq!(template.npcs[0].member_id, 50);
    assert_eq!(
        template.npcs[0].member_type, "Npc",
        "a generated single-NPC template"
    );

    let stored = server
        .game_data
        .systems
        .npcs
        .npc_spawners
        .get_npc_spawner_template(9571)
        .unwrap();
    assert_eq!(stored.npcs.len(), 2, "the group template keeps its rows");
    assert!(
        stored
            .npcs
            .iter()
            .any(|n| n.member_type == "NpcGroup" && n.member_id == 50),
        "the group row is intact: {:?}",
        stored.npcs
    );
}

// ===========================================================================================
// `npc_groups.enable_respawn`: the 10.x `NpcGroupSupporter` partial respawn
// (`sub_39343460` / `sub_393422D0` / `sub_39341EF0`, Z:852468 / 851557 / 851350).
// ===========================================================================================

struct RespawnFixture {
    server: Arc<GameServer>,
    spawner: Arc<NpcSpawner>,
    _client: TestClient,
    _player: crate::world::PlayerRef,
    members: Vec<NpcRef>,
}

/// Spawns the three-member group 50 through a spawner at (100, 100, 100) with a 2 s corpse
/// despawn and the template's 10 s spawn delay (the group respawn timer).
fn spawn_group_for_respawn(enable_respawn: bool) -> RespawnFixture {
    let mut data = GameData::empty();
    data.systems.npcs.npc_groups = if enable_respawn {
        respawn_sea_worm_group()
    } else {
        sea_worm_group()
    };
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![template(9571, Vec::new())],
        vec![group_member_row(10075, 9571, 50)],
    )
    .unwrap();
    let server = Arc::new(test_server_with(data));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    let spawner = make_spawner_with_despawn(
        template(9571, vec![group_member_row(10075, 9571, 50)]),
        8564,
        2,
    );
    let client = TestClient::new(&server, 1, 1);
    let player = spawn_player(&server, &client, test_character(1, "P", 100.0, 100.0), 5000);
    spawner.update(&server);
    let members = spawner
        .spawned_npcs()
        .get(&spawner.spawner_id)
        .cloned()
        .expect("the group spawned");
    assert_eq!(members.len(), 3);
    RespawnFixture {
        server,
        spawner,
        _client: client,
        _player: player,
        members,
    }
}

fn spawn_manager_of(server: &GameServer) -> Arc<spawn_manager::SpawnManager> {
    let world = server.world.world(0).expect("world");
    server.systems.npcs.spawns.get_or_create(&world)
}

fn respawn_tick(f: &RespawnFixture) {
    spawn_manager_of(&f.server).check_respawns_tick(&f.server);
}

/// A flat heightmap at `surface` metres for the test zone's cell (0, 0), so the group respawn
/// ground probe finds a floor at the leader's distant position.
fn flat_heightmap(surface: f32) -> Arc<HeightmapWorlds> {
    let t = WorldHeightmap::with_size(0, "main_world", 2, 2, 65535.0 / 4096.0);
    let value = (f64::from(surface) * (65535.0 / 4096.0)) as u16;
    let points = (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize;
    assert!(t.insert_cell_with_holes(0, 0, vec![value; points], &[]));
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

/// Spawns the group through `NpcSpawnerNpc.Spawn` with `owner_id` (a summon / event spawner
/// member) so the batch records its owner.
fn spawn_group_with_owner(owner_id: u32) -> (Arc<GameServer>, Arc<NpcSpawner>, Vec<NpcRef>) {
    let mut data = GameData::empty();
    data.systems.npcs.npc_groups = respawn_sea_worm_group();
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![template(9571, Vec::new())],
        vec![group_member_row(10075, 9571, 50)],
    )
    .unwrap();
    let server = Arc::new(test_server_with(data));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    let spawner = make_spawner_with_despawn(
        template(9571, vec![group_member_row(10075, 9571, 50)]),
        8564,
        2,
    );
    let spawned = super::spawn_member(
        &server,
        &group_member_row(10075, 9571, 50),
        &spawner,
        owner_id,
    )
    .expect("spawn member")
    .expect("group members");
    assert_eq!(spawned.len(), 3);
    (server, spawner, spawned)
}

fn kill(server: &GameServer, npc: &NpcRef) {
    {
        let mut d = npc.data();
        d.unit.hp = 0;
        d.dead_time_ms = crate::world::now_ms();
    }
    spawn_manager::on_npc_died(server, npc);
}

fn living(f: &RespawnFixture) -> Vec<NpcRef> {
    f.spawner
        .spawned_npcs()
        .values()
        .flatten()
        .filter(|n| !n.data().unit.is_dead())
        .cloned()
        .collect()
}

fn member_with_offset(f: &RespawnFixture, offset: (f32, f32, f32)) -> NpcRef {
    f.members
        .iter()
        .find(|n| n.data().npc_group_formation_offset == offset)
        .cloned()
        .expect("member with the formation offset")
}

/// `enable_respawn = 't'`: a dead member is respawned alone after the spawner's respawn delay,
/// at the leader position plus its formation offset, and keeps the batch identity; the
/// spawner's own whole-batch respawn is not scheduled for it.
#[tokio::test]
async fn enable_respawn_respawns_a_lost_member_into_the_same_batch() {
    let f = spawn_group_for_respawn(true);
    let manager = spawn_manager_of(&f.server);
    let leader = f
        .members
        .iter()
        .find(|n| n.data().npc_group_is_move_leader)
        .cloned()
        .expect("move leader");
    let victim = member_with_offset(&f, (2.0, -3.0, 0.0));
    let batch = leader.data().npc_group_instance;
    assert_ne!(batch, 0);

    // Settle the timer while the whole batch is alive (10.x restarts it every ineligible tick).
    respawn_tick(&f);
    kill(&f.server, &victim);
    assert!(
        manager.pending_respawns().is_empty(),
        "the group owns the lost slot, not a spawner respawn deadline"
    );
    assert_eq!(
        f.server
            .systems
            .npcs
            .group_batches
            .reserved_slots(&f.spawner),
        1,
        "the lost slot is reserved against the spawner refill"
    );

    // The corpse is still there at 1 s: nothing respawns.
    spawn_manager::advance_test_clock(Duration::from_secs(1));
    respawn_tick(&f);
    assert_eq!(
        living(&f).len(),
        2,
        "no respawn while the corpse is present"
    );

    // At 3 s the corpse is gone but the 10 s delay has not elapsed.
    spawn_manager::advance_test_clock(Duration::from_secs(2));
    respawn_tick(&f);
    assert_eq!(living(&f).len(), 2, "no respawn before the spawner delay");

    // At 10 s after the last ineligible tick the lost member comes back.
    spawn_manager::advance_test_clock(Duration::from_secs(7));
    respawn_tick(&f);
    let alive = living(&f);
    assert_eq!(alive.len(), 3, "the lost member respawned");
    let respawned = alive
        .iter()
        .find(|n| {
            n.obj_id != leader.obj_id && n.data().npc_group_formation_offset == (2.0, -3.0, 0.0)
        })
        .expect("the respawned member keeps its formation offset");
    assert!(
        !Arc::ptr_eq(respawned, &victim),
        "a fresh NPC replaced the lost member (object ids are reusable)"
    );
    {
        let d = respawned.data();
        assert_eq!(d.npc_group_instance, batch, "same batch id");
        assert!(!d.npc_group_is_move_leader);
        assert_eq!(d.npc_group_leader_obj_id, leader.obj_id);
        assert_eq!(d.npc_group_formation_offset, (2.0, -3.0, 0.0));
        assert_eq!(d.npc_group_formation_tension, 1.0);
    }
    let t = respawned.data().unit.transform;
    assert_eq!(
        (t.x, t.y, t.z),
        (102.0, 97.0, 100.0),
        "leader position plus the formation offset"
    );
    assert!(
        f.spawner
            .spawned_npcs()
            .get(&f.spawner.spawner_id)
            .is_some_and(|l| l.iter().any(|n| Arc::ptr_eq(n, respawned))),
        "the respawned member is registered on its spawner again"
    );
    assert_eq!(
        f.server
            .systems
            .npcs
            .group_batches
            .reserved_slots(&f.spawner),
        0,
        "the slot is no longer reserved"
    );
}

/// The 10.x eligibility gate: while any living member of the batch is in combat, nothing is
/// respawned and the delay restarts once the fight ends.
#[tokio::test]
async fn enable_respawn_waits_while_a_member_fights() {
    let f = spawn_group_for_respawn(true);
    let victim = member_with_offset(&f, (2.0, -3.0, 0.0));
    let fighter = f
        .members
        .iter()
        .find(|n| !n.data().npc_group_is_move_leader && n.obj_id != victim.obj_id)
        .cloned()
        .expect("another living member");

    respawn_tick(&f);
    kill(&f.server, &victim);
    spawn_manager::advance_test_clock(Duration::from_secs(3));
    respawn_tick(&f);

    fighter.data().unit.is_in_battle = true;
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    respawn_tick(&f);
    assert_eq!(
        living(&f).len(),
        2,
        "no member respawns while the batch is fighting"
    );

    fighter.data().unit.is_in_battle = false;
    respawn_tick(&f);
    spawn_manager::advance_test_clock(Duration::from_secs(9));
    respawn_tick(&f);
    assert_eq!(living(&f).len(), 2, "the delay restarted after combat");
    spawn_manager::advance_test_clock(Duration::from_secs(1));
    respawn_tick(&f);
    assert_eq!(
        living(&f).len(),
        3,
        "the member respawns after the new delay"
    );
}

/// `RespawnLostGroupMembers` is anchored on the move leader: a dead leader blocks the partial
/// respawn, and the last death of the batch returns to the spawner's whole-batch respawn.
#[tokio::test]
async fn enable_respawn_needs_the_move_leader() {
    let f = spawn_group_for_respawn(true);
    let manager = spawn_manager_of(&f.server);
    let leader = f
        .members
        .iter()
        .find(|n| n.data().npc_group_is_move_leader)
        .cloned()
        .expect("move leader");
    let followers: Vec<NpcRef> = f
        .members
        .iter()
        .filter(|n| !n.data().npc_group_is_move_leader)
        .cloned()
        .collect();

    respawn_tick(&f);
    kill(&f.server, &leader);
    assert!(
        manager.pending_respawns().is_empty(),
        "the two living followers still own the leader's lost slot"
    );
    spawn_manager::advance_test_clock(Duration::from_secs(3));
    respawn_tick(&f);
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    respawn_tick(&f);
    assert_eq!(
        living(&f).len(),
        2,
        "without the move leader the lost member is not respawned"
    );

    kill(&f.server, &followers[0]);
    assert!(
        manager.pending_respawns().is_empty(),
        "one follower still lives"
    );
    kill(&f.server, &followers[1]);
    assert_eq!(
        manager.pending_respawns().len(),
        1,
        "the last death schedules the spawner's whole-batch respawn"
    );
    assert_eq!(
        f.server
            .systems
            .npcs
            .group_batches
            .reserved_slots(&f.spawner),
        0,
        "the batch is gone"
    );

    spawn_manager::advance_test_clock(Duration::from_secs(3));
    respawn_tick(&f);
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    respawn_tick(&f);
    f.spawner.update(&f.server);
    assert_eq!(
        living(&f).len(),
        3,
        "the emptied group respawns as one batch through the spawner"
    );
}

/// The partial respawn is a spawn: a closed spawner time window refuses it until the window
/// reopens (the due timer then resumes).
#[tokio::test]
async fn enable_respawn_respects_the_spawner_window() {
    let f = spawn_group_for_respawn(true);
    let victim = member_with_offset(&f, (2.0, -3.0, 0.0));
    respawn_tick(&f);
    kill(&f.server, &victim);
    spawn_manager::advance_test_clock(Duration::from_secs(3));
    respawn_tick(&f);

    let open = f.spawner.template().expect("template");
    let mut closed = (*open).clone();
    closed.start_time = 13.0;
    closed.end_time = 14.0;
    f.spawner.set_template(Some(Arc::new(closed)));
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    respawn_tick(&f);
    assert_eq!(
        living(&f).len(),
        2,
        "no respawn while the spawner time window is closed"
    );

    let mut reopened = (*open).clone();
    reopened.start_time = 0.0;
    reopened.end_time = 0.0;
    f.spawner.set_template(Some(Arc::new(reopened)));
    respawn_tick(&f);
    assert_eq!(
        living(&f).len(),
        3,
        "the due respawn resumes when the window reopens"
    );
}

/// A batch with no living member can never respawn; the sweep drops it so its reserved slots
/// cannot block the spawner's refills forever.
#[tokio::test]
async fn a_batch_without_a_living_member_is_dropped() {
    let f = spawn_group_for_respawn(true);
    let leader = f
        .members
        .iter()
        .find(|n| n.data().npc_group_is_move_leader)
        .cloned()
        .expect("move leader");
    let followers: Vec<NpcRef> = f
        .members
        .iter()
        .filter(|n| !n.data().npc_group_is_move_leader)
        .cloned()
        .collect();
    // The followers die normally and mark their slots; the leader's death bypasses the batch
    // marking (`npc_group_instance` cleared for its death path only), which is the leak this
    // defensive sweep exists for: the batch stays behind with no living member.
    let batch = leader.data().npc_group_instance;
    // The spawn manager owns the world's respawn/despawn queues: create it before the deaths,
    // as the game tick would.
    respawn_tick(&f);
    for follower in &followers {
        kill(&f.server, follower);
    }
    {
        let mut d = leader.data();
        d.npc_group_instance = 0;
        d.unit.hp = 0;
        d.dead_time_ms = crate::world::now_ms();
    }
    spawn_manager::on_npc_died(&f.server, &leader);
    leader.data().npc_group_instance = batch;
    assert_eq!(
        f.server
            .systems
            .npcs
            .group_batches
            .reserved_slots(&f.spawner),
        2,
        "the two dead follower slots are still tracked before the sweep"
    );
    respawn_tick(&f);
    assert_eq!(
        f.server
            .systems
            .npcs
            .group_batches
            .reserved_slots(&f.spawner),
        0,
        "the dead batch no longer reserves spawner slots"
    );
}

/// An out-of-place leader re-anchors the lost member on its spawn/home position plus the
/// formation offset instead of the leader's temporary spot.
#[tokio::test]
async fn enable_respawn_reanchors_on_the_leader_home_when_it_strays() {
    let f = spawn_group_for_respawn(true);
    let leader = f
        .members
        .iter()
        .find(|n| n.data().npc_group_is_move_leader)
        .cloned()
        .expect("move leader");
    let victim = member_with_offset(&f, (2.0, -3.0, 0.0));
    f.server
        .systems
        .npcs
        .heightmaps
        .install(flat_heightmap(100.0));
    respawn_tick(&f);
    // The leader chases something 100 m from its spawn (100, 100, 100).
    leader.data().unit.transform.set_xyz(200.0, 200.0, 100.0);
    kill(&f.server, &victim);
    spawn_manager::advance_test_clock(Duration::from_secs(3));
    respawn_tick(&f);
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    respawn_tick(&f);
    let respawned = living(&f)
        .into_iter()
        .find(|n| n.data().npc_group_formation_offset == (2.0, -3.0, 0.0))
        .expect("the lost member respawned");
    let t = respawned.data().unit.transform;
    assert!(
        (t.x - 102.0).abs() < 0.01 && (t.y - 97.0).abs() < 0.01 && (t.z - 100.0).abs() < 0.2,
        "the lost member is anchored on the leader's spawn/home, not on the leader at (200, 200): {t:?}"
    );
}

/// A respawned member keeps the batch's `Unit.OwnerId` (summon / event spawner members).
#[tokio::test]
async fn enable_respawn_keeps_the_batch_owner() {
    let (server, spawner, members) = spawn_group_with_owner(77);
    let victim = members
        .iter()
        .find(|n| n.data().npc_group_formation_offset == (2.0, -3.0, 0.0))
        .cloned()
        .expect("member");
    assert_eq!(victim.data().unit.owner_id, 77);
    spawn_manager_of(&server).check_respawns_tick(&server);
    kill(&server, &victim);
    spawn_manager::advance_test_clock(Duration::from_secs(3));
    spawn_manager_of(&server).check_respawns_tick(&server);
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    spawn_manager_of(&server).check_respawns_tick(&server);
    let respawned = spawner
        .spawned_npcs()
        .values()
        .flatten()
        .find(|n| {
            n.data().npc_group_formation_offset == (2.0, -3.0, 0.0)
                && !members.iter().any(|m| Arc::ptr_eq(m, n))
        })
        .cloned()
        .expect("the lost member respawned");
    assert_eq!(
        respawned.data().unit.owner_id,
        77,
        "the respawned member keeps the batch owner"
    );
}

/// `enable_respawn = 'f'`: unchanged - the death keeps the ordinary spawner respawn deadline,
/// no batch is tracked and the group only refills through the spawner once it is empty.
#[tokio::test]
async fn disabled_respawn_keeps_the_spawner_refill() {
    let f = spawn_group_for_respawn(false);
    let manager = spawn_manager_of(&f.server);
    let victim = member_with_offset(&f, (2.0, -3.0, 0.0));

    respawn_tick(&f);
    kill(&f.server, &victim);
    assert_eq!(
        manager.pending_respawns().len(),
        1,
        "a disabled group keeps the per-NPC spawner respawn"
    );
    assert_eq!(
        f.server
            .systems
            .npcs
            .group_batches
            .reserved_slots(&f.spawner),
        0,
        "no partial-respawn batch is tracked"
    );

    spawn_manager::advance_test_clock(Duration::from_secs(3));
    respawn_tick(&f);
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    respawn_tick(&f);
    assert_eq!(living(&f).len(), 2, "no partial respawn with the flag off");

    for npc in f.members.iter().filter(|n| n.obj_id != victim.obj_id) {
        kill(&f.server, npc);
    }
    spawn_manager::advance_test_clock(Duration::from_secs(3));
    respawn_tick(&f);
    spawn_manager::advance_test_clock(Duration::from_secs(10));
    respawn_tick(&f);
    f.spawner.update(&f.server);
    assert_eq!(living(&f).len(), 3, "the emptied batch respawns as before");
}
