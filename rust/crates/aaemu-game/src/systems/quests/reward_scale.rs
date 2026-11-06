//! 1.2 client quest reward scaling (`Quest:RewardScaling`, default off).
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
//! The amounts are display-only in the client: the server decides what it pays (the quest
//! packets carry no reward figure). Whether the original zone server applied these factors is
//! INFERRED from the client UI (a window showing a different number than the payout would be a
//! visible bug); no capture of a retail grant exists. The flag keeps today's unscaled payout
//! until the live check (quest-window number vs. exp / copper actually gained) settles it
//! (`porting-md#2`, capture-pending; see `PORTING.md`).

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
/// The full payout needs a quest at most 5 levels below the player (up to player level 21);
/// each further level pays 10 % less, and 10 or more levels down pays 50 %. From player level
/// 22 the client disables the reduction entirely (the `g = p` branch, a discontinuity in the
/// original formula).
pub fn gap_exp(base: i32, quest_level: u8, player_level: u8) -> i32 {
    let p = i32::from(player_level);
    let q = i32::from(quest_level);
    let g = if ((p + 1) as f32 * 0.2 + 0.5) as i32 > 4 {
        p
    } else {
        5
    };
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
        // p = 21 is the last reduced level (d = -5 -> 50 %); p = 22 never reduces.
        assert_eq!(gap_exp(1000, 11, 21), 500);
        assert_eq!(gap_exp(1000, 1, 22), 1000);
        assert_eq!(gap_exp(1000, 1, 1), 1000);
        // d < -4 branch truncates the half toward zero.
        assert_eq!(gap_exp(1001, 0, 10), 500);
        // d = -3 keeps the truncating f32 result.
        assert_eq!(gap_exp(101, 2, 10), 70);
        // d = -4 is still the 60 % branch: (int)(-400.4 + 1001).
        assert_eq!(gap_exp(1001, 1, 10), 600);
    }
}
