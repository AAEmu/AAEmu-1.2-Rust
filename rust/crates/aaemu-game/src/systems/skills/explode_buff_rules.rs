//! `ExplodeBuffRules` (special type 46): burns the target's beneficial effects away and pays
//! damage for each one burned.
//!
//! Ported from commit `c78b3a659`
//! (`AAEmu.Game/Models/Game/Skills/ExplodeBuffRules.cs` and its `AAEmu.UnitTests` cases). The 1.2
//! C# only logged this special type. Kept pure so the policy is testable without a server.

/// `BuffKind.Good` (the 1.2 `buffs.kind` of a beneficial effect).
pub const KIND_GOOD: i32 = 1;

/// One effect on the target, as the removal decision sees it (`BurnCandidate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnCandidate {
    /// The buff slot (`Buff.Index`); the selection burns the lowest ones first.
    pub index: u32,
    /// `BuffTemplate.Kind`.
    pub kind: i32,
    /// `Buff.Passive`.
    pub passive: bool,
    /// `BuffTemplate.System`.
    pub system: bool,
    /// `Buff.IsEnded()`: an effect already expiring is on its way out and is not counted.
    pub ended: bool,
}

/// `ExplodeBuffRules.MaxBuffsToBurn(value2, value3)`: the shipped rows set the two slots to the
/// same number (5009 / 9531 `20`, the Timeout row 6411 `50`), so which one is the cap cannot be
/// told apart; the larger one is used and a negative pair names no effect.
pub fn max_buffs_to_burn(value2: i32, value3: i32) -> usize {
    value2.max(value3).max(0) as usize
}

/// The effects to burn: beneficial, neither passive nor system, lowest slot first so a repeated
/// cast always burns the same ones. Passives and system buffs are part of what the unit is rather
/// than something an enemy's skill can strip.
pub fn select_buffs(buffs: &[BurnCandidate], max_count: usize) -> Vec<BurnCandidate> {
    let mut chosen: Vec<BurnCandidate> = buffs
        .iter()
        .copied()
        .filter(|buff| buff.kind == KIND_GOOD && !buff.passive && !buff.system && !buff.ended)
        .collect();
    chosen.sort_by_key(|buff| buff.index);
    chosen.truncate(max_count);
    chosen
}

/// Damage for the effects burned: none burned or no per-effect damage leaves the target
/// untouched, otherwise exact integer arithmetic.
pub fn damage_for(burned_count: usize, damage_per_buff: i32) -> i32 {
    if burned_count == 0 || damage_per_buff <= 0 {
        return 0;
    }
    let damage = (burned_count as u128).saturating_mul(damage_per_buff as u128);
    damage.min(i32::MAX as u128) as i32
}

#[cfg(test)]
#[path = "explode_buff_rules_tests.rs"]
mod explode_buff_rules_tests;
