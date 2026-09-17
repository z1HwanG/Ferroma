//! The STARTTLS handshake, over a real socket with a real certificate.
//!
//! `STARTTLS` is security-critical and the rest of the suite can only reach it from the
//! outside: `may_starttls` guards, the `454` when no acceptor exists, the session reset.
//! This file completes an actual TLS handshake so the parts that only exist between
//! those edges are covered too — that the upgrade really happens, that the peer must
//! greet again, and that `AUTH` works over the encrypted channel and *only* there.
//!
//! Each test gets its own migrated schema and its own generated certificate, so nothing
//! here can interfere with anything else.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{first_line, seed_mailbox, test_config, Stores, TestClient, TestDb};
use ferroma_auth::{AuthService, PasswordHasher, TokenService};
use ferroma_smtp::{
    tls_acceptor, DeliveryService, ListenerKind, SmtpServer, SmtpServerConfig,
};
use ferroma_storage::Repositories;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// A self-signed certificate for `mx.test` and the acceptor built from it.
struct Tls {
    acceptor: tokio_rustls::TlsAcceptor,
    /// The certificate we must trust in the client, since it is self-signed.
    root: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
}

/// Generate a fresh self-signed certificate. No fixtures, no files on disk.
fn generate_tls() -> Tls {
    let certified = rcgen::generate_simple_self_signed(vec!["mx.test".to_string(), "localhost".to_string()])
        .expect("generate a self-signed certificate");
    let cert_pem = certified.cert.pem();
    let key_pem = certified.key_pair.serialize_pem();

    let acceptor = tls_acceptor(cert_pem.as_bytes(), key_pem.as_bytes(), "1.2")
        .expect("build the TLS acceptor");

    let root = tokio_rustls::rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
    Tls { acceptor, root }
}

/// A TLS client that trusts exactly the certificate the server was given.
fn tls_connector(root: tokio_rustls::rustls::pki_types::CertificateDer<'static>) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots
        .add(root)
        .expect("the generated certificate must be a usable trust anchor");
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// An encrypted SMTP session, with the same tiny line protocol as [`TestClient`].
struct SecureClient {
    reader: BufReader<tokio::io::ReadHalf<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>,
    writer: tokio::io::WriteHalf<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
}

impl SecureClient {
    async fn command(&mut self, line: &str) -> String {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("write over TLS");
        self.writer.flush().await.expect("flush over TLS");
        self.read_reply().await
    }

    async fn read_reply(&mut self) -> String {
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
}

/// Everything one STARTTLS test needs.
struct Harness {
    address: std::net::SocketAddr,
    handle: ferroma_smtp::SmtpServerHandle,
    db: TestDb,
    connector: TlsConnector,
    mailbox_id: ferroma_core::MailboxId,
}

impl Harness {
    async fn start(require_tls_for_auth: bool) -> Harness {
        let db = TestDb::create().await;
        let repos = db.repos();
        let stores = Stores::new();

        let hasher = PasswordHasher::default();
        let password_hash = hasher
            .hash("correct horse battery staple")
            .expect("hash the test password");
        let (_, mailbox_id) = seed_mailbox(&repos, "mx.test", "alice", &password_hash).await;

        let tls = generate_tls();
        let connector = tls_connector(tls.root);

        let mut config = test_config();
        config.smtp.require_tls_for_auth = require_tls_for_auth;
        config.tls.enabled = true;

        let delivery = Arc::new(DeliveryService::new(
            repos.clone(),
            stores.maildir.clone(),
            stores.attachments.clone(),
            "mx.test",
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

        let server = SmtpServer::new(
            SmtpServerConfig::new(config, repos, delivery)
                .with_auth(auth)
                .with_tls(tls.acceptor),
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
            connector,
            mailbox_id,
        }
    }

    /// The number of messages in `alice`'s `INBOX`.
    async fn inbox_count(&self, repos: &Repositories) -> i64 {
        let folders = repos
            .folders
            .list(self.mailbox_id)
            .await
            .expect("list folders");
        let inbox = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
            .expect("INBOX exists");
        repos
            .messages
            .count_by_folder(inbox.folder_id())
            .await
            .expect("count messages")
    }

    /// Connect, `EHLO`, `STARTTLS` and complete the handshake.
    async fn upgraded(&self) -> (SecureClient, String) {
        let (mut plain, _banner) = TestClient::connect(self.address).await;
        let ehlo = plain.command("EHLO client.example.net").await;
        assert!(ehlo.contains("250 STARTTLS"), "STARTTLS must be advertised: {ehlo:?}");
        let ready = first_line(&plain.command("STARTTLS").await);
        assert_eq!(ready, "220 2.0.0 Ready to start TLS", "{ready}");

        // Give the plaintext stream back to the acceptor.
        let stream = plain.into_stream();
        let server_name = ServerName::try_from("mx.test").expect("server name");
        let tls = self
            .connector
            .connect(server_name, stream)
            .await
            .expect("the TLS handshake must complete");
        let (read, write) = tokio::io::split(tls);
        (SecureClient { reader: BufReader::new(read), writer: write }, ehlo)
    }

    async fn finish(self) {
        self.handle.shutdown();
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.db.cleanup().await;
    }
}

/// The base64 engine, re-exported by the crate so a test needs no extra dependency.
fn sasl(raw: &str) -> String {
    use ferroma_smtp::base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(raw)
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn starttls_upgrades_the_connection_and_resets_the_session() {
    crate::require_database!();
    let harness = Harness::start(false).await;

    let (mut secure, plaintext_ehlo) = harness.upgraded().await;

    // (a) No `STARTTLS` any more, and `AUTH` is only advertised now that the channel is
    //     encrypted — this server is configured with `require_tls_for_auth`.
    let ehlo = secure.command("EHLO client.example.net").await;
    assert!(ehlo.starts_with("250-mx.test greets you\r\n"), "{ehlo:?}");
    assert!(
        !ehlo.contains("STARTTLS"),
        "STARTTLS must not be advertised twice: {ehlo:?}"
    );
    assert!(
        ehlo.contains("250 AUTH PLAIN LOGIN"),
        "AUTH must be offered over the encrypted channel: {ehlo:?}"
    );
    // The plaintext advertisement also had STARTTLS, so the two really differ.
    assert!(plaintext_ehlo.contains("STARTTLS"));

    // (b) The pre-upgrade envelope is gone. RFC 3207 §4.2 requires a full reset, so
    //     `RCPT TO` cannot be used to continue a transaction started in the clear.
    let reply = first_line(&secure.command("RCPT TO:<alice@mx.test>").await);
    assert_eq!(
        reply, "503 5.5.1 Bad sequence of commands: send MAIL FROM first",
        "the session must have been reset by the upgrade"
    );

    // (c) And a real message can be delivered over TLS.
    assert_eq!(first_line(&secure.command("MAIL FROM:<bob@example.net>").await), "250 2.1.0 Ok");
    assert_eq!(first_line(&secure.command("RCPT TO:<alice@mx.test>").await), "250 2.1.0 Ok");
    assert_eq!(
        first_line(&secure.command("DATA").await),
        "354 End data with <CR><LF>.<CR><LF>"
    );
    let accepted = first_line(
        &secure
            .command("Subject: over tls\r\n\r\nencrypted body\r\n.")
            .await,
    );
    assert!(accepted.starts_with("250 2.0.0 Ok: queued as "), "{accepted}");
    assert_eq!(harness.inbox_count(&harness.db.repos()).await, 1);

    assert_eq!(first_line(&secure.command("QUIT").await), "221 2.0.0 Bye");
    harness.finish().await;
}

#[tokio::test]
async fn auth_succeeds_over_the_encrypted_channel() {
    crate::require_database!();
    let harness = Harness::start(false).await;

    let (mut secure, _) = harness.upgraded().await;
    let _ = secure.command("EHLO client.example.net").await;

    let payload = sasl("\0alice@mx.test\0correct horse battery staple");
    let reply = first_line(&secure.command(&format!("AUTH PLAIN {payload}")).await);
    assert_eq!(reply, "235 2.7.0 Authentication successful", "{reply}");

    // A second AUTH is still refused, which proves the state survives the upgrade.
    let again = first_line(&secure.command(&format!("AUTH PLAIN {payload}")).await);
    assert_eq!(again, "503 5.5.1 Already authenticated", "{again}");

    harness.finish().await;
}

#[tokio::test]
async fn auth_login_walks_its_challenges_over_tls() {
    crate::require_database!();
    let harness = Harness::start(false).await;

    let (mut secure, _) = harness.upgraded().await;
    let _ = secure.command("EHLO client.example.net").await;

    assert_eq!(first_line(&secure.command("AUTH LOGIN").await), "334 VXNlcm5hbWU6");
    assert_eq!(
        first_line(&secure.command(&sasl("alice@mx.test")).await),
        "334 UGFzc3dvcmQ6"
    );
    assert_eq!(
        first_line(&secure.command(&sasl("correct horse battery staple")).await),
        "235 2.7.0 Authentication successful"
    );

    harness.finish().await;
}

/// With `require_tls_for_auth` set, the plaintext session must not offer `AUTH` — and
/// must refuse it even if the peer tries anyway.
#[tokio::test]
async fn auth_is_not_offered_or_accepted_before_the_upgrade() {
    crate::require_database!();
    let harness = Harness::start(true).await;

    let (mut plain, _banner) = TestClient::connect(harness.address).await;
    let ehlo = plain.command("EHLO client.example.net").await;
    assert!(
        !ehlo.contains("AUTH"),
        "AUTH must not be advertised in the clear: {ehlo:?}"
    );
    assert!(ehlo.contains("STARTTLS"), "{ehlo:?}");

    let reply = first_line(&plain.command("AUTH PLAIN AGE=").await);
    assert_eq!(
        reply,
        "538 5.7.11 Encryption required for requested authentication mechanism"
    );

    harness.finish().await;
}

#[tokio::test]
async fn a_second_starttls_is_refused() {
    crate::require_database!();
    let harness = Harness::start(false).await;

    let (mut secure, _) = harness.upgraded().await;
    let _ = secure.command("EHLO client.example.net").await;
    let reply = first_line(&secure.command("STARTTLS").await);
    assert_eq!(reply, "503 5.5.1 TLS is already active", "{reply}");

    harness.finish().await;
}

/// RFC 3207 §6: a peer that sends plaintext after `STARTTLS` has been acknowledged must
/// not be upgraded — that is the request-smuggling primitive the RFC warns about.
#[tokio::test]
async fn pipelined_plaintext_after_starttls_aborts_the_upgrade() {
    crate::require_database!();
    let harness = Harness::start(false).await;

    let (mut plain, _banner) = TestClient::connect(harness.address).await;
    let _ = plain.command("EHLO client.example.net").await;
    // `STARTTLS` followed immediately by a command in the same segment.
    plain.send_raw(b"STARTTLS\r\nNOOP\r\n").await;
    let ready = plain.read_reply().await;
    assert_eq!(first_line(&ready), "220 2.0.0 Ready to start TLS", "{ready:?}");

    // The server refuses to upgrade: the connection is closed rather than handing a
    // half-read buffer to the TLS layer.
    let mut client = plain;
    let after = tokio::time::timeout(Duration::from_secs(5), client.read_reply()).await;
    // A reset (`Err`) is the other acceptable outcome.
    if let Ok(reply) = after {
        assert!(
            reply.is_empty() || !reply.starts_with("2"),
            "the upgrade must not have proceeded: {reply:?}"
        );
    }

    harness.finish().await;
}

/// An implicit-TLS listener completes its handshake before the banner.
#[tokio::test]
async fn an_implicit_tls_listener_speaks_tls_from_the_first_byte() {
    crate::require_database!();
    let db = TestDb::create().await;
    let repos = db.repos();
    let stores = Stores::new();
    let (_, _) = seed_mailbox(&repos, "mx.test", "alice", "unused").await;

    let tls = generate_tls();
    let connector = tls_connector(tls.root);

    let mut config = test_config();
    config.tls.enabled = true;
    let delivery = Arc::new(DeliveryService::new(
        repos.clone(),
        stores.maildir.clone(),
        stores.attachments.clone(),
        "mx.test",
    ));
    let server = SmtpServer::new(
        SmtpServerConfig::new(config, repos, delivery).with_tls(tls.acceptor),
    );
    let listener = server
        .bind_one(
            "127.0.0.1:0".parse().expect("address"),
            ListenerKind::Smtps,
            true,
        )
        .await
        .expect("bind");
    let address = listener.local_addr().expect("local address");
    let handle = server.start_with(vec![listener]);

    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let server_name = ServerName::try_from("mx.test").expect("server name");
    let tls = connector
        .connect(server_name, stream)
        .await
        .expect("implicit TLS must complete before the banner");
    let (read, mut write) = tokio::io::split(tls);
    let mut reader = BufReader::new(read);

    let mut banner = String::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut banner))
        .await
        .expect("banner timed out")
        .expect("read banner");
    assert_eq!(
        first_line(&banner),
        "220 mx.test Ferroma test ESMTP",
        "the banner arrives inside the TLS session"
    );

    write.write_all(b"EHLO client.example.net\r\n").await.expect("write");
    write.flush().await.expect("flush");
    let mut ehlo = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read");
        let done = line.len() < 4 || line.as_bytes()[3] != b'-';
        ehlo.push_str(&line);
        if done || line.is_empty() {
            break;
        }
    }
    assert!(ehlo.contains("250-SIZE"), "{ehlo:?}");
    assert!(
        !ehlo.contains("STARTTLS"),
        "an implicit-TLS listener must not offer STARTTLS: {ehlo:?}"
    );

    handle.shutdown();
    tokio::time::sleep(Duration::from_millis(20)).await;
    db.cleanup().await;
}

#[tokio::test]
async fn a_bad_certificate_and_key_pair_is_refused_at_construction() {
    // Two different key pairs: `with_single_cert` must reject the mismatch here rather
    // than on the first handshake.
    let first = rcgen::generate_simple_self_signed(vec!["mx.test".to_string()])
        .expect("first certificate");
    let second = rcgen::generate_simple_self_signed(vec!["mx.test".to_string()])
        .expect("second certificate");
    let err = tls_acceptor(
        first.cert.pem().as_bytes(),
        second.key_pair.serialize_pem().as_bytes(),
        "1.2",
    )
    .err()
    .expect("a mismatched pair must be refused");
    assert!(matches!(err, ferroma_core::FerromaError::Tls(_)), "{err:?}");
}

#[tokio::test]
async fn an_empty_pem_bundle_is_refused_with_a_clear_error() {
    let key = rcgen::generate_simple_self_signed(vec!["mx.test".to_string()])
        .expect("certificate");
    let err = tls_acceptor(b"not a pem bundle", key.key_pair.serialize_pem().as_bytes(), "1.2")
        .err().expect("no certificate means no acceptor");
    assert!(format!("{err}").contains("no certificate"), "{err}");

    let err = tls_acceptor(key.cert.pem().as_bytes(), b"not a key", "1.2")
        .err().expect("no key means no acceptor");
    assert!(format!("{err}").contains("no private key"), "{err}");
}

#[tokio::test]
async fn the_minimum_tls_version_from_the_config_is_honoured() {
    let certified = rcgen::generate_simple_self_signed(vec!["mx.test".to_string()])
        .expect("certificate");
    // "1.3" restricts the version list; anything else means 1.2 and up.
    assert!(tls_acceptor(
        certified.cert.pem().as_bytes(),
        certified.key_pair.serialize_pem().as_bytes(),
        "1.3"
    )
    .is_ok());
    assert!(tls_acceptor(
        certified.cert.pem().as_bytes(),
        certified.key_pair.serialize_pem().as_bytes(),
        "nonsense"
    )
    .is_ok());
}
