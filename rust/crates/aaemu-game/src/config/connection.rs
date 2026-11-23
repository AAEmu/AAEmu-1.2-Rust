use super::lenient;
use serde::Deserialize;

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
