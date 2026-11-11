//! `Models/Game/Configurations.cs` `DungeonsConfig` / `DungeonLoadConfig`: the `Dungeons`
//! configuration section (`Configurations/Dungeons.json`), bound into
//! [`crate::config::AppConfig`] (keys are lower-cased by the loader).

use serde::Deserialize;

use crate::config::lenient;

/// Tower defense event schedules. With no explicit `Events`, the 1.2 open-world defaults are
/// derived from the game data (`tower_def::default_events`); `Enabled: false` turns every
/// event off and a non-empty `Events` overrides the derived list. Times and day intervals come
/// from compact.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TowerDefenseConfig {
    #[serde(rename = "enabled", deserialize_with = "lenient")]
    pub enabled: bool,
    #[serde(rename = "events")]
    pub events: Vec<TowerDefenseEventConfig>,
}

impl Default for TowerDefenseConfig {
    fn default() -> Self {
        Self {
            // The 1.2 open-world rifts and the Halcyona war ran on every retail server; the
            // derived defaults need no operator action. `Enabled: false` still turns them off.
            enabled: true,
            events: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TowerDefenseEventConfig {
    #[serde(rename = "id", deserialize_with = "lenient")]
    pub id: u32,
    /// World instance id, not the client world template id.
    #[serde(rename = "worldid", deserialize_with = "lenient")]
    pub world_id: u32,
    #[serde(rename = "zonegroupid", deserialize_with = "lenient")]
    pub zone_group_id: u16,
    /// Zone key used by the 1.2 event packets.
    #[serde(rename = "zoneid", deserialize_with = "lenient")]
    pub zone_id: u32,
}

/// `DungeonLoadConfig` (one `AutoCreate` entry).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DungeonLoadConfig {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "channel", deserialize_with = "lenient")]
    pub channel: u32,
    #[serde(rename = "id", deserialize_with = "lenient")]
    pub id: u32,
}

/// `Dungeons.EntryLimit` (Rust only): the C# `CheckEntryAttemptCount` cap, 3 entries within
/// 4 hours. `Enabled` missing follows the era profile (`World.Profile`: off for `EuAuroria`,
/// on for `EuLaunch` / `Kr12`). `Count` may replace the C# 3 for every zone group except the
/// data's "unlimited" rows (`EnterCount` 1000: zone group 49 and the channel dungeons).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EntryLimitConfig {
    #[serde(
        rename = "enabled",
        default,
        deserialize_with = "crate::config::lenient_option"
    )]
    pub enabled: Option<bool>,
    #[serde(rename = "count", deserialize_with = "lenient")]
    pub count: u32,
    /// The C# 4 hour window.
    #[serde(rename = "windowseconds", deserialize_with = "lenient")]
    pub window_seconds: u32,
}

impl Default for EntryLimitConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            count: 3,
            window_seconds: 4 * 3600,
        }
    }
}

/// `DungeonsConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DungeonsConfig {
    #[serde(rename = "towerdefense")]
    pub tower_defense: TowerDefenseConfig,
    /// `AutoCleanupAfterKick`: remove a dungeon nobody is left in after a kick (QoL, not retail).
    #[serde(rename = "autocleanupafterkick", deserialize_with = "lenient")]
    pub auto_cleanup_after_kick: bool,
    /// `AutoTeamDisbandKickTime`: seconds after leaving the party before being kicked out.
    #[serde(rename = "autoteamdisbandkicktime", deserialize_with = "lenient")]
    pub auto_team_disband_kick_time: i32,
    /// `AutoCreate`: dungeon instances created at startup.
    #[serde(rename = "autocreate")]
    pub auto_create: Vec<DungeonLoadConfig>,
    /// `KickOnBattlefieldFactionChange` (Rust-only switch, default `true` = C# behaviour):
    /// entering a battlefield sets the corps faction, and `Character.SetFaction` then kicks a
    /// guild member whose guild's mother faction differs out of the guild and the party.
    /// `false` keeps guild and party membership through battlefield faction changes.
    #[serde(
        rename = "kickonbattlefieldfactionchange",
        deserialize_with = "lenient"
    )]
    pub kick_on_battlefield_faction_change: bool,
    /// `EnforceNoPvp` (Rust-only, default `true`): inside an instance whose `indun_zones` row
    /// has `pvp = false`, a character-owned attacker cannot attack another character-owned
    /// unit. The 1.2 client already resolves such a pair as non-hostile
    /// (`FUN_390281c0` / `FUN_39837a40`); `false` restores the pre-rule faction-only decision.
    #[serde(rename = "enforcenopvp", deserialize_with = "lenient")]
    pub enforce_no_pvp: bool,
    /// `NoGraveyardUseReturnPortal` (Rust-only, default `true`): in an instance whose
    /// `indun_zones` row has `has_graveyard = false`, a temple resurrection uses the closest
    /// open-world return portal (the client's ordinary "Revive", not "Revive at the entrance")
    /// before the instance's Temple Priestess. Inferred destination, see `PORTING.md`.
    #[serde(rename = "nograveyardusereturnportal", deserialize_with = "lenient")]
    pub no_graveyard_use_return_portal: bool,
    /// `EntryLimit` (Rust only): the C# visit cap, see [`EntryLimitConfig`].
    #[serde(rename = "entrylimit")]
    pub entry_limit: EntryLimitConfig,
    /// `RequireEntryItems` (Rust-only, default `true`): entering a dungeon consumes the
    /// `indun_zones.item_id` ticket. `false` neither requires nor consumes it, for servers whose
    /// era / data should not gate dungeons by item (1.2-era owner recollection, 2026-11-10).
    #[serde(rename = "requireentryitems", deserialize_with = "lenient")]
    pub require_entry_items: bool,
}

impl Default for DungeonsConfig {
    fn default() -> Self {
        Self {
            tower_defense: TowerDefenseConfig::default(),
            auto_cleanup_after_kick: true,
            auto_team_disband_kick_time: 30,
            auto_create: Vec::new(),
            kick_on_battlefield_faction_change: true,
            enforce_no_pvp: true,
            no_graveyard_use_return_portal: true,
            entry_limit: EntryLimitConfig::default(),
            require_entry_items: true,
        }
    }
}

impl DungeonsConfig {
    /// Whether the C# entry cap applies: `Dungeons.EntryLimit.Enabled` wins, otherwise the era
    /// profile decides (`World.Profile`).
    pub fn effective_entry_limit_enabled(&self, profile: crate::config::ServerProfile) -> bool {
        self.entry_limit
            .enabled
            .unwrap_or_else(|| profile.dungeon_entry_limit())
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{AppConfig, ServerProfile};

    #[test]
    fn dungeons_section_binds() {
        let v = serde_json::json!({
            "Dungeons": {
                "AutoCleanupAfterKick": false,
                "AutoTeamDisbandKickTime": 15,
                "KickOnBattlefieldFactionChange": false,
                "EnforceNoPvp": false,
                "NoGraveyardUseReturnPortal": false,
                "AutoCreate": [ { "Id": 1, "Name": "arche_mall_world", "Channel": 0 } ]
            },
            "World": { "MaxInstances": 7 }
        });
        let c = AppConfig::from_value(v).unwrap();
        assert!(!c.dungeons.auto_cleanup_after_kick);
        assert_eq!(c.dungeons.auto_team_disband_kick_time, 15);
        assert!(!c.dungeons.kick_on_battlefield_faction_change);
        assert!(!c.dungeons.enforce_no_pvp);
        assert!(!c.dungeons.no_graveyard_use_return_portal);
        assert!(
            AppConfig::default()
                .dungeons
                .kick_on_battlefield_faction_change
        );
        assert_eq!(c.dungeons.auto_create.len(), 1);
        assert_eq!(c.dungeons.auto_create[0].name, "arche_mall_world");
        assert_eq!(c.dungeons.auto_create[0].id, 1);
        assert_eq!(c.world.max_instances, 7);
        let d = AppConfig::default();
        assert!(d.dungeons.auto_cleanup_after_kick);
        assert_eq!(d.dungeons.auto_team_disband_kick_time, 30);
        assert!(d.dungeons.enforce_no_pvp);
        assert!(d.dungeons.no_graveyard_use_return_portal);
        assert_eq!(d.world.max_instances, 32);
    }

    #[test]
    fn entry_limit_and_entry_items_bind_and_follow_the_profile() {
        let d = AppConfig::default();
        assert_eq!(d.dungeons.entry_limit.count, 3);
        assert_eq!(d.dungeons.entry_limit.window_seconds, 4 * 3600);
        assert!(d.dungeons.entry_limit.enabled.is_none());
        assert!(d.dungeons.require_entry_items);
        assert!(!d
            .dungeons
            .effective_entry_limit_enabled(ServerProfile::EuAuroria));
        assert!(d
            .dungeons
            .effective_entry_limit_enabled(ServerProfile::EuLaunch));
        assert!(d
            .dungeons
            .effective_entry_limit_enabled(ServerProfile::Kr12));

        let c = AppConfig::from_value(serde_json::json!({
            "World": { "Profile": "EuLaunch" },
            "Dungeons": {
                "EntryLimit": { "Enabled": false, "Count": "5", "WindowSeconds": 7200 },
                "RequireEntryItems": "false"
            }
        }))
        .unwrap();
        assert!(!c
            .dungeons
            .effective_entry_limit_enabled(ServerProfile::EuLaunch));
        assert_eq!(c.dungeons.entry_limit.count, 5);
        assert_eq!(c.dungeons.entry_limit.window_seconds, 7200);
        assert!(!c.dungeons.require_entry_items);

        // An explicit `Enabled` wins in both directions.
        let c = AppConfig::from_value(serde_json::json!({
            "Dungeons": { "EntryLimit": { "Enabled": true } }
        }))
        .unwrap();
        assert!(c
            .dungeons
            .effective_entry_limit_enabled(ServerProfile::EuAuroria));
        let c = AppConfig::from_value(serde_json::json!({
            "World": { "Profile": "Kr12" },
            "Dungeons": { "EntryLimit": { "Enabled": false } }
        }))
        .unwrap();
        assert!(!c
            .dungeons
            .effective_entry_limit_enabled(ServerProfile::Kr12));
    }

    #[test]
    fn tower_defense_schedules_bind_and_default_to_enabled() {
        // The 1.2 open-world events ran without operator action, so a missing `Enabled` keeps
        // them on; the old default (off) left the Halcyona war and every rift unstarted.
        let config = AppConfig::from_value(serde_json::json!({"Dungeons": {"TowerDefense": {
            "Enabled": "true", "Events": [{"Id": "12", "WorldId": 0, "ZoneGroupId": 5, "ZoneId": 100}]
        }}})).unwrap();
        assert!(config.dungeons.tower_defense.enabled);
        assert_eq!(config.dungeons.tower_defense.events[0].id, 12);
        assert_eq!(config.dungeons.tower_defense.events[0].zone_group_id, 5);
        assert!(AppConfig::default().dungeons.tower_defense.enabled);
        assert!(AppConfig::default()
            .dungeons
            .tower_defense
            .events
            .is_empty());
        let off = AppConfig::from_value(serde_json::json!({"Dungeons": {"TowerDefense": {
            "Enabled": false
        }}}))
        .unwrap();
        assert!(!off.dungeons.tower_defense.enabled);
    }
}
