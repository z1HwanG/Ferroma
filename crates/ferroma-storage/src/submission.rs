//! Atomic database metadata commit for an already-staged outbound Sent message.
//!
//! This does not stage or delete Maildir files and does not insert attachment rows.
//! Callers must clean up a staged body after an error; attachment-bearing submissions
//! must use a separate flow until attachment rows can share this transaction.

use crate::error::{Result, StorageError};
use crate::models::Message;
use crate::repository::{NewMessage, Recipient, Repositories};
use ferroma_core::UserId;

/// One attachment row to write alongside a staged submission.
///
/// The bytes are already in the blob store; only the row that points at them is
/// written here, so it commits with the message that owns it.
#[derive(Debug, Clone)]
pub struct SubmissionAttachment {
    /// Original file name, when the part had one.
    pub filename: Option<String>,
    /// MIME type of the part.
    pub content_type: String,
    /// Size of the stored blob.
    pub size_bytes: i64,
    /// Path of the blob in the attachment store.
    pub storage_path: String,
    /// `Content-ID` for inline parts, without angle brackets.
    pub content_id: Option<String>,
    /// Whether the part is referenced from the HTML body.
    pub is_inline: bool,
    /// SHA-256 of the blob.
    pub checksum_sha256: Option<String>,
    /// An existing row to replace — the uploader's placeholder, which hangs from
    /// a hidden draft message until the real one exists. It is deleted in the
    /// same transaction, scoped to this submission's owner.
    pub replaces: Option<i64>,
}

/// All database inputs for one staged Sent copy and its remote delivery recipients.
#[derive(Debug, Clone)]
pub struct Submission {
    /// Authenticated owner of the Sent mailbox and queue rows.
    pub user_id: UserId,
    /// Already-staged Sent message metadata.
    pub message: NewMessage,
    /// Header recipients recorded with the Sent copy.
    pub recipients: Vec<Recipient>,
    /// Attachment rows to write, in the order the message lists them.
    pub attachments: Vec<SubmissionAttachment>,
    /// SMTP envelope sender shared by every remote queue row.
    pub sender: String,
    /// SMTP envelope recipients, one pending queue entry per address.
    pub remote_recipients: Vec<String>,
    /// Maximum delivery attempts shared by every remote queue row.
    pub max_attempts: i32,
}

impl Submission {
    /// A submission with no attachment rows.
    pub fn without_attachments(
        user_id: UserId,
        message: NewMessage,
        recipients: Vec<Recipient>,
        sender: String,
        remote_recipients: Vec<String>,
        max_attempts: i32,
    ) -> Self {
        Submission {
            user_id,
            message,
            recipients,
            attachments: Vec::new(),
            sender,
            remote_recipients,
            max_attempts,
        }
    }
}

/// Commit one Sent message, its header recipients, attachment rows, outbound
/// queue, usage and creation log in a single transaction.
///
/// The owner and destination folder are locked before reading actual live message
/// totals. Both the account quota and an optional mailbox override are enforced;
/// cached `users.used_bytes` is repaired from live rows in the same transaction.
/// A failed recipient, attachment, journal or queue insert rolls everything back,
/// including folder counters, UID allocation and the deleted placeholder rows.
/// The caller owns the staged Maildir body and blob files and must remove the
/// body on failure. `message.has_attachments` and `message.attachment_count` must
/// agree with `attachments`, so a row can never claim attachments it does not have.
pub async fn store_submission(repos: &Repositories, submission: &Submission) -> Result<Message> {
    let new = &submission.message;
    if new.size_bytes < 0 || new.attachment_count < 0 {
        return Err(StorageError::Invalid(
            "message size_bytes and attachment_count must be >= 0".into(),
        ));
    }
    if new.attachment_count != submission.attachments.len() as i32
        || new.has_attachments != !submission.attachments.is_empty()
    {
        return Err(StorageError::Invalid(
            "message attachment_count and has_attachments must match the submission's rows".into(),
        ));
    }
    if submission
        .attachments
        .iter()
        .any(|attachment| attachment.size_bytes < 0 || attachment.content_type.trim().is_empty())
    {
        return Err(StorageError::Invalid(
            "an attachment needs a content type and a non-negative size".into(),
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
    for attachment in &submission.attachments {
        // The uploader's placeholder hangs from a hidden draft and is owned by the
        // same user; replacing it here is what keeps the blob's only row pointing
        // at the message that actually goes out.
        if let Some(replaced) = attachment.replaces {
            sqlx::query(
                "DELETE FROM attachments a USING messages m, mailboxes b
                  WHERE a.id = $1 AND a.message_id = m.id AND m.mailbox_id = b.id
                    AND b.user_id = $2",
            )
            .bind(replaced)
            .bind(submission.user_id.get())
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO attachments (message_id, filename, content_type, size_bytes,
                                      storage_path, content_id, is_inline, checksum_sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(message.id)
        .bind(attachment.filename.as_deref())
        .bind(&attachment.content_type)
        .bind(attachment.size_bytes)
        .bind(&attachment.storage_path)
        .bind(attachment.content_id.as_deref())
        .bind(attachment.is_inline)
        .bind(attachment.checksum_sha256.as_deref())
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
