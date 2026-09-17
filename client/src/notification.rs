//! Push notifications (specification §34).
//!
//! §34's pipeline is `mail.received` → event bus → notification service → the
//! platform. This module is the client's middle box: it turns a realtime frame
//! into a [`Notification`], asks a [`NotificationPolicy`] whether it should be
//! shown at all, and hands it to a [`Notifier`].
//!
//! ```text
//! frame ──► Notification::from_event ──► NotificationService::notify
//!                                              │            │
//!                                     NotificationPolicy  Notifier
//!                                       (should_notify)   (record it)
//! ```
//!
//! # The core never draws a toast
//!
//! This is deliberate, and it is stated plainly everywhere it matters: the client
//! core has **no platform notification code at all**. [`PlatformNotifier`] logs at
//! `info` and writes a row to the `notifications` table; the UI shell — the Tauri
//! or other front end that owns a Windows toast, a libnotify call or a
//! `UNUserNotificationCenter` request — drains the rows with `delivered = 0` via
//! [`pending_notifications`] and shows them, then calls [`mark_delivered`]. There
//! is no `notify-send`, no `osascript`, no PowerShell and no `std::process` call
//! anywhere in this file, because a headless core that shells out to a desktop
//! would be untestable on a server and wrong on a platform without one.
//!
//! # What a notification is allowed to say
//!
//! A `mail.received` frame carries `from`, `subject` and `snippet` and *never* a
//! body (§34, `docs/client.md` §14.1), so a banner cannot leak message content
//! into the OS notification store, which Windows and macOS persist and index. The
//! banner is therefore built from what the frame already has: the sender as the
//! title and the subject as the body. No body is ever fetched to make a richer
//! banner.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Row;

use crate::database::ClientDatabase;
use crate::error::{BoxFuture, ClientResult};

/// How long a synchronous [`NotificationPolicy`] hook waits for its pause-flag
/// lookup before it gives up and lets the notification through.
const PAUSE_LOOKUP_TIMEOUT: Duration = Duration::from_millis(500);

/// What kind of thing happened (§34, `docs/fcp.md` §8).
///
/// [`NotificationKind::Other`] keeps the raw event name, so a server that grows a
/// new event is still announced rather than silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationKind {
    /// A message arrived in a mailbox (`mail.received`).
    MailReceived,
    /// A message's flags changed (`mail.flag_changed`).
    MailFlagChanged,
    /// A message was deleted (`mail.deleted`).
    MailDeleted,
    /// A message moved between folders (`mail.moved`).
    MailMoved,
    /// A draft was updated (`draft.updated`).
    DraftUpdated,
    /// The delivery state of an outgoing message changed (`delivery.updated`).
    DeliveryUpdated,
    /// This device was revoked (`device.revoked`).
    DeviceRevoked,
    /// The event bus dropped frames for this subscriber (`replay_gap`); the
    /// client must run a sync immediately.
    ReplayGap,
    /// Any other event name, kept verbatim.
    Other(String),
}

impl NotificationKind {
    /// The event name this kind came from — `"mail.received"`, `"draft.updated"`,
    /// …
    ///
    /// This is exactly the string stored in `notifications.kind`, so a shell can
    /// map a stored row back with [`NotificationKind::from_str_opt`].
    pub fn as_str(&self) -> &str {
        match self {
            NotificationKind::MailReceived => "mail.received",
            NotificationKind::MailFlagChanged => "mail.flag_changed",
            NotificationKind::MailDeleted => "mail.deleted",
            NotificationKind::MailMoved => "mail.moved",
            NotificationKind::DraftUpdated => "draft.updated",
            NotificationKind::DeliveryUpdated => "delivery.updated",
            NotificationKind::DeviceRevoked => "device.revoked",
            NotificationKind::ReplayGap => "replay.gap",
            NotificationKind::Other(name) => name.as_str(),
        }
    }

    /// Parse an event name.
    ///
    /// This is an inherent function rather than an implementation of
    /// [`std::str::FromStr`]: it is infallible-by-shape (anything unrecognised
    /// becomes [`NotificationKind::Other`]) and only the empty string is a
    /// `None`, which is not what the standard trait means.
    pub fn from_str_opt(name: &str) -> Option<NotificationKind> {
        let kind = match name {
            "" => return None,
            "mail.received" => NotificationKind::MailReceived,
            "mail.flag_changed" => NotificationKind::MailFlagChanged,
            "mail.deleted" => NotificationKind::MailDeleted,
            "mail.moved" => NotificationKind::MailMoved,
            "draft.updated" => NotificationKind::DraftUpdated,
            "delivery.updated" => NotificationKind::DeliveryUpdated,
            "device.revoked" => NotificationKind::DeviceRevoked,
            // §23 spells the gap marker `{"replay_gap": true}`; both spellings are
            // read so neither a frame nor a stored row is misread.
            "replay.gap" | "replay_gap" => NotificationKind::ReplayGap,
            other => NotificationKind::Other(other.to_string()),
        };
        Some(kind)
    }

    /// Whether this kind belongs to the `mail.*` family, which a paused account
    /// suppresses (specification §31 pause sync).
    pub fn is_mail(&self) -> bool {
        self.as_str().starts_with("mail.")
    }
}

/// One notification, ready to be delivered (§34).
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// What happened.
    pub kind: NotificationKind,
    /// The account the event belongs to, when the frame said.
    pub account_id: Option<i64>,
    /// The banner title — never empty.
    pub title: String,
    /// The banner body, when there is one.
    pub body: Option<String>,
    /// The message the event is about, when it is about one. A shell
    /// deduplicates by this (the same arrival can reach it over both the socket
    /// and a sync), and can open the message when the banner is clicked.
    pub message_id: Option<i64>,
    /// When the event happened: the frame's `at` when it carried a parseable
    /// one, otherwise the moment it was turned into a notification.
    pub at: DateTime<Utc>,
    /// The frame's payload, verbatim, so a shell can word the banner itself.
    /// `None` when the event carried no fields at all.
    pub payload: Option<Value>,
}

impl Notification {
    /// Build the notification that a realtime event becomes (§34).
    ///
    /// A `mail.received` frame uses its `from` field as the title and its
    /// `subject` as the body — the two things §34 allows a lock-screen banner to
    /// show. Every other kind gets a fixed title, because the interesting part of
    /// a flag change or a revocation is what the shell chooses to say about it,
    /// and the raw payload is kept for exactly that.
    pub fn from_event(kind: &str, account_id: Option<i64>, payload: &Value) -> Notification {
        let kind = NotificationKind::from_str_opt(kind)
            .unwrap_or_else(|| NotificationKind::Other(kind.to_string()));
        let at = payload
            .get("at")
            .and_then(Value::as_str)
            .and_then(crate::util::parse_rfc3339)
            .unwrap_or_else(Utc::now);
        let (title, body) = banner(&kind, payload);

        Notification {
            kind,
            account_id,
            title,
            body,
            message_id: payload.get("message_id").and_then(Value::as_i64),
            at,
            payload: if payload.is_null() {
                None
            } else {
                Some(payload.clone())
            },
        }
    }
}

/// The title and body a kind gets from its payload.
///
/// Only `mail.received` reads the payload: it is the one event §34 gives banner
/// content to.
fn banner(kind: &NotificationKind, payload: &Value) -> (String, Option<String>) {
    /// A non-empty string field of the payload.
    fn text<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
        payload
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    match kind {
        NotificationKind::MailReceived => {
            let title = text(payload, "from").unwrap_or("New message").to_string();
            let body = text(payload, "subject").map(str::to_string);
            (title, body)
        }
        NotificationKind::MailFlagChanged => ("Message updated".to_string(), None),
        NotificationKind::MailDeleted => ("Message deleted".to_string(), None),
        NotificationKind::MailMoved => ("Message moved".to_string(), None),
        NotificationKind::DraftUpdated => ("Draft updated".to_string(), None),
        NotificationKind::DeliveryUpdated => ("Delivery updated".to_string(), None),
        NotificationKind::DeviceRevoked => ("Device signed out".to_string(), None),
        NotificationKind::ReplayGap => ("Some events were missed".to_string(), None),
        NotificationKind::Other(name) if name.is_empty() => ("Notification".to_string(), None),
        NotificationKind::Other(name) => (name.clone(), None),
    }
}

/// Delivers a notification.
///
/// The seam between the core and the platform: the core only ever calls
/// [`Notifier::notify`], and a shell decides what "deliver" means.
pub trait Notifier: Send + Sync + 'static {
    /// Deliver (or record) `notification`.
    fn notify(&self, notification: &Notification);
}

/// A notifier that does nothing at all.
///
/// Used by tests, by the headless CLI, and by any run where the user turned
/// notifications off — it records nothing and logs nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopNotifier;

impl Notifier for NoopNotifier {
    fn notify(&self, _notification: &Notification) {}
}

/// A notifier that records every notification in the local cache.
///
/// It writes the row a UI shell drains (`delivered = 0`) and logs at `info`; it
/// never touches a platform API.
#[derive(Debug, Clone)]
pub struct RecordingNotifier {
    db: Arc<ClientDatabase>,
    bridge: Arc<Bridge>,
}

impl RecordingNotifier {
    /// A recorder over `db`.
    pub fn new(db: Arc<ClientDatabase>) -> Self {
        RecordingNotifier {
            db,
            bridge: Arc::new(Bridge::spawn()),
        }
    }

    /// The cache the rows are written to.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// Log at `info` and record the row so a UI shell can pick it up later.
    ///
    /// The payload is stored as its JSON text, the kind as its event name, and
    /// `delivered` stays `0` until the shell says it showed the notification.
    /// There is no platform call here: not `notify-send`, not `osascript`, not a
    /// WinRT toast — the shell owns presentation.
    pub async fn notify_now(&self, notification: &Notification) -> ClientResult<()> {
        record_notification(&self.db, notification).await
    }
}

/// Write one `notifications` row and log it at `info`.
///
/// This is the whole of the "platform dispatch": a log line and a durable row
/// that the UI shell drains. Nothing here shells out to a system notifier.
async fn record_notification(
    db: &ClientDatabase,
    notification: &Notification,
) -> ClientResult<()> {
    let payload = notification.payload.as_ref().map(|value| value.to_string());

    sqlx::query(
        "INSERT INTO notifications (account_id, kind, title, body, payload, created_at, delivered)
         VALUES (?, ?, ?, ?, ?, ?, 0)",
    )
    .bind(notification.account_id)
    .bind(notification.kind.as_str())
    .bind(notification.title.as_str())
    .bind(notification.body.as_deref())
    .bind(payload.as_deref())
    .bind(crate::util::to_rfc3339(notification.at))
    .execute(db.pool())
    .await?;

    tracing::info!(
        kind = notification.kind.as_str(),
        account_id = ?notification.account_id,
        "notification recorded for the UI shell to drain"
    );
    Ok(())
}

impl Notifier for RecordingNotifier {
    /// Record the notification, ignoring a database failure.
    ///
    /// A notification is a courtesy, never a correctness requirement: if SQLite
    /// refuses the row, the failure is logged at `warn` and the caller carries on
    /// with the sync that produced the event.
    ///
    /// [`Notifier::notify`] is synchronous because it is called from the event
    /// loop, while the insert goes through an asynchronous pool — so the write
    /// runs on the bridge thread and this call waits, briefly and boundedly, for
    /// its result.
    fn notify(&self, notification: &Notification) {
        let db = Arc::clone(&self.db);
        let kind = notification.kind.as_str().to_string();
        let notification = notification.clone();
        let (sender, receiver) = mpsc::channel::<ClientResult<()>>();
        let job: BridgeJob = Box::new(move || {
            Box::pin(async move {
                let result = record_notification(&db, &notification).await;
                let _ = sender.send(result);
            })
        });
        if !self.bridge.run(job) {
            tracing::warn!(
                kind = kind.as_str(),
                "the notification thread is gone; the notification was not recorded"
            );
            return;
        }
        match receiver.recv_timeout(PAUSE_LOOKUP_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(
                kind = kind.as_str(),
                error = %err,
                "could not record the notification"
            ),
            Err(_) => tracing::warn!(
                kind = kind.as_str(),
                "timed out recording the notification"
            ),
        }
    }
}

/// The platform-dispatch stub (§34).
///
/// It is honest about what it does: it logs at `info` and records the
/// notification in the `notifications` table, and that is all. The actual toast is
/// drawn by the UI shell, which drains the rows with `delivered = 0` through
/// [`pending_notifications`] — Windows, Linux and macOS each need their own code
/// for that, and it belongs in the shell that already links the platform toolkit,
/// not in this headless core. Nothing here shells out to a system notifier.
#[derive(Debug, Clone)]
pub struct PlatformNotifier {
    recorder: RecordingNotifier,
}

impl PlatformNotifier {
    /// A platform notifier that records through `db`.
    pub fn new(db: Arc<ClientDatabase>) -> Self {
        PlatformNotifier {
            recorder: RecordingNotifier::new(db),
        }
    }

    /// The recorder underneath, for a shell that wants the row back directly.
    pub fn recorder(&self) -> &RecordingNotifier {
        &self.recorder
    }

    /// Record the notification now, reporting a database failure.
    pub async fn notify_now(&self, notification: &Notification) -> ClientResult<()> {
        self.recorder.notify_now(notification).await
    }
}

impl Notifier for PlatformNotifier {
    /// Record the notification; the shell does the rest.
    fn notify(&self, notification: &Notification) {
        self.recorder.notify(notification);
    }
}

/// Whether a notification should be delivered right now (§34).
///
/// The policy is a separate seam from the notifier so the rules can be tested —
/// and changed — without touching delivery: a do-not-disturb window, "the user is
/// already looking at this mailbox", or a per-account mute all belong here.
pub trait NotificationPolicy: Send + Sync + 'static {
    /// Whether this notification should be delivered right now.
    fn should_notify(&self, notification: &Notification) -> bool;
}

/// A policy that lets everything through.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysNotify;

impl NotificationPolicy for AlwaysNotify {
    fn should_notify(&self, _notification: &Notification) -> bool {
        true
    }
}

/// A policy that honours account state: a paused account is silent (§31).
///
/// `mail.*` events for an account whose `accounts.paused` flag is set are
/// suppressed, and so is everything when the user turns notifications off for the
/// client as a whole. An account the cache has never heard of is *not* paused —
/// the safe default is to tell the user rather than to swallow the event.
#[derive(Debug)]
pub struct AccountAwarePolicy {
    db: Arc<ClientDatabase>,
    bridge: Bridge,
    kinds_enabled: AtomicBool,
}

impl AccountAwarePolicy {
    /// A policy over `db`, with notification kinds enabled.
    pub fn new(db: Arc<ClientDatabase>) -> Self {
        AccountAwarePolicy {
            db,
            bridge: Bridge::spawn(),
            kinds_enabled: AtomicBool::new(true),
        }
    }

    /// Turn whole-class suppression on or off.
    ///
    /// `false` suppresses every notification regardless of account — the switch a
    /// "do not disturb" toggle or the §52 通知 `enabled` setting drives. It is
    /// `&self`, not `&mut self`, because the policy is shared behind an `Arc` with
    /// the shell that flips it.
    pub fn set_kinds_enabled(&self, enabled: bool) {
        self.kinds_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Whether whole-class suppression is on.
    pub fn kinds_enabled(&self) -> bool {
        self.kinds_enabled.load(Ordering::SeqCst)
    }

    /// Whether the account is paused (§31).
    ///
    /// The [`NotificationPolicy`] hook is synchronous while the pause flag lives
    /// behind an asynchronous pool, so the lookup runs on the bridge thread and
    /// the caller waits on a channel with a timeout. A failure, a timeout or an
    /// unknown account all mean "not paused": a notification nobody asked to
    /// suppress must not be lost because SQLite was busy.
    fn account_paused(&self, account_id: i64) -> bool {
        let pool = self.db.pool().clone();
        let (sender, receiver) = mpsc::channel::<bool>();

        // One indexed lookup on one connection: no scan, no join, no lock held
        // beyond the statement.
        let job: BridgeJob = Box::new(move || -> BoxFuture<'static, ()> {
            Box::pin(async move {
                let paused = match sqlx::query("SELECT paused FROM accounts WHERE id = ?")
                    .bind(account_id)
                    .fetch_optional(&pool)
                    .await
                {
                    Ok(Some(row)) => row.get::<i64, _>("paused") != 0,
                    Ok(None) => false,
                    Err(err) => {
                        tracing::warn!(
                            account_id,
                            error = %err,
                            "could not read the account's pause flag; treating it as unpaused"
                        );
                        false
                    }
                };
                let _ = sender.send(paused);
            })
        });

        if !self.bridge.run(job) {
            tracing::warn!(
                account_id,
                "the notification policy thread is gone; treating the account as unpaused"
            );
            return false;
        }

        match receiver.recv_timeout(PAUSE_LOOKUP_TIMEOUT) {
            Ok(paused) => paused,
            Err(_) => {
                tracing::warn!(
                    account_id,
                    "timed out reading the account's pause flag; treating it as unpaused"
                );
                false
            }
        }
    }
}

impl NotificationPolicy for AccountAwarePolicy {
    fn should_notify(&self, notification: &Notification) -> bool {
        if !self.kinds_enabled() {
            return false;
        }
        match notification.account_id {
            Some(account_id) => !self.account_paused(account_id),
            None => true,
        }
    }
}

/// One unit of work for [`Bridge`]: a factory that builds the future to run.
type BridgeJob = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// A dedicated thread with its own Tokio runtime.
///
/// [`NotificationPolicy::should_notify`] is synchronous — the seam is called from
/// the event loop — but the pause flag lives in SQLite behind an asynchronous
/// pool. Blocking the caller's runtime is not an option: `Runtime::block_on`
/// panics inside an async context, and parking a current-thread runtime to wait
/// for its own task would deadlock. So the query runs on this thread instead, and
/// the caller waits on a channel with a timeout.
struct Bridge {
    jobs: Mutex<Sender<BridgeJob>>,
}

impl Bridge {
    /// Start the bridge thread.
    ///
    /// A failure to start is not fatal: every lookup then reports "not paused" and
    /// notifications are delivered rather than lost.
    fn spawn() -> Bridge {
        let (sender, receiver) = mpsc::channel::<BridgeJob>();
        let spawned = std::thread::Builder::new()
            .name("ferroma-notifications".to_string())
            .spawn(move || run_bridge(receiver));
        if let Err(err) = spawned {
            tracing::warn!(
                error = %err,
                "could not start the notification policy thread; pause flags will not be read"
            );
        }
        Bridge {
            jobs: Mutex::new(sender),
        }
    }

    /// Queue `job`; `false` when the bridge is gone.
    fn run(&self, job: BridgeJob) -> bool {
        match self.jobs.lock() {
            Ok(jobs) => jobs.send(job).is_ok(),
            Err(_) => false,
        }
    }
}

impl std::fmt::Debug for Bridge {
    /// The queue and the thread are not printable; the name is enough.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Bridge")
    }
}

/// The bridge thread's body: run queued jobs until every sender is gone.
fn run_bridge(receiver: Receiver<BridgeJob>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "the notification policy thread could not start a runtime"
            );
            return;
        }
    };
    while let Ok(job) = receiver.recv() {
        runtime.block_on(job());
    }
}

/// Turns a realtime event into a notification and delivers it (§34).
pub struct NotificationService {
    notifier: Arc<dyn Notifier>,
    policy: Arc<dyn NotificationPolicy>,
}

impl NotificationService {
    /// A service with an explicit delivery and policy seam.
    pub fn new(notifier: Arc<dyn Notifier>, policy: Arc<dyn NotificationPolicy>) -> Self {
        NotificationService { notifier, policy }
    }

    /// The production wiring: record the notification, honour account state.
    pub fn with_recorder(db: Arc<ClientDatabase>) -> Self {
        let notifier: Arc<dyn Notifier> = Arc::new(PlatformNotifier::new(Arc::clone(&db)));
        let policy: Arc<dyn NotificationPolicy> = Arc::new(AccountAwarePolicy::new(db));
        NotificationService::new(notifier, policy)
    }

    /// The notifier this service delivers through.
    pub fn notifier(&self) -> &Arc<dyn Notifier> {
        &self.notifier
    }

    /// The policy this service asks before delivering.
    pub fn policy(&self) -> &Arc<dyn NotificationPolicy> {
        &self.policy
    }

    /// Deliver `notification`, returning whether it was actually delivered.
    ///
    /// A suppressed notification is not delivered *and not recorded*: the policy
    /// is consulted first, so a muted account leaves no row for the shell to
    /// drain and show anyway.
    pub fn notify(&self, notification: &Notification) -> bool {
        if !self.policy.should_notify(notification) {
            tracing::debug!(
                kind = notification.kind.as_str(),
                account_id = ?notification.account_id,
                "notification suppressed"
            );
            return false;
        }
        self.notifier.notify(notification);
        true
    }

    /// Turn a realtime event into a notification and deliver it (§34).
    pub fn handle_event(&self, kind: &str, account_id: Option<i64>, payload: &Value) -> bool {
        self.notify(&Notification::from_event(kind, account_id, payload))
    }
}

/// The row a shell drains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotification {
    /// `notifications.id` — pass it back to [`mark_delivered`].
    pub id: i64,
    /// The account the event belongs to, when it was known.
    pub account_id: Option<i64>,
    /// The event name (`NotificationKind::as_str`).
    pub kind: String,
    /// The banner title.
    pub title: String,
    /// The banner body, when there is one.
    pub body: Option<String>,
    /// When the notification was recorded (RFC 3339, UTC).
    pub created_at: String,
}

impl PendingNotification {
    /// Read one `notifications` row.
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> ClientResult<Self> {
        Ok(PendingNotification {
            id: row.get("id"),
            account_id: row.get("account_id"),
            kind: row.get("kind"),
            title: row.get("title"),
            body: row.get("body"),
            created_at: row.get("created_at"),
        })
    }
}

/// The notifications a shell has not shown yet, oldest first.
///
/// The shell calls this on startup and after every event, shows what it finds -
/// a toast on Windows, libnotify on Linux, `UNUserNotificationCenter` on macOS -
/// and then calls [`mark_delivered`]. `limit` bounds one drain so a long offline
/// stretch cannot open fifty windows at once.
pub async fn pending_notifications(
    db: &ClientDatabase,
    limit: i64,
) -> ClientResult<Vec<PendingNotification>> {
    let rows = sqlx::query(
        "SELECT id, account_id, kind, title, body, created_at
         FROM notifications WHERE delivered = 0 ORDER BY id ASC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?;

    rows.iter().map(PendingNotification::from_row).collect()
}

/// Mark notifications as shown.
///
/// Idempotent, and a no-op for an empty slice; a shell that drains and then
/// crashes before marking simply shows them again, which is the harmless
/// direction to fail in.
pub async fn mark_delivered(db: &ClientDatabase, ids: &[i64]) -> ClientResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool().begin().await?;
    for id in ids {
        sqlx::query("UPDATE notifications SET delivered = 1 WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::json;

    /// A cache with one account: `id = 1`, paused or not.
    async fn db_with_account(paused: bool) -> (TempDir, Arc<ClientDatabase>) {
        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(
            ClientDatabase::open(dir.path().join("c.db"))
                .await
                .expect("open"),
        );
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, paused, created_at, updated_at)
             VALUES (1, 'alice@example.com', 'http://x/api/v1/client', 'dev-1', ?, 'now', 'now')",
        )
        .bind(if paused { 1i64 } else { 0i64 })
        .execute(db.pool())
        .await
        .expect("account");
        (dir, db)
    }

    /// The `mail.received` frame of `docs/fcp.md` §8.
    fn received_frame() -> Value {
        json!({
            "seq": 1842,
            "type": "mail.received",
            "mailbox_id": 3,
            "message_id": 4822,
            "from": "bob@example.net",
            "subject": "Re: Invoice",
            "snippet": "Thanks, got it.",
            "at": "2026-09-16T12:00:00Z"
        })
    }

    /// The banner a `mail.received` frame becomes.
    fn mail_notification(account_id: Option<i64>) -> Notification {
        Notification::from_event("mail.received", account_id, &received_frame())
    }

    /// Every row of `notifications`, oldest first.
    async fn notification_rows(db: &ClientDatabase) -> Vec<sqlx::sqlite::SqliteRow> {
        sqlx::query(
            "SELECT id, account_id, kind, title, body, payload, created_at, delivered
             FROM notifications ORDER BY id ASC",
        )
        .fetch_all(db.pool())
        .await
        .expect("rows")
    }

    #[test]
    fn notification_kinds_round_trip_through_their_wire_names() {
        let kinds = [
            NotificationKind::MailReceived,
            NotificationKind::MailFlagChanged,
            NotificationKind::MailDeleted,
            NotificationKind::MailMoved,
            NotificationKind::DraftUpdated,
            NotificationKind::DeliveryUpdated,
            NotificationKind::DeviceRevoked,
            NotificationKind::ReplayGap,
            NotificationKind::Other("mail.sent".to_string()),
        ];
        for kind in kinds {
            let name = kind.as_str();
            assert_eq!(
                NotificationKind::from_str_opt(name).expect("round trip"),
                kind,
                "{name} did not round-trip"
            );
        }

        assert_eq!(NotificationKind::MailReceived.as_str(), "mail.received");
        assert_eq!(
            NotificationKind::MailFlagChanged.as_str(),
            "mail.flag_changed"
        );
        assert_eq!(NotificationKind::MailDeleted.as_str(), "mail.deleted");
        assert_eq!(NotificationKind::MailMoved.as_str(), "mail.moved");
        assert_eq!(NotificationKind::DraftUpdated.as_str(), "draft.updated");
        assert_eq!(
            NotificationKind::DeliveryUpdated.as_str(),
            "delivery.updated"
        );
        assert_eq!(NotificationKind::DeviceRevoked.as_str(), "device.revoked");
    }

    #[test]
    fn an_unknown_event_kind_maps_to_other() {
        assert_eq!(
            NotificationKind::from_str_opt("mail.read"),
            Some(NotificationKind::Other("mail.read".to_string()))
        );
        assert_eq!(
            NotificationKind::from_str_opt("something.new").expect("known"),
            NotificationKind::Other("something.new".to_string())
        );
        // The §23 gap marker is read under both spellings.
        assert_eq!(
            NotificationKind::from_str_opt("replay_gap"),
            Some(NotificationKind::ReplayGap)
        );
        assert_eq!(NotificationKind::ReplayGap.as_str(), "replay.gap");
        // Only the empty name is unknown-unknown.
        assert_eq!(NotificationKind::from_str_opt(""), None);
    }

    #[test]
    fn notification_kinds_know_the_mail_family() {
        assert!(NotificationKind::MailReceived.is_mail());
        assert!(NotificationKind::MailMoved.is_mail());
        assert!(NotificationKind::Other("mail.sent".to_string()).is_mail());
        assert!(!NotificationKind::DraftUpdated.is_mail());
        assert!(!NotificationKind::DeviceRevoked.is_mail());
        assert!(!NotificationKind::ReplayGap.is_mail());
    }

    #[test]
    fn a_mail_received_event_becomes_the_documented_banner() {
        let notification = Notification::from_event("mail.received", Some(7), &received_frame());
        assert_eq!(notification.kind, NotificationKind::MailReceived);
        assert_eq!(notification.account_id, Some(7));
        assert_eq!(notification.title, "bob@example.net");
        assert_eq!(notification.body.as_deref(), Some("Re: Invoice"));
        assert_eq!(notification.message_id, Some(4822));
        assert_eq!(
            crate::util::to_rfc3339(notification.at),
            "2026-09-16T12:00:00Z",
            "the frame's `at` is the event time"
        );
        let payload = notification.payload.expect("payload kept");
        assert_eq!(payload["snippet"], json!("Thanks, got it."));
        assert_eq!(payload["mailbox_id"], json!(3));
    }

    #[test]
    fn an_unknown_event_kind_becomes_an_other_notification() {
        let payload = json!({"sequence": 9, "device_uid": "dev-1"});
        let notification = Notification::from_event("device.revoked", None, &payload);
        // A known kind, but one without banner content of its own.
        assert_eq!(notification.kind, NotificationKind::DeviceRevoked);
        assert_eq!(notification.title, "Device signed out");
        assert_eq!(notification.body, None);
        assert_eq!(notification.message_id, None);

        let notification = Notification::from_event("team.invited", None, &payload);
        assert_eq!(
            notification.kind,
            NotificationKind::Other("team.invited".to_string())
        );
        assert_eq!(notification.title, "team.invited");
    }

    #[test]
    fn a_banner_always_has_a_title() {
        let notification = Notification::from_event("mail.received", None, &json!({}));
        assert_eq!(notification.title, "New message");
        assert_eq!(notification.body, None);
        assert!(notification.payload.is_some(), "an empty object is still a payload");

        let notification = Notification::from_event("mail.received", None, &json!(null));
        assert_eq!(notification.title, "New message");
        assert!(notification.payload.is_none(), "a null payload is no payload");
        assert!(notification.message_id.is_none());

        // A blank sender must not produce a blank banner.
        let notification = Notification::from_event(
            "mail.received",
            None,
            &json!({"from": "  ", "subject": "   "}),
        );
        assert_eq!(notification.title, "New message");
        assert_eq!(notification.body, None);
    }

    #[test]
    fn a_frame_without_a_timestamp_is_stamped_now() {
        let before = Utc::now();
        let notification = Notification::from_event("draft.updated", None, &json!({"draft_id": 5}));
        assert!(notification.at >= before);
        assert!(notification.at <= Utc::now());
    }

    #[tokio::test]
    async fn a_recording_notifier_writes_exactly_one_row() {
        let (_dir, db) = db_with_account(false).await;
        let notifier = RecordingNotifier::new(Arc::clone(&db));
        notifier
            .notify_now(&mail_notification(Some(1)))
            .await
            .expect("record");

        let rows = notification_rows(&db).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.get::<Option<i64>, _>("account_id"), Some(1));
        assert_eq!(row.get::<String, _>("kind"), "mail.received");
        assert_eq!(row.get::<String, _>("title"), "bob@example.net");
        assert_eq!(
            row.get::<Option<String>, _>("body").as_deref(),
            Some("Re: Invoice")
        );
        assert_eq!(row.get::<i64, _>("delivered"), 0);
        assert_eq!(row.get::<String, _>("created_at"), "2026-09-16T12:00:00Z");
        let payload: Value = serde_json::from_str(&row.get::<String, _>("payload")).expect("json");
        assert_eq!(payload["message_id"], json!(4822));
        assert_eq!(payload["from"], json!("bob@example.net"));
        assert_eq!(payload["type"], json!("mail.received"));
    }

    #[tokio::test]
    async fn a_notification_without_a_payload_is_recorded_with_a_null_payload() {
        let (_dir, db) = db_with_account(false).await;
        let notifier = RecordingNotifier::new(Arc::clone(&db));
        let notification = Notification::from_event("replay_gap", None, &json!(null));
        notifier.notify_now(&notification).await.expect("record");

        let rows = notification_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("kind"), "replay.gap");
        assert_eq!(rows[0].get::<Option<String>, _>("payload"), None);
        assert_eq!(rows[0].get::<Option<i64>, _>("account_id"), None);
    }

    #[tokio::test]
    async fn the_noop_notifier_writes_nothing() {
        let (_dir, db) = db_with_account(false).await;
        let notifier = NoopNotifier;
        notifier.notify(&mail_notification(Some(1)));
        assert!(notification_rows(&db).await.is_empty());
    }

    #[tokio::test]
    async fn the_platform_notifier_records_and_shells_out_to_nothing() {
        let (_dir, db) = db_with_account(false).await;
        let notifier = PlatformNotifier::new(Arc::clone(&db));
        // The trait method: it records, logs, and returns. Nothing is spawned and
        // no platform API is called — the UI shell drains the row.
        notifier.notify(&mail_notification(Some(1)));
        notifier.notify_now(&mail_notification(Some(1))).await.expect("record");

        let rows = notification_rows(&db).await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.get::<i64, _>("delivered") == 0));
        assert_eq!(notifier.recorder().database().path(), db.path());

        let pending = pending_notifications(&db, 10).await.expect("pending");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].kind, "mail.received");
        assert_eq!(pending[0].title, "bob@example.net");
    }

    #[tokio::test]
    async fn a_paused_account_is_suppressed() {
        let (_dir, paused_db) = db_with_account(true).await;
        let policy = AccountAwarePolicy::new(Arc::clone(&paused_db));

        assert!(
            !policy.should_notify(&mail_notification(Some(1))),
            "§31: a paused account must not notify"
        );
        // The same policy over an unpaused account allows it.
        let (_dir2, running_db) = db_with_account(false).await;
        let running = AccountAwarePolicy::new(running_db);
        assert!(running.should_notify(&mail_notification(Some(1))));
    }

    #[tokio::test]
    async fn an_unknown_account_and_no_account_are_both_allowed() {
        let (_dir, db) = db_with_account(false).await;
        let policy = AccountAwarePolicy::new(db);

        // Account 99 was never cached: the safe answer is to tell the user.
        assert!(policy.should_notify(&mail_notification(Some(99))));
        assert!(policy.should_notify(&mail_notification(None)));
        assert!(policy.kinds_enabled());
    }

    #[tokio::test]
    async fn disabling_kinds_suppresses_everything() {
        let (_dir, db) = db_with_account(false).await;
        let policy = AccountAwarePolicy::new(Arc::clone(&db));
        policy.set_kinds_enabled(false);
        assert!(!policy.kinds_enabled());

        assert!(!policy.should_notify(&mail_notification(Some(1))));
        assert!(!policy.should_notify(&mail_notification(None)));

        policy.set_kinds_enabled(true);
        assert!(policy.should_notify(&mail_notification(Some(1))));
    }

    #[tokio::test]
    async fn the_service_records_a_received_frame() {
        let (_dir, db) = db_with_account(false).await;
        let service = NotificationService::with_recorder(Arc::clone(&db));

        assert!(service.handle_event("mail.received", Some(1), &received_frame()));
        assert!(service.notify(&mail_notification(None)));

        let rows = notification_rows(&db).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<String, _>("title"), "bob@example.net");
        assert_eq!(rows[1].get::<Option<i64>, _>("account_id"), None);
    }

    #[tokio::test]
    async fn a_suppressed_notification_is_neither_delivered_nor_recorded() {
        let (_dir, db) = db_with_account(true).await;
        let policy = Arc::new(AccountAwarePolicy::new(Arc::clone(&db)));
        let notifier: Arc<dyn Notifier> = Arc::new(PlatformNotifier::new(Arc::clone(&db)));
        let service = NotificationService::new(notifier, policy.clone());

        assert!(!service.notify(&mail_notification(Some(1))));
        assert!(!service.handle_event("mail.received", Some(1), &received_frame()));
        assert!(notification_rows(&db).await.is_empty());

        // And the same for a global mute, with no account involved at all.
        policy.set_kinds_enabled(false);
        assert!(!service.notify(&mail_notification(None)));
        assert!(notification_rows(&db).await.is_empty());
    }

    #[tokio::test]
    async fn a_policy_of_always_notify_delivers_everything() {
        let (_dir, db) = db_with_account(false).await;
        let notifier: Arc<dyn Notifier> = Arc::new(RecordingNotifier::new(Arc::clone(&db)));
        let service = NotificationService::new(notifier, Arc::new(AlwaysNotify));

        assert!(service.handle_event("device.revoked", Some(1), &json!({"device_uid": "dev-1"})));
        assert!(service.handle_event("team.invited", None, &json!({})));
        assert!(service.policy().should_notify(&mail_notification(None)));

        let pending = pending_notifications(&db, 10).await.expect("pending");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].kind, "device.revoked");
        assert_eq!(pending[1].kind, "team.invited");
    }

    #[tokio::test]
    async fn pending_notifications_are_oldest_first_and_marking_clears_them() {
        let (_dir, db) = db_with_account(false).await;
        let notifier = RecordingNotifier::new(Arc::clone(&db));
        for subject in ["First", "Second", "Third"] {
            let payload = json!({"from": "bob@example.net", "subject": subject});
            notifier
                .notify_now(&Notification::from_event(
                    "mail.received",
                    Some(1),
                    &payload,
                ))
                .await
                .expect("record");
        }

        let pending = pending_notifications(&db, 10).await.expect("pending");
        let bodies: Vec<Option<String>> = pending.iter().map(|row| row.body.clone()).collect();
        assert_eq!(
            bodies,
            vec![
                Some("First".to_string()),
                Some("Second".to_string()),
                Some("Third".to_string())
            ]
        );
        assert!(pending.windows(2).all(|pair| pair[0].id < pair[1].id));

        // `limit` bounds one drain.
        assert_eq!(pending_notifications(&db, 2).await.expect("pending").len(), 2);

        let ids: Vec<i64> = pending.iter().map(|row| row.id).collect();
        mark_delivered(&db, &ids).await.expect("mark");
        assert!(pending_notifications(&db, 10).await.expect("pending").is_empty());
        // The rows are still there, just delivered.
        assert_eq!(notification_rows(&db).await.len(), 3);
        // Marking again is harmless.
        mark_delivered(&db, &ids).await.expect("mark again");
        mark_delivered(&db, &[]).await.expect("no ids is a no-op");
    }

    #[tokio::test]
    async fn marking_only_some_notifications_delivered_keeps_the_rest() {
        let (_dir, db) = db_with_account(false).await;
        let notifier = RecordingNotifier::new(Arc::clone(&db));
        for subject in ["First", "Second"] {
            let payload = json!({"from": "bob@example.net", "subject": subject});
            notifier
                .notify_now(&Notification::from_event(
                    "mail.received",
                    Some(1),
                    &payload,
                ))
                .await
                .expect("record");
        }

        let pending = pending_notifications(&db, 10).await.expect("pending");
        assert_eq!(pending.len(), 2);
        mark_delivered(&db, &[pending[0].id])
            .await
            .expect("mark one");

        let left = pending_notifications(&db, 10).await.expect("pending");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].body.as_deref(), Some("Second"));
    }
}
