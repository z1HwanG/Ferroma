//! Shared helpers for `ferroma-auth` integration tests.
//!
//! These tests need a real PostgreSQL. Point them at one with:
//!
//! ```text
//! FERROMA_TEST_DATABASE_URL=postgres://ferroma@127.0.0.1:5433/postgres
//! ```
//!
//! # Isolation: one schema per test
//!
//! Each test gets its own PostgreSQL **schema**, selected through the connection's
//! `search_path`, and the migrations run into it. See the longer explanation in
//! `crates/ferroma-storage/tests/common/mod.rs` — a database-per-test design does
//! not work on this host, because `DROP DATABASE ... WITH (FORCE)` requires
//! signalling the checkpointer and this sandbox forbids cross-process signalling.

#![allow(dead_code)]

use std::time::Duration;

use ferroma_auth::{AuthService, TokenService};
use ferroma_core::Limits;
use ferroma_storage::Database;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};

/// The name of the shared integration-test database.
pub const TEST_DATABASE: &str = "ferroma_test";

/// A signing secret long enough for HS256. Obviously not a real secret.
pub const TEST_SECRET: &str = "ferroma-test-secret-that-is-at-least-32-bytes-long";

/// The administrative connection URL.
pub fn admin_url() -> String {
    std::env::var("FERROMA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferroma@127.0.0.1:5433/postgres".to_string())
}

/// Whether an integration database is reachable.
///
/// Panics rather than returning `false`, because a skip that the harness counts as a
/// pass is a hollow green — see the longer explanation in
/// `crates/ferroma-storage/tests/common/mod.rs`. `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1`
/// opts into skipping explicitly.
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
        Err(err) => {
            if std::env::var_os("FERROMA_TEST_SKIP_WITHOUT_DATABASE").is_some() {
                return false;
            }
            panic!(
                "cannot reach the integration PostgreSQL at {}: {err}\n\
                 Start it with `scripts/dev-postgres.ps1 start`, point \
                 FERROMA_TEST_DATABASE_URL at another one, or set \
                 FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 to skip these tests explicitly.",
                admin_url()
            );
        }
    }
}

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

/// Create the shared test database if it does not exist yet.
///
/// Tolerant of losing the race: every test calls this at start-up, so on a fresh
/// cluster many of them see "no such database" at once and all issue `CREATE
/// DATABASE`; one wins and the rest get `42P04 duplicate_database`, which is success.
async fn ensure_test_database() {
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&admin_url())
        .await
        .expect("cannot connect to the test PostgreSQL — see scripts/dev-postgres.ps1");
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

/// A migrated schema, an `AuthService` bound to it, and a cleanup guard.
pub struct TestAuth {
    schema: String,
    db: Database,
    admin: PgPool,
    /// The service under test, with deliberately cheap Argon2 parameters so the
    /// suite stays fast.
    pub auth: AuthService,
    /// The token service, so tests can mint and verify tokens directly.
    pub tokens: TokenService,
}

impl TestAuth {
    /// The migrated database.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// The pooled connection.
    pub fn pool(&self) -> &PgPool {
        self.db.pool()
    }

    /// Repositories bound to this schema.
    pub fn repos(&self) -> ferroma_storage::Repositories {
        self.db.repositories()
    }

    /// Create an account with the standard test password.
    pub async fn create_user(&self, email: &str) -> ferroma_storage::models::User {
        self.auth
            .create_user(email, TEST_PASSWORD, Some("Test User"), false, true, None)
            .await
            .expect("create_user")
    }

    /// Create an admin account.
    pub async fn create_admin(&self, email: &str) -> ferroma_storage::models::User {
        self.auth
            .create_user(email, TEST_PASSWORD, Some("Admin"), true, true, None)
            .await
            .expect("create_admin")
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
}

/// The password every test account uses. Long enough to pass the policy.
pub const TEST_PASSWORD: &str = "correct horse battery staple";

/// Build a fresh, migrated schema with an `AuthService` over it.
pub async fn fresh_auth() -> TestAuth {
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

    let options: PgConnectOptions = url.parse().expect("valid PostgreSQL URL");
    let options = options.options([("search_path", schema.as_str())]);

    let db = Database::connect_with_options(
        options,
        // Small on purpose: every test in every test binary opens its own pool, so a
        // large per-test pool exhausts PostgreSQL's `max_connections` and shows up as
        // unrelated timeouts once the suite runs under load.
        2,
        1,
        Duration::from_secs(10),
        Duration::from_secs(60),
        Duration::from_secs(300),
        false,
    )
    .await
    .expect("cannot connect to the test schema");
    db.migrate().await.expect("migrations must apply cleanly");

    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&admin_url())
        .await
        .expect("cannot connect for cleanup");

    let tokens = TokenService::new(TEST_SECRET, 3600, 2_592_000, "mail.example.com").unwrap();
    // `fast_for_tests` keeps 19 MiB Argon2 runs out of a test suite that creates a
    // dozen accounts; the production parameters are covered by unit tests.
    let auth = AuthService::new(
        db.repositories(),
        tokens.clone(),
        ferroma_auth::PasswordHasher::fast_for_tests(),
        Limits::default(),
    );

    TestAuth {
        schema,
        db,
        admin,
        auth,
        tokens,
    }
}
