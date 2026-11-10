//! `LoginController`: account lookup/creation, password checks, reconnection tokens.

use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use dashmap::DashMap;
use regex::Regex;
use sqlx::MySqlPool;

use super::game::GameController;
use crate::config::{KoreaAuthOptions, ReconnectOptions, SecurityOptions};
use crate::models::{
    AccountId, GameServerId, KoreaAuthInfo, LoginDeniedReason, LoginResult, ReconnectFailure,
    ReconnectResult,
};
use crate::network::internal::InternalConnection;
use crate::packets::l2g::LgPlayerReconnect;
use crate::services::abuse::{CreationLimit, PasswordLimits};
use crate::services::password::{
    Password, PasswordKind, PasswordService, PasswordVerificationResult,
};
use crate::services::two_factor::TwoFactorService;
use crate::services::{korea, sha256_crypt};

/// Allows Unicode letters and digits (any script), plus `_ . - @`.
///
/// Faithful to the C# `^[\p{L}\p{Nd}_.\-@]{1,32}$`, including two .NET regex quirks:
/// `$` also matches before a final `\n` (so `"name\n"` is accepted), and .NET matches UTF-16
/// code units, so characters outside the BMP never match `\p{L}` (handled in [`is_valid_username`]).
static USERNAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{Nd}_.\-@]{1,32}\n?\z").expect("valid regex"));

pub fn is_valid_username(username: &str) -> bool {
    username.chars().all(|c| (c as u32) <= 0xFFFF) && USERNAME_REGEX.is_match(username)
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `string.ReplaceLineEndings(" ")` for log output.
fn log_safe(s: &str) -> String {
    s.replace("\r\n", " ").replace(
        ['\r', '\n', '\u{0085}', '\u{2028}', '\u{2029}', '\u{000C}'],
        " ",
    )
}

/// The columns of `users` the login path uses.
type UserRow = (u64, Option<String>, Option<String>, i64, i64);

/// One issued reconnect token. The token itself is the map key.
struct ReconnectToken {
    account_id: AccountId,
    /// The client IP of the fully authenticated login that the token was issued for.
    client_ip: IpAddr,
    expires_at: Instant,
    created_at: Instant,
}

pub struct LoginController {
    pool: MySqlPool,
    passwords: PasswordService,
    auto_account: bool,
    korea: KoreaAuthOptions,
    two_factor: TwoFactorService,
    reconnect: ReconnectOptions,
    /// (game server, token) -> token binding. Tokens are single-use and expire.
    tokens: DashMap<(GameServerId, u32), ReconnectToken>,
    /// Client IP of the last fully authenticated login, for token binding.
    last_login_ip: DashMap<AccountId, IpAddr>,
    /// Failed-password limits per client IP and per account name (`Security:PasswordFailure*`).
    password_limits: PasswordLimits,
    /// Accounts one IP may create per hour (`Security:AccountCreationsPerIpHour`).
    creation_limit: CreationLimit,
}

impl LoginController {
    pub fn new(
        pool: MySqlPool,
        passwords: PasswordService,
        auto_account: bool,
        korea: KoreaAuthOptions,
        two_factor: TwoFactorService,
        reconnect: ReconnectOptions,
        security: &SecurityOptions,
    ) -> Self {
        Self {
            pool,
            passwords,
            auto_account,
            korea,
            two_factor,
            reconnect,
            tokens: DashMap::new(),
            last_login_ip: DashMap::new(),
            password_limits: PasswordLimits::new(
                security.password_failure_limit,
                Duration::from_secs(security.password_failure_window_seconds.into()),
                Duration::from_secs(security.password_block_seconds.into()),
            ),
            creation_limit: CreationLimit::new(
                security.account_creations_per_ip_hour,
                Duration::from_secs(3600),
            ),
        }
    }

    /// A refused password attempt, answered exactly like a wrong password.
    fn bad_password() -> LoginResult {
        LoginResult::denied(LoginDeniedReason::BAD_ACCOUNT)
    }

    /// Counts a failed password attempt of `username` from `ip`; warns once per started block.
    fn record_password_failure(&self, username: &str, ip: IpAddr) {
        let blocks = self
            .password_limits
            .record_failure(ip, username, Instant::now());
        if blocks.ip {
            tracing::warn!("Too many failed password attempts from {ip}: blocked");
        }
        if blocks.account {
            tracing::warn!(
                "Too many failed password attempts for account '{}' (last from {ip}): blocked",
                log_safe(username)
            );
        }
    }

    async fn verify_password(
        &self,
        stored: String,
        password: &Password,
    ) -> anyhow::Result<PasswordVerificationResult> {
        let svc = self.passwords.clone();
        let pw = password.clone();
        Ok(tokio::task::spawn_blocking(move || svc.verify_password(&stored, &pw)).await?)
    }

    async fn hash_for_storage(&self, password: &Password) -> anyhow::Result<String> {
        let svc = self.passwords.clone();
        let pw = password.clone();
        Ok(tokio::task::spawn_blocking(move || svc.hash_for_storage(&pw)).await??)
    }

    async fn korea_hash(&self, plaintext: &str) -> anyhow::Result<String> {
        let pw = plaintext.to_string();
        let rounds = self.korea.rounds;
        Ok(tokio::task::spawn_blocking(move || korea::compute(&pw, rounds)).await?)
    }

    /// EU method auth.
    pub async fn login(
        &self,
        username: &str,
        password: &Password,
        ip: IpAddr,
    ) -> anyhow::Result<LoginResult> {
        // A blocked IP or account name is answered like a wrong password without touching the
        // database (and without any hint that a limit exists).
        if self
            .password_limits
            .is_blocked(ip, username, Instant::now())
        {
            return Ok(Self::bad_password());
        }
        let row: Option<UserRow> = sqlx::query_as(
            "SELECT CAST(id AS UNSIGNED), password, korea_challenge_hash, CAST(banned AS SIGNED), CAST(ban_reason AS SIGNED) \
             FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        let Some((id, stored_password, stored_korea_hash, banned, ban_reason)) = row else {
            if self.auto_account {
                return self.create_and_login_invalid(username, password, ip).await;
            }
            self.record_password_failure(username, ip);
            return Ok(Self::bad_password());
        };

        // C#: reader.GetString("password") throws on NULL.
        let stored_password = stored_password.context("users.password is NULL")?;
        let verification = self.verify_password(stored_password, password).await?;
        if verification == PasswordVerificationResult::Failed {
            self.record_password_failure(username, ip);
            return Ok(Self::bad_password());
        }
        self.password_limits.record_success(username);

        let account_id = AccountId(id as u32);
        // The account's verified factors are always enforced by the authentication flow. The ban
        // check then happens after them, like the Korea flow, so a password holder who lacks a
        // factor learns nothing about the account.
        let two_factor = self.two_factor.get_requirements(account_id).await?;
        if banned != 0 && !two_factor.any_required() {
            return Ok(LoginResult::denied(LoginDeniedReason(ban_reason as u8)));
        }

        // Determine what needs rehashing, which is only possible when we have a plaintext password
        let plaintext = password.kind == PasswordKind::Plaintext;
        let rehash_pbkdf2 =
            verification == PasswordVerificationResult::SuccessRehashNeeded && plaintext;
        let korea_rehash_needed = self.korea.enabled
            && plaintext
            && match &stored_korea_hash {
                None => true,
                Some(h) => sha256_crypt::parse_rounds(h)? != self.korea.rounds,
            };

        let now = unix_now();
        let result = if rehash_pbkdf2 && korea_rehash_needed {
            let hash = self.hash_for_storage(password).await?;
            let korea_hash = self.korea_hash(&password.value).await?;
            sqlx::query(
                "UPDATE `users` SET password = ?, korea_challenge_hash = ?, updated_at = ? WHERE id = ?",
            )
            .bind(hash)
            .bind(korea_hash)
            .bind(now)
            .bind(account_id.0)
            .execute(&self.pool)
            .await?
        } else if rehash_pbkdf2 {
            let hash = self.hash_for_storage(password).await?;
            sqlx::query("UPDATE `users` SET password = ?, updated_at = ? WHERE id = ?")
                .bind(hash)
                .bind(now)
                .bind(account_id.0)
                .execute(&self.pool)
                .await?
        } else if korea_rehash_needed {
            let korea_hash = self.korea_hash(&password.value).await?;
            sqlx::query("UPDATE `users` SET korea_challenge_hash = ?, updated_at = ? WHERE id = ?")
                .bind(korea_hash)
                .bind(now)
                .bind(account_id.0)
                .execute(&self.pool)
                .await?
        } else {
            // `last_ip` / `last_login` are written by `record_login` once every factor passed.
            sqlx::query("UPDATE `users` SET updated_at = ? WHERE id = ?")
                .bind(now)
                .bind(account_id.0)
                .execute(&self.pool)
                .await?
        };
        if result.rows_affected() != 1 {
            tracing::warn!("Database update failed, error occurred while updating the account");
        }

        Ok(LoginResult::ok_with_two_factor(account_id, two_factor))
    }

    /// Records a fully authenticated login after every required factor passed: the account's
    /// `last_ip` / `last_login` / `updated_at` and the "connected" log line.
    pub async fn record_login(
        &self,
        account_id: AccountId,
        username: &str,
        ip: IpAddr,
    ) -> anyhow::Result<()> {
        let now = unix_now();
        let ip_s = ip.to_string();
        let result = sqlx::query(
            "UPDATE `users` SET last_ip = ?, last_login = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&ip_s)
        .bind(now)
        .bind(now)
        .bind(account_id.0)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            tracing::warn!(
                "Database update failed, error occurred while updating account login IP and time"
            );
        }
        self.last_login_ip.insert(account_id, ip);
        tracing::info!("{} connected.", log_safe(username));
        Ok(())
    }

    /// The Korea challenge-response material for `username`, or `None` if the account does not
    /// exist or has no `korea_challenge_hash` yet (the user must log in via EU auth first to seed
    /// it while `KoreaAuth:Enabled=true`).
    pub async fn get_korea_auth_info(
        &self,
        username: &str,
    ) -> anyhow::Result<Option<KoreaAuthInfo>> {
        let row: Option<(u64, Option<String>)> = sqlx::query_as(
            "SELECT CAST(id AS UNSIGNED), korea_challenge_hash FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, Some(stored))) = row else {
            return Ok(None);
        };
        let (rounds, salt, raw) = sha256_crypt::parse(&stored)?;
        Ok(Some(KoreaAuthInfo {
            account_id: AccountId(id as u32),
            challenge_key_hash: raw,
            challenge_salt: salt,
            challenge_rounds: rounds,
        }))
    }

    /// Ban status of an account; called after successful challenge verification so account status
    /// is not leaked before authentication.
    pub async fn check_ban_status(
        &self,
        account_id: AccountId,
    ) -> anyhow::Result<(bool, LoginDeniedReason)> {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT CAST(banned AS SIGNED), CAST(ban_reason AS SIGNED) FROM users WHERE id = ?",
        )
        .bind(account_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            None => (false, LoginDeniedReason::default()),
            Some((banned, reason)) => {
                let banned = banned != 0;
                (
                    banned,
                    if banned {
                        LoginDeniedReason(reason as u8)
                    } else {
                        LoginDeniedReason::default()
                    },
                )
            }
        })
    }

    async fn create_and_login_invalid(
        &self,
        username: &str,
        password: &Password,
        ip: IpAddr,
    ) -> anyhow::Result<LoginResult> {
        if !is_valid_username(username) {
            return Ok(LoginResult::denied(LoginDeniedReason::BAD_ACCOUNT));
        }
        if !self.creation_limit.try_acquire(ip, Instant::now()) {
            tracing::warn!(
                "Account creation limit reached for {ip}: refusing to create '{}'",
                log_safe(username)
            );
            return Ok(Self::bad_password());
        }
        let password_hash = self.hash_for_storage(password).await?;
        let now = unix_now();
        let ip_s = ip.to_string();

        let inserted = if self.korea.enabled && password.kind == PasswordKind::Plaintext {
            let korea_hash = self.korea_hash(&password.value).await?;
            sqlx::query(
                "INSERT into users (username, password, korea_challenge_hash, email, last_ip, last_login, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(username)
            .bind(&password_hash)
            .bind(korea_hash)
            .bind("")
            .bind(&ip_s)
            .bind(now)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await?
        } else {
            sqlx::query(
                "INSERT into users (username, password, email, last_ip, last_login, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(username)
            .bind(&password_hash)
            .bind("")
            .bind(&ip_s)
            .bind(now)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await?
        };
        if inserted.rows_affected() != 1 {
            return Ok(LoginResult::denied(LoginDeniedReason::LOGIN_UNKNOWN));
        }
        tracing::debug!(
            "Created account from invalid username login with value {}",
            username
        );
        Box::pin(self.login(username, password, ip)).await
    }

    /// Stores a reconnection token for `account_id` on `gs_id`, bound to the client IP of the
    /// last fully authenticated login, and acknowledges it to the game server.
    pub async fn add_reconnection_token(
        &self,
        connection: &InternalConnection,
        gs_id: GameServerId,
        account_id: AccountId,
        token: u32,
    ) -> anyhow::Result<()> {
        let Some(client_ip) = self.lookup_last_login_ip(account_id).await? else {
            tracing::warn!(
                "No completed login from account {}; not issuing a reconnect token",
                account_id
            );
            return Ok(());
        };
        let now = Instant::now();
        self.tokens.retain(|_, t| t.expires_at > now);
        let cap = self.reconnect.max_tokens_per_account as usize;
        while self
            .tokens
            .iter()
            .filter(|e| e.value().account_id == account_id)
            .count()
            >= cap
        {
            let oldest = self
                .tokens
                .iter()
                .filter(|e| e.value().account_id == account_id)
                .min_by_key(|e| e.value().created_at)
                .map(|e| *e.key());
            let Some(oldest) = oldest else { break };
            self.tokens.remove(&oldest);
        }
        self.tokens.insert(
            (gs_id, token),
            ReconnectToken {
                account_id,
                client_ip,
                expires_at: now + self.reconnect.token_ttl,
                created_at: now,
            },
        );
        connection.send_packet(&LgPlayerReconnect { token });
        Ok(())
    }

    /// The client IP of the account's last fully authenticated login. Falls back to
    /// `users.last_ip` (written by [`Self::record_login`]) after a login-server restart.
    async fn lookup_last_login_ip(&self, account_id: AccountId) -> anyhow::Result<Option<IpAddr>> {
        if let Some(ip) = self.last_login_ip.get(&account_id) {
            return Ok(Some(*ip));
        }
        let row: Option<(String,)> = sqlx::query_as("SELECT last_ip FROM users WHERE id = ?")
            .bind(account_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(ip,)| ip.parse().ok()))
    }

    /// Validates and consumes a reconnection token. A token is single-use: it is removed before
    /// any check, so a failed attempt (wrong account/IP, expired) cannot be retried. An `Err`
    /// mirrors a database failure; ordinary denials are `ReconnectFailure` values.
    pub fn reconnect(
        &self,
        games: &GameController,
        gs_id: GameServerId,
        account_id: AccountId,
        token: u32,
        client_ip: IpAddr,
    ) -> anyhow::Result<ReconnectResult> {
        let mut gs_id = gs_id;
        if !self.tokens.iter().any(|e| e.key().0 == gs_id) {
            match games.try_get_parent_id(gs_id) {
                Some(parent) => gs_id = parent,
                None => {
                    return Ok(ReconnectResult::failed(
                        ReconnectFailure::GameServerUnavailable,
                    ))
                }
            }
        }
        let Some((_, entry)) = self.tokens.remove(&(gs_id, token)) else {
            return Ok(ReconnectResult::failed(ReconnectFailure::InvalidToken));
        };
        if entry.expires_at <= Instant::now() {
            return Ok(ReconnectResult::failed(ReconnectFailure::ExpiredToken));
        }
        if entry.account_id != account_id {
            return Ok(ReconnectResult::failed(ReconnectFailure::AccountMismatch));
        }
        if entry.client_ip != client_ip {
            return Ok(ReconnectResult::failed(ReconnectFailure::IpMismatch));
        }
        Ok(ReconnectResult::success(account_id))
    }

    /// Test hook: pretend `account_id` completed a login from `ip`, so the reconnect binding can
    /// be exercised without a database-backed `record_login`.
    #[doc(hidden)]
    pub fn set_last_login_ip_for_test(&self, account_id: AccountId, ip: IpAddr) {
        self.last_login_ip.insert(account_id, ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;

    use crate::network::internal::InternalConnection;
    use crate::server::LoginServer;
    use crate::test_support::{test_config, test_server, MockBackend};

    const TEST_ACCOUNT: AccountId = AccountId(42);
    const TEST_GS: GameServerId = GameServerId(1);
    const TEST_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const OTHER_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2));

    fn security_server(security: serde_json::Value) -> Arc<LoginServer> {
        test_server(
            test_config(serde_json::json!({ "Security": security })),
            Arc::new(MockBackend::default()),
        )
    }

    /// A blocked IP or account name is answered like a wrong password without a database round
    /// trip (the test server's pool cannot connect, so reaching it would be an error).
    #[tokio::test]
    async fn a_blocked_ip_or_account_is_denied_without_touching_the_database() {
        let server = security_server(serde_json::json!({ "PasswordFailureLimit": 2 }));
        let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
        let other = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));
        let pw = Password::from_plaintext("pw");
        server.logins.record_password_failure("Alice", ip);
        server.logins.record_password_failure("alice", other);
        // The account name is blocked for every IP ...
        let r = server.logins.login("ALICE", &pw, ip).await.unwrap();
        assert!(!r.success);
        assert_eq!(r.denial_reason, LoginDeniedReason::BAD_ACCOUNT);
        let r = server
            .logins
            .login(
                "alice",
                &pw,
                IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 3)),
            )
            .await
            .unwrap();
        assert_eq!(r.denial_reason, LoginDeniedReason::BAD_ACCOUNT);
        // ... and the IP that failed twice is blocked for every name.
        server.logins.record_password_failure("bob", ip);
        let r = server.logins.login("carol", &pw, ip).await.unwrap();
        assert_eq!(r.denial_reason, LoginDeniedReason::BAD_ACCOUNT);
    }

    #[tokio::test]
    async fn account_creation_over_the_ip_limit_is_denied_like_a_wrong_password() {
        let server = security_server(serde_json::json!({ "AccountCreationsPerIpHour": 1 }));
        let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
        // Another creation already used the IP's hourly share.
        assert!(server.logins.creation_limit.try_acquire(ip, Instant::now()));
        let r = server
            .logins
            .create_and_login_invalid("newbie", &Password::from_plaintext("pw"), ip)
            .await
            .unwrap();
        assert!(!r.success);
        assert_eq!(r.denial_reason, LoginDeniedReason::BAD_ACCOUNT);
    }

    fn reconnect_server(ttl: Duration, cap: i32) -> Arc<LoginServer> {
        // `parse_timespan` reads the fraction as a fraction of a second: .020 = 20 ms.
        let ttl = format!("00:00:00.{:03}", ttl.as_millis());
        let server = test_server(
            test_config(serde_json::json!({
                "Reconnect": { "TokenTtl": ttl, "MaxTokensPerAccount": cap }
            })),
            Arc::new(MockBackend::default()),
        );
        server
            .logins
            .set_last_login_ip_for_test(TEST_ACCOUNT, TEST_IP);
        server
    }

    async fn seed(server: &LoginServer, account: AccountId, token: u32) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let conn = InternalConnection::new(TEST_IP, tx);
        server
            .logins
            .add_reconnection_token(&conn, TEST_GS, account, token)
            .await
            .unwrap();
    }

    fn check(
        server: &LoginServer,
        account: AccountId,
        token: u32,
        ip: IpAddr,
    ) -> Option<ReconnectFailure> {
        let r = server
            .logins
            .reconnect(&server.games, TEST_GS, account, token, ip)
            .unwrap();
        assert_eq!(r.success, r.failure.is_none());
        r.failure
    }

    #[tokio::test]
    async fn sequential_reconnect_tokens_are_refused() {
        let server = reconnect_server(Duration::from_secs(60), 5);
        seed(&server, TEST_ACCOUNT, 0xDEAD_BEEF).await;
        for token in 1..=1000 {
            assert_eq!(
                check(&server, TEST_ACCOUNT, token, TEST_IP),
                Some(ReconnectFailure::InvalidToken),
                "sequential token {token} was accepted"
            );
        }
        assert_eq!(check(&server, TEST_ACCOUNT, 0xDEAD_BEEF, TEST_IP), None);
    }

    #[tokio::test]
    async fn reconnect_token_is_single_use() {
        let server = reconnect_server(Duration::from_secs(60), 5);
        seed(&server, TEST_ACCOUNT, 777).await;
        assert_eq!(check(&server, TEST_ACCOUNT, 777, TEST_IP), None);
        // The consumed token left no table for this world, so the retry is the "unavailable"
        // denial; either way the same token is never accepted twice.
        assert_eq!(
            check(&server, TEST_ACCOUNT, 777, TEST_IP),
            Some(ReconnectFailure::GameServerUnavailable)
        );
    }

    #[tokio::test]
    async fn expired_reconnect_token_is_refused() {
        let server = reconnect_server(Duration::from_millis(20), 5);
        seed(&server, TEST_ACCOUNT, 777).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            check(&server, TEST_ACCOUNT, 777, TEST_IP),
            Some(ReconnectFailure::ExpiredToken)
        );
    }

    #[tokio::test]
    async fn reconnect_token_is_bound_to_the_account_and_login_ip() {
        let server = reconnect_server(Duration::from_secs(60), 5);
        seed(&server, TEST_ACCOUNT, 777).await;
        assert_eq!(
            check(&server, AccountId(43), 777, TEST_IP),
            Some(ReconnectFailure::AccountMismatch)
        );
        seed(&server, TEST_ACCOUNT, 778).await;
        assert_eq!(
            check(&server, TEST_ACCOUNT, 778, OTHER_IP),
            Some(ReconnectFailure::IpMismatch)
        );
    }

    #[tokio::test]
    async fn reconnect_tokens_are_capped_per_account() {
        let server = reconnect_server(Duration::from_secs(60), 2);
        for token in [11, 12, 13] {
            seed(&server, TEST_ACCOUNT, token).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // The oldest token was evicted; the two newest still work.
        assert_eq!(
            check(&server, TEST_ACCOUNT, 11, TEST_IP),
            Some(ReconnectFailure::InvalidToken)
        );
        assert_eq!(check(&server, TEST_ACCOUNT, 12, TEST_IP), None);
        assert_eq!(check(&server, TEST_ACCOUNT, 13, TEST_IP), None);
    }

    #[test]
    fn username_rules_match_dotnet_regex() {
        assert!(is_valid_username("test"));
        assert!(is_valid_username("José_99.x-y@z"));
        assert!(is_valid_username("테스트"));
        assert!(is_valid_username(&"a".repeat(32)));
        assert!(!is_valid_username(&"a".repeat(33)));
        assert!(!is_valid_username(""));
        assert!(!is_valid_username("bad name"));
        assert!(!is_valid_username("semi;colon"));
        // .NET `$` quirk: a single trailing newline is accepted.
        assert!(is_valid_username("name\n"));
        assert!(!is_valid_username("name\n\n"));
        // Non-BMP letters never match in .NET (UTF-16 surrogates).
        assert!(!is_valid_username("a\u{10400}"));
    }
}
