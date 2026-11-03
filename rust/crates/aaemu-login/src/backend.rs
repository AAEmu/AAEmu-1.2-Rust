//! The account/2FA operations the authentication flows depend on — the Rust counterpart of the
//! C# `ILoginController`, `ITwoFactorService`, `IOtpService`, `IPcCertService` and `IArsService`
//! seams that the flows receive through DI.
//!
//! [`DbAuthBackend`] is the production implementation (MySQL-backed); tests substitute their own
//! implementation via [`crate::server::LoginServer::with_auth_backend`].

use std::net::IpAddr;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::FutureExt;

use crate::controllers::login::LoginController;
use crate::models::{
    AccountId, KoreaAuthInfo, LoginDeniedReason, LoginResult, TwoFactorRequirements,
};
use crate::services::password::Password;
use crate::services::two_factor::{
    ArsService, ArsSession, FactorValidation, OtpService, PcCertService, TwoFactorService,
};

/// Account and two-factor operations used by the authentication flows. Dyn-compatible (futures
/// are boxed) so the server can hold an `Arc<dyn AuthBackend>`.
pub trait AuthBackend: Send + Sync {
    /// `ILoginController.Login` (EU method auth).
    fn login<'a>(
        &'a self,
        username: &'a str,
        password: &'a Password,
        ip: IpAddr,
    ) -> BoxFuture<'a, anyhow::Result<LoginResult>>;

    /// `ILoginController.GetKoreaAuthInfoAsync`.
    fn get_korea_auth_info<'a>(
        &'a self,
        username: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<KoreaAuthInfo>>>;

    /// `ILoginController.CheckBanStatusAsync`.
    fn check_ban_status(
        &self,
        account_id: AccountId,
    ) -> BoxFuture<'_, anyhow::Result<(bool, LoginDeniedReason)>>;

    /// `ITwoFactorService.GetRequirementsAsync`.
    fn get_two_factor_requirements(
        &self,
        account_id: AccountId,
    ) -> BoxFuture<'_, anyhow::Result<TwoFactorRequirements>>;

    /// `IOtpService.ValidateAsync`, including the per-account failure limiter and replay check.
    fn validate_otp<'a>(
        &'a self,
        account_id: AccountId,
        code: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<FactorValidation>>;

    /// `IPcCertService.ValidateAsync`, including the per-account failure limiter.
    fn validate_pc_cert<'a>(
        &'a self,
        account_id: AccountId,
        pin: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<FactorValidation>>;

    /// Records a fully authenticated login (`users.last_ip`, `last_login`, `updated_at`) and the
    /// "connected" log line. Called only after every required factor has passed.
    fn record_login<'a>(
        &'a self,
        account_id: AccountId,
        username: &'a str,
        ip: IpAddr,
    ) -> BoxFuture<'a, anyhow::Result<()>>;

    /// `IArsService.CreateSessionAsync`: creates the session and asks the phone provider to call.
    fn create_ars_session(
        &self,
        account_id: AccountId,
        timeout_seconds: i32,
    ) -> BoxFuture<'_, anyhow::Result<ArsSession>>;
}

/// The production [`AuthBackend`]: the MySQL-backed controllers and services.
pub struct DbAuthBackend {
    pub logins: Arc<LoginController>,
    pub two_factor: TwoFactorService,
    pub otp: OtpService,
    pub pc_cert: PcCertService,
    pub ars: Arc<ArsService>,
}

impl AuthBackend for DbAuthBackend {
    fn login<'a>(
        &'a self,
        username: &'a str,
        password: &'a Password,
        ip: IpAddr,
    ) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        self.logins.login(username, password, ip).boxed()
    }

    fn get_korea_auth_info<'a>(
        &'a self,
        username: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<KoreaAuthInfo>>> {
        self.logins.get_korea_auth_info(username).boxed()
    }

    fn check_ban_status(
        &self,
        account_id: AccountId,
    ) -> BoxFuture<'_, anyhow::Result<(bool, LoginDeniedReason)>> {
        self.logins.check_ban_status(account_id).boxed()
    }

    fn get_two_factor_requirements(
        &self,
        account_id: AccountId,
    ) -> BoxFuture<'_, anyhow::Result<TwoFactorRequirements>> {
        async move { Ok(self.two_factor.get_requirements(account_id).await?) }.boxed()
    }

    fn validate_otp<'a>(
        &'a self,
        account_id: AccountId,
        code: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<FactorValidation>> {
        async move { Ok(self.otp.validate(account_id, code).await?) }.boxed()
    }

    fn validate_pc_cert<'a>(
        &'a self,
        account_id: AccountId,
        pin: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<FactorValidation>> {
        async move { Ok(self.pc_cert.validate(account_id, pin).await?) }.boxed()
    }

    fn record_login<'a>(
        &'a self,
        account_id: AccountId,
        username: &'a str,
        ip: IpAddr,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        self.logins.record_login(account_id, username, ip).boxed()
    }

    fn create_ars_session(
        &self,
        account_id: AccountId,
        timeout_seconds: i32,
    ) -> BoxFuture<'_, anyhow::Result<ArsSession>> {
        async move {
            let phone_number = self
                .two_factor
                .get_config(account_id)
                .await?
                .and_then(|c| c.ars_phone_number)
                .unwrap_or_default();
            self.ars
                .create_session(account_id, &phone_number, timeout_seconds)
                .await
        }
        .boxed()
    }
}
