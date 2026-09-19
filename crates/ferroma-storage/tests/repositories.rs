//! Integration tests for the repository layer: identity, addresses, folders and mail.
//!
//! Every test runs against a real, throwaway, fully migrated PostgreSQL schema
//! (see `tests/common/mod.rs`), so the tests can run in parallel without seeing
//! each other's rows.

mod common;

use chrono::{DateTime, TimeZone, Utc};
use ferroma_core::{DomainId, MailboxId, MessageId, UserId};
use ferroma_storage::models::Message;
use ferroma_storage::repository::{
    MessageSearch, NewAttachment, NewMailbox, NewMessage, NewUser, Recipient,
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

/// Same as [`setup!`], but with a wide connection pool.
///
/// The concurrency tests below assert that UID allocation and queue claiming are
/// serialized by *row locks*. With the default two-connection pool the tasks would
/// instead queue up on connection acquisition, which is a weaker test of the same
/// property — so these tests ask for a pool wide enough to keep many statements
/// genuinely in flight at once.
macro_rules! setup_wide {
    () => {{
        if !common::database_available().await {
            eprintln!(
                "skipping: no PostgreSQL reachable at {}",
                common::admin_url()
            );
            return;
        }
        common::fresh_database_with_pool(16).await
    }};
}

/// A fixed instant, so `internal_date` ordering is deterministic.
fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
}

/// The handful of ids nearly every test needs.
struct Fixture {
    user_id: UserId,
    domain_id: DomainId,
    mailbox_id: MailboxId,
    inbox_id: MailboxId,
}

/// Seed a user, a domain, an address and its six standard folders.
async fn fixture(t: &TestDatabase) -> Fixture {
    let repos = t.repos();

    let user = repos
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
        .create("example.com", Some("the test domain"))
        .await
        .expect("seed domain");

    let mailbox = repos
        .mailboxes
        .create(NewMailbox {
            user_id: user.user_id(),
            domain_id: domain.domain_id(),
            local_part: "alice".into(),
            display_name: Some("Alice".into()),
            is_primary: true,
            quota_bytes: None,
        })
        .await
        .expect("seed mailbox");

    // `mailboxes.create` already provisions the six standard folders; this call
    // returns them (and shows that `ensure_standard` stays a no-op on a complete
    // mailbox, which every test in this file then relies on).
    let folders = repos
        .folders
        .ensure_standard(mailbox.mailbox_id())
        .await
        .expect("seed folders");

    Fixture {
        user_id: user.user_id(),
        domain_id: domain.domain_id(),
        mailbox_id: mailbox.mailbox_id(),
        inbox_id: folders
            .iter()
            .find(|folder| folder.is_inbox())
            .expect("INBOX must exist")
            .folder_id(),
    }
}

/// A `NewMessage` with sensible defaults.
fn message_in(f: &Fixture, folder: MailboxId, subject: &str, offset_secs: i64) -> NewMessage {
    NewMessage {
        folder_id: folder,
        mailbox_id: f.mailbox_id,
        rfc_message_id: Some(format!("<{subject}@example.net>")),
        thread_id: None,
        subject: Some(subject.to_string()),
        sender: Some("bob@example.net".into()),
        sender_name: Some("Bob".into()),
        snippet: Some(format!("{subject} preview")),
        size_bytes: 512,
        storage_path: format!("example.com/alice/Maildir/cur/{subject}:2,"),
        checksum_sha256: None,
        flags: String::new(),
        internal_date: Some(at(offset_secs)),
        sent_at: Some(at(offset_secs)),
        has_attachments: false,
        attachment_count: 0,
        is_draft: false,
    }
}

/// Store one message in the fixture's INBOX.
async fn seed_message(t: &TestDatabase, f: &Fixture, subject: &str, offset_secs: i64) -> Message {
    t.repos()
        .messages
        .insert(message_in(f, f.inbox_id, subject, offset_secs))
        .await
        .expect("insert message")
}

// ===========================================================================
// users
// ===========================================================================

#[tokio::test]
async fn user_create_lowercases_and_finds_case_insensitively() {
    let t = setup!();
    let repos = t.repos();

    let user = repos
        .users
        .create(NewUser {
            email: "  Alice@Example.COM ".into(),
            password_hash: "hash".into(),
            display_name: Some("Alice".into()),
            is_admin: false,
            enabled: true,
            quota_bytes: Some(2048),
        })
        .await
        .expect("create");

    assert_eq!(user.email, "alice@example.com");
    assert_eq!(user.quota_bytes, 2048);
    assert!(user.enabled);
    assert!(!user.is_admin);

    let found = repos
        .users
        .find_by_email("ALICE@EXAMPLE.COM")
        .await
        .unwrap()
        .expect("case-insensitive lookup");
    assert_eq!(found.id, user.id);

    let by_id = repos.users.find_by_id(user.user_id()).await.unwrap();
    assert_eq!(by_id.map(|u| u.id), Some(user.id));
    assert_eq!(
        repos.users.require_by_email("alice@example.com").await.unwrap().id,
        user.id
    );
    assert_eq!(repos.users.require_by_id(user.user_id()).await.unwrap().id, user.id);

    t.cleanup().await;
}

#[tokio::test]
async fn user_default_quota_is_one_gib() {
    let t = setup!();
    let repos = t.repos();

    let user = repos
        .users
        .create(NewUser {
            email: "q@example.com".into(),
            password_hash: "hash".into(),
            display_name: None,
            is_admin: false,
            enabled: true,
            quota_bytes: None,
        })
        .await
        .unwrap();

    assert_eq!(user.quota_bytes, 1_073_741_824);
    assert_eq!(user.used_bytes, 0);
    assert_eq!(user.failed_logins, 0);
    assert!(user.locked_until.is_none());
    assert!(user.last_login_at.is_none());

    t.cleanup().await;
}

#[tokio::test]
async fn user_duplicate_email_is_conflict() {
    let t = setup!();
    let repos = t.repos();

    let new = |email: &str| NewUser {
        email: email.into(),
        password_hash: "hash".into(),
        display_name: None,
        is_admin: false,
        enabled: true,
        quota_bytes: None,
    };

    repos.users.create(new("dup@example.com")).await.unwrap();
    let err = repos
        .users
        .create(new("DUP@example.com"))
        .await
        .expect_err("the same address twice must conflict");

    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");
    assert_eq!(repos.users.count().await.unwrap(), 1);

    t.cleanup().await;
}

#[tokio::test]
async fn user_missing_lookups_are_none_and_require_is_not_found() {
    let t = setup!();
    let repos = t.repos();

    assert!(repos.users.find_by_id(UserId::new(4242)).await.unwrap().is_none());
    assert!(repos.users.find_by_email("nobody@example.com").await.unwrap().is_none());

    let err = repos.users.require_by_id(UserId::new(4242)).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");
    assert!(err.to_string().contains("4242"), "{err}");

    let err = repos
        .users
        .require_by_email("nobody@example.com")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn user_list_counts_and_paging() {
    let t = setup!();
    let repos = t.repos();

    for name in ["a", "b", "c"] {
        repos
            .users
            .create(NewUser {
                email: format!("{name}@example.com"),
                password_hash: "hash".into(),
                display_name: None,
                is_admin: name == "a",
                enabled: true,
                quota_bytes: None,
            })
            .await
            .unwrap();
    }

    assert_eq!(repos.users.count().await.unwrap(), 3);
    assert_eq!(repos.users.count_admins().await.unwrap(), 1);

    let all = repos.users.list(10, 0).await.unwrap();
    assert_eq!(all.len(), 3);
    // Newest first: the ids descend.
    assert!(all[0].id > all[1].id && all[1].id > all[2].id);

    let page = repos.users.list(1, 1).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, all[1].id);

    assert!(repos.users.list(10, 10).await.unwrap().is_empty());
    assert!(repos.users.list(-1, -1).await.unwrap().is_empty());

    t.cleanup().await;
}

#[tokio::test]
async fn user_setters_round_trip() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos.users.set_enabled(f.user_id, false).await.unwrap();
    repos.users.set_admin(f.user_id, true).await.unwrap();
    repos.users.set_display_name(f.user_id, Some("Alice Liddell")).await.unwrap();
    repos.users.set_quota(f.user_id, 4096).await.unwrap();
    repos.users.update_password(f.user_id, "$argon2id$new").await.unwrap();

    let user = repos.users.require_by_id(f.user_id).await.unwrap();
    assert!(!user.enabled);
    assert!(user.is_admin);
    assert_eq!(user.display_name.as_deref(), Some("Alice Liddell"));
    assert_eq!(user.quota_bytes, 4096);
    assert_eq!(user.password_hash, "$argon2id$new");

    repos.users.set_display_name(f.user_id, None).await.unwrap();
    assert!(repos
        .users
        .require_by_id(f.user_id)
        .await
        .unwrap()
        .display_name
        .is_none());

    let err = repos.users.set_enabled(UserId::new(9999), false).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn user_lockout_after_threshold_clears_on_success() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let now = at(0);

    let first = repos
        .users
        .record_login_failure(f.user_id, now, 900, 3)
        .await
        .unwrap();
    assert_eq!(first.failed_logins, 1);
    assert!(first.locked_until.is_none(), "one failure must not lock");

    let second = repos
        .users
        .record_login_failure(f.user_id, now, 900, 3)
        .await
        .unwrap();
    assert_eq!(second.failed_logins, 2);
    assert!(second.locked_until.is_none());

    let third = repos
        .users
        .record_login_failure(f.user_id, now, 900, 3)
        .await
        .unwrap();
    assert_eq!(third.failed_logins, 3);
    let locked_until = third.locked_until.expect("the third failure must lock");
    assert_eq!(locked_until, now + chrono::Duration::seconds(900));
    assert!(!third.is_login_allowed(now));
    assert!(third.is_login_allowed(locked_until + chrono::Duration::seconds(1)));

    // A success wipes both the counter and the lock.
    repos.users.record_login_success(f.user_id, at(10)).await.unwrap();
    let recovered = repos.users.require_by_id(f.user_id).await.unwrap();
    assert_eq!(recovered.failed_logins, 0);
    assert!(recovered.locked_until.is_none());
    assert_eq!(recovered.last_login_at, Some(at(10)));
    assert!(recovered.is_login_allowed(now));

    t.cleanup().await;
}

#[tokio::test]
async fn user_add_usage_clamps_at_zero() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    assert_eq!(repos.users.add_usage(f.user_id, 1000).await.unwrap(), 1000);
    assert_eq!(repos.users.add_usage(f.user_id, 500).await.unwrap(), 1500);
    assert_eq!(repos.users.add_usage(f.user_id, -2000).await.unwrap(), 0);
    assert_eq!(repos.users.add_usage(f.user_id, 7).await.unwrap(), 7);

    let err = repos.users.add_usage(UserId::new(9999), 1).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn user_delete_reports_and_cascades() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    assert!(!repos.users.delete(UserId::new(9999)).await.unwrap());
    assert!(repos.users.delete(f.user_id).await.unwrap());
    assert!(!repos.users.delete(f.user_id).await.unwrap());

    assert_eq!(t.count("users").await, 0);
    assert_eq!(t.count("mailboxes").await, 0);
    assert_eq!(t.count("folders").await, 0);

    t.cleanup().await;
}

// ===========================================================================
// domains
// ===========================================================================

#[tokio::test]
async fn domain_create_lowercases_and_duplicate_conflicts() {
    let t = setup!();
    let repos = t.repos();

    let domain = repos.domains.create(" Example.COM ", Some("hi")).await.unwrap();
    assert_eq!(domain.name, "example.com");
    assert_eq!(domain.description.as_deref(), Some("hi"));
    assert!(domain.enabled);

    let err = repos.domains.create("EXAMPLE.com", None).await.unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn domain_find_and_require_are_case_insensitive() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    assert_eq!(
        repos.domains.find_by_name("EXAMPLE.COM").await.unwrap().map(|d| d.id),
        Some(f.domain_id.get())
    );
    assert_eq!(
        repos.domains.find_by_id(f.domain_id).await.unwrap().map(|d| d.id),
        Some(f.domain_id.get())
    );
    assert_eq!(
        repos.domains.require_by_name("Example.Com").await.unwrap().domain_id(),
        f.domain_id
    );

    let err = repos.domains.require_by_name("nope.example").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");
    assert!(repos.domains.find_by_name("nope.example").await.unwrap().is_none());

    t.cleanup().await;
}

#[tokio::test]
async fn domain_list_enabled_and_count() {
    let t = setup!();
    let repos = t.repos();

    let a = repos.domains.create("b.example", None).await.unwrap();
    repos.domains.create("a.example", None).await.unwrap();
    repos.domains.set_enabled(a.domain_id(), false).await.unwrap();

    let all = repos.domains.list().await.unwrap();
    assert_eq!(
        all.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        vec!["a.example", "b.example"]
    );

    let enabled = repos.domains.list_enabled().await.unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].name, "a.example");
    assert_eq!(repos.domains.count().await.unwrap(), 2);

    t.cleanup().await;
}

#[tokio::test]
async fn domain_setters_round_trip() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos
        .domains
        .set_description(f.domain_id, Some("primary"))
        .await
        .unwrap();
    repos
        .domains
        .set_catch_all(f.domain_id, Some("Postmaster"))
        .await
        .unwrap();
    repos
        .domains
        .set_dkim(
            f.domain_id,
            Some("ferroma"),
            Some("PRIVATE KEY"),
            Some("PUBLIC KEY"),
        )
        .await
        .unwrap();

    let domain = repos.domains.find_by_id(f.domain_id).await.unwrap().unwrap();
    assert_eq!(domain.description.as_deref(), Some("primary"));
    assert_eq!(domain.catch_all.as_deref(), Some("postmaster"));
    assert_eq!(domain.dkim_selector.as_deref(), Some("ferroma"));
    assert_eq!(domain.dkim_private_key.as_deref(), Some("PRIVATE KEY"));
    assert_eq!(domain.dkim_public_key.as_deref(), Some("PUBLIC KEY"));

    repos.domains.set_catch_all(f.domain_id, None).await.unwrap();
    repos.domains.set_dkim(f.domain_id, None, None, None).await.unwrap();
    let cleared = repos.domains.find_by_id(f.domain_id).await.unwrap().unwrap();
    assert!(cleared.catch_all.is_none());
    assert!(cleared.dkim_selector.is_none());

    let err = repos
        .domains
        .set_enabled(DomainId::new(9999), false)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn domain_delete_cascades_and_reports() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos
        .aliases
        .create(f.domain_id, "sales", "alice@example.com")
        .await
        .unwrap();

    assert!(!repos.domains.delete(DomainId::new(9999)).await.unwrap());
    assert!(repos.domains.delete(f.domain_id).await.unwrap());
    assert_eq!(t.count("domains").await, 0);
    assert_eq!(t.count("mailboxes").await, 0);
    assert_eq!(t.count("aliases").await, 0);

    t.cleanup().await;
}

// ===========================================================================
// aliases
// ===========================================================================

#[tokio::test]
async fn alias_create_find_and_list() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let alias = repos
        .aliases
        .create(f.domain_id, " Sales ", "Alice@Example.com")
        .await
        .unwrap();
    assert_eq!(alias.local_part, "sales");
    assert_eq!(alias.target, "alice@example.com");
    assert!(alias.enabled);

    let found = repos.aliases.find(f.domain_id, "SALES").await.unwrap().unwrap();
    assert_eq!(found.id, alias.id);
    assert_eq!(
        repos.aliases.find_by_id(alias.id).await.unwrap().map(|a| a.id),
        Some(alias.id)
    );

    repos
        .aliases
        .create(f.domain_id, "info", "sales")
        .await
        .unwrap();
    let listed = repos.aliases.list_by_domain(f.domain_id).await.unwrap();
    assert_eq!(
        listed.iter().map(|a| a.local_part.as_str()).collect::<Vec<_>>(),
        vec!["info", "sales"]
    );

    let err = repos
        .aliases
        .create(f.domain_id, "sales", "other@example.com")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn alias_find_only_returns_enabled_rows() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let alias = repos
        .aliases
        .create(f.domain_id, "sales", "alice@example.com")
        .await
        .unwrap();

    repos.aliases.set_enabled(alias.id, false).await.unwrap();
    assert!(repos.aliases.find(f.domain_id, "sales").await.unwrap().is_none());
    assert!(repos.aliases.find_by_id(alias.id).await.unwrap().is_some());
    assert_eq!(repos.aliases.list_by_domain(f.domain_id).await.unwrap().len(), 1);

    repos.aliases.set_enabled(alias.id, true).await.unwrap();
    assert!(repos.aliases.find(f.domain_id, "sales").await.unwrap().is_some());

    let err = repos.aliases.set_enabled(9999, true).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn alias_set_target_and_delete() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let alias = repos
        .aliases
        .create(f.domain_id, "sales", "alice@example.com")
        .await
        .unwrap();

    repos
        .aliases
        .set_target(alias.id, "Bob@Example.NET")
        .await
        .unwrap();
    let updated = repos.aliases.find_by_id(alias.id).await.unwrap().unwrap();
    assert_eq!(updated.target, "bob@example.net");

    let err = repos.aliases.set_target(9999, "x@example.com").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    assert!(repos.aliases.delete(alias.id).await.unwrap());
    assert!(!repos.aliases.delete(alias.id).await.unwrap());

    t.cleanup().await;
}

// ===========================================================================
// mailboxes
// ===========================================================================

#[tokio::test]
async fn mailbox_create_normalises_and_finds_by_address() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let mailbox = repos
        .mailboxes
        .create(NewMailbox {
            user_id: f.user_id,
            domain_id: f.domain_id,
            local_part: " Bob ".into(),
            display_name: None,
            is_primary: false,
            quota_bytes: None,
        })
        .await
        .unwrap();
    assert_eq!(mailbox.local_part, "bob");
    assert!(!mailbox.is_primary);

    let found = repos
        .mailboxes
        .find_by_address("EXAMPLE.COM", "BOB")
        .await
        .unwrap()
        .expect("case-insensitive address lookup");
    assert_eq!(found.id, mailbox.id);

    let joined = repos
        .mailboxes
        .find_by_address_with_domain("example.com", "bob")
        .await
        .unwrap()
        .expect("joined lookup");
    assert_eq!(joined.domain, "example.com");
    assert_eq!(joined.address(), "bob@example.com");
    assert_eq!(joined.mailbox.id, mailbox.id);

    let err = repos
        .mailboxes
        .create(NewMailbox {
            user_id: f.user_id,
            domain_id: f.domain_id,
            local_part: "bob".into(),
            display_name: None,
            is_primary: false,
            quota_bytes: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn mailbox_address_lookup_returns_disabled_but_exists_reports_it() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos.mailboxes.set_enabled(f.mailbox_id, false).await.unwrap();

    let found = repos
        .mailboxes
        .find_by_address("example.com", "alice")
        .await
        .unwrap()
        .expect("a disabled address is still returned");
    assert!(!found.enabled);

    assert!(repos
        .mailboxes
        .address_exists("example.com", "ALICE")
        .await
        .unwrap());
    assert!(!repos
        .mailboxes
        .address_exists("example.com", "carol")
        .await
        .unwrap());
    assert!(!repos
        .mailboxes
        .address_exists("example.net", "alice")
        .await
        .unwrap());

    t.cleanup().await;
}

#[tokio::test]
async fn mailbox_primary_is_unique_per_user() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    // alice is primary (from the fixture).
    let secondary = repos
        .mailboxes
        .create(NewMailbox {
            user_id: f.user_id,
            domain_id: f.domain_id,
            local_part: "alice2".into(),
            display_name: None,
            is_primary: false,
            quota_bytes: None,
        })
        .await
        .unwrap();

    assert_eq!(
        repos.mailboxes.find_primary(f.user_id).await.unwrap().map(|m| m.id),
        Some(f.mailbox_id.get())
    );

    // Promoting the second address demotes the first, it does not explode.
    repos.mailboxes.set_primary(secondary.mailbox_id(), true).await.unwrap();
    assert_eq!(
        repos.mailboxes.find_primary(f.user_id).await.unwrap().map(|m| m.id),
        Some(secondary.id)
    );
    assert!(!repos.mailboxes.find_by_id(f.mailbox_id).await.unwrap().unwrap().is_primary);

    // And `create` with `is_primary` does the same.
    let third = repos
        .mailboxes
        .create(NewMailbox {
            user_id: f.user_id,
            domain_id: f.domain_id,
            local_part: "alice3".into(),
            display_name: None,
            is_primary: true,
            quota_bytes: None,
        })
        .await
        .unwrap();
    assert!(third.is_primary);
    assert!(!repos
        .mailboxes
        .find_by_id(secondary.mailbox_id())
        .await
        .unwrap()
        .unwrap()
        .is_primary);

    repos.mailboxes.set_primary(third.mailbox_id(), false).await.unwrap();
    assert!(repos.mailboxes.find_primary(f.user_id).await.unwrap().is_none());

    t.cleanup().await;
}

#[tokio::test]
async fn mailbox_lists_by_user_and_domain() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos
        .mailboxes
        .create(NewMailbox {
            user_id: f.user_id,
            domain_id: f.domain_id,
            local_part: "aaa".into(),
            display_name: None,
            is_primary: false,
            quota_bytes: None,
        })
        .await
        .unwrap();

    let listed = repos.mailboxes.list_by_user(f.user_id).await.unwrap();
    // Primary first, then alphabetical.
    assert_eq!(
        listed.iter().map(|m| m.local_part.as_str()).collect::<Vec<_>>(),
        vec!["alice", "aaa"]
    );

    let joined = repos
        .mailboxes
        .list_by_user_with_domain(f.user_id)
        .await
        .unwrap();
    assert_eq!(joined.len(), 2);
    assert_eq!(joined[0].address(), "alice@example.com");

    let by_domain = repos.mailboxes.list_by_domain(f.domain_id).await.unwrap();
    assert_eq!(
        by_domain.iter().map(|m| m.local_part.as_str()).collect::<Vec<_>>(),
        vec!["aaa", "alice"]
    );

    t.cleanup().await;
}

#[tokio::test]
async fn mailbox_setters_and_delete() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos
        .mailboxes
        .set_display_name(f.mailbox_id, Some("Alice's mail"))
        .await
        .unwrap();
    repos.mailboxes.set_quota(f.mailbox_id, Some(4096)).await.unwrap();
    repos.mailboxes.set_quota(f.mailbox_id, None).await.unwrap();

    let mailbox = repos.mailboxes.find_by_id(f.mailbox_id).await.unwrap().unwrap();
    assert_eq!(mailbox.display_name.as_deref(), Some("Alice's mail"));
    assert!(mailbox.quota_bytes.is_none());

    let err = repos
        .mailboxes
        .set_display_name(MailboxId::new(9999), None)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    assert!(repos.mailboxes.delete(f.mailbox_id).await.unwrap());
    assert_eq!(t.count("folders").await, 0);

    t.cleanup().await;
}

#[tokio::test]
async fn mailbox_effective_quota_prefers_its_own() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos.users.set_quota(f.user_id, 10_000).await.unwrap();
    assert_eq!(repos.mailboxes.quota(f.mailbox_id).await.unwrap(), 10_000);

    repos.mailboxes.set_quota(f.mailbox_id, Some(2_000)).await.unwrap();
    assert_eq!(repos.mailboxes.quota(f.mailbox_id).await.unwrap(), 2_000);

    let err = repos.mailboxes.quota(MailboxId::new(9999)).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn mailbox_check_quota_reports_the_numbers() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos.users.set_quota(f.user_id, 1_000).await.unwrap();

    // Under quota: fine.
    repos.mailboxes.check_quota(f.mailbox_id, 999).await.unwrap();

    // Over quota: the error carries used / needed / limit.
    let err = repos.mailboxes.check_quota(f.mailbox_id, 1_001).await.unwrap_err();
    match err {
        StorageError::QuotaExceeded {
            mailbox_id,
            used,
            needed,
            limit,
        } => {
            assert_eq!(mailbox_id, f.mailbox_id.get());
            assert_eq!(used, 0);
            assert_eq!(needed, 1_001);
            assert_eq!(limit, 1_000);
        }
        other => panic!("expected QuotaExceeded, got {other:?}"),
    }

    // Storing bytes moves the used figure.
    seed_message(&t, &f, "quota", 0).await;
    let err = repos.mailboxes.check_quota(f.mailbox_id, 900).await.unwrap_err();
    match err {
        StorageError::QuotaExceeded { used, needed, limit, .. } => {
            assert_eq!(used, 512);
            assert_eq!(needed, 900);
            assert_eq!(limit, 1_000);
        }
        other => panic!("expected QuotaExceeded, got {other:?}"),
    }

    t.cleanup().await;
}

#[tokio::test]
async fn mailbox_usage_is_account_wide_and_recomputable() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let second = repos
        .mailboxes
        .create(NewMailbox {
            user_id: f.user_id,
            domain_id: f.domain_id,
            local_part: "second".into(),
            display_name: None,
            is_primary: false,
            quota_bytes: None,
        })
        .await
        .unwrap();
    let second_inbox = repos
        .folders
        .require_by_name(second.mailbox_id(), "INBOX")
        .await
        .unwrap();

    seed_message(&t, &f, "one", 0).await;
    let mut other = message_in(&f, second_inbox.folder_id(), "two", 1);
    other.mailbox_id = second.mailbox_id();
    other.size_bytes = 1000;
    repos.messages.insert(other).await.unwrap();

    assert_eq!(repos.mailboxes.used_bytes(f.mailbox_id).await.unwrap(), 1512);
    assert_eq!(repos.mailboxes.used_bytes(second.mailbox_id()).await.unwrap(), 1512);

    assert_eq!(
        repos.mailboxes.add_usage(f.mailbox_id, 100).await.unwrap(),
        100
    );
    // The user's own counter is what moved.
    assert_eq!(
        repos.users.require_by_id(f.user_id).await.unwrap().used_bytes,
        100
    );

    assert_eq!(
        repos.mailboxes.recompute_usage(second.mailbox_id()).await.unwrap(),
        1512
    );
    assert_eq!(
        repos.users.require_by_id(f.user_id).await.unwrap().used_bytes,
        1512
    );

    let err = repos.mailboxes.add_usage(MailboxId::new(9999), 1).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

// ===========================================================================
// folders
// ===========================================================================

#[tokio::test]
async fn folder_ensure_standard_creates_six_marked_folders() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let folders = repos.folders.ensure_standard(f.mailbox_id).await.unwrap();
    let pairs: Vec<(&str, Option<&str>)> = folders
        .iter()
        .map(|folder| (folder.name.as_str(), folder.special_use.as_deref()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("INBOX", None),
            ("Sent", Some("\\Sent")),
            ("Drafts", Some("\\Drafts")),
            ("Trash", Some("\\Trash")),
            ("Junk", Some("\\Junk")),
            ("Archive", Some("\\Archive")),
        ]
    );
    for folder in &folders {
        assert!(folder.subscribed, "{} must start subscribed", folder.name);
        assert_eq!(folder.uid_validity, 1);
        assert_eq!(folder.uid_next, 1);
    }
    assert_eq!(t.count("folders").await, 6);

    // Idempotent: the same rows, no duplicates, no new ids.
    let again = repos.folders.ensure_standard(f.mailbox_id).await.unwrap();
    assert_eq!(
        again.iter().map(|folder| folder.id).collect::<Vec<_>>(),
        folders.iter().map(|folder| folder.id).collect::<Vec<_>>()
    );
    assert_eq!(t.count("folders").await, 6);

    t.cleanup().await;
}

#[tokio::test]
async fn folder_ensure_standard_adopts_an_unmarked_folder() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    // A mailbox whose folders predate this scheme: the rows carry the standard names
    // but no `special_use`, and INBOX is spelled the legacy way. Row inserts are raw
    // SQL on purpose — `folders.create` would canonicalise the name and the marker,
    // which is exactly the state being tested.
    sqlx::query("DELETE FROM folders WHERE mailbox_id = $1 AND name IN ('INBOX', 'Sent')")
        .bind(f.mailbox_id.get())
        .execute(t.pool())
        .await
        .unwrap();

    let plain = repos
        .folders
        .create(f.mailbox_id, "Sent", None)
        .await
        .expect("a plain Sent folder");
    assert!(plain.special_use.is_none());

    sqlx::query("INSERT INTO folders (mailbox_id, name) VALUES ($1, 'inbox')")
        .bind(f.mailbox_id.get())
        .execute(t.pool())
        .await
        .unwrap();

    let folders = repos.folders.ensure_standard(f.mailbox_id).await.unwrap();
    assert_eq!(t.count("folders").await, 6, "nothing may be duplicated");

    let sent = folders.iter().find(|folder| folder.name == "Sent").unwrap();
    assert_eq!(sent.id, plain.id, "the existing row is adopted, not replaced");
    assert_eq!(sent.special_use.as_deref(), Some("\\Sent"));

    // The legacy `inbox` row is adopted as-is; no second INBOX appears.
    let inbox = folders.iter().find(|folder| folder.is_inbox()).unwrap();
    assert_eq!(inbox.name, "inbox");
    assert_eq!(
        repos.folders.find_by_name(f.mailbox_id, "InBox").await.unwrap().map(|folder| folder.id),
        Some(inbox.id)
    );

    // Idempotent from here on.
    let again = repos.folders.ensure_standard(f.mailbox_id).await.unwrap();
    assert_eq!(
        again.iter().map(|folder| folder.id).collect::<Vec<_>>(),
        folders.iter().map(|folder| folder.id).collect::<Vec<_>>()
    );
    assert_eq!(t.count("folders").await, 6);

    t.cleanup().await;
}

#[tokio::test]
async fn folder_list_puts_inbox_first_then_alphabetical() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    repos.folders.create(f.mailbox_id, "zeta", None).await.unwrap();
    repos.folders.create(f.mailbox_id, "Alpha", None).await.unwrap();

    let names: Vec<String> = repos
        .folders
        .list(f.mailbox_id)
        .await
        .unwrap()
        .into_iter()
        .map(|folder| folder.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "INBOX", "Alpha", "Archive", "Drafts", "Junk", "Sent", "Trash", "zeta"
        ]
    );

    t.cleanup().await;
}

#[tokio::test]
async fn folder_find_by_name_is_case_insensitive_only_for_inbox() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let inbox = repos
        .folders
        .find_by_name(f.mailbox_id, "inbox")
        .await
        .unwrap()
        .expect("INBOX matches case-insensitively");
    assert_eq!(inbox.id, f.inbox_id.get());
    assert!(repos
        .folders
        .find_by_name(f.mailbox_id, "InBox")
        .await
        .unwrap()
        .is_some());

    repos.folders.create(f.mailbox_id, "Work", None).await.unwrap();
    assert!(repos.folders.find_by_name(f.mailbox_id, "Work").await.unwrap().is_some());
    assert!(
        repos.folders.find_by_name(f.mailbox_id, "work").await.unwrap().is_none(),
        "only INBOX is case-insensitive"
    );

    assert_eq!(
        repos
            .folders
            .require_by_name(f.mailbox_id, "Work")
            .await
            .unwrap()
            .name,
        "Work"
    );
    let err = repos.folders.require_by_name(f.mailbox_id, "Nope").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");
    assert!(err.to_string().contains("Nope"), "{err}");

    assert_eq!(
        repos.folders.find_by_id(f.inbox_id).await.unwrap().map(|folder| folder.id),
        Some(f.inbox_id.get())
    );
    assert!(repos.folders.find_by_id(MailboxId::new(9999)).await.unwrap().is_none());

    t.cleanup().await;
}

#[tokio::test]
async fn folder_create_normalises_inbox_and_conflicts_on_duplicates() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    // A mailbox that has lost its INBOX (a repair script, an over-eager client):
    // `create` must spell it canonically whatever case the caller uses.
    assert!(repos.folders.delete(f.inbox_id).await.unwrap());

    let inbox = repos.folders.create(f.mailbox_id, "inbox", None).await.unwrap();
    assert_eq!(inbox.name, "INBOX");
    assert!(inbox.is_inbox());

    let err = repos
        .folders
        .create(f.mailbox_id, "INBOX", None)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");

    let err = repos
        .folders
        .create(f.mailbox_id, "Bad", Some("\\Nonsense"))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Invalid(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn folder_rename_and_special_use() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let work = repos.folders.create(f.mailbox_id, "Work", None).await.unwrap();
    let renamed = repos.folders.rename(work.folder_id(), "Work/2026").await.unwrap();
    assert_eq!(renamed.name, "Work/2026");

    let err = repos.folders.rename(work.folder_id(), "Sent").await.unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");

    let err = repos.folders.rename(MailboxId::new(9999), "Nowhere").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    let flagged = repos
        .folders
        .create(f.mailbox_id, "Later", None)
        .await
        .unwrap();
    repos
        .folders
        .set_special_use(flagged.folder_id(), Some("\\Flagged"))
        .await
        .unwrap();
    assert_eq!(
        repos
            .folders
            .find_by_id(flagged.folder_id())
            .await
            .unwrap()
            .unwrap()
            .special_use
            .as_deref(),
        Some("\\Flagged")
    );

    let err = repos
        .folders
        .set_special_use(flagged.folder_id(), Some("nope"))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Invalid(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn folder_subscription_round_trip() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let work = repos.folders.create(f.mailbox_id, "Work", None).await.unwrap();
    repos.folders.set_subscribed(work.folder_id(), false).await.unwrap();

    // Six standard folders (the fixture's, which `mailboxes.create` also provisions)
    // plus Work, minus the one just unsubscribed.
    let subscribed = repos.folders.list_subscribed(f.mailbox_id).await.unwrap();
    assert_eq!(subscribed.len(), 6);
    assert!(subscribed.iter().all(|folder| folder.name != "Work"));
    assert_eq!(subscribed[0].name, "INBOX", "INBOX still sorts first");

    repos.folders.set_subscribed(work.folder_id(), true).await.unwrap();
    assert_eq!(repos.folders.list_subscribed(f.mailbox_id).await.unwrap().len(), 7);

    let err = repos
        .folders
        .set_subscribed(MailboxId::new(9999), true)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn folder_allocate_uid_is_race_free() {
    let t = setup_wide!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let mut handles = Vec::new();
    for _ in 0..32 {
        let repos = repos.clone();
        let folder = f.inbox_id;
        handles.push(tokio::spawn(async move {
            let mut uids = Vec::with_capacity(20);
            for _ in 0..20 {
                uids.push(repos.folders.allocate_uid(folder).await.expect("allocate UID"));
            }
            uids
        }));
    }

    let mut all = Vec::new();
    for handle in handles {
        all.extend(handle.await.expect("task must not panic"));
    }
    assert_eq!(all.len(), 640);

    all.sort_unstable();
    let expected: Vec<i64> = (1..=640).collect();
    assert_eq!(all, expected, "UIDs must be exactly 1..=640, with no duplicates");

    let inbox = repos.folders.find_by_id(f.inbox_id).await.unwrap().unwrap();
    assert_eq!(inbox.uid_next, 641);

    t.cleanup().await;
}

#[tokio::test]
async fn folder_allocate_uid_and_modseq_on_a_missing_folder() {
    let t = setup!();
    let repos = t.repos();

    let err = repos.folders.allocate_uid(MailboxId::new(9999)).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");
    let err = repos.folders.bump_modseq(MailboxId::new(9999)).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");
    let err = repos.folders.recount(MailboxId::new(9999)).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn folder_bump_modseq_and_uid_validity_and_delete() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    assert_eq!(repos.folders.bump_modseq(f.inbox_id).await.unwrap(), 2);
    assert_eq!(repos.folders.bump_modseq(f.inbox_id).await.unwrap(), 3);

    repos.folders.set_uid_validity(f.inbox_id, 1234).await.unwrap();
    assert_eq!(
        repos.folders.find_by_id(f.inbox_id).await.unwrap().unwrap().uid_validity,
        1234
    );

    assert!(repos.folders.delete(f.inbox_id).await.unwrap());
    assert!(!repos.folders.delete(f.inbox_id).await.unwrap());

    t.cleanup().await;
}

#[tokio::test]
async fn folder_recount_matches_the_messages() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let first = seed_message(&t, &f, "one", 0).await;
    let second = seed_message(&t, &f, "two", 1).await;
    repos.messages.mark_seen(first.message_id(), true).await.unwrap();

    let folder = repos.folders.recount(f.inbox_id).await.unwrap();
    assert_eq!(folder.message_count, 2);
    assert_eq!(folder.unseen_count, 1);
    assert_eq!(folder.total_bytes, 1024);

    // Expunged messages drop out of every counter.
    repos.messages.mark_deleted(second.message_id()).await.unwrap();
    repos.messages.expunge(f.inbox_id).await.unwrap();

    let folder = repos.folders.recount(f.inbox_id).await.unwrap();
    assert_eq!(folder.message_count, 1);
    assert_eq!(folder.unseen_count, 0);
    assert_eq!(folder.total_bytes, 512);

    t.cleanup().await;
}

// ===========================================================================
// messages
// ===========================================================================

#[tokio::test]
async fn message_insert_allocates_uid_and_finds_by_uid() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let first = seed_message(&t, &f, "one", 0).await;
    let second = seed_message(&t, &f, "two", 1).await;

    assert_eq!(first.uid, 1);
    assert_eq!(second.uid, 2);
    assert_eq!(first.folder_id, f.inbox_id.get());
    assert_eq!(first.mailbox_id, f.mailbox_id.get());
    assert_eq!(first.subject.as_deref(), Some("one"));
    assert_eq!(first.internal_date, at(0));
    assert_eq!(first.received_at, first.created_at);
    assert_eq!(first.flags, "");
    assert!(first.is_live());

    let found = repos
        .messages
        .find_by_uid(f.inbox_id, 2)
        .await
        .unwrap()
        .expect("uid 2 exists");
    assert_eq!(found.id, second.id);
    assert!(repos.messages.find_by_uid(f.inbox_id, 99).await.unwrap().is_none());

    assert_eq!(
        repos
            .messages
            .require_by_id(first.message_id())
            .await
            .unwrap()
            .id,
        first.id
    );

    t.cleanup().await;
}

#[tokio::test]
async fn message_insert_without_a_folder_is_not_found() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let err = repos
        .messages
        .insert(message_in(&f, MailboxId::new(9999), "orphan", 0))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_require_by_id_on_a_missing_row() {
    let t = setup!();
    let repos = t.repos();

    assert!(repos.messages.find_by_id(MessageId::new(9999)).await.unwrap().is_none());
    let err = repos
        .messages
        .require_by_id(MessageId::new(9999))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_list_by_folder_is_newest_first() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    seed_message(&t, &f, "old", 0).await;
    seed_message(&t, &f, "new", 100).await;
    seed_message(&t, &f, "middle", 50).await;

    let subjects: Vec<String> = repos
        .messages
        .list_by_folder(f.inbox_id, 10, 0)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|message| message.subject)
        .collect();
    assert_eq!(subjects, vec!["new", "middle", "old"]);

    let page = repos.messages.list_by_folder(f.inbox_id, 1, 1).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].subject.as_deref(), Some("middle"));

    assert_eq!(
        repos.messages.list_by_mailbox(f.mailbox_id, 10, 0).await.unwrap().len(),
        3
    );

    t.cleanup().await;
}

#[tokio::test]
async fn message_counts_max_uid_and_newest() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    assert_eq!(repos.messages.max_uid(f.inbox_id).await.unwrap(), 0);
    assert_eq!(repos.messages.count_by_folder(f.inbox_id).await.unwrap(), 0);

    let first = seed_message(&t, &f, "one", 0).await;
    seed_message(&t, &f, "two", 10).await;
    seed_message(&t, &f, "three", 20).await;

    assert_eq!(repos.messages.max_uid(f.inbox_id).await.unwrap(), 3);
    assert_eq!(repos.messages.count_by_folder(f.inbox_id).await.unwrap(), 3);
    assert_eq!(repos.messages.count_unseen(f.inbox_id).await.unwrap(), 3);

    repos.messages.mark_seen(first.message_id(), true).await.unwrap();
    assert_eq!(repos.messages.count_unseen(f.inbox_id).await.unwrap(), 2);

    let newest = repos.messages.newest(f.inbox_id, 2).await.unwrap();
    assert_eq!(
        newest
            .iter()
            .map(|message| message.subject.clone().unwrap())
            .collect::<Vec<_>>(),
        vec!["three", "two"]
    );

    t.cleanup().await;
}

#[tokio::test]
async fn message_list_by_uids_and_unexpunged() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let a = seed_message(&t, &f, "a", 0).await;
    let b = seed_message(&t, &f, "b", 1).await;
    let c = seed_message(&t, &f, "c", 2).await;

    let picked = repos.messages.list_by_uids(f.inbox_id, &[3, 1]).await.unwrap();
    assert_eq!(
        picked.iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![1, 3],
        "results are in UID order, not request order"
    );
    assert!(repos.messages.list_by_uids(f.inbox_id, &[]).await.unwrap().is_empty());

    assert_eq!(repos.messages.list_unexpunged(f.inbox_id).await.unwrap().len(), 3);

    repos.messages.mark_deleted(b.message_id()).await.unwrap();
    let expunged = repos.messages.expunge(f.inbox_id).await.unwrap();
    assert_eq!(expunged.len(), 1);
    assert_eq!(expunged[0].id, b.id);

    let live = repos.messages.list_unexpunged(f.inbox_id).await.unwrap();
    assert_eq!(live.len(), 2);
    assert_eq!(
        live.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![a.id, c.id]
    );

    t.cleanup().await;
}

#[tokio::test]
async fn message_find_by_rfc_message_id_spans_folders() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let sent = repos
        .folders
        .require_by_name(f.mailbox_id, "Sent")
        .await
        .unwrap();

    let stored = seed_message(&t, &f, "hello", 0).await;
    let copy = repos
        .messages
        .copy_to_folder(stored.message_id(), sent.folder_id(), f.mailbox_id)
        .await
        .unwrap();

    let found = repos
        .messages
        .find_by_rfc_message_id(f.mailbox_id, "<hello@example.net>")
        .await
        .unwrap();
    assert_eq!(found.len(), 2);
    assert!(found.iter().any(|m| m.id == stored.id));
    assert!(found.iter().any(|m| m.id == copy.id));
    assert!(repos
        .messages
        .find_by_rfc_message_id(f.mailbox_id, "<nope@example.net>")
        .await
        .unwrap()
        .is_empty());

    t.cleanup().await;
}

#[tokio::test]
async fn message_add_flags_is_a_union_and_idempotent() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "flags", 0).await;

    assert_eq!(
        repos.messages.add_flags(message.message_id(), "seen").await.unwrap(),
        "seen"
    );
    assert_eq!(
        repos.messages.add_flags(message.message_id(), "flagged $label1").await.unwrap(),
        "seen flagged $label1"
    );
    // Idempotent, including on case: `\Seen` is already `seen`.
    assert_eq!(
        repos.messages.add_flags(message.message_id(), "SEEN flagged").await.unwrap(),
        "seen flagged $label1"
    );

    assert_eq!(
        repos
            .messages
            .require_by_id(message.message_id())
            .await
            .unwrap()
            .flags,
        "seen flagged $label1"
    );

    let err = repos.messages.add_flags(MessageId::new(9999), "seen").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_remove_flags_subtracts_only_what_is_asked() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "flags", 0).await;

    repos.messages.set_flags(message.message_id(), "seen flagged $label1").await.unwrap();
    assert_eq!(
        repos.messages.remove_flags(message.message_id(), "FLAGGED").await.unwrap(),
        "seen $label1"
    );
    // Removing something that is not there changes nothing.
    assert_eq!(
        repos.messages.remove_flags(message.message_id(), "deleted").await.unwrap(),
        "seen $label1"
    );
    assert_eq!(
        repos
            .messages
            .remove_flags(message.message_id(), "seen $label1")
            .await
            .unwrap(),
        ""
    );

    let err = repos.messages.remove_flags(MessageId::new(9999), "seen").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_mark_seen_toggles_the_flag() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "seen", 0).await;

    repos.messages.mark_seen(message.message_id(), true).await.unwrap();
    assert_eq!(
        repos.messages.require_by_id(message.message_id()).await.unwrap().flags,
        "seen"
    );

    repos.messages.mark_seen(message.message_id(), false).await.unwrap();
    assert_eq!(
        repos.messages.require_by_id(message.message_id()).await.unwrap().flags,
        ""
    );

    let err = repos.messages.mark_seen(MessageId::new(9999), true).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_mark_and_clear_deleted_manage_the_soft_delete() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "doomed", 0).await;

    repos.messages.set_flags(message.message_id(), "seen").await.unwrap();
    repos.messages.mark_deleted(message.message_id()).await.unwrap();
    let deleted = repos.messages.require_by_id(message.message_id()).await.unwrap();
    assert_eq!(deleted.flags, "seen deleted");
    assert!(deleted.deleted_at.is_some());
    assert!(deleted.is_live(), "marking deleted does not expunge");

    // Idempotent.
    repos.messages.mark_deleted(message.message_id()).await.unwrap();
    assert_eq!(
        repos.messages.require_by_id(message.message_id()).await.unwrap().flags,
        "seen deleted"
    );

    repos.messages.clear_deleted(message.message_id()).await.unwrap();
    let cleared = repos.messages.require_by_id(message.message_id()).await.unwrap();
    assert_eq!(cleared.flags, "seen");
    assert!(cleared.deleted_at.is_none());

    let err = repos.messages.mark_deleted(MessageId::new(9999)).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");
    let err = repos.messages.clear_deleted(MessageId::new(9999)).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_expunge_returns_only_deleted_rows() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let keep = seed_message(&t, &f, "keep", 0).await;
    let drop_one = seed_message(&t, &f, "drop-one", 1).await;
    let drop_two = seed_message(&t, &f, "drop-two", 2).await;

    repos.messages.mark_deleted(drop_one.message_id()).await.unwrap();
    repos.messages.mark_deleted(drop_two.message_id()).await.unwrap();

    let expunged = repos.messages.expunge(f.inbox_id).await.unwrap();
    assert_eq!(expunged.len(), 2);
    assert_eq!(
        expunged.iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![2, 3],
        "expunged rows come back in UID order"
    );
    assert!(expunged.iter().all(|m| !m.is_live()));

    // Expunging again finds nothing.
    assert!(repos.messages.expunge(f.inbox_id).await.unwrap().is_empty());

    assert_eq!(repos.messages.count_by_folder(f.inbox_id).await.unwrap(), 1);
    let survivor = repos.messages.require_by_id(keep.message_id()).await.unwrap();
    assert!(survivor.is_live());
    assert_eq!(survivor.flags, "");
    assert!(repos.messages.find_by_uid(f.inbox_id, 1).await.unwrap().is_some());
    assert!(
        repos.messages.find_by_uid(f.inbox_id, 2).await.unwrap().is_none(),
        "an expunged UID is gone from the folder"
    );

    t.cleanup().await;
}

#[tokio::test]
async fn message_hard_delete_returns_the_row_once() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "gone", 0).await;

    let removed = repos.messages.hard_delete(message.message_id()).await.unwrap();
    assert_eq!(removed.map(|m| m.id), Some(message.id));
    assert!(repos.messages.hard_delete(message.message_id()).await.unwrap().is_none());
    assert_eq!(t.count("messages").await, 0);

    t.cleanup().await;
}

#[tokio::test]
async fn message_move_allocates_a_fresh_uid_in_the_target() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let trash = repos.folders.require_by_name(f.mailbox_id, "Trash").await.unwrap();
    let kept = seed_message(&t, &f, "stays", 0).await;
    let moved = seed_message(&t, &f, "moves", 1).await;

    let after = repos
        .messages
        .move_to_folder(moved.message_id(), trash.folder_id(), f.mailbox_id)
        .await
        .unwrap();

    assert_eq!(after.id, moved.id, "a move is the same row");
    assert_eq!(after.folder_id, trash.id);
    assert_eq!(after.uid, 1, "the target folder hands out its own first UID");
    assert_eq!(after.subject.as_deref(), Some("moves"));

    assert_eq!(repos.messages.count_by_folder(f.inbox_id).await.unwrap(), 1);
    assert_eq!(repos.messages.count_by_folder(trash.folder_id()).await.unwrap(), 1);
    assert!(repos.messages.find_by_uid(f.inbox_id, 2).await.unwrap().is_none());
    assert!(repos.messages.find_by_uid(trash.folder_id(), 1).await.unwrap().is_some());
    assert_eq!(
        repos.messages.require_by_id(kept.message_id()).await.unwrap().uid,
        1
    );

    let err = repos
        .messages
        .move_to_folder(MessageId::new(9999), trash.folder_id(), f.mailbox_id)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    let err = repos
        .messages
        .move_to_folder(kept.message_id(), MailboxId::new(9999), f.mailbox_id)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_copy_preserves_the_source_and_duplicates_the_sub_rows() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let archive = repos.folders.require_by_name(f.mailbox_id, "Archive").await.unwrap();
    let stored = seed_message(&t, &f, "archive-me", 0).await;
    repos
        .messages
        .insert_recipients(
            stored.message_id(),
            &[Recipient {
                kind: "to".into(),
                address: "alice@example.com".into(),
                display_name: Some("Alice".into()),
                ordinal: 0,
            }],
        )
        .await
        .unwrap();
    repos
        .attachments
        .insert(
            stored.message_id(),
            NewAttachment {
                filename: Some("report.pdf".into()),
                content_type: "application/pdf".into(),
                size_bytes: 128,
                storage_path: "ab/cd/report.pdf".into(),
                content_id: None,
                is_inline: false,
                checksum_sha256: Some("deadbeef".into()),
            },
        )
        .await
        .unwrap();

    let copy = repos
        .messages
        .copy_to_folder(stored.message_id(), archive.folder_id(), f.mailbox_id)
        .await
        .unwrap();

    assert_ne!(copy.id, stored.id);
    assert_eq!(copy.uid, 1);
    assert_eq!(copy.folder_id, archive.id);
    assert_eq!(copy.rfc_message_id, stored.rfc_message_id);
    assert_eq!(copy.size_bytes, stored.size_bytes);
    assert_eq!(copy.storage_path, stored.storage_path);
    assert_eq!(copy.flags, stored.flags);

    // The source is untouched.
    assert_eq!(repos.messages.count_by_folder(f.inbox_id).await.unwrap(), 1);
    assert_eq!(
        repos
            .messages
            .require_by_id(stored.message_id())
            .await
            .unwrap()
            .folder_id,
        f.inbox_id.get()
    );

    // Recipients and attachment rows follow the copy; the blob path is shared.
    assert_eq!(repos.messages.recipients(copy.message_id()).await.unwrap().len(), 1);
    let attachments = repos.attachments.list_by_message(copy.message_id()).await.unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].storage_path, "ab/cd/report.pdf");
    assert_eq!(
        repos.attachments.referenced_paths().await.unwrap(),
        vec!["ab/cd/report.pdf".to_string()]
    );

    let err = repos
        .messages
        .copy_to_folder(MessageId::new(9999), archive.folder_id(), f.mailbox_id)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_set_storage_path_and_snippet() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "later", 0).await;

    repos
        .messages
        .set_storage_path(message.message_id(), "example.com/alice/Maildir/cur/moved:2,")
        .await
        .unwrap();
    repos
        .messages
        .set_snippet(message.message_id(), Some("a new preview"))
        .await
        .unwrap();

    let updated = repos.messages.require_by_id(message.message_id()).await.unwrap();
    assert_eq!(updated.storage_path, "example.com/alice/Maildir/cur/moved:2,");
    assert_eq!(updated.snippet.as_deref(), Some("a new preview"));

    repos.messages.set_snippet(message.message_id(), None).await.unwrap();
    assert!(repos
        .messages
        .require_by_id(message.message_id())
        .await
        .unwrap()
        .snippet
        .is_none());

    let err = repos
        .messages
        .set_storage_path(MessageId::new(9999), "x")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn message_search_filters_and_pages() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    let sent = repos.folders.require_by_name(f.mailbox_id, "Sent").await.unwrap();

    let invoice = seed_message(&t, &f, "Invoice 100% due", 0).await;
    let mut flagged = message_in(&f, f.inbox_id, "Flagged note", 10);
    flagged.flags = "flagged".into();
    flagged.has_attachments = true;
    flagged.attachment_count = 1;
    let flagged = repos.messages.insert(flagged).await.unwrap();
    let mut in_sent = message_in(&f, sent.folder_id(), "Sent item", 20);
    in_sent.sender = Some("alice@example.com".into());
    let in_sent = repos.messages.insert(in_sent).await.unwrap();

    let base = || MessageSearch {
        folder_id: None,
        mailbox_id: None,
        subject: None,
        sender: None,
        text: None,
        unread_only: false,
        flagged_only: false,
        with_attachments_only: false,
        since: None,
        before: None,
        limit: 100,
        offset: 0,
    };

    // Folder scoping.
    let inbox_only = repos
        .messages
        .search(MessageSearch {
            folder_id: Some(f.inbox_id),
            ..base()
        })
        .await
        .unwrap();
    assert_eq!(inbox_only.len(), 2);
    assert!(inbox_only.iter().all(|m| m.folder_id == f.inbox_id.get()));

    // Subject substring, with the `%` escaped rather than treated as a wildcard.
    let literal = repos
        .messages
        .search(MessageSearch {
            subject: Some("100%".into()),
            ..base()
        })
        .await
        .unwrap();
    assert_eq!(literal.len(), 1);
    assert_eq!(literal[0].id, invoice.id);

    let wildcard_is_literal = repos
        .messages
        .search(MessageSearch {
            subject: Some("%".into()),
            ..base()
        })
        .await
        .unwrap();
    assert_eq!(wildcard_is_literal.len(), 1);

    // Sender and free text.
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                sender: Some("alice@example.com".into()),
                ..base()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                text: Some("preview".into()),
                ..base()
            })
            .await
            .unwrap()
            .len(),
        3
    );

    // Flags and attachments.
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                unread_only: true,
                ..base()
            })
            .await
            .unwrap()
            .len(),
        3
    );
    repos.messages.mark_seen(invoice.message_id(), true).await.unwrap();
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                unread_only: true,
                ..base()
            })
            .await
            .unwrap()
            .len(),
        2
    );
    let flagged_hits = repos
        .messages
        .search(MessageSearch {
            flagged_only: true,
            ..base()
        })
        .await
        .unwrap();
    assert_eq!(flagged_hits.len(), 1);
    assert_eq!(flagged_hits[0].id, flagged.id);
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                with_attachments_only: true,
                ..base()
            })
            .await
            .unwrap()
            .len(),
        1
    );

    // Dates.
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                since: Some(at(15)),
                ..base()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                before: Some(at(15)),
                ..base()
            })
            .await
            .unwrap()
            .len(),
        2
    );

    // Paging and mailbox scoping.
    let first_page = repos
        .messages
        .search(MessageSearch {
            limit: 2,
            ..base()
        })
        .await
        .unwrap();
    assert_eq!(first_page.len(), 2);
    let second_page = repos
        .messages
        .search(MessageSearch {
            limit: 2,
            offset: 2,
            ..base()
        })
        .await
        .unwrap();
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0].id, invoice.id);
    assert_eq!(
        repos
            .messages
            .search(MessageSearch {
                mailbox_id: Some(f.mailbox_id),
                ..base()
            })
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(in_sent.uid, 1);

    t.cleanup().await;
}

#[tokio::test]
async fn message_recipients_round_trip() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "recipients", 0).await;

    assert!(repos.messages.recipients(message.message_id()).await.unwrap().is_empty());
    // An empty insert is a no-op, not an error.
    repos.messages.insert_recipients(message.message_id(), &[]).await.unwrap();

    repos
        .messages
        .insert_recipients(
            message.message_id(),
            &[
                Recipient {
                    kind: "to".into(),
                    address: "bob@example.net".into(),
                    display_name: Some("Bob".into()),
                    ordinal: 0,
                },
                Recipient {
                    kind: "to".into(),
                    address: "dave@example.net".into(),
                    display_name: None,
                    ordinal: 1,
                },
                Recipient {
                    kind: "cc".into(),
                    address: "carol@example.net".into(),
                    display_name: None,
                    ordinal: 0,
                },
                Recipient {
                    kind: "reply-to".into(),
                    address: "bob@example.net".into(),
                    display_name: None,
                    ordinal: 0,
                },
            ],
        )
        .await
        .unwrap();

    // `Recipient::ordinal` is the position *within its kind*, and `recipients()`
    // orders by `(ordinal, id)` — so the first entry of every kind comes first, and
    // the second `to` follows them. Callers that render "To:" and "Cc:" separately
    // group by `kind`.
    let recipients = repos.messages.recipients(message.message_id()).await.unwrap();
    assert_eq!(recipients.len(), 4);
    assert_eq!(
        recipients
            .iter()
            .map(|recipient| (recipient.kind.as_str(), recipient.ordinal))
            .collect::<Vec<_>>(),
        vec![("to", 0), ("cc", 0), ("reply-to", 0), ("to", 1)]
    );
    assert_eq!(recipients[0].address, "bob@example.net");
    assert_eq!(recipients[1].address, "carol@example.net");

    // Within a kind, the ordinals decide.
    let to: Vec<&str> = recipients
        .iter()
        .filter(|recipient| recipient.kind == "to")
        .map(|recipient| recipient.address.as_str())
        .collect();
    assert_eq!(to, vec!["bob@example.net", "dave@example.net"]);

    // A list whose ordinals are all zero keeps insertion order.
    repos.messages.delete_recipients(message.message_id()).await.unwrap();
    repos
        .messages
        .insert_recipients(
            message.message_id(),
            &[
                Recipient {
                    kind: "cc".into(),
                    address: "carol@example.net".into(),
                    display_name: None,
                    ordinal: 0,
                },
                Recipient {
                    kind: "to".into(),
                    address: "dave@example.net".into(),
                    display_name: None,
                    ordinal: 0,
                },
            ],
        )
        .await
        .unwrap();
    let by_insertion = repos.messages.recipients(message.message_id()).await.unwrap();
    assert_eq!(
        by_insertion
            .iter()
            .map(|recipient| recipient.address.as_str())
            .collect::<Vec<_>>(),
        vec!["carol@example.net", "dave@example.net"]
    );

    repos.messages.delete_recipients(message.message_id()).await.unwrap();
    assert!(repos.messages.recipients(message.message_id()).await.unwrap().is_empty());
    // Deleting nothing is not an error either.
    repos.messages.delete_recipients(message.message_id()).await.unwrap();

    t.cleanup().await;
}

// ===========================================================================
// attachments
// ===========================================================================

/// Insert one attachment on a fresh message.
async fn seed_attachment(
    t: &TestDatabase,
    f: &Fixture,
    path: &str,
    size: i64,
) -> (MessageId, ferroma_core::AttachmentId) {
    let message = seed_message(t, f, path, 0).await;
    let attachment = t
        .repos()
        .attachments
        .insert(
            message.message_id(),
            NewAttachment {
                filename: Some("file.bin".into()),
                content_type: "application/octet-stream".into(),
                size_bytes: size,
                storage_path: path.into(),
                content_id: Some("cid-1".into()),
                is_inline: false,
                checksum_sha256: None,
            },
        )
        .await
        .unwrap();
    (message.message_id(), attachment.attachment_id())
}

#[tokio::test]
async fn attachment_insert_find_and_list() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();
    let message = seed_message(&t, &f, "with-attachment", 0).await;

    let first = repos
        .attachments
        .insert(
            message.message_id(),
            NewAttachment {
                filename: Some("a.txt".into()),
                content_type: "text/plain".into(),
                size_bytes: 10,
                storage_path: "aa/a.txt".into(),
                content_id: None,
                is_inline: false,
                checksum_sha256: Some("aa".into()),
            },
        )
        .await
        .unwrap();
    repos
        .attachments
        .insert(
            message.message_id(),
            NewAttachment {
                filename: None,
                content_type: "image/png".into(),
                size_bytes: 20,
                storage_path: "bb/b.png".into(),
                content_id: Some("logo".into()),
                is_inline: true,
                checksum_sha256: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(first.message_id, message.id);
    assert_eq!(first.filename.as_deref(), Some("a.txt"));

    let found = repos
        .attachments
        .find_by_id(first.attachment_id())
        .await
        .unwrap()
        .expect("attachment by id");
    assert_eq!(found.id, first.id);
    assert!(repos
        .attachments
        .find_by_id(ferroma_core::AttachmentId::new(9999))
        .await
        .unwrap()
        .is_none());

    let listed = repos.attachments.list_by_message(message.message_id()).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[1].is_inline);
    assert_eq!(listed[1].content_id.as_deref(), Some("logo"));

    let err = repos
        .attachments
        .insert(
            MessageId::new(9999),
            NewAttachment {
                filename: None,
                content_type: "text/plain".into(),
                size_bytes: 1,
                storage_path: "x".into(),
                content_id: None,
                is_inline: false,
                checksum_sha256: None,
            },
        )
        .await
        .unwrap_err();
    assert!(err.is_foreign_key_violation(), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn attachment_delete_referenced_paths_and_total_size() {
    let t = setup!();
    let f = fixture(&t).await;
    let repos = t.repos();

    assert!(repos.attachments.referenced_paths().await.unwrap().is_empty());
    assert_eq!(repos.attachments.total_size().await.unwrap(), 0);

    let (_, first) = seed_attachment(&t, &f, "aa/shared.bin", 100).await;
    let (_, second) = seed_attachment(&t, &f, "bb/other.bin", 250).await;
    // A second row pointing at the same blob: content-addressed storage.
    let (_, third) = seed_attachment(&t, &f, "aa/shared.bin", 100).await;

    assert_eq!(repos.attachments.total_size().await.unwrap(), 450);
    assert_eq!(
        repos.attachments.referenced_paths().await.unwrap(),
        vec!["aa/shared.bin".to_string(), "bb/other.bin".to_string()]
    );

    assert!(repos.attachments.delete(first).await.unwrap());
    assert!(!repos.attachments.delete(first).await.unwrap());
    // The blob is still referenced by the third row.
    assert_eq!(
        repos.attachments.referenced_paths().await.unwrap(),
        vec!["aa/shared.bin".to_string(), "bb/other.bin".to_string()]
    );

    assert!(repos.attachments.delete(third).await.unwrap());
    assert_eq!(
        repos.attachments.referenced_paths().await.unwrap(),
        vec!["bb/other.bin".to_string()]
    );
    assert!(repos.attachments.delete(second).await.unwrap());
    assert!(repos.attachments.referenced_paths().await.unwrap().is_empty());

    t.cleanup().await;
}
