//! Shared helpers for `ferroma-storage` integration tests.
//!
//! These tests need a real PostgreSQL. Point them at one with:
//!
//! ```text
//! FERROMA_TEST_DATABASE_URL=postgres://ferroma@127.0.0.1:5433/postgres
//! ```
//!
//! The default matches the development cluster that `scripts/dev-postgres.ps1`
//! starts inside the workspace, so `cargo test -p ferroma-storage` works out of the
//! box on a developer machine.
//!
//! # Isolation strategy: one schema per test, not one database per test
//!
//! The obvious design — create a database per test — does not work on this host.
//! `DROP DATABASE ... WITH (FORCE)` makes PostgreSQL signal its checkpointer and
//! terminate other backends, and this sandbox forbids cross-process signalling:
//!
//! ```text
//! ERROR:  could not signal for checkpoint: Operation not permitted
//! STATEMENT:  DROP DATABASE IF EXISTS "ferroma_test_..." WITH (FORCE)
//! ```
//!
//! Under the parallel test harness those errors accumulated until a backend died
//! and took the whole cluster with it. (`pg_terminate_backend` sends the same
//! forbidden signal, so it is not a workaround either.)
//!
//! Instead there is **one** `ferroma_test` database for the whole run, and each test
//! gets its own PostgreSQL **schema**, selected through the connection's
//! `search_path`. The embedded migrations run into that schema, so every test still
//! sees a pristine, fully-migrated, empty database — and teardown is an ordinary
//! `DROP SCHEMA ... CASCADE`, which signals nothing and cannot disturb a neighbour.

#![allow(dead_code)]

use std::time::Duration;

use ferroma_storage::Database;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};

/// The name of the shared integration-test database.
pub const TEST_DATABASE: &str = "ferroma_test";

/// The administrative connection URL (points at the `postgres` maintenance database).
pub fn admin_url() -> String {
    std::env::var("FERROMA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferroma@127.0.0.1:5433/postgres".to_string())
}

/// Whether an integration database is reachable.
///
/// # Silence is never green
///
/// This function used to return `false` so a test could print "skipping" and return
/// early, and the harness counted that early return as a **pass**. The result was a
/// hollow green: when the development cluster was down, `cargo test` reported
/// "56 passed, 0 failed" in a suspiciously short time while not one test had run.
///
/// It now panics instead, unless the operator explicitly opts into skipping with
/// `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1`. A machine without PostgreSQL can still run
/// the suite; it just has to say so.
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
                 These tests need a database. Start the development cluster with\n\
                 `scripts/dev-postgres.ps1 start` (or `scripts/pg-supervisor.ps1`), point\n\
                 FERROMA_TEST_DATABASE_URL at another one, or set\n\
                 FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 to skip them explicitly.",
                admin_url()
            );
        }
    }
}

/// Connect to the maintenance database.
///
/// Retries, because the development cluster on this host is supervised and can be
/// mid-restart: this sandbox forbids cross-process signalling, and PostgreSQL's
/// startup process performs an end-of-recovery checkpoint by signalling the
/// checkpointer, which fails with `Operation not permitted` and takes the server
/// down. `pg-supervisor.ps1` brings it back within seconds, so a few retries turn a
/// spurious failure into a short pause.
pub async fn admin_pool() -> PgPool {
    let mut last_error = None;
    for attempt in 0..6 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        match PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&admin_url())
            .await
        {
            Ok(pool) => return pool,
            Err(e) => last_error = Some(e),
        }
    }

    panic!(
        "cannot connect to the test PostgreSQL at {}: {}\n\
         Start it with `scripts/dev-postgres.ps1 start` (or `scripts/pg-supervisor.ps1`).",
        admin_url(),
        last_error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    )
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

/// Create the shared test database if it does not exist yet.
///
/// Deliberately never drops it: dropping an in-use database is what broke this
/// suite once already.
///
/// The create is tolerant of losing a race. Every test in every test binary calls
/// this at start-up, so on a fresh cluster a dozen of them see "no such database"
/// simultaneously and all issue `CREATE DATABASE`; exactly one wins and the rest get
/// `42P04 duplicate_database`. That is success, not failure.
pub async fn ensure_test_database() {
    let admin = admin_pool().await;
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

/// Create a fresh, migrated schema and hand back a database bound to it.
///
/// The pool is deliberately tiny (2 connections). Cargo runs test *binaries* in
/// parallel and every test opens its own pool, so a generous per-test pool
/// multiplies out past PostgreSQL's connection limit — the symptom is unrelated
/// timeouts in whichever suite happens to be running. Tests that genuinely need a
/// wide parallel window should call [`fresh_database_with_pool`] instead.
pub async fn fresh_database() -> TestDatabase {
    fresh_database_with_pool(2).await
}

/// Like [`fresh_database`], but with an explicit pool size.
///
/// Use this for the handful of tests whose whole point is concurrency — UID
/// allocation and queue claiming — where a two-connection pool would serialize the
/// tasks on connection acquisition instead of on the row locks under test.
pub async fn fresh_database_with_pool(max_connections: u32) -> TestDatabase {
    ensure_test_database().await;

    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let url = with_database(&admin_url(), TEST_DATABASE);

    // Bootstrap connection: create the schema itself. `search_path` cannot point at
    // a schema that does not exist yet.
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect(&url)
        .await
        .expect("cannot connect to the shared test database");
    bootstrap
        .execute(format!("CREATE SCHEMA \"{schema}\"").as_str())
        .await
        .expect("cannot create the test schema");
    bootstrap.close().await;

    // The real pool: every connection lands inside the schema, so `sqlx`'s
    // migrations create `_sqlx_migrations` and all tables there.
    let options: PgConnectOptions = url
        .parse()
        .expect("test database URL must be a valid PostgreSQL URL");
    let options = options.options([("search_path", schema.as_str())]);

    let db = Database::connect_with_options(
        options,
        max_connections,
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
        url,
        db,
        admin: admin_pool().await,
    }
}

/// A migrated schema that cleans itself up.
pub struct TestDatabase {
    schema: String,
    url: String,
    db: Database,
    admin: PgPool,
}

impl TestDatabase {
    /// The migrated database.
    pub fn db(&self) -> &Database {
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

    /// The connection URL (the shared database, not the schema).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Drop the schema. Called explicitly at the end of a test.
    pub async fn cleanup(self) {
        let schema = self.schema.clone();
        self.db.close().await;
        // An ordinary DDL statement: no backend is signalled, nothing outside this
        // schema is touched.
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
}

/// Skip the current test when no database is reachable, printing the reason.
#[macro_export]
macro_rules! require_database {
    () => {
        if !$crate::common::database_available().await {
            eprintln!("skipping: no PostgreSQL reachable");
            return;
        }
    };
}
