//! Integration tests for the client-facing repositories: the outbound queue,
//! sessions and devices, the sync journal, drafts, audit, throttling and settings.
//!
//! Like `tests/repositories.rs`, every test gets its own freshly migrated schema and
//! can therefore run alongside its neighbours.

mod common;

use chrono::{DateTime, TimeZone, Utc};
use ferroma_core::{Cursor, DeviceId, MailboxId, MessageId, QueueId, SessionId, UserId};
use ferroma_storage::models::User;
use ferroma_storage::repository::{
    AuditFilter, DeviceUpsert, DraftUpdate, NewAuditLog, NewChange, NewDraft, NewMailbox, NewMessage,
    NewQueueEntry, NewSession, NewUser,
};
use ferroma_storage::StorageError;

use common::{fresh_database, TestDatabase};

/// Skip when PostgreSQL is unreachable, otherwise hand back a pristine migrated
/// schema.
macro_rules! setup {
    () => {{
        if !common::database_available().await {
            eprintln!(
                "skipping: no PostgreSQL reachable at {}",
                common::admin_url()
            );
            return;
        }
        fresh_database().await
    }};
}

/// A fixed instant, so scheduling assertions are deterministic.
fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
}

/// The seed every test starts from.
struct Fixture {
    user_id: UserId,
    mailbox_id: MailboxId,
    inbox_id: MailboxId,
    message_id: MessageId,
}

/// Seed a user, a domain, an address, its folders and one stored message — the queue
/// needs a real message row to point at.
async fn fixture(t: &TestDatabase) -> Fixture {
    let repos = t.repos();

    let user: User = repos
        .users
        .create(NewUser {
            email: "alice@example.com".into(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
            display_name: Some("Alice".into()),
            is_admin: false,
            enabled: true,
            quota_bytes: None,
        })
        .await
        .expect("seed user");

    let domain = repos
        .domains
        .create("example.com", None)
        .await
        .expect("seed domain");

    let mailbox = repos
        .mailboxes
        .create(NewMailbox {
            user_id: user.user_id(),
            domain_id: domain.domain_id(),
            local_part: "alice".into(),
            display_name: None,
            is_primary: true,
            quota_bytes: None,
        })
        .await
        .expect("seed mailbox");

    let folders = repos
        .folders
        .ensure_standard(mailbox.mailbox_id())
        .await
        .expect("seed folders");
    let inbox_id = folders
        .iter()
        .find(|folder| folder.is_inbox())
        .expect("INBOX")
        .folder_id();

    let message = repos
        .messages
        .insert(NewMessage {
            folder_id: inbox_id,
            mailbox_id: mailbox.mailbox_id(),
            rfc_message_id: Some("<queued@example.com>".into()),
            thread_id: None,
            subject: Some("outbound".into()),
            sender: Some("alice@example.com".into()),
            sender_name: Some("Alice".into()),
            snippet: None,
            size_bytes: 256,
            storage_path: "example.com/alice/Maildir/cur/queued:2,".into(),
            checksum_sha256: None,
            flags: String::new(),
            internal_date: Some(at(0)),
            sent_at: None,
            has_attachments: false,
            attachment_count: 0,
            is_draft: false,
        })
        .await
        .expect("seed message");

    Fixture {
        user_id: user.user_id(),
        mailbox_id: mailbox.mailbox_id(),
        inbox_id,
        message_id: message.message_id(),
    }
}

/// Queue one recipient for the fixture's message.
async fn enqueue(t: &TestDatabase, f: &Fixture, recipient: &str) -> QueueId {
    t.repos()
        .queue
        .enqueue(NewQueueEntry {
            message_id: f.message_id,
            user_id: Some(f.user_id),
            sender: "alice@example.com".into(),
            recipient: recipient.into(),
            max_attempts: 5,
        })
        .await
        .expect("enqueue")
        .queue_id()
}

/// Register a device, which most session tests need.
async fn device(t: &TestDatabase, f: &Fixture, uid: &str) -> DeviceId {
    t.repos()
        .devices
        .upsert(DeviceUpsert {
            user_id: f.user_id,
            device_uid: uid.into(),
            name: Some("Workstation".into()),
            platform: Some("windows".into()),
            client_version: Some("0.1.0".into()),
            protocol_version: Some(1),
            ip: Some("203.0.113.7".into()),
        })
        .await
        .expect("upsert device")
        .device_id()
}

/// Open one session that expires `expires_in_secs` from now (negative = already
/// expired).
async fn session(t: &TestDatabase, f: &Fixture, token_hash: &str, expires_in_secs: i64) -> SessionId {
    t.repos()
        .sessions
        .create(NewSession {
            user_id: f.user_id,
            kind: "web".into(),
            token_hash: token_hash.into(),
            device_id: None,
            ip: Some("203.0.113.7".into()),
            user_agent: Some("ferroma-test/0.1".into()),
            expires_at: Utc::now() + chrono::Duration::seconds(expires_in_secs),
        })
        .await
        .expect("create session")
        .session_id()
}

// ===========================================================================
// the outbound queue
// ===========================================================================

#[tokio::test]
async fn queue_enqueue_then_claim_takes_it_exactly_once() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let id = enqueue(&t, &f, "bob@example.net").await;
    let entry = repos.queue.find_by_id(id).await.unwrap().expect("queued");
    assert_eq!(entry.status, "pending");
    assert_eq!(entry.attempts, 0);
    assert_eq!(entry.max_attempts, 5);
    assert!(entry.next_attempt_at.is_some(), "a new entry is due immediately");
    assert!(entry.delivered_at.is_none());
    assert_eq!(entry.sender, "alice@example.com");
    assert_eq!(entry.recipient, "bob@example.net");
    assert_eq!(entry.user_id, Some(f.user_id.get()));

    let claimed = repos.queue.claim_due(10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, id.get());
    assert_eq!(claimed[0].status, "delivering");
    assert_eq!(claimed[0].attempts, 1);
    assert!(claimed[0].last_attempt_at.is_some());

    // The row moved out of the due set, so it cannot be claimed twice.
    assert!(repos.queue.claim_due(10).await.unwrap().is_empty());
    assert_eq!(
        repos.queue.find_by_id(id).await.unwrap().unwrap().attempts,
        1
    );

    t.cleanup().await;
}

#[tokio::test]
async fn queue_claim_respects_the_limit_and_the_due_time() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    for recipient in ["a@example.net", "b@example.net", "c@example.net"] {
        enqueue(&t, &f, recipient).await;
    }
    assert!(repos.queue.claim_due(0).await.unwrap().is_empty());
    assert_eq!(repos.queue.claim_due(2).await.unwrap().len(), 2);
    assert_eq!(repos.queue.claim_due(10).await.unwrap().len(), 1);

    // An entry scheduled in the future is not due.
    let late = enqueue(&t, &f, "d@example.net").await;
    repos
        .queue
        .mark_retry(late, Utc::now() + chrono::Duration::hours(1), "later", None, None, None)
        .await
        .unwrap();
    assert!(repos.queue.claim_due(10).await.unwrap().is_empty());

    t.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_concurrent_claims_never_hand_out_the_same_row() {
    // A wide pool on purpose: the property under test is that `FOR UPDATE SKIP
    // LOCKED` serializes the claims at the row level. With the default two-connection
    // pool the four workers would queue on connection acquisition instead, which
    // tests something weaker.
    let t = common::fresh_database_with_pool(16).await;
    let f = fixture(&t).await;
    let repos = t.repos();

    for index in 0..24 {
        enqueue(&t, &f, &format!("r{index}@example.net")).await;
    }

    let mut handles = Vec::new();
    for _ in 0..4 {
        let repos = repos.clone();
        handles.push(tokio::spawn(async move {
            repos.queue.claim_due(24).await.expect("claim")
        }));
    }

    let mut claimed = Vec::new();
    for handle in handles {
        claimed.extend(handle.await.expect("claim task").into_iter().map(|e| e.id));
    }

    assert_eq!(claimed.len(), 24, "every due row must be claimed exactly once");
    let mut sorted = claimed.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 24, "no row may be claimed by two workers");

    assert_eq!(repos.queue.count_by_status("delivering").await.unwrap(), 24);
    assert_eq!(repos.queue.count_by_status("pending").await.unwrap(), 0);

    t.cleanup().await;
}

#[tokio::test]
async fn queue_mark_delivered_sets_the_final_state() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let id = enqueue(&t, &f, "bob@example.net").await;
    repos.queue.claim_due(1).await.unwrap();
    repos
        .queue
        .mark_delivered(id, Some("mx1.example.net"), Some(250), Some("2.0.0 Ok"))
        .await
        .unwrap();

    let entry = repos.queue.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(entry.status, "delivered");
    assert!(entry.delivered_at.is_some());
    assert!(entry.next_attempt_at.is_none());
    assert!(entry.last_error.is_none());
    assert_eq!(entry.remote_mx.as_deref(), Some("mx1.example.net"));
    assert_eq!(entry.last_status_code, Some(250));
    assert_eq!(entry.last_status_text.as_deref(), Some("2.0.0 Ok"));
    assert_eq!(entry.attempts, 1);

    let err = repos
        .queue
        .mark_delivered(QueueId::new(9999), None, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn queue_mark_retry_schedules_and_keeps_details() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let id = enqueue(&t, &f, "bob@example.net").await;
    repos.queue.claim_due(1).await.unwrap();

    let later = at(600);
    repos
        .queue
        .mark_retry(id, later, "451 try later", Some("mx2.example.net"), Some(451), Some("4.7.1"))
        .await
        .unwrap();

    let entry = repos.queue.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(entry.status, "retry");
    assert_eq!(entry.next_attempt_at, Some(later));
    assert_eq!(entry.last_error.as_deref(), Some("451 try later"));
    assert_eq!(entry.remote_mx.as_deref(), Some("mx2.example.net"));
    assert_eq!(entry.last_status_code, Some(451));
    assert_eq!(entry.last_status_text.as_deref(), Some("4.7.1"));
    assert!(entry.delivered_at.is_none());

    // Back in the due set once the clock passes.
    repos
        .queue
        .mark_retry(id, at(-5), "soon", None, None, None)
        .await
        .unwrap();
    let reclaimed = repos.queue.claim_due(1).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].attempts, 2, "a retry counts as another attempt");
    // `None` remote details leave the previous ones in place.
    assert_eq!(reclaimed[0].remote_mx.as_deref(), Some("mx2.example.net"));

    t.cleanup().await;
}

#[tokio::test]
async fn queue_mark_failed_stops_the_attempts() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let id = enqueue(&t, &f, "bob@example.net").await;
    repos.queue.claim_due(1).await.unwrap();
    repos
        .queue
        .mark_failed(id, "550 no such user", Some("mx1.example.net"), Some(550), Some("5.1.1"))
        .await
        .unwrap();

    let entry = repos.queue.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(entry.status, "failed");
    assert!(entry.next_attempt_at.is_none());
    assert_eq!(entry.last_error.as_deref(), Some("550 no such user"));
    assert_eq!(entry.last_status_code, Some(550));
    assert!(!entry.is_due());
    assert!(repos.queue.claim_due(10).await.unwrap().is_empty());

    t.cleanup().await;
}

#[tokio::test]
async fn queue_cancel_only_touches_unfinished_entries() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let pending = enqueue(&t, &f, "pending@example.net").await;
    let delivered = enqueue(&t, &f, "done@example.net").await;
    repos.queue.claim_due(10).await.unwrap();
    repos
        .queue
        .mark_delivered(delivered, None, Some(250), None)
        .await
        .unwrap();

    assert!(!repos.queue.cancel(pending).await.unwrap(), "in-flight delivery cannot be cancelled");
    repos.queue.mark_retry(pending, chrono::Utc::now(), "retry", None, None, None)
        .await.unwrap();
    assert!(repos.queue.cancel(pending).await.unwrap());
    assert!(!repos.queue.cancel(pending).await.unwrap(), "already cancelled");
    assert!(!repos.queue.cancel(delivered).await.unwrap(), "delivered is final");
    assert!(!repos.queue.cancel(QueueId::new(9999)).await.unwrap());

    assert_eq!(
        repos.queue.find_by_id(pending).await.unwrap().unwrap().status,
        "cancelled"
    );

    t.cleanup().await;
}

#[tokio::test]
async fn queue_requeue_stale_recovers_a_crashed_worker() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let stuck = enqueue(&t, &f, "stuck@example.net").await;
    let fresh = enqueue(&t, &f, "fresh@example.net").await;
    let claimed = repos.queue.claim_due(10).await.unwrap();
    assert_eq!(claimed.len(), 2);

    // Nothing is old enough yet.
    assert_eq!(repos.queue.requeue_stale(Utc::now() - chrono::Duration::hours(1)).await.unwrap(), 0);

    // Pretend the worker died: its attempt started two hours ago.
    sqlx::query("UPDATE mail_queue SET last_attempt_at = $2 WHERE id = $1")
        .bind(stuck.get())
        .bind(Utc::now() - chrono::Duration::hours(2))
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(
        repos
            .queue
            .requeue_stale(Utc::now() - chrono::Duration::minutes(30))
            .await
            .unwrap(),
        1
    );

    let recovered = repos.queue.find_by_id(stuck).await.unwrap().unwrap();
    assert_eq!(recovered.status, "retry");
    assert!(recovered.next_attempt_at.is_some());
    assert_eq!(recovered.last_error.as_deref(), Some("requeued after worker restart"));

    // The freshly claimed entry was left alone.
    assert_eq!(
        repos.queue.find_by_id(fresh).await.unwrap().unwrap().status,
        "delivering"
    );

    t.cleanup().await;
}

#[tokio::test]
async fn stale_worker_cannot_overwrite_reclaimed_attempt() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let id = enqueue(&t, &f, "leased@example.net").await;
    let first = repos.queue.claim_due(1).await.unwrap().pop().unwrap();
    assert_eq!(first.attempts, 1);
    repos.queue.requeue_stale(Utc::now()).await.unwrap();
    let newer = repos.queue.claim_due(1).await.unwrap().pop().unwrap();
    assert_eq!(newer.attempts, 2);
    assert!(!repos.queue.finish_claim(id, first.attempts, "delivered",
        None, None, None, Some(250), None, false).await.unwrap());
    assert_eq!(repos.queue.find_by_id(id).await.unwrap().unwrap().status, "delivering");
    assert!(repos.queue.finish_claim(id, newer.attempts, "retry",
        Some(Utc::now()), Some("temporary"), None, Some(451), None, false).await.unwrap());
    assert_eq!(repos.queue.find_by_id(id).await.unwrap().unwrap().status, "retry");
    assert!(!repos.queue.finish_claim(id, newer.attempts, "failed",
        None, Some("too late"), None, None, None, true).await.unwrap());
    t.cleanup().await;
}

#[tokio::test]
async fn bounce_task_persists_failure_retry_and_completion() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let id = enqueue(&t, &f, "dsn@example.net").await;
    let delivery = repos.queue.claim_due(1).await.unwrap().pop().unwrap();
    assert!(repos.queue.finish_claim(id, delivery.attempts, "failed", None,
        Some("550 rejected"), None, Some(550), None, true).await.unwrap());
    let failed = repos.queue.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.bounce_status, "pending");
    assert_eq!(failed.bounce_attempts, 0);
    assert!(failed.bounce_next_attempt_at.is_some());

    let first = repos.queue.claim_due_bounces(1).await.unwrap().pop().unwrap();
    assert_eq!(first.bounce_status, "processing");
    assert_eq!(first.bounce_attempts, 1);
    assert!(first.bounce_claimed_at.is_some());
    assert!(repos.queue.claim_due_bounces(1).await.unwrap().is_empty());
    // A queued DSN still owns the original message body.
    let blocked = sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(f.message_id.get()).execute(t.pool()).await;
    assert!(blocked.is_err(), "pending/processing DSN must prevent cascading deletion");
    let parent_blocked = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(f.user_id.get()).execute(t.pool()).await;
    assert!(parent_blocked.is_err(), "parent cascade must preserve a pending DSN body");

    let later = Utc::now() + chrono::Duration::hours(1);
    assert!(repos.queue.finish_bounce(id, first.bounce_attempts, false, Some(later), None).await.unwrap());
    let retry = repos.queue.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(retry.bounce_status, "pending");
    assert_eq!(retry.bounce_next_attempt_at.unwrap().timestamp_micros(), later.timestamp_micros());
    assert!(retry.bounce_claimed_at.is_none());
    assert!(repos.queue.claim_due_bounces(1).await.unwrap().is_empty());
    sqlx::query("UPDATE mail_queue SET bounce_next_attempt_at = NOW() - INTERVAL '1 second' WHERE id = $1")
        .bind(id.get()).execute(t.pool()).await.unwrap();
    let second = repos.queue.claim_due_bounces(1).await.unwrap().pop().unwrap();
    assert_eq!(second.bounce_attempts, 2);
    assert!(!repos.queue.finish_bounce(id, first.bounce_attempts, true, None, None).await.unwrap());
    assert!(repos.queue.finish_bounce(id, second.bounce_attempts, true, None,
        Some(f.message_id)).await.unwrap());
    let sent = repos.queue.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(sent.bounce_status, "sent");
    assert_eq!(sent.bounce_message_id, Some(f.message_id.get()));
    assert!(sent.bounce_next_attempt_at.is_none());
    assert!(repos.queue.claim_due_bounces(1).await.unwrap().is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn bounce_stale_recovery_and_historical_failures_are_safe() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let old = enqueue(&t, &f, "old@example.net").await;
    repos.queue.mark_failed(old, "previous failure", None, None, None).await.unwrap();
    assert_eq!(repos.queue.find_by_id(old).await.unwrap().unwrap().bounce_status, "skipped");
    let quiet = enqueue(&t, &f, "quiet@example.net").await;
    let claimed = repos.queue.claim_due(10).await.unwrap();
    let attempt = claimed.iter().find(|row| row.id == quiet.get()).unwrap().attempts;
    assert!(repos.queue.finish_claim(quiet, attempt, "failed", None,
        Some("policy disabled"), None, None, None, false).await.unwrap());
    assert_eq!(repos.queue.find_by_id(quiet).await.unwrap().unwrap().bounce_status, "skipped");
    assert!(repos.queue.claim_due_bounces(10).await.unwrap().is_empty());
    // A row that already says failed without an explicit DSN request is not due.
    let legacy = enqueue(&t, &f, "legacy@example.net").await;
    sqlx::query("UPDATE mail_queue SET status = 'failed' WHERE id = $1")
        .bind(legacy.get()).execute(t.pool()).await.unwrap();
    assert_eq!(repos.queue.find_by_id(legacy).await.unwrap().unwrap().bounce_status, "none");
    assert!(repos.queue.claim_due_bounces(10).await.unwrap().is_empty());

    let id = enqueue(&t, &f, "stuck@example.net").await;
    let claimed = repos.queue.claim_due(1).await.unwrap().pop().unwrap();
    assert!(repos.queue.finish_claim(id, claimed.attempts, "failed", None,
        Some("failure"), None, None, None, true).await.unwrap());
    let first = repos.queue.claim_due_bounces(1).await.unwrap().pop().unwrap();
    assert_eq!(repos.queue.recover_stale_bounces(Utc::now() - chrono::Duration::hours(1)).await.unwrap(), 0);
    sqlx::query("UPDATE mail_queue SET bounce_claimed_at = NOW() - INTERVAL '2 hours' WHERE id = $1")
        .bind(id.get()).execute(t.pool()).await.unwrap();
    assert_eq!(repos.queue.recover_stale_bounces(Utc::now() - chrono::Duration::minutes(30)).await.unwrap(), 1);
    let next = repos.queue.claim_due_bounces(1).await.unwrap().pop().unwrap();
    assert_eq!(next.bounce_attempts, first.bounce_attempts + 1);
    assert!(!repos.queue.finish_bounce(id, first.bounce_attempts, true, None, None).await.unwrap());
    assert!(repos.queue.finish_bounce(id, next.bounce_attempts, true, None, None).await.unwrap());
    t.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounce_concurrent_claims_are_disjoint() {
    let t = common::fresh_database_with_pool(16).await;
    let f = fixture(&t).await;
    let repos = t.repos();
    for index in 0..24 {
        enqueue(&t, &f, &format!("dsn{index}@example.net")).await;
    }
    let delivery = repos.queue.claim_due(24).await.unwrap();
    for row in delivery {
        assert!(repos.queue.finish_claim(row.queue_id(), row.attempts, "failed", None,
            Some("permanent"), None, None, None, true).await.unwrap());
    }
    let mut handles = Vec::new();
    for _ in 0..4 {
        let repos = repos.clone();
        handles.push(tokio::spawn(async move {
            repos.queue.claim_due_bounces(24).await.expect("claim DSN")
        }));
    }
    let mut ids = Vec::new();
    for handle in handles {
        ids.extend(handle.await.unwrap().into_iter().map(|row| row.id));
    }
    assert_eq!(ids.len(), 24);
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 24, "no concurrent worker may claim the same DSN");
    t.cleanup().await;
}

#[tokio::test]
async fn queue_stats_lists_and_counts() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let empty = repos.queue.stats().await.unwrap();
    assert_eq!(empty.pending, 0);
    assert_eq!(empty.outstanding(), 0);
    assert!(repos.queue.next_due_at().await.unwrap().is_none());

    enqueue(&t, &f, "a@example.net").await;
    enqueue(&t, &f, "b@example.net").await;
    let retried = enqueue(&t, &f, "c@example.net").await;
    let failed = enqueue(&t, &f, "d@example.net").await;
    let cancelled = enqueue(&t, &f, "e@example.net").await;
    let delivered = enqueue(&t, &f, "f@example.net").await;

    repos.queue.claim_due(2).await.unwrap(); // a, b -> delivering
    repos.queue.claim_due(10).await.unwrap(); // c, d, e, f -> delivering
    repos
        .queue
        .mark_retry(retried, at(3600), "later", None, None, None)
        .await
        .unwrap();
    repos.queue.mark_failed(failed, "bounce", None, None, None).await.unwrap();
    repos.queue.mark_retry(cancelled, at(3600), "later", None, None, None)
        .await.unwrap();
    assert!(repos.queue.cancel(cancelled).await.unwrap());
    repos
        .queue
        .mark_delivered(delivered, None, Some(250), None)
        .await
        .unwrap();

    let stats = repos.queue.stats().await.unwrap();
    assert_eq!(
        stats,
        ferroma_storage::repository::QueueStats {
            pending: 0,
            delivering: 2,
            retry: 1,
            delivered: 1,
            failed: 1,
            cancelled: 1,
        }
    );
    assert_eq!(stats.outstanding(), 3);

    assert_eq!(repos.queue.count_by_status("delivering").await.unwrap(), 2);
    assert_eq!(repos.queue.count_by_status("retry").await.unwrap(), 1);
    assert_eq!(repos.queue.count_by_status("failed").await.unwrap(), 1);
    assert_eq!(repos.queue.count_by_status("nonsense").await.unwrap(), 0);

    assert_eq!(repos.queue.list_by_message(f.message_id).await.unwrap().len(), 6);
    let failed_only = repos.queue.list_by_status("failed", 10, 0).await.unwrap();
    assert_eq!(failed_only.len(), 1);
    assert_eq!(failed_only[0].id, failed.get());
    assert_eq!(repos.queue.list_by_status("failed", 10, 10).await.unwrap().len(), 0);

    // The retry is scheduled an hour out; the delivered and failed rows are not.
    let next = repos.queue.next_due_at().await.unwrap();
    assert_eq!(next, Some(at(3600)));

    t.cleanup().await;
}

#[tokio::test]
async fn queue_count_sent_since_powers_the_daily_limit() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let first = enqueue(&t, &f, "a@example.net").await;
    enqueue(&t, &f, "b@example.net").await;
    enqueue(&t, &f, "c@example.net").await;

    assert_eq!(repos.queue.count_sent_since(f.user_id, at(-10)).await.unwrap(), 3);
    assert_eq!(repos.queue.count_sent_since(f.user_id, Utc::now()).await.unwrap(), 0);
    assert_eq!(
        repos.queue.count_sent_since(UserId::new(9999), at(-10)).await.unwrap(),
        0
    );

    // Age one entry out of the window.
    sqlx::query("UPDATE mail_queue SET created_at = $2 WHERE id = $1")
        .bind(first.get())
        .bind(Utc::now() - chrono::Duration::days(2))
        .execute(t.pool())
        .await
        .unwrap();
    assert_eq!(
        repos
            .queue
            .count_sent_since(f.user_id, Utc::now() - chrono::Duration::days(1))
            .await
            .unwrap(),
        2
    );

    t.cleanup().await;
}

#[tokio::test]
async fn delivery_attempts_record_list_and_delete() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let queue_id = enqueue(&t, &f, "bob@example.net").await;
    assert!(repos.delivery_attempts.list_by_queue(queue_id).await.unwrap().is_empty());

    let first = repos
        .delivery_attempts
        .record(ferroma_storage::repository::NewDeliveryAttempt {
            queue_id,
            attempt: 1,
            remote_mx: Some("mx1.example.net".into()),
            status_code: Some(451),
            status_text: Some("try later".into()),
            error: None,
            duration_ms: Some(120),
        })
        .await
        .unwrap();
    assert_eq!(first.queue_id, queue_id.get());
    assert_eq!(first.attempt, 1);
    assert_eq!(first.duration_ms, Some(120));

    repos
        .delivery_attempts
        .record(ferroma_storage::repository::NewDeliveryAttempt {
            queue_id,
            attempt: 2,
            remote_mx: Some("mx1.example.net".into()),
            status_code: None,
            status_text: None,
            error: Some("connection reset".into()),
            duration_ms: None,
        })
        .await
        .unwrap();

    let history = repos.delivery_attempts.list_by_queue(queue_id).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].attempt, 1);
    assert_eq!(history[1].error.as_deref(), Some("connection reset"));

    assert_eq!(repos.delivery_attempts.delete_for_queue(queue_id).await.unwrap(), 2);
    assert_eq!(repos.delivery_attempts.delete_for_queue(queue_id).await.unwrap(), 0);
    assert!(repos.delivery_attempts.list_by_queue(queue_id).await.unwrap().is_empty());

    // Deleting the queue entry cascades the history away.
    repos
        .delivery_attempts
        .record(ferroma_storage::repository::NewDeliveryAttempt {
            queue_id,
            attempt: 1,
            remote_mx: None,
            status_code: None,
            status_text: None,
            error: None,
            duration_ms: None,
        })
        .await
        .unwrap();
    assert_eq!(t.count("delivery_attempts").await, 1);
    sqlx::query("DELETE FROM mail_queue WHERE id = $1")
        .bind(queue_id.get())
        .execute(t.pool())
        .await
        .unwrap();
    assert_eq!(t.count("delivery_attempts").await, 0);

    t.cleanup().await;
}

// ===========================================================================
// sessions
// ===========================================================================

#[tokio::test]
async fn session_create_and_lookups() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let id = session(&t, &f, "hash-web-1", 3600).await;
    let created = repos.sessions.find_by_id(id).await.unwrap().expect("session");
    assert_eq!(created.user_id, f.user_id.get());
    assert_eq!(created.kind, "web");
    assert_eq!(created.token_hash, "hash-web-1");
    assert_eq!(created.ip.as_deref(), Some("203.0.113.7"));
    assert!(created.is_valid_at(Utc::now()));
    assert!(created.device_id.is_none());

    let by_token = repos
        .sessions
        .find_by_token_hash("hash-web-1")
        .await
        .unwrap()
        .expect("by token hash");
    assert_eq!(by_token.id, created.id);

    assert!(repos
        .sessions
        .find_by_token_hash("no-such-token")
        .await
        .unwrap()
        .is_none());
    assert!(repos.sessions.find_by_id(SessionId::new(9999)).await.unwrap().is_none());

    // The same token hash twice is a conflict, not a second session.
    let err = repos
        .sessions
        .create(NewSession {
            user_id: f.user_id,
            kind: "api".into(),
            token_hash: "hash-web-1".into(),
            device_id: None,
            ip: None,
            user_agent: None,
            expires_at: Utc::now() + chrono::Duration::hours(1),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn session_touch_revokes_and_counts() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let id = session(&t, &f, "hash-1", 3600).await;
    let before = repos.sessions.find_by_id(id).await.unwrap().unwrap();

    repos.sessions.touch(id, Some("198.51.100.4")).await.unwrap();
    let after = repos.sessions.find_by_id(id).await.unwrap().unwrap();
    assert!(after.last_seen_at >= before.last_seen_at);
    assert_eq!(after.ip.as_deref(), Some("198.51.100.4"));

    // `None` keeps the recorded address.
    repos.sessions.touch(id, None).await.unwrap();
    assert_eq!(
        repos.sessions.find_by_id(id).await.unwrap().unwrap().ip.as_deref(),
        Some("198.51.100.4")
    );

    let err = repos.sessions.touch(SessionId::new(9999), None).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    assert_eq!(repos.sessions.count_active().await.unwrap(), 1);
    assert!(repos.sessions.revoke(id).await.unwrap());
    assert!(!repos.sessions.revoke(id).await.unwrap());
    assert_eq!(repos.sessions.count_active().await.unwrap(), 0);
    assert!(!repos
        .sessions
        .find_by_id(id)
        .await
        .unwrap()
        .unwrap()
        .is_valid_at(Utc::now()));

    t.cleanup().await;
}

#[tokio::test]
async fn session_revoke_all_for_user_and_for_device() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let device_id = device(&t, &f, "dev-1").await;
    let other = device(&t, &f, "dev-2").await;

    let first = session(&t, &f, "hash-1", 3600).await;
    let second = repos
        .sessions
        .create(NewSession {
            user_id: f.user_id,
            kind: "client".into(),
            token_hash: "hash-2".into(),
            device_id: Some(device_id),
            ip: None,
            user_agent: None,
            expires_at: Utc::now() + chrono::Duration::hours(1),
        })
        .await
        .unwrap()
        .session_id();
    let third = repos
        .sessions
        .create(NewSession {
            user_id: f.user_id,
            kind: "client".into(),
            token_hash: "hash-3".into(),
            device_id: Some(other),
            ip: None,
            user_agent: None,
            expires_at: Utc::now() + chrono::Duration::hours(1),
        })
        .await
        .unwrap()
        .session_id();

    assert_eq!(repos.sessions.revoke_for_device(device_id).await.unwrap(), 1);
    assert!(!repos.sessions.find_by_id(second).await.unwrap().unwrap().is_valid_at(Utc::now()));
    assert!(repos.sessions.find_by_id(third).await.unwrap().unwrap().is_valid_at(Utc::now()));

    assert_eq!(repos.sessions.revoke_for_device(device_id).await.unwrap(), 0);
    assert_eq!(repos.sessions.revoke_all_for_user(f.user_id).await.unwrap(), 2);
    assert_eq!(repos.sessions.revoke_all_for_user(f.user_id).await.unwrap(), 0);
    assert_eq!(repos.sessions.count_active().await.unwrap(), 0);
    assert!(repos.sessions.find_by_id(first).await.unwrap().unwrap().revoked_at.is_some());

    t.cleanup().await;
}

#[tokio::test]
async fn session_list_for_user_hides_revoked_by_default() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let live = session(&t, &f, "hash-live", 3600).await;
    let dead = session(&t, &f, "hash-dead", 3600).await;
    repos.sessions.revoke(dead).await.unwrap();

    let visible = repos.sessions.list_for_user(f.user_id, false).await.unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, live.get());

    let all = repos.sessions.list_for_user(f.user_id, true).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, dead.get(), "newest first");

    assert!(repos
        .sessions
        .list_for_user(UserId::new(9999), true)
        .await
        .unwrap()
        .is_empty());

    t.cleanup().await;
}

#[tokio::test]
async fn session_delete_expired_cleans_up() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let expired = session(&t, &f, "hash-expired", -3600).await;
    let live = session(&t, &f, "hash-live", 3600).await;
    let revoked = session(&t, &f, "hash-revoked", 3600).await;
    repos.sessions.revoke(revoked).await.unwrap();

    assert_eq!(repos.sessions.delete_expired(Utc::now()).await.unwrap(), 2);
    assert!(repos.sessions.find_by_id(expired).await.unwrap().is_none());
    assert!(repos.sessions.find_by_id(revoked).await.unwrap().is_none());
    assert!(repos.sessions.find_by_id(live).await.unwrap().is_some());
    assert_eq!(repos.sessions.delete_expired(Utc::now()).await.unwrap(), 0);

    t.cleanup().await;
}

// ===========================================================================
// devices
// ===========================================================================

#[tokio::test]
async fn device_upsert_twice_yields_one_refreshed_row() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let first = repos
        .devices
        .upsert(DeviceUpsert {
            user_id: f.user_id,
            device_uid: "  installation-1  ".into(),
            name: Some("Old name".into()),
            platform: Some("windows".into()),
            client_version: Some("0.1.0".into()),
            protocol_version: Some(1),
            ip: Some("203.0.113.7".into()),
        })
        .await
        .unwrap();
    assert_eq!(first.device_uid, "installation-1");
    assert!(first.last_seen_at.is_some());
    assert!(first.is_active());

    let second = repos
        .devices
        .upsert(DeviceUpsert {
            user_id: f.user_id,
            device_uid: "installation-1".into(),
            name: Some("New name".into()),
            platform: None,
            client_version: Some("0.2.0".into()),
            protocol_version: None,
            ip: None,
        })
        .await
        .unwrap();

    assert_eq!(second.id, first.id, "one row per (user, device_uid)");
    assert_eq!(t.count("devices").await, 1);
    assert_eq!(second.name.as_deref(), Some("New name"));
    assert_eq!(second.client_version.as_deref(), Some("0.2.0"));
    // Fields the client did not resend are kept.
    assert_eq!(second.platform.as_deref(), Some("windows"));
    assert_eq!(second.protocol_version, Some(1));
    assert_eq!(second.last_ip.as_deref(), Some("203.0.113.7"));

    // A revoked device comes back when it registers again.
    let id = second.device_id();
    repos.devices.revoke(id).await.unwrap();
    assert!(!repos.devices.find_by_id(id).await.unwrap().unwrap().is_active());
    let revived = repos
        .devices
        .upsert(DeviceUpsert {
            user_id: f.user_id,
            device_uid: "installation-1".into(),
            name: None,
            platform: None,
            client_version: None,
            protocol_version: None,
            ip: None,
        })
        .await
        .unwrap();
    assert_eq!(revived.id, first.id);
    assert!(revived.revoked_at.is_none());
    assert_eq!(t.count("devices").await, 1);

    let err = repos
        .devices
        .upsert(DeviceUpsert {
            user_id: f.user_id,
            device_uid: "   ".into(),
            name: None,
            platform: None,
            client_version: None,
            protocol_version: None,
            ip: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Invalid(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn device_find_list_and_counts() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let first = device(&t, &f, "dev-1").await;
    let second = device(&t, &f, "dev-2").await;

    assert_eq!(
        repos.devices.find_by_id(first).await.unwrap().map(|d| d.id),
        Some(first.get())
    );
    assert!(repos.devices.find_by_id(DeviceId::new(9999)).await.unwrap().is_none());
    assert_eq!(
        repos
            .devices
            .find_by_uid(f.user_id, "dev-2")
            .await
            .unwrap()
            .map(|d| d.id),
        Some(second.get())
    );
    assert!(repos
        .devices
        .find_by_uid(f.user_id, "dev-3")
        .await
        .unwrap()
        .is_none());
    assert!(repos
        .devices
        .find_by_uid(UserId::new(9999), "dev-1")
        .await
        .unwrap()
        .is_none());

    assert_eq!(repos.devices.list_for_user(f.user_id, false).await.unwrap().len(), 2);
    assert_eq!(repos.devices.count_active_for_user(f.user_id).await.unwrap(), 2);

    repos.devices.revoke(second).await.unwrap();
    assert_eq!(repos.devices.list_for_user(f.user_id, false).await.unwrap().len(), 1);
    assert_eq!(repos.devices.list_for_user(f.user_id, true).await.unwrap().len(), 2);
    assert_eq!(repos.devices.count_active_for_user(f.user_id).await.unwrap(), 1);
    assert_eq!(repos.devices.list_active().await.unwrap().len(), 1);

    assert!(!repos.devices.revoke(second).await.unwrap());
    assert!(!repos.devices.revoke(DeviceId::new(9999)).await.unwrap());

    t.cleanup().await;
}

#[tokio::test]
async fn device_touch_and_delete() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let id = device(&t, &f, "dev-1").await;
    repos.devices.touch(id, Some("198.51.100.9")).await.unwrap();
    let touched = repos.devices.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(touched.last_ip.as_deref(), Some("198.51.100.9"));
    assert!(touched.last_seen_at.is_some());

    let err = repos.devices.touch(DeviceId::new(9999), None).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    assert!(repos.devices.delete(id).await.unwrap());
    assert!(!repos.devices.delete(id).await.unwrap());
    assert_eq!(t.count("devices").await, 0);

    t.cleanup().await;
}

// ===========================================================================
// sync states
// ===========================================================================

#[tokio::test]
async fn sync_state_get_defaults_to_zero_and_set_upserts() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let device_id = device(&t, &f, "dev-1").await;

    assert_eq!(
        repos.sync_states.get(device_id, f.mailbox_id, None).await.unwrap(),
        0,
        "a device that never synced is at cursor 0"
    );

    repos.sync_states.set(device_id, f.mailbox_id, None, 42).await.unwrap();
    assert_eq!(repos.sync_states.get(device_id, f.mailbox_id, None).await.unwrap(), 42);

    // The same key updates in place instead of inserting a second row.
    repos.sync_states.set(device_id, f.mailbox_id, None, 43).await.unwrap();
    assert_eq!(repos.sync_states.get(device_id, f.mailbox_id, None).await.unwrap(), 43);
    assert_eq!(t.count("client_sync_states").await, 1);

    t.cleanup().await;
}

#[tokio::test]
async fn sync_state_account_and_folder_rows_are_distinct() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let device_id = device(&t, &f, "dev-1").await;
    let others_device = device(&t, &f, "dev-2").await;

    repos.sync_states.set(device_id, f.mailbox_id, None, 10).await.unwrap();
    repos
        .sync_states
        .set(device_id, f.mailbox_id, Some(f.inbox_id), 20)
        .await
        .unwrap();
    repos
        .sync_states
        .set(others_device, f.mailbox_id, None, 30)
        .await
        .unwrap();

    assert_eq!(repos.sync_states.get(device_id, f.mailbox_id, None).await.unwrap(), 10);
    assert_eq!(
        repos
            .sync_states
            .get(device_id, f.mailbox_id, Some(f.inbox_id))
            .await
            .unwrap(),
        20
    );
    assert_eq!(
        repos.sync_states.get(others_device, f.mailbox_id, None).await.unwrap(),
        30
    );
    assert_eq!(t.count("client_sync_states").await, 3);

    // A second folder is another row again.
    let sent = repos.folders.require_by_name(f.mailbox_id, "Sent").await.unwrap();
    repos
        .sync_states
        .set(device_id, f.mailbox_id, Some(sent.folder_id()), 21)
        .await
        .unwrap();
    assert_eq!(t.count("client_sync_states").await, 4);
    assert_eq!(repos.sync_states.get(device_id, f.mailbox_id, None).await.unwrap(), 10);

    t.cleanup().await;
}

#[tokio::test]
async fn sync_state_list_reset_and_delete_for_folder() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let device_id = device(&t, &f, "dev-1").await;
    let other_device = device(&t, &f, "dev-2").await;
    let sent = repos.folders.require_by_name(f.mailbox_id, "Sent").await.unwrap();

    repos.sync_states.set(device_id, f.mailbox_id, None, 10).await.unwrap();
    repos
        .sync_states
        .set(device_id, f.mailbox_id, Some(f.inbox_id), 20)
        .await
        .unwrap();
    repos
        .sync_states
        .set(other_device, f.mailbox_id, Some(f.inbox_id), 5)
        .await
        .unwrap();

    let listed = repos.sync_states.list_for_device(device_id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].folder_id.is_none(), "account level first");
    assert_eq!(listed[0].cursor_value(), Cursor(10));
    assert_eq!(listed[1].folder_id, Some(f.inbox_id.get()));

    // Dropping a folder drops every cursor pointing at it, for every device.
    assert_eq!(repos.sync_states.delete_for_folder(f.inbox_id).await.unwrap(), 2);
    assert_eq!(
        repos.sync_states.get(device_id, f.mailbox_id, Some(f.inbox_id)).await.unwrap(),
        0
    );
    assert_eq!(t.count("client_sync_states").await, 1);

    // `sent` is unused; make sure it is a real folder id.
    assert_eq!(sent.name, "Sent");
    assert_eq!(repos.sync_states.reset_for_device(device_id).await.unwrap(), 1);
    assert_eq!(repos.sync_states.reset_for_device(device_id).await.unwrap(), 0);
    assert_eq!(repos.sync_states.get(device_id, f.mailbox_id, None).await.unwrap(), 0);
    assert!(repos.sync_states.list_for_device(device_id).await.unwrap().is_empty());

    t.cleanup().await;
}

// ===========================================================================
// operations (idempotency)
// ===========================================================================

#[tokio::test]
async fn operations_begin_is_fresh_then_replays_the_cached_result() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let outcome = repos
        .operations
        .begin("op_1", Some(f.user_id), "message.send")
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ferroma_storage::repository::OperationOutcome::Fresh
    ));

    repos
        .operations
        .complete("op_1", serde_json::json!({ "message_id": 42 }))
        .await
        .unwrap();

    let replay = repos
        .operations
        .begin("op_1", Some(f.user_id), "message.send")
        .await
        .unwrap();
    match replay {
        ferroma_storage::repository::OperationOutcome::Replay(operation) => {
            assert_eq!(operation.operation_id, "op_1");
            assert_eq!(operation.kind, "message.send");
            assert_eq!(operation.status, "applied");
            assert_eq!(
                operation.result,
                Some(serde_json::json!({ "message_id": 42 }))
            );
            assert!(operation.completed_at.is_some());
        }
        other => panic!("expected a replay, got {other:?}"),
    }

    // A *different* id is fresh again.
    assert!(matches!(
        repos.operations.begin("op_2", None, "message.send").await.unwrap(),
        ferroma_storage::repository::OperationOutcome::Fresh
    ));

    let err = repos.operations.begin("   ", None, "x").await.unwrap_err();
    assert!(matches!(err, StorageError::Invalid(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn operations_fail_records_the_error_and_find_returns_it() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos.operations.begin("op_bad", Some(f.user_id), "folder.create").await.unwrap();
    repos
        .operations
        .fail("op_bad", serde_json::json!({ "error": "name taken" }))
        .await
        .unwrap();

    let found = repos.operations.find("op_bad").await.unwrap().expect("recorded");
    assert_eq!(found.status, "failed");
    assert_eq!(found.result, Some(serde_json::json!({ "error": "name taken" })));
    assert!(repos.operations.find("op_missing").await.unwrap().is_none());

    let err = repos
        .operations
        .complete("op_missing", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");
    let err = repos
        .operations
        .fail("op_missing", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn operations_purge_older_than_drops_dead_keys() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos.operations.begin("op_old", Some(f.user_id), "x").await.unwrap();
    repos.operations.begin("op_new", Some(f.user_id), "x").await.unwrap();
    sqlx::query("UPDATE operations SET created_at = $2 WHERE operation_id = $1")
        .bind("op_old")
        .bind(Utc::now() - chrono::Duration::days(3))
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(
        repos
            .operations
            .purge_older_than(Utc::now() - chrono::Duration::days(1))
            .await
            .unwrap(),
        1
    );
    assert!(repos.operations.find("op_old").await.unwrap().is_none());
    assert!(repos.operations.find("op_new").await.unwrap().is_some());

    t.cleanup().await;
}

// ===========================================================================
// change log
// ===========================================================================

#[tokio::test]
async fn change_log_changes_since_is_exclusive_ordered_and_limited() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let mut seqs = Vec::new();
    for index in 0..5 {
        let entry = repos
            .change_log
            .append(NewChange {
                user_id: f.user_id,
                mailbox_id: Some(f.mailbox_id),
                folder_id: Some(f.inbox_id),
                message_id: Some(f.message_id),
                kind: "message_created".into(),
                payload: serde_json::json!({ "index": index }),
            })
            .await
            .unwrap();
        seqs.push(entry.seq);
    }

    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "seq increases");

    let all = repos
        .change_log
        .changes_since(f.user_id, Cursor::ZERO, 100)
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
    assert_eq!(all[0].seq, seqs[0]);
    assert_eq!(all[4].kind, "message_created");
    assert_eq!(all[0].payload, serde_json::json!({ "index": 0 }));
    assert_eq!(all[0].cursor(), Cursor(seqs[0]));
    assert_eq!(all[0].message_id, Some(f.message_id.get()));

    // An exclusive cursor never repeats the entry it points at.
    let after = repos
        .change_log
        .changes_since(f.user_id, Cursor(seqs[1]), 100)
        .await
        .unwrap();
    assert_eq!(
        after.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        vec![seqs[2], seqs[3], seqs[4]]
    );

    let limited = repos
        .change_log
        .changes_since(f.user_id, Cursor::ZERO, 2)
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
    // …and the client resumes from the last one it saw.
    let resumed = repos
        .change_log
        .changes_since(f.user_id, limited[1].cursor(), 2)
        .await
        .unwrap();
    assert_eq!(resumed[0].seq, seqs[2]);

    assert_eq!(repos.change_log.max_seq(f.user_id).await.unwrap(), seqs[4]);
    assert_eq!(repos.change_log.max_seq(UserId::new(9999)).await.unwrap(), 0);

    t.cleanup().await;
}

#[tokio::test]
async fn concurrent_change_transactions_commit_in_cursor_order_for_one_user() {
    if !common::database_available().await { return; }
    let t = common::fresh_database_with_pool(4).await;
    let f = fixture(&t).await;
    let triggers: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pg_trigger WHERE tgrelid = 'change_log'::regclass
            AND tgname = 'change_log_commit_order'",
    ).fetch_one(t.pool()).await.unwrap();
    assert_eq!(triggers.0, 1, "cursor trigger must be migrated");
    let mut first = t.pool().begin().await.unwrap();
    let first_seq: i64 = sqlx::query_scalar(
        "INSERT INTO change_log (user_id, mailbox_id, kind, payload)
         VALUES ($1, $2, 'message_updated', '{}'::jsonb) RETURNING seq",
    ).bind(f.user_id.get()).bind(f.mailbox_id.get())
        .fetch_one(&mut *first).await.unwrap();
    let pool = t.pool().clone();
    let user = f.user_id.get();
    let mailbox = f.mailbox_id.get();
    let second = tokio::spawn(async move {
        let mut tx = pool.begin().await.unwrap();
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO change_log (user_id, mailbox_id, kind, payload)
             VALUES ($1, $2, 'message_updated', '{}'::jsonb) RETURNING seq",
        ).bind(user).bind(mailbox).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        seq
    });
    // The second writer must wait for the first transaction to finish, even if
    // its sequence default was already evaluated. It cannot commit ahead of it.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!second.is_finished(), "a later cursor cannot commit first");
    assert!(t.repos().change_log.changes_since(f.user_id, Cursor::ZERO, 10).await.unwrap().is_empty());
    first.commit().await.unwrap();
    let second_seq = second.await.unwrap();
    assert!(second_seq > first_seq);
    let rows = t.repos().change_log.changes_since(f.user_id, Cursor::ZERO, 10).await.unwrap();
    assert_eq!(rows.iter().map(|row| row.seq).collect::<Vec<_>>(), vec![first_seq, second_seq]);
    t.cleanup().await;
}

#[tokio::test]
async fn change_log_filters_by_mailbox_and_owner() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let other_user = repos
        .users
        .create(NewUser {
            email: "bob@example.com".into(),
            password_hash: "hash".into(),
            display_name: None,
            is_admin: false,
            enabled: true,
            quota_bytes: None,
        })
        .await
        .unwrap();

    repos
        .change_log
        .append(NewChange {
            user_id: f.user_id,
            mailbox_id: Some(f.mailbox_id),
            folder_id: Some(f.inbox_id),
            message_id: None,
            kind: "message_updated".into(),
            payload: serde_json::json!({}),
        })
        .await
        .unwrap();
    repos
        .change_log
        .append(NewChange {
            user_id: f.user_id,
            mailbox_id: None,
            folder_id: None,
            message_id: None,
            kind: "settings_updated".into(),
            payload: serde_json::json!({}),
        })
        .await
        .unwrap();
    repos
        .change_log
        .append(NewChange {
            user_id: other_user.user_id(),
            mailbox_id: None,
            folder_id: None,
            message_id: None,
            kind: "message_created".into(),
            payload: serde_json::json!({}),
        })
        .await
        .unwrap();

    let in_mailbox = repos
        .change_log
        .changes_since_in_mailbox(f.user_id, f.mailbox_id, Cursor::ZERO, 100)
        .await
        .unwrap();
    assert_eq!(in_mailbox.len(), 1);
    assert_eq!(in_mailbox[0].kind, "message_updated");

    assert_eq!(
        repos
            .change_log
            .changes_since(f.user_id, Cursor::ZERO, 100)
            .await
            .unwrap()
            .len(),
        2,
        "another account's changes are invisible"
    );
    assert_eq!(
        repos
            .change_log
            .changes_since_in_mailbox(f.user_id, MailboxId::new(9999), Cursor::ZERO, 100)
            .await
            .unwrap()
            .len(),
        0
    );

    t.cleanup().await;
}

#[tokio::test]
async fn change_log_prunes_by_age() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    for kind in ["message_created", "message_deleted"] {
        repos
            .change_log
            .append(NewChange {
                user_id: f.user_id,
                mailbox_id: None,
                folder_id: None,
                message_id: None,
                kind: kind.into(),
                payload: serde_json::json!({}),
            })
            .await
            .unwrap();
    }
    sqlx::query("UPDATE change_log SET created_at = $1 WHERE kind = 'message_created'")
        .bind(Utc::now() - chrono::Duration::days(30))
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(
        repos
            .change_log
            .prune_older_than(Utc::now() - chrono::Duration::days(7))
            .await
            .unwrap(),
        1
    );
    assert_eq!(t.count("change_log").await, 1);

    t.cleanup().await;
}

// ===========================================================================
// drafts
// ===========================================================================

#[tokio::test]
async fn drafts_create_find_and_list() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let draft = repos
        .drafts
        .create(NewDraft {
            user_id: f.user_id,
            mailbox_id: Some(f.mailbox_id),
            folder_id: Some(f.inbox_id),
            subject: Some("Half written".into()),
            body_text: Some("hello".into()),
            body_html: None,
            recipients: serde_json::json!([{ "address": "bob@example.net", "name": "Bob" }]),
            attachments: serde_json::json!([]),
            in_reply_to: Some("<parent@example.net>".into()),
            reference_ids: serde_json::json!(["<parent@example.net>"]),
        })
        .await
        .unwrap();

    assert_eq!(draft.subject.as_deref(), Some("Half written"));
    assert_eq!(draft.recipients[0]["address"], "bob@example.net");
    assert_eq!(draft.attachments, serde_json::json!([]));
    assert_eq!(draft.reference_ids[0], "<parent@example.net>");
    assert!(draft.message_id.is_none());

    let found = repos.drafts.find_by_id(draft.draft_id()).await.unwrap().unwrap();
    assert_eq!(found.id, draft.id);
    assert_eq!(found.mailbox_id, Some(f.mailbox_id.get()));
    assert!(repos
        .drafts
        .find_by_id(ferroma_core::DraftId::new(9999))
        .await
        .unwrap()
        .is_none());

    repos
        .drafts
        .create(NewDraft {
            user_id: f.user_id,
            mailbox_id: None,
            folder_id: None,
            subject: Some("Second".into()),
            body_text: None,
            body_html: None,
            recipients: serde_json::json!([]),
            attachments: serde_json::json!([]),
            in_reply_to: None,
            reference_ids: serde_json::json!([]),
        })
        .await
        .unwrap();

    assert_eq!(repos.drafts.count_for_user(f.user_id).await.unwrap(), 2);
    assert_eq!(repos.drafts.list_for_user(f.user_id, 10, 0).await.unwrap().len(), 2);
    assert_eq!(repos.drafts.list_for_user(f.user_id, 1, 1).await.unwrap().len(), 1);
    assert!(repos
        .drafts
        .list_for_user(UserId::new(9999), 10, 0)
        .await
        .unwrap()
        .is_empty());

    t.cleanup().await;
}

#[tokio::test]
async fn drafts_partial_update_leaves_untouched_fields_alone() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let draft = repos
        .drafts
        .create(NewDraft {
            user_id: f.user_id,
            mailbox_id: Some(f.mailbox_id),
            folder_id: Some(f.inbox_id),
            subject: Some("Original".into()),
            body_text: Some("first".into()),
            body_html: Some("<p>first</p>".into()),
            recipients: serde_json::json!([{ "address": "bob@example.net" }]),
            attachments: serde_json::json!([]),
            in_reply_to: Some("<parent@example.net>".into()),
            reference_ids: serde_json::json!([]),
        })
        .await
        .unwrap();

    let updated = repos
        .drafts
        .update(
            draft.draft_id(),
            DraftUpdate {
                body_text: Some("second".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.body_text.as_deref(), Some("second"));
    assert_eq!(updated.subject.as_deref(), Some("Original"), "subject untouched");
    assert_eq!(updated.body_html.as_deref(), Some("<p>first</p>"));
    assert_eq!(updated.recipients[0]["address"], "bob@example.net");
    assert_eq!(updated.in_reply_to.as_deref(), Some("<parent@example.net>"));
    assert_eq!(updated.mailbox_id, Some(f.mailbox_id.get()));
    assert!(updated.updated_at >= draft.updated_at);

    // Every field at once, including the message link.
    let message_id = f.message_id;
    let full = repos
        .drafts
        .update(
            draft.draft_id(),
            DraftUpdate {
                subject: Some("Rewritten".into()),
                body_text: Some("third".into()),
                body_html: None,
                recipients: Some(serde_json::json!([{ "address": "carol@example.net" }])),
                attachments: Some(serde_json::json!([{ "filename": "a.txt" }])),
                in_reply_to: Some("<other@example.net>".into()),
                reference_ids: Some(serde_json::json!(["<other@example.net>"])),
                message_id: Some(message_id),
            },
        )
        .await
        .unwrap();

    assert_eq!(full.subject.as_deref(), Some("Rewritten"));
    assert_eq!(full.body_text.as_deref(), Some("third"));
    // `None` in the update means "leave it", so the HTML body survives.
    assert_eq!(full.body_html.as_deref(), Some("<p>first</p>"));
    assert_eq!(full.recipients[0]["address"], "carol@example.net");
    assert_eq!(full.attachments[0]["filename"], "a.txt");
    assert_eq!(full.reference_ids[0], "<other@example.net>");
    assert_eq!(full.message_id, Some(message_id.get()));

    // An update with nothing set still succeeds and touches only `updated_at`.
    let untouched = repos
        .drafts
        .update(draft.draft_id(), DraftUpdate::default())
        .await
        .unwrap();
    assert_eq!(untouched.subject.as_deref(), Some("Rewritten"));

    let err = repos
        .drafts
        .update(ferroma_core::DraftId::new(9999), DraftUpdate::default())
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn drafts_delete_reports() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let draft = repos
        .drafts
        .create(NewDraft {
            user_id: f.user_id,
            mailbox_id: None,
            folder_id: None,
            subject: None,
            body_text: None,
            body_html: None,
            recipients: serde_json::json!([]),
            attachments: serde_json::json!([]),
            in_reply_to: None,
            reference_ids: serde_json::json!([]),
        })
        .await
        .unwrap();

    assert!(repos.drafts.delete(draft.draft_id()).await.unwrap());
    assert!(!repos.drafts.delete(draft.draft_id()).await.unwrap());
    assert_eq!(repos.drafts.count_for_user(f.user_id).await.unwrap(), 0);
    assert_eq!(t.count("drafts").await, 0);

    t.cleanup().await;
}

// ===========================================================================
// login attempts
// ===========================================================================

#[tokio::test]
async fn login_attempts_count_failures_per_email_and_ip() {
    let t = setup!();
    let repos = t.repos();
    let since = Utc::now() - chrono::Duration::hours(1);

    repos
        .login_attempts
        .record("Alice@Example.com", Some("203.0.113.7"), "password", false)
        .await
        .unwrap();
    repos
        .login_attempts
        .record("alice@example.com", Some("203.0.113.7"), "password", false)
        .await
        .unwrap();
    repos
        .login_attempts
        .record("alice@example.com", Some("198.51.100.4"), "password", true)
        .await
        .unwrap();
    repos
        .login_attempts
        .record("bob@example.com", Some("203.0.113.7"), "password", false)
        .await
        .unwrap();

    // Case does not open a second bucket.
    assert_eq!(
        repos
            .login_attempts
            .count_failures_for_email("ALICE@example.com", since)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        repos
            .login_attempts
            .count_failures_for_ip("203.0.113.7", since)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        repos
            .login_attempts
            .count_failures_for_ip("198.51.100.4", since)
            .await
            .unwrap(),
        0,
        "a success is not a failure"
    );
    assert_eq!(
        repos
            .login_attempts
            .count_failures_for_email(
                "alice@example.com",
                Utc::now() + chrono::Duration::hours(1)
            )
            .await
            .unwrap(),
        0,
        "a window that starts in the future excludes everything"
    );

    t.cleanup().await;
}

#[tokio::test]
async fn login_attempts_recent_and_prune() {
    let t = setup!();
    let repos = t.repos();

    let recorded = repos
        .login_attempts
        .record("alice@example.com", Some("203.0.113.7"), "password", false)
        .await
        .unwrap();
    assert_eq!(recorded.email, "alice@example.com");
    assert!(!recorded.success);
    assert_eq!(recorded.kind, "password");

    for _ in 0..3 {
        repos
            .login_attempts
            .record("alice@example.com", None, "token", true)
            .await
            .unwrap();
    }

    let recent = repos
        .login_attempts
        .recent_for_email("ALICE@example.com", 2)
        .await
        .unwrap();
    assert_eq!(recent.len(), 2);
    assert!(recent[0].id > recent[1].id, "newest first");

    sqlx::query("UPDATE login_attempts SET created_at = $1 WHERE success")
        .bind(Utc::now() - chrono::Duration::days(90))
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(
        repos
            .login_attempts
            .prune_older_than(Utc::now() - chrono::Duration::days(30))
            .await
            .unwrap(),
        3
    );
    assert_eq!(t.count("login_attempts").await, 1);

    t.cleanup().await;
}

// ===========================================================================
// settings
// ===========================================================================

#[tokio::test]
async fn settings_get_set_all_and_delete() {
    let t = setup!();
    let repos = t.repos();

    assert!(repos.settings.get("smtp.banner").await.unwrap().is_none());

    repos
        .settings
        .set("smtp.banner", serde_json::json!("Ferroma ESMTP"))
        .await
        .unwrap();
    repos
        .settings
        .set("limits.max_message_bytes", serde_json::json!(25_000_000))
        .await
        .unwrap();

    assert_eq!(
        repos.settings.get("smtp.banner").await.unwrap(),
        Some(serde_json::json!("Ferroma ESMTP"))
    );

    // Overwriting keeps one row.
    repos
        .settings
        .set("smtp.banner", serde_json::json!("Ferroma"))
        .await
        .unwrap();
    assert_eq!(
        repos.settings.get("smtp.banner").await.unwrap(),
        Some(serde_json::json!("Ferroma"))
    );
    assert_eq!(t.count("settings").await, 2);

    let all = repos.settings.all().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].key, "limits.max_message_bytes", "alphabetical");
    assert_eq!(all[1].value, serde_json::json!("Ferroma"));

    let err = repos.settings.set("  ", serde_json::json!(1)).await.unwrap_err();
    assert!(matches!(err, StorageError::Invalid(_)), "{err:?}");

    assert!(repos.settings.delete("smtp.banner").await.unwrap());
    assert!(!repos.settings.delete("smtp.banner").await.unwrap());
    assert!(repos.settings.get("smtp.banner").await.unwrap().is_none());

    t.cleanup().await;
}

// ===========================================================================
// audit trail
// ===========================================================================

/// Append one audit entry.
async fn audit(t: &TestDatabase, f: &Fixture, action: &str, target: &str) -> i64 {
    t.repos()
        .audit
        .record(NewAuditLog {
            actor_user_id: Some(f.user_id),
            action: action.into(),
            target_type: Some(target.into()),
            target_id: Some("7".into()),
            ip: Some("203.0.113.7".into()),
            user_agent: Some("ferroma-test/0.1".into()),
            details: serde_json::json!({ "action": action }),
        })
        .await
        .expect("record audit")
        .id
}

#[tokio::test]
async fn audit_record_and_list_with_filters() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let created = audit(&t, &f, "user.created", "user").await;
    let deleted = audit(&t, &f, "user.deleted", "user").await;
    let domain = audit(&t, &f, "domain.created", "domain").await;

    let recorded = repos.audit.list(AuditFilter::new(10, 0)).await.unwrap();
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0].id, domain, "newest first");
    assert_eq!(recorded[0].target_type.as_deref(), Some("domain"));
    assert_eq!(recorded[0].details, serde_json::json!({ "action": "domain.created" }));
    assert_eq!(recorded[0].actor_user_id, Some(f.user_id.get()));
    assert_eq!(recorded[0].ip.as_deref(), Some("203.0.113.7"));

    let by_action = repos
        .audit
        .list(AuditFilter {
            action: Some("user.deleted".into()),
            ..AuditFilter::new(10, 0)
        })
        .await
        .unwrap();
    assert_eq!(by_action.len(), 1);
    assert_eq!(by_action[0].id, deleted);

    let by_target = repos
        .audit
        .list(AuditFilter {
            target_type: Some("user".into()),
            ..AuditFilter::new(10, 0)
        })
        .await
        .unwrap();
    assert_eq!(by_target.len(), 2);
    assert!(by_target.iter().any(|entry| entry.id == created));

    let by_actor = repos
        .audit
        .list(AuditFilter {
            actor_user_id: Some(f.user_id),
            ..AuditFilter::new(10, 0)
        })
        .await
        .unwrap();
    assert_eq!(by_actor.len(), 3);

    let other_actor = repos
        .audit
        .list(AuditFilter {
            actor_user_id: Some(UserId::new(9999)),
            ..AuditFilter::new(10, 0)
        })
        .await
        .unwrap();
    assert!(other_actor.is_empty());

    let future = repos
        .audit
        .list(AuditFilter {
            since: Some(Utc::now() + chrono::Duration::hours(1)),
            ..AuditFilter::new(10, 0)
        })
        .await
        .unwrap();
    assert!(future.is_empty(), "nothing happened in the future");

    let past = repos
        .audit
        .list(AuditFilter {
            since: Some(Utc::now() - chrono::Duration::hours(1)),
            ..AuditFilter::new(10, 0)
        })
        .await
        .unwrap();
    assert_eq!(past.len(), 3);

    t.cleanup().await;
}

#[tokio::test]
async fn audit_count_and_paging_agree_with_list() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    for index in 0..5 {
        audit(&t, &f, &format!("action.{index}"), "user").await;
    }

    let filter = AuditFilter::new(2, 0);
    assert_eq!(repos.audit.count(filter.clone()).await.unwrap(), 5);
    assert_eq!(repos.audit.list(filter).await.unwrap().len(), 2);
    assert_eq!(repos.audit.list(AuditFilter::new(2, 2)).await.unwrap().len(), 2);
    assert_eq!(repos.audit.list(AuditFilter::new(2, 4)).await.unwrap().len(), 1);
    assert_eq!(repos.audit.list(AuditFilter::new(2, 6)).await.unwrap().len(), 0);

    let filtered = AuditFilter {
        action: Some("action.0".into()),
        ..AuditFilter::new(10, 0)
    };
    assert_eq!(repos.audit.count(filtered.clone()).await.unwrap(), 1);
    assert_eq!(repos.audit.list(filtered).await.unwrap().len(), 1);

    // `count` ignores limit/offset.
    assert_eq!(
        repos
            .audit
            .count(AuditFilter::new(0, 100))
            .await
            .unwrap(),
        5
    );

    t.cleanup().await;
}

#[tokio::test]
async fn audit_prune_and_system_entries() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    // A system action has no actor.
    let system = repos
        .audit
        .record(NewAuditLog {
            actor_user_id: None,
            action: "server.started".into(),
            target_type: None,
            target_id: None,
            ip: None,
            user_agent: None,
            details: serde_json::json!({ "version": "0.1.0" }),
        })
        .await
        .unwrap();
    assert!(system.actor_user_id.is_none());
    assert_eq!(system.details["version"], "0.1.0");

    audit(&t, &f, "user.created", "user").await;
    sqlx::query("UPDATE audit_logs SET created_at = $1 WHERE action = 'user.created'")
        .bind(Utc::now() - chrono::Duration::days(400))
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(
        repos
            .audit
            .prune_older_than(Utc::now() - chrono::Duration::days(365))
            .await
            .unwrap(),
        1
    );
    assert_eq!(t.count("audit_logs").await, 1);

    t.cleanup().await;
}

// ===========================================================================
// cross-aggregate: a message row the queue points at
// ===========================================================================

#[tokio::test]
async fn queue_entries_disappear_with_their_message() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let first = enqueue(&t, &f, "a@example.net").await;
    let second = enqueue(&t, &f, "b@example.net").await;
    assert_eq!(t.count("mail_queue").await, 2);

    assert!(matches!(repos.messages.hard_delete(f.message_id).await, Err(StorageError::Conflict(_))));
    repos.queue.mark_delivered(first, None, None, None).await.unwrap();
    repos.queue.mark_failed(second, "permanent failure", None, None, None).await.unwrap();
    repos.messages.hard_delete(f.message_id).await.unwrap();
    assert_eq!(t.count("mail_queue").await, 0, "the queue cascades with the message");

    t.cleanup().await;
}
