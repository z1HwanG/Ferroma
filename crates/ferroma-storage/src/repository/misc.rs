//! Small aggregates: drafts, login attempts and DB-backed settings.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};

use ferroma_core::{DraftId, MailboxId, MessageId, UserId};

use crate::error::{Result, StorageError};
use crate::models::{Draft, LoginAttempt, Setting};
use crate::repository::{limit_of, normalise, not_found, offset_of};

/// Everything needed to save a new draft.
#[derive(Debug, Clone)]
pub struct NewDraft {
    /// The account that owns the draft.
    pub user_id: UserId,
    /// The address the draft will be sent from, once it is sent.
    pub mailbox_id: Option<MailboxId>,
    /// The folder the draft is filed in.
    pub folder_id: Option<MailboxId>,
    /// Subject line.
    pub subject: Option<String>,
    /// Plain-text body.
    pub body_text: Option<String>,
    /// HTML body.
    pub body_html: Option<String>,
    /// JSON array of `{address, name}`.
    pub recipients: serde_json::Value,
    /// JSON array of `{filename, content_type, size_bytes, storage_path}`.
    pub attachments: serde_json::Value,
    /// The `Message-ID` this draft replies to.
    pub in_reply_to: Option<String>,
    /// JSON array of `Message-ID` strings.
    pub reference_ids: serde_json::Value,
}

/// A partial update: only the `Some(..)` fields change.
///
/// `None` means "leave it alone". Because `Option<Option<T>>` would be needed to say
/// "clear this column", clearing is expressed by sending the empty value (`""`,
/// `[]`), which is what the client protocol does too.
#[derive(Debug, Clone, Default)]
pub struct DraftUpdate {
    /// New subject line.
    pub subject: Option<String>,
    /// New plain-text body.
    pub body_text: Option<String>,
    /// New HTML body.
    pub body_html: Option<String>,
    /// New recipient list.
    pub recipients: Option<serde_json::Value>,
    /// New attachment list.
    pub attachments: Option<serde_json::Value>,
    /// New `In-Reply-To`.
    pub in_reply_to: Option<String>,
    /// New reference chain.
    pub reference_ids: Option<serde_json::Value>,
    /// Link the draft to the message row it was materialised into.
    pub message_id: Option<MessageId>,
}

/// Saved drafts.
#[derive(Debug, Clone)]
pub struct DraftsRepository {
    pool: PgPool,
}

impl DraftsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Save a new draft.
    pub async fn create(&self, new: NewDraft) -> Result<Draft> {
        Ok(sqlx::query_as::<_, Draft>(
            "INSERT INTO drafts (user_id, mailbox_id, folder_id, subject, body_text, body_html,
                                 recipients, attachments, in_reply_to, reference_ids)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING *",
        )
        .bind(new.user_id.get())
        .bind(new.mailbox_id.map(MailboxId::get))
        .bind(new.folder_id.map(MailboxId::get))
        .bind(new.subject.as_deref())
        .bind(new.body_text.as_deref())
        .bind(new.body_html.as_deref())
        .bind(&new.recipients)
        .bind(&new.attachments)
        .bind(new.in_reply_to.as_deref())
        .bind(&new.reference_ids)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Look a draft up by id.
    pub async fn find_by_id(&self, id: DraftId) -> Result<Option<Draft>> {
        Ok(sqlx::query_as::<_, Draft>("SELECT * FROM drafts WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// An account's drafts, most recently touched first.
    pub async fn list_for_user(&self, user_id: UserId, limit: i64, offset: i64) -> Result<Vec<Draft>> {
        Ok(sqlx::query_as::<_, Draft>(
            "SELECT * FROM drafts WHERE user_id = $1
              ORDER BY updated_at DESC, id DESC LIMIT $2 OFFSET $3",
        )
        .bind(user_id.get())
        .bind(limit_of(limit))
        .bind(offset_of(offset))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Apply a partial update and return the stored row.
    ///
    /// Only the `SET` clauses for the fields the caller actually supplied are built,
    /// so an autosave that only carries the body cannot clobber the recipients.
    pub async fn update(&self, id: DraftId, update: DraftUpdate) -> Result<Draft> {
        let mut builder = QueryBuilder::<Postgres>::new("UPDATE drafts SET updated_at = NOW()");

        if let Some(subject) = update.subject {
            builder.push(", subject = ").push_bind(subject);
        }
        if let Some(body_text) = update.body_text {
            builder.push(", body_text = ").push_bind(body_text);
        }
        if let Some(body_html) = update.body_html {
            builder.push(", body_html = ").push_bind(body_html);
        }
        if let Some(recipients) = update.recipients {
            builder.push(", recipients = ").push_bind(recipients);
        }
        if let Some(attachments) = update.attachments {
            builder.push(", attachments = ").push_bind(attachments);
        }
        if let Some(in_reply_to) = update.in_reply_to {
            builder.push(", in_reply_to = ").push_bind(in_reply_to);
        }
        if let Some(reference_ids) = update.reference_ids {
            builder.push(", reference_ids = ").push_bind(reference_ids);
        }
        if let Some(message_id) = update.message_id {
            builder.push(", message_id = ").push_bind(message_id.get());
        }

        builder.push(" WHERE id = ").push_bind(id.get());
        builder.push(" RETURNING *");

        builder
            .build_query_as::<Draft>()
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| not_found(format!("draft {id}")))
    }

    /// Delete a draft. Returns `false` when it did not exist.
    pub async fn delete(&self, id: DraftId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM drafts WHERE id = $1")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// How many drafts an account holds.
    pub async fn count_for_user(&self, user_id: UserId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drafts WHERE user_id = $1")
            .bind(user_id.get())
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }
}

/// Login attempts, for throttling.
#[derive(Debug, Clone)]
pub struct LoginAttemptsRepository {
    pool: PgPool,
}

impl LoginAttemptsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Record one login attempt, successful or not.
    ///
    /// The address is lower-cased on the way in, so `Alice@…` and `alice@…` count
    /// against the same throttle bucket; otherwise the attacker would just vary case.
    pub async fn record(
        &self,
        email: &str,
        ip: Option<&str>,
        kind: &str,
        success: bool,
    ) -> Result<LoginAttempt> {
        Ok(sqlx::query_as::<_, LoginAttempt>(
            "INSERT INTO login_attempts (email, ip, kind, success) VALUES ($1, $2, $3, $4)
             RETURNING *",
        )
        .bind(normalise(email))
        .bind(ip)
        .bind(kind)
        .bind(success)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Failed attempts for one address since `since`.
    pub async fn count_failures_for_email(&self, email: &str, since: DateTime<Utc>) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM login_attempts
              WHERE email = $1 AND NOT success AND created_at >= $2",
        )
        .bind(normalise(email))
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Failed attempts from one peer address since `since`.
    pub async fn count_failures_for_ip(&self, ip: &str, since: DateTime<Utc>) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM login_attempts
              WHERE ip = $1 AND NOT success AND created_at >= $2",
        )
        .bind(ip)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// The most recent attempts for one address, newest first.
    pub async fn recent_for_email(&self, email: &str, limit: i64) -> Result<Vec<LoginAttempt>> {
        Ok(sqlx::query_as::<_, LoginAttempt>(
            "SELECT * FROM login_attempts WHERE email = $1
              ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(normalise(email))
        .bind(limit_of(limit))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Forget attempts older than `cutoff`.
    pub async fn prune_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let done = sqlx::query("DELETE FROM login_attempts WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// DB-backed settings.
#[derive(Debug, Clone)]
pub struct SettingsRepository {
    pool: PgPool,
}

impl SettingsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Read one setting. `None` when it was never written, so the caller can fall
    /// back to the `ferroma.toml` default.
    pub async fn get(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let row: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT value FROM settings WHERE key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(value,)| value))
    }

    /// Write (or overwrite) one setting.
    pub async fn set(&self, key: &str, value: serde_json::Value) -> Result<()> {
        if key.trim().is_empty() {
            return Err(StorageError::Invalid("setting key must not be blank".into()));
        }
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()",
        )
        .bind(key.trim())
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every setting, alphabetical.
    pub async fn all(&self) -> Result<Vec<Setting>> {
        Ok(sqlx::query_as::<_, Setting>("SELECT * FROM settings ORDER BY key ASC")
            .fetch_all(&self.pool)
            .await?)
    }

    /// Delete one setting, falling back to the configured default.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        let done = sqlx::query("DELETE FROM settings WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}
