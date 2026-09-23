//! The shared Maildir/database boundary for copying or moving a message.
//!
//! The destination body is written before any row is changed. A failed database
//! write removes that staged body, leaving the original row and file intact. The
//! row and its new path change together in SQL; the old body is deleted only after
//! a successful move. PostgreSQL and the filesystem cannot share a transaction:
//! a crash between staging and commit may leave an orphan, never a row whose only
//! body was already deleted. The caller owns sync-log and event publication.

use ferroma_core::{MailboxId, MessageId, UserId};

use crate::error::{Result, StorageError};
use crate::models::Message;
use crate::{Maildir, Repositories};

/// Whether relocation preserves the source message or transfers its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relocation {
    /// Insert a new row with a fresh folder UID, consuming account quota.
    Copy,
    /// Retain the row id and give it a new folder UID.
    Move,
}

/// Where a message is filed on disk and in the database.
#[derive(Debug, Clone, Copy)]
pub struct Destination<'a> {
    /// Existing destination folder id.
    pub folder_id: MailboxId,
    /// Canonical destination folder name.
    pub folder_name: &'a str,
    /// Address domain for the Maildir path.
    pub domain: &'a str,
    /// Address local part for the Maildir path.
    pub local_part: &'a str,
}

/// Stage the destination body, update its row and path together, then clean up.
///
/// The caller must authenticate ownership before calling this function. A copy
/// checks quota and repairs cached usage within its database transaction; moving
/// an existing row consumes no extra quota. The destination must belong to the
/// same address as `source`. Supply `Some(user)` for an authenticated protocol
/// mutation: the durable cursor entry commits with the row. `None` is reserved
/// for repository-level callers that record no user-facing event.
pub async fn relocate_message(
    repos: &Repositories,
    maildir: &Maildir,
    source: &Message,
    to: Destination<'_>,
    action: Relocation,
    user: Option<UserId>,
) -> Result<Message> {
    let destination = repos.folders.find_by_id(to.folder_id).await?
        .ok_or_else(|| StorageError::NotFound(format!("folder {}", to.folder_id)))?;
    if destination.mailbox_id != source.mailbox_id || destination.name != to.folder_name {
        return Err(StorageError::Invalid("destination folder does not match the source address or name".into()));
    }
    let mailbox_id = MailboxId::new(source.mailbox_id);
    if action == Relocation::Copy {
        // Cheap early rejection. The repository repeats this under the owner
        // row lock immediately before the insert, so concurrent COPYs are safe.
        repos.mailboxes.check_quota(mailbox_id, source.size_bytes).await?;
    }
    let bytes = maildir.read(&source.storage_path)?;
    let staged = maildir.store(to.domain, to.local_part, to.folder_name, &bytes, &source.flags)?;
    let updated = match action {
        Relocation::Copy => repos.messages.copy_to_folder_with_path_logged(
            MessageId::new(source.id), to.folder_id, mailbox_id, Some(&staged.path), user,
            Some(&source.storage_path),
        ).await,
        Relocation::Move => repos.messages.move_to_folder_with_path_logged(
            MessageId::new(source.id), to.folder_id, mailbox_id, Some(&staged.path), user,
            Some(&source.storage_path),
        ).await,
    };
    let updated = match updated {
        Ok(row) => row,
        Err(error) => {
            if let Err(cleanup) = maildir.delete(&staged.path) {
                tracing::warn!(path = %staged.path, %cleanup, "failed to remove staged Maildir body");
            }
            return Err(error);
        }
    };
    if action == Relocation::Move {
        if let Err(error) = maildir.delete(&source.storage_path) {
            tracing::warn!(message_id = source.id, %error, "old Maildir body remains after MOVE");
        }
    }
    Ok(updated)
}
