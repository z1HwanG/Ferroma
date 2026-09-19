//! Shared helpers for `ferroma-imap` integration tests.
//!
//! These tests need a **real** PostgreSQL, a **real** Maildir and a **real**
//! socket: the point of the suite is to drive the listener the way Thunderbird
//! does, not to poke at an in-process fake.
//!
//! # Isolation
//!
//! `ferroma-storage`'s own suite gives every test its own PostgreSQL *schema*,
//! which needs `sqlx` to build the connection options. `ferroma-imap`'s
//! `Cargo.toml` does not depend on `sqlx` (and the task forbids adding
//! dependencies), so this helper isolates by **data** instead: every test gets
//! its own domain name, its own user address and its own Maildir root, derived
//! from a per-process counter plus a timestamp. All the unique indexes that
//! matter (`users.email`, `domains.name`, `mailboxes (domain_id, local_part)`,
//! `folders (mailbox_id, name)`) are then naturally disjoint.
//!
//! A run leaves rows behind in `ferroma_test`. That is deliberate: dropping a
//! database or signalling PostgreSQL is forbidden on this host (see
//! `AGENTS.md` §2.1), and re-running the migrations is idempotent.
//!
//! # Skipping
//!
//! Following the repository rule that a skipped test must not look like a pass,
//! an unreachable database **fails** the suite. Set
//! `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1` to opt into skipping explicitly.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroma_imap::{ImapServerConfig, MapAuthenticator};
use ferroma_storage::models::{Folder, Message, User};
use ferroma_storage::repository::{NewMailbox, NewMessage, NewUser};
use ferroma_storage::{Database, Maildir, Repositories};
use ferroma_core::{DomainId, MailboxId, UserId};

/// The shared integration-test database.
pub const TEST_DATABASE: &str = "ferroma_test";

/// Per-process counter, so two tests in the same process never collide.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The administrative connection URL (the `postgres` maintenance database).
pub fn admin_url() -> String {
    std::env::var("FERROMA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferroma@127.0.0.1:5433/postgres".to_string())
}

/// The URL of the shared test database, derived from [`admin_url`].
pub fn test_database_url() -> String {
    let url = admin_url();
    match url.rfind('/') {
        Some(index) => {
            let (prefix, rest) = url.split_at(index + 1);
            let suffix = rest.find('?').map(|q| &rest[q..]).unwrap_or("");
            format!("{prefix}{TEST_DATABASE}{suffix}")
        }
        None => format!("{url}/{TEST_DATABASE}"),
    }
}

/// How long the harness waits for a reachable database before giving up.
///
/// Generous for the same reason as the reply ceilings in the protocol tests: a
/// busy machine must not be reported as a broken one.
const DATABASE_DEADLINE: Duration = Duration::from_secs(60);

/// Whether the test database is reachable.
///
/// Retries until a generous deadline rather than a fixed few times:
/// `scripts/pg-supervisor.ps1` may be mid-restart when a suite starts, and the
/// suite runs many binaries at once, so a connect that a loaded machine needs
/// longer than one attempt's budget for must not turn into "no database" — that
/// would fail the whole run for the wrong reason. A database that really is
/// absent still fails, after [`DATABASE_DEADLINE`].
pub async fn database_available() -> bool {
    let deadline = tokio::time::Instant::now() + DATABASE_DEADLINE;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match Database::connect_with(
            &test_database_url(),
            1,
            1,
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(300),
            false,
        )
        .await
        {
            Ok(db) => {
                db.close().await;
                return true;
            }
            Err(err) => {
                if attempt == 1 {
                    eprintln!("waiting for PostgreSQL: {err}");
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Fails the test when no database is reachable, unless skipping was requested.
///
/// Returns `true` when the test should run, `false` when it was opted out of.
pub async fn require_database() -> bool {
    if database_available().await {
        return true;
    }
    let skip = std::env::var("FERROMA_TEST_SKIP_WITHOUT_DATABASE").is_ok_and(|v| v == "1");
    assert!(
        skip,
        "no PostgreSQL at {}: start it with scripts/dev-postgres.ps1, \
         or set FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 to skip explicitly",
        test_database_url()
    );
    eprintln!("skipping: no PostgreSQL and FERROMA_TEST_SKIP_WITHOUT_DATABASE=1");
    false
}

/// A unique token for one test.
pub fn unique_token(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{label}{}-{count}", nanos % 1_000_000_000)
}

/// A harness holding a migrated database, a Maildir and a seeded account.
pub struct Harness {
    /// The connected database.
    pub db: Database,
    /// The repositories over that database.
    pub repos: Repositories,
    /// The Maildir the server reads message bodies from.
    pub maildir: Maildir,
    /// The temporary directory the Maildir lives in, kept alive by the harness.
    pub root: tempfile::TempDir,
    /// The seeded domain name.
    pub domain: String,
    /// The seeded local part.
    pub local_part: String,
    /// The seeded address (`local_part@domain`).
    pub address: String,
    /// The seeded user's id.
    pub user: UserId,
    /// The seeded domain's id.
    pub domain_id: DomainId,
    /// The seeded address's id.
    pub mailbox: MailboxId,
    /// The credential the account answers to.
    pub password: String,
}

impl Harness {
    /// Connect, migrate, and seed one account with its standard folders.
    pub async fn new(label: &str) -> Harness {
        let db = Database::connect_with(
            &test_database_url(),
            8,
            1,
            Duration::from_secs(15),
            Duration::from_secs(60),
            Duration::from_secs(300),
            false,
        )
        .await
        .expect("cannot connect to the integration database");
        db.migrate().await.expect("migrations must apply cleanly");

        let token = unique_token(label);
        let root = tempfile::tempdir().expect("cannot create a temporary Maildir root");
        let maildir = Maildir::new(
            root.path().join("mail"),
            false,
            ferroma_core::config::MailboxLayout::Maildir,
        );
        let repos = db.repositories();

        let domain = format!("{token}.example");
        let local_part = "alice".to_string();
        let address = format!("{local_part}@{domain}");
        let password = "correct-horse-battery-staple".to_string();

        let user: User = repos
            .users
            .create(NewUser {
                email: address.clone(),
                // A pre-computed Argon2id hash of the password, so the test does
                // not pay for hashing.
                password_hash: "$argon2id$v=19$m=19456,t=2,p=1$\
                    c29tZXNhbHR2YWx1ZQ$\
                    YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5eg"
                    .to_string(),
                display_name: Some("Alice".into()),
                is_admin: false,
                enabled: true,
                quota_bytes: Some(1_073_741_824),
            })
            .await
            .expect("cannot create the test user");
        let domain_row = repos
            .domains
            .create(&domain, Some("integration test"))
            .await
            .expect("cannot create the test domain");
        let mailbox = repos
            .mailboxes
            .create(NewMailbox {
                user_id: user.user_id(),
                domain_id: domain_row.domain_id(),
                local_part: local_part.clone(),
                display_name: Some("Alice".into()),
                is_primary: true,
                quota_bytes: None,
            })
            .await
            .expect("cannot create the test address");

        maildir
            .ensure_mailbox(&domain, &local_part)
            .expect("cannot create the Maildir");
        repos
            .folders
            .ensure_standard(mailbox.mailbox_id())
            .await
            .expect("cannot create the standard folders");

        Harness {
            db,
            repos,
            maildir,
            root,
            domain,
            local_part,
            address,
            user: user.user_id(),
            domain_id: domain_row.domain_id(),
            mailbox: mailbox.mailbox_id(),
            password,
        }
    }

    /// An authenticator that accepts this harness's account.
    pub fn authenticator(&self) -> MapAuthenticator {
        MapAuthenticator::single(&self.address, &self.password, self.user.get())
    }

    /// The server configuration the tests build an `ImapServer` from.
    ///
    /// The port is never bound: every test binds its own listener on an
    /// ephemeral port and hands the accepted socket to `serve_stream`. It only
    /// has to pass [`ImapServerConfig::validate`], which rejects `0`.
    pub fn server_config(&self) -> ImapServerConfig {
        ImapServerConfig {
            host: "127.0.0.1".into(),
            port: 143,
            imaps_port: 0,
            banner: "Ferroma test IMAP".into(),
            max_append_size: 1_048_576,
            // Both ceilings are far above what the test needs: the point is that
            // a starved scheduler delays a push rather than making the *server*
            // end the idle first, which would fail the test for the wrong reason.
            max_idle_secs: 300,
            idle_timeout_secs: 600,

            maildir_root: self.root.path().join("mail"),
            ..ImapServerConfig::default()
        }
    }

    /// Write one message into a folder and record its row.
    ///
    /// Returns the stored row, so a test can assert on its UID and flags.
    pub async fn seed_message(&self, folder: &str, raw: &[u8]) -> Message {
        let folder_row = self
            .repos
            .folders
            .require_by_name(self.mailbox, folder)
            .await
            .expect("the folder must exist");
        let stored = self
            .maildir
            .store(&self.domain, &self.local_part, folder, raw, "")
            .expect("cannot store the seeded message");
        let parsed = ferroma_mail::ParsedMessage::parse(raw).ok();
        let sender = parsed.as_ref().and_then(|p| p.from().first().cloned());
        self.repos
            .messages
            .insert(NewMessage {
                folder_id: folder_row.folder_id(),
                mailbox_id: self.mailbox,
                rfc_message_id: parsed
                    .as_ref()
                    .and_then(ferroma_mail::ParsedMessage::message_id)
                    .map(|id| id.to_string()),
                thread_id: None,
                subject: parsed.as_ref().and_then(ferroma_mail::ParsedMessage::subject),
                sender: sender.as_ref().map(|m| m.address.to_string()),
                sender_name: sender.as_ref().and_then(|m| m.name.clone()),
                snippet: parsed.as_ref().map(|p| p.snippet(120)),
                size_bytes: raw.len() as i64,
                storage_path: stored.path,
                checksum_sha256: Some(stored.sha256),
                flags: String::new(),
                internal_date: None,
                sent_at: parsed.as_ref().and_then(ferroma_mail::ParsedMessage::date),
                has_attachments: parsed
                    .as_ref()
                    .map(ferroma_mail::ParsedMessage::has_attachments)
                    .unwrap_or(false),
                attachment_count: 0,
                is_draft: false,
            })
            .await
            .expect("cannot insert the seeded message")
    }

    /// One folder row by name.
    pub async fn folder(&self, name: &str) -> Folder {
        self.repos
            .folders
            .require_by_name(self.mailbox, name)
            .await
            .expect("the folder must exist")
    }

    /// The configured Maildir root.
    pub fn maildir_root(&self) -> PathBuf {
        self.root.path().join("mail")
    }

    /// Close the pool; the temporary directory is dropped with the harness.
    pub async fn cleanup(self) {
        self.db.close().await;
    }
}

/// A message fixture: a small `multipart/alternative` with a text and an HTML
/// part, in the shape Thunderbird sends.
pub fn sample_message(subject: &str) -> Vec<u8> {
    format!(
        "From: Bob <bob@example.net>\r\n\
         To: Alice <alice@example.com>\r\n\
         Subject: {subject}\r\n\
         Date: Wed, 08 Jul 2026 09:00:00 +0000\r\n\
         Message-ID: <{subject}@example.net>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=\"TB\"\r\n\
         \r\n\
         --TB\r\n\
         Content-Type: text/plain; charset=UTF-8\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\
         \r\n\
         Hello=20there\r\n\
         --TB\r\n\
         Content-Type: text/html; charset=UTF-8\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\
         \r\n\
         <html><body>Hello</body></html>\r\n\
         --TB--\r\n"
    )
    .into_bytes()
}
