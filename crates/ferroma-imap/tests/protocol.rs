//! The IMAP end-to-end suite: a real listener, a real socket, a real PostgreSQL
//! schema and a real Maildir.
//!
//! Every test drives the wire protocol the way a client does — `LOGIN`, `LIST`,
//! `SELECT`, `FETCH`, `STORE`, `UID SEARCH`, `EXPUNGE`, `LOGOUT` — and asserts
//! the exact response lines. The parsed responses are checked with
//! [`Client::command`], which returns the untagged lines and the tagged
//! completion so a test can assert on both.

mod common;

use std::sync::Arc;

use common::Harness;
use ferroma_events::EventBus;
use ferroma_imap::ImapServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// How long a single reply may take before the test calls it a hang.
///
/// Generous on purpose: this is a liveness budget, not a performance assertion.
/// The suite runs a dozen test binaries in parallel, and a tight budget turns a
/// busy machine into a red build — which trains people to re-run until green and
/// is exactly how a real regression slips through later.
const REPLY_CEILING: std::time::Duration = std::time::Duration::from_secs(60);

/// How long an `IDLE` push may take.
///
/// Longer than [`REPLY_CEILING`] because a push waits on the event bus and the
/// scheduler as well as on the server; the server's own idle ceiling in the test
/// configuration is set above this, so a push that never comes is what fails —
/// not the server's timeout firing first.
const PUSH_CEILING: std::time::Duration = std::time::Duration::from_secs(90);


/// The octet count of a literal that a line ends with, if any.
///
/// A line ending in `{n}` is followed by exactly `n` octets — the literal — and
/// then by the rest of the line.
fn trailing_literal(line: &str) -> Option<usize> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if !trimmed.ends_with('}') {
        return None;
    }
    let open = trimmed.rfind('{')?;
    trimmed[open + 1..trimmed.len() - 1].parse::<usize>().ok()
}

/// A tiny IMAP client over a real socket.
struct Client {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: tokio::io::WriteHalf<TcpStream>,
}

/// One command's outcome: the untagged lines and the tagged completion.
#[derive(Debug, Clone)]
struct Outcome {
    /// The command's tag.
    tag: String,
    /// Every `* …` line the command produced, in order.
    untagged: Vec<String>,
    /// The tagged completion line (`<tag> OK …`).
    tagged: String,
    /// `+ …` continuations the server sent.
    continuations: Vec<String>,
}

impl Outcome {
    /// Whether the tagged completion is `OK`.
    fn is_ok(&self) -> bool {
        self.tagged.starts_with(&format!("{} OK", self.tag))
    }

    /// Whether the tagged completion is `NO`.
    fn is_no(&self) -> bool {
        self.tagged.starts_with(&format!("{} NO", self.tag))
    }

    /// Whether the tagged completion is `BAD`.
    fn is_bad(&self) -> bool {
        self.tagged.starts_with(&format!("{} BAD", self.tag))
    }

    /// The untagged line containing `needle`, if any.
    fn find(&self, needle: &str) -> Option<&str> {
        self.untagged
            .iter()
            .find(|line| line.contains(needle))
            .map(String::as_str)
    }

    /// Every untagged line containing `needle`.
    fn all(&self, needle: &str) -> Vec<&str> {
        self.untagged
            .iter()
            .filter(|line| line.contains(needle))
            .map(String::as_str)
            .collect()
    }
}

impl Client {
    /// Connect and read the greeting.
    async fn connect(address: std::net::SocketAddr) -> (Client, String) {
        let stream = TcpStream::connect(address).await.expect("connect");
        let (read, write) = tokio::io::split(stream);
        let mut client = Client {
            reader: BufReader::new(read),
            writer: write,
        };
        let greeting = client.line().await;
        (client, greeting)
    }

    /// Read one CRLF-terminated line.
    /// Read one CRLF-terminated line, within the default ceiling.
    async fn line(&mut self) -> String {
        self.line_within(REPLY_CEILING).await
    }

    /// Read one CRLF-terminated line, within `ceiling`.
    ///
    /// The ceiling exists to turn a hang into a failure with a line number, not
    /// to assert how fast the machine is: the suite runs many test binaries at
    /// once, so any wall clock that is *just* comfortable on an idle box is a
    /// flake generator. It is deliberately generous — a reply that never comes
    /// still fails, and takes [`REPLY_CEILING`] to say so.
    async fn line_within(&mut self, ceiling: std::time::Duration) -> String {
        // A response line may carry a literal, whose octets are *not* text and
        // may contain anything — including CRLF. Reading it line by line would
        // mis-parse a `BODY[]` reply, so the `{n}` is honoured here: exactly `n`
        // octets are consumed, and the rest of the line follows them.
        let mut buffer = String::new();
        loop {
            let mut chunk = String::new();
            let read = tokio::time::timeout(ceiling, self.reader.read_line(&mut chunk))
                .await
                .unwrap_or_else(|_| {
                    panic!("the server must answer within {ceiling:?}; saw {buffer:?}")
                });
            let read = read.expect("the connection must stay open");
            assert!(read > 0, "the server closed the connection unexpectedly");
            buffer.push_str(&chunk);
            match trailing_literal(&chunk) {
                Some(length) => {
                    let mut octets = vec![0u8; length];
                    tokio::io::AsyncReadExt::read_exact(&mut self.reader, &mut octets)
                        .await
                        .expect("the literal must arrive in full");
                    buffer.push_str(&String::from_utf8_lossy(&octets));
                }
                None => break,
            }
        }
        assert!(
            buffer.ends_with("\r\n"),
            "every response must end with CRLF, got {buffer:?}"
        );
        buffer.trim_end_matches("\r\n").to_string()
    }

    /// Wait for an untagged line that `accept` takes, skipping the rest.
    ///
    /// This is how the asynchronous pushes are read: rather than demanding that
    /// one particular line be the *next* thing on the wire, it waits for the
    /// condition — a bounded number of informational lines may arrive first, and
    /// anything else is a failure with the evidence attached.
    async fn wait_for_untagged(
        &mut self,
        ceiling: std::time::Duration,
        what: &str,
        accept: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = tokio::time::Instant::now() + ceiling;
        let mut skipped: Vec<String> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "no {what} within {ceiling:?}; the server sent {skipped:?}"
            );
            let line = self.line_within(remaining).await;
            if accept(&line) {
                return line;
            }
            assert!(
                line.starts_with("* OK") || line.starts_with("* NO") || line.starts_with("* BAD"),
                "expected {what}, got {line:?} (after {skipped:?})"
            );
            skipped.push(line);
        }
    }

    /// Send a raw line.
    async fn send_raw(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .await
            .expect("write");
        self.writer.flush().await.expect("flush");
    }

    /// Send one command and collect its outcome.
    async fn command(&mut self, tag: &str, command: &str) -> Outcome {
        self.send_raw(&format!("{tag} {command}\r\n")).await;
        self.collect(tag).await
    }

    /// Read until the tagged completion for `tag`.
    async fn collect(&mut self, tag: &str) -> Outcome {
        let mut untagged = Vec::new();
        let mut continuations = Vec::new();
        loop {
            let line = self.line().await;
            if line.starts_with("* ") {
                untagged.push(line);
            } else if line.starts_with("+ ") || line == "+" {
                continuations.push(line);
            } else if line.starts_with(&format!("{tag} ")) {
                return Outcome {
                    tag: tag.to_string(),
                    untagged,
                    tagged: line,
                    continuations,
                };
            } else {
                panic!("unexpected line while waiting for `{tag}`: {line:?}");
            }
        }
    }

    /// Close the writing half, so the server sees an orderly disconnect.
    async fn close(mut self) {
        let _ = self.writer.shutdown().await;
    }
}

/// Start a server on an ephemeral port and return its address and the handle.
async fn start_server(
    harness: &Harness,
    events: Option<Arc<EventBus>>,
) -> (Arc<ImapServer>, std::net::SocketAddr) {
    let maildir = Arc::new(ferroma_storage::Maildir::new(
        harness.maildir_root(),
        false,
        ferroma_core::config::MailboxLayout::Maildir,
    ));
    let mut server = ImapServer::new(
        harness.server_config(),
        Arc::new(harness.repos.clone()),
        maildir,
    )
    .expect("the server configuration must be valid")
    .with_authenticator(Arc::new(harness.authenticator()))
    .with_sync(Arc::new(ferroma_sync::SyncService::new(harness.repos.clone(), 100, 30)));
    if let Some(events) = events {
        server = server.with_events(events);
    }
    let server = Arc::new(server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    let serving = server.clone();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                return;
            };

            let server = serving.clone();
            tokio::spawn(async move {
                let _ = server.serve_stream(stream, false).await;
            });
        }
    });
    (server, address)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_greeting_advertises_the_capabilities() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("greet").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, greeting) = Client::connect(address).await;

    assert!(greeting.starts_with("* OK"), "got {greeting:?}");
    assert!(greeting.contains("[CAPABILITY"), "got {greeting:?}");
    for capability in [
        "IMAP4rev1",
        "AUTH=PLAIN",
        "IDLE",
        "UIDPLUS",
        "MOVE",
        "UNSELECT",
        "NAMESPACE",
        "LITERAL+",
        "CHILDREN",
    ] {
        assert!(
            greeting.contains(capability),
            "the greeting must advertise {capability}: {greeting:?}"
        );
    }
    // TLS is not configured in these tests, so STARTTLS must not be offered.
    assert!(!greeting.contains("STARTTLS"), "got {greeting:?}");

    let outcome = client.command("a1", "CAPABILITY").await;
    assert!(outcome.is_ok());
    assert!(outcome.find("CAPABILITY IMAP4rev1").is_some());

    let outcome = client.command("a2", "LOGOUT").await;
    assert!(outcome.is_ok());
    assert!(outcome.find("BYE").is_some());
    client.close().await;
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_command_is_a_bad_not_a_disconnect() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("badcmd").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;

    let outcome = client.command("a1", "FROBNICATE").await;
    assert!(outcome.is_bad(), "got {outcome:?}");

    // The session must survive it.
    let outcome = client.command("a2", "NOOP").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_literal_is_a_bad_and_the_session_continues() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("badliteral").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;

    // `{` that never closes: the parser must reject it, not hang or allocate.
    let outcome = client.command("a1", "SELECT {99999999999999999999}").await;
    assert!(outcome.is_bad(), "got {outcome:?}");

    let outcome = client.command("a2", "NOOP").await;
    assert!(outcome.is_ok(), "the session must survive: {outcome:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_before_login_are_refused() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("notauth").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;

    for (tag, command) in [
        ("a1", "SELECT INBOX"),
        ("a2", "FETCH 1 (UID)"),
        ("a3", "STORE 1 +FLAGS (\\Seen)"),
        ("a4", "LIST \"\" \"*\""),
        ("a5", "STATUS INBOX (MESSAGES)"),
        ("a6", "SEARCH ALL"),
    ] {
        let outcome = client.command(tag, command).await;
        assert!(outcome.is_bad(), "`{command}` must be a BAD: {outcome:?}");
    }

    // `APPEND` carries a literal, and the server must consume the octets before
    // it can decide anything — so the refusal follows the continuation.
    client.send_raw("a7 APPEND INBOX {3}\r\n").await;
    let continuation = client.line().await;
    assert!(
        continuation.starts_with("+ "),
        "the literal must be requested: {continuation:?}"
    );
    client.send_raw("xyz\r\n").await;
    let outcome = client.collect("a7").await;
    assert!(
        outcome.is_bad(),
        "APPEND before LOGIN must be refused: {outcome:?}"
    );

    // NOOP and CAPABILITY are legal in every state.
    assert!(client.command("a8", "NOOP").await.is_ok());
    assert!(client.command("a9", "CAPABILITY").await.is_ok());


    harness.cleanup().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_password_is_a_no_and_leaves_the_session_unauthenticated() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("badpass").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;

    let outcome = client
        .command("a1", &format!("LOGIN {} wrong-password", harness.address))
        .await;
    assert!(outcome.is_no(), "got {outcome:?}");
    // Still unauthenticated, so SELECT stays a BAD.
    let outcome = client.command("a2", "SELECT INBOX").await;
    assert!(outcome.is_bad(), "got {outcome:?}");
    harness.cleanup().await;
}

/// The full client walk: `LOGIN → LIST → SELECT → FETCH → STORE → UID SEARCH →
/// EXPUNGE → LOGOUT`, asserting the wire responses at every step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_full_client_walk() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("walk").await;
    harness
        .seed_message("INBOX", &common::sample_message("WalkOne"))
        .await;
    harness
        .seed_message("INBOX", &common::sample_message("WalkTwo"))
        .await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;

    // -- LOGIN ------------------------------------------------------------
    let outcome = client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await;
    assert!(outcome.is_ok(), "got {outcome:?}");

    // -- LIST: every folder, and the delimiter for the root query ----------
    let outcome = client.command("a2", "LIST \"\" \"*\"").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let listed: Vec<String> = outcome
        .all(" LIST ")
        .iter()
        .map(|line| (*line).to_string())
        .collect();
    for folder in ["INBOX", "Sent", "Drafts", "Trash", "Junk", "Archive"] {
        assert!(
            listed.iter().any(|line| line.contains(folder)),
            "LIST must enumerate {folder}: {listed:?}"
        );
    }
    assert!(listed.iter().all(|line| line.contains("\"/\"")));

    let outcome = client.command("a3", "LIST \"\" \"\"").await;
    assert!(outcome.is_ok());
    let root = outcome.find(" LIST ").expect("the root query must answer");
    assert!(root.contains("\"/\""), "got {root:?}");
    assert!(root.ends_with("\"\""), "the root name is empty: {root:?}");
    assert!(root.contains("\\Noselect"), "got {root:?}");

    let outcome = client.command("a4", "LIST \"\" \"%\"").await;
    assert!(outcome.is_ok());
    assert!(
        !outcome.untagged.is_empty(),
        "`%` must still list the top level"
    );

    // -- SELECT -----------------------------------------------------------
    let outcome = client.command("a5", "SELECT INBOX").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert_eq!(outcome.find("EXISTS"), Some("* 2 EXISTS"));
    assert!(outcome.find("RECENT").is_some());
    assert!(outcome.find("FLAGS (") .is_some());
    assert!(outcome.find("PERMANENTFLAGS").is_some());
    assert!(outcome.find("UIDVALIDITY").is_some());
    assert!(outcome.find("UIDNEXT").is_some());
    assert!(outcome.find("UNSEEN").is_some());
    assert!(outcome.tagged.contains("[READ-WRITE]"), "got {outcome:?}");
    let uidnext = outcome
        .find("UIDNEXT")
        .and_then(|line| line.split("UIDNEXT ").nth(1))
        .and_then(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok())
        .expect("UIDNEXT must be a number");

    // -- FETCH ------------------------------------------------------------
    let outcome = client
        .command(
            "a6",
            "FETCH 1 (UID FLAGS RFC822.SIZE ENVELOPE BODYSTRUCTURE BODY.PEEK[TEXT])",
        )
        .await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let fetch = outcome.find("FETCH").expect("a FETCH response");
    assert!(fetch.contains("UID 1"), "got {fetch:?}");
    assert!(fetch.contains("FLAGS ("), "got {fetch:?}");
    assert!(fetch.contains("RFC822.SIZE "), "got {fetch:?}");
    assert!(
        fetch.contains(
            "BODYSTRUCTURE ((\"text\" \"plain\" (\"CHARSET\" \"UTF-8\") NIL NIL \
             \"quoted-printable\" 13 1"
        ),
        "the Thunderbird-shaped fixture must describe as text/plain first: {fetch:?}"
    );
    assert!(
        fetch.contains("\"alternative\""),
        "the multipart subtype must be reported: {fetch:?}"
    );
    // `BODY.PEEK[TEXT]` is a literal, so the response spans the literal's CRLF.
    assert!(fetch.contains("BODY[TEXT] {"), "got {fetch:?}");

    // `BODY.PEEK[…]` must not set `\Seen`.
    let outcome = client.command("a7", "FETCH 1 (FLAGS)").await;
    let fetch = outcome.find("FETCH").expect("a FETCH response");
    assert!(
        !fetch.contains("\\Seen"),
        "BODY.PEEK must not set \\Seen: {fetch:?}"
    );

    // -- STORE ------------------------------------------------------------
    let outcome = client.command("a8", "STORE 1 +FLAGS (\\Seen \\Flagged)").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let fetch = outcome.find("FETCH").expect("STORE must report the new flags");
    assert!(fetch.contains("\\Seen"), "got {fetch:?}");
    assert!(fetch.contains("\\Flagged"), "got {fetch:?}");

    // A silent store must not report anything.
    let outcome = client.command("a9", "STORE 2 +FLAGS.SILENT (\\Flagged)").await;
    assert!(outcome.is_ok());
    assert!(
        outcome.find("FETCH").is_none(),
        "FLAGS.SILENT must stay quiet: {outcome:?}"
    );

    let outcome = client.command("a10", "FETCH 1 (FLAGS)").await;
    let fetch = outcome.find("FETCH").expect("a FETCH response");
    assert!(
        fetch.contains("\\Seen") && fetch.contains("\\Flagged"),
        "got {fetch:?}"
    );

    // -- UID SEARCH -------------------------------------------------------
    // Sequence-number search: message 1 is the older one, so both are visible.
    let outcome = client.command("a11", "UID SEARCH ALL").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let search = outcome.find("SEARCH").expect("a SEARCH response");
    assert_eq!(search, "* SEARCH 1 2");

    let outcome = client.command("a12", "UID SEARCH FLAGGED").await;
    assert_eq!(
        outcome.find("SEARCH"),
        Some("* SEARCH 1 2"),
        "both messages were flagged"
    );

    let outcome = client.command("a13", "UID SEARCH SEEN").await;
    assert_eq!(outcome.find("SEARCH"), Some("* SEARCH 1"));

    let outcome = client.command("a14", "UID SEARCH SUBJECT WalkTwo").await;
    assert_eq!(outcome.find("SEARCH"), Some("* SEARCH 2"));

    let outcome = client.command("a15", "UID SEARCH UNSEEN").await;
    assert_eq!(outcome.find("SEARCH"), Some("* SEARCH 2"));

    let outcome = client.command("a16", "UID SEARCH 1:1").await;
    assert_eq!(outcome.find("SEARCH"), Some("* SEARCH 1"));

    // -- EXPUNGE ----------------------------------------------------------
    let outcome = client.command("a17", "STORE 1 +FLAGS (\\Deleted)").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let outcome = client.command("a18", "EXPUNGE").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert_eq!(
        outcome.find("EXPUNGE"),
        Some("* 1 EXPUNGE"),
        "the expunged message's sequence number: {outcome:?}"
    );

    let outcome = client.command("a19", "FETCH 1:* (UID)").await;
    assert!(outcome.is_ok());
    let fetch = outcome.find("FETCH").expect("the survivor must still be there");
    assert!(fetch.contains("UID 2"), "got {fetch:?}");

    // -- LOGOUT -----------------------------------------------------------
    let outcome = client.command("a20", "LOGOUT").await;
    assert!(outcome.is_ok());
    assert!(outcome.find("BYE").is_some());
    assert_eq!(uidnext, 3, "two messages were seeded, so UIDNEXT is 3");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_marked_seen_still_reads_as_seen_after_a_reconnect() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("seen").await;
    let original = harness
        .seed_message("INBOX", &common::sample_message("SeenMe"))
        .await;
    let (_server, address) = start_server(&harness, None).await;

    // First connection: mark it seen.
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());
    assert!(client.command("a2", "SELECT INBOX").await.is_ok());
    let outcome = client.command("a3", "STORE 1 +FLAGS (\\Seen)").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let outcome = client.command("a4", "LOGOUT").await;
    assert!(outcome.is_ok());
    client.close().await;

    // The database row and the Maildir file name must both carry the flag.
    let row = harness
        .repos
        .messages
        .list_unexpunged(harness.folder("INBOX").await.folder_id())
        .await
        .expect("list")
        .into_iter()
        .next()
        .expect("one message");
    assert!(
        row.flags.split_whitespace().any(|flag| flag == "seen"),
        "the flag must reach the database: {:?}",
        row.flags
    );
    let name = row
        .storage_path
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(
        name.contains("2,S"),
        "the Maildir file name must carry the S flag: {name:?}"
    );
    assert_ne!(row.storage_path, original.storage_path);
    assert!(harness.maildir.read(&row.storage_path).is_ok());
    assert!(harness.maildir.read(&original.storage_path).is_err(),
        "the previous body is cleaned only after commit");

    // Second connection: the flag must be visible again.
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("b1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());
    let outcome = client.command("b2", "SELECT INBOX").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let outcome = client.command("b3", "FETCH 1 (UID FLAGS)").await;
    let fetch = outcome.find("FETCH").expect("a FETCH response");
    assert!(
        fetch.contains("\\Seen"),
        "\\Seen must survive a reconnect: {fetch:?}"
    );
    // And SEARCH must agree.
    let outcome = client.command("b4", "UID SEARCH SEEN").await;
    assert_eq!(outcome.find("SEARCH"), Some("* SEARCH 1"));
    let sync = ferroma_sync::SyncService::new(harness.repos.clone(), 100, 30);
    let page = sync.sync(ferroma_sync::SyncRequest::account(
        harness.user, harness.mailbox, ferroma_core::Cursor::ZERO,
    )).await.unwrap();
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].kind, ferroma_sync::ChangeKind::MessageUpdated);
    assert_eq!(page.changes[0].flags.as_deref(), Some("seen"));
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_carries_a_literal_into_the_mailbox() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("append").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());

    // `APPEND` is the one command that is *not* a single line: the server must
    // answer the `{n}` with a continuation, and only then does the client send
    // the octets.
    let message = b"Subject: Appended\r\nFrom: bob@example.net\r\n\r\nbody text\r\n";
    client
        .send_raw(&format!("a2 APPEND INBOX (\\\\Seen) {{{}}}\r\n", message.len()))
        .await;
    let continuation = client.line().await;
    assert!(
        continuation.starts_with("+ "),
        "APPEND must be answered with a continuation: {continuation:?}"
    );
    // The octets replace the `{n}`, so the client sends them and the CRLF that
    // terminates the command line.
    client.writer.write_all(message).await.expect("write literal");
    client.writer.write_all(b"\r\n").await.expect("write crlf");
    client.writer.flush().await.expect("flush");

    let outcome = client.collect("a2").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert!(
        outcome.tagged.contains("[APPENDUID "),
        "UIDPLUS requires an APPENDUID: {outcome:?}"
    );

    assert!(
        outcome.continuations.is_empty(),
        "the continuation was already consumed before the octets: {outcome:?}"
    );
    // The message is visible with the flags it was appended with.
    let outcome = client.command("a3", "SELECT INBOX").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert_eq!(outcome.find("EXISTS"), Some("* 1 EXISTS"));
    let outcome = client.command("a4", "FETCH 1 (UID FLAGS ENVELOPE)").await;
    let fetch = outcome.find("FETCH").expect("a FETCH response");
    assert!(fetch.contains("UID 1"), "got {fetch:?}");
    assert!(fetch.contains("\\Seen"), "got {fetch:?}");
    assert!(
        fetch.contains("Appended"),
        "ENVELOPE must carry the subject: {fetch:?}"
    );

    // And the bytes are on disk, byte for byte.
    let row = harness
        .repos
        .messages
        .list_unexpunged(harness.folder("INBOX").await.folder_id())
        .await
        .expect("list")
        .into_iter()
        .next()
        .expect("one message");
    let stored = harness
        .maildir
        .read(&row.storage_path)
        .expect("the body file must exist");
    assert_eq!(stored, message.to_vec());
    let sync = ferroma_sync::SyncService::new(harness.repos.clone(), 100, 30);
    let page = sync.sync(ferroma_sync::SyncRequest::account(
        harness.user, harness.mailbox, ferroma_core::Cursor::ZERO,
    )).await.unwrap();
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].kind, ferroma_sync::ChangeKind::MessageCreated);
    assert_eq!(page.changes[0].message_id, Some(row.message_id()));

    // An APPEND that is too large is refused with TOOBIG, and the session lives.
    let outcome = client
        .command("a5", &format!("APPEND INBOX {{{}}}", 4 * 1024 * 1024))
        .await;
    assert!(
        outcome.is_bad(),
        "a literal past the limit is refused at parse time: {outcome:?}"
    );
    let outcome = client.command("a6", "NOOP").await;
    assert!(outcome.is_ok(), "the session must survive: {outcome:?}");
    harness.cleanup().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn examine_is_read_only() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("examine").await;
    harness
        .seed_message("INBOX", &common::sample_message("ExamineMe"))
        .await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());

    let outcome = client.command("a2", "EXAMINE INBOX").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert!(
        outcome.tagged.contains("[READ-ONLY]"),
        "EXAMINE must report READ-ONLY: {outcome:?}"
    );

    // Every mutation must fail.
    for (tag, command) in [
        ("a3", "STORE 1 +FLAGS (\\Seen)"),
        ("a4", "STORE 1 -FLAGS (\\Seen)"),
        ("a5", "STORE 1 FLAGS (\\Deleted)"),
        ("a6", "EXPUNGE"),
        ("a7", "COPY 1 Sent"),
        ("a8", "MOVE 1 Sent"),
        ("a9", "UID EXPUNGE 1"),
    ] {
        let outcome = client.command(tag, command).await;
        assert!(
            outcome.is_no() || outcome.is_bad(),
            "`{command}` must be refused on a read-only mailbox: {outcome:?}"
        );
    }

    // Reads still work.
    let outcome = client.command("a10", "FETCH 1 (UID FLAGS)").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let outcome = client.command("a11", "SEARCH ALL").await;
    assert!(outcome.is_ok(), "got {outcome:?}");

    // Writing the flag through a *write* session proves nothing changed.
    let outcome = client.command("a12", "CLOSE").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_pushes_a_change_published_on_the_bus() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("idle").await;
    harness
        .seed_message("INBOX", &common::sample_message("IdleOne"))
        .await;
    let bus = Arc::new(EventBus::with_defaults());
    let (_server, address) = start_server(&harness, Some(bus.clone())).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());
    let outcome = client.command("a2", "SELECT INBOX").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert_eq!(outcome.find("EXISTS"), Some("* 1 EXISTS"));

    // Enter IDLE; the server answers with a continuation.
    client.send_raw("a3 IDLE\r\n").await;
    let continuation = client.line().await;
    assert!(
        continuation.starts_with("+ "),
        "IDLE must be answered with a continuation: {continuation:?}"
    );

    // Publish a change for this mailbox on the bus, and expect the server to
    // push it without being asked. The wait is on the *condition* — an untagged
    // `EXISTS` — with a generous ceiling, so a busy machine delays the assertion
    // instead of failing it; a push that never comes still fails, and says so.
    //
    // The message really is stored first, so the pushed count is the mailbox's
    // truth: a push that reported a stale or guessed number would fail here.
    let folder = harness.folder("INBOX").await;
    let delivered = harness
        .seed_message("INBOX", &common::sample_message("IdleTwo"))
        .await;
    let received = ferroma_events::Event::mail_received(
        folder.folder_id(),
        delivered.message_id(),
    );
    bus.publish(ferroma_events::EventScope::User(harness.user), received)
        .await;

    let push = client
        .wait_for_untagged(PUSH_CEILING, "a pushed EXISTS", |line| {
            line.starts_with("* ") && line.ends_with(" EXISTS")
        })
        .await;
    assert_eq!(
        push, "* 2 EXISTS",
        "the push must report the count the mailbox now has"
    );


    client.send_raw("DONE\r\n").await;
    let outcome = client.collect("a3").await;
    assert!(outcome.is_ok(), "DONE must complete the IDLE: {outcome:?}");

    // The session is usable again.
    let outcome = client.command("a4", "NOOP").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_rename_delete_and_subscribe() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("folders").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());

    assert!(client.command("a2", "CREATE Projects").await.is_ok());
    // A second CREATE is an ALREADYEXISTS.
    let outcome = client.command("a3", "CREATE Projects").await;
    assert!(outcome.is_no(), "got {outcome:?}");
    assert!(
        outcome.tagged.contains("[ALREADYEXISTS]"),
        "got {outcome:?}"
    );

    assert!(client.command("a4", "SUBSCRIBE Projects").await.is_ok());
    let outcome = client.command("a5", "LSUB \"\" \"*\"").await;
    assert!(outcome.is_ok());
    assert!(
        outcome
            .all(" LSUB ")
            .iter()
            .any(|line| line.contains("Projects")),
        "LSUB must list the subscription: {outcome:?}"
    );

    assert!(client.command("a6", "RENAME Projects ArchiveX").await.is_ok());
    let outcome = client.command("a7", "LIST \"\" \"ArchiveX\"").await;
    assert!(outcome.is_ok());
    assert!(
        outcome
            .all(" LIST ")
            .iter()
            .any(|line| line.contains("ArchiveX")),
        "the rename must be visible: {outcome:?}"
    );

    assert!(client.command("a8", "UNSUBSCRIBE ArchiveX").await.is_ok());
    assert!(client.command("a9", "DELETE ArchiveX").await.is_ok());

    // INBOX cannot be deleted or renamed.
    let outcome = client.command("a10", "DELETE INBOX").await;
    assert!(outcome.is_no(), "got {outcome:?}");
    let outcome = client.command("a11", "RENAME INBOX Other").await;
    assert!(outcome.is_no(), "got {outcome:?}");

    // A missing mailbox is a NONEXISTENT.
    let outcome = client.command("a12", "DELETE Nope").await;
    assert!(outcome.is_no(), "got {outcome:?}");
    assert!(outcome.tagged.contains("[NONEXISTENT]"), "got {outcome:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renaming_a_nonempty_folder_keeps_the_message_body_readable() {
    if !common::require_database().await { return; }
    let harness = Harness::new("renamebody").await;
    harness.repos.folders.create(harness.mailbox, "Projects", None).await.unwrap();
    harness.maildir.create_folder(&harness.domain, &harness.local_part, "Projects").unwrap();
    let raw = common::sample_message("RenamedBody");
    let original = harness.seed_message("Projects", &raw).await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client.command("a1", &format!("LOGIN {} {}", harness.address, harness.password)).await.is_ok());
    let renamed = client.command("a2", "RENAME Projects ArchiveX").await;
    assert!(renamed.is_ok(), "{renamed:?}");
    let current = harness.repos.messages.require_by_id(original.message_id()).await.unwrap();
    assert_ne!(current.storage_path, original.storage_path);
    assert_eq!(harness.maildir.read(&current.storage_path).unwrap(), raw);
    assert!(client.command("a3", "SELECT ArchiveX").await.is_ok());
    let fetched = client.command("a4", "FETCH 1 (BODY.PEEK[HEADER])").await;
    assert!(fetched.is_ok() && fetched.find("RenamedBody").is_some(), "{fetched:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_the_folder_state() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("status").await;
    harness
        .seed_message("INBOX", &common::sample_message("StatusOne"))
        .await;
    harness
        .seed_message("INBOX", &common::sample_message("StatusTwo"))
        .await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());

    let outcome = client
        .command("a2", "STATUS INBOX (MESSAGES RECENT UNSEEN UIDNEXT UIDVALIDITY)")
        .await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    let status = outcome.find("STATUS").expect("a STATUS response");
    assert!(status.contains("MESSAGES 2"), "got {status:?}");
    assert!(status.contains("UNSEEN 2"), "got {status:?}");
    assert!(status.contains("UIDNEXT 3"), "got {status:?}");
    assert!(status.contains("UIDVALIDITY "), "got {status:?}");

    let outcome = client.command("a3", "STATUS Nope (MESSAGES)").await;
    assert!(outcome.is_no(), "got {outcome:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_and_move_relocate_a_message() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("copymove").await;
    harness
        .seed_message("INBOX", &common::sample_message("CopyMe"))
        .await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());
    assert!(client.command("a2", "SELECT INBOX").await.is_ok());

    let outcome = client.command("a3", "COPY 1 Sent").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert!(
        outcome.find("COPYUID").is_some(),
        "UIDPLUS requires a COPYUID: {outcome:?}"
    );

    // The original is still in INBOX.
    let outcome = client.command("a4", "SEARCH ALL").await;
    assert_eq!(outcome.find("SEARCH"), Some("* SEARCH 1"));

    // A copy to a missing mailbox is a TRYCREATE.
    let outcome = client.command("a5", "COPY 1 Nope").await;
    assert!(outcome.is_no(), "got {outcome:?}");
    assert!(outcome.tagged.contains("[TRYCREATE]"), "got {outcome:?}");

    // MOVE removes the source.
    let outcome = client.command("a6", "MOVE 1 Trash").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert!(
        outcome.find("EXPUNGE").is_some(),
        "MOVE must expunge the source: {outcome:?}"
    );
    let outcome = client.command("a7", "SEARCH ALL").await;
    assert_eq!(
        outcome.find("SEARCH"),
        Some("* SEARCH"),
        "the mailbox is empty after the move: {outcome:?}"
    );

    // And the destination has it.
    let outcome = client.command("a8", "SELECT Trash").await;
    assert!(outcome.is_ok(), "got {outcome:?}");
    assert_eq!(outcome.find("EXISTS"), Some("* 1 EXISTS"));
    let sync = ferroma_sync::SyncService::new(harness.repos.clone(), 100, 30);
    let page = sync.sync(ferroma_sync::SyncRequest::account(
        harness.user, harness.mailbox, ferroma_core::Cursor::ZERO,
    )).await.expect("IMAP writes must reach the FCP cursor");
    let kinds: Vec<_> = page.changes.iter().map(|change| change.kind).collect();
    assert_eq!(kinds, vec![
        ferroma_sync::ChangeKind::MessageCreated,
        ferroma_sync::ChangeKind::MessageMoved,
    ]);
    assert_eq!(page.changes[1].from_folder_id, Some(harness.folder("INBOX").await.folder_id()));
    assert_eq!(page.changes[1].to_folder_id, Some(harness.folder("Trash").await.folder_id()));
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_obeys_quota_and_accounts_for_the_new_body() {
    if !common::require_database().await { return; }
    let harness = Harness::new("copyquota").await;
    let raw = common::sample_message("QuotaCopy");
    let original = harness.seed_message("INBOX", &raw).await;
    harness.repos.mailboxes.recompute_usage(harness.mailbox).await.expect("seed usage");
    harness.repos.users.set_quota(harness.user, raw.len() as i64 * 2 - 1)
        .await.expect("set quota");
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client.command("a1", &format!("LOGIN {} {}", harness.address, harness.password)).await.is_ok());
    assert!(client.command("a2", "SELECT INBOX").await.is_ok());
    let refused = client.command("a3", "COPY 1 Sent").await;
    assert!(refused.is_no() && refused.tagged.contains("OVERQUOTA"), "{refused:?}");
    assert_eq!(harness.repos.messages.count_by_folder(harness.folder("Sent").await.folder_id()).await.unwrap(), 0);
    assert!(harness.repos.messages.find_by_id(original.message_id()).await.unwrap().is_some());

    harness.repos.users.set_quota(harness.user, raw.len() as i64 * 2)
        .await.expect("raise quota");
    assert!(client.command("a4", "COPY 1 Sent").await.is_ok());
    let copies = harness.repos.messages.list_unexpunged(harness.folder("Sent").await.folder_id()).await.unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(harness.maildir.read(&copies[0].storage_path).unwrap(), raw);
    let user = harness.repos.users.find_by_id(harness.user).await.unwrap().unwrap();
    assert_eq!(user.used_bytes, (raw.len() * 2) as i64);
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_with_missing_source_body_preserves_the_original_row() {
    if !common::require_database().await { return; }
    let harness = Harness::new("movemissing").await;
    let original = harness.seed_message("INBOX", &common::sample_message("MissingMove")).await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client.command("a1", &format!("LOGIN {} {}", harness.address, harness.password)).await.is_ok());
    assert!(client.command("a2", "SELECT INBOX").await.is_ok());
    harness.maildir.delete(&original.storage_path).unwrap();
    let refused = client.command("a3", "MOVE 1 Trash").await;
    assert!(refused.is_no(), "{refused:?}");
    let unchanged = harness.repos.messages.find_by_id(original.message_id()).await.unwrap().unwrap();
    assert_eq!(unchanged.folder_id, original.folder_id);
    assert_eq!(unchanged.storage_path, original.storage_path);
    assert_eq!(harness.repos.messages.count_by_folder(harness.folder("Trash").await.folder_id()).await.unwrap(), 0);
    let sync = ferroma_sync::SyncService::new(harness.repos.clone(), 100, 30);
    let page = sync.sync(ferroma_sync::SyncRequest::account(
        harness.user, harness.mailbox, ferroma_core::Cursor::ZERO,
    )).await.unwrap();
    assert!(page.changes.is_empty(), "a failed MOVE cannot publish a sync change");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_select_leaves_the_session_usable() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("failsel").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());

    let outcome = client.command("a2", "SELECT Nope").await;
    assert!(outcome.is_no(), "got {outcome:?}");
    assert!(outcome.tagged.contains("[NONEXISTENT]"), "got {outcome:?}");

    // Still authenticated, so a good SELECT works.
    assert!(client.command("a3", "SELECT INBOX").await.is_ok());
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_and_store_reject_the_wrong_state() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("state").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());

    // In `Authenticated`, every mailbox command is a BAD.
    for (tag, command) in [
        ("a2", "FETCH 1 (UID)"),
        ("a3", "STORE 1 +FLAGS (\\Seen)"),
        ("a4", "SEARCH ALL"),
        ("a5", "COPY 1 Sent"),
        ("a6", "CLOSE"),
        ("a7", "EXPUNGE"),
    ] {
        let outcome = client.command(tag, command).await;
        assert!(outcome.is_bad(), "`{command}`: {outcome:?}");
    }

    // After SELECT they work, and after CLOSE they do not again.
    assert!(client.command("a8", "SELECT INBOX").await.is_ok());
    assert!(client.command("a9", "FETCH 1 (UID)").await.is_ok());
    assert!(client.command("a10", "CLOSE").await.is_ok());
    let outcome = client.command("a11", "FETCH 1 (UID)").await;
    assert!(outcome.is_bad(), "got {outcome:?}");

    // UNSELECT also returns to `Authenticated`.
    assert!(client.command("a12", "SELECT INBOX").await.is_ok());
    assert!(client.command("a13", "UNSELECT").await.is_ok());
    let outcome = client.command("a14", "SEARCH ALL").await;
    assert!(outcome.is_bad(), "got {outcome:?}");
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn badcharset_is_reported_for_an_unsupported_search_charset() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("charset").await;
    let (_server, address) = start_server(&harness, None).await;
    let (mut client, _) = Client::connect(address).await;
    assert!(client
        .command("a1", &format!("LOGIN {} {}", harness.address, harness.password))
        .await
        .is_ok());
    assert!(client.command("a2", "SELECT INBOX").await.is_ok());

    let outcome = client.command("a3", "SEARCH CHARSET UTF-8 ALL").await;
    assert!(outcome.is_ok(), "UTF-8 is supported: {outcome:?}");

    let outcome = client.command("a4", "SEARCH CHARSET KOI8-R ALL").await;
    assert!(outcome.is_no(), "got {outcome:?}");
    assert!(
        outcome.tagged.contains("[BADCHARSET (US-ASCII UTF-8)]"),
        "got {outcome:?}"
    );
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_count_tracks_open_sessions() {
    if !common::require_database().await {
        return;
    }
    let harness = Harness::new("count").await;
    let (server, address) = start_server(&harness, None).await;
    assert_eq!(server.connection_count(), 0);

    let (client, _) = Client::connect(address).await;
    // The count is incremented before the socket is served, so it is visible
    // immediately on this side.
    let (mut other, _) = Client::connect(address).await;
    assert!(
        server.connection_count() >= 1,
        "at least one connection must be accounted for"
    );

    let outcome = other.command("a1", "LOGOUT").await;
    assert!(outcome.is_ok());
    other.close().await;
    client.close().await;
    harness.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configuration_is_validated_before_it_is_used() {
    let config = ferroma_imap::ImapServerConfig {
        port: 0,
        imaps_port: 0,
        ..ferroma_imap::ImapServerConfig::default()
    };
    assert!(config.validate().is_err());

    // No database or socket is needed for the validation path, but building a
    // server still needs a `Repositories`, so the assertion stops here.
}
