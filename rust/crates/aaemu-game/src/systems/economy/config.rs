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
    /// Strict `mail_guide` deletion for system mail whose attachments are still unclaimed.
    /// Default **false**: system mail has no player sender to return the escrow to, so it is
    /// kept until the receiver claims the attachments. An operator can enable the guide's
    /// wording (delete at the window) here.
    #[serde(
        rename = "deletesystemmailwithattachments",
        deserialize_with = "crate::config::lenient"
    )]
    pub delete_system_mail_with_attachments: bool,
    /// Unix seconds of the first start that installed the retention windows (the
    /// `2026-11-08_aaemu_game_mail_retention.sql` marker). Mail that already existed then gets
    /// `deadline = max(rule, this + window)`, so an upgrade cannot delete old mail on its first
    /// tick. 0 disables the grace; the game server fills it from the update marker at startup.
    #[serde(
        rename = "retentiongracestartunix",
        deserialize_with = "crate::config::lenient"
    )]
    pub retention_grace_start_unix: i64,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            // `mail_guide` (`ui_texts` 4465).
            read_retention_days: 5,
            unread_retention_days: 30,
            system_read_retention_days: 5,
            system_unread_retention_days: 90,
            // There is no sender to return system attachments to: keep them by default.
            delete_system_mail_with_attachments: false,
            retention_grace_start_unix: 0,
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
