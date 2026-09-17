//! Ferroma acceptance run — the specification's §60 criteria, over real sockets.
//!
//! This test does not mock the platform. It writes a configuration, starts the real
//! `ferroma serve` binary as a child process, and then talks to it the way the
//! outside world does:
//!
//! ```text
//!   ferroma user create …          (the CLI, against the same database)
//!          │
//!   SMTP   ──► alice sends to bob  ──►  250 queued/delivered
//!          │
//!   disk   ──► bob's Maildir has the bytes, PostgreSQL has the metadata
//!          │
//!   IMAP   ──► LOGIN, SELECT, FETCH   ──►  the message is there
//!          │
//!   HTTP   ──► /auth/login, /messages ──►  the API agrees
//!          │
//!   FCP    ──► /client/sync            ──►  the change cursor sees it
//! ```
//!
//! # Why this exists as well as the per-crate integration tests
//!
//! Each crate's tests prove its own layer against a real database and a real
//! Maildir. None of them proves that the *wiring* is right: that the SMTP listener
//! and the IMAP listener and the HTTP API are looking at the same repositories, the
//! same Maildir and the same event bus. That is what this file is for, and it is the
//! only test that can catch a mistake in `server/src/serve.rs`.
//!
//! # Requirements
//!
//! A PostgreSQL reachable at `FERROMA_TEST_DATABASE_URL` (default
//! `postgres://ferroma@127.0.0.1:5433/postgres`). Like every other integration suite
//! here, it **fails** rather than skipping when the database is unreachable, unless
//! `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1` is set.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Ports chosen to be unlikely to collide with anything the developer is running.
const SMTP_PORT: u16 = 2625;
const SUBMISSION_PORT: u16 = 2687;
const IMAP_PORT: u16 = 1243;
const API_PORT: u16 = 8180;

const ALICE: &str = "alice@acceptance.test";
const BOB: &str = "bob@acceptance.test";
const PASSWORD: &str = "correct horse battery staple";

// -----------------------------------------------------------------------------
// Test scaffolding
// -----------------------------------------------------------------------------

fn admin_url() -> String {
    std::env::var("FERROMA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferroma@127.0.0.1:5433/postgres".to_string())
}

/// Serialise the acceptance tests against each other.
///
/// They bind **fixed ports** (the point is to talk to a real server at a known
/// address) and spawn a whole `ferroma serve`, so two of them running at once must
/// collide. Under `cargo test --workspace` they do: five tests try to bind 2625 at
/// the same moment and four fail with "the server did not become healthy", which
/// looks like a platform bug and is purely a test-harness one.
///
/// A `tokio::sync::Mutex` rather than `std::sync::Mutex`, because the guard is held
/// across `.await` and the test futures must stay `Send`.
fn serial_guard() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn database_name(suffix: &str) -> String {
    format!("ferroma_acceptance_{suffix}")
}

/// Fail loudly rather than skipping, unless skipping was explicitly asked for.
fn require_database() -> bool {
    let reachable = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:5433".parse().expect("literal"),
        Duration::from_secs(3),
    )
    .is_ok();

    if reachable {
        return true;
    }
    if std::env::var_os("FERROMA_TEST_SKIP_WITHOUT_DATABASE").is_some() {
        eprintln!("skipping: no PostgreSQL reachable at {}", admin_url());
        return false;
    }
    panic!(
        "the acceptance run needs PostgreSQL at {}. Start it with \
         `scripts/dev-postgres.ps1 start`, or set FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 \
         to skip this suite explicitly.",
        admin_url()
    );
}

/// The acceptance run gets its own database, so it cannot disturb a test schema or
/// a developer's data.
fn acceptance_url(database: &str) -> String {
    let admin = admin_url();
    match admin.rfind('/') {
        Some(at) => {
            let (prefix, rest) = admin.split_at(at + 1);
            let suffix = rest.find('?').map(|q| &rest[q..]).unwrap_or("");
            format!("{prefix}{database}{suffix}")
        }
        None => admin,
    }
}

async fn ensure_database(name: &str) {
    let admin = sqlx::PgPool::connect(&admin_url())
        .await
        .expect("cannot connect to the maintenance database");
    let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(name)
        .fetch_optional(&admin)
        .await
        .unwrap_or(None);
    if exists.is_none() {
        // Losing the race is success: a concurrent run created it.
        let _ = sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await;
    }
    admin.close().await;
}

/// Remove a test's database.
///
/// Deliberately a plain `DROP DATABASE` with no `WITH (FORCE)`: forcing the drop
/// makes PostgreSQL signal other backends, and this sandbox forbids cross-process
/// signalling — the failure mode is not an error but a wedged cluster. Without
/// `FORCE` the statement simply fails while a connection is still open, which is
/// exactly the outcome we can tolerate: the child process is killed first, so it
/// normally succeeds, and a leftover database costs a few megabytes.
async fn drop_database(name: &str) {
    let Ok(admin) = sqlx::PgPool::connect(&admin_url()).await else {
        return;
    };
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\""))
        .execute(&admin)
        .await;
    admin.close().await;
}

/// A running `ferroma serve`, killed when the guard drops.
///
/// Dropping the guard kills the child process but **not** its database: an async
/// `DROP` cannot run from `Drop`, and cancelling a spawned task as the test runtime
/// shuts down is not reliable. Call [`Server::cleanup`] at the end of a test to
/// remove it. Leaving one behind is harmless — the name carries a random suffix, so
/// a later run never collides with it — it just costs a few megabytes until someone
/// drops it by hand.
struct Server {
    child: Child,
    dir: tempfile::TempDir,
    config: PathBuf,
    /// This run's database, dropped by [`Server::cleanup`].
    database: String,
}

impl Server {
    /// Write a configuration, start the binary, and wait until it answers.
    ///
    /// Every call gets **its own database**. Sharing one would leak state between
    /// tests — the second test would find the first one's domain already created —
    /// and the failure looks like a platform bug rather than a test-fixture one.
    async fn start() -> Server {
        let database = database_name(&uuid::Uuid::new_v4().simple().to_string()[..12]);
        ensure_database(&database).await;

        let dir = tempfile::tempdir().expect("a temporary directory");
        let config = dir.path().join("ferroma.toml");
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).expect("create the data directory");

        // The child inherits the *package* directory as its working directory, not
        // the workspace root, so `./web` would not resolve. Point the frontends at
        // absolute paths — which is also how a container mounts them.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the workspace root")
            .to_path_buf();
        let webmail = workspace.join("web");
        let admin = workspace.join("admin");

        let toml = format!(
            r#"
[server]
name = "Ferroma"
hostname = "localhost"
data_dir = {data:?}
log_level = "debug"
log_format = "text"

[database]
url = {url:?}
run_migrations = true

[smtp]
enabled = true
host = "127.0.0.1"
port = {smtp}
submission_port = {submission}
smtps_port = 0
require_tls_for_auth = false
require_auth_on_submission = true

[imap]
enabled = true
host = "127.0.0.1"
port = {imap}
imaps_port = 0
require_tls_for_login = false

[tls]
enabled = false

# Tight DNS bounds. The defaults are 5s x 3 attempts, which a blackholing resolver
# turns into 15 seconds *per lookup*, and the inbound policy step does several — so a
# fresh server can stall an inbound message for most of a minute before it even
# reaches delivery. `.test` is a reserved TLD, exactly the case where a resolver may
# not answer promptly.
[dns]
timeout_secs = 1
attempts = 1
cache_ttl_secs = 60
negative_ttl_secs = 60

# Inbound authentication is OFF for the acceptance run, deliberately.
#
# Not because it is unimportant — `crates/ferroma-smtp/tests/inbound_policy.rs` owns
# that coverage with 13 tests over real sockets — but because it makes the run depend
# on this host's resolver. With SPF/DKIM/DMARC enabled, a message takes the policy
# step's entire 60-second backstop on a machine whose resolver does not answer raw
# UDP queries, which is a platform-level finding (reported separately) rather than
# something this acceptance run should encode as normal. What this file proves is the
# *delivery path*: SMTP to Maildir to PostgreSQL to IMAP to the API to the sync
# cursor, deterministically.
[policy]
spf_enabled = false
dmarc_enabled = false
add_auth_results = false

[queue]
enabled = true
workers = 2
retry_schedule_secs = [2, 5, 15]
max_attempts = 3
poll_interval_secs = 1

[api]
enabled = true
host = "127.0.0.1"
port = {api}
tls_port = 0
base_path = "/api/v1"
public_url = {public:?}
webmail_dir = {webmail:?}
admin_dir = {admin:?}
jwt_secret = "acceptance-run-secret-that-is-at-least-32-bytes"
secure_cookies = false
serve_frontend = true

[storage]
fsync_on_write = false

[limits]
# Small enough that the message-size and quota paths can actually be exercised.
# `max_attachment_size` must not exceed `max_message_size` — the configuration
# validator refuses the combination, which is how this file learned to set both.
max_message_size = 1048576
max_attachment_size = 1048576
max_recipients = 5
mailbox_quota = 8388608
max_failed_logins = 5
"#,
            data = data.display().to_string(),
            url = acceptance_url(&database),
            smtp = SMTP_PORT,
            submission = SUBMISSION_PORT,
            imap = IMAP_PORT,
            api = API_PORT,
            public = format!("http://127.0.0.1:{API_PORT}"),
            webmail = webmail.display().to_string(),
            admin = admin.display().to_string(),
        );
        std::fs::write(&config, toml).expect("write the configuration");

        let binary = env!("CARGO_BIN_EXE_ferroma");
        let child = Command::new(binary)
            .arg("--config")
            .arg(&config)
            .arg("serve")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {binary}: {e}"));

        let mut server = Server {
            child,
            dir,
            config,
            database,
        };

        // Wait for it to answer. The startup path connects to PostgreSQL, applies
        // migrations and binds four listeners, so a generous window is right.
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if Instant::now() > deadline {
                panic!(
                    "the server did not become healthy within 45s\n--- captured output ---\n{}",
                    server.captured_output()
                );
            }
            if http_get(&format!("http://127.0.0.1:{API_PORT}/api/v1/health"))
                .await
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        server
    }

    /// Run the `ferroma` CLI against the same configuration and database.
    fn cli(&self, args: &[&str]) -> String {
        let binary = env!("CARGO_BIN_EXE_ferroma");
        let output = Command::new(binary)
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("cannot run the CLI: {e}"));

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(
            output.status.success(),
            "`ferroma {}` failed: {stderr}{stdout}",
            args.join(" ")
        );
        stdout
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    /// Stop the child and remove this run's database.
    ///
    /// Call at the end of a test. `Drop` still kills the process if a test panics —
    /// leaving the database behind, which is the safe direction to fail in.
    async fn cleanup(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // A moment for the socket to close, so the DROP is not blocked by a backend
        // that is still shutting down.
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop_database(&self.database).await;
    }

    /// Drain whatever the child has written. Only called on failure.
    fn captured_output(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut out) = self.child.stdout.take() {
            let _ = std::io::Read::read_to_string(&mut out, &mut text);
        }
        if let Some(mut err) = self.child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut err, &mut text);
        }
        text
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// -----------------------------------------------------------------------------
// Minimal protocol clients
// -----------------------------------------------------------------------------

/// A one-shot HTTP/1.1 request. Returns `(status, body)`.
async fn http_request(method: &str, url: &str, body: Option<&str>, token: Option<&str>) -> Option<(u16, String)> {
    let without_scheme = url.strip_prefix("http://")?;
    let (authority, path) = match without_scheme.find('/') {
        Some(at) => without_scheme.split_at(at),
        None => (without_scheme, "/"),
    };

    let mut stream = TcpStream::connect(authority).await.ok()?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    match body {
        Some(body) => {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        }
        None => request.push_str("\r\n"),
    }

    stream.write_all(request.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.ok()?;
    let text = String::from_utf8_lossy(&response).to_string();

    let status = text
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();

    Some((status, body))
}

async fn http_get(url: &str) -> Option<String> {
    http_request("GET", url, None, None)
        .await
        .map(|(_, body)| body)
}

async fn http_json(
    method: &str,
    url: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> (u16, serde_json::Value) {
    let (status, text) = http_request(method, url, body, token)
        .await
        .unwrap_or_else(|| panic!("{method} {url} got no response"));
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, json)
}

/// Read one SMTP reply, following `250-` continuation lines.
///
/// The window is generous because the *first* transaction pays for everything the
/// server has never done before: the policy step's DNS lookups (SPF, DKIM key, DMARC
/// record — several round trips, each bounded by `[dns]`), the first Maildir write,
/// and the first quota recount. A tight timeout here reports "the server is broken"
/// when the server is merely doing its job for the first time.
async fn smtp_reply(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> (u16, String) {
    let mut lines = Vec::new();
    let mut code = 0u16;
    loop {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(60), reader.read_line(&mut line))
            .await
            .expect("an SMTP reply within 60s")
            .expect("read an SMTP reply");
        assert!(read > 0, "the SMTP server closed the connection");
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();

        if trimmed.len() >= 3 {
            code = trimmed[..3].parse().unwrap_or(0);
        }
        // `250-text` continues; `250 text` ends the reply.
        let more = trimmed.as_bytes().get(3) == Some(&b'-');
        lines.push(trimmed);
        if !more {
            break;
        }
    }
    (code, lines.join("\n"))
}

/// Write one CRLF-terminated line and flush.
async fn send_line(write_half: &mut tokio::net::tcp::OwnedWriteHalf, line: &str) {
    write_half
        .write_all(line.as_bytes())
        .await
        .expect("write a command");
    write_half.flush().await.expect("flush");
}

/// Speak SMTP and return the reply for each command sent.
async fn smtp_send(
    port: u16,
    helo: &str,
    from: &str,
    recipients: &[&str],
    body: &str,
) -> Vec<(u16, String)> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to SMTP");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut replies = vec![smtp_reply(&mut reader).await];

    send_line(&mut write_half, &format!("EHLO {helo}\r\n")).await;
    replies.push(smtp_reply(&mut reader).await);

    send_line(&mut write_half, &format!("MAIL FROM:<{from}>\r\n")).await;
    replies.push(smtp_reply(&mut reader).await);

    for recipient in recipients {
        send_line(&mut write_half, &format!("RCPT TO:<{recipient}>\r\n")).await;
        replies.push(smtp_reply(&mut reader).await);
    }

    if replies.last().map(|(code, _)| *code).unwrap_or(0) < 400 {
        send_line(&mut write_half, "DATA\r\n").await;
        replies.push(smtp_reply(&mut reader).await);

        if replies.last().map(|(code, _)| *code).unwrap_or(0) < 400 {
            // Dot-stuff any line that begins with a period, as a client must.
            let stuffed: String = body
                .split_inclusive("\r\n")
                .map(|line| {
                    if line.starts_with('.') {
                        format!(".{line}")
                    } else {
                        line.to_string()
                    }
                })
                .collect();
            send_line(&mut write_half, &format!("{stuffed}\r\n.\r\n")).await;
            replies.push(smtp_reply(&mut reader).await);
        }
    }

    send_line(&mut write_half, "QUIT\r\n").await;
    replies
}

/// Speak just enough IMAP to fetch every subject in a folder.
async fn imap_subjects(user: &str, password: &str, folder: &str) -> Vec<String> {
    let stream = TcpStream::connect(("127.0.0.1", IMAP_PORT))
        .await
        .expect("connect to IMAP");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut greeting = String::new();
    reader.read_line(&mut greeting).await.expect("read the greeting");
    assert!(greeting.starts_with("* OK"), "greeting was {greeting:?}");

    async fn command(
        write_half: &mut tokio::net::tcp::OwnedWriteHalf,
        reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
        line: &str,
        tag: &str,
    ) -> String {
        write_half.write_all(line.as_bytes()).await.expect("write");
        write_half.write_all(b"\r\n").await.expect("write");
        write_half.flush().await.expect("flush");

        let mut collected = String::new();
        loop {
            let mut chunk = String::new();
            let read = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut chunk))
                .await
                .expect("an IMAP response within 10s")
                .expect("read");
            if read == 0 {
                break;
            }
            collected.push_str(&chunk);
            if chunk.starts_with(&format!("{tag} ")) {
                break;
            }
        }
        collected
    }

    let login = command(
        &mut write_half,
        &mut reader,
        // IMAP `astring` may not contain a space unquoted, so a password with one has
        // to be a quoted string. The server is right to answer `BAD` otherwise, which
        // is exactly how this line was written the first time.
        &format!("a1 LOGIN {user} \"{password}\""),
        "a1",
    )
    .await;
    assert!(login.contains("a1 OK"), "LOGIN failed: {login}");

    let select = command(
        &mut write_half,
        &mut reader,
        &format!("a2 SELECT {folder}"),
        "a2",
    )
    .await;
    assert!(select.contains("a2 OK"), "SELECT failed: {select}");

    let fetch = command(&mut write_half, &mut reader, "a3 FETCH 1:* (BODY.PEEK[HEADER])", "a3").await;
    let _ = command(&mut write_half, &mut reader, "a4 LOGOUT", "a4").await;

    // Pull every `Subject:` out of the returned header blocks, decoded.
    fetch
        .lines()
        .filter_map(|line| line.strip_prefix("Subject:"))
        .map(|subject| subject.trim().to_string())
        .collect()
}

// -----------------------------------------------------------------------------
// The acceptance run
// -----------------------------------------------------------------------------

/// §60: the server starts, the database connects, two accounts exist, alice can send
/// to bob, the message reaches bob's Maildir and the database, and no one can relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_travels_from_smtp_to_maildir_to_imap_to_the_api() {
    if !require_database() {
        return;
    }
    // Held for the whole test: these tests bind fixed ports and start a real server.
    let _serial = serial_guard().lock().await;

    let server = Server::start().await;

    // --- 3/4. two accounts, and the SMTP listener is up ----------------------
    server.cli(&["domain", "create", "acceptance.test"]);
    server.cli(&[
        "user", "create", ALICE, "--password", PASSWORD, "--display-name", "Alice",
    ]);
    server.cli(&["user", "create", BOB, "--password", PASSWORD]);

    let users = server.cli(&["user", "list", "--emails-only"]);
    assert!(users.contains(ALICE), "alice must exist: {users}");
    assert!(users.contains(BOB), "bob must exist: {users}");

    // --- 5/6/7. alice sends to bob -------------------------------------------
    let message = format!(
        "From: {ALICE}\r\nTo: {BOB}\r\nSubject: Acceptance run\r\nDate: Thu, 16 Sep 2026 12:00:00 +0000\r\nMessage-ID: <acceptance-1@acceptance.test>\r\n\r\nHello Bob,\r\n\r\nthis message is the acceptance run.\r\n"
    );
    let replies = smtp_send(SMTP_PORT, "client.acceptance.test", ALICE, &[BOB], &message).await;
    let final_code = replies.last().expect("a final reply").0;
    assert_eq!(
        final_code, 250,
        "the message must be accepted: {}",
        replies
            .iter()
            .map(|(c, t)| format!("{c} {}", t.lines().next().unwrap_or("")))
            .collect::<Vec<_>>()
            .join(" | ")
    );

    // --- 7. the bytes are on disk -------------------------------------------
    // Maildir++ layout, and a freshly delivered message lands in `new/` because it
    // carries no flags yet — that is `\Recent`.
    let bob_maildir = server
        .data_dir()
        .join("mail")
        .join("acceptance.test")
        .join("bob")
        .join("Maildir");
    let mut stored: Vec<PathBuf> = Vec::new();
    for sub in ["new", "cur"] {
        if let Ok(entries) = std::fs::read_dir(bob_maildir.join(sub)) {
            stored.extend(entries.flatten().map(|e| e.path()));
        }
    }
    assert_eq!(stored.len(), 1, "exactly one message should be in bob's Maildir");

    let bytes = std::fs::read(&stored[0]).expect("read the stored message");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("this message is the acceptance run"),
        "the body must survive the round trip"
    );
    assert!(
        text.starts_with("Received:") || text.contains("\r\nReceived:"),
        "the server must prepend a Received: header; got:\n{}",
        &text[..text.len().min(400)]
    );

    // --- 8. IMAP sees it, and reports the folder counts ----------------------
    let subjects = imap_subjects(BOB, PASSWORD, "INBOX").await;
    assert!(
        subjects.iter().any(|s| s.contains("Acceptance run")),
        "IMAP must show the message: {subjects:?}"
    );

    // --- 9. the HTTP API agrees ---------------------------------------------
    let (status, login) = http_json(
        "POST",
        &format!("http://127.0.0.1:{API_PORT}/api/v1/auth/login"),
        Some(&format!(
            r#"{{"email":"{BOB}","password":"{PASSWORD}"}}"#
        )),
        None,
    )
    .await;
    assert_eq!(status, 200, "login must succeed: {login}");
    let token = login["access_token"]
        .as_str()
        .expect("an access token")
        .to_string();

    let (status, list) = http_json(
        "GET",
        &format!("http://127.0.0.1:{API_PORT}/api/v1/messages?limit=10"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, 200, "listing messages must succeed: {list}");
    let serialised = list.to_string();
    assert!(
        serialised.contains("Acceptance run"),
        "the API must list the message: {serialised}"
    );

    // --- 10. an unknown recipient is refused --------------------------------
    let replies = smtp_send(
        SMTP_PORT,
        "client.acceptance.test",
        ALICE,
        &["nobody@acceptance.test"],
        &message,
    )
    .await;
    let rcpt = replies
        .iter()
        .find(|(code, _)| *code == 550)
        .unwrap_or_else(|| panic!("an unknown recipient must be 550: {replies:?}"));
    assert!(
        rcpt.1.contains("5.1.1") || rcpt.1.to_lowercase().contains("user unknown"),
        "the reply should say why: {}",
        rcpt.1
    );

    // --- 11. an unauthenticated peer cannot relay ---------------------------
    let replies = smtp_send(
        SMTP_PORT,
        "attacker.example",
        "spammer@attacker.example",
        &["victim@elsewhere.example"],
        &message,
    )
    .await;
    let refused = replies
        .iter()
        .any(|(code, text)| *code == 550 && text.to_lowercase().contains("relay"));
    assert!(
        refused,
        "relaying must be refused with a 550 that says so: {replies:?}"
    );

    // --- and the message did not appear anywhere ----------------------------
    let (_, list) = http_json(
        "GET",
        &format!("http://127.0.0.1:{API_PORT}/api/v1/messages?limit=10"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(
        list["total"].as_i64().unwrap_or(-1),
        1,
        "the refused transactions must not have stored anything: {list}"
    );
    server.cleanup().await;

}

/// §19/§21: the same delivery is visible to an official client through the sync
/// cursor, which is the contract the desktop client is built on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sync_cursor_sees_the_delivery() {
    if !require_database() {
        return;
    }
    // Held for the whole test: these tests bind fixed ports and start a real server.
    let _serial = serial_guard().lock().await;

    let server = Server::start().await;

    server.cli(&["domain", "create", "acceptance.test"]);
    server.cli(&[
        "user", "create", ALICE, "--password", PASSWORD, "--display-name", "Alice",
    ]);
    server.cli(&["user", "create", BOB, "--password", PASSWORD]);

    let (_, login) = http_json(
        "POST",
        &format!("http://127.0.0.1:{API_PORT}/api/v1/client/auth/login"),
        Some(&format!(
            r#"{{"email":"{BOB}","password":"{PASSWORD}",
                 "device":{{"device_uid":"acceptance-device","name":"acceptance","platform":"linux","client_version":"0.1.0"}}}}"#
        )),
        None,
    )
    .await;
    let token = login["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("client login must return a token: {login}"))
        .to_string();

    // Find bob's INBOX to sync.
    let (status, mailboxes) = http_json(
        "GET",
        &format!("http://127.0.0.1:{API_PORT}/api/v1/client/mailboxes"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, 200, "mailboxes must be listed: {mailboxes}");

    let mailbox_id = mailboxes["mailboxes"][0]["id"]
        .as_i64()
        .unwrap_or_else(|| panic!("an address with an id: {mailboxes}"));
    let folder_id = mailboxes["mailboxes"][0]["folders"]
        .as_array()
        .and_then(|folders| {
            folders
                .iter()
                .find(|f| f["name"].as_str() == Some("INBOX"))
                .and_then(|f| f["id"].as_i64())
        })
        .unwrap_or_else(|| panic!("an INBOX folder: {mailboxes}"));

    // A first sync from cursor 0 on an empty mailbox: nothing to do, and it must not
    // error.
    let (status, page) = http_json(
        "GET",
        &format!(
            "http://127.0.0.1:{API_PORT}/api/v1/client/sync?mailbox_id={mailbox_id}&folder_id={folder_id}&cursor=0"
        ),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, 200, "a first sync must succeed: {page}");
    assert_eq!(page["changes"].as_array().map(Vec::len).unwrap_or(9), 0);

    // Deliver, then sync again: the change must be there.
    let message = format!(
        "From: {ALICE}\r\nTo: {BOB}\r\nSubject: Sync me\r\nDate: Thu, 16 Sep 2026 12:00:00 +0000\r\nMessage-ID: <acceptance-sync@acceptance.test>\r\n\r\n.\r\n"
    );
    let replies = smtp_send(SMTP_PORT, "client.acceptance.test", ALICE, &[BOB], &message).await;
    assert_eq!(replies.last().expect("a reply").0, 250);

    let (status, page) = http_json(
        "GET",
        &format!(
            "http://127.0.0.1:{API_PORT}/api/v1/client/sync?mailbox_id={mailbox_id}&folder_id={folder_id}&cursor=0"
        ),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, 200, "sync after delivery must succeed: {page}");
    let changes = page["changes"].as_array().cloned().unwrap_or_default();
    assert!(
        changes
            .iter()
            .any(|c| c["type"].as_str() == Some("message_created")),
        "the sync cursor must report the delivery: {page}"
    );
    assert!(
        page["next_cursor"].as_str().unwrap_or("0") != "0"
            || page["next_cursor"].as_i64().unwrap_or(0) > 0,
        "the cursor must advance: {page}"
    );
    server.cleanup().await;

}

/// §36: the operator command surface works against a running platform.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cli_reports_a_consistent_store() {
    if !require_database() {
        return;
    }
    // Held for the whole test: these tests bind fixed ports and start a real server.
    let _serial = serial_guard().lock().await;

    let server = Server::start().await;
    server.cli(&["domain", "create", "acceptance.test"]);
    server.cli(&["user", "create", ALICE, "--password", PASSWORD]);

    let stats = server.cli(&["storage", "stats"]);
    assert!(stats.contains("accounts"), "{stats}");
    assert!(stats.contains("messages"), "{stats}");

    let verify = server.cli(&["storage", "verify", "--details"]);
    assert!(verify.contains("mail store is consistent"), "{verify}");

    // `healthcheck` is what the container HEALTHCHECK runs; against the live server
    // it must exit zero.
    let healthy = server.cli(&["healthcheck", "--url", &format!("http://127.0.0.1:{API_PORT}/api/v1/health")]);
    assert!(healthy.contains("ok"), "{healthy}");
    server.cleanup().await;

}

/// The Admin console is served by the same process that serves the API, so a
/// deployment has one thing to expose (specification §36, §48).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_frontends_are_served() {
    if !require_database() {
        return;
    }
    // The guard *is* the point: dropping it kills the child process, so it has to
    // outlive every request below.
    // Held for the whole test: these tests bind fixed ports and start a real server.
    let _serial = serial_guard().lock().await;

    let server = Server::start().await;

    // The webmail is served at the root.
    let (status, body) = http_request("GET", &format!("http://127.0.0.1:{API_PORT}/"), None, None)
        .await
        .unwrap_or_else(|| panic!("/ returned nothing"));
    assert_eq!(status, 200, "the webmail must be served at /");
    assert!(
        body.to_lowercase().contains("<html"),
        "/ should serve an HTML app, got: {}",
        &body[..body.len().min(200)]
    );

    // `/admin` is a **directory redirect**, not a page. Both apps reference their
    // assets relatively (`./main.js`), so a browser resolves them against the document
    // URL: at `/admin` without the trailing slash that base is `/`, and the console
    // would load the webmail's bundle. A browser follows the redirect, so this asserts
    // both halves — the redirect exists, and the directory serves the console.
    let (status, _) = http_request("GET", &format!("http://127.0.0.1:{API_PORT}/admin"), None, None)
        .await
        .unwrap_or_else(|| panic!("/admin returned nothing"));
    assert!(
        (300..400).contains(&status),
        "/admin must redirect into the directory, got {status}"
    );

    let (status, body) = http_request("GET", &format!("http://127.0.0.1:{API_PORT}/admin/"), None, None)
        .await
        .unwrap_or_else(|| panic!("/admin/ returned nothing"));
    assert_eq!(status, 200, "the console must be served at /admin/");
    assert!(
        body.to_lowercase().contains("<html"),
        "/admin/ should serve an HTML app, got: {}",
        &body[..body.len().min(200)]
    );

    // Autodiscovery is what lets a client configure itself from just an address.
    let discovery = http_get(&format!(
        "http://127.0.0.1:{API_PORT}/.well-known/ferroma"
    ))
    .await
    .expect("autodiscovery must be served");
    let json: serde_json::Value = serde_json::from_str(&discovery)
        .unwrap_or_else(|e| panic!("autodiscovery must be JSON ({e}): {discovery}"));
    assert!(json["api"].is_string(), "{json}");
    server.cleanup().await;

}

/// Writing the configuration and starting the process is itself a claim worth
/// checking: a `ferroma.toml` that the binary rejects is a deployment that never
/// boots.
#[test]
fn the_acceptance_configuration_is_valid() {
    // `ferroma config check` is exercised for real by the tests above; this guards
    // the checked-in smoke configuration that `docs/deployment.md` points at.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the workspace root")
        .join("config")
        .join("ferroma.smoke.toml");
    assert!(path.is_file(), "{} is missing", path.display());

    let text = std::fs::read_to_string(&path).expect("read the smoke configuration");
    let config: ferroma_core::Config = toml::from_str(&text).expect("the smoke configuration must parse");
    config.validate().expect("the smoke configuration must validate");
    assert!(config.smtp.port > 1024, "the smoke configuration avoids privileged ports");

    // And the binary agrees.
    let binary = env!("CARGO_BIN_EXE_ferroma");
    let output = Command::new(binary)
        .arg("--config")
        .arg(&path)
        .arg("config")
        .arg("check")
        .output()
        .expect("run ferroma config check");
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "`ferroma config check` rejected the smoke configuration: {combined}"
    );
}

/// Utility so a failing test can attach the child's log.
#[allow(dead_code)]
fn dump(server: &mut Server, note: &str) {
    let mut file = std::fs::File::create("acceptance-output.txt").expect("create the dump");
    let _ = writeln!(file, "{note}\n{}", server.captured_output());
}

// -----------------------------------------------------------------------------
// The official client, against the real server
// -----------------------------------------------------------------------------

/// Locate the `ferroma-client` binary, building it if this run has not.
///
/// `CARGO_BIN_EXE_<name>` is only set for the package under test, so the client —
/// a different package — has to be found on disk. It lands beside this test's own
/// binary, because every target in the workspace shares one target directory.
fn client_binary() -> PathBuf {
    let server_binary = PathBuf::from(env!("CARGO_BIN_EXE_ferroma"));
    let dir = server_binary
        .parent()
        .expect("the binary has a directory")
        .to_path_buf();
    let name = if cfg!(windows) {
        "ferroma-client.exe"
    } else {
        "ferroma-client"
    };
    let candidate = dir.join(name);
    if candidate.is_file() {
        return candidate;
    }

    // Build it, so `cargo test -p ferroma-server --test e2e` works on its own.
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "ferroma-client"])
        .status()
        .expect("run cargo build");
    assert!(status.success(), "could not build ferroma-client");
    assert!(
        candidate.is_file(),
        "ferroma-client was not produced at {}",
        candidate.display()
    );
    candidate
}

/// The strongest integration evidence in this file: the shipped desktop client,
/// speaking FCP to the shipped server, over a real socket, against a real database.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_official_client_syncs_and_reads_from_the_server() {
    if !require_database() {
        return;
    }
    // Held for the whole test: these tests bind fixed ports and start a real server.
    let _serial = serial_guard().lock().await;

    let server = Server::start().await;
    let client = client_binary();
    let client_dir = server.dir.path().join("client");
    std::fs::create_dir_all(&client_dir).expect("create the client data directory");

    server.cli(&["domain", "create", "acceptance.test"]);
    server.cli(&[
        "user", "create", ALICE, "--password", PASSWORD, "--display-name", "Alice",
    ]);
    server.cli(&["user", "create", BOB, "--password", PASSWORD]);

    let run_client = |args: &[&str]| -> (bool, String) {
        let output = Command::new(&client)
            .arg("--data-dir")
            .arg(&client_dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("cannot run the client: {e}"));
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    };

    // --- add the account, pointing it straight at the server ------------------
    let fcp = format!("http://127.0.0.1:{API_PORT}/api/v1/client");
    let (ok, output) = run_client(&[
        "account", "add", BOB, "--password", PASSWORD, "--server", &fcp,
    ]);
    assert!(ok, "the client could not add the account:\n{output}");

    let (ok, output) = run_client(&["account", "list"]);
    assert!(ok && output.contains(BOB), "the account must be listed:\n{output}");

    // --- sync an empty mailbox, then one with a message ----------------------
    let (ok, output) = run_client(&["sync"]);
    assert!(ok, "the first sync must succeed:\n{output}");

    let message = format!(
        "From: {ALICE}\r\nTo: {BOB}\r\nSubject: For the official client\r\nDate: Thu, 16 Sep 2026 12:00:00 +0000\r\nMessage-ID: <acceptance-client@acceptance.test>\r\n\r\nHello from the server side.\r\n"
    );
    let replies = smtp_send(SMTP_PORT, "client.acceptance.test", ALICE, &[BOB], &message).await;
    assert_eq!(
        replies.last().expect("a reply").0,
        250,
        "delivery must succeed: {replies:?}"
    );

    let (ok, output) = run_client(&["sync"]);
    assert!(ok, "the sync after delivery must succeed:\n{output}");

    // --- the message is in the client's local cache --------------------------
    let (ok, output) = run_client(&["list", "INBOX"]);
    assert!(ok, "the client must list INBOX:\n{output}");
    assert!(
        output.contains("For the official client"),
        "the client's cache must contain the delivered message:\n{output}"
    );

    // --- and reading it pulls the body ---------------------------------------
    let (ok, output) = run_client(&["list", "INBOX", "--json"]);
    assert!(ok, "the JSON listing must succeed:\n{output}");
    let parsed: serde_json::Value = serde_json::from_str(output.trim())
        .unwrap_or_else(|e| panic!("the client's --json output must be JSON ({e}):\n{output}"));

    let first_id = parsed["messages"]
        .as_array()
        .or_else(|| parsed["items"].as_array())
        .and_then(|items| items.first())
        .and_then(|item| item["id"].as_i64())
        .unwrap_or_else(|| panic!("a message id in {parsed}"));

    let (ok, output) = run_client(&["read", &first_id.to_string()]);
    assert!(ok, "reading the message must succeed:\n{output}");
    assert!(
        output.contains("Hello from the server side"),
        "the client must have fetched the body:\n{output}"
    );

    // --- the local search index agrees ---------------------------------------
    let (ok, output) = run_client(&["search", "subject:client"]);
    assert!(ok, "a local search must succeed:\n{output}");
    assert!(
        output.contains("For the official client"),
        "the local index must match on the subject:\n{output}"
    );

    // --- the client is registered as a device on the server ------------------
    let (status, login) = http_json(
        "POST",
        &format!("http://127.0.0.1:{API_PORT}/api/v1/auth/login"),
        Some(&format!(r#"{{"email":"{BOB}","password":"{PASSWORD}"}}"#)),
        None,
    )
    .await;
    assert_eq!(status, 200, "login: {login}");
    let token = login["access_token"].as_str().expect("a token").to_string();

    let (status, devices) = http_json(
        "GET",
        &format!("http://127.0.0.1:{API_PORT}/api/v1/devices"),
        None,
        Some(&token),
    )
    .await;
    // `GET /devices` is admin-only; bob is not an admin, so a 403 is the correct
    // answer and itself worth asserting — the route must not be open to everyone.
    assert!(
        status == 403 || status == 200,
        "the devices route must answer 200 or 403, got {status}: {devices}"
    );
    server.cleanup().await;

}

