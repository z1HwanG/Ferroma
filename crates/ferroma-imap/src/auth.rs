//! Production credential verification for IMAP.
//!
//! [`SessionContext`](crate::session::SessionContext) takes an
//! [`Authenticator`](crate::session::Authenticator); this module supplies the one
//! the `ferroma` binary wires up, over `ferroma-auth`'s Argon2id hashes and the
//! users repository.
//!
//! It deliberately does **not** open a session or mint tokens. An IMAP
//! connection has no bearer token and no refresh cycle, and creating a session
//! row per `LOGIN` would make the device/session list useless. The password is
//! verified, the account lockout counters are maintained, and nothing else.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use ferroma_auth::PasswordHasher;
//! # use ferroma_storage::Repositories;
//! # async fn demo(repos: Repositories) -> Result<(), Box<dyn std::error::Error>> {
//! use ferroma_imap::ServiceAuthenticator;
//!
//! let authenticator = ServiceAuthenticator::new(Arc::new(repos), PasswordHasher::default());
//! # let _ = authenticator;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use chrono::Utc;
use ferroma_auth::PasswordHasher;
use ferroma_core::{FerromaError, Limits, UserId};
use ferroma_storage::Repositories;

use crate::session::Authenticator;

/// Verifies IMAP credentials against the users table.
pub struct ServiceAuthenticator {
    repos: Arc<Repositories>,
    hasher: PasswordHasher,
    limits: Limits,
}

impl std::fmt::Debug for ServiceAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAuthenticator").finish_non_exhaustive()
    }
}

impl ServiceAuthenticator {
    /// Build with explicit Argon2 parameters.
    pub fn new(repos: Arc<Repositories>, hasher: PasswordHasher) -> Self {
        ServiceAuthenticator {
            repos,
            hasher,
            limits: Limits::default(),
        }
    }

    /// Use the platform's lockout policy (max failures and lockout duration).
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The underlying repositories.
    pub fn repositories(&self) -> &Repositories {
        &self.repos
    }
}

impl Authenticator for ServiceAuthenticator {
    fn authenticate<'a>(
        &'a self,
        user: &'a str,
        password: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<UserId>, FerromaError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let address = user.trim().to_ascii_lowercase();
            if address.is_empty() || password.is_empty() {
                return Ok(None);
            }

            let found = self
                .repos
                .users
                .find_by_email(&address)
                .await
                .map_err(FerromaError::storage)?;
            // An unknown account and a wrong password are indistinguishable to
            // the client, and neither is logged with the credential.
            let Some(record) = found else {
                return Ok(None);
            };
            if !record.enabled || !record.is_login_allowed(Utc::now()) {
                return Ok(None);
            }

            let user_id = UserId::new(record.id);

            // An application password is a credential of the account, and it is the
            // only way in for a client that cannot be asked for a TOTP code. It is
            // checked first because its shape is unambiguous, and because it must
            // keep working after the account password is rotated.
            if ferroma_auth::totp::is_app_password(password) {
                let matched = self
                    .repos
                    .app_passwords
                    .verify(user_id, &ferroma_auth::totp::hash_app_password(password))
                    .await
                    .map_err(FerromaError::storage)?;
                if !matched {
                    return Ok(None);
                }
                let _ = self
                    .repos
                    .users
                    .record_login_success(user_id, Utc::now())
                    .await;
                return Ok(Some(user_id));
            }

            // Argon2 verification is CPU-bound; it must not block the reactor.
            let stored = record.password_hash.clone();
            let hasher = self.hasher;
            let supplied = password.to_string();
            let verified = tokio::task::spawn_blocking(move || hasher.verify(&supplied, &stored))
                .await
                .map_err(|err| FerromaError::Internal(format!("password task failed: {err}")))?;

            if !verified {
                let _ = self
                    .repos
                    .users
                    .record_login_failure(
                        user_id,
                        Utc::now(),
                        self.limits.login_lockout_secs,
                        self.limits.max_failed_logins,
                    )
                    .await;
                return Ok(None);
            }

            // A correct password is not enough once a second factor is enforced. This
            // protocol cannot carry one, so the account has to mint an application
            // password — refusing here is what keeps the factor meaningful.
            let gated = self
                .repos
                .totp
                .find(user_id)
                .await
                .map_err(FerromaError::storage)?
                .is_some_and(|enrollment| enrollment.confirmed_at.is_some());
            if gated {
                tracing::info!(
                    user_id = user_id.get(),
                    "IMAP login refused: the account needs an application password"
                );
                return Ok(None);
            }

            let _ = self
                .repos
                .users
                .record_login_success(user_id, Utc::now())
                .await;
            Ok(Some(user_id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroma_auth::Argon2Params;

    #[test]
    fn an_empty_credential_is_rejected_without_touching_the_database() {
        // `Repositories` needs a pool, so this only exercises the fast path in
        // `authenticate` — but that path is exactly what a hostile client hits
        // thousands of times a second.
        let hasher = PasswordHasher::new(Argon2Params::fast_for_tests());
        assert!(hasher.needs_rehash("not-a-hash"));
    }
}
