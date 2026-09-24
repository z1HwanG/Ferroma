//! Ferroma authentication.
//!
//! One service is the only path from a credential to an identity:
//!
//! * [`password`] — Argon2id hashing, verification, rehash detection and policy.
//! * [`token`] — HS256 access tokens plus opaque, hashed refresh/session secrets.
//! * [`service`] — [`service::AuthService`]: login with throttling, session
//!   lifecycle, refresh-token rotation, device registration and revocation.
//!
//! ```no_run
//! use ferroma_auth::{service::SessionKind, token::TokenService, AuthService};
//! use ferroma_core::Limits;
//! use ferroma_storage::{Database, Repositories};
//!
//! # async fn demo(db: Database) -> Result<(), Box<dyn std::error::Error>> {
//! let repos: Repositories = db.repositories();
//! let tokens = TokenService::new(
//!     "a-very-long-development-secret-that-is-32-plus-bytes",
//!     3600,
//!     2_592_000,
//!     "mail.example.com",
//! )?;
//! let auth = AuthService::with_defaults(repos, tokens, Limits::default());
//!
//! auth.create_user("alice@example.com", "correct horse battery", Some("Alice"), false, true, None)
//!     .await?;
//!
//! let outcome = auth
//!     .login(
//!         "alice@example.com",
//!         "correct horse battery",
//!         SessionKind::Web,
//!         None,
//!         Some("demo"),
//!         None,
//!     )
//!     .await?;
//!
//! let identity = auth.authenticate(&outcome.tokens.access_token).await?;
//! assert_eq!(identity.email(), "alice@example.com");
//! # Ok(())
//! # }
//! ```
//!
//! # What this crate deliberately does not do
//!
//! * It never logs a password, a token or a password hash.
//! * It never reveals whether an address exists: unknown accounts, disabled
//!   accounts and wrong passwords all produce the same `Unauthorized` message.
//! * It does not implement OAuth2/OIDC or 2FA. Those are explicitly later work in
//!   the specification (§15), and the token layer is shaped so they can be added
//!   without changing the session model.

#![warn(missing_docs)]

pub mod password;
pub mod service;
pub mod token;
pub mod totp;

pub use password::{validate_password, Argon2Params, PasswordHasher};
pub use service::{
    shared, AuthService, Authenticated, DeviceInfo, LoginOutcome, SessionKind, SharedAuth,
    TokenPair, TotpEnrollment, TotpStatus,
};
pub use token::{AccessClaims, TokenService};
pub use totp::{
    generate_app_password, generate_secret, hash_app_password, hash_recovery_code, otpauth_uri,
    verify_code, RECOVERY_CODE_COUNT,
};
