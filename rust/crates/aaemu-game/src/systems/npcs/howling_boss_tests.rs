//! Greater Howling Abyss boss follow-ups (tester report after PR #157): Calleil's lightning
//! elementals, Musperosa's orbs, lasso and spirit, Godfrey's skills and hazard combat logs.

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

fn tick_ai(server: &GameServer, npc: &NpcRef) {
    crate::systems::ai::manager::run_now(server, npc.obj_id, |ai| ai.tick(100, true));
}

/// #10: the lightning summons (21116) join Calleil's fight. Without the boss's target
/// their idle skill 21114 removes every 12912 within 25 m on the first AI tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_calleil_lightning_elemental_joins_the_fight() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = npc_at(&server, &world, 12759, [322.4, 500.4, 170.8]);
    let (_client, player) = player_at(&server, 1, [322.4, 514.0, 170.8]);
    let victim = WorldObject::from(&player);
    crate::systems::ai::npc::set_current_aggro_target(&boss, Some(&victim));
    let caster = WorldObject::from(&boss);
    assert_eq!(
        skill::unit_use_skill(&server, &caster, 21116, &caster),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let elemental = world
        .all_npcs()
        .into_iter()
        .find(|npc| npc.data().unit.template_id == 12912)
        .expect("Calleil's lightning elemental");
    assert!(elemental
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&player.obj_id));
    for _ in 0..10 {
        tick_ai(&server, &elemental);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        world.npc(elemental.obj_id).is_some(),
        "the elemental does not vanish through its idle skill"
    );
}

/// #16: the white orbs' Unknown Power (5328) stacks persist for their 60 s once the
/// victim leaves the orb. Only the orb's inside-buff stack leaves with the area.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_unknown_power_outlasts_the_petrifying_orb() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = WorldObject::from(npc_at(&server, &world, 12758, [463.4, 679.2, 167.0]));
    let (_client, player) = player_at(&server, 1, [467.2, 679.2, 167.0]);
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21055, &boss),
        skill_result::SUCCESS
    );
    let mut orb = None;
    for _ in 0..60 {
        orb = world
            .all_doodads()
            .into_iter()
            .find(|doodad| doodad.data().template_id == 6261);
        if orb.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let orb = orb.expect("Musperosa's petrifying orb");
    let at = orb.data().transform;
    let move_to = |x: f32| {
        player.data().unit.transform.set_xyz(x, at.y, at.z);
        world.add_visible_object(&player, &server.view_ctx());
    };
    move_to(at.x);
    let stacks = || player.data().unit.buffs.buff_count_by_id(5328);
    for _ in 0..10 {
        server.systems.npcs.area_triggers.tick(&server);
        if stacks() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(stacks() >= 3, "each orb pulse adds a stack");
    // Step out before the orb's explosion.
    move_to(at.x + 10.0);
    server.systems.npcs.area_triggers.tick(&server);
    assert!(stacks() >= 2, "only the inside stack leaves with the area");
    for _ in 0..30 {
        server.systems.npcs.area_triggers.tick(&server);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(world.doodad(orb.obj_id).is_none(), "the orb finished");
    assert!(
        stacks() >= 2,
        "Unknown Power stays after the orb ends, {} stacks",
        stacks()
    );
}

/// #12: Trieste's secret weapon detonation (21126) stuns the frenzied spirit and removes
/// its Frenzy (5580, tag 754).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_secret_weapon_stuns_the_frenzied_spirit() {
    use super::gimmick::manager::{spawn_from_effect, SpawnGimmickParams};

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let spirit = npc_at(&server, &world, 12786, [463.4, 679.2, 167.0]);
    let spirit_obj = WorldObject::from(&spirit);
    crate::systems::skills::buffs::add_buff_by_id(&server, &spirit_obj, 5580, &spirit_obj, 0);
    assert!(spirit.data().unit.buffs.check_buff(5580));
    let (_client, player) = player_at(&server, 1, [463.4, 668.0, 167.0]);
    let bolt = spawn_from_effect(
        &server,
        &world,
        &SpawnGimmickParams {
            gimmick_id: 66,
            scale: 1.0,
            ..Default::default()
        },
        &WorldObject::from(&player),
    )
    .expect("Trieste bolt");
    bolt.data().unit.transform.set_xyz(463.4, 677.0, 168.0);
    let hooks = server.systems.npcs.hooks.skills.get().expect("skill hooks");
    assert!(hooks.use_gimmick_collision_skill(
        &server,
        &WorldObject::from(&bolt),
        &spirit_obj,
        21126
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let data = spirit.data();
    assert!(data.unit.buffs.check_buff(5366), "the spirit is stunned");
    assert!(!data.unit.buffs.check_buff(5580), "the bolt removes Frenzy");
}

/// Places a combat boss with AI at `xyz` with `percent` of its health.
fn boss_with_ai(
    server: &GameServer,
    world: &WorldInstance,
    template: u32,
    xyz: [f32; 3],
    percent: i32,
) -> NpcRef {
    let boss = npc_at(server, world, template, xyz);
    crate::systems::ai::hooks::spawn_positioned(server, &boss);
    let mut data = boss.data();
    data.unit.hp = data.unit.max_hp / 100 * percent;
    drop(data);
    boss
}

/// The skills a boss started, read from the cooldowns its casts set.
fn cast_skills(boss: &NpcRef, ids: &[u32]) -> Vec<u32> {
    let data = boss.data();
    ids.iter()
        .copied()
        .filter(|id| data.unit.cooldowns.cooldowns.contains_key(id))
        .collect()
}

/// #21: below 44% health Musperosa's AI selects Lasso (21066). It needs no target range,
/// and the pull buff (5338, Leap controller 6999) reaches her combat target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_musperosa_ai_selects_lasso_below_44_percent() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = boss_with_ai(&server, &world, 12758, [463.4, 679.2, 167.0], 40);
    let (_client, player) = player_at(&server, 1, [463.4, 670.0, 167.0]);
    let now = crate::world::now_ms();
    {
        // Leave the lasso as the only skill off cooldown.
        let mut data = boss.data();
        for id in [
            21038, 21040, 21042, 21043, 21044, 21046, 21047, 21049, 21053, 21054, 21055, 21056,
            21057, 21058, 21615,
        ] {
            data.unit.cooldowns.add_cooldown(id, 120_000, now);
        }
    }
    crate::systems::ai::hooks::engage(&server, &boss, &WorldObject::from(&player));
    for _ in 0..40 {
        tick_ai(&server, &boss);
        let lasso_caster = player
            .data()
            .unit
            .buffs
            .effects
            .iter()
            .find(|e| e.template.id == 5338)
            .map(|e| crate::systems::skills::buffs::buff_skill_caster(e).obj_id());
        if let Some(caster) = lasso_caster {
            // The client's own Leap (controller 6999) pulls toward the buff's skill caster.
            assert_eq!(caster, boss.obj_id);
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "Musperosa never lassoed her target: cast {:?}",
        cast_skills(&boss, &[21066])
    );
}

/// #19: Godfrey's authored combat skills start below 97% health whether his target
/// damaged him or reached him through proximity aggro.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_godfrey_uses_combat_skills_on_proximity_aggro() {
    const COMBAT: [u32; 15] = [
        21143, 21144, 21145, 21146, 21147, 21152, 21158, 21159, 21165, 21167, 21180, 21181, 21183,
        21201, 21202,
    ];
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = boss_with_ai(&server, &world, 12761, [531.1, 478.3, 188.0], 90);
    let (_client, player) = player_at(&server, 1, [533.0, 478.3, 188.0]);
    for _ in 0..60 {
        tick_ai(&server, &boss);
        if !cast_skills(&boss, &COMBAT).is_empty() {
            assert!(boss
                .data()
                .unit
                .skill_state
                .aggro_table
                .contains_key(&player.obj_id));
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "Godfrey used no combat skill: in battle {}, aggro {:?}",
        boss.data().unit.is_in_battle,
        boss.data()
            .unit
            .skill_state
            .aggro_table
            .keys()
            .collect::<Vec<_>>()
    );
}

/// One `SCUnitDamagedPacket` seen by a client: (cast action kind, its skill / buff id,
/// skill caster kind, skill caster id, attacker id, target id).
type DamageLog = (u8, u32, u8, u32, u32, u32);

fn bc(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

fn parse_damage(body: &[u8]) -> Option<DamageLog> {
    let kind = *body.first()?;
    let id = u32::from_le_bytes(body.get(1..5)?.try_into().ok()?);
    let o = match kind {
        0 => 7,
        1 => 15,
        2 => 12,
        4 => 8,
        _ => return None,
    };
    let caster = body.get(o..o + 10)?;
    Some((
        kind,
        id,
        caster[0],
        bc(&caster[1..4]),
        bc(&caster[4..7]),
        bc(&caster[7..10]),
    ))
}

/// The damage packets in the frames a client received since the last drain, inside
/// `CompressedGamePackets` batches too.
fn drain_damage(client: &mut TestClient) -> Vec<DamageLog> {
    let op = crate::opcodes::sc::SCUnitDamagedPacket;
    let mut seen = Vec::new();
    for frame in client.drain_frames() {
        let bytes = &frame.bytes[..];
        match bytes.get(1) {
            Some(1) if u16::from_le_bytes([bytes[4], bytes[5]]) == op => {
                seen.extend(parse_damage(&bytes[6..]));
            }
            Some(4) => {
                let Some(payload) = crate::systems::skills::packets::inflate_stored(&bytes[4..])
                else {
                    continue;
                };
                let marker = [0, 0, op.to_le_bytes()[0], op.to_le_bytes()[1]];
                for i in 0..payload.len().saturating_sub(4) {
                    if payload[i..i + 4] == marker {
                        seen.extend(parse_damage(&payload[i + 4..]));
                    }
                }
            }
            _ => {}
        }
    }
    seen
}

/// #11: Calleil's Hellfire explosion (21082, doodad 6273, clout 1542) reports its damage
/// to the victim's combat log, attributed to Calleil. The r208022 client drops a skill cast
/// action whose timeline it does not know, so the clout casts as its doodad (kind 4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_hellfire_damage_reaches_the_combat_log() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = npc_at(&server, &world, 12759, [322.4, 500.4, 170.8]);
    let (mut client, player) = player_at(&server, 1, [323.0, 501.0, 170.6]);
    let caster = WorldObject::from(&boss);
    client.drain_frames();
    assert_eq!(
        skill::unit_use_skill(&server, &caster, 21082, &caster),
        skill_result::SUCCESS
    );
    let mut logs = Vec::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        logs.extend(drain_damage(&mut client));
    }
    assert!(player.data().unit.hp < 10_000, "the explosion hits");
    let hits: Vec<_> = logs.iter().filter(|log| log.5 == player.obj_id).collect();
    assert!(!hits.is_empty(), "the victim receives the damage packet");
    assert!(
        hits.iter()
            .all(|log| (log.0, log.1, log.4) == (4, 6273, boss.obj_id)),
        "{hits:?}"
    );
}

/// PR #165 review: crossing the orb's edge again must not take a pulse stack. The second
/// entry adds no inside stack while pulse stacks remain, so its exit removes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_orb_reentry_keeps_the_pulse_stacks() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let boss = WorldObject::from(npc_at(&server, &world, 12758, [463.4, 679.2, 167.0]));
    let (_client, player) = player_at(&server, 1, [467.2, 679.2, 167.0]);
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21055, &boss),
        skill_result::SUCCESS
    );
    let mut orb = None;
    for _ in 0..60 {
        orb = world
            .all_doodads()
            .into_iter()
            .find(|doodad| doodad.data().template_id == 6261);
        if orb.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let orb = orb.expect("Musperosa's petrifying orb");
    let at = orb.data().transform;
    let move_to = |x: f32| {
        player.data().unit.transform.set_xyz(x, at.y, at.z);
        world.add_visible_object(&player, &server.view_ctx());
        server.systems.npcs.area_triggers.tick(&server);
    };
    let stacks = || player.data().unit.buffs.buff_count_by_id(5328);
    move_to(at.x);
    for _ in 0..10 {
        if stacks() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        server.systems.npcs.area_triggers.tick(&server);
    }
    assert!(stacks() >= 3, "each orb pulse adds a stack");
    move_to(at.x + 10.0);
    let pulse_stacks = stacks();
    assert!(
        pulse_stacks >= 2,
        "the first exit takes only its inside stack"
    );
    // Cross the edge again: in, then straight back out.
    move_to(at.x);
    let inside = stacks();
    move_to(at.x + 10.0);
    assert_eq!(stacks(), inside, "the second exit removes no pulse stack");
    assert!(stacks() >= pulse_stacks);
    for _ in 0..30 {
        server.systems.npcs.area_triggers.tick(&server);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(world.doodad(orb.obj_id).is_none(), "the orb finished");
    assert!(stacks() >= pulse_stacks, "the pulse stacks keep their 60 s");
}
