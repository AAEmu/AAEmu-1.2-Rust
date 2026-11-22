use std::{path::Path, sync::Arc, time::Duration};

use crate::server::GameServer;
use crate::systems::skills::{skill, types::skill_result};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::{NpcRef, PlayerRef, WorldInstance, WorldObject};

use super::{burntcastle_dungeon_tests::real_encounter_server, spawn_manager::TemplateExists};

const GREATER: &str = "instance_howling_abyss_2";
const ZONE: u32 = 280;

fn load_greater(db: &str) -> (Arc<GameServer>, Arc<WorldInstance>) {
    load_world(db, GREATER, ZONE)
}

/// The real-data encounter server for `name` (zone `zone`) with its shipped spawn files.
fn load_world(db: &str, name: &str, zone: u32) -> (Arc<GameServer>, Arc<WorldInstance>) {
    let (server, world) = real_encounter_server(db, name, zone);
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
        .join(name);
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
    npc_in(server, world, template, xyz, ZONE)
}

fn npc_in(
    server: &GameServer,
    world: &WorldInstance,
    template: u32,
    xyz: [f32; 3],
    zone: u32,
) -> NpcRef {
    let npc =
        super::npc::manager::create(server, server.ids.object.get_next_id().unwrap(), template)
            .expect("Howling NPC template");
    {
        let mut data = npc.data();
        data.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
        data.unit.transform.zone_id = zone;
    }
    world.spawn(&npc, &server.view_ctx());
    npc
}

fn player_at(server: &Arc<GameServer>, id: u32, xyz: [f32; 3]) -> (TestClient, PlayerRef) {
    player_in(server, id, xyz, ZONE)
}

fn player_in(
    server: &Arc<GameServer>,
    id: u32,
    xyz: [f32; 3],
    zone: u32,
) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, id);
    let mut ch = test_character(id, "Tester", xyz[0], xyz[1]);
    ch.unit.level = 50;
    ch.unit.hp = 10_000;
    ch.unit.max_hp = 10_000;
    ch.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
    ch.unit.transform.zone_id = zone;
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

// Normal Howling Abyss (`instance_howling_abyss`, zone 265): the special cases shared with
// Greater, which the tests above only cover on zone 280.

const NORMAL: &str = "instance_howling_abyss";
const NORMAL_ZONE: u32 = 265;
/// Normal Calleil (Greater 12759) at his placement.
const NORMAL_CALLEIL: u32 = 11201;
const CALLEIL_AT: [f32; 3] = [322.4, 500.4, 170.8];

fn load_normal(db: &str) -> (Arc<GameServer>, Arc<WorldInstance>) {
    load_world(db, NORMAL, NORMAL_ZONE)
}

fn first_npc(world: &WorldInstance, template: u32) -> Option<NpcRef> {
    world
        .all_npcs()
        .into_iter()
        .find(|npc| npc.data().unit.template_id == template)
}

/// Polls every 100 ms until `done`, at most `ms`.
async fn wait_until(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..ms / 100 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    done()
}

fn has_aggro_on(npc: &NpcRef, player: &PlayerRef) -> bool {
    npc.data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&player.obj_id)
}

/// N1: Normal Viboros (11265) lays its egg (18288, egg 11565) and the egg's hatching (18706)
/// summons the hatchling 11273 with the boss's target. The plain `SpawnEffect` row
/// (`use_summoner_aggro_target` 'f') would leave it idle.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_normal_viboros_hatchling_engages() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_normal(&db);
    let boss = npc_in(&server, &world, 11265, [664.9, 585.0, 196.3], NORMAL_ZONE);
    let (_client, player) = player_in(&server, 1, [657.0, 587.0, 196.6], NORMAL_ZONE);
    let target = WorldObject::from(&player);
    crate::systems::ai::npc::set_current_aggro_target(&boss, Some(&target));
    let boss = WorldObject::from(&boss);
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 18288, &boss),
        skill_result::SUCCESS
    );
    let mut egg = None;
    assert!(
        wait_until(5_000, || {
            egg = first_npc(&world, 11565);
            egg.is_some()
        })
        .await,
        "Viboros's egg from the event spawner"
    );
    let egg = WorldObject::from(&egg.unwrap());
    assert_eq!(
        skill::unit_use_skill(&server, &egg, 18706, &egg),
        skill_result::SUCCESS
    );
    let mut young = None;
    assert!(
        wait_until(2_000, || {
            young = first_npc(&world, 11273);
            young.is_some()
        })
        .await,
        "Viboros's hatchling"
    );
    assert!(
        has_aggro_on(&young.unwrap(), &player),
        "the hatchling takes the boss's target"
    );
}

/// N1: Normal Calleil's lightning summon (20106) brings the elementals 12384 into his fight.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_normal_calleil_lightning_elemental_joins_the_fight() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_normal(&db);
    let boss = npc_in(&server, &world, NORMAL_CALLEIL, CALLEIL_AT, NORMAL_ZONE);
    let (_client, player) = player_in(&server, 1, [322.4, 514.0, 170.8], NORMAL_ZONE);
    let victim = WorldObject::from(&player);
    crate::systems::ai::npc::set_current_aggro_target(&boss, Some(&victim));
    let caster = WorldObject::from(&boss);
    assert_eq!(
        skill::unit_use_skill(&server, &caster, 20106, &caster),
        skill_result::SUCCESS
    );
    let mut elemental = None;
    assert!(
        wait_until(3_000, || {
            elemental = first_npc(&world, 12384);
            elemental.is_some()
        })
        .await,
        "Calleil's lightning elemental"
    );
    assert!(
        has_aggro_on(&elemental.unwrap(), &player),
        "the elemental takes Calleil's target"
    );
}

/// N2: Normal Calleil's fire elementals (12385) from his spawners 102342 (20110) and 102334 /
/// 102340 (20122) get his target. The rows' `use_summoner_aggro_target` is 'f'.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_normal_calleil_fire_elementals_spawn_in_combat() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_normal(&db);
    let spawns = server.systems.npcs.spawns.get_or_create(&world);
    for spawner in [102334, 102340, 102342] {
        assert_eq!(
            spawns.get_npc_spawner(spawner).len(),
            1,
            "spawner {spawner}"
        );
    }
    let boss = npc_in(&server, &world, NORMAL_CALLEIL, CALLEIL_AT, NORMAL_ZONE);
    let (_client, player) = player_in(&server, 1, [324.0, 500.4, 170.8], NORMAL_ZONE);
    let victim = WorldObject::from(&player);
    crate::systems::ai::npc::set_current_aggro_target(&boss, Some(&victim));
    let boss = WorldObject::from(&boss);
    for (skill_id, count) in [(20110, 1), (20122, 3)] {
        assert_eq!(
            skill::unit_use_skill(&server, &boss, skill_id, &boss),
            skill_result::SUCCESS
        );
        let elementals = || -> Vec<NpcRef> {
            world
                .all_npcs()
                .into_iter()
                .filter(|npc| npc.data().unit.template_id == 12385)
                .collect()
        };
        assert!(
            wait_until(3_000, || elementals().len() >= count).await,
            "skill {skill_id} spawns fire elementals: {}",
            elementals().len()
        );
        for elemental in elementals() {
            assert!(
                has_aggro_on(&elemental, &player),
                "skill {skill_id}: the fire elemental takes Calleil's target"
            );
        }
    }
}

/// D7: Normal Calleil's Hellfire (18963, doodad 5170, clout 1415) carries its damage without
/// an inside buff, like Greater's clout 1542.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_normal_hellfire_damages_the_victim() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_normal(&db);
    let boss = npc_in(&server, &world, NORMAL_CALLEIL, CALLEIL_AT, NORMAL_ZONE);
    let (_client, player) = player_in(&server, 1, [323.0, 501.0, 170.6], NORMAL_ZONE);
    let caster = WorldObject::from(&boss);
    assert_eq!(
        skill::unit_use_skill(&server, &caster, 18963, &caster),
        skill_result::SUCCESS
    );
    let hit = wait_until(4_000, || {
        server.systems.npcs.area_triggers.tick(&server);
        player.data().unit.hp < 10_000
    })
    .await;
    assert!(hit, "the Hellfire explosion damages the player");
}

/// D5: the Howling spike traps 5516 (clout 1295), 5706 (1334) and 5708 (1336) give Injury
/// (3999) as a contact buff: it outlasts the 100 ms pulse that applied it.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_normal_spike_injury_outlasts_the_pulse() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_normal(&db);
    server.systems.npcs.doodads.tasks.set_manual(true);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .spawn_doodads(&server);
    for (i, (template, pulse)) in [(5516, 14141), (5706, 14641), (5708, 14647)]
        .into_iter()
        .enumerate()
    {
        let spike = world
            .all_doodads()
            .into_iter()
            .find(|doodad| doodad.data().template_id == template)
            .unwrap_or_else(|| panic!("Howling spike {template}"));
        let t = spike.data().transform;
        let (_client, victim) = player_in(&server, i as u32 + 1, [t.x, t.y, t.z], NORMAL_ZONE);
        super::doodad::manager::do_change_phase(&server, &world, &spike, None, pulse);
        assert!(
            victim.data().unit.buffs.check_buff(3999),
            "spike {template}'s pulse injures the player on it"
        );
        server.systems.npcs.doodads.tasks.advance(&server, 250);
        server.systems.npcs.area_triggers.tick(&server);
        assert!(
            victim.data().unit.buffs.check_buff(3999),
            "spike {template}'s injury outlasts its pulse"
        );
    }
}

/// S11: Normal Godfrey's dialogue line 07 (19372, effect 23742) continues once (19373) from the
/// caster, however many players stand in its 100 m area.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_normal_godfrey_line_continues_once_with_multiple_nearby_targets() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_normal(&db);
    let godfrey = WorldObject::from(npc_in(
        &server,
        &world,
        12104,
        [551.0, 487.0, 188.0],
        NORMAL_ZONE,
    ));
    let (mut client, _p1) = player_in(&server, 1, [552.0, 487.0, 188.0], NORMAL_ZONE);
    let (_client2, _p2) = player_in(&server, 2, [553.0, 487.0, 188.0], NORMAL_ZONE);
    let (_client3, _p3) = player_in(&server, 3, [554.0, 487.0, 188.0], NORMAL_ZONE);
    client.drain();
    assert_eq!(
        skill::unit_use_skill(&server, &godfrey, 19372, &godfrey),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let continuations = client
        .drain()
        .into_iter()
        .filter(|(opcode, body)| {
            *opcode == crate::opcodes::sc::SCSkillFiredPacket
                && body.starts_with(&19373u32.to_le_bytes())
        })
        .count();
    assert_eq!(
        continuations, 1,
        "one continuation regardless of the player count"
    );
}

/// S11: the Normal last-room cue (12144) puts its cinema call buff (19442, effect 23953, buff
/// 4498) on itself, not on the players in its 500 m area.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_normal_cinema_call_buff_stays_on_the_cue() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_normal(&db);
    let cue = WorldObject::from(npc_in(
        &server,
        &world,
        12144,
        [463.6, 699.6, 167.0],
        NORMAL_ZONE,
    ));
    let (_c1, p1) = player_in(&server, 1, [464.5, 690.0, 167.0], NORMAL_ZONE);
    let (_c2, p2) = player_in(&server, 2, [460.0, 695.0, 167.0], NORMAL_ZONE);
    assert_eq!(
        skill::unit_use_skill(&server, &cue, 19442, &cue),
        skill_result::SUCCESS
    );
    let mut on_cue = false;
    for _ in 0..20 {
        on_cue |= cue.with_base_unit(|b| b.buffs().check_buff(4498));
        for p in [&p1, &p2] {
            assert!(
                !p.data().unit.buffs.check_buff(4498),
                "the cinema call buff reaches a player"
            );
        }
        if on_cue {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(on_cue, "the cue carries its own cinema call buff");
}

/// A plot event source or target: a position (`Some((x, y, z))`) or a unit (`None`).
type PlotPoint = Option<(f32, f32, f32)>;

/// `(event id, source, target)` of the `SCPlotEventPacket`s a client received since the last
/// drain, inside `CompressedGamePackets` batches too.
fn drain_plot_events(client: &mut TestClient) -> Vec<(u32, PlotPoint, PlotPoint)> {
    fn object(s: &mut aaemu_commons::PacketStream) -> Option<PlotPoint> {
        match s.read_u8().ok()? {
            1 => {
                s.read_bc().ok()?;
                Some(None)
            }
            2 => {
                let p = s.read_position().ok()?;
                for _ in 0..3 {
                    s.read_i8().ok()?;
                }
                Some(Some(p))
            }
            _ => None,
        }
    }
    fn parse(body: &[u8]) -> Option<(u32, PlotPoint, PlotPoint)> {
        let mut s = aaemu_commons::PacketStream::from_bytes(body.to_vec());
        s.read_u16().ok()?;
        let event = s.read_u32().ok()?;
        s.read_u32().ok()?;
        let source = object(&mut s)?;
        let target = object(&mut s)?;
        Some((event, source, target))
    }
    let op = crate::opcodes::sc::SCPlotEventPacket;
    let mut seen = Vec::new();
    for frame in client.drain_frames() {
        let bytes = &frame.bytes[..];
        match bytes.get(1) {
            Some(1) if u16::from_le_bytes([bytes[4], bytes[5]]) == op => {
                seen.extend(parse(&bytes[6..]));
            }
            Some(4) => {
                let Some(payload) = crate::systems::skills::packets::inflate_stored(&bytes[4..])
                else {
                    continue;
                };
                let marker = [0, 0, op.to_le_bytes()[0], op.to_le_bytes()[1]];
                for i in 0..payload.len().saturating_sub(4) {
                    if payload[i..i + 4] == marker {
                        seen.extend(parse(&payload[i + 4..]));
                    }
                }
            }
            _ => {}
        }
    }
    seen
}

/// S19: Calleil's fireball (Normal 18976 / Greater 21131, a position skill aimed at his own
/// spot, as his AI casts it) falls on random points 5 m from him. Its marker event (4023 /
/// 5919) puts each point on the floor and launches the fireball from 5 m (the event's height
/// offset) straight above it. Without that, the marker hangs 5 m above the floor and the
/// fireball starts at Calleil.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_calleil_fireball_lands_on_the_floor() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    for (name, zone, calleil, fireball, marker) in [
        (NORMAL, NORMAL_ZONE, NORMAL_CALLEIL, 18976, 4023),
        (GREATER, ZONE, 12759, 21131, 5919),
    ] {
        let (server, world) = real_encounter_server(&db, name, zone);
        let boss = WorldObject::from(npc_in(&server, &world, calleil, CALLEIL_AT, zone));
        let (mut client, player) = player_in(
            &server,
            1,
            [CALLEIL_AT[0], CALLEIL_AT[1] + 12.0, CALLEIL_AT[2]],
            zone,
        );
        client.drain_frames();
        assert_eq!(
            skill::unit_use_skill(&server, &boss, fireball, &WorldObject::from(&player)),
            skill_result::SUCCESS,
            "{name}: Calleil casts {fireball}"
        );
        let mut markers = Vec::new();
        wait_until(8_000, || {
            markers.extend(
                drain_plot_events(&mut client)
                    .into_iter()
                    .filter(|(id, _, _)| *id == marker),
            );
            markers.len() >= 3
        })
        .await;
        assert!(markers.len() >= 3, "{name}: marker events {markers:?}");
        let floor = CALLEIL_AT[2];
        let near = |z: f32, expected: f32| (z - expected).abs() < 0.1;
        for (_, source, target) in &markers {
            let (Some(source), Some(target)) = (source, target) else {
                panic!("{name}: marker {marker} between positions: {markers:?}");
            };
            assert!(
                near(target.2, floor) && near(source.2, floor + 5.0),
                "{name}: marker {marker} on the floor, launched 5 m above: {markers:?}"
            );
            assert!(
                (source.0 - target.0).abs() < 0.1 && (source.1 - target.1).abs() < 0.1,
                "{name}: the launch point is above the marker: {markers:?}"
            );
        }
    }
}
