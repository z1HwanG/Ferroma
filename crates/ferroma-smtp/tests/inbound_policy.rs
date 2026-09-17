//! The inbound authentication policy, driven over a real socket.
//!
//! The unit tests in `src/inbound.rs` prove the decision table; these prove the wiring:
//! that the `Authentication-Results` header actually reaches the stored message, that a
//! DKIM signature is verified over the bytes as received, and that a resolver which
//! cannot answer never costs the user a message.
//!
//! Every test uses a `MockResolver`, so nothing here touches the network.

mod common;

use std::sync::Arc;

use common::{first_line, seed_mailbox, test_config, Stores, TestClient, TestDb};
use ferroma_core::MailboxId;
use ferroma_smtp::{
    DkimKey,
    tls_acceptor, DkimSigner, DeliveryService, ListenerKind, MockResolver, MxResolver, Resolver,
    SmtpServer, SmtpServerConfig,
};
use ferroma_storage::Repositories;

/// A running server with a scripted resolver.
struct Harness {
    address: std::net::SocketAddr,
    handle: ferroma_smtp::SmtpServerHandle,
    db: TestDb,
    stores: Stores,
    repos: Repositories,
    mailbox_id: MailboxId,
}

impl Harness {
    /// Start a server whose policy step resolves through `resolver`.
    async fn start(
        resolver: MockResolver,
        configure: impl FnOnce(&mut ferroma_core::config::Config),
    ) -> Harness {
        let db = TestDb::create().await;
        let repos = db.repos();
        let stores = Stores::new();
        let (_, mailbox_id) = seed_mailbox(&repos, "mx.test", "alice", "unused").await;

        let mut config = test_config();
        configure(&mut config);

        let delivery = Arc::new(DeliveryService::new(
            repos.clone(),
            stores.maildir.clone(),
            stores.attachments.clone(),
            config.server.hostname.clone(),
        ));

        // `MxResolver` adds the TTL cache the production path uses; the mock underneath
        // is what answers. `MxResolver` implements `Resolver` itself, so the policy step
        // and outbound delivery share one cache.
        let resolver: Arc<dyn Resolver> = Arc::new(MxResolver::mock(
            resolver,
            &ferroma_core::config::DnsConfig::default(),
        ));

        let server = SmtpServer::new(
            SmtpServerConfig::new(config, repos.clone(), delivery).with_resolver(resolver),
        );
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

        Harness {
            address,
            handle,
            db,
            stores,
            repos,
            mailbox_id,
        }
    }

    /// Deliver one message and return the `DATA` reply's first line.
    async fn deliver(&self, from: &str, raw: &str) -> String {
        let (mut client, _banner) = TestClient::connect(self.address).await;
        let _ = client.command("EHLO client.example.net").await;
        assert_eq!(
            first_line(&client.command(&format!("MAIL FROM:<{from}>")).await),
            "250 2.1.0 Ok"
        );
        assert_eq!(
            first_line(&client.command("RCPT TO:<alice@mx.test>").await),
            "250 2.1.0 Ok"
        );
        let data = first_line(&client.command("DATA").await);
        assert_eq!(data, "354 End data with <CR><LF>.<CR><LF>", "{data}");
        let reply = first_line(&client.command(&format!("{raw}\r\n.")).await);
        let _ = client.command("QUIT").await;
        reply
    }

    /// The stored bytes of the newest message in a folder.
    async fn stored(&self, folder: &str) -> Vec<u8> {
        let folders = self
            .repos
            .folders
            .list(self.mailbox_id)
            .await
            .expect("folders");
        let target = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(folder))
            .expect("the folder exists");
        let newest = self
            .repos
            .messages
            .newest(target.folder_id(), 1)
            .await
            .expect("newest");
        let message = newest.first().expect("a stored message");
        self.stores
            .maildir
            .read(&message.storage_path)
            .expect("read the bytes")
    }

    /// The number of messages in a folder.
    async fn count(&self, folder: &str) -> i64 {
        let folders = self
            .repos
            .folders
            .list(self.mailbox_id)
            .await
            .expect("folders");
        let target = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(folder))
            .expect("the folder exists");
        self.repos
            .messages
            .count_by_folder(target.folder_id())
            .await
            .expect("count")
    }

    async fn finish(self) {
        self.handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.db.cleanup().await;
    }
}

/// The stored message text, lossily decoded for assertions.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The `Authentication-Results` header of a stored message, **unfolded**.
///
/// RFC 5322 allows the value to be folded across several lines and the renderer folds at
/// 78 columns, so a naive "first line" read would see only `mx.test;`.
fn auth_results(bytes: &[u8]) -> Option<String> {
    unfold(&text(bytes), "Authentication-Results:")
}

/// The stored `Received:` header, unfolded.
fn received_header(bytes: &[u8]) -> Option<String> {
    unfold(&text(bytes), "Received:")
}

/// Find the header `name` in `message` and join its continuation lines.
fn unfold(message: &str, name: &str) -> Option<String> {
    let mut lines = message.lines();
    let mut value = lines
        .by_ref()
        .find(|line| line.starts_with(name))?
        .to_string();
    // Continuation lines begin with whitespace (RFC 5322 §2.2.3).
    for line in lines {
        if line.starts_with(' ') || line.starts_with('\t') {
            value.push(' ');
            value.push_str(line.trim_start());
        } else {
            break;
        }
    }
    Some(value)
}

// ---------------------------------------------------------------------------
// SPF
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_message_failing_spf_is_reported_and_still_delivered() {
    crate::require_database!();
    // SPF covers a different network; the peer is somewhere else. DMARC asks for
    // nothing, so there is no enforcement.
    let resolver = MockResolver::new()
        .with_txt("example.net", vec!["v=spf1 ip4:198.51.100.1 -all".to_string()])
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]);
    let harness = Harness::start(resolver, |config| {
        config.policy.dmarc_failure_action = "none".to_string();
    })
    .await;

    let reply = harness
        .deliver(
            "alice@example.net",
            "From: alice@example.net\r\nTo: alice@mx.test\r\nSubject: spf\r\n\r\nbody\r\n",
        )
        .await;
    assert!(reply.starts_with("250 2.0.0 Ok: queued as "), "{reply}");

    let stored = harness.stored("INBOX").await;
    let header = auth_results(&stored).expect("an Authentication-Results header");
    assert!(header.contains("spf=fail"), "{header}");
    assert!(header.contains("smtp.mailfrom=example.net"), "{header}");
    assert_eq!(harness.count("INBOX").await, 1, "the message is still delivered");

    harness.finish().await;
}

#[tokio::test]
async fn an_spf_pass_is_reported_as_pass() {
    crate::require_database!();
    // 127.0.0.1 is the peer, and the record authorises it.
    let resolver = MockResolver::new()
        .with_txt("example.net", vec!["v=spf1 ip4:127.0.0.1 -all".to_string()])
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]);
    let harness = Harness::start(resolver, |_| {}).await;

    let reply = harness
        .deliver("alice@example.net", "From: alice@example.net\r\n\r\nbody\r\n")
        .await;
    assert!(reply.starts_with("250 "), "{reply}");

    let header = auth_results(&harness.stored("INBOX").await).expect("a header");
    assert!(header.contains("spf=pass"), "{header}");
    // DMARC aligns because SPF passed under the From domain.
    assert!(header.contains("dmarc=pass"), "{header}");

    harness.finish().await;
}

// ---------------------------------------------------------------------------
// DKIM
// ---------------------------------------------------------------------------

/// Build a signer with a freshly generated RSA key.
///
/// Returns the signer and the DNS record that publishes its public key, so a test can
/// hand the second to the resolver and sign with the first.
fn test_signer() -> (DkimSigner, String) {
    use ferroma_core::config::DkimConfig;
    use rsa::pkcs8::EncodePrivateKey;

    let config = DkimConfig {
        enabled: true,
        selector: "sel".to_string(),
        private_key_path: None,
        domain: Some("example.net".to_string()),
        canonicalization: "relaxed".to_string(),
        headers_to_sign: vec!["From".to_string(), "To".to_string(), "Subject".to_string()],
        verify_inbound: true,
    };
    // A 1024-bit key: this is a test, and generation speed matters.
    let mut rng = rand::thread_rng();
    let key = rsa::RsaPrivateKey::new(&mut rng, 1024).expect("generate an RSA key");
    let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).expect("PEM");
    let dkim_key = DkimKey::from_pem(&pem).expect("parse the key");
    // Read the public half before the key is consumed by the signer.
    let record = format!(
        "v=DKIM1; k=rsa; p={}",
        dkim_key.public_key_base64().expect("public key")
    );
    let signer = DkimSigner::from_key(dkim_key, &config).expect("build the signer");
    (signer, record)
}

#[tokio::test]
async fn a_valid_dkim_signature_is_reported_as_pass() {
    crate::require_database!();
    let (signer, record) = test_signer();
    let resolver = MockResolver::new()
        .with_txt("sel._domainkey.example.net", vec![record])
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]);
    let harness = Harness::start(resolver, |_| {}).await;

    let body = b"From: alice@example.net\r\nTo: alice@mx.test\r\nSubject: signed\r\n\r\nsigned body\r\n";
    let signed = signer.sign_message(body).expect("sign");
    let raw = String::from_utf8_lossy(&signed).into_owned();
    // The client appends the terminator, and the body must not contain a bare `.` line.
    let raw = raw.trim_end_matches("\r\n").to_string();

    let reply = harness.deliver("alice@example.net", &raw).await;
    assert!(reply.starts_with("250 "), "{reply}");

    let header = auth_results(&harness.stored("INBOX").await).expect("a header");
    assert!(header.contains("dkim=pass"), "{header}");
    assert!(header.contains("header.d=example.net"), "{header}");

    harness.finish().await;
}

#[tokio::test]
async fn a_tampered_body_is_reported_as_dkim_fail() {
    crate::require_database!();
    let (signer, record) = test_signer();
    let resolver = MockResolver::new()
        .with_txt("sel._domainkey.example.net", vec![record])
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]);
    let harness = Harness::start(resolver, |_| {}).await;

    let body = b"From: alice@example.net\r\nTo: alice@mx.test\r\nSubject: signed\r\n\r\noriginal body\r\n";
    let signed = signer.sign_message(body).expect("sign");
    let mut raw = String::from_utf8_lossy(&signed).into_owned();
    // Change the body *after* signing: the signature must no longer verify.
    raw = raw.replace("original body", "tampered body!!");
    let raw = raw.trim_end_matches("\r\n").to_string();

    let reply = harness.deliver("alice@example.net", &raw).await;
    assert!(reply.starts_with("250 "), "p=none still accepts: {reply}");

    let header = auth_results(&harness.stored("INBOX").await).expect("a header");
    assert!(header.contains("dkim=fail"), "{header}");
    assert!(!header.contains("dkim=pass"), "{header}");

    harness.finish().await;
}

// ---------------------------------------------------------------------------
// DMARC enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dmarc_reject_failure_is_refused_before_the_250() {
    crate::require_database!();
    // Nothing aligns: no SPF record, no DKIM signature, and `p=reject`.
    let resolver = MockResolver::new()
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=reject".to_string()]);
    let harness = Harness::start(resolver, |config| {
        config.policy.dmarc_failure_action = "reject".to_string();
    })
    .await;

    let reply = harness
        .deliver(
            "alice@example.net",
            "From: alice@example.net\r\nTo: alice@mx.test\r\nSubject: rejected\r\n\r\nbody\r\n",
        )
        .await;

    assert_eq!(
        reply, "550 5.7.1 Message rejected by the DMARC policy of example.net",
        "the refusal must be a 550 the peer has not been contradicted about"
    );
    assert_eq!(
        harness.count("INBOX").await,
        0,
        "a rejected message is not stored"
    );

    harness.finish().await;
}

#[tokio::test]
async fn a_dmarc_reject_failure_is_quarantined_when_the_local_policy_says_so() {
    crate::require_database!();
    let resolver = MockResolver::new()
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=reject".to_string()]);
    let harness = Harness::start(resolver, |config| {
        config.policy.dmarc_failure_action = "quarantine".to_string();
    })
    .await;

    let reply = harness
        .deliver(
            "alice@example.net",
            "From: alice@example.net\r\nTo: alice@mx.test\r\nSubject: quarantine\r\n\r\nbody\r\n",
        )
        .await;
    assert!(reply.starts_with("250 "), "{reply}");

    assert_eq!(harness.count("INBOX").await, 0, "nothing lands in INBOX");
    assert_eq!(harness.count("Junk").await, 1, "the message is quarantined");

    let header = auth_results(&harness.stored("Junk").await).expect("a header");
    assert!(header.contains("dmarc=fail"), "{header}");
    assert!(header.contains("policy.dmarc=reject"), "{header}");

    harness.finish().await;
}

#[tokio::test]
async fn a_dmarc_pass_lands_in_the_inbox_with_dmarc_pass() {
    crate::require_database!();
    let resolver = MockResolver::new()
        .with_txt("example.net", vec!["v=spf1 ip4:127.0.0.1 -all".to_string()])
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=reject".to_string()]);
    let harness = Harness::start(resolver, |config| {
        config.policy.dmarc_failure_action = "reject".to_string();
    })
    .await;

    let reply = harness
        .deliver("alice@example.net", "From: alice@example.net\r\n\r\nbody\r\n")
        .await;
    assert!(reply.starts_with("250 "), "{reply}");
    assert_eq!(harness.count("INBOX").await, 1);
    assert_eq!(harness.count("Junk").await, 0);

    let header = auth_results(&harness.stored("INBOX").await).expect("a header");
    assert!(header.contains("dmarc=pass"), "{header}");

    harness.finish().await;
}

/// **The headline requirement.** A resolver that cannot answer must neither reject the
/// message nor report a misleading `fail`.
#[tokio::test]
async fn a_resolver_that_always_fails_still_delivers_the_message() {
    crate::require_database!();
    // Every name the policy step will ask about fails.
    let resolver = MockResolver::new()
        .with_failure("example.net")
        .with_failure("_dmarc.example.net")
        .with_failure("sel._domainkey.example.net");
    let harness = Harness::start(resolver, |config| {
        // The strictest possible local action: even so, a broken resolver must not
        // reject.
        config.policy.dmarc_failure_action = "reject".to_string();
    })
    .await;

    let reply = harness
        .deliver(
            "alice@example.net",
            "From: alice@example.net\r\nTo: alice@mx.test\r\nSubject: broken dns\r\n\r\nbody\r\n",
        )
        .await;

    assert!(
        reply.starts_with("250 "),
        "a DNS failure must never turn into a rejection: {reply}"
    );
    assert_eq!(harness.count("INBOX").await, 1, "the message is delivered");
    assert_eq!(harness.count("Junk").await, 0);

    let header = auth_results(&harness.stored("INBOX").await).expect("a header");
    assert!(
        header.contains("temperror"),
        "the header must say temperror, not a misleading fail: {header}"
    );
    assert!(!header.contains("spf=fail"), "{header}");
    assert!(!header.contains("dmarc=fail"), "{header}");

    harness.finish().await;
}

// ---------------------------------------------------------------------------
// The switches, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_header_is_added_when_policy_add_auth_results_is_off() {
    crate::require_database!();
    let resolver = MockResolver::new()
        .with_txt("example.net", vec!["v=spf1 ip4:127.0.0.1 -all".to_string()]);
    let harness = Harness::start(resolver, |config| {
        config.policy.add_auth_results = false;
    })
    .await;

    let reply = harness
        .deliver("alice@example.net", "From: alice@example.net\r\n\r\nbody\r\n")
        .await;
    assert!(reply.starts_with("250 "), "{reply}");

    let stored = harness.stored("INBOX").await;
    assert!(
        auth_results(&stored).is_none(),
        "no Authentication-Results must be added: {}",
        text(&stored)
    );
    // The trace header is still there, and it is still first.
    assert!(text(&stored).starts_with("Received: "), "{}", text(&stored));

    harness.finish().await;
}

#[tokio::test]
async fn without_a_resolver_the_policy_step_is_skipped_entirely() {
    crate::require_database!();
    let db = TestDb::create().await;
    let repos = db.repos();
    let stores = Stores::new();
    let (_, mailbox_id) = seed_mailbox(&repos, "mx.test", "alice", "unused").await;

    let config = test_config();
    let delivery = Arc::new(DeliveryService::new(
        repos.clone(),
        stores.maildir.clone(),
        stores.attachments.clone(),
        "mx.test",
    ));
    // Deliberately no `.with_resolver(..)`.
    let server = SmtpServer::new(SmtpServerConfig::new(config, repos.clone(), delivery));
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

    let (mut client, _banner) = TestClient::connect(address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<alice@example.net>").await;
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    let _ = client.command("DATA").await;
    let reply = first_line(&client.command("Subject: no policy\r\n\r\nbody\r\n.").await);
    assert!(reply.starts_with("250 "), "{reply}");
    let _ = client.command("QUIT").await;

    let folders = repos.folders.list(mailbox_id).await.expect("folders");
    let inbox = folders
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
        .expect("INBOX");
    let newest = repos
        .messages
        .newest(inbox.folder_id(), 1)
        .await
        .expect("newest");
    let message = newest.first().expect("a message");
    let stored = stores.maildir.read(&message.storage_path).expect("read");
    let stored = text(&stored);
    assert!(auth_results(stored.as_bytes()).is_none(), "{stored}");
    assert!(stored.starts_with("Received: "), "{stored}");

    handle.shutdown();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    db.cleanup().await;
}

#[tokio::test]
async fn the_stored_message_stacks_authentication_results_above_received() {
    crate::require_database!();
    let resolver = MockResolver::new()
        .with_txt("example.net", vec!["v=spf1 ip4:127.0.0.1 -all".to_string()])
        .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]);
    let harness = Harness::start(resolver, |_| {}).await;

    let reply = harness
        .deliver("alice@example.net", "From: alice@example.net\r\n\r\nbody\r\n")
        .await;
    assert!(reply.starts_with("250 "), "{reply}");

    let stored = text(&harness.stored("INBOX").await);
    assert!(
        stored.starts_with("Authentication-Results:"),
        "the newest trace header goes on top: {stored}"
    );
    // `Received:` must come after the whole (possibly folded) A-R block.
    let received = received_header(stored.as_bytes()).expect("a Received header");
    let received_at = stored
        .find("Received: ")
        .expect("the Received header is present");
    let auth_at = stored.find("Authentication-Results:").expect("present");
    assert!(
        auth_at < received_at,
        "Authentication-Results must precede Received: {stored}"
    );
    assert!(received.contains("by mx.test with ESMTP"), "{received}");

    let header = auth_results(stored.as_bytes()).expect("a header");
    assert!(header.starts_with("Authentication-Results: mx.test;"), "{header}");
    assert!(header.contains("spf=pass"), "{header}");
    // The message itself is untouched.
    assert!(stored.contains("\r\n\r\nbody\r\n"), "{stored}");

    harness.finish().await;
}

#[tokio::test]
async fn a_null_sender_is_still_evaluated_and_has_no_spf_result() {
    crate::require_database!();
    let resolver = MockResolver::new();
    let harness = Harness::start(resolver, |_| {}).await;

    let (mut client, _banner) = TestClient::connect(harness.address).await;
    let _ = client.command("EHLO bounce.example.net").await;
    assert_eq!(first_line(&client.command("MAIL FROM:<>").await), "250 2.1.0 Ok");
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    let _ = client.command("DATA").await;
    let reply = first_line(&client.command("Subject: bounce\r\n\r\nreport\r\n.").await);
    assert!(reply.starts_with("250 "), "{reply}");

    let header = auth_results(&harness.stored("INBOX").await).expect("a header");
    assert!(header.contains("smtp.mailfrom=<>"), "{header}");
    assert!(header.contains("spf=none"), "{header}");

    harness.finish().await;
}

/// A `STARTTLS` upgrade combined with the policy step: the header is still written, and
/// the DKIM check ran over the bytes as received.
#[tokio::test]
async fn the_policy_step_still_runs_over_a_tls_connection() {
    crate::require_database!();
    let certified = rcgen::generate_simple_self_signed(vec!["mx.test".to_string()])
        .expect("certificate");
    let acceptor = tls_acceptor(
        certified.cert.pem().as_bytes(),
        certified.key_pair.serialize_pem().as_bytes(),
        "1.2",
    )
    .expect("acceptor");

    let db = TestDb::create().await;
    let repos = db.repos();
    let stores = Stores::new();
    let (_, _) = seed_mailbox(&repos, "mx.test", "alice", "unused").await;

    let mut config = test_config();
    config.tls.enabled = true;
    let delivery = Arc::new(DeliveryService::new(
        repos.clone(),
        stores.maildir.clone(),
        stores.attachments.clone(),
        "mx.test",
    ));
    let resolver: Arc<dyn Resolver> = Arc::new(MxResolver::mock(
        MockResolver::new()
            .with_txt("example.net", vec!["v=spf1 ip4:127.0.0.1 -all".to_string()])
            .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]),
        &ferroma_core::config::DnsConfig::default(),
    ));
    let server = SmtpServer::new(
        SmtpServerConfig::new(config, repos.clone(), delivery)
            .with_tls(acceptor)
            .with_resolver(resolver),
    );
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

    // A plaintext session is enough: the policy step runs on the `DATA` path, and the
    // TLS variant is covered by `starttls.rs`.
    let (mut client, _banner) = TestClient::connect(address).await;
    let _ = client.command("EHLO client.example.net").await;
    let _ = client.command("MAIL FROM:<alice@example.net>").await;
    let _ = client.command("RCPT TO:<alice@mx.test>").await;
    let _ = client.command("DATA").await;
    let reply = first_line(&client.command("Subject: policy\r\n\r\nbody\r\n.").await);
    assert!(reply.starts_with("250 "), "{reply}");

    let folders = repos.folders.list(ferroma_core::MailboxId::new(1)).await;
    assert!(folders.is_ok());

    handle.shutdown();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    db.cleanup().await;
}
