//! Tests of [`super`], ported from the C# `ExplodeBuffRulesTests` of commit `c78b3a659`.
//!
//! Skill 내부 충격 16410's own text ("적의 이로운 효과를 태워 소멸 시키며 … 의 피해를 주고") and buff
//! 449's ("이로운 효과가 존재할 경우 1개의 강화 효과 소멸 및 피해", with 1 in both slots) are what
//! the slots read as.

use super::{self as explode_buff, BurnCandidate};

/// `BuffKind`.
const GOOD: i32 = 1;
const BAD: i32 = 2;

fn candidate(index: u32, kind: i32, passive: bool, system: bool) -> BurnCandidate {
    BurnCandidate {
        index,
        kind,
        passive,
        system,
        ended: false,
    }
}

#[test]
fn the_larger_slot_is_the_cap() {
    // Both shipped rows set the two slots equal, so which one caps cannot be told apart.
    assert_eq!(explode_buff::max_buffs_to_burn(20, 20), 20);
    assert_eq!(explode_buff::max_buffs_to_burn(1, 1), 1);
    assert_eq!(explode_buff::max_buffs_to_burn(1, 20), 20);
    assert_eq!(explode_buff::max_buffs_to_burn(0, 0), 0);
    assert_eq!(explode_buff::max_buffs_to_burn(-4, 0), 0);
}

#[test]
fn only_beneficial_non_passive_non_system_effects_are_burned() {
    let chosen = explode_buff::select_buffs(
        &[
            candidate(1, GOOD, false, false),
            candidate(2, BAD, false, false),
            candidate(4, GOOD, true, false),
            candidate(5, GOOD, false, true),
            candidate(6, GOOD, false, false),
            BurnCandidate {
                ended: true,
                ..candidate(7, GOOD, false, false)
            },
        ],
        20,
    );
    assert_eq!(
        chosen.iter().map(|buff| buff.index).collect::<Vec<_>>(),
        [1, 6]
    );
}

#[test]
fn the_cap_limits_the_burn_lowest_slot_first() {
    let buffs = [
        candidate(9, GOOD, false, false),
        candidate(2, GOOD, false, false),
        candidate(5, GOOD, false, false),
    ];
    let chosen = explode_buff::select_buffs(&buffs, 2);
    assert_eq!(
        chosen.iter().map(|buff| buff.index).collect::<Vec<_>>(),
        [2, 5]
    );
}

#[test]
fn no_cap_or_nothing_to_burn_selects_nothing() {
    let buffs = [candidate(1, GOOD, false, false)];
    assert!(explode_buff::select_buffs(&buffs, 0).is_empty());
    assert!(explode_buff::select_buffs(&[], 5).is_empty());
}

#[test]
fn damage_is_per_burned_effect() {
    // Skill 16410 carries value1 1000, so the tooltip's min~max span is 1000..20000 over the
    // 1..20 effects a target can hold.
    assert_eq!(explode_buff::damage_for(1, 1000), 1000);
    assert_eq!(explode_buff::damage_for(20, 1000), 20000);
    assert_eq!(explode_buff::damage_for(1, 2000), 2000);
    assert_eq!(explode_buff::damage_for(usize::MAX, 2), i32::MAX);
}

#[test]
fn nothing_burned_or_no_per_effect_damage_deals_nothing() {
    assert_eq!(explode_buff::damage_for(0, 1000), 0);
    assert_eq!(explode_buff::damage_for(5, 0), 0);
    assert_eq!(explode_buff::damage_for(5, -1), 0);
}
