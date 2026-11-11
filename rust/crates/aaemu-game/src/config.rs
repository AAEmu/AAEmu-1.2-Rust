//! Port of `AAEmu.Game.Models.AppConfiguration` and the configuration loading in `Program.cs`.
//!
//! The C# server binds `Microsoft.Extensions.Configuration` over, in order:
//! `Config.json`, every `Configurations/**/*.json` (ordinal-sorted), then `Config.Local.json`.
//! Later files override earlier ones key-by-key; keys are case-insensitive and scalar values may
//! be given as strings (e.g. `"Port": "1234"`). Arrays are merged element-by-element by index, as
//! the Microsoft binder flattens them to `Key:0`, `Key:1`, ...
//!
//! After the files come the user secrets (`AddUserSecrets<GameService>`: `secrets.json` of the
//! `UserSecretsId` of `AAEmu.Game.csproj`), the environment variables (`Section__Key`) and the
//! command line (`--Section:Key=value`, `/Key value`, `Key=value`), as in `BuildConfiguration`.
//!
//! Every member of `AppConfiguration` / `Models/Game/Configurations.cs` is bound (full parity);
//! unknown keys are ignored. [`ConfigCell`] gives the runtime the C# `AppConfiguration.Instance`
//! semantics: reads see the current values, GM commands (`World set ...`) and
//! `Program.ReloadConfiguration` replace them.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Implements [`ConfigEnum`] (the binder accepts the member name, case-insensitively, or its
/// numeric value).
macro_rules! impl_config_enum {
    ($t:ident { $($v:ident = $n:literal),* $(,)? }) => {
        impl ConfigEnum for $t {
            fn parse_config(s: &str) -> Option<Self> {
                $(if s.eq_ignore_ascii_case(stringify!($v)) || s == stringify!($n) {
                    return Some(Self::$v);
                })*
                None
            }
        }
    };
}

/// An enum bound from the configuration (`Enum.Parse(value, ignoreCase: true)`).
pub trait ConfigEnum: Sized {
    fn parse_config(s: &str) -> Option<Self>;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(rename = "id", deserialize_with = "lenient")]
    pub id: u8,
    #[serde(rename = "additionalesid", deserialize_with = "lenient_vec")]
    pub additionales_id: Vec<u8>,
    #[serde(rename = "secretkey")]
    pub secret_key: String,
    #[serde(rename = "connections")]
    pub connections: DbConnections,
    #[serde(rename = "network")]
    pub network: NetworkConfig,
    #[serde(rename = "streamnetwork")]
    pub stream_network: NetworkConfig,
    #[serde(rename = "loginnetwork")]
    pub login_network: NetworkConfig,
    /// `WebApiNetwork` (`None` = section missing: "WebApi will not start").
    #[serde(rename = "webapinetwork")]
    pub web_api_network: Option<WebApiConfig>,
    /// `MaxConcurencyThreadPool` (bound but unused by the C# server as well).
    #[serde(rename = "maxconcurencythreadpool", deserialize_with = "lenient")]
    pub max_concurency_thread_pool: i32,
    /// `DiscordToken` (`None` / empty = the Discord bot does not start).
    #[serde(rename = "discordtoken")]
    pub discord_token: Option<String>,
    /// `AccessLevel` (command name -> level). Keys keep their spelling (the C# lookup in
    /// `AccessLevelManager.GetLevel` is case-sensitive); filled by [`AppConfig::from_value`].
    #[serde(skip)]
    pub access_level: Vec<(String, i32)>,
    #[serde(rename = "account")]
    pub account: AccountConfig,
    /// `Characters` (Rust only: the lobby character-slot limit).
    #[serde(rename = "characters")]
    pub characters: CharactersConfig,
    #[serde(rename = "labor")]
    pub labor: CurrencyValuesConfig,
    /// `LaborOffline` (labor granted per offline tick on login).
    #[serde(rename = "laboroffline")]
    pub labor_offline: CurrencyValuesConfig,
    #[serde(rename = "credits")]
    pub credits: CurrencyValuesConfig,
    #[serde(rename = "loyalty")]
    pub loyalty: CurrencyValuesConfig,
    #[serde(rename = "world")]
    pub world: WorldConfig,
    #[serde(rename = "housing")]
    pub housing: crate::systems::housing::config::HousingConfig,
    /// `Crafting` (recipe learning gate).
    #[serde(rename = "crafting")]
    pub crafting: crate::systems::economy::config::CraftingConfig,
    /// `Loot` (`Configurations/Loot.json`: party loot announcements).
    #[serde(rename = "loot")]
    pub loot: crate::systems::economy::config::LootAnnounceConfig,
    /// `Mail` retention (`Configurations/Mail.json`; the 1.2 `mail_guide` windows).
    #[serde(rename = "mail")]
    pub mail: crate::systems::economy::config::MailConfig,
    /// `Quest` (Rust only: quest reward scaling, default off).
    #[serde(rename = "quest")]
    pub quest: crate::systems::quests::config::QuestConfig,
    /// `Music` (`systems::services::music`: user song upload sanity).
    #[serde(rename = "music")]
    pub music: crate::systems::services::music::MusicConfig,
    #[serde(rename = "clientdata")]
    pub client_data: ClientDataConfig,
    /// `Specialty`.
    #[serde(rename = "specialty")]
    pub specialty: SpecialtyConfig,
    /// `Scripts`.
    #[serde(rename = "scripts")]
    pub scripts: ScriptsConfig,
    /// `Events` (Rust only: seasonal event windows).
    #[serde(rename = "events")]
    pub events: EventsConfig,
    /// `CharacterNameRegex` (`None` = NameManager's default pattern).
    #[serde(rename = "characternameregex")]
    pub character_name_regex: Option<String>,
    /// Enables the 1.2 `allowed_name_chars` whitelist for character names. The stock client
    /// enforces the table itself, and the whitelist overrides `CharacterNameRegex` where the two
    /// disagree (digits/Cyrillic), so it is opt-in.
    #[serde(rename = "characternameuseallowedchars", deserialize_with = "lenient")]
    pub character_name_use_allowed_chars: bool,
    #[serde(rename = "defaultlanguage")]
    pub default_language: String,
    #[serde(rename = "debuginfo", deserialize_with = "lenient")]
    pub debug_info: bool,
    #[serde(rename = "debuginfolevel", deserialize_with = "lenient")]
    pub debug_info_level: u32,
    /// `Debug` (Rust only: opt-in capture aids, off by default).
    #[serde(rename = "debug")]
    pub debug: DebugConfig,
    /// `Expedition` / `Justice` / `Chat` (`systems::social::config`).
    #[serde(rename = "expedition")]
    pub expedition: crate::systems::social::config::ExpeditionConfig,
    #[serde(rename = "justice")]
    pub justice: crate::systems::social::config::JusticeConfig,
    /// Chat spam delays sent to the client and enforced by the server (`Configurations/Chat.json`).
    #[serde(rename = "chat")]
    pub chat: crate::systems::social::config::ChatConfig,
    /// `Social` (`systems::social::config`: Rust-only social feature switches).
    #[serde(rename = "social")]
    pub social: crate::systems::social::config::SocialConfig,
    /// `Dungeons` (`systems::instances::config`).
    #[serde(rename = "dungeons")]
    pub dungeons: crate::systems::instances::config::DungeonsConfig,
    /// `Interaction` (`systems::npcs::config`: NPC interaction reach).
    #[serde(rename = "interaction")]
    pub interaction: crate::systems::npcs::config::InteractionConfig,
    /// `Ai` (`systems::ai::config`, Rust only: NPC target selection / pull aggro).
    #[serde(rename = "ai")]
    pub ai: crate::systems::ai::config::AiConfig,
    /// `HeightMapsEnable` (C# default false; the shipped Config.json sets it to true).
    #[serde(rename = "heightmapsenable", deserialize_with = "lenient")]
    pub heightmaps_enable: bool,
    /// `Physics` (`systems::ai::collision::config`, Rust only: the CryPhysics collision world).
    #[serde(rename = "physics")]
    pub physics: crate::systems::ai::collision::config::PhysicsConfig,
    /// `FeatureSet` (`Configurations/FeatureSet.json`: the client feature switches).
    #[serde(rename = "featureset")]
    pub feature_set: crate::data::features::FeatureSetConfig,
    /// `FileManager.AppPath`: the directory holding `Config.json` and `Data/compact.sqlite3`.
    #[serde(skip)]
    pub app_path: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            id: 0,
            additionales_id: Vec::new(),
            secret_key: String::new(),
            connections: DbConnections::default(),
            network: NetworkConfig::default(),
            stream_network: NetworkConfig::default(),
            login_network: NetworkConfig::default(),
            web_api_network: None,
            max_concurency_thread_pool: 0,
            discord_token: None,
            access_level: Vec::new(),
            account: AccountConfig::default(),
            characters: CharactersConfig::default(),
            labor: CurrencyValuesConfig::default(),
            labor_offline: CurrencyValuesConfig::default(),
            credits: CurrencyValuesConfig::default(),
            loyalty: CurrencyValuesConfig::default(),
            world: WorldConfig::default(),
            housing: Default::default(),
            crafting: Default::default(),
            loot: Default::default(),
            mail: Default::default(),
            quest: Default::default(),
            music: Default::default(),
            client_data: ClientDataConfig::default(),
            specialty: SpecialtyConfig::default(),
            scripts: ScriptsConfig::default(),
            events: EventsConfig::default(),
            character_name_regex: None,
            character_name_use_allowed_chars: false,
            default_language: "en_us".into(),
            debug_info: true,
            debug_info_level: 100,
            debug: DebugConfig::default(),
            expedition: Default::default(),
            justice: Default::default(),
            chat: Default::default(),
            social: Default::default(),
            dungeons: Default::default(),
            interaction: Default::default(),
            ai: Default::default(),
            heightmaps_enable: false,
            physics: Default::default(),
            feature_set: Default::default(),
            app_path: PathBuf::from("."),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    #[serde(rename = "host")]
    pub host: String,
    #[serde(rename = "port", deserialize_with = "lenient")]
    pub port: u16,
    #[serde(rename = "numconnections", deserialize_with = "lenient")]
    pub num_connections: i32,
    /// Rust addition: failed `X2EnterWorld` attempts (invalid cookie or account mismatch) from
    /// one IP within `EnterWorldFailureWindowSeconds` that block that IP (`0` = throttle off).
    #[serde(rename = "enterworldfailurelimit", deserialize_with = "lenient")]
    pub enter_world_failure_limit: u32,
    /// Rust addition: the window the failures are counted in.
    #[serde(
        rename = "enterworldfailurewindowseconds",
        deserialize_with = "lenient"
    )]
    pub enter_world_failure_window_seconds: u32,
    /// Rust addition: how long an IP stays blocked once it reached the failure limit.
    #[serde(rename = "enterworldblockseconds", deserialize_with = "lenient")]
    pub enter_world_block_seconds: u32,
    /// Rust addition: a connection that has not completed `X2EnterWorld` within this many seconds
    /// is closed (`0` = never).
    #[serde(rename = "preauthtimeoutseconds", deserialize_with = "lenient")]
    pub pre_auth_timeout_seconds: u32,
    /// Rust addition: most simultaneous connections per IP that have not completed
    /// `X2EnterWorld`; further sockets are dropped at accept (`0` = unlimited).
    #[serde(rename = "maxunauthenticatedperip", deserialize_with = "lenient")]
    pub max_unauthenticated_per_ip: u32,
    /// Rust addition: sustained inbound frames per second of one connection (`0` = no limit).
    #[serde(rename = "maxframespersecond", deserialize_with = "lenient")]
    pub max_frames_per_second: u32,
    /// Rust addition: seconds of `MaxFramesPerSecond` a connection may burst above the rate.
    #[serde(rename = "frameburstseconds", deserialize_with = "lenient")]
    pub frame_burst_seconds: u32,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 0,
            num_connections: 0,
            enter_world_failure_limit: 5,
            enter_world_failure_window_seconds: 60,
            enter_world_block_seconds: 300,
            pre_auth_timeout_seconds: 30,
            max_unauthenticated_per_ip: 256,
            max_frames_per_second: 500,
            frame_burst_seconds: 5,
        }
    }
}

/// HTTP listener configuration. An absent key leaves only `/status` accessible.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct WebApiConfig {
    #[serde(rename = "host")]
    pub host: String,
    #[serde(rename = "port", deserialize_with = "lenient")]
    pub port: u16,
    #[serde(rename = "enabled", deserialize_with = "lenient")]
    pub enabled: bool,
    #[serde(rename = "apikey")]
    pub api_key: Option<String>,
}

impl Default for WebApiConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1280,
            enabled: true,
            api_key: None,
        }
    }
}

impl std::fmt::Debug for WebApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebApiConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("enabled", &self.enabled)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DbConnections {
    #[serde(rename = "mysqlprovider")]
    pub mysql_provider: MySqlConnectionSettings,
    /// `AutoApplyUpdates` (apply DB schema updates without a prompt).
    #[serde(rename = "autoapplyupdates", deserialize_with = "lenient")]
    pub auto_apply_updates: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MySqlConnectionSettings {
    #[serde(rename = "host")]
    pub host: String,
    #[serde(rename = "port", deserialize_with = "lenient")]
    pub port: u16,
    #[serde(rename = "user")]
    pub user: String,
    #[serde(rename = "password")]
    pub password: String,
    #[serde(rename = "database")]
    pub database: String,
}

impl MySqlConnectionSettings {
    pub fn connect_options(&self) -> sqlx::mysql::MySqlConnectOptions {
        sqlx::mysql::MySqlConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .username(&self.user)
            .password(&self.password)
            .database(&self.database)
    }
}

/// `AccountConfig`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AccountConfig {
    /// `NameRegex` (allowed account names).
    #[serde(rename = "nameregex")]
    pub name_regex: String,
    #[serde(rename = "accessleveldefault", deserialize_with = "lenient")]
    pub access_level_default: i32,
    #[serde(rename = "accesslevelfirstaccount", deserialize_with = "lenient")]
    pub access_level_first_account: i32,
    #[serde(rename = "accesslevelfirstcharacter", deserialize_with = "lenient")]
    pub access_level_first_character: i32,
    #[serde(rename = "deletereleasename", deserialize_with = "lenient")]
    pub delete_release_name: bool,
    #[serde(rename = "deletetimings")]
    pub delete_timings: Vec<AccountDeleteDelayTiming>,
    /// Payment state of accounts whose `accounts.pay_method` is 0 (never written by a GM or the
    /// (not yet ported) premium shop). Default `premium`: the owner decision of 2026-11-03 keeps
    /// the C# behaviour for existing databases.
    #[serde(rename = "defaultpayment", deserialize_with = "lenient_enum")]
    pub default_payment: DefaultPayment,
    /// The seven `canXxx` flags of `SCInitialConfig`, per effective payment method (Rust only,
    /// WP-J). The 1.2 client uses them to gate its own UI; the server has no restriction data,
    /// so every default is `true` (today's bytes). The per-method source is INFERRED
    /// (`ACCOUNT-RESTRICT-1`, capture-pending), not client-evidenced.
    #[serde(rename = "restrictions")]
    pub restrictions: AccountRestrictionsConfig,
    /// `SCInitialConfig.idleKickTime` (raw `u16`, `0` = off; Rust only, WP-J). The unit is
    /// **UNKNOWN** (the 1.2 `CGame::Update` consumer was not located): do not document it as
    /// minutes or seconds. The client performs the idle kick itself; the server never kicks.
    /// **Needs one real-client run** with `Account:IdleKickTime=1`: about 1 s means seconds,
    /// about 1 min means minutes. `aaemu-testclient` does not emulate the kick.
    #[serde(rename = "idlekicktime", deserialize_with = "lenient")]
    pub idle_kick_time: u16,
    /// The lobby `SCTrionConfig` values (Rust only, WP-UI4). The defaults are the formerly
    /// hard-coded test URLs. What the client does with `activate: false` is UNKNOWN (CAP-UI-5;
    /// the 1.2 consumer of the struct was not traced), so the default keeps today's `true`.
    #[serde(rename = "trion")]
    pub trion: TrionConfig,
}

/// `Account:Trion` (Rust only, WP-UI4): `SCTrionConfigPacket` sent in the lobby. The embedded
/// browser panels (`webbrowser/web_help`, `web_market`, `web_play_diary`, promotion event,
/// in-game notification) load these URLs; the defaults are the C# test values ("TODO - config
/// files"). `activate: false` is the way to hide the panels once verified, but the client's
/// reaction is UNKNOWN (CAP-UI-5).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrionConfig {
    #[serde(rename = "activate", deserialize_with = "lenient")]
    pub activate: bool,
    #[serde(rename = "auth_url")]
    pub auth_url: String,
    #[serde(rename = "platform_url")]
    pub platform_url: String,
    #[serde(rename = "commerce_url")]
    pub commerce_url: String,
}

impl Default for TrionConfig {
    fn default() -> Self {
        Self {
            activate: true,
            auth_url: "http://localhost/aaemu/login".into(),
            platform_url: "http://localhost/aaemu/platform".into(),
            commerce_url: "http://localhost/aaemu/shop".into(),
        }
    }
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            name_regex: "^[a-zA-Z0-9]{1,18}$".into(),
            access_level_default: 0,
            access_level_first_account: 100,
            access_level_first_character: 100,
            delete_release_name: false,
            delete_timings: Vec::new(),
            default_payment: DefaultPayment::Premium,
            restrictions: AccountRestrictionsConfig::default(),
            idle_kick_time: 0,
            trion: TrionConfig::default(),
        }
    }
}

/// `Account:Restrictions` (Rust only, WP-J): the seven `SCInitialConfig` `canXxx` booleans, keyed
/// by the account's effective payment method. The 1.2 client only uses them to gate its UI; the
/// server does not enforce them. INFERRED source (`ACCOUNT-RESTRICT-1`): the 1.2 client reader
/// (`FUN_397b2a00` -> `FUN_397b1fb0`) names the flags but shows no server-side source.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AccountRestrictionsConfig {
    #[serde(rename = "premium")]
    pub premium: RestrictionFlags,
    #[serde(rename = "demo")]
    pub demo: RestrictionFlags,
    #[serde(rename = "none")]
    pub none: RestrictionFlags,
}

impl AccountRestrictionsConfig {
    /// The flags sent for an effective payment method.
    pub fn for_payment(
        &self,
        method: crate::net::connection::PaymentMethodType,
    ) -> &RestrictionFlags {
        use crate::net::connection::PaymentMethodType;
        match method {
            PaymentMethodType::Premium => &self.premium,
            PaymentMethodType::Demo => &self.demo,
            PaymentMethodType::None => &self.none,
        }
    }
}

/// The seven `SCInitialConfig` `canXxx` booleans in wire order. Every default is `true`, which
/// keeps the bytes the server sent before WP-J.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RestrictionFlags {
    #[serde(rename = "canplacehouse")]
    pub can_place_house: bool,
    #[serde(rename = "canpaytax")]
    pub can_pay_tax: bool,
    #[serde(rename = "canuseauction")]
    pub can_use_auction: bool,
    #[serde(rename = "cantrade")]
    pub can_trade: bool,
    #[serde(rename = "cansendmail")]
    pub can_send_mail: bool,
    #[serde(rename = "canusebank")]
    pub can_use_bank: bool,
    #[serde(rename = "canusecopper")]
    pub can_use_copper: bool,
}

impl Default for RestrictionFlags {
    fn default() -> Self {
        Self {
            can_place_house: true,
            can_pay_tax: true,
            can_use_auction: true,
            can_trade: true,
            can_send_mail: true,
            can_use_bank: true,
            can_use_copper: true,
        }
    }
}

/// `Account:DefaultPayment` (Rust only, WP-B): the effective payment of an account whose
/// `accounts.pay_method` is still 0. The numeric values are the 1.2 `PaymentMethodType`
/// (`Premium = 1`, `Demo = 3`, `None = 5`) the client receives in `SCAccountInfo.payMethod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultPayment {
    #[default]
    Premium,
    Demo,
    None,
}

impl_config_enum!(DefaultPayment { Premium = 1, Demo = 3, None = 5 });

/// `Characters` (Rust only): the lobby character-slot limit and the pre-select period.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CharactersConfig {
    /// Base character slots of every account, before the purchased
    /// `accounts.extra_character_slots` are added. Must equal the login server's
    /// `Login:MaxCharactersPerAccount`; the login server advertises it, this server enforces it.
    /// There is no cross-process check of the two values.
    #[serde(rename = "maxperaccount", deserialize_with = "lenient")]
    pub max_per_account: u8,
    /// Whether the lobby accepts `CSEditCharacter` (character edit at character select). Must
    /// equal the login server's `Login:PreSelectCharacterPeriod`: the login server advertises
    /// the period in `ACJoinResponse.afs` and the client only sends the edit packet while it is
    /// set. There is no cross-process check of the two values.
    #[serde(rename = "preselectperiod", deserialize_with = "lenient")]
    pub pre_select_period: bool,
}

impl Default for CharactersConfig {
    fn default() -> Self {
        Self {
            max_per_account: aaemu_commons::character_slots::DEFAULT_MAX_CHARACTERS_PER_ACCOUNT,
            pre_select_period: false,
        }
    }
}

/// `AccountDeleteDelayTiming`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AccountDeleteDelayTiming {
    #[serde(rename = "level", deserialize_with = "lenient")]
    pub level: u8,
    /// Minutes.
    #[serde(rename = "delay", deserialize_with = "lenient")]
    pub delay: i32,
}

/// `WindModelType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindModelType {
    /// Retail-like: wind only along the N-S axis.
    #[default]
    Official,
    /// Wind direction rotates over the day.
    Realistic,
}

/// `SeaWeatherModelType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SeaWeatherModelType {
    #[default]
    Official,
    Realistic,
}

impl_config_enum!(WindModelType { Official = 0, Realistic = 1 });
impl_config_enum!(SeaWeatherModelType { Official = 0, Realistic = 1 });

/// The retail era the server emulates (`World.Profile`, Rust only).
///
/// The default is the owner decision of 2026-11-08: Trion EU "Auroria" (4 Nov 2014 to
/// 9 Mar 2015), i.e. the Trion 1.2 build after the Conquest of Auroria update. `EuLaunch` is
/// the Trion NA/EU launch era (12/16 Sep to 3 Nov 2014, before Auroria). `Kr12` is the XL KR
/// 1.2 data our templates come from (`r208022`). The timeline and sources are in the audit
/// research `research/trion-1.2-eu-na.md`; `PORTING.md` cites the dates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServerProfile {
    /// Trion EU "Auroria" era: level cap 50, no player nations, sieges and Auroria claims on.
    #[default]
    EuAuroria,
    /// Trion NA/EU launch era: level cap 50, no player nations, no sieges and no Auroria.
    EuLaunch,
    /// XL KR 1.2 (`r208022`): level cap 55, player nations and dominion sieges on.
    Kr12,
}

impl ServerProfile {
    /// Player level cap (`ExperienceManager.PlayerLevelCap`). Trion's 1.2 builds kept level 50
    /// until Secrets of Ayanad (2015-03-10); the KR 1.2 data goes to 55.
    pub fn player_level_cap(self) -> u8 {
        match self {
            Self::EuAuroria | Self::EuLaunch => 50,
            Self::Kr12 => 55,
        }
    }

    /// Player nations (independence, diplomacy, national tax, monuments): KR only. No public
    /// Trion 1.2 source shows player-nation founding before the later updates.
    pub fn player_nations(self) -> bool {
        matches!(self, Self::Kr12)
    }

    /// Dominion sieges and Auroria territory claims: the Conquest of Auroria update
    /// (2014-11-04 NA, 2014-11-06 EU) shipped them; the launch era did not have them.
    pub fn sieges(self) -> bool {
        !matches!(self, Self::EuLaunch)
    }

    /// The C# dungeon entry cap (`IndunManager.CheckEntryAttemptCount`: 3 entries per 4 hours).
    /// The owner's EU 1.2 recollection (2026-11-10) is that 1.2 had no entry limit (the
    /// "5 per dungeon" cap came later), so `EuAuroria` leaves it off; the other profiles keep
    /// the C# default. `Dungeons.EntryLimit.Enabled` overrides this when set.
    pub fn dungeon_entry_limit(self) -> bool {
        !matches!(self, Self::EuAuroria)
    }

    /// The Archeum Lodestone max HP of the era, over the `r208022` `housings.hp` value
    /// (1,000,000); `None` keeps the template value.
    ///
    /// `EuAuroria` is the NA Nov 2014 siege HUD's 4,100,000/4,100,000 bar
    /// (`D:/aaemu-audits/video-evidence/siege-2014-11-30-ollo.md` §2.6, grade C; the same
    /// Nov 2014 change is described in the period r/archeage thread "Wall HP went from 400k
    /// to 1.2M and the lodestone went from 1M to 4M HP"). The value is a Trion Auroria
    /// server balance change and is **not** in the `r208022` client data; the launch era had
    /// no sieges, and `Kr12` keeps the template value.
    pub fn lodestone_max_hp(self) -> Option<i32> {
        match self {
            Self::EuAuroria => Some(4_100_000),
            Self::EuLaunch | Self::Kr12 => None,
        }
    }

    /// The castle Defense Tower max HP of the era, over the `r208022` `housings.hp` value
    /// (600,000); `None` keeps the template value.
    ///
    /// `EuAuroria` is the NA Nov 2014 target frame "Nuian Defense Tower" 1,500,000 HP
    /// (`siege-2014-11-30-ollo.md` §1/§2.6, grade C). Like [`Self::lodestone_max_hp`] this is
    /// a Trion Auroria server balance value, not a `r208022` data value.
    pub fn defense_tower_max_hp(self) -> Option<i32> {
        match self {
            Self::EuAuroria => Some(1_500_000),
            Self::EuLaunch | Self::Kr12 => None,
        }
    }
}

impl_config_enum!(ServerProfile {
    EuAuroria = 0,
    EuLaunch = 1,
    Kr12 = 2
});

impl Display for ServerProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EuAuroria => "EuAuroria",
            Self::EuLaunch => "EuLaunch",
            Self::Kr12 => "Kr12",
        })
    }
}

/// `WorldConfig` (`Configurations/World.json`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorldConfig {
    /// The retail era profile (`World.Profile`, Rust only): see [`ServerProfile`].
    #[serde(rename = "profile", deserialize_with = "lenient_enum")]
    pub profile: ServerProfile,
    /// `MOTD`.
    #[serde(rename = "motd")]
    pub motd: String,
    #[serde(rename = "logoutmessage")]
    pub logout_message: String,
    /// Minutes.
    #[serde(rename = "autosaveinterval", deserialize_with = "lenient")]
    pub auto_save_interval: f64,
    #[serde(rename = "exprate", deserialize_with = "lenient")]
    pub exp_rate: f64,
    #[serde(rename = "honorrate", deserialize_with = "lenient")]
    pub honor_rate: f64,
    #[serde(rename = "pvphonorrate", deserialize_with = "lenient")]
    pub pvp_honor_rate: f64,
    #[serde(rename = "vocationrate", deserialize_with = "lenient")]
    pub vocation_rate: f64,
    #[serde(rename = "lootrate", deserialize_with = "lenient")]
    pub loot_rate: f64,
    #[serde(rename = "goldlootmultiplier", deserialize_with = "lenient")]
    pub gold_loot_multiplier: f64,
    /// `GrowthRate` (doodad growth time divisor).
    #[serde(rename = "growthrate", deserialize_with = "lenient")]
    pub growth_rate: f64,
    #[serde(rename = "daysfortaxpayment", deserialize_with = "lenient")]
    pub days_for_tax_payment: u32,
    #[serde(rename = "ignorefalldamageaccesslevel", deserialize_with = "lenient")]
    pub ignore_fall_damage_access_level: i32,
    /// Minimum effective access level at which the radar / telescope shows every object in the
    /// world regardless of the registered range (`RadarManager`, the C# had this hard-coded).
    #[serde(rename = "radargmaccesslevel", deserialize_with = "lenient")]
    pub radar_gm_access_level: i32,
    #[serde(rename = "godmode", deserialize_with = "lenient")]
    pub god_mode: bool,
    /// `MoveGuardEnabled`: the server checks every client-reported unit position against a
    /// speed budget (`world::move_guard`) and refuses jumps / gross speed hacks.
    #[serde(rename = "moveguardenabled", deserialize_with = "lenient")]
    pub move_guard_enabled: bool,
    /// `MoveGuardMaxSpeed`: sustained horizontal speed (m/s) of a player on foot or an owned
    /// mate (the budget refills at this rate of server time).
    #[serde(rename = "moveguardmaxspeed", deserialize_with = "lenient")]
    pub move_guard_max_speed: f32,
    /// `MoveGuardMaxUpSpeed`: sustained upward speed (m/s); falling is free.
    #[serde(rename = "moveguardmaxupspeed", deserialize_with = "lenient")]
    pub move_guard_max_up_speed: f32,
    /// `MoveGuardBurstSeconds`: how many seconds of speed the budget may hold (lag spikes,
    /// dash skills).
    #[serde(rename = "moveguardburstseconds", deserialize_with = "lenient")]
    pub move_guard_burst_seconds: f32,
    /// `MoveGuardVehicleMaxSpeed`: sustained speed (m/s) of a client-driven land vehicle.
    #[serde(rename = "moveguardvehiclemaxspeed", deserialize_with = "lenient")]
    pub move_guard_vehicle_max_speed: f32,
    /// `MoveGuardExemptAccessLevel`: characters at or above this access level are not checked.
    #[serde(rename = "moveguardexemptaccesslevel", deserialize_with = "lenient")]
    pub move_guard_exempt_access_level: i32,
    /// `MoveGuardKickAfterViolations`: disconnect a player after this many refused moves within
    /// `MoveGuardViolationWindowSeconds`; `0` never kicks.
    #[serde(rename = "moveguardkickafterviolations", deserialize_with = "lenient")]
    pub move_guard_kick_after_violations: u32,
    /// `MoveGuardViolationWindowSeconds`: the window of `MoveGuardKickAfterViolations`.
    #[serde(
        rename = "moveguardviolationwindowseconds",
        deserialize_with = "lenient"
    )]
    pub move_guard_violation_window_seconds: f32,
    /// `GeoDataMode` (navmesh heights and A* paths, `systems::ai::geodata`).
    #[serde(rename = "geodatamode", deserialize_with = "lenient")]
    pub geo_data_mode: bool,
    /// `PreLoadTerrain`: load every heightmap cell at startup instead of on demand.
    #[serde(rename = "preloadterrain", deserialize_with = "lenient")]
    pub pre_load_terrain: bool,
    #[serde(rename = "maxinstances", deserialize_with = "lenient")]
    pub max_instances: u32,
    #[serde(rename = "targetphysicstps", deserialize_with = "lenient")]
    pub target_physics_tps: f32,
    #[serde(rename = "windmodel", deserialize_with = "lenient_enum")]
    pub wind_model: WindModelType,
    #[serde(rename = "seaweathermodel", deserialize_with = "lenient_enum")]
    pub sea_weather_model: SeaWeatherModelType,
    #[serde(rename = "actabilityrate", deserialize_with = "lenient")]
    pub actability_rate: f64,
    #[serde(rename = "tagshareenabled", deserialize_with = "lenient")]
    pub tag_share_enabled: bool,
    #[serde(rename = "usepersistenthousedoodads", deserialize_with = "lenient")]
    pub use_persistent_house_doodads: bool,
    #[serde(rename = "explossrateatdeath", deserialize_with = "lenient")]
    pub exp_loss_rate_at_death: f32,
    #[serde(rename = "pvedurabilitylossrate", deserialize_with = "lenient")]
    pub pve_durability_loss_rate: f32,
    #[serde(rename = "pvpdurabilitylossrate", deserialize_with = "lenient")]
    pub pvp_durability_loss_rate: f32,
    #[serde(rename = "minimumexplosslevel", deserialize_with = "lenient")]
    pub minimum_exp_loss_level: i32,
    /// `PortalUseRange`: how close (metres, 2D, minus the model radii) a character must be to a
    /// portal gate NPC or navi doodad to use it (`CSUsePortal`, `CSNaviTeleport`).
    #[serde(rename = "portaluserange", deserialize_with = "lenient")]
    pub portal_use_range: f32,
    /// `NpcPrewarmRegionMargin`: how many extra regions beyond the current player visibility
    /// neighborhoods are prewarmed as players move, so nearby NPCs are already prepared before
    /// a player reaches them. Server-side prewarming only; client-side NPC visibility is still
    /// decided per player by the usual region/view distance.
    #[serde(rename = "npcprewarmregionmargin", deserialize_with = "lenient")]
    pub npc_prewarm_region_margin: u32,
    /// `NpcSpawnTickMs`: how often (milliseconds) the proximity spawn/despawn pass runs.
    #[serde(rename = "npcspawntickms", deserialize_with = "lenient")]
    pub npc_spawn_tick_ms: u64,
    /// `NpcSpawnsPerTick`: maximum number of NPCs activated per proximity pass, budgeting
    /// work on busy ticks.
    #[serde(rename = "npcspawnspertick", deserialize_with = "lenient")]
    pub npc_spawns_per_tick: u32,
    /// `NpcWarmRetentionSeconds`: how long ordinary NPC instances stay warm after the last
    /// player leaves before they are reclaimed; their AI may already sleep during this time.
    #[serde(rename = "npcwarmretentionseconds", deserialize_with = "lenient")]
    pub npc_warm_retention_seconds: u64,
    /// `NpcResidentServiceNpcs`: when true, ordinary service NPCs (quest givers, vendors, ...)
    /// are retained after being visited/prepared; this does not mean their AI never sleeps.
    #[serde(rename = "npcresidentservicenpcs", deserialize_with = "lenient")]
    pub npc_resident_service_npcs: bool,
    /// `AiWorkerThreads`: worker threads used to run AI update batches. `1` (the default)
    /// runs AI updates sequentially in order; values above `1` enable bounded concurrency
    /// over independent NPC AI updates and relax strict ordering.
    #[serde(rename = "aiworkerthreads", deserialize_with = "lenient")]
    pub ai_worker_threads: u32,
    /// `NpcSpawnScatterTrigger`: a spawning NPC that would land within this distance (m, 3D) of
    /// another live NPC is scattered instead (see `systems::npcs::stacked_npcs`).
    #[serde(rename = "npcspawnscattertrigger", deserialize_with = "lenient")]
    pub npc_spawn_scatter_trigger: f32,
    /// `NpcSpawnScatterMin`: inner radius (m) of the scatter ring.
    #[serde(rename = "npcspawnscattermin", deserialize_with = "lenient")]
    pub npc_spawn_scatter_min: f32,
    /// `NpcSpawnScatterMax`: outer radius (m) of the scatter ring; `0` disables scattering.
    #[serde(rename = "npcspawnscattermax", deserialize_with = "lenient")]
    pub npc_spawn_scatter_max: f32,
    /// `StackedNpcCheckSeconds`: interval of the stacked-NPC check; `0` disables it.
    #[serde(rename = "stackednpccheckseconds", deserialize_with = "lenient")]
    pub stacked_npc_check_seconds: u64,
    /// `StackedNpcRadius`: NPCs closer than this (m, 3D) count as stacked.
    #[serde(rename = "stackednpcradius", deserialize_with = "lenient")]
    pub stacked_npc_radius: f32,
    /// `StackedNpcMinSeconds`: how long a stack must persist before it is logged.
    #[serde(rename = "stackednpcminseconds", deserialize_with = "lenient")]
    pub stacked_npc_min_seconds: u64,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            profile: ServerProfile::default(),
            motd: String::new(),
            logout_message: String::new(),
            auto_save_interval: 5.0,
            exp_rate: 1.0,
            honor_rate: 1.0,
            pvp_honor_rate: 1.0,
            vocation_rate: 1.0,
            loot_rate: 1.0,
            gold_loot_multiplier: 1.0,
            growth_rate: 1.0,
            days_for_tax_payment: 7,
            ignore_fall_damage_access_level: 100,
            radar_gm_access_level: 100,
            god_mode: false,
            move_guard_enabled: true,
            move_guard_max_speed: 30.0,
            move_guard_max_up_speed: 30.0,
            move_guard_burst_seconds: 3.0,
            move_guard_vehicle_max_speed: 45.0,
            move_guard_exempt_access_level: 50,
            move_guard_kick_after_violations: 0,
            move_guard_violation_window_seconds: 60.0,
            geo_data_mode: false,
            pre_load_terrain: false,
            max_instances: 32,
            target_physics_tps: 25.0,
            wind_model: WindModelType::Official,
            sea_weather_model: SeaWeatherModelType::Official,
            actability_rate: 1.0,
            tag_share_enabled: false,
            use_persistent_house_doodads: false,
            exp_loss_rate_at_death: 0.05,
            pve_durability_loss_rate: 1.0,
            pvp_durability_loss_rate: 1.0,
            minimum_exp_loss_level: 10,
            // The data's `open_portal_effects.distance` is 3 m and the 1.2 tooltip places the
            // gate 5 m away; 5 m also matches `portal::NAVI_USE_RANGE` and the NPC services.
            portal_use_range: 5.0,
            npc_prewarm_region_margin: 1,
            npc_spawn_tick_ms: 250,
            npc_spawns_per_tick: 64,
            npc_warm_retention_seconds: 60,
            npc_resident_service_npcs: true,
            ai_worker_threads: 1,
            npc_spawn_scatter_trigger: 1.0,
            npc_spawn_scatter_min: 1.5,
            npc_spawn_scatter_max: 3.0,
            stacked_npc_check_seconds: 60,
            stacked_npc_radius: 0.5,
            stacked_npc_min_seconds: 120,
        }
    }
}

/// `SpecialtyConfig` (`Configurations/Specialty.json`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SpecialtyConfig {
    #[serde(rename = "maxspecialtyratio", deserialize_with = "lenient")]
    pub max_specialty_ratio: i32,
    #[serde(rename = "minspecialtyratio", deserialize_with = "lenient")]
    pub min_specialty_ratio: i32,
    #[serde(rename = "ratiodecreaseperpack", deserialize_with = "lenient")]
    pub ratio_decrease_per_pack: f64,
    #[serde(rename = "ratioincreasepertick", deserialize_with = "lenient")]
    pub ratio_increase_per_tick: f64,
    #[serde(rename = "ratiodecreasetickminutes", deserialize_with = "lenient")]
    pub ratio_decrease_tick_minutes: f64,
    #[serde(rename = "ratioregentickminutes", deserialize_with = "lenient")]
    pub ratio_regen_tick_minutes: f64,
    #[serde(rename = "tradepackmaildelayinminutes", deserialize_with = "lenient")]
    /// Delay of the trade-pack sale mail. The 1.2 client says the price plus 5 % interest
    /// arrives after 22 hours (`ui_texts` 4743 / 6195); the C# default was 480 minutes.
    pub trade_pack_mail_delay_in_minutes: f64,
    /// `SpecialtyManager` negotiation ("bargaining") bonus, percent of the pre-interest sale
    /// price. The 1.2 client shows the bonus (`ui_texts` `sell_backpack_body2`,
    /// `SPECIALTY_MARGIN` 626, `SPECIALTY_COIN_MARGIN` 677) but no formula or threshold is
    /// evidenced; 0 keeps the C# 1.2 behaviour.
    #[serde(rename = "negotiationbonuspercent", deserialize_with = "lenient")]
    pub negotiation_bonus_percent: f64,
    /// Apply the Commerce `GetLaborCostMultiplier` table to the pack-sale labor. The 1.2 C#
    /// charged the flat 60 and skill 25205 has `consume_lp = 0`, so this stays off by default
    /// (capture-pending LABOR-1).
    #[serde(rename = "scalesalelaborbycommerce", deserialize_with = "lenient")]
    pub scale_sale_labor_by_commerce: bool,
}

impl Default for SpecialtyConfig {
    fn default() -> Self {
        Self {
            max_specialty_ratio: 130,
            min_specialty_ratio: 70,
            // `0.5f` widened to double.
            ratio_decrease_per_pack: f64::from(0.5f32),
            ratio_increase_per_tick: 5.0,
            ratio_decrease_tick_minutes: 1.0,
            ratio_regen_tick_minutes: 60.0,
            // The 1.2 client says the trade-pack price, with 5 % interest, arrives by
            // mail after 22 hours (`ui_texts` 4743 / 6195); the C# used 480 minutes.
            trade_pack_mail_delay_in_minutes: 22.0 * 60.0,
            negotiation_bonus_percent: 0.0,
            scale_sale_labor_by_commerce: false,
        }
    }
}

/// `ScriptsConfig.LoadStrategyType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LoadStrategyType {
    Compilation,
    #[default]
    Reflection,
}

impl_config_enum!(LoadStrategyType { Compilation = 0, Reflection = 1 });

/// `ScriptsConfig`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ScriptsConfig {
    #[serde(rename = "loadstrategy", deserialize_with = "lenient_enum")]
    pub load_strategy: LoadStrategyType,
}

/// `Events` (Rust only: seasonal event windows that the compact data dates one-off years).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EventsConfig {
    /// `Events:Halloween`.
    #[serde(rename = "halloween")]
    pub halloween: HalloweenConfig,
    /// `Events:ScheduledEvents`.
    #[serde(rename = "scheduledevents")]
    pub scheduled_events: ScheduledEventsConfig,
}

/// `Events:ScheduledEvents` (Rust only, gap audit `sc-unused-3#3`): the allow-list of
/// `game_schedules` rows whose window edges are announced with `SCScheduledEventStartedPacket`
/// (0x1ec) / `SCScheduledEventEndedPacket` (0x1ed). Off by default: the wire format is
/// client-evidenced, but the original trigger and message text are INFERRED (no original-server
/// capture exists). An empty list announces nothing. The client keeps one active event id, so
/// configuring overlapping events means a later Started replaces the earlier one.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ScheduledEventsConfig {
    #[serde(rename = "enabled", deserialize_with = "lenient")]
    pub enabled: bool,
    /// The allow-list in announcement order (`system::npcs::schedule_events`).
    #[serde(rename = "events")]
    pub events: Vec<ScheduledEventEntry>,
}

/// One allow-listed event: the `game_schedules` row and the texts the client shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ScheduledEventEntry {
    /// `game_schedules.id` (must exist; invalid and duplicate ids are skipped at startup).
    #[serde(rename = "scheduleid", deserialize_with = "lenient")]
    pub schedule_id: i32,
    /// The banner text of the opening edge (`SCScheduledEventStartedPacket.openMsg`).
    #[serde(rename = "openmsg")]
    pub open_msg: String,
    /// The banner text of the closing edge (`SCScheduledEventEndedPacket.closeMsg`).
    #[serde(rename = "closemsg")]
    pub close_msg: String,
}

/// `Events:Halloween`: the window applied in memory to the retail Halloween rows of
/// `game_schedules` (`systems::npcs::schedule::HALLOWEEN_SCHEDULE_IDS`, dated 2013/2014;
/// `compact.sqlite3` is read-only). Applied once at startup (`main.rs`, after `GameData::load`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HalloweenConfig {
    /// Enables the in-memory window (retail rows stay expired when false).
    #[serde(rename = "enabled", deserialize_with = "lenient")]
    pub enabled: bool,
    /// Explicit UTC window start, `YYYY-MM-DDTHH:MM:SSZ` (`""` = retail window with the year
    /// rolled yearly). Both `Start` and `End` must be set to use the override.
    #[serde(rename = "start")]
    pub start: String,
    /// Explicit UTC window end, `YYYY-MM-DDTHH:MM:SSZ` (see `start`).
    #[serde(rename = "end")]
    pub end: String,
}

impl Default for HalloweenConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            start: String::new(),
            end: String::new(),
        }
    }
}

/// `Debug` (Rust only: opt-in aids for capturing the original client, off by default).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    /// `Debug:ReplyDelay` (`net::reply_delay`).
    #[serde(rename = "replydelay")]
    pub reply_delay: ReplyDelayConfig,
}

/// `Debug:ReplyDelay`: holds selected server replies for `DelayMs` so a capture can tell client
/// rules from reactions to our server. Off unless `DelayMs` is set and a selector resolves.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReplyDelayConfig {
    /// Delay applied to a selected frame, in milliseconds (`0` = off).
    #[serde(rename = "delayms", deserialize_with = "lenient")]
    pub delay_ms: u64,
    /// S->C packet names (`SCSkillStarted` or `SCSkillStartedPacket`, case-insensitive).
    #[serde(rename = "packets")]
    pub packets: Vec<String>,
    /// C->S packet names: every frame sent while such a packet is handled is delayed.
    #[serde(rename = "repliesto")]
    pub replies_to: Vec<String>,
    /// Only connections whose character has this name are affected (`None` = all).
    #[serde(rename = "charactername")]
    pub character_name: Option<String>,
    /// Once a selected frame was delayed, hold every later frame of the connection too, so the
    /// full order is preserved (see [`crate::net::reply_delay`]).
    #[serde(rename = "delayeverythingafter", deserialize_with = "lenient")]
    pub delay_everything_after: bool,
}

/// `ClientDataConfig`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClientDataConfig {
    #[serde(rename = "sources")]
    pub sources: Vec<String>,
}

/// `CurrencyValuesConfig` (Labor / Credits / Loyalty).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CurrencyValuesConfig {
    #[serde(rename = "default", deserialize_with = "lenient")]
    pub default: i32,
    #[serde(rename = "dailylogin", deserialize_with = "lenient")]
    pub daily_login: i32,
    #[serde(rename = "tickminutes", deserialize_with = "lenient")]
    pub tick_minutes: i32,
    #[serde(rename = "tickamount", deserialize_with = "lenient")]
    pub tick_amount: i32,
    #[serde(rename = "tickamountpremium", deserialize_with = "lenient")]
    pub tick_amount_premium: i32,
    /// Loyalty only: tokens per full `hourly_minutes` of in-world play (0 = off).
    #[serde(rename = "hourlyamount", deserialize_with = "lenient")]
    pub hourly_amount: i32,
    /// Loyalty only: minutes of in-world play per hourly grant (0 = off).
    #[serde(rename = "hourlyminutes", deserialize_with = "lenient")]
    pub hourly_minutes: i32,
    /// Loyalty only: most hourly tokens an account may receive in one UTC day (0 = off).
    #[serde(rename = "hourlydailymax", deserialize_with = "lenient")]
    pub hourly_daily_max: i32,
}

impl CurrencyValuesConfig {
    /// `GetTickAmount(isPremium)`.
    pub fn get_tick_amount(&self, is_premium: bool) -> i32 {
        if is_premium {
            self.tick_amount_premium
        } else {
            self.tick_amount
        }
    }
}

impl Default for CurrencyValuesConfig {
    fn default() -> Self {
        Self {
            default: 0,
            daily_login: 0,
            tick_minutes: 5,
            tick_amount: 0,
            tick_amount_premium: 0,
            hourly_amount: 0,
            hourly_minutes: 0,
            hourly_daily_max: 0,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("binding configuration: {0}")]
    Bind(serde_json::Error),
}

/// `UserSecretsId` of `AAEmu.Game.csproj` (`AddUserSecrets<GameService>()`).
pub const USER_SECRETS_ID: &str = "426ad5c3-9a72-44e9-88f5-4b5940b98c28";

/// The top-level `AppConfiguration` members (environment variables are only applied below
/// these keys; anything else could never bind).
const TOP_LEVEL_KEYS: [&str; 34] = [
    "physics",
    "featureset",
    "id",
    "additionalesid",
    "secretkey",
    "connections",
    "network",
    "streamnetwork",
    "loginnetwork",
    "webapinetwork",
    "characternameregex",
    "maxconcurencythreadpool",
    "heightmapsenable",
    "discordtoken",
    "expedition",
    "world",
    "dungeons",
    "accesslevel",
    "account",
    "characters",
    "labor",
    "laboroffline",
    "credits",
    "loyalty",
    "clientdata",
    "specialty",
    "scripts",
    "events",
    "justice",
    "social",
    "defaultlanguage",
    "debuginfo",
    "debuginfolevel",
    "debug",
];

impl AppConfig {
    /// Loads `Config.json`, `Configurations/**/*.json` and `Config.Local.json` from `app_path`
    /// (every file optional, as `AddJsonFile(file, optional: true)`), then the user secrets
    /// and the environment variables (no command line).
    pub fn load(app_path: &Path) -> Result<Self, ConfigError> {
        Self::load_with(app_path, &[])
    }

    /// `Program.BuildConfiguration(args)` + bind: the JSON files, the user secrets
    /// ([`user_secrets_path`]), the environment variables and the command line `args`.
    pub fn load_with(app_path: &Path, args: &[String]) -> Result<Self, ConfigError> {
        Self::load_from(
            app_path,
            user_secrets_path().as_deref(),
            std::env::vars(),
            args,
        )
    }

    /// [`AppConfig::load_with`] with explicit sources (tests).
    pub fn load_from(
        app_path: &Path,
        secrets: Option<&Path>,
        env: impl IntoIterator<Item = (String, String)>,
        args: &[String],
    ) -> Result<Self, ConfigError> {
        let mut merged = Value::Object(Map::new());
        for file in config_files(app_path) {
            tracing::info!("Config: {}", file.display());
            if let Some(v) = read_config_json(&file)? {
                merge(&mut merged, expand_colon_keys(v));
            }
        }
        if let Some(path) = secrets {
            if let Some(v) = read_config_json(path)? {
                merge(&mut merged, expand_colon_keys(v));
            }
        }
        apply_env(&mut merged, env);
        apply_args(&mut merged, args);
        let mut cfg = Self::from_value(merged)?;
        cfg.app_path = app_path.to_path_buf();
        Ok(cfg)
    }

    /// Binds an already merged JSON document (keys in any case).
    pub fn from_value(value: Value) -> Result<Self, ConfigError> {
        let access_level = access_levels(&value)?;
        let mut cfg: Self =
            serde_json::from_value(lowercase_keys(value)).map_err(ConfigError::Bind)?;
        cfg.access_level = access_level;
        Ok(cfg)
    }

    /// `AccessLevel[command]` with the C# lookup (exact key).
    pub fn access_level_of(&self, command: &str) -> Option<i32> {
        self.access_level
            .iter()
            .find(|(k, _)| k == command)
            .map(|(_, v)| *v)
    }

    /// Player nations: the explicit `Social.PlayerNations` value wins, otherwise the era
    /// profile decides (`World.Profile`).
    pub fn player_nations_enabled(&self) -> bool {
        self.social
            .player_nations
            .unwrap_or_else(|| self.world.profile.player_nations())
    }

    /// The inferred weekly siege calendar: the explicit `Housing.SiegeCalendar` value wins,
    /// otherwise the era profile decides (`World.Profile`). This key drives the calendar only;
    /// Auroria claims and the client siege bit follow [`Self::auroria_claims_enabled`].
    pub fn siege_calendar_enabled(&self) -> bool {
        self.housing
            .siege_calendar
            .unwrap_or_else(|| self.world.profile.sieges())
    }

    /// Auroria territory claims and the client `FeatureSet.siege` bit: they follow the era
    /// profile (`World.Profile`) and are not switched by `Housing.SiegeCalendar`, so an
    /// operator can run GM-scheduled sieges only while claims stay enabled. An explicit
    /// `FeatureSet.siege` value still overrides the client bit in
    /// [`crate::data::FeatureSet::apply_config`].
    pub fn auroria_claims_enabled(&self) -> bool {
        self.world.profile.sieges()
    }
}

/// The configuration files in `BuildConfiguration` order: `Config.json`,
/// `Configurations/**/*.json` (ordinal order of the full paths), `Config.Local.json`.
pub fn config_files(app_path: &Path) -> Vec<PathBuf> {
    let mut files = vec![app_path.join("Config.json")];
    let mut extra = Vec::new();
    collect_json(&app_path.join("Configurations"), &mut extra);
    extra.sort();
    files.extend(extra);
    files.push(app_path.join("Config.Local.json"));
    files
}

/// Reads one optional JSON configuration file (`None` when it does not exist).
fn read_config_json(path: &Path) -> Result<Option<Value>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    serde_json::from_str(&relax_json(text.trim_start_matches('\u{feff}')))
        .map(Some)
        .map_err(|source| ConfigError::Json {
            path: path.to_path_buf(),
            source,
        })
}

/// `PathHelper.GetSecretsPathFromSecretsId`: `%APPDATA%/Microsoft/UserSecrets/<id>/secrets.json`
/// on Windows, `~/.microsoft/usersecrets/<id>/secrets.json` elsewhere (`None` when the base
/// directory is unknown).
pub fn user_secrets_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| {
            PathBuf::from(d)
                .join("Microsoft")
                .join("UserSecrets")
                .join(USER_SECRETS_ID)
                .join("secrets.json")
        })
    } else {
        std::env::var_os("HOME").map(|d| {
            PathBuf::from(d)
                .join(".microsoft")
                .join("usersecrets")
                .join(USER_SECRETS_ID)
                .join("secrets.json")
        })
    }
}

/// JSON keys containing `:` are configuration paths (`"World:ExpRate": 2` in `secrets.json`).
pub fn expand_colon_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Value::Object(Map::new());
            for (k, v) in m {
                let mut nested = expand_colon_keys(v);
                for seg in k.rsplit(':').filter(|s| !s.is_empty()) {
                    let mut o = Map::new();
                    o.insert(seg.to_string(), nested);
                    nested = Value::Object(o);
                }
                merge(&mut out, nested);
            }
            out
        }
        Value::Array(a) => Value::Array(a.into_iter().map(expand_colon_keys).collect()),
        other => other,
    }
}

/// Sets a `:`-separated (case-insensitive) key path to a string value (an existing key keeps
/// its spelling; array elements are addressed by index).
pub fn set_path(root: &mut Value, path: &str, value: &str) {
    let segments: Vec<&str> = path.split(':').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return;
    }
    let mut node = root;
    for seg in segments {
        if let (true, Ok(idx)) = (node.is_array(), seg.parse::<usize>()) {
            let arr = node.as_array_mut().expect("array");
            while arr.len() <= idx {
                arr.push(Value::Object(Map::new()));
            }
            node = &mut arr[idx];
            continue;
        }
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        let obj = node.as_object_mut().expect("object");
        let key = obj
            .keys()
            .find(|k| k.eq_ignore_ascii_case(seg))
            .cloned()
            .unwrap_or_else(|| seg.to_string());
        node = obj.entry(key).or_insert(Value::Null);
    }
    *node = Value::String(value.to_string());
}

/// `AddEnvironmentVariables()`: `Section__Key` is `Section:Key`. Only keys below a known
/// `AppConfiguration` member are applied (others can never bind).
pub fn apply_env(root: &mut Value, vars: impl IntoIterator<Item = (String, String)>) {
    for (name, value) in vars {
        let key = name.replace("__", ":");
        let first = key
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if TOP_LEVEL_KEYS.contains(&first.as_str()) {
            set_path(root, &key, &value);
        }
    }
}

/// `AddCommandLine(args)`: `--Key=Value`, `/Key=Value`, `Key=Value`, `--Key Value` and
/// `/Key Value` (single-dash keys need a switch mapping in .NET and are ignored).
pub fn apply_args(root: &mut Value, args: &[String]) {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let (stripped, prefixed) = if let Some(s) = arg.strip_prefix("--") {
            (s, true)
        } else if let Some(s) = arg.strip_prefix('/') {
            (s, true)
        } else if arg.starts_with('-') {
            i += 1;
            continue;
        } else {
            (arg.as_str(), false)
        };
        if let Some((k, v)) = stripped.split_once('=') {
            set_path(root, k, v);
        } else if prefixed && i + 1 < args.len() {
            set_path(root, stripped, &args[i + 1]);
            i += 1;
        }
        i += 1;
    }
}

/// The `AccessLevel` dictionary with the keys' spelling, in `GetChildren` order (keys sorted
/// case-insensitively).
fn access_levels(root: &Value) -> Result<Vec<(String, i32)>, ConfigError> {
    let section = root.as_object().and_then(|m| {
        m.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("accesslevel"))
            .map(|(_, v)| v)
    });
    let Some(map) = section.and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        let level: i32 = parse_scalar::<i32, serde_json::Error>(v).map_err(ConfigError::Bind)?;
        out.push((k.clone(), level));
    }
    out.sort_by_key(|(k, _)| k.to_ascii_lowercase());
    Ok(out)
}

/// `AppConfiguration.Instance`: the current configuration, replaceable at runtime (GM
/// commands, `Program.ReloadConfiguration`). Reads deref to a snapshot that stays valid
/// forever: a replaced configuration is intentionally leaked (a few KiB per GM change).
pub struct ConfigCell(std::sync::RwLock<&'static AppConfig>);

impl ConfigCell {
    pub fn new(config: AppConfig) -> Self {
        Self(std::sync::RwLock::new(Box::leak(Box::new(config))))
    }

    /// The current configuration.
    pub fn get(&self) -> &'static AppConfig {
        *self.0.read().expect("config poisoned")
    }

    /// Replaces the configuration (`ReloadConfiguration`).
    pub fn replace(&self, config: AppConfig) {
        *self.0.write().expect("config poisoned") = Box::leak(Box::new(config));
    }

    /// Changes values (`AppConfiguration.Instance.World.ExpRate = v`).
    pub fn update(&self, f: impl FnOnce(&mut AppConfig)) {
        let mut guard = self.0.write().expect("config poisoned");
        let mut next = (*guard).clone();
        f(&mut next);
        *guard = Box::leak(Box::new(next));
    }
}

impl std::ops::Deref for ConfigCell {
    type Target = AppConfig;

    fn deref(&self) -> &AppConfig {
        self.get()
    }
}

impl std::fmt::Debug for ConfigCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

impl From<AppConfig> for ConfigCell {
    fn from(c: AppConfig) -> Self {
        Self::new(c)
    }
}

/// The JSON configuration provider accepts `//` and `/* */` comments and trailing commas
/// (`JsonCommentHandling.Skip`, `AllowTrailingCommas`); strip them so serde_json can parse.
pub fn relax_json(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let skip_trivia = |mut j: usize| -> usize {
        loop {
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            match (chars.get(j), chars.get(j + 1)) {
                (Some('/'), Some('/')) => {
                    while j < chars.len() && chars[j] != '\n' {
                        j += 1;
                    }
                }
                (Some('/'), Some('*')) => {
                    j += 2;
                    while j < chars.len() && !(chars[j] == '*' && chars.get(j + 1) == Some(&'/')) {
                        j += 1;
                    }
                    j += 2;
                }
                _ => return j,
            }
        }
    };

    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if matches!(chars.get(i + 1), Some('/') | Some('*')) => {
                i = skip_trivia(i);
            }
            ',' => {
                // Drop a trailing comma (next significant character closes the container).
                if !matches!(chars.get(skip_trivia(i + 1)), Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            out.push(path);
        }
    }
}

/// Merges `src` over `dst` with Microsoft.Extensions.Configuration semantics.
pub fn merge(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                let existing = d.keys().find(|dk| dk.eq_ignore_ascii_case(&k)).cloned();
                match existing {
                    Some(key) => merge(d.get_mut(&key).expect("key exists"), v),
                    None => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(d), Value::Array(s)) => {
            for (i, v) in s.into_iter().enumerate() {
                if i < d.len() {
                    merge(&mut d[i], v);
                } else {
                    d.push(v);
                }
            }
        }
        (d, s) => *d = s,
    }
}

fn lowercase_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), lowercase_keys(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(lowercase_keys).collect()),
        other => other,
    }
}

fn parse_scalar<T, E>(v: &Value) -> Result<T, E>
where
    T: FromStr,
    T::Err: Display,
    E: de::Error,
{
    let s = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => return Err(E::custom("null")),
        other => return Err(E::custom(format!("expected a scalar, got {other}"))),
    };
    // The binder accepts "True"/"False" for booleans.
    let s = if s.eq_ignore_ascii_case("true") {
        "true".to_string()
    } else if s.eq_ignore_ascii_case("false") {
        "false".to_string()
    } else {
        s
    };
    s.parse::<T>()
        .map_err(|e| E::custom(format!("invalid value {s:?}: {e}")))
}

/// Accepts a JSON number, bool or numeric string (the Microsoft binder stores everything as
/// strings).
pub(crate) fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    let v = Value::deserialize(d)?;
    parse_scalar(&v)
}

/// An optional scalar: a missing key leaves `None`, an explicit value (including a JSON
/// string or number) binds like [`lenient`]. Used where a missing key should follow the
/// [`ServerProfile`] while an explicit value wins.
pub(crate) fn lenient_option<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    let v = Value::deserialize(d)?;
    match v {
        Value::Null => Ok(None),
        other => parse_scalar(&other).map(Some),
    }
}

/// A configuration enum: its member name (any case) or numeric value.
pub(crate) fn lenient_enum<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: ConfigEnum,
{
    let v = Value::deserialize(d)?;
    let s = match &v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        other => {
            return Err(de::Error::custom(format!(
                "expected an enum value, got {other}"
            )))
        }
    };
    T::parse_config(&s).ok_or_else(|| de::Error::custom(format!("invalid enum value {s:?}")))
}

fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    match Value::deserialize(d)? {
        Value::Array(items) => items.iter().map(parse_scalar).collect(),
        Value::Null => Ok(Vec::new()),
        other => Err(de::Error::custom(format!("expected an array, got {other}"))),
    }
}

/// A JSON object whose keys and values may be numbers or numeric strings (the Microsoft
/// binder flattens `Section:Map:Key` entries to strings).
pub(crate) fn lenient_map<'de, D, K, V>(d: D) -> Result<std::collections::BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: FromStr + Ord,
    K::Err: Display,
    V: FromStr,
    V::Err: Display,
{
    match Value::deserialize(d)? {
        Value::Object(map) => {
            let mut out = std::collections::BTreeMap::new();
            for (key, value) in map {
                let key = key
                    .trim()
                    .parse::<K>()
                    .map_err(|e| de::Error::custom(format!("invalid map key {key:?}: {e}")))?;
                out.insert(key, parse_scalar(&value)?);
            }
            Ok(out)
        }
        Value::Null => Ok(std::collections::BTreeMap::new()),
        other => Err(de::Error::custom(format!(
            "expected an object, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_case_insensitively_and_binds_strings() {
        let mut base = json!({
            "Id": 1, "SecretKey": "test", "AdditionalesId": [],
            "Network": { "Host": "*", "Port": 1239, "NumConnections": 10 },
            "LoginNetwork": { "Host": "%login_host%", "Port": "%login_port%" }
        });
        merge(
            &mut base,
            json!({ "loginnetwork": { "host": "127.0.0.1", "PORT": "1234" }, "Account": { "DeleteReleaseName": "True" } }),
        );
        let cfg = AppConfig::from_value(base).unwrap();
        assert_eq!(cfg.id, 1);
        assert_eq!(cfg.network.port, 1239);
        assert_eq!(cfg.login_network.host, "127.0.0.1");
        assert_eq!(cfg.login_network.port, 1234);
        assert!(cfg.account.delete_release_name);
        assert_eq!(cfg.account.access_level_first_account, 100);
    }

    /// Test accounts receive their starting Credits/Loyalty when the game creates their account
    /// row on first login. The defaults stay 0, so a normal server is unaffected.
    #[test]
    fn test_server_starting_credits_and_loyalty_bind() {
        assert_eq!(AppConfig::default().credits.default, 0);
        assert_eq!(AppConfig::default().loyalty.default, 0);
        let cfg = AppConfig::from_value(json!({
            "Credits": { "Default": 1_000_000 },
            "Loyalty": { "Default": "5000" }
        }))
        .unwrap();
        assert_eq!(cfg.credits.default, 1_000_000);
        assert_eq!(cfg.loyalty.default, 5_000);
    }

    /// The shipped example file binds to the values the wiki page documents (a typo in the file
    /// would otherwise only show up on a test server).
    #[test]
    fn the_shipped_test_server_example_binds_to_its_documented_values() {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../app/ExampleConfig.TestServer.json"))
                .expect("the example is valid JSON");
        let cfg = AppConfig::from_value(value).unwrap();
        assert_eq!(cfg.credits.default, 1_000_000);
        assert_eq!(cfg.loyalty.default, 5_000);
    }

    #[test]
    fn characters_config_binds_the_base_slot_count() {
        let cfg = AppConfig::from_value(json!({
            "Characters": { "MaxPerAccount": "8" }
        }))
        .unwrap();
        assert_eq!(cfg.characters.max_per_account, 8);
        assert_eq!(AppConfig::default().characters.max_per_account, 6);
    }

    #[test]
    fn characters_config_binds_the_pre_select_period() {
        let cfg = AppConfig::from_value(json!({
            "Characters": { "PreSelectPeriod": "True" }
        }))
        .unwrap();
        assert!(cfg.characters.pre_select_period);
        assert!(!AppConfig::default().characters.pre_select_period);
        let cfg = AppConfig::from_value(json!({
            "Characters": { "PreSelectPeriod": false }
        }))
        .unwrap();
        assert!(!cfg.characters.pre_select_period);
    }

    #[test]
    fn account_restrictions_and_idle_kick_bind_from_config() {
        use crate::net::connection::PaymentMethodType;

        let default = AppConfig::default();
        assert_eq!(default.account.idle_kick_time, 0);
        for method in [
            PaymentMethodType::Premium,
            PaymentMethodType::Demo,
            PaymentMethodType::None,
        ] {
            let flags = default.account.restrictions.for_payment(method);
            assert!(flags.can_place_house);
            assert!(flags.can_pay_tax);
            assert!(flags.can_use_auction);
            assert!(flags.can_trade);
            assert!(flags.can_send_mail);
            assert!(flags.can_use_bank);
            assert!(flags.can_use_copper);
        }

        let cfg = AppConfig::from_value(json!({
            "Account": {
                "IdleKickTime": "1800",
                "Restrictions": {
                    "Demo": { "CanTrade": false, "CanSendMail": false }
                }
            }
        }))
        .unwrap();
        assert_eq!(cfg.account.idle_kick_time, 1800);
        let demo = cfg
            .account
            .restrictions
            .for_payment(PaymentMethodType::Demo);
        assert!(!demo.can_trade);
        assert!(!demo.can_send_mail);
        // Missing flags keep the default `true` (only the two listed are off).
        assert!(demo.can_place_house);
        assert!(demo.can_pay_tax);
        assert!(demo.can_use_auction);
        assert!(demo.can_use_bank);
        assert!(demo.can_use_copper);
        // The other methods are unaffected.
        assert!(
            cfg.account
                .restrictions
                .for_payment(PaymentMethodType::Premium)
                .can_trade
        );
        assert!(
            cfg.account
                .restrictions
                .for_payment(PaymentMethodType::None)
                .can_trade
        );
    }

    #[test]
    fn account_trion_binds_from_config() {
        let default = AppConfig::default();
        assert!(default.account.trion.activate);
        assert_eq!(
            default.account.trion.auth_url,
            "http://localhost/aaemu/login"
        );
        assert_eq!(
            default.account.trion.platform_url,
            "http://localhost/aaemu/platform"
        );
        assert_eq!(
            default.account.trion.commerce_url,
            "http://localhost/aaemu/shop"
        );

        let cfg = AppConfig::from_value(json!({
            "Account": {
                "Trion": {
                    "Activate": "False",
                    "Auth_Url": "https://auth.example.test/l",
                    "Platform_Url": "https://platform.example.test",
                    "Commerce_Url": "https://shop.example.test/c"
                }
            }
        }))
        .unwrap();
        assert!(!cfg.account.trion.activate);
        assert_eq!(cfg.account.trion.auth_url, "https://auth.example.test/l");
        assert_eq!(
            cfg.account.trion.platform_url,
            "https://platform.example.test"
        );
        assert_eq!(
            cfg.account.trion.commerce_url,
            "https://shop.example.test/c"
        );
    }

    #[test]
    fn binds_debug_reply_delay_from_json() {
        let cfg = AppConfig::from_value(json!({
            "Debug": {
                "ReplyDelay": {
                    "DelayMs": "600",
                    "Packets": ["SCSkillStarted", "SCSkillFired"],
                    "RepliesTo": ["CSStartSkill"],
                    "CharacterName": "Capture",
                    "DelayEverythingAfter": true
                }
            }
        }))
        .unwrap();
        let delay = &cfg.debug.reply_delay;
        assert_eq!(delay.delay_ms, 600);
        assert_eq!(delay.packets, ["SCSkillStarted", "SCSkillFired"]);
        assert_eq!(delay.replies_to, ["CSStartSkill"]);
        assert_eq!(delay.character_name.as_deref(), Some("Capture"));
        assert!(delay.delay_everything_after);
    }

    #[test]
    fn relaxed_json() {
        let src = "{\n // c\n \"a\": \"x//y\\\"\", /* b */ \"b\": [1, 2, // t\n],\n}";
        let v: Value = serde_json::from_str(&relax_json(src)).unwrap();
        assert_eq!(v, json!({ "a": "x//y\"", "b": [1, 2] }));
    }

    #[test]
    fn arrays_merge_by_index() {
        let mut a = json!({ "A": [1, 2, 3] });
        merge(&mut a, json!({ "a": [9] }));
        assert_eq!(a, json!({ "A": [9, 2, 3] }));
    }

    /// The shipped application directory (`aaemu-game/app/`).
    fn shipped_app_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("app")
    }

    #[test]
    fn shipped_configuration_binds_every_section() {
        let app = shipped_app_path();
        if !app.join("Config.json").exists() {
            return;
        }
        // Config.json keeps `%login_port%` / `%db_port%` placeholders (filled by the installer):
        // override them like the C# command line would.
        let args: Vec<String> = [
            "--LoginNetwork:Port=1234",
            "/Connections:MySQLProvider:Port",
            "3306",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cfg = AppConfig::load_from(&app, None, Vec::new(), &args).unwrap();
        assert_eq!(cfg.login_network.port, 1234);
        assert_eq!(cfg.connections.mysql_provider.port, 3306);
        let web = cfg.web_api_network.as_ref().expect("WebApiNetwork");
        assert_eq!((web.host.as_str(), web.port), ("127.0.0.1", 1280));
        assert!(web.enabled);
        assert!(web.api_key.is_none());
        assert_eq!(cfg.max_concurency_thread_pool, 8);
        assert!(cfg.discord_token.is_none());
        // World.json
        assert_eq!(cfg.world.motd, "Welcome to AAEmu!");
        assert_eq!(cfg.world.days_for_tax_payment, 7);
        assert!(cfg.world.geo_data_mode);
        assert_eq!(cfg.world.max_instances, 32);
        assert_eq!(cfg.world.wind_model, WindModelType::Official);
        assert_eq!(cfg.world.exp_loss_rate_at_death, 0.05);
        assert_eq!(cfg.world.minimum_exp_loss_level, 10);
        assert_eq!(cfg.world.portal_use_range, 5.0);
        // Dungeons.json
        assert_eq!(cfg.dungeons.auto_team_disband_kick_time, 15);
        assert_eq!(
            cfg.dungeons.auto_create,
            vec![crate::systems::instances::config::DungeonLoadConfig {
                name: "arche_mall_world".into(),
                channel: 0,
                id: 1
            }]
        );
        // Mail.json: the 1.2 client's `mail_guide` windows.
        assert_eq!(cfg.mail.read_retention_days, 5);
        assert_eq!(cfg.mail.unread_retention_days, 30);
        assert_eq!(cfg.mail.system_read_retention_days, 5);
        assert_eq!(cfg.mail.system_unread_retention_days, 90);
        assert!(!cfg.mail.delete_system_mail_with_attachments);
        assert_eq!(cfg.mail.retention_grace_start_unix, 0);
        // Specialty.json, AccessLevels.json, CharacterSettings.json
        assert_eq!(cfg.specialty.trade_pack_mail_delay_in_minutes, 22.0 * 60.0);
        assert_eq!(cfg.access_level_of("help"), Some(0));
        assert_eq!(cfg.access_level_of("fly"), Some(50));
        assert_eq!(cfg.access_level_of("teleport"), Some(50));
        assert_eq!(cfg.access_level_of("announce"), Some(50));
        assert_eq!(cfg.access_level_of("Announce"), None);
        assert!(cfg.account.delete_release_name);
        assert_eq!(cfg.account.name_regex, "^[a-zA-Z0-9]{1,18}$");
        assert_eq!(cfg.scripts.load_strategy, LoadStrategyType::Reflection);
        assert_eq!(cfg.client_data.sources.len(), 4);
        // Events.json / Config.json: the Halloween window defaults to the yearly roll.
        assert!(cfg.events.halloween.enabled);
        assert_eq!(cfg.events.halloween.start, "");
        assert_eq!(cfg.events.halloween.end, "");
        // FeatureSet.json: the shipped switches leave the profile-driven bits (`siege`,
        // `nations`) unset, the rest match the current bytes, and the unnamed-bit override list
        // starts empty.
        assert_eq!(cfg.feature_set.siege, None);
        assert_eq!(cfg.feature_set.nations, None);
        assert!(!cfg.feature_set.aa_point);
        assert!(!cfg.feature_set.dwarf_warborn);
        assert!(cfg.feature_set.extra_bits.is_empty());
        // The shipped file applied to the default profile and stock level caps.
        assert_eq!(cfg.world.profile, ServerProfile::EuAuroria);
        let mut fset = crate::data::FeatureSet::initialized(
            cfg.world.profile.player_level_cap(),
            crate::data::experience::MATE_LEVEL_CAP,
        );
        fset.apply_config(
            &cfg.feature_set,
            cfg.auroria_claims_enabled(),
            cfg.player_nations_enabled(),
        );
        assert_eq!(
            fset.bytes(),
            &[0x1B, 50, 0x0F, 0x0F, 0xF9, 0x29, 0x33, 0x89, 50, 0x08, 0x1a]
        );
    }

    #[test]
    fn feature_set_section_binds_switches_and_extra_bits() {
        let cfg = AppConfig::from_value(json!({
            "FeatureSet": {
                "Siege": false,
                "DwarfWarborn": "true",
                "aaPoint": true,
                "ExtraBits": { "78": true, "32": "false" }
            }
        }))
        .unwrap();
        assert_eq!(cfg.feature_set.siege, Some(false));
        assert!(cfg.feature_set.dwarf_warborn);
        assert!(cfg.feature_set.aa_point);
        assert_eq!(cfg.feature_set.extra_bits.get(&78), Some(&true));
        assert_eq!(cfg.feature_set.extra_bits.get(&32), Some(&false));

        // The section is a top-level key, so the `FeatureSet__Key` environment form binds too.
        let mut value = json!({});
        apply_env(
            &mut value,
            [("FeatureSet__Siege".to_string(), "true".to_string())],
        );
        let cfg = AppConfig::from_value(value).unwrap();
        assert_eq!(cfg.feature_set.siege, Some(true));
    }

    #[test]
    fn web_api_security_settings_bind_from_environment() {
        let mut value = json!({"WebApiNetwork": {"Host": "127.0.0.1"}});
        apply_env(
            &mut value,
            [
                (
                    "WebApiNetwork__ApiKey".into(),
                    "synthetic-environment-key".into(),
                ),
                ("WebApiNetwork__Enabled".into(), "false".into()),
            ],
        );
        let config = AppConfig::from_value(value).unwrap();
        let web = config.web_api_network.unwrap();
        assert_eq!(web.api_key.as_deref(), Some("synthetic-environment-key"));
        assert!(!web.enabled);
        assert!(AppConfig::from_value(json!({"WebApiNetwork": null}))
            .unwrap()
            .web_api_network
            .is_none());
    }

    #[test]
    fn overrides_apply_in_builder_order() {
        let dir = std::env::temp_dir().join(format!("aaemu-cfg-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Configurations")).unwrap();
        std::fs::write(
            dir.join("Config.json"),
            r#"{ "Id": 1, "World": { "ExpRate": 1.0 }, "WebApiNetwork": { "Host": "*", "Port": 1 } }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("Configurations/World.json"),
            r#"{ "World": { "ExpRate": 2.0, "WindModel": "realistic", "RadarGmAccessLevel": 90 } }"#,
        )
        .unwrap();
        let secrets = dir.join("secrets.json");
        std::fs::write(&secrets, r#"{ "World:ExpRate": 3, "DiscordToken": "tok" }"#).unwrap();
        // Secrets beat the files.
        let cfg = AppConfig::load_from(&dir, Some(&secrets), Vec::new(), &[]).unwrap();
        assert_eq!(cfg.world.exp_rate, 3.0);
        assert_eq!(cfg.world.wind_model, WindModelType::Realistic);
        assert_eq!(cfg.world.radar_gm_access_level, 90);
        assert_eq!(cfg.discord_token.as_deref(), Some("tok"));
        // Environment beats secrets; unknown top-level variables are ignored.
        let env = vec![
            ("WORLD__EXPRATE".to_string(), "4".to_string()),
            ("Path".to_string(), "C:/x".to_string()),
            ("WebApiNetwork__Port".to_string(), "1280".to_string()),
        ];
        let cfg = AppConfig::load_from(&dir, Some(&secrets), env.clone(), &[]).unwrap();
        assert_eq!(cfg.world.exp_rate, 4.0);
        assert_eq!(cfg.web_api_network.unwrap().port, 1280);
        // The command line beats everything.
        let args = vec!["World:ExpRate=5".to_string(), "-x".to_string()];
        let cfg = AppConfig::load_from(&dir, Some(&secrets), env, &args).unwrap();
        assert_eq!(cfg.world.exp_rate, 5.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_sections_keep_the_csharp_defaults() {
        let cfg = AppConfig::from_value(json!({})).unwrap();
        assert!(cfg.web_api_network.is_none());
        assert_eq!(cfg.labor_offline.tick_minutes, 5);
        assert_eq!(cfg.world.target_physics_tps, 25.0);
        assert_eq!(cfg.world.radar_gm_access_level, 100);
        assert!(cfg.dungeons.auto_cleanup_after_kick);
        assert_eq!(cfg.specialty.ratio_decrease_per_pack, f64::from(0.5f32));
        assert_eq!(cfg.credits.get_tick_amount(true), 0);
        assert!(cfg.events.halloween.enabled);
        assert_eq!(cfg.events.halloween.start, "");
        assert_eq!(cfg.events.halloween.end, "");
        let cfg = AppConfig::from_value(json!({
            "Credits": { "TickAmount": 1, "TickAmountPremium": "2" },
            "Scripts": { "LoadStrategy": 0 },
            "AccessLevel": { "Zeta": "5", "alpha": 1 }
        }))
        .unwrap();
        assert_eq!(cfg.credits.get_tick_amount(false), 1);
        assert_eq!(cfg.credits.get_tick_amount(true), 2);
        assert_eq!(cfg.scripts.load_strategy, LoadStrategyType::Compilation);
        assert_eq!(
            cfg.access_level,
            vec![("alpha".to_string(), 1), ("Zeta".to_string(), 5)]
        );
        assert!(AppConfig::from_value(json!({ "World": { "WindModel": "Nope" } })).is_err());
    }

    #[test]
    fn era_profile_defaults_and_explicit_flags_win() {
        let cfg = AppConfig::from_value(json!({})).unwrap();
        assert_eq!(cfg.world.profile, ServerProfile::EuAuroria);
        assert_eq!(cfg.world.profile.player_level_cap(), 50);
        assert!(!cfg.player_nations_enabled());
        assert!(cfg.siege_calendar_enabled());
        assert!(cfg.auroria_claims_enabled());
        assert_eq!(format!("{}", cfg.world.profile), "EuAuroria");
        assert_eq!(cfg.world.profile.lodestone_max_hp(), Some(4_100_000));
        assert_eq!(cfg.world.profile.defense_tower_max_hp(), Some(1_500_000));

        let cfg = AppConfig::from_value(json!({ "World": { "Profile": "EuLaunch" } })).unwrap();
        assert_eq!(cfg.world.profile, ServerProfile::EuLaunch);
        assert_eq!(cfg.world.profile.player_level_cap(), 50);
        assert!(!cfg.player_nations_enabled());
        assert!(!cfg.siege_calendar_enabled());
        assert!(!cfg.auroria_claims_enabled());
        assert_eq!(cfg.world.profile.lodestone_max_hp(), None);
        assert_eq!(cfg.world.profile.defense_tower_max_hp(), None);

        let cfg = AppConfig::from_value(json!({ "World": { "Profile": "Kr12" } })).unwrap();
        assert_eq!(cfg.world.profile, ServerProfile::Kr12);
        assert_eq!(cfg.world.profile.player_level_cap(), 55);
        assert!(cfg.player_nations_enabled());
        assert!(cfg.siege_calendar_enabled());
        assert!(cfg.auroria_claims_enabled());
        assert_eq!(cfg.world.profile.lodestone_max_hp(), None);
        assert_eq!(cfg.world.profile.defense_tower_max_hp(), None);

        // An explicit flag wins over the profile in both directions.
        let cfg = AppConfig::from_value(json!({
            "World": { "Profile": "EuLaunch" },
            "Social": { "PlayerNations": true },
            "Housing": { "SiegeCalendar": "true" }
        }))
        .unwrap();
        assert!(cfg.player_nations_enabled());
        assert!(cfg.siege_calendar_enabled());
        // Claims follow the launch profile; the explicit calendar does not enable them.
        assert!(!cfg.auroria_claims_enabled());
        let cfg = AppConfig::from_value(json!({
            "World": { "Profile": "Kr12" },
            "Social": { "PlayerNations": "false" },
            "Housing": { "SiegeCalendar": false }
        }))
        .unwrap();
        assert!(!cfg.player_nations_enabled());
        assert!(!cfg.siege_calendar_enabled());
        // Claims follow the KR profile; the explicit calendar does not disable them.
        assert!(cfg.auroria_claims_enabled());
        assert_eq!(cfg.world.profile.player_level_cap(), 55);

        // The key binds through the environment (`World__Profile`) and the command line.
        let mut value = json!({});
        apply_env(
            &mut value,
            [("World__Profile".to_string(), "Kr12".to_string())],
        );
        let cfg = AppConfig::from_value(value).unwrap();
        assert_eq!(cfg.world.profile, ServerProfile::Kr12);
        let mut value = json!({});
        apply_args(&mut value, &["--World:Profile=EuLaunch".to_string()]);
        let cfg = AppConfig::from_value(value).unwrap();
        assert_eq!(cfg.world.profile, ServerProfile::EuLaunch);
        assert!(AppConfig::from_value(json!({ "World": { "Profile": "Nope" } })).is_err());
    }

    #[test]
    fn siege_calendar_is_independent_of_claims_and_the_client_bit() {
        use crate::data::experience::MATE_LEVEL_CAP;
        use crate::data::{Feature, FeatureSet};

        let client_siege = |cfg: &AppConfig| {
            let mut f =
                FeatureSet::initialized(cfg.world.profile.player_level_cap(), MATE_LEVEL_CAP);
            f.apply_config(
                &cfg.feature_set,
                cfg.auroria_claims_enabled(),
                cfg.player_nations_enabled(),
            );
            f.check(Feature::Siege)
        };

        // Auroria + calendar off (GM schedules only): claims and the client bit stay on.
        let cfg = AppConfig::from_value(json!({
            "World": { "Profile": "EuAuroria" },
            "Housing": { "SiegeCalendar": false }
        }))
        .unwrap();
        assert!(!cfg.siege_calendar_enabled());
        assert!(cfg.auroria_claims_enabled());
        assert!(client_siege(&cfg));

        // Launch: claims and the client bit are off even with the calendar forced on.
        let cfg = AppConfig::from_value(json!({
            "World": { "Profile": "EuLaunch" },
            "Housing": { "SiegeCalendar": true }
        }))
        .unwrap();
        assert!(cfg.siege_calendar_enabled());
        assert!(!cfg.auroria_claims_enabled());
        assert!(!client_siege(&cfg));

        // Kr12: claims stay on with the calendar off.
        let cfg = AppConfig::from_value(json!({
            "World": { "Profile": "Kr12" },
            "Housing": { "SiegeCalendar": "false" }
        }))
        .unwrap();
        assert!(!cfg.siege_calendar_enabled());
        assert!(cfg.auroria_claims_enabled());
        assert!(client_siege(&cfg));

        // An explicit FeatureSet.siege value wins over the profile under Auroria.
        let cfg = AppConfig::from_value(json!({
            "World": { "Profile": "EuAuroria" },
            "FeatureSet": { "Siege": false }
        }))
        .unwrap();
        assert!(cfg.auroria_claims_enabled());
        assert!(!client_siege(&cfg));
    }

    #[test]
    fn world_npc_settings_default_and_bind_leniently() {
        // Missing settings keep their documented defaults.
        let cfg = AppConfig::from_value(json!({ "World": {} })).unwrap();
        assert_eq!(cfg.world.npc_prewarm_region_margin, 1);
        assert_eq!(cfg.world.npc_spawn_tick_ms, 250);
        assert_eq!(cfg.world.npc_spawns_per_tick, 64);
        assert_eq!(cfg.world.npc_warm_retention_seconds, 60);
        assert!(cfg.world.npc_resident_service_npcs);
        assert_eq!(cfg.world.ai_worker_threads, 1);
        assert_eq!(cfg.world.npc_spawn_scatter_trigger, 1.0);
        assert_eq!(cfg.world.npc_spawn_scatter_min, 1.5);
        assert_eq!(cfg.world.npc_spawn_scatter_max, 3.0);
        assert_eq!(cfg.world.stacked_npc_check_seconds, 60);
        assert_eq!(cfg.world.stacked_npc_radius, 0.5);
        assert_eq!(cfg.world.stacked_npc_min_seconds, 120);
        // Binding is case-insensitive and accepts string values.
        let cfg = AppConfig::from_value(json!({
            "world": {
                "npcprewarmregionmargin": "2",
                "NPCSpawnTickMs": "500",
                "NpcSpawnsPerTick": 8,
                "npcwarmretentionseconds": "300",
                "NpcResidentServiceNpcs": "False",
                "AIWorkerThreads": "4",
                "NpcSpawnScatterTrigger": "2",
                "npcspawnscattermin": 2.5,
                "NpcSpawnScatterMax": "0",
                "StackedNpcCheckSeconds": "30",
                "StackedNpcRadius": 0.25,
                "StackedNpcMinSeconds": "10"
            }
        }))
        .unwrap();
        assert_eq!(cfg.world.npc_prewarm_region_margin, 2);
        assert_eq!(cfg.world.npc_spawn_tick_ms, 500);
        assert_eq!(cfg.world.npc_spawns_per_tick, 8);
        assert_eq!(cfg.world.npc_warm_retention_seconds, 300);
        assert!(!cfg.world.npc_resident_service_npcs);
        assert_eq!(cfg.world.ai_worker_threads, 4);
        assert_eq!(cfg.world.npc_spawn_scatter_trigger, 2.0);
        assert_eq!(cfg.world.npc_spawn_scatter_min, 2.5);
        assert_eq!(cfg.world.npc_spawn_scatter_max, 0.0);
        assert_eq!(cfg.world.stacked_npc_check_seconds, 30);
        assert_eq!(cfg.world.stacked_npc_radius, 0.25);
        assert_eq!(cfg.world.stacked_npc_min_seconds, 10);
    }

    #[test]
    fn loot_announce_to_party_defaults_false_and_binds_leniently() {
        let cfg = AppConfig::from_value(json!({})).unwrap();
        assert!(!cfg.loot.announce_to_party);
        let cfg = AppConfig::from_value(json!({
            "Loot": { "AnnounceToParty": "True" }
        }))
        .unwrap();
        assert!(cfg.loot.announce_to_party);
    }

    #[test]
    fn events_halloween_binds_leniently() {
        let cfg = AppConfig::from_value(json!({
            "Events": { "Halloween": {
                "Enabled": "False",
                "Start": "2026-10-14T00:00:00Z",
                "End": "2026-11-01T00:00:00Z"
            } }
        }))
        .unwrap();
        assert!(!cfg.events.halloween.enabled);
        assert_eq!(cfg.events.halloween.start, "2026-10-14T00:00:00Z");
        assert_eq!(cfg.events.halloween.end, "2026-11-01T00:00:00Z");
    }

    #[test]
    fn scheduled_events_default_off_and_bind_leniently() {
        let cfg = AppConfig::from_value(json!({})).unwrap();
        assert!(!cfg.events.scheduled_events.enabled);
        assert!(cfg.events.scheduled_events.events.is_empty());

        let cfg = AppConfig::from_value(json!({
            "Events": { "ScheduledEvents": {
                "Enabled": "True",
                "Events": [
                    { "ScheduleId": "14", "OpenMsg": "Weekend dungeon open",
                      "CloseMsg": "Weekend dungeon closed" }
                ]
            } }
        }))
        .unwrap();
        assert!(cfg.events.scheduled_events.enabled);
        assert_eq!(
            cfg.events.scheduled_events.events,
            vec![ScheduledEventEntry {
                schedule_id: 14,
                open_msg: "Weekend dungeon open".into(),
                close_msg: "Weekend dungeon closed".into(),
            }]
        );
    }

    #[test]
    fn config_cell_replaces_values_at_runtime() {
        let cell = ConfigCell::new(AppConfig::default());
        let before = cell.get();
        cell.update(|c| c.world.exp_rate = 7.0);
        assert_eq!(cell.world.exp_rate, 7.0);
        // Earlier snapshots stay valid and unchanged.
        assert_eq!(before.world.exp_rate, 1.0);
        cell.replace(AppConfig::default());
        assert_eq!(cell.world.exp_rate, 1.0);
    }
}
