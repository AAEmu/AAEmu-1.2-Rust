use std::sync::Arc;

use crate::models::unit::SkillAttribute;
use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use super::super::combat as unit_combat;
use super::super::data::SkillTemplate;
use super::super::state::{SkillRef, SkillRun};
use super::super::stats::{stat, StatKind};
use super::super::types::SkillCaster;

/// `GetAbLevel(ability)`: characters from their ability exp, other units their level (or the
/// owner-provided override).
pub fn ab_level(server: &GameServer, unit: &WorldObject, ability: u8) -> i32 {
    match unit {
        WorldObject::Player(p) => {
            let c = p.data();
            crate::db_character::ab_level(server, &c, ability)
        }
        o => o
            .with_unit(|u| {
                i32::from(
                    u.unit()
                        .skill_state
                        .ab_level_override
                        .unwrap_or(u.unit().level),
                )
            })
            .unwrap_or(0),
    }
}

/// `new Skill(template, owner)`.
pub fn new_skill_for(
    server: &GameServer,
    template: Arc<SkillTemplate>,
    owner: Option<&WorldObject>,
) -> SkillRef {
    let level = match owner {
        // A character's rank is saturated like the stored/learned one: a skill above the
        // current ability level would otherwise wrap to ~255 (`as u8`) and blow up L / T / mana.
        // NPC skill levels stay the C# byte cast.
        Some(o) if o.as_player().is_some() => {
            rank_for_level(&template, ab_level(server, o, template.ability_id))
        }
        Some(o) => SkillRun::level_for(&template, ab_level(server, o, template.ability_id)),
        None => 1,
    };
    SkillRun::new(template, level)
}

/// The 1.2 effective skill level `L` (`FUN_39897fe0`): `ability_level + (rank - 1) *
/// level_step`.
///
/// This is the level the client's own tooltip computation passes to the skill evaluator
/// (`FUN_3941e750`), not the player's ability level: a rank-1 skill uses its own
/// `ability_level`. The client never has a rank below 1 for a usable skill, so a 0 (an
/// unknown/never-learned skill) is read as rank 1.
pub fn effective_level(t: &SkillTemplate, rank: u8) -> i32 {
    t.ability_level + (i32::from(rank.max(1)) - 1) * t.level_step
}

/// The 1.2 rank for a caster level: `(level - ability_level) / level_step + 1`, at least 1;
/// `level_step <= 0` is rank 1 (the client never divides by a zero step).
pub fn rank_for_level(t: &SkillTemplate, level: i32) -> u8 {
    crate::data::skills::stored_skill_level(t.ability_level, t.level_step, level)
}

/// The `casting_inc` rank term of the 1.2 skill evaluator (`FUN_398a7710`):
/// `T = max(0, (L - ability_level) * casting_inc * 0.001)`. It multiplies both the mana
/// `mana_level_md` term and the cast time.
pub fn rank_inc(t: &SkillTemplate, level: i32) -> f64 {
    (f64::from(level - t.ability_level) * f64::from(t.casting_inc) * 0.001).max(0.0)
}

/// The 1.2 skill level `L` a cast evaluates at (`FUN_3941e750` -> `FUN_39897fe0`):
///
/// - a character's class skill (`ability_id != 0`) uses the learned rank on the `SkillRun`;
/// - an `ability_id == 0` skill takes the rank from the character's level
///   (`(charLevel - ability_level) / level_step + 1`), learned or not;
/// - a non-character caster (an NPC) keeps the effective level the old `Skill.ManaCost` used
///   as its `LevelMana` argument: the unit level or `ab_level_override`, whatever the
///   `level_step` (the server-side NPC cost is not client-visible; capture-pending).
pub fn cast_skill_level(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> i32 {
    let t = &skill.template;
    if caster.as_player().is_none() {
        ab_level(server, caster, t.ability_id)
    } else if t.ability_id == 0 {
        effective_level(t, rank_for_level(t, ab_level(server, caster, 0)))
    } else {
        effective_level(t, skill.level)
    }
}

/// The `(L, T)` pair of a cast: [`cast_skill_level`] and [`rank_inc`]. The mana charge, the
/// mana pre-check and the plot `ManaCost` effect all read this pair, so a cast the pre-check
/// allows is charged exactly the value it checked.
pub fn level_and_inc(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> (i32, f64) {
    let level = cast_skill_level(server, skill, caster);
    (level, rank_inc(&skill.template, level))
}

/// Mana per `mana_level_md` point at skill level `level`: the character `unit_formulas` rows
/// `LevelDps` (kind 12, `(level * 1.6 + 8) * 3`) and `LevelMana` (kind 13,
/// `level_dps / 3.65`) of `compact.sqlite3`, evaluated through the precompiled formula
/// expressions (no parsing per cast). 0 when the data has no such formulas.
///
/// Deviation: the C# `Skill.ManaCost` hardcodes `((abLevel - 1) * 1.6 + 8) * 3 / 3.65` with the
/// caster's ability level; the 1.2 client evaluates the data formulas at the skill level `L`
/// ([`effective_level`]), so the data and the client win (see PORTING.md).
pub fn mana_level_scale(server: &GameServer, level: i32) -> f64 {
    use crate::data::formulas::{FormulaOwnerType, UnitFormulaKind};
    let formulas = &server.game_data.formulas;
    let (Some(dps), Some(mana)) = (
        formulas.get_unit_formula(FormulaOwnerType::Character, UnitFormulaKind::LevelDps),
        formulas.get_unit_formula(FormulaOwnerType::Character, UnitFormulaKind::LevelMana),
    ) else {
        return 0.0;
    };
    let level_dps = dps.formula.evaluate(&std::collections::HashMap::from([(
        "ab_level",
        f64::from(level),
    )]));
    mana.formula
        .evaluate(&std::collections::HashMap::from([("level_dps", level_dps)]))
}

/// Whether a cast has no `mana_level_md` (`LevelMana`) term: an ability-less skill
/// (`ability_id == 0`) used from an item (`SkillCaster::Item`).
///
/// Retail charged no scaled mana for item uses (owner play-test 2026-11-11: socketing a
/// lunagem, opening "Open" boxes, mount/pet summons); the 1.2 data gives those item skills
/// `mana_cost` 0 with `mana_level_md` 1.0, so only the level term can charge them. The fixed
/// `mana_cost` (a few quest capture/plant item skills have one) still applies. The 1.2 client
/// evaluator (`FUN_398a01b0` kind 0) has no item branch and its tooltip path (`FUN_3941e750`)
/// computes `L` from the character level for ability-0 skills, so the client alone cannot
/// decide the charge; retail behaviour is the source (see PORTING.md). The C# charged
/// `LevelMana(0)` (~6) and the rank port `a5a9efa05` the character level (~50): both were
/// regressions for item uses.
pub fn skips_level_mana(t: &SkillTemplate, source: &SkillCaster) -> bool {
    t.ability_id == 0 && matches!(source, SkillCaster::Item { .. })
}

/// `Skill.ManaCost(caster)`: `TRUNC(mana_cost + mana_level_md * (1 + T) * LevelMana(L))`, then
/// the caster's skill modifiers (`FUN_398a01b0` kind 0, then `FUN_3989f900`). An item use of
/// an ability-less skill keeps only the fixed `mana_cost` ([`skips_level_mana`]).
///
/// The original r208022 client truncates the final value, not rounds it: 68/68 tooltip
/// readings reproduce exactly, 32 of them with a fractional mana part >= 0.5 (see PORTING.md).
pub fn mana_cost(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    source: &SkillCaster,
) -> i32 {
    let t = &skill.template;
    // A gimmick (a thrown bomb, a rolling rock) has no mana pool (`mp` 0): its skills charge
    // nothing and are never refused for mana, here and in `consume_mana` alike. Twelve of the 46
    // gimmick skills have `mana_cost` 0 but a level-scaled `mana_level_md` (1.0 or 5.5), which a
    // level-1 caster with no mana could never pay: without this exemption `check_resources`
    // refused every cast of 11 of the 30 contact gimmicks.
    if caster.as_gimmick().is_some() {
        return 0;
    }
    let cost2 = if skips_level_mana(t, source) {
        f64::from(t.mana_cost)
    } else {
        let (level, inc) = level_and_inc(server, skill, caster);
        let base = mana_level_scale(server, level);
        f64::from(t.mana_cost) + f64::from(t.mana_level_md) * (1.0 + inc) * base
    };
    let tags = server.game_data.systems.skills.skill_tags(t.id).to_vec();
    caster
        .with_unit(|u| {
            u.unit()
                .skill_modifiers
                .apply(t.id, &tags, SkillAttribute::ManaCost, cost2)
        })
        .unwrap_or(cost2) as i32
}

/// The 1.2 cast time of a skill (`FUN_398a01b0` kind 4): `(int)(casting_time * (1 + T))`,
/// then the caster's skill modifiers, then the cast-speed stat `(stat + 1000) / 1000`
/// ([`StatKind::CastTimeMul`]), truncated to ms. `CastTimeMultiplier` is applied separately
/// by the caller.
pub fn cast_time_ms(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> i32 {
    let t = &skill.template;
    if t.casting_time <= 0 {
        return 0;
    }
    let (_, inc) = level_and_inc(server, skill, caster);
    let base = (f64::from(t.casting_time) * (1.0 + inc)) as i32;
    let mul = stat(server, caster, StatKind::CastTimeMul);
    (mul * apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::CastTime,
        f64::from(base),
    )) as i32
}

/// `BaseUnit.ApplySkillModifiers(skill, attribute, value)`.
pub fn apply_skill_modifiers(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    attr: SkillAttribute,
    value: f64,
) -> f64 {
    let tags = server
        .game_data
        .systems
        .skills
        .skill_tags(skill.template.id)
        .to_vec();
    caster
        .with_unit(|u| {
            u.unit()
                .skill_modifiers
                .apply(skill.template.id, &tags, attr, value)
        })
        .unwrap_or(value)
}

/// `Skill.ConsumeMana(caster)`.
///
/// The post-cast (persistent-mana-regen) window mirrors the 1.2 client rather than the C#:
/// `OnSkillFired` (`FUN_3939b010` @ 0x3939b010, DEC 750714) sets the client's `LastCast` only
/// when the skill has a `timing_id` (`+0x1bc`) and does not keep mana regen (`+0xdd`,
/// `keep_mana_regen`); `X2::Unit::UpdateManaRegen` (`FUN_390b63f0` @ 0x390b63f0, DEC 150487)
/// then uses `persistent_mana_regen` (attribute 68) until `LastCast + 5000`. The item-use
/// skills (regrade scrolls: 22520 weapon / 22522 armor regrade, and 2283 `keep_mana_regen`
/// rows in `compact.sqlite3`) keep mana regen, so the client never stalls it for them. The C#
/// `Skill.ConsumeMana` set `IsInPostCast` for every cast, which kept a regrading character on
/// the persistent rate forever; see PORTING.md.
pub fn consume_mana(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    source: &SkillCaster,
) {
    if !caster.is_unit() {
        return;
    }
    let cost = mana_cost(server, skill, caster, source);
    unit_combat::reduce_current_mp(server, caster, cost);
    if skill.template.timing_id == 0 || skill.template.keep_mana_regen {
        return;
    }
    if let Some(p) = caster.as_player() {
        let mut c = p.data();
        c.skill_char.last_cast_ms = now_ms();
        c.skill_char.is_in_post_cast = true;
    }
}
