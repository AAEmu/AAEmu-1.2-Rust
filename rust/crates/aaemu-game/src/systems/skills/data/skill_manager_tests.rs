//! Port of AAEmu.UnitTests/Game/Core/Managers/SkillManagerTests.cs
//!
//! `SkillManager`'s tables are split between `systems::skills::Data` (skills, buffs, effects,
//! tags, modifiers, combat buffs, triggers, reagents / products) and the phase-2
//! `data::skills::SkillData` (default skills, common skills, passive buffs). The C# tests fill
//! the private dictionaries by reflection; here the (public) fields are filled directly. There
//! are no injected `IAnimationManager` / `IPlotManager`: the animation and plot tables are
//! fields of `Data` loaded by `Data::load`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::effects::{EffectKind, EffectTemplate, EffectType, HealEffect, SpawnGimmickEffect};
use super::{BuffTemplate, BuffTriggerTemplate, Data, SkillItemAmount, SkillTemplate};
use crate::data::skills::{DefaultSkill, PassiveBuffTemplate, SkillData};
use crate::models::unit::{CombatBuffTemplate, SkillAttribute, SkillModifier, UnitModifierType};
use crate::systems::skills::tests::damage_effect;

fn skills_with(templates: Vec<SkillTemplate>) -> Data {
    let mut d = Data::default();
    for t in templates {
        d.skills.insert(t.id, Arc::new(t));
    }
    d
}

// ---- Constructor ----

/// C#: constructing `SkillManager` with mocked deps calls neither of them
/// (`Mock.VerifyNoOtherCalls`). Rust: the default tables are empty and not loaded.
#[test]
fn constructor_with_mocked_dependencies_does_not_throw() {
    let d = Data::default();
    assert!(!d.is_loaded());
    assert!(d.skills.is_empty() && d.buffs.is_empty() && d.effects.is_empty());
    assert!(d.plots.plots.is_empty() && d.anims.anims.is_empty());
    let s = SkillData::default();
    assert!(s.default_skills.is_empty() && s.common_skills.is_empty());
}

// ---- GetSkillTemplate ----

#[test]
fn get_skill_template_returns_skill_when_exists() {
    let d = skills_with(vec![SkillTemplate {
        id: 1,
        cooldown_time: 5000,
        casting_time: 1000,
        ..SkillTemplate::default()
    }]);
    let result = d.skill(1).expect("skill");
    assert_eq!(result.id, 1);
}

// ---- GetBuffTemplate ----

#[test]
fn get_buff_template_returns_buff_when_exists() {
    let mut d = Data::default();
    d.buffs.insert(
        1,
        Arc::new(BuffTemplate {
            id: 1,
            duration: 30000,
            ..BuffTemplate::default()
        }),
    );
    let result = d.buff(1).expect("buff");
    assert_eq!(result.id, 1);
    assert_eq!(result.duration, 30000);
}

// ---- GetPassiveBuffTemplate ----

#[test]
fn get_passive_buff_template_returns_passive_buff_when_exists() {
    let mut s = SkillData::default();
    s.passive_buffs.insert(
        1,
        PassiveBuffTemplate {
            id: 1,
            buff_id: 100,
            level: 5,
            ..PassiveBuffTemplate::default()
        },
    );
    let result = s.passive_buff(1).expect("passive buff");
    assert_eq!(result.id, 1);
    assert_eq!(result.buff_id, 100);
}

// ---- GetEffectTemplate ----

fn heal_effect(id: u32) -> HealEffect {
    HealEffect {
        id,
        use_fixed_heal: false,
        fixed_min: 0,
        fixed_max: 0,
        use_level_heal: false,
        level_md: 0.0,
        level_va_start: 0,
        level_va_end: 0,
        percent: false,
        use_charged_buff: false,
        charged_buff_id: 0,
        charged_mul: 0.0,
        slave_applicable: false,
        ignore_heal_aggro: false,
        dps_multiplier: 0.0,
        actability_group_id: 0,
        actability_step: 0,
        actability_mul: 0.0,
        actability_add: 0.0,
    }
}

#[test]
fn get_effect_template_returns_effect_when_exists() {
    let mut d = Data::default();
    d.effect_types.insert(
        1,
        EffectType {
            id: 1,
            actual_id: 1,
            actual_type: "DamageEffect".into(),
        },
    );
    d.effects.insert(
        EffectKind::DamageEffect,
        HashMap::from([(1, EffectTemplate::Damage(Box::new(damage_effect(1, 0))))]),
    );
    let (kind, result) = d.effect_by_type_id(1).expect("effect");
    assert_eq!(kind, EffectKind::DamageEffect);
    assert_eq!(result.id(), 1);
}

#[test]
fn get_effect_template_with_type_returns_effect_when_exists() {
    let mut d = Data::default();
    d.effects.insert(
        EffectKind::HealEffect,
        HashMap::from([(1, EffectTemplate::Heal(heal_effect(1)))]),
    );
    let result = d.effect_by_name(1, "HealEffect").expect("effect");
    assert_eq!(result.id(), 1);
}

#[test]
fn get_effect_template_with_type_returns_null_for_non_existent_type() {
    let d = Data::default();
    assert!(d.effect_by_name(1, "NonExistentEffect").is_none());
}

// ---- IsDefaultSkill / IsCommonSkill ----

#[test]
fn is_default_skill_returns_true_when_skill_is_default() {
    let mut s = SkillData::default();
    s.default_skills.push(DefaultSkill {
        skill_id: 1,
        slot: 0,
        add_to_slot: false,
    });
    assert!(s.is_default_skill(1));
}

#[test]
fn is_default_skill_returns_false_when_skill_is_not_default() {
    let s = SkillData::default();
    assert!(!s.is_default_skill(1));
}

#[test]
fn is_common_skill_returns_true_when_skill_is_common() {
    let mut s = SkillData::default();
    s.common_skills = vec![1, 2, 3];
    assert!(s.is_common_skill(1));
}

#[test]
fn is_common_skill_returns_false_when_skill_is_not_common() {
    let mut s = SkillData::default();
    s.common_skills = vec![1, 2, 3];
    assert!(!s.is_common_skill(999));
}

// ---- GetBuffTags ----

#[test]
fn get_buff_tags_returns_tags_when_exists() {
    let mut d = Data::default();
    d.buff_tags.insert(1, vec![10, 20, 30]);
    assert_eq!(d.buff_tags(1).len(), 3);
}

// ---- GetDefaultSkills ----

#[test]
fn get_default_skills_returns_skills_when_loaded() {
    let mut s = SkillData::default();
    s.default_skills.push(DefaultSkill {
        skill_id: 1,
        slot: 0,
        add_to_slot: false,
    });
    s.default_skills.push(DefaultSkill {
        skill_id: 2,
        slot: 1,
        add_to_slot: false,
    });
    assert_eq!(s.default_skills.len(), 2);
}

// ---- GetModifiersByOwnerId ----

#[test]
fn get_modifiers_by_owner_id_returns_modifiers_when_exists() {
    let mut d = Data::default();
    d.skill_modifiers.insert(
        1,
        vec![SkillModifier {
            id: 1,
            owner_id: 1,
            tag_id: 0,
            skill_attribute: SkillAttribute::ManaCost,
            unit_modifier_type: UnitModifierType::Value,
            value: 0,
            skill_id: 0,
            synergy: false,
        }],
    );
    assert_eq!(d.skill_modifiers_by_owner(1).len(), 1);
}

// ---- GetCombatBuffs ----

#[test]
fn get_combat_buffs_returns_buffs_when_exists() {
    let mut d = Data::default();
    d.combat_buffs.insert(
        1,
        vec![CombatBuffTemplate {
            id: 1,
            hit_skill_id: 0,
            hit_type: 0,
            req_skill_id: 0,
            buff_id: 100,
            buff_from_source: false,
            buff_to_source: false,
            req_buff_id: 1,
            is_heal_spell: false,
        }],
    );
    assert_eq!(d.combat_buffs_for(1).len(), 1);
}

// ---- GetSkillReagentsBySkillId / GetSkillProductsBySkillId ----

#[test]
fn get_skill_reagents_by_skill_id_returns_reagents_when_exists() {
    let d = Data {
        skill_reagents: vec![SkillItemAmount {
            id: 1,
            skill_id: 1,
            item_id: 100,
            amount: 5,
        }],
        ..Data::default()
    };
    assert_eq!(d.reagents_for(1).len(), 1);
}

#[test]
fn get_skill_products_by_skill_id_returns_products_when_exists() {
    let d = Data {
        skill_products: vec![SkillItemAmount {
            id: 1,
            skill_id: 1,
            item_id: 200,
            amount: 1,
        }],
        ..Data::default()
    };
    assert_eq!(d.products_for(1).len(), 1);
}

// ---- GetBuffTriggerTemplates ----

#[test]
fn get_buff_trigger_templates_returns_triggers_when_exists() {
    let mut d = Data::default();
    d.buff_triggers.insert(
        1,
        vec![BuffTriggerTemplate {
            id: 1,
            kind: 0,
            effect_on_source: false,
            effect: (EffectKind::DamageEffect, 0),
            use_damage_amount: false,
            use_original_source: false,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            synergy: false,
        }],
    );
    assert_eq!(d.buff_triggers_for(1).len(), 1);
}

// ---- GetSkillActAbility ----

/// `ActabilityType.None`.
const ACTABILITY_NONE: i32 = 0;
/// `ActabilityType.Alchemy`.
const ACTABILITY_ALCHEMY: i32 = 1;

#[test]
fn get_skill_act_ability_returns_none_when_skill_not_found() {
    let d = Data::default();
    assert_eq!(d.skill_actability(1), ACTABILITY_NONE);
}

#[test]
fn get_skill_act_ability_returns_act_ability_when_skill_exists() {
    let d = skills_with(vec![SkillTemplate {
        id: 1,
        actability_group_id: ACTABILITY_ALCHEMY,
        ..SkillTemplate::default()
    }]);
    assert_eq!(d.skill_actability(1), ACTABILITY_ALCHEMY);
}

// ---- GetNpSkillTemplate ----

/// `SkillUseConditionKind.None`.
const SKILL_USE_CONDITION_NONE: i32 = -1;

#[test]
fn get_np_skill_template_returns_null_when_skill_not_found() {
    let d = Data::default();
    assert!(d.np_skill(1, SKILL_USE_CONDITION_NONE).is_none());
}

#[test]
fn get_np_skill_template_returns_skill_when_valid() {
    let d = skills_with(vec![SkillTemplate {
        id: 1,
        ignore_global_cooldown: true,
        ..SkillTemplate::default()
    }]);
    let result = d.np_skill(1, SKILL_USE_CONDITION_NONE).expect("skill");
    assert_eq!(result.id, 1);
}

// ---- GetSpawnGimmickEffect ----

#[test]
fn get_spawn_gimmick_effect_returns_effect_when_found() {
    let mut d = Data::default();
    d.effects.insert(
        EffectKind::SpawnGimmickEffect,
        HashMap::from([(
            1,
            EffectTemplate::SpawnGimmick(SpawnGimmickEffect {
                id: 1,
                gimmick_id: 100,
                offset_from_source: false,
                offset_coordinate_id: 0,
                offset: [0.0; 3],
                scale: 0.0,
                velocity_coordinate_id: 0,
                velocity: [0.0; 3],
                ang_vel_coordinate_id: 0,
                ang_vel: [0.0; 3],
            }),
        )]),
    );
    let result = d.spawn_gimmick_effect(100).expect("effect");
    assert_eq!(result.gimmick_id, 100);
}

// ---- Edge cases ----

#[test]
fn get_skill_template_handles_max_u_int32() {
    let d = Data::default();
    assert!(d.skill(u32::MAX).is_none());
}

#[test]
fn multiple_get_calls_return_consistent_results() {
    let d = skills_with(vec![SkillTemplate {
        id: 1,
        cooldown_time: 5000,
        ..SkillTemplate::default()
    }]);
    let r1 = d.skill(1).expect("1");
    let r2 = d.skill(1).expect("2");
    let r3 = d.skill(1).expect("3");
    assert_eq!(r2.id, r1.id);
    assert_eq!(r3.id, r2.id);
}

// ---- Close-range damage penalty skill set (`formulas` row 12) ----

/// The set comes from the en_us tooltips, not from a hard-coded id list: the colour markup the
/// data puts around "-20%" / "8m" is stripped, and rows that only mention 8 m or 20% for
/// another reason (a 38 m tentacle, a bonus wording, a skill name) are not members.
#[test]
fn range_penalty_skills_are_built_from_the_en_us_tooltip_text() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE localized_texts (tbl_name TEXT, tbl_column_name TEXT, idx INT, en_us TEXT);
         INSERT INTO localized_texts VALUES
           ('skills', 'desc', 14835,
            'Fires arrows. Damage decreases |cFFFF9C27-20%|r if target is within |cFFFF9C278m|r.'),
           ('skills', 'desc', 19052,
            'Fires a toxic arrow. Damage is reduced -20% if target is within 8m.'),
           ('skills', 'desc', 10481,
            'Poison Damage over time. Damage reduced -20% if target is within 8m.'),
           ('skills', 'desc', 13328,
            'Uses a tentacle within 30m - 45m and 38m ahead with a length of 20m.'),
           ('skills', 'desc', 10029,
            'Deals 20% damage to enemies within 8m of the target area.'),
           ('skills', 'desc', 20001,
            'Deals 20% more damage to targets within 8m.'),
           ('skills', 'name', 30001,
            'Damage decreases -20% if target is within 8m.');",
    )
    .unwrap();
    assert_eq!(
        super::load_range_penalty_skills(&conn),
        HashSet::from([10481, 14835, 19052])
    );
}

/// The real 1.2 compact: exactly the tooltip skills the audit named (`verify-claims-10x.md`
/// 11a): Endless Arrows (14835-14837, 22961), Shoot Arrow 16064, Charged Bolt (16210, 23544),
/// Venom Arrow 19052, Piercing Shot (13564, 23545), Toxic Shot 10481, Mark (12139, 20590,
/// 23548) and 26730 (a description without a `skills` row).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn real_compact_range_penalty_skills_are_the_tooltip_skills() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        super::load_range_penalty_skills(&conn),
        HashSet::from([
            10481, 12139, 13564, 14835, 14836, 14837, 16064, 16210, 19052, 20590, 22961, 23544,
            23545, 23548, 26730,
        ])
    );
}
