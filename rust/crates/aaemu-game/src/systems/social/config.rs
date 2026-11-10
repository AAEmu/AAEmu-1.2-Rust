//! `Models/Game/Expeditions/ExpeditionConfig` and `Models/Game/JusticeConfig`: the `Expedition`
//! and `Justice` configuration sections (`Configurations/Expedition.json`, `Justice.json`),
//! bound into [`crate::config::AppConfig`] (keys are lower-cased by the loader).
//!
//! Note: the C# classes have no defaults for `Expedition` (a missing section makes
//! `ExpeditionManager.Load` throw on `new Regex(null)`); the Rust defaults are the values of the
//! shipped `Expedition.json` so tests and minimal configurations work.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::config::{lenient, lenient_map};
use crate::systems::social::expedition::ExpeditionRolePolicy;

/// `WorldSpawnPosition` (the members the configuration uses).
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SpawnPosition {
    #[serde(rename = "worldid", deserialize_with = "lenient")]
    pub world_id: u32,
    #[serde(rename = "zoneid", deserialize_with = "lenient")]
    pub zone_id: u32,
    #[serde(rename = "x", deserialize_with = "lenient")]
    pub x: f32,
    #[serde(rename = "y", deserialize_with = "lenient")]
    pub y: f32,
    #[serde(rename = "z", deserialize_with = "lenient")]
    pub z: f32,
    /// Degrees.
    #[serde(rename = "yaw", deserialize_with = "lenient")]
    pub yaw: f32,
}

/// `ExpeditionConfigCreate`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExpeditionCreateConfig {
    #[serde(rename = "cost", deserialize_with = "lenient")]
    pub cost: i32,
    #[serde(rename = "level", deserialize_with = "lenient")]
    pub level: u8,
    #[serde(rename = "partymembercount", deserialize_with = "lenient")]
    pub party_member_count: u8,
}

impl Default for ExpeditionCreateConfig {
    fn default() -> Self {
        Self {
            cost: 10000,
            level: 5,
            party_member_count: 5,
        }
    }
}

/// One `RolePolicies` entry (`ExpeditionRolePolicy` without the expedition id).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RolePolicyConfig {
    #[serde(rename = "role", deserialize_with = "lenient")]
    pub role: u8,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "dominiondeclare", deserialize_with = "lenient")]
    pub dominion_declare: bool,
    #[serde(rename = "invite", deserialize_with = "lenient")]
    pub invite: bool,
    #[serde(rename = "expel", deserialize_with = "lenient")]
    pub expel: bool,
    #[serde(rename = "promote", deserialize_with = "lenient")]
    pub promote: bool,
    #[serde(rename = "dismiss", deserialize_with = "lenient")]
    pub dismiss: bool,
    #[serde(rename = "chat", deserialize_with = "lenient")]
    pub chat: bool,
    #[serde(rename = "managerchat", deserialize_with = "lenient")]
    pub manager_chat: bool,
    #[serde(rename = "siegemaster", deserialize_with = "lenient")]
    pub siege_master: bool,
    #[serde(rename = "joinsiege", deserialize_with = "lenient")]
    pub join_siege: bool,
}

impl RolePolicyConfig {
    /// `rolePolicy.Clone()` with `ExpeditionId = expeditionId`.
    pub fn to_policy(&self, expedition_id: u32) -> ExpeditionRolePolicy {
        ExpeditionRolePolicy {
            expedition_id,
            role: self.role,
            name: self.name.clone(),
            dominion_declare: self.dominion_declare,
            invite: self.invite,
            expel: self.expel,
            promote: self.promote,
            dismiss: self.dismiss,
            chat: self.chat,
            manager_chat: self.manager_chat,
            siege_master: self.siege_master,
            join_siege: self.join_siege,
        }
    }
}

/// `ExpeditionConfig`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExpeditionConfig {
    #[serde(rename = "create")]
    pub create: ExpeditionCreateConfig,
    #[serde(rename = "nameregex")]
    pub name_regex: String,
    #[serde(rename = "rolepolicies")]
    pub role_policies: Vec<RolePolicyConfig>,
}

impl Default for ExpeditionConfig {
    fn default() -> Self {
        let p = |role: u8, name: &str, flags: [bool; 9]| RolePolicyConfig {
            role,
            name: name.into(),
            dominion_declare: flags[0],
            invite: flags[1],
            expel: flags[2],
            promote: flags[3],
            dismiss: flags[4],
            chat: flags[5],
            manager_chat: flags[6],
            siege_master: flags[7],
            join_siege: flags[8],
        };
        let (t, f) = (true, false);
        Self {
            create: ExpeditionCreateConfig::default(),
            name_regex: "^[a-zA-Zа-яА-Я ]{3,32}$".into(),
            role_policies: vec![
                p(0, "Apprentice", [t, f, f, f, f, t, f, f, t]),
                p(1, "Member", [t, f, f, f, f, t, f, f, t]),
                p(2, "Officer", [t, t, f, f, f, t, f, f, t]),
                p(3, "Commander", [t, t, t, f, f, t, f, t, t]),
                p(255, "General", [t, t, t, t, t, t, t, t, t]),
            ],
        }
    }
}

/// `CourtRoomConfig`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CourtRoomConfig {
    #[serde(rename = "id", deserialize_with = "lenient")]
    pub id: u32,
    #[serde(rename = "name")]
    pub name: String,
    /// `FactionsEnum`.
    #[serde(rename = "faction", deserialize_with = "lenient")]
    pub faction: u32,
    #[serde(rename = "defendant")]
    pub defendant: SpawnPosition,
    #[serde(rename = "holdingcell")]
    pub holding_cell: SpawnPosition,
}

impl Default for CourtRoomConfig {
    fn default() -> Self {
        Self {
            id: 0,
            name: "Court Room".into(),
            faction: 0,
            defendant: SpawnPosition::default(),
            holding_cell: SpawnPosition::default(),
        }
    }
}

/// `JailConfig`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JailConfig {
    #[serde(rename = "id", deserialize_with = "lenient")]
    pub id: u32,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "faction", deserialize_with = "lenient")]
    pub faction: u32,
    #[serde(rename = "pos")]
    pub pos: SpawnPosition,
}

impl Default for JailConfig {
    fn default() -> Self {
        Self {
            id: 0,
            name: "Jail".into(),
            faction: 0,
            pos: SpawnPosition::default(),
        }
    }
}

/// `JusticeConfig` (C# defaults: no court rooms / jails).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JusticeConfig {
    #[serde(rename = "keephistory", deserialize_with = "lenient")]
    pub keep_history: bool,
    #[serde(rename = "allowjuryescape", deserialize_with = "lenient")]
    pub allow_jury_escape: bool,
    #[serde(rename = "courtrooms")]
    pub court_rooms: Vec<CourtRoomConfig>,
    #[serde(rename = "jails")]
    pub jails: Vec<JailConfig>,
    #[serde(rename = "botreportprimesuspectcount", deserialize_with = "lenient")]
    pub bot_report_prime_suspect_count: i32,
}

impl Default for JusticeConfig {
    fn default() -> Self {
        Self {
            keep_history: true,
            allow_jury_escape: true,
            court_rooms: Vec::new(),
            jails: Vec::new(),
            bot_report_prime_suspect_count: 5,
        }
    }
}

/// The 1.2 chat-spam delay configuration (`Configurations/Chat.json`).
///
/// `SCChatSpamDelayPacket` carries 15 chat-type groups (one byte per client chat type) and one
/// delay per group. The client caches them and refuses a message while the previous one's delay
/// has not elapsed (native `FUN_393C2540`, localized message `0x108`). The server enforces the
/// same values, so a modified client cannot bypass the limit.
///
/// The retail values are not present in `compact.sqlite3` and were not captured, so the shipped
/// `Chat.json` keeps every delay at 0 (the previous C# behaviour, **capture-pending** case
/// `CHAT-SPAM-1`). Set `ChatGroupDelays[i]` in milliseconds to enable the limit for group `i`;
/// `ChatTypeGroups` maps the client's chat types `0..=14` to those groups. The server refuses a
/// message earlier than the group delay (minus the jitter grace) with `SCErrorMsgPacket(0x108)`,
/// the code the client's own throttle displays.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChatConfig {
    /// Group index for each chat type `0..=14` (default: one group per type).
    #[serde(rename = "chattypegroups")]
    pub chat_type_groups: Vec<u8>,
    /// Milliseconds between two messages of a group.
    #[serde(rename = "chatgroupdelays")]
    pub chat_group_delays_ms: Vec<u32>,
    /// `SCChatSpamDelayPacket.applyConfig` (retail contents unknown).
    #[serde(rename = "applyconfig")]
    pub apply_config: String,
    /// `SCChatSpamDelayPacket.detectConfig` (retail contents unknown).
    #[serde(rename = "detectconfig")]
    pub detect_config: String,
    /// Answer a channel-bound chat failure with `SCChatFailedPacket(error, channel name)`
    /// instead of the `SCErrorMsgPacket` toast.
    ///
    /// Default `false`: which failures retail 1.2 answered with `SCChatFailed` is not captured
    /// (`CHAT-FAILED-1`, capture-pending), so the default keeps the previous frames.
    #[serde(rename = "chatfailedpacket", deserialize_with = "lenient")]
    pub chat_failed_packet: bool,
    /// Minimum character level for each chat type (the 1.2 `ChatType`, e.g. `"0"` Say, `"1"`
    /// Shout, `"-3"` Whisper); a missing entry or `0` means no gate (the previous behaviour).
    ///
    /// A sender below the configured level is refused with
    /// `CHAT_CANNOT_SEND_SINCE_LEVEL_LOW` 786 and the message is not routed. Characters with
    /// `access_level >= 100` (the same GM threshold as the closed-zone bypass) are exempt.
    /// Retail's per-type levels are not evidenced, so the shipped `Chat.json` leaves the map
    /// empty (capture-pending `CHAT-LEVEL-1`).
    #[serde(rename = "minlevelbychattype", deserialize_with = "lenient_map")]
    pub min_level_by_chat_type: BTreeMap<i16, u8>,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            // The C# packet sent every chat type in group 0; tests use these defaults. The
            // shipped Chat.json assigns one group per type.
            chat_type_groups: vec![0; 15],
            // Flow tests use the defaults: keep the limit off unless the shipped Chat.json is
            // loaded. The file enables the public-channel delays.
            chat_group_delays_ms: vec![0; 15],
            apply_config: String::new(),
            detect_config: String::new(),
            chat_failed_packet: false,
            min_level_by_chat_type: BTreeMap::new(),
        }
    }
}

impl ChatConfig {
    /// Group of a client chat type (`0..=14`); out-of-range groups fall back to 0.
    pub fn type_group(&self, chat_type: usize) -> usize {
        self.chat_type_groups
            .get(chat_type)
            .map_or(chat_type, |&g| usize::from(g))
            .min(14)
    }

    /// Delay of a group; missing entries mean "no limit".
    pub fn delay_ms(&self, group: usize) -> u32 {
        self.chat_group_delays_ms.get(group).copied().unwrap_or(0)
    }

    /// `MinLevelByChatType` for a chat type; missing = `0` = no gate.
    pub fn min_level(&self, chat_type: i16) -> u8 {
        self.min_level_by_chat_type
            .get(&chat_type)
            .copied()
            .unwrap_or(0)
    }

    /// The 15 group bytes written by `SCChatSpamDelayPacket`.
    pub fn packet_groups(&self) -> [u8; 15] {
        let mut out = [0u8; 15];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.type_group(i) as u8;
        }
        out
    }

    /// The 15 delays (seconds) written by `SCChatSpamDelayPacket`.
    pub fn packet_delays(&self) -> [f32; 15] {
        let mut out = [0.0f32; 15];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.delay_ms(i) as f32 / 1000.0;
        }
        out
    }
}

/// The Rust-only `Social` section (`Configurations/Social.json`): feature switches for social
/// content whose 1.2 behaviour is not fully evidenced yet.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SocialConfig {
    /// Fill the `CSNaviOpenBounty` answer from the bounty ledger instead of the empty list.
    ///
    /// Default `false`: the C# comment records that a filled board broke the 1.2 client Lua and
    /// the cause was never confirmed on the original client (`PORTING.md`, BOUNTY-board).
    #[serde(rename = "bountyboard", deserialize_with = "lenient")]
    pub bounty_board: bool,
    /// Player nations (independence, diplomacy, national tax, monuments). `None` = the era
    /// profile decides (`World.Profile`; KR 1.2 only). An explicit `true` / `false` wins.
    #[serde(
        rename = "playernations",
        default,
        deserialize_with = "crate::config::lenient_option"
    )]
    pub player_nations: Option<bool>,
    /// Family member cap (`FAMILY_MAXIMUM`). The 1.2 compact has no family tables or
    /// `content_configs` keys and the 1.2 `Family.Write` only says `// TODO max length 8`; the
    /// 10.x server reads `family_max_count` from `content_configs` and its tests use 8, so the
    /// default here is 8 (`systems::social::family::max_members`).
    #[serde(rename = "familymaxmembers", deserialize_with = "lenient")]
    pub family_max_members: usize,
}

impl Default for SocialConfig {
    fn default() -> Self {
        Self {
            bounty_board: false,
            player_nations: None,
            family_max_members: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;

    #[test]
    fn binds_the_shipped_sections() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
        let mut merged = serde_json::Value::Object(Default::default());
        for f in [
            "Configurations/Expedition.json",
            "Configurations/Justice.json",
            "Configurations/Chat.json",
            "Configurations/Social.json",
        ] {
            let text = std::fs::read_to_string(root.join(f)).expect("config file");
            let v: serde_json::Value = serde_json::from_str(&crate::config::relax_json(
                text.trim_start_matches('\u{feff}'),
            ))
            .expect("json");
            for (k, v) in v.as_object().expect("object") {
                merged
                    .as_object_mut()
                    .expect("object")
                    .insert(k.clone(), v.clone());
            }
        }
        let cfg = AppConfig::from_value(merged).expect("bind");
        let e = &cfg.expedition;
        assert_eq!(
            (e.create.cost, e.create.level, e.create.party_member_count),
            (10000, 5, 5)
        );
        assert_eq!(e.name_regex, "^[a-zA-Zа-яА-Я ]{3,32}$");
        assert_eq!(e.role_policies.len(), 5);
        assert_eq!(e.role_policies[4].role, 255);
        assert!(e.role_policies[3].expel && !e.role_policies[3].promote);
        // The defaults equal the shipped file.
        let d = super::ExpeditionConfig::default();
        for (a, b) in d.role_policies.iter().zip(&e.role_policies) {
            assert_eq!(format!("{a:?}"), format!("{b:?}"));
        }
        let j = &cfg.justice;
        assert_eq!(j.court_rooms.len(), 4);
        assert_eq!(j.court_rooms[2].faction, 149);
        assert_eq!(j.court_rooms[0].defendant.yaw, -28.0);
        assert_eq!(j.court_rooms[3].holding_cell.x, 17020.0);
        assert_eq!(j.jails.len(), 2);
        assert_eq!(j.jails[1].pos.y, 9566.0);
        assert!(j.keep_history && j.allow_jury_escape);
        assert_eq!(j.bot_report_prime_suspect_count, 5);
        let c = &cfg.chat;
        assert_eq!(c.packet_groups(), std::array::from_fn(|i| i as u8));
        // The shipped limits are off: the retail delays are not evidenced (CHAT-SPAM-1).
        assert!(c.chat_group_delays_ms.iter().all(|&ms| ms == 0));
        assert!(c.packet_delays().iter().all(|&s| s == 0.0));
        // Retail's SCChatFailed failures are not captured (CHAT-FAILED-1): the toast stays.
        assert!(!c.chat_failed_packet);
        // Retail's per-type levels are not evidenced (CHAT-LEVEL-1): no gate.
        assert!(c.min_level_by_chat_type.is_empty());
        assert_eq!(c.min_level(1), 0);
        // The bounty board stays off until an original-client run passes (BOUNTY-board).
        assert!(!cfg.social.bounty_board);
        // Player nations follow the era profile unless the key is explicit.
        assert_eq!(cfg.social.player_nations, None);
    }

    #[test]
    fn min_level_map_binds_numeric_keys_and_string_values() {
        let cfg: super::ChatConfig = serde_json::from_value(serde_json::json!({
            "minlevelbychattype": { "1": 15, "-3": "20", "0": 0 }
        }))
        .expect("bind");
        assert_eq!(cfg.min_level(1), 15);
        assert_eq!(cfg.min_level(-3), 20);
        assert_eq!(cfg.min_level(0), 0);
        assert_eq!(cfg.min_level(7), 0);
    }

    #[test]
    fn chat_groups_and_delays_clamp_and_fall_back() {
        let c = super::ChatConfig {
            chat_type_groups: vec![9, 99],
            chat_group_delays_ms: vec![500],
            ..Default::default()
        };
        assert_eq!(c.type_group(0), 9);
        assert_eq!(c.type_group(1), 14); // clamped
        assert_eq!(c.type_group(7), 7); // missing entry = identity
        assert_eq!(c.delay_ms(9), 0); // missing delay = off
        assert_eq!(c.packet_groups()[1], 14);
    }
}
