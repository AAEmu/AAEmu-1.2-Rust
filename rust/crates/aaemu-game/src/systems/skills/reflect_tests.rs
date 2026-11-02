//! Damage reflection (`buffs.reflection_*`): the `reflection_type_id` match, chance and
//! `reflection_target_ratio`, the ignored `reflection_ratio` (REFLECT-1), one reflection per hit
//! (REFLECT-2), no ping-pong between two reflectors, the ordinary death path of a reflected
//! kill, and the absence of item procs on the reflected hit (REFLECT-3).
//!
//! Real-data backing: 36 buffs carry `reflection_chance > 0` in the 1.2 compact database; their
//! tooltips name the hit type ("Melee Damage", "magic damage", "ranged") and the returned
//! percentage (386 "Reflects 50%"). `reflection_ratio` has no settled meaning and is not used.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::items::ItemProcTemplate;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, NpcRef, WorldObject};

use super::data::effects::BuffEffect;
use super::data::{BuffTemplate, EffectKind, EffectTemplate, SkillTemplate};
use super::effects::{EffectArgs, EffectSource};
use super::geometry::Target;
use super::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};

const OBSERVER: u32 = 0x100;
const ATTACKER: u32 = 0x201;
const VICTIM: u32 = 0x202;

/// `BuffTemplate` row of a reflector (the columns the 1.2 `buffs` table carries).
fn reflection_buff(
    id: u32,
    chance: i32,
    type_id: u32,
    ratio: i32,
    target_ratio: i32,
) -> BuffTemplate {
    BuffTemplate {
        id,
        kind: 1,
        duration: 60_000,
        stack_rule: 1,
        reflection_chance: chance,
        reflection_type_id: type_id,
        reflection_ratio: ratio,
        reflection_target_ratio: target_ratio,
        ..BuffTemplate::default()
    }
}

fn fixture(data: GameData) -> (Arc<GameServer>, TestClient, NpcRef, NpcRef) {
    let server = Arc::new(test_server_with(data));
    super::bind(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let observer = spawn_player(
        &server,
        &client,
        test_character(1, "Observer", 100.0, 100.0),
        OBSERVER,
    );
    client.conn.active_char = Some(observer);
    let world = server.world.world(0).expect("world");
    let mut attacker = Npc::new(1);
    attacker.unit.hp = 1000;
    attacker.unit.max_hp = 1000;
    attacker.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let attacker = Entity::new(ATTACKER, attacker);
    world.spawn(&attacker, &server.view_ctx());
    let mut victim = Npc::new(2);
    victim.unit.hp = 1000;
    victim.unit.max_hp = 1000;
    victim.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let victim = Entity::new(VICTIM, victim);
    world.spawn(&victim, &server.view_ctx());
    (server, client, attacker, victim)
}

fn args(caster: &WorldObject, target: &WorldObject) -> EffectArgs<'static> {
    EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(caster.obj_id()),
        target: Target::Obj(target.clone()),
        target_caster: SkillCastTarget::Unit(target.obj_id()),
        cast: CastAction::Skill {
            skill_id: 1,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    }
}

fn hit(server: &GameServer, attacker: &NpcRef, victim: &NpcRef, amount: i32, damage_type: i32) {
    let mut effect = super::tests::damage_effect(1, amount);
    effect.damage_type = damage_type;
    super::effects::combat::damage(
        server,
        &effect,
        &args(&WorldObject::from(attacker), &WorldObject::from(victim)),
        None,
    );
}

/// A reflected `SCUnitDamagedPacket`: `(buff_id, owner_obj_id, buff_index, caster, target,
/// damage)`.
fn reflect_packets(client: &mut TestClient) -> Vec<(u32, u32, u32, u32, u32, i32)> {
    let mut out = Vec::new();
    for (op, body) in client.drain() {
        if op != sc::SCUnitDamagedPacket {
            continue;
        }
        let mut s = PacketStream::from_bytes(body);
        if s.read_u8().unwrap() != 2 {
            continue; // CastAction::Buff only
        }
        let buff_id = s.read_u32().unwrap();
        let owner = s.read_bc().unwrap();
        let index = s.read_u32().unwrap();
        assert!(s.read_bool().unwrap());
        assert!(!s.read_bool().unwrap());
        assert_eq!(s.read_u8().unwrap(), 0, "SkillCasterUnit");
        let _caster_unit = s.read_bc().unwrap();
        let caster = s.read_bc().unwrap();
        let target = s.read_bc().unwrap();
        let _crime = s.read_u8().unwrap();
        let damage = s.read_pisc_w(3).unwrap()[0] as i32;
        out.push((buff_id, owner, index, caster, target, damage));
    }
    out
}

/// The first `SCUnitDeathPacket`: `(victim, killer)`.
fn death_packet(client: &mut TestClient) -> Option<(u32, u32)> {
    for (op, body) in client.drain() {
        if op != sc::SCUnitDeathPacket {
            continue;
        }
        let mut s = PacketStream::from_bytes(body);
        let victim = s.read_bc().unwrap();
        let _reason = s.read_u8().unwrap();
        let _wait = s.read_i32().unwrap();
        let _lost_exp = s.read_i32().unwrap();
        let _durability = s.read_u8().unwrap();
        let killer = s.read_bc().unwrap();
        return Some((victim, killer));
    }
    None
}

#[tokio::test]
async fn reflection_matches_the_hit_type_and_floors_the_target_ratio() {
    let mut data = GameData::empty();
    data.systems
        .skills
        .add_buff(reflection_buff(386, 100, 1, 100, 50));
    let (server, mut client, attacker, victim) = fixture(data);
    let victim_obj = WorldObject::from(&victim);
    super::buffs::add_buff_by_id(&server, &victim_obj, 386, &victim_obj, 0);
    let index = victim
        .data()
        .unit
        .buffs
        .effect_from_buff_id(386)
        .expect("buff")
        .index;

    // Melee 101: floor(101 * 50 / 100) = 50 goes back; the victim still takes the full 101.
    hit(&server, &attacker, &victim, 101, 1);
    assert_eq!(attacker.data().unit.hp, 950);
    assert_eq!(victim.data().unit.hp, 899);
    let packets = reflect_packets(&mut client);
    assert_eq!(packets.len(), 1);
    assert_eq!(
        packets[0],
        (386, VICTIM, index, VICTIM, ATTACKER, 50),
        "the reflected hit is cast by the reflecting buff"
    );

    // Magic does not match the melee buff's reflection_type_id.
    hit(&server, &attacker, &victim, 100, 2);
    assert_eq!(attacker.data().unit.hp, 950);
    assert_eq!(victim.data().unit.hp, 799);
    assert!(reflect_packets(&mut client).is_empty());
}

#[tokio::test]
async fn reflection_chance_zero_never_fires_and_one_hundred_always_does() {
    for (chance, attacker_hp) in [(0, 1000), (100, 950)] {
        let mut data = GameData::empty();
        data.systems
            .skills
            .add_buff(reflection_buff(900, chance, 1, 100, 50));
        let (server, _client, attacker, victim) = fixture(data);
        let victim_obj = WorldObject::from(&victim);
        super::buffs::add_buff_by_id(&server, &victim_obj, 900, &victim_obj, 0);
        hit(&server, &attacker, &victim, 100, 1);
        assert_eq!(attacker.data().unit.hp, attacker_hp, "chance {chance}");
    }
}

#[tokio::test]
async fn reflection_ignores_reflection_ratio_and_returns_the_target_ratio() {
    let mut data = GameData::empty();
    // 4407 Magnetic Shield (Reflect Melee Damage): ratio 10, target_ratio 100.
    data.systems
        .skills
        .add_buff(reflection_buff(4407, 100, 1, 10, 100));
    // 5646 Thorn Armor: ratio 100, target_ratio 6.
    data.systems
        .skills
        .add_buff(reflection_buff(5646, 100, 1, 100, 6));
    let (server, _client, attacker, victim) = fixture(data);
    let victim_obj = WorldObject::from(&victim);
    super::buffs::add_buff_by_id(&server, &victim_obj, 4407, &victim_obj, 0);
    // REFLECT-1: the reflector still takes the full hit; ratio 10 does not reduce it.
    hit(&server, &attacker, &victim, 101, 1);
    assert_eq!(victim.data().unit.hp, 899);
    assert_eq!(attacker.data().unit.hp, 899, "100% reflected, not 10%");

    super::buffs::remove_buff(&server, &victim_obj, 4407);
    super::buffs::add_buff_by_id(&server, &victim_obj, 5646, &victim_obj, 0);
    hit(&server, &attacker, &victim, 101, 1);
    assert_eq!(attacker.data().unit.hp, 893, "floor(101 * 6 / 100) = 6");
}

#[tokio::test]
async fn two_reflectors_do_not_ping_pong() {
    let mut data = GameData::empty();
    data.systems
        .skills
        .add_buff(reflection_buff(386, 100, 1, 100, 50));
    let (server, mut client, attacker, victim) = fixture(data);
    for who in [&attacker, &victim] {
        let obj = WorldObject::from(who);
        super::buffs::add_buff_by_id(&server, &obj, 386, &obj, 0);
    }
    hit(&server, &attacker, &victim, 100, 1);
    // The reflected 50 never re-enters the damage effect: the attacker keeps 950 (not 925).
    assert_eq!(attacker.data().unit.hp, 950);
    assert_eq!(victim.data().unit.hp, 900);
    assert_eq!(reflect_packets(&mut client).len(), 1);
}

#[tokio::test]
async fn reflection_kills_the_attacker_through_the_normal_death_path() {
    let mut data = GameData::empty();
    data.systems
        .skills
        .add_buff(reflection_buff(386, 100, 1, 100, 50));
    let (server, mut client, attacker, victim) = fixture(data);
    attacker.data().unit.hp = 10;
    let victim_obj = WorldObject::from(&victim);
    super::buffs::add_buff_by_id(&server, &victim_obj, 386, &victim_obj, 0);

    hit(&server, &attacker, &victim, 100, 1);
    assert_eq!(attacker.data().unit.hp, 0);
    assert!(attacker.data().unit.is_dead());
    assert!(attacker.data().dead_time_ms > 0, "Npc.DoDie ran");
    assert_eq!(death_packet(&mut client), Some((ATTACKER, VICTIM)));
    // The landed hit still hurts the victim.
    assert_eq!(victim.data().unit.hp, 900);
}

/// The real 1.2 catalogs: the 36 reflection buffs (type / chance / ratio columns) and the
/// `use_damage_amount` restore-mana triggers. Only heal and mana effects read the amount;
/// the `SpecialEffect` (Charge) rows have no formula (see `PORTING.md`).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_reflection_and_use_damage_amount_catalogs() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT id, reflection_chance, reflection_type_id, reflection_ratio, \
             reflection_target_ratio FROM buffs WHERE reflection_chance > 0 \
             ORDER BY reflection_type_id, id",
        )
        .unwrap();
    let rows: Vec<(u32, i32, u32, i32, i32)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (167, 100, 1, 100, 100),
            (386, 100, 1, 100, 50),
            (1035, 50, 1, 100, 15),
            (2049, 100, 1, 100, 50),
            (2923, 100, 1, 100, 50),
            (2924, 100, 1, 100, 60),
            (3564, 100, 1, 100, 100),
            (4407, 100, 1, 10, 100),
            (4516, 100, 1, 100, 30),
            (4667, 100, 1, 100, 20),
            (4780, 100, 1, 100, 100),
            (4998, 100, 1, 100, 50),
            (5531, 100, 1, 100, 100),
            (5646, 100, 1, 100, 6),
            (5813, 100, 1, 100, 100),
            (5857, 100, 1, 100, 100),
            (7656, 100, 1, 100, 70),
            (177, 47, 2, 100, 100),
            (3563, 100, 2, 100, 100),
            (4366, 100, 2, 10, 100),
            (4705, 100, 2, 100, 20),
            (4782, 100, 2, 100, 100),
            (4999, 100, 2, 100, 50),
            (5532, 100, 2, 100, 100),
            (5814, 100, 2, 100, 100),
            (5859, 100, 2, 100, 100),
            (5533, 100, 3, 100, 100),
            (4408, 100, 4, 10, 100),
            (4706, 100, 4, 100, 20),
            (4781, 100, 4, 100, 100),
            (5000, 100, 4, 100, 50),
            (5404, 100, 4, 100, 100),
            (5534, 100, 4, 100, 100),
            (5815, 100, 4, 100, 100),
            (5858, 100, 4, 100, 100),
            (7132, 100, 4, 50, 50),
        ]
    );

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM buff_triggers bt JOIN effects e ON e.id = bt.effect_id \
             WHERE bt.use_damage_amount = 't' AND e.actual_type = 'RestoreManaEffect'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 95, "RestoreManaEffect use_damage_amount triggers");
    let mut stmt = conn
        .prepare(
            "SELECT bt.buff_id, bt.event_id, r.fixed_min, r.fixed_max, r.percent \
             FROM buff_triggers bt JOIN effects e ON e.id = bt.effect_id \
             JOIN restore_mana_effects r ON e.actual_type = 'RestoreManaEffect' \
             AND r.id = e.actual_id WHERE bt.use_damage_amount = 't' ORDER BY bt.buff_id",
        )
        .unwrap();
    let rows: Vec<(u32, i64, i32, i32, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 95);
    assert!(rows.contains(&(753, 4, 30, 30, "t".to_owned())));
    assert!(rows.contains(&(806, 3, 20, 20, "t".to_owned())));
    assert!(rows.iter().all(|r| r.2 > 0 && r.2 <= r.3 && r.4 == "t"));
    // 6710 is the only ranged source (fixed 10-20).
    assert_eq!(
        rows.iter().filter(|r| r.2 != r.3).collect::<Vec<_>>(),
        [&(6710, 4, 10, 20, "t".to_owned())]
    );

    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT e.actual_type FROM buff_triggers bt \
             JOIN effects e ON e.id = bt.effect_id WHERE bt.use_damage_amount = 't' \
             ORDER BY e.actual_type",
        )
        .unwrap();
    let types: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(types, ["HealEffect", "RestoreManaEffect", "SpecialEffect"]);
}

#[tokio::test]
async fn reflected_damage_rolls_no_item_procs() {
    const PROC: u32 = 7;
    const PROC_SKILL: u32 = 800;
    const PROC_BUFF: u32 = 700;
    let mut data = GameData::empty();
    data.systems
        .skills
        .add_buff(reflection_buff(386, 100, 1, 100, 50));
    data.systems.skills.add_buff(BuffTemplate {
        id: PROC_BUFF,
        kind: 1,
        duration: 60_000,
        ..BuffTemplate::default()
    });
    data.systems.skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: 1800,
            buff_id: Some(PROC_BUFF),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        0,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: PROC_SKILL,
        target_type: 0, // SelfTarget
        effects: vec![super::tests::skill_effect(
            1800,
            EffectKind::BuffEffect,
            1800,
        )],
        ..super::tests::skill_template(PROC_SKILL)
    });
    data.items.item_procs.insert(
        PROC,
        ItemProcTemplate {
            id: PROC,
            skill_id: PROC_SKILL,
            chance_kind: crate::systems::economy::procs::chance_kind::HIT_ANY,
            chance_rate: 100,
            cooldown_sec: 0,
            ..Default::default()
        },
    );
    let (server, mut client, attacker, victim) = fixture(data);
    // The victim's proc rolls on damage the victim deals. The reflected hit is damage the
    // victim "deals", so piping it through the damage effect would fire it.
    victim
        .data()
        .unit
        .item_procs
        .set_active(&server.game_data.items, [PROC]);
    let victim_obj = WorldObject::from(&victim);
    super::buffs::add_buff_by_id(&server, &victim_obj, 386, &victim_obj, 0);
    client.drain();

    hit(&server, &attacker, &victim, 100, 1);
    assert_eq!(attacker.data().unit.hp, 950, "the reflection happened");
    assert!(
        !victim.data().unit.buffs.check_buff(PROC_BUFF),
        "the reflected hit must not roll the reflector's item procs"
    );
    assert!(
        !client.drain_ops().contains(&sc::SCSkillFiredPacket),
        "no proc skill was cast"
    );
}
