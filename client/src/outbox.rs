//! The Outbox: the eight-state send pipeline of specification §28.
//!
//! ```text
//! Compose → Draft → Pending → Uploading → Queued → Sending → Sent
//!                              ↘ Failed ↙        ↘ Retrying ↗
//! ```
//!
//! The state is persisted in SQLite (`outbox.state`), so closing the client in
//! the middle of a send — or losing power — resumes exactly where it stopped.
//! Only the transitions in [`OutboxState::can_transition_to`] are legal; every
//! other one is refused with [`ClientError::Conflict`], which keeps a UI bug from
//! silently resurrecting a sent message or skipping the upload of an attachment.
//!
//! The view the user sees is [`OutboxCounts::summary`]: `发件箱: sending 2,
//! failed 1, sent 152`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferroma_core::OperationId;
use sqlx::Row;

use crate::database::ClientDatabase;
use crate::error::{ClientError, ClientResult};
use crate::util::{now_rfc3339, parse_rfc3339};

/// The eight states of a message on its way out (§28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboxState {
    /// Composed but not yet queued for sending; the user can still edit it.
    Draft,
    /// Queued to send: durable, waiting for the network.
    Pending,
    /// Uploading this message's attachments.
    Uploading,
    /// Handed to the server's mail queue.
    Queued,
    /// The server is delivering it.
    Sending,
    /// Delivered to every recipient. Terminal.
    Sent,
    /// Given up. The message and its text are kept so nothing is lost.
    Failed,
    /// A previous attempt failed temporarily; another is scheduled.
    Retrying,
}

impl OutboxState {
    /// Every state, in pipeline order.
    pub const ALL: [OutboxState; 8] = [
        OutboxState::Draft,
        OutboxState::Pending,
        OutboxState::Uploading,
        OutboxState::Queued,
        OutboxState::Sending,
        OutboxState::Sent,
        OutboxState::Failed,
        OutboxState::Retrying,
    ];

    /// The string stored in `outbox.state`.
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxState::Draft => "draft",
            OutboxState::Pending => "pending",
            OutboxState::Uploading => "uploading",
            OutboxState::Queued => "queued",
            OutboxState::Sending => "sending",
            OutboxState::Sent => "sent",
            OutboxState::Failed => "failed",
            OutboxState::Retrying => "retrying",
        }
    }

    /// Parse a `state` column back into an enum.
    pub fn parse(raw: &str) -> ClientResult<Self> {
        Ok(match raw {
            "draft" => OutboxState::Draft,
            "pending" => OutboxState::Pending,
            "uploading" => OutboxState::Uploading,
            "queued" => OutboxState::Queued,
            "sending" => OutboxState::Sending,
            "sent" => OutboxState::Sent,
            "failed" => OutboxState::Failed,
            "retrying" => OutboxState::Retrying,
            other => {
                return Err(ClientError::cache(format!(
                    "unknown outbox state {other:?}"
                )))
            }
        })
    }

    /// Whether the message has left the pipeline for good.
    ///
    /// A `Sent` row is history; a `Failed` row is still the user's, and can be
    /// retried or turned back into a draft.
    pub fn is_terminal(self) -> bool {
        matches!(self, OutboxState::Sent)
    }

    /// Whether a restart has to pick this row up again.
    pub fn needs_resume(self) -> bool {
        !matches!(self, OutboxState::Sent | OutboxState::Failed)
    }

    /// The legal transitions out of this state.
    ///
    /// The table is deliberately small and explicit; anything not listed here is
    /// a bug in the caller, not a state the server can produce:
    ///
    /// | From | To |
    /// |---|---|
    /// | `Draft` | `Pending` |
    /// | `Pending` | `Uploading`, `Queued`, `Failed`, `Draft` |
    /// | `Uploading` | `Queued`, `Failed`, `Retrying` |
    /// | `Queued` | `Sending`, `Failed`, `Draft` |
    /// | `Sending` | `Sent`, `Retrying`, `Failed` |
    /// | `Sent` | — (terminal) |
    /// | `Failed` | `Pending`, `Draft` |
    /// | `Retrying` | `Sending`, `Failed` |
    pub fn can_transition_to(self, next: OutboxState) -> bool {
        use OutboxState::*;
        match self {
            Draft => matches!(next, Pending),
            Pending => matches!(next, Uploading | Queued | Failed | Draft),
            Uploading => matches!(next, Queued | Failed | Retrying),
            Queued => matches!(next, Sending | Failed | Draft),
            Sending => matches!(next, Sent | Retrying | Failed),
            Sent => false,
            Failed => matches!(next, Pending | Draft),
            Retrying => matches!(next, Sending | Failed),
        }
    }
}

impl std::fmt::Display for OutboxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One message in the outbox.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxItem {
    /// Local row id.
    pub id: i64,
    /// The account that owns it.
    pub account_id: i64,
    /// The idempotency key, generated when the draft entered the outbox.
    pub operation_id: String,
    /// Where it is in the pipeline.
    pub state: OutboxState,
    /// The address to send from.
    pub mailbox_id: Option<i64>,
    /// The `From:` address.
    pub from_address: String,
    /// The `To:` addresses.
    pub to: Vec<String>,
    /// The `Cc:` addresses.
    pub cc: Vec<String>,
    /// The `Bcc:` addresses.
    pub bcc: Vec<String>,
    /// The subject.
    pub subject: String,
    /// The plain-text body.
    pub text_body: Option<String>,
    /// The HTML body.
    pub html_body: Option<String>,
    /// The attachments, by server id once uploaded.
    pub attachment_ids: Vec<i64>,
    /// The `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// The `References` header.
    pub references: Vec<String>,
    /// When the item was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed state.
    pub updated_at: DateTime<Utc>,
    /// How many delivery attempts have been made.
    pub attempts: u32,
    /// The last failure, for the UI.
    pub last_error: Option<String>,
    /// The stored copy in the Sent folder, once the server answered.
    pub server_message_id: Option<i64>,
    /// How many recipients were enqueued.
    pub queued_recipients: Option<i64>,
    /// When the server accepted it.
    pub sent_at: Option<DateTime<Utc>>,
}

impl OutboxItem {
    /// A one-line description for the Outbox list.
    pub fn summary(&self) -> String {
        if self.subject.trim().is_empty() {
            format!("(no subject) → {}", self.to.join(", "))
        } else {
            format!("{} → {}", self.subject, self.to.join(", "))
        }
    }

    /// The idempotency key as the core type.
    pub fn operation_key(&self) -> OperationId {
        OperationId::new(self.operation_id.clone())
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> ClientResult<Self> {
        let state: String = row.try_get("state")?;
        let parse_list = |raw: String| -> Vec<String> {
            serde_json::from_str(&raw).unwrap_or_default()
        };
        let parse_ids = |raw: String| -> Vec<i64> { serde_json::from_str(&raw).unwrap_or_default() };
        Ok(OutboxItem {
            id: row.try_get("id")?,
            account_id: row.try_get("account_id")?,
            operation_id: row.try_get("operation_id")?,
            state: OutboxState::parse(&state)?,
            mailbox_id: row.try_get("mailbox_id")?,
            from_address: row.try_get("from_address")?,
            to: parse_list(row.try_get("to_json")?),
            cc: parse_list(row.try_get("cc_json")?),
            bcc: parse_list(row.try_get("bcc_json")?),
            subject: row.try_get("subject")?,
            text_body: row.try_get("text_body")?,
            html_body: row.try_get("html_body")?,
            attachment_ids: parse_ids(row.try_get("attachment_ids_json")?),
            in_reply_to: row.try_get("in_reply_to")?,
            references: parse_list(row.try_get("references_json")?),
            created_at: parse_rfc3339(&row.try_get::<String, _>("created_at")?)
                .unwrap_or_else(Utc::now),
            updated_at: parse_rfc3339(&row.try_get::<String, _>("updated_at")?)
                .unwrap_or_else(Utc::now),
            attempts: row.try_get::<i64, _>("attempts")?.max(0) as u32,
            last_error: row.try_get("last_error")?,
            server_message_id: row.try_get("server_message_id")?,
            queued_recipients: row.try_get("queued_recipients")?,
            sent_at: row
                .try_get::<Option<String>, _>("sent_at")?
                .and_then(|raw| parse_rfc3339(&raw)),
        })
    }
}

/// A message being put into the outbox.
#[derive(Debug, Clone)]
pub struct NewOutboxItem {
    /// The owning account.
    pub account_id: i64,
    /// Which address to send from.
    pub mailbox_id: Option<i64>,
    /// The `From:` address.
    pub from_address: String,
    /// The `To:` addresses.
    pub to: Vec<String>,
    /// The `Cc:` addresses.
    pub cc: Vec<String>,
    /// The `Bcc:` addresses.
    pub bcc: Vec<String>,
    /// The subject.
    pub subject: String,
    /// The plain-text body.
    pub text_body: Option<String>,
    /// The HTML body.
    pub html_body: Option<String>,
    /// Attachments already uploaded.
    pub attachment_ids: Vec<i64>,
    /// The `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// The `References` header.
    pub references: Vec<String>,
    /// Whether to stop at `Draft` (the user pressed "save") or go to `Pending`
    /// (the user pressed "send").
    pub send_now: bool,
}

impl NewOutboxItem {
    /// A minimal message to `to` with `subject` and `text`.
    pub fn simple(account_id: i64, from: &str, to: &[String], subject: &str, text: &str) -> Self {
        NewOutboxItem {
            account_id,
            mailbox_id: None,
            from_address: from.to_string(),
            to: to.to_vec(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.to_string(),
            text_body: Some(text.to_string()),
            html_body: None,
            attachment_ids: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            send_now: true,
        }
    }
}

/// How many messages sit in each state — the Outbox header of §28.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboxCounts {
    /// Composed but not queued.
    pub draft: i64,
    /// Queued, waiting for the network.
    pub pending: i64,
    /// Uploading attachments.
    pub uploading: i64,
    /// Handed to the server's queue.
    pub queued: i64,
    /// Being delivered by the server.
    pub sending: i64,
    /// Delivered.
    pub sent: i64,
    /// Given up.
    pub failed: i64,
    /// Waiting for another attempt.
    pub retrying: i64,
}

impl OutboxCounts {
    /// Everything that is on its way: `Pending + Uploading + Queued + Sending +
    /// Retrying`.
    ///
    /// This is the number the Outbox shows as "正在发送": a message the user
    /// pressed *send* on but that has not reached the server yet is exactly what
    /// they mean by "sending".
    pub fn in_flight(&self) -> i64 {
        self.pending + self.uploading + self.queued + self.sending + self.retrying
    }

    /// The one-line header of §28: `sending 2, failed 1, sent 152`.
    pub fn summary(&self) -> String {
        format!(
            "sending {}, failed {}, sent {}",
            self.in_flight(),
            self.failed,
            self.sent
        )
    }

    /// The count for one state.
    pub fn get(&self, state: OutboxState) -> i64 {
        match state {
            OutboxState::Draft => self.draft,
            OutboxState::Pending => self.pending,
            OutboxState::Uploading => self.uploading,
            OutboxState::Queued => self.queued,
            OutboxState::Sending => self.sending,
            OutboxState::Sent => self.sent,
            OutboxState::Failed => self.failed,
            OutboxState::Retrying => self.retrying,
        }
    }
}

/// The persisted Outbox.
#[derive(Debug, Clone)]
pub struct Outbox {
    db: Arc<ClientDatabase>,
}

impl Outbox {
    /// Wrap a cache.
    pub fn new(db: Arc<ClientDatabase>) -> Self {
        Outbox { db }
    }

    /// The cache behind the outbox.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// Put a message in the outbox.
    ///
    /// The row — including its `operation_id` — is committed **before** anything
    /// touches the network, which is what makes "nothing the user typed is lost"
    /// true across a crash (`docs/fcp.md` §11).
    pub async fn enqueue(&self, item: NewOutboxItem) -> ClientResult<OutboxItem> {
        let operation_id = OperationId::generate();
        self.enqueue_with_id(item, operation_id).await
    }

    /// Put a message in the outbox with a caller-supplied idempotency key.
    pub async fn enqueue_with_id(
        &self,
        item: NewOutboxItem,
        operation_id: OperationId,
    ) -> ClientResult<OutboxItem> {
        let state = if item.send_now {
            OutboxState::Pending
        } else {
            OutboxState::Draft
        };
        let now = now_rfc3339();
        let result = sqlx::query(
            "INSERT INTO outbox (account_id, operation_id, state, mailbox_id, from_address,
                                 to_json, cc_json, bcc_json, subject, text_body, html_body,
                                 attachment_ids_json, in_reply_to, references_json,
                                 created_at, updated_at, attempts)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(item.account_id)
        .bind(operation_id.as_str())
        .bind(state.as_str())
        .bind(item.mailbox_id)
        .bind(&item.from_address)
        .bind(serde_json::to_string(&item.to)?)
        .bind(serde_json::to_string(&item.cc)?)
        .bind(serde_json::to_string(&item.bcc)?)
        .bind(&item.subject)
        .bind(&item.text_body)
        .bind(&item.html_body)
        .bind(serde_json::to_string(&item.attachment_ids)?)
        .bind(&item.in_reply_to)
        .bind(serde_json::to_string(&item.references)?)
        .bind(&now)
        .bind(&now)
        .execute(self.db.pool())
        .await?;

        self.get(result.last_insert_rowid())
            .await?
            .ok_or_else(|| ClientError::cache("the outbox row vanished right after insert"))
    }

    /// One item by row id.
    pub async fn get(&self, id: i64) -> ClientResult<Option<OutboxItem>> {
        let row = sqlx::query("SELECT * FROM outbox WHERE id = ?")
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?;
        match row {
            Some(row) => Ok(Some(OutboxItem::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// One item by idempotency key.
    pub async fn find_by_operation(&self, operation_id: &str) -> ClientResult<Option<OutboxItem>> {
        let row = sqlx::query("SELECT * FROM outbox WHERE operation_id = ?")
            .bind(operation_id)
            .fetch_optional(self.db.pool())
            .await?;
        match row {
            Some(row) => Ok(Some(OutboxItem::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Everything in the outbox, newest first.
    pub async fn list(&self, account_id: i64) -> ClientResult<Vec<OutboxItem>> {
        let rows = sqlx::query("SELECT * FROM outbox WHERE account_id = ? ORDER BY id DESC")
            .bind(account_id)
            .fetch_all(self.db.pool())
            .await?;
        rows.iter().map(OutboxItem::from_row).collect()
    }

    /// Everything in one state, oldest first (the order it must be sent in).
    pub async fn list_in_state(
        &self,
        account_id: i64,
        state: OutboxState,
    ) -> ClientResult<Vec<OutboxItem>> {
        let rows = sqlx::query(
            "SELECT * FROM outbox WHERE account_id = ? AND state = ? ORDER BY id ASC",
        )
        .bind(account_id)
        .bind(state.as_str())
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(OutboxItem::from_row).collect()
    }

    /// Everything a restart has to pick up again.
    ///
    /// `Sent` is history and `Failed` is waiting for the user, so neither is
    /// resumed automatically.
    pub async fn resumable(&self, account_id: i64) -> ClientResult<Vec<OutboxItem>> {
        let rows = sqlx::query(
            "SELECT * FROM outbox WHERE account_id = ? AND state NOT IN ('sent', 'failed')
             ORDER BY id ASC",
        )
        .bind(account_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(OutboxItem::from_row).collect()
    }

    /// Move an item to `next`, refusing illegal transitions.
    ///
    /// The check and the write happen in one transaction so two concurrent
    /// senders cannot both drive the same row.
    pub async fn transition(&self, id: i64, next: OutboxState) -> ClientResult<OutboxItem> {
        let mut tx = self.db.pool().begin().await?;
        let row = sqlx::query("SELECT state FROM outbox WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")))?;
        let current = OutboxState::parse(&row.try_get::<String, _>("state")?)?;
        if !current.can_transition_to(next) {
            return Err(ClientError::conflict(format!(
                "an outbox item cannot go from {current} to {next}"
            )));
        }
        sqlx::query("UPDATE outbox SET state = ?, updated_at = ? WHERE id = ?")
            .bind(next.as_str())
            .bind(now_rfc3339())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        self.get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")))
    }

    /// Record a failed attempt and move to `Retrying` when the failure is
    /// temporary, or to `Failed` when it is not.
    ///
    /// Returns the resulting state.
    pub async fn record_failure(
        &self,
        id: i64,
        error: &str,
        retryable: bool,
    ) -> ClientResult<OutboxState> {
        let item = self
            .get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")))?;
        let next = if retryable {
            OutboxState::Retrying
        } else {
            OutboxState::Failed
        };
        let next = if item.state.can_transition_to(next) {
            next
        } else if item.state.can_transition_to(OutboxState::Failed) {
            // `Retrying` may not be reachable from every state (a `Pending` item
            // that fails validation, for instance).
            OutboxState::Failed
        } else {
            item.state
        };

        sqlx::query(
            "UPDATE outbox SET state = ?, attempts = attempts + 1, last_error = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(next.as_str())
        .bind(error)
        .bind(now_rfc3339())
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(next)
    }

    /// Record that the server accepted the message.
    ///
    /// Legal from `Sending` (and, for a short-circuit path, from `Queued`); a
    /// `Sent` row is terminal.
    pub async fn mark_sent(
        &self,
        id: i64,
        server_message_id: Option<i64>,
        queued_recipients: Option<i64>,
    ) -> ClientResult<OutboxItem> {
        let now = now_rfc3339();
        let mut tx = self.db.pool().begin().await?;
        let row = sqlx::query("SELECT state FROM outbox WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")))?;
        let current = OutboxState::parse(&row.try_get::<String, _>("state")?)?;
        if current == OutboxState::Sent {
            // Idempotent: a replay of the same send must not fail.
            drop(tx);
            return self
                .get(id)
                .await?
                .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")));
        }
        if !current.can_transition_to(OutboxState::Sent) {
            return Err(ClientError::conflict(format!(
                "an outbox item cannot go from {current} to sent"
            )));
        }
        sqlx::query(
            "UPDATE outbox SET state = 'sent', server_message_id = ?, queued_recipients = ?,
             sent_at = ?, updated_at = ?, last_error = NULL WHERE id = ?",
        )
        .bind(server_message_id)
        .bind(queued_recipients)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")))
    }

    /// Edit a message that has not been queued yet.
    ///
    /// Only `Draft` and `Failed` are editable: once something is `Pending` the
    /// user must cancel it first, so a half-sent message can never change under
    /// the sender's feet.
    pub async fn update_draft(
        &self,
        id: i64,
        subject: &str,
        text_body: Option<&str>,
        html_body: Option<&str>,
        to: &[String],
    ) -> ClientResult<OutboxItem> {
        let item = self
            .get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")))?;
        if !matches!(item.state, OutboxState::Draft | OutboxState::Failed) {
            return Err(ClientError::conflict(format!(
                "an outbox item in state {} is no longer editable",
                item.state
            )));
        }
        sqlx::query(
            "UPDATE outbox SET subject = ?, text_body = ?, html_body = ?, to_json = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(subject)
        .bind(text_body)
        .bind(html_body)
        .bind(serde_json::to_string(to)?)
        .bind(now_rfc3339())
        .bind(id)
        .execute(self.db.pool())
        .await?;
        self.get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no outbox item {id}")))
    }

    /// Forget an item for good.
    pub async fn remove(&self, id: i64) -> ClientResult<bool> {
        let result = sqlx::query("DELETE FROM outbox WHERE id = ?")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The counts of §28.
    pub async fn counts(&self, account_id: i64) -> ClientResult<OutboxCounts> {
        let rows = sqlx::query("SELECT state, COUNT(*) AS n FROM outbox WHERE account_id = ? GROUP BY state")
            .bind(account_id)
            .fetch_all(self.db.pool())
            .await?;
        let mut counts = OutboxCounts::default();
        for row in rows {
            let state = OutboxState::parse(&row.try_get::<String, _>("state")?)?;
            let n: i64 = row.try_get("n")?;
            match state {
                OutboxState::Draft => counts.draft = n,
                OutboxState::Pending => counts.pending = n,
                OutboxState::Uploading => counts.uploading = n,
                OutboxState::Queued => counts.queued = n,
                OutboxState::Sending => counts.sending = n,
                OutboxState::Sent => counts.sent = n,
                OutboxState::Failed => counts.failed = n,
                OutboxState::Retrying => counts.retrying = n,
            }
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    async fn outbox() -> (TempDir, Outbox) {
        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(
            ClientDatabase::open(dir.path().join("cache.db"))
                .await
                .expect("open"),
        );
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (1, 'alice@example.com', 'http://x/api/v1/client', 'dev', 'now', 'now')",
        )
        .execute(db.pool())
        .await
        .expect("account");
        (dir, Outbox::new(db))
    }

    fn item(state: OutboxState) -> NewOutboxItem {
        NewOutboxItem {
            account_id: 1,
            mailbox_id: None,
            from_address: "alice@example.com".into(),
            to: vec!["bob@example.net".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Invoice".into(),
            text_body: Some("hi".into()),
            html_body: None,
            attachment_ids: vec![],
            in_reply_to: None,
            references: vec![],
            send_now: state == OutboxState::Pending,
        }
    }

    #[test]
    fn every_state_round_trips_through_its_string() {
        for state in OutboxState::ALL {
            assert_eq!(OutboxState::parse(state.as_str()).expect("parse"), state);
            assert_eq!(state.to_string(), state.as_str());
        }
        assert!(OutboxState::parse("teleporting").is_err());
    }

    #[test]
    fn every_legal_transition_is_allowed_and_every_illegal_one_is_not() {
        use OutboxState::*;
        let legal = [
            (Draft, Pending),
            (Pending, Uploading),
            (Pending, Queued),
            (Pending, Failed),
            (Pending, Draft),
            (Uploading, Queued),
            (Uploading, Failed),
            (Uploading, Retrying),
            (Queued, Sending),
            (Queued, Failed),
            (Queued, Draft),
            (Sending, Sent),
            (Sending, Retrying),
            (Sending, Failed),
            (Failed, Pending),
            (Failed, Draft),
            (Retrying, Sending),
            (Retrying, Failed),
        ];
        for from in OutboxState::ALL {
            for to in OutboxState::ALL {
                let expected = legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{from} -> {to} should be {}",
                    if expected { "legal" } else { "illegal" }
                );
            }
        }
    }

    #[test]
    fn terminal_states_are_exactly_sent() {
        assert!(OutboxState::Sent.is_terminal());
        for state in OutboxState::ALL {
            if state != OutboxState::Sent {
                assert!(!state.is_terminal(), "{state} must not be terminal");
            }
        }
        assert!(!OutboxState::Failed.needs_resume(), "failed waits for the user");
        assert!(OutboxState::Sending.needs_resume());
        assert!(!OutboxState::Sent.needs_resume());
    }

    #[tokio::test]
    async fn a_new_message_starts_pending_or_draft() {
        let (_dir, outbox) = outbox().await;
        let pending = outbox.enqueue(item(OutboxState::Pending)).await.expect("send");
        assert_eq!(pending.state, OutboxState::Pending);
        assert!(pending.operation_id.starts_with("op_"));

        let draft = outbox.enqueue(item(OutboxState::Draft)).await.expect("draft");
        assert_eq!(draft.state, OutboxState::Draft);
        assert_ne!(draft.operation_id, pending.operation_id);

        let listed = outbox.list(1).await.expect("list");
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn the_full_pipeline_can_be_walked() {
        let (_dir, outbox) = outbox().await;
        let sent = outbox.enqueue(item(OutboxState::Pending)).await.expect("enqueue");
        let uploading = outbox
            .transition(sent.id, OutboxState::Uploading)
            .await
            .expect("uploading");
        assert_eq!(uploading.state, OutboxState::Uploading);
        let queued = outbox
            .transition(sent.id, OutboxState::Queued)
            .await
            .expect("queued");
        assert_eq!(queued.state, OutboxState::Queued);
        let sending = outbox
            .transition(sent.id, OutboxState::Sending)
            .await
            .expect("sending");
        assert_eq!(sending.state, OutboxState::Sending);
        let done = outbox
            .mark_sent(sent.id, Some(99), Some(1))
            .await
            .expect("sent");
        assert_eq!(done.state, OutboxState::Sent);
        assert_eq!(done.server_message_id, Some(99));
        assert_eq!(done.queued_recipients, Some(1));
        assert!(done.sent_at.is_some());
    }

    #[tokio::test]
    async fn an_illegal_transition_is_refused_and_changes_nothing() {
        let (_dir, outbox) = outbox().await;
        let sent = outbox.enqueue(item(OutboxState::Pending)).await.expect("enqueue");
        // Pending -> Sent skips the whole upload/send pipeline.
        let err = outbox
            .transition(sent.id, OutboxState::Sent)
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ClientError::Conflict(_)));
        assert_eq!(
            outbox.get(sent.id).await.expect("get").expect("row").state,
            OutboxState::Pending
        );
    }

    #[tokio::test]
    async fn a_sent_message_can_never_move_again() {
        let (_dir, outbox) = outbox().await;
        let sent = outbox.enqueue(item(OutboxState::Pending)).await.expect("enqueue");
        outbox.transition(sent.id, OutboxState::Queued).await.expect("queued");
        outbox.transition(sent.id, OutboxState::Sending).await.expect("sending");
        outbox.mark_sent(sent.id, Some(1), Some(1)).await.expect("sent");
        for next in OutboxState::ALL {
            let err = outbox.transition(sent.id, next).await.expect_err("must refuse");
            assert!(matches!(err, ClientError::Conflict(_)), "{next} was allowed");
        }
    }

    #[tokio::test]
    async fn marking_sent_twice_is_idempotent() {
        let (_dir, outbox) = outbox().await;
        let sent = outbox.enqueue(item(OutboxState::Pending)).await.expect("enqueue");
        outbox.transition(sent.id, OutboxState::Queued).await.expect("queued");
        outbox.transition(sent.id, OutboxState::Sending).await.expect("sending");
        let first = outbox.mark_sent(sent.id, Some(1), Some(1)).await.expect("first");
        let second = outbox.mark_sent(sent.id, Some(1), Some(1)).await.expect("second");
        assert_eq!(first.state, OutboxState::Sent);
        assert_eq!(second.state, OutboxState::Sent);
    }

    #[tokio::test]
    async fn a_retryable_failure_moves_to_retrying_and_a_permanent_one_to_failed() {
        let (_dir, outbox) = outbox().await;
        let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
        outbox.transition(a.id, OutboxState::Uploading).await.expect("uploading");
        let state = outbox
            .record_failure(a.id, "connection reset", true)
            .await
            .expect("record");
        assert_eq!(state, OutboxState::Retrying);
        let reloaded = outbox.get(a.id).await.expect("get").expect("row");
        assert_eq!(reloaded.attempts, 1);
        assert_eq!(reloaded.last_error.as_deref(), Some("connection reset"));

        let b = outbox.enqueue(item(OutboxState::Pending)).await.expect("b");
        outbox.transition(b.id, OutboxState::Queued).await.expect("queued");
        let state = outbox
            .record_failure(b.id, "too big", false)
            .await
            .expect("record");
        assert_eq!(state, OutboxState::Failed);
    }

    #[tokio::test]
    async fn a_retrying_message_can_be_retried_and_finish() {
        let (_dir, outbox) = outbox().await;
        let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
        outbox.transition(a.id, OutboxState::Uploading).await.expect("uploading");
        outbox.record_failure(a.id, "reset", true).await.expect("fail");
        outbox.transition(a.id, OutboxState::Sending).await.expect("sending");
        let done = outbox.mark_sent(a.id, Some(7), Some(1)).await.expect("sent");
        assert_eq!(done.state, OutboxState::Sent);
    }

    #[tokio::test]
    async fn a_failed_message_can_be_queued_again_or_edited() {
        let (_dir, outbox) = outbox().await;
        let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
        outbox.transition(a.id, OutboxState::Queued).await.expect("queued");
        outbox.record_failure(a.id, "bounced", false).await.expect("fail");
        assert_eq!(
            outbox.get(a.id).await.expect("get").expect("row").state,
            OutboxState::Failed
        );

        let edited = outbox
            .update_draft(a.id, "New subject", Some("body"), None, &["carol@example.net".into()])
            .await
            .expect("edit");
        assert_eq!(edited.subject, "New subject");
        assert_eq!(edited.to, vec!["carol@example.net".to_string()]);
        assert_eq!(edited.html_body, None);

        let retried = outbox.transition(a.id, OutboxState::Pending).await.expect("retry");
        assert_eq!(retried.state, OutboxState::Pending);
    }

    #[tokio::test]
    async fn a_queued_message_is_no_longer_editable() {
        let (_dir, outbox) = outbox().await;
        let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
        let err = outbox
            .update_draft(a.id, "x", None, None, &[])
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ClientError::Conflict(_)));
    }

    #[tokio::test]
    async fn counts_and_the_summary_line_match_the_specification() {
        let (_dir, outbox) = outbox().await;
        // Two in flight, one failed, one sent.
        let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
        outbox.transition(a.id, OutboxState::Queued).await.expect("queued");
        let b = outbox.enqueue(item(OutboxState::Pending)).await.expect("b");
        outbox.transition(b.id, OutboxState::Queued).await.expect("queued");
        outbox.transition(b.id, OutboxState::Sending).await.expect("sending");
        let c = outbox.enqueue(item(OutboxState::Pending)).await.expect("c");
        outbox.transition(c.id, OutboxState::Queued).await.expect("queued");
        outbox.record_failure(c.id, "no route", false).await.expect("fail");
        let d = outbox.enqueue(item(OutboxState::Pending)).await.expect("d");
        outbox.transition(d.id, OutboxState::Queued).await.expect("queued");
        outbox.transition(d.id, OutboxState::Sending).await.expect("sending");
        outbox.mark_sent(d.id, Some(1), Some(1)).await.expect("sent");

        let counts = outbox.counts(1).await.expect("counts");
        assert_eq!(counts.pending, 0);
        assert_eq!(counts.queued, 1);
        assert_eq!(counts.sending, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.sent, 1);
        assert_eq!(counts.in_flight(), 2);
        assert_eq!(counts.summary(), "sending 2, failed 1, sent 1");
        assert_eq!(counts.get(OutboxState::Sent), 1);
    }

    #[tokio::test]
    async fn the_outbox_survives_a_reopen() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("cache.db");
        let id = {
            let db = Arc::new(ClientDatabase::open(&path).await.expect("open"));
            sqlx::query(
                "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
                 VALUES (1, 'a@b.c', 'http://x/api/v1/client', 'dev', 'now', 'now')",
            )
            .execute(db.pool())
            .await
            .expect("account");
            let outbox = Outbox::new(db.clone());
            let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
            outbox.transition(a.id, OutboxState::Uploading).await.expect("uploading");
            outbox.record_failure(a.id, "reset", true).await.expect("fail");
            db.close().await;
            a.id
        };

        let db = Arc::new(ClientDatabase::open(&path).await.expect("reopen"));
        let outbox = Outbox::new(db);
        let reloaded = outbox.get(id).await.expect("get").expect("row");
        assert_eq!(reloaded.state, OutboxState::Retrying);
        assert_eq!(reloaded.attempts, 1);
        assert_eq!(reloaded.subject, "Invoice");
        assert_eq!(reloaded.to, vec!["bob@example.net".to_string()]);
        assert_eq!(reloaded.text_body.as_deref(), Some("hi"));

        let resumable = outbox.resumable(1).await.expect("resumable");
        assert_eq!(resumable.len(), 1, "a restart resumes non-terminal rows");
    }

    #[tokio::test]
    async fn a_sent_message_is_not_resumed_after_a_restart() {
        let (_dir, outbox) = outbox().await;
        let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
        outbox.transition(a.id, OutboxState::Queued).await.expect("queued");
        outbox.transition(a.id, OutboxState::Sending).await.expect("sending");
        outbox.mark_sent(a.id, Some(1), Some(1)).await.expect("sent");
        assert!(outbox.resumable(1).await.expect("resumable").is_empty());
    }

    #[tokio::test]
    async fn removing_an_item_and_wiping_an_account_work() {
        let (_dir, outbox) = outbox().await;
        let a = outbox.enqueue(item(OutboxState::Pending)).await.expect("a");
        assert!(outbox.remove(a.id).await.expect("remove"));
        assert!(outbox.get(a.id).await.expect("get").is_none());
        assert!(!outbox.remove(a.id).await.expect("remove again"));

        outbox.enqueue(item(OutboxState::Pending)).await.expect("b");
        outbox.database().wipe_account(1).await.expect("wipe");
        assert!(outbox.list(1).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_missing_item_reports_not_found() {
        let (_dir, outbox) = outbox().await;
        let err = outbox.transition(4242, OutboxState::Pending).await.expect_err("missing");
        assert!(matches!(err, ClientError::NotFound(_)));
        assert!(outbox.get(4242).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn enqueueing_with_a_known_key_is_a_single_row() {
        let (_dir, outbox) = outbox().await;
        let key = OperationId::generate();
        let first = outbox
            .enqueue_with_id(item(OutboxState::Pending), key.clone())
            .await
            .expect("first");
        let found = outbox
            .find_by_operation(key.as_str())
            .await
            .expect("find")
            .expect("row");
        assert_eq!(found.id, first.id);
        assert_eq!(outbox.list(1).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn the_summary_line_names_the_recipients_when_there_is_no_subject() {
        let (_dir, outbox) = outbox().await;
        let mut new_item = item(OutboxState::Draft);
        new_item.subject = String::new();
        let draft = outbox.enqueue(new_item).await.expect("draft");
        assert_eq!(draft.summary(), "(no subject) → bob@example.net");
    }
}
