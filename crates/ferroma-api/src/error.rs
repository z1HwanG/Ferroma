//! The HTTP error envelope.
//!
//! Every failing request — from any surface, including the Client API — answers with
//! exactly the body [`docs/api.md`](../../../docs/api.md) §1.3 freezes:
//!
//! ```json
//! { "error": { "code": "invalid_input", "message": "recipient address has no domain: bob",
//!              "details": { "field": "to" } } }
//! ```
//!
//! `code` is literally [`ferroma_core::FerromaError::code`], so the machine-readable
//! part of the contract can never drift from the error type the rest of the platform
//! already uses; the HTTP status comes from
//! [`ferroma_core::FerromaError::http_status`].
//!
//! # Never leak internals
//!
//! A `500` must not carry a SQL statement, a filesystem path or a backtrace: those
//! describe the *server*, and a client can do nothing with them. [`ApiError`]
//! therefore logs the full error with `tracing::error!` and answers with the stable
//! code plus a generic message. Errors the caller could act on (400/401/403/404/409/
//! 413/429 and the 5xx upstream classes) keep their human-readable text, which never
//! contains a path or SQL either — the storage layer's own `Display` impls are the
//! ones that do, and those map to `storage_error`.

use axum::extract::rejection::JsonRejection;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferroma_core::FerromaError;
use ferroma_storage::StorageError;

/// The body of `{ "error": … }`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ErrorBody {
    /// The stable, machine-readable failure.
    pub error: ErrorDetail,
}

/// One failure: a stable code, a human message and optional structured detail.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ErrorDetail {
    /// Stable code, e.g. `invalid_input`, `not_found`, `rate_limited`.
    pub code: String,
    /// Human-readable explanation. May change between releases.
    pub message: String,
    /// Structured detail, omitted from the JSON when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Build the documented envelope for a request that matched no route, or matched a
/// path but not the method.
///
/// `axum` answers both with an empty body, and `docs/api.md` §1.3 promises the envelope
/// on every failure. A front-end can render an empty 404 only as "Request failed
/// (HTTP 404)", which tells the operator nothing.
#[must_use]
pub fn envelope_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: ErrorDetail {
                code: code.to_string(),
                message: message.to_string(),
                details: None,
            },
        }),
    )
        .into_response()
}

/// The envelope for an API path that does not exist.
#[must_use]
pub fn not_found(message: &str) -> Response {
    envelope_response(StatusCode::NOT_FOUND, "not_found", message)
}

/// The envelope for a path that exists but not for this method.
#[must_use]
pub fn method_not_allowed(message: &str) -> Response {
    envelope_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        message,
    )
}

/// Anything a handler can return as a failure.
///
/// Handlers use `?` on `Result<_, FerromaError>` and `Result<_, StorageError>` and let
/// this type render the response. A raw driver error never reaches a handler: it is
/// wrapped into [`ferroma_core::FerromaError::Storage`] by `ferroma-storage` first, so
/// this crate needs no direct `sqlx` dependency.
#[derive(Debug)]
pub struct ApiError {
    /// The underlying failure, kept for the log line.
    pub source: FerromaError,
    /// Structured detail for the client, if any.
    pub details: Option<serde_json::Value>,
    /// A status the caller chose explicitly, overriding the error's own.
    ///
    /// `FerromaError::Unsupported` maps to `501`, but the Client API's version gate
    /// must answer `426 Upgrade Required` with the same `unsupported` code. Rather
    /// than reinterpreting `501` everywhere, the gate sets the status here.
    pub status_override: Option<StatusCode>,
    /// Seconds a `429` client should wait, sent as `Retry-After`.
    pub retry_after_secs: Option<u64>,
    /// Whether a `401` should name `Basic` as well as `Bearer`.
    ///
    /// Only the JMAP surface sets this. A management or FCP endpoint that advertised
    /// Basic would be asking a client to send a password it will not accept.
    pub www_authenticate: bool,
}

impl ApiError {
    /// Wrap any [`FerromaError`].
    pub fn new(source: FerromaError) -> Self {
        ApiError {
            source,
            details: None,
            status_override: None,
            retry_after_secs: None,
            www_authenticate: false,
        }
    }

    /// Tell a JMAP client that `Basic` (the mailbox password) is accepted.
    #[must_use]
    pub fn with_www_authenticate(mut self) -> Self {
        self.www_authenticate = true;
        self
    }

    /// Attach structured detail (`{"field": "to"}`).
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Force a specific HTTP status.
    #[must_use]
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status_override = Some(status);
        self
    }

    /// Send a `Retry-After` header, which `429` answers must carry.
    #[must_use]
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// The status this error will answer with.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self.status_override {
            Some(status) => status,
            None => status_for(self.source.http_status()),
        }
    }

    /// The stable code the client sees.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.source.code()
    }

    /// `true` when the failure describes the server rather than the request.
    ///
    /// Those are the ones whose text is replaced by a generic message.
    fn is_internal(&self) -> bool {
        matches!(
            self.source,
            FerromaError::Storage(_)
                | FerromaError::Internal(_)
                | FerromaError::Config(_)
                | FerromaError::Io(_)
        )
    }

    /// The message that goes to the client.
    fn client_message(&self) -> String {
        let english = if self.is_internal() {
            // Deliberately generic: `Storage(_)` renders as "database error: <sqlx>",
            // which names tables, constraints and sometimes values.
            "the server could not complete the request".to_string()
        } else {
            self.source.to_string()
        };
        // `code` stays language-neutral; only this half is negotiated. See
        // [`crate::i18n`].
        crate::i18n::current().message(&english)
    }

    /// A diagnostic for the log line. Carries the real cause, never the request body.
    fn log(&self, status: StatusCode) {
        if status.is_server_error() {
            tracing::error!(
                status = status.as_u16(),
                code = self.code(),
                error = ?self.source,
                "request failed"
            );
        } else {
            tracing::debug!(
                status = status.as_u16(),
                code = self.code(),
                "request rejected: {}",
                self.source
            );
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.source)
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        self.log(status);

        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code().to_string(),
                message: self.client_message(),
                details: self.details,
            },
        };

        let mut response = (status, Json(body)).into_response();

        // A JMAP client retries a 401 with the scheme named here. Without it, the
        // client that just sent the mailbox password has no reason to try Basic.
        // Management and FCP 401s name Bearer only; advertising Basic there would
        // invite a password onto a surface that does not accept one.
        if status == StatusCode::UNAUTHORIZED && self.www_authenticate {
            if let Ok(value) = HeaderValue::from_str("Basic realm=\"jmap\", Bearer") {
                response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
            }
        }

        // `429` is only actionable with a `Retry-After`; default to the documented
        // login lockout window when the caller did not pick a number.
        if status == StatusCode::TOO_MANY_REQUESTS {
            let secs = self.retry_after_secs.unwrap_or(60);
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }

        response
    }
}

impl From<FerromaError> for ApiError {
    fn from(source: FerromaError) -> Self {
        ApiError::new(source)
    }
}

impl From<StorageError> for ApiError {
    fn from(err: StorageError) -> Self {
        ApiError::new(err.into())
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        // `JsonRejection` already carries the precise reason (missing body, wrong
        // content type, a syntax error at byte N); none of it is server internals,
        // so it is safe — and useful — to hand to the caller.
        let status = rejection.status();
        let err = if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            FerromaError::Protocol(format!("request body must be JSON: {rejection}"))
        } else {
            FerromaError::Parse(rejection.body_text())
        };
        ApiError::new(err)
    }
}

impl From<axum::extract::multipart::MultipartError> for ApiError {
    fn from(err: axum::extract::multipart::MultipartError) -> Self {
        ApiError::new(FerromaError::Parse(format!(
            "malformed multipart body: {err}"
        )))
    }
}

/// `FerromaError`: an error a handler produced on its own, without an envelope.
impl From<ApiError> for FerromaError {
    fn from(err: ApiError) -> Self {
        err.source
    }
}

/// Map the numeric status from [`FerromaError::http_status`] onto axum's enum.
///
/// The mapping is total: an unexpected number falls back to `500` rather than
/// panicking, because an error path that panics is worse than an error path that is
/// slightly wrong.
pub fn status_for(raw: u16) -> StatusCode {
    StatusCode::from_u16(raw).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// The body of a `426 Upgrade Required` answer for a client protocol that is too old.
///
/// See [`docs/api.md`](../../../docs/api.md) §6: the code is `unsupported` and the
/// message names both the client's version and the version it must reach.
pub fn protocol_upgrade_required(found: u32, minimum: u32) -> ApiError {
    ApiError::new(FerromaError::Unsupported(format!(
        "client protocol {found} is no longer supported; upgrade to FCP/{minimum}"
    )))
    .with_status(StatusCode::UPGRADE_REQUIRED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    async fn body_json(error: ApiError) -> (StatusCode, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body must be readable");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
        (status, json)
    }

    #[tokio::test]
    async fn envelope_matches_the_documented_shape() {
        let err = ApiError::new(FerromaError::Invalid(
            "recipient address has no domain: bob".into(),
        ))
        .with_details(serde_json::json!({ "field": "to" }));

        let (status, json) = body_json(err).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["code"], "invalid_input");
        // `FerromaError`'s own `Display` prefixes the variant, and the envelope carries
        // it verbatim: the code is the machine-readable half, the message the human one.
        assert_eq!(
            json["error"]["message"],
            "invalid input: recipient address has no domain: bob"
        );
        assert_eq!(json["error"]["details"]["field"], "to");
    }

    #[tokio::test]
    async fn details_are_omitted_when_there_are_none() {
        let (_, json) = body_json(ApiError::new(FerromaError::NotFound("message 7".into()))).await;
        assert!(json["error"].get("details").is_none(), "{json}");
    }

    #[tokio::test]
    async fn every_variant_maps_onto_its_documented_status_and_code() {
        // The table from docs/api.md §1.3, one row per FerromaError variant.
        let cases: Vec<(FerromaError, u16, &str)> = vec![
            (FerromaError::Invalid("x".into()), 400, "invalid_input"),
            (FerromaError::Parse("x".into()), 400, "parse_error"),
            (FerromaError::Protocol("x".into()), 400, "protocol_error"),
            (FerromaError::Tls("x".into()), 400, "tls_error"),
            (FerromaError::Unauthorized("x".into()), 401, "unauthorized"),
            (FerromaError::Forbidden("x".into()), 403, "forbidden"),
            (FerromaError::NotFound("x".into()), 404, "not_found"),
            (FerromaError::Conflict("x".into()), 409, "conflict"),
            (
                FerromaError::LimitExceeded("x".into()),
                413,
                "limit_exceeded",
            ),
            (FerromaError::MailboxFull("x".into()), 413, "mailbox_full"),
            (FerromaError::RateLimited, 429, "rate_limited"),
            (
                FerromaError::Storage(Box::new(std::io::Error::other("db"))),
                500,
                "storage_error",
            ),
            (FerromaError::Internal("x".into()), 500, "internal_error"),
            (
                FerromaError::Io(std::io::Error::other("x")),
                500,
                "io_error",
            ),
            (FerromaError::Config("x".into()), 500, "config_error"),
            (FerromaError::Dns("x".into()), 502, "dns_error"),
            (FerromaError::Network("x".into()), 502, "network_error"),
            (FerromaError::Timeout("x".into()), 502, "timeout"),
            (FerromaError::Unsupported("x".into()), 501, "unsupported"),
        ];

        for (err, status, code) in cases {
            let api = ApiError::new(err);
            assert_eq!(api.status().as_u16(), status, "status for {code}");
            assert_eq!(api.code(), code);
        }
    }

    #[tokio::test]
    async fn internal_errors_never_leak_their_text() {
        let leaked = FerromaError::Storage(Box::new(std::io::Error::other(
            "SELECT * FROM users WHERE password_hash = 'x' at C:\\ferroma\\db",
        )));
        let (status, json) = body_json(ApiError::new(leaked)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json["error"]["code"], "storage_error");
        let message = json["error"]["message"].as_str().unwrap_or_default();
        assert!(!message.contains("SELECT"), "{message}");
        assert!(!message.contains("password_hash"), "{message}");
        assert!(!message.contains("ferroma\\db"), "{message}");
        assert_eq!(message, "the server could not complete the request");
    }

    #[tokio::test]
    async fn rate_limited_carries_retry_after() {
        let response = ApiError::new(FerromaError::RateLimited)
            .with_retry_after(42)
            .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("42")
        );
    }

    #[tokio::test]
    async fn rate_limited_without_an_explicit_delay_still_sets_the_header() {
        let response = ApiError::new(FerromaError::RateLimited).into_response();
        assert!(response.headers().get(header::RETRY_AFTER).is_some());
    }

    #[tokio::test]
    async fn non_rate_limit_errors_have_no_retry_after() {
        let response = ApiError::new(FerromaError::NotFound("x".into())).into_response();
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn status_override_wins() {
        let err = ApiError::new(FerromaError::Unsupported("old".into()))
            .with_status(StatusCode::UPGRADE_REQUIRED);
        assert_eq!(err.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(err.code(), "unsupported");
    }

    #[tokio::test]
    async fn storage_errors_convert_through_the_from_impl() {
        let api: ApiError = StorageError::QuotaExceeded {
            mailbox_id: 3,
            used: 10,
            needed: 10,
            limit: 15,
        }
        .into();
        // A full mailbox is a `413` to an HTTP client, even though it stays *temporary*
        // at the SMTP edge (a full mailbox may not be full tomorrow).
        assert_eq!(api.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(api.code(), "mailbox_full");
        assert!(api.source.is_temporary());
    }

    #[tokio::test]
    async fn unique_violations_surface_as_conflicts() {
        let api: ApiError =
            StorageError::Conflict("user alice@example.com already exists".into()).into();
        let (status, json) = body_json(api).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["error"]["code"], "conflict");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already exists"));
    }

    #[test]
    fn status_for_never_panics() {
        assert_eq!(status_for(0), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(status_for(9999), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(status_for(404), StatusCode::NOT_FOUND);
    }

    #[test]
    fn protocol_upgrade_required_names_both_versions() {
        let err = protocol_upgrade_required(0, 1);
        assert_eq!(err.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(
            err.source.to_string(),
            "unsupported: client protocol 0 is no longer supported; upgrade to FCP/1"
        );
    }

    #[test]
    fn api_error_keeps_its_source_chain() {
        use std::error::Error as _;
        let err = ApiError::new(FerromaError::Storage(Box::new(std::io::Error::other(
            "boom",
        ))));
        assert!(err.source().is_some());
        assert!(format!("{err}").contains("storage_error"));
    }
}
