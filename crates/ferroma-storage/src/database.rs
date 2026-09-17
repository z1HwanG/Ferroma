//! The PostgreSQL pool and its lifecycle.
//!
//! One [`Database`] is shared by every subsystem in the server process. It owns the
//! `sqlx` pool, applies the embedded migrations and hands out repositories.
//!
//! # Migrations are compiled in
//!
//! `sqlx::migrate!("../../migrations")` embeds the SQL at build time, so a release
//! binary (or a scratch Docker image) can migrate a database without shipping the
//! `migrations/` directory.
//!
//! # Runtime-checked queries
//!
//! The crate deliberately avoids `sqlx::query!` macros: they require a live database
//! (or a checked-in `.sqlx` offline cache) at *compile* time, which would make a
//! clean `cargo build` impossible on a machine without PostgreSQL. All queries go
//! through `sqlx::query_as` with `FromRow` row structs instead, and the schema is
//! verified by the integration tests in `tests/`.

use std::str::FromStr;
use std::time::Duration;

use ferroma_core::config::DatabaseConfig;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;

use crate::error::{Result, StorageError};

/// The embedded migration set.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// A connection pool plus the operations that belong to the database as a whole.
#[derive(Clone)]
pub struct Database {
    pool: PgPool,
    /// The URL the pool was opened with, when it was opened from one. Integration
    /// tests open the pool from explicit `PgConnectOptions` (to pin a `search_path`)
    /// and get a synthesised description instead.
    url: Option<String>,
    statement_logging: bool,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the URL: it contains the password.
        f.debug_struct("Database")
            .field("pool_size", &self.pool.size())
            .field("statement_logging", &self.statement_logging)
            .finish()
    }
}

impl Database {
    /// Connect using the `[database]` configuration block.
    ///
    /// Connection establishment is bounded by `acquire_timeout_secs`: a single
    /// `acquire_timeout` covers both waiting for a free pooled connection and
    /// opening a brand-new one, which is why there is no separate connect timeout.
    pub async fn connect(config: &DatabaseConfig) -> Result<Self> {
        Self::connect_with(
            &config.url,
            config.max_connections,
            config.min_connections,
            Duration::from_secs(config.acquire_timeout_secs),
            Duration::from_secs(config.idle_timeout_secs),
            Duration::from_secs(config.max_lifetime_secs),
            config.log_statements,
        )
        .await
    }

    /// Connect with explicit pool tuning. Used by [`Database::connect`] and tests.
    pub async fn connect_with(
        url: &str,
        max_connections: u32,
        min_connections: u32,
        acquire_timeout: Duration,
        idle_timeout: Duration,
        max_lifetime: Duration,
        statement_logging: bool,
    ) -> Result<Self> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|e| StorageError::Invalid(format!("invalid database URL: {e}")))?
            .application_name("ferroma")
            .ssl_mode(PgSslMode::Prefer)
            .statement_cache_capacity(100);

        let mut db = Self::connect_with_options(
            options,
            max_connections,
            min_connections,
            acquire_timeout,
            idle_timeout,
            max_lifetime,
            statement_logging,
        )
        .await
        .map_err(|e| match e {
            StorageError::Invalid(message) if message.starts_with("cannot connect") => {
                StorageError::Invalid(format!(
                    "cannot connect to PostgreSQL at {}: {}",
                    redact(url),
                    message.trim_start_matches("cannot connect: ")
                ))
            }
            other => other,
        })?;
        db.url = Some(url.to_string());
        Ok(db)
    }

    /// Connect from pre-built options.
    ///
    /// The integration tests use this to pin a per-test `search_path`; anything that
    /// needs a connection setting sqlx does not expose through a URL does too.
    pub async fn connect_with_options(
        options: PgConnectOptions,
        max_connections: u32,
        min_connections: u32,
        acquire_timeout: Duration,
        idle_timeout: Duration,
        max_lifetime: Duration,
        statement_logging: bool,
    ) -> Result<Self> {
        let options = options
            .application_name("ferroma")
            .statement_cache_capacity(100);

        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .min_connections(min_connections.min(max_connections.max(1)))
            .acquire_timeout(acquire_timeout)
            .idle_timeout(Some(idle_timeout))
            .max_lifetime(Some(max_lifetime))
            // A pooled connection can be killed by the database restarting; verify
            // before handing it out rather than failing the caller's first query.
            .test_before_acquire(true)
            .connect_with(options)
            .await
            .map_err(|e| StorageError::Invalid(format!("cannot connect: {e}")))?;

        Ok(Database {
            pool,
            url: None,
            statement_logging,
        })
    }

    /// The underlying pool, for repositories and for `sqlx` transactions.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The connection this pool was opened against, with the password replaced by
    /// `***`. Falls back to `host:port/database` when the pool was opened from
    /// explicit options rather than a URL.
    pub fn redacted_url(&self) -> String {
        match &self.url {
            Some(url) => redact(url),
            None => {
                let options = self.pool.connect_options();
                format!(
                    "postgres://{}:{}/{}",
                    options.get_host(),
                    options.get_port(),
                    options.get_database().unwrap_or("<default>")
                )
            }
        }
    }

    /// Whether SQL statement logging is enabled.
    pub fn statement_logging(&self) -> bool {
        self.statement_logging
    }

    /// Apply every pending migration. Idempotent.
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// How many migrations are defined, and how many are already applied.
    pub async fn migration_status(&self) -> Result<(usize, usize)> {
        let total = MIGRATOR.migrations.len();
        let (applied,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success")
                .fetch_one(&self.pool)
                .await?;
        Ok((total, applied as usize))
    }

    /// Round-trip a trivial query. Used by the HTTP `/health` endpoint.
    pub async fn health(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// The PostgreSQL server version string, e.g. `PostgreSQL 16.15`.
    pub async fn server_version(&self) -> Result<String> {
        let (version,): (String,) = sqlx::query_as("SELECT version()").fetch_one(&self.pool).await?;
        Ok(version.split(" on ").next().unwrap_or(&version).to_string())
    }

    /// Size of the database on disk, in bytes.
    pub async fn size_bytes(&self) -> Result<i64> {
        let (size,): (i64,) =
            sqlx::query_as("SELECT pg_database_size(current_database())::bigint")
                .fetch_one(&self.pool)
                .await?;
        Ok(size)
    }

    /// Pool statistics, for the Admin dashboard.
    pub fn pool_stats(&self) -> PoolStats {
        PoolStats {
            size: self.pool.size(),
            idle: self.pool.num_idle() as u32,
            max: self.pool.options().get_max_connections(),
        }
    }

    /// Close the pool, waiting for in-flight queries to finish.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Repositories bound to this database.
    pub fn repositories(&self) -> crate::repository::Repositories {
        crate::repository::Repositories::new(self.pool.clone())
    }
}

/// A snapshot of pool utilisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PoolStats {
    /// Total connections currently open.
    pub size: u32,
    /// Connections sitting idle.
    pub idle: u32,
    /// Configured maximum.
    pub max: u32,
}

impl PoolStats {
    /// Connections actively running a query.
    pub fn in_use(&self) -> u32 {
        self.size.saturating_sub(self.idle)
    }

    /// Utilisation as a percentage of `max`.
    pub fn utilisation(&self) -> f64 {
        if self.max == 0 {
            0.0
        } else {
            f64::from(self.size) * 100.0 / f64::from(self.max)
        }
    }
}

/// Replace the password in a PostgreSQL URL with `***`.
pub fn redact(url: &str) -> String {
    // postgres://user:password@host/db  ->  postgres://user:***@host/db
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after_scheme = scheme_end + 3;
    let authority_end = url[after_scheme..]
        .find('/')
        .map(|i| after_scheme + i)
        .unwrap_or(url.len());
    let authority = &url[after_scheme..authority_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return url.to_string();
    };
    format!(
        "{}{}:***{}",
        &url[..after_scheme],
        &userinfo[..colon],
        &url[after_scheme + at..]
    )
}

/// Split a PostgreSQL URL into `(everything before the database name, the name,
/// the query string)`.
///
/// Deliberately hand-rolled rather than pulling in a URL parser: the only URLs this
/// has to understand are the ones PostgreSQL itself accepts, and the shape is fixed.
fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let scheme_end = url.find("://")?;
    let after_scheme = scheme_end + 3;
    let path_start = url[after_scheme..].find('/')? + after_scheme;
    let rest = &url[path_start + 1..];
    let (name, suffix) = match rest.find('?') {
        Some(q) => (&rest[..q], &rest[q..]),
        None => (rest, ""),
    };
    Some((&url[..path_start + 1], name, suffix))
}

impl Database {
    /// The database name a PostgreSQL URL points at.
    pub fn database_name(url: &str) -> Result<String> {
        let (_, name, _) = split_url(url)
            .ok_or_else(|| StorageError::Invalid(format!("not a PostgreSQL URL: {}", redact(url))))?;
        if name.is_empty() {
            return Err(StorageError::Invalid(format!(
                "{} has no database name; append one, e.g. /ferroma",
                redact(url)
            )));
        }
        Ok(name.to_string())
    }

    /// The same URL pointed at a database that always exists, so a missing target
    /// database can be created.
    pub fn maintenance_url(url: &str) -> Result<String> {
        let (prefix, _, suffix) = split_url(url)
            .ok_or_else(|| StorageError::Invalid(format!("not a PostgreSQL URL: {}", redact(url))))?;
        Ok(format!("{prefix}postgres{suffix}"))
    }

    /// Create the database this URL points at, if it does not exist.
    ///
    /// Returns `true` when it created one. This is what turns the most common
    /// first-run failure — `database "ferroma" does not exist` — into something the
    /// operator can fix with one command instead of reaching for `psql`.
    ///
    /// Connects to the `postgres` maintenance database, because you cannot create the
    /// database you are connected to.
    pub async fn ensure_database_exists(url: &str) -> Result<bool> {
        let name = Self::database_name(url)?;
        if name == "postgres" {
            return Ok(false);
        }
        let maintenance = Self::maintenance_url(url)?;

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&maintenance)
            .await
            .map_err(|e| {
                StorageError::Invalid(format!(
                    "cannot reach the PostgreSQL server at {} to create database {name}: {e}",
                    redact(&maintenance)
                ))
            })?;

        let existing: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(&name)
            .fetch_optional(&pool)
            .await?;

        let created = if existing.is_some() {
            false
        } else {
            // No `WITH (FORCE)` anywhere: dropping or forcing is not what this does,
            // and forcing requires signalling other backends.
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&pool)
                .await?;
            true
        };

        pool.close().await;
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_hides_the_password() {
        assert_eq!(
            redact("postgres://ferroma:s3cret@localhost:5432/ferroma"),
            "postgres://ferroma:***@localhost:5432/ferroma"
        );
        assert_eq!(
            redact("postgres://ferroma:s3cret@db:5432/ferroma?sslmode=disable"),
            "postgres://ferroma:***@db:5432/ferroma?sslmode=disable"
        );
        // No credentials at all: unchanged.
        assert_eq!(
            redact("postgres://ferroma@localhost/ferroma"),
            "postgres://ferroma@localhost/ferroma"
        );
        assert_eq!(redact("not-a-url"), "not-a-url");
    }

    #[test]
    fn the_database_name_is_extracted() {
        assert_eq!(
            Database::database_name("postgres://ferroma:pw@localhost:5432/ferroma").unwrap(),
            "ferroma"
        );
        assert_eq!(
            Database::database_name("postgres://ferroma@db/ferroma?sslmode=require").unwrap(),
            "ferroma"
        );
        assert_eq!(
            Database::database_name("postgresql://u@h:5433/ferroma_dev").unwrap(),
            "ferroma_dev"
        );

        // A URL with no database is a configuration mistake worth naming.
        let err = Database::database_name("postgres://ferroma@localhost:5432/").unwrap_err();
        assert!(format!("{err}").contains("no database name"), "{err}");
        assert!(Database::database_name("mysql://nope").is_err());
    }

    #[test]
    fn the_maintenance_url_swaps_only_the_database() {
        assert_eq!(
            Database::maintenance_url("postgres://ferroma:pw@localhost:5432/ferroma").unwrap(),
            "postgres://ferroma:pw@localhost:5432/postgres"
        );
        // The query string survives: `sslmode` must still apply to the maintenance
        // connection, or creating the database fails for a different reason than the
        // one the operator is trying to fix.
        assert_eq!(
            Database::maintenance_url("postgres://ferroma@db/ferroma?sslmode=require").unwrap(),
            "postgres://ferroma@db/postgres?sslmode=require"
        );
    }

    #[test]
    fn pool_stats_arithmetic() {
        let s = PoolStats { size: 10, idle: 4, max: 20 };
        assert_eq!(s.in_use(), 6);
        assert!((s.utilisation() - 50.0).abs() < f64::EPSILON);
        assert_eq!(PoolStats { size: 0, idle: 0, max: 0 }.utilisation(), 0.0);
    }

    #[test]
    fn migrations_are_embedded() {
        // The build would fail if `migrations/` were missing; this guards against
        // someone deleting 0001_initial.sql and shipping an empty schema.
        assert!(!MIGRATOR.migrations.is_empty(), "no migrations embedded");
        assert_eq!(MIGRATOR.migrations[0].version, 1);
    }

    #[test]
    fn invalid_url_is_rejected_before_touching_the_network() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(Database::connect_with(
                "mysql://nope",
                1,
                0,
                Duration::from_millis(50),
                Duration::from_secs(1),
                Duration::from_secs(1),
                false,
            ))
            .unwrap_err();
        assert!(matches!(err, StorageError::Invalid(_)), "{err:?}");
    }
}
