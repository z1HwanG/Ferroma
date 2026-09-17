//! The local SQLite cache (specification §26).
//!
//! `Server = Source of Truth; Client SQLite = Local Cache.` Everything in this
//! module exists to make the client usable offline and fast online — never to
//! outlive or contradict the server.
//!
//! # Schema map
//!
//! The full DDL lives in `migrations/0001_init.sql`, with a "which column serves
//! which query" note on every table. In brief:
//!
//! | Table | Serves |
//! |---|---|
//! | `accounts` | the account list, the refresh token, the pause flag, the §52 sync window |
//! | `mailboxes` | the addresses an account may send from (§4) |
//! | `folders` | the sidebar, `special_use` mapping, `uid_validity` invalidation |
//! | `messages` | the list view (folder + date), unread counts, UID lookup, tombstones |
//! | `message_headers` | the raw header list in the reading pane |
//! | `message_bodies` | lazily downloaded text/html/raw bodies |
//! | `attachments` | attachment metadata plus a pointer to the blob cache |
//! | `blob_cache` | the on-disk, content-addressed attachment cache + LRU eviction |
//! | `drafts` | offline-first drafts, `dirty` = not yet pushed (§7) |
//! | `outbox` | the eight-state send pipeline (§28) |
//! | `pending_operations` | the offline FIFO queue (§27) |
//! | `sync_state` | one cursor per stream — the heart of §3 |
//! | `devices` | the device list (§9) |
//! | `settings` | the settings surface (§52), one JSON blob per section |
//! | `notifications` | notifications the shell has not shown yet (§34) |
//! | `messages_fts` | FTS5 index for local search (§30), with a `LIKE` fallback |
//!
//! # Pragmas
//!
//! * `journal_mode = WAL` — a reader (the UI) never blocks the sync writer.
//! * `foreign_keys = ON` — deleting an account or a message really does cascade.
//! * `busy_timeout` — two client processes (or the UI and the sync task) wait
//!   instead of failing with `SQLITE_BUSY`.
//!
//! # FTS5 is optional
//!
//! A SQLite built without FTS5 cannot create `messages_fts`. That is detected at
//! open time: the client logs a warning, records the fact in `settings`, and
//! search falls back to `LIKE` over `messages`/`message_bodies`. **Opening the
//! database never fails because of a missing search extension.**

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::error::{ClientError, ClientResult};

/// How locally-cached mail is searched (§30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// A real FTS5 index exists; queries use `MATCH` with ranking.
    Fts5,
    /// No FTS5 in the linked SQLite: fall back to `LIKE '%term%'` scans.
    Like,
}

impl SearchMode {
    /// The string stored in `settings` so the fallback survives a restart.
    pub fn as_str(self) -> &'static str {
        match self {
            SearchMode::Fts5 => "fts5",
            SearchMode::Like => "like",
        }
    }
}

/// One row of the local search index.
///
/// It is a *denormalised* projection of a message: everything search can match on
/// lives here, so a query never has to join five tables to answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchDocument {
    /// The account the message belongs to.
    pub account_id: i64,
    /// The server-side message id.
    pub message_id: i64,
    /// The folder the message currently lives in.
    pub folder_id: Option<i64>,
    /// `Subject:` header.
    pub subject: String,
    /// Sender address (and display name), as one string.
    pub sender: String,
    /// Comma-joined recipient addresses.
    pub recipients: String,
    /// Plain-text body, when it has been downloaded.
    pub body: String,
    /// Attachment file names, space separated.
    pub attachments: String,
}

/// Settings key under which the detected search mode is recorded.
const KEY_SEARCH_MODE: &str = "cache.search_mode";

/// The client's SQLite cache.
///
/// Cheap to clone by reference; wrap it in an `Arc` to share it between the sync
/// engine, the UI seam and the CLI.
#[derive(Debug, Clone)]
pub struct ClientDatabase {
    pool: SqlitePool,
    path: Option<PathBuf>,
    mode: SearchMode,
    /// `true` when [`SearchMode::Like`] was forced by the caller rather than
    /// detected — used by tests to exercise the degradation path on a SQLite
    /// that *does* have FTS5.
    forced_mode: bool,
}

impl ClientDatabase {
    /// Open (creating if necessary) the cache at `path` and migrate it.
    ///
    /// Parent directories are created. The file is put in WAL mode.
    pub async fn open(path: impl AsRef<Path>) -> ClientResult<Self> {
        Self::open_inner(Some(path.as_ref().to_path_buf()), None).await
    }

    /// Open a private, throw-away database in memory.
    ///
    /// The pool is limited to a single connection: every `:memory:` connection
    /// would otherwise get its own empty database.
    pub async fn open_in_memory() -> ClientResult<Self> {
        Self::open_inner(None, None).await
    }

    /// Open the cache, forcing the search backend instead of detecting it.
    ///
    /// This exists for tests (and for a support flag): it lets the `LIKE`
    /// degradation path be exercised on a SQLite that does ship FTS5.
    pub async fn open_with_search_mode(
        path: impl AsRef<Path>,
        mode: SearchMode,
    ) -> ClientResult<Self> {
        Self::open_inner(Some(path.as_ref().to_path_buf()), Some(mode)).await
    }

    async fn open_inner(path: Option<PathBuf>, forced: Option<SearchMode>) -> ClientResult<Self> {
        let mut options = match &path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)?;
                    }
                }
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Normal)
            }
            None => SqliteConnectOptions::from_str("sqlite::memory:")
                .map_err(|e| ClientError::cache(format!("in-memory sqlite options: {e}")))?,
        };

        options = options
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
            .pragma("cache_size", "-8000");

        let max_connections = if path.is_some() { 8 } else { 1 };

        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await?;

        let mut db = ClientDatabase {
            pool,
            path,
            mode: SearchMode::Fts5,
            forced_mode: forced.is_some(),
        };
        db.migrate().await?;
        db.mode = db.resolve_search_mode(forced).await?;
        db.ensure_search_index().await?;
        db.record_search_mode().await?;
        Ok(db)
    }

    /// The underlying pool, for modules that need to run their own SQL.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The file this cache lives in, or `None` for an in-memory database.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Which search backend is active.
    pub fn search_mode(&self) -> SearchMode {
        self.mode
    }

    /// Whether the linked SQLite can create FTS5 virtual tables.
    pub fn fts5_available(&self) -> bool {
        self.mode == SearchMode::Fts5
    }

    /// Run the embedded migrations. Idempotent: `sqlx` records applied versions
    /// in `_sqlx_migrations`, so a second call is a no-op.
    pub async fn migrate(&self) -> ClientResult<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|e| ClientError::cache(format!("migration failed: {e}")))?;
        Ok(())
    }

    /// Decide whether FTS5 works, honouring a forced mode.
    async fn resolve_search_mode(&self, forced: Option<SearchMode>) -> ClientResult<SearchMode> {
        // Even when forced, still probe: a forced `Fts5` on a build without the
        // extension must degrade rather than fail every query.
        let available = self.probe_fts5().await?;
        let mode = match forced {
            Some(SearchMode::Like) => SearchMode::Like,
            Some(SearchMode::Fts5) => {
                if available {
                    SearchMode::Fts5
                } else {
                    SearchMode::Like
                }
            }
            None => {
                if available {
                    SearchMode::Fts5
                } else {
                    SearchMode::Like
                }
            }
        };
        if mode == SearchMode::Like && !self.forced_mode {
            tracing::warn!(
                "this SQLite build has no FTS5 support; local search falls back to LIKE scans"
            );
        }
        Ok(mode)
    }

    /// Try to create an FTS5 virtual table in a scratch transaction.
    async fn probe_fts5(&self) -> ClientResult<bool> {
        let mut conn = self.pool.acquire().await?;
        let probe = sqlx::query("CREATE VIRTUAL TABLE IF NOT EXISTS __ferroma_fts5_probe USING fts5(x)")
            .execute(&mut *conn)
            .await;
        match probe {
            Ok(_) => {
                let _ = sqlx::query("DROP TABLE IF EXISTS __ferroma_fts5_probe")
                    .execute(&mut *conn)
                    .await;
                Ok(true)
            }
            Err(err) => {
                tracing::debug!(error = %err, "FTS5 probe failed");
                Ok(false)
            }
        }
    }

    /// Upgrade a previously-`Like` database when FTS5 turns out to be available,
    /// and (re)create the index tables.
    async fn ensure_search_index(&self) -> ClientResult<()> {
        let mut conn = self.pool.acquire().await?;
        // Drop a stale probe table from an interrupted run.
        let _ = sqlx::query("DROP TABLE IF EXISTS __ferroma_fts5_probe")
            .execute(&mut *conn)
            .await;

        if self.mode == SearchMode::Fts5 {
            sqlx::query(
                "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                     subject, sender, recipients, body, attachments,
                     account_id UNINDEXED, message_id UNINDEXED,
                     tokenize = 'unicode61 remove_diacritics 2'
                 )",
            )
            .execute(&mut *conn)
            .await
            .map_err(|e| ClientError::cache(format!("creating the FTS5 index failed: {e}")))?;
        }
        Ok(())
    }

    /// Persist the resolved mode so a support engineer can see why search is slow.
    async fn record_search_mode(&self) -> ClientResult<()> {
        self.put_setting(KEY_SEARCH_MODE, self.mode.as_str()).await
    }

    /// Read a `settings` row.
    pub async fn get_setting(&self, key: &str) -> ClientResult<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// Write a `settings` row (upsert).
    pub async fn put_setting(&self, key: &str, value: &str) -> ClientResult<()> {
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(crate::util::now_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every table (and virtual table) in the cache, sorted.
    pub async fn table_names(&self) -> ClientResult<Vec<String>> {
        let rows = sqlx::query(
            "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut names: Vec<String> = rows.into_iter().map(|r| r.get::<String, _>("name")).collect();
        names.sort();
        Ok(names)
    }

    /// The value of a SQLite pragma, as text.
    pub async fn pragma(&self, name: &str) -> ClientResult<String> {
        // The pragma name cannot be bound as a parameter; it comes from our own
        // code, never from the network or the user, and is whitelisted here.
        let allowed = ["journal_mode", "foreign_keys", "busy_timeout", "synchronous"];
        if !allowed.contains(&name) {
            return Err(ClientError::invalid(format!("pragma {name} is not whitelisted")));
        }
        let row = sqlx::query(&format!("PRAGMA {name}"))
            .fetch_one(&self.pool)
            .await?;
        // SQLite is dynamically typed and `query` (unlike `query_as`) cannot say
        // what the column type is; these four pragmas are known to us.
        Ok(if name == "journal_mode" {
            row.try_get::<String, _>(0).unwrap_or_default()
        } else {
            row.try_get::<i64, _>(0)
                .map(|v| v.to_string())
                .unwrap_or_default()
        })
    }

    /// Delete every cached row that belongs to `account_id`.
    ///
    /// Used when the user removes an account or asks for a cache wipe. The
    /// `ON DELETE CASCADE` clauses do most of the work; the content-addressed
    /// blob files are left to the attachment cache's eviction.
    pub async fn wipe_account(&self, account_id: i64) -> ClientResult<()> {
        let mut tx = self.pool.begin().await?;
        if self.mode == SearchMode::Fts5 {
            let _ = sqlx::query("DELETE FROM messages_fts WHERE account_id = ?")
                .bind(account_id.to_string())
                .execute(&mut *tx)
                .await;
        }
        // `ON DELETE CASCADE` clears mailboxes, folders, messages, headers,
        // bodies, attachments, drafts, outbox, pending operations, sync state and
        // devices.
        sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The number of cached rows in `table` for an account, for the "storage"
    /// pane of the settings surface and for the tests.
    pub async fn count_account_rows(&self, table: &str, account_id: i64) -> ClientResult<i64> {
        const ALLOWED: [&str; 9] = [
            "mailboxes",
            "folders",
            "messages",
            "attachments",
            "drafts",
            "outbox",
            "pending_operations",
            "devices",
            "sync_state",
        ];
        if !ALLOWED.contains(&table) {
            return Err(ClientError::invalid(format!("table {table} is not account-scoped")));
        }
        let row = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table} WHERE account_id = ?"))
            .bind(account_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    /// Close the pool, flushing WAL contents into the main database file.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Run `query` on a connection or transaction.
///
/// The sync engine needs to run the *same* statements inside a transaction and
/// outside one; this alias keeps that switch explicit at the call site.
pub type DbConnection = SqliteConnection;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    async fn open_temp() -> (TempDir, ClientDatabase) {
        let dir = TempDir::new().expect("temp dir");
        let db = ClientDatabase::open(dir.path().join("cache.db"))
            .await
            .expect("open");
        (dir, db)
    }

    #[tokio::test]
    async fn open_creates_the_file_and_every_table() {
        let (_dir, db) = open_temp().await;
        let tables = db.table_names().await.expect("tables");
        for expected in [
            "accounts",
            "mailboxes",
            "folders",
            "messages",
            "message_headers",
            "message_bodies",
            "attachments",
            "blob_cache",
            "drafts",
            "outbox",
            "pending_operations",
            "sync_state",
            "devices",
            "settings",
            "notifications",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}; got {tables:?}"
            );
        }
    }

    #[tokio::test]
    async fn wal_is_enabled_on_a_file_database() {
        let (_dir, db) = open_temp().await;
        let mode = db.pragma("journal_mode").await.expect("pragma");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn foreign_keys_are_enabled() {
        let (_dir, db) = open_temp().await;
        assert_eq!(db.pragma("foreign_keys").await.expect("pragma"), "1");
        assert_eq!(db.pragma("busy_timeout").await.expect("pragma"), "5000");
    }

    #[tokio::test]
    async fn migrate_is_idempotent_and_survives_a_reopen() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("cache.db");
        {
            let db = ClientDatabase::open(&path).await.expect("open");
            db.migrate().await.expect("second migrate");
            db.put_setting("ui.theme", "dark").await.expect("set");
        }
        let db = ClientDatabase::open(&path).await.expect("reopen");
        db.migrate().await.expect("third migrate");
        assert_eq!(
            db.get_setting("ui.theme").await.expect("get"),
            Some("dark".to_string())
        );
        let tables = db.table_names().await.expect("tables");
        assert_eq!(
            tables.iter().filter(|t| t.as_str() == "accounts").count(),
            1,
            "reopening must not duplicate the schema"
        );
    }

    #[tokio::test]
    async fn in_memory_database_works_and_has_the_schema() {
        let db = ClientDatabase::open_in_memory().await.expect("open");
        let tables = db.table_names().await.expect("tables");
        assert!(tables.iter().any(|t| t == "messages"));
        assert!(db.path().is_none());
    }

    #[tokio::test]
    async fn foreign_keys_cascade_from_an_account() {
        let (_dir, db) = open_temp().await;
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (1, 'a@example.com', 'http://x/api/v1/client', 'dev', 'now', 'now')",
        )
        .execute(db.pool())
        .await
        .expect("insert account");
        sqlx::query(
            "INSERT INTO folders (account_id, id, mailbox_id, name) VALUES (1, 5, 3, 'INBOX')",
        )
        .execute(db.pool())
        .await
        .expect("insert folder");

        sqlx::query("DELETE FROM accounts WHERE id = 1")
            .execute(db.pool())
            .await
            .expect("delete account");
        let remaining: i64 = sqlx::query("SELECT COUNT(*) AS n FROM folders")
            .fetch_one(db.pool())
            .await
            .expect("count")
            .get("n");
        assert_eq!(remaining, 0, "deleting an account must clear its cache");
    }

    #[tokio::test]
    async fn foreign_keys_reject_an_orphan_row() {
        let (_dir, db) = open_temp().await;
        let err = sqlx::query(
            "INSERT INTO folders (account_id, id, mailbox_id, name) VALUES (99, 5, 3, 'INBOX')",
        )
        .execute(db.pool())
        .await;
        assert!(err.is_err(), "a folder without an account must be refused");
    }

    #[tokio::test]
    async fn search_mode_is_recorded_in_settings() {
        let (_dir, db) = open_temp().await;
        let recorded = db
            .get_setting(KEY_SEARCH_MODE)
            .await
            .expect("setting")
            .expect("present");
        assert_eq!(recorded, db.search_mode().as_str());
    }

    #[tokio::test]
    async fn forced_like_mode_still_opens_and_does_not_create_the_fts_table() {
        let dir = TempDir::new().expect("temp dir");
        let db = ClientDatabase::open_with_search_mode(dir.path().join("c.db"), SearchMode::Like)
            .await
            .expect("open");
        assert_eq!(db.search_mode(), SearchMode::Like);
        assert!(!db.fts5_available());
        let tables = db.table_names().await.expect("tables");
        assert!(!tables.iter().any(|t| t == "messages_fts"));
        // The rest of the schema is intact.
        assert!(tables.iter().any(|t| t == "messages"));
    }

    #[tokio::test]
    async fn unwhitelisted_pragma_is_rejected() {
        let (_dir, db) = open_temp().await;
        let err = db.pragma("database_list; DROP TABLE messages").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn wipe_account_removes_only_that_account() {
        let (_dir, db) = open_temp().await;
        for id in [1i64, 2] {
            sqlx::query(
                "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
                 VALUES (?, ?, 'http://x/api/v1/client', 'dev', 'now', 'now')",
            )
            .bind(id)
            .bind(format!("a{id}@example.com"))
            .execute(db.pool())
            .await
            .expect("account");
            sqlx::query(
                "INSERT INTO folders (account_id, id, mailbox_id, name) VALUES (?, 5, 3, 'INBOX')",
            )
            .bind(id)
            .execute(db.pool())
            .await
            .expect("folder");
        }
        db.wipe_account(1).await.expect("wipe");
        assert_eq!(db.count_account_rows("folders", 1).await.expect("count"), 0);
        assert_eq!(db.count_account_rows("folders", 2).await.expect("count"), 1);
    }

    #[tokio::test]
    async fn account_scoped_counts_refuse_unknown_tables() {
        let (_dir, db) = open_temp().await;
        assert!(db.count_account_rows("settings", 1).await.is_err());
    }
}
