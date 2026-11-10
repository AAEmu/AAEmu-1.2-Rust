//! Configuration (`AppConfiguration`, `NetworkConfig`, `DBConnectionsConfig`, `KoreaAuthOptions`).
//!
//! Mirrors the ASP.NET configuration pipeline of `Program.cs`:
//! `Config.json` <- `Config.Local.json` <- environment variables (`Section__Key`) <- command line
//! (`--Section:Key=value` / `--Section:Key value`), later sources winning. Like
//! `Microsoft.Extensions.Configuration`:
//! * keys are case-insensitive (all keys are lower-cased before binding),
//! * arrays merge element-wise by index (they are `Section:0:Key` paths in .NET),
//! * scalars may be given as strings (`"Port": "3306"`, `"AutoAccount": "true"`),
//! * `TimeSpan` values use the .NET `[d.]hh:mm[:ss[.fffffff]]` format.
//!
//! Not ported: user secrets and the `Logging` section (logging is configured via `RUST_LOG`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, Deserializer};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid configuration: {0}")]
    Bind(#[from] serde_json::Error),
    #[error("invalid configuration: {0}")]
    Validation(String),
}

/// General application configuration (`AppConfiguration`, bound from the configuration root).
#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    /// The secret key used to verify game servers when registering with the login server.
    #[serde(rename = "secretkey")]
    pub secret_key: String,
    /// Whether accounts should be created automatically if they do not exist.
    #[serde(rename = "autoaccount", default, deserialize_with = "lenient_bool")]
    pub auto_account: bool,
    /// Whether to skip host resolution of game server hostnames.
    #[serde(rename = "skiphostresolve", default, deserialize_with = "lenient_bool")]
    pub skip_host_resolve: bool,
    /// Timeout for the game server to respond to an EnterWorld request.
    #[serde(
        rename = "enterworldtimeout",
        default = "default_enter_world_timeout",
        deserialize_with = "timespan"
    )]
    pub enter_world_timeout: Duration,
    #[serde(rename = "gameservers")]
    pub game_servers: Vec<GameServerConfig>,
    /// `Network` section: public listener for game clients.
    #[serde(rename = "network")]
    pub network: NetworkConfig,
    /// `InternalNetwork` section: listener for game servers.
    #[serde(rename = "internalnetwork")]
    pub internal_network: NetworkConfig,
    /// `Connections` section.
    #[serde(rename = "connections")]
    pub connections: DbConnectionsConfig,
    /// `KoreaAuth` section.
    #[serde(rename = "koreaauth", default)]
    pub korea_auth: KoreaAuthOptions,
    /// `Login` section (Rust only: the character slots and pre-select period advertised to the
    /// client).
    #[serde(rename = "login", default)]
    pub login: LoginConfig,
    /// `TwoFactor` section: factor attempt limits.
    #[serde(rename = "twofactor", default)]
    pub two_factor: TwoFactorOptions,
    /// `Reconnect` section: game reconnect token lifetime and per-account cap.
    #[serde(rename = "reconnect", default)]
    pub reconnect: ReconnectOptions,
    /// `Security` section (Rust only): password failure limits, account creation limit and the
    /// pre-authentication timeout.
    #[serde(rename = "security", default)]
    pub security: SecurityOptions,
    /// `ConnectionStrings` section (`ConnectionStrings:MySqlConnection` overrides `Connections`).
    #[serde(rename = "connectionstrings", default)]
    pub connection_strings: HashMap<String, String>,
    /// `urls` (`ASPNETCORE_URLS`): extra HTTP endpoints, used for the health checks.
    #[serde(rename = "urls", default)]
    pub urls: Option<String>,
}

fn default_enter_world_timeout() -> Duration {
    Duration::from_secs(10)
}

/// `Login`: the account character slots and pre-select period advertised in `ACJoinResponse` and
/// `ACAuthResponse` (base slots) and `SCGetSlotCount` (the game server sends the purchased slots).
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct LoginConfig {
    /// Base character slots of every account. Must equal the game server's
    /// `Characters:MaxPerAccount`, which enforces it.
    #[serde(rename = "maxcharactersperaccount", deserialize_with = "lenient_u8")]
    pub max_characters_per_account: u8,
    /// `ACJoinResponse.afs` byte 1: "additional characters per server when using the slot
    /// increase item" (the client's expandable limit).
    #[serde(
        rename = "additionalcharactersperserver",
        deserialize_with = "lenient_u8"
    )]
    pub additional_characters_per_server: u8,
    /// Whether the client is told it is in the character pre-select period (character edit at
    /// character select). The client's Start button then sends `CSEditCharacter` instead of
    /// entering the world, and the game server applies the edit while its
    /// `Characters:PreSelectPeriod` is on.
    #[serde(rename = "preselectcharacterperiod", deserialize_with = "lenient_bool")]
    pub pre_select_character_period: bool,
}

impl Default for LoginConfig {
    fn default() -> Self {
        Self {
            max_characters_per_account:
                aaemu_commons::character_slots::DEFAULT_MAX_CHARACTERS_PER_ACCOUNT,
            additional_characters_per_server: 0,
            pre_select_character_period: false,
        }
    }
}

/// Configuration of a single game server.
#[derive(Clone, Debug, Deserialize)]
pub struct GameServerConfig {
    /// The unique identifier of the game server.
    #[serde(rename = "id", deserialize_with = "lenient_u8")]
    pub id: u8,
    /// Display name, as shown on the client's server selection screen.
    #[serde(rename = "name")]
    pub name: String,
    /// Host address (IP or domain name) of the game server; must be reachable by clients.
    #[serde(rename = "host")]
    pub host: String,
    /// Port the game server listens on for clients.
    #[serde(rename = "port", deserialize_with = "lenient_u16")]
    pub port: u16,
    /// Whether to hide the server from the client's server selection screen.
    #[serde(rename = "hidden", default, deserialize_with = "lenient_bool")]
    pub hidden: bool,
    /// World login queue: the maximum slot occupancy (accounts in the lobby/world plus pending
    /// world cookies, as reported by `GL_GAME_SERVER_LOAD`) before `CAEnterWorld` is queued.
    /// `0` (default) disables the queue: every request goes straight to the game server, which
    /// is the behaviour before this feature.
    #[serde(rename = "maxconcurrent", default, deserialize_with = "lenient_u32")]
    pub max_concurrent: u32,
    /// Maximum number of clients waiting in this world's queue. When it is reached a new
    /// `CAEnterWorld` is refused with `ACEnterWorldDenied` instead of being queued. `0` means
    /// the packet field limit (`u16::MAX`). Ignored while `MaxConcurrent` is 0.
    #[serde(
        rename = "queuelimit",
        default = "default_queue_limit",
        deserialize_with = "lenient_u16"
    )]
    pub queue_limit: u16,
}

/// Default `GameServers[].QueueLimit`.
pub const DEFAULT_QUEUE_LIMIT: u16 = 100;

fn default_queue_limit() -> u16 {
    DEFAULT_QUEUE_LIMIT
}

/// `NetworkConfig` (`PublicNetworkConfig` / `InternalNetworkConfig`).
#[derive(Clone, Debug, Deserialize)]
pub struct NetworkConfig {
    /// Host address to listen on (`*` = any).
    #[serde(rename = "host")]
    pub host: String,
    #[serde(rename = "port", deserialize_with = "lenient_u16")]
    pub port: u16,
    /// Number of simultaneous connections allowed (not enforced, as in the C# server).
    #[serde(rename = "numconnections", default, deserialize_with = "lenient_i32")]
    pub num_connections: i32,
}

/// `DBConnectionsConfig`.
#[derive(Clone, Debug, Deserialize)]
pub struct DbConnectionsConfig {
    #[serde(rename = "mysqlprovider")]
    pub mysql_provider: MySqlConnectionSettings,
    /// Apply database schema updates without an interactive prompt (`cargo xtask dev`, Docker, CI).
    #[serde(
        rename = "autoapplyupdates",
        default,
        deserialize_with = "lenient_bool"
    )]
    pub auto_apply_updates: bool,
}

/// `AAEmu.Commons.Models.MySqlConnectionSettings`.
#[derive(Clone, Debug, Deserialize)]
pub struct MySqlConnectionSettings {
    #[serde(rename = "host", default)]
    pub host: String,
    #[serde(rename = "port", default, deserialize_with = "lenient_u16")]
    pub port: u16,
    #[serde(rename = "user", default)]
    pub user: String,
    #[serde(rename = "password", default)]
    pub password: String,
    #[serde(rename = "database", default)]
    pub database: String,
}

/// `KoreaAuthOptions`: Korea challenge-response authentication (V2, AES-256 with a
/// sha256_crypt key).
///
/// Security trade-off: sha256_crypt at 100,000 rounds is approximately 3x weaker than PBKDF2@310k
/// for offline attacks against a stolen DB; at 300,000 rounds it reaches approximate parity. The
/// hash is never transmitted to clients — only the salt and round count are sent.
#[derive(Clone, Debug, Deserialize)]
pub struct KoreaAuthOptions {
    /// When `false` (default), `korea_challenge_hash` is never written to the DB and clients
    /// using Korea auth cannot log in.
    #[serde(rename = "enabled", default, deserialize_with = "lenient_bool")]
    pub enabled: bool,
    /// sha256_crypt rounds for newly computed hashes. Valid range: 1,000–999,999,999.
    #[serde(
        rename = "rounds",
        default = "default_rounds",
        deserialize_with = "lenient_i32"
    )]
    pub rounds: i32,
    /// Maximum number of OTP entry attempts before denying authentication.
    #[serde(
        rename = "maxotpattempts",
        default = "default_attempts",
        deserialize_with = "lenient_i32"
    )]
    pub max_otp_attempts: i32,
    /// Maximum number of certificate entry attempts before denying authentication.
    #[serde(
        rename = "maxcertattempts",
        default = "default_attempts",
        deserialize_with = "lenient_i32"
    )]
    pub max_cert_attempts: i32,
    /// Timeout for ARS (phone verification).
    #[serde(
        rename = "arstimeout",
        default = "default_ars_timeout",
        deserialize_with = "timespan"
    )]
    pub ars_timeout: Duration,
}

/// `TwoFactorOptions`: factor attempt limits.
///
/// The account's `user_2fa` row decides which factors are required; an account with a verified
/// factor always has to present it. These options rate-limit failed OTP and PC-certificate
/// submissions.
#[derive(Clone, Debug, Deserialize)]
pub struct TwoFactorOptions {
    /// Failed factor submissions allowed per account before it is locked. Must be >= 1.
    #[serde(
        rename = "maxotpfailures",
        default = "default_max_otp_failures",
        deserialize_with = "lenient_i32"
    )]
    pub max_otp_failures: i32,
    /// How long an account stays locked after too many failed factor submissions. Must be >= 1.
    #[serde(
        rename = "otplockoutminutes",
        default = "default_otp_lockout_minutes",
        deserialize_with = "lenient_i32"
    )]
    pub otp_lockout_minutes: i32,
}

fn default_max_otp_failures() -> i32 {
    5
}

fn default_otp_lockout_minutes() -> i32 {
    15
}

impl Default for TwoFactorOptions {
    fn default() -> Self {
        Self {
            max_otp_failures: default_max_otp_failures(),
            otp_lockout_minutes: default_otp_lockout_minutes(),
        }
    }
}

/// `SecurityOptions` (Rust only): abuse limits of the public login listener. `0` switches a limit
/// off.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SecurityOptions {
    /// Failed password attempts within `PasswordFailureWindowSeconds` after which further password
    /// attempts are refused for `PasswordBlockSeconds`. Counted per client IP and, separately,
    /// per account name.
    #[serde(rename = "passwordfailurelimit", deserialize_with = "lenient_u32")]
    pub password_failure_limit: u32,
    #[serde(
        rename = "passwordfailurewindowseconds",
        deserialize_with = "lenient_u32"
    )]
    pub password_failure_window_seconds: u32,
    #[serde(rename = "passwordblockseconds", deserialize_with = "lenient_u32")]
    pub password_block_seconds: u32,
    /// New accounts one client IP may create per hour through `AutoAccount`. Loopback addresses
    /// are exempt.
    #[serde(rename = "accountcreationsperiphour", deserialize_with = "lenient_u32")]
    pub account_creations_per_ip_hour: u32,
    /// A client connection that has not authenticated within this many seconds is closed.
    /// Authenticated connections (world list, character select) are never timed out by it.
    #[serde(rename = "preauthtimeoutseconds", deserialize_with = "lenient_u32")]
    pub pre_auth_timeout_seconds: u32,
}

impl Default for SecurityOptions {
    fn default() -> Self {
        Self {
            password_failure_limit: 10,
            password_failure_window_seconds: 300,
            password_block_seconds: 300,
            account_creations_per_ip_hour: 5,
            pre_auth_timeout_seconds: 60,
        }
    }
}

/// `ReconnectOptions`: game-server reconnect tokens (the `SCReconnectAuth` cookie).
#[derive(Clone, Debug, Deserialize)]
pub struct ReconnectOptions {
    /// How long a token issued by the game server can be used. Tokens are single-use.
    #[serde(
        rename = "tokenttl",
        default = "default_reconnect_ttl",
        deserialize_with = "timespan"
    )]
    pub token_ttl: Duration,
    /// Maximum unconsumed tokens kept per account; the oldest is evicted beyond this.
    #[serde(
        rename = "maxtokensperaccount",
        default = "default_max_tokens_per_account",
        deserialize_with = "lenient_i32"
    )]
    pub max_tokens_per_account: i32,
}

fn default_reconnect_ttl() -> Duration {
    Duration::from_secs(60)
}

fn default_max_tokens_per_account() -> i32 {
    5
}

impl Default for ReconnectOptions {
    fn default() -> Self {
        Self {
            token_ttl: default_reconnect_ttl(),
            max_tokens_per_account: default_max_tokens_per_account(),
        }
    }
}

fn default_rounds() -> i32 {
    100_000
}

fn default_attempts() -> i32 {
    3
}

fn default_ars_timeout() -> Duration {
    Duration::from_secs(180)
}

impl Default for KoreaAuthOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            rounds: default_rounds(),
            max_otp_attempts: default_attempts(),
            max_cert_attempts: default_attempts(),
            ars_timeout: default_ars_timeout(),
        }
    }
}

impl AppConfig {
    /// `ValidateDataAnnotations` / `ValidateOnStart`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let k = &self.korea_auth;
        if !(1_000..=999_999_999).contains(&k.rounds) {
            return Err(ConfigError::Validation(format!(
                "KoreaAuth:Rounds must be between 1000 and 999999999 (got {})",
                k.rounds
            )));
        }
        if k.max_otp_attempts < 1 || k.max_cert_attempts < 1 {
            return Err(ConfigError::Validation(
                "KoreaAuth:MaxOtpAttempts/MaxCertAttempts must be >= 1".into(),
            ));
        }
        if self.two_factor.max_otp_failures < 1 {
            return Err(ConfigError::Validation(
                "TwoFactor:MaxOtpFailures must be >= 1".into(),
            ));
        }
        if self.two_factor.otp_lockout_minutes < 1 {
            return Err(ConfigError::Validation(
                "TwoFactor:OtpLockoutMinutes must be >= 1".into(),
            ));
        }
        if self.reconnect.token_ttl.is_zero() {
            return Err(ConfigError::Validation(
                "Reconnect:TokenTtl must be greater than zero".into(),
            ));
        }
        if self.reconnect.max_tokens_per_account < 1 {
            return Err(ConfigError::Validation(
                "Reconnect:MaxTokensPerAccount must be >= 1".into(),
            ));
        }
        for (name, n) in [
            ("Network", &self.network),
            ("InternalNetwork", &self.internal_network),
        ] {
            if n.host.is_empty() {
                return Err(ConfigError::Validation(format!("{name}:Host is required")));
            }
        }
        Ok(())
    }

    /// The `ConnectionStrings:MySqlConnection` override, if configured.
    pub fn mysql_connection_string(&self) -> Option<&str> {
        self.connection_strings
            .get("mysqlconnection")
            .map(String::as_str)
    }

    /// Loads `Config.json` and `Config.Local.json` from `dir` (both optional), then applies
    /// environment variables and command-line arguments.
    pub fn load(dir: &Path, args: &[String]) -> Result<Self, ConfigError> {
        let mut root = Value::Object(Map::new());
        for name in ["Config.json", "Config.Local.json"] {
            let path = dir.join(name);
            if let Some(v) = read_json_file(&path)? {
                merge(&mut root, lowercase_keys(v));
            }
        }
        apply_env(&mut root, std::env::vars());
        apply_args(&mut root, args);
        Self::from_value(root)
    }

    /// Binds and validates an already-merged (lower-cased) configuration tree.
    pub fn from_value(root: Value) -> Result<Self, ConfigError> {
        let cfg: Self = serde_json::from_value(root)?;
        cfg.validate()?;
        Ok(cfg)
    }
}

fn read_json_file(path: &Path) -> Result<Option<Value>, ConfigError> {
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
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str(text)
        .map(Some)
        .map_err(|source| ConfigError::Json {
            path: path.to_path_buf(),
            source,
        })
}

/// Lower-cases every object key (configuration keys are case-insensitive).
pub fn lowercase_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (k.to_lowercase(), lowercase_keys(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(lowercase_keys).collect()),
        other => other,
    }
}

/// Deep merge with `Microsoft.Extensions.Configuration` semantics: objects merge by key, arrays
/// by index, scalars are replaced.
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(b), Value::Array(o)) => {
            for (i, v) in o.into_iter().enumerate() {
                if i < b.len() {
                    merge(&mut b[i], v);
                } else {
                    b.push(v);
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// Sets a `:`-separated (case-insensitive) key path to a string value.
pub fn set_path(root: &mut Value, path: &str, value: &str) {
    let segments: Vec<String> = path
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    if segments.is_empty() {
        return;
    }
    let mut node = root;
    for seg in &segments {
        if let (true, Ok(idx)) = (node.is_array(), seg.parse::<usize>()) {
            let arr = node.as_array_mut().unwrap();
            while arr.len() <= idx {
                arr.push(Value::Object(Map::new()));
            }
            node = &mut arr[idx];
            continue;
        }
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        node = node
            .as_object_mut()
            .unwrap()
            .entry(seg.clone())
            .or_insert(Value::Null);
    }
    *node = Value::String(value.to_string());
}

const TOP_LEVEL_KEYS: [&str; 15] = [
    "secretkey",
    "autoaccount",
    "skiphostresolve",
    "enterworldtimeout",
    "gameservers",
    "network",
    "internalnetwork",
    "connections",
    "koreaauth",
    "login",
    "twofactor",
    "reconnect",
    "security",
    "connectionstrings",
    "urls",
];

/// Environment variables: `Section__Key` maps to `Section:Key`. Only keys whose first segment is
/// a known configuration key are applied. `ASPNETCORE_URLS` maps to `urls`.
pub fn apply_env(root: &mut Value, vars: impl IntoIterator<Item = (String, String)>) {
    for (name, value) in vars {
        let key = if name.eq_ignore_ascii_case("ASPNETCORE_URLS") {
            "urls".to_string()
        } else {
            name.replace("__", ":")
        };
        let first = key.split(':').next().unwrap_or_default().to_lowercase();
        if TOP_LEVEL_KEYS.contains(&first.as_str()) {
            set_path(root, &key, &value);
        }
    }
}

/// Command line: `--Key=Value`, `/Key=Value`, `Key=Value` and `--Key Value` / `/Key Value`.
pub fn apply_args(root: &mut Value, args: &[String]) {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let (stripped, prefixed) = if let Some(s) = arg.strip_prefix("--") {
            (s, true)
        } else if let Some(s) = arg.strip_prefix('/') {
            (s, true)
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

/// Parses a .NET `TimeSpan` string: `[-][d.]hh:mm[:ss[.fffffff]]` or a plain number of days.
pub fn parse_timespan(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.starts_with('-') {
        return None; // negative timeouts are meaningless here
    }
    if !s.contains(':') {
        return s
            .parse::<u64>()
            .ok()
            .and_then(|d| d.checked_mul(86_400))
            .map(Duration::from_secs);
    }
    let (days, rest) = match s.split_once('.') {
        Some((d, r)) if !d.contains(':') => (d.parse::<u64>().ok()?, r),
        _ => (0, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let hours: u64 = parts[0].parse().ok()?;
    let minutes: u64 = parts[1].parse().ok()?;
    let (secs, nanos) = if parts.len() == 3 {
        match parts[2].split_once('.') {
            Some((sec, frac)) => {
                if !frac.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let digits: String = frac.chars().take(9).collect();
                let nanos = format!("{digits:0<9}").parse::<u32>().ok()?;
                (sec.parse::<u64>().ok()?, nanos)
            }
            None => (parts[2].parse::<u64>().ok()?, 0),
        }
    } else {
        (0, 0)
    };
    if hours > 23 || minutes > 59 || secs > 59 {
        return None;
    }
    // `hours`, `minutes` and `secs` are range-checked above, so only the day scaling
    // and the final addition can overflow.
    let time_of_day = hours * 3600 + minutes * 60 + secs;
    let total_secs = days.checked_mul(86_400)?.checked_add(time_of_day)?;
    Some(Duration::new(total_secs, nanos))
}

// ---- lenient scalar deserializers (the .NET binder converts strings) ---------------------------

fn scalar_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        other => Err(de::Error::custom(format!("expected a scalar, got {other}"))),
    }
}

fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = scalar_string(d)?;
    match s.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(de::Error::custom(format!("'{s}' is not a valid boolean"))),
    }
}

macro_rules! lenient_int {
    ($name:ident, $t:ty) => {
        fn $name<'de, D: Deserializer<'de>>(d: D) -> Result<$t, D::Error> {
            let s = scalar_string(d)?;
            s.trim()
                .parse::<$t>()
                .map_err(|_| de::Error::custom(format!("'{s}' is not a valid {}", stringify!($t))))
        }
    };
}

lenient_int!(lenient_u8, u8);
lenient_int!(lenient_u16, u16);
lenient_int!(lenient_u32, u32);
lenient_int!(lenient_i32, i32);

fn timespan<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = scalar_string(d)?;
    parse_timespan(&s).ok_or_else(|| de::Error::custom(format!("'{s}' is not a valid TimeSpan")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        lowercase_keys(json!({
            "SecretKey": "test",
            "AutoAccount": true,
            "InternalNetwork": { "Host": "*", "Port": 1234 },
            "Network": { "Host": "*", "NumConnections": 10, "Port": 1237 },
            "Connections": { "MySQLProvider": {
                "Database": "aaemu_login", "Host": "%db_host%", "Password": "%db_password%",
                "Port": "%db_port%", "User": "%db_user%" } },
            "GameServers": []
        }))
    }

    #[test]
    fn merges_local_over_base() {
        let mut root = base();
        merge(
            &mut root,
            lowercase_keys(json!({
                "connections": { "MySQLProvider": { "Host": "127.0.0.1", "Port": "3307", "User": "root", "Password": "pw" } },
                "GameServers": [ { "Host": "127.0.0.1", "Id": 1, "Name": "AAEmu", "Port": 1239 } ]
            })),
        );
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.secret_key, "test");
        assert!(cfg.auto_account);
        assert_eq!(cfg.connections.mysql_provider.port, 3307);
        assert_eq!(cfg.connections.mysql_provider.database, "aaemu_login");
        assert_eq!(cfg.game_servers.len(), 1);
        assert_eq!(cfg.game_servers[0].port, 1239);
        assert_eq!(cfg.network.port, 1237);
        assert_eq!(cfg.enter_world_timeout, Duration::from_secs(10));
        assert_eq!(cfg.korea_auth.rounds, 100_000);
    }

    #[test]
    fn security_options_default_and_bind() {
        let mut root = base();
        merge(
            &mut root,
            lowercase_keys(json!({ "Connections": { "MySQLProvider": { "Port": "3306" } } })),
        );
        let cfg = AppConfig::from_value(root.clone()).unwrap();
        let d = &cfg.security;
        assert_eq!(d.password_failure_limit, 10);
        assert_eq!(d.password_failure_window_seconds, 300);
        assert_eq!(d.password_block_seconds, 300);
        assert_eq!(d.account_creations_per_ip_hour, 5);
        assert_eq!(d.pre_auth_timeout_seconds, 60);

        apply_env(
            &mut root,
            [
                (
                    "Security__PasswordFailureLimit".to_string(),
                    "0".to_string(),
                ),
                (
                    "Security__PreAuthTimeoutSeconds".to_string(),
                    "5".to_string(),
                ),
            ],
        );
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.security.password_failure_limit, 0);
        assert_eq!(cfg.security.pre_auth_timeout_seconds, 5);
        assert_eq!(cfg.security.account_creations_per_ip_hour, 5);
    }

    #[test]
    fn game_server_queue_config_binds_and_defaults() {
        let mut root = base();
        merge(
            &mut root,
            lowercase_keys(json!({
                "Connections": { "MySQLProvider": { "Port": "3306" } },
                "GameServers": [ {
                    "Host": "127.0.0.1", "Id": 1, "Name": "AAEmu", "Port": 1239,
                    "MaxConcurrent": "2", "QueueLimit": "7"
                } ]
            })),
        );
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.game_servers[0].max_concurrent, 2);
        assert_eq!(cfg.game_servers[0].queue_limit, 7);

        // Absent settings keep the pre-queue behaviour: no capacity gate, default cap.
        let mut root = base();
        merge(
            &mut root,
            lowercase_keys(json!({
                "Connections": { "MySQLProvider": { "Port": "3306" } },
                "GameServers": [ {
                    "Host": "127.0.0.1", "Id": 1, "Name": "AAEmu", "Port": 1239
                } ]
            })),
        );
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.game_servers[0].max_concurrent, 0);
        assert_eq!(
            cfg.game_servers[0].queue_limit,
            crate::config::DEFAULT_QUEUE_LIMIT
        );

        let mut root = base();
        merge(
            &mut root,
            lowercase_keys(json!({
                "Connections": { "MySQLProvider": { "Port": "3306" } },
                "GameServers": [ {
                    "Host": "127.0.0.1", "Id": 1, "Name": "AAEmu", "Port": 1239,
                    "MaxConcurrent": "not-a-number"
                } ]
            })),
        );
        assert!(AppConfig::from_value(root).is_err());
    }

    #[test]
    fn login_slot_config_binds_the_pre_select_flag() {
        let mut root = base();
        merge(
            &mut root,
            lowercase_keys(json!({
                "Connections": { "MySQLProvider": { "Port": "3306" } },
                "Login": {
                    "MaxCharactersPerAccount": "8",
                    "AdditionalCharactersPerServer": "2",
                    "PreSelectCharacterPeriod": "false"
                }
            })),
        );
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.login.max_characters_per_account, 8);
        assert_eq!(cfg.login.additional_characters_per_server, 2);
        assert!(!cfg.login.pre_select_character_period);

        let mut root = base();
        merge(
            &mut root,
            lowercase_keys(json!({
                "Connections": { "MySQLProvider": { "Port": "3306" } },
                "Login": { "PreSelectCharacterPeriod": true }
            })),
        );
        let cfg = AppConfig::from_value(root)
            .expect("the pre-select period is allowed now that the lobby edit packet exists");
        assert!(cfg.login.pre_select_character_period);
        assert!(!LoginConfig::default().pre_select_character_period);
    }

    #[test]
    fn unresolved_placeholder_port_fails() {
        assert!(AppConfig::from_value(base()).is_err());
    }

    #[test]
    fn shipped_internal_network_binds_loopback() {
        let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
        let args: Vec<String> = [
            "--Connections:MySQLProvider:Host=127.0.0.1",
            "--Connections:MySQLProvider:Port=3306",
            "--Connections:MySQLProvider:User=aaemu",
            "--Connections:MySQLProvider:Password=secret",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cfg = AppConfig::load(&app, &args).unwrap();
        assert_eq!(cfg.internal_network.host, "127.0.0.1");
    }

    #[test]
    fn env_and_args_override() {
        let mut root = base();
        apply_env(
            &mut root,
            vec![
                (
                    "Connections__MySQLProvider__Port".to_string(),
                    "3306".to_string(),
                ),
                ("KoreaAuth__Enabled".to_string(), "True".to_string()),
                ("PATH".to_string(), "ignored".to_string()),
            ],
        );
        apply_args(
            &mut root,
            &[
                "--EnterWorldTimeout=00:00:30".into(),
                "--GameServers:0:Id".into(),
                "2".into(),
            ],
        );
        set_path(&mut root, "GameServers:0:Name", "X");
        set_path(&mut root, "GameServers:0:Host", "h");
        set_path(&mut root, "GameServers:0:Port", "1");
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.connections.mysql_provider.port, 3306);
        assert!(cfg.korea_auth.enabled);
        assert_eq!(cfg.enter_world_timeout, Duration::from_secs(30));
        assert_eq!(cfg.game_servers[0].id, 2);
    }

    #[test]
    fn timespans() {
        assert_eq!(parse_timespan("00:03:00"), Some(Duration::from_secs(180)));
        assert_eq!(
            parse_timespan("1.02:00:00"),
            Some(Duration::from_secs(93_600))
        );
        assert_eq!(
            parse_timespan("00:00:01.5"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_timespan("2"), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_timespan("10:30"), Some(Duration::from_secs(37_800)));
        assert_eq!(parse_timespan("bogus"), None);
    }

    #[test]
    fn timespan_plain_day_count_overflow_is_rejected() {
        // A plain number is a day count, so it is multiplied by 86_400 before it becomes
        // Duration seconds. u64::MAX / 86_400 (213_503_982_334_601) is the largest
        // whole-day value that still fits; larger ones must yield None, not wrap or panic.
        const MAX_DAYS: u64 = 213_503_982_334_601;
        assert_eq!(
            parse_timespan(&MAX_DAYS.to_string()),
            Some(Duration::from_secs(MAX_DAYS * 86_400))
        );
        assert_eq!(parse_timespan(&(MAX_DAYS + 1).to_string()), None);
        assert_eq!(parse_timespan(&u64::MAX.to_string()), None);
        assert_eq!(parse_timespan("2"), Some(Duration::from_secs(172_800)));
    }

    #[test]
    fn timespan_day_and_time_of_day_overflow_is_rejected() {
        // The largest representable value is u64::MAX seconds, i.e. 213_503_982_334_601
        // days plus 25_215 seconds (07:00:15); one more second overflows the addition,
        // and one more day overflows the day-to-seconds multiplication on its own.
        assert_eq!(
            parse_timespan("213503982334601.07:00:15"),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(parse_timespan("213503982334601.07:00:16"), None);
        assert_eq!(parse_timespan("213503982334602.00:00:00"), None);
        assert_eq!(parse_timespan("18446744073709551615.00:00:00"), None);
        // An ordinary day-plus-time value is unchanged.
        assert_eq!(
            parse_timespan("2.03:04:05"),
            Some(Duration::from_secs(183_845))
        );
    }

    #[test]
    fn timespan_fraction_must_be_all_digits() {
        assert_eq!(parse_timespan("00:00:01.123456789oops"), None);
        assert_eq!(parse_timespan("00:00:01.123456789\u{e9}"), None);
        // A valid long numeric fraction keeps the existing 9-digit truncation.
        assert_eq!(
            parse_timespan("00:00:01.1234567899"),
            Some(Duration::new(1, 123_456_789))
        );
        assert_eq!(
            parse_timespan("00:00:02.25"),
            Some(Duration::new(2, 250_000_000))
        );
    }

    #[test]
    fn two_factor_defaults_are_valid() {
        let mut root = base();
        set_path(&mut root, "Connections:MySQLProvider:Port", "3306");
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.two_factor.max_otp_failures, 5);
        assert_eq!(cfg.two_factor.otp_lockout_minutes, 15);
        assert_eq!(cfg.reconnect.token_ttl, Duration::from_secs(60));
        assert_eq!(cfg.reconnect.max_tokens_per_account, 5);
    }

    #[test]
    fn two_factor_parses_env_style_overrides() {
        let mut root = base();
        set_path(&mut root, "Connections:MySQLProvider:Port", "3306");
        apply_env(
            &mut root,
            vec![
                ("TwoFactor__MaxOtpFailures".to_string(), "3".to_string()),
                ("TwoFactor__OtpLockoutMinutes".to_string(), "7".to_string()),
                ("Reconnect__TokenTtl".to_string(), "00:00:30".to_string()),
                (
                    "Reconnect__MaxTokensPerAccount".to_string(),
                    "2".to_string(),
                ),
            ],
        );
        let cfg = AppConfig::from_value(root).unwrap();
        assert_eq!(cfg.two_factor.max_otp_failures, 3);
        assert_eq!(cfg.two_factor.otp_lockout_minutes, 7);
        assert_eq!(cfg.reconnect.token_ttl, Duration::from_secs(30));
        assert_eq!(cfg.reconnect.max_tokens_per_account, 2);
    }

    #[test]
    fn two_factor_refuses_zero_limits() {
        for (key, value) in [
            ("TwoFactor:MaxOtpFailures", "0"),
            ("TwoFactor:OtpLockoutMinutes", "0"),
            ("Reconnect:MaxTokensPerAccount", "0"),
            ("Reconnect:TokenTtl", "00:00:00"),
        ] {
            let mut root = base();
            set_path(&mut root, "Connections:MySQLProvider:Port", "3306");
            set_path(&mut root, key, value);
            assert!(
                matches!(AppConfig::from_value(root), Err(ConfigError::Validation(_))),
                "{key}={value} was accepted"
            );
        }
    }

    #[test]
    fn validation() {
        let mut root = base();
        set_path(&mut root, "Connections:MySQLProvider:Port", "3306");
        set_path(&mut root, "KoreaAuth:Rounds", "10");
        assert!(matches!(
            AppConfig::from_value(root),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn malformed_enter_world_timeout_fails_binding() {
        let mut root = base();
        set_path(&mut root, "Connections:MySQLProvider:Port", "3306");
        set_path(&mut root, "EnterWorldTimeout", "00:00:01.123456789oops");
        assert!(matches!(
            AppConfig::from_value(root),
            Err(ConfigError::Bind(_))
        ));
    }
}
