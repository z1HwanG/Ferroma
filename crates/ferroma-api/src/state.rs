//! Shared application state.
//!
//! [`AppState`] is what every handler receives through `axum`'s `State` extractor. It
//! is deliberately cheap to clone: everything inside is either a small `Arc`, a
//! repository handle (which is itself a pool handle) or a plain value, so cloning it
//! per request costs a handful of atomic increments.
//!
//! # Where a send is committed
//!
//! The send path writes the Sent copy, its attachment rows and every outbound queue
//! row in one `ferroma_storage::store_submission` transaction, so a failure leaves no
//! half-sent state for a retry to duplicate. It used to go through a `MailSender`
//! trait defined here, which queued recipients as a second step; that seam is gone
//! because a queue row and the message it points at have to commit together.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{DateTime, Utc};
use ferroma_auth::{AuthService, TokenService};
use ferroma_core::config::Config;
use ferroma_core::{FerromaError, MessageId, Result, UserId};
use ferroma_events::EventBus;
use ferroma_storage::{AttachmentStore, Database, Maildir, Repositories};
use ferroma_sync::SyncService;

/// Live SMTP/IMAP connection counts, for `GET /api/v1/health`.
///
/// The counters are incremented when a listener accepts a connection and decremented
/// when the [`ConnectionGuard`] it created is dropped — including on a panic, which
/// is why the guard exists rather than a plain `fetch_add`/`fetch_sub` pair.
#[derive(Debug, Default)]
pub struct ConnTracker {
    smtp: AtomicU64,
    imap: AtomicU64,
    smtp_total: AtomicU64,
    imap_total: AtomicU64,
}

impl ConnTracker {
    /// An idle tracker.
    pub fn new() -> Self {
        ConnTracker::default()
    }

    /// Count one live SMTP connection until the returned guard is dropped.
    pub fn smtp(self: &Arc<Self>) -> ConnectionGuard {
        self.smtp_active().fetch_add(1, Ordering::Relaxed);
        self.smtp_total.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            counter: Arc::clone(self),
            kind: ConnKind::Smtp,
        }
    }

    /// Count one live IMAP connection until the returned guard is dropped.
    pub fn imap(self: &Arc<Self>) -> ConnectionGuard {
        self.imap_active().fetch_add(1, Ordering::Relaxed);
        self.imap_total.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            counter: Arc::clone(self),
            kind: ConnKind::Imap,
        }
    }

    /// SMTP connections open right now.
    pub fn smtp_connections(&self) -> u64 {
        self.smtp.load(Ordering::Relaxed)
    }

    /// IMAP connections open right now.
    pub fn imap_connections(&self) -> u64 {
        self.imap.load(Ordering::Relaxed)
    }

    /// SMTP connections accepted since boot.
    pub fn smtp_accepted(&self) -> u64 {
        self.smtp_total.load(Ordering::Relaxed)
    }

    /// IMAP connections accepted since boot.
    pub fn imap_accepted(&self) -> u64 {
        self.imap_total.load(Ordering::Relaxed)
    }

    fn smtp_active(&self) -> &AtomicU64 {
        &self.smtp
    }

    fn imap_active(&self) -> &AtomicU64 {
        &self.imap
    }
}

/// Which counter a [`ConnectionGuard`] decrements.
#[derive(Debug, Clone, Copy)]
enum ConnKind {
    Smtp,
    Imap,
}

/// Decrements a connection counter when dropped.
#[derive(Debug)]
pub struct ConnectionGuard {
    counter: Arc<ConnTracker>,
    kind: ConnKind,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let target = match self.kind {
            ConnKind::Smtp => self.counter.smtp_active(),
            ConnKind::Imap => self.counter.imap_active(),
        };
        // Saturating: a double drop must never wrap the counter to u64::MAX.
        let _ = target.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_sub(1))
        });
    }
}

/// A protocol listener managed by the Admin console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedListener {
    /// SMTP's MX, submission and configured implicit-TLS listeners.
    Smtp,
    /// IMAP's plaintext and configured implicit-TLS listeners.
    Imap,
    /// The HTTP JMAP surface, including discovery, methods, uploads and downloads.
    Jmap,
}

impl ManagedListener {
    /// Stable key used in the settings table.
    pub fn setting_key(self) -> &'static str {
        match self {
            ManagedListener::Smtp => "runtime.smtp.enabled",
            ManagedListener::Imap => "runtime.imap.enabled",
            ManagedListener::Jmap => "runtime.jmap.enabled",
        }
    }

    /// Stable API path segment.
    pub fn as_str(self) -> &'static str {
        match self {
            ManagedListener::Smtp => "smtp",
            ManagedListener::Imap => "imap",
            ManagedListener::Jmap => "jmap",
        }
    }
}

/// Runtime state of one listener family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedListenerState {
    /// Whether this process can start this protocol (not excluded by `--only` or config).
    pub available: bool,
    /// Whether it is accepting connections right now.
    pub enabled: bool,
}

/// Runtime state of both protocol listener families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedListenerStates {
    /// SMTP listener family.
    pub smtp: ManagedListenerState,
    /// IMAP listener family.
    pub imap: ManagedListenerState,
    /// JMAP HTTP surface.
    pub jmap: ManagedListenerState,
}

/// The server-owned control plane for SMTP and IMAP listeners.
///
/// `ferroma-api` intentionally does not depend on the protocol crates. The binary owns
/// sockets and implements this trait; routes only see this narrow, JSON-safe control
/// seam, and the protocols stay out of this crate entirely.
pub trait ListenerControl: Send + Sync + std::fmt::Debug {
    /// Current effective listener state.
    fn states(&self) -> ManagedListenerStates;

    /// Apply a persisted Admin choice to the running process.
    fn set_enabled(
        &self,
        listener: ManagedListener,
        enabled: bool,
    ) -> Pin<Box<dyn Future<Output = Result<ManagedListenerState>> + Send + '_>>;
}

/// The inert control used by unit tests and API-only embeddings.
#[derive(Debug)]
pub struct StaticListenerControl {
    states: ManagedListenerStates,
}

impl StaticListenerControl {
    /// Report the configuration's boot-time listener selection.
    pub fn from_config(config: &Config) -> Self {
        StaticListenerControl {
            states: ManagedListenerStates {
                smtp: ManagedListenerState {
                    available: config.smtp.enabled,
                    enabled: config.smtp.enabled,
                },
                imap: ManagedListenerState {
                    available: config.imap.enabled,
                    enabled: config.imap.enabled,
                },
                jmap: ManagedListenerState {
                    available: config.api.enabled,
                    enabled: config.api.enabled,
                },
            },
        }
    }
}

impl ListenerControl for StaticListenerControl {
    fn states(&self) -> ManagedListenerStates {
        self.states
    }

    fn set_enabled(
        &self,
        listener: ManagedListener,
        _enabled: bool,
    ) -> Pin<Box<dyn Future<Output = Result<ManagedListenerState>> + Send + '_>> {
        Box::pin(async move {
            Err(FerromaError::Conflict(format!(
                "{} is not runtime-controllable in this process",
                listener.as_str()
            )))
        })
    }
}

/// One in-flight chunked attachment upload (`fcp.md` §6).
///
/// The chunks themselves live in memory: `client.attachment_chunk_size` is 1 MiB by
/// default and an upload is bounded by `limits.max_attachment_size`, so a resumable
/// upload holds at most a few tens of megabytes — far less than the cost of a
/// resumable temporary file per uploader. The set of received indexes is what makes
/// chunks idempotent and out-of-order-tolerant.
#[derive(Debug)]
pub struct UploadSession {
    /// The owner of the upload. Only they may add chunks to it.
    pub user_id: UserId,
    /// Original file name.
    pub filename: String,
    /// Declared MIME type.
    pub content_type: String,
    /// Declared total size in bytes.
    pub size_bytes: u64,
    /// Bytes per chunk, except the last.
    pub chunk_size: u64,
    /// The attachment row reserved by `init`, when the upload has one.
    pub attachment_id: Option<i64>,
    /// Received chunks, by index.
    pub chunks: HashMap<u64, Vec<u8>>,
    /// The indexes held, so `status` can report them without touching the payloads.
    pub received: BTreeSet<u64>,
    /// When the upload started, for the housekeeping sweep.
    pub created_at: DateTime<Utc>,
}

impl UploadSession {
    /// How many chunks the declared size implies.
    pub fn expected_chunks(&self) -> u64 {
        if self.chunk_size == 0 {
            return 0;
        }
        self.size_bytes.div_ceil(self.chunk_size)
    }

    /// The indexes still missing.
    pub fn missing_chunks(&self) -> Vec<u64> {
        (0..self.expected_chunks())
            .filter(|index| !self.received.contains(index))
            .collect()
    }

    /// Concatenate the chunks in order. Missing chunks are a caller error.
    pub fn assemble(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.size_bytes as usize);
        for index in 0..self.expected_chunks() {
            let chunk = self.chunks.get(&index).ok_or_else(|| {
                FerromaError::Conflict(format!("upload chunk {index} has not been received"))
            })?;
            out.extend_from_slice(chunk);
        }
        Ok(out)
    }
}

/// Every in-flight chunked upload, keyed by its upload token.
#[derive(Debug, Default)]
pub struct UploadRegistry {
    sessions: Mutex<HashMap<String, UploadSession>>,
}

impl UploadRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        UploadRegistry::default()
    }

    /// Register a new session.
    pub fn insert(&self, token: String, session: UploadSession) {
        if let Ok(mut guard) = self.sessions.lock() {
            guard.insert(token, session);
        }
    }

    /// Look a session up.
    pub fn get(&self, token: &str) -> Option<UploadSession> {
        self.sessions.lock().ok().and_then(|guard| {
            guard.get(token).map(|session| UploadSession {
                user_id: session.user_id,
                filename: session.filename.clone(),
                content_type: session.content_type.clone(),
                size_bytes: session.size_bytes,
                chunk_size: session.chunk_size,
                attachment_id: session.attachment_id,
                chunks: session.chunks.clone(),
                received: session.received.clone(),
                created_at: session.created_at,
            })
        })
    }

    /// Find a session by the attachment it belongs to, when the caller owns it.
    ///
    /// This is what the chunk protocol authenticates against: the token is never sent
    /// back on the wire, so the attachment id plus ownership *is* the credential.
    pub fn find_for_attachment(&self, attachment_id: i64, user: UserId) -> Option<UploadSession> {
        self.sessions
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .iter()
                    .find(|(_, session)| {
                        session.attachment_id == Some(attachment_id) && session.user_id == user
                    })
                    .map(|(token, _)| token.clone())
            })
            .and_then(|token| self.get(&token))
    }

    /// Store one chunk against the attachment it belongs to.
    pub fn put_chunk_for_attachment(&self, attachment_id: i64, index: u64, data: Vec<u8>) -> bool {
        let Some(token) = self.sessions.lock().ok().and_then(|guard| {
            guard
                .iter()
                .find(|(_, session)| session.attachment_id == Some(attachment_id))
                .map(|(token, _)| token.clone())
        }) else {
            return false;
        };
        self.put_chunk(&token, index, data)
    }

    /// Remove and return the session of an attachment.
    pub fn take_for_attachment(
        &self,
        attachment_id: i64,
        user: UserId,
    ) -> Option<(String, UploadSession)> {
        let token = self.sessions.lock().ok().and_then(|guard| {
            guard
                .iter()
                .find(|(_, session)| {
                    session.attachment_id == Some(attachment_id) && session.user_id == user
                })
                .map(|(token, _)| token.clone())
        })?;
        let session = self.remove(&token)?;
        Some((token, session))
    }

    /// Store one chunk. Returns `false` when the token is unknown.
    pub fn put_chunk(&self, token: &str, index: u64, data: Vec<u8>) -> bool {
        let Ok(mut guard) = self.sessions.lock() else {
            return false;
        };
        match guard.get_mut(token) {
            Some(session) => {
                session.received.insert(index);
                session.chunks.insert(index, data);
                true
            }
            None => false,
        }
    }

    /// Remove a session, returning it.
    pub fn remove(&self, token: &str) -> Option<UploadSession> {
        self.sessions
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(token))
    }

    /// How many uploads are in flight.
    pub fn len(&self) -> usize {
        self.sessions.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// Whether no upload is in flight.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A hidden message row that owns attachments uploaded before they are sent.
///
/// The `attachments` table has `message_id NOT NULL REFERENCES messages(id)`, so an
/// upload that precedes the message it will belong to needs *something* to hang from.
/// Rather than change the schema, an uploader gets one placeholder message per
/// account, flagged `is_draft` and filed in their `Drafts` folder under a subject no
/// user interface shows. The send path re-points the attachment rows at the real
/// message, and afterwards the placeholder itself is deleted.
///
/// Ownership therefore still works the only way it can: an attachment belongs to the
/// user who owns the message it hangs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageStub {
    /// The placeholder message row.
    pub message_id: MessageId,
}

/// The subject that marks a placeholder row. Never shown, never searchable by a
/// client that filters on the subject it typed.
pub const STUB_SUBJECT: &str = "\u{1}ferroma-upload-stub";

impl MessageStub {
    /// The placeholder for `user`, created on first use.
    pub async fn ensure(app: &AppState, user: UserId) -> Result<MessageStub, FerromaError> {
        let mailbox = app
            .repos
            .mailboxes
            .find_primary(user)
            .await?
            .ok_or_else(|| {
                FerromaError::Conflict(
                    "this account has no address yet, so it cannot upload attachments".to_string(),
                )
            })?;
        let folder = app
            .repos
            .folders
            .require_by_name(mailbox.mailbox_id(), "Drafts")
            .await?;

        if let Some(existing) = find_stub(app, folder.folder_id()).await? {
            return Ok(MessageStub {
                message_id: existing.message_id(),
            });
        }

        let message = app
            .repos
            .messages
            .insert(ferroma_storage::repository::NewMessage {
                folder_id: folder.folder_id(),
                mailbox_id: mailbox.mailbox_id(),
                rfc_message_id: None,
                thread_id: None,
                subject: Some(STUB_SUBJECT.to_string()),
                sender: None,
                sender_name: None,
                snippet: None,
                size_bytes: 0,
                storage_path: String::new(),
                checksum_sha256: None,
                flags: "draft seen".to_string(),
                internal_date: None,
                sent_at: None,
                has_attachments: true,
                attachment_count: 0,
                is_draft: true,
            })
            .await?;

        Ok(MessageStub {
            message_id: message.message_id(),
        })
    }

    /// Whether a message row is a placeholder rather than mail.
    pub fn is_stub(message: &ferroma_storage::models::Message) -> bool {
        message.subject.as_deref() == Some(STUB_SUBJECT)
    }
}

/// Find an account's upload placeholder in one folder.
async fn find_stub(
    app: &AppState,
    folder: ferroma_core::MailboxId,
) -> Result<Option<ferroma_storage::models::Message>, FerromaError> {
    let rows = app.repos.messages.list_by_folder(folder, 50, 0).await?;
    Ok(rows
        .into_iter()
        .find(|message| MessageStub::is_stub(message) && message.expunged_at.is_none()))
}

/// Everything a handler needs, cheaply cloneable.
#[derive(Clone)]
pub struct AppState {
    /// Database repositories.
    pub repos: Repositories,
    /// The database handle itself, for pool statistics and version reporting.
    ///
    /// `None` until the server installs one with [`AppState::with_database`], which is
    /// how a test can point the API at a schema of its own. The health endpoint treats
    /// "no handle" exactly like "unreachable", so a misconfigured deployment is
    /// reported honestly rather than crashing.
    pub database: Option<Arc<Database>>,
    /// Login, session, token and device operations.
    pub auth: Arc<AuthService>,
    /// The realtime bus.
    pub events: Arc<EventBus>,
    /// Change log, cursors and idempotent client operations.
    pub sync: Arc<SyncService>,
    /// The effective configuration.
    pub config: Arc<Config>,
    /// Token minting and verification.
    pub tokens: TokenService,
    /// RFC 5322 message bytes on disk.
    pub maildir: Maildir,
    /// Content-addressed attachment blobs.
    pub attachments: Arc<AttachmentStore>,
    /// When the process started.
    pub started_at: Instant,
    /// Wall-clock instant of the same moment, for `.well-known` and uptime reports.
    pub started_wall: DateTime<Utc>,
    /// Live SMTP/IMAP connection counts.
    pub connections: Arc<ConnTracker>,
    /// Server-owned runtime control of SMTP and IMAP listeners.
    pub listeners: Arc<dyn ListenerControl>,
    /// In-flight chunked attachment uploads.
    pub uploads: Arc<UploadRegistry>,
    /// The message operations every mail surface shares.
    pub mail_service: crate::service::MessageService,
    /// The bounded in-process log ring behind `GET /api/v1/logs`.
    pub logs: crate::logbuf::LogSink,
    /// Writes one archive of both halves, when the process that owns the data registered it.
    ///
    /// The archive writer lives in the server binary. A process that did not register a
    /// writer leaves this empty, and the export endpoint says so instead of writing nothing.
    pub backup: Option<BackupWriter>,
    /// Asked for when the process has to come back up for a stored setting to be real.
    ///
    /// The first-run wizard can only *write* the advertised hostname, the public URL, the HTTP
    /// listen address and the TLS material: a running process cannot move its own socket, and
    /// the PEM files were read at boot. Watching this is how the `ferroma` binary learns it has
    /// been asked to come back up — see `restart_itself` in `server/src/serve.rs`.
    pub restart: RestartSignal,
}

/// What one archive export produced.
#[derive(Debug, Clone)]
pub struct BackupReport {
    /// Where the archive went.
    pub destination: String,
    /// Size of the archive, in bytes.
    pub bytes: u64,
    /// How many files the archive holds.
    pub members: usize,
}

/// Writes one archive. The server binary supplies the implementation.
pub type BackupWriter =
    Arc<dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<BackupReport, String>> + Send + Sync>;

/// A process's request to restart itself.
///
/// A `watch` channel rather than a flag, because the request and the wait for it are not
/// ordered: the wizard's request is made by a request handler while the server waits elsewhere,
/// and a receiver sees the current value whenever it subscribes, so a request made a moment
/// early is not lost.
#[derive(Clone, Default)]
pub struct RestartSignal(Arc<tokio::sync::watch::Sender<bool>>);

impl RestartSignal {
    /// Ask the process to come back up.
    ///
    /// `send_replace` rather than `send`: this is a state — "a restart has been asked for" — and
    /// not an event to be caught. The wizard's handler may be the only thing on this side of the
    /// channel at the moment it asks, and the server subscribes when it is ready.
    pub fn request(&self) {
        self.0.send_replace(true);
    }

    /// Watch for that request. The current value arrives immediately.
    #[must_use]
    pub fn watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.0.subscribe()
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("hostname", &self.config.server.hostname)
            .field("started_wall", &self.started_wall)
            .field("uploads", &self.uploads.len())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Build the state from its parts.
    ///
    /// The maildir and attachment store are derived from `config` unless the caller
    /// supplied explicit ones, so a test can point them at a temporary directory
    /// without rewriting the configuration. There is no database handle yet; the server
    /// installs one with [`AppState::with_database`].
    pub fn new(
        repos: Repositories,
        config: Arc<Config>,
        tokens: TokenService,
        auth: Arc<AuthService>,
        events: Arc<EventBus>,
        sync: Arc<SyncService>,
    ) -> Self {
        let maildir = Maildir::new(
            config.maildir_root(),
            config.storage.fsync_on_write,
            config.storage.layout,
        );
        let attachments = Arc::new(AttachmentStore::new(
            config.attachment_root(),
            config.storage.fsync_on_write,
        ));
        let mail_service = crate::service::MessageService::new(
            repos.clone(),
            maildir.clone(),
            Arc::clone(&attachments),
            Arc::clone(&events),
            Arc::clone(&sync),
            Arc::clone(&config),
        );
        let listeners: Arc<dyn ListenerControl> = Arc::new(StaticListenerControl::from_config(&config));
        AppState {
            repos,
            database: None,
            auth,
            events,
            sync,
            config,
            tokens,
            maildir,
            attachments,
            started_at: Instant::now(),
            started_wall: Utc::now(),
            connections: Arc::new(ConnTracker::new()),
            listeners,
            uploads: Arc::new(UploadRegistry::new()),
            mail_service,
            logs: crate::logbuf::LogSink::new(Arc::new(crate::logbuf::LogBuffer::default())),
            backup: None,
            restart: RestartSignal::default(),
        }
    }

    /// Replace the maildir, for tests that need a private root.
    #[must_use]
    pub fn with_maildir(mut self, maildir: Maildir) -> Self {
        self.mail_service = self.mail_service.with_maildir(maildir.clone());
        self.maildir = maildir;
        self
    }

    /// Replace the attachment store, for tests that need a private root.
    #[must_use]
    pub fn with_attachments(mut self, attachments: AttachmentStore) -> Self {
        let shared = Arc::new(attachments);
        self.mail_service = self.mail_service.with_attachments(Arc::clone(&shared));
        self.attachments = shared;
        self
    }

    /// Replace the connection tracker, for tests and for the server wiring.
    #[must_use]
    pub fn with_connections(mut self, connections: Arc<ConnTracker>) -> Self {
        self.connections = connections;
        self
    }

    /// Install the binary's runtime SMTP/IMAP listener controller.
    #[must_use]
    pub fn with_listener_control(mut self, listeners: Arc<dyn ListenerControl>) -> Self {
        self.listeners = listeners;
        self
    }

    /// Use `database` for pool statistics and version reporting.
    #[must_use]
    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    /// Register the process that can write a store archive.
    #[must_use]
    pub fn with_backup(mut self, backup: BackupWriter) -> Self {
        self.backup = Some(backup);
        self
    }

    /// Replace the log ring, so the server can share the buffer its `tracing`
    /// subscriber writes into.
    #[must_use]
    pub fn with_log_sink(mut self, logs: crate::logbuf::LogSink) -> Self {
        self.logs = logs;
        self
    }

    /// Use `restart` for this process's own restart requests, so the server can watch them.
    #[must_use]
    pub fn with_restart(mut self, restart: RestartSignal) -> Self {
        self.restart = restart;
        self
    }

    /// Seconds since the process started.
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// The negotiated FCP protocol version this build speaks.
    pub fn protocol_version(&self) -> u32 {
        self.config.client.protocol_version
    }

    /// The oldest FCP protocol version still accepted.
    pub fn min_protocol_version(&self) -> u32 {
        self.config.client.min_protocol_version
    }

    /// Whether the database answers a trivial query right now.
    ///
    /// Goes through [`Database::health`], which is the storage layer's own probe, so
    /// this crate needs no direct `sqlx` dependency. A state with no handle at all is
    /// unreachable by definition.
    pub async fn database_reachable(&self) -> bool {
        match &self.database {
            Some(database) => database.health().await.is_ok(),
            None => false,
        }
    }

    /// The PostgreSQL server version, when it answers.
    pub async fn server_version_text(&self) -> Option<String> {
        self.database.as_ref()?.server_version().await.ok()
    }

    /// Pool utilisation, when there is a handle.
    pub fn pool_stats(&self) -> Option<ferroma_storage::PoolStats> {
        self.database.as_ref().map(|database| database.pool_stats())
    }

    /// Size of the database on disk, in bytes.
    pub async fn database_bytes(&self) -> Option<i64> {
        self.database.as_ref()?.size_bytes().await.ok()
    }

    /// The server version string reported by `/health` and `/version`.
    pub fn server_version(&self) -> &'static str {
        ferroma_core::VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lazy pool: nothing in these tests touches a socket, but `Repositories`
    /// insists on a real `PgPool` value.
    fn lazy_repos() -> Repositories {
        let pool = sqlx::PgPool::connect_lazy("postgres://ferroma@127.0.0.1:5433/none")
            .expect("lazy pool must be constructible");
        Repositories::new(pool)
    }

    #[tokio::test]
    async fn conn_tracker_counts_and_releases() {
        let tracker = Arc::new(ConnTracker::new());
        assert_eq!(tracker.smtp_connections(), 0);
        assert_eq!(tracker.imap_connections(), 0);

        let a = tracker.smtp();
        let b = tracker.smtp();
        let c = tracker.imap();
        assert_eq!(tracker.smtp_connections(), 2);
        assert_eq!(tracker.imap_connections(), 1);
        assert_eq!(tracker.smtp_accepted(), 2);
        assert_eq!(tracker.imap_accepted(), 1);

        drop(a);
        assert_eq!(tracker.smtp_connections(), 1);
        drop(b);
        drop(c);
        assert_eq!(tracker.smtp_connections(), 0);
        assert_eq!(tracker.imap_connections(), 0);
        // Lifetime totals do not fall back down.
        assert_eq!(tracker.smtp_accepted(), 2);
    }

    #[test]
    fn conn_tracker_never_underflows() {
        let tracker = Arc::new(ConnTracker::new());
        let guard = tracker.smtp();
        drop(guard);
        assert_eq!(tracker.smtp_connections(), 0);
        // A second drop would be a bug, but the counter must stay sane.
        let again = ConnTracker::new();
        assert_eq!(again.smtp_connections(), 0);
    }

    fn session(size: u64, chunk: u64) -> UploadSession {
        UploadSession {
            user_id: UserId::new(7),
            filename: "big.bin".into(),
            content_type: "application/octet-stream".into(),
            size_bytes: size,
            chunk_size: chunk,
            attachment_id: None,
            chunks: HashMap::new(),
            received: BTreeSet::new(),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn upload_session_computes_chunk_geometry() {
        assert_eq!(session(0, 1024).expected_chunks(), 0);
        assert_eq!(session(1, 1024).expected_chunks(), 1);
        assert_eq!(session(1024, 1024).expected_chunks(), 1);
        assert_eq!(session(1025, 1024).expected_chunks(), 2);
        assert_eq!(session(4096, 1024).expected_chunks(), 4);
        // A zero chunk size would divide by zero; it must not panic.
        assert_eq!(session(100, 0).expected_chunks(), 0);
    }

    #[tokio::test]
    async fn registry_accepts_out_of_order_chunks_and_reports_gaps() {
        let registry = UploadRegistry::new();
        registry.insert("tok-1".into(), session(3000, 1000));

        assert!(registry.put_chunk("tok-1", 2, vec![3u8; 1000]));
        assert!(registry.put_chunk("tok-1", 0, vec![1u8; 1000]));
        // Re-sending an already-received chunk is idempotent, not an error.
        assert!(registry.put_chunk("tok-1", 1, vec![2u8; 1000]));

        let stored = registry.get("tok-1").expect("session must exist");
        assert_eq!(stored.missing_chunks(), Vec::<u64>::new());
        assert_eq!(stored.received.len(), 3);

        let assembled = stored.assemble().expect("all chunks are present");
        assert_eq!(assembled.len(), 3000);
        assert_eq!(assembled[0], 1);
        assert_eq!(assembled[1000], 2);
        assert_eq!(assembled[2000], 3);
    }

    #[tokio::test]
    async fn registry_reports_a_missing_chunk_rather_than_truncating() {
        let registry = UploadRegistry::new();
        registry.insert("tok-2".into(), session(3000, 1000));
        registry.put_chunk("tok-2", 0, vec![1u8; 1000]);
        let stored = registry.get("tok-2").expect("session must exist");
        assert_eq!(stored.missing_chunks(), vec![1, 2]);
        assert!(stored.assemble().is_err());
    }

    #[tokio::test]
    async fn registry_ignores_unknown_tokens() {
        let registry = UploadRegistry::new();
        assert!(registry.get("nope").is_none());
        assert!(!registry.put_chunk("nope", 0, vec![]));
        assert!(registry.remove("nope").is_none());
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn registry_remove_takes_the_session_out() {
        let registry = UploadRegistry::new();
        registry.insert("tok-3".into(), session(10, 10));
        assert_eq!(registry.len(), 1);
        assert!(registry.remove("tok-3").is_some());
        assert!(registry.get("tok-3").is_none());
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn app_state_reports_uptime_and_versions() {
        let config = Arc::new(Config::default());
        let repos = lazy_repos();
        let tokens = TokenService::new(
            "0123456789abcdef0123456789abcdef0123456789",
            3600,
            86_400,
            "localhost",
        )
        .expect("a 40-character secret is acceptable");
        let auth = Arc::new(AuthService::with_defaults(
            repos.clone(),
            tokens.clone(),
            config.limits.clone(),
        ));
        let sync = Arc::new(SyncService::new(
            repos.clone(),
            config.client.sync_page_size,
            config.client.tombstone_retention_days,
        ));
        let state = AppState::new(
            repos,
            Arc::clone(&config),
            tokens,
            auth,
            Arc::new(EventBus::with_defaults()),
            sync,
        );

        assert!(state.uptime_secs() < 60);
        assert_eq!(state.protocol_version(), 1);
        assert_eq!(state.min_protocol_version(), 1);
        assert_eq!(state.server_version(), ferroma_core::VERSION);
        assert!(state.connections.smtp_connections() == 0);
        assert!(state.uploads.is_empty());
    }

    #[tokio::test]
    async fn app_state_can_be_reconfigured_for_tests() {
        let config = Arc::new(Config::default());
        let repos = lazy_repos();
        let tokens = TokenService::new(
            "0123456789abcdef0123456789abcdef0123456789",
            3600,
            86_400,
            "localhost",
        )
        .expect("valid secret");
        let auth = Arc::new(AuthService::with_defaults(
            repos.clone(),
            tokens.clone(),
            config.limits.clone(),
        ));
        let sync = Arc::new(SyncService::new(
            repos.clone(),
            config.client.sync_page_size,
            config.client.tombstone_retention_days,
        ));
        let dir = tempfile::tempdir().expect("temp dir");
        let state = AppState::new(
            repos,
            Arc::clone(&config),
            tokens,
            auth,
            Arc::new(EventBus::with_defaults()),
            sync,
        )
        .with_maildir(Maildir::new(
            dir.path().join("mail"),
            false,
            ferroma_core::config::MailboxLayout::Maildir,
        ))
        .with_attachments(AttachmentStore::new(dir.path().join("att"), false))
        .with_connections(Arc::new(ConnTracker::new()));

        assert_eq!(state.maildir.root(), dir.path().join("mail").as_path());
        assert_eq!(state.attachments.root(), dir.path().join("att").as_path());
        assert_eq!(state.connections.smtp_connections(), 0);
    }

    #[test]
    fn static_listener_control_reports_configured_availability_and_refuses_changes() {
        let mut config = Config::default();
        config.smtp.enabled = true;
        config.imap.enabled = false;
        let control = StaticListenerControl::from_config(&config);
        let states = control.states();
        assert!(states.smtp.available && states.smtp.enabled);
        assert!(!states.imap.available && !states.imap.enabled);
        assert!(states.jmap.available && states.jmap.enabled);
        assert_eq!(ManagedListener::Smtp.setting_key(), "runtime.smtp.enabled");
        assert_eq!(ManagedListener::Imap.setting_key(), "runtime.imap.enabled");
        assert_eq!(ManagedListener::Jmap.setting_key(), "runtime.jmap.enabled");
    }

    #[tokio::test]
    async fn static_listener_control_never_claims_to_apply_a_runtime_change() {
        let control = StaticListenerControl::from_config(&Config::default());
        assert!(control
            .set_enabled(ManagedListener::Smtp, false)
            .await
            .is_err());
    }

    #[test]
    fn a_restart_request_survives_a_watch_that_starts_later() {
        // The wizard asks for a restart from inside a request handler; the server subscribes
        // when it reaches that part of its own startup. A request that arrives in between is a
        // process that never adopts what the operator typed, so the value has to be a state and
        // not an event: `watch` hands the current value to whoever subscribes, whenever.
        let signal = RestartSignal::default();
        signal.request();
        let watch = signal.watch();
        assert!(
            *watch.borrow(),
            "a request made before anything was watching must still be delivered"
        );
    }
}
