//! Quest configuration (`Quest:*`).

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QuestConfig {
    /// Scale the paid quest exp / copper like the 1.2 client's quest window
    /// (`X2Quest.GetQuestContextRewardExp` / `...RewardCopper`): the `quest_supplies` fallback
    /// by the quest degree factor, exp (act and fallback) by the quest-vs-player level-gap
    /// factor. The original zone server's behaviour is INFERRED, not captured
    /// (`porting-md#2`); `false` keeps today's unscaled payout.
    #[serde(rename = "rewardscaling", deserialize_with = "crate::config::lenient")]
    pub reward_scaling: bool,
}
