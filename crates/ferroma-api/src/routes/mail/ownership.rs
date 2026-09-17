//! Ownership checks — the security property every id-taking route depends on.
//!
//! `docs/api.md` §5.2 is explicit: *"A message the caller does not own is a `404`,
//! never a `403` — the API does not confirm that someone else's message exists."*
//! The same rule applies to drafts, folders and addresses, and it is here, in one
//! module, so that every route goes through the same check instead of inventing its
//! own.
//!
//! The pattern is always the same three steps:
//!
//! 1. load the row by id;
//! 2. compare its owner against the authenticated user;
//! 3. a missing row and a row owned by somebody else both become
//!    [`FerromaError::NotFound`], with the same message shape.
//!
//! A route that takes an id from the request body (a target folder, an attachment)
//! uses the corresponding helper too, so nothing can be reached by guessing an id.

use ferroma_core::{FerromaError, MailboxId, UserId};
use ferroma_storage::models::{Draft, Folder, Mailbox, Message};
use ferroma_storage::Repositories;

/// Turn a lookup that found nothing, or found somebody else's row, into a `404`.
fn not_found(what: &str) -> FerromaError {
    FerromaError::NotFound(format!("no such {what}"))
}

/// The address `id`, when the caller owns it.
pub async fn owned_mailbox(
    repos: &Repositories,
    id: MailboxId,
    user: UserId,
) -> Result<Mailbox, FerromaError> {
    let mailbox = repos.mailboxes.find_by_id(id).await?;
    match mailbox {
        Some(mailbox) if mailbox.user_id == user.get() => Ok(mailbox),
        _ => Err(not_found("mailbox")),
    }
}

/// The folder `id`, when the caller owns the address it lives in.
pub async fn owned_folder(
    repos: &Repositories,
    id: MailboxId,
    user: UserId,
) -> Result<(Folder, Mailbox), FerromaError> {
    let folder = repos.folders.find_by_id(id).await?;
    let Some(folder) = folder else {
        return Err(not_found("folder"));
    };
    let mailbox = owned_mailbox(repos, MailboxId::new(folder.mailbox_id), user).await?;
    Ok((folder, mailbox))
}

/// The message `id`, when the caller owns the address it lives in.
///
/// Works for live rows and for tombstones alike: a message that was expunged still
/// has an owner, and telling the caller "gone" is more accurate than "unknown".
pub async fn owned_message(
    repos: &Repositories,
    id: ferroma_core::MessageId,
    user: UserId,
) -> Result<(Message, Mailbox), FerromaError> {
    let message = repos.messages.find_by_id(id).await?;
    let Some(message) = message else {
        return Err(not_found("message"));
    };
    let mailbox = owned_mailbox(repos, MailboxId::new(message.mailbox_id), user).await?;
    Ok((message, mailbox))
}

/// The same as [`owned_message`], but a tombstone counts as missing.
pub async fn owned_live_message(
    repos: &Repositories,
    id: ferroma_core::MessageId,
    user: UserId,
) -> Result<(Message, Mailbox), FerromaError> {
    let (message, mailbox) = owned_message(repos, id, user).await?;
    if message.expunged_at.is_some() {
        return Err(not_found("message"));
    }
    Ok((message, mailbox))
}

/// The draft `id`, when the caller owns it.
pub async fn owned_draft(
    repos: &Repositories,
    id: ferroma_core::DraftId,
    user: UserId,
) -> Result<Draft, FerromaError> {
    let draft = repos.drafts.find_by_id(id).await?;
    match draft {
        Some(draft) if draft.user_id == user.get() => Ok(draft),
        _ => Err(not_found("draft")),
    }
}

/// The attachment row `id`, when the caller owns the message it belongs to.
///
/// An attachment uploaded but not yet sent hangs from the uploader's placeholder
/// message (see [`crate::state::MessageStub`]), which they own — so ownership is still
/// decided by the message, exactly as it is for real mail.
pub async fn owned_attachment(
    repos: &Repositories,
    id: ferroma_core::AttachmentId,
    user: UserId,
) -> Result<ferroma_storage::models::AttachmentRow, FerromaError> {
    let row = repos.attachments.find_by_id(id).await?;
    let Some(row) = row else {
        return Err(not_found("attachment"));
    };
    owned_message(repos, ferroma_core::MessageId::new(row.message_id), user).await?;
    Ok(row)
}

/// Read one element out of `drafts.recipients`, which is `[{address, name}]`.
pub fn draft_recipients(value: &serde_json::Value) -> Vec<serde_json::Value> {
    value.as_array().cloned().unwrap_or_default()
}

/// The device `id`, when the caller owns it.
pub async fn owned_device(
    repos: &Repositories,
    id: ferroma_core::DeviceId,
    user: UserId,
) -> Result<ferroma_storage::models::Device, FerromaError> {
    let device = repos.devices.find_by_id(id).await?;
    match device {
        Some(device) if device.user_id == user.get() => Ok(device),
        _ => Err(not_found("device")),
    }
}

/// The queue row `id`, when it belongs to a message the caller sent.
///
/// The Admin queue endpoints are *not* scoped this way — an administrator sees the
/// whole server — but a non-admin client watching its own outbox is: the check goes
/// through the message, which is where ownership actually lives.
pub async fn owned_queue_entry(
    repos: &Repositories,
    id: ferroma_core::QueueId,
    user: UserId,
) -> Result<ferroma_storage::models::QueueEntry, FerromaError> {
    let entry = repos.queue.find_by_id(id).await?;
    let Some(entry) = entry else {
        return Err(not_found("queue entry"));
    };
    owned_message(repos, ferroma_core::MessageId::new(entry.message_id), user).await?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_missing_row_reports_the_same_shape() {
        // The message must not distinguish "nobody's" from "somebody else's".
        assert_eq!(not_found("message").to_string(), "not found: no such message");
        assert_eq!(not_found("draft").to_string(), "not found: no such draft");
        assert_eq!(not_found("mailbox").to_string(), "not found: no such mailbox");
        assert_eq!(not_found("folder").to_string(), "not found: no such folder");
        assert_eq!(not_found("device").to_string(), "not found: no such device");
        assert_eq!(
            not_found("attachment").to_string(),
            "not found: no such attachment"
        );
    }

    #[test]
    fn ownership_failures_are_not_found_and_never_forbidden() {
        // This is the whole point: a 403 would confirm the row exists.
        let err = not_found("message");
        assert_eq!(err.http_status(), 404);
        assert_eq!(err.code(), "not_found");
        assert_ne!(err.http_status(), 403);
    }
}
