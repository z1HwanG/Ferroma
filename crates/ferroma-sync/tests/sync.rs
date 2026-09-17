//! End-to-end synchronisation tests against a real PostgreSQL schema.
//!
//! The unit tests prove the wire shapes; these prove the behaviour the official
//! clients depend on: exclusive cursors, paging, stale-cursor recovery, and an
//! idempotency guard that really does run an operation exactly once.

use std::time::Duration;

use ferroma_core::{Cursor, FerromaError, MailboxId, MessageId, UserId};
use ferroma_storage::repository::{NewChange, NewMailbox, NewUser, OperationOutcome};
use ferroma_storage::Database;
use ferroma_sync::{ChangeKind, SyncRequest, SyncService};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::Executor;

const TEST_DATABASE: &str = "ferroma_test";

fn admin_url() -> String {
    std::env::var("FERROMA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferroma@127.0.0.1:5433/postgres".to_string())
}

/// Whether an integration database is reachable.
///
/// Panics rather than returning `false`: a skip the harness counts as a pass is a
/// hollow green — see `crates/ferroma-storage/tests/common/mod.rs`.
/// `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1` opts into skipping explicitly.
async fn database_available() -> bool {
    match PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3))
        .connect(&admin_url())
        .await
    {
        Ok(pool) => {
            pool.close().await;
            true
        }
        Err(err) => {
            if std::env::var_os("FERROMA_TEST_SKIP_WITHOUT_DATABASE").is_some() {
                return false;
            }
            panic!(
                "cannot reach the integration PostgreSQL at {}: {err}\n\
                 Start it with `scripts/dev-postgres.ps1 start`, or set \
                 FERROMA_TEST_SKIP_WITHOUT_DATABASE=1 to skip these tests explicitly.",
                admin_url()
            );
        }
    }
}

fn with_database(url: &str, db: &str) -> String {
    match url.rfind('/') {
        Some(idx) => {
            let (prefix, rest) = url.split_at(idx + 1);
            let suffix = rest.find('?').map(|q| &rest[q..]).unwrap_or("");
            format!("{prefix}{db}{suffix}")
        }
        None => format!("{url}/{db}"),
    }
}

/// A migrated schema, a `SyncService` over it, and a seeded user/mailbox/folder.
struct Harness {
    schema: String,
    db: Database,
    admin: sqlx::PgPool,
    service: SyncService,
    user_id: UserId,
    mailbox_id: MailboxId,
    folder_id: MailboxId,
    other_folder_id: MailboxId,
}

impl Harness {
    async fn new() -> Self {
        // One shared database, one schema per test. A database-per-test design does
        // not work on this host: `DROP DATABASE ... WITH (FORCE)` needs to signal
        // the checkpointer, and this sandbox forbids cross-process signalling.
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&admin_url())
            .await
            .expect("cannot connect to the test PostgreSQL");
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(TEST_DATABASE)
            .fetch_optional(&admin)
            .await
            .unwrap_or(None);
        if exists.is_none() {
            // Tolerant of losing the race: every test calls this at start-up, so on a
            // fresh cluster many see "no such database" at once and all issue `CREATE
            // DATABASE`; one wins and the rest get `42P04 duplicate_database`, which
            // is success, not failure.
            if let Err(err) = admin
                .execute(format!("CREATE DATABASE \"{TEST_DATABASE}\"").as_str())
                .await
            {
                let won_by_someone_else: Option<(i32,)> =
                    sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
                        .bind(TEST_DATABASE)
                        .fetch_optional(&admin)
                        .await
                        .unwrap_or(None);
                assert!(
                    won_by_someone_else.is_some(),
                    "cannot create the shared test database {TEST_DATABASE}: {err}"
                );
            }
        }

        let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
        let url = with_database(&admin_url(), TEST_DATABASE);
        let bootstrap = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        bootstrap
            .execute(format!("CREATE SCHEMA \"{schema}\"").as_str())
            .await
            .unwrap();
        bootstrap.close().await;

        let options: PgConnectOptions = url.parse().unwrap();
        let options = options.options([("search_path", schema.as_str())]);
        let db = Database::connect_with_options(
            options,
            // Small on purpose: every test opens its own pool and cargo runs test
            // binaries in parallel, so a large pool here exhausts PostgreSQL's
            // `max_connections` and produces unrelated timeouts under load.
            2,
            1,
            Duration::from_secs(10),
            Duration::from_secs(60),
            Duration::from_secs(300),
            false,
        )
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let repos = db.repositories();
        let user = repos
            .users
            .create(NewUser {
                email: "alice@example.com".into(),
                password_hash: "$argon2id$x".into(),
                display_name: None,
                is_admin: false,
                quota_bytes: None,
            })
            .await
            .unwrap();
        let domain = repos.domains.create("example.com", None).await.unwrap();
        let mailbox = repos
            .mailboxes
            .create(NewMailbox {
                user_id: UserId::new(user.id),
                domain_id: ferroma_core::DomainId::new(domain.id),
                local_part: "alice".into(),
                display_name: None,
                is_primary: true,
                quota_bytes: None,
            })
            .await
            .unwrap();
        let folders = repos
            .folders
            .ensure_standard(MailboxId::new(mailbox.id))
            .await
            .unwrap();
        let inbox = folders.iter().find(|f| f.is_inbox()).unwrap().id;
        let sent = folders
            .iter()
            .find(|f| f.special_use.as_deref() == Some("\\Sent"))
            .unwrap()
            .id;

        Harness {
            schema,
            service: SyncService::new(db.repositories(), 500, 30),
            db,
            admin,
            user_id: UserId::new(user.id),
            mailbox_id: MailboxId::new(mailbox.id),
            folder_id: MailboxId::new(inbox),
            other_folder_id: MailboxId::new(sent),
        }
    }

    fn repos(&self) -> ferroma_storage::Repositories {
        self.db.repositories()
    }

    /// Append a `message_created` change for a synthetic message id.
    async fn record_created(&self, message_id: i64) -> i64 {
        self.service
            .record(NewChange {
                user_id: self.user_id,
                mailbox_id: Some(self.mailbox_id),
                folder_id: Some(self.folder_id),
                message_id: Some(MessageId::new(message_id)),
                kind: ChangeKind::MessageCreated.as_str().to_string(),
                payload: serde_json::json!({ "uid": message_id, "flags": "" }),
            })
            .await
            .unwrap()
            .seq
    }

    async fn cleanup(self) {
        let schema = self.schema.clone();
        self.db.close().await;
        let _ = self
            .admin
            .execute(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE").as_str())
            .await;
        let _ = self.admin.close().await;
    }
}

/// Build a harness, or return early (and print why) when no database is reachable.
///
/// The double braces matter: without them the expanded `if … { return; }` swallows
/// the trailing expression and every use site gets `()`.
macro_rules! harness {
    () => {{
        if !database_available().await {
            eprintln!("skipping: no PostgreSQL reachable");
            return;
        }
        Harness::new().await
    }};
}

#[tokio::test]
async fn a_first_sync_returns_everything_from_zero() {
    let h = harness!();
    h.record_created(101).await;
    h.record_created(102).await;
    h.record_created(103).await;

    let page = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor::ZERO))
        .await
        .unwrap();

    assert_eq!(page.changes.len(), 3);
    assert_eq!(page.next_cursor, Cursor(3));
    assert_eq!(page.latest_cursor, Cursor(3));
    assert!(!page.has_more);
    assert_eq!(page.changes[0].kind, ChangeKind::MessageCreated);
    assert_eq!(page.changes[0].message_id, Some(MessageId::new(101)));
    assert_eq!(page.changes[2].message_id, Some(MessageId::new(103)));

    h.cleanup().await;
}

#[tokio::test]
async fn the_cursor_is_exclusive() {
    let h = harness!();
    h.record_created(101).await;
    let second = h.record_created(102).await;

    let page = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor(second - 1)))
        .await
        .unwrap();

    // Only the change *after* the cursor.
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].seq, second);

    // Asking again from the returned cursor yields nothing.
    let again = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, page.next_cursor))
        .await
        .unwrap();
    assert!(again.is_empty());
    assert_eq!(again.next_cursor, page.next_cursor, "an empty page does not move the cursor");

    h.cleanup().await;
}

#[tokio::test]
async fn paging_reports_has_more_and_never_skips_a_change() {
    let h = harness!();
    for id in 1..=25 {
        h.record_created(id).await;
    }

    let mut cursor = Cursor::ZERO;
    let mut seen: Vec<i64> = Vec::new();
    let mut pages = 0;

    loop {
        let page = h
            .service
            .sync(SyncRequest {
                user_id: h.user_id,
                mailbox_id: h.mailbox_id,
                folder_id: Some(h.folder_id),
                cursor,
                limit: Some(7),
            })
            .await
            .unwrap();
        pages += 1;
        assert!(pages <= 10, "paging must terminate");

        for change in &page.changes {
            seen.push(change.seq);
        }
        if !page.has_more {
            assert!(page.changes.len() <= 7);
            break;
        }
        assert_eq!(page.changes.len(), 7, "a full page when more are waiting");
        cursor = page.next_cursor;
    }

    assert_eq!(pages, 4, "25 changes at 7 per page: 7+7+7+4");
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    assert_eq!(seen, sorted, "changes arrive in ascending seq order");
    assert_eq!(seen, (1..=25).collect::<Vec<_>>(), "no change is skipped or repeated");

    h.cleanup().await;
}

#[tokio::test]
async fn a_cursor_ahead_of_the_server_is_a_conflict() {
    let h = harness!();
    h.record_created(1).await;

    let err = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor(999)))
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Conflict(_)), "{err:?}");
    assert!(err.to_string().contains("full resync"), "{err}");

    h.cleanup().await;
}

#[tokio::test]
async fn a_cursor_older_than_the_retained_history_is_a_conflict() {
    let h = harness!();
    for id in 1..=5 {
        h.record_created(id).await;
    }

    // Simulate the retention job having pruned everything up to seq 3.
    sqlx::query("DELETE FROM change_log WHERE seq <= 3")
        .execute(h.repos().pool())
        .await
        .unwrap();

    // A client at cursor 1 has missed changes 2 and 3, which no longer exist.
    let err = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor(1)))
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Conflict(_)), "{err:?}");

    // A first-time client (cursor 0) is never told its cursor is stale: it is going
    // to do a full sync anyway, and everything it needs is still there.
    let page = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor::ZERO))
        .await
        .unwrap();
    assert_eq!(page.changes.len(), 2, "the retained changes");

    // A client that is exactly caught up to the oldest retained entry is fine.
    let ok = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor(3)))
        .await
        .unwrap();
    assert_eq!(ok.changes.len(), 2);

    h.cleanup().await;
}

#[tokio::test]
async fn account_level_and_folder_level_sync_see_different_streams() {
    let h = harness!();
    h.record_created(1).await;
    h.service
        .record_folder_change(
            h.user_id,
            h.mailbox_id,
            h.other_folder_id,
            "Sent",
            ChangeKind::FolderCreated,
        )
        .await
        .unwrap();

    let account = h
        .service
        .sync(SyncRequest::account(h.user_id, h.mailbox_id, Cursor::ZERO))
        .await
        .unwrap();
    assert_eq!(account.changes.len(), 2, "account level sees both");

    let folder = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor::ZERO))
        .await
        .unwrap();
    assert_eq!(folder.changes.len(), 2, "the mailbox stream carries both changes");

    h.cleanup().await;
}

#[tokio::test]
async fn placeholder_carrying_changes_survive_a_round_trip() {
    let h = harness!();
    h.service
        .record_message_deleted(
            h.user_id,
            h.mailbox_id,
            h.folder_id,
            MessageId::new(4712),
            117,
            true,
        )
        .await
        .unwrap();

    let page = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor::ZERO))
        .await
        .unwrap();
    let change = &page.changes[0];
    assert_eq!(change.kind, ChangeKind::MessageDeleted);
    assert_eq!(change.message_id, Some(MessageId::new(4712)));
    assert_eq!(change.uid, Some(117));
    assert_eq!(change.permanent, Some(true));

    // The documented wire shape.
    let json = serde_json::to_value(change).unwrap();
    assert_eq!(json["type"], "message_deleted");
    assert_eq!(json["permanent"], true);

    h.cleanup().await;
}

#[tokio::test]
async fn pruning_only_removes_old_changes() {
    let h = harness!();
    for id in 1..=3 {
        h.record_created(id).await;
    }
    // Backdate two of them beyond the 30-day window.
    sqlx::query("UPDATE change_log SET created_at = NOW() - INTERVAL '90 days' WHERE seq <= 2")
        .execute(h.repos().pool())
        .await
        .unwrap();

    assert_eq!(h.service.prune().await.unwrap(), 2);

    let page = h
        .service
        .sync(SyncRequest::folder(h.user_id, h.mailbox_id, h.folder_id, Cursor::ZERO))
        .await
        .unwrap();
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].seq, 3);

    h.cleanup().await;
}

#[tokio::test]
async fn an_operation_runs_once_and_replays_its_result() {
    let h = harness!();
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // First call: the closure runs.
    let c = counter.clone();
    let first: serde_json::Value = h
        .service
        .with_operation("op_mark_read_1", h.user_id, "mark_read", || async move {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({ "message_id": 4821, "seen": true }))
        })
        .await
        .unwrap();
    assert_eq!(first["message_id"], 4821);
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Replay: the closure must NOT run again, and the same value comes back.
    let c = counter.clone();
    let second: serde_json::Value = h
        .service
        .with_operation("op_mark_read_1", h.user_id, "mark_read", || async move {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({ "message_id": 9999, "seen": false }))
        })
        .await
        .unwrap();
    assert_eq!(second, first, "the recorded result is replayed verbatim");
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1, "no second execution");

    h.cleanup().await;
}

#[tokio::test]
async fn a_failed_operation_is_recorded_and_replayed_as_a_failure() {
    let h = harness!();
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let c = counter.clone();
    let err = h
        .service
        .with_operation::<serde_json::Value, _, _>("op_fail_1", h.user_id, "move", || async move {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(FerromaError::Forbidden("not your message".into()))
        })
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Forbidden(_)));

    // The retry does not re-run the work, and reports a failure.
    let c = counter.clone();
    let err = h
        .service
        .with_operation::<serde_json::Value, _, _>("op_fail_1", h.user_id, "move", || async move {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({ "unexpected": true }))
        })
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Conflict(_)), "{err:?}");
    assert!(err.to_string().contains("forbidden"), "{err}");
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

    h.cleanup().await;
}

#[tokio::test]
async fn an_unfinished_operation_asks_the_client_to_retry() {
    let h = harness!();
    // Claim the id without completing it, as a crash mid-request would leave it.
    let outcome = h
        .repos()
        .operations
        .begin("op_in_flight", Some(h.user_id), "send")
        .await
        .unwrap();
    assert!(matches!(outcome, OperationOutcome::Fresh));

    let err = h
        .service
        .with_operation::<serde_json::Value, _, _>("op_in_flight", h.user_id, "send", || async {
            Ok(serde_json::json!({ "should": "not run" }))
        })
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Conflict(_)), "{err:?}");
    assert!(err.to_string().contains("has not finished"), "{err}");

    h.cleanup().await;
}

#[tokio::test]
async fn an_empty_operation_id_is_refused() {
    let h = harness!();
    let err = h
        .service
        .with_operation::<serde_json::Value, _, _>("   ", h.user_id, "send", || async {
            Ok(serde_json::json!({}))
        })
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Invalid(_)));
    h.cleanup().await;
}

#[tokio::test]
async fn device_cursors_are_per_folder_and_resettable() {
    let h = harness!();
    let device = h
        .repos()
        .devices
        .upsert(ferroma_storage::repository::DeviceUpsert {
            user_id: h.user_id,
            device_uid: "laptop-1".into(),
            name: None,
            platform: Some("windows".into()),
            client_version: None,
            protocol_version: Some(1),
            ip: None,
        })
        .await
        .unwrap();
    let device_id = ferroma_core::DeviceId::new(device.id);

    // Unknown device/folder pair defaults to zero, so a first sync starts clean.
    assert_eq!(
        h.service.device_cursor(device_id, h.mailbox_id, Some(h.folder_id)).await.unwrap(),
        Cursor::ZERO
    );

    h.service
        .set_device_cursor(device_id, h.mailbox_id, Some(h.folder_id), Cursor(17))
        .await
        .unwrap();
    h.service
        .set_device_cursor(device_id, h.mailbox_id, Some(h.other_folder_id), Cursor(4))
        .await
        .unwrap();
    h.service
        .set_device_cursor(device_id, h.mailbox_id, None, Cursor(9))
        .await
        .unwrap();

    assert_eq!(
        h.service.device_cursor(device_id, h.mailbox_id, Some(h.folder_id)).await.unwrap(),
        Cursor(17)
    );
    assert_eq!(
        h.service.device_cursor(device_id, h.mailbox_id, Some(h.other_folder_id)).await.unwrap(),
        Cursor(4),
        "cursors are independent per folder"
    );
    assert_eq!(
        h.service.device_cursor(device_id, h.mailbox_id, None).await.unwrap(),
        Cursor(9),
        "the account-level cursor is its own row"
    );

    // Updating is an upsert, not a duplicate insert.
    h.service
        .set_device_cursor(device_id, h.mailbox_id, Some(h.folder_id), Cursor(18))
        .await
        .unwrap();
    let states = h.repos().sync_states.list_for_device(device_id).await.unwrap();
    assert_eq!(states.len(), 3, "one row per (folder or account) — no duplicates");

    // Resetting makes the next sync start from zero.
    assert_eq!(h.service.reset_device(device_id).await.unwrap(), 3);
    assert_eq!(
        h.service.device_cursor(device_id, h.mailbox_id, Some(h.folder_id)).await.unwrap(),
        Cursor::ZERO
    );

    h.cleanup().await;
}

#[tokio::test]
async fn changes_are_scoped_to_their_owner() {
    let h = harness!();
    h.record_created(1).await;

    // A second user with their own mailbox sees an empty stream.
    let other = h
        .repos()
        .users
        .create(NewUser {
            email: "bob@example.com".into(),
            password_hash: "$argon2id$x".into(),
            display_name: None,
            is_admin: false,
            quota_bytes: None,
        })
        .await
        .unwrap();
    let some_mailbox = h.mailbox_id;

    let page = h
        .service
        .sync(SyncRequest::account(UserId::new(other.id), some_mailbox, Cursor::ZERO))
        .await
        .unwrap();
    assert!(page.is_empty(), "one user's changes must never appear in another's stream");

    h.cleanup().await;
}
