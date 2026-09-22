//! Repositories: one small type per aggregate, all sharing the same pool.
//!
//! ```no_run
//! # async fn demo(db: ferroma_storage::Database) -> ferroma_storage::Result<()> {
//! let repos = db.repositories();
//! let user = repos.users.find_by_email("alice@example.com").await?;
//! # Ok(())
//! # }
//! ```
//!
//! A repository never opens its own connection and never owns a transaction; callers
//! that need several statements to be atomic take a `&mut PgConnection` themselves
//! via [`Repositories::pool`].

mod audit;
mod contacts;
mod auth;
mod domains;
mod mailboxes;
mod messages;
mod misc;
mod queue;
mod sync;
mod users;

pub use audit::{AuditFilter, AuditRepository, NewAuditLog};
pub use contacts::ContactsRepository;
pub use auth::{DeviceUpsert, DevicesRepository, NewSession, SessionsRepository};
pub use domains::{AliasesRepository, DomainsRepository};
pub use mailboxes::{FoldersRepository, MailboxWithDomain, MailboxesRepository, NewMailbox};
pub use messages::{
    AttachmentsRepository, MessageSearch, MessagesRepository, NewAttachment, NewMessage, Recipient,
};
pub use misc::{
    DraftUpdate, DraftsRepository, LoginAttemptsRepository, NewDraft, SettingsRepository,
};
pub use queue::{
    DeliveryAttemptsRepository, NewDeliveryAttempt, NewQueueEntry, QueueRepository, QueueStats,
};
pub use sync::{
    ChangeLogRepository, NewChange, OperationOutcome, OperationsRepository, SyncStatesRepository,
};
pub use users::{NewUser, UsersRepository};

use sqlx::PgPool;

/// Every repository, bound to one pool.
#[derive(Debug, Clone)]
pub struct Repositories {
    pool: PgPool,
    /// Login identities.
    pub users: UsersRepository,
    /// Managed domains.
    pub domains: DomainsRepository,
    /// Forwarding aliases.
    pub aliases: AliasesRepository,
    /// Addresses (`alice@example.com`).
    pub mailboxes: MailboxesRepository,
    /// IMAP folders.
    pub folders: FoldersRepository,
    /// Stored messages.
    pub messages: MessagesRepository,
    /// Attachment rows.
    pub attachments: AttachmentsRepository,
    /// Outbound queue.
    pub queue: QueueRepository,
    /// Per-attempt delivery log.
    pub delivery_attempts: DeliveryAttemptsRepository,
    /// Web/API/client sessions.
    pub sessions: SessionsRepository,
    /// Official client installations.
    pub devices: DevicesRepository,
    /// Per-device sync cursors.
    pub sync_states: SyncStatesRepository,
    /// Client operation journal (idempotency).
    pub operations: OperationsRepository,
    /// The sync change log.
    pub change_log: ChangeLogRepository,
    /// Drafts.
    pub drafts: DraftsRepository,
    /// Admin/security audit trail.
    pub audit: AuditRepository,
    /// Login attempts, for throttling.
    pub login_attempts: LoginAttemptsRepository,
    /// DB-backed settings.
    pub settings: SettingsRepository,
    /// Addresses the account has sent to or received from.
    pub contacts: ContactsRepository,
}

impl Repositories {
    /// Build every repository over `pool`.
    pub fn new(pool: PgPool) -> Self {
        Repositories {
            users: UsersRepository::new(pool.clone()),
            domains: DomainsRepository::new(pool.clone()),
            aliases: AliasesRepository::new(pool.clone()),
            mailboxes: MailboxesRepository::new(pool.clone()),
            folders: FoldersRepository::new(pool.clone()),
            messages: MessagesRepository::new(pool.clone()),
            attachments: AttachmentsRepository::new(pool.clone()),
            queue: QueueRepository::new(pool.clone()),
            delivery_attempts: DeliveryAttemptsRepository::new(pool.clone()),
            sessions: SessionsRepository::new(pool.clone()),
            devices: DevicesRepository::new(pool.clone()),
            sync_states: SyncStatesRepository::new(pool.clone()),
            operations: OperationsRepository::new(pool.clone()),
            change_log: ChangeLogRepository::new(pool.clone()),
            drafts: DraftsRepository::new(pool.clone()),
            audit: AuditRepository::new(pool.clone()),
            login_attempts: LoginAttemptsRepository::new(pool.clone()),
            settings: SettingsRepository::new(pool.clone()),
            contacts: ContactsRepository::new(pool.clone()),
            pool,
        }
    }

    /// The shared pool, for multi-statement transactions and ad-hoc queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begin a transaction spanning several repositories.
    pub async fn begin(&self) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, crate::StorageError> {
        Ok(self.pool.begin().await?)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers.
//
// These are deliberately crate-private: they exist so that every repository
// normalises and validates the same way, not to widen the public API.
// ---------------------------------------------------------------------------

/// Lower-case and trim a value that the schema stores in its normalised form
/// (`users.email`, `domains.name`, `mailboxes.local_part`, `aliases.local_part`).
///
/// The schema enforces the same rule with a `CHECK (value = lower(value))`, so a
/// caller that forgets to normalise would otherwise get an opaque constraint
/// violation instead of a stored row.
pub(crate) fn normalise(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Turn a unique-constraint violation into [`crate::StorageError::Conflict`].
///
/// Every other error is passed through untouched, so callers keep the real cause.
pub(crate) fn unique_conflict(err: crate::StorageError, what: impl std::fmt::Display) -> crate::StorageError {
    if err.is_unique_violation() {
        crate::StorageError::Conflict(format!("{what} already exists"))
    } else {
        err
    }
}

/// A `LIMIT` PostgreSQL will accept. Negative limits become `0`.
pub(crate) fn limit_of(limit: i64) -> i64 {
    limit.max(0)
}

/// An `OFFSET` PostgreSQL will accept. Negative offsets become `0`.
pub(crate) fn offset_of(offset: i64) -> i64 {
    offset.max(0)
}

/// The `NotFound` error for `what`, e.g. `user 7`.
pub(crate) fn not_found(what: impl std::fmt::Display) -> crate::StorageError {
    crate::StorageError::NotFound(what.to_string())
}

/// Build a case-insensitive `LIKE` pattern with `%` and `_` in the needle escaped,
/// so a user searching for `50%` does not match everything.
pub(crate) fn like_pattern(needle: &str) -> String {
    let mut escaped = String::with_capacity(needle.len() + 2);
    escaped.push('%');
    for ch in needle.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('%');
    escaped
}
