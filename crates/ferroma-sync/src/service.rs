//! The sync service: cursors, change recording and idempotent operations.
//!
//! # Recording
//!
//! Every mutation anywhere in Ferroma that a client must learn about calls one of
//! the `record_*` helpers here. They are deliberately the *only* writer of the
//! `change_log`, so the wire vocabulary in [`crate::change`] and the stored rows can
//! never drift apart.
//!
//! # Serving
//!
//! [`SyncService::sync`] is the whole of `GET /api/v1/client/sync`. It never
//! recomputes mailbox state: it reads `change_log` rows after the client's cursor,
//! caps the page, and reports whether more are waiting.
//!
//! # Idempotency
//!
//! [`SyncService::with_operation`] is the mechanism behind the specification's §55
//! requirement that a retried client request must not execute twice. A client
//! generates an `operation_id` *before* it enqueues the work locally; if the
//! response is lost, the retry finds the recorded operation and gets the original
//! result back without the side effect happening again.

use std::future::Future;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use ferroma_core::{Cursor, FerromaError, MailboxId, MessageId, Result, UserId};
use ferroma_storage::models::{ChangeLogEntry, Message};
use ferroma_storage::repository::{NewChange, OperationOutcome};
use ferroma_storage::{Repositories, StorageError};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::change::{ChangeKind, SyncChange, SyncPage};

/// What a client asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRequest {
    /// Whose mailbox.
    pub user_id: UserId,
    /// Which address.
    pub mailbox_id: MailboxId,
    /// One folder, or `None` for account-level changes (the folder list, drafts).
    pub folder_id: Option<MailboxId>,
    /// The last cursor the client applied. [`Cursor::ZERO`] on a first sync.
    pub cursor: Cursor,
    /// Page size, capped by the service's configured maximum.
    pub limit: Option<usize>,
}

impl SyncRequest {
    /// A request for one folder.
    pub fn folder(user_id: UserId, mailbox_id: MailboxId, folder_id: MailboxId, cursor: Cursor) -> Self {
        SyncRequest {
            user_id,
            mailbox_id,
            folder_id: Some(folder_id),
            cursor,
            limit: None,
        }
    }

    /// A request for account-level changes.
    pub fn account(user_id: UserId, mailbox_id: MailboxId, cursor: Cursor) -> Self {
        SyncRequest {
            user_id,
            mailbox_id,
            folder_id: None,
            cursor,
            limit: None,
        }
    }
}

/// Records and serves mailbox changes.
#[derive(Debug, Clone)]
pub struct SyncService {
    repos: Repositories,
    /// Maximum changes per page (`client.sync_page_size`).
    page_size: usize,
    /// How long a tombstone is kept, in days (`client.tombstone_retention_days`).
    tombstone_days: u32,
}

impl SyncService {
    /// Build the service.
    pub fn new(repos: Repositories, page_size: usize, tombstone_days: u32) -> Self {
        SyncService {
            repos,
            page_size: page_size.max(1),
            tombstone_days,
        }
    }

    /// The repositories this service writes through.
    pub fn repositories(&self) -> &Repositories {
        &self.repos
    }

    /// The configured maximum page size.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    // -------------------------------------------------------------------------
    // Serving
    // -------------------------------------------------------------------------

    /// Serve one page of changes.
    ///
    /// Returns [`FerromaError::Conflict`] when the client's cursor is older than the
    /// oldest retained entry *and* the client has synced before, which is the signal
    /// to throw away its cache for this folder and sync from zero. A client sending
    /// `0` is doing a first sync and is never told its cursor is stale.
    pub async fn sync(&self, request: SyncRequest) -> Result<SyncPage> {
        let limit = request.limit.unwrap_or(self.page_size).clamp(1, self.page_size);
        let latest = self.latest_cursor(request.user_id).await?;

        if request.cursor > latest {
            // The server's log is behind the client's cursor. That happens after a
            // restore from a backup taken before the client's last sync.
            return Err(FerromaError::Conflict(format!(
                "cursor {} is ahead of the server's latest cursor {latest}; full resync required",
                request.cursor
            )));
        }

        if request.cursor.0 > 0 {
            if let Some(oldest) = self.oldest_cursor(request.user_id).await? {
                if request.cursor < Cursor(oldest.0.saturating_sub(1)) {
                    return Err(FerromaError::Conflict(
                        "cursor is older than the retained change history; full resync required"
                            .into(),
                    ));
                }
            }
        }

        // Fetch one more than the page so `has_more` is exact without a second query.
        let probe = limit as i64 + 1;
        let rows = match request.folder_id {
            Some(_) => {
                // Folder scoping also has to surface moves *into* the folder from
                // elsewhere, so the filter is by the account and the client walks
                // the whole stream per folder. Fetching by mailbox keeps ordering
                // intact; the client skips changes for folders it does not hold.
                self.repos
                    .change_log
                    .changes_since_in_mailbox(request.user_id, request.mailbox_id, request.cursor, probe)
                    .await
                    .map_err(map_storage)?
            }
            None => self
                .repos
                .change_log
                .changes_since(request.user_id, request.cursor, probe)
                .await
                .map_err(map_storage)?,
        };

        let has_more = rows.len() > limit;
        let rows: Vec<ChangeLogEntry> = rows.into_iter().take(limit).collect();

        let next_cursor = rows
            .last()
            .map(|r| Cursor(r.seq))
            .unwrap_or(request.cursor);

        let changes: Vec<SyncChange> = rows.iter().filter_map(SyncChange::from_row).collect();

        Ok(SyncPage {
            next_cursor,
            has_more,
            latest_cursor: latest,
            changes,
        })
    }

    /// The newest cursor the server holds for a user.
    pub async fn latest_cursor(&self, user_id: UserId) -> Result<Cursor> {
        let seq = self
            .repos
            .change_log
            .max_seq(user_id)
            .await
            .map_err(map_storage)?;
        Ok(Cursor(seq))
    }

    /// The oldest retained cursor for a user, if the log has anything at all.
    pub async fn oldest_cursor(&self, user_id: UserId) -> Result<Option<Cursor>> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT MIN(seq) FROM change_log WHERE user_id = $1")
                .bind(user_id.get())
                .fetch_optional(self.repos.pool())
                .await
                .map_err(|e| map_storage(StorageError::Database(e)))?;
        Ok(row.map(|(seq,)| Cursor(seq)))
    }

    /// The cursor a device last acknowledged for a folder, or [`Cursor::ZERO`].
    pub async fn device_cursor(
        &self,
        device_id: ferroma_core::DeviceId,
        mailbox_id: MailboxId,
        folder_id: Option<MailboxId>,
    ) -> Result<Cursor> {
        let value = self
            .repos
            .sync_states
            .get(device_id, mailbox_id, folder_id)
            .await
            .map_err(map_storage)?;
        Ok(Cursor(value))
    }

    /// Persist the cursor a device acknowledged. Called after a successful page.
    pub async fn set_device_cursor(
        &self,
        device_id: ferroma_core::DeviceId,
        mailbox_id: MailboxId,
        folder_id: Option<MailboxId>,
        cursor: Cursor,
    ) -> Result<()> {
        self.repos
            .sync_states
            .set(device_id, mailbox_id, folder_id, cursor.0)
            .await
            .map_err(map_storage)
    }

    /// Forget everything a device knows, so its next sync starts from zero.
    pub async fn reset_device(&self, device_id: ferroma_core::DeviceId) -> Result<u64> {
        self.repos
            .sync_states
            .reset_for_device(device_id)
            .await
            .map_err(map_storage)
    }

    // -------------------------------------------------------------------------
    // Recording
    // -------------------------------------------------------------------------

    /// Append a change to the log.
    pub async fn record(&self, change: NewChange) -> Result<ChangeLogEntry> {
        let entry = self
            .repos
            .change_log
            .append(change)
            .await
            .map_err(map_storage)?;
        tracing::debug!(
            seq = entry.seq,
            user_id = entry.user_id,
            kind = %entry.kind,
            message_id = ?entry.message_id,
            "change recorded"
        );
        Ok(entry)
    }

    /// A message arrived in (or was filed into) a folder.
    pub async fn record_message_created(&self, user_id: UserId, message: &Message) -> Result<ChangeLogEntry> {
        self.record(NewChange {
            user_id,
            mailbox_id: Some(MailboxId::new(message.mailbox_id)),
            folder_id: Some(MailboxId::new(message.folder_id)),
            message_id: Some(MessageId::new(message.id)),
            kind: ChangeKind::MessageCreated.as_str().to_string(),
            payload: serde_json::json!({
                "uid": message.uid,
                "flags": message.flags,
                "size_bytes": message.size_bytes,
                "subject": message.subject,
                "sender": message.sender,
                "has_attachments": message.has_attachments,
            }),
        })
        .await
    }

    /// A message's flags changed.
    pub async fn record_message_flags(&self, user_id: UserId, message: &Message) -> Result<ChangeLogEntry> {
        self.record(NewChange {
            user_id,
            mailbox_id: Some(MailboxId::new(message.mailbox_id)),
            folder_id: Some(MailboxId::new(message.folder_id)),
            message_id: Some(MessageId::new(message.id)),
            kind: ChangeKind::MessageUpdated.as_str().to_string(),
            payload: serde_json::json!({
                "uid": message.uid,
                "flags": message.flags,
            }),
        })
        .await
    }

    /// A message left a folder.
    ///
    /// `permanent` distinguishes "the row is gone" from "it moved to Trash" (or a
    /// folder the client may not hold). The tombstone outlives the row on purpose.
    pub async fn record_message_deleted(
        &self,
        user_id: UserId,
        mailbox_id: MailboxId,
        folder_id: MailboxId,
        message_id: MessageId,
        uid: i64,
        permanent: bool,
    ) -> Result<ChangeLogEntry> {
        self.record(NewChange {
            user_id,
            mailbox_id: Some(mailbox_id),
            folder_id: Some(folder_id),
            // Deliberately kept even for a hard delete: `change_log.message_id` has
            // no foreign key precisely so this survives.
            message_id: Some(message_id),
            kind: ChangeKind::MessageDeleted.as_str().to_string(),
            payload: serde_json::json!({ "uid": uid, "permanent": permanent }),
        })
        .await
    }

    /// A message moved between folders.
    pub async fn record_message_moved(
        &self,
        user_id: UserId,
        from_folder_id: MailboxId,
        message: &Message,
    ) -> Result<ChangeLogEntry> {
        self.record(NewChange {
            user_id,
            mailbox_id: Some(MailboxId::new(message.mailbox_id)),
            folder_id: Some(MailboxId::new(message.folder_id)),
            message_id: Some(MessageId::new(message.id)),
            kind: ChangeKind::MessageMoved.as_str().to_string(),
            payload: serde_json::json!({
                "uid": message.uid,
                "flags": message.flags,
                "from_folder_id": from_folder_id.get(),
                "to_folder_id": message.folder_id,
            }),
        })
        .await
    }

    /// A folder was created, renamed or its subscription changed.
    pub async fn record_folder_change(
        &self,
        user_id: UserId,
        mailbox_id: MailboxId,
        folder_id: MailboxId,
        name: &str,
        kind: ChangeKind,
    ) -> Result<ChangeLogEntry> {
        debug_assert!(kind.is_folder(), "record_folder_change needs a folder kind");
        self.record(NewChange {
            user_id,
            mailbox_id: Some(mailbox_id),
            folder_id: Some(folder_id),
            message_id: None,
            kind: kind.as_str().to_string(),
            payload: serde_json::json!({ "name": name }),
        })
        .await
    }

    /// A draft was created, changed or removed.
    pub async fn record_draft_change(
        &self,
        user_id: UserId,
        mailbox_id: Option<MailboxId>,
        draft_id: ferroma_core::DraftId,
        subject: Option<&str>,
        kind: ChangeKind,
    ) -> Result<ChangeLogEntry> {
        debug_assert!(kind.is_draft(), "record_draft_change needs a draft kind");
        self.record(NewChange {
            user_id,
            mailbox_id,
            folder_id: None,
            message_id: None,
            kind: kind.as_str().to_string(),
            payload: serde_json::json!({ "draft_id": draft_id.get(), "subject": subject }),
        })
        .await
    }

    // -------------------------------------------------------------------------
    // Retention
    // -------------------------------------------------------------------------

    /// Delete change-log entries older than the tombstone retention window.
    ///
    /// A client whose cursor predates the oldest surviving entry is told to resync;
    /// see [`SyncService::sync`].
    pub async fn prune(&self) -> Result<u64> {
        let cutoff = self.prune_cutoff(Utc::now());
        let removed = self
            .repos
            .change_log
            .prune_older_than(cutoff)
            .await
            .map_err(map_storage)?;
        if removed > 0 {
            tracing::info!(removed, cutoff = %cutoff, "pruned the change log");
        }
        Ok(removed)
    }

    /// The instant before which changes are pruned.
    pub fn prune_cutoff(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now - ChronoDuration::days(i64::from(self.tombstone_days))
    }

    /// Delete applied operations older than the tombstone window.
    pub async fn prune_operations(&self) -> Result<u64> {
        let cutoff = self.prune_cutoff(Utc::now());
        self.repos
            .operations
            .purge_older_than(cutoff)
            .await
            .map_err(map_storage)
    }

    // -------------------------------------------------------------------------
    // Idempotency
    // -------------------------------------------------------------------------

    /// Run `operation` at most once for a given `operation_id`.
    ///
    /// * First call: runs the closure, records its JSON result, returns it.
    /// * Repeat with the same id: returns the recorded result **without** running the
    ///   closure again.
    /// * Repeat while the first call is still running (or after a crash inside it):
    ///   [`FerromaError::Conflict`], so the client retries rather than assuming the
    ///   work happened.
    /// * If the first call failed, the recorded failure is replayed as an error.
    pub async fn with_operation<T, F, Fut>(
        &self,
        operation_id: &str,
        user_id: UserId,
        kind: &str,
        operation: F,
    ) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if operation_id.trim().is_empty() {
            return Err(FerromaError::Invalid("operation_id must not be empty".into()));
        }

        match self
            .repos
            .operations
            .begin(operation_id, Some(user_id), kind)
            .await
            .map_err(map_storage)?
        {
            OperationOutcome::Replay(recorded) => {
                let Some(result) = recorded.result else {
                    return Err(FerromaError::Conflict(format!(
                        "operation {operation_id} has not finished; retry later"
                    )));
                };
                if recorded.status == "failed" {
                    let code = result
                        .get("code")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("internal_error");
                    let message = result
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the operation failed");
                    tracing::debug!(operation_id, code, "replaying a recorded failure");
                    return Err(FerromaError::Conflict(format!("{code}: {message}")));
                }
                let value: T = serde_json::from_value(result).map_err(|e| {
                    FerromaError::Internal(format!(
                        "operation {operation_id} recorded a result this build cannot read: {e}"
                    ))
                })?;
                tracing::debug!(operation_id, kind, "replayed a recorded operation");
                Ok(value)
            }
            OperationOutcome::Fresh => match operation().await {
                Ok(value) => {
                    let encoded = serde_json::to_value(&value)?;
                    if let Err(e) = self.repos.operations.complete(operation_id, encoded).await {
                        // The work succeeded; failing to record it would make a retry
                        // redo it, so this is worth shouting about.
                        tracing::error!(operation_id, error = %e, "could not record a completed operation");
                    }
                    Ok(value)
                }
                Err(err) => {
                    let payload = serde_json::json!({
                        "code": err.code(),
                        "message": err.to_string(),
                    });
                    if let Err(e) = self.repos.operations.fail(operation_id, payload).await {
                        tracing::error!(operation_id, error = %e, "could not record a failed operation");
                    }
                    Err(err)
                }
            },
        }
    }

    /// The recorded state of an operation, for diagnostics.
    pub async fn operation(&self, operation_id: &str) -> Result<Option<ferroma_storage::models::Operation>> {
        self.repos
            .operations
            .find(operation_id)
            .await
            .map_err(map_storage)
    }
}

fn map_storage(err: StorageError) -> FerromaError {
    err.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A service over a pool that will never be connected to. Only useful for tests
    /// that exercise pure logic; `PgPool::connect_lazy` needs a Tokio context, which
    /// is why the callers below are `#[tokio::test]`.
    fn lazy_service(page_size: usize) -> SyncService {
        let pool = sqlx::PgPool::connect_lazy("postgres://ferroma@127.0.0.1:5433/none").unwrap();
        SyncService::new(Repositories::new(pool), page_size, 30)
    }

    #[tokio::test]
    async fn page_size_is_at_least_one() {
        // A zero page size would make `sync` return nothing forever.
        assert_eq!(lazy_service(0).page_size(), 1);
        assert_eq!(lazy_service(500).page_size(), 500);
    }

    #[tokio::test]
    async fn prune_cutoff_uses_the_tombstone_window() {
        let s = lazy_service(500);
        assert_eq!(s.page_size(), 500);
        let now = Utc.with_ymd_and_hms(2026, 9, 16, 12, 0, 0).unwrap();
        assert_eq!(s.prune_cutoff(now), now - ChronoDuration::days(30));
    }

    #[test]
    fn sync_request_constructors() {
        let folder = SyncRequest::folder(UserId::new(7), MailboxId::new(3), MailboxId::new(5), Cursor(9));
        assert_eq!(folder.folder_id, Some(MailboxId::new(5)));
        assert_eq!(folder.cursor, Cursor(9));
        assert_eq!(folder.limit, None);

        let account = SyncRequest::account(UserId::new(7), MailboxId::new(3), Cursor::ZERO);
        assert_eq!(account.folder_id, None);
        assert_eq!(account.cursor, Cursor::ZERO);
    }
}
