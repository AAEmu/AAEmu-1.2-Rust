//! Authentication flows (`AAEmu.Login.Core.Authentication`).
//!
//! Flows are pure decision-makers that return [`AuthFlowResult`] values; the login session
//! manages the lifecycle (keeping the flow on `Pending`, clearing it on `Success`/`Denied`).
//! Only [`KoreaAuthFlow`] is multi-step; the C# interfaces `IChallengeAuthFlow`,
//! `IChallenge2AuthFlow`, `IOtpAuthFlow`, `ICertAuthFlow` and `IArsAuthFlow` are its
//! [`KoreaStep`]s.

use std::net::IpAddr;
use std::time::Duration;

use rand::RngCore;

use crate::models::{
    AccountId, GameServerId, LoginCharacterInfo, LoginDeniedReason, TwoFactorRequirements,
};
use crate::packets::l2c::WorldInfo;
use crate::packets::PacketError;
use crate::server::LoginServer;
use crate::services::korea;
use crate::services::password::Password;
use crate::services::two_factor::FactorValidation;

/// The result of an authentication flow step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthFlowResult {
    /// Authentication completed successfully.
    Success {
        account_id: AccountId,
        username: Option<String>,
    },
    /// Authentication was denied; the reason is sent to the client.
    Denied(LoginDeniedReason),
    /// The flow is in progress and managing itself (it already sent its own prompt).
    Pending,
}

/// `ILoginClient`: sends response packets to the client, decoupling flows from packet building.
pub trait LoginClient: Send + Sync {
    fn send_auth_success(&self, account_id: AccountId) -> Result<(), PacketError>;
    fn send_auth_denied(&self, reason: LoginDeniedReason) -> Result<(), PacketError>;
    fn send_world_list(
        &self,
        worlds: Vec<WorldInfo>,
        characters: Vec<LoginCharacterInfo>,
    ) -> Result<(), PacketError>;
    /// V1 Korea challenge (`ACChallengePacket`). V1 is not enabled — SHA-1 without a per-account
    /// salt is ~17Mx weaker than PBKDF2@600k. Preserved for protocol completeness only.
    fn send_challenge(&self, salt: u32, hc: &[u32]) -> Result<(), PacketError>;
    /// V2 Korea challenge (`ACChallenge2Packet`). The client derives the AES-256 key from `round`
    /// and `salt` via sha256_crypt, then computes the 8-uint challenge response.
    fn send_challenge2(&self, round: i32, salt: &str, hc: &[u32]) -> Result<(), PacketError>;
    fn send_otp_prompt(&self, maximum_tries: i32, current_try: i32) -> Result<(), PacketError>;
    fn send_cert_prompt(&self, maximum_tries: i32, current_try: i32) -> Result<(), PacketError>;
    fn send_ars_prompt(&self, session_code: &str, timeout: Duration) -> Result<(), PacketError>;
    fn send_world_cookie(&self, cookie: u32, host: &str, port: u16) -> Result<(), PacketError>;
    fn send_enter_world_denied(&self, reason: u8) -> Result<(), PacketError>;
}

/// Username/password authentication (EU method); single step.
#[derive(Clone, Debug)]
pub struct PasswordAuthFlow {
    /// `None` reproduces the C# behaviour of a Trion ticket without username/password, where
    /// a `null` username is passed down to the login controller.
    pub username: Option<String>,
    pub password: Option<Password>,
    pub client_ip: IpAddr,
}

/// Reconnection validation, wrapped as a flow so it goes through the normal authenticate path.
#[derive(Clone, Debug)]
pub struct ReconnectAuthFlow {
    pub gs_id: GameServerId,
    pub account_id: AccountId,
    pub token: u32,
    /// The IP of the connection presenting the token; must match the login the token was issued
    /// for.
    pub client_ip: IpAddr,
}

/// Any authentication flow.
#[derive(Debug)]
pub enum AuthFlow {
    Password(PasswordAuthFlow),
    Reconnect(ReconnectAuthFlow),
    Korea(Box<KoreaAuthFlow>),
    /// The post-password second-factor steps of a password login (after [`AuthFlow::Password`]
    /// verified the password and the account requires 2FA).
    SecondFactor(Box<SecondFactorFlow>),
}

impl AuthFlow {
    /// The C# class name, for logging.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Password(_) => "PasswordAuthFlow",
            Self::Reconnect(_) => "ReconnectAuthFlow",
            Self::Korea(_) => "KoreaAuthFlow",
            Self::SecondFactor(_) => "SecondFactorAuthFlow",
        }
    }

    /// Whether this flow can be continued by a client packet (`CAOtpNumberPacket`,
    /// `CAPcCertNumberPacket`, ...). `Connected` flows that already returned their final result
    /// are never stored.
    pub fn awaits_continuation(&self) -> bool {
        matches!(self, Self::Korea(_) | Self::SecondFactor(_))
    }

    /// A denied reconnect closes the connection: its token is consumed on the attempt, so the
    /// same socket must not be able to keep guessing.
    pub fn denial_closes_connection(&self) -> bool {
        matches!(self, Self::Reconnect(_))
    }

    /// `IAuthenticationFlow.StartAsync`. An `Err` corresponds to an exception escaping the C#
    /// flow (e.g. a database error).
    pub async fn start(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
    ) -> anyhow::Result<AuthFlowResult> {
        match self {
            Self::Password(f) => {
                let Some(username) = f.username.clone() else {
                    // C#: `SELECT ... username=NULL` finds nothing; then either BadAccount or,
                    // with AutoAccount, `UsernameRegex().IsMatch(null)` throws.
                    if server.config.auto_account {
                        anyhow::bail!("Value cannot be null. (Parameter 'input'): Trion ticket without username");
                    }
                    return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_ACCOUNT));
                };
                let Some(password) = f.password.clone() else {
                    if server.config.auto_account {
                        anyhow::bail!("Value cannot be null. (Parameter 'input'): password");
                    }
                    return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_ACCOUNT));
                };
                let client_ip = f.client_ip;
                let r = server.auth.login(&username, &password, client_ip).await?;
                if !r.success {
                    return Ok(AuthFlowResult::Denied(r.denial_reason));
                }
                // Enforce the account's 2FA on every password path (Trion ticket, Mail.Ru and
                // plain password). The password is verified; the factors and the final ban check
                // happen in the shared state machine.
                if r.two_factor.any_required() {
                    let mut second_factor =
                        SecondFactorFlow::new(r.account_id, username, client_ip, r.two_factor);
                    let result = second_factor.start(server, client).await?;
                    *self = Self::SecondFactor(Box::new(second_factor));
                    return Ok(result);
                }
                // No factors: the login is complete. Record it only now, like the factor path.
                server
                    .auth
                    .record_login(r.account_id, &username, client_ip)
                    .await?;
                Ok(AuthFlowResult::Success {
                    account_id: r.account_id,
                    username: Some(username),
                })
            }
            Self::Reconnect(f) => {
                let r = server.logins.reconnect(
                    &server.games,
                    f.gs_id,
                    f.account_id,
                    f.token,
                    f.client_ip,
                )?;
                if !r.success {
                    let failure = r.failure;
                    if let Some(failure) = failure {
                        tracing::debug!(
                            "Reconnect for account {} on world {} refused: {:?}",
                            f.account_id,
                            f.gs_id,
                            failure
                        );
                        return Ok(AuthFlowResult::Denied(failure.reason()));
                    }
                    return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
                }
                // The token is only issued by the game after a fully authenticated session, but
                // the account may have been banned since; do not skip the ban check.
                let (banned, reason) = server.auth.check_ban_status(r.account_id).await?;
                if banned {
                    return Ok(AuthFlowResult::Denied(reason));
                }
                Ok(AuthFlowResult::Success {
                    account_id: r.account_id,
                    username: None,
                })
            }
            Self::Korea(f) => f.start(server, client).await,
            Self::SecondFactor(f) => f.start(server, client).await,
        }
    }

    /// Executes one continuation step (`IAuthenticationFlow.ContinueAsync` and the factor
    /// interfaces of the C# flows).
    pub async fn step(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
        step: KoreaStep,
    ) -> anyhow::Result<AuthFlowResult> {
        match self {
            Self::Korea(f) => f.step(server, client, step).await,
            Self::SecondFactor(f) => f.step(server, client, step).await,
            _ => Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE)),
        }
    }
}

/// A continuation of a multi-step flow, triggered by a client packet (or the ARS callback).
/// `Debug` redacts OTP codes and PC-certificate PINs.
#[derive(Clone)]
pub enum KoreaStep {
    /// `IChallengeAuthFlow.ContinueAsync` (`CAChallengeResponsePacket`).
    ChallengeV1 { ch: [u32; 4], pw: Vec<u8> },
    /// `IChallenge2AuthFlow.ContinueV2Async` (`CAChallengeResponse2Packet`).
    ChallengeV2 { ch: [u32; 8] },
    /// `IOtpAuthFlow.SubmitOtpAsync` (`CAOtpNumberPacket`).
    Otp(String),
    /// `ICertAuthFlow.SubmitCertAsync` (`CAPcCertNumberPacket`).
    Cert(String),
    /// `IArsAuthFlow.CompleteArsAsync` (external phone system callback).
    Ars(bool),
}

impl std::fmt::Debug for KoreaStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChallengeV1 { ch, pw } => f
                .debug_struct("ChallengeV1")
                .field("ch", ch)
                .field("pw", &format!("<redacted {} bytes>", pw.len()))
                .finish(),
            Self::ChallengeV2 { ch } => f.debug_struct("ChallengeV2").field("ch", ch).finish(),
            Self::Otp(_) => f.debug_tuple("Otp").field(&"<redacted>").finish(),
            Self::Cert(_) => f.debug_tuple("Cert").field(&"<redacted>").finish(),
            Self::Ars(success) => f.debug_tuple("Ars").field(success).finish(),
        }
    }
}

impl KoreaStep {
    /// The C# flow interface this step belongs to (for the type-mismatch log).
    pub fn interface_name(&self) -> &'static str {
        match self {
            Self::ChallengeV1 { .. } => "IChallengeAuthFlow",
            Self::ChallengeV2 { .. } => "IChallenge2AuthFlow",
            Self::Otp(_) => "IOtpAuthFlow",
            Self::Cert(_) => "ICertAuthFlow",
            Self::Ars(_) => "IArsAuthFlow",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KoreaState {
    AwaitingChallengeResponse,
    AwaitingSecondFactor,
}

/// The ordered post-password verification steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecondFactorStep {
    NotStarted,
    AwaitingOtp,
    AwaitingCert,
    AwaitingArs,
    Completed,
}

/// The shared post-password half of a multi-step login: OTP, PC certificate and ARS as required
/// by the account's `user_2fa` row, then the ban check and `Success`.
///
/// Used by [`KoreaAuthFlow`] after its challenge-response and directly by the password
/// (Trion ticket / Mail.Ru / plain password) flows.
pub struct SecondFactorFlow {
    account_id: AccountId,
    username: String,
    /// The client IP, recorded with the login once every factor passed.
    client_ip: IpAddr,
    requirements: TwoFactorRequirements,
    step: SecondFactorStep,
    otp_attempts: i32,
    cert_attempts: i32,
}

impl std::fmt::Debug for SecondFactorFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecondFactorFlow")
            .field("account_id", &self.account_id)
            .field("username", &self.username)
            .field("requirements", &self.requirements)
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl SecondFactorFlow {
    pub fn new(
        account_id: AccountId,
        username: impl Into<String>,
        client_ip: IpAddr,
        requirements: TwoFactorRequirements,
    ) -> Self {
        Self {
            account_id,
            username: username.into(),
            client_ip,
            requirements,
            step: SecondFactorStep::NotStarted,
            otp_attempts: 0,
            cert_attempts: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_step(mut self, step: SecondFactorStep) -> Self {
        self.step = step;
        self
    }

    /// Sends the first required prompt, or completes immediately (ban check + `Success`).
    pub async fn start(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
    ) -> anyhow::Result<AuthFlowResult> {
        self.advance(server, client).await
    }

    /// Executes one factor step.
    pub async fn step(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
        step: KoreaStep,
    ) -> anyhow::Result<AuthFlowResult> {
        match step {
            KoreaStep::Otp(code) => self.submit_otp(server, client, &code).await,
            KoreaStep::Cert(pin) => self.submit_cert(server, client, &pin).await,
            KoreaStep::Ars(success) => self.complete_ars(server, client, success).await,
            KoreaStep::ChallengeV1 { .. } | KoreaStep::ChallengeV2 { .. } => {
                Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE))
            }
        }
    }

    async fn submit_otp(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
        code: &str,
    ) -> anyhow::Result<AuthFlowResult> {
        if self.step != SecondFactorStep::AwaitingOtp {
            return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
        }
        self.otp_attempts += 1;
        match server.auth.validate_otp(self.account_id, code).await? {
            FactorValidation::Valid => self.advance(server, client).await,
            FactorValidation::Locked => {
                tracing::warn!(
                    "OTP login refused for account {}: too many failed attempts",
                    self.account_id
                );
                Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE))
            }
            FactorValidation::Invalid => {
                let max = server.config.korea_auth.max_otp_attempts;
                if self.otp_attempts >= max {
                    return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
                }
                // Send retry packet
                client.send_otp_prompt(max, self.otp_attempts + 1)?;
                Ok(AuthFlowResult::Pending)
            }
        }
    }

    async fn submit_cert(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
        pin: &str,
    ) -> anyhow::Result<AuthFlowResult> {
        if self.step != SecondFactorStep::AwaitingCert {
            return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
        }
        self.cert_attempts += 1;
        match server.auth.validate_pc_cert(self.account_id, pin).await? {
            FactorValidation::Valid => self.advance(server, client).await,
            FactorValidation::Locked => {
                tracing::warn!(
                    "PC certificate login refused for account {}: too many failed attempts",
                    self.account_id
                );
                Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE))
            }
            FactorValidation::Invalid => {
                let max = server.config.korea_auth.max_cert_attempts;
                if self.cert_attempts >= max {
                    return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
                }
                // Send retry packet
                client.send_cert_prompt(max, self.cert_attempts + 1)?;
                Ok(AuthFlowResult::Pending)
            }
        }
    }

    async fn complete_ars(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
        success: bool,
    ) -> anyhow::Result<AuthFlowResult> {
        if self.step != SecondFactorStep::AwaitingArs || !success {
            return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
        }
        self.advance(server, client).await
    }

    /// Advances to the next required verification step, or completes authentication. The ban
    /// check happens immediately before the final Success to avoid leaking account status
    /// prior to successful authentication.
    async fn advance(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
    ) -> anyhow::Result<AuthFlowResult> {
        if self.step <= SecondFactorStep::NotStarted && self.requirements.requires_otp {
            self.step = SecondFactorStep::AwaitingOtp;
            client.send_otp_prompt(0, 0)?;
            return Ok(AuthFlowResult::Pending);
        }
        if self.step <= SecondFactorStep::AwaitingOtp && self.requirements.requires_pc_cert {
            self.step = SecondFactorStep::AwaitingCert;
            client.send_cert_prompt(0, 0)?;
            return Ok(AuthFlowResult::Pending);
        }
        if self.step <= SecondFactorStep::AwaitingCert && self.requirements.requires_ars {
            self.step = SecondFactorStep::AwaitingArs;
            let timeout = server.config.korea_auth.ars_timeout;
            let session = server
                .auth
                .create_ars_session(self.account_id, timeout.as_secs() as i32)
                .await?;
            client.send_ars_prompt(&session.session_code, timeout)?;
            return Ok(AuthFlowResult::Pending);
        }
        // All steps complete. Check ban status before issuing success, then record the login.
        self.step = SecondFactorStep::Completed;
        let (banned, reason) = server.auth.check_ban_status(self.account_id).await?;
        if banned {
            return Ok(AuthFlowResult::Denied(reason));
        }
        server
            .auth
            .record_login(self.account_id, &self.username, self.client_ip)
            .await?;
        Ok(AuthFlowResult::Success {
            account_id: self.account_id,
            username: Some(self.username.clone()),
        })
    }
}

/// Korea authentication: V2 challenge-response, then the shared [`SecondFactorFlow`] with OTP,
/// certificate and ARS verification as required by the account (`user_2fa`).
pub struct KoreaAuthFlow {
    username: String,
    client_ip: IpAddr,
    state: KoreaState,
    account_id: AccountId,
    // V2 challenge state (cleared after successful verification)
    v2_hc: Vec<u32>,
    challenge_key: Option<[u8; 32]>,
    second_factor: Option<SecondFactorFlow>,
}

impl std::fmt::Debug for KoreaAuthFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KoreaAuthFlow")
            .field("username", &self.username)
            .field("state", &self.state)
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

impl KoreaAuthFlow {
    pub fn new(username: impl Into<String>, client_ip: IpAddr) -> Self {
        Self {
            username: username.into(),
            client_ip,
            state: KoreaState::AwaitingChallengeResponse,
            account_id: AccountId::default(),
            v2_hc: Vec::new(),
            challenge_key: None,
            second_factor: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_second_factor(&mut self, flow: SecondFactorFlow) {
        self.state = KoreaState::AwaitingSecondFactor;
        self.second_factor = Some(flow);
    }

    #[cfg(test)]
    pub(crate) fn set_state_not_awaiting_challenge(&mut self) {
        self.state = KoreaState::AwaitingSecondFactor;
    }

    async fn start(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
    ) -> anyhow::Result<AuthFlowResult> {
        if !server.config.korea_auth.enabled {
            return Ok(AuthFlowResult::Denied(LoginDeniedReason::LOGIN_UNKNOWN));
        }
        // Look up the account's Korea challenge material.
        // No ban check here — doing so before authentication would leak whether an account exists.
        let Some(info) = server.auth.get_korea_auth_info(&self.username).await? else {
            // Account doesn't exist, or has no korea_challenge_hash yet. The user must log in via
            // EU auth first (with KoreaAuth:Enabled=true) to seed the hash.
            return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_ACCOUNT));
        };
        self.account_id = info.account_id;
        self.challenge_key = Some(info.challenge_key_hash);

        // Generate 8 cryptographically secure random uint32 challenge values
        let mut rng_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut rng_bytes);
        self.v2_hc = rng_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();

        client.send_challenge2(info.challenge_rounds, &info.challenge_salt, &self.v2_hc)?;
        Ok(AuthFlowResult::Pending)
    }

    /// Executes one continuation step.
    pub async fn step(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
        step: KoreaStep,
    ) -> anyhow::Result<AuthFlowResult> {
        match step {
            // V1 Korea challenge is disabled (SHA-1 without per-account salt is ~17Mx weaker than
            // PBKDF2@600k). The server never issues ACChallengePacket, so always deny.
            KoreaStep::ChallengeV1 { .. } => {
                Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE))
            }
            KoreaStep::ChallengeV2 { ch } => self.continue_v2(server, client, &ch).await,
            KoreaStep::Otp(_) | KoreaStep::Cert(_) | KoreaStep::Ars(_) => {
                let Some(second_factor) = self.second_factor.as_mut() else {
                    return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
                };
                second_factor.step(server, client, step).await
            }
        }
    }

    async fn continue_v2(
        &mut self,
        server: &LoginServer,
        client: &dyn LoginClient,
        ch: &[u32],
    ) -> anyhow::Result<AuthFlowResult> {
        if self.state != KoreaState::AwaitingChallengeResponse {
            return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE));
        }
        let key = self.challenge_key.unwrap_or_default();
        let key_slice: &[u8] = if self.challenge_key.is_some() {
            &key
        } else {
            &[]
        };
        if !korea::verify_v2(key_slice, &self.v2_hc, ch) {
            return Ok(AuthFlowResult::Denied(LoginDeniedReason::BAD_ACCOUNT));
        }
        // Clear key material now that verification is done
        self.challenge_key = None;
        self.v2_hc.clear();

        // Query 2FA requirements from the database, then run the shared post-password steps.
        let requirements = server
            .auth
            .get_two_factor_requirements(self.account_id)
            .await?;
        let mut second_factor = SecondFactorFlow::new(
            self.account_id,
            self.username.clone(),
            self.client_ip,
            requirements,
        );
        let result = second_factor.start(server, client).await?;
        self.state = KoreaState::AwaitingSecondFactor;
        self.second_factor = Some(second_factor);
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
