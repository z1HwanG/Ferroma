//! Ferroma official desktop client — shared core.
//!
//! This crate is the platform-independent half of the official client
//! (specification §24–§34). A UI shell — Tauri or otherwise — drives it through
//! [`ui::ClientHandle`]; everything else is headless and testable without a
//! server.
//!
//! `Server = Source of Truth`; everything here is cache and pending intent.
//!
//! * [`account`] — the account manager: add (with autodiscovery), remove, pause,
//!   re-authenticate, per-account sync status (§31).
//! * [`api`] — the FCP HTTP client, its token handling and its retry policy.
//! * [`attachment`] — the content-addressed attachment cache, with `Range`
//!   resume and chunked upload resume (§29).
//! * [`autodiscover`] — `https://<domain>/.well-known/ferroma` plus the
//!   conventional-hostname fallback (§32).
//! * [`database`] — the local SQLite cache (§26).
//! * [`draft`] — offline-first drafts and the §7 conflict rule.
//! * [`error`] — the client's error type.
//! * [`events`] — the realtime WebSocket client and its frame router (§23).
//! * [`notification`] — the `Notifier` seam and the pending-notification table
//!   (§34).
//! * [`operations`] — the offline `pending_operations` queue and its flusher
//!   (§27, §55).
//! * [`outbox`] — the eight-state send pipeline (§28).
//! * [`search`] — the local search query language and the FTS5 index, with the
//!   server fallback (§30).
//! * [`settings`] — the settings surface of §52.
//! * [`sync`] — the sync engine: cursors, idempotent change application,
//!   `uid_validity` invalidation and full resync (§3).
//! * [`ui`] — the documented seam a shell subscribes to.
//! * [`util`] — timestamps, digests and small string helpers.

#![warn(missing_docs)]

pub mod account;
pub mod api;
pub mod attachment;
pub mod autodiscover;
pub mod database;
pub mod draft;
pub mod error;
pub mod events;
pub mod notification;
pub mod operations;
pub mod outbox;
pub mod search;
pub mod settings;
pub mod sync;
pub mod ui;
pub mod util;

#[cfg(test)]
pub(crate) mod testutil;

pub use account::{Account, AccountId, AccountManager, SyncStatus};
pub use api::{
    ApiError, AttachmentDto, Change, DeviceInfo, DeviceRecord, DraftPayload, DraftRecord, FcpClient,
    FolderInfo, MailboxInfo, MessageAction, MessageDetail, MessageItem, MessageList, MessagePatch,
    RetryPolicy, SendRequest, SendResponse, SyncPage, TokenStore, UserRef,
};
pub use attachment::{AttachmentCache, CachedBlob};
pub use autodiscover::{Discovery, DiscoverySource};
pub use database::{ClientDatabase, SearchDocument, SearchMode};
pub use draft::{Draft, DraftStore, OverwriteNotice};
pub use error::{BoxFuture, ClientError, ClientResult};
pub use events::{Backoff, EventRouter, ServerFrame};
pub use notification::{Notification, NotificationKind, NotificationService, Notifier};
pub use operations::{OperationFlusher, OperationKind, PendingOperation, PendingQueue};
pub use outbox::{Outbox, OutboxItem, OutboxState};
pub use search::{SearchExecutor, SearchQuery, SearchResults, SearchSource};
pub use settings::{Settings, SettingsStore, SyncWindow};
pub use sync::{SyncEngine, SyncOutcome, SyncProgress, SyncSummary};
pub use ui::{ClientEvent, ClientHandle};
