//! The client's error type and the boxed-future alias used by its trait seams.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use ferroma_core::FerromaError;

use crate::api::ApiError;

/// A boxed, `Send` future.
///
/// The client crate deliberately avoids pulling in `async-trait` (its manifest is
/// frozen), so object-safe traits that need asynchronous methods spell them out
/// with this alias instead.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The client's result alias.
pub type ClientResult<T> = Result<T, ClientError>;

/// Everything the client core can fail with.
///
/// [`ferroma_core::FerromaError`] stays the platform-wide vocabulary; this type
/// wraps it with the few states that only exist on the client side (a rotated
/// refresh token, a corrupt cache blob, a WebSocket that went away). It converts
/// back into `FerromaError` at the boundary, so a UI shell can treat either one.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server answered with the documented error envelope (`docs/api.md` §1.3).
    #[error("{0}")]
    Api(#[from] ApiError),

    /// A refresh was attempted and the server refused the credentials again
    /// (`docs/fcp.md` §11): the token family is revoked or the device was
    /// revoked. The user must sign in again; the local cache is kept.
    #[error("session expired: the server rejected the refreshed credentials; sign in again")]
    SessionExpired,

    /// An authenticated request was made before any token was available.
    #[error("not signed in")]
    NotAuthenticated,

    /// A local SQLite cache failure.
    #[error("cache database error: {0}")]
    Database(#[from] sqlx::Error),

    /// The local cache is structurally unusable (bad row, missing parent folder…).
    #[error("cache error: {0}")]
    Cache(String),

    /// Filesystem failure while touching the attachment cache.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A transport failure that never produced a structured server error.
    #[error("network error: {0}")]
    Network(String),

    /// A request exceeded its deadline.
    #[error("timeout: {0}")]
    Timeout(String),

    /// The server (or the local cache) sent something we could not parse.
    #[error("parse error: {0}")]
    Parse(String),

    /// A state precondition was violated — `409`, an illegal outbox transition.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The caller supplied invalid input.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// The server does not implement the capability (`501`, a missing feature flag).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The entity does not exist (`404`) or is not cached locally.
    #[error("not found: {0}")]
    NotFound(String),

    /// The realtime socket failed (handshake, framing, transport).
    #[error("realtime socket error: {0}")]
    WebSocket(String),

    /// The server asked us to slow down (`429`). `retry_after` is what its
    /// `Retry-After` header said, when it said anything.
    #[error("rate limited")]
    RateLimited {
        /// The wait the server asked for, in seconds, if it sent `Retry-After`.
        retry_after: Option<Duration>,
    },

    /// A `ferroma-core` failure surfaced unchanged.
    #[error(transparent)]
    Core(#[from] FerromaError),
}

impl ClientError {
    /// Whether retrying the identical request later could plausibly succeed
    /// (`docs/fcp.md` §11): `429`, every `5xx`, and transport failures.
    ///
    /// Every other `4xx` is permanent — retrying it would just repeat the same
    /// rejection — so the caller must surface it instead.
    pub fn is_retryable(&self) -> bool {
        match self {
            ClientError::Api(api) => api.is_retryable(),
            ClientError::Network(_) | ClientError::Timeout(_) | ClientError::RateLimited { .. } => {
                true
            }
            ClientError::SessionExpired => false,
            ClientError::NotAuthenticated => false,
            ClientError::Database(_) | ClientError::Cache(_) => false,
            ClientError::Io(_) => false,
            ClientError::Parse(_) => false,
            ClientError::Conflict(_) => false,
            ClientError::Invalid(_) => false,
            ClientError::Unsupported(_) => false,
            ClientError::NotFound(_) => false,
            ClientError::WebSocket(_) => true,
            ClientError::Core(err) => err.is_temporary(),
        }
    }

    /// The HTTP status behind this error, when it came from the wire.
    pub fn api_status(&self) -> Option<u16> {
        match self {
            ClientError::Api(api) => Some(api.status),
            ClientError::SessionExpired | ClientError::NotAuthenticated => Some(401),
            ClientError::Conflict(_) => Some(409),
            ClientError::Unsupported(_) => Some(501),
            ClientError::NotFound(_) => Some(404),
            ClientError::Invalid(_) | ClientError::Parse(_) => Some(400),
            ClientError::RateLimited { .. } => Some(429),
            _ => None,
        }
    }

    /// The stable machine-readable code (`FerromaError::code()` on the server).
    pub fn code(&self) -> &str {
        match self {
            ClientError::Api(api) => api.code.as_str(),
            ClientError::SessionExpired | ClientError::NotAuthenticated => "unauthorized",
            ClientError::Database(_) | ClientError::Cache(_) => "storage_error",
            ClientError::Io(_) => "io_error",
            ClientError::Network(_) => "network_error",
            ClientError::Timeout(_) => "timeout",
            ClientError::Parse(_) => "parse_error",
            ClientError::Conflict(_) => "conflict",
            ClientError::Invalid(_) => "invalid_input",
            ClientError::Unsupported(_) => "unsupported",
            ClientError::NotFound(_) => "not_found",
            ClientError::WebSocket(_) => "network_error",
            ClientError::RateLimited { .. } => "rate_limited",
            ClientError::Core(err) => err.code(),
        }
    }

    /// A short message safe to show the user: it never contains a token.
    pub fn user_message(&self) -> String {
        match self {
            ClientError::Api(api) => api.message.clone(),
            other => other.to_string(),
        }
    }

    /// Build a [`ClientError::Cache`].
    pub fn cache(msg: impl Into<String>) -> Self {
        ClientError::Cache(msg.into())
    }

    /// Build a [`ClientError::Parse`].
    pub fn parse(msg: impl Into<String>) -> Self {
        ClientError::Parse(msg.into())
    }

    /// Build a [`ClientError::Invalid`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        ClientError::Invalid(msg.into())
    }

    /// Build a [`ClientError::Conflict`].
    pub fn conflict(msg: impl Into<String>) -> Self {
        ClientError::Conflict(msg.into())
    }

    /// Build a [`ClientError::NotFound`].
    pub fn not_found(msg: impl Into<String>) -> Self {
        ClientError::NotFound(msg.into())
    }

    /// Build a [`ClientError::Network`].
    pub fn network(msg: impl Into<String>) -> Self {
        ClientError::Network(msg.into())
    }

    /// The time a `429` asked us to wait, if this error carries one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ClientError::Api(api) => api.retry_after,
            ClientError::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(err: serde_json::Error) -> Self {
        ClientError::Parse(format!("json: {err}"))
    }
}

impl From<ClientError> for FerromaError {
    fn from(err: ClientError) -> Self {
        match err {
            ClientError::Core(core) => core,
            ClientError::Database(e) => FerromaError::storage(e),
            ClientError::Io(e) => FerromaError::Io(e),
            ClientError::Api(api) => {
                FerromaError::Protocol(format!("{} {}: {}", api.status, api.code, api.message))
            }
            ClientError::SessionExpired | ClientError::NotAuthenticated => {
                FerromaError::Unauthorized("session expired".to_string())
            }
            ClientError::Cache(msg) => FerromaError::Storage(Box::new(CacheError(msg))),
            ClientError::Network(msg) => FerromaError::Network(msg),
            ClientError::Timeout(msg) => FerromaError::Timeout(msg),
            ClientError::Parse(msg) => FerromaError::Parse(msg),
            ClientError::Conflict(msg) => FerromaError::Conflict(msg),
            ClientError::Invalid(msg) => FerromaError::Invalid(msg),
            ClientError::Unsupported(msg) => FerromaError::Unsupported(msg),
            ClientError::NotFound(msg) => FerromaError::NotFound(msg),
            ClientError::WebSocket(msg) => FerromaError::Network(msg),
            ClientError::RateLimited { .. } => FerromaError::RateLimited,
        }
    }
}

/// A trivially boxable carrier so [`ClientError::Cache`] can become a
/// [`FerromaError::Storage`] without inventing a database error.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct CacheError(String);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiError;

    #[test]
    fn retryable_classification_follows_fcp_section_11() {
        assert!(ClientError::Api(ApiError::new(429, "rate_limited", "slow down")).is_retryable());
        assert!(ClientError::Api(ApiError::new(503, "internal_error", "boom")).is_retryable());
        assert!(ClientError::Network("reset".into()).is_retryable());
        assert!(ClientError::Timeout("slow".into()).is_retryable());
        assert!(!ClientError::Api(ApiError::new(403, "forbidden", "no")).is_retryable());
        assert!(!ClientError::Api(ApiError::new(400, "invalid_input", "no")).is_retryable());
        assert!(!ClientError::SessionExpired.is_retryable());
        assert!(!ClientError::Conflict("x".into()).is_retryable());
    }

    #[test]
    fn codes_and_statuses_are_stable() {
        assert_eq!(ClientError::SessionExpired.code(), "unauthorized");
        assert_eq!(ClientError::SessionExpired.api_status(), Some(401));
        assert_eq!(ClientError::RateLimited { retry_after: None }.api_status(), Some(429));
        assert_eq!(
            ClientError::Api(ApiError::new(500, "storage_error", "x")).code(),
            "storage_error"
        );
    }

    #[test]
    fn converts_into_core_error_without_losing_the_class() {
        let err: FerromaError = ClientError::SessionExpired.into();
        assert!(matches!(err, FerromaError::Unauthorized(_)));
        let err: FerromaError = ClientError::Network("reset".into()).into();
        assert!(err.is_temporary());
        let err: FerromaError = ClientError::Conflict("cursor".into()).into();
        assert_eq!(err.code(), "conflict");
    }

    #[test]
    fn api_errors_keep_their_envelope_and_retry_after() {
        let mut api = ApiError::new(429, "rate_limited", "slow down");
        api.retry_after = Some(Duration::from_secs(7));
        let err = ClientError::Api(api);
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(err.user_message(), "slow down");
    }

    #[test]
    fn user_messages_never_echo_a_token() {
        let err = ClientError::Network("connection reset".into());
        assert!(!err.user_message().contains("Bearer"));
    }
}
