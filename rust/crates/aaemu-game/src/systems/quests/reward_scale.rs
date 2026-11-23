//! 1.2 client quest reward scaling (`Quest:RewardScaling`, enabled by default).
//!
//! `X2Quest.GetQuestContextRewardExp` (`FUN_394a1370`) and `...RewardCopper`
//! (`FUN_394a0fd0`) compute the number the quest window shows from the client tables:
//!
//! * With no `QuestActSupplyExp` / `QuestActSupplyCopper` act in the Reward component, the
//!   level's `quest_supplies` value is scaled by a degree factor (`FUN_397d5eb0` /
//!   `FUN_397d5dd0`): 0 and 1 -> 0.5, 2 -> 0.75, 3 -> 1.0, 4 -> 1.25, 5 and above -> 1.5
//!   (a NULL degree reads as 0).
//! * Exp, act value or scaled supply, is then reduced by the quest-vs-player level gap
//!   (`FUN_397d3810`). Copper has no gap factor.
//!
//! The server uses the r208022 display formulas for payout. The flag restores unscaled
//! base amounts for servers that need the previous behavior.

/// Degree factor of the `quest_supplies` fallback (`FUN_397d5eb0` / `FUN_397d5dd0`).
pub fn degree_factor(degree: i32) -> f32 {
    match degree {
        0 | 1 => 0.5,
        2 => 0.75,
        3 => 1.0,
        4 => 1.25,
        _ => 1.5,
    }
}

/// `(int)((float)exp * k[degree])` (f32, truncating toward zero).
pub fn scaled_supply_exp(exp: i32, degree: i32) -> i32 {
    (exp as f32 * degree_factor(degree)) as i32
}

/// `(int)((float)copper * k[degree])` (f32, truncating toward zero).
pub fn scaled_supply_copper(copper: i32, degree: i32) -> i32 {
    (copper as f32 * degree_factor(degree)) as i32
}

/// Exp level-gap factor (`FUN_397d3810`): `base` for a quest level `quest_level` paid to a
/// character of `player_level`.
///
/// The allowance `g` is `max(5, (int)((p + 1) * 0.2 + 0.5))`: a quest at most `g` levels below
/// the player pays in full, each further level pays 10 % less, and `g + 5` or more levels down
/// pays 50 %. The Ghidra output shows the larger branch as `&param_3` (the player level),
/// which would disable the reduction from player level 22. That is the MSVC `std::max`
/// temporary sharing the argument's stack slot. The live r208022 client shows 255 XP, not
/// 510, for quest 97 (quest level 5, base 510) at player level 22 (2026-11-23,
/// `lanes\live-audit\run\audit-evidence.txt`), which only the max reading gives.
pub fn gap_exp(base: i32, quest_level: u8, player_level: u8) -> i32 {
    let p = i32::from(player_level);
    let q = i32::from(quest_level);
    let g = (((p + 1) as f32 * 0.2 + 0.5) as i32).max(5);
    let d = g - p + q;
    if d >= 0 {
        base
    } else if d < -4 {
        (base as f32 - base as f32 * 0.5) as i32
    } else {
        ((d * base) as f32 * 0.1 + base as f32) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degree_factor_wire_values() {
        assert_eq!(degree_factor(-1), 1.5);
        assert_eq!(degree_factor(0), 0.5);
        assert_eq!(degree_factor(1), 0.5);
        assert_eq!(degree_factor(2), 0.75);
        assert_eq!(degree_factor(3), 1.0);
        assert_eq!(degree_factor(4), 1.25);
        assert_eq!(degree_factor(5), 1.5);
        assert_eq!(degree_factor(9), 1.5);
    }

    #[test]
    fn scaled_supply_truncates_like_the_client() {
        assert_eq!(scaled_supply_exp(1_001, 2), 750);
        assert_eq!(scaled_supply_exp(1_001, 3), 1_001);
        assert_eq!(scaled_supply_copper(1_001, 4), 1_251);
        assert_eq!(scaled_supply_copper(1, 0), 0);
    }

    #[test]
    fn gap_exp_table() {
        // p = 10: q >= 5 full, then 90 / 80 / 70 / 60 / 50 %.
        for (q, want) in [(5, 1000), (4, 900), (3, 800), (2, 700), (1, 600), (0, 500)] {
            assert_eq!(gap_exp(1000, q, 10), want, "p=10 q={q}");
        }
        // The allowance stays 5 up to player level 26, so level 22 still reduces
        // (live r208022: quest 97 shows 255 XP at level 22).
        assert_eq!(gap_exp(1000, 11, 21), 500);
        assert_eq!(gap_exp(1000, 1, 22), 500);
        assert_eq!(gap_exp(1000, 17, 22), 1000);
        assert_eq!(gap_exp(1000, 16, 22), 900);
        // From level 27 the allowance grows to (int)(28 * 0.2 + 0.5) = 6.
        assert_eq!(gap_exp(1000, 21, 27), 1000);
        assert_eq!(gap_exp(1000, 20, 27), 900);
        assert_eq!(gap_exp(1000, 1, 1), 1000);
        // d < -4 branch truncates the half toward zero.
        assert_eq!(gap_exp(1001, 0, 10), 500);
        // d = -3 keeps the truncating f32 result.
        assert_eq!(gap_exp(101, 2, 10), 70);
        // d = -4 is still the 60 % branch: (int)(-400.4 + 1001).
        assert_eq!(gap_exp(1001, 1, 10), 600);
    }
}
