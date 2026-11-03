//! Two-factor authentication services (`AAEmu.Login.Core.Services.TwoFactor`): repository,
//! requirements, TOTP (Otp.NET compatible), PC-cert PIN and ARS sessions.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use futures::future::BoxFuture;
use futures::FutureExt;
use hmac::{Hmac, Mac};
use rand::{Rng, RngCore};
use sqlx::MySqlPool;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex as AsyncMutex;

use super::password::{Password, PasswordService, PasswordVerificationResult};
use crate::config::TwoFactorOptions;
use crate::models::{AccountId, TwoFactorConfig, TwoFactorMethod, TwoFactorRequirements};

/// Database access for two-factor authentication configuration (`user_2fa`).
#[derive(Clone, Debug)]
pub struct TwoFactorRepository {
    pool: MySqlPool,
}

impl TwoFactorRepository {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }

    /// The 2FA configuration for an account, or `None` if none exists.
    pub async fn get_config(&self, account_id: AccountId) -> sqlx::Result<Option<TwoFactorConfig>> {
        let row: Option<TwoFactorRow> = sqlx::query_as(
            "SELECT CAST(user_id AS SIGNED) AS user_id, CAST(enabled_methods AS SIGNED) AS enabled_methods, \
                    otp_secret, CAST(otp_verified AS SIGNED) AS otp_verified, cert_pin_hash, ars_phone_number, \
                    CAST(ars_phone_verified AS SIGNED) AS ars_phone_verified, \
                    CAST(created_at AS SIGNED) AS created_at, CAST(updated_at AS SIGNED) AS updated_at \
             FROM user_2fa WHERE user_id = ?",
        )
        .bind(account_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| TwoFactorConfig {
            user_id: AccountId(r.user_id as u32),
            enabled_methods: TwoFactorMethod(r.enabled_methods as u8),
            otp_secret: r.otp_secret,
            otp_verified: r.otp_verified != 0,
            cert_pin_hash: r.cert_pin_hash,
            ars_phone_number: r.ars_phone_number,
            ars_phone_verified: r.ars_phone_verified != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }))
    }
}

/// A `user_2fa` row. Integer columns are CAST in the query so decoding does not depend on the
/// exact (signed/unsigned) column types of a given schema revision.
#[derive(sqlx::FromRow)]
struct TwoFactorRow {
    user_id: i64,
    enabled_methods: i64,
    otp_secret: Option<String>,
    otp_verified: i64,
    cert_pin_hash: Option<String>,
    ars_phone_number: Option<String>,
    ars_phone_verified: i64,
    created_at: i64,
    updated_at: i64,
}

/// Which 2FA methods a config requires: only methods that are both enabled AND properly
/// verified/configured.
pub fn requirements_for(config: Option<&TwoFactorConfig>) -> TwoFactorRequirements {
    let Some(config) = config else {
        return TwoFactorRequirements::NONE;
    };
    let methods = config.enabled_methods;
    let non_empty = |s: &Option<String>| s.as_deref().is_some_and(|v| !v.is_empty());
    TwoFactorRequirements {
        requires_otp: methods.contains(TwoFactorMethod::OTP)
            && config.otp_verified
            && non_empty(&config.otp_secret),
        requires_pc_cert: methods.contains(TwoFactorMethod::PC_CERT)
            && non_empty(&config.cert_pin_hash),
        requires_ars: methods.contains(TwoFactorMethod::ARS)
            && config.ars_phone_verified
            && non_empty(&config.ars_phone_number),
    }
}

/// `TwoFactorService`: entry point for querying 2FA requirements during authentication.
#[derive(Clone, Debug)]
pub struct TwoFactorService {
    repository: TwoFactorRepository,
}

impl TwoFactorService {
    pub fn new(repository: TwoFactorRepository) -> Self {
        Self { repository }
    }

    pub async fn get_requirements(
        &self,
        account_id: AccountId,
    ) -> sqlx::Result<TwoFactorRequirements> {
        Ok(requirements_for(
            self.repository.get_config(account_id).await?.as_ref(),
        ))
    }

    /// The raw `user_2fa` configuration (for the ARS phone number and similar).
    pub async fn get_config(&self, account_id: AccountId) -> sqlx::Result<Option<TwoFactorConfig>> {
        self.repository.get_config(account_id).await
    }
}

/// Otp.NET-compatible Base32 (RFC 4648 alphabet, case-insensitive, `=` padding).
pub mod base32 {
    /// `Base32Encoding.ToBytes`. Trailing `=` are ignored; any other non-alphabet char is an error.
    pub fn decode(input: &str) -> Option<Vec<u8>> {
        let input = input.trim_end_matches('=');
        let mut out = Vec::with_capacity(input.len() * 5 / 8);
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for c in input.chars() {
            let v = match c {
                'A'..='Z' => c as u32 - 'A' as u32,
                'a'..='z' => c as u32 - 'a' as u32,
                '2'..='7' => c as u32 - '2' as u32 + 26,
                _ => return None,
            };
            buffer = (buffer << 5) | v;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push((buffer >> bits) as u8);
                buffer &= (1 << bits) - 1;
            }
        }
        Some(out)
    }

    /// `Base32Encoding.ToString`: uppercase, padded with `=` to a multiple of 8 chars.
    pub fn encode(input: &[u8]) -> String {
        const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = String::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for &b in input {
            buffer = (buffer << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
        }
        while !out.len().is_multiple_of(8) {
            out.push('=');
        }
        out
    }
}

/// TOTP as configured by the C# `OtpService`: `new Totp(secret, totpSize: 8)` — HMAC-SHA1,
/// 30-second step, 8 digits.
pub mod totp {
    use super::*;

    pub const STEP: u64 = 30;
    pub const DIGITS: u32 = 8;

    /// The code for a given time-step counter.
    pub fn code_for_counter(secret: &[u8], counter: u64) -> String {
        let mut mac =
            Hmac::<sha1::Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
        mac.update(&counter.to_be_bytes());
        let hash = mac.finalize().into_bytes();
        let offset = usize::from(hash[hash.len() - 1] & 0x0F);
        let binary = (u32::from(hash[offset] & 0x7F) << 24)
            | (u32::from(hash[offset + 1]) << 16)
            | (u32::from(hash[offset + 2]) << 8)
            | u32::from(hash[offset + 3]);
        let code = binary % 10u32.pow(DIGITS);
        format!("{code:0width$}", width = DIGITS as usize)
    }

    pub fn code_at(secret: &[u8], unix_seconds: u64) -> String {
        code_for_counter(secret, unix_seconds / STEP)
    }

    /// `Totp.VerifyTotp(code, out _, new VerificationWindow(previous: 1, future: 1))`.
    pub fn verify(secret: &[u8], code: &str, unix_seconds: u64) -> bool {
        matching_step(secret, code, unix_seconds).is_some()
    }

    /// The accepted time-step counter the code matches, if any, within the +-1 window.
    ///
    /// Every window slot is compared (no early return) so a match cannot be timed. The caller
    /// uses the returned counter for replay protection.
    pub fn matching_step(secret: &[u8], code: &str, unix_seconds: u64) -> Option<u64> {
        let counter = unix_seconds / STEP;
        let mut matched = None;
        for c in [counter.wrapping_sub(1), counter, counter + 1] {
            let expected = code_for_counter(secret, c);
            // Otp.NET ValuesEqual: constant-time comparison of equal-length strings.
            let equal = expected.len() == code.len()
                && bool::from(expected.as_bytes().ct_eq(code.as_bytes()));
            if equal {
                matched = Some(c);
            }
        }
        matched
    }
}

/// A source of the current time (the C# `TimeProvider`).
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// The result of one OTP / PC-certificate submission after the per-account limiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactorValidation {
    Valid,
    /// The code or PIN was wrong (or a TOTP step was replayed). The failure has been counted.
    Invalid,
    /// The account is locked after too many failures; nothing was checked.
    Locked,
}

impl FactorValidation {
    pub fn is_valid(self) -> bool {
        self == Self::Valid
    }
}

/// Per-account factor policy (`TwoFactor:MaxOtpFailures` / `OtpLockoutMinutes`), shared by the
/// OTP and PC-certificate checks (each service owns its own counter).
#[derive(Clone)]
pub struct FactorPolicy {
    /// Failures allowed before locking (config validation enforces >= 1).
    pub max_failures: u32,
    pub lockout: Duration,
    pub clock: Clock,
}

impl FactorPolicy {
    pub fn from_options(options: &TwoFactorOptions) -> Self {
        Self {
            max_failures: options.max_otp_failures.max(1) as u32,
            lockout: Duration::from_secs(options.otp_lockout_minutes.max(1) as u64 * 60),
            clock: Arc::new(SystemTime::now),
        }
    }

    /// A policy reading the time from `clock` (tests).
    pub fn with_clock(options: &TwoFactorOptions, clock: Clock) -> Self {
        Self {
            clock,
            ..Self::from_options(options)
        }
    }
}

impl Default for FactorPolicy {
    fn default() -> Self {
        Self::from_options(&TwoFactorOptions::default())
    }
}

impl std::fmt::Debug for FactorPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactorPolicy")
            .field("max_failures", &self.max_failures)
            .field("lockout", &self.lockout)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default)]
struct FactorAttemptEntry {
    failures: u32,
    locked_until: Option<SystemTime>,
    /// Highest accepted TOTP time step, for replay protection.
    last_accepted_step: Option<u64>,
}

/// Per-account factor failure counting and locking (OTP and PC certificate); the OTP service also
/// uses it for TOTP replay protection.
///
/// The state is shared by every connection, so reconnecting (or opening parallel connections)
/// cannot reset the counter. Each account has one async gate held across the whole
/// check-and-record sequence, so two concurrent submissions cannot both pass the limit.
#[derive(Clone)]
pub struct FactorAttemptLimiter {
    max_failures: u32,
    lockout: Duration,
    clock: Clock,
    attempts: Arc<DashMap<AccountId, FactorAttemptEntry>>,
    /// One gate per account; bounded by the number of accounts that submit a factor.
    gates: Arc<DashMap<AccountId, Arc<AsyncMutex<()>>>>,
}

impl std::fmt::Debug for FactorAttemptLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactorAttemptLimiter")
            .field("max_failures", &self.max_failures)
            .field("lockout", &self.lockout)
            .field("attempts", &self.attempts.len())
            .finish_non_exhaustive()
    }
}

impl FactorAttemptLimiter {
    pub fn new(policy: FactorPolicy) -> Self {
        Self {
            max_failures: policy.max_failures,
            lockout: policy.lockout,
            clock: policy.clock,
            attempts: Arc::new(DashMap::new()),
            gates: Arc::new(DashMap::new()),
        }
    }

    fn gate(&self, account_id: AccountId) -> Arc<AsyncMutex<()>> {
        self.gates
            .entry(account_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn now_seconds(&self) -> u64 {
        (self.clock)()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Whether the account is currently locked. Never changes state.
    pub fn is_locked(&self, account_id: AccountId) -> bool {
        let now = (self.clock)();
        self.attempts
            .get(&account_id)
            .is_some_and(|e| e.locked_until.is_some_and(|until| now < until))
    }

    /// Records a successful factor check and clears the failure counter.
    fn record_success(&self, account_id: AccountId) {
        let mut entry = self.attempts.entry(account_id).or_default();
        entry.failures = 0;
        entry.locked_until = None;
    }

    /// Counts one failure; returns the resulting outcome (the lock is set on the failure that
    /// reaches the configured maximum).
    fn record_failure(&self, account_id: AccountId) -> FactorValidation {
        let now = (self.clock)();
        let mut entry = self.attempts.entry(account_id).or_default();
        entry.failures = entry.failures.saturating_add(1);
        if self.max_failures > 0 && entry.failures >= self.max_failures {
            entry.locked_until = Some(now + self.lockout);
            tracing::warn!(
                "factor attempts for account {} locked after {} failures for {:?}",
                account_id,
                entry.failures,
                self.lockout
            );
            FactorValidation::Locked
        } else {
            tracing::debug!(
                "factor validation failed for account {} (attempt {})",
                account_id,
                entry.failures
            );
            FactorValidation::Invalid
        }
    }

    /// Clears an expired lock and returns whether the account may attempt now.
    fn ready(&self, account_id: AccountId, now: SystemTime) -> bool {
        let mut entry = self.attempts.entry(account_id).or_default();
        match entry.locked_until {
            Some(until) if now < until => false,
            Some(_) => {
                entry.locked_until = None;
                entry.failures = 0;
                true
            }
            None => true,
        }
    }

    /// Checks `code` and records the result. The caller must hold [`Self::gate`] for the account.
    fn check_locked(
        &self,
        account_id: AccountId,
        secret: Option<&[u8]>,
        code: &str,
    ) -> FactorValidation {
        let now = (self.clock)();
        if !self.ready(account_id, now) {
            return FactorValidation::Locked;
        }
        let Some(secret) = secret else {
            tracing::warn!(
                "OTP validation attempted for account {} with no OTP configured",
                account_id
            );
            return self.record_failure(account_id);
        };
        let Some(step) = totp::matching_step(secret, code, self.now_seconds()) else {
            return self.record_failure(account_id);
        };
        {
            let mut entry = self.attempts.entry(account_id).or_default();
            if entry.last_accepted_step.is_some_and(|last| step <= last) {
                // A replayed code is refused but is not a brute-force attempt on its own:
                // counting it would let anyone who observed one code lock the account.
                tracing::debug!(
                    "OTP replay refused for account {} (step {})",
                    account_id,
                    step
                );
                return FactorValidation::Invalid;
            }
            entry.last_accepted_step = Some(step);
            entry.failures = 0;
            entry.locked_until = None;
        }
        FactorValidation::Valid
    }
}

fn otp_secret_bytes(config: Option<&TwoFactorConfig>) -> Option<Vec<u8>> {
    let secret = config?.otp_secret.as_deref().filter(|s| !s.is_empty())?;
    let bytes = base32::decode(secret);
    if bytes.is_none() {
        tracing::error!("invalid Base32 OTP secret in user_2fa");
    }
    bytes
}

/// `OtpService`: TOTP operations plus the per-account failure limiter.
#[derive(Clone, Debug)]
pub struct OtpService {
    repository: TwoFactorRepository,
    limiter: FactorAttemptLimiter,
}

impl OtpService {
    /// 160 bits, standard for TOTP.
    const SECRET_LENGTH: usize = 20;

    pub fn new(repository: TwoFactorRepository, policy: FactorPolicy) -> Self {
        Self {
            repository,
            limiter: FactorAttemptLimiter::new(policy),
        }
    }

    /// Validates a TOTP code for an account. The OTP lock is checked before the database read.
    pub async fn validate(
        &self,
        account_id: AccountId,
        code: &str,
    ) -> sqlx::Result<FactorValidation> {
        let gate = self.limiter.gate(account_id);
        let _guard = gate.lock().await;
        if self.limiter.is_locked(account_id) {
            tracing::warn!("OTP validation refused for locked account {}", account_id);
            return Ok(FactorValidation::Locked);
        }
        let config = self.repository.get_config(account_id).await?;
        Ok(self.limiter.check_locked(
            account_id,
            otp_secret_bytes(config.as_ref()).as_deref(),
            code,
        ))
    }

    /// The part of [`Self::validate`] after the `user_2fa` lookup, for tests and callers that
    /// already hold the configuration. The limiter is shared with [`Self::validate`].
    pub async fn validate_config(
        &self,
        config: Option<&TwoFactorConfig>,
        account_id: AccountId,
        code: &str,
    ) -> FactorValidation {
        let gate = self.limiter.gate(account_id);
        let _guard = gate.lock().await;
        self.limiter
            .check_locked(account_id, otp_secret_bytes(config).as_deref(), code)
    }

    /// A new Base32-encoded TOTP secret for account setup.
    pub fn generate_secret(&self) -> String {
        let mut bytes = [0u8; Self::SECRET_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        base32::encode(&bytes)
    }
}

/// `PcCertService`: PC certificate PIN verification (PINs are hashed like passwords).
///
/// PIN guesses are serialised per account and counted under the same lockout policy as OTP:
/// PBKDF2 is expensive, so a locked account is refused before the hash or the database is
/// touched.
#[derive(Clone, Debug)]
pub struct PcCertService {
    repository: TwoFactorRepository,
    passwords: PasswordService,
    limiter: FactorAttemptLimiter,
}

impl PcCertService {
    pub fn new(
        repository: TwoFactorRepository,
        passwords: PasswordService,
        policy: FactorPolicy,
    ) -> Self {
        Self {
            repository,
            passwords,
            limiter: FactorAttemptLimiter::new(policy),
        }
    }

    pub async fn validate(
        &self,
        account_id: AccountId,
        pin: &str,
    ) -> sqlx::Result<FactorValidation> {
        let gate = self.limiter.gate(account_id);
        let _guard = gate.lock().await;
        if self.limiter.is_locked(account_id) {
            tracing::warn!(
                "PcCert validation refused for locked account {}",
                account_id
            );
            return Ok(FactorValidation::Locked);
        }
        let config = self.repository.get_config(account_id).await?;
        Ok(self.validate_locked(config, account_id, pin).await)
    }

    /// Validates `pin` against an already loaded 2FA configuration (the part of
    /// [`Self::validate`] after the `ITwoFactorRepository.GetConfigAsync` lookup). The limiter
    /// is shared by every clone of the service.
    pub async fn validate_config(
        &self,
        config: Option<TwoFactorConfig>,
        account_id: AccountId,
        pin: &str,
    ) -> FactorValidation {
        let gate = self.limiter.gate(account_id);
        let _guard = gate.lock().await;
        self.validate_locked(config, account_id, pin).await
    }

    async fn validate_locked(
        &self,
        config: Option<TwoFactorConfig>,
        account_id: AccountId,
        pin: &str,
    ) -> FactorValidation {
        let now = (self.limiter.clock)();
        if !self.limiter.ready(account_id, now) {
            return FactorValidation::Locked;
        }
        let Some(hash) = config
            .and_then(|c| c.cert_pin_hash)
            .filter(|s| !s.is_empty())
        else {
            tracing::warn!(
                "PcCert validation attempted for account {} with no PIN configured",
                account_id
            );
            return self.limiter.record_failure(account_id);
        };
        // Reuse the password service for PIN verification - same hashing approach
        let passwords = self.passwords.clone();
        let pin = Password::from_plaintext(pin);
        let result = tokio::task::spawn_blocking(move || passwords.verify_password(&hash, &pin))
            .await
            .unwrap_or(PasswordVerificationResult::Failed);
        if result == PasswordVerificationResult::Failed {
            tracing::debug!("PcCert PIN validation failed for account {}", account_id);
            self.limiter.record_failure(account_id)
        } else {
            self.limiter.record_success(account_id);
            FactorValidation::Valid
        }
    }
}

/// An active ARS phone callback session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArsSession {
    /// The 4-digit code displayed to the user and spoken during the call.
    pub session_code: String,
    pub expires_at: SystemTime,
}

/// Places the ARS verification phone call (the C# `IArsPhoneProvider` seam).
///
/// The default is [`DisabledArsPhoneProvider`]: no external call is made. An operator who wants
/// real ARS supplies an implementation that dials the number and speaks/enters `code`. The code
/// must never be logged.
pub trait ArsPhoneProvider: Send + Sync {
    fn call<'a>(
        &'a self,
        account_id: AccountId,
        phone_number: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// The default provider: ARS is not integrated, so no call is placed.
#[derive(Clone, Debug, Default)]
pub struct DisabledArsPhoneProvider;

impl ArsPhoneProvider for DisabledArsPhoneProvider {
    fn call<'a>(
        &'a self,
        account_id: AccountId,
        phone_number: &'a str,
        _code: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        async move {
            tracing::warn!(
                "ARS phone provider is disabled; refusing the login for account {} (phone {})",
                account_id,
                mask_phone(phone_number)
            );
            anyhow::bail!("ARS phone provider is disabled")
        }
        .boxed()
    }
}

/// Shows at most the last four digits of a phone number in logs.
pub fn mask_phone(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    let keep = digits.len().min(4);
    format!(
        "{}{}",
        "*".repeat(digits.len() - keep),
        &digits[digits.len() - keep..]
    )
}

/// `ArsService`: ARS (Automatic Response System) phone callback verification.
pub struct ArsService {
    active_sessions: DashMap<AccountId, ArsSession>,
    clock: Clock,
    provider: Arc<dyn ArsPhoneProvider>,
}

impl Default for ArsService {
    fn default() -> Self {
        Self::with_provider(
            Arc::new(SystemTime::now),
            Arc::new(DisabledArsPhoneProvider),
        )
    }
}

impl std::fmt::Debug for ArsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArsService")
            .field("active_sessions", &self.active_sessions)
            .finish_non_exhaustive()
    }
}

impl ArsService {
    /// An ARS service reading the time from `clock` (`new ArsService(timeProvider, ...)`).
    pub fn with_clock(clock: Clock) -> Self {
        Self::with_provider(clock, Arc::new(DisabledArsPhoneProvider))
    }

    /// An ARS service with a phone-call provider.
    pub fn with_provider(clock: Clock, provider: Arc<dyn ArsPhoneProvider>) -> Self {
        Self {
            active_sessions: DashMap::new(),
            clock,
            provider,
        }
    }

    /// Creates (or replaces) the ARS session for an account and asks the provider to place the
    /// call. The session is only registered when the provider accepts it.
    pub async fn create_session(
        &self,
        account_id: AccountId,
        phone_number: &str,
        timeout_seconds: i32,
    ) -> anyhow::Result<ArsSession> {
        // Generate a random 4-digit code
        let code = format!("{:04}", rand::rngs::OsRng.gen_range(0..10_000));
        let expires_at = (self.clock)() + Duration::from_secs(timeout_seconds.max(0) as u64);
        self.provider.call(account_id, phone_number, &code).await?;
        let session = ArsSession {
            session_code: code,
            expires_at,
        };
        self.active_sessions.insert(account_id, session.clone());
        tracing::debug!(
            "Created ARS session for account {}, expires at {:?}",
            account_id,
            expires_at
        );
        Ok(session)
    }

    /// Completes an ARS session; called when the user confirms via phone.
    pub fn complete_session(&self, account_id: AccountId, session_code: &str) -> bool {
        let Some(session) = self.active_sessions.get(&account_id).map(|s| s.clone()) else {
            tracing::debug!(
                "ARS completion attempted for account {} with no active session",
                account_id
            );
            return false;
        };
        if (self.clock)() > session.expires_at {
            self.active_sessions.remove(&account_id);
            tracing::debug!("ARS session for account {} has expired", account_id);
            return false;
        }
        if !bool::from(
            session
                .session_code
                .as_bytes()
                .ct_eq(session_code.as_bytes()),
        ) {
            tracing::debug!("ARS code mismatch for account {}", account_id);
            return false;
        }
        self.active_sessions.remove(&account_id);
        tracing::debug!(
            "ARS session completed successfully for account {}",
            account_id
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totp_matches_otp_net() {
        // Otp.NET: new Totp(Base32("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP"), totpSize: 8).ComputeTotp(1700000000)
        let secret = base32::decode("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP").unwrap();
        assert_eq!(totp::code_at(&secret, 1_700_000_000), "80406058");
        assert!(totp::verify(&secret, "80406058", 1_700_000_000));
        assert!(totp::verify(&secret, "80406058", 1_700_000_000 + 30));
        assert!(!totp::verify(&secret, "80406058", 1_700_000_000 + 90));
        assert!(!totp::verify(&secret, "8040605", 1_700_000_000));
    }

    #[test]
    fn base32_matches_otp_net() {
        assert_eq!(base32::encode(&[1, 2, 3, 4, 5, 6, 7]), "AEBAGBAFAYDQ====");
        assert_eq!(base32::decode("mzxw6ytboi======").unwrap(), b"foobar");
        assert_eq!(
            base32::decode("AEBAGBAFAYDQ====").unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(base32::decode("A1"), None);
    }

    fn cfg(methods: u8) -> TwoFactorConfig {
        TwoFactorConfig {
            user_id: AccountId(1),
            enabled_methods: TwoFactorMethod(methods),
            otp_secret: Some("ABC".into()),
            otp_verified: true,
            cert_pin_hash: Some("x".into()),
            ars_phone_number: Some("123".into()),
            ars_phone_verified: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn requirements() {
        assert_eq!(requirements_for(None), TwoFactorRequirements::NONE);
        let r = requirements_for(Some(&cfg(7)));
        assert!(r.requires_otp && r.requires_pc_cert && !r.requires_ars);
        let mut c = cfg(1);
        c.otp_secret = Some(String::new());
        assert!(!requirements_for(Some(&c)).any_required());
    }

    #[tokio::test]
    async fn ars_sessions() {
        let ars = accepting_ars(Arc::new(SystemTime::now));
        let s = ars
            .create_session(AccountId(1), "01012345678", 60)
            .await
            .unwrap();
        assert_eq!(s.session_code.len(), 4);
        assert!(!ars.complete_session(AccountId(1), "wrong"));
        assert!(ars.complete_session(AccountId(1), &s.session_code));
        assert!(!ars.complete_session(AccountId(1), &s.session_code));
        let s = ars.create_session(AccountId(2), "", 0).await.unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(!ars.complete_session(AccountId(2), &s.session_code));
    }

    #[tokio::test]
    async fn disabled_ars_provider_refuses_to_create_a_session() {
        let ars = ArsService::default();
        assert!(ars
            .create_session(AccountId(1), "01012345678", 60)
            .await
            .is_err());
        assert!(!ars.complete_session(AccountId(1), "0000"));
    }

    #[test]
    fn phone_numbers_are_masked_for_logs() {
        assert_eq!(mask_phone("010-1234-5678"), "*******5678");
        assert_eq!(mask_phone("1234"), "1234");
        assert_eq!(mask_phone(""), "");
    }

    // ---- Port of AAEmu.UnitTests/Login/Core/Services/TwoFactor/ArsServiceTests.cs ------------

    /// `FakeTimeProvider`: a manually advanced clock.
    struct FakeTime(Arc<std::sync::Mutex<SystemTime>>);

    impl FakeTime {
        fn new() -> Self {
            // FakeTimeProvider starts at 2000-01-01T00:00:00Z.
            Self(Arc::new(std::sync::Mutex::new(
                UNIX_EPOCH + Duration::from_secs(946_684_800),
            )))
        }

        fn now(&self) -> SystemTime {
            *self.0.lock().unwrap()
        }

        fn advance(&self, d: Duration) {
            *self.0.lock().unwrap() += d;
        }

        fn clock(&self) -> Clock {
            let t = self.0.clone();
            Arc::new(move || *t.lock().unwrap())
        }
    }

    const ARS_ACCOUNT: AccountId = AccountId(7);
    const ARS_OTHER_ACCOUNT: AccountId = AccountId(99);

    /// A provider that accepts the call; the default one is deliberately disabled.
    #[derive(Clone, Debug, Default)]
    struct AcceptingArsProvider;

    impl ArsPhoneProvider for AcceptingArsProvider {
        fn call<'a>(
            &'a self,
            _account_id: AccountId,
            _phone_number: &'a str,
            _code: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            async move { Ok(()) }.boxed()
        }
    }

    fn accepting_ars(clock: Clock) -> ArsService {
        ArsService::with_provider(clock, Arc::new(AcceptingArsProvider))
    }

    #[tokio::test]
    async fn ars_create_session_returns_session_with_four_digit_code() {
        let time = FakeTime::new();
        let session = accepting_ars(time.clock())
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();

        assert_eq!(session.session_code.len(), 4);
        let code: i32 = session.session_code.parse().expect("numeric code");
        assert!((0..=9999).contains(&code));
    }

    #[tokio::test]
    async fn ars_create_session_sets_correct_expiry() {
        let time = FakeTime::new();
        let expected_expiry = time.now() + Duration::from_secs(120);

        let session = accepting_ars(time.clock())
            .create_session(ARS_ACCOUNT, "01012345678", 120)
            .await
            .unwrap();

        assert_eq!(session.expires_at, expected_expiry);
    }

    #[tokio::test]
    async fn ars_create_session_replaces_existing_session() {
        let time = FakeTime::new();
        let sut = accepting_ars(time.clock());
        sut.create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();
        let second = sut
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();

        // Only the second session should be active
        assert!(sut.complete_session(ARS_ACCOUNT, &second.session_code));
    }

    #[tokio::test]
    async fn ars_complete_session_no_active_session_returns_false() {
        let time = FakeTime::new();
        assert!(!accepting_ars(time.clock()).complete_session(ARS_ACCOUNT, "1234"));
    }

    #[tokio::test]
    async fn ars_complete_session_expired_session_returns_false() {
        let time = FakeTime::new();
        let sut = accepting_ars(time.clock());
        let session = sut
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();
        time.advance(Duration::from_secs(61));

        assert!(!sut.complete_session(ARS_ACCOUNT, &session.session_code));
    }

    #[tokio::test]
    async fn ars_complete_session_wrong_code_returns_false() {
        let time = FakeTime::new();
        let sut = accepting_ars(time.clock());
        let session = sut
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();
        let wrong_code = if session.session_code == "1234" {
            "5678"
        } else {
            "1234"
        };

        assert!(!sut.complete_session(ARS_ACCOUNT, wrong_code));
    }

    #[tokio::test]
    async fn ars_complete_session_correct_code_returns_true() {
        let time = FakeTime::new();
        let sut = accepting_ars(time.clock());
        let session = sut
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();

        assert!(sut.complete_session(ARS_ACCOUNT, &session.session_code));
    }

    #[tokio::test]
    async fn ars_complete_session_after_success_session_consumed() {
        let time = FakeTime::new();
        let sut = accepting_ars(time.clock());
        let session = sut
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();
        sut.complete_session(ARS_ACCOUNT, &session.session_code);

        assert!(!sut.complete_session(ARS_ACCOUNT, &session.session_code));
    }

    #[tokio::test]
    async fn ars_complete_session_different_account_returns_false() {
        let time = FakeTime::new();
        let sut = accepting_ars(time.clock());
        let session = sut
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();

        assert!(!sut.complete_session(ARS_OTHER_ACCOUNT, &session.session_code));
    }

    #[tokio::test]
    async fn ars_complete_session_at_exact_expiry_returns_false() {
        let time = FakeTime::new();
        let sut = accepting_ars(time.clock());
        let session = sut
            .create_session(ARS_ACCOUNT, "01012345678", 60)
            .await
            .unwrap();
        time.advance(Duration::from_secs(60));
        // At exactly ExpiresAt the session is still valid (now > expiresAt); one more tick
        // (100 ns) pushes past the boundary.
        assert_eq!(time.now(), session.expires_at);
        time.advance(Duration::from_nanos(100));

        assert!(!sut.complete_session(ARS_ACCOUNT, &session.session_code));
    }

    // ---- Per-account OTP limiter and replay protection --------------------------------------

    const OTP_ACCOUNT: AccountId = AccountId(77);
    const OTP_OTHER_ACCOUNT: AccountId = AccountId(78);
    const OTP_SECRET_B32: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

    fn otp_cfg() -> TwoFactorConfig {
        TwoFactorConfig {
            user_id: OTP_ACCOUNT,
            enabled_methods: TwoFactorMethod::OTP,
            otp_secret: Some(OTP_SECRET_B32.into()),
            otp_verified: true,
            cert_pin_hash: None,
            ars_phone_number: None,
            ars_phone_verified: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn otp_sut(clock: &FakeTime, max_failures: i32) -> OtpService {
        let options = TwoFactorOptions {
            max_otp_failures: max_failures,
            otp_lockout_minutes: 15,
        };
        let repo = TwoFactorRepository::new(
            sqlx::mysql::MySqlPoolOptions::new()
                .connect_lazy_with(sqlx::mysql::MySqlConnectOptions::new()),
        );
        OtpService::new(repo, FactorPolicy::with_clock(&options, clock.clock()))
    }

    fn otp_at(time: &FakeTime) -> String {
        let secret = base32::decode(OTP_SECRET_B32).unwrap();
        let secs = time.now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        totp::code_at(&secret, secs)
    }

    /// The limiter is per account, not per connection: the state is shared by clones of the
    /// service (one per login connection) and a wrong code counts on both.
    #[tokio::test]
    async fn otp_wrong_codes_lock_the_account_across_connections() {
        let time = FakeTime::new();
        let sut = otp_sut(&time, 3);
        let other_connection = sut.clone();
        let cfg = otp_cfg();

        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, "00000000")
                .await,
            FactorValidation::Invalid
        );
        assert_eq!(
            other_connection
                .validate_config(Some(&cfg), OTP_ACCOUNT, "11111111")
                .await,
            FactorValidation::Invalid
        );
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, "22222222")
                .await,
            FactorValidation::Locked,
            "the third failure must lock the account"
        );

        // A valid code cannot be used while locked.
        assert_eq!(
            other_connection
                .validate_config(Some(&cfg), OTP_ACCOUNT, &otp_at(&time))
                .await,
            FactorValidation::Locked
        );
        // Another account is unaffected.
        let mut other_cfg = otp_cfg();
        other_cfg.user_id = OTP_OTHER_ACCOUNT;
        assert_eq!(
            sut.validate_config(Some(&other_cfg), OTP_OTHER_ACCOUNT, &otp_at(&time))
                .await,
            FactorValidation::Valid
        );
    }

    /// A denied login does not reset the account counter: the next login starts from the
    /// failures already recorded, so the second connection locks the account.
    #[tokio::test]
    async fn otp_denied_login_does_not_reset_the_account_counter() {
        let time = FakeTime::new();
        let sut = otp_sut(&time, 2);
        let cfg = otp_cfg();

        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, "00000000")
                .await,
            FactorValidation::Invalid
        );
        // Re-login on a new connection (same service, shared state).
        assert_eq!(
            sut.clone()
                .validate_config(Some(&cfg), OTP_ACCOUNT, "11111111")
                .await,
            FactorValidation::Locked
        );
    }

    #[tokio::test]
    async fn otp_lock_expires_after_the_configured_duration() {
        let time = FakeTime::new();
        let sut = otp_sut(&time, 2);
        let cfg = otp_cfg();

        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, "00000000")
                .await,
            FactorValidation::Invalid
        );
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, "00000000")
                .await,
            FactorValidation::Locked
        );
        time.advance(Duration::from_secs(15 * 60));
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &otp_at(&time))
                .await,
            FactorValidation::Valid
        );
    }

    #[tokio::test]
    async fn otp_replayed_time_step_is_refused() {
        let time = FakeTime::new();
        let sut = otp_sut(&time, 5);
        let cfg = otp_cfg();
        let code = otp_at(&time);

        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &code).await,
            FactorValidation::Valid
        );
        // The same step is refused on a second connection sharing the service.
        assert_eq!(
            sut.clone()
                .validate_config(Some(&cfg), OTP_ACCOUNT, &code)
                .await,
            FactorValidation::Invalid
        );
        // Still inside the drift window, and after it, the used step stays refused.
        time.advance(Duration::from_secs(30));
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &code).await,
            FactorValidation::Invalid
        );
        time.advance(Duration::from_secs(60));
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &code).await,
            FactorValidation::Invalid
        );
        // The next step's code works.
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &otp_at(&time))
                .await,
            FactorValidation::Valid
        );
    }

    #[tokio::test]
    async fn otp_success_resets_the_failure_count() {
        let time = FakeTime::new();
        let sut = otp_sut(&time, 3);
        let cfg = otp_cfg();

        for _ in 0..2 {
            assert_eq!(
                sut.validate_config(Some(&cfg), OTP_ACCOUNT, "00000000")
                    .await,
                FactorValidation::Invalid
            );
        }
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &otp_at(&time))
                .await,
            FactorValidation::Valid
        );
        time.advance(Duration::from_secs(30));
        // Two more failures must not lock: the success reset the counter.
        for _ in 0..2 {
            assert_eq!(
                sut.validate_config(Some(&cfg), OTP_ACCOUNT, "00000000")
                    .await,
                FactorValidation::Invalid
            );
        }
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &otp_at(&time))
                .await,
            FactorValidation::Valid
        );
    }

    /// With the minimum accepted limit (1), the first wrong code locks the account. A limit of
    /// 0 is refused by config validation, not treated as "unlimited".
    #[tokio::test]
    async fn otp_max_failures_one_locks_after_one_failure() {
        let time = FakeTime::new();
        let sut = otp_sut(&time, 1);
        let cfg = otp_cfg();

        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, "00000000")
                .await,
            FactorValidation::Locked
        );
        assert_eq!(
            sut.validate_config(Some(&cfg), OTP_ACCOUNT, &otp_at(&time))
                .await,
            FactorValidation::Locked
        );
    }

    // ---- Port of AAEmu.UnitTests/Login/Core/Services/TwoFactor/PcCertServiceTests.cs ---------
    // The mocked `ITwoFactorRepository.GetConfigAsync` result is passed to
    // `validate_config`; the mocked `IPasswordService.VerifyPassword` results are produced by a
    // real `PasswordService` with real stored hashes (Success = current PBKDF2 hash,
    // SuccessRehashNeeded = legacy Base64(SHA-256) hash, Failed = wrong PIN).

    const PC_ACCOUNT: AccountId = AccountId(42);
    const TEST_PIN: &str = "12345678";

    fn pc_cert_sut() -> (PcCertService, PasswordService) {
        pc_cert_sut_with_max(TwoFactorOptions::default().max_otp_failures)
    }

    fn pc_cert_sut_with_max(max_failures: i32) -> (PcCertService, PasswordService) {
        let passwords = PasswordService { iterations: 1000 };
        let repo = TwoFactorRepository::new(
            sqlx::mysql::MySqlPoolOptions::new()
                .connect_lazy_with(sqlx::mysql::MySqlConnectOptions::new()),
        );
        let options = TwoFactorOptions {
            max_otp_failures: max_failures,
            otp_lockout_minutes: 15,
        };
        (
            PcCertService::new(
                repo,
                passwords.clone(),
                FactorPolicy::from_options(&options),
            ),
            passwords,
        )
    }

    fn make_config(cert_pin_hash: Option<&str>) -> TwoFactorConfig {
        TwoFactorConfig {
            user_id: PC_ACCOUNT,
            enabled_methods: TwoFactorMethod::PC_CERT,
            otp_secret: None,
            otp_verified: false,
            cert_pin_hash: cert_pin_hash.map(str::to_string),
            ars_phone_number: None,
            ars_phone_verified: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn pc_cert_validate_no_config_returns_false() {
        let (sut, _) = pc_cert_sut();
        assert!(!sut
            .validate_config(None, PC_ACCOUNT, TEST_PIN)
            .await
            .is_valid());
    }

    #[tokio::test]
    async fn pc_cert_validate_config_with_null_hash_returns_false() {
        let (sut, _) = pc_cert_sut();
        assert!(!sut
            .validate_config(Some(make_config(None)), PC_ACCOUNT, TEST_PIN)
            .await
            .is_valid());
    }

    #[tokio::test]
    async fn pc_cert_validate_config_with_empty_hash_returns_false() {
        let (sut, _) = pc_cert_sut();
        assert!(!sut
            .validate_config(Some(make_config(Some(""))), PC_ACCOUNT, TEST_PIN)
            .await
            .is_valid());
    }

    #[tokio::test]
    async fn pc_cert_validate_correct_pin_returns_true() {
        let (sut, passwords) = pc_cert_sut();
        let hash = passwords
            .hash_for_storage(&Password::from_plaintext(TEST_PIN))
            .unwrap();
        assert_eq!(
            passwords.verify_password(&hash, &Password::from_plaintext(TEST_PIN)),
            PasswordVerificationResult::Success
        );

        assert!(sut
            .validate_config(Some(make_config(Some(&hash))), PC_ACCOUNT, TEST_PIN)
            .await
            .is_valid());
    }

    #[tokio::test]
    async fn pc_cert_validate_correct_pin_legacy_format_returns_true() {
        use base64::Engine;
        use sha2::Digest;
        let (sut, passwords) = pc_cert_sut();
        let hash = base64::engine::general_purpose::STANDARD
            .encode(sha2::Sha256::digest(TEST_PIN.as_bytes()));
        assert_eq!(
            passwords.verify_password(&hash, &Password::from_plaintext(TEST_PIN)),
            PasswordVerificationResult::SuccessRehashNeeded
        );

        assert!(sut
            .validate_config(Some(make_config(Some(&hash))), PC_ACCOUNT, TEST_PIN)
            .await
            .is_valid());
    }

    #[tokio::test]
    async fn pc_cert_validate_wrong_pin_returns_false() {
        let (sut, passwords) = pc_cert_sut();
        let hash = passwords
            .hash_for_storage(&Password::from_plaintext("87654321"))
            .unwrap();

        assert!(!sut
            .validate_config(Some(make_config(Some(&hash))), PC_ACCOUNT, TEST_PIN)
            .await
            .is_valid());
    }

    /// PIN guesses are limited per account, across service clones (login connections). The
    /// second wrong PIN reaches the limit, and even the correct PIN is then refused.
    #[tokio::test]
    async fn pc_cert_wrong_pins_lock_the_account_across_connections() {
        let (sut, passwords) = pc_cert_sut_with_max(2);
        let hash = passwords
            .hash_for_storage(&Password::from_plaintext(TEST_PIN))
            .unwrap();
        let config = make_config(Some(&hash));
        let other_connection = sut.clone();

        assert_eq!(
            sut.validate_config(Some(config.clone()), PC_ACCOUNT, "00000000")
                .await,
            FactorValidation::Invalid
        );
        assert_eq!(
            other_connection
                .validate_config(Some(config.clone()), PC_ACCOUNT, "11111111")
                .await,
            FactorValidation::Locked,
            "the second wrong PIN must lock the account"
        );
        assert_eq!(
            sut.validate_config(Some(config), PC_ACCOUNT, TEST_PIN)
                .await,
            FactorValidation::Locked,
            "a correct PIN must not bypass the lock"
        );
    }
}
