//! The FCP HTTP client (`docs/fcp.md`, `docs/api.md` §6).
//!
//! [`FcpClient`] is a thin, honest wrapper around the Ferroma Client Protocol:
//!
//! * it announces itself on every request (§1 of FCP);
//! * it holds the access token in memory and the refresh token in the account
//!   record, refreshing **once** on `401` and giving up on the second one (§11);
//! * it parses the documented error envelope into a typed [`ApiError`] — a
//!   hostile or buggy server produces an error, never a panic;
//! * it retries `429` (honouring `Retry-After`), `5xx` and transport failures
//!   with bounded exponential backoff and jitter, and never retries another
//!   `4xx`.
//!
//! Nothing in this module writes to the local cache: it is the wire, not the
//! policy.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use ferroma_core::{OperationId, PROTOCOL_VERSION};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, RANGE, USER_AGENT};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::{BoxFuture, ClientError, ClientResult};

/// The client name sent in `X-Ferroma-Client` and `User-Agent`.
pub const CLIENT_NAME: &str = "FerromaClient";

/// Header announcing which client is calling.
pub const HEADER_CLIENT: &str = "X-Ferroma-Client";

/// Header announcing which protocol version the client speaks.
pub const HEADER_PROTOCOL: &str = "X-Ferroma-Protocol";

/// Header announcing the operating system.
pub const HEADER_PLATFORM: &str = "X-Ferroma-Platform";

/// The protocol version this client implements.
pub const CLIENT_PROTOCOL_VERSION: u32 = PROTOCOL_VERSION;

/// `Windows`/`Linux`/`macOS`, as the `platform` field of the device record.
pub const fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else {
        "unknown"
    }
}

/// `FerromaClient/0.1.0` — the value of `X-Ferroma-Client`.
pub fn client_version_string() -> String {
    format!("{CLIENT_NAME}/{}", ferroma_core::VERSION)
}

/// A structured FCP error: the documented envelope of `docs/api.md` §1.3.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("HTTP {status} {code}: {message}")]
pub struct ApiError {
    /// The HTTP status code.
    pub status: u16,
    /// The stable, machine-readable code (`FerromaError::code()` server-side).
    pub code: String,
    /// The human-readable message. May change between server versions.
    pub message: String,
    /// Optional structured detail, present only when the server had something
    /// specific to say (for example `{"field": "to"}`).
    pub details: Option<serde_json::Value>,
    /// The wait a `429` asked for, parsed from `Retry-After`.
    pub retry_after: Option<Duration>,
}

impl ApiError {
    /// Build an error from its parts.
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
            retry_after: None,
        }
    }

    /// Whether retrying could plausibly succeed (`docs/fcp.md` §11).
    pub fn is_retryable(&self) -> bool {
        self.status == 429 || (500..600).contains(&self.status)
    }

    /// Whether the server said the client's protocol is too old (`426`).
    pub fn is_upgrade_required(&self) -> bool {
        self.status == 426
    }

    /// Parse a response body into an [`ApiError`].
    ///
    /// A body that is not the documented envelope — HTML from a proxy, a
    /// truncated stream, an empty body — still yields a usable error: the code
    /// degrades to `http_<status>` and the message carries a truncated excerpt.
    pub fn parse(status: u16, body: &[u8], retry_after: Option<Duration>) -> Self {
        let fallback_message = || {
            let text = String::from_utf8_lossy(body);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                format!("HTTP {status} with an empty body")
            } else {
                format!("HTTP {status}: {}", truncate(trimmed, 200))
            }
        };

        let parsed: Option<Envelope> = serde_json::from_slice(body).ok();
        match parsed {
            Some(envelope) => ApiError {
                status,
                code: envelope.error.code,
                message: envelope.error.message,
                details: envelope.error.details,
                retry_after,
            },
            None => ApiError {
                status,
                code: format!("http_{status}"),
                message: fallback_message(),
                details: None,
                retry_after,
            },
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    error: EnvelopeError,
}

#[derive(Deserialize)]
struct EnvelopeError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    details: Option<serde_json::Value>,
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let head: String = value.chars().take(max).collect();
    format!("{head}…")
}

/// Bounded exponential backoff with jitter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Total attempts, including the first one. `1` disables retrying.
    pub max_attempts: u32,
    /// The delay before the second attempt.
    pub base_delay: Duration,
    /// The ceiling for a single backoff sleep.
    pub max_delay: Duration,
    /// Fraction of the delay that is randomised, `0.0`..=`1.0`.
    pub jitter: f64,
    /// A `Retry-After` larger than this is clamped, so a hostile server cannot
    /// park the client for an hour.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(20),
            jitter: 0.25,
            max_retry_after: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries — used by tests and by the CLI's `--no-retry`.
    pub fn none() -> Self {
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        }
    }

    /// Whether another attempt is allowed after `attempt` attempts have been made.
    pub fn allows(&self, attempt: u32) -> bool {
        attempt < self.max_attempts
    }

    /// The backoff before attempt number `attempt + 1` (1-based: `delay_for(1)`
    /// is the delay after the first failure).
    ///
    /// Deterministic when `jitter == 0.0`, which is what the tests assert.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exp = attempt.saturating_sub(1).min(16);
        let base = self.base_delay.as_millis() as u64;
        let capped = self.max_delay.as_millis() as u64;
        let mut millis = base.saturating_mul(1u64 << exp).min(capped);
        if self.jitter > 0.0 {
            let spread = (millis as f64 * self.jitter.clamp(0.0, 1.0)) as u64;
            if spread > 0 {
                let roll = rand::random::<u64>() % (spread * 2 + 1);
                millis = millis.saturating_sub(spread).saturating_add(roll);
            }
        }
        Duration::from_millis(millis)
    }

    /// The sleep a `429` asks for, clamped to [`RetryPolicy::max_retry_after`].
    pub fn retry_after_or(&self, requested: Option<Duration>, attempt: u32) -> Duration {
        match requested {
            Some(wait) => wait.min(self.max_retry_after),
            None => self.delay_for(attempt),
        }
    }
}

/// Where the rotating refresh token is persisted.
///
/// `docs/fcp.md` §2: the access token lives in memory, but the refresh token has
/// to survive a restart — it lives in the account record. The trait keeps the
/// API client independent of SQLite.
pub trait TokenStore: Send + Sync + 'static {
    /// The stored refresh token, if the account has one.
    fn load(&self) -> BoxFuture<'_, Option<String>>;

    /// Persist a freshly rotated refresh token.
    fn save(&self, refresh_token: String) -> BoxFuture<'_, ClientResult<()>>;

    /// Forget both tokens (logout, or a revoked device).
    fn clear(&self) -> BoxFuture<'_, ClientResult<()>>;
}

/// An in-memory [`TokenStore`], used by tests and by the CLI before an account
/// exists.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    token: std::sync::Mutex<Option<String>>,
}

impl MemoryTokenStore {
    /// An empty store.
    pub fn new() -> Self {
        MemoryTokenStore::default()
    }

    /// An empty store that already holds `refresh_token`.
    pub fn with_token(refresh_token: impl Into<String>) -> Self {
        MemoryTokenStore {
            token: std::sync::Mutex::new(Some(refresh_token.into())),
        }
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> BoxFuture<'_, Option<String>> {
        let value = self.token.lock().ok().and_then(|guard| guard.clone());
        Box::pin(async move { value })
    }

    fn save(&self, refresh_token: String) -> BoxFuture<'_, ClientResult<()>> {
        if let Ok(mut guard) = self.token.lock() {
            *guard = Some(refresh_token);
        }
        Box::pin(async move { Ok(()) })
    }

    fn clear(&self) -> BoxFuture<'_, ClientResult<()>> {
        if let Ok(mut guard) = self.token.lock() {
            *guard = None;
        }
        Box::pin(async move { Ok(()) })
    }
}

/// How the client identifies itself to `POST /auth/login`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    /// A client-generated uid, stable for the installation.
    pub device_uid: String,
    /// A human name, e.g. `Alice's laptop`.
    pub name: String,
    /// `windows` | `linux` | `macos` | `android` | `ios`.
    pub platform: String,
    /// The client build, e.g. `0.1.0`.
    pub client_version: String,
}

impl DeviceInfo {
    /// A device record for this installation.
    pub fn local(device_uid: impl Into<String>, name: impl Into<String>) -> Self {
        DeviceInfo {
            device_uid: device_uid.into(),
            name: name.into(),
            platform: platform_name().to_string(),
            client_version: ferroma_core::VERSION.to_string(),
        }
    }
}

/// A user as the client API reports it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UserRef {
    /// Server-side user id.
    pub id: i64,
    /// The account's address.
    pub email: String,
    /// Optional display name.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// The token pair plus the device id (`docs/fcp.md` §2).
#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    /// The short-lived bearer token.
    pub access_token: String,
    /// The single-use refresh token; it rotates on every refresh.
    pub refresh_token: String,
    /// Always `Bearer` today, but read from the wire anyway.
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Access-token lifetime in seconds.
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
    /// The `devices` row this login created.
    #[serde(default)]
    pub device_id: Option<i64>,
    /// The authenticated user.
    pub user: UserRef,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

fn default_expires_in() -> u64 {
    3600
}

/// The response of `POST /auth/refresh`.
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshResponse {
    /// The new access token.
    pub access_token: String,
    /// The new refresh token; the previous one is now invalid.
    pub refresh_token: String,
    /// Access-token lifetime in seconds.
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
}

/// Limits advertised by `GET /account` (`docs/fcp.md` §1).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ServerLimits {
    /// Largest message the server accepts.
    #[serde(default = "default_max_message_size")]
    pub max_message_size: u64,
    /// Maximum recipients per message.
    #[serde(default)]
    pub max_recipients: usize,
    /// Bytes per attachment upload chunk.
    #[serde(default = "default_chunk_size")]
    pub attachment_chunk_size: u64,
    /// Changes per sync page.
    #[serde(default = "default_sync_page_size")]
    pub sync_page_size: usize,
}

fn default_max_message_size() -> u64 {
    26_214_400
}
fn default_chunk_size() -> u64 {
    1_048_576
}
fn default_sync_page_size() -> usize {
    500
}

impl Default for ServerLimits {
    fn default() -> Self {
        ServerLimits {
            max_message_size: default_max_message_size(),
            max_recipients: 100,
            attachment_chunk_size: default_chunk_size(),
            sync_page_size: default_sync_page_size(),
        }
    }
}

/// `GET /account`: the negotiated versions and the mailbox list.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountInfo {
    /// The authenticated user.
    pub user: UserRef,
    /// The addresses this account may send from.
    #[serde(default)]
    pub mailboxes: Vec<MailboxInfo>,
    /// The protocol version the server answered with.
    #[serde(default = "default_protocol")]
    pub protocol_version: u32,
    /// The oldest protocol the server still accepts.
    #[serde(default = "default_protocol")]
    pub min_protocol_version: u32,
    /// The server build.
    #[serde(default)]
    pub server_version: String,
    /// The server's hostname, shown in the "about" pane.
    #[serde(default)]
    pub server_hostname: String,
    /// Advertised limits.
    #[serde(default)]
    pub limits: ServerLimits,
    /// Optional capability flags (`sync`, `events`, `drafts`, …).
    #[serde(default)]
    pub features: Vec<String>,
}

fn default_protocol() -> u32 {
    CLIENT_PROTOCOL_VERSION
}

impl AccountInfo {
    /// Whether the server advertises `feature`.
    ///
    /// A server that sends no `features` array at all is treated as supporting
    /// the mandatory core; an explicitly listed feature must be present.
    pub fn supports(&self, feature: &str) -> bool {
        self.features.is_empty() || self.features.iter().any(|f| f == feature)
    }
}

/// One address of an account, with its folder tree.
#[derive(Debug, Clone, Deserialize)]
pub struct MailboxInfo {
    /// Server-side mailbox id.
    pub id: i64,
    /// The address, e.g. `alice@example.com`.
    pub address: String,
    /// Optional display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Whether this is the default `From:` address.
    #[serde(default)]
    pub is_primary: bool,
    /// The folders in this mailbox.
    #[serde(default)]
    pub folders: Vec<FolderInfo>,
}

/// A folder, with the counters the UI and the first-sync progress bar need.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FolderInfo {
    /// Server-side folder id.
    pub id: i64,
    /// The name, `/`-separated for nesting.
    pub name: String,
    /// `\Sent`, `\Drafts`, `\Trash`, … — the client maps this, never the name.
    #[serde(default)]
    pub special_use: Option<String>,
    /// Messages in the folder.
    #[serde(default)]
    pub message_count: i64,
    /// Unseen messages in the folder.
    #[serde(default)]
    pub unseen_count: i64,
    /// Renumbering epoch; a change wipes the folder's cache.
    #[serde(default)]
    pub uid_validity: i64,
    /// The next UID the server will hand out.
    #[serde(default)]
    pub uid_next: i64,
}

/// One entry of a sync page (`docs/fcp.md` §3).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Change {
    /// A message appeared in a folder. Metadata only — the body is lazy.
    MessageCreated {
        /// Global, per-user sequence number.
        seq: i64,
        /// The message.
        message_id: i64,
        /// The IMAP UID inside the folder, when the server knows it.
        #[serde(default)]
        uid: Option<i64>,
        /// The folder, when the change is not already scoped to one.
        #[serde(default)]
        folder_id: Option<i64>,
    },
    /// Flags or other metadata changed.
    MessageUpdated {
        /// Sequence number.
        seq: i64,
        /// The message.
        message_id: i64,
        /// The new flag set, space separated.
        #[serde(default)]
        flags: Option<String>,
        /// The folder, when the server restates it.
        #[serde(default)]
        folder_id: Option<i64>,
        /// The UID, when the server restates it.
        #[serde(default)]
        uid: Option<i64>,
        /// Whether the message now has attachments.
        #[serde(default)]
        has_attachments: Option<bool>,
    },
    /// A message is gone; the change outlives the row as a tombstone.
    MessageDeleted {
        /// Sequence number.
        seq: i64,
        /// The message.
        message_id: i64,
    },
    /// A message moved between folders.
    MessageMoved {
        /// Sequence number.
        seq: i64,
        /// The message.
        message_id: i64,
        /// Where it came from.
        #[serde(default)]
        from_folder_id: Option<i64>,
        /// Where it went.
        #[serde(default)]
        to_folder_id: Option<i64>,
        /// The UID in the destination folder.
        #[serde(default)]
        uid: Option<i64>,
    },
    /// A folder was created.
    FolderCreated {
        /// Sequence number.
        seq: i64,
        /// The folder.
        folder_id: i64,
        /// Its name.
        #[serde(default)]
        name: Option<String>,
        /// Its mailbox.
        #[serde(default)]
        mailbox_id: Option<i64>,
        /// Its special-use flag.
        #[serde(default)]
        special_use: Option<String>,
    },
    /// A folder was renamed, recounted, or renumbered.
    FolderUpdated {
        /// Sequence number.
        seq: i64,
        /// The folder.
        folder_id: i64,
        /// The new name, when it changed.
        #[serde(default)]
        name: Option<String>,
        /// The new UID epoch — a change invalidates the folder's cache.
        #[serde(default)]
        uid_validity: Option<i64>,
        /// The new message count.
        #[serde(default)]
        message_count: Option<i64>,
        /// The new unseen count.
        #[serde(default)]
        unseen_count: Option<i64>,
        /// The new UID next value.
        #[serde(default)]
        uid_next: Option<i64>,
    },
    /// A folder was deleted.
    FolderDeleted {
        /// Sequence number.
        seq: i64,
        /// The folder.
        folder_id: i64,
    },
    /// A draft appeared on another device.
    DraftCreated {
        /// Sequence number.
        seq: i64,
        /// The draft.
        draft_id: i64,
    },
    /// A draft changed elsewhere.
    DraftUpdated {
        /// Sequence number.
        seq: i64,
        /// The draft.
        draft_id: i64,
    },
    /// A draft was deleted elsewhere.
    DraftDeleted {
        /// Sequence number.
        seq: i64,
        /// The draft.
        draft_id: i64,
    },
    /// A change type this client build does not know about.
    ///
    /// It is deliberately *not* an error: a newer server may add change types,
    /// and an older client must keep advancing its cursor rather than get stuck.
    #[serde(other)]
    Unknown,
}

impl Change {
    /// The global sequence number, or `None` for an unknown change type.
    pub fn seq(&self) -> Option<i64> {
        match self {
            Change::MessageCreated { seq, .. }
            | Change::MessageUpdated { seq, .. }
            | Change::MessageDeleted { seq, .. }
            | Change::MessageMoved { seq, .. }
            | Change::FolderCreated { seq, .. }
            | Change::FolderUpdated { seq, .. }
            | Change::FolderDeleted { seq, .. }
            | Change::DraftCreated { seq, .. }
            | Change::DraftUpdated { seq, .. }
            | Change::DraftDeleted { seq, .. } => Some(*seq),
            Change::Unknown => None,
        }
    }

    /// A short name for logs and the progress callback (`message_created`, …).
    pub fn kind(&self) -> &'static str {
        match self {
            Change::MessageCreated { .. } => "message_created",
            Change::MessageUpdated { .. } => "message_updated",
            Change::MessageDeleted { .. } => "message_deleted",
            Change::MessageMoved { .. } => "message_moved",
            Change::FolderCreated { .. } => "folder_created",
            Change::FolderUpdated { .. } => "folder_updated",
            Change::FolderDeleted { .. } => "folder_deleted",
            Change::DraftCreated { .. } => "draft_created",
            Change::DraftUpdated { .. } => "draft_updated",
            Change::DraftDeleted { .. } => "draft_deleted",
            Change::Unknown => "unknown",
        }
    }
}

/// One page of the change log.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncPage {
    /// The cursor to store **after** every change has been applied.
    pub next_cursor: String,
    /// Whether another page is waiting.
    #[serde(default)]
    pub has_more: bool,
    /// The changes, in ascending `seq` order.
    #[serde(default)]
    pub changes: Vec<Change>,
}

/// One recipient or sender on the wire.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AddressDto {
    /// The address.
    pub address: String,
    /// The display name, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl AddressDto {
    /// The address alone.
    pub fn bare(address: impl Into<String>) -> Self {
        AddressDto {
            address: address.into(),
            name: None,
        }
    }
}

/// A message as it appears in a list.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MessageItem {
    /// Server-side message id.
    pub id: i64,
    /// IMAP UID inside the folder.
    #[serde(default)]
    pub uid: Option<i64>,
    /// The folder it lives in.
    #[serde(default)]
    pub folder_id: Option<i64>,
    /// The subject.
    #[serde(default)]
    pub subject: Option<String>,
    /// The sender.
    #[serde(default, rename = "from")]
    pub from: Option<AddressDto>,
    /// The recipients.
    #[serde(default)]
    pub to: Vec<AddressDto>,
    /// A short plain-text preview.
    #[serde(default)]
    pub snippet: Option<String>,
    /// Space separated flags.
    #[serde(default)]
    pub flags: Option<String>,
    /// Size on disk.
    #[serde(default)]
    pub size_bytes: u64,
    /// Whether the message has attachments.
    #[serde(default)]
    pub has_attachments: bool,
    /// How many attachments.
    #[serde(default)]
    pub attachment_count: i64,
    /// When the server received it.
    #[serde(default)]
    pub internal_date: Option<String>,
    /// The `Date:` header.
    #[serde(default)]
    pub sent_at: Option<String>,
    /// The RFC 5322 `Message-ID`.
    #[serde(default)]
    pub rfc_message_id: Option<String>,
}

/// A page of messages.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MessageList {
    /// The messages, newest first.
    #[serde(default)]
    pub items: Vec<MessageItem>,
    /// The total number of matches, for paging.
    #[serde(default)]
    pub total: i64,
    /// The page size the server used.
    #[serde(default)]
    pub limit: i64,
    /// The offset the server used.
    #[serde(default)]
    pub offset: i64,
}

/// One header of a full message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HeaderDto {
    /// Header name.
    pub name: String,
    /// Header value, unfolded.
    pub value: String,
}

/// Attachment metadata.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AttachmentDto {
    /// Server-side attachment id.
    pub id: i64,
    /// The file name.
    #[serde(default)]
    pub filename: String,
    /// The MIME type.
    #[serde(default)]
    pub content_type: String,
    /// Size in bytes.
    #[serde(default)]
    pub size_bytes: u64,
    /// The blob digest, when the server computed one.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The `Content-ID`, for inline images.
    #[serde(default)]
    pub content_id: Option<String>,
    /// `attachment` or `inline`.
    #[serde(default)]
    pub disposition: Option<String>,
}

/// A full message: list item plus bodies, attachments and raw headers.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MessageDetail {
    /// The list-item fields.
    #[serde(flatten)]
    pub item: MessageItem,
    /// The plain-text body.
    #[serde(default)]
    pub text_body: Option<String>,
    /// The HTML body, sanitised server-side when configured.
    #[serde(default)]
    pub html_body: Option<String>,
    /// The attachments.
    #[serde(default)]
    pub attachments: Vec<AttachmentDto>,
    /// The raw header list, in wire order.
    #[serde(default)]
    pub headers: Vec<HeaderDto>,
}

/// The body of `POST /messages`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SendRequest {
    /// The idempotency key for this send, generated when the item entered the
    /// outbox — never at send time.
    pub operation_id: String,
    /// Which address to send from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox_id: Option<i64>,
    /// The `From:` address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The `To:` addresses.
    #[serde(default)]
    pub to: Vec<String>,
    /// The `Cc:` addresses.
    #[serde(default)]
    pub cc: Vec<String>,
    /// The `Bcc:` addresses.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// The subject.
    #[serde(default)]
    pub subject: String,
    /// The plain-text body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The HTML body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// The ids of previously uploaded attachments.
    #[serde(default)]
    pub attachment_ids: Vec<i64>,
    /// The `In-Reply-To` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// The `References` header.
    #[serde(default)]
    pub references: Vec<String>,
}

/// What the server did with a submission (`docs/api.md` §7).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SendResponse {
    /// The stored copy in the Sent folder.
    #[serde(default)]
    pub message_id: Option<i64>,
    /// How many recipients were enqueued.
    #[serde(default)]
    pub queued: i64,
    /// The recipient addresses that were accepted.
    #[serde(default)]
    pub recipients: Vec<String>,
}

/// The flags `PATCH /messages/:id` can change.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessagePatch {
    /// Mark seen/unseen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<bool>,
    /// Mark flagged/unflagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flagged: Option<bool>,
    /// Mark answered/unanswered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered: Option<bool>,
    /// Mark deleted/undeleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
}

/// The single-message actions `docs/fcp.md` §5 exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAction {
    /// `POST /messages/:id/read`
    Read,
    /// `POST /messages/:id/unread`
    Unread,
    /// `POST /messages/:id/star`
    Star,
    /// `POST /messages/:id/archive`
    Archive,
    /// `POST /messages/:id/trash`
    Trash,
}

impl MessageAction {
    /// The path segment of the action.
    pub fn path_segment(self) -> &'static str {
        match self {
            MessageAction::Read => "read",
            MessageAction::Unread => "unread",
            MessageAction::Star => "star",
            MessageAction::Archive => "archive",
            MessageAction::Trash => "trash",
        }
    }
}

/// A draft as it travels to the server (`docs/fcp.md` §7).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DraftPayload {
    /// The subject.
    #[serde(default)]
    pub subject: String,
    /// The plain-text body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The HTML body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// The `To:` addresses.
    #[serde(default)]
    pub to: Vec<String>,
    /// The `Cc:` addresses.
    #[serde(default)]
    pub cc: Vec<String>,
    /// The `Bcc:` addresses.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// The `In-Reply-To` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// The `References` header.
    #[serde(default)]
    pub references: Vec<String>,
    /// Already-uploaded attachment ids.
    #[serde(default)]
    pub attachment_ids: Vec<i64>,
}

/// What the server says about an overwritten draft (`docs/fcp.md` §7).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DraftConflict {
    /// Always true when present.
    #[serde(default)]
    pub detected: bool,
    /// The `updated_at` the client overwrote.
    #[serde(default)]
    pub server_updated_at: Option<String>,
}

/// A stored draft.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DraftRecord {
    /// Server-side draft id.
    pub id: i64,
    /// The subject.
    #[serde(default)]
    pub subject: String,
    /// The plain-text body.
    #[serde(default)]
    pub text: Option<String>,
    /// The HTML body.
    #[serde(default)]
    pub html: Option<String>,
    /// The `To:` addresses.
    #[serde(default)]
    pub to: Vec<String>,
    /// The `Cc:` addresses.
    #[serde(default)]
    pub cc: Vec<String>,
    /// The `Bcc:` addresses.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// When it was last written.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// What this write overwrote, if anything.
    #[serde(default)]
    pub conflict: Option<DraftConflict>,
}

/// A device as `GET /devices` reports it (`docs/fcp.md` §9).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DeviceRecord {
    /// Server-side device id.
    pub id: i64,
    /// The client-generated installation uid.
    #[serde(default)]
    pub device_uid: String,
    /// Human name.
    #[serde(default)]
    pub name: Option<String>,
    /// Operating system.
    #[serde(default)]
    pub platform: Option<String>,
    /// Client build.
    #[serde(default)]
    pub client_version: Option<String>,
    /// Protocol version.
    #[serde(default)]
    pub protocol_version: Option<u32>,
    /// Last time the device called.
    #[serde(default)]
    pub last_seen_at: Option<String>,
    /// Last address it called from.
    #[serde(default)]
    pub last_ip: Option<String>,
    /// When it was first seen.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Whether it is revoked.
    #[serde(default)]
    pub revoked: bool,
}

/// `GET /devices`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DeviceList {
    /// The devices.
    #[serde(default)]
    pub devices: Vec<DeviceRecord>,
}

/// `POST /attachments/init`.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadInit {
    /// The id to address the upload with.
    pub attachment_id: i64,
    /// The size every chunk must have except the last.
    #[serde(default = "default_chunk_size")]
    pub chunk_size: u64,
    /// A token that authorises the chunk requests.
    #[serde(default)]
    pub upload_token: String,
}

/// `GET /attachments/:id/status` — which chunks the server already holds.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UploadStatus {
    /// The attachment being uploaded.
    #[serde(default)]
    pub attachment_id: i64,
    /// The size every chunk must have except the last.
    #[serde(default = "default_chunk_size")]
    pub chunk_size: u64,
    /// The indices the server holds. Chunks may be uploaded out of order.
    #[serde(default)]
    pub received: Vec<u64>,
    /// Whether the upload has been completed.
    #[serde(default)]
    pub complete: bool,
    /// Total size, when known.
    #[serde(default)]
    pub size_bytes: u64,
}

/// `POST /attachments/:id/complete`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UploadComplete {
    /// The finished attachment.
    #[serde(default)]
    pub id: i64,
    /// Its file name.
    #[serde(default)]
    pub filename: String,
    /// Its MIME type.
    #[serde(default)]
    pub content_type: String,
    /// Its size.
    #[serde(default)]
    pub size_bytes: u64,
    /// The digest the server verified.
    #[serde(default)]
    pub sha256: Option<String>,
}

/// What a request needs, independent of how many times it is attempted.
#[derive(Debug, Clone)]
struct RequestSpec {
    method: Method,
    path: String,
    query: Vec<(String, String)>,
    body: Option<serde_json::Value>,
    /// Whether to attach the bearer token.
    authenticated: bool,
    /// Raw body overrides `body` (used by the simple attachment upload path).
    raw: Option<(Vec<u8>, String)>,
    /// An extra `Range` header, for resumable downloads.
    range: Option<String>,
}

impl RequestSpec {
    fn new(method: Method, path: impl Into<String>) -> Self {
        RequestSpec {
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            authenticated: true,
            raw: None,
            range: None,
        }
    }

    fn get(path: impl Into<String>) -> Self {
        RequestSpec::new(Method::GET, path)
    }

    fn post(path: impl Into<String>) -> Self {
        RequestSpec::new(Method::POST, path)
    }

    fn patch(path: impl Into<String>) -> Self {
        RequestSpec::new(Method::PATCH, path)
    }

    fn delete(path: impl Into<String>) -> Self {
        RequestSpec::new(Method::DELETE, path)
    }

    fn put(path: impl Into<String>) -> Self {
        RequestSpec::new(Method::PUT, path)
    }

    fn query(mut self, key: &str, value: impl Into<String>) -> Self {
        self.query.push((key.to_string(), value.into()));
        self
    }

    fn json(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }

    fn raw(mut self, body: Vec<u8>, content_type: &str) -> Self {
        self.raw = Some((body, content_type.to_string()));
        self
    }

    fn anonymous(mut self) -> Self {
        self.authenticated = false;
        self
    }

    fn range(mut self, range: String) -> Self {
        self.range = Some(range);
        self
    }
}

#[derive(Debug, Clone)]
struct AccessToken {
    value: String,
    expires_at: DateTime<Utc>,
}

struct Inner {
    http: reqwest::Client,
    base: String,
    /// The installation uid, sent with `refresh` (§2). Mutable so it can be set
    /// after the client has been shared.
    device_uid: RwLock<String>,
    retry: RetryPolicy,
    tokens: Arc<dyn TokenStore>,
    access: RwLock<Option<AccessToken>>,
    protocol_version: u32,
}

/// The Ferroma Client Protocol HTTP client.
///
/// Cheap to clone: every clone shares the same token state and connection pool.
#[derive(Clone)]
pub struct FcpClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for FcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FcpClient")
            .field("base_url", &self.inner.base)
            .field("device_uid", &self.inner.device_uid)
            .field("retry", &self.inner.retry)
            .finish_non_exhaustive()
    }
}

impl FcpClient {
    /// Build a client for `base_url` (typically `…/api/v1/client`).
    ///
    /// A trailing `/` is stripped so paths can be appended directly.
    pub fn new(base_url: impl Into<String>, tokens: Arc<dyn TokenStore>) -> ClientResult<Self> {
        Self::with_retry(base_url, tokens, RetryPolicy::default())
    }

    /// Build a client with an explicit retry policy.
    pub fn with_retry(
        base_url: impl Into<String>,
        tokens: Arc<dyn TokenStore>,
        retry: RetryPolicy,
    ) -> ClientResult<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| ClientError::Network(format!("building the http client failed: {e}")))?;
        let base = base_url.into().trim_end_matches('/').to_string();
        Ok(FcpClient {
            inner: Arc::new(Inner {
                http,
                base,
                device_uid: RwLock::new(String::new()),
                retry,
                tokens,
                access: RwLock::new(None),
                protocol_version: CLIENT_PROTOCOL_VERSION,
            }),
        })
    }

    /// The base URL this client talks to.
    pub fn base_url(&self) -> &str {
        &self.inner.base
    }

    /// The retry policy in force.
    pub fn retry_policy(&self) -> RetryPolicy {
        self.inner.retry
    }

    /// Record the installation's device uid, sent with `refresh` (§2).
    pub async fn set_device_uid(&self, device_uid: impl Into<String>) {
        *self.inner.device_uid.write().await = device_uid.into();
    }

    /// The installation uid this client presents.
    pub async fn device_uid(&self) -> String {
        self.inner.device_uid.read().await.clone()
    }

    /// Install an access token obtained elsewhere (a login, or a restore).
    pub async fn set_access_token(&self, token: impl Into<String>, expires_in: Duration) {
        let expires_at = Utc::now() + chrono::Duration::from_std(expires_in).unwrap_or_default();
        *self.inner.access.write().await = Some(AccessToken {
            value: token.into(),
            expires_at,
        });
    }

    /// The access token currently held in memory, if any.
    pub async fn access_token(&self) -> Option<String> {
        self.inner
            .access
            .read()
            .await
            .as_ref()
            .map(|token| token.value.clone())
    }

    /// Forget the in-memory token and the stored refresh token.
    pub async fn clear_tokens(&self) -> ClientResult<()> {
        *self.inner.access.write().await = None;
        self.inner.tokens.clear().await
    }

    /// Whether a refresh token is available, so a `401` can be recovered from.
    pub async fn can_refresh(&self) -> bool {
        self.inner.tokens.load().await.is_some()
    }

    // -- authentication ----------------------------------------------------

    /// `POST /auth/login`.
    ///
    /// The access token stays in memory; the refresh token is handed to the
    /// [`TokenStore`] so it survives a restart.
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        device: &DeviceInfo,
    ) -> ClientResult<LoginResponse> {
        let body = serde_json::json!({
            "email": email,
            "password": password,
            "device": device,
        });
        let spec = RequestSpec::post("/auth/login").json(body).anonymous();
        let response: LoginResponse = self.request_json(&spec).await?;
        self.inner
            .tokens
            .save(response.refresh_token.clone())
            .await?;
        self.set_access_token(
            response.access_token.clone(),
            Duration::from_secs(response.expires_in),
        )
        .await;
        Ok(response)
    }

    /// `POST /auth/refresh`, rotating the refresh token.
    ///
    /// A `401` here means the whole token family was revoked: that is
    /// [`ClientError::SessionExpired`], and the user has to sign in again.
    ///
    /// Boxed because `refresh` and [`FcpClient::send_with_policy`] call each
    /// other: the retry loop refreshes on `401`, and the refresh itself is a
    /// request that must not recurse infinitely.
    pub fn refresh(&self) -> BoxFuture<'_, ClientResult<RefreshResponse>> {
        Box::pin(self.refresh_inner())
    }

    async fn refresh_inner(&self) -> ClientResult<RefreshResponse> {
        let Some(refresh_token) = self.inner.tokens.load().await else {
            return Err(ClientError::SessionExpired);
        };
        let body = serde_json::json!({
            "refresh_token": refresh_token,
            "device_uid": self.device_uid().await,
        });
        let spec = RequestSpec::post("/auth/refresh").json(body).anonymous();
        match self.request_json::<RefreshResponse>(&spec).await {
            Ok(response) => {
                self.inner
                    .tokens
                    .save(response.refresh_token.clone())
                    .await?;
                self.set_access_token(
                    response.access_token.clone(),
                    Duration::from_secs(response.expires_in),
                )
                .await;
                Ok(response)
            }
            Err(ClientError::Api(api)) if api.status == 401 => {
                // "Presenting an already-used refresh token revokes the entire
                // token family and returns 401" (FCP §2).
                let _ = self.inner.tokens.clear().await;
                Err(ClientError::SessionExpired)
            }
            Err(other) => Err(other),
        }
    }

    /// `POST /auth/logout`. Tokens are discarded whether or not the call worked:
    /// the user asked to leave.
    pub async fn logout(&self, refresh_token: Option<String>) -> ClientResult<()> {
        let token = match refresh_token {
            Some(token) => Some(token),
            None => self.inner.tokens.load().await,
        };
        let body = match token {
            Some(token) => serde_json::json!({ "refresh_token": token }),
            None => serde_json::json!({}),
        };
        let spec = RequestSpec::post("/auth/logout").json(body);
        let outcome = self.request_empty(&spec).await;
        self.clear_tokens().await?;
        match outcome {
            Ok(()) => Ok(()),
            // A 401 means the session was already gone — which is the goal.
            Err(ClientError::Api(api)) if api.status == 401 => Ok(()),
            Err(ClientError::SessionExpired) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// Head room kept before an access token is considered stale: refreshing a
    /// second early is cheaper than a round trip that is certain to 401.
    const EXPIRY_SKEW: chrono::Duration = chrono::Duration::seconds(30);

    /// Make sure an access token is available, refreshing if the memory copy is
    /// gone (a fresh process that only has the account's refresh token) or has
    /// run out.
    async fn prepare_auth(&self, spec: &RequestSpec) -> ClientResult<()> {
        if !spec.authenticated {
            return Ok(());
        }
        let usable = {
            let guard = self.inner.access.read().await;
            match guard.as_ref() {
                Some(token) => token.expires_at - Self::EXPIRY_SKEW > Utc::now(),
                None => false,
            }
        };
        if usable {
            return Ok(());
        }
        // The token is absent or stale; drop it and ask for a new one.
        *self.inner.access.write().await = None;
        if self.inner.tokens.load().await.is_some() {
            self.refresh().await?;
            return Ok(());
        }
        Err(ClientError::NotAuthenticated)
    }

    // -- transport ---------------------------------------------------------

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.inner.base, path)
    }

    /// The version headers every FCP request carries (`docs/fcp.md` §1).
    fn version_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(&client_version_string()) {
            headers.insert(HEADER_CLIENT, value);
        }
        headers.insert(
            HEADER_PROTOCOL,
            HeaderValue::from_str(&self.inner.protocol_version.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("1")),
        );
        if let Ok(value) = HeaderValue::from_str(platform_name()) {
            headers.insert(HEADER_PLATFORM, value);
        }
        if let Ok(value) = HeaderValue::from_str(&format!(
            "{CLIENT_NAME}/{} ({}; {})",
            ferroma_core::VERSION,
            platform_name(),
            std::env::consts::ARCH
        )) {
            headers.insert(USER_AGENT, value);
        }
        headers
    }

    fn build_request(
        &self,
        spec: &RequestSpec,
        token: Option<&str>,
        headers: &HeaderMap,
    ) -> ClientResult<reqwest::Request> {
        let mut builder = self
            .inner
            .http
            .request(spec.method.clone(), self.url(&spec.path))
            .headers(headers.clone());
        if !spec.query.is_empty() {
            builder = builder.query(&spec.query);
        }
        if let Some(range) = &spec.range {
            if let Ok(value) = HeaderValue::from_str(range) {
                builder = builder.header(RANGE, value);
            }
        }
        match (&spec.raw, &spec.body) {
            (Some((bytes, content_type)), _) => {
                builder = builder
                    .header(CONTENT_TYPE, content_type.clone())
                    .body(bytes.clone());
            }
            (None, Some(body)) => {
                builder = builder.json(body);
            }
            (None, None) => {}
        }
        if let Some(token) = token {
            builder = builder.bearer_auth(token);
        }
        builder
            .build()
            .map_err(|e| ClientError::Invalid(format!("could not build the request: {e}")))
    }

    /// Send one request, applying the `401`-refresh-retry rule and the retry
    /// policy for `429`/`5xx`/transport failures.
    ///
    /// The returned response may still be an error status (a `4xx` that is not
    /// retryable is handed back so the caller can read the envelope).
    async fn send_with_policy(&self, spec: &RequestSpec) -> ClientResult<Response> {
        let headers = self.version_headers();
        let mut attempt: u32 = 0;
        let mut refreshed = false;
        loop {
            attempt += 1;
            self.prepare_auth(spec).await?;
            let token = if spec.authenticated {
                self.access_token().await
            } else {
                None
            };
            let request = self.build_request(spec, token.as_deref(), &headers)?;

            let response = match self.inner.http.execute(request).await {
                Ok(response) => response,
                Err(err) => {
                    let mapped = map_reqwest_error(&err);
                    if mapped.is_retryable() && self.inner.retry.allows(attempt) {
                        let delay = self.inner.retry.delay_for(attempt);
                        tracing::debug!(
                            attempt,
                            delay_ms = delay.as_millis() as u64,
                            path = %spec.path,
                            "transport failure; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(mapped);
                }
            };

            let status = response.status().as_u16();

            if status == 401 && spec.authenticated {
                if refreshed {
                    // A second 401 after a successful refresh: stop, keep the cache.
                    return Err(ClientError::SessionExpired);
                }
                refreshed = true;
                tracing::debug!(path = %spec.path, "401 from the server; refreshing once");
                match self.refresh().await {
                    Ok(_) => continue,
                    Err(err) => return Err(err),
                }
            }

            if status == 429 {
                let wait = retry_after_of(response.headers());
                if self.inner.retry.allows(attempt) {
                    let delay = self.inner.retry.retry_after_or(wait, attempt);
                    tracing::debug!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        path = %spec.path,
                        "rate limited; honouring Retry-After"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(ClientError::RateLimited {
                    retry_after: wait,
                });
            }

            if (500..600).contains(&status) && self.inner.retry.allows(attempt) {
                let delay = self.inner.retry.delay_for(attempt);
                tracing::debug!(
                    attempt,
                    status,
                    delay_ms = delay.as_millis() as u64,
                    path = %spec.path,
                    "server error; backing off"
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            return Ok(response);
        }
    }

    /// Turn a response into either a parsed body or a typed [`ApiError`].
    async fn read_json<T: DeserializeOwned>(&self, response: Response) -> ClientResult<T> {
        let status = response.status().as_u16();
        let retry_after = retry_after_of(response.headers());
        let bytes = response
            .bytes()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        if !(200..300).contains(&status) {
            return Err(ClientError::Api(ApiError::parse(
                status,
                &bytes,
                retry_after,
            )));
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            ClientError::Parse(format!("HTTP {status} body was not the expected JSON: {e}"))
        })
    }

    async fn request_json<T: DeserializeOwned>(&self, spec: &RequestSpec) -> ClientResult<T> {
        let response = self.send_with_policy(spec).await?;
        self.read_json(response).await
    }

    async fn request_empty(&self, spec: &RequestSpec) -> ClientResult<()> {
        let response = self.send_with_policy(spec).await?;
        let status = response.status().as_u16();
        let retry_after = retry_after_of(response.headers());
        let bytes = response
            .bytes()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(ClientError::Api(ApiError::parse(
            status,
            &bytes,
            retry_after,
        )))
    }

    // -- account and mailboxes --------------------------------------------

    /// `GET /account` (`docs/fcp.md` §1).
    pub async fn account(&self) -> ClientResult<AccountInfo> {
        self.request_json(&RequestSpec::get("/account")).await
    }

    /// `GET /mailboxes` (`docs/fcp.md` §4).
    pub async fn mailboxes(&self) -> ClientResult<Vec<MailboxInfo>> {
        #[derive(Deserialize)]
        struct Envelope {
            #[serde(default)]
            mailboxes: Vec<MailboxInfo>,
        }
        let envelope: Envelope = self.request_json(&RequestSpec::get("/mailboxes")).await?;
        Ok(envelope.mailboxes)
    }

    // -- sync --------------------------------------------------------------

    /// `GET /sync` (`docs/fcp.md` §3).
    ///
    /// `cursor` is opaque: it is only ever echoed back.
    pub async fn sync(
        &self,
        mailbox_id: i64,
        folder_id: Option<i64>,
        cursor: &str,
        limit: Option<usize>,
    ) -> ClientResult<SyncPage> {
        let mut spec = RequestSpec::get("/sync")
            .query("mailbox_id", mailbox_id.to_string())
            .query("cursor", cursor.to_string());
        if let Some(folder_id) = folder_id {
            spec = spec.query("folder_id", folder_id.to_string());
        }
        if let Some(limit) = limit {
            spec = spec.query("limit", limit.to_string());
        }
        self.request_json(&spec).await
    }

    // -- messages ----------------------------------------------------------

    /// `GET /messages`.
    pub async fn messages(
        &self,
        mailbox_id: i64,
        folder_id: Option<i64>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> ClientResult<MessageList> {
        let mut spec = RequestSpec::get("/messages").query("mailbox_id", mailbox_id.to_string());
        if let Some(folder_id) = folder_id {
            spec = spec.query("folder_id", folder_id.to_string());
        }
        if let Some(limit) = limit {
            spec = spec.query("limit", limit.to_string());
        }
        if let Some(offset) = offset {
            spec = spec.query("offset", offset.to_string());
        }
        self.request_json(&spec).await
    }

    /// `GET /messages/:id`.
    pub async fn message(&self, message_id: i64) -> ClientResult<MessageDetail> {
        self.request_json(&RequestSpec::get(format!("/messages/{message_id}")))
            .await
    }

    /// `GET /messages/:id/raw` — the RFC 5322 bytes.
    pub async fn raw_message(&self, message_id: i64) -> ClientResult<Vec<u8>> {
        let response = self
            .send_with_policy(&RequestSpec::get(format!("/messages/{message_id}/raw")))
            .await?;
        let status = response.status().as_u16();
        let retry_after = retry_after_of(response.headers());
        let bytes = response
            .bytes()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        if (200..300).contains(&status) {
            Ok(bytes.to_vec())
        } else {
            Err(ClientError::Api(ApiError::parse(
                status,
                &bytes,
                retry_after,
            )))
        }
    }

    /// `POST /messages` — send. The `operation_id` inside the body is the
    /// idempotency key, so a retry never double-sends.
    pub async fn send(&self, request: &SendRequest) -> ClientResult<SendResponse> {
        let body = serde_json::to_value(request)?;
        self.request_json(&RequestSpec::post("/messages").json(body))
            .await
    }

    /// `PATCH /messages/:id`.
    pub async fn patch_message(&self, message_id: i64, patch: &MessagePatch) -> ClientResult<()> {
        let body = serde_json::to_value(patch)?;
        self.request_empty(&RequestSpec::patch(format!("/messages/{message_id}")).json(body))
            .await
    }

    /// `DELETE /messages/:id`.
    pub async fn delete_message(&self, message_id: i64, permanent: bool) -> ClientResult<()> {
        let mut spec = RequestSpec::delete(format!("/messages/{message_id}"));
        if permanent {
            spec = spec.query("permanent", "true");
        }
        self.request_empty(&spec).await
    }

    /// `POST /messages/:id/{read,unread,star,archive,trash}`.
    pub async fn message_action(
        &self,
        message_id: i64,
        action: MessageAction,
        operation_id: &OperationId,
    ) -> ClientResult<()> {
        let body = serde_json::json!({ "operation_id": operation_id.as_str() });
        self.request_empty(
            &RequestSpec::post(format!("/messages/{message_id}/{}", action.path_segment()))
                .json(body),
        )
        .await
    }

    /// `POST /messages/:id/move`.
    pub async fn move_message(
        &self,
        message_id: i64,
        folder_id: i64,
        operation_id: &OperationId,
    ) -> ClientResult<()> {
        let body = serde_json::json!({
            "folder_id": folder_id,
            "operation_id": operation_id.as_str(),
        });
        self.request_empty(&RequestSpec::post(format!("/messages/{message_id}/move")).json(body))
            .await
    }

    // -- drafts ------------------------------------------------------------

    /// `POST /drafts`.
    pub async fn create_draft(&self, draft: &DraftPayload) -> ClientResult<DraftRecord> {
        let body = serde_json::to_value(draft)?;
        self.request_json(&RequestSpec::post("/drafts").json(body))
            .await
    }

    /// `GET /drafts`.
    pub async fn list_drafts(&self) -> ClientResult<Vec<DraftRecord>> {
        let value: serde_json::Value = self.request_json(&RequestSpec::get("/drafts")).await?;
        match value {
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(|item| {
                    serde_json::from_value(item)
                        .map_err(|e| ClientError::Parse(format!("draft entry: {e}")))
                })
                .collect(),
            serde_json::Value::Object(mut map) => {
                let drafts = map.remove("drafts").unwrap_or(serde_json::Value::Array(vec![]));
                serde_json::from_value(drafts)
                    .map_err(|e| ClientError::Parse(format!("draft list: {e}")))
            }
            other => Err(ClientError::Parse(format!(
                "draft list had an unexpected shape: {other}"
            ))),
        }
    }

    /// `PATCH /drafts/:id`. The response says what the write overwrote (§7).
    pub async fn update_draft(
        &self,
        draft_id: i64,
        draft: &DraftPayload,
    ) -> ClientResult<DraftRecord> {
        let body = serde_json::to_value(draft)?;
        self.request_json(&RequestSpec::patch(format!("/drafts/{draft_id}")).json(body))
            .await
    }

    /// `DELETE /drafts/:id`.
    pub async fn delete_draft(&self, draft_id: i64) -> ClientResult<()> {
        self.request_empty(&RequestSpec::delete(format!("/drafts/{draft_id}")))
            .await
    }

    // -- devices -----------------------------------------------------------

    /// `GET /devices` (`docs/fcp.md` §9).
    pub async fn devices(&self) -> ClientResult<Vec<DeviceRecord>> {
        let list: DeviceList = self.request_json(&RequestSpec::get("/devices")).await?;
        Ok(list.devices)
    }

    /// `POST /devices/:id/revoke`.
    pub async fn revoke_device(&self, device_id: i64) -> ClientResult<()> {
        self.request_empty(&RequestSpec::post(format!("/devices/{device_id}/revoke")))
            .await
    }

    /// `DELETE /devices/:id`.
    pub async fn delete_device(&self, device_id: i64) -> ClientResult<()> {
        self.request_empty(&RequestSpec::delete(format!("/devices/{device_id}")))
            .await
    }

    // -- search ------------------------------------------------------------

    /// `GET /search` — the server-side fallback of §10.
    pub async fn search(
        &self,
        query: &str,
        mailbox_id: Option<i64>,
        folder_id: Option<i64>,
        limit: Option<usize>,
    ) -> ClientResult<MessageList> {
        let mut spec = RequestSpec::get("/search").query("q", query.to_string());
        if let Some(mailbox_id) = mailbox_id {
            spec = spec.query("mailbox_id", mailbox_id.to_string());
        }
        if let Some(folder_id) = folder_id {
            spec = spec.query("folder_id", folder_id.to_string());
        }
        if let Some(limit) = limit {
            spec = spec.query("limit", limit.to_string());
        }
        self.request_json(&spec).await
    }

    // -- attachments -------------------------------------------------------

    /// `POST /attachments` — one-shot multipart upload for small files.
    pub async fn upload_attachment(
        &self,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> ClientResult<AttachmentDto> {
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(bytes)
                .file_name(filename.to_string())
                .mime_str(content_type)
                .map_err(|e| ClientError::Invalid(format!("bad content type: {e}")))?,
        );
        let headers = self.version_headers();
        self.prepare_auth(&RequestSpec::post("/attachments")).await?;
        let token = self.access_token().await;
        let mut request = self
            .inner
            .http
            .post(self.url("/attachments"))
            .headers(headers)
            .multipart(form);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        self.read_json(response).await
    }

    /// `POST /attachments/init` — start a resumable chunked upload (§6).
    pub async fn init_upload(
        &self,
        filename: &str,
        content_type: &str,
        size_bytes: u64,
    ) -> ClientResult<UploadInit> {
        let body = serde_json::json!({
            "filename": filename,
            "content_type": content_type,
            "size_bytes": size_bytes,
        });
        self.request_json(&RequestSpec::post("/attachments/init").json(body))
            .await
    }

    /// `GET /attachments/:id/status` — which chunks the server already holds.
    pub async fn upload_status(&self, attachment_id: i64) -> ClientResult<UploadStatus> {
        self.request_json(&RequestSpec::get(format!(
            "/attachments/{attachment_id}/status"
        )))
        .await
    }

    /// `PUT /attachments/:id/chunk?index=N`.
    pub async fn upload_chunk(
        &self,
        attachment_id: i64,
        index: u64,
        bytes: Vec<u8>,
    ) -> ClientResult<()> {
        let spec = RequestSpec::put(format!("/attachments/{attachment_id}/chunk"))
            .query("index", index.to_string())
            .raw(bytes, "application/octet-stream");
        self.request_empty(&spec).await
    }

    /// `POST /attachments/:id/complete` — the server verifies the digest.
    pub async fn complete_upload(
        &self,
        attachment_id: i64,
        sha256: &str,
    ) -> ClientResult<UploadComplete> {
        let body = serde_json::json!({ "sha256": sha256 });
        self.request_json(
            &RequestSpec::post(format!("/attachments/{attachment_id}/complete")).json(body),
        )
        .await
    }

    /// `GET /attachments/:id` — a streaming download.
    ///
    /// Pass `range` (an inclusive byte range, with an optional end) to resume a
    /// partial file: `Some((n, None))` asks for `bytes=n-`, which is what a
    /// resume wants. The server answers `206 Partial Content`.
    pub async fn download_attachment(
        &self,
        attachment_id: i64,
        range: Option<(u64, Option<u64>)>,
    ) -> ClientResult<Response> {
        let mut spec = RequestSpec::get(format!("/attachments/{attachment_id}"));
        if let Some((start, end)) = range {
            spec = match end {
                Some(end) => spec.range(format!("bytes={start}-{end}")),
                None => spec.range(format!("bytes={start}-")),
            };
        }
        let response = self.send_with_policy(&spec).await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = retry_after_of(response.headers());
        let bytes = response
            .bytes()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        Err(ClientError::Api(ApiError::parse(
            status.as_u16(),
            &bytes,
            retry_after,
        )))
    }

    /// `GET /attachments/:id` into memory, with `Range` resume support.
    pub async fn download_attachment_bytes(
        &self,
        attachment_id: i64,
        range: Option<(u64, Option<u64>)>,
    ) -> ClientResult<Vec<u8>> {
        let response = self.download_attachment(attachment_id, range).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        Ok(bytes.to_vec())
    }

    /// The `ETag` of an attachment, which is the blob's SHA-256 (`docs/fcp.md` §6).
    pub async fn attachment_etag(&self, attachment_id: i64) -> ClientResult<Option<String>> {
        let response = self
            .download_attachment(attachment_id, Some((0, Some(0))))
            .await?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim_matches('"').to_string());
        Ok(etag)
    }

    /// A base64 helper used by the attachment tests and by the CLI's `--base64`.
    pub fn encode_base64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

/// Parse a `Retry-After` header: the spec allows seconds or an HTTP date; only
/// the seconds form is honoured, and a bogus value is ignored rather than fatal.
pub fn retry_after_of(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Map a `reqwest` failure onto the client's vocabulary, keeping timeouts
/// distinguishable from other transport failures.
fn map_reqwest_error(err: &reqwest::Error) -> ClientError {
    if err.is_timeout() {
        ClientError::Timeout(err.to_string())
    } else if err.is_builder() {
        ClientError::Invalid(err.to_string())
    } else {
        ClientError::Network(err.to_string())
    }
}

/// Whether a status is one the retry policy may repeat.
#[allow(dead_code)]
fn status_is_retryable(status: StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockServer, MockResponse};

    async fn make_client(server: &MockServer) -> (FcpClient, Arc<MemoryTokenStore>) {
        let store = Arc::new(MemoryTokenStore::new());
        let c = FcpClient::with_retry(
            server.fcp_base_url(),
            store.clone(),
            RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
                jitter: 0.0,
                max_retry_after: Duration::from_secs(5),
            },
        )
        .expect("client");
        c.set_device_uid("dev-1").await;
        (c, store)
    }

    // -- retry policy ------------------------------------------------------

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = RetryPolicy {
            max_attempts: 6,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            jitter: 0.0,
            max_retry_after: Duration::from_secs(1),
        };
        assert_eq!(policy.delay_for(1), Duration::from_millis(10));
        assert_eq!(policy.delay_for(2), Duration::from_millis(20));
        assert_eq!(policy.delay_for(3), Duration::from_millis(40));
        assert_eq!(policy.delay_for(4), Duration::from_millis(50));
        assert_eq!(policy.delay_for(9), Duration::from_millis(50));
    }

    #[test]
    fn jitter_stays_within_its_band() {
        let policy = RetryPolicy {
            jitter: 0.5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(1000),
            ..RetryPolicy::default()
        };
        for _ in 0..50 {
            let delay = policy.delay_for(1).as_millis() as u64;
            assert!((50..=150).contains(&delay), "delay out of band: {delay}");
        }
    }

    #[test]
    fn retry_after_clamps_a_hostile_server() {
        let policy = RetryPolicy {
            max_retry_after: Duration::from_secs(30),
            ..RetryPolicy::default()
        };
        assert_eq!(
            policy.retry_after_or(Some(Duration::from_secs(600)), 1),
            Duration::from_secs(30)
        );
        assert_eq!(
            policy.retry_after_or(Some(Duration::from_secs(2)), 1),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn retry_after_header_parses_or_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(retry_after_of(&headers), Some(Duration::from_secs(7)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after_of(&headers), None);
        assert_eq!(retry_after_of(&HeaderMap::new()), None);
    }

    #[test]
    fn status_classification_matches_fcp_11() {
        assert!(status_is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(status_is_retryable(StatusCode::BAD_GATEWAY));
        assert!(!status_is_retryable(StatusCode::FORBIDDEN));
        assert!(!status_is_retryable(StatusCode::NOT_FOUND));
    }

    // -- error envelope ----------------------------------------------------

    #[test]
    fn the_error_envelope_is_parsed_with_details() {
        let body = br#"{"error":{"code":"invalid_input","message":"no domain: bob","details":{"field":"to"}}}"#;
        let err = ApiError::parse(400, body, None);
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "invalid_input");
        assert_eq!(err.message, "no domain: bob");
        assert_eq!(
            err.details.as_ref().and_then(|d| d.get("field")).and_then(|v| v.as_str()),
            Some("to")
        );
        assert!(!err.is_retryable());
    }

    #[test]
    fn a_malformed_error_body_still_produces_a_typed_error() {
        let err = ApiError::parse(502, b"<html>bad gateway</html>", None);
        assert_eq!(err.status, 502);
        assert_eq!(err.code, "http_502");
        assert!(err.message.contains("bad gateway"));
        assert!(err.is_retryable());

        let empty = ApiError::parse(500, b"", None);
        assert_eq!(empty.code, "http_500");
        assert!(empty.message.contains("empty body"));
    }

    #[test]
    fn a_very_long_error_body_is_truncated() {
        let body = vec![b'x'; 10_000];
        let err = ApiError::parse(500, &body, None);
        assert!(err.message.chars().count() < 260);
    }

    #[test]
    fn upgrade_required_is_recognised() {
        let err = ApiError::parse(
            426,
            br#"{"error":{"code":"unsupported","message":"upgrade to FCP/1"}}"#,
            None,
        );
        assert!(err.is_upgrade_required());
    }

    // -- happy path --------------------------------------------------------

    #[tokio::test]
    async fn happy_path_mailboxes() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/mailboxes",
            200,
            r#"{"mailboxes":[{"id":3,"address":"alice@example.com","display_name":"Alice","is_primary":true,
                 "folders":[{"id":5,"name":"INBOX","message_count":412,"unseen_count":3,"uid_validity":1,"uid_next":118}]}]}"#,
        );
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("access-1", Duration::from_secs(600))
            .await;

        let mailboxes = client.mailboxes().await.expect("mailboxes");
        assert_eq!(mailboxes.len(), 1);
        assert_eq!(mailboxes[0].address, "alice@example.com");
        assert_eq!(mailboxes[0].folders[0].name, "INBOX");
        assert_eq!(mailboxes[0].folders[0].message_count, 412);
    }

    #[tokio::test]
    async fn every_request_carries_the_version_headers() {
        let server = MockServer::start().await;
        server.json_route("GET", "/api/v1/client/account", 200, r#"{"user":{"id":7,"email":"a@b.c"}}"#);
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("access-1", Duration::from_secs(600))
            .await;
        let info = client.account().await.expect("account");
        assert_eq!(info.user.id, 7);

        let request = server.requests_for("/api/v1/client/account").remove(0);
        assert_eq!(
            request.header(HEADER_CLIENT.to_ascii_lowercase().as_str()),
            Some(client_version_string().as_str())
        );
        assert_eq!(
            request.header(HEADER_PROTOCOL.to_ascii_lowercase().as_str()),
            Some("1")
        );
        assert_eq!(
            request.header(HEADER_PLATFORM.to_ascii_lowercase().as_str()),
            Some(platform_name())
        );
        assert_eq!(request.header("authorization"), Some("Bearer access-1"));
    }

    #[tokio::test]
    async fn sync_parameters_are_sent_and_the_page_is_parsed() {
        let server = MockServer::start().await;
        server.route("GET", "/api/v1/client/sync", |_req, _n| {
            MockResponse::json(
                r#"{"next_cursor":"1841","has_more":false,"changes":[
                     {"type":"message_created","seq":1836,"message_id":4821,"uid":117},
                     {"type":"folder_created","seq":1840,"folder_id":6,"name":"Archive/2026"},
                     {"type":"something_new","seq":1841}]}"#,
            )
        });
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let page = client.sync(3, Some(5), "0", Some(500)).await.expect("sync");
        assert_eq!(page.next_cursor, "1841");
        assert!(!page.has_more);
        assert_eq!(page.changes.len(), 3);
        assert_eq!(page.changes[0].kind(), "message_created");
        assert_eq!(page.changes[1].seq(), Some(1840));
        assert_eq!(page.changes[2], Change::Unknown);

        let request = server.requests_for("/api/v1/client/sync").remove(0);
        let params = request.query_params();
        assert_eq!(params.get("mailbox_id").map(String::as_str), Some("3"));
        assert_eq!(params.get("folder_id").map(String::as_str), Some("5"));
        assert_eq!(params.get("cursor").map(String::as_str), Some("0"));
        assert_eq!(params.get("limit").map(String::as_str), Some("500"));
    }

    // -- authentication ----------------------------------------------------

    #[tokio::test]
    async fn login_stores_the_refresh_token_and_keeps_the_access_token_in_memory() {
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/auth/login",
            200,
            r#"{"access_token":"access-1","refresh_token":"rt_1","token_type":"Bearer",
                "expires_in":3600,"device_id":12,"user":{"id":7,"email":"alice@example.com"}}"#,
        );
        let (client, store) = make_client(&server).await;
        let device = DeviceInfo::local("dev-1", "Alice's laptop");
        let response = client
            .login("alice@example.com", "hunter2", &device)
            .await
            .expect("login");
        assert_eq!(response.device_id, Some(12));
        assert_eq!(client.access_token().await.as_deref(), Some("access-1"));
        assert_eq!(store.load().await.as_deref(), Some("rt_1"));

        let request = server.requests_for("/api/v1/client/auth/login").remove(0);
        let body = request.json();
        assert_eq!(body["email"], "alice@example.com");
        assert_eq!(body["device"]["device_uid"], "dev-1");
        assert_eq!(body["device"]["platform"], platform_name());
    }

    #[tokio::test]
    async fn a_401_triggers_one_refresh_and_then_succeeds() {
        let server = MockServer::start().await;
        server.script(
            "GET",
            "/api/v1/client/account",
            vec![
                MockResponse::error(401, "unauthorized", "expired"),
                MockResponse::json(r#"{"user":{"id":7,"email":"a@b.c"}}"#),
            ],
        );
        server.json_route(
            "POST",
            "/api/v1/client/auth/refresh",
            200,
            r#"{"access_token":"access-2","refresh_token":"rt_2","expires_in":3600}"#,
        );

        let (client, store) = make_client(&server).await;
        store.save("rt_1".to_string()).await.expect("seed");
        client
            .set_access_token("stale", Duration::from_secs(600))
            .await;

        let info = client.account().await.expect("account after refresh");
        assert_eq!(info.user.id, 7);
        assert_eq!(client.access_token().await.as_deref(), Some("access-2"));
        assert_eq!(store.load().await.as_deref(), Some("rt_2"));
        assert_eq!(server.count_for("/api/v1/client/auth/refresh"), 1);
        assert_eq!(server.count_for("/api/v1/client/account"), 2);
    }

    #[tokio::test]
    async fn a_second_401_gives_up_with_session_expired() {
        let server = MockServer::start().await;
        server.script(
            "GET",
            "/api/v1/client/account",
            vec![
                MockResponse::error(401, "unauthorized", "expired"),
                MockResponse::error(401, "unauthorized", "still expired"),
            ],
        );
        server.json_route(
            "POST",
            "/api/v1/client/auth/refresh",
            200,
            r#"{"access_token":"access-2","refresh_token":"rt_2","expires_in":3600}"#,
        );
        let (client, store) = make_client(&server).await;
        store.save("rt_1".to_string()).await.expect("seed");
        client
            .set_access_token("stale", Duration::from_secs(600))
            .await;

        let err = client.account().await.expect_err("must give up");
        assert!(matches!(err, ClientError::SessionExpired), "got {err:?}");
        assert_eq!(server.count_for("/api/v1/client/auth/refresh"), 1);
        assert_eq!(server.count_for("/api/v1/client/account"), 2);
    }

    #[tokio::test]
    async fn a_refused_refresh_means_the_token_family_is_revoked() {
        let server = MockServer::start().await;
        server.error_route("GET", "/api/v1/client/account", 401, "unauthorized", "nope");
        server.error_route("POST", "/api/v1/client/auth/refresh", 401, "unauthorized", "reused");
        let (client, store) = make_client(&server).await;
        store.save("rt_used".to_string()).await.expect("seed");
        client
            .set_access_token("stale", Duration::from_secs(600))
            .await;

        let err = client.account().await.expect_err("must give up");
        assert!(matches!(err, ClientError::SessionExpired));
        assert_eq!(store.load().await, None, "a revoked family clears the token");
    }

    #[tokio::test]
    async fn a_client_without_tokens_reports_not_signed_in() {
        let server = MockServer::start().await;
        server.json_route("GET", "/api/v1/client/account", 200, "{}");
        let (client, _store) = make_client(&server).await;
        let err = client.account().await.expect_err("must refuse");
        assert!(matches!(err, ClientError::NotAuthenticated));
        assert_eq!(server.request_count(), 0, "nothing should have been sent");
    }

    #[tokio::test]
    async fn a_restart_with_only_a_refresh_token_refreshes_before_calling() {
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/auth/refresh",
            200,
            r#"{"access_token":"access-new","refresh_token":"rt_2","expires_in":3600}"#,
        );
        server.json_route(
            "GET",
            "/api/v1/client/mailboxes",
            200,
            r#"{"mailboxes":[{"id":3,"address":"a@b.c"}]}"#,
        );
        let (client, store) = make_client(&server).await;
        store.save("rt_1".to_string()).await.expect("seed");
        let mailboxes = client.mailboxes().await.expect("mailboxes");
        assert_eq!(mailboxes.len(), 1);
        assert_eq!(server.count_for("/api/v1/client/auth/refresh"), 1);
    }

    #[tokio::test]
    async fn logout_clears_the_tokens_even_when_the_call_fails() {
        let server = MockServer::start().await;
        server.error_route("POST", "/api/v1/client/auth/logout", 500, "internal_error", "boom");
        let (client, store) = make_client(&server).await;
        // A 500 is retried then surfaced, but the tokens must still be gone.
        store.save("rt_1".to_string()).await.expect("seed");
        client
            .set_access_token("access", Duration::from_secs(60))
            .await;
        let _ = client.logout(None).await;
        assert_eq!(store.load().await, None);
        assert!(client.access_token().await.is_none());
    }

    // -- retry behaviour ---------------------------------------------------

    #[tokio::test]
    async fn a_429_is_retried_honouring_retry_after() {
        let server = MockServer::start().await;
        server.script(
            "GET",
            "/api/v1/client/mailboxes",
            vec![
                MockResponse::json_with_headers(
                    429,
                    r#"{"error":{"code":"rate_limited","message":"slow down"}}"#,
                    vec![("Retry-After", "0")],
                ),
                MockResponse::json(r#"{"mailboxes":[]}"#),
            ],
        );
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let start = std::time::Instant::now();
        let mailboxes = client.mailboxes().await.expect("retried");
        assert!(mailboxes.is_empty());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(server.count_for("/api/v1/client/mailboxes"), 2);
    }

    #[tokio::test]
    async fn a_429_that_never_clears_is_surfaced_with_its_retry_after() {
        let server = MockServer::start().await;
        server.route("GET", "/api/v1/client/mailboxes", |_req, _n| {
            MockResponse::json_with_headers(
                429,
                r#"{"error":{"code":"rate_limited","message":"slow down"}}"#,
                vec![("Retry-After", "3")],
            )
        });
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.mailboxes().await.expect_err("rate limited");
        assert!(matches!(err, ClientError::RateLimited { .. }), "got {err:?}");
        assert_eq!(server.count_for("/api/v1/client/mailboxes"), 3, "three attempts");
    }

    #[tokio::test]
    async fn a_500_is_retried_and_then_surfaced() {
        let server = MockServer::start().await;
        server.error_route("GET", "/api/v1/client/account", 500, "storage_error", "db down");
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.account().await.expect_err("must fail");
        match err {
            ClientError::Api(api) => {
                assert_eq!(api.status, 500);
                assert_eq!(api.code, "storage_error");
                assert!(api.is_retryable());
            }
            other => panic!("expected an api error, got {other:?}"),
        }
        assert_eq!(server.count_for("/api/v1/client/account"), 3);
    }

    #[tokio::test]
    async fn a_404_is_never_retried() {
        let server = MockServer::start().await;
        server.error_route("GET", "/api/v1/client/messages/9", 404, "not_found", "gone");
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.message(9).await.expect_err("must fail");
        assert_eq!(err.api_status(), Some(404));
        assert_eq!(server.count_for("/api/v1/client/messages/9"), 1);
    }

    #[tokio::test]
    async fn a_403_is_never_retried() {
        let server = MockServer::start().await;
        server.error_route("DELETE", "/api/v1/client/messages/9", 403, "forbidden", "no");
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.delete_message(9, false).await.expect_err("must fail");
        assert_eq!(err.api_status(), Some(403));
        assert_eq!(server.count_for("/api/v1/client/messages/9"), 1);
    }

    #[tokio::test]
    async fn an_upgrade_required_error_is_surfaced_verbatim() {
        let server = MockServer::start().await;
        server.error_route(
            "GET",
            "/api/v1/client/account",
            426,
            "unsupported",
            "client protocol 0 is no longer supported",
        );
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.account().await.expect_err("must fail");
        match err {
            ClientError::Api(api) => {
                assert!(api.is_upgrade_required());
                assert!(api.message.contains("no longer supported"));
            }
            other => panic!("expected an api error, got {other:?}"),
        }
    }

    // -- hostile servers ---------------------------------------------------

    #[tokio::test]
    async fn malformed_json_produces_a_typed_error_not_a_panic() {
        let server = MockServer::start().await;
        server.route("GET", "/api/v1/client/mailboxes", |_req, _n| {
            MockResponse::malformed(200, "{ this is not json ")
        });
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.mailboxes().await.expect_err("must fail");
        assert!(matches!(err, ClientError::Parse(_)), "got {err:?}");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn a_wrong_shaped_body_produces_a_typed_error() {
        let server = MockServer::start().await;
        server.json_route("GET", "/api/v1/client/mailboxes", 200, r#"{"mailboxes":"nope"}"#);
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.mailboxes().await.expect_err("must fail");
        assert!(matches!(err, ClientError::Parse(_)));
    }

    #[tokio::test]
    async fn a_connection_reset_mid_body_produces_a_retryable_network_error() {
        let server = MockServer::start().await;
        server.route("GET", "/api/v1/client/mailboxes", |_req, _n| {
            // Announce far more bytes than are written, then reset.
            MockResponse::Truncate {
                head: b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{\"mail"
                    .to_vec(),
            }
        });
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.mailboxes().await.expect_err("must fail cleanly");
        assert!(
            matches!(err, ClientError::Network(_) | ClientError::Timeout(_)),
            "got {err:?}"
        );
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn a_connection_reset_before_any_response_is_a_network_error() {
        let server = MockServer::start().await;
        server.route("GET", "/api/v1/client/account", |_req, _n| MockResponse::Reset);
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let err = client.account().await.expect_err("must fail cleanly");
        assert!(matches!(err, ClientError::Network(_)), "got {err:?}");
        assert_eq!(server.count_for("/api/v1/client/account"), 3, "retried");
    }

    // -- messages and operations ------------------------------------------

    #[tokio::test]
    async fn sending_carries_the_operation_id_generated_before_the_call() {
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/messages",
            200,
            r#"{"message_id":4822,"queued":1,"recipients":["bob@example.net"]}"#,
        );
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let operation = OperationId::generate();
        let response = client
            .send(&SendRequest {
                operation_id: operation.as_str().to_string(),
                to: vec!["bob@example.net".into()],
                subject: "Hi".into(),
                text: Some("hello".into()),
                ..SendRequest::default()
            })
            .await
            .expect("sent");
        assert_eq!(response.queued, 1);

        let request = server.requests_for("/api/v1/client/messages").remove(0);
        assert_eq!(request.json()["operation_id"], operation.as_str());
    }

    #[tokio::test]
    async fn message_actions_and_moves_hit_the_documented_paths() {
        let server = MockServer::start().await;
        for path in [
            "/api/v1/client/messages/4821/read",
            "/api/v1/client/messages/4821/unread",
            "/api/v1/client/messages/4821/star",
            "/api/v1/client/messages/4821/archive",
            "/api/v1/client/messages/4821/trash",
            "/api/v1/client/messages/4821/move",
        ] {
            server.json_route("POST", path, 200, "{}");
        }
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        for action in [
            MessageAction::Read,
            MessageAction::Unread,
            MessageAction::Star,
            MessageAction::Archive,
            MessageAction::Trash,
        ] {
            client
                .message_action(4821, action, &OperationId::generate())
                .await
                .expect("action");
        }
        client
            .move_message(4821, 6, &OperationId::generate())
            .await
            .expect("move");
        let move_request = server
            .requests_for("/api/v1/client/messages/4821/move")
            .remove(0);
        assert_eq!(move_request.json()["folder_id"], 6);
    }

    #[tokio::test]
    async fn patch_and_delete_shape_the_request_correctly() {
        let server = MockServer::start().await;
        server.json_route("PATCH", "/api/v1/client/messages/4821", 200, "{}");
        server.json_route("DELETE", "/api/v1/client/messages/4821", 204, "");
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        client
            .patch_message(
                4821,
                &MessagePatch {
                    seen: Some(true),
                    ..MessagePatch::default()
                },
            )
            .await
            .expect("patch");
        let request = server
            .calls("PATCH", "/api/v1/client/messages/4821")
            .remove(0);
        assert_eq!(request.json()["seen"], true);
        assert!(request.json().get("flagged").is_none(), "unset flags are omitted");

        client.delete_message(4821, true).await.expect("delete");
        let request = server
            .calls("DELETE", "/api/v1/client/messages/4821")
            .remove(0);
        assert_eq!(request.query_params().get("permanent").map(String::as_str), Some("true"));
    }

    // -- drafts, devices, search ------------------------------------------

    #[tokio::test]
    async fn draft_conflict_is_reported_to_the_caller() {
        let server = MockServer::start().await;
        server.json_route(
            "PATCH",
            "/api/v1/client/drafts/44",
            200,
            r#"{"id":44,"updated_at":"2026-09-16T12:00:01Z",
                "conflict":{"detected":true,"server_updated_at":"2026-09-16T11:59:58Z"}}"#,
        );
        let (client, _store) = make_client(&server).await;
        client
            .set_access_token("t", Duration::from_secs(60))
            .await;
        let record = client
            .update_draft(44, &DraftPayload { subject: "later".into(), ..DraftPayload::default() })
            .await
            .expect("updated");
        let conflict = record.conflict.expect("conflict reported");
        assert!(conflict.detected);
        assert_eq!(conflict.server_updated_at.as_deref(), Some("2026-09-16T11:59:58Z"));
    }

    #[tokio::test]
    async fn the_draft_list_accepts_both_documented_shapes() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/drafts",
            200,
            r#"[{"id":1,"subject":"a"},{"id":2,"subject":"b"}]"#,
        );
        let (client, _store) = make_client(&server).await;
        client.set_access_token("t", Duration::from_secs(60)).await;
        assert_eq!(client.list_drafts().await.expect("list").len(), 2);

        let server2 = MockServer::start().await;
        server2.json_route(
            "GET",
            "/api/v1/client/drafts",
            200,
            r#"{"drafts":[{"id":3,"subject":"c"}]}"#,
        );
        let (client2, _store2) = make_client(&server2).await;
        client2.set_access_token("t", Duration::from_secs(60)).await;
        let drafts = client2.list_drafts().await.expect("list");
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id, 3);
    }

    #[tokio::test]
    async fn devices_are_listed_and_revoked() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            "/api/v1/client/devices",
            200,
            r#"{"devices":[{"id":12,"device_uid":"3f2c","name":"Alice's laptop","platform":"windows",
                "client_version":"0.7.0","protocol_version":1,"revoked":false}]}"#,
        );
        server.json_route("POST", "/api/v1/client/devices/12/revoke", 200, "{}");
        let (client, _store) = make_client(&server).await;
        client.set_access_token("t", Duration::from_secs(60)).await;
        let devices = client.devices().await.expect("devices");
        assert_eq!(devices[0].id, 12);
        assert!(!devices[0].revoked);
        client.revoke_device(12).await.expect("revoke");
        assert_eq!(server.count_for("/api/v1/client/devices/12/revoke"), 1);
    }

    #[tokio::test]
    async fn search_passes_the_query_through_verbatim() {
        let server = MockServer::start().await;
        server.json_route("GET", "/api/v1/client/search", 200, r#"{"items":[],"total":0}"#);
        let (client, _store) = make_client(&server).await;
        client.set_access_token("t", Duration::from_secs(60)).await;
        client
            .search("from:bob subject:invoice has:attachment", Some(3), Some(5), Some(50))
            .await
            .expect("search");
        let request = server.requests_for("/api/v1/client/search").remove(0);
        // `:` and the space are percent-encoded (the space as `+` or `%20`,
        // depending on the encoder); a raw space would truncate the query.
        assert!(request.query.contains("from%3Abob"), "{}", request.query);
        assert!(request.query.contains("subject%3Ainvoice"), "{}", request.query);
        assert!(!request.query.contains(" q=from:bob"), "{}", request.query);
        let params = request.query_params();
        assert_eq!(params.get("mailbox_id").map(String::as_str), Some("3"));
        assert_eq!(params.get("limit").map(String::as_str), Some("50"));
    }

    // -- attachments -------------------------------------------------------

    #[tokio::test]
    async fn a_chunked_upload_resumes_from_the_server_status() {
        let server = MockServer::start().await;
        server.json_route(
            "POST",
            "/api/v1/client/attachments/init",
            200,
            r#"{"attachment_id":77,"chunk_size":4,"upload_token":"ut_1"}"#,
        );
        server.json_route(
            "GET",
            "/api/v1/client/attachments/77/status",
            200,
            r#"{"attachment_id":77,"chunk_size":4,"received":[0,1],"complete":false,"size_bytes":10}"#,
        );
        server.route("PUT", "/api/v1/client/attachments/77/chunk", |_req, _n| {
            MockResponse::json_status(200, "{}")
        });
        server.json_route(
            "POST",
            "/api/v1/client/attachments/77/complete",
            200,
            r#"{"id":77,"filename":"a.bin","content_type":"application/octet-stream","size_bytes":10,"sha256":"deadbeef"}"#,
        );
        let (client, _store) = make_client(&server).await;
        client.set_access_token("t", Duration::from_secs(60)).await;

        let init = client.init_upload("a.bin", "application/octet-stream", 10).await.expect("init");
        assert_eq!(init.attachment_id, 77);
        assert_eq!(init.chunk_size, 4);
        let status = client.upload_status(init.attachment_id).await.expect("status");
        assert_eq!(status.received, vec![0, 1]);
        client.upload_chunk(77, 2, vec![0, 1, 2, 3]).await.expect("chunk");
        let done = client.complete_upload(77, "deadbeef").await.expect("complete");
        assert_eq!(done.id, 77);

        let chunk_request = server
            .requests_for("/api/v1/client/attachments/77/chunk")
            .remove(0);
        assert_eq!(chunk_request.query_params().get("index").map(String::as_str), Some("2"));
        assert_eq!(chunk_request.body.len(), 4);
    }

    #[tokio::test]
    async fn a_range_download_asks_for_the_right_bytes() {
        let server = MockServer::start().await;
        server.route("GET", "/api/v1/client/attachments/9", |req, _n| {
            if req.header("range").is_some() {
                MockResponse::json_with_headers(206, "cdef", vec![("ETag", "\"abcd\"")])
            } else {
                MockResponse::json_status(200, "abcdef")
            }
        });
        let (client, _store) = make_client(&server).await;
        client.set_access_token("t", Duration::from_secs(60)).await;
        let bytes = client
            .download_attachment_bytes(9, Some((4, Some(7))))
            .await
            .expect("range download");
        assert_eq!(bytes, b"cdef");
        let request = server.requests_for("/api/v1/client/attachments/9").remove(0);
        assert_eq!(request.header("range"), Some("bytes=4-7"));

        let resumed = client
            .download_attachment_bytes(9, Some((3, None)))
            .await
            .expect("resume download");
        assert_eq!(resumed, b"cdef");
        let request = server
            .requests_for("/api/v1/client/attachments/9")
            .into_iter()
            .nth(1)
            .expect("second request");
        assert_eq!(request.header("range"), Some("bytes=3-"));

        let whole = client
            .download_attachment_bytes(9, None)
            .await
            .expect("full download");
        assert_eq!(whole, b"abcdef");
    }

    #[tokio::test]
    async fn a_failed_upload_surfaces_the_envelope() {
        let server = MockServer::start().await;
        server.error_route("POST", "/api/v1/client/attachments/init", 413, "limit_exceeded", "too big");
        let (client, _store) = make_client(&server).await;
        client.set_access_token("t", Duration::from_secs(60)).await;
        let err = client.init_upload("big.bin", "application/octet-stream", 1 << 40).await;
        let err = err.expect_err("must fail");
        assert_eq!(err.api_status(), Some(413));
        assert_eq!(err.code(), "limit_exceeded");
    }

    #[test]
    fn base64_helper_is_standard_alphabet() {
        assert_eq!(FcpClient::encode_base64(b"abc"), "YWJj");
    }

    #[test]
    fn account_info_feature_discovery() {
        let info: AccountInfo = serde_json::from_str(
            r#"{"user":{"id":1,"email":"a@b.c"},"features":["sync","drafts"]}"#,
        )
        .expect("parse");
        assert!(info.supports("sync"));
        assert!(!info.supports("events"));
        assert_eq!(info.limits.sync_page_size, 500);

        let bare: AccountInfo =
            serde_json::from_str(r#"{"user":{"id":1,"email":"a@b.c"}}"#).expect("parse");
        assert!(bare.supports("anything"), "no features list means core only");
        assert_eq!(bare.protocol_version, CLIENT_PROTOCOL_VERSION);
    }
}
