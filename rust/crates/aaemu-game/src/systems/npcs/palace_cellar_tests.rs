//! Palace Cellar scripts against the read-only 1.2 compact database.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::server::GameServer;
use crate::systems::ai;
use crate::systems::quests::spheres::load_sphere_areas;
use crate::systems::skills::{
    combat, skill,
    types::{skill_result, KillReason},
};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::{DoodadRef, NpcRef, PlayerRef, WorldInstance, WorldObject};

use super::doodad::manager as doodad;
use super::spawn_manager::TemplateExists;

const WORLD: &str = "instance_sal_temple";
const ZONE: u32 = 240;

fn setup() -> Option<(Arc<GameServer>, Arc<WorldInstance>)> {
    let db = std::env::var("AAEMU_COMPACT_DB").ok()?;
    let (server, world) = super::burntcastle_dungeon_tests::real_encounter_server(&db, WORLD, ZONE);
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let gd = &server.game_data.systems.npcs;
    let exists = TemplateExists {
        npc: &|id| gd.npc.exist(id),
        doodad: &|id| gd.doodad.exist(id),
        transfer: &|id| gd.npc_spawners.transfer_template_ids.contains(&id),
        gimmick: &|id| gd.gimmick.exist(id),
        slave: &|id| gd.npc_spawners.slave_template_ids.contains(&id),
    };
    let spawns = server.systems.npcs.spawns.get_or_create(&world);
    let stats = spawns
        .load_spawn_files(&server, &app.join("Data/Worlds").join(WORLD), &exists)
        .unwrap();
    assert_eq!(stats.doodad_missing_templates, 0);
    spawns.spawn_doodads(&server);
    Some((server, world))
}

fn spawn_npc(server: &GameServer, template: u32) -> NpcRef {
    server
        .systems
        .npcs
        .spawns
        .get(0)
        .unwrap()
        .get_all_spawners()
        .into_values()
        .flatten()
        .find(|s| s.unit_id == template && s.is_placement())
        .unwrap_or_else(|| panic!("missing Palace Cellar spawn {template}"))
        .force_spawn(server, 0)
        .unwrap()
}

fn doodad_of(world: &WorldInstance, template: u32, north: Option<bool>) -> DoodadRef {
    world
        .all_doodads()
        .into_iter()
        .find(|d| {
            let d = d.data();
            d.template_id == template && north.is_none_or(|north| (d.transform.y > 300.0) == north)
        })
        .unwrap_or_else(|| panic!("missing Palace Cellar doodad {template}/{north:?}"))
}

fn place_player(
    server: &Arc<GameServer>,
    id: u32,
    x: f32,
    y: f32,
    z: f32,
) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, id);
    let mut ch = test_character(id, &format!("P{id}"), x, y);
    ch.unit.level = 30;
    ch.unit.hp = 10_000;
    ch.unit.max_hp = 10_000;
    ch.unit.transform.set_xyz(x, y, z);
    ch.unit.transform.zone_id = ZONE;
    let p = spawn_player(server, &client, ch, 0x10_0000 + id);
    (client, p)
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_palace_cellar_barrier_and_both_ways_to_extinguish_the_door_fire() {
    let Some((server, world)) = setup() else {
        return;
    };
    let actor = WorldObject::from(spawn_npc(&server, 10073));
    let barrier = doodad_of(&world, 4580, None);
    assert_eq!(barrier.data().func_group_id, 11230);
    assert_eq!(
        skill::unit_use_skill(&server, &actor, 16594, &actor),
        skill_result::SUCCESS
    );
    assert_eq!(barrier.data().func_group_id, 11231);
    assert_eq!(
        skill::unit_use_skill(&server, &actor, 16600, &actor),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        world.doodad(barrier.obj_id).is_none(),
        "the actor's exit removes the barrier"
    );

    let flame = doodad_of(&world, 4741, None);
    let lever = doodad_of(&world, 4740, None);
    let (_client, player) = place_player(&server, 1, 541.52, 250.008, 144.033);
    let user = WorldObject::from(&player);
    server.systems.npcs.area_triggers.tick(&server);
    assert!(
        player.data().unit.buffs.check_buff(2783),
        "active flame burns"
    );

    doodad::use_doodad(&server, &world, &lever, Some(&user), 16220, 0);
    tokio::time::sleep(Duration::from_millis(2_800)).await;
    assert_eq!(
        flame.data().func_group_id,
        11716,
        "the underwater lever puts out the flame"
    );
    assert!(!player.data().unit.buffs.check_buff(2783));

    doodad::do_change_phase(&server, &world, &flame, None, 11715);
    for expected in [12709, 12710, 12711] {
        doodad::on_skill_hit(&server, &world, &flame, Some(&user), 18107);
        assert_eq!(flame.data().func_group_id, expected);
        server.systems.npcs.area_triggers.tick(&server);
        assert_eq!(
            server
                .systems
                .npcs
                .area_triggers
                .attached_to(flame.obj_id)
                .len(),
            usize::from(expected != 12711),
        );
    }
    assert!(!player.data().unit.buffs.check_buff(2783));
    assert!(server
        .systems
        .npcs
        .area_triggers
        .attached_to(flame.obj_id)
        .is_empty());
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_palace_cellar_flame_947_releases_its_inside_buff_on_phase_change() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = super::burntcastle_dungeon_tests::real_encounter_server(&db, WORLD, ZONE);
    let (_client, player) = place_player(&server, 1, 541.52, 250.008, 144.033);
    let (_other_client, outside) = place_player(&server, 2, 555.52, 250.008, 144.033);
    let flame = doodad::create(&server, &world, 0, 4109, None, true).unwrap();
    flame.data().transform = WorldObject::from(&player).transform();
    doodad::spawn(&server, &world, &flame);

    doodad::do_change_phase(&server, &world, &flame, None, 9972);
    let triggers = &server.systems.npcs.area_triggers;
    let attached = triggers.attached_to(flame.obj_id);
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].inside_buff_id, Some(2783));
    triggers.tick(&server);
    assert!(player.data().unit.buffs.check_buff(2783));
    assert!(!outside.data().unit.buffs.check_buff(2783));

    doodad::do_change_phase(&server, &world, &flame, None, 10390);
    assert!(attached[0].is_removed());
    assert!(triggers.attached_to(flame.obj_id).is_empty());
    assert!(!player.data().unit.buffs.check_buff(2783));
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_palace_cellar_boss_deaths_open_their_own_gates_and_drop_keys() {
    use crate::models::item_task::task_type;
    use crate::net::registry::ClientPacket;
    use crate::systems::economy::{inventory::Inv, looting::CSLootOpenBagPacket};

    const WATER: u32 = 15694;
    let Some((server, world)) = setup() else {
        return;
    };
    let spawns = server.systems.npcs.spawns.get(0).unwrap();
    assert!(!spawns
        .get_all_spawners()
        .into_values()
        .flatten()
        .any(|s| s.unit_id == 9810 && s.is_placement()));
    let (mut client, player) = place_player(&server, 1, 577.0, 250.0, 144.0);
    let victim = WorldObject::from(&player);
    client.conn.active_char = Some(player.clone());
    let inventory = server.systems.npcs.hooks.economy.get().unwrap();
    Inv::of(&player).bag().lock().unwrap().set_container_size(1);
    for (boss_id, north, chest_id, open_skill, key_id) in [
        (9809, false, 6121, 20613, 20045),
        (9796, true, 6125, 20616, 20046),
    ] {
        let npc = spawn_npc(&server, boss_id);
        let boss = WorldObject::from(&npc);
        let gate = doodad_of(&world, 4577, Some(north));
        let restriction = doodad_of(&world, 5947, Some(north));
        let other = doodad_of(&world, 4577, Some(!north));
        let other_group = other.data().func_group_id;
        assert_eq!(
            skill::unit_use_skill(&server, &boss, 17421, &boss),
            skill_result::SUCCESS
        );
        assert_eq!(gate.data().func_group_id, 11223);
        assert_eq!(restriction.data().func_group_id, 15321);
        if boss_id == 9809 {
            ai::npc::set_current_aggro_target(&npc, Some(&victim));
            assert_eq!(
                skill::unit_use_skill(&server, &boss, 16219, &victim),
                skill_result::SUCCESS
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(
                world
                    .all_npcs()
                    .iter()
                    .filter(|n| n.data().unit.template_id == 9810)
                    .count(),
                1
            );
        }
        // The boss dies below its standing height. The gate and the chest are owned by the
        // encounter, including when the kill is just beyond the gate skill's 45 m radius.
        let p = boss.transform();
        boss.with_transform(|t| t.set_xyz(p.x + 12.0, p.y + 8.0, p.z - 3.0));
        npc.data().unit.hp = 0;
        combat::do_die(&server, &boss, Some(&victim), KillReason::Damage);
        for _ in 0..3 {
            ai::manager::tick_all(&server, 100);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            gate.data().func_group_id,
            11228,
            "boss {boss_id} opens its gate on death"
        );
        assert_eq!(other.data().func_group_id, other_group);
        assert_eq!(restriction.data().func_group_id, 15322);
        let chest = doodad_of(&world, chest_id, None);
        assert_eq!(chest.data().transform.x, p.x + 12.0);
        victim.with_transform(|t| t.set_xyz(p.x + 12.0, p.y + 8.0, p.z - 3.0));
        ai::npc::check_moved_position(&server, &victim);
        assert!(inventory.acquire_item(&server, &player, task_type::LOOT, WATER, 1, -1));
        doodad::use_doodad(&server, &world, &chest, Some(&victim), open_skill, 0);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(inventory.item_count(&server, &player, key_id), 0);
        assert!(chest.data().has_loot_item());
        assert!(chest.data().phase.pending_loot.is_some());
        assert_eq!(
            inventory.bag_consume_item(&server, &player, task_type::LOOT, WATER, 1),
            1
        );
        CSLootOpenBagPacket {
            obj_id: chest.obj_id,
            obj2_id: 0,
            loot_all: true,
        }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
        assert_eq!(inventory.item_count(&server, &player, key_id), 1);
        assert_eq!(
            inventory.bag_consume_item(&server, &player, task_type::LOOT, key_id, 1),
            1
        );
    }
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_palace_cellar_akmits_fire_goes_out_in_shallow_water() {
    use crate::systems::ai::physics::water::{WaterBodyArea, WaterBodyAreaType};
    use crate::systems::skills::{buffs, controllers::Vec3, water};

    let Some((server, world)) = setup() else {
        return;
    };
    let mut pool = WaterBodyArea::new("Akmit's pool", WaterBodyAreaType::Polygon);
    pool.depth = 13.0;
    pool.points = vec![
        Vec3::new(632.0, 450.0, 147.72),
        Vec3::new(635.0, 450.0, 147.72),
        Vec3::new(635.0, 453.0, 147.72),
        Vec3::new(632.0, 453.0, 147.72),
        Vec3::new(632.0, 450.0, 147.72),
    ];
    pool.update_bounds();
    ai::physics::world_water(&server, &world).add(pool);
    let (_client, player) = place_player(&server, 1, 631.0, 451.0, 147.62);
    let victim = WorldObject::from(&player);
    let akmit = WorldObject::from(spawn_npc(&server, 12189));
    buffs::add_buff_by_id(&server, &victim, 2574, &akmit, 0);
    water::update_character_under_water(&server, &player);
    assert!(
        player.data().unit.buffs.check_buff(2574),
        "dry ground keeps the debuff"
    );
    victim.with_transform(|t| t.set_xyz(633.23, 451.13, 147.62));
    water::update_character_under_water(&server, &player);
    assert!(!player.data().unit.is_under_water);
    assert!(
        !player.data().unit.buffs.check_buff(2574),
        "shallow water puts out the fire"
    );
    buffs::add_buff_by_id(&server, &victim, 2574, &akmit, 0);
    assert!(
        !player.data().unit.buffs.check_buff(2574),
        "Akmit cannot ignite a player already standing in shallow water"
    );
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_palace_cellar_spheres_launch_the_boulder_and_akmit_cutscene() {
    use crate::data::client_files::ClientFileManager;
    use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};

    let Some((server, world)) = setup() else {
        return;
    };
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let pak = std::env::var("AAEMU_GAME_PAK").ok();
    if let Some(pak) = &pak {
        let files = Arc::new(ClientFileManager::from_sources(
            std::slice::from_ref(pak),
            &app,
        ));
        server
            .systems
            .npcs
            .heightmaps
            .install(Arc::new(HeightmapWorlds::new(
                files,
                &server.game_data.worlds,
                HeightmapConfig {
                    heightmaps_enable: true,
                    ..Default::default()
                },
            )));
    }
    server
        .systems
        .quests
        .spheres
        .set_areas(load_sphere_areas(&app, |id| matches!(id, 1425 | 1291)));
    let summoner = spawn_npc(&server, 10035);
    spawn_npc(&server, 10087);
    let (mut client, player) = place_player(&server, 1, 589.808, 372.422, 146.999);
    server.systems.quests.spheres.tick_at(&server, 0);
    let gimmicks = server.systems.npcs.gimmicks.for_world(world.id);
    for _ in 0..30 {
        ai::manager::tick_all(&server, 100);
        if !gimmicks.active().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let active = gimmicks
        .active()
        .pop()
        .expect("the trap sphere launches a boulder");
    let start = active.gimmick.data().unit.transform;
    let launch_velocity = active.gimmick.data().vel;
    assert_eq!(active.gimmick.data().unit.template_id, 42);
    if pak.is_none() {
        ai::collision::flow_tests::enable_collision(&server, summoner.data().unit.transform.z);
    }
    for _ in 0..32 {
        super::gimmick::manager::gimmick_tick(
            &server,
            &world,
            &active,
            Duration::from_millis(50),
            crate::world::now_ms(),
        );
    }
    let moved = active.gimmick.data().unit.transform;
    assert!(
        (moved.x - start.x).hypot(moved.y - start.y) > 10.0,
        "the boulder keeps rolling after landing: {start:?} -> {moved:?}, velocity {launch_velocity:?}"
    );
    assert_ne!(moved.rot_x, start.rot_x);
    assert!(moved.z < start.z);

    let obj = WorldObject::from(&player);
    obj.with_transform(|t| t.set_xyz(587.442, 433.952, 161.06));
    ai::npc::check_moved_position(&server, &obj);
    server.systems.quests.spheres.tick_at(&server, 500);
    for _ in 0..30 {
        ai::manager::tick_all(&server, 100);
        if player.data().quest_state.requested_cinema.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sequence = client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == crate::opcodes::sc::SCPlaySequencePacket)
        .collect::<Vec<_>>();
    assert_eq!(
        sequence,
        vec![(
            crate::opcodes::sc::SCPlaySequencePacket,
            b"\x09\x00id_240_01\x00\x00\x00".to_vec()
        )]
    );
}
