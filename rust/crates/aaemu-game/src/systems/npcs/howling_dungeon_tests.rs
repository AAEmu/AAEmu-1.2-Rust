use std::{path::Path, sync::Arc, time::Duration};

use crate::server::GameServer;
use crate::systems::skills::{skill, types::skill_result};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::{NpcRef, PlayerRef, WorldInstance, WorldObject};

use super::{burntcastle_dungeon_tests::real_encounter_server, spawn_manager::TemplateExists};

const GREATER: &str = "instance_howling_abyss_2";
const ZONE: u32 = 280;

fn load_greater(db: &str) -> (Arc<GameServer>, Arc<WorldInstance>) {
    let (server, world) = real_encounter_server(db, GREATER, ZONE);
    let data = &server.game_data.systems.npcs;
    let no = |_: u32| false;
    let exists = TemplateExists {
        npc: &|id| data.npc.exist(id),
        doodad: &|id| data.doodad.exist(id),
        transfer: &no,
        gimmick: &no,
        slave: &no,
    };
    let folder = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds")
        .join(GREATER);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .load_spawn_files(&server, &folder, &exists)
        .unwrap();
    (server, world)
}

fn npc_at(server: &GameServer, world: &WorldInstance, template: u32, xyz: [f32; 3]) -> NpcRef {
    let npc =
        super::npc::manager::create(server, server.ids.object.get_next_id().unwrap(), template)
            .expect("Howling NPC template");
    {
        let mut data = npc.data();
        data.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
        data.unit.transform.zone_id = ZONE;
    }
    world.spawn(&npc, &server.view_ctx());
    npc
}

fn player_at(server: &Arc<GameServer>, id: u32, xyz: [f32; 3]) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, id);
    let mut ch = test_character(id, "Tester", xyz[0], xyz[1]);
    ch.unit.level = 50;
    ch.unit.hp = 10_000;
    ch.unit.max_hp = 10_000;
    ch.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
    ch.unit.transform.zone_id = ZONE;
    let player = spawn_player(server, &client, ch, 0x10_0000 + id);
    (client, player)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_godfrey_encounter_advances_once_with_multiple_nearby_targets() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let godfrey = WorldObject::from(npc_at(&server, &world, 12788, [551.0, 487.0, 188.0]));
    let (mut client, _p1) = player_at(&server, 1, [552.0, 487.0, 188.0]);
    let (_client2, _p2) = player_at(&server, 2, [553.0, 487.0, 188.0]);
    let (_client3, _p3) = player_at(&server, 3, [554.0, 487.0, 188.0]);
    client.drain();
    assert_eq!(
        skill::unit_use_skill(&server, &godfrey, 21422, &godfrey),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let continuations = client
        .drain()
        .into_iter()
        .filter(|(opcode, body)| {
            *opcode == crate::opcodes::sc::SCSkillFiredPacket
                && body.starts_with(&21423u32.to_le_bytes())
        })
        .count();
    assert_eq!(
        continuations, 1,
        "one continuation regardless of the player count"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_calleil_fire_elemental_spawns_in_combat() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let spawns = server.systems.npcs.spawns.get_or_create(&world);
    assert_eq!(spawns.get_npc_spawner(110541).len(), 1);
    let boss = npc_at(&server, &world, 12759, [322.4, 500.4, 170.8]);
    let (_client, player) = player_at(&server, 1, [324.0, 500.4, 170.8]);
    let victim = WorldObject::from(&player);
    crate::systems::ai::npc::set_current_aggro_target(&boss, Some(&victim));
    let boss = WorldObject::from(&boss);
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21111, &boss),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let elemental = world
        .all_npcs()
        .into_iter()
        .find(|npc| npc.data().unit.template_id == 12760)
        .expect("Calleil's fire elemental");
    assert!(elemental
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&player.obj_id));
    for _ in 0..18 {
        crate::systems::ai::manager::run_now(&server, elemental.obj_id, |ai| ai.tick(100, true));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        world.npc(elemental.obj_id).is_some(),
        "the elemental survives spawn initialization"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_last_room_starts_its_cinematic() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let cue = npc_at(&server, &world, 13020, [463.4, 699.5, 167.0]);
    crate::systems::ai::hooks::spawn_positioned(&server, &cue);
    let (mut client, _player) = player_at(&server, 1, [464.493, 646.141, 167.092]);
    let (mut upstairs, _player) = player_at(&server, 2, [464.0, 690.0, 187.0]);
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let areas = crate::systems::quests::spheres::load_sphere_areas(&app, |id| id == 2187);
    server.systems.quests.spheres.set_areas(areas);
    server.systems.quests.spheres.tick_at(&server, 0);
    let mut played = false;
    for _ in 0..60 {
        crate::systems::ai::manager::run_now(&server, cue.obj_id, |ai| ai.tick(100, true));
        tokio::time::sleep(Duration::from_millis(100)).await;
        played |= client.drain().iter().any(|(opcode, body)| {
            *opcode == crate::opcodes::sc::SCPlaySequencePacket
                && body.windows(9).any(|bytes| bytes == b"id_280_01")
        });
        if played && world.npc(cue.obj_id).is_none() {
            break;
        }
    }
    assert!(played, "the room entrance triggers the Greater cinema");
    assert!(
        world.npc(cue.obj_id).is_none(),
        "the cinematic cue actor retires"
    );
    assert!(!upstairs
        .drain()
        .iter()
        .any(|(opcode, _)| { *opcode == crate::opcodes::sc::SCPlaySequencePacket }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_viboros_egg_hatches_and_the_young_engages() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = npc_at(&server, &world, 12762, [664.9, 585.0, 196.3]);
    let (_client, player) = player_at(&server, 1, [657.0, 587.0, 196.6]);
    let target = WorldObject::from(&player);
    crate::systems::ai::npc::set_current_aggro_target(&boss, Some(&target));
    let boss = WorldObject::from(&boss);
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21238, &boss),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(2_350)).await;
    let egg = WorldObject::from(
        world
            .all_npcs()
            .into_iter()
            .find(|npc| npc.data().unit.template_id == 12958)
            .expect("Viboros's egg from the event spawner"),
    );
    assert_eq!(
        skill::unit_use_skill(&server, &egg, 21230, &egg),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        world.object(egg.obj_id()).is_none(),
        "the shell vanishes on hatching"
    );
    let young = world
        .all_npcs()
        .into_iter()
        .find(|npc| npc.data().unit.template_id == 12959)
        .expect("Viboros's hatchling");
    assert!(young
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&player.obj_id));
    for _ in 0..12 {
        crate::systems::ai::manager::run_now(&server, young.obj_id, |ai| ai.tick(100, true));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        world.npc(young.obj_id).is_some(),
        "the hatchling survives initialization"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_boss_charges_cover_ground() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    for (i, (boss_id, charge, start)) in [
        (12762, 21235, [664.9, 585.0, 196.3]),
        (12758, 21042, [463.4, 679.2, 167.0]),
    ]
    .into_iter()
    .enumerate()
    {
        let boss = npc_at(&server, &world, boss_id, start);
        let (_client, victim) = player_at(
            &server,
            (i + 1) as u32,
            [start[0] + 6.0, start[1], start[2]],
        );
        let caster = WorldObject::from(&boss);
        let target = WorldObject::from(&victim);
        if charge == 21235 {
            // Viboros can only charge a player carrying his mark.
            crate::systems::skills::buffs::add_buff_by_id(&server, &target, 5483, &caster, 0);
        }
        assert_eq!(
            skill::unit_use_skill(&server, &caster, charge, &target),
            skill_result::SUCCESS,
            "boss {boss_id} starts charge {charge}"
        );
        tokio::time::sleep(Duration::from_millis(1_700)).await;
        assert!(
            boss.data().unit.transform.x > start[0] + 0.5,
            "charge {charge} moved the boss"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_calleil_water_and_hellfire_phases() {
    use crate::systems::{ai::physics::world_water, skills::controllers::Vec3};

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .spawn_doodads(&server);
    let water_controller = world
        .all_doodads()
        .into_iter()
        .find(|doodad| doodad.data().template_id == 6288)
        .expect("Calleil's rising water control");
    let boss = WorldObject::from(npc_at(&server, &world, 12759, [322.4, 500.4, 170.8]));
    let (_client, p) = player_at(&server, 1, [323.0, 501.0, 170.6]);
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21092, &boss),
        skill_result::SUCCESS
    );
    let water = world_water(&server, &world);
    let pool = Vec3::new(323.0, 501.0, 170.5);
    assert_eq!(water_controller.data().func_group_id, 16637);
    assert!(water.is_water(pool).is_none());
    tokio::time::sleep(Duration::from_millis(2_250)).await;
    assert_eq!(water_controller.data().func_group_id, 16638);
    assert!(water.is_water(pool).is_some(), "water rises into the room");
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21082, &boss),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(3_350)).await;
    assert!(
        p.data().unit.hp < 10_000,
        "player in the Hellfire explosion takes damage"
    );
    tokio::time::sleep(Duration::from_millis(3_300)).await;
    assert!(
        water.is_water(pool).is_none(),
        "the next phase lowers the water"
    );
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_floor_spikes_apply_damage_and_injury() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .spawn_doodads(&server);
    let spike = world
        .all_doodads()
        .into_iter()
        .find(|doodad| doodad.data().template_id == 5706)
        .expect("Howling floor spike");
    let t = spike.data().transform;
    let (_client, victim) = player_at(&server, 1, [t.x, t.y, t.z]);
    super::doodad::manager::do_change_phase(&server, &world, &spike, None, 14641);
    assert!(
        victim.data().unit.hp < 10_000,
        "the spike's pulse deals damage"
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        victim.data().unit.buffs.check_buff(3999),
        "injury outlasts the spike pulse"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_musperosa_uses_lasso_and_petrifying_orbs() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = WorldObject::from(npc_at(&server, &world, 12758, [463.4, 679.2, 167.0]));
    let (_client, target) = player_at(&server, 1, [467.2, 679.2, 167.0]);
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21066, &boss),
        skill_result::SUCCESS
    );
    let mut lasso = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        lasso |= target.data().unit.buffs.check_buff(5338);
    }
    assert!(lasso, "Musperosa's Lasso reaches the player");
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21055, &boss),
        skill_result::SUCCESS
    );
    for _ in 0..60 {
        if world
            .all_doodads()
            .iter()
            .any(|doodad| doodad.data().template_id == 6261)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Musperosa's petrifying orb should spawn in the arena");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_blackscale_pack_aggros_only_after_players_descend() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let medusa = npc_at(&server, &world, 12784, [466.1, 585.6, 154.9]);
    crate::systems::ai::hooks::spawn_positioned(&server, &medusa);
    let (_client, player) = player_at(&server, 1, [466.3, 585.8, 161.0]);
    let is_aggro = || {
        medusa
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&player.obj_id)
    };
    for _ in 0..12 {
        crate::systems::ai::manager::run_now(&server, medusa.obj_id, |ai| ai.tick(100, true));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!is_aggro());
    player.data().unit.transform.set_height(154.9);
    world.add_visible_object(&player, &server.view_ctx());
    for _ in 0..12 {
        crate::systems::ai::manager::run_now(&server, medusa.obj_id, |ai| ai.tick(100, true));
        if is_aggro() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Medusa should aggro a player on her floor");
}
