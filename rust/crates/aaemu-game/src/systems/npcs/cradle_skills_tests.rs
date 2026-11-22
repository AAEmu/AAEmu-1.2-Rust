//! Kroloal Cradle (`instance_cradle_of_destruction`, zone 264) mob skills and caster AI from
//! the tester follow-up of 2026-11-22 (items 2, 19, 20, 21 and 22).
//!
//! The Putrid Demon's Miasma (18431), Poison Belch (18451) and the Academy Apprentice's Toxic
//! Globe (18425) summon doodads (5115, 5151, 5929) whose `UseOriginSource` clouts carry the
//! payload. Element Fanatic Blangar and the Maddened Shaman are big monsters with ranged base
//! attacks. All tests need `AAEMU_COMPACT_DB`.

use std::sync::Arc;
use std::time::Duration;

use crate::server::GameServer;
use crate::systems::skills::{skill, types::skill_result};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::{now_ms, NpcRef, PlayerRef, WorldInstance, WorldObject};

use super::burntcastle_dungeon_tests::real_encounter_server;

const WORLD: &str = "instance_cradle_of_destruction";
const ZONE: u32 = 264;

/// The Putrid Demon of the corridor before the first boss room (11357).
const PUTRID_DEMON: u32 = 11357;
const ACADEMY_APPRENTICE: u32 = 11156;
const ELEMENT_FANATIC_BLANGAR: u32 = 11354;
const MADDENED_SHAMAN: u32 = 11370;

/// A spot in the corridor near the doubled Putrid Demon (X 713, Y 744, Z 173).
const AT: [f32; 3] = [700.0, 744.0, 173.0];

fn npc_at(server: &GameServer, world: &WorldInstance, template: u32, xyz: [f32; 3]) -> NpcRef {
    let npc =
        super::npc::manager::create(server, server.ids.object.get_next_id().unwrap(), template)
            .expect("Cradle NPC template");
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
    ch.unit.level = 47;
    ch.unit.hp = 100_000;
    ch.unit.max_hp = 100_000;
    ch.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
    ch.unit.transform.zone_id = ZONE;
    let player = spawn_player(server, &client, ch, 0x10_0000 + id);
    (client, player)
}

fn offset(xyz: [f32; 3], dx: f32) -> [f32; 3] {
    [xyz[0] + dx, xyz[1], xyz[2]]
}

fn hp(unit: &WorldObject) -> i32 {
    unit.with_unit(|u| u.unit().hp).unwrap_or(0)
}

fn has_buff(unit: &WorldObject, buff_id: u32) -> bool {
    unit.with_base_unit(|b| b.buffs().check_buff(buff_id))
}

/// Ticks the area triggers every 100 ms until `done` or `ms` elapsed.
async fn wait_for(server: &GameServer, ms: u64, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..ms / 100 {
        server.systems.npcs.area_triggers.tick(server);
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    done()
}

/// #2: Miasma's cloud (doodad 5115, clout 1139, Hostile, buff 3884 "Fart" then 3887) hits the
/// Putrid Demon's enemies and never the demon. With the faction-less doodad as the caster the
/// Hostile area took every unit inside, the demon included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_miasma_spares_the_putrid_demon() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_encounter_server(&db, WORLD, ZONE);
    let demon = WorldObject::from(&npc_at(&server, &world, PUTRID_DEMON, AT));
    let (_client, player) = player_at(&server, 1, offset(AT, 2.0));
    let victim = WorldObject::from(&player);
    assert_eq!(
        skill::unit_use_skill(&server, &demon, 18431, &demon),
        skill_result::SUCCESS
    );
    let hit = wait_for(&server, 5_000, || has_buff(&victim, 3884)).await;
    assert!(hit, "Miasma's cloud reaches the demon's victim");
    assert!(hp(&victim) < 100_000, "the 8 m burst damages the victim");
    let cloud = world
        .all_doodads()
        .into_iter()
        .find(|d| d.data().template_id == 5115)
        .expect("Miasma's cloud");
    let triggers = server.systems.npcs.area_triggers.attached_to(cloud.obj_id);
    assert_eq!(
        triggers[0].caster.as_ref().map(WorldObject::obj_id),
        Some(demon.obj_id()),
        "the demon is the cloud's origin source"
    );
    for _ in 0..5 {
        server.systems.npcs.area_triggers.tick(&server);
    }
    assert!(
        !has_buff(&demon, 3884),
        "the demon is not in its own Miasma"
    );
    assert!(!has_buff(&demon, 3887));
    assert_eq!(hp(&demon), demon.with_unit(|u| u.unit().max_hp).unwrap());
}

/// #19: Poison Belch (18451, plot 405) places doodad 5151 under the victim. Three seconds
/// later its clout 1151 (Hostile, no inside buff, damage effect 4065 scaled by the caster's
/// DPS) bursts. The clout effects without an inside buff were limited to beneficial buffs and
/// ran as the doodad, so the burst played and did nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_poison_belch_bursts_on_its_victim() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_encounter_server(&db, WORLD, ZONE);
    let demon = WorldObject::from(&npc_at(&server, &world, PUTRID_DEMON, AT));
    let (_client, player) = player_at(&server, 1, offset(AT, 4.0));
    let victim = WorldObject::from(&player);
    // A prop beside the demon must not take the burst away from the victim.
    let prop = super::doodad::manager::create(&server, &world, 0, 5033, None, true).unwrap();
    prop.data().transform.set_xyz(AT[0] - 1.0, AT[1], AT[2]);
    super::doodad::manager::spawn(&server, &world, &prop);
    assert_eq!(
        skill::unit_use_skill(&server, &demon, 18451, &victim),
        skill_result::SUCCESS
    );
    let hit = wait_for(&server, 5_000, || hp(&victim) < 100_000).await;
    assert!(hit, "the belch bursts on its victim");
    assert_eq!(hp(&demon), demon.with_unit(|u| u.unit().max_hp).unwrap());
}

/// #20: Toxic Globe (18425, plot 397) flies at a random enemy within 30 m and places doodad
/// 5929 under the units within 5 m of the impact. Its clout 1428 (Hostile, no inside buff,
/// damage effect 4166, twice the caster's DPS) had the same gap as Poison Belch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_toxic_globe_damages_its_target() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_encounter_server(&db, WORLD, ZONE);
    let apprentice = WorldObject::from(&npc_at(&server, &world, ACADEMY_APPRENTICE, AT));
    let (_client, player) = player_at(&server, 1, offset(AT, 10.0));
    let victim = WorldObject::from(&player);
    assert_eq!(
        skill::unit_use_skill(&server, &apprentice, 18425, &apprentice),
        skill_result::SUCCESS
    );
    let hit = wait_for(&server, 5_000, || hp(&victim) < 100_000).await;
    assert!(hit, "the globe's burst damages its target");
    assert_eq!(
        hp(&apprentice),
        apprentice.with_unit(|u| u.unit().max_hp).unwrap()
    );
}

/// One 100 ms AI tick; the victim is kept alive.
async fn tick(server: &GameServer, npc: &NpcRef, victim: &PlayerRef) {
    crate::systems::ai::manager::run_now(server, npc.obj_id, |ai| ai.tick(100, true));
    victim.data().unit.hp = 100_000;
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn distance(npc: &NpcRef, player: &PlayerRef) -> f32 {
    let a = npc.data().unit.transform;
    let b = player.data().unit.transform;
    (a.x - b.x).hypot(a.y - b.y)
}

/// Engages `template` with a victim `start` metres away for eight seconds. Returns the closest
/// and the final distance and whether one of `skills` was used.
async fn fight(template: u32, start: f32, skills: &[u32]) -> (f32, f32, bool) {
    let db = std::env::var("AAEMU_COMPACT_DB").unwrap();
    let (server, world) = real_encounter_server(&db, WORLD, ZONE);
    // GodMode: the victim takes no damage and the fight goes on.
    let mut config = server.systems.skills.config();
    config.god_mode = true;
    server.systems.skills.set_config(config);
    let npc = npc_at(&server, &world, template, AT);
    let (_client, player) = player_at(&server, 1, offset(AT, start));
    crate::systems::ai::hooks::spawn_positioned(&server, &npc);
    tick(&server, &npc, &player).await;
    crate::systems::ai::hooks::engage(&server, &npc, &WorldObject::from(&player));
    let mut closest = f32::MAX;
    for _ in 0..80 {
        tick(&server, &npc, &player).await;
        closest = closest.min(distance(&npc, &player));
    }
    let used = {
        let mut data = npc.data();
        skills
            .iter()
            .any(|&id| data.unit.cooldowns.check_cooldown(id, now_ms()))
    };
    (closest, distance(&npc, &player), used)
}

/// #21: Element Fanatic Blangar shoots from bow range (10431, 15 m, attack start scale 0.5).
/// His combat skills never set the chase range, so he closed to 0.5 m before every shot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_element_fanatic_blangar_keeps_his_bow_range() {
    if std::env::var("AAEMU_COMPACT_DB").is_err() {
        return;
    }
    let (closest, last, used) = fight(ELEMENT_FANATIC_BLANGAR, 14.0, &[10431, 18521, 18526]).await;
    assert!(closest > 6.0, "Blangar stays at range: closest {closest}");
    assert!(last < 9.0, "Blangar closes to his bow range: {last}");
    assert!(used, "Blangar shoots");
}

/// #22: the Maddened Shaman casts Flame Cluster (19981, 20 m) and her fire floor from range.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_maddened_shaman_casts_from_range() {
    if std::env::var("AAEMU_COMPACT_DB").is_err() {
        return;
    }
    let (closest, last, used) = fight(MADDENED_SHAMAN, 16.0, &[19981, 18504, 18552]).await;
    assert!(
        closest > 8.0,
        "the Shaman stays at range: closest {closest}"
    );
    assert!(
        last < 11.5,
        "the Shaman closes to her casting range: {last}"
    );
    assert!(used, "the Shaman casts");
}
