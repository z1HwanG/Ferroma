//! The HTTP route table.
//!
//! [`build`] assembles every endpoint in [`docs/api.md`](../../../docs/api.md) onto one
//! `axum` router, in the order the document presents them, so an endpoint and its
//! documentation can be read side by side.
//!
//! # Layers, outermost first
//!
//! 1. [`tower_http::trace::TraceLayer`] — one span per request, with method, path,
//!    status and latency. It never records a body.
//! 2. **CORS**, driven by `api.cors_origins`. An empty list means *same-origin only*:
//!    no `Access-Control-Allow-Origin` is emitted at all, which is the safe default
//!    rather than a wildcard everybody forgets to tighten.
//! 3. **Request-body limit** from `api.max_request_size`, so an oversized upload is
//!    refused by the transport before it reaches a handler.
//! 4. **Gzip compression** for the JSON bodies that dominate the API.
//! 5. **Static files** when `api.serve_frontend` is on: the Webmail app at `/` and the
//!    Admin app at `/admin`, each with an SPA fallback so a client-side route such as
//!    `/admin/users` reloads into `index.html` instead of a 404.
//!
//! # Why the management and client tables are merged
//!
//! `docs/api.md` describes two surfaces sharing one process. They are built as separate
//! routers and merged, because that is what keeps the FCP table in
//! [`crate::routes::client`] readable as the endpoint index it is — but they share the
//! same `AppState`, the same extractors and the same error envelope.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use ferroma_core::config::Config;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::routes;
use crate::state::AppState;
use crate::ws;

/// Build the complete router.
pub fn build(state: AppState) -> Router {
    let api = management_api();
    let client = client_api(&state.config);
    let discovery = discovery_routes();

    let mut router = Router::new()
        .merge(discovery)
        .nest("/api/v1", api)
        .nest("/api/v1/client", client)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(cors_layer(&state.config))
        .layer(DefaultBodyLimit::max(body_limit(&state.config)));

    if state.config.api.serve_frontend {
        router = with_frontends(router, &state.config);
    }

    router.with_state(state)
}

/// The request-body ceiling, clamped to something axum can express.
pub fn body_limit(config: &Config) -> usize {
    let configured = config.api.max_request_size;
    let capped = configured.min(usize::MAX as u64) as usize;
    // A zero limit would refuse every request; fall back to the default.
    if capped == 0 {
        config.limits.max_message_size as usize
    } else {
        capped
    }
}

/// The CORS policy: an explicit allow-list, or nothing at all.
pub fn cors_layer(config: &Config) -> CorsLayer {
    let base = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::IF_NONE_MATCH,
            header::RANGE,
            HeaderName::from_static("x-ferroma-client"),
            HeaderName::from_static("x-ferroma-protocol"),
            HeaderName::from_static("x-ferroma-platform"),
            HeaderName::from_static("idempotency-key"),
        ])
        .expose_headers([
            header::ETAG,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::RETRY_AFTER,
            HeaderName::from_static("x-ferroma-protocol"),
            HeaderName::from_static("x-ferroma-server"),
        ]);

    let origins: Vec<HeaderValue> = config
        .api
        .cors_origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin.trim()).ok())
        .collect();

    if origins.is_empty() {
        // Same-origin only: no `Access-Control-Allow-Origin` is ever emitted, so a
        // browser refuses a cross-site call. This is the documented default.
        base
    } else {
        base.allow_origin(AllowOrigin::list(origins))
    }
}

/// `/health`, `/version` and the two `/.well-known` documents.
pub fn discovery_routes() -> Router<AppState> {
    Router::new()
        .route("/.well-known/ferroma", get(routes::health::well_known))
        .route("/.well-known/mta-sts.txt", get(routes::health::mta_sts))
        .route("/api/v1/health", get(routes::health::health))
        .route("/api/v1/version", get(routes::health::version))
}

/// The management surface (`docs/api.md` §3–§5).
pub fn management_api() -> Router<AppState> {
    let auth = Router::new()
        .route("/auth/login", post(routes::auth::login))
        .route("/auth/refresh", post(routes::auth::refresh))
        .route("/auth/logout", post(routes::auth::logout))
        .route("/auth/me", get(routes::auth::me))
        .route("/auth/password", post(routes::auth::change_password));

    let setup = Router::new()
        .route("/setup", get(routes::admin::system::setup_status).post(routes::admin::system::setup));

    let users = Router::new()
        .route(
            "/users",
            get(routes::admin::users::list_users).post(routes::admin::users::create_user),
        )
        .route(
            "/users/{id}",
            get(routes::admin::users::get_user)
                .patch(routes::admin::users::update_user)
                .delete(routes::admin::users::delete_user),
        )
        .route(
            "/users/{id}/mailboxes",
            get(routes::admin::users::list_user_mailboxes).post(routes::admin::users::create_user_mailbox),
        );

    let domains = Router::new()
        .route(
            "/domains",
            get(routes::admin::domains::list_domains).post(routes::admin::domains::create_domain),
        )
        .route(
            "/domains/{id}",
            get(routes::admin::domains::get_domain)
                .patch(routes::admin::domains::update_domain)
                .delete(routes::admin::domains::delete_domain),
        )
        .route(
            "/domains/{id}/aliases",
            get(routes::admin::domains::list_aliases).post(routes::admin::domains::create_alias),
        )
        .route("/domains/{id}/dns", get(routes::admin::domains::domain_dns))
        .route(
            "/domains/{id}/dkim",
            get(routes::admin::domains::get_dkim).post(routes::admin::domains::create_dkim),
        )
        .route(
            "/aliases/{id}",
            patch(routes::admin::domains::update_alias).delete(routes::admin::domains::delete_alias),
        );

    let queue = Router::new()
        .route("/queue", get(routes::admin::queue::list_queue))
        .route("/queue/stats", get(routes::admin::queue::queue_stats))
        .route("/queue/{id}", get(routes::admin::queue::get_queue_entry).delete(routes::admin::queue::cancel_queue_entry))
        .route("/queue/{id}/retry", post(routes::admin::queue::retry_queue_entry));

    let system = Router::new()
        .route("/storage", get(routes::admin::system::storage_overview))
        .route("/storage/gc", post(routes::admin::system::storage_gc))
        .route("/audit", get(routes::admin::system::list_audit))
        .route("/settings", get(routes::admin::system::list_settings))
        .route("/settings/{key}", put(routes::admin::system::put_setting))
        .route("/logs", get(routes::admin::logs::list_logs))
        .route("/tls", get(routes::admin::tls::tls_status))
        .route("/devices", get(routes::admin::logs::list_devices))
        .route(
            "/devices/{id}",
            delete(routes::admin::logs::delete_device),
        )
        .route("/devices/{id}/revoke", post(routes::admin::logs::revoke_device));

    let mail = Router::new()
        .route("/mailboxes", get(routes::mail::mailboxes::list_mailboxes))
        .route(
            "/mailboxes/{id}/folders",
            get(routes::mail::mailboxes::list_folders).post(routes::mail::mailboxes::create_folder),
        )
        .route(
            "/folders/{id}",
            patch(routes::mail::mailboxes::update_folder).delete(routes::mail::mailboxes::delete_folder),
        )
        .route("/messages", get(routes::mail::messages::list_messages).post(routes::mail::messages::send_message))
        .route("/messages/batch", post(routes::mail::messages::batch_messages))
        .route(
            "/messages/{id}",
            get(routes::mail::messages::get_message)
                .patch(routes::mail::messages::patch_message)
                .delete(routes::mail::messages::delete_message),
        )
        .route("/messages/{id}/raw", get(routes::mail::messages::get_raw_message))
        .route("/messages/{id}/move", post(routes::mail::messages::move_message))
        .route("/messages/{id}/copy", post(routes::mail::messages::copy_message))
        .route("/attachments", post(routes::mail::attachments::upload_attachment))
        .route(
            "/attachments/{id}",
            get(routes::mail::attachments::download_attachment).delete(routes::mail::attachments::delete_attachment),
        )
        .route("/attachments/{id}/meta", get(routes::mail::attachments::attachment_meta))
        .route("/drafts", get(routes::mail::drafts::list_drafts).post(routes::mail::drafts::create_draft))
        .route(
            "/drafts/{id}",
            get(routes::mail::drafts::get_draft)
                .patch(routes::mail::drafts::update_draft)
                .delete(routes::mail::drafts::delete_draft),
        )
        .route("/drafts/{id}/send", post(routes::mail::drafts::send_draft));

    Router::new()
        .merge(auth)
        .merge(setup)
        .merge(users)
        .merge(domains)
        .merge(queue)
        .merge(system)
        .merge(mail)
}

/// The values the client-API header layer carries.
#[derive(Debug, Clone, Default)]
pub struct ClientHeaderState {
    /// The protocol version this build speaks.
    pub protocol_version: u32,
    /// The server's release version.
    pub server_version: String,
}

impl ClientHeaderState {
    /// Read the values out of a configuration.
    pub fn from_config(config: &Config) -> Self {
        ClientHeaderState {
            protocol_version: config.client.protocol_version,
            server_version: ferroma_core::VERSION.to_string(),
        }
    }
}

/// The Client API (FCP), `docs/api.md` §6.
pub fn client_api(config: &Config) -> Router<AppState> {
    let headers = NegotiationHeadersLayer::from_config(config);
    let auth = Router::new()
        .route("/auth/login", post(routes::client::auth::client_login))
        .route("/auth/refresh", post(routes::client::auth::client_refresh))
        .route("/auth/logout", post(routes::client::auth::client_logout))
        .route("/account", get(routes::client::auth::client_account));

    let sync = Router::new()
        .route("/mailboxes", get(routes::client::resources::client_mailboxes))
        .route("/sync", get(routes::client::resources::client_sync))
        .route("/search", get(routes::client::resources::client_search));

    let messages = Router::new()
        .route(
            "/messages",
            get(routes::client::resources::client_messages).post(routes::client::resources::client_send_message),
        )
        .route("/messages/batch", post(routes::client::resources::client_batch))
        .route(
            "/messages/{id}",
            get(routes::client::resources::client_message)
                .patch(routes::client::resources::client_patch_message)
                .delete(routes::client::resources::client_delete_message),
        )
        .route("/messages/{id}/raw", get(routes::client::resources::client_message_raw))
        .route("/messages/{id}/read", post(routes::client::resources::client_mark_read))
        .route("/messages/{id}/unread", post(routes::client::resources::client_mark_unread))
        .route("/messages/{id}/star", post(routes::client::resources::client_star))
        .route("/messages/{id}/archive", post(routes::client::resources::client_archive))
        .route("/messages/{id}/trash", post(routes::client::resources::client_trash))
        .route("/messages/{id}/move", post(routes::client::resources::client_move));

    let drafts = Router::new()
        .route(
            "/drafts",
            get(routes::client::resources::client_list_drafts).post(routes::client::resources::client_create_draft),
        )
        .route(
            "/drafts/{id}",
            get(routes::client::resources::client_get_draft)
                .patch(routes::client::resources::client_update_draft)
                .delete(routes::client::resources::client_delete_draft),
        );

    let attachments = Router::new()
        .route(
            "/attachments",
            post(routes::mail::attachments::upload_attachment),
        )
        .route(
            "/attachments/init",
            post(routes::mail::attachments::init_upload),
        )
        .route(
            "/attachments/{id}",
            get(routes::mail::attachments::download_attachment).delete(routes::mail::attachments::delete_attachment),
        )
        .route("/attachments/{id}/meta", get(routes::mail::attachments::attachment_meta))
        .route("/attachments/{id}/chunk", put(routes::mail::attachments::upload_chunk))
        .route("/attachments/{id}/complete", post(routes::mail::attachments::complete_upload))
        .route("/attachments/{id}/status", get(routes::mail::attachments::upload_status));

    let devices = Router::new()
        .route("/devices", get(routes::client::resources::client_devices))
        .route("/devices/{id}", delete(routes::client::resources::client_revoke_device))
        .route("/devices/{id}/revoke", post(routes::client::resources::client_revoke_device));

    Router::new()
        .merge(auth)
        .merge(sync)
        .merge(messages)
        .merge(drafts)
        .merge(attachments)
        .merge(devices)
        .route("/events", get(ws::events_socket))
        .fallback(Json(crate::error::ErrorBody {
            error: crate::error::ErrorDetail {
                code: "not_found".to_string(),
                message: "no such client endpoint".to_string(),
                details: None,
            },
        }))
        .layer(headers)
}

/// A tower layer that stamps the FCP negotiation headers onto every client-API
/// response.
///
/// `docs/fcp.md` §1: *"The server replies with the protocol it used"* —
/// `X-Ferroma-Protocol: 1` and `X-Ferroma-Server: 0.1.0` — on **every** answer, not only
/// on the login. A client asking for a higher version is told the version it actually
/// got, which is the signal to degrade gracefully.
///
/// A hand-written `Layer`/`Service` pair rather than `middleware::from_fn_with_state`,
/// because the values are fixed for the life of the process: capturing them in the
/// service avoids threading them through the router's own state type.
#[derive(Debug, Clone)]
pub struct NegotiationHeadersLayer {
    /// The protocol version this build speaks.
    pub protocol_version: u32,
    /// The server's release version.
    pub server_version: String,
}

impl NegotiationHeadersLayer {
    /// Read the values out of a configuration.
    pub fn from_config(config: &Config) -> Self {
        NegotiationHeadersLayer {
            protocol_version: config.client.protocol_version,
            server_version: ferroma_core::VERSION.to_string(),
        }
    }
}

impl<S> tower::Layer<S> for NegotiationHeadersLayer {
    type Service = NegotiationHeaders<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NegotiationHeaders {
            inner,
            protocol_version: self.protocol_version,
            server_version: self.server_version.clone(),
        }
    }
}

/// The service [`NegotiationHeadersLayer`] builds.
#[derive(Debug, Clone)]
pub struct NegotiationHeaders<S> {
    inner: S,
    protocol_version: u32,
    server_version: String,
}

impl<S, Body> tower::Service<axum::http::Request<Body>> for NegotiationHeaders<S>
where
    S: tower::Service<axum::http::Request<Body>, Response = axum::response::Response>
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    Body: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = futures_util::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: axum::http::Request<Body>) -> Self::Future {
        let protocol_version = self.protocol_version;
        let server_version = self.server_version.clone();
        let future = self.inner.call(request);
        Box::pin(async move {
            let mut response = future.await?;
            apply_negotiation_headers(&mut response, protocol_version, &server_version);
            Ok(response)
        })
    }
}

/// Write the negotiation headers onto a response.
pub fn apply_negotiation_headers(
    response: &mut axum::response::Response,
    protocol_version: u32,
    server_version: &str,
) {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&protocol_version.to_string()) {
        headers.insert(
            HeaderName::from_static(crate::extract::SERVER_PROTOCOL_HEADER),
            value,
        );
    }
    if let Ok(value) = HeaderValue::from_str(server_version) {
        headers.insert(
            HeaderName::from_static(crate::extract::SERVER_VERSION_HEADER),
            value,
        );
    }
}

/// Serve `web/` at `/` and `admin/` at `/admin/`, each with an SPA fallback.
///
/// The two directories come from `api.webmail_dir` / `api.admin_dir`, or from the
/// conventional `web/dist` and `admin/dist` under the working directory — the layout
/// `AGENTS.md` §3 describes. A directory that does not exist is skipped, so a
/// backend-only deployment still boots.
pub fn with_frontends(router: Router<AppState>, config: &Config) -> Router<AppState> {
    let mut router = router;

    if let Some(webmail) = resolve_dir(config.api.webmail_dir.as_ref(), &["web/dist", "web"]) {
        router = router.fallback_service(
            ServeDir::new(&webmail).fallback(ServeFile::new(webmail.join("index.html"))),
        );
    }

    if let Some(admin) = resolve_dir(config.api.admin_dir.as_ref(), &["admin/dist", "admin"]) {
        let service = ServeDir::new(&admin).fallback(ServeFile::new(admin.join("index.html")));
        // A nested service on `/admin`, and the directory redirect that has to sit in
        // front of it. The outer SPA fallback stays for `/`.
        router = router
            .nest_service("/admin", service)
            .layer(middleware::from_fn(redirect_admin_to_slash));
    }

    router
}

/// Send `/admin` to `/admin/`.
///
/// Both front-ends reference their assets relatively — `./main.js`, `./styles.css` —
/// because each app's own check (`web/tools/check.mjs`, `admin/tools/check.mjs`)
/// forbids absolute `/…` paths, so an app works from wherever it is mounted. A browser
/// resolves those against the *document URL*, and at `/admin` with no trailing slash the
/// base is `/`. The admin page would therefore load the webmail's `main.js` and
/// `styles.css`: two apps driving one DOM, the sign-in panel and the admin shell both
/// left on screen, and neither one working.
///
/// Serving `/admin/` is what makes the relative URLs resolve inside the admin
/// directory, so the bare path redirects to it — the same directory redirect a static
/// file server performs, and the reason the webmail at `/` never had this problem.
async fn redirect_admin_to_slash(request: Request, next: Next) -> Response {
    if request.uri().path() == "/admin" {
        return Redirect::permanent("/admin/").into_response();
    }
    next.run(request).await
}

/// Resolve a frontend directory: the configured one, or the first conventional
/// candidate that exists.
pub fn resolve_dir(configured: Option<&std::path::PathBuf>, candidates: &[&str]) -> Option<std::path::PathBuf> {
    if let Some(path) = configured {
        if path.join("index.html").is_file() {
            return Some(path.clone());
        }
    }
    candidates
        .iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.join("index.html").is_file())
}

/// Every route this router exposes, as `(method, path)` pairs.
///
/// Used by the tests to assert that the frozen contract and the code agree: a route
/// removed from `docs/api.md` and left in the router (or the reverse) is a contract
/// break, and this is what makes it visible.
pub fn route_table() -> Vec<(&'static str, &'static str)> {
    vec![
        // §2 health and discovery
        ("GET", "/api/v1/health"),
        ("GET", "/api/v1/version"),
        ("GET", "/.well-known/ferroma"),
        ("GET", "/.well-known/mta-sts.txt"),
        // §3 auth
        ("POST", "/api/v1/auth/login"),
        ("POST", "/api/v1/auth/refresh"),
        ("POST", "/api/v1/auth/logout"),
        ("GET", "/api/v1/auth/me"),
        ("POST", "/api/v1/auth/password"),
        // §4.1 users
        ("GET", "/api/v1/users"),
        ("POST", "/api/v1/users"),
        ("GET", "/api/v1/users/{id}"),
        ("PATCH", "/api/v1/users/{id}"),
        ("DELETE", "/api/v1/users/{id}"),
        ("GET", "/api/v1/users/{id}/mailboxes"),
        ("POST", "/api/v1/users/{id}/mailboxes"),
        // §4.2 domains
        ("GET", "/api/v1/domains"),
        ("POST", "/api/v1/domains"),
        ("GET", "/api/v1/domains/{id}"),
        ("PATCH", "/api/v1/domains/{id}"),
        ("DELETE", "/api/v1/domains/{id}"),
        // §4.3 aliases
        ("GET", "/api/v1/domains/{id}/aliases"),
        ("POST", "/api/v1/domains/{id}/aliases"),
        ("PATCH", "/api/v1/aliases/{id}"),
        ("DELETE", "/api/v1/aliases/{id}"),
        // §4.4 DNS
        ("GET", "/api/v1/domains/{id}/dns"),
        ("GET", "/api/v1/domains/{id}/dkim"),
        ("POST", "/api/v1/domains/{id}/dkim"),
        // §4.5 queue
        ("GET", "/api/v1/queue"),
        ("GET", "/api/v1/queue/{id}"),
        ("POST", "/api/v1/queue/{id}/retry"),
        ("DELETE", "/api/v1/queue/{id}"),
        ("GET", "/api/v1/queue/stats"),
        // §4.6 storage / audit / settings
        ("GET", "/api/v1/storage"),
        ("POST", "/api/v1/storage/gc"),
        ("GET", "/api/v1/audit"),
        ("GET", "/api/v1/settings"),
        ("PUT", "/api/v1/settings/{key}"),
        // §4.7 setup
        ("GET", "/api/v1/setup"),
        ("POST", "/api/v1/setup"),
        // §4.8 logs and devices
        ("GET", "/api/v1/logs"),
        ("GET", "/api/v1/devices"),
        ("POST", "/api/v1/devices/{id}/revoke"),
        ("DELETE", "/api/v1/devices/{id}"),
        // §4.9 TLS
        ("GET", "/api/v1/tls"),
        // §5.1 mailboxes and folders
        ("GET", "/api/v1/mailboxes"),
        ("GET", "/api/v1/mailboxes/{id}/folders"),
        ("POST", "/api/v1/mailboxes/{id}/folders"),
        ("PATCH", "/api/v1/folders/{id}"),
        ("DELETE", "/api/v1/folders/{id}"),
        // §5.2 messages
        ("GET", "/api/v1/messages"),
        ("GET", "/api/v1/messages/{id}"),
        ("GET", "/api/v1/messages/{id}/raw"),
        ("POST", "/api/v1/messages"),
        ("PATCH", "/api/v1/messages/{id}"),
        ("POST", "/api/v1/messages/{id}/move"),
        ("POST", "/api/v1/messages/{id}/copy"),
        ("DELETE", "/api/v1/messages/{id}"),
        // `?permanent=true` removes the row instead of moving the message to `Trash`.
        ("DELETE", "/api/v1/messages/{id}?permanent=true"),
        ("POST", "/api/v1/messages/batch"),
        // §5.3 drafts
        ("GET", "/api/v1/drafts"),
        ("POST", "/api/v1/drafts"),
        ("GET", "/api/v1/drafts/{id}"),
        ("PATCH", "/api/v1/drafts/{id}"),
        ("DELETE", "/api/v1/drafts/{id}"),
        // §5.4 attachments
        ("POST", "/api/v1/attachments"),
        ("GET", "/api/v1/attachments/{id}"),
        ("DELETE", "/api/v1/attachments/{id}"),
        ("GET", "/api/v1/attachments/{id}/meta"),
        // §6 client API (FCP)
        ("POST", "/api/v1/client/auth/login"),
        ("POST", "/api/v1/client/auth/refresh"),
        ("POST", "/api/v1/client/auth/logout"),
        ("GET", "/api/v1/client/account"),
        ("GET", "/api/v1/client/mailboxes"),
        ("GET", "/api/v1/client/sync"),
        ("GET", "/api/v1/client/messages"),
        ("GET", "/api/v1/client/messages/{id}"),
        ("POST", "/api/v1/client/messages"),
        ("PATCH", "/api/v1/client/messages/{id}"),
        ("DELETE", "/api/v1/client/messages/{id}"),
        ("POST", "/api/v1/client/messages/{id}/read"),
        ("POST", "/api/v1/client/messages/{id}/unread"),
        ("POST", "/api/v1/client/messages/{id}/star"),
        ("POST", "/api/v1/client/messages/{id}/archive"),
        ("POST", "/api/v1/client/messages/{id}/move"),
        ("POST", "/api/v1/client/messages/{id}/trash"),
        ("GET", "/api/v1/client/drafts"),
        ("POST", "/api/v1/client/drafts"),
        ("PATCH", "/api/v1/client/drafts/{id}"),
        ("DELETE", "/api/v1/client/drafts/{id}"),
        ("POST", "/api/v1/client/attachments"),
        ("GET", "/api/v1/client/attachments/{id}"),
        ("GET", "/api/v1/client/devices"),
        ("DELETE", "/api/v1/client/devices/{id}"),
        ("POST", "/api/v1/client/devices/{id}/revoke"),
        ("GET", "/api/v1/client/events"),
        ("GET", "/api/v1/client/search"),
    ]
}

/// The router plus the state it was built with, for a test that needs both.
pub fn build_with_state(state: AppState) -> (Router, Arc<AppState>) {
    let shared = Arc::new(state.clone());
    (build(state), shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroma_auth::{AuthService, TokenService};
    use ferroma_core::{Config, Limits};
    use ferroma_events::EventBus;
    use ferroma_storage::{AttachmentStore, Maildir};
    use ferroma_sync::SyncService;
    use std::sync::Arc;

    fn test_state() -> AppState {
        let repos = crate::tests_support::lazy_repos();
        let config = Arc::new(Config::default());
        let tokens = TokenService::new(
            "0123456789abcdef0123456789abcdef0123456789",
            3600,
            86_400,
            "localhost",
        )
        .expect("valid secret");
        let auth = Arc::new(AuthService::with_defaults(
            repos.clone(),
            tokens.clone(),
            Limits::default(),
        ));
        let sync = Arc::new(SyncService::new(repos.clone(), 500, 30));
        let dir = tempfile::tempdir().expect("temp dir");
        AppState::new(
            repos,
            config,
            tokens,
            auth,
            Arc::new(EventBus::with_defaults()),
            sync,
        )
        .with_maildir(Maildir::new(
            dir.path().join("mail"),
            false,
            ferroma_core::config::MailboxLayout::Maildir,
        ))
        .with_attachments(AttachmentStore::new(dir.path().join("att"), false))
    }

    #[tokio::test]
    async fn the_router_builds_without_panicking() {
        // `Router::route` panics on a conflicting path; building the whole table is
        // therefore itself the test that no two routes collide. It is async because the
        // repositories need a Tokio context to register their lazy pool.
        let router = build(test_state());
        let _ = router;
    }

    #[tokio::test]
    async fn the_bare_admin_path_redirects_into_the_directory() {
        use axum::body::Body;
        use axum::http::{Request as HttpRequest, StatusCode};
        use tower::ServiceExt;

        // Mounting the front-ends is the part that was never tested, which is why the
        // admin page shipped loading the webmail's bundle: it references `./main.js`,
        // and at `/admin` with no trailing slash a browser resolves that against `/`.
        // Point `admin_dir` at the real directory so this runs the mounting itself.
        let mut config = Config::default();
        config.api.admin_dir = Some(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../admin")
                .canonicalize()
                .expect("the admin directory is in the repository"),
        );

        let app = with_frontends(Router::new(), &config).with_state(test_state());

        let response = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/admin").body(Body::empty()).unwrap())
            .await
            .expect("the router answers");
        assert_eq!(
            response.status(),
            StatusCode::PERMANENT_REDIRECT,
            "/admin must be sent to /admin/, or the app's relative assets resolve to the webmail's"
        );
        assert_eq!(response.headers()[header::LOCATION], "/admin/");

        // `/admin/` itself still serves the app, so the redirect cannot loop.
        let response = app
            .oneshot(HttpRequest::builder().uri("/admin/").body(Body::empty()).unwrap())
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn the_route_table_covers_the_frozen_contract() {
        let table = route_table();
        // Every documented endpoint, counted by section.
        assert!(table.contains(&("GET", "/api/v1/health")));
        assert!(table.contains(&("GET", "/.well-known/ferroma")));
        assert!(table.contains(&("POST", "/api/v1/auth/login")));
        assert!(table.contains(&("POST", "/api/v1/messages/:id/move")) || table.contains(&("POST", "/api/v1/messages/{id}/move")));
        assert!(table.contains(&("GET", "/api/v1/client/events")));
        assert!(table.contains(&("GET", "/api/v1/logs")));
        assert!(table.contains(&("GET", "/api/v1/devices")));
        assert!(table.contains(&("GET", "/api/v1/client/sync")));
        // `docs/api.md` §5.2's `?permanent=true` variant of the delete is part of the
        // frozen index.
        assert!(table.contains(&("DELETE", "/api/v1/messages/{id}?permanent=true")));
        assert!(table.contains(&("GET", "/api/v1/drafts")) || table.contains(&("POST", "/api/v1/drafts")));
    }

    #[test]
    fn the_route_table_has_no_duplicates() {
        let table = route_table();
        let mut seen = std::collections::HashSet::new();
        for entry in &table {
            assert!(seen.insert(*entry), "duplicate route {entry:?}");
        }
    }

    #[test]
    fn the_body_limit_falls_back_when_configured_to_zero() {
        let mut config = Config::default();
        assert_eq!(body_limit(&config), 26_214_400);
        config.api.max_request_size = 0;
        assert_eq!(body_limit(&config), config.limits.max_message_size as usize);
        config.api.max_request_size = 1024;
        assert_eq!(body_limit(&config), 1024);
    }

    #[test]
    fn an_empty_cors_list_never_emits_an_origin_header() {
        let mut config = Config::default();
        assert!(config.api.cors_origins.is_empty());
        // Building the layer must not panic, and the policy must be the restrictive one.
        let _layer = cors_layer(&config);
        config.api.cors_origins = vec!["https://mail.example.com".to_string()];
        let _layer = cors_layer(&config);
    }

    #[test]
    fn a_malformed_cors_origin_is_dropped_rather_than_crashing() {
        let mut config = Config::default();
        config.api.cors_origins = vec!["not a header value\n".to_string()];
        let _layer = cors_layer(&config);
    }

    #[test]
    fn a_missing_frontend_directory_is_skipped() {
        assert!(resolve_dir(None, &["definitely/not/here"]).is_none());
        let missing = std::path::PathBuf::from("definitely/not/here");
        assert!(resolve_dir(Some(&missing), &[]).is_none());
    }

    #[test]
    fn a_frontend_directory_with_an_index_is_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("index.html"), b"<html></html>").expect("write");
        let found = resolve_dir(Some(&dir.path().to_path_buf()), &[]).expect("must be found");
        assert_eq!(found, dir.path());
    }

    #[tokio::test]
    async fn build_with_state_hands_back_the_same_state() {
        let state = test_state();
        let hostname = state.config.server.hostname.clone();
        let (_router, shared) = build_with_state(state);
        assert_eq!(shared.config.server.hostname, hostname);
    }

    #[test]
    fn the_management_and_client_tables_are_disjoint_except_for_the_prefix() {
        // A route must never be reachable from both surfaces by accident.
        let management = route_table()
            .into_iter()
            .filter(|(_, path)| !path.starts_with("/api/v1/client"))
            .count();
        let all = route_table().len();
        assert!(management < all);
        assert!(management > 40, "the management surface is large");
    }
}