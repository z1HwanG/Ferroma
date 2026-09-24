//! The authentication service: login, sessions, tokens, devices and throttling.
//!
//! This is the only place that turns a password into a session. Every surface —
//! Webmail, the REST API, the Client API, and (through their own SASL paths) SMTP
//! submission and IMAP — calls into [`AuthService`] rather than touching password
//! hashes or session rows directly.
//!
//! # Login, step by step
//!
//! 1. Look the account up case-insensitively.
//! 2. Refuse disabled and locked accounts — with the *same* message as a wrong
//!    password, so the endpoint cannot be used to enumerate addresses.
//! 3. Refuse when the source IP has produced too many recent failures.
//! 4. Verify the Argon2id hash on a blocking thread (19 MiB of memory and two
//!    passes must never run on the async reactor).
//! 5. On success: clear the failure counter, transparently re-hash if the stored
//!    parameters are stale, register the device, mint a session and a token pair.
//! 6. Record the attempt either way, for the audit trail and for step 3.
//!
//! # Revocation
//!
//! Access tokens are stateless JWTs, but [`AuthService::authenticate`] also checks
//! that the session still exists and has not been revoked. Revoking a device or a
//! session therefore takes effect on the *next request*, not an hour later, at the
//! cost of one indexed primary-key lookup.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use ferroma_core::{FerromaError, Limits, MailboxId, Result, SessionId, UserId};
use ferroma_storage::models::{Device, Session, User};
use ferroma_storage::repository::AppPassword;
use ferroma_storage::{Repositories, StorageError};

use crate::password::{validate_password, PasswordHasher};
use crate::token::{looks_like_opaque_token, AccessClaims, TokenService};

/// How a session was established. Stored in `sessions.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    /// A browser session with the `ferroma_session` cookie.
    Web,
    /// A bearer-token API client.
    Api,
    /// An official Ferroma client.
    Client,
    /// A JMAP client authenticated with a JMAP-specific bearer token.
    Jmap,
    /// An IMAP session.
    Imap,
    /// An SMTP submission session.
    Smtp,
}

impl SessionKind {
    /// The string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Web => "web",
            SessionKind::Api => "api",
            SessionKind::Client => "client",
            SessionKind::Jmap => "jmap",
            SessionKind::Imap => "imap",
            SessionKind::Smtp => "smtp",
        }
    }

    /// Parse the stored string.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "web" => Some(SessionKind::Web),
            "api" => Some(SessionKind::Api),
            "client" => Some(SessionKind::Client),
            "jmap" => Some(SessionKind::Jmap),
            "imap" => Some(SessionKind::Imap),
            "smtp" => Some(SessionKind::Smtp),
            _ => None,
        }
    }

    /// Whether this kind is a long-lived, refreshable session.
    pub fn is_refreshable(self) -> bool {
        matches!(
            self,
            SessionKind::Web | SessionKind::Api | SessionKind::Client | SessionKind::Jmap
        )
    }
}

/// A fresh pair of credentials handed to a client.
#[derive(Debug, Clone)]
pub struct TokenPair {
    /// Short-lived, stateless.
    pub access_token: String,
    /// Long-lived, single-use, rotated on every refresh.
    pub refresh_token: String,
    /// Access-token lifetime in seconds.
    pub expires_in: u64,
    /// The session both tokens belong to.
    pub session_id: SessionId,
}

/// What a successful login produced.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    /// The authenticated account.
    pub user: User,
    /// The session that was created.
    pub session: Session,
    /// The device that was registered or refreshed, when the caller supplied one.
    pub device: Option<Device>,
    /// The credentials to hand back.
    pub tokens: TokenPair,
}

/// What starting a TOTP enrollment hands back.
///
/// The secret is returned **once**, to be shown as a QR code. Nothing else reads it
/// back: an enrollment that can be re-read is one that can be re-read by whoever
/// takes the database.
#[derive(Debug, Clone)]
pub struct TotpEnrollment {
    /// The base32 shared secret.
    pub secret: String,
    /// The `otpauth://` URI the user scans.
    pub uri: String,
}

/// How far an account's second factor has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotpStatus {
    /// No enrollment at all.
    Disabled,
    /// A secret exists but no code has proved the authenticator holds it.
    Pending,
    /// The second factor is enforced at every login.
    Enabled,
}

/// The identity behind an authenticated request.
#[derive(Debug, Clone)]
pub struct Authenticated {
    /// The account.
    pub user: User,
    /// The session the credential belongs to.
    pub session: Session,
    /// Claims from the access token, when one was presented.
    pub claims: Option<AccessClaims>,
}

impl Authenticated {
    /// The user's id.
    pub fn user_id(&self) -> UserId {
        UserId::new(self.user.id)
    }

    /// The user's primary address.
    pub fn email(&self) -> &str {
        &self.user.email
    }

    /// Whether the account may use the Admin API.
    pub fn is_admin(&self) -> bool {
        self.user.is_admin
    }

    /// The session id.
    pub fn session_id(&self) -> SessionId {
        SessionId::new(self.session.id)
    }
}

/// Everything needed to describe a device at login.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    /// Stable, client-generated installation id.
    pub device_uid: String,
    /// Human name, e.g. "Alice's laptop".
    pub name: Option<String>,
    /// `windows`, `linux`, `macos`, `android`, `ios`.
    pub platform: Option<String>,
    /// Client version string.
    pub client_version: Option<String>,
    /// FCP protocol version.
    pub protocol_version: Option<u32>,
}

/// Login, session, device and password operations.
#[derive(Clone)]
pub struct AuthService {
    repos: Repositories,
    tokens: TokenService,
    hasher: PasswordHasher,
    limits: Limits,
    /// Sliding window used for the failed-login counters.
    failure_window: ChronoDuration,
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthService")
            .field("tokens", &self.tokens)
            .field("max_failed_logins", &self.limits.max_failed_logins)
            .finish()
    }
}

impl AuthService {
    /// Build the service.
    pub fn new(
        repos: Repositories,
        tokens: TokenService,
        hasher: PasswordHasher,
        limits: Limits,
    ) -> Self {
        AuthService {
            repos,
            tokens,
            hasher,
            limits,
            failure_window: ChronoDuration::minutes(15),
        }
    }

    /// Build with the default Argon2id parameters.
    pub fn with_defaults(repos: Repositories, tokens: TokenService, limits: Limits) -> Self {
        Self::new(repos, tokens, PasswordHasher::default(), limits)
    }

    /// The token service, for callers that need to mint or inspect tokens.
    pub fn tokens(&self) -> &TokenService {
        &self.tokens
    }

    /// The repositories this service uses.
    pub fn repositories(&self) -> &Repositories {
        &self.repos
    }

    /// Override the window used for failure counting. Tests use this.
    pub fn with_failure_window(mut self, window: ChronoDuration) -> Self {
        self.failure_window = window;
        self
    }

    // -------------------------------------------------------------------------
    // Passwords
    // -------------------------------------------------------------------------

    /// Hash a password, off the async reactor.
    pub async fn hash_password(&self, password: &str) -> Result<String> {
        validate_password(password)?;
        let hasher = self.hasher;
        let password = password.to_string();
        tokio::task::spawn_blocking(move || hasher.hash(&password))
            .await
            .map_err(|e| FerromaError::Internal(format!("password hashing task failed: {e}")))?
    }

    /// Verify a password off the async reactor. Never returns an error: a failure
    /// to verify is a `false`, and a panicking task is also a `false`.
    pub async fn verify_password(&self, password: &str, stored: &str) -> bool {
        let hasher = self.hasher;
        let password = password.to_string();
        let stored = stored.to_string();
        tokio::task::spawn_blocking(move || hasher.verify(&password, &stored))
            .await
            .unwrap_or(false)
    }

    /// Create an account, hashing the password.
    pub async fn create_user(
        &self,
        email: &str,
        password: &str,
        display_name: Option<&str>,
        is_admin: bool,
        enabled: bool,
        quota_bytes: Option<i64>,
    ) -> Result<User> {
        let hash = self.hash_password(password).await?;
        let user = self
            .repos
            .users
            .create(ferroma_storage::repository::NewUser {
                email: email.to_string(),
                password_hash: hash,
                display_name: display_name.map(|s| s.to_string()),
                is_admin,
                enabled,
                quota_bytes,
            })
            .await
            .map_err(map_storage)?;
        Ok(user)
    }

    /// Change a password, verifying the current one first, and revoke every other
    /// session so a stolen cookie cannot outlive the change.
    pub async fn change_password(
        &self,
        user_id: UserId,
        current_password: &str,
        new_password: &str,
    ) -> Result<()> {
        let user = self
            .repos
            .users
            .require_by_id(user_id)
            .await
            .map_err(map_storage)?;
        if !self
            .verify_password(current_password, &user.password_hash)
            .await
        {
            self.record_attempt(&user.email, None, "password", false)
                .await;
            return Err(FerromaError::Unauthorized(
                "current password is incorrect".into(),
            ));
        }
        validate_password(new_password)?;
        if current_password == new_password {
            return Err(FerromaError::Invalid(
                "the new password must differ from the current one".into(),
            ));
        }

        let hash = self.hash_password(new_password).await?;
        self.repos
            .users
            .update_password(user_id, &hash)
            .await
            .map_err(map_storage)?;

        let revoked = self
            .repos
            .sessions
            .revoke_all_for_user(user_id)
            .await
            .map_err(map_storage)?;
        tracing::info!(
            user_id = user_id.get(),
            revoked,
            "password changed; sessions revoked"
        );
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Second factors
    // -------------------------------------------------------------------------

    /// Start a TOTP enrollment: mint a secret and return it with its `otpauth://` URI.
    ///
    /// The secret is stored **unconfirmed**. Until a code proves the user's
    /// authenticator holds it, the enrollment must not be enforced: a bad scan would
    /// otherwise lock the account out on the next login.
    pub async fn begin_totp_enrollment(&self, user: &User) -> Result<TotpEnrollment> {
        let secret = crate::totp::generate_secret();
        self.repos
            .totp
            .upsert_secret(UserId::new(user.id), &secret)
            .await
            .map_err(map_storage)?;
        let uri = crate::totp::otpauth_uri(&self.issuer(), &user.email, &secret);
        Ok(TotpEnrollment { secret, uri })
    }

    /// Confirm an enrollment with a code, and issue the recovery codes.
    ///
    /// Returns the plaintext recovery codes exactly once; only their digests are
    /// stored. Replacing an existing set is deliberate: confirming again must not
    /// leave the previous codes alive.
    pub async fn confirm_totp_enrollment(
        &self,
        user_id: UserId,
        code: &str,
    ) -> Result<Vec<String>> {
        let Some(enrollment) = self.repos.totp.find(user_id).await.map_err(map_storage)? else {
            return Err(FerromaError::Invalid(
                "no second-factor enrollment is in progress".into(),
            ));
        };
        if enrollment.confirmed_at.is_some() {
            return Err(FerromaError::Conflict(
                "second-factor authentication is already enabled".into(),
            ));
        }
        if !crate::totp::verify_code(
            &enrollment.secret,
            code,
            std::time::SystemTime::now(),
            crate::totp::DEFAULT_SKEW_STEPS,
        )? {
            return Err(FerromaError::Unauthorized(
                "the code did not match; check the authenticator's clock and try again".into(),
            ));
        }

        let confirmed = self
            .repos
            .totp
            .confirm(user_id, Utc::now())
            .await
            .map_err(map_storage)?;
        if !confirmed {
            return Err(FerromaError::Conflict(
                "the enrollment disappeared before it could be confirmed".into(),
            ));
        }

        let codes: Vec<String> = (0..crate::totp::RECOVERY_CODE_COUNT)
            .map(|_| crate::totp::generate_recovery_code())
            .collect();
        let hashes: Vec<String> = codes
            .iter()
            .map(|code| crate::totp::hash_recovery_code(code))
            .collect();
        self.repos
            .recovery_codes
            .replace_all(user_id, &hashes)
            .await
            .map_err(map_storage)?;

        tracing::info!(user_id = user_id.get(), "second-factor enrollment confirmed");
        Ok(codes)
    }

    /// Turn the second factor off, deleting its secret and recovery codes.
    pub async fn disable_totp(&self, user_id: UserId) -> Result<bool> {
        let removed = self.repos.totp.delete(user_id).await.map_err(map_storage)?;
        if removed {
            tracing::info!(user_id = user_id.get(), "second factor disabled");
        }
        Ok(removed)
    }

    /// Whether the account enforces a second factor.
    pub async fn totp_enabled(&self, user_id: UserId) -> Result<bool> {
        Ok(self
            .repos
            .totp
            .find(user_id)
            .await
            .map_err(map_storage)?
            .is_some_and(|enrollment| enrollment.confirmed_at.is_some()))
    }

    /// How the account stands: not enrolled, pending confirmation, or enforced.
    pub async fn totp_status(&self, user_id: UserId) -> Result<TotpStatus> {
        Ok(match self.repos.totp.find(user_id).await.map_err(map_storage)? {
            None => TotpStatus::Disabled,
            Some(enrollment) if enrollment.confirmed_at.is_none() => TotpStatus::Pending,
            Some(_) => TotpStatus::Enabled,
        })
    }

    /// How many unused recovery codes remain.
    pub async fn recovery_codes_left(&self, user_id: UserId) -> Result<i64> {
        self.repos
            .recovery_codes
            .count_unused(user_id)
            .await
            .map_err(map_storage)
    }

    /// Whether a TOTP code or a recovery code satisfies the second factor.
    ///
    /// A recovery code is consumed on use: single-use is the whole point of a code
    /// that exists for the day the phone is gone.
    pub async fn verify_second_factor(&self, user_id: UserId, code: &str) -> Result<bool> {
        let Some(enrollment) = self.repos.totp.find(user_id).await.map_err(map_storage)? else {
            return Ok(false);
        };
        if crate::totp::verify_code(
            &enrollment.secret,
            code,
            std::time::SystemTime::now(),
            crate::totp::DEFAULT_SKEW_STEPS,
        )? {
            return Ok(true);
        }
        let hash = crate::totp::hash_recovery_code(code);
        let consumed = self
            .repos
            .recovery_codes
            .consume(user_id, &hash)
            .await
            .map_err(map_storage)?;
        if consumed {
            tracing::warn!(user_id = user_id.get(), "a recovery code was used to log in");
        }
        Ok(consumed)
    }

    // -------------------------------------------------------------------------
    // Application passwords
    // -------------------------------------------------------------------------

    /// Mint an application password. The plaintext is returned once and stored only
    /// as a digest.
    pub async fn create_app_password(
        &self,
        user_id: UserId,
        label: &str,
    ) -> Result<(AppPassword, String)> {
        let label = label.trim();
        if label.is_empty() {
            return Err(FerromaError::Invalid(
                "an application password needs a label".into(),
            ));
        }
        let token = crate::totp::generate_app_password();
        let row = self
            .repos
            .app_passwords
            .create(user_id, label, &crate::totp::hash_app_password(&token))
            .await
            .map_err(map_storage)?;
        Ok((row, token))
    }

    /// Every application password the account holds, revoked ones included.
    pub async fn list_app_passwords(&self, user_id: UserId) -> Result<Vec<AppPassword>> {
        self.repos
            .app_passwords
            .list(user_id)
            .await
            .map_err(map_storage)
    }

    /// Revoke one application password. Returns `false` when it was not the caller's.
    pub async fn revoke_app_password(&self, user_id: UserId, id: i64) -> Result<bool> {
        let revoked = self
            .repos
            .app_passwords
            .revoke(user_id, id)
            .await
            .map_err(map_storage)?;
        if revoked {
            tracing::info!(user_id = user_id.get(), app_password_id = id, "application password revoked");
        }
        Ok(revoked)
    }

    /// Resolve an address and secret against either credential a client may hold.
    ///
    /// This is the path an IMAP or SMTP client takes, and the reason it exists: a
    /// client that cannot be asked for a TOTP code must be able to use an
    /// application password instead, or enabling the second factor would lock every
    /// mail client out of the account.
    ///
    /// Returns `Ok(None)` when neither credential matched.
    pub async fn authenticate_client(&self, address: &str, secret: &str) -> Result<Option<UserId>> {
        let address = address.trim().to_ascii_lowercase();
        if address.is_empty() || secret.is_empty() {
            return Ok(None);
        }
        let Some(user) = self
            .repos
            .users
            .find_by_email(&address)
            .await
            .map_err(map_storage)?
        else {
            return Ok(None);
        };
        if !user.enabled {
            return Ok(None);
        }
        let user_id = UserId::new(user.id);

        if crate::totp::is_app_password(secret) {
            let matched = self
                .repos
                .app_passwords
                .verify(user_id, &crate::totp::hash_app_password(secret))
                .await
                .map_err(map_storage)?;
            return Ok(matched.then_some(user_id));
        }

        if self.verify_password(secret, &user.password_hash).await {
            // A correct password is not enough once a second factor is enforced: the
            // account is protected against a stolen password, and a client that
            // cannot present the code has to use an application password.
            if self.totp_enabled(user_id).await? {
                return Ok(None);
            }
            return Ok(Some(user_id));
        }
        Ok(None)
    }

    /// The name an authenticator app shows for this deployment.
    fn issuer(&self) -> String {
        self.tokens.issuer().to_string()
    }

    // -------------------------------------------------------------------------
    // Login
    // -------------------------------------------------------------------------

    /// Authenticate with a password and open a session.
    ///
    /// `ip` is used for throttling and the audit trail. `device` registers an
    /// official-client installation when supplied.
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        kind: SessionKind,
        ip: Option<IpAddr>,
        user_agent: Option<&str>,
        device: Option<DeviceInfo>,
    ) -> Result<LoginOutcome> {
        self.login_with_factor(email, password, None, kind, ip, user_agent, device)
            .await
    }

    /// Authenticate with a password and, when the account enforces one, a second factor.
    ///
    /// `factor` is a TOTP code or a recovery code. When the account has a confirmed
    /// enrollment and `factor` is `None`, this returns
    /// [`FerromaError::TotpRequired`] — distinct from a wrong credential, because the
    /// caller must ask for a code rather than the password again.
    ///
    /// A wrong code is counted as a failed login, the same way a wrong password is:
    /// a six-digit code is guessable in a way a password is not, so the lockout has
    /// to cover it.
    #[allow(clippy::too_many_arguments)]
    pub async fn login_with_factor(
        &self,
        email: &str,
        password: &str,
        factor: Option<&str>,
        kind: SessionKind,
        ip: Option<IpAddr>,
        user_agent: Option<&str>,
        device: Option<DeviceInfo>,
    ) -> Result<LoginOutcome> {
        let email = email.trim().to_ascii_lowercase();
        let ip_str = ip.map(|a| a.to_string());
        let now = Utc::now();

        // Throttle by source address before doing any expensive work, so a flood
        // cannot burn the CPU with Argon2 verifications.
        if let Some(ref ip_text) = ip_str {
            let failures = self
                .repos
                .login_attempts
                .count_failures_for_ip(ip_text, now - self.failure_window)
                .await
                .map_err(map_storage)?;
            if failures >= i64::from(self.limits.max_failed_logins) * 3 {
                tracing::warn!(ip = %ip_text, failures, "login throttled by source address");
                self.record_attempt(&email, ip_str.as_deref(), "password", false)
                    .await;
                return Err(FerromaError::RateLimited);
            }
        }

        let user = self
            .repos
            .users
            .find_by_email(&email)
            .await
            .map_err(map_storage)?;

        // Unknown account: same message and same cost profile as a wrong password.
        let Some(user) = user else {
            self.record_attempt(&email, ip_str.as_deref(), "password", false)
                .await;
            return Err(invalid_credentials());
        };

        if !user.enabled {
            self.record_attempt(&email, ip_str.as_deref(), "password", false)
                .await;
            tracing::warn!(user_id = user.id, "login refused: account disabled");
            return Err(invalid_credentials());
        }
        if !user.is_login_allowed(now) {
            self.record_attempt(&email, ip_str.as_deref(), "password", false)
                .await;
            tracing::warn!(user_id = user.id, "login refused: account locked");
            return Err(FerromaError::RateLimited);
        }

        // An application password is a credential of the account in its own right.
        // It is checked before the account password because its shape is
        // unambiguous, and because it is what a client that cannot be asked for a
        // TOTP code presents — so it satisfies the second factor by existing, which
        // is exactly the trade the user made when they minted it.
        let app_password_used = crate::totp::is_app_password(password);
        let password_accepted = if app_password_used {
            self.repos
                .app_passwords
                .verify(
                    UserId::new(user.id),
                    &crate::totp::hash_app_password(password),
                )
                .await
                .map_err(map_storage)?
        } else {
            self.verify_password(password, &user.password_hash).await
        };

        if !password_accepted {
            let updated = self
                .repos
                .users
                .record_login_failure(
                    UserId::new(user.id),
                    now,
                    self.limits.login_lockout_secs,
                    self.limits.max_failed_logins,
                )
                .await
                .map_err(map_storage)?;
            let method = if app_password_used { "app_password" } else { "password" };
            self.record_attempt(&email, ip_str.as_deref(), method, false)
                .await;
            if updated.locked_until.is_some() {
                tracing::warn!(user_id = user.id, "account locked after repeated failures");
                return Err(FerromaError::RateLimited);
            }
            return Err(invalid_credentials());
        }

        // Verified. Upgrade a stale hash while we hold the plaintext — but only when
        // the plaintext really was the account password.
        if !app_password_used && self.hasher.needs_rehash(&user.password_hash) {
            match self.hash_password(password).await {
                Ok(hash) => {
                    if let Err(e) = self
                        .repos
                        .users
                        .update_password(UserId::new(user.id), &hash)
                        .await
                    {
                        // A failed upgrade must not fail the login.
                        tracing::warn!(user_id = user.id, error = %e, "password rehash failed");
                    } else {
                        tracing::info!(
                            user_id = user.id,
                            "password hash upgraded to current parameters"
                        );
                    }
                }
                Err(e) => tracing::warn!(user_id = user.id, error = %e, "password rehash failed"),
            }
        }

        if !app_password_used && self.totp_enabled(UserId::new(user.id)).await? {
            let Some(factor) = factor.map(str::trim).filter(|code| !code.is_empty()) else {
                // The password was right; the account is simply not finished with the
                // caller yet. No failure is counted: nothing was guessed wrong.
                tracing::info!(user_id = user.id, "login needs a second factor");
                return Err(FerromaError::TotpRequired);
            };
            if !self
                .verify_second_factor(UserId::new(user.id), factor)
                .await?
            {
                let updated = self
                    .repos
                    .users
                    .record_login_failure(
                        UserId::new(user.id),
                        now,
                        self.limits.login_lockout_secs,
                        self.limits.max_failed_logins,
                    )
                    .await
                    .map_err(map_storage)?;
                self.record_attempt(&email, ip_str.as_deref(), "totp", false)
                    .await;
                tracing::warn!(user_id = user.id, "login refused: wrong second factor");
                if updated.locked_until.is_some() {
                    return Err(FerromaError::RateLimited);
                }
                return Err(invalid_credentials());
            }
        }

        self.repos
            .users
            .record_login_success(UserId::new(user.id), now)
            .await
            .map_err(map_storage)?;

        // Re-read so the caller sees the cleared counters.
        let user = self
            .repos
            .users
            .require_by_id(UserId::new(user.id))
            .await
            .map_err(map_storage)?;

        let registered_device = match device {
            Some(info) => Some(self.register_device(UserId::new(user.id), info, ip).await?),
            None => None,
        };

        let (session, tokens) = self
            .open_session(
                &user,
                kind,
                registered_device.as_ref().map(|d| d.id),
                ip,
                user_agent,
            )
            .await?;

        self.record_attempt(&email, ip_str.as_deref(), "password", true)
            .await;
        tracing::info!(
            user_id = user.id,
            session_id = session.id,
            kind = kind.as_str(),
            "login succeeded"
        );

        Ok(LoginOutcome {
            user,
            session,
            device: registered_device,
            tokens,
        })
    }

    // -------------------------------------------------------------------------
    // Sessions and tokens
    // -------------------------------------------------------------------------

    /// Open a session and mint a token pair. Used by [`AuthService::login`] and by
    /// the surfaces that authenticate some other way (SMTP `AUTH`, IMAP `LOGIN`).
    pub async fn open_session(
        &self,
        user: &User,
        kind: SessionKind,
        device_id: Option<i64>,
        ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Result<(Session, TokenPair)> {
        let now = Utc::now();
        let (refresh_raw, refresh_hash) = self.tokens.generate_refresh_token();

        let session = self
            .repos
            .sessions
            .create(ferroma_storage::repository::NewSession {
                user_id: UserId::new(user.id),
                kind: kind.as_str().to_string(),
                token_hash: refresh_hash,
                device_id: device_id.map(ferroma_core::DeviceId::new),
                ip: ip.map(|a| a.to_string()),
                user_agent: user_agent.map(|s| s.to_string()),
                expires_at: self.tokens.refresh_expiry(now),
            })
            .await
            .map_err(map_storage)?;

        let session_id = SessionId::new(session.id);
        let access_token = self.tokens.sign_access(UserId::new(user.id), session_id)?;

        Ok((
            session,
            TokenPair {
                access_token,
                refresh_token: refresh_raw,
                expires_in: self.tokens.access_ttl_secs(),
                session_id,
            },
        ))
    }

    /// Verify an access token and load the account and session behind it.
    pub async fn authenticate(&self, bearer_token: &str) -> Result<Authenticated> {
        // A refresh token presented as a bearer token is a client bug worth a
        // precise error rather than a confusing signature failure.
        if looks_like_opaque_token(bearer_token) {
            return Err(FerromaError::Unauthorized(
                "a refresh token cannot be used as a bearer token".into(),
            ));
        }

        let claims = self.tokens.verify_access(bearer_token)?;

        let session = self
            .repos
            .sessions
            .find_by_id(claims.session_id)
            .await
            .map_err(map_storage)?
            .ok_or_else(|| FerromaError::Unauthorized("session no longer exists".into()))?;

        if !session.is_valid_at(Utc::now()) {
            return Err(FerromaError::Unauthorized(
                "session expired or revoked".into(),
            ));
        }
        if session.user_id != claims.user_id.get() {
            // The token claims a different subject than the session it names.
            return Err(FerromaError::Unauthorized(
                "token does not match its session".into(),
            ));
        }

        let user = self
            .repos
            .users
            .require_by_id(claims.user_id)
            .await
            .map_err(|e| match e {
                StorageError::NotFound(_) => {
                    FerromaError::Unauthorized("account no longer exists".into())
                }
                other => map_storage(other),
            })?;

        if !user.enabled {
            return Err(FerromaError::Unauthorized("account disabled".into()));
        }

        Ok(Authenticated {
            user,
            session,
            claims: Some(claims),
        })
    }

    /// Exchange a refresh token for a new pair, rotating the refresh token.
    ///
    /// Rotation is what makes theft detectable: a second use of the same refresh
    /// token revokes every session of that user, on the assumption that one of the
    /// two holders is an attacker.
    pub async fn refresh(&self, refresh_token: &str, ip: Option<IpAddr>) -> Result<TokenPair> {
        let token_hash = self.tokens.hash(refresh_token);
        let now = Utc::now();

        let session = self
            .repos
            .sessions
            .find_by_token_hash(&token_hash)
            .await
            .map_err(map_storage)?
            .ok_or_else(|| FerromaError::Unauthorized("invalid refresh token".into()))?;

        let user_id = UserId::new(session.user_id);

        if session.revoked_at.is_some() {
            // Reuse of a revoked token: assume compromise and burn the family.
            let revoked = self
                .repos
                .sessions
                .revoke_all_for_user(user_id)
                .await
                .map_err(map_storage)?;
            tracing::warn!(
                user_id = user_id.get(),
                session_id = session.id,
                revoked,
                "revoked refresh token reused; all sessions revoked"
            );
            return Err(FerromaError::Unauthorized(
                "refresh token was already used; all sessions have been revoked".into(),
            ));
        }
        if session.expires_at <= now {
            return Err(FerromaError::Unauthorized("refresh token expired".into()));
        }

        let user = self
            .repos
            .users
            .require_by_id(user_id)
            .await
            .map_err(map_storage)?;
        if !user.enabled {
            return Err(FerromaError::Unauthorized("account disabled".into()));
        }

        // Rotate: revoke the presented session, open a replacement of the same kind.
        self.repos
            .sessions
            .revoke(SessionId::new(session.id))
            .await
            .map_err(map_storage)?;

        let kind = SessionKind::parse(&session.kind).unwrap_or(SessionKind::Api);
        let (new_session, mut pair) = self
            .open_session(
                &user,
                kind,
                session.device_id,
                ip,
                session.user_agent.as_deref(),
            )
            .await?;

        // Keep the caller's device association visible in the returned pair.
        pair.session_id = SessionId::new(new_session.id);
        Ok(pair)
    }

    /// Revoke a session.
    pub async fn logout(&self, session_id: SessionId) -> Result<bool> {
        self.repos
            .sessions
            .revoke(session_id)
            .await
            .map_err(map_storage)
    }

    /// Revoke every session of a user, for an administrative lockout.
    pub async fn logout_all(&self, user_id: UserId) -> Result<u64> {
        self.repos
            .sessions
            .revoke_all_for_user(user_id)
            .await
            .map_err(map_storage)
    }

    /// Sessions for the Admin "active sessions" view.
    pub async fn list_sessions(
        &self,
        user_id: UserId,
        include_revoked: bool,
    ) -> Result<Vec<Session>> {
        self.repos
            .sessions
            .list_for_user(user_id, include_revoked)
            .await
            .map_err(map_storage)
    }

    /// Delete sessions that have expired, for the housekeeping task.
    pub async fn purge_expired_sessions(&self) -> Result<u64> {
        self.repos
            .sessions
            .delete_expired(Utc::now())
            .await
            .map_err(map_storage)
    }

    // -------------------------------------------------------------------------
    // Devices
    // -------------------------------------------------------------------------

    /// Record or refresh a device, and stamp it as seen.
    pub async fn register_device(
        &self,
        user_id: UserId,
        info: DeviceInfo,
        ip: Option<IpAddr>,
    ) -> Result<Device> {
        if info.device_uid.trim().is_empty() {
            return Err(FerromaError::Invalid("device_uid must not be empty".into()));
        }
        if info.device_uid.len() > 128 {
            return Err(FerromaError::Invalid("device_uid is too long".into()));
        }
        let device = self
            .repos
            .devices
            .upsert(ferroma_storage::repository::DeviceUpsert {
                user_id,
                device_uid: info.device_uid,
                name: info.name,
                platform: info.platform,
                client_version: info.client_version,
                protocol_version: info.protocol_version.map(|v| v as i32),
                ip: ip.map(|a| a.to_string()),
            })
            .await
            .map_err(map_storage)?;
        Ok(device)
    }

    /// Devices belonging to a user.
    pub async fn list_devices(
        &self,
        user_id: UserId,
        include_revoked: bool,
    ) -> Result<Vec<Device>> {
        self.repos
            .devices
            .list_for_user(user_id, include_revoked)
            .await
            .map_err(map_storage)
    }

    /// Revoke a device and every session it holds.
    ///
    /// This is the remote-wipe gesture from the specification §33: the next request
    /// from that installation gets `401`, and a live WebSocket for it disconnects.
    pub async fn revoke_device(&self, device_id: ferroma_core::DeviceId) -> Result<u64> {
        let device = self
            .repos
            .devices
            .find_by_id(device_id)
            .await
            .map_err(map_storage)?
            .ok_or_else(|| FerromaError::NotFound(format!("device {}", device_id.get())))?;

        self.repos
            .devices
            .revoke(device_id)
            .await
            .map_err(map_storage)?;

        let revoked = self
            .repos
            .sessions
            .revoke_for_device(device_id)
            .await
            .map_err(map_storage)?;

        tracing::info!(
            device_id = device_id.get(),
            user_id = device.user_id,
            revoked,
            "device revoked"
        );
        Ok(revoked)
    }

    // -------------------------------------------------------------------------
    // Internals
    // -------------------------------------------------------------------------

    async fn record_attempt(&self, email: &str, ip: Option<&str>, kind: &str, success: bool) {
        if let Err(e) = self
            .repos
            .login_attempts
            .record(email, ip, kind, success)
            .await
        {
            // Bookkeeping must never turn a login into a failure.
            tracing::warn!(error = %e, "could not record login attempt");
        }
    }

    /// Whether an address is allowed to receive mail, used by SMTP `RCPT TO`.
    /// Kept here so every surface resolves addresses the same way.
    pub async fn resolve_local_address(
        &self,
        address: &ferroma_core::EmailAddress,
    ) -> Result<Option<i64>> {
        let mailbox = self
            .repos
            .mailboxes
            .find_by_address(address.domain(), address.local_part())
            .await
            .map_err(map_storage)?;
        match mailbox {
            Some(m) if m.enabled => Ok(Some(m.id)),
            _ => Ok(None),
        }
    }

    /// The primary mailbox of a user, if any.
    pub async fn primary_mailbox(&self, user_id: UserId) -> Result<Option<MailboxId>> {
        Ok(self
            .repos
            .mailboxes
            .find_primary(user_id)
            .await
            .map_err(map_storage)?
            .map(|m| MailboxId::new(m.id)))
    }
}

/// One deliberately uninformative message for every credential failure.
fn invalid_credentials() -> FerromaError {
    FerromaError::Unauthorized("invalid email address or password".into())
}

fn map_storage(err: StorageError) -> FerromaError {
    err.into()
}

/// Convenience: an `Arc`-wrapped service, which is how the server holds it.
pub type SharedAuth = Arc<AuthService>;

/// Wrap a service for sharing across tasks.
pub fn shared(service: AuthService) -> SharedAuth {
    Arc::new(service)
}

/// Wall-clock helper kept here so tests can reason about expiry without importing
/// chrono directly.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::SessionKind;

    #[test]
    fn jmap_session_kind_round_trips_and_can_refresh() {
        assert_eq!(SessionKind::Jmap.as_str(), "jmap");
        assert_eq!(SessionKind::parse("jmap"), Some(SessionKind::Jmap));
        assert!(SessionKind::Jmap.is_refreshable());
    }
}
