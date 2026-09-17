//! Ferroma core.
//!
//! This crate holds everything the rest of the platform agrees on and nothing
//! that is specific to a protocol or a storage backend:
//!
//! * [`config`] — the whole runtime configuration, loadable from TOML + environment.
//! * [`error`] — one error type (`FerromaError`) used across every crate.
//! * [`ids`] — typed identifiers, so a `UserId` can never be passed as a `MailboxId`.
//! * [`address`] — RFC 5321 email address parsing and normalisation.
//! * [`limits`] — the `[limits]` policy block from the project specification.
//! * [`logging`] — `tracing` initialisation shared by server and client.
//!
//! The crate deliberately does **not** depend on `sqlx`, `axum` or any TLS stack,
//! so the desktop client can reuse it without pulling in a database driver.

pub mod address;
pub mod config;
pub mod error;
pub mod ids;
pub mod limits;
pub mod logging;
pub mod version;

pub use address::EmailAddress;
pub use config::Config;
pub use error::{BoxError, FerromaError, Result};
pub use ids::{
    AttachmentId, AuditLogId, Cursor, DeviceId, DomainId, DraftId, MailboxId, MessageId,
    OperationId, QueueId, RfcMessageId, SessionId, UserId,
};
pub use limits::Limits;
pub use version::{BUILD_TIMESTAMP, PROTOCOL_VERSION, VERSION};
