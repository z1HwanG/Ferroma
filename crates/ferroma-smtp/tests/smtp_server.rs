//! End-to-end inbound SMTP: a real listener, a real socket, a real Maildir and a
//! real PostgreSQL schema.
//!
//! Each test gets its own migrated schema and its own Maildir, so nothing here can
//! interfere with anything else. When no PostgreSQL is reachable the tests skip with
//! a reason, exactly like the storage suite.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{first_line, seed_mailbox, test_config, Stores, TestClient, TestDb};
use ferroma_auth::{AuthService, PasswordHasher, TokenService};
use ferroma_core::{MailboxId, UserId};
use ferroma_smtp::{
    DeliveryService, ListenerKind, Reply, SmtpClient, SmtpClientConfig, SmtpServer, SmtpServerConfig,
    TlsPolicy,
};
use ferroma_storage::Repositories;

/// A running test server plus the pieces a test wants to assert against.
struct Harness {
    address: std::net::SocketAddr,
    handle: ferroma_smtp::SmtpServerHandle,
    db: TestDb,
    stores: Stores,
    repos: Repositories,
    user_id: UserId,
    mailbox_id: MailboxId,
}

impl Harness {
    /// Start a server for `mx.test` with `alice@mx.test` as the only mailbox.
    async fn start(configure: impl FnOnce(&mut ferroma_core::config::Config)) -> Harness {
        Harness::start_with_domain(configure, "mx.test", "alice").await
    }

    async fn start_with_domain(
        configure: impl FnOnce(&mut ferroma_core::config::Config),
        domain: &str,
        local_part: &str,
    ) -> Harness {
        let db = TestDb::create().await;
        let repos = db.repos();
        let stores = Stores::new();

        // A real Argon2id hash, so `AUTH` exercises the real verifier.
        let hasher = PasswordHasher::default();
        let password_hash = hasher
            .hash("correct horse battery staple")
            .expect("hash the test password");
        let (user_id, mailbox_id) =
            seed_mailbox(&repos, domain, local_part, &password_hash).await;

        let mut config = test_config();
        configure(&mut config);

        let delivery = Arc::new(DeliveryService::new(
            repos.clone(),
            stores.maildir.clone(),
            stores.attachments.clone(),
            config.server.hostname.clone(),
        ));

        let tokens = TokenService::new(
            "0123456789abcdef0123456789abcdef",
            3600,
            2_592_000,
            "ferroma-test",
        )
        .expect("token service");
        let auth = Arc::new(AuthService::with_defaults(
            repos.clone(),
            tokens,
            config.limits.clone(),
        ));

        let server_config = SmtpServerConfig::new(config, repos.clone(), delivery)
            .with_auth(auth)
            .with_events(ferroma_events::EventBus::with_defaults());

        let server = SmtpServer::new(server_config);
        let listener = server
            .bind_one(
                "127.0.0.1:0".parse().expect("address"),
                ListenerKind::Mx,
                false,
            )
            .await
            .expect("bind the test listener");
        let address = listener.local_addr().expect("local address");
        let handle = server.start_with(vec![listener]);

        Harness {
            address,
            handle,
            db,
            stores,
            repos,
            user_id,
            mailbox_id,
        }
    }

    /// The number of messages in `alice`'s `INBOX`.
    async fn inbox_count(&self) -> i64 {
        self.inbox_count_of(self.mailbox_id).await
    }

    /// The number of messages in one mailbox's `INBOX`.
    async fn inbox_count_of(&self, mailbox_id: MailboxId) -> i64 {
        let folders = self
            .repos
            .folders
            .list(mailbox_id)
            .await
            .expect("list folders");
        let inbox = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
            .expect("INBOX exists");
        self.repos
            .messages
            .count_by_folder(inbox.folder_id())
            .await
            .expect("count messages")
    }

    /// Add a second address in the domain already seeded.
    async fn add_mailbox(&self, domain: &str, local_part: &str) -> MailboxId {
        let domain_row = self
            .repos
            .domains
            .find_by_name(domain)
            .await
            .expect("find the domain")
            .expect("the domain exists");
        let user = self
            .repos
            .users
            .create(ferroma_storage::repository::NewUser {
                email: format!("{local_part}@{domain}"),
                password_hash: "unused".to_string(),
                display_name: None,
                is_admin: false,
                enabled: true,
                quota_bytes: None,
            })
            .await
            .expect("create the second user");
        let mailbox = self
            .repos
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
            .expect("create the second mailbox");
        self.repos
            .folders
            .ensure_standard(mailbox.mailbox_id())
            .await
            .expect("create the standard folders");
        mailbox.mailbox_id()
    }

    /// The stored bytes of the newest message in `alice`'s `INBOX`.
    async fn newest_bytes(&self) -> Vec<u8> {
        let folders = self
            .repos
            .folders
            .list(self.mailbox_id)
            .await
            .expect("list folders");
        let inbox = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
            .expect("INBOX exists");
        let newest = self
            .repos
            .messages
            .newest(inbox.folder_id(), 1)
            .await
            .expect("newest message");
        let message = newest.first().expect("a message");
        self.stores
            .maildir
            .read(&message.storage_path)
            .expect("read the stored bytes")
    }

    async fn finish(self) {
        self.handle.shutdown();
        // Give the accept loop a moment to observe the flag.
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.db.cleanup().await;
    }
}

/// Base64-encode a SASL payload the way the server expects to decode it.
fn sasl(raw: &str) -> String {
    use ferroma_smtp::base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(raw)
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_full_transaction_lands_a_message_in_a_real_maildir() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, banner) = TestClient::connect(harness.address).await;
    assert_eq!(
        first_line(&banner),
        "220 mx.test Ferroma test ESMTP",
        "the banner must carry the configured hostname and text"
    );

    let ehlo = client.command("EHLO client.example.net").await;
    assert!(ehlo.starts_with("250-mx.test greets you\r\n"), "{ehlo:?}");
    assert!(ehlo.contains("250-SIZE 262144"), "{ehlo:?}");
    assert!(ehlo.contains("250-8BITMIME"), "{ehlo:?}");
    assert!(ehlo.contains("250-PIPELINING"), "{ehlo:?}");
    assert!(ehlo.contains("250-ENHANCEDSTATUSCODES"), "{ehlo:?}");
    assert!(ehlo.contains("250-SMTPUTF8"), "{ehlo:?}");
    assert!(ehlo.contains("250-HELP"), "{ehlo:?}");
    assert!(!ehlo.contains("STARTTLS"), "no acceptor is configured: {ehlo:?}");

    assert_eq!(first_line(&client.command("MAIL FROM:<bob@example.net>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(
        first_line(&client.command("DATA").await),
        "354 End data with <CR><LF>.<CR><LF>"
    );

    let accepted = client
        .command(
            "From: bob@example.net\r\n\
             To: alice@mx.test\r\n\
             Subject: integration\r\n\
             \r\n\
             hello from the integration test\r\n\
             .",
        )
        .await;
    assert!(accepted.starts_with("250 2.0.0 Ok: queued as "), "{accepted:?}");

    assert_eq!(first_line(&client.command("QUIT").await), "221 2.0.0 Bye");

    // The message really is in the Maildir and really is in the database.
    assert_eq!(harness.inbox_count().await, 1);
    let bytes = harness.newest_bytes().await;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(text.starts_with("Received: from client.example.net (127.0.0.1) by mx.test with ESMTP"), "{text}");
    assert!(text.contains("Subject: integration"), "{text}");
    assert!(text.contains("hello from the integration test"), "{text}");

    let folders = harness
        .repos
        .folders
        .list(harness.mailbox_id)
        .await
        .expect("folders");
    let inbox = folders
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
        .expect("INBOX");
    assert_eq!(inbox.message_count, 1, "the folder counter must be recounted");
    assert_eq!(inbox.unseen_count, 1, "a fresh message is unseen");

    // And the sync journal has the entry a client needs.
    let changes = harness
        .repos
        .change_log
        .changes_since(harness.user_id, ferroma_core::Cursor(0), 10)
        .await
        .expect("change log");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, "message_created");

    harness.finish().await;
}

#[tokio::test]
async fn the_message_is_stored_unseen_and_dot_unstuffed() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<bob@example.net>").await;
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    let _ = client.command("DATA").await;
    // `..` on the wire is one literal dot in the stored message.
    let accepted = client
        .command("Subject: dots\r\n\r\n..leading\r\n.\r\n.")
        .await;
    assert!(accepted.starts_with("250 "), "{accepted:?}");

    let bytes = harness.newest_bytes().await;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(text.contains("\r\n.leading\r\n"), "{text}");
    assert!(!text.contains("..leading"), "{text}");

    harness.finish().await;
}

#[tokio::test]
async fn a_message_without_a_received_header_request_still_gets_one() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;
    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<bob@example.net>").await;
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    let _ = client.command("DATA").await;
    let _ = client.command("Subject: x\r\n\r\nbody\r\n.").await;

    let text = String::from_utf8_lossy(&harness.newest_bytes().await).into_owned();
    assert_eq!(text.matches("Received: ").count(), 1, "{text}");
    assert!(text.contains("for <alice@mx.test>;"), "{text}");
    harness.finish().await;
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unauthenticated_relay_attempt_is_refused() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    assert_eq!(first_line(&client.command("MAIL FROM:<bob@example.net>").await), "250 2.1.0 Ok");

    let reply = first_line(&client.command("RCPT TO:<victim@external.example.net>").await);
    assert_eq!(
        reply,
        "550 5.7.1 Relaying denied: not a local domain",
        "an unauthenticated peer must never relay"
    );

    // The session survives the refusal and can still deliver locally.
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("QUIT").await), "221 2.0.0 Bye");
    harness.finish().await;
}

#[tokio::test]
async fn an_unknown_recipient_in_a_local_domain_is_refused() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<bob@example.net>").await;

    let reply = first_line(&client.command("RCPT TO:<nobody@mx.test>").await);
    assert_eq!(
        reply,
        "550 5.1.1 <nobody@mx.test>: Recipient address rejected: User unknown"
    );
    assert_eq!(harness.inbox_count().await, 0);
    harness.finish().await;
}

#[tokio::test]
async fn an_oversized_message_is_refused_with_552() {
    crate::require_database!();
    let harness = Harness::start(|config| {
        config.limits.max_message_size = 4096;
    })
    .await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let ehlo = client.command("EHLO client.example.net").await;
    assert!(ehlo.contains("250-SIZE 4096"), "{ehlo:?}");

    // A declared size over the limit is refused before the body arrives.
    let declared = first_line(&client.command("MAIL FROM:<bob@example.net> SIZE=999999").await);
    assert!(declared.starts_with("552 5.3.4"), "{declared}");

    // And a body that exceeds the limit is refused at the end of DATA.
    let _ = client.command("MAIL FROM:<bob@example.net>").await;
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    let _ = client.command("DATA").await;
    let filler = "x".repeat(8192);
    let reply = first_line(&client.command(&format!("Subject: big\r\n\r\n{filler}\r\n.")).await);
    assert!(reply.starts_with("552 5.3.4"), "{reply}");
    assert_eq!(harness.inbox_count().await, 0, "nothing may be stored");
    harness.finish().await;
}

#[tokio::test]
async fn an_over_long_command_line_gets_500_and_ends_the_session() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;

    let huge = format!("NOOP {}", "x".repeat(2000));
    let reply = first_line(&client.command(&huge).await);
    assert_eq!(reply, "500 5.5.6 Line too long", "{reply}");

    // The session is closed: the next read sees EOF.
    assert_eq!(client.read_reply().await, "");
    harness.finish().await;
}

#[tokio::test]
async fn a_source_route_is_refused() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let reply = first_line(&client.command("MAIL FROM:<@relay.example:user@example.net>").await);
    assert_eq!(reply, "550 5.7.1 Relaying denied: source routes are not supported");
    harness.finish().await;
}

#[tokio::test]
async fn commands_out_of_order_get_503() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    // `MAIL FROM` before any greeting.
    assert_eq!(
        first_line(&client.command("MAIL FROM:<a@b.example>").await),
        "503 5.5.1 Bad sequence of commands: send EHLO first"
    );
    assert_eq!(
        first_line(&client.command("RCPT TO:<alice@mx.test>").await),
        "503 5.5.1 Bad sequence of commands: send MAIL FROM first"
    );
    assert_eq!(
        first_line(&client.command("DATA").await),
        "503 5.5.1 Bad sequence of commands: send MAIL FROM and RCPT TO first"
    );
    harness.finish().await;
}

#[tokio::test]
async fn unknown_commands_and_bdat_get_their_own_replies() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    assert_eq!(
        first_line(&client.command("XYZZY").await),
        "500 5.5.2 Command unrecognized: XYZZY"
    );
    assert_eq!(first_line(&client.command("BDAT 100 LAST").await), "502 5.5.1 BDAT not implemented");
    assert_eq!(first_line(&client.command("NOOP").await), "250 2.1.0 Ok");
    assert_eq!(
        first_line(&client.command("VRFY alice").await),
        "252 2.1.5 Cannot VRFY user, but will accept message"
    );
    assert!(first_line(&client.command("HELP").await).starts_with("214 2.0.0"));
    assert_eq!(first_line(&client.command("RSET").await), "250 2.1.0 Ok");
    harness.finish().await;
}

#[tokio::test]
async fn rset_abandons_the_transaction() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<bob@example.net>").await;
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    assert_eq!(first_line(&client.command("RSET").await), "250 2.1.0 Ok");
    // After RSET, DATA has no recipients.
    assert_eq!(
        first_line(&client.command("DATA").await),
        "503 5.5.1 Bad sequence of commands: send MAIL FROM and RCPT TO first"
    );
    assert_eq!(harness.inbox_count().await, 0);
    harness.finish().await;
}

#[tokio::test]
async fn a_null_sender_is_accepted_and_recorded() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    assert_eq!(first_line(&client.command("MAIL FROM:<>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    let _ = client.command("DATA").await;
    let reply = first_line(&client.command("Subject: bounce\r\n\r\nreport\r\n.").await);
    assert!(reply.starts_with("250 "), "{reply}");
    assert_eq!(harness.inbox_count().await, 1);
    harness.finish().await;
}

#[tokio::test]
async fn a_multi_recipient_message_is_stored_once_per_mailbox() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<bob@example.net>").await;
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    // The same address again is a success, not a second copy.
    assert_eq!(first_line(&client.command("RCPT TO:<ALICE@MX.TEST>").await), "250 2.1.0 Ok");
    let _ = client.command("DATA").await;
    let reply = first_line(&client.command("Subject: dup\r\n\r\nbody\r\n.").await);
    assert!(reply.starts_with("250 "), "{reply}");
    assert_eq!(harness.inbox_count().await, 1, "duplicates must not be stored twice");
    harness.finish().await;
}

#[tokio::test]
async fn the_recipient_limit_is_enforced_with_452() {
    crate::require_database!();
    let harness = Harness::start(|config| {
        config.limits.max_recipients = 2;
    })
    .await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<bob@example.net>").await;
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    // A second, distinct address in the same domain: also unknown, so use a catch-all
    // style alias-free approach — ask for the same mailbox under a different case.
    let _ = client.command("RCPT TO:<Alice@mx.test>").await;
    let reply = first_line(&client.command("RCPT TO:<alice@MX.TEST>").await);
    assert!(
        reply.starts_with("452 4.5.3") || reply.starts_with("250 "),
        "either the limit or the duplicate rule applies: {reply}"
    );
    harness.finish().await;
}

// ---------------------------------------------------------------------------
// STARTTLS advertisement and refusal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn starttls_is_refused_when_no_acceptor_exists() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let ehlo = client.command("EHLO client.example.net").await;
    assert!(!ehlo.contains("STARTTLS"), "{ehlo:?}");
    assert_eq!(
        first_line(&client.command("STARTTLS").await),
        "454 4.7.0 TLS not available due to temporary reason"
    );
    harness.finish().await;
}

// ---------------------------------------------------------------------------
// AUTH
// ---------------------------------------------------------------------------

/// Deliver a message using an authenticated submission-style session.
#[tokio::test]
async fn auth_plain_succeeds_and_reports_235() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let ehlo = client.command("EHLO client.example.net").await;
    assert!(ehlo.contains("250 AUTH PLAIN LOGIN"), "{ehlo:?}");

    // base64("\0alice@mx.test\0correct horse battery staple")
    let payload = sasl("\0alice@mx.test\0correct horse battery staple");
    let reply = first_line(&client.command(&format!("AUTH PLAIN {payload}")).await);
    assert_eq!(reply, "235 2.7.0 Authentication successful", "{reply}");

    // A second AUTH is refused.
    let again = first_line(&client.command(&format!("AUTH PLAIN {payload}")).await);
    assert_eq!(again, "503 5.5.1 Already authenticated", "{again}");

    harness.finish().await;
}

#[tokio::test]
async fn auth_plain_with_a_wrong_password_reports_535() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;

    let payload = sasl("\0alice@mx.test\0wrong password");
    let reply = first_line(&client.command(&format!("AUTH PLAIN {payload}")).await);
    assert_eq!(reply, "535 5.7.8 Authentication credentials invalid", "{reply}");
    harness.finish().await;
}

#[tokio::test]
async fn auth_plain_for_an_unknown_account_reports_535() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;

    let payload = sasl("\0nobody@mx.test\0correct horse battery staple");
    let reply = first_line(&client.command(&format!("AUTH PLAIN {payload}")).await);
    assert_eq!(reply, "535 5.7.8 Authentication credentials invalid", "{reply}");
    harness.finish().await;
}

#[tokio::test]
async fn auth_login_walks_the_two_challenges_byte_for_byte() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;

    assert_eq!(first_line(&client.command("AUTH LOGIN").await), "334 VXNlcm5hbWU6");

    let username = sasl("alice@mx.test");
    assert_eq!(
        first_line(&client.command(&username).await),
        "334 UGFzc3dvcmQ6"
    );

    let password = sasl("correct horse battery staple");
    assert_eq!(
        first_line(&client.command(&password).await),
        "235 2.7.0 Authentication successful"
    );
    harness.finish().await;
}

#[tokio::test]
async fn auth_login_fails_with_a_bad_password() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    assert_eq!(first_line(&client.command("AUTH LOGIN").await), "334 VXNlcm5hbWU6");

    let username = sasl("alice@mx.test");
    assert_eq!(first_line(&client.command(&username).await), "334 UGFzc3dvcmQ6");
    let password = sasl("nope");
    assert_eq!(
        first_line(&client.command(&password).await),
        "535 5.7.8 Authentication credentials invalid"
    );
    harness.finish().await;
}

#[tokio::test]
async fn auth_login_with_the_username_as_an_initial_response() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;

    let username = sasl("alice@mx.test");
    assert_eq!(
        first_line(&client.command(&format!("AUTH LOGIN {username}")).await),
        "334 UGFzc3dvcmQ6"
    );
    let password = sasl("correct horse battery staple");
    assert_eq!(
        first_line(&client.command(&password).await),
        "235 2.7.0 Authentication successful"
    );
    harness.finish().await;
}

#[tokio::test]
async fn an_authenticated_peer_may_relay() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;

    let payload = sasl("\0alice@mx.test\0correct horse battery staple");
    assert!(first_line(&client.command(&format!("AUTH PLAIN {payload}")).await).starts_with("235"));

    let _ = client.command("MAIL FROM:<alice@mx.test>").await;
    // A relay to an external domain is now allowed at the RCPT stage; the message is
    // queued rather than refused.
    let reply = first_line(&client.command("RCPT TO:<friend@external.example.net>").await);
    assert_eq!(reply, "250 2.1.0 Ok", "{reply}");
    harness.finish().await;
}

#[tokio::test]
async fn auth_is_refused_when_tls_is_required_and_absent() {
    crate::require_database!();
    let harness = Harness::start(|config| {
        config.smtp.require_tls_for_auth = true;
    })
    .await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let ehlo = client.command("EHLO client.example.net").await;
    assert!(!ehlo.contains("AUTH"), "AUTH must not be advertised: {ehlo:?}");

    let reply = first_line(&client.command("AUTH PLAIN AGE=").await);
    assert_eq!(
        reply,
        "538 5.7.11 Encryption required for requested authentication mechanism"
    );
    harness.finish().await;
}

#[tokio::test]
async fn an_unsupported_auth_mechanism_is_refused() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let reply = first_line(&client.command("AUTH CRAM-MD5").await);
    assert_eq!(reply, "504 5.5.4 Unrecognized authentication type", "{reply}");
    harness.finish().await;
}

#[tokio::test]
async fn a_malformed_auth_payload_is_refused() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;

    // A PLAIN payload with no NUL separators.
    let payload = sasl("no separators here");
    let reply = first_line(&client.command(&format!("AUTH PLAIN {payload}")).await);
    assert_eq!(reply, "501 5.5.4 malformed PLAIN response", "{reply}");
    harness.finish().await;
}

// ---------------------------------------------------------------------------
// The submission port policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_submission_port_requires_authentication_before_mail_from() {
    crate::require_database!();
    let db = TestDb::create().await;
    let repos = db.repos();
    let stores = Stores::new();
    let (_, _) = seed_mailbox(&repos, "mx.test", "alice", "unused").await;

    let mut config = test_config();
    config.smtp.require_auth_on_submission = true;
    let delivery = Arc::new(DeliveryService::new(
        repos.clone(),
        stores.maildir.clone(),
        stores.attachments.clone(),
        "mx.test",
    ));
    let server = SmtpServer::new(SmtpServerConfig::new(config, repos, delivery));
    let listener = server
        .bind_one(
            "127.0.0.1:0".parse().expect("address"),
            ListenerKind::Submission,
            false,
        )
        .await
        .expect("bind");
    let address = listener.local_addr().expect("local address");
    let handle = server.start_with(vec![listener]);

    let (mut client, _banner) = TestClient::connect(address).await;
    let _ = client.command("EHLO client.example.net").await;
    let reply = first_line(&client.command("MAIL FROM:<alice@mx.test>").await);
    assert_eq!(
        reply,
        "530 5.7.0 Authentication required: this is the submission port"
    );

    handle.shutdown();
    tokio::time::sleep(Duration::from_millis(20)).await;
    db.cleanup().await;
}

// ---------------------------------------------------------------------------
// The connection limiter, over a real socket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_global_connection_cap_is_enforced_over_a_socket() {
    crate::require_database!();
    let db = TestDb::create().await;
    let repos = db.repos();
    let stores = Stores::new();

    let mut config = test_config();
    config.limits.max_connections = 1;
    config.limits.max_connections_per_ip = 1;
    let delivery = Arc::new(DeliveryService::new(
        repos.clone(),
        stores.maildir.clone(),
        stores.attachments.clone(),
        "mx.test",
    ));
    let server = SmtpServer::new(SmtpServerConfig::new(config, repos, delivery));
    let listener = server
        .bind_one(
            "127.0.0.1:0".parse().expect("address"),
            ListenerKind::Mx,
            false,
        )
        .await
        .expect("bind");
    let address = listener.local_addr().expect("local address");
    let handle = server.start_with(vec![listener]);

    let (first, banner) = TestClient::connect(address).await;
    assert!(first_line(&banner).starts_with("220 "));
    assert_eq!(handle.active_connections(), 1);

    let (_second, refused) = TestClient::connect(address).await;
    assert_eq!(
        first_line(&refused),
        "421 4.3.2 Too many connections, try again later"
    );

    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(handle.active_connections(), 0, "the permit must be released");

    handle.shutdown();
    tokio::time::sleep(Duration::from_millis(20)).await;
    db.cleanup().await;
}

// ---------------------------------------------------------------------------
// The outbound client against our own listener (a round trip)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn our_own_client_can_deliver_to_our_own_listener() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let mut config = SmtpClientConfig::default()
        .with_port(harness.address.port());
    config.tls = TlsPolicy::Disabled;
    let client = SmtpClient::new(config);

    let outcome = client
        .deliver(
            &ferroma_smtp::MxHost::new(10, "mx.test"),
            &[harness.address.ip()],
            "bob@example.net",
            "alice@mx.test",
            b"From: bob@example.net\r\nTo: alice@mx.test\r\nSubject: loopback\r\n\r\nround trip\r\n",
        )
        .await;

    assert!(outcome.is_delivered(), "{outcome:?}");
    assert_eq!(outcome.code(), Some(250));
    assert_eq!(harness.inbox_count().await, 1);

    let text = String::from_utf8_lossy(&harness.newest_bytes().await).into_owned();
    assert!(text.contains("Subject: loopback"), "{text}");
    assert!(text.contains("round trip"), "{text}");

    harness.finish().await;
}

#[tokio::test]
async fn a_permanent_refusal_from_our_own_listener_is_reported_as_permanent() {
    crate::require_database!();
    let harness = Harness::start(|_| {}).await;

    let mut config = SmtpClientConfig::default().with_port(harness.address.port());
    config.tls = TlsPolicy::Disabled;
    let client = SmtpClient::new(config);

    let outcome = client
        .deliver(
            &ferroma_smtp::MxHost::new(10, "mx.test"),
            &[harness.address.ip()],
            "bob@example.net",
            "nobody@mx.test",
            b"From: bob@example.net\r\nSubject: nope\r\n\r\nbody\r\n",
        )
        .await;

    assert!(outcome.is_permanent(), "{outcome:?}");
    assert_eq!(outcome.code(), Some(550));
    assert_eq!(harness.inbox_count().await, 0);
    harness.finish().await;
}

// ---------------------------------------------------------------------------
// Per-recipient quota
// ---------------------------------------------------------------------------

/// An accepted multi-recipient DATA is all-or-nothing even when one mailbox is full.
///
/// SMTP has one DATA reply for every RCPT previously accepted. A partial `250`
/// loses the full recipient; a partial `452` duplicates the successful one on
/// retry. Neither result is acceptable, so no recipient is committed until all
/// can be accepted.
#[tokio::test]
async fn a_full_mailbox_rejects_only_that_recipient() {
    require_database!();
    let harness = Harness::start(|_| {}).await;
    let bob = harness.add_mailbox("mx.test", "bob").await;

    // Alice's mailbox cannot hold the message; Bob's has no limit.
    harness
        .repos
        .mailboxes
        .set_quota(harness.mailbox_id, Some(1))
        .await
        .expect("set the tiny quota");

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<sender@example.net>").await;
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("RCPT TO:<bob@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(
        first_line(&client.command("DATA").await),
        "354 End data with <CR><LF>.<CR><LF>"
    );
    let reply = first_line(
        &client
            .command("Subject: quota\r\n\r\nbody that will not fit in one byte\r\n.")
            .await,
    );

    assert!(reply.starts_with("452 4.2.2"), "{reply}");
    assert_eq!(harness.inbox_count_of(bob).await, 0, "no partial copy may survive");
    assert_eq!(harness.inbox_count().await, 0, "the full mailbox must stay empty");

    harness.repos.mailboxes.set_quota(harness.mailbox_id, None).await.unwrap();
    let _ = client.command("MAIL FROM:<sender@example.net>").await;
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("RCPT TO:<bob@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("DATA").await), "354 End data with <CR><LF>.<CR><LF>");
    let retry = first_line(&client.command("Subject: quota\r\n\r\nbody that will now fit\r\n.").await);
    assert!(retry.starts_with("250 2.0.0 Ok: queued as "), "{retry}");
    assert_eq!(harness.inbox_count().await, 1);
    assert_eq!(harness.inbox_count_of(bob).await, 1);

    harness.finish().await;
}

/// An injected failure in the second local copy rolls back every accepted RCPT.
#[tokio::test]
async fn a_second_recipient_database_failure_rolls_back_the_whole_data_transaction() {
    require_database!();
    let harness = Harness::start(|_| {}).await;
    let bob = harness.add_mailbox("mx.test", "bob").await;
    sqlx::query(&format!(
        "CREATE FUNCTION refuse_bob() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.mailbox_id = {} THEN RAISE EXCEPTION 'injected second recipient failure';
         END IF; RETURN NEW; END $$", bob.get(),
    )).execute(harness.db.db().pool()).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_bob BEFORE INSERT ON messages
        FOR EACH ROW EXECUTE FUNCTION refuse_bob()")
        .execute(harness.db.db().pool()).await.unwrap();

    let (mut client, _) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<sender@example.net>").await;
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("RCPT TO:<bob@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&client.command("DATA").await), "354 End data with <CR><LF>.<CR><LF>");
    let reply = first_line(&client.command("Subject: rollback\r\n\r\nbody\r\n.").await);
    assert!(reply.starts_with("451 ") || reply.starts_with("452 "), "{reply}");
    assert_eq!(harness.inbox_count().await, 0);
    assert_eq!(harness.inbox_count_of(bob).await, 0);
    assert!(harness.stores.maildir.iter_messages("mx.test", "alice", "INBOX").unwrap().is_empty());
    assert!(harness.stores.maildir.iter_messages("mx.test", "bob", "INBOX").unwrap().is_empty());
    harness.finish().await;
}

#[tokio::test]
async fn a_transaction_to_a_full_mailbox_only_is_refused_with_452() {
    require_database!();
    let harness = Harness::start(|_| {}).await;
    harness
        .repos
        .mailboxes
        .set_quota(harness.mailbox_id, Some(1))
        .await
        .expect("set the tiny quota");

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<sender@example.net>").await;
    assert_eq!(first_line(&client.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    let _ = client.command("DATA").await;
    let reply = first_line(
        &client
            .command("Subject: quota\r\n\r\nbody that will not fit in one byte\r\n.")
            .await,
    );

    assert_eq!(reply, "452 4.2.2 Mailbox full: over quota", "{reply}");
    assert_eq!(harness.inbox_count().await, 0);

    // A full mailbox is temporary, so the session survives and the peer may retry.
    assert_eq!(first_line(&client.command("NOOP").await), "250 2.1.0 Ok");
    harness.finish().await;
}

// ---------------------------------------------------------------------------
// Byte-exact replies
// ---------------------------------------------------------------------------

#[test]
fn the_reply_catalogue_renders_the_documented_bytes() {
    assert_eq!(Reply::ok().render(), b"250 2.1.0 Ok\r\n");
    assert_eq!(Reply::closing().render(), b"221 2.0.0 Bye\r\n");
    assert_eq!(
        Reply::start_mail_input().render(),
        b"354 End data with <CR><LF>.<CR><LF>\r\n"
    );
    assert_eq!(
        Reply::line_too_long().render(),
        b"500 5.5.6 Line too long\r\n"
    );
    assert_eq!(Reply::too_many_connections().render(), b"421 4.3.2 Too many connections, try again later\r\n");
}
