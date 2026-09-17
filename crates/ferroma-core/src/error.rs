//! The single error type shared by every Ferroma crate.

use std::error::Error as StdError;

/// A boxed error, used to keep the original cause of a storage/network failure
/// without making `ferroma-core` depend on `sqlx`, `reqwest`, and friends.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// Convenience alias: `Result<T>` is `Result<T, FerromaError>`.
pub type Result<T, E = FerromaError> = std::result::Result<T, E>;

/// Everything that can go wrong inside Ferroma.
///
/// The variants are intentionally coarse — they describe *what layer* failed and
/// whether retrying could help, which is exactly what SMTP/IMAP status codes and
/// HTTP responses need to know.
#[derive(Debug, thiserror::Error)]
pub enum FerromaError {
    /// The configuration file or environment is unusable (missing hostname, bad port…).
    #[error("configuration error: {0}")]
    Config(String),

    /// Underlying filesystem/socket I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A protocol payload could not be parsed (MIME, SMTP command, IMAP literal…).
    #[error("parse error: {0}")]
    Parse(String),

    /// An entity referenced by id does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A uniqueness or state precondition was violated.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Caller-supplied data failed validation.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// Credentials were missing or wrong.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Authenticated, but not allowed (e.g. relaying to an external domain).
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A configured limit (size, recipients, quota) was exceeded.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    /// The recipient's mailbox is full.
    ///
    /// Deliberately **not** [`FerromaError::LimitExceeded`], because the two need
    /// opposite SMTP classes. A message that is too large is the sender's fault and
    /// will never fit: `552 5.3.4`, permanent. A mailbox that is full is a temporary
    /// condition on the receiving side — the owner may delete something — so RFC 3463
    /// classifies it `4.2.2`, and an MTA that retries for a few days is doing the
    /// right thing. A naive "quota exceeded ⇒ permanent" mapping bounces mail that
    /// would have been delivered tomorrow.
    #[error("mailbox full: {0}")]
    MailboxFull(String),

    /// A rate limiter rejected the request; retrying later is expected to work.
    #[error("rate limited")]
    RateLimited,

    /// The peer violated the protocol, or we could not make sense of it.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// TLS handshake or certificate handling failed.
    #[error("tls error: {0}")]
    Tls(String),

    /// DNS resolution (A/AAAA/MX/TXT/PTR) failed.
    #[error("dns error: {0}")]
    Dns(String),

    /// Database or mail-storage failure. Keeps the original cause in the chain.
    #[error("storage error: {0}")]
    Storage(#[source] BoxError),

    /// Outbound network failure (SMTP delivery, HTTP call to a webhook…).
    #[error("network error: {0}")]
    Network(String),

    /// A feature exists in the spec but is not implemented in this build.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// An operation exceeded its deadline.
    #[error("timed out: {0}")]
    Timeout(String),

    /// A bug. Should be logged with full context and returned as 500.
    #[error("internal error: {0}")]
    Internal(String),
}

impl FerromaError {
    /// Wrap any error as [`FerromaError::Storage`], preserving the source chain.
    pub fn storage<E>(err: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        FerromaError::Storage(Box::new(err))
    }

    /// Shorthand for [`FerromaError::Internal`].
    pub fn internal(msg: impl Into<String>) -> Self {
        FerromaError::Internal(msg.into())
    }

    /// Shorthand for [`FerromaError::Config`].
    pub fn config(msg: impl Into<String>) -> Self {
        FerromaError::Config(msg.into())
    }

    /// Shorthand for [`FerromaError::Invalid`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        FerromaError::Invalid(msg.into())
    }

    /// Shorthand for [`FerromaError::NotFound`].
    pub fn not_found(msg: impl Into<String>) -> Self {
        FerromaError::NotFound(msg.into())
    }

    /// Short machine-readable tag, used in structured logs and API error bodies.
    pub fn code(&self) -> &'static str {
        match self {
            FerromaError::Config(_) => "config_error",
            FerromaError::Io(_) => "io_error",
            FerromaError::Parse(_) => "parse_error",
            FerromaError::NotFound(_) => "not_found",
            FerromaError::Conflict(_) => "conflict",
            FerromaError::Invalid(_) => "invalid_input",
            FerromaError::Unauthorized(_) => "unauthorized",
            FerromaError::Forbidden(_) => "forbidden",
            FerromaError::LimitExceeded(_) => "limit_exceeded",
            FerromaError::MailboxFull(_) => "mailbox_full",
            FerromaError::RateLimited => "rate_limited",
            FerromaError::Protocol(_) => "protocol_error",
            FerromaError::Tls(_) => "tls_error",
            FerromaError::Dns(_) => "dns_error",
            FerromaError::Storage(_) => "storage_error",
            FerromaError::Network(_) => "network_error",
            FerromaError::Unsupported(_) => "unsupported",
            FerromaError::Timeout(_) => "timeout",
            FerromaError::Internal(_) => "internal_error",
        }
    }

    /// Whether retrying the same operation later could plausibly succeed.
    ///
    /// This is the single source of truth behind SMTP `4xx` vs `5xx` replies and
    /// behind the mail queue's "retry" vs "fail" decision.
    ///
    /// Note that [`FerromaError::MailboxFull`] is temporary here: a full mailbox may
    /// not be full tomorrow, so an MTA should retry it rather than bounce. A message
    /// that is simply too large is [`FerromaError::LimitExceeded`] and permanent.
    pub fn is_temporary(&self) -> bool {
        matches!(
            self,
            FerromaError::Io(_)
                | FerromaError::Network(_)
                | FerromaError::Dns(_)
                | FerromaError::RateLimited
                | FerromaError::Timeout(_)
                | FerromaError::Storage(_)
                | FerromaError::Internal(_)
                | FerromaError::MailboxFull(_)
        )
    }

    /// HTTP status code used by the REST/Client API for this error.
    pub fn http_status(&self) -> u16 {
        match self {
            FerromaError::Config(_) | FerromaError::Internal(_) => 500,
            FerromaError::Io(_) | FerromaError::Storage(_) => 500,
            FerromaError::Parse(_) | FerromaError::Invalid(_) => 400,
            FerromaError::Unauthorized(_) => 401,
            FerromaError::Forbidden(_) => 403,
            FerromaError::NotFound(_) => 404,
            FerromaError::Conflict(_) => 409,
            FerromaError::LimitExceeded(_) => 413,
            FerromaError::MailboxFull(_) => 413,
            FerromaError::RateLimited => 429,
            FerromaError::Protocol(_) | FerromaError::Tls(_) => 400,
            FerromaError::Dns(_) | FerromaError::Network(_) | FerromaError::Timeout(_) => 502,
            FerromaError::Unsupported(_) => 501,
        }
    }
}

impl From<serde_json::Error> for FerromaError {
    fn from(err: serde_json::Error) -> Self {
        FerromaError::Parse(format!("json: {err}"))
    }
}

impl From<toml::de::Error> for FerromaError {
    fn from(err: toml::de::Error) -> Self {
        FerromaError::Config(format!("toml: {err}"))
    }
}

impl From<std::net::AddrParseError> for FerromaError {
    fn from(err: std::net::AddrParseError) -> Self {
        FerromaError::Config(format!("invalid socket address: {err}"))
    }
}

impl From<std::num::ParseIntError> for FerromaError {
    fn from(err: std::num::ParseIntError) -> Self {
        FerromaError::Parse(format!("invalid integer: {err}"))
    }
}

impl From<url::ParseError> for FerromaError {
    fn from(err: url::ParseError) -> Self {
        FerromaError::Parse(format!("invalid url: {err}"))
    }
}

/// Shorthand used all over the codebase: `err!("no such mailbox {id}")`.
#[macro_export]
macro_rules! err {
    ($variant:ident, $($arg:tt)*) => {
        $crate::error::FerromaError::$variant(format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_classification_matches_queue_semantics() {
        assert!(FerromaError::Network("reset".into()).is_temporary());
        assert!(FerromaError::RateLimited.is_temporary());
        assert!(!FerromaError::Invalid("bad".into()).is_temporary());
        assert!(!FerromaError::NotFound("x".into()).is_temporary());
        assert!(!FerromaError::Forbidden("relay".into()).is_temporary());
    }

    #[test]
    fn a_full_mailbox_is_temporary_but_an_oversized_message_is_not() {
        // RFC 3463: a full mailbox is `4.2.2` — retryable, because the owner may
        // free space. A message that is too large is `5.3.4` — permanent, because
        // retrying will never shrink it. Getting this backwards bounces mail that
        // would have been delivered the next day.
        let full = FerromaError::MailboxFull("alice@example.com".into());
        assert!(full.is_temporary(), "a full mailbox must be retried, not bounced");
        assert_eq!(full.code(), "mailbox_full");

        let too_big = FerromaError::LimitExceeded("message exceeds 25 MiB".into());
        assert!(!too_big.is_temporary(), "an oversized message will never fit");
        assert_eq!(too_big.code(), "limit_exceeded");

        // Both are a 413 to an HTTP client, which is about storage, not SMTP class.
        assert_eq!(full.http_status(), 413);
        assert_eq!(too_big.http_status(), 413);
    }

    #[test]
    fn http_status_covers_the_common_cases() {
        assert_eq!(FerromaError::Unauthorized("x".into()).http_status(), 401);
        assert_eq!(FerromaError::NotFound("x".into()).http_status(), 404);
        assert_eq!(FerromaError::Conflict("x".into()).http_status(), 409);
        assert_eq!(FerromaError::RateLimited.http_status(), 429);
        assert_eq!(FerromaError::Unsupported("sieve".into()).http_status(), 501);
    }

    #[test]
    fn storage_errors_keep_their_source() {
        let io = std::io::Error::other("disk on fire");
        let err = FerromaError::storage(io);
        assert_eq!(err.code(), "storage_error");
        assert!(StdError::source(&err).is_some());
    }
}
