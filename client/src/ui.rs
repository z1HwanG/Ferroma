//! The shell seam: the documented, minimal API a UI shell drives.
//!
//! **There is no GUI in this crate.** A shell (Tauri, or anything else) holds a
//! [`ClientHandle`], asks it for folders and messages, tells it to send, and
//! subscribes to [`ClientEvent`]s to keep its views fresh. Everything the shell
//! needs is here; everything here is testable without a shell.
//!
//! ```no_run
//! # async fn example() -> Result<(), ferroma_client::ClientError> {
//! use ferroma_client::{ClientHandle, ClientEvent};
//! let handle = ClientHandle::open("C:/Users/me/AppData/Roaming/ferroma").await?;
//! let mut events = handle.subscribe();
//! tokio::spawn(async move {
//!     while let Ok(event) = events.recv().await {
//!         println!("{event:?}");
//!     }
//! });
//! let accounts = handle.list_accounts().await?;
//! # let _ = accounts;
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::account::{Account, AccountId, AccountManager};
use crate::attachment::AttachmentCache;
use crate::database::ClientDatabase;
use crate::draft::DraftStore;
use crate::error::{ClientError, ClientResult};
use crate::operations::{OperationKind, PendingQueue};
use crate::outbox::{NewOutboxItem, Outbox, OutboxCounts, OutboxItem};
use crate::search::{SearchExecutor, SearchResults};
use crate::settings::{Settings, SettingsStore};
use crate::sync::{ProgressCallback, SyncEngine, SyncProgress, SyncSummary};

use tokio::sync::broadcast;

/// Something that happened, which a shell should reflect.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The sync engine made progress; drives "syncing 412 of 1200".
    SyncProgress(SyncProgress),
    /// A sync run finished.
    SyncFinished(SyncSummary),
    /// A folder's counters changed.
    FoldersChanged {
        /// The account whose folder list changed.
        account_id: AccountId,
    },
    /// A message arrived (from the realtime socket or from a sync).
    MessageArrived {
        /// The account.
        account_id: AccountId,
        /// The message.
        message_id: i64,
        /// Its subject, for a notification.
        subject: Option<String>,
        /// Its sender.
        from: Option<String>,
    },
    /// The outbox changed state; drives "发件箱: sending 2, failed 1".
    OutboxChanged {
        /// The account.
        account_id: AccountId,
        /// The new counts.
        counts: Box<OutboxCounts>,
    },
    /// The offline queue grew or shrank.
    PendingOperationsChanged {
        /// The account.
        account_id: AccountId,
        /// How many operations are waiting.
        pending: i64,
    },
    /// The server rejected the refreshed credentials; the shell must ask the
    /// user to sign in again (`docs/fcp.md` §11).
    SessionExpired {
        /// The account.
        account_id: AccountId,
    },
    /// Something failed that the user should hear about.
    Error {
        /// A short, token-free message.
        message: String,
    },
}

/// A folder, as the sidebar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderView {
    /// The account it belongs to.
    pub account_id: AccountId,
    /// The server-side folder id.
    pub id: i64,
    /// The mailbox (address) it belongs to.
    pub mailbox_id: i64,
    /// The name, `/`-separated for nesting.
    pub name: String,
    /// `\Sent`, `\Drafts`, `\Trash`, … — never guessed from the name.
    pub special_use: Option<String>,
    /// How many messages the server reports.
    pub message_count: i64,
    /// How many are unseen.
    pub unseen_count: i64,
    /// A stable sort key: INBOX first, then the special folders, then the rest.
    pub sort_key: i64,
}

/// A message, as the list and the reading pane show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// The account.
    pub account_id: AccountId,
    /// The server-side id.
    pub id: i64,
    /// The folder it is in.
    pub folder_id: Option<i64>,
    /// The subject.
    pub subject: String,
    /// The sender's address.
    pub from_address: String,
    /// The sender's display name.
    pub from_name: Option<String>,
    /// Comma-joined recipients.
    pub to_summary: Option<String>,
    /// The preview line.
    pub snippet: Option<String>,
    /// Space separated flags.
    pub flags: String,
    /// When the server received it (RFC 3339).
    pub internal_date: Option<String>,
    /// Whether the message has attachments.
    pub has_attachments: bool,
    /// How many attachments.
    pub attachment_count: i64,
    /// Whether the body has been downloaded.
    pub body_state: String,
    /// The plain-text body, when it is cached.
    pub text_body: Option<String>,
    /// The HTML body, when it is cached.
    pub html_body: Option<String>,
}

impl MessageView {
    /// Whether the message has been read.
    pub fn is_read(&self) -> bool {
        self.flags.split_whitespace().any(|flag| flag == "seen")
    }

    /// Whether the message is flagged.
    pub fn is_flagged(&self) -> bool {
        self.flags.split_whitespace().any(|flag| flag == "flagged")
    }

    /// Whether the body still has to be downloaded.
    pub fn needs_body(&self) -> bool {
        self.text_body.is_none() && self.html_body.is_none()
    }

    /// One line for the list view.
    pub fn list_label(&self) -> String {
        let who = match &self.from_name {
            Some(name) if !name.is_empty() => name.clone(),
            _ => self.from_address.clone(),
        };
        format!("{who} — {}", if self.subject.is_empty() { "(no subject)" } else { &self.subject })
    }
}

/// The handle a shell holds.
#[derive(Clone)]
pub struct ClientHandle {
    db: Arc<ClientDatabase>,
    accounts: AccountManager,
    outbox: Outbox,
    queue: PendingQueue,
    settings: SettingsStore,
    attachments: AttachmentCache,
    drafts: DraftStore,
    events: broadcast::Sender<ClientEvent>,
}

impl std::fmt::Debug for ClientHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHandle")
            .field("data_dir", &self.data_dir())
            .finish_non_exhaustive()
    }
}

/// Where the client keeps its cache when the user does not say.
pub fn default_data_dir() -> ClientResult<PathBuf> {
    let base = dirs::data_dir()
        .ok_or_else(|| ClientError::cache("this platform has no data directory"))?;
    Ok(base.join("ferroma"))
}

impl ClientHandle {
    /// Open the client in `data_dir`, creating and migrating the cache.
    pub async fn open(data_dir: impl AsRef<Path>) -> ClientResult<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        let db = Arc::new(ClientDatabase::open(data_dir.join("ferroma.db")).await?);
        Self::with_database(db, &data_dir).await
    }

    /// Open the client in the platform's data directory.
    ///
    /// Convenience for a shell that has no opinion about storage layout; it is
    /// simply [`default_data_dir`] plus [`ClientHandle::open`].
    pub async fn open_in_default_dir() -> ClientResult<Self> {
        ClientHandle::open(default_data_dir()?).await
    }

    async fn with_database(db: Arc<ClientDatabase>, data_dir: &Path) -> ClientResult<Self> {
        let settings = SettingsStore::new(db.clone());
        let stored = settings.load().await.unwrap_or_default();
        let cap = stored.storage.max_cache_bytes.min(stored.sync.max_cache_bytes);
        let (events, _) = broadcast::channel(256);
        Ok(ClientHandle {
            accounts: AccountManager::new(db.clone())?,
            outbox: Outbox::new(db.clone()),
            queue: PendingQueue::new(db.clone()),
            settings,
            attachments: AttachmentCache::new(data_dir.join("blobs"), db.clone(), cap),
            drafts: DraftStore::new(db.clone()),
            db,
            events,
        })
    }

    /// The draft store (local CRUD; a shell that wants to push drafts attaches a
    /// server client with [`crate::draft::DraftStore::with_client`]).
    pub fn drafts(&self) -> &DraftStore {
        &self.drafts
    }

    /// The directory the client lives in.
    pub fn data_dir(&self) -> PathBuf {
        self.db
            .path()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The local cache.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// The account manager.
    pub fn accounts(&self) -> &AccountManager {
        &self.accounts
    }

    /// The outbox.
    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// The offline queue.
    pub fn queue(&self) -> &PendingQueue {
        &self.queue
    }

    /// The settings surface.
    pub fn settings_store(&self) -> &SettingsStore {
        &self.settings
    }

    /// The attachment cache.
    pub fn attachments(&self) -> &AttachmentCache {
        &self.attachments
    }

    /// Subscribe to state changes. Every subscriber sees every event that is
    /// emitted while it is alive.
    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.events.subscribe()
    }

    /// Publish an event. Returns how many subscribers received it; a shell that
    /// has not subscribed yet is not an error.
    pub fn emit(&self, event: ClientEvent) -> usize {
        self.events.send(event).unwrap_or(0)
    }

    /// A progress callback that turns sync progress into events.
    pub fn progress_callback(&self) -> ProgressCallback {
        let sender = self.events.clone();
        Arc::new(move |progress: SyncProgress| {
            let account_id = AccountId(progress.account_id);
            if progress.phase == crate::sync::SyncPhase::Done {
                let _ = sender.send(ClientEvent::FoldersChanged { account_id });
            }
            let _ = sender.send(ClientEvent::SyncProgress(progress));
        })
    }

    /// Every account.
    pub async fn list_accounts(&self) -> ClientResult<Vec<Account>> {
        self.accounts.list().await
    }

    /// The folders of an account, in sidebar order.
    pub async fn folders(&self, account: AccountId) -> ClientResult<Vec<FolderView>> {
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT id, mailbox_id, name, special_use, message_count, unseen_count
             FROM folders WHERE account_id = ?",
        )
        .bind(account.get())
        .fetch_all(self.db.pool())
        .await?;

        let mut folders: Vec<FolderView> = rows
            .iter()
            .map(|row| {
                let name: String = row.get("name");
                let special_use: Option<String> = row.get("special_use");
                Ok(FolderView {
                    account_id: account,
                    id: row.try_get("id")?,
                    mailbox_id: row.try_get("mailbox_id")?,
                    sort_key: folder_sort_key(&name, special_use.as_deref()),
                    name,
                    special_use,
                    message_count: row.try_get("message_count")?,
                    unseen_count: row.try_get("unseen_count")?,
                })
            })
            .collect::<ClientResult<Vec<_>>>()?;
        folders.sort_by(|a, b| {
            a.sort_key
                .cmp(&b.sort_key)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(folders)
    }

    /// One page of a folder's messages, newest first.
    pub async fn messages(
        &self,
        account: AccountId,
        folder_id: i64,
        limit: usize,
        offset: usize,
    ) -> ClientResult<Vec<MessageView>> {
        let rows = sqlx::query(
            "SELECT m.id, m.folder_id, m.subject, m.from_address, m.from_name, m.to_summary,
                    m.snippet, m.flags, m.internal_date, m.has_attachments, m.attachment_count,
                    m.body_state, b.text_body, b.html_body
             FROM messages m
             LEFT JOIN message_bodies b ON b.account_id = m.account_id AND b.message_id = m.id
             WHERE m.account_id = ? AND m.folder_id = ? AND m.deleted = 0
             ORDER BY m.internal_date DESC, m.id DESC
             LIMIT ? OFFSET ?",
        )
        .bind(account.get())
        .bind(folder_id)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(|row| message_view(account, row)).collect()
    }

    /// One message, downloading its body first when it is not cached.
    ///
    /// This is the lazy half of "metadata first, bodies on demand"
    /// (`docs/fcp.md` §3.4).
    pub async fn open_message(
        &self,
        account: AccountId,
        message_id: i64,
    ) -> ClientResult<MessageView> {
        let cached = self.cached_message(account, message_id).await?;
        let needs_body = match &cached {
            Some(view) => view.needs_body(),
            None => true,
        };
        if !needs_body {
            return cached.ok_or_else(|| ClientError::not_found(format!("no message {message_id}")));
        }

        let client = self.accounts.client(account).await?;
        let detail = client.message(message_id).await?;
        let engine = SyncEngine::new(self.db.clone(), client);
        engine.store_message_detail(account.get(), &detail).await?;
        // `GET /messages/:id` returns the bodies too, so cache them in the same
        // visit: the next open is offline.
        if detail.text_body.is_some() || detail.html_body.is_some() {
            engine
                .store_message_body(
                    account.get(),
                    message_id,
                    detail.text_body.as_deref(),
                    detail.html_body.as_deref(),
                    None,
                )
                .await?;
        }
        let _ = self.emit(ClientEvent::MessageArrived {
            account_id: account,
            message_id,
            subject: detail.item.subject.clone(),
            from: detail.item.from.as_ref().map(|from| from.address.clone()),
        });
        self.cached_message(account, message_id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no message {message_id}")))
    }

    /// A message straight from the cache, without a network call.
    pub async fn cached_message(
        &self,
        account: AccountId,
        message_id: i64,
    ) -> ClientResult<Option<MessageView>> {
        let row = sqlx::query(
            "SELECT m.id, m.folder_id, m.subject, m.from_address, m.from_name, m.to_summary,
                    m.snippet, m.flags, m.internal_date, m.has_attachments, m.attachment_count,
                    m.body_state, b.text_body, b.html_body
             FROM messages m
             LEFT JOIN message_bodies b ON b.account_id = m.account_id AND b.message_id = m.id
             WHERE m.account_id = ? AND m.id = ? AND m.deleted = 0",
        )
        .bind(account.get())
        .bind(message_id)
        .fetch_optional(self.db.pool())
        .await?;
        match row {
            Some(row) => Ok(Some(message_view(account, &row)?)),
            None => Ok(None),
        }
    }

    /// Queue a flag change, so it survives being offline (§27).
    pub async fn mark_read(
        &self,
        account: AccountId,
        message_id: i64,
        read: bool,
    ) -> ClientResult<String> {
        let kind = if read {
            OperationKind::MarkRead
        } else {
            OperationKind::MarkUnread
        };
        let operation = self
            .queue
            .enqueue(
                account.get(),
                kind,
                serde_json::json!({ "message_id": message_id }),
            )
            .await?;
        self.emit_pending(account).await;
        Ok(operation.operation_id)
    }

    /// Queue a move (archive, trash, or an explicit folder).
    pub async fn move_message(
        &self,
        account: AccountId,
        message_id: i64,
        folder_id: i64,
        kind: OperationKind,
    ) -> ClientResult<String> {
        let operation = self
            .queue
            .enqueue(
                account.get(),
                kind,
                serde_json::json!({ "message_id": message_id, "folder_id": folder_id }),
            )
            .await?;
        self.emit_pending(account).await;
        Ok(operation.operation_id)
    }

    /// Put a message in the outbox. The write is durable before this returns.
    pub async fn send(&self, item: NewOutboxItem) -> ClientResult<OutboxItem> {
        let account = AccountId(item.account_id);
        let stored = self.outbox.enqueue(item).await?;
        let counts = self.outbox.counts(account.get()).await?;
        let _ = self.emit(ClientEvent::OutboxChanged {
            account_id: account,
            counts: Box::new(counts),
        });
        Ok(stored)
    }

    /// The Outbox header of §28.
    pub async fn outbox_counts(&self, account: AccountId) -> ClientResult<OutboxCounts> {
        self.outbox.counts(account.get()).await
    }

    /// Sync one account, emitting progress as it goes.
    pub async fn sync(&self, account: AccountId) -> ClientResult<SyncSummary> {
        let client = self.accounts.client(account).await?;
        let engine = SyncEngine::new(self.db.clone(), client);
        let callback = self.progress_callback();
        let result = engine.sync_account(account.get(), Some(&callback)).await;
        match &result {
            Ok(summary) => {
                self.accounts.record_sync(account, None).await?;
                let _ = self.emit(ClientEvent::SyncFinished(summary.clone()));
            }
            Err(err) => {
                let message = err.user_message();
                self.accounts
                    .record_sync(account, Some(&message))
                    .await?;
                if matches!(err, ClientError::SessionExpired) {
                    let _ = self.emit(ClientEvent::SessionExpired { account_id: account });
                } else {
                    let _ = self.emit(ClientEvent::Error { message });
                }
            }
        }
        result
    }

    /// Search an account: the local index first, the server only when it comes
    /// up empty (§30).
    pub async fn search(
        &self,
        account: AccountId,
        query: &str,
        limit: usize,
    ) -> ClientResult<SearchResults> {
        let parsed = crate::search::SearchQuery::parse(query)?;
        let client = self.accounts.client(account).await.ok();
        let executor = match client {
            Some(client) => SearchExecutor::with_server(self.db.clone(), client),
            None => SearchExecutor::local(self.db.clone()),
        };
        executor.search(account.get(), &parsed, limit).await
    }

    /// The settings surface.
    pub async fn load_settings(&self) -> ClientResult<Settings> {
        self.settings.load().await
    }

    /// Persist the settings surface.
    pub async fn save_settings(&self, settings: &Settings) -> ClientResult<()> {
        self.settings.save(settings).await
    }

    async fn emit_pending(&self, account: AccountId) {
        let pending = self.queue.len(account.get()).await.unwrap_or(0);
        let _ = self.emit(ClientEvent::PendingOperationsChanged {
            account_id: account,
            pending,
        });
    }
}

fn folder_sort_key(name: &str, special_use: Option<&str>) -> i64 {
    match special_use {
        Some("\\Inbox") => 0,
        _ if name.eq_ignore_ascii_case("INBOX") => 0,
        Some("\\Drafts") => 10,
        Some("\\Sent") => 20,
        Some("\\Archive") => 30,
        Some("\\Junk") => 40,
        Some("\\Trash") => 50,
        _ => 100,
    }
}

fn message_view(account: AccountId, row: &sqlx::sqlite::SqliteRow) -> ClientResult<MessageView> {
    use sqlx::Row;
    Ok(MessageView {
        account_id: account,
        id: row.try_get("id")?,
        folder_id: row.try_get("folder_id")?,
        subject: row.try_get("subject")?,
        from_address: row.try_get("from_address")?,
        from_name: row.try_get("from_name")?,
        to_summary: row.try_get("to_summary")?,
        snippet: row.try_get("snippet")?,
        flags: row.try_get("flags")?,
        internal_date: row.try_get("internal_date")?,
        has_attachments: row.try_get::<i64, _>("has_attachments")? != 0,
        attachment_count: row.try_get("attachment_count")?,
        body_state: row.try_get("body_state")?,
        text_body: row.try_get("text_body")?,
        html_body: row.try_get("html_body")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    async fn handle() -> (TempDir, ClientHandle) {
        let dir = TempDir::new().expect("temp dir");
        let handle = ClientHandle::open(dir.path().join("data")).await.expect("open");
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (1, 'alice@example.com', 'http://127.0.0.1:1/api/v1/client', 'dev', 'now', 'now')",
        )
        .execute(handle.database().pool())
        .await
        .expect("account");
        (dir, handle)
    }

    async fn seed_folder(handle: &ClientHandle, folder_id: i64, name: &str, special: Option<&str>) {
        sqlx::query(
            "INSERT INTO folders (account_id, id, mailbox_id, name, special_use, message_count, unseen_count)
             VALUES (1, ?, 3, ?, ?, 2, 1)",
        )
        .bind(folder_id)
        .bind(name)
        .bind(special)
        .execute(handle.database().pool())
        .await
        .expect("folder");
    }

    async fn seed_message(handle: &ClientHandle, id: i64, folder_id: i64, subject: &str, date: &str) {
        sqlx::query(
            "INSERT INTO messages (account_id, id, folder_id, uid, subject, from_address, from_name,
                                   flags, internal_date, cached_at)
             VALUES (1, ?, ?, ?, ?, 'bob@example.net', 'Bob', 'seen', ?, 'now')",
        )
        .bind(id)
        .bind(folder_id)
        .bind(id)
        .bind(subject)
        .bind(date)
        .execute(handle.database().pool())
        .await
        .expect("message");
    }

    #[tokio::test]
    async fn a_subscriber_receives_emitted_events() {
        let (_dir, handle) = handle().await;
        let mut receiver = handle.subscribe();
        let delivered = handle.emit(ClientEvent::Error {
            message: "boom".into(),
        });
        assert_eq!(delivered, 1);
        let event = receiver.recv().await.expect("event");
        assert_eq!(event, ClientEvent::Error { message: "boom".into() });
    }

    #[tokio::test]
    async fn emitting_without_subscribers_is_not_an_error() {
        let (_dir, handle) = handle().await;
        assert_eq!(handle.emit(ClientEvent::Error { message: "x".into() }), 0);
    }

    #[tokio::test]
    async fn two_subscribers_both_see_the_event() {
        let (_dir, handle) = handle().await;
        let mut first = handle.subscribe();
        let mut second = handle.subscribe();
        handle.emit(ClientEvent::FoldersChanged {
            account_id: AccountId(1),
        });
        assert!(first.recv().await.is_ok());
        assert!(second.recv().await.is_ok());
    }

    #[tokio::test]
    async fn folders_come_back_in_sidebar_order() {
        let (_dir, handle) = handle().await;
        seed_folder(&handle, 9, "Junk", Some("\\Junk")).await;
        seed_folder(&handle, 5, "INBOX", None).await;
        seed_folder(&handle, 6, "Sent", Some("\\Sent")).await;
        seed_folder(&handle, 10, "Projects", None).await;

        let folders = handle.folders(AccountId(1)).await.expect("folders");
        let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["INBOX", "Sent", "Junk", "Projects"]);
        assert_eq!(folders[0].message_count, 2);
        assert_eq!(folders[0].unseen_count, 1);
    }

    #[tokio::test]
    async fn messages_come_back_newest_first_with_paging() {
        let (_dir, handle) = handle().await;
        seed_folder(&handle, 5, "INBOX", None).await;
        seed_message(&handle, 1, 5, "oldest", "2026-09-01T00:00:00Z").await;
        seed_message(&handle, 2, 5, "middle", "2026-09-05T00:00:00Z").await;
        seed_message(&handle, 3, 5, "newest", "2026-09-09T00:00:00Z").await;

        let page = handle.messages(AccountId(1), 5, 2, 0).await.expect("page");
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].subject, "newest");
        assert_eq!(page[1].subject, "middle");
        let second = handle.messages(AccountId(1), 5, 2, 2).await.expect("page2");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].subject, "oldest");
    }

    #[tokio::test]
    async fn a_deleted_message_never_appears() {
        let (_dir, handle) = handle().await;
        seed_folder(&handle, 5, "INBOX", None).await;
        seed_message(&handle, 1, 5, "gone", "2026-09-01T00:00:00Z").await;
        sqlx::query("UPDATE messages SET deleted = 1 WHERE id = 1")
            .execute(handle.database().pool())
            .await
            .expect("delete");
        assert!(handle
            .messages(AccountId(1), 5, 10, 0)
            .await
            .expect("page")
            .is_empty());
        assert!(handle.cached_message(AccountId(1), 1).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn the_message_view_reports_flags_and_body_state() {
        let (_dir, handle) = handle().await;
        seed_folder(&handle, 5, "INBOX", None).await;
        seed_message(&handle, 1, 5, "hello", "2026-09-01T00:00:00Z").await;
        let view = handle
            .cached_message(AccountId(1), 1)
            .await
            .expect("get")
            .expect("row");
        assert!(view.is_read());
        assert!(!view.is_flagged());
        assert!(view.needs_body());
        assert_eq!(view.list_label(), "Bob — hello");
        assert_eq!(view.body_state, "pending");
    }

    #[tokio::test]
    async fn opening_an_uncached_message_without_a_server_fails_cleanly() {
        let (_dir, handle) = handle().await;
        seed_folder(&handle, 5, "INBOX", None).await;
        seed_message(&handle, 1, 5, "hello", "2026-09-01T00:00:00Z").await;
        // The account has no credentials and points at nothing; the body fetch
        // must come back as an error, never as a panic.
        let err = handle
            .open_message(AccountId(1), 1)
            .await
            .expect_err("must fail");
        assert!(
            matches!(
                err,
                ClientError::Network(_)
                    | ClientError::Api(_)
                    | ClientError::RateLimited { .. }
                    | ClientError::NotAuthenticated
                    | ClientError::SessionExpired
                    | ClientError::Timeout(_)
            ),
            "unexpected error {err:?}"
        );
        // The cached metadata is still readable offline.
        let view = handle
            .cached_message(AccountId(1), 1)
            .await
            .expect("cached")
            .expect("row");
        assert_eq!(view.subject, "hello");
    }

    #[tokio::test]
    async fn opening_a_message_caches_the_body_it_downloaded() {
        use crate::testutil::MockServer;

        let dir = TempDir::new().expect("temp dir");
        let handle = ClientHandle::open(dir.path().join("data")).await.expect("open");
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/auth/refresh",
            200,
            r#"{"access_token":"access","refresh_token":"rt_2","expires_in":3600}"#,
        );
        server.json_route(
            "GET",
            "/api/v1/client/messages/1",
            200,
            r#"{"id":1,"uid":1,"folder_id":5,"subject":"hello",
                "from":{"address":"bob@example.net","name":"Bob"},
                "text_body":"the body over the wire","flags":"seen",
                "headers":[{"name":"Subject","value":"hello"}]}"#,
        );
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, refresh_token, created_at, updated_at)
             VALUES (1, 'alice@example.com', ?, 'dev', 'rt_1', 'now', 'now')",
        )
        .bind(server.fcp_base_url())
        .execute(handle.database().pool())
        .await
        .expect("account");
        seed_folder(&handle, 5, "INBOX", None).await;
        seed_message(&handle, 1, 5, "hello", "2026-09-01T00:00:00Z").await;

        let view = handle.open_message(AccountId(1), 1).await.expect("open");
        assert_eq!(view.text_body.as_deref(), Some("the body over the wire"));
        assert!(!view.needs_body(), "the body that arrived is cached");
        assert_eq!(server.count_for("/api/v1/client/messages/1"), 1);

        // Opening it again is served from the cache.
        let again = handle.open_message(AccountId(1), 1).await.expect("reopen");
        assert_eq!(again.text_body.as_deref(), Some("the body over the wire"));
        assert_eq!(
            server.count_for("/api/v1/client/messages/1"),
            1,
            "a cached body must not be fetched twice"
        );
    }

    #[tokio::test]
    async fn a_cached_message_opens_without_the_network() {
        let (_dir, handle) = handle().await;
        seed_folder(&handle, 5, "INBOX", None).await;
        seed_message(&handle, 1, 5, "hello", "2026-09-01T00:00:00Z").await;
        sqlx::query(
            "INSERT INTO message_bodies (account_id, message_id, text_body, fetched_at)
             VALUES (1, 1, 'the body', 'now')",
        )
        .execute(handle.database().pool())
        .await
        .expect("body");
        let view = handle
            .open_message(AccountId(1), 1)
            .await
            .expect("open");
        assert_eq!(view.text_body.as_deref(), Some("the body"));
        assert!(!view.needs_body());
    }

    #[tokio::test]
    async fn marking_read_queues_an_operation_and_announces_it() {
        let (_dir, handle) = handle().await;
        let mut receiver = handle.subscribe();
        let operation_id = handle
            .mark_read(AccountId(1), 42, true)
            .await
            .expect("queue");
        assert!(operation_id.starts_with("op_"));
        assert_eq!(handle.queue().len(1).await.expect("len"), 1);

        let event = receiver.recv().await.expect("event");
        match event {
            ClientEvent::PendingOperationsChanged { account_id, pending } => {
                assert_eq!(account_id, AccountId(1));
                assert_eq!(pending, 1);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test]
    async fn sending_puts_the_message_in_the_outbox_and_announces_counts() {
        let (_dir, handle) = handle().await;
        let mut receiver = handle.subscribe();
        let item = NewOutboxItem::simple(
            1,
            "alice@example.com",
            &["bob@example.net".to_string()],
            "Hi",
            "hello",
        );
        let stored = handle.send(item).await.expect("send");
        assert_eq!(stored.state, crate::outbox::OutboxState::Pending);

        let event = receiver.recv().await.expect("event");
        match event {
            ClientEvent::OutboxChanged { account_id, counts } => {
                assert_eq!(account_id, AccountId(1));
                assert_eq!(counts.in_flight(), 1);
            }
            other => panic!("unexpected event {other:?}"),
        }

        let counts = handle.outbox_counts(AccountId(1)).await.expect("counts");
        assert_eq!(counts.summary(), "sending 1, failed 0, sent 0");
    }

    #[tokio::test]
    async fn moving_a_message_queues_the_move() {
        let (_dir, handle) = handle().await;
        handle
            .move_message(AccountId(1), 42, 9, OperationKind::Move)
            .await
            .expect("queue");
        let operations = handle.queue().list(1).await.expect("list");
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].kind, OperationKind::Move);
        assert_eq!(operations[0].folder_id(), Some(9));
    }

    #[tokio::test]
    async fn settings_round_trip_through_the_handle() {
        let (_dir, handle) = handle().await;
        let mut settings = handle.load_settings().await.expect("load");
        assert_eq!(settings.sync.window, crate::settings::SyncWindow::All);
        settings.sync.window = crate::settings::SyncWindow::Days30;
        handle.save_settings(&settings).await.expect("save");

        let reloaded = handle.load_settings().await.expect("reload");
        assert_eq!(reloaded.sync.window, crate::settings::SyncWindow::Days30);
    }

    #[tokio::test]
    async fn the_handle_opens_its_cache_in_the_data_directory() {
        let dir = TempDir::new().expect("temp dir");
        let data = dir.path().join("profile");
        let handle = ClientHandle::open(&data).await.expect("open");
        assert!(data.join("ferroma.db").exists());
        assert_eq!(handle.data_dir(), data);
        assert!(handle.attachments().root().starts_with(&data));
    }

    #[tokio::test]
    async fn the_progress_callback_publishes_sync_progress() {
        let (_dir, handle) = handle().await;
        let mut receiver = handle.subscribe();
        let callback = handle.progress_callback();
        callback(SyncProgress {
            account_id: 1,
            folder_id: 5,
            folder_name: Some("INBOX".into()),
            applied: 412,
            total: Some(1200),
            phase: crate::sync::SyncPhase::Applying,
        });
        let event = receiver.recv().await.expect("event");
        match event {
            ClientEvent::SyncProgress(progress) => {
                assert_eq!(progress.applied, 412);
                assert_eq!(progress.label(), "412 of 1200");
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test]
    async fn syncing_an_account_whose_server_is_absent_reports_an_error_event() {
        let (_dir, handle) = handle().await;
        let mut receiver = handle.subscribe();
        let err = handle.sync(AccountId(1)).await.expect_err("must fail");
        assert!(err.is_retryable() || matches!(err, ClientError::NotAuthenticated));
        let event = receiver.recv().await.expect("event");
        assert!(matches!(event, ClientEvent::Error { .. }));
        let account = handle
            .accounts()
            .get(AccountId(1))
            .await
            .expect("get")
            .expect("row");
        assert!(account.last_error.is_some(), "the failure is recorded");
    }

    #[tokio::test]
    async fn a_bad_search_query_is_reported_before_any_work() {
        let (_dir, handle) = handle().await;
        let err = handle
            .search(AccountId(1), "from:", 10)
            .await
            .expect_err("must fail");
        assert!(matches!(err, ClientError::Invalid(_)));
    }
}
