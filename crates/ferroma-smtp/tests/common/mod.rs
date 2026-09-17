//! Shared helpers for `ferroma-smtp` integration tests.
//!
//! These tests drive a **real** listener over a **real** socket against a **real**
//! PostgreSQL schema and a **real** Maildir. Nothing here is a mock: if the SMTP
//! reply, the stored bytes or the database row is wrong, the test fails.
//!
//! # One schema per test
//!
//! Each test creates a PostgreSQL **schema** and pins it on the pool with `sqlx`'s
//! `PgConnectOptions::options([("search_path", …)])`, so every connection the pool
//! opens — including the migration runs — lands inside that schema. That is the same
//! isolation `ferroma-storage`'s own `tests/common/mod.rs` uses, and it is why `sqlx`
//! is a **dev-dependency** of this crate: nothing in `src/` may use it, but
//! `ferroma-storage` neither re-exports it nor exposes a way to run DDL, so an
//! integration test cannot create its own schema without naming the crate.
//!
//! Teardown is an ordinary `DROP SCHEMA … CASCADE` over the administrative
//! connection — never `DROP DATABASE`, and never anything that signals another
//! backend (see `AGENTS.md` §2.1).
//!
//! Everything else goes through [`ferroma_storage::Database`], which is the only
//! handle this crate has on PostgreSQL — and which is also the point: the tests
//! exercise the same API the server does.
//!
//! # Skipping
//!
//! An unreachable database **fails** the suite rather than skipping it: a skipped
//! test that the harness counts as a pass is a hollow green, and an integration suite
//! that quietly tests nothing is worse than no suite at all. The operator opts into
//! skipping explicitly:
//!
//! ```text
//! FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 cargo test -p ferroma-smtp
//! ```

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferroma_core::config::{Config, MailboxLayout};
use ferroma_core::{MailboxId, UserId};
use ferroma_storage::{AttachmentStore, Database, Maildir, Repositories};
use sqlx::postgres::PgConnectOptions;
use sqlx::Executor;

/// The shared integration-test database.
pub const TEST_DATABASE: &str = "ferroma_test";

/// The administrative connection URL, pointing at the maintenance database.
pub fn admin_url() -> String {
    std::env::var("FERROMA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferroma@127.0.0.1:5433/postgres".to_string())
}

/// Swap the database name in a PostgreSQL URL, keeping any query string.
fn with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.find('?') {
        Some(index) => (&url[..index], &url[index..]),
        None => (url, ""),
    };
    match base.rfind('/') {
        Some(index) => format!("{}{database}{query}", &base[..=index]),
        None => format!("{base}/{database}{query}"),
    }
}

/// The URL of the shared integration-test database.
pub fn test_database_url() -> String {
    with_database(&admin_url(), TEST_DATABASE)
}

/// Connect with a short timeout, retrying for a few seconds.
///
/// `scripts/pg-supervisor.ps1` may be mid-restart when a suite starts, so a single
/// failed connect is not proof that no database exists.
async fn try_connect(url: &str, attempts: u32) -> Option<Database> {
    for attempt in 0..attempts {
        match Database::connect_with(
            url,
            2,
            0,
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(30),
            false,
        )
        .await
        {
            Ok(db) => return Some(db),
            Err(_) if attempt + 1 < attempts => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(_) => return None,
        }
    }
    None
}

/// Whether an integration database is reachable.
pub async fn have_database() -> bool {
    let Some(db) = try_connect(&admin_url(), 6).await else {
        return false;
    };
    db.close().await;
    true
}

/// Whether the operator explicitly asked for a run without a database.
pub fn skipping_is_allowed() -> bool {
    matches!(
        std::env::var("FERROMA_TEST_SKIP_WITHOUT_DATABASE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Ensure a database is reachable, or stop the test.
///
/// An unreachable database **fails** the test unless
/// `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1` is set, because a silently skipped
/// integration test is a passing test that verified nothing.
#[macro_export]
macro_rules! require_database {
    () => {
        if !$crate::common::have_database().await {
            if $crate::common::skipping_is_allowed() {
                eprintln!(
                    "FERROMA_TEST_SKIP_WITHOUT_DATABASE is set: skipping (no PostgreSQL at {})",
                    $crate::common::admin_url()
                );
                return;
            }
            panic!(
                "no PostgreSQL reachable at {} — start it with scripts/dev-postgres.ps1, \
                 or set FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 to skip these tests",
                $crate::common::admin_url()
            );
        }
    };
}

/// Run one statement through a pool.
async fn execute(db: &Database, sql: &str) -> Result<(), String> {
    db.pool()
        .execute(sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A migrated schema that cleans itself up.
pub struct TestDb {
    schema: String,
    db: Database,
    /// An administrative connection **to the shared test database** — schemas are
    /// database-local, so the one that creates and drops them must live in the same
    /// database as the pool under test, not in the maintenance database.
    admin: Database,
}

/// How many connections one test's pool opens.
///
/// Deliberately small: cargo runs test binaries in parallel and every test opens its
/// own pool, so a generous pool per test exhausts PostgreSQL's `max_connections` and
/// shows up as an unrelated timeout in whichever suite happened to run first. A test
/// that genuinely needs more can ask for it with [`TestDb::create_with_pool`].
pub const TEST_POOL_SIZE: u32 = 2;

impl TestDb {
    /// Create the shared database (if needed), a schema, and the migrated pool.
    pub async fn create() -> TestDb {
        TestDb::create_with_pool(TEST_POOL_SIZE).await
    }

    /// [`TestDb::create`] with an explicit pool size.
    pub async fn create_with_pool(pool_size: u32) -> TestDb {
        // The maintenance connection: only `CREATE DATABASE` runs here, because a
        // database cannot be created from inside itself.
        let maintenance = try_connect(&admin_url(), 20)
            .await
            .expect("cannot connect to the test PostgreSQL — is it running?");

        // The shared database is created once and never dropped: dropping an in-use
        // database is what broke the storage suite (and `DROP DATABASE … WITH (FORCE)`
        // is forbidden outright on this host, because it makes PostgreSQL signal its
        // checkpointer).
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(TEST_DATABASE)
            .fetch_optional(maintenance.pool())
            .await
            .unwrap_or(None);
        if exists.is_none() {
            // On a fresh cluster every test sees "no such database" at the same
            // instant, so exactly one `CREATE DATABASE` wins and the losers get
            // `42P04 duplicate_database`. That is success, not failure — which is why
            // the error is ignored and the outcome is verified afterwards.
            let _ = execute(
                &maintenance,
                &format!("CREATE DATABASE \"{TEST_DATABASE}\""),
            )
            .await;
            let created: Option<(i32,)> =
                sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
                    .bind(TEST_DATABASE)
                    .fetch_optional(maintenance.pool())
                    .await
                    .unwrap_or(None);
            assert!(
                created.is_some(),
                "the shared test database {TEST_DATABASE} could not be created"
            );
        }
        maintenance.close().await;

        // Everything from here happens *inside* the shared test database.
        let admin = try_connect(&test_database_url(), 20)
            .await
            .expect("cannot connect to the shared test database");

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let schema = format!("smtp_{}", &suffix[..20]);

        execute(&admin, &format!("CREATE SCHEMA \"{schema}\""))
            .await
            .expect("cannot create the test schema");

        // Pin the schema through sqlx's connection options, the same way
        // `ferroma-storage`'s helper does. `search_path` is applied to *every*
        // connection the pool opens, so the migrations and every later query land in
        // this test's schema.
        let url = test_database_url();
        let options: PgConnectOptions = url
            .parse()
            .expect("the test database URL must be a valid PostgreSQL URL");
        let options = options.options([("search_path", schema.as_str())]);

        let db = Database::connect_with_options(
            options,
            pool_size.max(2),
            1,
            Duration::from_secs(20),
            Duration::from_secs(60),
            Duration::from_secs(300),
            false,
        )
        .await
        .expect("cannot connect to the test schema");
        db.migrate().await.expect("migrations must apply cleanly");

        // Prove the isolation before any test relies on it: if `options=` were
        // ignored, every table would land in `public` and a parallel suite would
        // collide.
        let (current,): (String,) = sqlx::query_as("SELECT current_schema()")
            .fetch_one(db.pool())
            .await
            .expect("cannot read the current schema");
        assert_eq!(
            current, schema,
            "the test schema was not selected: `options=-csearch_path` did not take effect"
        );

        TestDb {
            schema,
            db,
            admin,
        }
    }

    /// The migrated database.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Repositories bound to this schema.
    pub fn repos(&self) -> Repositories {
        self.db.repositories()
    }

    /// The name of this test's schema.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Drop the schema.
    ///
    /// Never `DROP DATABASE`, and never anything that signals another backend: this
    /// sandbox forbids cross-process signalling and a checkpoint request would take
    /// the whole cluster down with it.
    pub async fn cleanup(self) {
        self.db.close().await;
        let _ = execute(
            &self.admin,
            &format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema),
        )
        .await;
        self.admin.close().await;
    }
}

/// A temporary Maildir plus attachment store rooted in a fresh directory.
pub struct Stores {
    /// Kept alive so the directory outlives the stores.
    _root: tempfile::TempDir,
    /// Where message bytes land.
    pub maildir: Maildir,
    /// Where attachment blobs land.
    pub attachments: AttachmentStore,
}

impl Stores {
    /// Create a fresh pair of stores.
    pub fn new() -> Stores {
        let root = tempfile::tempdir().expect("temp dir");
        Stores {
            maildir: Maildir::new(root.path().join("mail"), false, MailboxLayout::Maildir),
            attachments: AttachmentStore::new(root.path().join("attachments"), false),
            _root: root,
        }
    }
}

/// A configuration for a test server: `mx.test`, ephemeral ports, modest limits.
pub fn test_config() -> Config {
    let mut config = Config::default();
    config.server.hostname = "mx.test".to_string();
    config.smtp.host = "127.0.0.1".to_string();
    config.smtp.port = 0;
    config.smtp.submission_port = 0;
    config.smtp.smtps_port = 0;
    config.smtp.banner = "Ferroma test ESMTP".to_string();
    config.smtp.helo_required = true;
    config.smtp.require_tls_for_auth = false;
    config.smtp.require_auth_on_submission = false;
    config.smtp.advertise_size = true;
    config.smtp.command_timeout_secs = 10;
    config.smtp.data_timeout_secs = 10;
    config.tls.enabled = false;
    config.limits.max_message_size = 262_144;
    config
}

/// A tiny line-oriented SMTP test client.
pub struct TestClient {
    reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl TestClient {
    /// Connect and read the banner, returning both.
    pub async fn connect(address: SocketAddr) -> (TestClient, String) {
        let stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect to the test server");
        let (read, write) = stream.into_split();
        let mut client = TestClient {
            reader: tokio::io::BufReader::new(read),
            writer: write,
        };
        let banner = client.read_reply().await;
        (client, banner)
    }

    /// Send one command line (CRLF is appended).
    pub async fn send(&mut self, line: &str) {
        use tokio::io::AsyncWriteExt;
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("write command");
        self.writer.flush().await.expect("flush");
    }

    /// Send raw bytes.
    pub async fn send_raw(&mut self, bytes: &[u8]) {
        use tokio::io::AsyncWriteExt;
        self.writer.write_all(bytes).await.expect("write bytes");
        self.writer.flush().await.expect("flush");
    }

    /// Read one complete reply (following `250-…` continuations).
    pub async fn read_reply(&mut self) -> String {
        use tokio::io::AsyncBufReadExt;
        let mut out = String::new();
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
                .await
                .expect("reply timed out")
                .expect("read reply");
            if read == 0 {
                break;
            }
            out.push_str(&line);
            let bytes = line.as_bytes();
            if bytes.len() < 4 || bytes[3] != b'-' {
                break;
            }
        }
        out
    }

    /// Send a command and return its reply.
    pub async fn command(&mut self, line: &str) -> String {
        self.send(line).await;
        self.read_reply().await
    }

    /// Give the socket back, so it can be handed to a TLS connector.
    ///
    /// Anything still buffered from the peer is **discarded** — which is what a
    /// `STARTTLS` upgrade requires. The server refuses to upgrade when *its* buffer is
    /// not empty, so a test that reaches this point has already proven the peer sent
    /// nothing after `STARTTLS`.
    pub fn into_stream(self) -> tokio::net::TcpStream {
        let TestClient { reader, writer } = self;
        reader
            .into_inner()
            .reunite(writer)
            .expect("the two halves came from the same socket")
    }

    /// Shut the connection down.
    pub async fn shutdown(mut self) {
        use tokio::io::AsyncWriteExt;
        let _ = self.writer.shutdown().await;
    }
}

/// The first line of a reply, without its CRLF.
///
/// Returns an owned `String` so callers can bind it without fighting a borrow from
/// the reply buffer.
pub fn first_line(reply: &str) -> String {
    reply
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .to_string()
}

/// A minimal SMTP server for the queue tests.
///
/// It answers with a scripted code at the end of `DATA` and records how many
/// messages it received, so a test can assert both the retry schedule and the
/// give-up behaviour without a real remote server.
pub struct FakeMx {
    /// The address it listens on.
    pub address: SocketAddr,
    /// How many messages it has answered for.
    received: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
}

impl FakeMx {
    /// Bind on an ephemeral port and start answering.
    pub async fn start(data_code: u16) -> FakeMx {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake MX");
        let address = listener.local_addr().expect("local address");
        let received = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let counter = Arc::clone(&received);
        let stop = Arc::clone(&shutdown);
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let accepted =
                    tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
                let Ok(Ok((socket, _peer))) = accepted else {
                    continue;
                };
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut reader = BufReader::new(read);
                    let _ = write.write_all(b"220 fake.mx ESMTP\r\n").await;
                    let mut line = String::new();
                    let mut in_data = false;
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let trimmed = line.trim_end();
                        if in_data {
                            if trimmed == "." {
                                counter.fetch_add(1, Ordering::AcqRel);
                                let reply = format!("{data_code} scripted reply\r\n");
                                let _ = write.write_all(reply.as_bytes()).await;
                                in_data = false;
                            }
                            continue;
                        }
                        let upper = trimmed.to_ascii_uppercase();
                        let reply: &[u8] = if upper.starts_with("EHLO") {
                            // RFC 5321 §4.2.1: a multi-line reply uses `250-` on every
                            // line but the last. A space on the first line ends the
                            // reply, and the leftover line is then misread as the
                            // answer to the *next* command.
                            b"250-fake.mx greets you\r\n250 PIPELINING\r\n"
                        } else if upper.starts_with("HELO") {
                            b"250 fake.mx\r\n"
                        } else if upper.starts_with("MAIL FROM") {
                            b"250 2.1.0 Ok\r\n"
                        } else if upper.starts_with("RCPT TO") {
                            b"250 2.1.5 Ok\r\n"
                        } else if upper == "DATA" {
                            in_data = true;
                            b"354 End data\r\n"
                        } else if upper == "QUIT" {
                            let _ = write.write_all(b"221 2.0.0 Bye\r\n").await;
                            break;
                        } else {
                            b"500 5.5.2 Unknown\r\n"
                        };
                        if write.write_all(reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        FakeMx {
            address,
            received,
            shutdown,
        }
    }

    /// How many messages the fake MX has answered for.
    pub fn received(&self) -> usize {
        self.received.load(Ordering::Acquire)
    }

    /// Stop listening.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

/// Create a domain plus a user with one address, and return the pieces.
pub async fn seed_mailbox(
    repos: &Repositories,
    domain: &str,
    local_part: &str,
    password_hash: &str,
) -> (UserId, MailboxId) {
    let domain_row = repos
        .domains
        .create(domain, None)
        .await
        .expect("create domain");
    let user = repos
        .users
        .create(ferroma_storage::repository::NewUser {
            email: format!("{local_part}@{domain}"),
            password_hash: password_hash.to_string(),
            display_name: Some(local_part.to_string()),
            is_admin: false,
            quota_bytes: None,
        })
        .await
        .expect("create user");
    let mailbox = repos
        .mailboxes
        .create(ferroma_storage::repository::NewMailbox {
            user_id: user.user_id(),
            domain_id: domain_row.domain_id(),
            local_part: local_part.to_string(),
            display_name: None,
            is_primary: true,
            quota_bytes: None,
        })
        .await
        .expect("create mailbox");
    repos
        .folders
        .ensure_standard(mailbox.mailbox_id())
        .await
        .expect("create the standard folders");
    (user.user_id(), mailbox.mailbox_id())
}
