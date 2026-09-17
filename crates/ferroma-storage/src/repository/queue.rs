//! `mail_queue` and `delivery_attempts` — the outbound side of the platform.
//!
//! The queue is a work table, and the only way work leaves it is
//! [`QueueRepository::claim_due`]. That method is the crate's second race-critical
//! operation (after UID allocation): it must hand every row to exactly one worker,
//! even when a dozen dispatchers poll at the same instant. It does that with
//! `FOR UPDATE SKIP LOCKED` inside a transaction — a worker that finds a row locked
//! by a peer simply moves on to the next one instead of waiting.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use ferroma_core::{MessageId, QueueId, UserId};

use crate::error::{Result, StorageError};
use crate::models::{DeliveryAttempt, QueueEntry};
use crate::repository::{limit_of, not_found, offset_of};

/// Everything needed to queue one recipient.
#[derive(Debug, Clone)]
pub struct NewQueueEntry {
    /// The stored message whose bytes are to be sent.
    pub message_id: MessageId,
    /// Who queued it, when it was an authenticated user.
    pub user_id: Option<UserId>,
    /// Envelope sender.
    pub sender: String,
    /// Envelope recipient.
    pub recipient: String,
    /// How many attempts the dispatcher may make before bouncing.
    pub max_attempts: i32,
}

/// How many queue rows sit in each state — the Admin dashboard's delivery widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, sqlx::FromRow)]
pub struct QueueStats {
    /// Waiting for a first attempt.
    pub pending: i64,
    /// Claimed by a worker right now.
    pub delivering: i64,
    /// Waiting for a later attempt.
    pub retry: i64,
    /// Accepted by the remote server.
    pub delivered: i64,
    /// Given up on (bounced).
    pub failed: i64,
    /// Withdrawn by the sender before delivery.
    pub cancelled: i64,
}

impl QueueStats {
    /// Rows that are neither finished nor in flight.
    pub fn outstanding(&self) -> i64 {
        self.pending + self.retry + self.delivering
    }
}

/// The outbound mail queue.
#[derive(Debug, Clone)]
pub struct QueueRepository {
    pool: PgPool,
}

impl QueueRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Queue one recipient, due immediately.
    pub async fn enqueue(&self, new: NewQueueEntry) -> Result<QueueEntry> {
        if new.max_attempts < 0 {
            return Err(StorageError::Invalid(
                "queue max_attempts must be >= 0".into(),
            ));
        }
        if new.sender.trim().is_empty() || new.recipient.trim().is_empty() {
            return Err(StorageError::Invalid(
                "queue entry needs a sender and a recipient".into(),
            ));
        }

        Ok(sqlx::query_as::<_, QueueEntry>(
            "INSERT INTO mail_queue (message_id, user_id, sender, recipient, max_attempts,
                                     status, next_attempt_at)
             VALUES ($1, $2, $3, $4, $5, 'pending', NOW())
             RETURNING *",
        )
        .bind(new.message_id.get())
        .bind(new.user_id.map(UserId::get))
        .bind(new.sender.trim())
        .bind(new.recipient.trim())
        .bind(new.max_attempts)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Atomically claim up to `limit` due entries for this worker.
    ///
    /// Every claimed row becomes `delivering` with `attempts` incremented and
    /// `last_attempt_at` stamped, so a crash leaves an unambiguous trace that
    /// [`QueueRepository::requeue_stale`] can find. Two concurrent callers can never
    /// receive the same row: the inner `SELECT ... FOR UPDATE SKIP LOCKED` locks the
    /// candidates and the locks are held until the transaction commits.
    pub async fn claim_due(&self, limit: i64) -> Result<Vec<QueueEntry>> {
        let limit = limit_of(limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query_as::<_, QueueEntry>(
            "WITH due AS (
                 SELECT id FROM mail_queue
                  WHERE status IN ('pending', 'retry')
                    AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())
                  ORDER BY next_attempt_at ASC NULLS FIRST, id ASC
                  LIMIT $1
                  FOR UPDATE SKIP LOCKED
             )
             UPDATE mail_queue q
                SET status = 'delivering',
                    attempts = attempts + 1,
                    last_attempt_at = NOW(),
                    updated_at = NOW()
               FROM due
              WHERE q.id = due.id
             RETURNING q.*",
        )
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(claimed)
    }

    /// Look a queue entry up by id.
    pub async fn find_by_id(&self, id: QueueId) -> Result<Option<QueueEntry>> {
        Ok(sqlx::query_as::<_, QueueEntry>("SELECT * FROM mail_queue WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Every queue entry for one stored message (one per recipient).
    pub async fn list_by_message(&self, message_id: MessageId) -> Result<Vec<QueueEntry>> {
        Ok(sqlx::query_as::<_, QueueEntry>(
            "SELECT * FROM mail_queue WHERE message_id = $1 ORDER BY id ASC",
        )
        .bind(message_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// A page of one status, newest first.
    pub async fn list_by_status(&self, status: &str, limit: i64, offset: i64) -> Result<Vec<QueueEntry>> {
        Ok(sqlx::query_as::<_, QueueEntry>(
            "SELECT * FROM mail_queue WHERE status = $1
              ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3",
        )
        .bind(status)
        .bind(limit_of(limit))
        .bind(offset_of(offset))
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many entries sit in one status.
    pub async fn count_by_status(&self, status: &str) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mail_queue WHERE status = $1")
            .bind(status)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Every status count in one round trip.
    pub async fn stats(&self) -> Result<QueueStats> {
        Ok(sqlx::query_as::<_, QueueStats>(
            "SELECT COUNT(*) FILTER (WHERE status = 'pending')    AS pending,
                    COUNT(*) FILTER (WHERE status = 'delivering') AS delivering,
                    COUNT(*) FILTER (WHERE status = 'retry')      AS retry,
                    COUNT(*) FILTER (WHERE status = 'delivered')  AS delivered,
                    COUNT(*) FILTER (WHERE status = 'failed')     AS failed,
                    COUNT(*) FILTER (WHERE status = 'cancelled')  AS cancelled
               FROM mail_queue",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Mark an entry delivered.
    ///
    /// `None` for the remote details leaves whatever the last attempt recorded in
    /// place; the transient error of an earlier attempt is always cleared.
    pub async fn mark_delivered(
        &self,
        id: QueueId,
        remote_mx: Option<&str>,
        status_code: Option<i32>,
        status_text: Option<&str>,
    ) -> Result<()> {
        let done = sqlx::query(
            "UPDATE mail_queue
                SET status = 'delivered',
                    delivered_at = NOW(),
                    next_attempt_at = NULL,
                    last_error = NULL,
                    remote_mx = COALESCE($2::TEXT, remote_mx),
                    last_status_code = COALESCE($3::INTEGER, last_status_code),
                    last_status_text = COALESCE($4::TEXT, last_status_text),
                    updated_at = NOW()
              WHERE id = $1",
        )
        .bind(id.get())
        .bind(remote_mx)
        .bind(status_code)
        .bind(status_text)
        .execute(&self.pool)
        .await?;
        touched(done.rows_affected(), id)
    }

    /// Mark an entry for a later attempt.
    pub async fn mark_retry(
        &self,
        id: QueueId,
        next_attempt_at: DateTime<Utc>,
        error: &str,
        remote_mx: Option<&str>,
        status_code: Option<i32>,
        status_text: Option<&str>,
    ) -> Result<()> {
        let done = sqlx::query(
            "UPDATE mail_queue
                SET status = 'retry',
                    next_attempt_at = $2,
                    last_error = $3,
                    remote_mx = COALESCE($4::TEXT, remote_mx),
                    last_status_code = COALESCE($5::INTEGER, last_status_code),
                    last_status_text = COALESCE($6::TEXT, last_status_text),
                    updated_at = NOW()
              WHERE id = $1",
        )
        .bind(id.get())
        .bind(next_attempt_at)
        .bind(error)
        .bind(remote_mx)
        .bind(status_code)
        .bind(status_text)
        .execute(&self.pool)
        .await?;
        touched(done.rows_affected(), id)
    }

    /// Give up on an entry: the message is bounced and never attempted again.
    pub async fn mark_failed(
        &self,
        id: QueueId,
        error: &str,
        remote_mx: Option<&str>,
        status_code: Option<i32>,
        status_text: Option<&str>,
    ) -> Result<()> {
        let done = sqlx::query(
            "UPDATE mail_queue
                SET status = 'failed',
                    next_attempt_at = NULL,
                    last_error = $2,
                    remote_mx = COALESCE($3::TEXT, remote_mx),
                    last_status_code = COALESCE($4::INTEGER, last_status_code),
                    last_status_text = COALESCE($5::TEXT, last_status_text),
                    updated_at = NOW()
              WHERE id = $1",
        )
        .bind(id.get())
        .bind(error)
        .bind(remote_mx)
        .bind(status_code)
        .bind(status_text)
        .execute(&self.pool)
        .await?;
        touched(done.rows_affected(), id)
    }

    /// Withdraw an entry that has not been delivered yet.
    ///
    /// Returns `false` when the entry is missing or already in a final state.
    pub async fn cancel(&self, id: QueueId) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE mail_queue
                SET status = 'cancelled', next_attempt_at = NULL, updated_at = NOW()
              WHERE id = $1 AND status IN ('pending', 'retry', 'delivering')",
        )
        .bind(id.get())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Return entries a crashed worker left in `delivering` to the queue.
    ///
    /// Only rows whose last attempt started before `older_than` are touched, so a
    /// worker that is merely slow is not robbed of its work.
    pub async fn requeue_stale(&self, older_than: DateTime<Utc>) -> Result<usize> {
        let done = sqlx::query(
            "UPDATE mail_queue
                SET status = 'retry',
                    next_attempt_at = NOW(),
                    last_error = COALESCE(last_error, 'requeued after worker restart'),
                    updated_at = NOW()
              WHERE status = 'delivering'
                AND last_attempt_at IS NOT NULL
                AND last_attempt_at < $1",
        )
        .bind(older_than)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() as usize)
    }

    /// How many entries this user has queued since `since` — the daily send limit.
    ///
    /// One row is one recipient, so a message addressed to three people counts three
    /// times; that is the figure an SMTP rate limiter wants.
    pub async fn count_sent_since(&self, user_id: UserId, since: DateTime<Utc>) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mail_queue WHERE user_id = $1 AND created_at >= $2",
        )
        .bind(user_id.get())
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// When the dispatcher next has work to do, if any.
    pub async fn next_due_at(&self) -> Result<Option<DateTime<Utc>>> {
        let (next,): (Option<DateTime<Utc>>,) = sqlx::query_as(
            "SELECT MIN(next_attempt_at) FROM mail_queue WHERE status IN ('pending', 'retry')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(next)
    }
}

/// Turn "the `UPDATE` matched no row" into [`StorageError::NotFound`].
fn touched(rows: u64, id: QueueId) -> Result<()> {
    if rows == 0 {
        return Err(not_found(format!("queue entry {id}")));
    }
    Ok(())
}

/// Everything needed to record one delivery attempt.
#[derive(Debug, Clone)]
pub struct NewDeliveryAttempt {
    /// The queue entry the attempt belongs to.
    pub queue_id: QueueId,
    /// Attempt number, matching `mail_queue.attempts`.
    pub attempt: i32,
    /// The MX host that was tried.
    pub remote_mx: Option<String>,
    /// The SMTP reply code.
    pub status_code: Option<i32>,
    /// The SMTP reply text.
    pub status_text: Option<String>,
    /// The transport error, when the attempt never got a reply.
    pub error: Option<String>,
    /// How long the attempt took.
    pub duration_ms: Option<i32>,
}

/// Per-attempt delivery log.
#[derive(Debug, Clone)]
pub struct DeliveryAttemptsRepository {
    pool: PgPool,
}

impl DeliveryAttemptsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Append one attempt.
    pub async fn record(&self, new: NewDeliveryAttempt) -> Result<DeliveryAttempt> {
        Ok(sqlx::query_as::<_, DeliveryAttempt>(
            "INSERT INTO delivery_attempts (queue_id, attempt, remote_mx, status_code,
                                             status_text, error, duration_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING *",
        )
        .bind(new.queue_id.get())
        .bind(new.attempt)
        .bind(new.remote_mx.as_deref())
        .bind(new.status_code)
        .bind(new.status_text.as_deref())
        .bind(new.error.as_deref())
        .bind(new.duration_ms)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The attempt history of one queue entry, oldest first.
    pub async fn list_by_queue(&self, queue_id: QueueId) -> Result<Vec<DeliveryAttempt>> {
        Ok(sqlx::query_as::<_, DeliveryAttempt>(
            "SELECT * FROM delivery_attempts WHERE queue_id = $1 ORDER BY attempt ASC, id ASC",
        )
        .bind(queue_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Drop the history of one queue entry, returning how many rows went.
    pub async fn delete_for_queue(&self, queue_id: QueueId) -> Result<u64> {
        let done = sqlx::query("DELETE FROM delivery_attempts WHERE queue_id = $1")
            .bind(queue_id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}
