//! Stealth: the server predicate uses the 1.2 `buffs.stealth` column (not buff tag 100), and
//! `skills.keep_stealth` keeps the caster's stealth through the skill's own cancel loop.

use crate::data::GameData;
use crate::systems::skills::data::effects::{EffectTemplate, SpecialEffect};
use crate::systems::skills::data::{BuffTemplate, EffectKind, SkillTemplate};
use crate::systems::skills::handlers::start_skill;
use crate::systems::skills::types::SkillCastTarget;
use crate::world::WorldObject;

use super::{cast_packet, data, fixture_with, skill_effect, skill_template, PLAYER_OBJ};

const STEALTH_BUFF: u32 = 50_100;
const KEEP_SKILL: u32 = 10_100;
const PLAIN_SKILL: u32 = 10_101;
const CHAIN_EFFECT: u32 = 9_100;
const CHAINED_SKILL: u32 = 10_102;
const CHAIN_SKILL: u32 = 10_103;

fn stealth_buff(id: u32) -> BuffTemplate {
    BuffTemplate {
        id,
        kind: 1,
        duration: 60_000,
        stack_rule: 1,
        stealth: true,
        remove_on_start_skill: true,
        remove_on_use_skill: true,
        ..BuffTemplate::default()
    }
}

/// A self-targeted skill with no effects; `cancel_ongoing_buffs` and `keep_stealth` are the
/// two flags under test.
fn flag_skill(id: u32, cancel_ongoing_buffs: bool, keep_stealth: bool) -> SkillTemplate {
    let mut t = skill_template(id);
    t.target_type = 0; // Self
    t.damage_type_id = 0;
    t.cancel_ongoing_buffs = cancel_ongoing_buffs;
    t.keep_stealth = keep_stealth;
    t
}

fn stealth_data() -> GameData {
    let mut d = data();
    let s = &mut d.systems.skills;
    s.add_buff(stealth_buff(STEALTH_BUFF));
    s.add_skill(flag_skill(KEEP_SKILL, true, true));
    s.add_skill(flag_skill(PLAIN_SKILL, true, false));
    d
}

#[tokio::test]
async fn keep_stealth_skill_does_not_break_stealth_but_a_plain_skill_does() {
    let mut f = fixture_with(stealth_data());
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    crate::systems::skills::buffs::add_buff_by_id(&server, &me, STEALTH_BUFF, &me, 0);
    assert!(me.with_base_unit(|b| b.buffs().check_buff(STEALTH_BUFF)));
    assert!(crate::systems::skills::buffs::has_stealth(&server, &me));

    // `skills.keep_stealth` = 't': the skill's own cancel loop must skip the stealth buff.
    start_skill(
        &server,
        &mut f.client.conn,
        cast_packet(KEEP_SKILL, SkillCastTarget::Unit(PLAYER_OBJ)),
    );
    assert!(
        me.with_base_unit(|b| b.buffs().check_buff(STEALTH_BUFF)),
        "a keep_stealth skill removed the stealth buff"
    );
    assert!(crate::systems::skills::buffs::has_stealth(&server, &me));

    // The same skill without keep_stealth still runs the generic remove_on_start_skill.
    start_skill(
        &server,
        &mut f.client.conn,
        cast_packet(PLAIN_SKILL, SkillCastTarget::Unit(PLAYER_OBJ)),
    );
    assert!(
        !me.with_base_unit(|b| b.buffs().check_buff(STEALTH_BUFF)),
        "a plain cancel_ongoing_buffs skill must break stealth"
    );
}

#[tokio::test]
async fn keep_stealth_skill_keeps_stealth_through_its_skill_use_chain() {
    let mut d = stealth_data();
    {
        let s = &mut d.systems.skills;
        s.add_effect(
            EffectKind::SpecialEffect,
            EffectTemplate::Special(SpecialEffect {
                id: CHAIN_EFFECT,
                special_effect_type_id: 33, // SkillUse
                value1: CHAINED_SKILL as i32,
                value2: 0,
                value3: 0,
                value4: 0,
            }),
            CHAIN_EFFECT,
        );
        // The chained skill is stealth-neutral (no cancel, no keep): only the outer skill's
        // keep_stealth can save the buff, through the SkillUse `UseSkill` trigger.
        s.add_skill(flag_skill(CHAINED_SKILL, false, false));
        let mut chain = flag_skill(CHAIN_SKILL, false, true);
        chain.effects.push(skill_effect(
            CHAIN_EFFECT,
            EffectKind::SpecialEffect,
            CHAIN_EFFECT,
        ));
        s.add_skill(chain);
    }
    let mut f = fixture_with(d);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    crate::systems::skills::buffs::add_buff_by_id(&server, &me, STEALTH_BUFF, &me, 0);

    start_skill(
        &server,
        &mut f.client.conn,
        cast_packet(CHAIN_SKILL, SkillCastTarget::Unit(PLAYER_OBJ)),
    );
    assert!(
        me.with_base_unit(|b| b.buffs().check_buff(STEALTH_BUFF)),
        "the SkillUse chain of a keep_stealth skill removed the stealth buff"
    );
}

/// Lists the real 1.2 stealth buffs and keep_stealth skills: the predicate must use the
/// `stealth` column, which differs from tag 100 in 40 rows.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn real_compact_db_stealth_column_and_keep_stealth_skills() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let ids = |sql: &str| -> Vec<u32> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };

    let stealth = ids("SELECT id FROM buffs WHERE stealth='t' ORDER BY id");
    assert_eq!(stealth.len(), 71);
    let tag100: std::collections::BTreeSet<u32> =
        ids("SELECT buff_id FROM tagged_buffs WHERE tag_id=100")
            .into_iter()
            .collect();
    assert_eq!(tag100.len(), 51);

    let stealth_buffs: std::collections::BTreeSet<u32> = stealth.iter().copied().collect();
    let missed: Vec<u32> = stealth_buffs.difference(&tag100).copied().collect();
    assert_eq!(
        missed,
        vec![
            255, 719, 1078, 1080, 1371, 1432, 1443, 1502, 1514, 1573, 2284, 2341, 2345, 2486, 2554,
            2979, 3155, 3527, 3594, 3752, 3772, 3866, 4099, 4566, 4631, 4648, 5205, 6058, 6474,
            7003,
        ]
    );
    let false_positives: Vec<u32> = tag100.difference(&stealth_buffs).copied().collect();
    assert_eq!(
        false_positives,
        vec![557, 1678, 1899, 1941, 1966, 3637, 5901, 6260, 6380, 7164]
    );

    let keep: Vec<u32> = ids("SELECT id FROM skills WHERE keep_stealth='t' ORDER BY id");
    assert_eq!(
        keep,
        vec![
            10082, 10496, 10693, 11210, 12149, 12455, 12474, 12522, 12562, 13202, 13203, 13204,
            13322, 13900, 14378, 14379, 14418, 15673, 15972, 17401, 17402, 17403, 17562, 17568,
            17625, 17971, 19054, 20090, 20731, 20969, 20990, 20992, 23749,
        ]
    );
    let cancelling: i64 = conn
        .query_row(
            "SELECT count(*) FROM skills WHERE keep_stealth='t' AND cancel_ongoing_buffs='t'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cancelling, 30);

    // Detection inputs: the two formula rows, the columns the client reads and the attribute
    // the predicate scales the range with (`unit attribute 94`, `DetectStealthRangeMul`).
    let detect: Vec<u32> = ids("SELECT id FROM buffs WHERE detect_stealth='t' ORDER BY id");
    assert_eq!(detect, vec![1496, 1503, 1574, 2232, 2233, 2284]);
    let anti: Vec<u32> = ids("SELECT id FROM buffs WHERE anti_stealth='t' ORDER BY id");
    assert_eq!(anti, vec![5212]);
    let front: String = conn
        .query_row("SELECT formula FROM formulas WHERE id=4", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        front,
        "min(40, 12 * (1 + (source_level - target_level) / 50) / (1 + stealth_level / 50))"
    );
    let back: String = conn
        .query_row("SELECT formula FROM formulas WHERE id=10", [], |r| r.get(0))
        .unwrap();
    assert_eq!(back, "0.5");
    let attr94: i64 = conn
        .query_row(
            "SELECT count(*) FROM unit_modifiers WHERE unit_attribute_id=94",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attr94, 21);
}
