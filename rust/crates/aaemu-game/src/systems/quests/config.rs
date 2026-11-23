//! Quest configuration (`Quest:*`).

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuestConfig {
    /// Use the r208022 quest window's degree (0x7e5eb0 / 0x7e5dd0) and XP level-gap
    /// (0x7e3810) factors for payouts. Set to false for unscaled base amounts.
    #[serde(rename = "rewardscaling", deserialize_with = "crate::config::lenient")]
    pub reward_scaling: bool,
}

impl Default for QuestConfig {
    fn default() -> Self {
        Self {
            reward_scaling: true,
        }
    }
}
