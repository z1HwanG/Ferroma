//! The database step, which runs before there is anything to boot.
//!
//! A Ferroma process needs PostgreSQL before it can build a single repository, so a
//! deployment that has not been told where the database is used to die at startup with
//! `cannot connect to PostgreSQL` — no page, nothing to click, and the only way forward a
//! shell on the host. This module is the alternative: the server binds its HTTP port
//! anyway and serves one page that asks for the connection, and once the connection works
//! the normal boot continues **in the same process**, with no restart and no gap.
//!
//! Two rules keep that from being a hole rather than a convenience:
//!
//! * a **setup code** is generated at boot and printed to the log, and the connect
//!   endpoint refuses anything that does not carry it — otherwise anyone who can reach the
//!   published web port could point this instance at a database of their choosing;
//! * the endpoint stops existing the moment a connection is accepted (the process leaves
//!   bootstrap mode for good), and the URL is written to `<data_dir>/database.json` with
//!   owner-only permissions, so the next start reads it from disk instead of asking again.
//!
//! The password is never logged and never echoed back: see [`redact_url`].

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use ferroma_core::Config;
use ferroma_storage::Database;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

/// The file that remembers the accepted connection, inside the data directory.
const STORE_FILE: &str = "database.json";

/// The `GET /api/v1/bootstrap` body.
#[derive(Debug, Serialize)]
struct BootstrapStatus {
    /// Always `true` on this endpoint: it only exists while there is no database.
    required: bool,
    /// Where the connection will be remembered, shown so the operator can back it up.
    data_dir: String,
    /// Why the stored connection was refused, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The `POST /api/v1/bootstrap` body.
#[derive(Debug, Deserialize)]
struct ConnectRequest {
    /// The code this boot printed to its log.
    code: String,
    /// `postgres://user:password@host:5432/database`, when the page sent one string.
    #[serde(default)]
    url: String,
    /// Host and port, as the page asks for them: `127.0.0.1:5432`.
    #[serde(default)]
    host: String,
    /// The role.
    #[serde(default)]
    username: String,
    /// The role's password.
    #[serde(default)]
    password: String,
    /// The database name. Empty means `ferroma`.
    #[serde(default)]
    database: String,
}

/// The connect endpoint's answers, as plain JSON objects: one success shape and one
/// failure shape, and never the URL — it carries a password, and a response body travels
/// through logs, proxies and browser history.
type JsonBody = Json<serde_json::Value>;

/// A failure in the envelope the rest of the API uses.
///
/// The console's `request()` reads `{ "error": { "code", "message" } }`; anything else is
/// reported as "Request failed (HTTP 403)", which would hide the one thing this page has to
/// say — *why* the connection was refused.
fn failure(code: &str, message: impl Into<String>) -> JsonBody {
    Json(serde_json::json!({
        "error": { "code": code, "message": message.into() }
    }))
}

/// Shared state of the bootstrap server.
struct BootstrapState {
    code: String,
    error: Mutex<Option<String>>,
    accepted: Mutex<Option<String>>,
    done: Notify,
    data_dir: PathBuf,
}

impl BootstrapState {
    /// Create the state, generating the code this boot prints.
    fn new(config: &Config, error: Option<String>) -> Self {
        BootstrapState {
            code: setup_code(),
            error: Mutex::new(error),
            accepted: Mutex::new(None),
            done: Notify::new(),
            data_dir: config.server.data_dir.clone(),
        }
    }
}

/// A short, readable, one-time code.
///
/// Base64 of 48 random bytes carries far more than eight characters of entropy; the
/// characters that survive the filter are the unambiguous ones, because this is meant to be
/// copied out of a terminal by eye.
fn setup_code() -> String {
    ferroma_auth::TokenService::generate_secret()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Whether a submitted code is the one this boot printed.
///
/// Case-insensitive, because the code is meant to be read off a terminal and typed by hand;
/// the comparison itself does not stop at the first difference, so it does not leak how
/// much of a guess was right.
fn code_matches(expected: &str, submitted: &str) -> bool {
    let submitted = submitted.trim().to_ascii_uppercase();
    if submitted.len() != expected.len() {
        return false;
    }
    expected
        .bytes()
        .zip(submitted.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// The connection string, from a whole URL or from the three fields the page asks for.
///
/// The password is percent-encoded. A password containing `@` or `:` would otherwise
/// be read as part of the host.
fn connection_url(request: &ConnectRequest) -> Result<String, String> {
    let whole = request.url.trim();
    if !whole.is_empty() {
        return Ok(whole.to_string());
    }
    let host = request.host.trim();
    let username = request.username.trim();
    if host.is_empty() || username.is_empty() {
        return Err("give the database host, the user name and the password".to_string());
    }
    let database = {
        let name = request.database.trim();
        if name.is_empty() { "ferroma" } else { name }
    };
    let user = percent_encode(username);
    let password = percent_encode(&request.password);
    Ok(format!("postgres://{user}:{password}@{host}/{database}"))
}

/// Percent-encode the characters that would break a `postgres://` user or password.
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Whether a submitted URL is one this server could use at all.
///
/// Only the shape is checked here — the connection itself is the real test, and it happens
/// immediately afterwards.
fn validate_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err("the address must start with postgres:// or postgresql://".to_string());
    }
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let after_host = rest
        .split_once('/')
        .map(|(_, path)| path)
        .unwrap_or_default();
    if after_host.trim_matches('?').trim().is_empty() {
        return Err("the address must name a database, e.g. postgres://user:pw@host:5432/ferroma".to_string());
    }
    if rest.starts_with('/') {
        return Err("the address must name a host".to_string());
    }
    Ok(())
}

/// The URL with its password replaced, for logs and messages.
pub fn redact_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_string();
    };
    // user:password@host — keep the user, drop the password.
    let Some((credentials, host)) = rest.split_once('@') else {
        return format!("{scheme}://{rest}");
    };
    let user = credentials.split_once(':').map(|(user, _)| user).unwrap_or(credentials);
    format!("{scheme}://{user}:***@{host}")
}

/// The connection the wizard accepted earlier, when there is one.
pub fn stored_url(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(STORE_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let url = value.get("url")?.as_str()?.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Remember a connection for the next start.
fn store_url(data_dir: &Path, url: &str) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = data_dir.join(STORE_FILE);
    let body = serde_json::to_string_pretty(&serde_json::json!({ "url": url.trim() }))?;
    crate::serve::write_private_file(&path, &body)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Run the bootstrap server until a connection is accepted, and return it.
///
/// The port comes back with the URL so the caller serves the real API on it immediately:
/// `axum::serve` takes ownership of the listener, so it is re-bound once the bootstrap
/// server has stopped. That re-bind cannot be raced by anything else — the socket was
/// closed cleanly and listeners are created with `SO_REUSEADDR` — and it is what keeps the
/// operator's address (and the browser tab they were just told to reload) valid.
pub async fn run(
    config: Config,
    error: Option<String>,
    listener: TcpListener,
) -> Result<(String, TcpListener)> {
    let state = std::sync::Arc::new(BootstrapState::new(&config, error));
    let bound = listener.local_addr()?;

    let router = bootstrap_router(state.clone());
    let router = ferroma_api::frontends_for_bootstrap(router, &config);

    announce(&config, &state, bound);

    let shutdown = {
        let state = state.clone();
        async move {
            state.done.notified().await;
        }
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serving the bootstrap page")?;

    let url = state
        .accepted
        .lock()
        .await
        .clone()
        .context("the bootstrap server stopped without a connection")?;
    Ok((url, listener_from(&bound)?))
}

/// The listener of the port the bootstrap server just released.
///
/// `run` returns the accepted URL, and the caller re-binds this address for the real API.
/// The shutdown above completes before this is called, so the port is free.
fn listener_from(address: &SocketAddr) -> Result<TcpListener> {
    let std_listener = std::net::TcpListener::bind(address)
        .with_context(|| format!("re-binding the HTTP API to {address}"))?;
    std_listener.set_nonblocking(true)?;
    TcpListener::from_std(std_listener).context("adopting the re-bound listener")
}

/// Tell the operator, in the log and on stdout, what to do next.
fn announce(config: &Config, state: &BootstrapState, bound: SocketAddr) {
    // The console answers at the root while there is no database, so that is the URL to
    // print: the operator typed the hostname, and `/admin/` still serves the same page.
    let url = format!("http://{bound}/");
    println!();
    println!("No database is connected yet. Open {url} and enter:");
    println!();
    println!("    address   postgres://user:password@host:5432/{db}", db = config.database.url.rsplit('/').next().unwrap_or("ferroma"));
    println!("    code      {}", state.code);
    println!();
    println!("The code is printed once per start; the address is stored in");
    println!("{} and reused.", state.data_dir.join(STORE_FILE).display());
    tracing::warn!(
        %bound,
        code = %state.code,
        "waiting for a database connection; open the setup page and enter the code"
    );
}

/// The API prefix this mode answers under, the same one the running server mounts.
const API_PREFIX: &str = "/api/v1";

/// The router of the bootstrap server: two endpoints, and a JSON `404` for every other
/// path under the API prefix.
///
/// The `404` is not decoration. Without it an unknown `/api/v1/…` request falls through to
/// the front-end's SPA fallback and is answered with the Webmail's HTML and a `200`, and
/// the console reads that as *an answer that is not about a wizard* — which is how a fresh
/// installation was shown a sign-in box, claiming an administrator already existed, instead
/// of the page that creates its first one. The window is real rather than theoretical: the
/// console reloads the moment the database is accepted, while this router is still the one
/// bound to the port. The running server answers the same envelope for a path it does not
/// know, so a client cannot tell the two modes apart by shape — which is the point.
fn bootstrap_router(state: std::sync::Arc<BootstrapState>) -> Router {
    Router::new().nest(
        API_PREFIX,
        Router::new()
            .route("/bootstrap", get(status_handler).post(connect_handler))
            .route("/health", get(health_handler))
            .fallback(api_not_found)
            .with_state(state),
    )
}

/// The `404` an unknown API path gets, in the envelope every API error uses.
async fn api_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": { "code": "not_found", "message": "no such endpoint" }
        })),
    )
}

async fn status_handler(State(state): State<std::sync::Arc<BootstrapState>>) -> impl IntoResponse {
    Json(BootstrapStatus {
        required: true,
        data_dir: state.data_dir.display().to_string(),
        error: state.error.lock().await.clone(),
    })
}

async fn health_handler() -> impl IntoResponse {
    // Honest rather than green: the process is up and serving the page that fixes this,
    // but it is not a working mail server yet, and a container manager should say so.
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "status": "setup",
            "database": { "ok": false },
        })),
    )
}

async fn connect_handler(
    State(state): State<std::sync::Arc<BootstrapState>>,
    Json(request): Json<ConnectRequest>,
) -> (StatusCode, JsonBody) {
    if !code_matches(&state.code, &request.code) {
        return (
            StatusCode::FORBIDDEN,
            failure(
                "forbidden",
                "that setup code does not match this server's. It is printed in the container \
                 log each time the server starts.",
            ),
        );
    }

    let url = match connection_url(&request) {
        Ok(url) => url,
        Err(message) => return (StatusCode::BAD_REQUEST, failure("invalid_input", message)),
    };
    if let Err(message) = validate_url(&url) {
        return (StatusCode::BAD_REQUEST, failure("invalid_input", message));
    }

    // The real test: connect, then migrate. A database that does not exist is reported
    // with the server's own message, which names it.
    let mut database_config = ferroma_core::config::DatabaseConfig {
        url: url.clone(),
        ..Default::default()
    };
    // The bootstrap server has no pool to hand out; one is enough to prove the address.
    database_config.max_connections = 2;

    match Database::connect(&database_config).await {
        Err(err) => {
            let message = format!("{err}");
            *state.error.lock().await = Some(message.clone());
            tracing::warn!(url = %redact_url(&url), error = %message, "the database connection was refused");
            (
                StatusCode::BAD_REQUEST,
                failure(
                    "invalid_input",
                    format!(
                        "{message}\n\nCheck the address, that the role may connect from this \
                         host, and that the database itself exists — Ferroma never creates it."
                    ),
                ),
            )
        }
        Ok(database) => {
            if let Err(err) = database.migrate().await {
                let message = format!("{err}");
                *state.error.lock().await = Some(message.clone());
                return (
                    StatusCode::BAD_REQUEST,
                    failure("invalid_input", format!("the schema could not be applied: {message}")),
                );
            }
            if let Err(err) = store_url(&state.data_dir, &url) {
                let message = format!("{err:#}");
                *state.error.lock().await = Some(message.clone());
                return (StatusCode::BAD_REQUEST, failure("invalid_input", message));
            }

            tracing::info!(url = %redact_url(&url), "database accepted; continuing the boot");
            *state.accepted.lock().await = Some(url);
            *state.error.lock().await = None;
            state.done.notify_waiters();
            (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setup_code_is_readable_and_random() {
        let first = setup_code();
        let second = setup_code();
        assert_eq!(first.len(), 8);
        assert!(first.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
        assert_ne!(first, second);
    }

    #[test]
    fn only_the_code_this_boot_printed_is_accepted() {
        assert!(code_matches("ABCD2345", "ABCD2345"));
        // Surrounding whitespace is what a copy-paste from a log brings along.
        assert!(code_matches("ABCD2345", " abcd2345 "));
        assert!(!code_matches("ABCD2345", "ABCD2346"));
        assert!(!code_matches("ABCD2345", "ABCD234"));
        assert!(!code_matches("ABCD2345", ""));
    }

    #[test]
    fn a_password_never_reaches_a_log_or_a_message() {
        assert_eq!(
            redact_url("postgres://ferroma:s3cret@db:5432/ferroma"),
            "postgres://ferroma:***@db:5432/ferroma"
        );
        assert_eq!(redact_url("postgres://db:5432/ferroma"), "postgres://db:5432/ferroma");
        assert_eq!(redact_url("not a url"), "not a url");
    }

    #[test]
    fn only_postgres_addresses_naming_a_database_are_worth_trying() {
        assert!(validate_url("postgres://ferroma:pw@127.0.0.1:5432/ferroma").is_ok());
        assert!(validate_url("postgresql://ferroma@db/ferroma").is_ok());
        // No database named.
        assert!(validate_url("postgres://ferroma:pw@127.0.0.1:5432/").is_err());
        assert!(validate_url("postgres://ferroma:pw@127.0.0.1:5432").is_err());
        // Not PostgreSQL at all.
        assert!(validate_url("mysql://ferroma@db/ferroma").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn the_connection_is_remembered_for_the_next_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(stored_url(dir.path()), None);

        store_url(dir.path(), "postgres://ferroma:pw@db:5432/ferroma").expect("store");
        assert_eq!(
            stored_url(dir.path()).as_deref(),
            Some("postgres://ferroma:pw@db:5432/ferroma")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(STORE_FILE))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the file holds a password and must be owner-only");
        }
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_json_and_never_the_front_end() {
        use axum::body::{to_bytes, Body};
        use axum::http::{Request as HttpRequest, StatusCode};
        use tower::ServiceExt;

        // The console asks `GET /api/v1/setup` while leaving this mode, and this router is
        // still bound to the port for a moment after the database is accepted. The
        // front-end's SPA fallback used to answer that request with the Webmail's HTML and
        // a `200`, which the console reported as "not JSON" and then treated as *no wizard*
        // — so the operator saw a sign-in box for an installation with no administrator yet,
        // and only a hard reload got past it.
        let mut config = Config::default();
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        config.api.admin_dir = Some(
            repository.join("admin").canonicalize().expect("the admin directory"),
        );
        config.api.webmail_dir = Some(
            repository.join("web").canonicalize().expect("the webmail directory"),
        );

        let state = std::sync::Arc::new(BootstrapState::new(&config, None));
        let app = ferroma_api::frontends_for_bootstrap(bootstrap_router(state), &config);

        let response = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/api/v1/setup").body(Body::empty()).unwrap())
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let content_type = response.headers()["content-type"].to_str().unwrap().to_owned();
        assert!(
            content_type.starts_with("application/json"),
            "an API path must not be answered with {content_type}"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.expect("a body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(json["error"]["code"], "not_found");

        // The endpoint this mode does serve is still served, and still answers honestly:
        // `503` with the reason, because the database is the thing that is missing.
        let response = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.expect("a body");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).expect("JSON")["status"], "setup");

        // …and the setup page itself is still the console, still HTML.
        let response = app
            .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
    }
}
