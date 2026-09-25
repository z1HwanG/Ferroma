//! Client synchronisation: cursors, the change journal, and the idempotency log.
//!
//! Three tables cooperate here:
//!
//! * `client_sync_states` remembers how far each device has read, per mailbox and
//!   optionally per folder. A missing row means "nothing read yet", i.e. cursor `0`.
//! * `change_log` is an append-only journal with a monotonic `seq`, which *is* the
//!   cursor clients send back. It is deliberately never rewritten: a client that has
//!   been offline for a week can still be caught up.
//! * `operations` makes a retried mutation safe (specification §55). `begin` claims
//!   an operation id; if it was already claimed the caller gets the cached result
//!   instead of executing the work twice.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use ferroma_core::{Cursor, DeviceId, MailboxId, MessageId, UserId};

use crate::error::{Result, StorageError};
use crate::models::{ChangeLogEntry, ClientSyncState, Operation};
use crate::repository::{limit_of, not_found};

/// Per-device sync cursors.
#[derive(Debug, Clone)]
pub struct SyncStatesRepository {
    pool: PgPool,
}

impl SyncStatesRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The cursor a device has reached, or `0` when it has never synced.
    ///
    /// `folder_id == None` asks for the account-level cursor (folder list, settings);
    /// a folder cursor is a separate row and the two never mix.
    pub async fn get(
        &self,
        device_id: DeviceId,
        mailbox_id: MailboxId,
        folder_id: Option<MailboxId>,
    ) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT cursor FROM client_sync_states
              WHERE device_id = $1 AND mailbox_id = $2
                AND COALESCE(folder_id, 0) = COALESCE($3::BIGINT, 0)",
        )
        .bind(device_id.get())
        .bind(mailbox_id.get())
        .bind(folder_id.map(MailboxId::get))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(cursor,)| cursor).unwrap_or(0))
    }

    /// Move a device's cursor.
    ///
    /// An upsert, because the unique index coalesces `folder_id` so that the
    /// account-level row cannot be duplicated.
    pub async fn set(
        &self,
        device_id: DeviceId,
        mailbox_id: MailboxId,
        folder_id: Option<MailboxId>,
        cursor: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO client_sync_states (device_id, mailbox_id, folder_id, cursor)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (device_id, mailbox_id, COALESCE(folder_id, 0))
             DO UPDATE SET cursor = EXCLUDED.cursor, updated_at = NOW()",
        )
        .bind(device_id.get())
        .bind(mailbox_id.get())
        .bind(folder_id.map(MailboxId::get))
        .bind(cursor)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every cursor a device holds.
    pub async fn list_for_device(&self, device_id: DeviceId) -> Result<Vec<ClientSyncState>> {
        Ok(sqlx::query_as::<_, ClientSyncState>(
            "SELECT * FROM client_sync_states WHERE device_id = $1
              ORDER BY mailbox_id ASC, folder_id ASC NULLS FIRST, id ASC",
        )
        .bind(device_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Forget every cursor of a device, so its next sync starts from scratch.
    ///
    /// Returns how many cursors were dropped; [`SyncStatesRepository::get`] answers
    /// `0` afterwards, which is the same thing as never having synced.
    pub async fn reset_for_device(&self, device_id: DeviceId) -> Result<u64> {
        let done = sqlx::query("DELETE FROM client_sync_states WHERE device_id = $1")
            .bind(device_id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// Forget the cursors pointing at one folder — called when a folder disappears.
    pub async fn delete_for_folder(&self, folder_id: MailboxId) -> Result<u64> {
        let done = sqlx::query("DELETE FROM client_sync_states WHERE folder_id = $1")
            .bind(folder_id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// The outcome of asking to begin an operation.
#[derive(Debug, Clone, PartialEq)]
pub enum OperationOutcome {
    /// The operation id was new; the caller owns the work and must `complete` or
    /// `fail` it.
    Fresh,
    /// The operation id was already recorded; the cached row is returned so the
    /// caller can replay the original response verbatim.
    Replay(Operation),
}

/// Client operation journal (idempotency).
#[derive(Debug, Clone)]
pub struct OperationsRepository {
    pool: PgPool,
}

impl OperationsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Claim `operation_id`.
    ///
    /// The claim is a single `INSERT ... ON CONFLICT DO NOTHING RETURNING *`, so it
    /// is atomic: of two concurrent retries of the same request exactly one sees
    /// [`OperationOutcome::Fresh`] and the other gets the recorded row back.
    pub async fn begin(
        &self,
        operation_id: &str,
        user_id: Option<UserId>,
        kind: &str,
    ) -> Result<OperationOutcome> {
        let operation_id = operation_id.trim();
        if operation_id.is_empty() {
            return Err(StorageError::Invalid("operation_id must not be blank".into()));
        }

        let claimed = sqlx::query_as::<_, Operation>(
            "INSERT INTO operations (operation_id, user_id, kind, status)
             VALUES ($1, $2, $3, 'applied')
             ON CONFLICT (operation_id) DO NOTHING
             RETURNING *",
        )
        .bind(operation_id)
        .bind(user_id.map(UserId::get))
        .bind(kind)
        .fetch_optional(&self.pool)
        .await?;

        if claimed.is_some() {
            return Ok(OperationOutcome::Fresh);
        }

        // Lost the race (or a genuine replay): the row must be there. A key is
        // scoped to both its owner and operation kind; never let a collision replay
        // another user's cached response or silently suppress a different mutation.
        for _ in 0..3 {
            if let Some(existing) = self.find(operation_id).await? {
                if existing.user_id != user_id.map(UserId::get) || existing.kind != kind {
                    return Err(StorageError::Conflict(
                        "operation_id is already used by another user or operation".into(),
                    ));
                }
                return Ok(OperationOutcome::Replay(existing));
            }
        }
        Err(StorageError::Conflict(format!(
            "operation {operation_id} was claimed but disappeared"
        )))
    }

    /// Store the response of a completed operation.
    pub async fn complete(&self, operation_id: &str, result: serde_json::Value) -> Result<()> {
        let done = sqlx::query(
            "UPDATE operations SET status = 'applied', result = $2, completed_at = NOW()
              WHERE operation_id = $1",
        )
        .bind(operation_id)
        .bind(result)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            return Err(not_found(format!("operation {operation_id}")));
        }
        Ok(())
    }

    /// Store the error of a failed operation.
    pub async fn fail(&self, operation_id: &str, error: serde_json::Value) -> Result<()> {
        let done = sqlx::query(
            "UPDATE operations SET status = 'failed', result = $2, completed_at = NOW()
              WHERE operation_id = $1",
        )
        .bind(operation_id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            return Err(not_found(format!("operation {operation_id}")));
        }
        Ok(())
    }

    /// Look one operation up.
    pub async fn find(&self, operation_id: &str) -> Result<Option<Operation>> {
        Ok(
            sqlx::query_as::<_, Operation>("SELECT * FROM operations WHERE operation_id = $1")
                .bind(operation_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Forget operations older than `cutoff`.
    ///
    /// A client only retries within its own timeout window, so anything much older
    /// than that is dead weight.
    pub async fn purge_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let done = sqlx::query("DELETE FROM operations WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// One change-log entry to append.
#[derive(Debug, Clone)]
pub struct NewChange {
    /// The account the change belongs to. Every change is addressed to exactly one.
    pub user_id: UserId,
    /// The address the change happened in, when there is one.
    pub mailbox_id: Option<MailboxId>,
    /// The folder the change happened in, when there is one.
    pub folder_id: Option<MailboxId>,
    /// The affected message. Not a foreign key: tombstones outlive rows.
    pub message_id: Option<MessageId>,
    /// `message_created`, `message_updated`, `folder_deleted`, …
    pub kind: String,
    /// Everything the client needs to apply the change without another round trip.
    pub payload: serde_json::Value,
}

/// The sync change log.
#[derive(Debug, Clone)]
pub struct ChangeLogRepository {
    pool: PgPool,
}

impl ChangeLogRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Append one change and return it, `seq` included.
    pub async fn append(&self, new: NewChange) -> Result<ChangeLogEntry> {
        Ok(sqlx::query_as::<_, ChangeLogEntry>(
            "INSERT INTO change_log (user_id, mailbox_id, folder_id, message_id, kind, payload)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING *",
        )
        .bind(new.user_id.get())
        .bind(new.mailbox_id.map(MailboxId::get))
        .bind(new.folder_id.map(MailboxId::get))
        .bind(new.message_id.map(MessageId::get))
        .bind(&new.kind)
        .bind(&new.payload)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Everything that happened to an account after `after`, oldest first.
    ///
    /// `after` is exclusive, so passing the `seq` of the last entry the client
    /// already has never repeats it. `limit` caps the page; a client that fills it
    /// simply asks again with the new last `seq`.
    pub async fn changes_since(
        &self,
        user_id: UserId,
        after: Cursor,
        limit: i64,
    ) -> Result<Vec<ChangeLogEntry>> {
        Ok(sqlx::query_as::<_, ChangeLogEntry>(
            "SELECT * FROM change_log
              WHERE user_id = $1 AND seq > $2
              ORDER BY seq ASC LIMIT $3",
        )
        .bind(user_id.get())
        .bind(after.0)
        .bind(limit_of(limit))
        .fetch_all(&self.pool)
        .await?)
    }

    /// The same, restricted to one address.
    pub async fn changes_since_in_mailbox(
        &self,
        user_id: UserId,
        mailbox_id: MailboxId,
        after: Cursor,
        limit: i64,
    ) -> Result<Vec<ChangeLogEntry>> {
        Ok(sqlx::query_as::<_, ChangeLogEntry>(
            "SELECT * FROM change_log
              WHERE user_id = $1 AND mailbox_id = $2 AND seq > $3
              ORDER BY seq ASC LIMIT $4",
        )
        .bind(user_id.get())
        .bind(mailbox_id.get())
        .bind(after.0)
        .bind(limit_of(limit))
        .fetch_all(&self.pool)
        .await?)
    }

    /// The highest `seq` an account has reached (`0` when it has none).
    pub async fn max_seq(&self, user_id: UserId) -> Result<i64> {
        let (max,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(seq), 0)::BIGINT FROM change_log WHERE user_id = $1",
        )
        .bind(user_id.get())
        .fetch_one(&self.pool)
        .await?;
        Ok(max)
    }

    /// Forget entries older than `cutoff`.
    ///
    /// Only safe when every device has synced past them; the retention sweep is
    /// responsible for knowing that.
    pub async fn prune_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let done = sqlx::query("DELETE FROM change_log WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}
