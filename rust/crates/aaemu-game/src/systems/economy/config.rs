//! `Crafting`, `Ics` and `Loot` configuration (`Configurations/Crafting.json`,
//! `Configurations/Ics.json`, `Configurations/Loot.json`).

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

/// `Ics` (`Configurations/Ics.json`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IcsConfig {
    /// AA points granted per account credit spent through `CSICSBuyAAPointPacket` (0 = the
    /// exchange is disabled). The retail value is UNKNOWN; the 1.2 client only multiplies the
    /// cash amount by the `SCICSExchangeRatio` it is sent. The product is computed in `i64`.
    #[serde(
        rename = "aapointexchangeratio",
        deserialize_with = "crate::config::lenient"
    )]
    pub aa_point_exchange_ratio: i32,
}

/// `Loot` (`Configurations/Loot.json`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LootAnnounceConfig {
    /// Send `SCItemAcquisitionPacket` (kind 1) to the other team members in loot range when a
    /// character picks up loot. Default **false**: the kind-1 semantics and the loot sources are
    /// INFERRED from the 1.2 client (WP-IT1), so the owner enables it after a live look at the
    /// chat tab.
    #[serde(
        rename = "announcetoparty",
        deserialize_with = "crate::config::lenient"
    )]
    pub announce_to_party: bool,
}
