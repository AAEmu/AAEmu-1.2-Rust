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

/// The Academy Test Subject (11284, AI param 1836) and the Faceless Butchers it summons are
/// authored to keep coming while it fights. Its 18409 (cooldown 30 s, skillDelay 8, plot 398)
/// summons two butchers (SpawnEffect 1539 / 1540, 4 s and 8 s into the plot) with no life time
/// and no despawn on the creator's death; a butcher's own InIdle skill 20066 (KillNpcWithout-
/// Corpse 11160, radius 0, vanish) removes it when its fight ends. In a 15 s fight one subject
/// therefore summons exactly two, and the two vanish once their victim is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_test_subject_summons_two_butchers_per_cooldown() {
    use crate::systems::skills::{combat, types::KillReason};
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_encounter_server(&db, WORLD, ZONE);
    let mut config = server.systems.skills.config();
    config.god_mode = true;
    server.systems.skills.set_config(config);
    let subject = npc_at(&server, &world, 11284, AT);
    let (_client, player) = player_at(&server, 1, offset(AT, 3.0));
    let victim = WorldObject::from(&player);
    crate::systems::ai::hooks::spawn_positioned(&server, &subject);
    let butchers = || {
        world
            .all_npcs()
            .into_iter()
            .filter(|n| n.data().unit.template_id == 11160)
            .collect::<Vec<_>>()
    };
    let tick_all = || {
        for npc in world.all_npcs() {
            crate::systems::ai::manager::run_now(&server, npc.obj_id, |ai| ai.tick(100, true));
        }
    };
    tick_all();
    crate::systems::ai::hooks::engage(&server, &subject, &victim);
    let mut most = 0;
    for _ in 0..150 {
        tick_all();
        player.data().unit.hp = 100_000;
        most = most.max(butchers().len());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        subject
            .data()
            .unit
            .cooldowns
            .check_cooldown(18409, now_ms()),
        "18409 is on its 30 s cooldown"
    );
    assert_eq!(most, 2, "one 18409 in 15 s summons two butchers");
    let summoned = butchers();
    assert_eq!(summoned.len(), 2);
    for b in &summoned {
        assert_eq!(
            b.data().current_aggro_target,
            player.obj_id,
            "a butcher takes its summoner's victim"
        );
    }
    // The victim dies: the butchers' fight ends and their idle skill removes them.
    let hp = player.data().unit.hp;
    let mut config = server.systems.skills.config();
    config.god_mode = false;
    server.systems.skills.set_config(config);
    combat::reduce_current_hp(&server, &victim, None, hp, KillReason::Damage);
    for _ in 0..60 {
        tick_all();
        if butchers().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(butchers().is_empty(), "20066 vanishes the idle butchers");
}

/// The Putrid Demon by the Academy room (11357 at (813.2, 840.2), yaw 165.9) is as aggressive
/// as the Academy pack (aggression, `attack_start_range_scale` 0.5, `sight_range_scale` 0.8,
/// faction 115, AI param 1846 alertToAttack). `CheckAggression` / `CheckAlert` only notice a
/// unit in the front half (`sight_fov_scale` 1.0) within 5 / 12 m, or one within 1.2 / 1.6 m
/// all round. In the live pass the player stood 4.3 m east of it, 104 degrees off its facing,
/// for 40 s without aggro; the same distance in front of it pulls it at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_putrid_demon_notices_a_player_in_front_or_close() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_encounter_server(&db, WORLD, ZONE);
    let mut config = server.systems.skills.config();
    config.god_mode = true;
    server.systems.skills.set_config(config);
    let spot = [813.2, 840.2, 192.9];
    let demon = npc_at(&server, &world, PUTRID_DEMON, spot);
    demon.data().unit.transform.rot_z = 165.9_f32.to_radians();
    let (_client, player) = player_at(&server, 1, offset(spot, 4.3));
    crate::systems::ai::hooks::spawn_positioned(&server, &demon);
    let aggro = |demon: &NpcRef| !demon.data().unit.skill_state.aggro_table.is_empty();
    for _ in 0..40 {
        tick(&server, &demon, &player).await;
    }
    assert!(!aggro(&demon), "4.3 m east, behind its side: no aggro");
    // 4.3 m along its facing (90 + 165.9 degrees from the x axis).
    let facing = (90.0_f32 + 165.9).to_radians();
    player.data().unit.transform.set_xyz(
        spot[0] + 4.3 * facing.cos(),
        spot[1] + 4.3 * facing.sin(),
        spot[2],
    );
    let mut noticed = false;
    for _ in 0..10 {
        tick(&server, &demon, &player).await;
        if aggro(&demon) {
            noticed = true;
            break;
        }
    }
    assert!(noticed, "4.3 m in front: aggro");
}

/// The `(x, y, z)` offsets of the `SCKnockBackUnitPacket`s a client received since the last
/// drain.
fn knock_backs(client: &mut TestClient) -> Vec<[f32; 3]> {
    let f = |b: &[u8], i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    client
        .drain()
        .into_iter()
        .filter(|(op, body)| *op == crate::opcodes::sc::SCKnockBackUnitPacket && body.len() >= 15)
        .map(|(_, b)| [f(&b, 3), f(&b, 7), f(&b, 11)])
        .collect()
}

/// S27: the Tooth of Appetite's charge (18462, KnockBack value1 1000) sends its player victim
/// an `SCKnockBackUnitPacket`, cut short by a wall behind the victim. Player knockbacks of
/// other skills stay with the client.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cradle_tooth_charge_knocks_the_player_back_to_the_wall() {
    use crate::systems::ai::collision::flow_tests::{add_wall_boxes, enable_collision};
    use aaemu_physics::math::Vec3;

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = real_encounter_server(&db, WORLD, ZONE);
    enable_collision(&server, AT[2]);
    let tooth = WorldObject::from(&npc_at(&server, &world, 11365, AT));
    let at = offset(AT, 3.0);
    let (mut client, player) = player_at(&server, 1, at);
    let victim = WorldObject::from(&player);
    // A 1 m wall cube whose near face is 0.5 m behind the victim, or none.
    let wall = [(0xC1A, Vec3::new(at[0] + 1.0, at[1], at[2] + 0.5), 0.5)];
    for walled in [false, true] {
        add_wall_boxes(&server, if walled { &wall } else { &[] });
        player.data().unit.transform.set_xyz(at[0], at[1], at[2]);
        tooth.with_unit(|u| {
            u.unit_mut().cooldowns = Default::default();
            u.unit_mut().global_cooldown_ms = 0;
        });
        client.drain();
        assert_eq!(
            skill::unit_use_skill(&server, &tooth, 18462, &victim),
            skill_result::SUCCESS
        );
        let mut seen = Vec::new();
        for _ in 0..30 {
            seen.extend(knock_backs(&mut client));
            if !seen.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let [x, y, z] = *seen.first().expect("the charge knocks the player back");
        assert!(y.abs() < 0.01 && z.abs() < 0.01, "{seen:?}");
        if walled {
            assert!(
                (0.0..0.55).contains(&x),
                "the wall stops the knockback: {seen:?}"
            );
        } else {
            assert!(
                (0.85..=1.0).contains(&x),
                "a free 1 m knockback (less the clip margin): {seen:?}"
            );
        }
        player.data().unit.hp = 100_000;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
