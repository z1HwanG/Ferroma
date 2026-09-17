//! Offline work: the `pending_operations` queue and its flusher
//! (specification §27 and §55).
//!
//! Two rules drive this module:
//!
//! 1. **Nothing the user typed may be lost to a network error.** An operation is
//!    written to SQLite *before* any network call, and removed only after the
//!    server accepted it.
//! 2. **Order is part of the meaning.** `mark_read` after `move`, `move` after
//!    `archive` — the flusher walks the queue in `seq` order and stops at the
//!    first retryable failure, so a later operation can never overtake an
//!    earlier one.
//!
//! The `operation_id` is generated when the operation is *queued*, never at send
//! time (`docs/fcp.md` §5): a crash between "we sent it" and "we recorded that we
//! sent it" therefore retries with the same key, and the server's idempotency
//! table turns the replay into a no-op instead of a second effect.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use ferroma_core::OperationId;
use serde_json::Value;
use sqlx::Row;

use crate::api::{FcpClient, SendRequest};
use crate::database::ClientDatabase;
use crate::error::{BoxFuture, ClientError, ClientResult};
use crate::util::{now_rfc3339, parse_rfc3339};

/// The kinds of work the offline queue can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    /// `POST /messages/:id/read`
    MarkRead,
    /// `POST /messages/:id/unread`
    MarkUnread,
    /// `POST /messages/:id/star`
    Star,
    /// `POST /messages/:id/archive`
    Archive,
    /// `POST /messages/:id/trash`
    Trash,
    /// `POST /messages/:id/move` with `folder_id` in the payload.
    Move,
    /// `DELETE /messages/:id`
    Delete,
    /// `POST /messages` — payload is a serialised [`SendRequest`].
    SendMessage,
    /// `POST`/`PATCH /drafts` — payload carries the draft id and body.
    SaveDraft,
    /// `DELETE /drafts/:id`
    DeleteDraft,
}

impl OperationKind {
    /// The string stored in `pending_operations.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            OperationKind::MarkRead => "mark_read",
            OperationKind::MarkUnread => "mark_unread",
            OperationKind::Star => "star",
            OperationKind::Archive => "archive",
            OperationKind::Trash => "trash",
            OperationKind::Move => "move",
            OperationKind::Delete => "delete",
            OperationKind::SendMessage => "send_message",
            OperationKind::SaveDraft => "save_draft",
            OperationKind::DeleteDraft => "delete_draft",
        }
    }

    /// Parse a `kind` column back into an enum.
    pub fn parse(raw: &str) -> ClientResult<Self> {
        Ok(match raw {
            "mark_read" => OperationKind::MarkRead,
            "mark_unread" => OperationKind::MarkUnread,
            "star" => OperationKind::Star,
            "archive" => OperationKind::Archive,
            "trash" => OperationKind::Trash,
            "move" => OperationKind::Move,
            "delete" => OperationKind::Delete,
            "send_message" => OperationKind::SendMessage,
            "save_draft" => OperationKind::SaveDraft,
            "delete_draft" => OperationKind::DeleteDraft,
            other => {
                return Err(ClientError::cache(format!(
                    "unknown queued operation kind {other:?}"
                )))
            }
        })
    }

    /// Whether this kind touches a message (and therefore carries a
    /// `message_id` in its payload).
    pub fn is_message_operation(self) -> bool {
        !matches!(
            self,
            OperationKind::SendMessage | OperationKind::SaveDraft | OperationKind::DeleteDraft
        )
    }
}

/// One row of `pending_operations`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingOperation {
    /// The FIFO order — the flusher must never reorder these.
    pub seq: i64,
    /// The idempotency key, generated at enqueue time.
    pub operation_id: String,
    /// The account it belongs to.
    pub account_id: i64,
    /// What to do.
    pub kind: OperationKind,
    /// The arguments.
    pub payload: Value,
    /// When it was queued.
    pub created_at: DateTime<Utc>,
    /// How many times the flusher has tried.
    pub attempts: u32,
    /// The last failure, for the UI.
    pub last_error: Option<String>,
}

impl PendingOperation {
    /// The `message_id` in the payload, when the kind carries one.
    pub fn message_id(&self) -> Option<i64> {
        self.payload.get("message_id").and_then(Value::as_i64)
    }

    /// The `folder_id` in the payload, when the kind carries one.
    pub fn folder_id(&self) -> Option<i64> {
        self.payload.get("folder_id").and_then(Value::as_i64)
    }

    /// The idempotency key as the core type.
    pub fn operation_key(&self) -> OperationId {
        OperationId::new(self.operation_id.clone())
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> ClientResult<Self> {
        let kind: String = row.try_get("kind")?;
        let payload: String = row.try_get("payload")?;
        let created_at: String = row.try_get("created_at")?;
        Ok(PendingOperation {
            seq: row.try_get("seq")?,
            operation_id: row.try_get("operation_id")?,
            account_id: row.try_get("account_id")?,
            kind: OperationKind::parse(&kind)?,
            payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            created_at: parse_rfc3339(&created_at).unwrap_or_else(Utc::now),
            attempts: row.try_get::<i64, _>("attempts")?.max(0) as u32,
            last_error: row.try_get("last_error")?,
        })
    }
}

/// The durable FIFO of operations the server has not accepted yet.
#[derive(Debug, Clone)]
pub struct PendingQueue {
    db: Arc<ClientDatabase>,
}

impl PendingQueue {
    /// Wrap a cache.
    pub fn new(db: Arc<ClientDatabase>) -> Self {
        PendingQueue { db }
    }

    /// The cache behind the queue.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// Queue an operation, generating its idempotency key **now**.
    ///
    /// The write is durable before this future resolves, so a crash immediately
    /// afterwards still leaves the operation queued exactly once.
    pub async fn enqueue(
        &self,
        account_id: i64,
        kind: OperationKind,
        payload: Value,
    ) -> ClientResult<PendingOperation> {
        self.enqueue_with_id(account_id, kind, payload, OperationId::generate())
            .await
    }

    /// Queue an operation with a caller-supplied idempotency key.
    ///
    /// This is what makes "the user pressed the button twice" and "we retried
    /// after a timeout" the same operation rather than two.
    pub async fn enqueue_with_id(
        &self,
        account_id: i64,
        kind: OperationKind,
        payload: Value,
        operation_id: OperationId,
    ) -> ClientResult<PendingOperation> {
        let created_at = now_rfc3339();
        let encoded = serde_json::to_string(&payload)?;
        let result = sqlx::query(
            "INSERT INTO pending_operations (operation_id, account_id, kind, payload, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(operation_id.as_str())
        .bind(account_id)
        .bind(kind.as_str())
        .bind(&encoded)
        .bind(&created_at)
        .execute(self.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Already queued (the caller replayed its own key): return the
            // existing row rather than duplicating the work.
            return self
                .find(operation_id.as_str())
                .await?
                .ok_or_else(|| ClientError::cache("queued operation vanished"));
        }

        self.find(operation_id.as_str())
            .await?
            .ok_or_else(|| ClientError::cache("queued operation vanished"))
    }

    /// Every queued operation for an account, oldest first.
    pub async fn list(&self, account_id: i64) -> ClientResult<Vec<PendingOperation>> {
        let rows = sqlx::query(
            "SELECT seq, operation_id, account_id, kind, payload, created_at, attempts, last_error
             FROM pending_operations WHERE account_id = ? ORDER BY seq ASC",
        )
        .bind(account_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(PendingOperation::from_row).collect()
    }

    /// Every queued operation for every account, oldest first per account.
    pub async fn list_all(&self) -> ClientResult<Vec<PendingOperation>> {
        let rows = sqlx::query(
            "SELECT seq, operation_id, account_id, kind, payload, created_at, attempts, last_error
             FROM pending_operations ORDER BY seq ASC",
        )
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(PendingOperation::from_row).collect()
    }

    /// How many operations are waiting for an account.
    pub async fn len(&self, account_id: i64) -> ClientResult<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM pending_operations WHERE account_id = ?")
            .bind(account_id)
            .fetch_one(self.db.pool())
            .await?;
        Ok(row.get("n"))
    }

    /// Whether the queue is empty for an account.
    pub async fn is_empty(&self, account_id: i64) -> ClientResult<bool> {
        Ok(self.len(account_id).await? == 0)
    }

    /// One operation by its idempotency key.
    pub async fn find(&self, operation_id: &str) -> ClientResult<Option<PendingOperation>> {
        let row = sqlx::query(
            "SELECT seq, operation_id, account_id, kind, payload, created_at, attempts, last_error
             FROM pending_operations WHERE operation_id = ?",
        )
        .bind(operation_id)
        .fetch_optional(self.db.pool())
        .await?;
        match row {
            Some(row) => Ok(Some(PendingOperation::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Record a failed attempt without losing the operation.
    pub async fn record_attempt(
        &self,
        operation_id: &str,
        error: Option<&str>,
    ) -> ClientResult<()> {
        sqlx::query(
            "UPDATE pending_operations SET attempts = attempts + 1, last_error = ?
             WHERE operation_id = ?",
        )
        .bind(error)
        .bind(operation_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Drop an operation the server has accepted (or that can never succeed).
    pub async fn remove(&self, operation_id: &str) -> ClientResult<bool> {
        let result = sqlx::query("DELETE FROM pending_operations WHERE operation_id = ?")
            .bind(operation_id)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Forget everything queued for an account (removing the account).
    pub async fn clear_account(&self, account_id: i64) -> ClientResult<u64> {
        let result = sqlx::query("DELETE FROM pending_operations WHERE account_id = ?")
            .bind(account_id)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

/// Something that can push one queued operation to the server.
///
/// The trait exists so the flusher's ordering and durability rules can be tested
/// without a server, and so a future IMAP fallback can be swapped in.
pub trait OperationSubmitter: Send + Sync + 'static {
    /// Submit `operation`. `Ok(())` means the server accepted it; an error is
    /// classified by [`ClientError::is_retryable`].
    fn submit<'a>(&'a self, operation: &'a PendingOperation) -> BoxFuture<'a, ClientResult<()>>;
}

/// The real submitter: it drives [`FcpClient`].
#[derive(Debug, Clone)]
pub struct FcpSubmitter {
    client: FcpClient,
}

impl FcpSubmitter {
    /// Wrap a client.
    pub fn new(client: FcpClient) -> Self {
        FcpSubmitter { client }
    }

    async fn submit_inner(&self, operation: &PendingOperation) -> ClientResult<()> {
        use crate::api::MessageAction;
        let key = operation.operation_key();
        match operation.kind {
            OperationKind::MarkRead => {
                let id = require_message_id(operation)?;
                self.client.message_action(id, MessageAction::Read, &key).await
            }
            OperationKind::MarkUnread => {
                let id = require_message_id(operation)?;
                self.client
                    .message_action(id, MessageAction::Unread, &key)
                    .await
            }
            OperationKind::Star => {
                let id = require_message_id(operation)?;
                self.client.message_action(id, MessageAction::Star, &key).await
            }
            OperationKind::Archive => {
                let id = require_message_id(operation)?;
                self.client
                    .message_action(id, MessageAction::Archive, &key)
                    .await
            }
            OperationKind::Trash => {
                let id = require_message_id(operation)?;
                self.client.message_action(id, MessageAction::Trash, &key).await
            }
            OperationKind::Move => {
                let id = require_message_id(operation)?;
                let folder = operation.folder_id().ok_or_else(|| {
                    ClientError::invalid("a move operation needs a folder_id in its payload")
                })?;
                self.client.move_message(id, folder, &key).await
            }
            OperationKind::Delete => {
                let id = require_message_id(operation)?;
                let permanent = operation
                    .payload
                    .get("permanent")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match self.client.delete_message(id, permanent).await {
                    Ok(()) => Ok(()),
                    // `DELETE` has no documented idempotency key (`docs/api.md`
                    // §1.5 covers `POST` and `operation_id` bodies only), so a
                    // replayed delete can come back as `404`. The user's intent —
                    // "this message is gone" — is satisfied either way.
                    Err(ClientError::Api(api)) if api.status == 404 => Ok(()),
                    Err(other) => Err(other),
                }
            }
            OperationKind::SendMessage => {
                let mut request: SendRequest = serde_json::from_value(operation.payload.clone())
                    .map_err(|e| {
                        ClientError::invalid(format!("a queued send has an unusable payload: {e}"))
                    })?;
                // The key belongs to the queued operation, not to whatever the
                // payload happened to carry.
                request.operation_id = operation.operation_id.clone();
                self.client.send(&request).await.map(|_| ())
            }
            OperationKind::SaveDraft => {
                let draft: crate::api::DraftPayload =
                    serde_json::from_value(operation.payload.clone()).map_err(|e| {
                        ClientError::invalid(format!("a queued draft has an unusable payload: {e}"))
                    })?;
                match operation.payload.get("draft_id").and_then(Value::as_i64) {
                    Some(id) if id > 0 => self.client.update_draft(id, &draft).await.map(|_| ()),
                    _ => self.client.create_draft(&draft).await.map(|_| ()),
                }
            }
            OperationKind::DeleteDraft => {
                let id = operation
                    .payload
                    .get("draft_id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| {
                        ClientError::invalid("a draft deletion needs a draft_id in its payload")
                    })?;
                self.client.delete_draft(id).await
            }
        }
    }
}

fn require_message_id(operation: &PendingOperation) -> ClientResult<i64> {
    operation.message_id().ok_or_else(|| {
        ClientError::invalid(format!(
            "a {} operation needs a message_id in its payload",
            operation.kind.as_str()
        ))
    })
}

impl OperationSubmitter for FcpSubmitter {
    fn submit<'a>(&'a self, operation: &'a PendingOperation) -> BoxFuture<'a, ClientResult<()>> {
        Box::pin(self.submit_inner(operation))
    }
}

/// The outcome of one flush pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlushReport {
    /// The ids of the operations the server accepted (and that were removed).
    pub applied: Vec<String>,
    /// The operations that failed permanently: id and message.
    pub failed: Vec<(String, String)>,
    /// The operation that stopped the pass, if any.
    pub blocked_on: Option<String>,
    /// Whether the block was a retryable failure (the queue is intact and will
    /// be retried later).
    pub retryable: bool,
    /// How many operations are still queued for the account.
    pub remaining: usize,
}

impl FlushReport {
    /// Whether everything queued was accepted.
    pub fn is_complete(&self) -> bool {
        self.blocked_on.is_none() && self.remaining == 0
    }
}

/// Walks the queue and submits operations in order.
#[derive(Clone)]
pub struct OperationFlusher {
    queue: PendingQueue,
    submitter: Arc<dyn OperationSubmitter>,
}

impl std::fmt::Debug for OperationFlusher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationFlusher")
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}

impl OperationFlusher {
    /// Build a flusher over `db`, driven by `submitter`.
    pub fn new(db: Arc<ClientDatabase>, submitter: Arc<dyn OperationSubmitter>) -> Self {
        OperationFlusher {
            queue: PendingQueue::new(db),
            submitter,
        }
    }

    /// The queue this flusher drains.
    pub fn queue(&self) -> &PendingQueue {
        &self.queue
    }

    /// Submit everything queued for `account_id`, in order.
    ///
    /// * a success is removed from the queue immediately (durably);
    /// * a permanent failure is recorded in the report and removed — retrying it
    ///   would only repeat the rejection;
    /// * a retryable failure keeps the entry, increments `attempts`, and **stops
    ///   the pass** so ordering is preserved.
    pub async fn flush(&self, account_id: i64) -> ClientResult<FlushReport> {
        let mut report = FlushReport::default();
        let operations = self.queue.list(account_id).await?;
        for operation in operations {
            match self.submitter.submit(&operation).await {
                Ok(()) => {
                    self.queue.remove(&operation.operation_id).await?;
                    report.applied.push(operation.operation_id.clone());
                }
                Err(err) if err.is_retryable() => {
                    self.queue
                        .record_attempt(&operation.operation_id, Some(&err.user_message()))
                        .await?;
                    report.blocked_on = Some(operation.operation_id.clone());
                    report.retryable = true;
                    break;
                }
                Err(err) => {
                    self.queue
                        .record_attempt(&operation.operation_id, Some(&err.user_message()))
                        .await?;
                    // Permanent: surface it once, then drop it. Keeping it would
                    // block the queue forever.
                    self.queue.remove(&operation.operation_id).await?;
                    report
                        .failed
                        .push((operation.operation_id.clone(), err.user_message()));
                }
            }
        }
        report.remaining = self.queue.len(account_id).await?.max(0) as usize;
        Ok(report)
    }
}

/// A submitter that answers from a script, for tests and for the CLI's dry run.
#[derive(Debug, Default)]
pub struct ScriptedSubmitter {
    outcomes: Mutex<VecDeque<ClientResult<()>>>,
    seen: Mutex<Vec<String>>,
}

impl ScriptedSubmitter {
    /// An empty script: every submission succeeds.
    pub fn new() -> Self {
        ScriptedSubmitter::default()
    }

    /// Append an outcome. Outcomes are consumed in order; when the script runs
    /// out, submissions succeed.
    pub fn push(&self, outcome: ClientResult<()>) {
        if let Ok(mut guard) = self.outcomes.lock() {
            guard.push_back(outcome);
        }
    }

    /// How many submissions were attempted, and with which ids.
    pub fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl OperationSubmitter for ScriptedSubmitter {
    fn submit<'a>(&'a self, operation: &'a PendingOperation) -> BoxFuture<'a, ClientResult<()>> {
        if let Ok(mut guard) = self.seen.lock() {
            guard.push(operation.operation_id.clone());
        }
        let outcome = self
            .outcomes
            .lock()
            .ok()
            .and_then(|mut guard| guard.pop_front());
        Box::pin(async move { outcome.unwrap_or(Ok(())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockServer, TempDir};
    use serde_json::json;

    async fn db_with_account() -> (TempDir, Arc<ClientDatabase>) {
        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(
            ClientDatabase::open(dir.path().join("cache.db"))
                .await
                .expect("open"),
        );
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (1, 'alice@example.com', 'http://x/api/v1/client', 'dev-1', 'now', 'now')",
        )
        .execute(db.pool())
        .await
        .expect("account");
        (dir, db)
    }

    #[test]
    fn operation_kinds_round_trip_through_their_strings() {
        for kind in [
            OperationKind::MarkRead,
            OperationKind::MarkUnread,
            OperationKind::Star,
            OperationKind::Archive,
            OperationKind::Trash,
            OperationKind::Move,
            OperationKind::Delete,
            OperationKind::SendMessage,
            OperationKind::SaveDraft,
            OperationKind::DeleteDraft,
        ] {
            assert_eq!(OperationKind::parse(kind.as_str()).expect("parse"), kind);
        }
        assert!(OperationKind::parse("nonsense").is_err());
        assert!(OperationKind::MarkRead.is_message_operation());
        assert!(!OperationKind::SendMessage.is_message_operation());
    }

    #[tokio::test]
    async fn queue_then_flush_then_empty() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        queue
            .enqueue(1, OperationKind::MarkRead, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        queue
            .enqueue(1, OperationKind::Star, json!({"message_id": 6}))
            .await
            .expect("enqueue");
        assert_eq!(queue.len(1).await.expect("len"), 2);

        let submitter = Arc::new(ScriptedSubmitter::new());
        let flusher = OperationFlusher::new(queue.database().clone(), submitter.clone());
        let report = flusher.flush(1).await.expect("flush");
        assert_eq!(report.applied.len(), 2);
        assert!(report.is_complete());
        assert!(queue.is_empty(1).await.expect("empty"));
        assert_eq!(submitter.seen().len(), 2);
    }

    #[tokio::test]
    async fn the_operation_id_is_generated_at_enqueue_time() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        let op = queue
            .enqueue(1, OperationKind::MarkRead, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        assert!(op.operation_id.starts_with("op_"));
        // Re-enqueuing the same key must not duplicate the row.
        queue
            .enqueue_with_id(
                1,
                OperationKind::MarkRead,
                json!({"message_id": 5}),
                op.operation_key(),
            )
            .await
            .expect("re-enqueue");
        assert_eq!(queue.len(1).await.expect("len"), 1);
    }

    #[tokio::test]
    async fn the_queue_preserves_insertion_order() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        for id in [1i64, 2, 3] {
            queue
                .enqueue(1, OperationKind::MarkRead, json!({"message_id": id}))
                .await
                .expect("enqueue");
        }
        let listed = queue.list(1).await.expect("list");
        let ids: Vec<i64> = listed.iter().filter_map(PendingOperation::message_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(listed.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[tokio::test]
    async fn a_permanent_failure_is_surfaced_and_removed() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        queue
            .enqueue(1, OperationKind::MarkRead, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        let submitter = Arc::new(ScriptedSubmitter::new());
        submitter.push(Err(ClientError::Api(crate::api::ApiError::new(
            403,
            "forbidden",
            "not yours",
        ))));
        let flusher = OperationFlusher::new(queue.database().clone(), submitter);
        let report = flusher.flush(1).await.expect("flush");
        assert!(report.applied.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].1, "not yours");
        assert!(report.blocked_on.is_none(), "a permanent failure does not block");
        assert!(queue.is_empty(1).await.expect("empty"));
    }

    #[tokio::test]
    async fn a_retryable_failure_keeps_the_entry_and_stops_the_pass() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        let first = queue
            .enqueue(1, OperationKind::Move, json!({"message_id": 5, "folder_id": 9}))
            .await
            .expect("enqueue");
        let second = queue
            .enqueue(1, OperationKind::MarkRead, json!({"message_id": 5}))
            .await
            .expect("enqueue");

        let submitter = Arc::new(ScriptedSubmitter::new());
        submitter.push(Err(ClientError::Network("connection reset".into())));
        let flusher = OperationFlusher::new(queue.database().clone(), submitter.clone());
        let report = flusher.flush(1).await.expect("flush");
        assert_eq!(report.blocked_on.as_deref(), Some(first.operation_id.as_str()));
        assert!(report.retryable);
        assert_eq!(report.remaining, 2);
        assert_eq!(submitter.seen(), vec![first.operation_id.clone()], "order preserved");

        let listed = queue.list(1).await.expect("list");
        assert_eq!(listed[0].operation_id, first.operation_id);
        assert_eq!(listed[0].attempts, 1);
        assert!(listed[0].last_error.is_some());
        assert_eq!(listed[1].operation_id, second.operation_id);
    }

    #[tokio::test]
    async fn a_later_flush_drains_what_the_first_one_blocked_on() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        queue
            .enqueue(1, OperationKind::MarkRead, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        queue
            .enqueue(1, OperationKind::Star, json!({"message_id": 5}))
            .await
            .expect("enqueue");

        let submitter = Arc::new(ScriptedSubmitter::new());
        submitter.push(Err(ClientError::Timeout("slow".into())));
        let flusher = OperationFlusher::new(queue.database().clone(), submitter.clone());
        let first = flusher.flush(1).await.expect("first");
        assert_eq!(first.remaining, 2);
        let second = flusher.flush(1).await.expect("second");
        assert_eq!(second.applied.len(), 2);
        assert!(queue.is_empty(1).await.expect("empty"));
    }

    #[tokio::test]
    async fn a_crash_mid_flush_leaves_the_queue_intact() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        let mut ids = Vec::new();
        for id in [1i64, 2, 3] {
            let op = queue
                .enqueue(1, OperationKind::MarkRead, json!({"message_id": id}))
                .await
                .expect("enqueue");
            ids.push(op.operation_id);
        }

        // A submitter that succeeds twice and then hangs forever: aborting the
        // flush task simulates the process dying mid-pass.
        let submitter = Arc::new(HangingSubmitter::new(2));
        let flusher = OperationFlusher::new(queue.database().clone(), submitter.clone());
        let handle = tokio::spawn(async move { flusher.flush(1).await });
        submitter.wait_until_hanging().await;
        handle.abort();
        let _ = handle.await;

        let remaining = queue.list(1).await.expect("list");
        assert_eq!(remaining.len(), 1, "only the un-submitted operation is left");
        assert_eq!(remaining[0].operation_id, ids[2]);
        assert_eq!(remaining[0].attempts, 0, "a crash is not an attempt");
        assert!(
            queue.find(&ids[0]).await.expect("find").is_none(),
            "the first success was committed before the crash"
        );
        assert!(queue.find(&ids[1]).await.expect("find").is_none());
    }

    /// A submitter that succeeds `successes` times, then never resolves.
    struct HangingSubmitter {
        successes: usize,
        calls: Mutex<usize>,
        hanging: Arc<tokio::sync::Notify>,
        hanging_now: Mutex<bool>,
    }

    impl HangingSubmitter {
        fn new(successes: usize) -> Self {
            HangingSubmitter {
                successes,
                calls: Mutex::new(0),
                hanging: Arc::new(tokio::sync::Notify::new()),
                hanging_now: Mutex::new(false),
            }
        }

        async fn wait_until_hanging(&self) {
            while !*self.hanging_now.lock().expect("lock") {
                self.hanging.notified().await;
            }
        }
    }

    impl OperationSubmitter for HangingSubmitter {
        fn submit<'a>(
            &'a self,
            _operation: &'a PendingOperation,
        ) -> BoxFuture<'a, ClientResult<()>> {
            let call = {
                let mut calls = self.calls.lock().expect("lock");
                *calls += 1;
                *calls
            };
            if call > self.successes {
                if let Ok(mut guard) = self.hanging_now.lock() {
                    *guard = true;
                }
                // `notify_one` stores a permit, so the waiter cannot miss it.
                self.hanging.notify_one();
                return Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Ok(())
                });
            }
            Box::pin(async move { Ok(()) })
        }
    }

    #[tokio::test]
    async fn the_queue_survives_a_reopen() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("cache.db");
        let operation_id = {
            let db = Arc::new(ClientDatabase::open(&path).await.expect("open"));
            sqlx::query(
                "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
                 VALUES (1, 'a@b.c', 'http://x/api/v1/client', 'dev', 'now', 'now')",
            )
            .execute(db.pool())
            .await
            .expect("account");
            let queue = PendingQueue::new(db.clone());
            let op = queue
                .enqueue(1, OperationKind::Move, json!({"message_id": 5, "folder_id": 9}))
                .await
                .expect("enqueue");
            db.close().await;
            op.operation_id
        };

        let db = Arc::new(ClientDatabase::open(&path).await.expect("reopen"));
        let queue = PendingQueue::new(db);
        let listed = queue.list(1).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].operation_id, operation_id);
        assert_eq!(listed[0].kind, OperationKind::Move);
        assert_eq!(listed[0].folder_id(), Some(9));
    }

    #[tokio::test]
    async fn removing_an_account_clears_its_queue() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db.clone());
        queue
            .enqueue(1, OperationKind::Star, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        db.wipe_account(1).await.expect("wipe");
        assert_eq!(queue.len(1).await.expect("len"), 0);
    }

    #[tokio::test]
    async fn a_delete_that_already_happened_counts_as_done() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        queue
            .enqueue(1, OperationKind::Delete, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        let server = MockServer::start().await;
        // A replayed delete: the message is already gone.
        server.error_route("DELETE", "/api/v1/client/messages/5", 404, "not_found", "gone");
        let client = FcpClient::with_retry(
            server.fcp_base_url(),
            Arc::new(crate::api::MemoryTokenStore::new()),
            crate::api::RetryPolicy::none(),
        )
        .expect("client");
        client
            .set_access_token("t", std::time::Duration::from_secs(60))
            .await;
        let flusher = OperationFlusher::new(
            queue.database().clone(),
            Arc::new(FcpSubmitter::new(client)),
        );
        let report = flusher.flush(1).await.expect("flush");
        assert_eq!(report.applied.len(), 1);
        assert!(report.failed.is_empty(), "a 404 on delete is not a failure");
        assert!(queue.is_empty(1).await.expect("empty"));
    }

    #[tokio::test]
    async fn a_move_without_a_folder_is_a_permanent_failure() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        queue
            .enqueue(1, OperationKind::Move, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        let server = MockServer::start().await;
        server.json_route("POST", "/api/v1/client/messages/5/move", 200, "{}");
        let client = FcpClient::with_retry(
            server.fcp_base_url(),
            Arc::new(crate::api::MemoryTokenStore::new()),
            crate::api::RetryPolicy::none(),
        )
        .expect("client");
        client
            .set_access_token("t", std::time::Duration::from_secs(60))
            .await;
        let flusher = OperationFlusher::new(queue.database().clone(), Arc::new(FcpSubmitter::new(client)));
        let report = flusher.flush(1).await.expect("flush");
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].1.contains("folder_id"));
        assert_eq!(server.request_count(), 0, "a malformed payload never reaches the wire");
    }

    #[tokio::test]
    async fn the_fcp_submitter_drives_the_documented_endpoints() {
        let (_dir, db) = db_with_account().await;
        let queue = PendingQueue::new(db);
        queue
            .enqueue(1, OperationKind::MarkRead, json!({"message_id": 5}))
            .await
            .expect("enqueue");
        queue
            .enqueue(1, OperationKind::Archive, json!({"message_id": 6}))
            .await
            .expect("enqueue");
        queue
            .enqueue(1, OperationKind::Move, json!({"message_id": 7, "folder_id": 9}))
            .await
            .expect("enqueue");
        queue
            .enqueue(
                1,
                OperationKind::SendMessage,
                serde_json::to_value(SendRequest {
                    operation_id: "ignored".into(),
                    to: vec!["bob@example.net".into()],
                    subject: "Hi".into(),
                    text: Some("hello".into()),
                    ..SendRequest::default()
                })
                .expect("encode"),
            )
            .await
            .expect("enqueue");

        let server = MockServer::start().await;
        server.json_route("POST", "/api/v1/client/messages/5/read", 200, "{}");
        server.json_route("POST", "/api/v1/client/messages/6/archive", 200, "{}");
        server.json_route("POST", "/api/v1/client/messages/7/move", 200, "{}");
        server.json_route("POST", "/api/v1/client/messages", 200, r#"{"message_id":99,"queued":1}"#);

        let client = FcpClient::with_retry(
            server.fcp_base_url(),
            Arc::new(crate::api::MemoryTokenStore::new()),
            crate::api::RetryPolicy::none(),
        )
        .expect("client");
        client
            .set_access_token("t", std::time::Duration::from_secs(60))
            .await;

        let flusher = OperationFlusher::new(
            queue.database().clone(),
            Arc::new(FcpSubmitter::new(client)),
        );
        let report = flusher.flush(1).await.expect("flush");
        assert_eq!(report.applied.len(), 4);
        assert!(report.is_complete());

        let send = server.requests_for("/api/v1/client/messages").remove(0);
        assert_eq!(send.json()["to"][0], "bob@example.net");
        let operation_id = send.json()["operation_id"].as_str().unwrap_or_default().to_string();
        assert!(operation_id.starts_with("op_"), "the queued key is used verbatim");
    }
}
