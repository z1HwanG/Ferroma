//! `messages`, `message_recipients` and `attachments`.
//!
//! This module is where the two stores meet: a row here is the database's promise
//! that `storage_path` holds the RFC 5322 bytes. Every method that removes a row
//! returns it (or a copy of it) so the caller can delete the file afterwards — this
//! repository never touches the filesystem itself.
//!
//! # Flags
//!
//! IMAP flags live in one space-separated, lower-case column (`messages.flags`,
//! e.g. `seen flagged $label1`). [`MessagesRepository::add_flags`] and
//! [`MessagesRepository::remove_flags`] treat it as a *set*: adding a flag that is
//! already present changes nothing, and the union keeps the order it was first
//! written in. Both run inside a transaction with `SELECT ... FOR UPDATE`, so two
//! concurrent `STORE` commands cannot lose each other's flag.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};

use ferroma_core::{AttachmentId, MailboxId, MessageId, UserId};

use crate::error::{Result, StorageError};
use crate::models::{AttachmentRow, Message, MessageRecipient};
use crate::repository::{like_pattern, limit_of, not_found, offset_of};

/// Everything needed to store a message row.
#[derive(Debug, Clone)]
pub struct NewMessage {
    /// The folder the message lands in. Its `uid_next` supplies the IMAP UID.
    pub folder_id: MailboxId,
    /// The owning address (denormalised from the folder).
    pub mailbox_id: MailboxId,
    /// RFC 5322 `Message-ID` header, if the sender supplied one.
    pub rfc_message_id: Option<String>,
    /// Root of the `References` chain, for conversation grouping.
    pub thread_id: Option<String>,
    /// Decoded `Subject` header.
    pub subject: Option<String>,
    /// Envelope `From` address.
    pub sender: Option<String>,
    /// Display name that accompanied `sender`.
    pub sender_name: Option<String>,
    /// Short plain-text preview for list views.
    pub snippet: Option<String>,
    /// Size of the stored bytes.
    pub size_bytes: i64,
    /// Path relative to the Maildir root.
    pub storage_path: String,
    /// SHA-256 of the stored bytes, for integrity checks.
    pub checksum_sha256: Option<String>,
    /// Space-separated IMAP flags.
    pub flags: String,
    /// IMAP `INTERNALDATE`. `None` means "now".
    pub internal_date: Option<DateTime<Utc>>,
    /// The `Date:` header.
    pub sent_at: Option<DateTime<Utc>>,
    /// Whether the message carries attachments.
    pub has_attachments: bool,
    /// How many attachment parts it carries.
    pub attachment_count: i32,
    /// Whether this is a draft rather than a received/sent message.
    pub is_draft: bool,
}

/// A staged local delivery to one explicitly identified owner and address.
///
/// The caller owns the staged Maildir file: database rollback does not remove it.
#[derive(Debug, Clone)]
pub struct BatchMessage {
    /// Owner of the target address; checked against `mailboxes.user_id`.
    pub user_id: UserId,
    /// Metadata and already-staged body path for this delivery.
    pub message: NewMessage,
    /// Header recipients to commit alongside this message.
    pub recipients: Vec<Recipient>,
}

/// One `To`/`Cc`/`Bcc`/`Reply-To` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    /// `to`, `cc`, `bcc`, `reply-to` or `sender`.
    pub kind: String,
    /// The address.
    pub address: String,
    /// The display name that accompanied the address.
    pub display_name: Option<String>,
    /// Position within its `kind`, for stable rendering.
    pub ordinal: i32,
}

/// Everything needed to store an attachment row.
#[derive(Debug, Clone)]
pub struct NewAttachment {
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
}

/// A server-side SEARCH (specification §30).
///
/// Every field is optional and they combine with `AND`; `limit`/`offset` page the
/// result. At least one of `folder_id`/`mailbox_id` should be set by callers that are
/// answering a protocol command, otherwise the search spans every account.
#[derive(Debug, Clone)]
pub struct MessageSearch {
    /// Restrict to one folder.
    pub folder_id: Option<MailboxId>,
    /// Restrict to one address.
    pub mailbox_id: Option<MailboxId>,
    /// Substring of the subject, case-insensitive.
    pub subject: Option<String>,
    /// Substring of the sender address or name, case-insensitive.
    pub sender: Option<String>,
    /// Substring of subject, sender or snippet, case-insensitive.
    pub text: Option<String>,
    /// Only messages without the `seen` flag.
    pub unread_only: bool,
    /// Only messages with the `flagged` flag.
    pub flagged_only: bool,
    /// Only messages that carry attachments.
    pub with_attachments_only: bool,
    /// Only messages with `internal_date >= since`.
    pub since: Option<DateTime<Utc>>,
    /// Only messages with `internal_date < before`.
    pub before: Option<DateTime<Utc>>,
    /// Maximum number of rows.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

/// Stored messages.
#[derive(Debug, Clone)]
pub struct MessagesRepository {
    pool: PgPool,
}

impl MessagesRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Insert a message, allocate its IMAP UID, and keep the folder's counters true.
    ///
    /// The `UPDATE ... RETURNING` on `folders.uid_next` takes the folder's row lock, so
    /// two simultaneous deliveries into the same folder receive different UIDs however
    /// they interleave. A missing folder is a [`StorageError::NotFound`].
    ///
    /// The counters move in the *same* statement. They used to be the caller's job —
    /// delivery remembered to `recount`, IMAP `APPEND`/`COPY` and the draft mirroring did
    /// not — and the result was a Drafts folder whose row said `0` while five messages sat
    /// in it, because nothing had ever told the row otherwise. A rule that every caller has
    /// to remember is a rule that will be forgotten; this one cannot be.
    pub async fn insert(&self, new: NewMessage) -> Result<Message> {
        self.insert_logged(new, None).await
    }

    /// Insert an authenticated message with quota, usage and creation cursor
    /// committed in the same transaction. `None` preserves repository-only
    /// insertion behavior for callers that account and journal separately.
    pub async fn insert_logged(&self, new: NewMessage, user: Option<UserId>) -> Result<Message> {
        if new.size_bytes < 0 {
            return Err(StorageError::Invalid(
                "message size_bytes must be >= 0".into(),
            ));
        }
        if new.attachment_count < 0 {
            return Err(StorageError::Invalid(
                "message attachment_count must be >= 0".into(),
            ));
        }

        // A message counts as unseen until its flag string says otherwise — the same test
        // `recount` applies, so the incremental update and the full recomputation agree.
        let unseen = i32::from(!flag_is_set(&new.flags, "seen"));

        // Two statements, one row each, in one transaction.
        //
        // The first attempt at this put both updates in a single statement as two
        // data-modifying CTEs — and PostgreSQL silently applied only one of them, because
        // both wrote the *same* `folders` row and a row may be updated once per statement.
        // The counters looked maintained and were not. Here the folder row is updated
        // exactly once, carrying the UID allocation and the counters together, and the row
        // lock that takes is still what serialises two deliveries into the same folder.
        let mut tx = self.pool.begin().await?;
        if let Some(user) = user {
            let quota: Option<(i64,)> = sqlx::query_as(
                "SELECT COALESCE(b.quota_bytes, u.quota_bytes) FROM mailboxes b
                   JOIN users u ON u.id = b.user_id
                  WHERE b.id = $1 AND u.id = $2 FOR UPDATE OF u",
            ).bind(new.mailbox_id.get()).bind(user.get())
                .fetch_optional(&mut *tx).await?;
            let (limit,) = quota.ok_or_else(|| not_found(format!("mailbox {}", new.mailbox_id)))?;
            let (used,): (i64,) = sqlx::query_as(
                "SELECT COALESCE(SUM(m.size_bytes), 0)::BIGINT FROM messages m
                  WHERE m.expunged_at IS NULL AND m.mailbox_id IN
                    (SELECT id FROM mailboxes WHERE user_id = $1)",
            ).bind(user.get()).fetch_one(&mut *tx).await?;
            if new.size_bytes > 0 && used.checked_add(new.size_bytes).is_none_or(|total| total > limit) {
                return Err(StorageError::QuotaExceeded {
                    mailbox_id: new.mailbox_id.get(), used, needed: new.size_bytes, limit,
                });
            }
        }

        let message = insert_message_tx(&mut tx, &new, unseen).await?;

        if let Some(user) = user {
            sqlx::query(
                "UPDATE users SET used_bytes = COALESCE((
                    SELECT SUM(m.size_bytes) FROM messages m WHERE m.expunged_at IS NULL
                      AND m.mailbox_id IN (SELECT id FROM mailboxes WHERE user_id = $1)
                ), 0), updated_at = NOW() WHERE id = $1",
            ).bind(user.get()).execute(&mut *tx).await?;
            append_relocation_change(&mut tx, user, &message, None).await?;
        }
        tx.commit().await?;

        Ok(message)
    }

    /// Atomically store a batch of staged local deliveries and their header recipients.
    ///
    /// Locks distinct owners by ascending user id before checking actual live message
    /// totals (not cached `used_bytes`), including the whole batch's pending bytes per
    /// account and address. A mailbox override additionally caps that address; the
    /// owner's quota always caps the account. Each folder must belong to the stated
    /// address and each address to the stated owner. Results preserve input order.
    /// An empty batch is a no-op. On any failure no database rows, counters, usage
    /// or change-log entries survive; the caller must clean up staged body files.
    pub async fn insert_batch_logged(&self, batch: &[BatchMessage]) -> Result<Vec<Message>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        for item in batch {
            if item.message.size_bytes < 0 {
                return Err(StorageError::Invalid("message size_bytes must be >= 0".into()));
            }
            if item.message.attachment_count < 0 {
                return Err(StorageError::Invalid("message attachment_count must be >= 0".into()));
            }
        }

        let mut tx = self.pool.begin().await?;
        let mut owners = BTreeMap::new();
        for item in batch {
            owners.insert(item.user_id.get(), 0_i64);
        }
        // Individual ordered SELECTs actually acquire row locks in id order,
        // unlike relying on the planner's ordering of a multi-row SELECT FOR UPDATE.
        for (user_id, quota) in &mut owners {
            *quota = sqlx::query_scalar::<_, i64>(
                "SELECT quota_bytes FROM users WHERE id = $1 FOR UPDATE",
            )
            .bind(*user_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| not_found(format!("user {user_id}")))?;
        }

        let mut account_totals: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
        let mut address_totals: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
        // Check all ownership and aggregate quotas before the first insert. The owner
        // locks above serialize cooperating logged writes against each other.
        for item in batch {
            let new = &item.message;
            let user_id = item.user_id.get();
            let mailbox_id = new.mailbox_id.get();
            let address_quota: Option<(Option<i64>,)> = sqlx::query_as(
                "SELECT b.quota_bytes FROM mailboxes b JOIN folders f ON f.mailbox_id = b.id
                  WHERE b.id = $1 AND b.user_id = $2 AND f.id = $3",
            )
            .bind(mailbox_id)
            .bind(user_id)
            .bind(new.folder_id.get())
            .fetch_optional(&mut *tx)
            .await?;
            let (address_quota,) = address_quota
                .ok_or_else(|| not_found(format!("folder {} in mailbox {} for user {}", new.folder_id, new.mailbox_id, item.user_id)))?;
            if let std::collections::btree_map::Entry::Vacant(slot) = account_totals.entry(user_id) {
                let used: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(m.size_bytes), 0)::BIGINT FROM messages m
                      WHERE m.expunged_at IS NULL AND m.mailbox_id IN
                        (SELECT id FROM mailboxes WHERE user_id = $1)",
                )
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await?;
                slot.insert((used, 0));
            }
            if let std::collections::btree_map::Entry::Vacant(slot) = address_totals.entry(mailbox_id) {
                let used: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM messages
                      WHERE expunged_at IS NULL AND mailbox_id = $1",
                )
                .bind(mailbox_id)
                .fetch_one(&mut *tx)
                .await?;
                slot.insert((used, 0));
            }
            let (used, pending) = account_totals.get_mut(&user_id).expect("account seeded above");
            let limit = owners[&user_id];
            let total = used.checked_add(*pending).and_then(|n| n.checked_add(new.size_bytes));
            if new.size_bytes > 0 && total.is_none_or(|n| n > limit) {
                return Err(StorageError::QuotaExceeded {
                    mailbox_id, used: used.saturating_add(*pending), needed: new.size_bytes, limit,
                });
            }
            *pending = pending.checked_add(new.size_bytes)
                .ok_or_else(|| StorageError::Invalid("batch size overflow".into()))?;

            let (used, pending) = address_totals.get_mut(&mailbox_id).expect("address seeded above");
            if let Some(limit) = address_quota {
                let total = used.checked_add(*pending).and_then(|n| n.checked_add(new.size_bytes));
                if new.size_bytes > 0 && total.is_none_or(|n| n > limit) {
                    return Err(StorageError::QuotaExceeded {
                        mailbox_id, used: used.saturating_add(*pending), needed: new.size_bytes, limit,
                    });
                }
            }
            *pending = pending.checked_add(new.size_bytes)
                .ok_or_else(|| StorageError::Invalid("batch size overflow".into()))?;
        }

        let mut inserted = Vec::with_capacity(batch.len());
        for item in batch {
            let new = &item.message;
            let unseen = i32::from(!flag_is_set(&new.flags, "seen"));
            let message = insert_message_tx(&mut tx, new, unseen).await?;
            for recipient in &item.recipients {
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
            append_relocation_change(&mut tx, item.user_id, &message, None).await?;
            inserted.push(message);
        }
        for user_id in owners.keys() {
            sqlx::query(
                "UPDATE users SET used_bytes = COALESCE((
                    SELECT SUM(m.size_bytes) FROM messages m WHERE m.expunged_at IS NULL
                      AND m.mailbox_id IN (SELECT id FROM mailboxes WHERE user_id = $1)
                ), 0), updated_at = NOW() WHERE id = $1",
            )
            .bind(*user_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// Reuse the normal UID/counter insert and creation journal in a larger transaction.
    pub(crate) async fn insert_submission_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        new: &NewMessage,
        user: UserId,
    ) -> Result<Message> {
        let unseen = i32::from(!flag_is_set(&new.flags, "seen"));
        let message = insert_message_tx(tx, new, unseen).await?;
        append_relocation_change(tx, user, &message, None).await?;
        Ok(message)
    }

    /// Look a message up by row id, tombstones included.
    pub async fn find_by_id(&self, id: MessageId) -> Result<Option<Message>> {
        Ok(
            sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE id = $1")
                .bind(id.get())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// [`MessagesRepository::find_by_id`], but a missing row is an error.
    pub async fn require_by_id(&self, id: MessageId) -> Result<Message> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| not_found(format!("message {id}")))
    }

    /// Look a message up by its IMAP UID inside a folder.
    ///
    /// Expunged messages are *not* returned: once a client has expunged a UID the
    /// folder no longer has it, and a stale hit would resurrect it in `FETCH`.
    pub async fn find_by_uid(&self, folder_id: MailboxId, uid: i64) -> Result<Option<Message>> {
        Ok(sqlx::query_as::<_, Message>(
            "SELECT * FROM messages WHERE folder_id = $1 AND uid = $2 AND expunged_at IS NULL",
        )
        .bind(folder_id.get())
        .bind(uid)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every copy of an RFC 5322 `Message-ID` inside one address (a message filed
    /// into several folders has several rows). Newest first.
    pub async fn find_by_rfc_message_id(
        &self,
        mailbox_id: MailboxId,
        rfc_id: &str,
    ) -> Result<Vec<Message>> {
        Ok(sqlx::query_as::<_, Message>(
            "SELECT * FROM messages
              WHERE mailbox_id = $1 AND rfc_message_id = $2 AND expunged_at IS NULL
              ORDER BY internal_date DESC, id DESC",
        )
        .bind(mailbox_id.get())
        .bind(rfc_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// A page of a folder's live messages, newest `internal_date` first.
    pub async fn list_by_folder(
        &self,
        folder_id: MailboxId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Message>> {
        Ok(sqlx::query_as::<_, Message>(
            "SELECT * FROM messages
              WHERE folder_id = $1 AND expunged_at IS NULL
              ORDER BY internal_date DESC, id DESC LIMIT $2 OFFSET $3",
        )
        .bind(folder_id.get())
        .bind(limit_of(limit))
        .bind(offset_of(offset))
        .fetch_all(&self.pool)
        .await?)
    }

    /// The live messages of one folder with the given UIDs, UID order.
    ///
    /// An empty `uids` slice matches nothing — which is what `FETCH 1:*` on an empty
    /// folder should do, and better than accidentally returning everything.
    pub async fn list_by_uids(&self, folder_id: MailboxId, uids: &[i64]) -> Result<Vec<Message>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as::<_, Message>(
            "SELECT * FROM messages
              WHERE folder_id = $1 AND uid = ANY($2) AND expunged_at IS NULL
              ORDER BY uid ASC",
        )
        .bind(folder_id.get())
        .bind(uids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// A page across every folder of one address, newest first.
    pub async fn list_by_mailbox(
        &self,
        mailbox_id: MailboxId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Message>> {
        Ok(sqlx::query_as::<_, Message>(
            "SELECT * FROM messages
              WHERE mailbox_id = $1 AND expunged_at IS NULL
              ORDER BY internal_date DESC, id DESC LIMIT $2 OFFSET $3",
        )
        .bind(mailbox_id.get())
        .bind(limit_of(limit))
        .bind(offset_of(offset))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every live message of a folder in UID order — the IMAP `SELECT` view.
    pub async fn list_unexpunged(&self, folder_id: MailboxId) -> Result<Vec<Message>> {
        Ok(sqlx::query_as::<_, Message>(
            "SELECT * FROM messages WHERE folder_id = $1 AND expunged_at IS NULL ORDER BY uid ASC",
        )
        .bind(folder_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many live messages a folder holds.
    pub async fn count_by_folder(&self, folder_id: MailboxId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages WHERE folder_id = $1 AND expunged_at IS NULL",
        )
        .bind(folder_id.get())
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// How many live messages of a folder lack the `seen` flag.
    pub async fn count_unseen(&self, folder_id: MailboxId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages
              WHERE folder_id = $1 AND expunged_at IS NULL
                AND NOT ('seen' = ANY(string_to_array(lower(flags), ' ')))",
        )
        .bind(folder_id.get())
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// The highest UID ever handed out in a folder (`0` when it is empty).
    pub async fn max_uid(&self, folder_id: MailboxId) -> Result<i64> {
        let (max,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(uid), 0)::BIGINT FROM messages WHERE folder_id = $1",
        )
        .bind(folder_id.get())
        .fetch_one(&self.pool)
        .await?;
        Ok(max)
    }

    /// The newest live messages, by the `(internal_date, id)` ordering key the sync
    /// engine uses as a `seq`-like cursor.
    pub async fn newest(&self, folder_id: MailboxId, limit: i64) -> Result<Vec<Message>> {
        Ok(sqlx::query_as::<_, Message>(
            "SELECT * FROM messages
              WHERE folder_id = $1 AND expunged_at IS NULL
              ORDER BY internal_date DESC, id DESC LIMIT $2",
        )
        .bind(folder_id.get())
        .bind(limit_of(limit))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Replace the whole flag string.
    pub async fn set_flags(&self, id: MessageId, flags: &str) -> Result<()> {
        let done = sqlx::query("UPDATE messages SET flags = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(flags)
            .execute(&self.pool)
            .await?;
        touched(done.rows_affected(), id)
    }

    /// Update flags, Maildir path and durable cursor as one database mutation.
    ///
    /// The expected path protects against a stale session overwriting a concurrent
    /// move. The caller must stage the Maildir rename before this call, and roll
    /// it back if the transaction fails. No journal entry is written on a no-op.
    pub async fn set_flags_with_path_logged(
        &self,
        id: MessageId,
        user: UserId,
        expected_path: &str,
        new_path: &str,
        flags: &str,
    ) -> Result<Message> {
        let mut tx = self.pool.begin().await?;
        let updated: Option<Message> = sqlx::query_as(
            "UPDATE messages m SET flags = $4, storage_path = $5, updated_at = NOW()
               FROM mailboxes b WHERE m.id = $1 AND m.mailbox_id = b.id
                 AND b.user_id = $2 AND m.storage_path = $3
                 AND m.flags IS DISTINCT FROM $4 RETURNING m.*",
        )
        .bind(id.get()).bind(user.get()).bind(expected_path)
        .bind(flags).bind(new_path)
        .fetch_optional(&mut *tx).await?;
        let message = if let Some(message) = updated {
            sqlx::query(
                "INSERT INTO change_log (user_id, mailbox_id, folder_id, message_id, kind, payload)
                 VALUES ($1, $2, $3, $4, 'message_updated', $5)",
            )
            .bind(user.get()).bind(message.mailbox_id).bind(message.folder_id)
            .bind(message.id).bind(serde_json::json!({"uid": message.uid, "flags": message.flags}))
            .execute(&mut *tx).await?;
            message
        } else {
            let current: Option<Message> = sqlx::query_as(
                "SELECT m.* FROM messages m JOIN mailboxes b ON b.id = m.mailbox_id
                  WHERE m.id = $1 AND b.user_id = $2 AND m.storage_path = $3
                    AND m.flags = $4",
            ).bind(id.get()).bind(user.get()).bind(expected_path).bind(flags)
                .fetch_optional(&mut *tx).await?;
            current.ok_or_else(|| not_found(format!("message {id}")))?
        };
        tx.commit().await?;
        Ok(message)
    }

    /// Union `flags` into the message's flags and return the new flag string.
    ///
    /// Idempotent: flags that are already set are not duplicated, and matching is
    /// case-insensitive (IMAP system flags are).
    pub async fn add_flags(&self, id: MessageId, flags: &str) -> Result<String> {
        self.mutate_flags(id, flags, FlagOp::Add).await
    }

    /// Remove `flags` from the message's flags and return the new flag string.
    ///
    /// Removing a flag that is not set is a no-op, not an error.
    pub async fn remove_flags(&self, id: MessageId, flags: &str) -> Result<String> {
        self.mutate_flags(id, flags, FlagOp::Remove).await
    }

    /// Add or clear `\Seen`.
    pub async fn mark_seen(&self, id: MessageId, seen: bool) -> Result<()> {
        if seen {
            self.add_flags(id, "seen").await?;
        } else {
            self.remove_flags(id, "seen").await?;
        }
        Ok(())
    }

    /// Add `\Deleted` and stamp the soft-delete column.
    ///
    /// The row stays visible to IMAP until [`MessagesRepository::expunge`] runs —
    /// that is what makes `\Deleted` recoverable before `EXPUNGE`.
    pub async fn mark_deleted(&self, id: MessageId) -> Result<()> {
        let done = sqlx::query(
            "UPDATE messages
                SET flags = CASE
                        WHEN 'deleted' = ANY(string_to_array(lower(flags), ' ')) THEN flags
                        WHEN btrim(flags) = '' THEN 'deleted'
                        ELSE flags || ' deleted'
                    END,
                    deleted_at = COALESCE(deleted_at, NOW()),
                    updated_at = NOW()
              WHERE id = $1",
        )
        .bind(id.get())
        .execute(&self.pool)
        .await?;
        touched(done.rows_affected(), id)
    }

    /// Clear `\Deleted` and the soft-delete column.
    pub async fn clear_deleted(&self, id: MessageId) -> Result<()> {
        let done = sqlx::query(
            "UPDATE messages
                SET flags = array_to_string(
                        ARRAY(
                            SELECT f FROM unnest(string_to_array(flags, ' ')) AS f
                             WHERE f <> '' AND lower(f) <> 'deleted'
                        ), ' '),
                    deleted_at = NULL,
                    updated_at = NOW()
              WHERE id = $1",
        )
        .bind(id.get())
        .execute(&self.pool)
        .await?;
        touched(done.rows_affected(), id)
    }

    /// Mark every `\Deleted` message of a folder expunged and return them.
    ///
    /// The rows are *not* deleted: they become tombstones so that a client which
    /// still holds the id can be told what happened. The caller deletes the files.
    pub async fn expunge(&self, folder_id: MailboxId) -> Result<Vec<Message>> {
        let mut tx = self.pool.begin().await?;

        // Lock candidate message rows before inspecting the queue. Enqueue's FK
        // takes a KEY SHARE lock on the same row, so it cannot slip in between
        // this check and the expunge commit.
        let candidates: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM messages WHERE folder_id = $1 AND expunged_at IS NULL
                AND 'deleted' = ANY(string_to_array(lower(flags), ' '))
              ORDER BY id FOR UPDATE",
        )
        .bind(folder_id.get())
        .fetch_all(&mut *tx)
        .await?;
        for (id,) in candidates {
            reject_active_queue(&mut tx, id).await?;
        }

        let mut expunged = sqlx::query_as::<_, Message>(
            "UPDATE messages SET expunged_at = NOW(), updated_at = NOW()
              WHERE folder_id = $1
                AND expunged_at IS NULL
                AND 'deleted' = ANY(string_to_array(lower(flags), ' '))
              RETURNING *",
        )
        .bind(folder_id.get())
        .fetch_all(&mut *tx)
        .await?;

        if !expunged.is_empty() {
            // The deltas are computed here rather than in SQL so the folder row is written
            // once, by one statement: `GREATEST` keeps a folder that was already drifting
            // from going negative.
            let removed = expunged.len() as i64;
            let unread = expunged
                .iter()
                .filter(|message| !flag_is_set(&message.flags, "seen"))
                .count() as i64;
            let bytes: i64 = expunged.iter().map(|message| message.size_bytes).sum();

            sqlx::query(
                "UPDATE folders
                    SET message_count = GREATEST(message_count - $2, 0),
                        unseen_count  = GREATEST(unseen_count - $3, 0),
                        total_bytes   = GREATEST(total_bytes - $4, 0),
                        updated_at    = NOW()
                  WHERE id = $1",
            )
            .bind(folder_id.get())
            .bind(removed)
            .bind(unread)
            .bind(bytes)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        // `UPDATE ... RETURNING` has no defined row order (PostgreSQL rejects
        // `ORDER BY` on `UPDATE`), so the UID ordering IMAP wants is applied here.
        expunged.sort_by_key(|message| message.uid);
        Ok(expunged)
    }

    /// Hard-delete one row and return it, so the caller can delete its files.
    ///
    /// Returns `None` when the row was already gone.
    pub async fn hard_delete(&self, id: MessageId) -> Result<Option<Message>> {
        let mut tx = self.pool.begin().await?;
        let row = lock_message_for_deletion(&mut tx, id, None).await?;
        if row.is_some() {
            reject_active_queue(&mut tx, id.get()).await?;
            sqlx::query("DELETE FROM messages WHERE id = $1")
                .bind(id.get()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    /// Delete one row and write its durable tombstone in the same transaction.
    ///
    /// Used for authenticated IMAP EXPUNGE/CLOSE. `None` means a prior operation
    /// already removed it. The Maildir file must be deleted only after commit.
    pub async fn hard_delete_logged(&self, id: MessageId, user: UserId) -> Result<Option<Message>> {
        let mut tx = self.pool.begin().await?;
        let row = lock_message_for_deletion(&mut tx, id, Some(user)).await?;
        if let Some(message) = &row {
            reject_active_queue(&mut tx, id.get()).await?;
            sqlx::query("DELETE FROM messages WHERE id = $1")
                .bind(id.get()).execute(&mut *tx).await?;
            sqlx::query(
                "INSERT INTO change_log (user_id, mailbox_id, folder_id, message_id, kind, payload)
                 VALUES ($1, $2, $3, $4, 'message_deleted', $5)",
            )
            .bind(user.get()).bind(message.mailbox_id).bind(message.folder_id)
            .bind(message.id).bind(serde_json::json!({"uid": message.uid, "permanent": true}))
            .execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE users SET used_bytes = GREATEST(used_bytes - $2, 0), updated_at = NOW()
                  WHERE id = $1",
            )
            .bind(user.get()).bind(message.size_bytes.max(0))
            .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    /// Move a message into another folder with a fresh UID in that folder.
    ///
    /// The source row is the same row afterwards — it simply has a new parent. The
    /// UID it had in the old folder is never reissued, as IMAP requires.
    pub async fn move_to_folder(
        &self,
        id: MessageId,
        to_folder_id: MailboxId,
        to_mailbox_id: MailboxId,
    ) -> Result<Message> {
        self.move_to_folder_with_path(id, to_folder_id, to_mailbox_id, None)
            .await
    }

    /// Move a message and its Maildir path in one database statement.
    /// `None` retains the path for callers that manage it separately.
    pub async fn move_to_folder_with_path(
        &self,
        id: MessageId,
        to_folder_id: MailboxId,
        to_mailbox_id: MailboxId,
        path: Option<&str>,
    ) -> Result<Message> {
        self.move_to_folder_with_path_logged(id, to_folder_id, to_mailbox_id, path, None, None).await
    }

    /// Move the row, path and optional durable sync change in one transaction.
    /// The caller supplies the authenticated user for a logged mutation. An
    /// expected source path rejects a stale snapshot after a concurrent move.
    pub async fn move_to_folder_with_path_logged(
        &self,
        id: MessageId,
        to_folder_id: MailboxId,
        to_mailbox_id: MailboxId,
        path: Option<&str>,
        user: Option<UserId>,
        expected_source: Option<&str>,
    ) -> Result<Message> {
        let mut tx = self.pool.begin().await?;
        let from_folder = if let Some(user) = user {
            let source: Option<(i64,)> = sqlx::query_as(
                "SELECT m.folder_id FROM messages m JOIN mailboxes b ON b.id = m.mailbox_id
                  WHERE m.id = $1 AND b.user_id = $2 AND m.mailbox_id = $3
                    AND ($4::TEXT IS NULL OR m.storage_path = $4) FOR UPDATE OF m",
            ).bind(id.get()).bind(user.get()).bind(to_mailbox_id.get())
                .bind(expected_source).fetch_optional(&mut *tx).await?;
            Some(source.ok_or_else(|| not_found(format!("message {id}")))?.0)
        } else { None };
        let moved = sqlx::query_as::<_, Message>(
            "WITH next_uid AS (
                 UPDATE folders SET uid_next = uid_next + 1, updated_at = NOW()
                  WHERE id = $2
                  RETURNING uid_next - 1 AS uid
             )
             UPDATE messages m
                SET folder_id = $2, mailbox_id = $3, uid = next_uid.uid,
                     storage_path = COALESCE($4, m.storage_path), updated_at = NOW()
               FROM next_uid
              WHERE m.id = $1 AND m.mailbox_id = $3
                AND ($5::TEXT IS NULL OR m.storage_path = $5)
             RETURNING m.*",
        )
        .bind(id.get())
        .bind(to_folder_id.get())
        .bind(to_mailbox_id.get())
        .bind(path)
        .bind(expected_source)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(message) = moved else {
            tx.rollback().await?;
            return Err(self.missing_message_or_folder(id, to_folder_id).await?);
        };
        if let (Some(user), Some(from_folder)) = (user, from_folder) {
            append_relocation_change(&mut tx, user, &message, Some(from_folder)).await?;
        }
        tx.commit().await?;
        Ok(message)
    }

    /// Copy a message into another folder with a fresh UID in that folder.
    ///
    /// Recipients and attachment rows are duplicated too. The attachment *blobs* are
    /// not: the attachment store is content-addressed, so both rows point at the same
    /// file and `AttachmentStore::gc` only removes it once neither row references it.
    pub async fn copy_to_folder(
        &self,
        id: MessageId,
        to_folder_id: MailboxId,
        to_mailbox_id: MailboxId,
    ) -> Result<Message> {
        self.copy_to_folder_with_path(id, to_folder_id, to_mailbox_id, None)
            .await
    }

    /// Copy a message with its new Maildir path in the same insert transaction.
    /// `None` retains the source path for callers that manage it separately.
    pub async fn copy_to_folder_with_path(
        &self,
        id: MessageId,
        to_folder_id: MailboxId,
        to_mailbox_id: MailboxId,
        path: Option<&str>,
    ) -> Result<Message> {
        self.copy_to_folder_with_path_logged(id, to_folder_id, to_mailbox_id, path, None, None).await
    }

    /// Copy the row, quota usage and optional durable sync change in one transaction.
    /// The caller supplies the authenticated user for a logged mutation. An
    /// expected source path rejects a stale snapshot after a concurrent move.
    pub async fn copy_to_folder_with_path_logged(
        &self,
        id: MessageId,
        to_folder_id: MailboxId,
        to_mailbox_id: MailboxId,
        path: Option<&str>,
        user: Option<UserId>,
        expected_source: Option<&str>,
    ) -> Result<Message> {
        let mut tx = self.pool.begin().await?;
        // Serialise copies for one account and check the live-message total in
        // the same transaction that inserts the copy. The cached used_bytes may
        // lag after an interrupted write; it must not decide whether quota fits.
        let owner: Option<(i64, i64)> = sqlx::query_as(
            "SELECT u.id, COALESCE(m.quota_bytes, u.quota_bytes)
               FROM mailboxes m JOIN users u ON u.id = m.user_id
              WHERE m.id = $1 FOR UPDATE OF u",
        )
        .bind(to_mailbox_id.get())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((user_id, limit)) = owner else {
            return Err(not_found(format!("mailbox {to_mailbox_id}")));
        };
        if user.is_some_and(|user| user.get() != user_id) {
            return Err(not_found(format!("mailbox {to_mailbox_id}")));
        }
        let size: Option<(i64,)> = sqlx::query_as(
            "SELECT size_bytes FROM messages WHERE id = $1 AND mailbox_id = $2
                AND expunged_at IS NULL AND ($3::TEXT IS NULL OR storage_path = $3)",
        )
        .bind(id.get())
        .bind(to_mailbox_id.get())
        .bind(expected_source)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((needed,)) = size else {
            return Err(not_found(format!("message {id}")));
        };
        let (used,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(ms.size_bytes), 0)::BIGINT FROM messages ms
              WHERE ms.expunged_at IS NULL AND ms.mailbox_id IN
                (SELECT id FROM mailboxes WHERE user_id = $1)",
        )
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
        if needed > 0 && used.checked_add(needed).is_none_or(|total| total > limit) {
            return Err(StorageError::QuotaExceeded {
                mailbox_id: to_mailbox_id.get(), used, needed, limit,
            });
        }

        let inserted: Option<(i64,)> = sqlx::query_as(
            "WITH next_uid AS (
                 UPDATE folders SET uid_next = uid_next + 1, updated_at = NOW()
                  WHERE id = $2
                  RETURNING uid_next - 1 AS uid
             )
             INSERT INTO messages (
                 folder_id, mailbox_id, uid, rfc_message_id, thread_id, subject, sender,
                 sender_name, snippet, size_bytes, storage_path, checksum_sha256, flags,
                 internal_date, sent_at, has_attachments, attachment_count, is_draft,
                 modseq, deleted_at
             )
             SELECT $2, $3, next_uid.uid, m.rfc_message_id, m.thread_id, m.subject, m.sender,
                    m.sender_name, m.snippet, m.size_bytes, COALESCE($4, m.storage_path), m.checksum_sha256,
                    m.flags, m.internal_date, m.sent_at, m.has_attachments, m.attachment_count,
                    m.is_draft, m.modseq, m.deleted_at
               FROM messages m, next_uid
              WHERE m.id = $1 AND m.mailbox_id = $3 AND m.expunged_at IS NULL
                AND ($5::TEXT IS NULL OR m.storage_path = $5)
             RETURNING id",
        )
        .bind(id.get())
        .bind(to_folder_id.get())
        .bind(to_mailbox_id.get())
        .bind(path)
        .bind(expected_source)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((new_id,)) = inserted else {
            tx.rollback().await?;
            return Err(self.missing_message_or_folder(id, to_folder_id).await?);
        };

        sqlx::query(
            "INSERT INTO message_recipients (message_id, kind, address, display_name, ordinal)
             SELECT $1, kind, address, display_name, ordinal
               FROM message_recipients WHERE message_id = $2",
        )
        .bind(new_id)
        .bind(id.get())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO attachments (message_id, filename, content_type, size_bytes,
                                      storage_path, content_id, is_inline, checksum_sha256)
             SELECT $1, filename, content_type, size_bytes, storage_path, content_id,
                    is_inline, checksum_sha256
               FROM attachments WHERE message_id = $2",
        )
        .bind(new_id)
        .bind(id.get())
        .execute(&mut *tx)
        .await?;

        let copied = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE id = $1")
            .bind(new_id)
            .fetch_one(&mut *tx)
            .await?;
        // Repair cached usage from the actual live rows, including this copy.
        // The owner lock above prevents two concurrent COPYs from both passing
        // on the same pre-copy total and overwriting each other's accounting.
        sqlx::query(
            "UPDATE users SET used_bytes = COALESCE((
                SELECT SUM(ms.size_bytes) FROM messages ms
                 WHERE ms.expunged_at IS NULL AND ms.mailbox_id IN
                    (SELECT id FROM mailboxes WHERE user_id = $1)
            ), 0), updated_at = NOW() WHERE id = $1",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        if let Some(user) = user {
            append_relocation_change(&mut tx, user, &copied, None).await?;
        }

        tx.commit().await?;
        Ok(copied)
    }

    /// Point a message at a different body file.
    pub async fn set_storage_path(&self, id: MessageId, storage_path: &str) -> Result<()> {
        let done =
            sqlx::query("UPDATE messages SET storage_path = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(storage_path)
                .execute(&self.pool)
                .await?;
        touched(done.rows_affected(), id)
    }

    /// Replace the list-view preview (`None` clears it).
    pub async fn set_snippet(&self, id: MessageId, snippet: Option<&str>) -> Result<()> {
        let done =
            sqlx::query("UPDATE messages SET snippet = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(snippet)
                .execute(&self.pool)
                .await?;
        touched(done.rows_affected(), id)
    }

    /// Server-side search over the live messages.
    ///
    /// All predicates are bound parameters; the only SQL that varies is which fixed
    /// fragments are present. `%` and `_` in a needle are escaped, so searching for
    /// `100%` does not match every subject.
    pub async fn search(&self, query: MessageSearch) -> Result<Vec<Message>> {
        let mut builder =
            QueryBuilder::<Postgres>::new("SELECT * FROM messages WHERE expunged_at IS NULL");

        if let Some(folder_id) = query.folder_id {
            builder.push(" AND folder_id = ").push_bind(folder_id.get());
        }
        if let Some(mailbox_id) = query.mailbox_id {
            builder
                .push(" AND mailbox_id = ")
                .push_bind(mailbox_id.get());
        }
        if let Some(subject) = query.subject.as_deref().filter(|s| !s.is_empty()) {
            builder
                .push(" AND subject ILIKE ")
                .push_bind(like_pattern(subject));
        }
        if let Some(sender) = query.sender.as_deref().filter(|s| !s.is_empty()) {
            builder
                .push(" AND (sender ILIKE ")
                .push_bind(like_pattern(sender))
                .push(" OR sender_name ILIKE ")
                .push_bind(like_pattern(sender))
                .push(")");
        }
        if let Some(text) = query.text.as_deref().filter(|s| !s.is_empty()) {
            let pattern = like_pattern(text);
            builder
                .push(" AND (subject ILIKE ")
                .push_bind(pattern.clone())
                .push(" OR sender ILIKE ")
                .push_bind(pattern.clone())
                .push(" OR snippet ILIKE ")
                .push_bind(pattern)
                .push(")");
        }
        if query.unread_only {
            builder.push(" AND NOT ('seen' = ANY(string_to_array(lower(flags), ' ')))");
        }
        if query.flagged_only {
            builder.push(" AND 'flagged' = ANY(string_to_array(lower(flags), ' '))");
        }
        if query.with_attachments_only {
            builder.push(" AND has_attachments");
        }
        if let Some(since) = query.since {
            builder.push(" AND internal_date >= ").push_bind(since);
        }
        if let Some(before) = query.before {
            builder.push(" AND internal_date < ").push_bind(before);
        }

        builder
            .push(" ORDER BY internal_date DESC, id DESC LIMIT ")
            .push_bind(limit_of(query.limit))
            .push(" OFFSET ")
            .push_bind(offset_of(query.offset));

        Ok(builder
            .build_query_as::<Message>()
            .fetch_all(&self.pool)
            .await?)
    }

    /// Attach the `To`/`Cc`/`Bcc` entries to a message.
    pub async fn insert_recipients(
        &self,
        message_id: MessageId,
        recipients: &[Recipient],
    ) -> Result<()> {
        if recipients.is_empty() {
            return Ok(());
        }

        let mut builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO message_recipients (message_id, kind, address, display_name, ordinal) ",
        );
        builder.push_values(recipients, |mut row, recipient| {
            row.push_bind(message_id.get())
                .push_bind(recipient.kind.as_str())
                .push_bind(recipient.address.as_str())
                .push_bind(recipient.display_name.as_deref())
                .push_bind(recipient.ordinal);
        });
        builder.build().execute(&self.pool).await?;
        Ok(())
    }

    /// A message's recipients, ordered by `(ordinal, id)`.
    ///
    /// `Recipient::ordinal` is the position *within its own kind* (`models.rs`), so a
    /// list that mixes kinds interleaves by position rather than grouping: the first
    /// `to`, the first `cc` and the first `reply-to` all sort before the second `to`.
    /// Callers that render `To:`/`Cc:`/`Bcc:` as separate headers group by `kind` —
    /// within a kind the rows come back in the order the sender wrote them. A list
    /// whose ordinals are all `0` (the common case) keeps its insertion order through
    /// the `id` tie-break.
    pub async fn recipients(&self, message_id: MessageId) -> Result<Vec<MessageRecipient>> {
        Ok(sqlx::query_as::<_, MessageRecipient>(
            "SELECT * FROM message_recipients WHERE message_id = $1 ORDER BY ordinal ASC, id ASC",
        )
        .bind(message_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Drop every recipient of a message.
    pub async fn delete_recipients(&self, message_id: MessageId) -> Result<()> {
        sqlx::query("DELETE FROM message_recipients WHERE message_id = $1")
            .bind(message_id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Apply a set operation to a message's flags under a row lock.
    async fn mutate_flags(&self, id: MessageId, flags: &str, op: FlagOp) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let current: Option<(String,)> =
            sqlx::query_as("SELECT flags FROM messages WHERE id = $1 FOR UPDATE")
                .bind(id.get())
                .fetch_optional(&mut *tx)
                .await?;
        let Some((current,)) = current else {
            tx.rollback().await?;
            return Err(not_found(format!("message {id}")));
        };

        let updated = match op {
            FlagOp::Add => merge_flags(&current, flags),
            FlagOp::Remove => subtract_flags(&current, flags),
        };

        if updated != current {
            sqlx::query("UPDATE messages SET flags = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(&updated)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(updated)
    }

    /// Distinguish "the message is gone" from "the target folder is gone".
    async fn missing_message_or_folder(
        &self,
        id: MessageId,
        folder_id: MailboxId,
    ) -> Result<StorageError> {
        if self.find_by_id(id).await?.is_none() {
            return Ok(not_found(format!("message {id}")));
        }
        Ok(not_found(format!("folder {folder_id}")))
    }
}

// A FOR UPDATE lock conflicts with the queue insertion's FK KEY SHARE lock.
// Keep the lock until commit so a concurrent enqueue cannot race the check.
async fn lock_message_for_deletion(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    id: MessageId,
    user: Option<UserId>,
) -> Result<Option<Message>> {
    Ok(sqlx::query_as::<_, Message>(
        "SELECT m.* FROM messages m JOIN mailboxes b ON b.id = m.mailbox_id
          WHERE m.id = $1 AND ($2::BIGINT IS NULL OR b.user_id = $2)
          FOR UPDATE OF m",
    )
    .bind(id.get())
    .bind(user.map(UserId::get))
    .fetch_optional(&mut **tx)
    .await?)
}

async fn reject_active_queue(tx: &mut sqlx::Transaction<'_, Postgres>, id: i64) -> Result<()> {
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM mail_queue
          WHERE message_id = $1 AND status IN ('pending', 'retry', 'delivering'))",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    if active {
        return Err(StorageError::Conflict(format!(
            "message {id} is referenced by an active delivery queue entry"
        )));
    }
    Ok(())
}

/// Allocate a UID and insert a row inside the caller's transaction.
pub(crate) async fn insert_message_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    new: &NewMessage,
    unseen: i32,
) -> Result<Message> {
    let uid = sqlx::query_scalar::<_, i64>(
        "UPDATE folders
            SET uid_next      = uid_next + 1,
                message_count = message_count + 1,
                unseen_count  = unseen_count + $2,
                total_bytes   = total_bytes + $3,
                updated_at    = NOW()
          WHERE id = $1 AND mailbox_id = $4
          RETURNING uid_next - 1",
    )
    .bind(new.folder_id.get())
    .bind(unseen)
    .bind(new.size_bytes)
    .bind(new.mailbox_id.get())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| not_found(format!("folder {}", new.folder_id)))?;

    Ok(sqlx::query_as::<_, Message>(
        "INSERT INTO messages (
             folder_id, mailbox_id, uid, rfc_message_id, thread_id, subject, sender,
             sender_name, snippet, size_bytes, storage_path, checksum_sha256, flags,
             internal_date, sent_at, has_attachments, attachment_count, is_draft
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 COALESCE($14::TIMESTAMPTZ, NOW()), $15, $16, $17, $18)
         RETURNING *",
    )
    .bind(new.folder_id.get())
    .bind(new.mailbox_id.get())
    .bind(uid)
    .bind(new.rfc_message_id.as_deref())
    .bind(new.thread_id.as_deref())
    .bind(new.subject.as_deref())
    .bind(new.sender.as_deref())
    .bind(new.sender_name.as_deref())
    .bind(new.snippet.as_deref())
    .bind(new.size_bytes)
    .bind(&new.storage_path)
    .bind(new.checksum_sha256.as_deref())
    .bind(&new.flags)
    .bind(new.internal_date)
    .bind(new.sent_at)
    .bind(new.has_attachments)
    .bind(new.attachment_count)
    .bind(new.is_draft)
    .fetch_one(&mut **tx)
    .await?)
}

/// Append the COPY/MOVE cursor entry before committing its message mutation.
pub(crate) async fn append_relocation_change(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    user: UserId,
    message: &Message,
    from_folder: Option<i64>,
) -> Result<()> {
    let (kind, payload) = if let Some(from) = from_folder {
        ("message_moved", serde_json::json!({
            "uid": message.uid, "flags": message.flags,
            "from_folder_id": from, "to_folder_id": message.folder_id,
        }))
    } else {
        ("message_created", serde_json::json!({
            "uid": message.uid, "flags": message.flags,
            "size_bytes": message.size_bytes, "subject": message.subject,
            "sender": message.sender, "has_attachments": message.has_attachments,
        }))
    };
    sqlx::query(
        "INSERT INTO change_log (user_id, mailbox_id, folder_id, message_id, kind, payload)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(user.get()).bind(message.mailbox_id).bind(message.folder_id)
    .bind(message.id).bind(kind).bind(payload)
    .execute(&mut **tx).await?;
    Ok(())
}

/// Which set operation [`MessagesRepository::mutate_flags`] applies.
#[derive(Debug, Clone, Copy)]
enum FlagOp {
    /// Union the given flags into the message.
    Add,
    /// Remove the given flags from the message.
    Remove,
}

/// Split a flag string into its distinct tokens, keeping the original spelling and
/// order but dropping case-insensitive duplicates and empty tokens.
fn flag_tokens(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in raw.split_whitespace() {
        if !out.iter().any(|seen| seen.eq_ignore_ascii_case(token)) {
            out.push(token.to_string());
        }
    }
    out
}

/// The union of two flag strings, with `current`'s order preserved.
fn merge_flags(current: &str, extra: &str) -> String {
    let mut tokens = flag_tokens(current);
    for token in flag_tokens(extra) {
        if !tokens.iter().any(|seen| seen.eq_ignore_ascii_case(&token)) {
            tokens.push(token);
        }
    }
    tokens.join(" ")
}

/// `current` minus every token named in `remove`, case-insensitively.
fn subtract_flags(current: &str, remove: &str) -> String {
    let drop = flag_tokens(remove);
    flag_tokens(current)
        .into_iter()
        .filter(|token| !drop.iter().any(|d| d.eq_ignore_ascii_case(token)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turn "the `UPDATE` matched no row" into [`StorageError::NotFound`].
fn touched(rows: u64, id: MessageId) -> Result<()> {
    if rows == 0 {
        return Err(not_found(format!("message {id}")));
    }
    Ok(())
}

/// Attachment rows. The bytes live in the attachment store.
#[derive(Debug, Clone)]
pub struct AttachmentsRepository {
    pool: PgPool,
}

impl AttachmentsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Record an attachment of a message.
    pub async fn insert(&self, message_id: MessageId, new: NewAttachment) -> Result<AttachmentRow> {
        if new.size_bytes < 0 {
            return Err(StorageError::Invalid(
                "attachment size_bytes must be >= 0".into(),
            ));
        }

        Ok(sqlx::query_as::<_, AttachmentRow>(
            "INSERT INTO attachments (message_id, filename, content_type, size_bytes,
                                      storage_path, content_id, is_inline, checksum_sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING *",
        )
        .bind(message_id.get())
        .bind(new.filename.as_deref())
        .bind(&new.content_type)
        .bind(new.size_bytes)
        .bind(&new.storage_path)
        .bind(new.content_id.as_deref())
        .bind(new.is_inline)
        .bind(new.checksum_sha256.as_deref())
        .fetch_one(&self.pool)
        .await?)
    }

    /// Look an attachment up by row id.
    pub async fn find_by_id(&self, id: AttachmentId) -> Result<Option<AttachmentRow>> {
        Ok(
            sqlx::query_as::<_, AttachmentRow>("SELECT * FROM attachments WHERE id = $1")
                .bind(id.get())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Every attachment of a message, in insertion order.
    pub async fn list_by_message(&self, message_id: MessageId) -> Result<Vec<AttachmentRow>> {
        Ok(sqlx::query_as::<_, AttachmentRow>(
            "SELECT * FROM attachments WHERE message_id = $1 ORDER BY id ASC",
        )
        .bind(message_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Delete one attachment row. Returns `false` when it was already gone.
    pub async fn delete(&self, id: AttachmentId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM attachments WHERE id = $1")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Every storage path still referenced — the keep-set for `AttachmentStore::gc`.
    ///
    /// Because the store is content-addressed, one path may be referenced by several
    /// messages; it must only be collected once *none* of them refer to it.
    pub async fn referenced_paths(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT storage_path FROM attachments ORDER BY storage_path ASC",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|(path,)| path)
        .collect())
    }

    /// Total bytes referenced by attachment rows.
    pub async fn total_size(&self) -> Result<i64> {
        let (total,): (i64,) =
            sqlx::query_as("SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM attachments")
                .fetch_one(&self.pool)
                .await?;
        Ok(total)
    }
}

/// Whether a flag string contains `flag`.
///
/// Flags are stored bare and lower-case (`seen`, `flagged draft`), which is why
/// `recount`'s SQL compares `'seen'` against the lower-cased string: the incremental
/// update in `insert` and that recomputation have to agree, or a folder would drift the
/// moment a message arrived.
fn flag_is_set(flags: &str, flag: &str) -> bool {
    flags
        .split_whitespace()
        .any(|entry| entry.eq_ignore_ascii_case(flag))
}
