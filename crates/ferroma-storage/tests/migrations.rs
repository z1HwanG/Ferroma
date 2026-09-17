//! Schema tests: the migration set is the platform's most load-bearing artifact,
//! so it is verified against a real PostgreSQL rather than reviewed by eye.
//!
//! These tests create throwaway databases; see `tests/common/mod.rs`.

mod common;

use common::{fresh_database, TestDatabase};
use ferroma_storage::StorageError;

/// Minimal rows so the rest of the suite has something to hang foreign keys on.
struct Seed {
    user_id: i64,
    domain_id: i64,
    mailbox_id: i64,
    folder_id: i64,
}

async fn seed(t: &TestDatabase) -> Seed {
    let pool = t.pool();
    let (user_id,): (i64,) = sqlx::query_as(
        "INSERT INTO users (email, password_hash) VALUES ('alice@example.com', '$argon2id$x') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("insert user");

    let (domain_id,): (i64,) = sqlx::query_as(
        "INSERT INTO domains (name) VALUES ('example.com') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("insert domain");

    let (mailbox_id,): (i64,) = sqlx::query_as(
        "INSERT INTO mailboxes (user_id, domain_id, local_part, is_primary)
         VALUES ($1, $2, 'alice', TRUE) RETURNING id",
    )
    .bind(user_id)
    .bind(domain_id)
    .fetch_one(pool)
    .await
    .expect("insert mailbox");

    let (folder_id,): (i64,) = sqlx::query_as(
        "INSERT INTO folders (mailbox_id, name) VALUES ($1, 'INBOX') RETURNING id",
    )
    .bind(mailbox_id)
    .fetch_one(pool)
    .await
    .expect("insert folder");

    Seed {
        user_id,
        domain_id,
        mailbox_id,
        folder_id,
    }
}

#[tokio::test]
async fn migrations_apply_cleanly_to_an_empty_database() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let (total, applied) = t.db().migration_status().await.unwrap();
    assert_eq!(total, applied, "every embedded migration must be recorded as applied");
    assert!(total >= 1);
    t.cleanup().await;
}

#[tokio::test]
async fn migrations_are_idempotent() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    // Running the migrator again must be a no-op, not an error: every restart does it.
    t.db().migrate().await.expect("second migrate must succeed");
    let (total, applied) = t.db().migration_status().await.unwrap();
    assert_eq!(total, applied);
    t.cleanup().await;
}

#[tokio::test]
async fn every_expected_table_exists() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let expected = [
        "users",
        "domains",
        "mailboxes",
        "aliases",
        "folders",
        "messages",
        "message_recipients",
        "attachments",
        "mail_queue",
        "delivery_attempts",
        "devices",
        "sessions",
        "client_sync_states",
        "drafts",
        "operations",
        "change_log",
        "audit_logs",
        "login_attempts",
        "settings",
    ];

    let rows: Vec<(String,)> = sqlx::query_as(
        // `current_schema()`, not `'public'`: the harness gives every test its own
        // schema and pins the connection's `search_path` to it, so that is where the
        // migrations actually created the tables. Asserting against `public` would
        // pass on a machine where someone had migrated by hand and fail everywhere
        // else — the opposite of what this test is for.
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'",
    )
    .fetch_all(t.pool())
    .await
    .unwrap();
    let present: Vec<String> = rows.into_iter().map(|(n,)| n).collect();

    for table in expected {
        assert!(present.contains(&table.to_string()), "missing table {table}; have {present:?}");
    }
    t.cleanup().await;
}

#[tokio::test]
async fn expected_indexes_exist() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let expected = [
        "users_email_key",
        "domains_name_key",
        "mailboxes_address_key",
        "mailboxes_primary_key",
        "folders_name_key",
        "messages_folder_uid_key",
        "messages_subject_fts_idx",
        "mail_queue_due_idx",
        "sessions_token_key",
        "devices_uid_key",
        "client_sync_states_key",
    ];
    let rows: Vec<(String,)> =
        // Same reasoning as `every_expected_table_exists`: the indexes live in this
        // test's own schema, which is what `search_path` points at.
        sqlx::query_as("SELECT indexname FROM pg_indexes WHERE schemaname = current_schema()")
            .fetch_all(t.pool())
            .await
            .unwrap();
    let present: Vec<String> = rows.into_iter().map(|(n,)| n).collect();
    for index in expected {
        assert!(present.contains(&index.to_string()), "missing index {index}; have {present:?}");
    }
    t.cleanup().await;
}

#[tokio::test]
async fn user_email_must_be_unique_and_lowercase() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    // Duplicate primary address.
    let dup = sqlx::query("INSERT INTO users (email, password_hash) VALUES ($1, 'x')")
        .bind("alice@example.com")
        .execute(t.pool())
        .await
        .unwrap_err();
    let err = StorageError::Database(dup);
    assert!(err.is_unique_violation(), "expected 23505, got {err:?}");
    assert_eq!(err.constraint(), Some("users_email_key"));

    // Mixed case is rejected by the CHECK, which keeps lookups index-friendly.
    let mixed = sqlx::query("INSERT INTO users (email, password_hash) VALUES ('Bob@example.com', 'x')")
        .execute(t.pool())
        .await
        .unwrap_err();
    assert!(StorageError::Database(mixed).is_check_violation());

    assert_eq!(t.count("users").await, 1);
    let _ = s;
    t.cleanup().await;
}

#[tokio::test]
async fn exactly_one_primary_address_per_user() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    let second = sqlx::query(
        "INSERT INTO mailboxes (user_id, domain_id, local_part, is_primary)
         VALUES ($1, $2, 'alice.alt', TRUE)",
    )
    .bind(s.user_id)
    .bind(s.domain_id)
    .execute(t.pool())
    .await
    .unwrap_err();
    let err = StorageError::Database(second);
    assert!(err.is_unique_violation(), "{err:?}");
    assert_eq!(err.constraint(), Some("mailboxes_primary_key"));

    // A second, non-primary address is fine.
    sqlx::query(
        "INSERT INTO mailboxes (user_id, domain_id, local_part, is_primary)
         VALUES ($1, $2, 'alice.alt', FALSE)",
    )
    .bind(s.user_id)
    .bind(s.domain_id)
    .execute(t.pool())
    .await
    .expect("non-primary address must be allowed");

    t.cleanup().await;
}

#[tokio::test]
async fn addresses_are_unique_per_domain() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    let dup = sqlx::query(
        "INSERT INTO mailboxes (user_id, domain_id, local_part) VALUES ($1, $2, 'alice')",
    )
    .bind(s.user_id)
    .bind(s.domain_id)
    .execute(t.pool())
    .await
    .unwrap_err();
    let err = StorageError::Database(dup);
    assert_eq!(err.constraint(), Some("mailboxes_address_key"));
    t.cleanup().await;
}

#[tokio::test]
async fn folder_special_use_is_constrained() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    // A known special-use marker is accepted...
    sqlx::query("INSERT INTO folders (mailbox_id, name, special_use) VALUES ($1, 'Sent', '\\Sent')")
        .bind(s.mailbox_id)
        .execute(t.pool())
        .await
        .expect("\\Sent must be accepted");

    // ...an unknown one is not.
    let bad = sqlx::query("INSERT INTO folders (mailbox_id, name, special_use) VALUES ($1, 'X', '\\Nope')")
        .bind(s.mailbox_id)
        .execute(t.pool())
        .await
        .unwrap_err();
    assert!(StorageError::Database(bad).is_check_violation());

    // Exactly one folder per special-use marker per mailbox.
    let dup = sqlx::query("INSERT INTO folders (mailbox_id, name, special_use) VALUES ($1, 'Sent2', '\\Sent')")
        .bind(s.mailbox_id)
        .execute(t.pool())
        .await
        .unwrap_err();
    assert!(StorageError::Database(dup).is_unique_violation());

    t.cleanup().await;
}

#[tokio::test]
async fn message_uids_are_unique_within_a_folder() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    let insert = |uid: i64| {
        let pool = t.pool().clone();
        let (folder_id, mailbox_id) = (s.folder_id, s.mailbox_id);
        async move {
            sqlx::query(
                "INSERT INTO messages (folder_id, mailbox_id, uid, size_bytes, storage_path)
                 VALUES ($1, $2, $3, 10, 'x')",
            )
            .bind(folder_id)
            .bind(mailbox_id)
            .bind(uid)
            .execute(&pool)
            .await
        }
    };

    insert(1).await.expect("first uid");
    insert(2).await.expect("second uid");
    let dup = insert(1).await.unwrap_err();
    let err = StorageError::Database(dup);
    assert!(err.is_unique_violation(), "{err:?}");
    assert_eq!(err.constraint(), Some("messages_folder_uid_key"));
    t.cleanup().await;
}

#[tokio::test]
async fn deleting_a_user_cascades_through_the_whole_mailbox_tree() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    sqlx::query(
        "INSERT INTO messages (folder_id, mailbox_id, uid, size_bytes, storage_path)
         VALUES ($1, $2, 1, 10, 'x')",
    )
    .bind(s.folder_id)
    .bind(s.mailbox_id)
    .execute(t.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO attachments (message_id, size_bytes, storage_path) SELECT id, 5, 'y' FROM messages WHERE uid = 1")
        .execute(t.pool())
        .await
        .unwrap();

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(s.user_id)
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(t.count("mailboxes").await, 0);
    assert_eq!(t.count("folders").await, 0);
    assert_eq!(t.count("messages").await, 0);
    assert_eq!(t.count("attachments").await, 0);
    // The domain is not owned by the user and must survive.
    assert_eq!(t.count("domains").await, 1);
    t.cleanup().await;
}

#[tokio::test]
async fn queue_status_is_constrained_and_due_lookup_works() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;
    let (message_id,): (i64,) = sqlx::query_as(
        "INSERT INTO messages (folder_id, mailbox_id, uid, size_bytes, storage_path)
         VALUES ($1, $2, 1, 10, 'x') RETURNING id",
    )
    .bind(s.folder_id)
    .bind(s.mailbox_id)
    .fetch_one(t.pool())
    .await
    .unwrap();

    let bad = sqlx::query(
        "INSERT INTO mail_queue (message_id, sender, recipient, status)
         VALUES ($1, 'a@example.com', 'b@example.net', 'exploded')",
    )
    .bind(message_id)
    .execute(t.pool())
    .await
    .unwrap_err();
    assert!(StorageError::Database(bad).is_check_violation());

    sqlx::query(
        "INSERT INTO mail_queue (message_id, sender, recipient, status, next_attempt_at)
         VALUES ($1, 'a@example.com', 'b@example.net', 'pending', NOW() - INTERVAL '1 minute')",
    )
    .bind(message_id)
    .execute(t.pool())
    .await
    .unwrap();

    // The dispatcher's query, which the partial index is designed for.
    let (due,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM mail_queue
         WHERE status IN ('pending', 'retry') AND next_attempt_at <= NOW()",
    )
    .fetch_one(t.pool())
    .await
    .unwrap();
    assert_eq!(due, 1);
    t.cleanup().await;
}

#[tokio::test]
async fn change_log_sequence_is_monotonic() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    let mut seqs = Vec::new();
    for _ in 0..5 {
        let (seq,): (i64,) = sqlx::query_as(
            "INSERT INTO change_log (user_id, mailbox_id, folder_id, message_id, kind)
             VALUES ($1, $2, $3, NULL, 'message_created') RETURNING seq",
        )
        .bind(s.user_id)
        .bind(s.mailbox_id)
        .bind(s.folder_id)
        .fetch_one(t.pool())
        .await
        .unwrap();
        seqs.push(seq);
    }
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "seq must be assigned in increasing order");
    assert_eq!(seqs.len(), 5);
    t.cleanup().await;
}

#[tokio::test]
async fn change_log_tombstones_survive_message_deletion() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;
    let (message_id,): (i64,) = sqlx::query_as(
        "INSERT INTO messages (folder_id, mailbox_id, uid, size_bytes, storage_path)
         VALUES ($1, $2, 1, 10, 'x') RETURNING id",
    )
    .bind(s.folder_id)
    .bind(s.mailbox_id)
    .fetch_one(t.pool())
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO change_log (user_id, mailbox_id, folder_id, message_id, kind)
         VALUES ($1, $2, $3, $4, 'message_deleted')",
    )
    .bind(s.user_id)
    .bind(s.mailbox_id)
    .bind(s.folder_id)
    .bind(message_id)
    .execute(t.pool())
    .await
    .unwrap();

    // Deleting the message must NOT remove the tombstone: an offline client still
    // has to learn that the message went away.
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(message_id)
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(t.count("change_log").await, 1);
    t.cleanup().await;
}

#[tokio::test]
async fn client_sync_state_keeps_one_account_level_row_per_device() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;
    let (device_id,): (i64,) = sqlx::query_as(
        "INSERT INTO devices (user_id, device_uid, platform) VALUES ($1, 'dev-1', 'windows') RETURNING id",
    )
    .bind(s.user_id)
    .fetch_one(t.pool())
    .await
    .unwrap();

    sqlx::query("INSERT INTO client_sync_states (device_id, mailbox_id, folder_id, cursor) VALUES ($1, $2, NULL, 1)")
        .bind(device_id)
        .bind(s.mailbox_id)
        .execute(t.pool())
        .await
        .expect("first account-level row");

    // The COALESCE(folder_id, 0) expression index must reject a second NULL row.
    let dup = sqlx::query("INSERT INTO client_sync_states (device_id, mailbox_id, folder_id, cursor) VALUES ($1, $2, NULL, 2)")
        .bind(device_id)
        .bind(s.mailbox_id)
        .execute(t.pool())
        .await
        .unwrap_err();
    assert!(StorageError::Database(dup).is_unique_violation());

    // Per-folder rows coexist.
    sqlx::query("INSERT INTO client_sync_states (device_id, mailbox_id, folder_id, cursor) VALUES ($1, $2, $3, 5)")
        .bind(device_id)
        .bind(s.mailbox_id)
        .bind(s.folder_id)
        .execute(t.pool())
        .await
        .expect("per-folder row");

    assert_eq!(t.count("client_sync_states").await, 2);
    t.cleanup().await;
}

#[tokio::test]
async fn operations_primary_key_enforces_idempotency() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    sqlx::query("INSERT INTO operations (operation_id, user_id, kind) VALUES ('op_1', $1, 'mark_read')")
        .bind(s.user_id)
        .execute(t.pool())
        .await
        .unwrap();

    let dup = sqlx::query("INSERT INTO operations (operation_id, user_id, kind) VALUES ('op_1', $1, 'mark_read')")
        .bind(s.user_id)
        .execute(t.pool())
        .await
        .unwrap_err();
    let err = StorageError::Database(dup);
    assert!(err.is_unique_violation(), "{err:?}");

    let bad = sqlx::query("INSERT INTO operations (operation_id, user_id, kind, status) VALUES ('op_2', $1, 'x', 'maybe')")
        .bind(s.user_id)
        .execute(t.pool())
        .await
        .unwrap_err();
    assert!(StorageError::Database(bad).is_check_violation());
    t.cleanup().await;
}

#[tokio::test]
async fn subject_full_text_index_is_usable() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let s = seed(&t).await;

    sqlx::query(
        "INSERT INTO messages (folder_id, mailbox_id, uid, subject, size_bytes, storage_path)
         VALUES ($1, $2, 1, 'Invoice for September', 10, 'a'),
                ($1, $2, 2, 'Meeting notes',         10, 'b')",
    )
    .bind(s.folder_id)
    .bind(s.mailbox_id)
    .execute(t.pool())
    .await
    .unwrap();

    // Force the planner to consider the expression index; if the index expression
    // did not match the query, this still returns correct rows.
    t.execute("SET enable_seqscan = off").await.unwrap();
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT subject FROM messages
         WHERE to_tsvector('simple', coalesce(subject, '')) @@ to_tsquery('simple', 'invoice')",
    )
    .fetch_all(t.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "Invoice for September");
    t.cleanup().await;
}

#[tokio::test]
async fn server_version_reports_postgres() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_database().await;
    let version = t.db().server_version().await.unwrap();
    assert!(version.starts_with("PostgreSQL"), "{version}");
    assert!(t.db().health().await.is_ok());
    assert!(t.db().size_bytes().await.unwrap() > 0);
    let stats = t.db().pool_stats();
    assert!(stats.max >= 1);
    t.cleanup().await;
}

#[tokio::test]
async fn the_test_harness_isolates_each_test_in_its_own_schema() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let a = fresh_database().await;
    let b = fresh_database().await;
    assert_ne!(a.name(), b.name());

    // Writes in one schema must be invisible in the other. This is the property
    // that lets the suite run in parallel without one test dropping another's data.
    a.execute("INSERT INTO domains (name) VALUES ('a.example')").await.unwrap();
    assert_eq!(a.count("domains").await, 1);
    assert_eq!(b.count("domains").await, 0, "schemas must not share tables");

    // The schema name is what the pool's search_path points at.
    let (current,): (String,) = sqlx::query_as("SELECT current_schema()")
        .fetch_one(a.pool())
        .await
        .unwrap();
    assert_eq!(current, a.name());

    a.cleanup().await;
    b.cleanup().await;
}

#[tokio::test]
async fn redacted_url_never_leaks_a_password() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    // The helper opens its pool from explicit options, so the description is
    // synthesised from host/port/database and can never contain a password.
    let t = fresh_database().await;
    let described = t.db().redacted_url();
    assert!(described.starts_with("postgres://"), "{described}");
    assert!(!described.contains("ferroma_test_pw"), "{described}");

    // A URL-bearing pool redacts the password itself.
    let raw = "postgres://ferroma:s3cret@127.0.0.1:5433/ferroma_test";
    let redacted = ferroma_storage::database::redact(raw);
    assert_eq!(redacted, "postgres://ferroma:***@127.0.0.1:5433/ferroma_test");
    t.cleanup().await;
}
