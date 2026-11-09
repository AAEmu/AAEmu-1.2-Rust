//! Port of AAEmu.UnitTests/Game/Models/Game/Skills/SkillTests.cs
//!
//! `Skill` is `SkillRun` (shared as `SkillRef = Arc<SkillRun>`): `Id`, `Template` and `Level`
//! are fixed at construction, the per-use members (`TlId`, `Cancelled`, `HitTypes`,
//! `CastTimeMultiplier`, ...) live in `state()`. A Rust skill always has a template: the C#
//! `new Skill()` / `new Skill(null, null)` (null template) is a skill of the default (id 0)
//! template. `new Skill(template, owner)` is `skill::new_skill_for`; the C# `StubUnit` overriding
//! `GetAbLevel` is an NPC with `ab_level_override` (non-character units use their level or that
//! override as ability level).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::data::GameData;
use crate::models::doodad::Doodad;
use crate::models::npc::Npc;
use crate::models::unit::{SkillAttribute, SkillModifier, UnitModifierType};
use crate::test_support::test_server_with;
use crate::world::{Entity, WorldObject};

use super::data::SkillTemplate;
use super::skill::{new_skill_for, use_skill};
use super::state::{SkillRef, SkillRun};
use super::types::{skill_result, SkillCastTarget, SkillCaster, SkillHitType};

/// `AbilityType.Fight`.
const FIGHT: u8 = 1;

/// `new Skill()`.
fn default_skill() -> SkillRef {
    SkillRun::new(Arc::new(SkillTemplate::default()), 0)
}

/// An NPC whose `GetAbLevel` is `level` (C# `StubUnit`).
fn stub_owner(level: u8) -> WorldObject {
    let mut npc = Npc::new(1);
    npc.unit.skill_state.ab_level_override = Some(level);
    WorldObject::from(&Entity::new(0x400, npc))
}

// ---- Constructor ----

#[test]
fn constructor_default_creates_skill_with_default_values() {
    let skill = default_skill();
    assert_eq!(skill.id, 0);
    assert_eq!(skill.template.id, 0); // C#: Template null
    assert_eq!(skill.level, 0);
    assert_eq!(skill.tl_id(), 0);
    assert!(skill.state().hit_types.is_empty());
}

#[tokio::test]
async fn constructor_with_template_and_owner_creates_skill_with_values() {
    let server = test_server_with(GameData::empty());
    let template = Arc::new(SkillTemplate {
        id: 100,
        ability_id: FIGHT,
        ability_level: 1,
        level_step: 1,
        ..SkillTemplate::default()
    });
    let skill = new_skill_for(&server, template.clone(), Some(&stub_owner(5)));
    assert_eq!(skill.id, 100);
    assert!(Arc::ptr_eq(&skill.template, &template));
    assert_eq!(skill.level, 5); // (5 - 1) / 1 + 1
}

#[tokio::test]
async fn constructor_with_template_and_no_owner_sets_level_to_one() {
    let server = test_server_with(GameData::empty());
    let template = Arc::new(SkillTemplate {
        id: 200,
        ..SkillTemplate::default()
    });
    let skill = new_skill_for(&server, template, None);
    assert_eq!(skill.id, 200);
    assert_eq!(skill.level, 1);
}

// ---- Properties ----

#[test]
fn id_set_and_get_returns_correct_value() {
    let mut skill = default_skill();
    Arc::get_mut(&mut skill).unwrap().id = 123;
    assert_eq!(skill.id, 123);
}

#[test]
fn level_set_and_get_returns_correct_value() {
    let mut skill = default_skill();
    Arc::get_mut(&mut skill).unwrap().level = 5;
    assert_eq!(skill.level, 5);
}

#[test]
fn tl_id_set_and_get_returns_correct_value() {
    let skill = default_skill();
    skill.state().tl_id = 456;
    assert_eq!(skill.tl_id(), 456);
}

#[test]
fn cast_time_multiplier_set_and_get_returns_correct_value() {
    let skill = default_skill();
    assert_eq!(skill.state().cast_time_multiplier, 1.0); // C# default 1f
    skill.state().cast_time_multiplier = 1.5;
    assert_eq!(skill.state().cast_time_multiplier, 1.5);
}

// ---- HitTypes ----

#[test]
fn hit_types_add_and_retrieve_returns_correct_value() {
    let skill = default_skill();
    skill.state().hit_types.insert(1, SkillHitType::MeleeHit);
    assert_eq!(skill.hit_type(1), Some(SkillHitType::MeleeHit));
}

// ---- Cancelled ----

#[test]
fn cancelled_set_and_get_returns_correct_value() {
    let skill = default_skill();
    skill.state().cancelled = true;
    assert!(skill.cancelled());
}

// ---- Callback ----

#[test]
fn callback_set_and_get_returns_correct_value() {
    let skill = default_skill();
    let called = Arc::new(AtomicBool::new(false));
    let flag = called.clone();
    *skill.callback.lock().unwrap() = Some(Box::new(move || flag.store(true, Ordering::SeqCst)));
    skill.invoke_callback();
    assert!(called.load(Ordering::SeqCst));
}

// ---- Use ----

/// C#: a `BaseUnit` that is not a `Unit` is rejected. Rust: a doodad (the only non-unit
/// `BaseUnit` kind).
#[tokio::test]
async fn use_with_non_unit_caster_returns_invalid_source() {
    let server = test_server_with(GameData::empty());
    let skill = default_skill();
    let caster = WorldObject::from(&Entity::new(0x500, Doodad::new(1)));
    let mut target = SkillCastTarget::Unit(0);
    let (result, _) = use_skill(
        &server,
        &skill,
        &caster,
        SkillCaster::Unit(0),
        &mut target,
        None,
        false,
    );
    assert_eq!(result, skill_result::INVALID_SOURCE);
}

// ---- Edge cases ----

#[test]
fn constructor_with_null_template_does_not_throw() {
    let skill = default_skill();
    assert_eq!(skill.id, 0);
}

#[tokio::test]
async fn level_with_zero_ability_level_calculates_correctly() {
    let server = test_server_with(GameData::empty());
    let template = Arc::new(SkillTemplate {
        id: 100,
        ability_id: FIGHT,
        ability_level: 0,
        level_step: 2,
        ..SkillTemplate::default()
    });
    let skill = new_skill_for(&server, template, Some(&stub_owner(10)));
    assert_eq!(skill.level, 6); // (10 - 0) / 2 + 1
}

/// Game data with only the character `LevelDps` / `LevelMana` unit formulas (the real
/// `compact.sqlite3` rows): `(ab_level * 1.6 + 8) * 3` and `level_dps / 3.65`.
fn formula_data() -> GameData {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE unit_formulas (id INTEGER, formula TEXT, kind_id INTEGER, owner_type_id INTEGER);
         CREATE TABLE unit_formula_variables (id INTEGER, unit_formula_id INTEGER, variable_kind_id INTEGER, key INTEGER, value REAL);
         CREATE TABLE wearable_formulas (id INTEGER, kind_id INTEGER, formula TEXT);
         CREATE TABLE formulas (id INTEGER, formula TEXT);
         INSERT INTO unit_formulas VALUES
          (12, '(ab_level * 1.6 + 8) * 3', 12, 0),
          (13, 'level_dps / 3.65', 13, 0);",
    )
    .unwrap();
    let mut data = GameData::empty();
    data.formulas = crate::data::formulas::FormulaManager::load(&c).unwrap();
    data
}

/// `Skill.ManaCost`: the scale comes from the character `LevelDps` / `LevelMana` unit formulas
/// of the data (no hardcoded `- 1` as in the C#), evaluated at the skill level `L`.
#[tokio::test]
async fn mana_level_scale_evaluates_the_data_formulas() {
    let server = test_server_with(formula_data());
    let mut n = Npc::new(1);
    n.unit.level = 55;
    let npc = WorldObject::from(&Entity::new(0x200, n));
    let scale = super::skill::mana_level_scale(&server, 55);
    assert!(
        (scale - (55.0 * 1.6 + 8.0) * 3.0 / 3.65).abs() < 1e-9,
        "{scale}"
    );
    let template = Arc::new(SkillTemplate {
        mana_cost: 30,
        mana_level_md: 1.5,
        ability_id: FIGHT,
        ability_level: 55,
        ..SkillTemplate::default()
    });
    let skill = SkillRun::new(template, 1);
    let cost = super::skill::mana_cost(&server, &skill, &npc);
    assert_eq!(cost, (30.0 + 1.5 * scale) as i32);

    // Without the formulas (no data) the scale is 0: only the fixed cost remains.
    let bare = test_server_with(GameData::empty());
    assert_eq!(super::skill::mana_level_scale(&bare, 55), 0.0);
}

/// The 1.2 skill-level argument `L` and the `casting_inc` term `T` reproduce the client's own
/// `FUN_39897fe0` / `FUN_398a7710` inputs (Arc Lightning's data: ability_level 15,
/// level_step 4, casting_inc 14).
#[test]
fn effective_level_and_casting_inc_term_match_the_1_2_client() {
    let t = SkillTemplate {
        ability_level: 15,
        level_step: 4,
        casting_inc: 14,
        ..SkillTemplate::default()
    };
    assert_eq!(super::skill::effective_level(&t, 1), 15);
    assert_eq!(super::skill::effective_level(&t, 5), 31);
    assert_eq!(super::skill::effective_level(&t, 10), 51);
    assert_eq!(super::skill::effective_level(&t, 11), 55);
    assert_eq!(super::skill::effective_level(&t, 0), 15); // never a rank below 1
    assert_eq!(super::skill::rank_inc(&t, 15), 0.0);
    assert!((super::skill::rank_inc(&t, 31) - 0.224).abs() < 1e-12);
    assert!((super::skill::rank_inc(&t, 55) - 0.56).abs() < 1e-12);
    assert_eq!(super::skill::rank_inc(&t, 10), 0.0); // below ability_level clamps to 0
}

/// A character's skill whose `ability_level` is above the caster's level is rank 1, not the
/// wrapped byte (~255) that blew up L / T / mana; auto-attack and casts agree on L.
#[tokio::test]
async fn new_skill_for_a_character_below_the_skill_level_is_rank_one() {
    let (server, _client, player, _npc) = super::cast_validation_tests::fixture(GameData::empty());
    let caster = WorldObject::from(&player);
    let template = Arc::new(SkillTemplate {
        id: 9002,
        ability_id: FIGHT,
        ability_level: 99,
        level_step: 4,
        casting_inc: 14,
        ..SkillTemplate::default()
    });
    let skill = super::skill::new_skill_for(&server, template, Some(&caster));
    assert_eq!(skill.level, 1);
    assert_eq!(super::skill::cast_skill_level(&server, &skill, &caster), 99);
}

/// `TRUNC(mana_cost + mana_level_md * (1 + T) * LevelMana(L))`: the learned rank and the
/// `casting_inc` term change the cost, and the final value is truncated (75.75 -> 75), not
/// rounded.
#[tokio::test]
async fn mana_cost_uses_the_rank_level_and_truncates_the_final_value() {
    let (server, _client, player, _npc) = super::cast_validation_tests::fixture(formula_data());
    let caster = WorldObject::from(&player);
    let template = Arc::new(SkillTemplate {
        id: 9001,
        ability_id: FIGHT,
        ability_level: 10,
        level_step: 10,
        mana_cost: 10,
        mana_level_md: 1.0,
        casting_inc: 100, // rank 2 -> L 20, T 1.0
        ..SkillTemplate::default()
    });
    let skill = SkillRun::new(template, 2);
    let scale = (20.0 * 1.6 + 8.0) * 3.0 / 3.65;
    let raw = 10.0 + 1.0 * (1.0 + 1.0) * scale;
    assert_eq!(
        raw as i32, 75,
        "the fixture must have a fractional part >= 0.5"
    );
    assert_eq!(super::skill::mana_cost(&server, &skill, &caster), 75);
}

/// The 1.2 cast time: `(int)(casting_time * (1 + T))` (`FUN_398a01b0` kind 4) before the
/// cast-speed stat and the skill modifiers, truncated to ms.
#[tokio::test]
async fn cast_time_uses_the_rank_casting_inc_term() {
    let (server, _client, player, _npc) = super::cast_validation_tests::fixture(GameData::empty());
    let caster = WorldObject::from(&player);
    let template = Arc::new(SkillTemplate {
        id: 10670,
        ability_id: FIGHT,
        ability_level: 15,
        level_step: 4,
        casting_inc: 14,
        casting_time: 2000,
        ..SkillTemplate::default()
    });
    // rank 11 -> L 55 -> T 0.56: 2000 * 1.56 = 3120 ms.
    let skill = SkillRun::new(template, 11);
    assert_eq!(super::skill::cast_time_ms(&server, &skill, &caster), 3120);
}

/// `FUN_398a01b0` kind 4 truncates `casting_time * (1 + T)` to an integer BEFORE the
/// caster's skill modifiers: 999 * 1.07 = 1068.93 -> 1068, then +100% -> 2136 (not 2137).
#[tokio::test]
async fn cast_time_truncates_before_the_skill_modifiers() {
    let (server, _client, player, _npc) = super::cast_validation_tests::fixture(GameData::empty());
    let template = Arc::new(SkillTemplate {
        id: 9003,
        ability_id: FIGHT,
        ability_level: 10,
        level_step: 10,
        casting_inc: 7, // rank 2 -> L 20, T 0.07
        casting_time: 999,
        ..SkillTemplate::default()
    });
    player
        .data()
        .unit
        .skill_modifiers
        .add_modifiers(&[SkillModifier {
            id: 1,
            owner_id: 1,
            tag_id: 0,
            skill_attribute: SkillAttribute::CastTime,
            unit_modifier_type: UnitModifierType::Percent,
            value: 100,
            skill_id: 9003,
            synergy: false,
        }]);
    let caster = WorldObject::from(&player);
    let skill = SkillRun::new(template, 2);
    assert_eq!(super::skill::cast_time_ms(&server, &skill, &caster), 2136);
}

/// An `ability_id == 0` skill takes the rank from the character's level
/// (`(charLevel - ability_level) / level_step + 1`), learned or not: a level-55 character
/// pays the old `LevelMana(55)` value, not `LevelMana(1)`.
#[tokio::test]
async fn ability_0_skill_takes_the_rank_from_the_character_level() {
    let (server, _client, player, _npc) = super::cast_validation_tests::fixture(formula_data());
    player.data().unit.level = 55;
    let caster = WorldObject::from(&player);
    let template = Arc::new(SkillTemplate {
        id: 23693,
        ability_id: 0,
        ability_level: 1,
        level_step: 1,
        mana_cost: 0,
        mana_level_md: 1.0,
        ..SkillTemplate::default()
    });
    let skill = SkillRun::new(template, 1);
    let scale55 = (55.0 * 1.6 + 8.0) * 3.0 / 3.65;
    assert_eq!(scale55 as i32, 78);
    assert_eq!(super::skill::mana_cost(&server, &skill, &caster), 78);
}

/// A non-character caster keeps its previous `LevelMana` argument: the unit level (or
/// `ab_level_override`) for any `level_step`, with only the `casting_inc` term on top.
#[tokio::test]
async fn npc_skill_keeps_its_old_mana_cost() {
    let server = test_server_with(formula_data());
    let mut n = Npc::new(1);
    n.unit.level = 54;
    let npc = WorldObject::from(&Entity::new(0x203, n));
    // Skill 23693's shape: ability 0, ability_level 1, level_step 0, md 150.
    let flat = Arc::new(SkillTemplate {
        id: 23693,
        ability_id: 0,
        ability_level: 1,
        level_step: 0,
        mana_cost: 0,
        mana_level_md: 150.0,
        ..SkillTemplate::default()
    });
    let old = (54.0 * 1.6 + 8.0) * 3.0 / 3.65 * 150.0;
    assert_eq!(old as i32, 11_638);
    assert_eq!(
        super::skill::mana_cost(&server, &SkillRun::new(flat, 1), &npc),
        11_638
    );
    // A step-1 ability-0 skill also keeps the unit level exactly.
    let stepped = Arc::new(SkillTemplate {
        id: 9002,
        ability_id: 0,
        ability_level: 1,
        level_step: 1,
        mana_cost: 0,
        mana_level_md: 1.0,
        ..SkillTemplate::default()
    });
    assert_eq!(
        super::skill::mana_cost(&server, &SkillRun::new(stepped, 1), &npc),
        ((54.0 * 1.6 + 8.0) * 3.0 / 3.65) as i32
    );
}

/// A rank-1 cast is charged `TRUNC(mana_cost + mana_level_md * LevelMana(L))`, and the
/// resource pre-check refuses exactly one mana less than that charge (the check and the
/// charge read the same `(L, T)`).
#[tokio::test]
async fn rank_1_cast_is_charged_the_truncated_formula_and_the_pre_check_agrees() {
    let (server, _client, player, npc) = super::cast_validation_tests::fixture(formula_data());
    let template = Arc::new(SkillTemplate {
        mana_cost: 5,
        mana_level_md: 2.0,
        ability_id: FIGHT,
        ability_level: 10,
        level_step: 10,
        ..super::tests::skill_template(9001)
    });
    // L = 10 at rank 1; LevelMana(10) = 72 / 3.65 = 19.726..., 5 + 2 * 19.726 = 44.45 -> 44.
    let cost = 44;
    let caster = WorldObject::from(&player);
    let cast = |mp: i32| {
        player.data().unit.mp = mp;
        use_skill(
            &server,
            &SkillRun::new(template.clone(), 1),
            &caster,
            SkillCaster::Unit(player.obj_id),
            &mut SkillCastTarget::Unit(npc.obj_id),
            Some(super::types::SkillObject::None),
            true,
        )
        .0
    };
    assert_eq!(
        super::skill::mana_cost(&server, &SkillRun::new(template.clone(), 1), &caster),
        cost
    );
    assert_eq!(cast(100), skill_result::SUCCESS);
    assert_eq!(player.data().unit.mp, 100 - cost);
    assert_eq!(cast(cost), skill_result::SUCCESS);
    assert_eq!(player.data().unit.mp, 0);
    assert_eq!(cast(cost - 1), skill_result::LACK_MANA);
    assert_eq!(player.data().unit.mp, cost - 1);
}

/// The original r208022 client's own tooltip readings: all 68 rows of
/// `skill-tooltips-2026-11-09/skill-tooltips.csv` (27 skills) reproduce the mana and
/// cast-time formulas with zero residual. Runs against the real `compact.sqlite3`.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
async fn real_compact_skill_tooltips_match_the_1_2_client() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let app = std::path::Path::new(&path)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let files = crate::data::client_files::ClientFileManager::from_sources(&[], app);
    let mut data = GameData::empty();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).unwrap();
    data.systems.skills = super::data::Data::load_from(&conn, &files).unwrap();
    let (server, _client, player, _npc) = super::cast_validation_tests::fixture(data);
    let caster = WorldObject::from(&player);
    // (skill_id, rank, L, client mana, client cast ms), in CSV order.
    let rows = [
        (10151, 1, 20, 394, 0),
        (10151, 5, 40, 710, 0),
        (10151, 8, 55, 946, 0),
        (10153, 1, 10, 56, 2000),
        (10153, 5, 50, 208, 2000),
        (10154, 1, 10, 24, 1500),
        (10154, 5, 50, 90, 1500),
        (10648, 1, 3, 21, 0),
        (10648, 5, 15, 52, 0),
        (10648, 10, 30, 92, 0),
        (10648, 18, 54, 155, 0),
        (10664, 1, 50, 1663, 5000),
        (10664, 2, 55, 1814, 5000),
        (10667, 1, 3, 47, 2000),
        (10667, 5, 15, 118, 2000),
        (10667, 10, 30, 207, 2000),
        (10667, 18, 54, 349, 2000),
        (10670, 1, 15, 249, 2000),
        (10670, 5, 31, 550, 2448),
        (10670, 10, 51, 1052, 3008),
        (10670, 11, 55, 1169, 3120),
        (10712, 1, 20, 57, 0),
        (10752, 1, 1, 16, 1000),
        (11314, 1, 30, 184, 2500),
        (11353, 1, 45, 427, 0),
        (11353, 4, 54, 504, 0),
        (11424, 1, 30, 69, 0),
        (11939, 1, 25, 591, 2000),
        (11939, 5, 45, 986, 2000),
        (11939, 7, 55, 1183, 2000),
        (11967, 1, 40, 562, 0),
        (11967, 4, 55, 749, 0),
        (12001, 1, 40, 355, 0),
        (12001, 4, 55, 473, 0),
        (10534, 1, 1, 31, 2000),
        (10534, 5, 21, 136, 2000),
        (10534, 10, 46, 268, 2000),
        (10534, 11, 51, 294, 2000),
        (10546, 1, 15, 263, 7000),
        (10546, 5, 55, 789, 7000),
        (10720, 1, 25, 690, 3000),
        (10720, 5, 45, 1150, 3000),
        (10720, 7, 55, 1380, 3000),
        (11933, 1, 40, 710, 1500),
        (11933, 4, 52, 899, 1500),
        (11948, 1, 35, 999, 2000),
        (11948, 5, 55, 1499, 2000),
        (11973, 1, 1, 17, 1000),
        (11973, 5, 21, 76, 1000),
        (11973, 10, 46, 150, 1000),
        (11973, 11, 51, 165, 1000),
        (13281, 1, 45, 920, 2000),
        (13281, 3, 55, 1546, 2800),
        (13286, 1, 20, 361, 2000),
        (13286, 5, 40, 833, 2560),
        (13286, 8, 55, 1293, 2980),
        (13564, 1, 3, 31, 0),
        (13564, 5, 19, 94, 0),
        (13564, 10, 39, 173, 0),
        (13564, 14, 55, 236, 0),
        (15073, 1, 20, 82, 500),
        (15073, 4, 50, 180, 500),
        (16210, 1, 1, 18, 0),
        (16210, 5, 13, 56, 0),
        (16210, 10, 28, 104, 0),
        (16210, 19, 55, 189, 0),
        (17412, 1, 40, 414, 1000),
        (17412, 2, 50, 506, 1000),
    ];
    assert_eq!(rows.len(), 68);
    for (id, rank, l, mana, cast) in rows {
        let template = server
            .game_data
            .systems
            .skills
            .skill(id)
            .unwrap_or_else(|| panic!("skill {id} missing from compact.sqlite3"))
            .clone();
        assert_eq!(
            super::skill::effective_level(&template, rank),
            l,
            "skill {id} rank {rank}: L"
        );
        let skill = SkillRun::new(template, rank);
        assert_eq!(
            super::skill::mana_cost(&server, &skill, &caster),
            mana,
            "skill {id} rank {rank}: mana"
        );
        assert_eq!(
            super::skill::cast_time_ms(&server, &skill, &caster),
            cast,
            "skill {id} rank {rank}: cast ms"
        );
    }
}
