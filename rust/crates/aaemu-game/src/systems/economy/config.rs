//! `Crafting` configuration (`Configurations/Crafting.json`).

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CraftingConfig {
    /// Refuse `CSExecuteCraft` for a craft that has a learn source (`item_recipes` /
    /// `train_craft_effects`) and is not in the character's learned set. Default **false**:
    /// whether retail 1.2 enforced the gate is UNKNOWN (WP-I1 UNKNOWN-1), so the default keeps
    /// the previous behaviour.
    #[serde(
        rename = "requirelearnedrecipes",
        deserialize_with = "crate::config::lenient"
    )]
    pub require_learned_recipes: bool,
}
