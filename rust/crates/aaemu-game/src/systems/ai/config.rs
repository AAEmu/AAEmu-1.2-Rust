//! `Ai` configuration (`Configurations/Ai.json`, Rust only): NPC target selection.
//!
//! **Added deviation (see `PORTING.md`):** the 10.x zone server (`NpcAggroTable`,
//! `sub_3931EB70`) re-ranks on a strict `>` and switches target as soon as anybody's threat is
//! higher. With `PullAggroEnabled` an NPC keeps a valid current target until another attacker's
//! threat exceeds the current target's threat times a ratio (melee or ranged, by the
//! challenger's distance). `PullAggroEnabled: false` restores the original behaviour.

use serde::Deserialize;

use crate::config::lenient;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Keep the current target until a challenger's threat exceeds `ratio` times its threat.
    #[serde(rename = "pullaggroenabled", deserialize_with = "lenient")]
    pub pull_aggro_enabled: bool,
    /// Ratio for a challenger within [`Self::pull_aggro_melee_range`] of the NPC.
    #[serde(rename = "pullaggromeleeratio", deserialize_with = "lenient")]
    pub pull_aggro_melee_ratio: f64,
    /// Ratio for a challenger farther away than the melee range.
    #[serde(rename = "pullaggrorangedratio", deserialize_with = "lenient")]
    pub pull_aggro_ranged_ratio: f64,
    /// Metres (model radii subtracted) up to which a challenger counts as melee. The default
    /// 4.0 is the melee reach the NPC skill code already uses (`picked == 2 && dist > 4.0`).
    #[serde(rename = "pullaggromeleerange", deserialize_with = "lenient")]
    pub pull_aggro_melee_range: f32,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            pull_aggro_enabled: true,
            pull_aggro_melee_ratio: 1.10,
            pull_aggro_ranged_ratio: 1.30,
            pull_aggro_melee_range: 4.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;

    #[test]
    fn ai_section_binds_and_defaults() {
        let default = AppConfig::default();
        assert!(default.ai.pull_aggro_enabled);
        assert_eq!(default.ai.pull_aggro_melee_ratio, 1.10);
        assert_eq!(default.ai.pull_aggro_ranged_ratio, 1.30);
        assert_eq!(default.ai.pull_aggro_melee_range, 4.0);

        let config = AppConfig::from_value(serde_json::json!({
            "Ai": {
                "PullAggroEnabled": "false",
                "PullAggroMeleeRatio": "1.5",
                "PullAggroRangedRatio": "2",
                "PullAggroMeleeRange": "6"
            }
        }))
        .unwrap();
        assert!(!config.ai.pull_aggro_enabled);
        assert_eq!(config.ai.pull_aggro_melee_ratio, 1.5);
        assert_eq!(config.ai.pull_aggro_ranged_ratio, 2.0);
        assert_eq!(config.ai.pull_aggro_melee_range, 6.0);
    }
}
