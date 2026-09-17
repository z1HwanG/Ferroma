//! Tokens: signed access tokens and opaque refresh/session secrets.
//!
//! # Two kinds of credential, two kinds of storage
//!
//! * **Access tokens** are stateless HS256 JWTs. The server verifies the signature
//!   and the expiry without touching the database, which is what keeps the hot path
//!   of every API request cheap. They are short-lived (one hour by default) because
//!   there is no way to revoke one before it expires.
//! * **Refresh tokens and session secrets** are 256 bits of OS randomness rendered
//!   as base64url, and only their SHA-256 is stored. A database leak therefore does
//!   not hand an attacker a usable session, and lookup is a single indexed equality
//!   on the hash.
//!
//! The JWT is hand-rolled on `hmac` + `sha2` + `base64`: HS256 is a
//! `BASE64URL(header).BASE64URL(payload).BASE64URL(HMAC-SHA256(...))` concatenation,
//! and the `jsonwebtoken` crate would add a dependency (and a `ring`/`aws-lc`
//! build) for ninety lines of code we can test directly.
//!
//! # What is deliberately *not* supported
//!
//! Only `HS256` is accepted. `alg: none` and algorithm confusion — the classic JWT
//! vulnerabilities — are impossible here because the header is not trusted at all:
//! the verifier recomputes the HMAC with its own secret and rejects anything whose
//! header does not say exactly `HS256`.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use chrono::{DateTime, TimeZone, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use ferroma_core::{config::ApiConfig, FerromaError, Result, SessionId, UserId};

type HmacSha256 = Hmac<Sha256>;

/// Length of a refresh/session secret, in bytes. 256 bits.
pub const OPAQUE_TOKEN_BYTES: usize = 32;

/// Prefix on refresh tokens, so a leaked string is recognisable in logs and scans.
pub const REFRESH_PREFIX: &str = "rt_";
/// Prefix on opaque session secrets.
pub const SESSION_PREFIX: &str = "st_";

/// The JWT header. `alg` is fixed; there is no negotiation and no `none`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    alg: String,
    typ: String,
}

/// Claims carried by an access token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the user id, as a string (JWT convention).
    pub sub: String,
    /// Session id, so a token can be tied to a revocable session.
    pub sid: i64,
    /// Token kind. Always `"access"` — refresh tokens are not JWTs.
    pub typ: String,
    /// Issuer: the server hostname.
    pub iss: String,
    /// Issued at, Unix seconds.
    pub iat: i64,
    /// Expiry, Unix seconds.
    pub exp: i64,
    /// Unique token id, for log correlation.
    pub jti: String,
}

/// Verified claims, with the ids parsed back into their types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessClaims {
    /// The authenticated user.
    pub user_id: UserId,
    /// The session this token belongs to.
    pub session_id: SessionId,
    /// Expiry.
    pub expires_at: DateTime<Utc>,
    /// Issued at.
    pub issued_at: DateTime<Utc>,
    /// Unique token id.
    pub jti: String,
}

/// Mints and verifies tokens for one server.
#[derive(Clone)]
pub struct TokenService {
    secret: Vec<u8>,
    access_ttl: Duration,
    refresh_ttl: Duration,
    issuer: String,
    /// `true` when the secret was generated at boot rather than configured. Every
    /// restart then invalidates all sessions, which is fine for development and a
    /// mistake in production.
    ephemeral_secret: bool,
}

impl std::fmt::Debug for TokenService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret.
        f.debug_struct("TokenService")
            .field("access_ttl_secs", &self.access_ttl.as_secs())
            .field("refresh_ttl_secs", &self.refresh_ttl.as_secs())
            .field("issuer", &self.issuer)
            .field("ephemeral_secret", &self.ephemeral_secret)
            .finish()
    }
}

impl TokenService {
    /// Build a service with an explicit secret.
    ///
    /// A secret shorter than 32 bytes is refused: HS256 with a weak key is a
    /// brute-forceable signature.
    pub fn new(
        secret: &str,
        access_ttl_secs: u64,
        refresh_ttl_secs: u64,
        issuer: impl Into<String>,
    ) -> Result<Self> {
        if secret.len() < 32 {
            return Err(FerromaError::Config(
                "api.jwt_secret must be at least 32 characters; generate one with `openssl rand -base64 48`".into(),
            ));
        }
        Ok(TokenService {
            secret: secret.as_bytes().to_vec(),
            access_ttl: Duration::from_secs(access_ttl_secs.max(60)),
            refresh_ttl: Duration::from_secs(refresh_ttl_secs.max(60)),
            issuer: issuer.into(),
            ephemeral_secret: false,
        })
    }

    /// Build a service from configuration.
    ///
    /// When `api.jwt_secret` is unset a random secret is generated and a warning is
    /// emitted: convenient for `cargo run`, unacceptable in production, where every
    /// restart would silently log everyone out.
    pub fn from_config(config: &ApiConfig, hostname: &str) -> Result<Self> {
        match config.jwt_secret.as_deref() {
            Some(secret) if !secret.trim().is_empty() => Self::new(
                secret,
                config.access_token_ttl_secs,
                config.refresh_token_ttl_secs,
                hostname,
            ),
            _ => {
                tracing::warn!(
                    "api.jwt_secret is not configured: generating an ephemeral secret. \
                     Every restart will invalidate all sessions. Set FERROMA_JWT_SECRET in production."
                );
                let mut secret_bytes = [0u8; 48];
                use rand::RngCore;
                rand::thread_rng().fill_bytes(&mut secret_bytes);
                let mut service = Self::new(
                    &B64.encode(secret_bytes),
                    config.access_token_ttl_secs,
                    config.refresh_token_ttl_secs,
                    hostname,
                )?;
                service.ephemeral_secret = true;
                Ok(service)
            }
        }
    }

    /// Whether the signing secret was generated at boot instead of configured.
    pub fn has_ephemeral_secret(&self) -> bool {
        self.ephemeral_secret
    }

    /// Access-token lifetime, in seconds.
    pub fn access_ttl_secs(&self) -> u64 {
        self.access_ttl.as_secs()
    }

    /// Refresh-token lifetime, in seconds.
    pub fn refresh_ttl_secs(&self) -> u64 {
        self.refresh_ttl.as_secs()
    }

    /// When a refresh token minted at `now` stops working.
    pub fn refresh_expiry(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + chrono::Duration::from_std(self.refresh_ttl).unwrap_or_else(|_| chrono::Duration::days(30))
    }

    /// Mint an access token for a user and session.
    pub fn sign_access(&self, user_id: UserId, session_id: SessionId) -> Result<String> {
        self.sign_access_at(user_id, session_id, Utc::now())
    }

    /// Mint an access token with an explicit issue time. Used by tests.
    pub fn sign_access_at(
        &self,
        user_id: UserId,
        session_id: SessionId,
        now: DateTime<Utc>,
    ) -> Result<String> {
        let claims = Claims {
            sub: user_id.get().to_string(),
            sid: session_id.get(),
            typ: "access".to_string(),
            iss: self.issuer.clone(),
            iat: now.timestamp(),
            exp: (now + chrono::Duration::from_std(self.access_ttl).unwrap_or_else(|_| chrono::Duration::hours(1)))
                .timestamp(),
            jti: uuid::Uuid::new_v4().simple().to_string(),
        };
        self.encode(&claims)
    }

    fn encode(&self, claims: &Claims) -> Result<String> {
        let header = Header {
            alg: "HS256".to_string(),
            typ: "JWT".to_string(),
        };
        let header_json = serde_json::to_vec(&header)?;
        let payload_json = serde_json::to_vec(claims)?;
        let signing_input = format!(
            "{}.{}",
            B64.encode(&header_json),
            B64.encode(&payload_json)
        );
        let signature = self.hmac(signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", B64.encode(signature)))
    }

    /// Verify an access token and return its claims.
    pub fn verify_access(&self, token: &str) -> Result<AccessClaims> {
        self.verify_access_at(token, Utc::now())
    }

    /// Verify an access token as of `now`. Used by tests to pin the clock.
    pub fn verify_access_at(&self, token: &str, now: DateTime<Utc>) -> Result<AccessClaims> {
        let mut parts = token.split('.');
        let (header_b64, payload_b64, signature_b64) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(unauthorized("malformed token")),
        };

        // The header is checked, never trusted: no "alg" negotiation exists.
        let header_json = B64
            .decode(header_b64)
            .map_err(|_| unauthorized("malformed token header"))?;
        let header: Header = serde_json::from_slice(&header_json)
            .map_err(|_| unauthorized("malformed token header"))?;
        if header.alg != "HS256" {
            return Err(unauthorized("unsupported token algorithm"));
        }

        // Signature first: never parse attacker-controlled claims before proving
        // the token was minted by us.
        let signature = B64
            .decode(signature_b64)
            .map_err(|_| unauthorized("malformed token signature"))?;
        let signing_input = format!("{header_b64}.{payload_b64}");
        let mut mac = self.new_mac()?;
        mac.update(signing_input.as_bytes());
        mac.verify_slice(&signature) // constant time
            .map_err(|_| unauthorized("invalid token signature"))?;

        let payload_json = B64
            .decode(payload_b64)
            .map_err(|_| unauthorized("malformed token payload"))?;
        let claims: Claims = serde_json::from_slice(&payload_json)
            .map_err(|_| unauthorized("malformed token payload"))?;

        if claims.typ != "access" {
            return Err(unauthorized("not an access token"));
        }
        if claims.iss != self.issuer {
            return Err(unauthorized("token issued for a different server"));
        }

        let exp = Utc
            .timestamp_opt(claims.exp, 0)
            .single()
            .ok_or_else(|| unauthorized("invalid token expiry"))?;
        if exp <= now {
            return Err(FerromaError::Unauthorized("token expired".into()));
        }
        // A token from the future means a broken clock or a forged claim; either
        // way, refuse it rather than trusting it indefinitely.
        let iat = Utc
            .timestamp_opt(claims.iat, 0)
            .single()
            .ok_or_else(|| unauthorized("invalid token issue time"))?;
        if iat > now + chrono::Duration::minutes(5) {
            return Err(unauthorized("token issued in the future"));
        }

        let user_id: i64 = claims
            .sub
            .parse()
            .map_err(|_| unauthorized("invalid token subject"))?;

        Ok(AccessClaims {
            user_id: UserId::new(user_id),
            session_id: SessionId::new(claims.sid),
            expires_at: exp,
            issued_at: iat,
            jti: claims.jti,
        })
    }

    /// Generate a refresh token. Returns `(raw, sha256_hex)`; store the hash.
    pub fn generate_refresh_token(&self) -> (String, String) {
        self.generate_opaque(REFRESH_PREFIX)
    }

    /// Generate an opaque session secret. Returns `(raw, sha256_hex)`.
    pub fn generate_session_token(&self) -> (String, String) {
        self.generate_opaque(SESSION_PREFIX)
    }

    fn generate_opaque(&self, prefix: &str) -> (String, String) {
        use rand::RngCore;
        let mut bytes = [0u8; OPAQUE_TOKEN_BYTES];
        rand::thread_rng().fill_bytes(&mut bytes);
        let raw = format!("{prefix}{}", B64.encode(bytes));
        let hash = hash_token(&raw);
        (raw, hash)
    }

    /// The stored form of an opaque token.
    pub fn hash(&self, raw: &str) -> String {
        hash_token(raw)
    }

    fn new_mac(&self) -> Result<HmacSha256> {
        HmacSha256::new_from_slice(&self.secret)
            .map_err(|e| FerromaError::Internal(format!("HMAC key rejected: {e}")))
    }

    fn hmac(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut mac = self.new_mac()?;
        mac.update(data);
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

/// SHA-256 of a raw token, lower-case hex. The only form ever written to the database.
pub fn hash_token(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// Whether a string looks like one of our opaque tokens, for log scrubbing.
pub fn looks_like_opaque_token(value: &str) -> bool {
    value.starts_with(REFRESH_PREFIX) || value.starts_with(SESSION_PREFIX)
}

fn unauthorized(message: &str) -> FerromaError {
    FerromaError::Unauthorized(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "a-very-long-development-secret-that-is-32-plus-bytes";

    fn service() -> TokenService {
        TokenService::new(SECRET, 3600, 2_592_000, "mail.example.com").unwrap()
    }

    #[test]
    fn access_tokens_round_trip() {
        let s = service();
        let token = s.sign_access(UserId::new(7), SessionId::new(12)).unwrap();
        assert_eq!(token.split('.').count(), 3);

        let claims = s.verify_access(&token).unwrap();
        assert_eq!(claims.user_id, UserId::new(7));
        assert_eq!(claims.session_id, SessionId::new(12));
        assert!(!claims.jti.is_empty());
    }

    #[test]
    fn a_short_secret_is_refused() {
        let err = TokenService::new("too-short", 3600, 3600, "x").unwrap_err();
        assert!(matches!(err, FerromaError::Config(_)), "{err:?}");
    }

    #[test]
    fn tampering_with_the_payload_breaks_verification() {
        let s = service();
        let token = s.sign_access(UserId::new(7), SessionId::new(12)).unwrap();
        let parts: Vec<&str> = token.split('.').collect();

        // Re-encode the payload with a different subject, keeping the signature.
        let payload = B64.decode(parts[1]).unwrap();
        let mut claims: Claims = serde_json::from_slice(&payload).unwrap();
        claims.sub = "1".to_string();
        let forged_payload = B64.encode(serde_json::to_vec(&claims).unwrap());
        let forged = format!("{}.{}.{}", parts[0], forged_payload, parts[2]);

        assert!(s.verify_access(&forged).is_err());
    }

    #[test]
    fn a_token_signed_with_another_secret_is_rejected() {
        let a = TokenService::new(SECRET, 3600, 3600, "mail.example.com").unwrap();
        let b = TokenService::new(
            "another-secret-that-is-also-long-enough-ok",
            3600,
            3600,
            "mail.example.com",
        )
        .unwrap();
        let token = a.sign_access(UserId::new(1), SessionId::new(1)).unwrap();
        assert!(b.verify_access(&token).is_err());
    }

    #[test]
    fn the_none_algorithm_is_impossible() {
        let s = service();
        let header = B64.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = B64.encode(
            serde_json::to_vec(&Claims {
                sub: "1".into(),
                sid: 1,
                typ: "access".into(),
                iss: "mail.example.com".into(),
                iat: Utc::now().timestamp(),
                exp: Utc::now().timestamp() + 600,
                jti: "x".into(),
            })
            .unwrap(),
        );
        let forged = format!("{header}.{payload}.");
        let err = s.verify_access(&forged).unwrap_err();
        assert!(matches!(err, FerromaError::Unauthorized(_)), "{err:?}");
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let s = service();
        let issued = Utc::now() - chrono::Duration::hours(3);
        let token = s.sign_access_at(UserId::new(1), SessionId::new(1), issued).unwrap();

        // Valid at issue time…
        assert!(s.verify_access_at(&token, issued + chrono::Duration::minutes(30)).is_ok());
        // …and expired two hours later, since the TTL is one hour.
        let err = s.verify_access_at(&token, issued + chrono::Duration::hours(2)).unwrap_err();
        assert!(matches!(err, FerromaError::Unauthorized(_)));
    }

    #[test]
    fn a_token_issued_in_the_future_is_rejected() {
        let s = service();
        let future = Utc::now() + chrono::Duration::hours(2);
        let token = s.sign_access_at(UserId::new(1), SessionId::new(1), future).unwrap();
        assert!(s.verify_access(&token).is_err());
    }

    #[test]
    fn a_token_from_another_server_is_rejected() {
        let a = TokenService::new(SECRET, 3600, 3600, "mail.example.com").unwrap();
        let b = TokenService::new(SECRET, 3600, 3600, "other.example.com").unwrap();
        let token = a.sign_access(UserId::new(1), SessionId::new(1)).unwrap();
        let err = b.verify_access(&token).unwrap_err();
        assert!(format!("{err}").contains("different server"), "{err}");
    }

    #[test]
    fn malformed_tokens_are_rejected_without_panicking() {
        let s = service();
        for bad in [
            "",
            ".",
            "..",
            "a.b",
            "a.b.c.d",
            "!!!.!!!.!!!",
            "eyJhbGciOiJIUzI1NiJ9.notbase64.sig",
        ] {
            assert!(s.verify_access(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn opaque_tokens_are_unique_have_a_prefix_and_are_only_stored_hashed() {
        let s = service();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let (raw, hash) = s.generate_refresh_token();
            assert!(raw.starts_with(REFRESH_PREFIX), "{raw}");
            assert_eq!(hash.len(), 64, "sha256 hex");
            assert_eq!(hash, hash_token(&raw));
            assert!(!hash.contains(&raw), "the raw token must not appear in the hash");
            assert!(seen.insert(raw), "tokens must not repeat");
        }

        let (raw, hash) = s.generate_session_token();
        assert!(raw.starts_with(SESSION_PREFIX));
        assert_eq!(s.hash(&raw), hash);
    }

    #[test]
    fn refresh_tokens_carry_at_least_256_bits_of_entropy() {
        let s = service();
        let (raw, _) = s.generate_refresh_token();
        let encoded = raw.trim_start_matches(REFRESH_PREFIX);
        let decoded = B64.decode(encoded).unwrap();
        assert_eq!(decoded.len(), OPAQUE_TOKEN_BYTES);
        assert_eq!(OPAQUE_TOKEN_BYTES * 8, 256);
    }

    #[test]
    fn refresh_expiry_uses_the_configured_ttl() {
        let s = service();
        let now = Utc.with_ymd_and_hms(2026, 9, 16, 12, 0, 0).unwrap();
        assert_eq!(
            s.refresh_expiry(now),
            now + chrono::Duration::seconds(2_592_000)
        );
        assert_eq!(s.refresh_ttl_secs(), 2_592_000);
        assert_eq!(s.access_ttl_secs(), 3600);
    }

    #[test]
    fn token_recognition_helper_matches_only_our_prefixes() {
        assert!(looks_like_opaque_token("rt_abc"));
        assert!(looks_like_opaque_token("st_abc"));
        assert!(!looks_like_opaque_token("eyJhbGciOi.abc.def"));
        assert!(!looks_like_opaque_token(""));
    }

    #[test]
    fn an_ephemeral_secret_is_flagged() {
        let unset = ApiConfig {
            jwt_secret: None,
            ..ApiConfig::default()
        };
        assert!(TokenService::from_config(&unset, "localhost")
            .unwrap()
            .has_ephemeral_secret());

        let configured = ApiConfig {
            jwt_secret: Some(SECRET.to_string()),
            ..ApiConfig::default()
        };
        assert!(!TokenService::from_config(&configured, "localhost")
            .unwrap()
            .has_ephemeral_secret());

        // An empty string counts as unset rather than as a one-byte key.
        let blank = ApiConfig {
            jwt_secret: Some("   ".to_string()),
            ..ApiConfig::default()
        };
        assert!(TokenService::from_config(&blank, "localhost")
            .unwrap()
            .has_ephemeral_secret());
    }

    #[test]
    fn debug_output_never_contains_the_secret() {
        let s = service();
        let rendered = format!("{s:?}");
        assert!(!rendered.contains(SECRET), "{rendered}");
    }
}
