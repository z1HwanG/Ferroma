//! Local drafts that sync to the server (`docs/fcp.md` §7).
//!
//! Drafts are server-side so the same draft appears on every device, but the
//! client keeps an offline-first copy: text the user typed is written to SQLite
//! first and pushed afterwards. A draft carries a stable `local_uid` so the
//! compose window survives a restart even before the server has assigned an id.
//!
//! **Conflict policy — last write wins, and the client is told what it
//! overwrote** (§7):
//!
//! * pushing a dirty draft always wins locally, and the server's answer says
//!   whether it replaced a newer server-side version — that is returned as an
//!   [`OverwriteNotice`] so the UI can say "this draft was also edited on your
//!   phone; your version was kept";
//! * pulling never overwrites a draft that is still `dirty`: the user's unsent
//!   text outranks a change made elsewhere, because losing typed text is the one
//!   failure §11 forbids outright.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::api::{DraftPayload, FcpClient};
use crate::database::ClientDatabase;
use crate::error::{ClientError, ClientResult};
use crate::util::{now_rfc3339, parse_rfc3339};

/// A draft, as the client stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    /// The owning account.
    pub account_id: i64,
    /// A client-generated, stable identifier for this draft.
    pub local_uid: String,
    /// The server's id, once the draft has been pushed.
    pub server_id: Option<i64>,
    /// Which address it will be sent from.
    pub mailbox_id: Option<i64>,
    /// The subject.
    pub subject: String,
    /// The plain-text body.
    pub text_body: Option<String>,
    /// The HTML body.
    pub html_body: Option<String>,
    /// `To:` addresses.
    pub to: Vec<String>,
    /// `Cc:` addresses.
    pub cc: Vec<String>,
    /// `Bcc:` addresses.
    pub bcc: Vec<String>,
    /// The `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// The `References` header.
    pub references: Vec<String>,
    /// Already-uploaded attachments.
    pub attachment_ids: Vec<i64>,
    /// The `updated_at` the server last reported.
    pub server_updated_at: Option<DateTime<Utc>>,
    /// The last local edit.
    pub local_updated_at: DateTime<Utc>,
    /// Whether local edits have not been pushed yet.
    pub dirty: bool,
    /// Whether the server (or the user) deleted it.
    pub deleted: bool,
}

impl Draft {
    /// The subject, or a placeholder for the list view.
    pub fn display_subject(&self) -> &str {
        if self.subject.trim().is_empty() {
            "(no subject)"
        } else {
            &self.subject
        }
    }

    /// The recipients, as one line.
    pub fn recipients(&self) -> String {
        let mut all = self.to.clone();
        all.extend(self.cc.iter().cloned());
        all.extend(self.bcc.iter().cloned());
        all.join(", ")
    }

    /// The wire payload for `POST`/`PATCH /drafts`.
    pub fn payload(&self) -> DraftPayload {
        DraftPayload {
            subject: self.subject.clone(),
            text: self.text_body.clone(),
            html: self.html_body.clone(),
            to: self.to.clone(),
            cc: self.cc.clone(),
            bcc: self.bcc.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachment_ids: self.attachment_ids.clone(),
        }
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> ClientResult<Self> {
        let list = |raw: String| -> Vec<String> { serde_json::from_str(&raw).unwrap_or_default() };
        let ids = |raw: String| -> Vec<i64> { serde_json::from_str(&raw).unwrap_or_default() };
        Ok(Draft {
            account_id: row.try_get("account_id")?,
            local_uid: row.try_get("local_uid")?,
            server_id: row.try_get("id")?,
            mailbox_id: row.try_get("mailbox_id")?,
            subject: row.try_get("subject")?,
            text_body: row.try_get("text_body")?,
            html_body: row.try_get("html_body")?,
            to: list(row.try_get("to_json")?),
            cc: list(row.try_get("cc_json")?),
            bcc: list(row.try_get("bcc_json")?),
            in_reply_to: row.try_get("in_reply_to")?,
            references: list(row.try_get("references_json")?),
            attachment_ids: ids(row.try_get("attachment_ids_json")?),
            server_updated_at: row
                .try_get::<Option<String>, _>("server_updated_at")?
                .and_then(|raw| parse_rfc3339(&raw)),
            local_updated_at: parse_rfc3339(&row.try_get::<String, _>("local_updated_at")?)
                .unwrap_or_else(Utc::now),
            dirty: row.try_get::<i64, _>("dirty")? != 0,
            deleted: row.try_get::<i64, _>("deleted")? != 0,
        })
    }
}

/// The content of a draft the caller wants to store.
#[derive(Debug, Clone, Default)]
pub struct NewDraft {
    /// Which address to send from.
    pub mailbox_id: Option<i64>,
    /// The subject.
    pub subject: String,
    /// The plain-text body.
    pub text_body: Option<String>,
    /// The HTML body.
    pub html_body: Option<String>,
    /// `To:` addresses.
    pub to: Vec<String>,
    /// `Cc:` addresses.
    pub cc: Vec<String>,
    /// `Bcc:` addresses.
    pub bcc: Vec<String>,
    /// The `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// The `References` header.
    pub references: Vec<String>,
    /// Attachments to keep attached.
    pub attachment_ids: Vec<i64>,
}

/// What a push overwrote on the server (§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverwriteNotice {
    /// The server-side draft id.
    pub server_id: i64,
    /// The local edit that won.
    pub local_updated_at: DateTime<Utc>,
    /// The server version that was replaced, when the server reported one.
    pub server_updated_at: Option<DateTime<Utc>>,
}

impl OverwriteNotice {
    /// A sentence the UI can show verbatim.
    pub fn message(&self) -> String {
        match self.server_updated_at {
            Some(when) => format!(
                "this draft had also been changed elsewhere ({when}); your version was kept"
            ),
            None => "this draft had also been changed elsewhere; your version was kept".to_string(),
        }
    }
}

/// The result of pushing one draft.
#[derive(Debug, Clone, PartialEq)]
pub struct PushOutcome {
    /// The draft as it now stands locally.
    pub draft: Draft,
    /// Set when the push replaced a newer server-side copy.
    pub notice: Option<OverwriteNotice>,
}

/// Local draft CRUD that syncs to the server.
#[derive(Debug, Clone)]
pub struct DraftStore {
    db: Arc<ClientDatabase>,
    client: Option<FcpClient>,
}

impl DraftStore {
    /// A store with no server: everything stays local (`dirty` forever).
    pub fn new(db: Arc<ClientDatabase>) -> Self {
        DraftStore { db, client: None }
    }

    /// A store that can push and pull.
    pub fn with_client(db: Arc<ClientDatabase>, client: FcpClient) -> Self {
        DraftStore {
            db,
            client: Some(client),
        }
    }

    /// The cache behind the store.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// Store a new draft. Nothing is sent; call [`DraftStore::push`] for that.
    pub async fn create(&self, account_id: i64, draft: NewDraft) -> ClientResult<Draft> {
        let local_uid = format!("local_{}", uuid::Uuid::new_v4().simple());
        self.upsert(account_id, &local_uid, draft, true).await
    }

    /// Overwrite an existing draft's content.
    pub async fn update(
        &self,
        account_id: i64,
        local_uid: &str,
        draft: NewDraft,
    ) -> ClientResult<Draft> {
        if self.get(account_id, local_uid).await?.is_none() {
            return Err(ClientError::not_found(format!("no draft {local_uid}")));
        }
        self.upsert(account_id, local_uid, draft, true).await
    }

    async fn upsert(
        &self,
        account_id: i64,
        local_uid: &str,
        draft: NewDraft,
        dirty: bool,
    ) -> ClientResult<Draft> {
        let now = now_rfc3339();
        sqlx::query(
            "INSERT INTO drafts (account_id, local_uid, mailbox_id, subject, text_body, html_body,
                                 to_json, cc_json, bcc_json, in_reply_to, references_json,
                                 attachment_ids_json, local_updated_at, dirty, deleted)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
             ON CONFLICT(account_id, local_uid) DO UPDATE SET
                 mailbox_id = excluded.mailbox_id,
                 subject = excluded.subject,
                 text_body = excluded.text_body,
                 html_body = excluded.html_body,
                 to_json = excluded.to_json,
                 cc_json = excluded.cc_json,
                 bcc_json = excluded.bcc_json,
                 in_reply_to = excluded.in_reply_to,
                 references_json = excluded.references_json,
                 attachment_ids_json = excluded.attachment_ids_json,
                 local_updated_at = excluded.local_updated_at,
                 dirty = excluded.dirty,
                 deleted = 0",
        )
        .bind(account_id)
        .bind(local_uid)
        .bind(draft.mailbox_id)
        .bind(&draft.subject)
        .bind(&draft.text_body)
        .bind(&draft.html_body)
        .bind(serde_json::to_string(&draft.to)?)
        .bind(serde_json::to_string(&draft.cc)?)
        .bind(serde_json::to_string(&draft.bcc)?)
        .bind(&draft.in_reply_to)
        .bind(serde_json::to_string(&draft.references)?)
        .bind(serde_json::to_string(&draft.attachment_ids)?)
        .bind(&now)
        .bind(i64::from(dirty))
        .execute(self.db.pool())
        .await?;

        self.get(account_id, local_uid)
            .await?
            .ok_or_else(|| ClientError::cache("the draft row vanished after the write"))
    }

    /// One draft by its local uid.
    pub async fn get(&self, account_id: i64, local_uid: &str) -> ClientResult<Option<Draft>> {
        let row = sqlx::query("SELECT * FROM drafts WHERE account_id = ? AND local_uid = ?")
            .bind(account_id)
            .bind(local_uid)
            .fetch_optional(self.db.pool())
            .await?;
        match row {
            Some(row) => Ok(Some(Draft::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// One draft by its server id.
    pub async fn get_by_server_id(
        &self,
        account_id: i64,
        server_id: i64,
    ) -> ClientResult<Option<Draft>> {
        let row = sqlx::query("SELECT * FROM drafts WHERE account_id = ? AND id = ?")
            .bind(account_id)
            .bind(server_id)
            .fetch_optional(self.db.pool())
            .await?;
        match row {
            Some(row) => Ok(Some(Draft::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Every live draft of an account, newest first.
    pub async fn list(&self, account_id: i64) -> ClientResult<Vec<Draft>> {
        let rows = sqlx::query(
            "SELECT * FROM drafts WHERE account_id = ? AND deleted = 0 ORDER BY local_updated_at DESC",
        )
        .bind(account_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(Draft::from_row).collect()
    }

    /// The drafts that still need pushing.
    pub async fn dirty(&self, account_id: i64) -> ClientResult<Vec<Draft>> {
        let rows = sqlx::query(
            "SELECT * FROM drafts WHERE account_id = ? AND dirty = 1 AND deleted = 0
             ORDER BY local_updated_at ASC",
        )
        .bind(account_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(Draft::from_row).collect()
    }

    /// Delete a draft, locally and (when possible) on the server.
    ///
    /// The local delete wins even if the server call fails: the user asked for
    /// the draft to be gone, and leaving it in the list would be worse than a
    /// stale server copy that the next sync removes.
    pub async fn delete(&self, account_id: i64, local_uid: &str) -> ClientResult<()> {
        let draft = self
            .get(account_id, local_uid)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no draft {local_uid}")))?;

        sqlx::query("DELETE FROM drafts WHERE account_id = ? AND local_uid = ?")
            .bind(account_id)
            .bind(local_uid)
            .execute(self.db.pool())
            .await?;

        if let (Some(client), Some(server_id)) = (&self.client, draft.server_id) {
            if let Err(err) = client.delete_draft(server_id).await {
                if !matches!(err, ClientError::Api(ref api) if api.status == 404) {
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    /// Push one draft to the server.
    ///
    /// `POST /drafts` for a draft the server has never seen, `PATCH` afterwards.
    /// The §7 conflict answer becomes an [`OverwriteNotice`].
    pub async fn push(&self, account_id: i64, local_uid: &str) -> ClientResult<PushOutcome> {
        let client = self
            .client
            .clone()
            .ok_or_else(|| ClientError::Unsupported("this draft store has no server".into()))?;
        let draft = self
            .get(account_id, local_uid)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no draft {local_uid}")))?;
        let payload = draft.payload();

        let (server_id, server_updated_at, notice) = match draft.server_id {
            Some(server_id) => {
                let record = client.update_draft(server_id, &payload).await?;
                let notice = record.conflict.filter(|c| c.detected).map(|conflict| {
                    OverwriteNotice {
                        server_id,
                        local_updated_at: draft.local_updated_at,
                        server_updated_at: conflict
                            .server_updated_at
                            .as_deref()
                            .and_then(parse_rfc3339),
                    }
                });
                (record.id, record.updated_at, notice)
            }
            None => {
                let record = client.create_draft(&payload).await?;
                let notice = record.conflict.filter(|c| c.detected).map(|conflict| {
                    OverwriteNotice {
                        server_id: record.id,
                        local_updated_at: draft.local_updated_at,
                        server_updated_at: conflict
                            .server_updated_at
                            .as_deref()
                            .and_then(parse_rfc3339),
                    }
                });
                (record.id, record.updated_at, notice)
            }
        };

        sqlx::query(
            "UPDATE drafts SET id = ?, server_updated_at = ?, dirty = 0
             WHERE account_id = ? AND local_uid = ?",
        )
        .bind(server_id)
        .bind(&server_updated_at)
        .bind(account_id)
        .bind(local_uid)
        .execute(self.db.pool())
        .await?;

        let updated = self
            .get(account_id, local_uid)
            .await?
            .ok_or_else(|| ClientError::cache("the draft row vanished after the push"))?;
        Ok(PushOutcome {
            draft: updated,
            notice,
        })
    }

    /// Push every dirty draft, oldest edit first.
    ///
    /// A failure stops the pass so the order of edits is preserved; the drafts
    /// that were already pushed stay pushed.
    pub async fn push_all(&self, account_id: i64) -> ClientResult<Vec<PushOutcome>> {
        let mut outcomes = Vec::new();
        for draft in self.dirty(account_id).await? {
            outcomes.push(self.push(account_id, &draft.local_uid).await?);
        }
        Ok(outcomes)
    }

    /// Pull the server's drafts into the cache.
    ///
    /// A draft that is still `dirty` locally is **not** overwritten: the user's
    /// unsent text wins until it has been pushed. Everything else follows the
    /// server, which is the source of truth.
    pub async fn pull(&self, account_id: i64) -> ClientResult<usize> {
        let client = self
            .client
            .clone()
            .ok_or_else(|| ClientError::Unsupported("this draft store has no server".into()))?;
        let records = client.list_drafts().await?;
        let mut applied = 0usize;

        for record in records {
            let existing = self.get_by_server_id(account_id, record.id).await?;
            if let Some(existing) = &existing {
                if existing.dirty {
                    continue;
                }
            }
            let local_uid = existing
                .map(|draft| draft.local_uid)
                .unwrap_or_else(|| format!("srv:{}", record.id));
            let now = now_rfc3339();
            sqlx::query(
                "INSERT INTO drafts (account_id, local_uid, id, subject, text_body, html_body,
                                     to_json, cc_json, bcc_json, server_updated_at,
                                     local_updated_at, dirty, deleted)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0)
                 ON CONFLICT(account_id, local_uid) DO UPDATE SET
                     id = excluded.id,
                     subject = excluded.subject,
                     text_body = excluded.text_body,
                     html_body = excluded.html_body,
                     to_json = excluded.to_json,
                     cc_json = excluded.cc_json,
                     bcc_json = excluded.bcc_json,
                     server_updated_at = excluded.server_updated_at,
                     dirty = 0,
                     deleted = 0",
            )
            .bind(account_id)
            .bind(&local_uid)
            .bind(record.id)
            .bind(&record.subject)
            .bind(&record.text)
            .bind(&record.html)
            .bind(serde_json::to_string(&record.to)?)
            .bind(serde_json::to_string(&record.cc)?)
            .bind(serde_json::to_string(&record.bcc)?)
            .bind(&record.updated_at)
            .bind(&now)
            .execute(self.db.pool())
            .await?;
            applied += 1;
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RetryPolicy;
    use crate::testutil::{MockResponse, MockServer, TempDir};

    async fn fixture() -> (TempDir, Arc<ClientDatabase>) {
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
        (dir, db)
    }

    fn new_draft(subject: &str) -> NewDraft {
        NewDraft {
            mailbox_id: Some(3),
            subject: subject.to_string(),
            text_body: Some("hello".into()),
            html_body: None,
            to: vec!["bob@example.net".into()],
            cc: vec![],
            bcc: vec![],
            in_reply_to: None,
            references: vec![],
            attachment_ids: vec![],
        }
    }

    async fn client_for(server: &MockServer) -> FcpClient {
        let client = FcpClient::with_retry(
            server.fcp_base_url(),
            Arc::new(crate::api::MemoryTokenStore::new()),
            RetryPolicy::none(),
        )
        .expect("client");
        client
            .set_access_token("t", std::time::Duration::from_secs(60))
            .await;
        client
    }

    #[tokio::test]
    async fn a_new_draft_is_stored_locally_and_marked_dirty() {
        let (_dir, db) = fixture().await;
        let store = DraftStore::new(db);
        let draft = store.create(1, new_draft("Invoice")).await.expect("create");
        assert!(draft.local_uid.starts_with("local_"));
        assert!(draft.server_id.is_none());
        assert!(draft.dirty);
        assert!(!draft.deleted);
        assert_eq!(draft.to, vec!["bob@example.net".to_string()]);
        assert_eq!(store.list(1).await.expect("list").len(), 1);
        assert_eq!(store.dirty(1).await.expect("dirty").len(), 1);
    }

    #[tokio::test]
    async fn editing_a_draft_keeps_its_identity() {
        let (_dir, db) = fixture().await;
        let store = DraftStore::new(db);
        let draft = store.create(1, new_draft("first")).await.expect("create");
        let updated = store
            .update(1, &draft.local_uid, new_draft("second"))
            .await
            .expect("update");
        assert_eq!(updated.local_uid, draft.local_uid);
        assert_eq!(updated.subject, "second");
        assert_eq!(store.list(1).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn editing_an_unknown_draft_is_not_found() {
        let (_dir, db) = fixture().await;
        let store = DraftStore::new(db);
        let err = store
            .update(1, "nope", new_draft("x"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, ClientError::NotFound(_)));
    }

    #[tokio::test]
    async fn deleting_a_local_draft_needs_no_server() {
        let (_dir, db) = fixture().await;
        let store = DraftStore::new(db);
        let draft = store.create(1, new_draft("bye")).await.expect("create");
        store.delete(1, &draft.local_uid).await.expect("delete");
        assert!(store.list(1).await.expect("list").is_empty());
        assert!(store
            .delete(1, &draft.local_uid)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_draft_survives_a_reopen() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("cache.db");
        let local_uid = {
            let db = Arc::new(ClientDatabase::open(&path).await.expect("open"));
            sqlx::query(
                "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
                 VALUES (1, 'a@b.c', 'http://x/api/v1/client', 'dev', 'now', 'now')",
            )
            .execute(db.pool())
            .await
            .expect("account");
            let store = DraftStore::new(db.clone());
            let draft = store.create(1, new_draft("draft")).await.expect("create");
            db.close().await;
            draft.local_uid
        };
        let db = Arc::new(ClientDatabase::open(&path).await.expect("reopen"));
        let store = DraftStore::new(db);
        let reloaded = store
            .get(1, &local_uid)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(reloaded.subject, "draft");
        assert!(reloaded.dirty, "unpushed text is still pending after a restart");
    }

    #[tokio::test]
    async fn pushing_creates_then_updates_on_the_server() {
        let (_dir, db) = fixture().await;
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/drafts",
            200,
            r#"{"id":44,"updated_at":"2026-09-16T12:00:00Z"}"#,
        );
        server.json_route(
            "PATCH",
            "/api/v1/client/drafts/44",
            200,
            r#"{"id":44,"updated_at":"2026-09-16T12:05:00Z"}"#,
        );
        let store = DraftStore::with_client(db, client_for(&server).await);
        let draft = store.create(1, new_draft("Invoice")).await.expect("create");

        let first = store.push(1, &draft.local_uid).await.expect("push");
        assert_eq!(first.draft.server_id, Some(44));
        assert!(!first.draft.dirty);
        assert!(first.notice.is_none());
        assert_eq!(server.count_for("/api/v1/client/drafts"), 1);

        store
            .update(1, &draft.local_uid, new_draft("Invoice v2"))
            .await
            .expect("update");
        let second = store.push(1, &draft.local_uid).await.expect("push again");
        assert_eq!(second.draft.server_id, Some(44));
        assert_eq!(server.count_for("/api/v1/client/drafts/44"), 1);

        let body = server
            .requests_for("/api/v1/client/drafts/44")
            .remove(0)
            .json();
        assert_eq!(body["subject"], "Invoice v2");
        assert_eq!(body["to"][0], "bob@example.net");
    }

    #[tokio::test]
    async fn pushing_reports_what_it_overwrote() {
        let (_dir, db) = fixture().await;
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/drafts",
            200,
            r#"{"id":44,"updated_at":"2026-09-16T12:00:01Z",
                "conflict":{"detected":true,"server_updated_at":"2026-09-16T11:59:58Z"}}"#,
        );
        let store = DraftStore::with_client(db, client_for(&server).await);
        let draft = store.create(1, new_draft("mine")).await.expect("create");
        let outcome = store.push(1, &draft.local_uid).await.expect("push");

        assert!(!outcome.draft.dirty, "the local write won and is now the server's");
        let notice = outcome.notice.expect("the client must be told");
        assert_eq!(notice.server_id, 44);
        assert_eq!(
            notice.server_updated_at.map(|dt| dt.to_rfc3339()),
            Some("2026-09-16T11:59:58+00:00".to_string())
        );
        assert!(notice.message().contains("your version was kept"));
    }

    #[tokio::test]
    async fn pushing_without_a_server_is_unsupported() {
        let (_dir, db) = fixture().await;
        let store = DraftStore::new(db);
        let draft = store.create(1, new_draft("x")).await.expect("create");
        let err = store.push(1, &draft.local_uid).await.expect_err("must fail");
        assert!(matches!(err, ClientError::Unsupported(_)));
    }

    #[tokio::test]
    async fn pulling_adopts_the_servers_drafts() {
        let (_dir, db) = fixture().await;
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/drafts",
            200,
            r#"[{"id":44,"subject":"from the phone","text":"hi","to":["carol@example.net"],
                 "updated_at":"2026-09-16T12:00:00Z"}]"#,
        );
        let store = DraftStore::with_client(db, client_for(&server).await);
        let applied = store.pull(1).await.expect("pull");
        assert_eq!(applied, 1);

        let listed = store.list(1).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].server_id, Some(44));
        assert_eq!(listed[0].subject, "from the phone");
        assert!(!listed[0].dirty);
        assert_eq!(store.dirty(1).await.expect("dirty").len(), 0);
    }

    #[tokio::test]
    async fn pulling_never_overwrites_unsent_local_edits() {
        let (_dir, db) = fixture().await;
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/drafts",
            200,
            r#"[{"id":44,"subject":"from the phone","updated_at":"2026-09-16T12:00:00Z"}]"#,
        );
        let store = DraftStore::with_client(db, client_for(&server).await);

        // A draft that exists on both sides but was edited locally: it already
        // carries the server id, and it is still dirty.
        let local = store.create(1, new_draft("mine, edited")).await.expect("create");
        sqlx::query("UPDATE drafts SET id = 44 WHERE account_id = 1 AND local_uid = ?")
            .bind(&local.local_uid)
            .execute(store.database().pool())
            .await
            .expect("link");

        store.pull(1).await.expect("pull");
        let still_mine = store
            .get(1, &local.local_uid)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            still_mine.subject, "mine, edited",
            "the user's unsent text must not be thrown away"
        );
        assert!(still_mine.dirty);

        // A second, clean draft from the server is adopted as usual.
        let adopted = store
            .get_by_server_id(1, 44)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(adopted.subject, "mine, edited");
    }

    #[tokio::test]
    async fn push_all_pushes_every_dirty_draft_in_order() {
        let (_dir, db) = fixture().await;
        let server = MockServer::start().await;
        server.route("POST", "/api/v1/client/drafts", |_req, n| {
            MockResponse::json(format!(
                r#"{{"id":{},"updated_at":"2026-09-16T12:00:00Z"}}"#,
                40 + n
            ))
        });
        let store = DraftStore::with_client(db, client_for(&server).await);
        store.create(1, new_draft("one")).await.expect("one");
        store.create(1, new_draft("two")).await.expect("two");
        let outcomes = store.push_all(1).await.expect("push all");
        assert_eq!(outcomes.len(), 2);
        assert!(store.dirty(1).await.expect("dirty").is_empty());
        assert_eq!(server.count_for("/api/v1/client/drafts"), 2);
    }

    #[tokio::test]
    async fn deleting_a_pushed_draft_also_deletes_it_on_the_server() {
        let (_dir, db) = fixture().await;
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/drafts",
            200,
            r#"{"id":44,"updated_at":"2026-09-16T12:00:00Z"}"#,
        );
        server.json_route("DELETE", "/api/v1/client/drafts/44", 204, "");
        let store = DraftStore::with_client(db, client_for(&server).await);
        let draft = store.create(1, new_draft("bye")).await.expect("create");
        store.push(1, &draft.local_uid).await.expect("push");
        store.delete(1, &draft.local_uid).await.expect("delete");
        assert!(store.list(1).await.expect("list").is_empty());
        assert_eq!(server.count_for("/api/v1/client/drafts/44"), 1);
    }

    #[tokio::test]
    async fn a_deleted_draft_on_the_server_does_not_break_a_delete() {
        let (_dir, db) = fixture().await;
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/drafts",
            200,
            r#"{"id":44,"updated_at":"2026-09-16T12:00:00Z"}"#,
        );
        server.error_route("DELETE", "/api/v1/client/drafts/44", 404, "not_found", "gone");
        let store = DraftStore::with_client(db, client_for(&server).await);
        let draft = store.create(1, new_draft("bye")).await.expect("create");
        store.push(1, &draft.local_uid).await.expect("push");
        store
            .delete(1, &draft.local_uid)
            .await
            .expect("an already-deleted draft is fine");
    }

    #[tokio::test]
    async fn the_display_helpers_are_usable_by_the_list_view() {
        let (_dir, db) = fixture().await;
        let store = DraftStore::new(db);
        let mut draft = new_draft("");
        draft.to = vec!["a@b.c".into()];
        draft.cc = vec!["c@d.e".into()];
        let stored = store.create(1, draft).await.expect("create");
        assert_eq!(stored.display_subject(), "(no subject)");
        assert_eq!(stored.recipients(), "a@b.c, c@d.e");
        let payload = stored.payload();
        assert_eq!(payload.to, vec!["a@b.c".to_string()]);
    }
}
