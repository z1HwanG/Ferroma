//! Ferroma storage — PostgreSQL metadata plus on-disk mail.
//!
//! Two stores, one truth:
//!
//! * [`Database`] — a `sqlx` PostgreSQL pool, the embedded migrations, and
//!   [`repository::Repositories`]: typed access to users, domains, mailboxes,
//!   folders, messages, the queue, sessions, devices, drafts, the sync journal and
//!   the audit trail.
//! * [`Maildir`] — the RFC 5322 bytes on the filesystem, laid out exactly as the
//!   specification §13 describes.
//! * [`AttachmentStore`] — content-addressed blobs, so the same attachment stored
//!   by two messages occupies one file.
//!
//! The database is authoritative for *what exists*; the filesystem is authoritative
//! for *the bytes*. A row without its file is a [`StorageError::BodyMissing`]; a file
//! without its row is garbage collected by [`AttachmentStore::gc`] and
//! [`Maildir::sweep_tmp`].
//!
//! ```no_run
//! use ferroma_core::config::Config;
//! use ferroma_storage::{AttachmentStore, Database, Maildir};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let config = Config::load(None)?;
//! let db = Database::connect(&config.database).await?;
//! db.migrate().await?;
//!
//! let repos = db.repositories();
//! let maildir = Maildir::new(config.maildir_root(), config.storage.fsync_on_write, config.storage.layout);
//! let attachments = AttachmentStore::new(config.attachment_root(), config.storage.fsync_on_write);
//!
//! maildir.ensure_mailbox("example.com", "alice")?;
//! let stored = maildir.store("example.com", "alice", "INBOX", b"From: a@b\r\n\r\nhi\r\n", "")?;
//! println!("stored {} ({} bytes)", stored.path, stored.size);
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

pub mod attachment;
pub mod database;
pub mod error;
pub mod maildir;
pub mod models;
pub mod repository;

pub use attachment::{AttachmentStore, StoredBlob};
pub use database::{Database, PoolStats};
pub use error::{Result, StorageError};
pub use maildir::{Maildir, MaildirEntry, StoredMessage};
pub use models::{
    Alias, AttachmentRow, AuditLog, ChangeLogEntry, ClientSyncState, DeliveryAttempt, Device,
    Domain, Draft, Folder, LoginAttempt, Mailbox, Message, MessageRecipient, Operation, QueueEntry,
    Session, Setting, User,
};
pub use repository::Repositories;
