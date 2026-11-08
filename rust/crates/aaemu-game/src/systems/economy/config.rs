//! `Crafting`, `Loot` and `Mail` configuration (`Configurations/Crafting.json`,
//! `Configurations/Loot.json`, `Configurations/Mail.json`).

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

/// `Mail` retention (`Configurations/Mail.json`). The 1.2 client's `mail_guide`
/// (`ui_texts` 4465) says: normal mail is kept 5 days after it is read or 30 days unread;
/// system mail is kept 5 days after it is read or 90 days unread. Mail with unclaimed
/// attachments keeps the unread deadline. The commercial mailbox has its own guide
/// (`comercial_mail_guide`, `ui_texts` 4464: read mail is deleted at once, unread kept), so
/// its types are not on this timer.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    /// Days a read normal (player) mail is kept after it was read.
    #[serde(
        rename = "readretentiondays",
        deserialize_with = "crate::config::lenient"
    )]
    pub read_retention_days: i64,
    /// Days an unread normal (player) mail is kept after receipt.
    #[serde(
        rename = "unreadretentiondays",
        deserialize_with = "crate::config::lenient"
    )]
    pub unread_retention_days: i64,
    /// Days a read system mail is kept after it was read.
    #[serde(
        rename = "systemreadretentiondays",
        deserialize_with = "crate::config::lenient"
    )]
    pub system_read_retention_days: i64,
    /// Days an unread system mail is kept after receipt.
    #[serde(
        rename = "systemunreadretentiondays",
        deserialize_with = "crate::config::lenient"
    )]
    pub system_unread_retention_days: i64,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            // `mail_guide` (`ui_texts` 4465).
            read_retention_days: 5,
            unread_retention_days: 30,
            system_read_retention_days: 5,
            system_unread_retention_days: 90,
        }
    }
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
