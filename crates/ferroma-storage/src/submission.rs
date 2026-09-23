//! Atomic database metadata commit for an already-staged outbound Sent message.
//!
//! This does not stage or delete Maildir files and does not insert attachment rows.
//! Callers must clean up a staged body after an error; attachment-bearing submissions
//! must use a separate flow until attachment rows can share this transaction.

use crate::error::{Result, StorageError};
use crate::models::Message;
use crate::repository::{NewMessage, Recipient, Repositories};
use ferroma_core::UserId;

/// All database inputs for one staged Sent copy and its remote delivery recipients.
#[derive(Debug, Clone)]
pub struct Submission {
    /// Authenticated owner of the Sent mailbox and queue rows.
    pub user_id: UserId,
    /// Already-staged Sent message metadata, without attachment rows.
    pub message: NewMessage,
    /// Header recipients recorded with the Sent copy.
    pub recipients: Vec<Recipient>,
    /// SMTP envelope sender shared by every remote queue row.
    pub sender: String,
    /// SMTP envelope recipients, one pending queue entry per address.
    pub remote_recipients: Vec<String>,
    /// Maximum delivery attempts shared by every remote queue row.
    pub max_attempts: i32,
}

/// Commit one Sent message, header recipients, outbound queue, usage and creation log.
///
/// The owner and destination folder are locked before reading actual live message
/// totals. Both the account quota and an optional mailbox override are enforced;
/// cached `users.used_bytes` is repaired from live rows in the same transaction.
/// A failed recipient, journal or queue insert rolls everything back, including
/// folder counters and UID allocation. The caller owns the staged Maildir body and
/// must remove it on failure. Attachment rows are outside this API: submissions
/// with attachment metadata are rejected rather than committing an incomplete copy.
/// An empty remote recipient list is allowed (a local-only Sent copy).
pub async fn store_submission(repos: &Repositories, submission: &Submission) -> Result<Message> {
    let new = &submission.message;
    if new.size_bytes < 0 || new.attachment_count < 0 {
        return Err(StorageError::Invalid(
            "message size_bytes and attachment_count must be >= 0".into(),
        ));
    }
    if new.has_attachments || new.attachment_count != 0 {
        return Err(StorageError::Invalid(
            "submission does not support attachment rows".into(),
        ));
    }
    if submission.max_attempts < 0
        || (!submission.remote_recipients.is_empty()
            && (submission.sender.trim().is_empty()
                || submission
                    .remote_recipients
                    .iter()
                    .any(|address| address.trim().is_empty())))
    {
        return Err(StorageError::Invalid(
            "queue entry needs sender, recipient and valid attempt budget".into(),
        ));
    }

    let mut tx = repos.pool().begin().await?;
    let owner_limit: i64 =
        sqlx::query_scalar("SELECT quota_bytes FROM users WHERE id = $1 FOR UPDATE")
            .bind(submission.user_id.get())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StorageError::NotFound(format!("user {}", submission.user_id)))?;
    let address_limit: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT b.quota_bytes FROM mailboxes b JOIN folders f ON f.mailbox_id = b.id
          WHERE b.id = $1 AND b.user_id = $2 AND f.id = $3 FOR UPDATE OF f",
    )
    .bind(new.mailbox_id.get())
    .bind(submission.user_id.get())
    .bind(new.folder_id.get())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        StorageError::NotFound(format!(
            "folder {} in mailbox {} for user {}",
            new.folder_id, new.mailbox_id, submission.user_id
        ))
    })?;

    let account_used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(m.size_bytes), 0)::BIGINT FROM messages m
          WHERE m.expunged_at IS NULL AND m.mailbox_id IN
            (SELECT id FROM mailboxes WHERE user_id = $1)",
    )
    .bind(submission.user_id.get())
    .fetch_one(&mut *tx)
    .await?;
    let address_used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM messages
          WHERE expunged_at IS NULL AND mailbox_id = $1",
    )
    .bind(new.mailbox_id.get())
    .fetch_one(&mut *tx)
    .await?;
    for (used, limit) in [
        (account_used, Some(owner_limit)),
        (address_used, address_limit),
    ] {
        if let Some(limit) = limit {
            if new.size_bytes > 0
                && used
                    .checked_add(new.size_bytes)
                    .is_none_or(|total| total > limit)
            {
                return Err(StorageError::QuotaExceeded {
                    mailbox_id: new.mailbox_id.get(),
                    used,
                    needed: new.size_bytes,
                    limit,
                });
            }
        }
    }

    let message = repos
        .messages
        .insert_submission_tx(&mut tx, new, submission.user_id)
        .await?;
    for recipient in &submission.recipients {
        sqlx::query(
            "INSERT INTO message_recipients (message_id, kind, address, display_name, ordinal)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(message.id)
        .bind(&recipient.kind)
        .bind(&recipient.address)
        .bind(recipient.display_name.as_deref())
        .bind(recipient.ordinal)
        .execute(&mut *tx)
        .await?;
    }
    for recipient in &submission.remote_recipients {
        sqlx::query(
            "INSERT INTO mail_queue (message_id, user_id, sender, recipient, max_attempts,
                                     status, next_attempt_at)
             VALUES ($1, $2, $3, $4, $5, 'pending', NOW())",
        )
        .bind(message.id)
        .bind(submission.user_id.get())
        .bind(submission.sender.trim())
        .bind(recipient.trim())
        .bind(submission.max_attempts)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE users SET used_bytes = COALESCE((
            SELECT SUM(m.size_bytes) FROM messages m WHERE m.expunged_at IS NULL
              AND m.mailbox_id IN (SELECT id FROM mailboxes WHERE user_id = $1)
        ), 0), updated_at = NOW() WHERE id = $1",
    )
    .bind(submission.user_id.get())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(message)
}
