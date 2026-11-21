//! Sharpwind Mines regression tests using the 1.2 compact data and shipped paths.

use std::sync::Arc;
use std::time::Duration;

use crate::models::item_task::task_type;
use crate::server::GameServer;
use crate::systems::ai::{hooks, manager as ai};
use crate::systems::skills::data::effects::NpcControlEffect;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::types::skill_result;
use crate::systems::skills::{relation, skill};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::{ObjectKind, PlayerRef, WorldInstance, WorldObject};

const WORLD: &str = "instance_cuttingwind_deadmine";
const ZONE: u32 = 262;

fn fixture() -> (Arc<GameServer>, Arc<WorldInstance>) {
    let db =
        std::env::var("AAEMU_COMPACT_DB").expect("set AAEMU_COMPACT_DB to the 1.2 compact data");
    super::burntcastle_dungeon_tests::real_encounter_server(&db, WORLD, ZONE)
}

fn place_player(
    server: &Arc<GameServer>,
    id: u32,
    x: f32,
    y: f32,
    z: f32,
) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, id);
    let mut ch = test_character(id, &format!("Sharpwind {id}"), x, y);
    ch.unit.level = 40;
    ch.unit.faction_id = 101;
    ch.unit.hp = 100;
    ch.unit.max_hp = 10_000;
    ch.unit.mp = 100;
    ch.unit.max_mp = 10_000;
    ch.unit.transform.set_xyz(x, y, z);
    ch.unit.transform.zone_id = ZONE;
    let player = spawn_player(server, &client, ch, 0x10_0000 + id);
    (client, player)
}

fn place_npc(
    server: &GameServer,
    world: &WorldInstance,
    template: u32,
    x: f32,
    y: f32,
    z: f32,
) -> WorldObject {
    let npc = super::npc::manager::create(server, 0, template).expect("NPC template");
    {
        let mut data = npc.data();
        data.unit.transform.set_xyz(x, y, z);
        data.unit.transform.zone_id = ZONE;
    }
    hooks::spawn_positioned(server, &npc);
    world.spawn(&npc, &server.view_ctx());
    hooks::spawned(server, &npc, "");
    WorldObject::from(&npc)
}

fn place_doodad(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    template: u32,
    x: f32,
    y: f32,
    z: f32,
) -> WorldObject {
    let doodad = super::doodad::manager::create(server, world, 0, template, None, true).unwrap();
    {
        let mut data = doodad.data();
        data.transform.set_xyz(x, y, z);
        data.transform.zone_id = ZONE;
    }
    super::doodad::phase::init_doodad(&super::doodad::runtime::DoodadCtx::new(
        server,
        world.clone(),
        doodad.clone(),
    ));
    super::doodad::manager::spawn(server, world, &doodad);
    WorldObject::from(&doodad)
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_consumed_staves_still_restore_health_and_mana() {
    let (server, world) = fixture();
    for (id, template, interaction) in [(1, 5362, 19019), (2, 5363, 19022)] {
        let x = 400.0 + id as f32 * 25.0;
        let (_client, player) = place_player(&server, id, x, 330.0, 166.0);
        let staff = place_doodad(&server, &world, template, x + 1.0, 330.0, 166.0);
        assert_eq!(
            skill::unit_use_skill(&server, &WorldObject::from(&player), interaction, &staff),
            skill_result::SUCCESS,
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(world.object(staff.obj_id()).is_none(), "staff was consumed");
        let unit = &player.data().unit;
        let restored = if template == 5362 { unit.hp } else { unit.mp };
        assert!(
            restored > 100,
            "staff {template}: restored value {restored}"
        );
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_ranged_thieves_keep_their_distance_and_cast_illusion() {
    let (server, world) = fixture();
    for (id, template) in [(1, 11358), (2, 11110), (3, 11111)] {
        let x = 400.0 + id as f32 * 100.0;
        let (_client, player) = place_player(&server, id, x, 330.0, 166.0);
        let thief = place_npc(&server, &world, template, x - 12.0, 330.0, 166.0);
        let npc = thief.as_npc().unwrap();
        if template == 11111 {
            let mut data = npc.data();
            data.unit.hp = data.unit.max_hp / 2;
        }
        ai::tick_all(&server, 100);
        hooks::engage(&server, npc, &WorldObject::from(&player));
        for _ in 0..5 {
            ai::tick_all(&server, 100);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            (thief.transform().x - (x - 12.0)).abs() < 0.25,
            "{template} closed in"
        );
        if template == 11111 {
            let clones = world
                .all_of_kind(ObjectKind::Npc)
                .into_iter()
                .filter(|object| object.as_npc().unwrap().data().unit.template_id == 11582)
                .count();
            assert_eq!(clones, 2, "Illusion spawns both clones at half health");
        }
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_frozen_zone_hits_enemies_only() {
    let (server, world) = fixture();
    let (_client, victim) = place_player(&server, 1, 410.0, 330.0, 166.0);
    victim.data().unit.hp = 10_000;
    let illusionist = place_npc(&server, &world, 11111, 415.0, 330.0, 166.0);
    let ally = place_npc(&server, &world, 11110, 411.0, 330.0, 166.0);
    let pos = crate::systems::skills::geometry::pos_unit_from(
        &server,
        &Target::Obj(WorldObject::from(&victim)),
    );
    super::doodad::interactions::summon_doodad_at(&server, &illusionist, &pos, 5150);
    for _ in 0..24 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        server.systems.npcs.area_triggers.tick(&server);
    }
    assert!(victim.data().unit.buffs.check_buff(3903));
    assert!(!ally.with_base_unit(|unit| unit.buffs().check_buff(3903)));
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_nerta_is_untargetable_through_both_bomb_drop_buffs() {
    let (server, world) = fixture();
    let (_client, player) = place_player(&server, 1, 620.0, 330.0, 166.0);
    let player = Target::Obj(WorldObject::from(&player));
    let nerta = place_npc(&server, &world, 11362, 622.0, 330.0, 166.0);
    let target = Target::Obj(nerta.clone());
    assert!(relation::can_attack(&server, &player, &target));
    assert_eq!(
        skill::unit_use_skill(&server, &nerta, 18813, &nerta),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(nerta.as_npc().unwrap().data().unit.invisible);
    assert!(!relation::can_attack(&server, &player, &target));
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(!nerta.as_npc().unwrap().data().unit.invisible);
    assert!(relation::can_attack(&server, &player, &target));
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_okape_plays_the_cinema_and_tornadoes_travel() {
    let (server, world) = fixture();
    let (mut client, _player) = place_player(&server, 1, 615.0, 641.0, 139.8);
    let (mut below, _player) = place_player(&server, 2, 615.0, 641.0, 124.0);
    client.drain();
    below.drain();
    place_npc(&server, &world, 12188, 613.275, 641.428, 139.873);
    let tornado = place_npc(&server, &world, 12014, 609.908, 598.787, 135.267);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let sequence = b"id_262_01";
    assert!(client
        .drain()
        .iter()
        .any(|(_, body)| body.windows(sequence.len()).any(|b| b == sequence)));
    assert!(!below
        .drain()
        .iter()
        .any(|(_, body)| body.windows(sequence.len()).any(|b| b == sequence)));
    let before = tornado.transform();
    tornado.with_base_unit(|unit| {
        assert!(unit.buffs().check_buff(4225));
    });
    tornado
        .as_npc()
        .unwrap()
        .data()
        .unit
        .buffs
        .effects
        .iter_mut()
        .find(|buff| buff.template.id == 4225)
        .unwrap()
        .start_time_ms -= 4_000;
    for _ in 0..3 {
        ai::tick_all(&server, 100);
    }
    let after = tornado.transform();
    assert!((after.x - before.x).hypot(after.y - before.y) > 0.1);
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_allistair_runs_and_opens_the_warehouse_chest() {
    let (server, world) = fixture();
    let (_client, _player) = place_player(&server, 1, 514.0, 316.0, 166.0);
    let allistair = place_npc(&server, &world, 12111, 512.107, 315.263, 165.938);
    let position = allistair.transform();
    let npc = allistair.as_npc().unwrap();
    hooks::npc_control_effect(
        &server,
        &NpcControlEffect {
            id: 0,
            category_id: 5,
            param_int: 198,
            param_string: String::new(),
        },
        npc,
    );
    for _ in 0..40 {
        ai::tick_all(&server, 100);
    }
    assert!((allistair.transform().x - position.x).abs() > 1.0);
    let chest = place_doodad(&server, &world, 5600, 398.5, 325.1, 168.537);
    allistair.with_transform(|t| t.set_xyz(399.282, 325.742, 168.537));
    world.add_visible_object(&allistair, &server.view_ctx());
    assert_eq!(
        skill::unit_use_skill(&server, &allistair, 19504, &allistair),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_ne!(chest.as_doodad().unwrap().data().func_group_id, 14412);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_powder_keg_consumes_the_pack_then_breaks_the_wall() {
    let (server, world) = fixture();
    let (_client, player) = place_player(&server, 1, 519.932, 362.755, 169.029);
    let rock = place_doodad(&server, &world, 5280, 519.932, 365.755, 169.029);
    let objects = || {
        world
            .all_of_kind(ObjectKind::Doodad)
            .into_iter()
            .filter(|obj| obj.as_doodad().unwrap().data().template_id == 5951)
            .count()
    };
    let caster = WorldObject::from(&player);
    let inventory = server.systems.npcs.hooks.economy.get().unwrap();
    assert!(inventory.try_equip_new_backpack(
        &server,
        &player,
        task_type::SKILL_REAGENTS,
        25002,
        1
    ));
    assert_eq!(
        skill::unit_use_skill(&server, &caster, 18715, &caster),
        skill_result::SUCCESS
    );
    for _ in 0..40 {
        if super::hooks::backpack_slot_empty(&player) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(super::hooks::backpack_slot_empty(&player));
    assert_eq!(objects(), 1);
    assert_ne!(
        skill::unit_use_skill(&server, &caster, 18715, &caster),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(objects(), 0);
    assert_ne!(rock.as_doodad().unwrap().data().func_group_id, 13665);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_colossus_smashes_hit_in_front() {
    let (server, world) = fixture();
    for (index, id) in [18374, 18401].into_iter().enumerate() {
        let x = 597.0 + index as f32 * 30.0;
        let boss = place_npc(&server, &world, 11363, x, 423.4, 153.7);
        let (_client, front) = place_player(&server, index as u32 * 2 + 1, x, 427.0, 153.7);
        let (_client, back) = place_player(&server, index as u32 * 2 + 2, x, 419.8, 153.7);
        front.data().unit.hp = 10_000;
        back.data().unit.hp = 10_000;
        crate::systems::ai::npc::look_towards(
            &server,
            boss.as_npc().unwrap(),
            crate::systems::ai::npc::world_pos(&server, &WorldObject::from(&front)),
            4,
        );
        assert_eq!(
            skill::unit_use_skill(&server, &boss, id, &boss),
            skill_result::SUCCESS
        );
        for _ in 0..50 {
            if front.data().unit.hp < 10_000 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(front.data().unit.hp < 10_000, "smash {id} hits in front");
        assert_eq!(
            back.data().unit.hp,
            10_000,
            "smash {id} leaves the back safe"
        );
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_colossus_chases_inside_its_room() {
    let (server, world) = fixture();
    let (_client, player) = place_player(&server, 1, 617.74, 446.267, 151.099);
    let boss = place_npc(&server, &world, 11363, 614.4, 423.4, 153.7);
    let npc = boss.as_npc().unwrap();
    for _ in 0..3 {
        ai::tick_all(&server, 100);
    }
    hooks::engage(&server, npc, &WorldObject::from(&player));
    for _ in 0..5 {
        ai::tick_all(&server, 100);
    }
    assert!(
        boss.transform().y > 423.6,
        "the boss chases within the room"
    );
    assert!(npc.data().unit.is_in_battle);
    assert!(crate::systems::ai::npc::in_stonebrother_arena(
        crate::systems::ai::npc::world_pos(&server, &boss)
    ));
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_aggro_help_cannot_chain_to_the_floor_above() {
    let (server, world) = fixture();
    let (_client, player) = place_player(&server, 1, 410.0, 330.0, 164.0);
    let victim = WorldObject::from(&player);
    let leader = place_npc(&server, &world, 11358, 411.0, 330.0, 164.0);
    let ally = place_npc(&server, &world, 11110, 416.0, 330.0, 164.0);
    let upstairs = place_npc(&server, &world, 11110, 416.0, 330.0, 169.0);
    let has_aggro = |npc: &WorldObject| {
        npc.as_npc()
            .unwrap()
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&victim.obj_id())
    };
    ai::run_now(&server, leader.obj_id(), |context| {
        crate::systems::ai::behaviors::update_aggro_help(context, &victim, 200.0)
    })
    .unwrap();
    assert!(has_aggro(&ally), "the nearby ally still joins");
    assert!(!has_aggro(&upstairs), "the other floor stays idle");
}
