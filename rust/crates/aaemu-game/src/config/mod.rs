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

mod account;
mod connection;
mod world;
pub use account::{
    AccountConfig, AccountDeleteDelayTiming, AccountRestrictionsConfig, CharactersConfig,
    DefaultPayment, RestrictionFlags, TrionConfig,
};
pub use connection::{DbConnections, MySqlConnectionSettings, NetworkConfig, WebApiConfig};
pub use world::{SeaWeatherModelType, ServerProfile, WindModelType, WorldConfig};

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
mod tests;
