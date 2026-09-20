//! Shared test scaffolding: a migrated PostgreSQL schema and a ready-to-drive router.
//!
//! # One schema per test
//!
//! This is the pattern `crates/ferroma-storage/tests/common/mod.rs` established, copied
//! here rather than shared (that file belongs to another crate, which this one must not
//! edit). `DROP DATABASE ... WITH (FORCE)` cannot run on this host — PostgreSQL signals
//! its checkpointer and the sandbox forbids cross-process signalling — so every test
//! gets its own **schema** inside one shared `ferroma_test` database, selected through
//! the connection's `search_path`. Migrations run into that schema, so each test sees a
//! pristine database, and teardown is an ordinary `DROP SCHEMA ... CASCADE`.
//!
//! # Driving the router
//!
//! [`TestApp`] owns a [`ferroma_api::AppState`] plus a temporary Maildir and blob store,
//! and exposes [`TestApp::request`], which builds a `Request` and runs it through the
//! router with `tower::ServiceExt::oneshot`. No socket is involved, so a test asserts on
//! exactly the bytes a real client would receive without the flakiness of a port.
//!
//! A test that needs no database at all is still a `#[tokio::test]`; it simply never
//! calls [`TestApp::new`].

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use ferroma_api::{AppState, ConnTracker, MailSender};
use ferroma_auth::{AuthService, TokenService};
use ferroma_core::config::MailboxLayout;
use ferroma_core::{Config, Limits};
use ferroma_events::EventBus;
use ferroma_storage::{AttachmentStore, Database, Maildir};
use ferroma_sync::SyncService;
use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};
use tower::ServiceExt;

/// The name of the shared integration-test database.
pub const TEST_DATABASE: &str = "ferroma_test";

/// The administrative connection URL (the `postgres` maintenance database).
pub fn admin_url() -> String {
    std::env::var("FERROMA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferroma@127.0.0.1:5433/postgres".to_string())
}

/// `true` when an integration database is reachable.
///
/// Deliberately *not* used to skip silently: `AGENTS.md` §2.1 records that a skipped
/// test the harness counts as a pass is a hollow green, and this repository has already
/// been burned by one. An unreachable database **fails** the suite; an operator who
/// really has no database opts in explicitly with
/// `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1`.
pub async fn database_available() -> bool {
    match PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3))
        .connect(&admin_url())
        .await
    {
        Ok(pool) => {
            pool.close().await;
            true
        }
        Err(_) => false,
    }
}

/// Whether the operator explicitly opted into skipping without a database.
pub fn skipping_allowed() -> bool {
    std::env::var("FERROMA_TEST_SKIP_WITHOUT_DATABASE")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Skip the current test when no database is reachable — **only** when the operator
/// opted in. Otherwise the test fails loudly, because a silent skip is a lie.
#[macro_export]
macro_rules! require_database {
    () => {
        if !$crate::common::database_available().await {
            if $crate::common::skipping_allowed() {
                eprintln!(
                    "skipping: no PostgreSQL reachable and FERROMA_TEST_SKIP_WITHOUT_DATABASE is set"
                );
                return;
            }
            panic!(
                "no PostgreSQL reachable at {} — start it with scripts/dev-postgres.ps1, \
                 or set FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 to skip deliberately",
                $crate::common::admin_url()
            );
        }
    };
}

/// Swap the database name in a PostgreSQL URL, keeping any query string.
fn with_database(url: &str, db: &str) -> String {
    match url.rfind('/') {
        Some(idx) => {
            let (prefix, rest) = url.split_at(idx + 1);
            let suffix = rest.find('?').map(|q| &rest[q..]).unwrap_or("");
            format!("{prefix}{db}{suffix}")
        }
        None => format!("{url}/{db}"),
    }
}

/// Connect to the maintenance database.
async fn admin_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&admin_url())
        .await
        .expect("cannot connect to the test PostgreSQL — see scripts/dev-postgres.ps1")
}

/// Create the shared test database if it does not exist yet.
///
/// Deliberately never drops it: dropping an in-use database is what broke the storage
/// suite originally.
///
/// Tolerant of losing the race, like the storage/auth/sync harnesses: every test calls
/// this at start-up, so on a fresh cluster a dozen of them see "no such database" at the
/// same instant and all issue `CREATE DATABASE`; one wins and the rest get `42P04
/// duplicate_database` (this server reports it as `23505` on
/// `pg_database_datname_index`). That is success, not failure — so the error is ignored
/// and the outcome is verified afterwards. Failing on it made a whole-suite run red.
async fn ensure_test_database() {
    let admin = connect_with_retry(&admin_url()).await;
    let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(TEST_DATABASE)
        .fetch_optional(&admin)
        .await
        .unwrap_or(None);
    if exists.is_none() {
        if let Err(err) = admin
            .execute(format!("CREATE DATABASE \"{TEST_DATABASE}\"").as_str())
            .await
        {
            let won_by_someone_else: Option<(i32,)> =
                sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
                    .bind(TEST_DATABASE)
                    .fetch_optional(&admin)
                    .await
                    .unwrap_or(None);
            assert!(
                won_by_someone_else.is_some(),
                "cannot create the shared test database {TEST_DATABASE}: {err}"
            );
        }
    }
    admin.close().await;
}

/// Connect, retrying for a few seconds.
///
/// The supervisor may be mid-restart when a suite starts, and `AGENTS.md` §2.1 asks the
/// integration harnesses to tolerate that rather than fail a whole run.
async fn connect_with_retry(url: &str) -> PgPool {
    let mut last_error = None;
    for _ in 0..20 {
        match PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(3))
            .connect(url)
            .await
        {
            Ok(pool) => return pool,
            Err(err) => {
                last_error = Some(err);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!(
        "cannot connect to the test PostgreSQL ({}): {}",
        admin_url(),
        last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    );
}

/// A migrated schema that cleans itself up.
pub struct TestDatabase {
    schema: String,
    db: Database,
    admin: PgPool,
}

impl TestDatabase {
    /// The migrated database.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// The database handle, for `AppState::with_database`.
    pub fn handle(&self) -> &Database {
        &self.db
    }

    /// The pooled connection.
    pub fn pool(&self) -> &PgPool {
        self.db.pool()
    }

    /// Repositories bound to this database.
    pub fn repos(&self) -> ferroma_storage::Repositories {
        self.db.repositories()
    }

    /// The name of this test's schema.
    pub fn name(&self) -> &str {
        &self.schema
    }

    /// Drop the schema.
    pub async fn cleanup(self) {
        let schema = self.schema.clone();
        self.db.close().await;
        let _ = self
            .admin
            .execute(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE").as_str())
            .await;
        let _ = self.admin.close().await;
    }

    /// Run a statement, for test setup.
    pub async fn execute(&self, sql: &str) -> Result<(), sqlx::Error> {
        self.db.pool().execute(sql).await.map(|_| ())
    }

    /// Count rows of a table.
    pub async fn count(&self, table: &str) -> i64 {
        let (n,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(self.db.pool())
            .await
            .unwrap_or((0,));
        n
    }

    /// Read one scalar, for assertions about stored state.
    pub async fn scalar<T>(&self, sql: &str) -> Option<T>
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
    {
        sqlx::query_scalar::<_, T>(sql)
            .fetch_optional(self.db.pool())
            .await
            .ok()
            .flatten()
    }
}

/// Create a fresh, migrated schema.
pub async fn fresh_database() -> TestDatabase {
    ensure_test_database().await;

    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let url = with_database(&admin_url(), TEST_DATABASE);

    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("cannot connect to the shared test database");
    bootstrap
        .execute(format!("CREATE SCHEMA \"{schema}\"").as_str())
        .await
        .expect("cannot create the test schema");
    bootstrap.close().await;

    let options: PgConnectOptions = url
        .parse()
        .expect("test database URL must be a valid PostgreSQL URL");
    let options = options.options([("search_path", schema.as_str())]);

    let db = Database::connect_with_options(
        options,
        8,
        1,
        Duration::from_secs(10),
        Duration::from_secs(60),
        Duration::from_secs(300),
        false,
    )
    .await
    .expect("cannot connect to the test schema");
    db.migrate().await.expect("migrations must apply cleanly");

    TestDatabase {
        schema,
        db,
        admin: admin_pool().await,
    }
}

/// A running API over a migrated schema, a temporary Maildir and a temporary blob store.
pub struct TestApp {
    /// The router under test.
    pub router: Router,
    /// The state the router was built with.
    pub state: AppState,
    /// The migrated database, cleaned up by [`TestApp::cleanup`].
    pub database: TestDatabase,
    /// The temporary directory holding the Maildir and the blob store.
    dir: tempfile::TempDir,
}

impl TestApp {
    /// Build an app over a fresh schema.
    pub async fn new() -> Self {
        Self::with_config(Config::default()).await
    }

    /// Build an app with a tweaked configuration.
    pub async fn with_config(mut config: Config) -> Self {
        let database = fresh_database().await;
        let repos = database.repos();
        let dir = tempfile::tempdir().expect("temp dir");

        config.api.jwt_secret = Some("test-secret-0123456789abcdef0123456789".to_string());
        let config = Arc::new(config);

        let tokens = TokenService::new(
            config
                .api
                .jwt_secret
                .as_deref()
                .unwrap_or("test-secret-0123456789abcdef0123456789"),
            config.api.access_token_ttl_secs,
            config.api.refresh_token_ttl_secs,
            &config.server.hostname,
        )
        .expect("the test secret is long enough");
        let auth = Arc::new(AuthService::with_defaults(
            repos.clone(),
            tokens.clone(),
            Limits::default(),
        ));
        let sync = Arc::new(SyncService::new(
            repos.clone(),
            config.client.sync_page_size,
            config.client.tombstone_retention_days,
        ));

        let state = AppState::new(
            repos,
            Arc::clone(&config),
            tokens,
            auth,
            Arc::new(EventBus::with_defaults()),
            sync,
        )
        .with_database(Arc::new(database.handle().clone()))
        .with_maildir(Maildir::new(
            dir.path().join("mail"),
            false,
            MailboxLayout::Maildir,
        ))
        .with_attachments(AttachmentStore::new(dir.path().join("attachments"), false))
        .with_connections(Arc::new(ConnTracker::new()));

        let router = ferroma_api::build(state.clone());
        TestApp {
            router,
            state,
            database,
            dir,
        }
    }

    /// Replace the delivery seam, so a test can assert on what would be queued.
    pub fn with_mail_sender(mut self, sender: Arc<dyn MailSender>) -> Self {
        self.state = self.state.clone().with_mail_sender(sender);
        self.router = ferroma_api::build(self.state.clone());
        self
    }

    /// The temporary directory the stores live in.
    pub fn data_dir(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// The database.
    pub fn db(&self) -> &TestDatabase {
        &self.database
    }

    /// Issue a request and collect the response.
    pub async fn request(&self, request: Request<Body>) -> TestResponse {
        let router = self.router.clone();
        let response = router
            .oneshot(request)
            .await
            .expect("the router is infallible for a well-formed request");
        TestResponse::from_response(response).await
    }

    /// A `GET` with an optional bearer token.
    pub async fn get(&self, path: &str, token: Option<&str>) -> TestResponse {
        let mut builder = Request::builder().method("GET").uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.request(builder.body(Body::empty()).expect("valid request"))
            .await
    }

    /// A `GET` on the client surface, with the FCP headers.
    pub async fn client_get(&self, path: &str, token: Option<&str>) -> TestResponse {
        let mut builder = Request::builder()
            .method("GET")
            .uri(path)
            .header("x-ferroma-client", "FerromaClient/0.7.0")
            .header("x-ferroma-protocol", "1")
            .header("x-ferroma-platform", "windows");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.request(builder.body(Body::empty()).expect("valid request"))
            .await
    }

    /// A JSON `POST`/`PATCH`/`PUT`/`DELETE`.
    pub async fn json(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> TestResponse {
        self.json_with_headers(method, path, token, body, &[]).await
    }

    /// A JSON request with extra headers.
    pub async fn json_with_headers(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
        extra: &[(&str, &str)],
    ) -> TestResponse {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        self.request(
            builder
                .body(Body::from(serde_json::to_vec(&body).expect("body must serialise")))
                .expect("valid request"),
        )
        .await
    }

    /// A JSON request on the client surface.
    pub async fn client_json(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> TestResponse {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-ferroma-client", "FerromaClient/0.7.0")
            .header("x-ferroma-protocol", "1");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.request(
            builder
                .body(Body::from(serde_json::to_vec(&body).expect("body must serialise")))
                .expect("valid request"),
        )
        .await
    }

    /// A `DELETE` with an optional token.
    pub async fn delete(&self, path: &str, token: Option<&str>) -> TestResponse {
        let mut builder = Request::builder().method("DELETE").uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.request(builder.body(Body::empty()).expect("valid request"))
            .await
    }

    /// Drop the schema.
    pub async fn cleanup(self) {
        self.database.cleanup().await;
    }
}

/// A response, fully buffered so assertions can read it twice.
pub struct TestResponse {
    /// The status code.
    pub status: StatusCode,
    /// The headers.
    pub headers: axum::http::HeaderMap,
    /// The body bytes.
    pub body: Vec<u8>,
}

impl TestResponse {
    /// Buffer an axum response.
    pub async fn from_response(response: axum::response::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .expect("the body must be readable")
            .to_vec();
        TestResponse {
            status,
            headers,
            body,
        }
    }

    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!(
                "body is not JSON ({err}): status={} body={}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// The body as text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// Assert the status and return the JSON body.
    ///
    /// Only for a response that *has* a JSON body. A `204 No Content` — which several
    /// endpoints answer with — must be checked with [`TestResponse::is`] instead, because
    /// there is nothing to parse.
    pub fn expect(&self, status: StatusCode) -> Value {
        self.is(status);
        self.json()
    }

    /// Assert the status and return nothing, for a `204` or any body-less answer.
    pub fn is(&self, status: StatusCode) -> &Self {
        if self.status != status {
            panic!(
                "status mismatch: expected {status}, got {} with body {:?}",
                self.status,
                self.text()
            );
        }
        self
    }

    /// The `error.code` of an error envelope.
    pub fn error_code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// One header, as a string.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }
}

/// Log in and return the access token.
pub async fn login(app: &TestApp, email: &str, password: &str) -> String {
    let response = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            serde_json::json!({ "email": email, "password": password }),
        )
        .await;
    let body = response.expect(StatusCode::OK);
    body["access_token"]
        .as_str()
        .expect("a successful login returns an access token")
        .to_string()
}

/// Create a domain, an account and an address, and return `(user_id, mailbox_id, token)`.
///
/// The three steps every mail test needs, in the order the API requires: a domain must
/// exist before an address can live in it. The domain step is **idempotent**, because
/// several tests seed two accounts into one domain and a second creation is a `409` by
/// design — the same rule the API documents for a duplicate.
pub async fn seed_account(
    app: &TestApp,
    admin_token: &str,
    domain: &str,
    email: &str,
    password: &str,
) -> (i64, i64, String) {
    let existing = app.get("/api/v1/domains", Some(admin_token)).await;
    let already_there = existing
        .json()
        .get("items")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .any(|row| row["name"].as_str() == Some(domain))
        })
        .unwrap_or(false);

    if !already_there {
        app.json(
            "POST",
            "/api/v1/domains",
            Some(admin_token),
            serde_json::json!({ "name": domain }),
        )
        .await
        .expect(StatusCode::CREATED);
    }

    let user_body = app
        .json(
            "POST",
            "/api/v1/users",
            Some(admin_token),
            serde_json::json!({
                "email": email,
                "password": password,
                "display_name": "Test User"
            }),
        )
        .await
        .expect(StatusCode::CREATED);
    let user_id = user_body["id"].as_i64().expect("user id");

    // `POST /users` provisions the account's primary address itself — the domain was
    // created above, so the address is part of that same response. Reading it back is
    // what keeps this helper idempotent; creating it again would be the documented
    // `409 conflict` for a duplicate address.
    let mailbox_id = user_body["mailboxes"]
        .as_array()
        .and_then(|mailboxes| mailboxes.first())
        .and_then(|mailbox| mailbox["id"].as_i64())
        .unwrap_or_else(|| {
            panic!(
                "POST /users did not create {}'s primary address: {user_body}",
                local_part_of(email)
            )
        });

    let token = login(app, email, password).await;
    (user_id, mailbox_id, token)
}

/// The part of an address before the `@`.
fn local_part_of(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
}

/// The id of one of an address's folders, by name.
pub async fn folder_id(app: &TestApp, token: &str, mailbox_id: i64, name: &str) -> i64 {
    let body = app
        .get(&format!("/api/v1/mailboxes/{mailbox_id}/folders"), Some(token))
        .await
        .expect(StatusCode::OK);
    body["folders"]
        .as_array()
        .expect("folders is an array")
        .iter()
        .find(|folder| folder["name"].as_str() == Some(name))
        .and_then(|folder| folder["id"].as_i64())
        .unwrap_or_else(|| panic!("no folder named {name}: {body}"))
}