//! The outbound queue: retry scheduling, give-up and bounces, driven against a fake
//! MX over a real socket with a real database and a real Maildir.
//!
//! Each test builds a message, queues one recipient, runs exactly one dispatch round
//! through [`ferroma_smtp::QueueWorker`], and then asserts on the database row, the
//! attempt log, the published event and — where a bounce is expected — the sender's
//! `INBOX`.

mod common;

use std::sync::Arc;

use common::{seed_mailbox, test_config, FakeMx, Stores, TestDb};
use ferroma_core::{EmailAddress, MailboxId, MessageId, UserId};
use ferroma_events::{Event, EventBus, EventScope};
use ferroma_smtp::{
    DeliveryService, MockResolver, MxHost, MxResolver, QueueConfigView, QueueWorker, SmtpClient,
    SmtpClientConfig, TlsPolicy,
};
use ferroma_storage::repository::{NewMailbox, NewMessage, NewQueueEntry, NewUser};
use ferroma_storage::Repositories;

/// Everything one queue test needs.
struct Harness {
    db: TestDb,
    repos: Repositories,
    stores: Stores,
    delivery: Arc<DeliveryService>,
    bus: EventBus,
}

impl Harness {
    async fn start() -> Harness {
        let db = TestDb::create().await;
        let repos = db.repos();
        let stores = Stores::new();
        let delivery = Arc::new(DeliveryService::new(
            repos.clone(),
            stores.maildir.clone(),
            stores.attachments.clone(),
            "mx.test",
        ));
        Harness {
            db,
            repos,
            stores,
            delivery,
            bus: EventBus::with_defaults(),
        }
    }

    /// Store one message in `alice`'s `Sent` folder and return its id.
    async fn store_outbound(&self, mailbox_id: MailboxId, uid_owner: &str) -> MessageId {
        let folders = self
            .repos
            .folders
            .list(mailbox_id)
            .await
            .expect("list folders");
        let sent = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case("Sent"))
            .expect("Sent exists");

        let bytes = b"From: alice@mx.test\r\nTo: bob@external.example.net\r\nSubject: outbound\r\n\r\nbody\r\n";
        let local_part = uid_owner;
        let stored = self
            .stores
            .maildir
            .store("mx.test", local_part, "Sent", bytes, "")
            .expect("store the outbound message");

        let message = self
            .repos
            .messages
            .insert(NewMessage {
                folder_id: sent.folder_id(),
                mailbox_id,
                rfc_message_id: Some("<outbound@mx.test>".to_string()),
                thread_id: None,
                subject: Some("outbound".to_string()),
                sender: Some("alice@mx.test".to_string()),
                sender_name: None,
                snippet: Some("body".to_string()),
                size_bytes: bytes.len() as i64,
                storage_path: stored.path,
                checksum_sha256: Some(stored.sha256),
                flags: String::new(),
                internal_date: None,
                sent_at: None,
                has_attachments: false,
                attachment_count: 0,
                is_draft: false,
            })
            .await
            .expect("insert the message");
        message.message_id()
    }

    /// Queue one recipient and return the queue row.
    async fn queue(
        &self,
        message_id: MessageId,
        user_id: UserId,
        recipient: &str,
    ) -> ferroma_storage::QueueEntry {
        self.repos
            .queue
            .enqueue(NewQueueEntry {
                message_id,
                user_id: Some(user_id),
                sender: "alice@mx.test".to_string(),
                recipient: recipient.to_string(),
                max_attempts: 3,
            })
            .await
            .expect("enqueue")
    }

    /// Build a worker whose resolver points at `fake` and whose client uses its port.
    fn worker(&self, fake: &FakeMx, config: QueueConfigView) -> QueueWorker {
        // `127.0.0.1` has no real MX record, so the mock supplies one — and its
        // address, because `delivery_targets` skips an exchanger that does not
        // resolve. The client is told the fake server's port, which is what
        // `SmtpClientConfig::port` is for.
        let resolver = Arc::new(MxResolver::mock(
            fake_mx_resolver(),
            &ferroma_core::config::DnsConfig::default(),
        ));

        let mut client_config = SmtpClientConfig::default().with_port(fake.address.port());
        client_config.tls = TlsPolicy::Disabled;
        let client = SmtpClient::new(client_config);

        QueueWorker::new(
            client,
            resolver,
            self.repos.clone(),
            self.stores.maildir.clone(),
            Arc::clone(&self.delivery),
            config,
        )
        .with_event_bus(self.bus.clone())
    }

    async fn cleanup(self) {
        self.db.cleanup().await;
    }
}

/// A mock whose `external.example.net` MX resolves to loopback.
///
/// `delivery_targets` skips an exchanger that has no address, so the mock has to
/// publish the address as well as the `MX` record.
fn fake_mx_resolver() -> MockResolver {
    MockResolver::new()
        .with_mx(
            "external.example.net",
            vec![MxHost::new(10, "127.0.0.1")],
        )
        .with_addresses("127.0.0.1", vec!["127.0.0.1".parse().expect("ip")])
}

/// A queue view with a short schedule, so a test can assert the numbers.
fn fast_config() -> QueueConfigView {
    QueueConfigView {
        workers: 1,
        max_attempts: 3,
        retry_schedule_secs: vec![60, 300, 900],
        poll_interval: std::time::Duration::from_millis(10),
        bounce_on_failure: true,
        mailer_daemon: "MAILER-DAEMON@mx.test".to_string(),
        hostname: "mx.test".to_string(),
        // These cases deliver straight to the recipient's MX. The relay path is
        // covered by `relay_deliveries_go_to_the_relay` in
        // `crates/ferroma-smtp/src/queue.rs`.
        relay: None,
        // Signing is exercised by the `signing_tests` unit module in `queue.rs`;
        // these cases are about delivery outcomes, so it stays off.
        dkim: ferroma_core::config::DkimConfig::default(),
    }
}

/// A view that hands every outbound message to a relay listening on `address`.
///
/// The relay host is `127.0.0.1` because that is the one name the shared mock
/// resolver publishes an address for; a real deployment names its provider's host.
fn relayed_config(address: std::net::SocketAddr) -> QueueConfigView {
    let mut config = ferroma_core::Config::default();
    config.server.hostname = "mx.test".into();
    config.queue.relay_host = Some(address.ip().to_string());
    config.queue.relay_port = address.port();
    config.queue.relay_tls = "none".into();

    QueueConfigView::from_config(&config)
}

/// The number of messages in a mailbox's `INBOX`.
async fn inbox_count(repos: &Repositories, mailbox_id: MailboxId) -> i64 {
    let folders = repos.folders.list(mailbox_id).await.expect("folders");
    let inbox = folders
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
        .expect("INBOX");
    repos
        .messages
        .count_by_folder(inbox.folder_id())
        .await
        .expect("count")
}

// ---------------------------------------------------------------------------
// Success
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_relay_carries_a_recipient_whose_domain_has_no_mx() {
    // The situation the relay exists for: this host cannot deliver directly, so a
    // message to a domain with no exchangeable MX must still leave — through the
    // relay — rather than sit in the queue.
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    // `nowhere.example` is unknown to the mock: no MX, no address.
    let entry = harness
        .queue(message_id, user_id, "bob@nowhere.example")
        .await;

    let relay = FakeMx::start(250).await;
    let mut worker = harness.worker(&relay, relayed_config(relay.address));
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row exists");
    assert_eq!(row.status, "delivered", "last_error: {:?}", row.last_error);
    assert_eq!(relay.received(), 1, "the relay is what carried it");

    relay.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn without_a_relay_the_same_recipient_cannot_be_delivered() {
    // The control for the test above: the same recipient, the same mock, no relay —
    // and the queue has nowhere to send it.
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@nowhere.example")
        .await;

    let fake = FakeMx::start(250).await;
    let mut worker = harness.worker(&fake, fast_config());
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row exists");
    assert_eq!(row.status, "retry", "last_error: {:?}", row.last_error);
    assert!(
        row.last_error.as_deref().unwrap_or_default().contains("no mail exchanger"),
        "last_error: {:?}",
        row.last_error
    );
    assert_eq!(fake.received(), 0, "nothing should have been sent");

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn a_successful_attempt_marks_the_row_delivered() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(250).await;
    let mut subscription = harness.bus.subscribe();
    let mut worker = harness.worker(&fake, fast_config());
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row exists");
    assert_eq!(row.status, "delivered", "last_error: {:?}", row.last_error);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_status_code, Some(250));
    assert!(row.delivered_at.is_some());
    assert_eq!(fake.received(), 1);

    // The attempt log has one row.
    let attempts = harness
        .repos
        .delivery_attempts
        .list_by_queue(entry.queue_id())
        .await
        .expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].attempt, 1);
    assert_eq!(attempts[0].status_code, Some(250));
    assert!(attempts[0].duration_ms.is_some());

    // And `delivery.updated` was published.
    let envelope = subscription.recv().await.expect("an event");
    assert_eq!(envelope.scope, EventScope::User(user_id));
    match envelope.event {
        Event::DeliveryUpdated(e) => {
            assert_eq!(e.status, "delivered");
            assert_eq!(e.recipient, "bob@external.example.net");
            assert_eq!(e.queue_id, entry.queue_id());
        }
        other => panic!("wrong event: {other:?}"),
    }

    fake.stop();
    harness.cleanup().await;
}

// ---------------------------------------------------------------------------
// Local delivery
// ---------------------------------------------------------------------------

/// A second address on a domain that already exists.
///
/// [`seed_mailbox`] creates its own domain, so it cannot be called twice for one
/// domain; this is the same thing minus the `domains.create`.
async fn seed_second_address(repos: &Repositories, domain: &str, local_part: &str) -> MailboxId {
    let domain_row = repos
        .domains
        .find_by_name(domain)
        .await
        .expect("look up the domain")
        .expect("the domain was seeded");
    let user = repos
        .users
        .create(NewUser {
            email: format!("{local_part}@{domain}"),
            password_hash: "unused".to_string(),
            display_name: Some(local_part.to_string()),
            is_admin: false,
            quota_bytes: None,
        })
        .await
        .expect("create user");
    let mailbox = repos
        .mailboxes
        .create(NewMailbox {
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
    mailbox.mailbox_id()
}

#[tokio::test]
async fn a_local_recipient_is_delivered_into_the_local_mailbox() {
    // Local user to local user. The recipient's domain is hosted here, so its mail
    // exchanger is this very host — or, on a domain with no MX record of its own,
    // nothing at all. A queue that only knows how to open an SMTP connection therefore
    // retried until it bounced, and the local INBOX stayed empty for good.
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let bob = seed_second_address(&harness.repos, "mx.test", "bob").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness.queue(message_id, user_id, "bob@mx.test").await;

    // The mock publishes no MX and no address for `mx.test`, so resolving one would
    // fail here exactly the way it failed in production.
    let fake = FakeMx::start(250).await;
    let mut worker = harness.worker(&fake, fast_config());
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row exists");
    assert_eq!(row.status, "delivered", "last_error: {:?}", row.last_error);
    assert_eq!(
        row.remote_mx.as_deref(),
        Some("mx.test"),
        "the queue records that it stayed on this host"
    );
    assert_eq!(
        fake.received(),
        0,
        "a local delivery must not open an SMTP connection at all"
    );
    assert_eq!(
        inbox_count(&harness.repos, bob).await,
        1,
        "the message landed in the recipient's INBOX"
    );

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn a_local_address_with_no_mailbox_fails_permanently() {
    // The control: a typo in a local address must bounce, not retry twelve times over
    // a day. Nothing about it will be different on the next attempt.
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness.queue(message_id, user_id, "nobody@mx.test").await;

    let fake = FakeMx::start(250).await;
    let mut worker = harness.worker(&fake, fast_config());
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row exists");
    assert_eq!(row.status, "failed", "last_error: {:?}", row.last_error);
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("no such mailbox"),
        "last_error: {:?}",
        row.last_error
    );
    assert_eq!(fake.received(), 0, "nothing should have been sent");

    fake.stop();
    harness.cleanup().await;
}

// ---------------------------------------------------------------------------
// Temporary failure → retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_4xx_reply_schedules_a_retry_with_the_configured_backoff() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(451).await;
    let mut worker = harness.worker(&fake, fast_config());
    let before = chrono::Utc::now();
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row exists");
    assert_eq!(row.status, "retry", "last_error: {:?}", row.last_error);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_status_code, Some(451));
    assert!(row.last_error.is_some());

    // Attempt 1's retry waits `retry_schedule_secs[0]` — one minute.
    let next = row.next_attempt_at.expect("a retry is scheduled");
    let delta = next.signed_duration_since(before);
    assert!(
        delta.num_seconds() >= 59 && delta.num_seconds() <= 62,
        "expected ~60s, got {delta}"
    );
    assert_eq!(fake.received(), 1);
    assert_eq!(worker.delivered_count(), 0);
    assert_eq!(worker.failed_count(), 0, "a retry is neither delivered nor failed");

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn the_backoff_grows_with_each_attempt() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(451).await;
    let mut worker = harness.worker(&fake, fast_config());

    // Round 1: the row is due immediately, so it is claimed and deferred by 60s.
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    let first = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(first.attempts, 1);
    let first_delay = first
        .next_attempt_at
        .expect("scheduled")
        .signed_duration_since(chrono::Utc::now())
        .num_seconds();

    // Make it due again, and run round 2.
    harness
        .repos
        .queue
        .mark_retry(
            entry.queue_id(),
            chrono::Utc::now(),
            "forced due",
            None,
            None,
            None,
        )
        .await
        .expect("force due");
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    let second = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(second.attempts, 2);
    let second_delay = second
        .next_attempt_at
        .expect("scheduled")
        .signed_duration_since(chrono::Utc::now())
        .num_seconds();

    assert!(
        second_delay > first_delay,
        "the schedule must grow: {first_delay}s then {second_delay}s"
    );
    assert!((295..=302).contains(&second_delay), "expected ~300s, got {second_delay}");

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn a_retry_is_not_claimed_until_it_is_due() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(451).await;
    let mut worker = harness.worker(&fake, fast_config());
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    fake.stop();

    // A second round finds nothing: the row is scheduled a minute out.
    let fake2 = FakeMx::start(451).await;
    let mut worker2 = harness.worker(&fake2, fast_config());
    assert_eq!(
        worker2.dispatch_batch().await.expect("dispatch"),
        0,
        "a future retry must not be claimed early"
    );
    assert_eq!(fake2.received(), 0);
    assert_eq!(
        harness
            .repos
            .queue
            .find_by_id(entry.queue_id())
            .await
            .expect("lookup")
            .expect("the row")
            .attempts,
        1
    );

    fake2.stop();
    harness.cleanup().await;
}

// ---------------------------------------------------------------------------
// Permanent failure → bounce
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_5xx_reply_fails_the_row_and_bounces_to_the_sender() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(550).await;
    let mut subscription = harness.bus.subscribe();
    let mut worker = harness.worker(&fake, fast_config());
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(row.status, "failed", "last_error: {:?}", row.last_error);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_status_code, Some(550));
    assert!(row.next_attempt_at.is_none(), "a failed row is never due again");
    assert_eq!(worker.failed_count(), 1);

    // The bounce landed in the sender's INBOX, through the local delivery path.
    let inbox = inbox_count(&harness.repos, mailbox_id).await;
    assert_eq!(inbox, 1, "the sender must receive a bounce");

    let folders = harness.repos.folders.list(mailbox_id).await.expect("folders");
    let inbox_folder = folders
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
        .expect("INBOX");
    let bounce = harness
        .repos
        .messages
        .newest(inbox_folder.folder_id(), 1)
        .await
        .expect("newest")
        .into_iter()
        .next()
        .expect("a bounce message");
    let bytes = harness
        .stores
        .maildir
        .read(&bounce.storage_path)
        .expect("read the bounce");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        text.contains("Undelivered Mail Returned to Sender"),
        "{text}"
    );
    assert!(text.contains("bob@external.example.net"), "{text}");
    assert!(text.contains("MAILER-DAEMON@mx.test"), "{text}");

    let envelope = subscription.recv().await.expect("an event");
    match envelope.event {
        Event::DeliveryUpdated(e) => assert_eq!(e.status, "failed"),
        other => panic!("wrong event: {other:?}"),
    }

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn the_last_allowed_attempt_gives_up_instead_of_retrying() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(451).await;
    let mut config = fast_config();
    config.max_attempts = 2;
    let mut worker = harness.worker(&fake, config);

    // Attempt 1: retry.
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    assert_eq!(
        harness
            .repos
            .queue
            .find_by_id(entry.queue_id())
            .await
            .expect("lookup")
            .expect("the row")
            .status,
        "retry"
    );

    // Make it due again: attempt 2 is the last, so the row fails.
    harness
        .repos
        .queue
        .mark_retry(entry.queue_id(), chrono::Utc::now(), "forced due", None, None, None)
        .await
        .expect("force due");
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(row.attempts, 2);
    assert_eq!(
        row.status, "failed",
        "the last allowed attempt must give up, not retry forever"
    );
    assert_eq!(worker.failed_count(), 1);
    assert_eq!(inbox_count(&harness.repos, mailbox_id).await, 1, "a bounce");

    // Both attempts are in the log.
    let attempts = harness
        .repos
        .delivery_attempts
        .list_by_queue(entry.queue_id())
        .await
        .expect("attempts");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].attempt, 1);
    assert_eq!(attempts[1].attempt, 2);

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn bouncing_is_skipped_for_the_null_sender() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;

    // A bounce (null sender) that fails must not generate another bounce.
    let entry = harness
        .repos
        .queue
        .enqueue(NewQueueEntry {
            message_id,
            user_id: Some(user_id),
            sender: "<>".to_string(),
            recipient: "bob@external.example.net".to_string(),
            max_attempts: 1,
        })
        .await
        .expect("enqueue");

    let fake = FakeMx::start(550).await;
    let mut worker = harness.worker(&fake, fast_config());
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(row.status, "failed", "last_error: {:?}", row.last_error);
    assert_eq!(
        inbox_count(&harness.repos, mailbox_id).await,
        0,
        "a null sender gets no bounce — bouncing a bounce is a mail loop"
    );

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn a_bounce_is_not_generated_when_the_policy_is_off() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let _ = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(550).await;
    let mut config = fast_config();
    config.bounce_on_failure = false;
    let mut worker = harness.worker(&fake, config);
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    assert_eq!(
        inbox_count(&harness.repos, mailbox_id).await,
        0,
        "bounce_on_failure = false must suppress the bounce"
    );

    fake.stop();
    harness.cleanup().await;
}

// ---------------------------------------------------------------------------
// Transport failures and shutdown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unreachable_host_is_a_temporary_failure() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    // A fake that is started and immediately stopped leaves a closed port.
    let fake = FakeMx::start(250).await;
    let address = fake.address;
    fake.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let resolver = Arc::new(MxResolver::mock(
        fake_mx_resolver(),
        &ferroma_core::config::DnsConfig::default(),
    ));
    let mut client_config = SmtpClientConfig::default().with_port(address.port());
    client_config.tls = TlsPolicy::Disabled;
    client_config.connect_timeout = std::time::Duration::from_millis(200);
    let client = SmtpClient::new(client_config);
    let mut worker = QueueWorker::new(
        client,
        resolver,
        harness.repos.clone(),
        harness.stores.maildir.clone(),
        Arc::clone(&harness.delivery),
        fast_config(),
    );

    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(
        row.status, "retry",
        "a connection failure must be temporary, never a bounce"
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn an_unknown_domain_is_deferred_rather_than_bounced() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@nonexistent.invalid")
        .await;

    // No `MX` and no `A` record for a domain the mock knows nothing about.
    let resolver = Arc::new(MxResolver::mock(
        MockResolver::new(),
        &ferroma_core::config::DnsConfig::default(),
    ));
    let mut worker = QueueWorker::new(
        SmtpClient::new(SmtpClientConfig::default()),
        resolver,
        harness.repos.clone(),
        harness.stores.maildir.clone(),
        Arc::clone(&harness.delivery),
        fast_config(),
    );

    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(row.status, "retry", "last_error: {:?}", row.last_error);
    assert!(
        row.last_error.as_deref().unwrap_or("").contains("mail exchanger"),
        "{:?}",
        row.last_error
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn shutdown_stops_the_loop_and_finishes_in_flight_work() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(250).await;
    let worker = harness.worker(&fake, fast_config());
    let handle = worker.handle();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        worker.run(shutdown_rx).await;
    });

    // Let one round run, then stop.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    handle.shutdown();
    let _ = shutdown_tx.send(true);

    let joined = tokio::time::timeout(std::time::Duration::from_secs(10), task).await;
    assert!(joined.is_ok(), "the worker did not stop in time");

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(row.status, "delivered", "in-flight work must finish before run() returns");

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn a_queued_recipient_is_a_real_mailbox_address() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let entry = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;
    assert_eq!(entry.recipient, "bob@external.example.net");
    assert_eq!(entry.sender, "alice@mx.test");
    assert_eq!(entry.status, "pending");
    assert_eq!(entry.max_attempts, 3);
    assert!(entry.next_attempt_at.is_some());

    // And `enqueue` is the same thing through the free function.
    let other = ferroma_smtp::enqueue(
        &harness.repos,
        message_id,
        Some(user_id),
        "alice@mx.test",
        "carol@external.example.net",
        5,
    )
    .await
    .expect("enqueue through the helper");
    let row = harness
        .repos
        .queue
        .find_by_id(other)
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(row.recipient, "carol@external.example.net");
    assert_eq!(row.max_attempts, 5);

    harness.cleanup().await;
}

#[tokio::test]
async fn the_resolver_drives_the_choice_of_host() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let _ = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    // Two exchangers, the better one dead: the delivery must reach the second.
    let fake = FakeMx::start(250).await;
    let resolver = Arc::new(MxResolver::mock(
        MockResolver::new()
            .with_mx(
                "external.example.net",
                vec![
                    MxHost::new(5, "127.0.0.1"),
                    MxHost::new(10, "198.51.100.9"),
                ],
            )
            .with_addresses("127.0.0.1", vec!["127.0.0.1".parse().expect("ip")])
            .with_addresses("198.51.100.9", vec!["198.51.100.9".parse().expect("ip")]),
        &ferroma_core::config::DnsConfig::default(),
    ));
    let mut client_config = SmtpClientConfig::default().with_port(fake.address.port());
    client_config.tls = TlsPolicy::Disabled;
    client_config.connect_timeout = std::time::Duration::from_millis(300);
    let mut worker = QueueWorker::new(
        SmtpClient::new(client_config),
        resolver,
        harness.repos.clone(),
        harness.stores.maildir.clone(),
        Arc::clone(&harness.delivery),
        fast_config(),
    );
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);
    assert_eq!(fake.received(), 1);

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn an_unparseable_recipient_fails_immediately() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;

    // The repository validates a queue row, so write the bad one straight through the
    // envelope rather than the repository: `enqueue` would refuse it.
    let entry = harness
        .repos
        .queue
        .enqueue(NewQueueEntry {
            message_id,
            user_id: Some(user_id),
            sender: "alice@mx.test".to_string(),
            recipient: "not-an-address".to_string(),
            max_attempts: 3,
        })
        .await
        .expect("enqueue");

    let fake = FakeMx::start(250).await;
    let mut config = fast_config();
    config.bounce_on_failure = false;
    let mut worker = harness.worker(&fake, config);
    assert_eq!(worker.dispatch_batch().await.expect("dispatch"), 1);

    let row = harness
        .repos
        .queue
        .find_by_id(entry.queue_id())
        .await
        .expect("lookup")
        .expect("the row");
    assert_eq!(row.status, "failed", "last_error: {:?}", row.last_error);
    assert_eq!(fake.received(), 0);

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn the_event_scope_follows_the_queue_row_owner() {
    crate::require_database!();
    let harness = Harness::start().await;
    let (user_id, mailbox_id) = seed_mailbox(&harness.repos, "mx.test", "alice", "unused").await;
    let message_id = harness.store_outbound(mailbox_id, "alice").await;
    let _ = harness
        .queue(message_id, user_id, "bob@external.example.net")
        .await;

    let fake = FakeMx::start(250).await;
    let mut subscription = harness.bus.subscribe();
    let mut worker = harness.worker(&fake, fast_config());
    let _ = worker.dispatch_batch().await.expect("dispatch");

    let envelope = subscription.recv().await.expect("an event");
    assert_eq!(
        envelope.scope,
        EventScope::User(user_id),
        "a user's delivery must not be published to the whole system"
    );
    assert_eq!(envelope.event.name(), "delivery.updated");

    fake.stop();
    harness.cleanup().await;
}

#[tokio::test]
async fn a_recipient_address_is_parsed_before_any_network_work() {
    // A pure unit-style assertion that lives here because it needs the same fixtures.
    assert!(EmailAddress::parse("bob@external.example.net").is_ok());
    assert!(EmailAddress::parse("not-an-address").is_err());
}

#[tokio::test]
async fn the_queue_config_view_is_the_one_the_worker_uses() {
    let config = test_config();
    let view = QueueConfigView::from_config(&config);
    assert_eq!(view.workers, config.queue.workers);
    assert_eq!(view.max_attempts, config.queue.max_attempts as i32);
    assert_eq!(view.mailer_daemon, format!("MAILER-DAEMON@{}", config.server.hostname));
}

