//! The synchronisation service.
//!
//! `ferroma-sync` is the server side of the contract described in
//! [`docs/sync.md`](../../docs/sync.md) and [`docs/fcp.md`](../../docs/fcp.md):
//!
//! * Every change to a mailbox is appended to the `change_log` table, which is the
//!   single source of the sync cursor.
//! * `GET /api/v1/client/sync` is served by [`SyncService::sync`], which returns the
//!   changes after the client's cursor, in order, capped at a page.
//! * Client-side mutations carry an `operation_id`; [`SyncService::with_operation`]
//!   makes replaying one a no-op that returns the cached response, which is what
//!   lets an offline client retry without fear.
//!
//! The crate deliberately holds no state of its own — it is a thin, well-tested
//! layer over `ferroma-storage`'s `change_log`, `operations` and `client_sync_states`
//! repositories.

#![warn(missing_docs)]

pub mod change;
pub mod service;

pub use change::{ChangeKind, SyncChange, SyncPage};
pub use service::{SyncRequest, SyncService};
