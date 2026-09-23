//! Ferroma HTTP surface.
//!
//! Three audiences, one router:
//!
//! * **Management/REST API** — `/api/v1/…` (specification §18) for Webmail and Admin.
//! * **Client API / FCP** — `/api/v1/client/…` (specification §19, §20) for the
//!   official desktop clients, including incremental sync and device management.
//! * **Realtime** — `/api/v1/client/events` over WebSocket (specification §23).
//!
//! Plus `.well-known/ferroma` autodiscovery (§32) and the static Webmail/Admin apps.
//!
//! Everything here is subordinate to one document: [`docs/api.md`](../../../docs/api.md)
//! is the frozen contract — every endpoint, every error envelope, every status code —
//! and [`docs/fcp.md`](../../../docs/fcp.md) is the client protocol's semantics. Where
//! this crate has to choose, it chooses what those two documents say.
//!
//! # The three surfaces
//!
//! | Surface | Base path | Auth |
//! |---|---|---|
//! | Management API | `/api/v1` | bearer token **or** `ferroma_session` cookie |
//! | Client API (FCP) | `/api/v1/client` | bearer token only, plus the `X-Ferroma-*` headers |
//! | Realtime | `/api/v1/client/events` | bearer token, then a WebSocket |
//!
//! # Wiring it up
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use ferroma_api::{build, AppState};
//! # let (repos, config, tokens, auth, events, sync, maildir, attachments) = todo!();
//! let state = AppState::new(repos, config, tokens, auth, events, sync)
//!     .with_maildir(maildir)
//!     .with_attachments(attachments);
//! let router = build(state);
//! # Ok(())
//! # }
//! ```
//!
//! # The delivery seam
//!
//! The send path talks to [`state::MailSender`] rather than to a queue worker, so this
//! crate has no compile-time dependency on a *behaviour* of `ferroma-smtp`: the
//! production implementation ([`state::QueueMailSender`]) writes one `mail_queue` row
//! per recipient through `ferroma-storage`, and an alternative can be substituted with
//! [`state::AppState::with_mail_sender`]. See that module's documentation for the full
//! rationale.

#![warn(missing_docs)]

pub mod error;
pub mod extract;
pub mod i18n;
pub mod logbuf;
pub mod router;
pub mod routes;
pub mod service;
pub mod state;
pub mod tests_support;
pub mod ws;

pub use error::{ApiError, ErrorBody, ErrorDetail};
pub use extract::{
    AdminUser, AuthUser, ClientAuth, ClientInfo, JmapAuth, Page, Pagination, DEFAULT_LIMIT,
    MAX_LIMIT, SESSION_COOKIE,
};
pub use i18n::Locale;
pub use logbuf::{floor_for, LogBuffer, LogEntry, LogFilter, LogSink, DEFAULT_CAPACITY};
pub use router::{
    body_limit, build, build_with_root, client_api, cors_layer, frontends_for_bootstrap, jmap_api,
    management_api, route_table, RootApp,
};
pub use routes::admin::system::setup_required;
pub use service::{MessageService, SendRequest, SendResult};
pub use state::{
    AppState, ConnTracker, ConnectionGuard, RestartSignal, UploadRegistry, UploadSession,
};

/// The protocol version this crate's routes negotiate over.
pub const PROTOCOL_VERSION: u32 = ferroma_core::PROTOCOL_VERSION;

#[cfg(test)]
mod crate_tests {
    use super::*;

    #[test]
    fn the_public_surface_is_reexported() {
        // A compile-time check that the items the server wires up are reachable from
        // the crate root, not only from their modules.
        let _: fn(AppState) -> axum::Router = build;
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(DEFAULT_LIMIT, 50);
        assert_eq!(MAX_LIMIT, 500);
        assert_eq!(SESSION_COOKIE, "ferroma_session");
    }

    #[test]
    fn the_route_table_is_not_empty() {
        assert!(route_table().len() > 80, "{} routes", route_table().len());
    }

    #[test]
    fn the_error_helpers_are_reexported() {
        let error = ApiError::new(ferroma_core::FerromaError::NotFound("x".into()));
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
        let body = ErrorBody {
            error: ErrorDetail {
                code: "not_found".into(),
                message: "x".into(),
                details: None,
            },
        };
        assert_eq!(body.error.code, "not_found");
    }
}
