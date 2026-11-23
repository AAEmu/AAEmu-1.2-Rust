//! Quest configuration (`Quest:*`).

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuestConfig {
    /// Maximum active or completed daily quests per character. Zero disables the quota.
    /// r208022 reads the world-specific quest_daily_limit at RVA 0x582b20.
    #[serde(
        rename = "dailylimit",
        alias = "quest_daily_limit",
        deserialize_with = "crate::config::lenient"
    )]
    pub daily_limit: usize,

    /// Use the r208022 quest window's degree (0x7e5eb0 / 0x7e5dd0) and XP level-gap
    /// (0x7e3810) factors for payouts. Set to false for unscaled base amounts.
    #[serde(rename = "rewardscaling", deserialize_with = "crate::config::lenient")]
    pub reward_scaling: bool,
}

impl Default for QuestConfig {
    fn default() -> Self {
        Self {
            daily_limit: 0,
            reward_scaling: true,
        }
    }
}
