//! The message operations both mail surfaces share.
//!
//! Webmail (`/api/v1/messages`) and the desktop client (`/api/v1/client/messages`)
//! expose different verbs over the same objects — `POST /messages/:id/read` on one and
//! `PATCH {"seen": true}` on the other — so the *behaviour* lives here once and the
//! routes are thin adapters. Every method here:
//!
//! * checks ownership (through [`crate::routes::mail::ownership`], so a foreign id is
//!   a `404` and never a `403`);
//! * keeps the three stores agreeing — the Maildir file, the `messages` row and the
//!   `change_log` entry a syncing client reads;
//! * publishes the realtime event the Webmail UI reacts to.
//!
//! # Rate limiting and limits
//!
//! Submission is limited per account (`docs/api.md` §5.2): `limits.submission_rate_limit`
//! messages per rolling hour and `limits.daily_send_limit` per UTC day, both counted
//! from `mail_queue` rather than from memory so a restart cannot reset the budget.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferroma_core::config::Config;
use ferroma_core::{FerromaError, MailboxId, MessageId, UserId};
use ferroma_events::{Event, EventBus, EventScope};
use ferroma_storage::models::{Mailbox, Message};
use ferroma_storage::{AttachmentStore, Maildir, Repositories};
use ferroma_sync::SyncService;
use serde::{Deserialize, Serialize};

use crate::routes::mail::ownership::{owned_live_message, owned_mailbox, owned_message};
use crate::routes::mail::store::{
    describe_built_message, domain_name, outgoing_from, OutgoingMessage, StoredOutgoing,
};
use crate::state::MailSender;

/// What a send request asks for.
#[derive(Debug, Clone, Default)]
pub struct SendRequest {
    /// The `From` address. Must be an address the caller owns.
    pub from: String,
    /// Display name for the `From` header.
    pub from_name: Option<String>,
    /// `To` recipients.
    pub to: Vec<String>,
    /// `Cc` recipients.
    pub cc: Vec<String>,
    /// `Bcc` recipients.
    pub bcc: Vec<String>,
    /// Subject.
    pub subject: String,
    /// Plain-text body.
    pub text: Option<String>,
    /// HTML body (sanitised before it is stored).
    pub html: Option<String>,
    /// Attachment ids, already uploaded to `/attachments`.
    pub attachment_ids: Vec<i64>,
    /// The message being replied to.
    pub in_reply_to: Option<String>,
    /// The reference chain.
    pub references: Vec<String>,
    /// A `Reply-To` header.
    pub reply_to: Option<String>,
    /// File the message in `Drafts` with `\Draft` instead of queueing it.
    pub draft: bool,
}

impl SendRequest {
    /// An empty request: no recipients, no body, not a draft.
    ///
    /// Written out rather than derived so the defaults are visible next to the fields
    /// they belong to, and so a future field cannot silently default to something
    /// dangerous.
    pub fn empty() -> Self {
        SendRequest::default()
    }
}

/// The outcome of a send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    /// The stored copy's row id.
    pub message_id: i64,
    /// How many recipients were queued.
    pub queued: usize,
    /// The recipients that were queued, in order.
    pub recipients: Vec<String>,
    /// Whether the message was filed as a draft instead of sent.
    pub draft: bool,
}

/// Message operations over one server's stores.
#[derive(Clone)]
pub struct MessageService {
    repos: Repositories,
    maildir: Maildir,
    attachments: Arc<AttachmentStore>,
    events: Arc<EventBus>,
    sync: Arc<SyncService>,
    config: Arc<Config>,
    mail: Arc<dyn MailSender>,
}

impl std::fmt::Debug for MessageService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageService")
            .field("hostname", &self.config.server.hostname)
            .finish_non_exhaustive()
    }
}

impl MessageService {
    /// Build the service.
    pub fn new(
        repos: Repositories,
        maildir: Maildir,
        attachments: Arc<AttachmentStore>,
        events: Arc<EventBus>,
        sync: Arc<SyncService>,
        config: Arc<Config>,
        mail: Arc<dyn MailSender>,
    ) -> Self {
        MessageService {
            repos,
            maildir,
            attachments,
            events,
            sync,
            config,
            mail,
        }
    }

    /// Swap the maildir (tests, and a future relocation).
    #[must_use]
    pub fn with_maildir(mut self, maildir: Maildir) -> Self {
        self.maildir = maildir;
        self
    }

    /// Swap the attachment store.
    #[must_use]
    pub fn with_attachments(mut self, attachments: Arc<AttachmentStore>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Swap the delivery seam.
    #[must_use]
    pub fn with_mail_sender(mut self, mail: Arc<dyn MailSender>) -> Self {
        self.mail = mail;
        self
    }

    /// The repositories this service writes through.
    pub fn repositories(&self) -> &Repositories {
        &self.repos
    }

    /// The Maildir this service stores bytes in.
    pub fn maildir(&self) -> &Maildir {
        &self.maildir
    }

    /// The blob store.
    pub fn attachments(&self) -> &AttachmentStore {
        &self.attachments
    }

    // -------------------------------------------------------------------------
    // Reading
    // -------------------------------------------------------------------------

    /// A message's RFC 5322 bytes.
    pub async fn raw_message(
        &self,
        id: MessageId,
        user: UserId,
    ) -> Result<(Message, Mailbox, Vec<u8>), FerromaError> {
        let (message, mailbox) = owned_live_message(&self.repos, id, user).await?;
        let bytes = self.maildir.read(&message.storage_path)?;
        Ok((message, mailbox, bytes))
    }

    /// The folder a message would use for a given special use marker.
    pub async fn folder_by_special_use(
        &self,
        mailbox_id: MailboxId,
        special_use: &str,
    ) -> Result<ferroma_storage::models::Folder, FerromaError> {
        let folders = self.repos.folders.list(mailbox_id).await?;
        folders
            .into_iter()
            .find(|folder| folder.special_use.as_deref() == Some(special_use))
            .ok_or_else(|| FerromaError::NotFound(format!("no folder with special_use {special_use}")))
    }

    /// The `Trash` folder of an address, creating the standard set if it is missing.
    pub async fn trash_folder(
        &self,
        mailbox: &Mailbox,
    ) -> Result<ferroma_storage::models::Folder, FerromaError> {
        if let Ok(folder) = self
            .folder_by_special_use(mailbox.mailbox_id(), "\\Trash")
            .await
        {
            return Ok(folder);
        }
        let folders = self.repos.folders.ensure_standard(mailbox.mailbox_id()).await?;
        folders
            .into_iter()
            .find(|folder| folder.special_use.as_deref() == Some("\\Trash"))
            .ok_or_else(|| FerromaError::NotFound("the Trash folder is missing".to_string()))
    }

    /// The `Archive` folder of an address.
    pub async fn archive_folder(
        &self,
        mailbox: &Mailbox,
    ) -> Result<ferroma_storage::models::Folder, FerromaError> {
        if let Ok(folder) = self
            .folder_by_special_use(mailbox.mailbox_id(), "\\Archive")
            .await
        {
            return Ok(folder);
        }
        let folders = self.repos.folders.ensure_standard(mailbox.mailbox_id()).await?;
        folders
            .into_iter()
            .find(|folder| folder.special_use.as_deref() == Some("\\Archive"))
            .ok_or_else(|| FerromaError::NotFound("the Archive folder is missing".to_string()))
    }

    // -------------------------------------------------------------------------
    // Sending
    // -------------------------------------------------------------------------

    /// Send (or save as a draft) a composed message.
    pub async fn send(&self, user: UserId, request: &SendRequest) -> Result<SendResult, FerromaError> {
        let (mailbox, domain) = crate::routes::mail::store::resolve_sender(
            &self.repos,
            &request.from,
            user,
        )
        .await?;

        let outgoing = self.build_outgoing(&mailbox, &domain, request, user).await?;
        let recipients = outgoing.envelope_recipients();

        if !request.draft {
            self.check_send_limits(user, recipients.len()).await?;
        }

        let folder_name = if request.draft { "Drafts" } else { "Sent" };
        let flags = if request.draft { "draft seen" } else { "seen" };

        // Guarantee the target folder exists on disk and in the database; an
        // account created by an older build, or by hand, may be missing `Sent`.
        self.repos.folders.ensure_standard(mailbox.mailbox_id()).await?;
        self.maildir
            .ensure_mailbox(&domain, &mailbox.local_part)?;

        let bytes = outgoing.build(&domain)?;
        let body = describe_built_message(&bytes, &outgoing);
        let stored = self
            .store_bytes(&domain, &mailbox, folder_name, flags, request.draft, &outgoing, &body)
            .await?;

        self.sync
            .record_message_created(user, &stored.message)
            .await?;

        let (queued, queued_recipients) = if request.draft {
            (0usize, Vec::new())
        } else {
            let outcome = self
                .mail
                .enqueue(Some(user), stored.message.message_id(), outgoing.from.clone(), recipients)
                .await?;
            (outcome.queued, outcome.recipients)
        };

        // Every attachment now hangs from the real message, so the uploader's
        // placeholder has nothing left to own.
        if let Err(err) = self.cleanup_stub(user).await {
            tracing::debug!(error = %err, "could not remove the upload placeholder");
        }

        if !request.draft {
            self.events
                .publish(
                    EventScope::User(user),
                    Event::mail_sent(
                        mailbox.mailbox_id(),
                        stored.message.message_id(),
                        queued,
                        true,
                    ),
                )
                .await;
        }

        tracing::info!(
            user_id = user.get(),
            message_id = stored.message.id,
            queued,
            draft = request.draft,
            "message stored"
        );

        Ok(SendResult {
            message_id: stored.message.id,
            queued,
            recipients: queued_recipients,
            draft: request.draft,
        })
    }

    /// Build the outgoing message, resolving and validating attachments.
    async fn build_outgoing(
        &self,
        mailbox: &Mailbox,
        domain: &str,
        request: &SendRequest,
        user: UserId,
    ) -> Result<OutgoingMessage, FerromaError> {
        let _ = domain;

        if !request.draft && request.to.is_empty() && request.cc.is_empty() && request.bcc.is_empty() {
            return Err(FerromaError::Invalid(
                "a message needs at least one recipient".to_string(),
            ));
        }

        let total_recipients = request.to.len() + request.cc.len() + request.bcc.len();
        if total_recipients > self.config.limits.max_recipients {
            return Err(FerromaError::LimitExceeded(format!(
                "{total_recipients} recipients exceeds the limit of {}",
                self.config.limits.max_recipients
            )));
        }
        if request.attachment_ids.len() > self.config.limits.max_attachments {
            return Err(FerromaError::LimitExceeded(format!(
                "{} attachments exceeds the limit of {}",
                request.attachment_ids.len(),
                self.config.limits.max_attachments
            )));
        }

        let from = mailbox.address(domain);
        let mut attachments = Vec::with_capacity(request.attachment_ids.len());
        for id in &request.attachment_ids {
            let row = crate::routes::mail::ownership::owned_attachment(
                &self.repos,
                ferroma_core::AttachmentId::new(*id),
                user,
            )
            .await?;
            // A placeholder row has no bytes on disk yet.
            if row.storage_path.is_empty() {
                return Err(FerromaError::Conflict(format!(
                    "attachment {id} has not finished uploading"
                )));
            }
            let data = self.attachments.read(&row.storage_path)?;
            if data.len() as u64 > self.config.limits.max_attachment_size {
                return Err(FerromaError::LimitExceeded(format!(
                    "attachment {id} exceeds the limit of {} bytes",
                    self.config.limits.max_attachment_size
                )));
            }
            attachments.push(crate::routes::mail::store::ComposeAttachment {
                id: Some(row.id),
                filename: row
                    .filename
                    .clone()
                    .unwrap_or_else(|| format!("attachment-{id}")),
                content_type: row.content_type.clone(),
                data,
                storage_path: Some(row.storage_path.clone()),
            });
        }

        Ok(outgoing_from(from, request, attachments))
    }

    /// Refuse a send that would breach the hourly or daily budget.
    pub async fn check_send_limits(
        &self,
        user: UserId,
        recipients: usize,
    ) -> Result<(), FerromaError> {
        if recipients > self.config.limits.max_recipients {
            return Err(FerromaError::LimitExceeded(format!(
                "{recipients} recipients exceeds the limit of {}",
                self.config.limits.max_recipients
            )));
        }

        let now = Utc::now();
        let hourly = self
            .repos
            .queue
            .count_sent_since(user, now - chrono::Duration::hours(1))
            .await?;
        if hourly >= i64::from(self.config.limits.submission_rate_limit) {
            return Err(FerromaError::RateLimited);
        }

        let today = self.utc_day_start(now);
        let daily = self
            .repos
            .queue
            .count_sent_since(user, today)
            .await?;
        if daily >= i64::from(self.config.limits.daily_send_limit) {
            return Err(FerromaError::RateLimited);
        }

        Ok(())
    }

    /// Midnight UTC of the day `now` falls in.
    pub fn utc_day_start(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now.date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|naive| naive.and_utc())
            .unwrap_or(now)
    }

    /// Write bytes to the Maildir and the matching rows to the database.
    #[allow(clippy::too_many_arguments)]
    async fn store_bytes(
        &self,
        domain: &str,
        mailbox: &Mailbox,
        folder: &str,
        flags: &str,
        is_draft: bool,
        outgoing: &OutgoingMessage,
        body: &crate::routes::mail::store::ParsedBody,
    ) -> Result<StoredOutgoing, FerromaError> {
        crate::routes::mail::store::store_message(
            &self.repos,
            &self.maildir,
            domain,
            mailbox,
            folder,
            flags,
            is_draft,
            outgoing,
            body,
        )
        .await
    }

    // -------------------------------------------------------------------------
    // Mutating one message
    // -------------------------------------------------------------------------

    /// Apply the four boolean flags `PATCH /messages/:id` accepts.
    pub async fn set_message_flags(
        &self,
        id: MessageId,
        user: UserId,
        seen: Option<bool>,
        flagged: Option<bool>,
        answered: Option<bool>,
        deleted: Option<bool>,
    ) -> Result<Message, FerromaError> {
        let (message, _mailbox) = owned_live_message(&self.repos, id, user).await?;

        if let Some(seen) = seen {
            self.repos.messages.mark_seen(id, seen).await?;
        }
        if let Some(flagged) = flagged {
            if flagged {
                self.repos.messages.add_flags(id, "flagged").await?;
            } else {
                self.repos.messages.remove_flags(id, "flagged").await?;
            }
        }
        if let Some(answered) = answered {
            if answered {
                self.repos.messages.add_flags(id, "answered").await?;
            } else {
                self.repos.messages.remove_flags(id, "answered").await?;
            }
        }
        if let Some(deleted) = deleted {
            if deleted {
                self.repos.messages.mark_deleted(id).await?;
            } else {
                self.repos.messages.clear_deleted(id).await?;
            }
        }

        let updated = self
            .repos
            .messages
            .find_by_id(id)
            .await?
            .unwrap_or(message);

        // Keep the Maildir file name in step with the flags, which is what an IMAP
        // client reads directly off the filesystem.
        let mailbox = owned_mailbox(&self.repos, MailboxId::new(updated.mailbox_id), user).await?;
        let domain = domain_name(&self.repos, mailbox.domain_id).await?;
        let folder = self
            .repos
            .folders
            .find_by_id(MailboxId::new(updated.folder_id))
            .await?
            .ok_or_else(|| FerromaError::NotFound(format!("folder {}", updated.folder_id)))?;
        if let Err(err) = self.refresh_maildir(&domain, &mailbox, &folder.name, &updated).await {
            tracing::warn!(message_id = updated.id, error = %err, "could not rename the maildir file");
        }

        self.sync
            .record_message_flags(user, &updated)
            .await?;

        if seen.is_some() {
            self.events
                .publish(
                    EventScope::User(user),
                    Event::mail_read(
                        mailbox.mailbox_id(),
                        updated.message_id(),
                        updated.flags.contains("seen"),
                    ),
                )
                .await;
        } else {
            self.events
                .publish(
                    EventScope::User(user),
                    Event::mail_flag_changed(mailbox.mailbox_id(), updated.message_id(), updated.flags.clone()),
                )
                .await;
        }

        Ok(updated)
    }

    /// Apply flag changes and persist the new Maildir path.
    async fn refresh_maildir(
        &self,
        domain: &str,
        mailbox: &Mailbox,
        folder: &str,
        message: &Message,
    ) -> Result<(), FerromaError> {
        let _ = (domain, mailbox, folder);
        let path = self.maildir.set_flags(&message.storage_path, &message.flags)?;
        if path != message.storage_path {
            self.repos
                .messages
                .set_storage_path(message.message_id(), &path)
                .await?;
        }
        Ok(())
    }

    /// Move a message into another folder of the same address.
    pub async fn move_message(
        &self,
        id: MessageId,
        user: UserId,
        target_folder: MailboxId,
    ) -> Result<Message, FerromaError> {
        let (message, mailbox) = owned_message(&self.repos, id, user).await?;
        let target = self
            .repos
            .folders
            .find_by_id(target_folder)
            .await?
            .filter(|folder| folder.mailbox_id == message.mailbox_id)
            .ok_or_else(|| FerromaError::NotFound("no such folder".to_string()))?;

        let domain = domain_name(&self.repos, mailbox.domain_id).await?;
        let from_folder = message.folder_id;
        let from_folder_row = self
            .repos
            .folders
            .find_by_id(MailboxId::new(from_folder))
            .await?;
        let from_name = from_folder_row
            .as_ref()
            .map(|folder| folder.name.clone())
            .unwrap_or_else(|| "INBOX".to_string());

        // Move the file first: a row pointing at a file in the old folder would be
        // served from a directory an IMAP client no longer looks in.
        let new_path = self.maildir.move_message(
            &message.storage_path,
            &domain,
            &mailbox.local_part,
            &target.name,
            &message.flags,
        )?;

        let moved = self
            .repos
            .messages
            .move_to_folder(id, target.folder_id(), mailbox.mailbox_id())
            .await?;
        self.repos
            .messages
            .set_storage_path(id, &new_path)
            .await?;
        let moved = self
            .repos
            .messages
            .find_by_id(id)
            .await?
            .unwrap_or(moved);

        self.sync
            .record_message_moved(user, MailboxId::new(from_folder), &moved)
            .await?;
        self.events
            .publish(
                EventScope::User(user),
                Event::mail_moved(
                    MailboxId::new(from_folder),
                    target.folder_id(),
                    moved.message_id(),
                ),
            )
            .await;

        tracing::debug!(
            message_id = moved.id,
            from = %from_name,
            to = %target.name,
            "message moved"
        );
        Ok(moved)
    }

    /// Copy a message into another folder of the same address.
    pub async fn copy_message(
        &self,
        id: MessageId,
        user: UserId,
        target_folder: MailboxId,
    ) -> Result<Message, FerromaError> {
        let (message, mailbox) = owned_message(&self.repos, id, user).await?;
        let target = self
            .repos
            .folders
            .find_by_id(target_folder)
            .await?
            .filter(|folder| folder.mailbox_id == message.mailbox_id)
            .ok_or_else(|| FerromaError::NotFound("no such folder".to_string()))?;

        let domain = domain_name(&self.repos, mailbox.domain_id).await?;
        let bytes = self.maildir.read(&message.storage_path)?;
        let stored = self
            .maildir
            .store(&domain, &mailbox.local_part, &target.name, &bytes, &message.flags)?;

        let copied = self
            .repos
            .messages
            .copy_to_folder(id, target.folder_id(), mailbox.mailbox_id())
            .await?;
        self.repos
            .messages
            .set_storage_path(copied.message_id(), &stored.path)
            .await?;
        let copied = self
            .repos
            .messages
            .find_by_id(copied.message_id())
            .await?
            .unwrap_or(copied);

        self.sync
            .record_message_created(user, &copied)
            .await?;
        Ok(copied)
    }

    /// Delete a message: to `Trash`, or for good.
    ///
    /// `permanent = false` moves the message to the address's `Trash` folder, which is
    /// what the Webmail delete button means. `permanent = true` removes the row and
    /// the file and leaves a tombstone in the change log so a syncing client learns
    /// the id is gone rather than silently keeping a stale copy.
    pub async fn delete_message(
        &self,
        id: MessageId,
        user: UserId,
        permanent: bool,
    ) -> Result<(), FerromaError> {
        let (message, mailbox) = owned_message(&self.repos, id, user).await?;

        if !permanent && self.config.storage.soft_delete {
            let trash = self.trash_folder(&mailbox).await?;
            if trash.id != message.folder_id {
                self.move_message(id, user, trash.folder_id()).await?;
                return Ok(());
            }
        }

        let folder_id = MailboxId::new(message.folder_id);
        let uid = message.uid;

        // The row first: losing the file is recoverable, losing the row while the
        // file survives would leave mail the platform cannot account for.
        self.repos.messages.hard_delete(id).await?;
        if let Err(err) = self.maildir.delete(&message.storage_path) {
            tracing::warn!(message_id = message.id, error = %err, "maildir file could not be removed");
        }

        self.sync
            .record_message_deleted(user, mailbox.mailbox_id(), folder_id, id, uid, true)
            .await?;
        self.events
            .publish(
                EventScope::User(user),
                Event::mail_deleted(mailbox.mailbox_id(), id, true),
            )
            .await;
        Ok(())
    }

    /// Mark a folder's `\Deleted` messages expunged and delete their files.
    pub async fn expunge_folder(
        &self,
        mailbox: &Mailbox,
        folder: MailboxId,
        user: UserId,
    ) -> Result<usize, FerromaError> {
        let expunged = self.repos.messages.expunge(folder).await?;
        let domain = domain_name(&self.repos, mailbox.domain_id).await?;
        let _ = domain;
        for message in &expunged {
            if let Err(err) = self.maildir.delete(&message.storage_path) {
                tracing::warn!(message_id = message.id, error = %err, "maildir file could not be removed");
            }
            self.sync
                .record_message_deleted(
                    user,
                    mailbox.mailbox_id(),
                    folder,
                    message.message_id(),
                    message.uid,
                    true,
                )
                .await?;
        }
        Ok(expunged.len())
    }

    /// Publish `mail.received` for a message that arrived by some other path.
    pub async fn announce_received(&self, user: UserId, mailbox: MailboxId, message: &Message) {
        self.events
            .publish(
                EventScope::User(user),
                Event::MailReceived(ferroma_events::MailReceived {
                    mailbox_id: mailbox,
                    message_id: message.message_id(),
                    from: message.sender.clone(),
                    subject: message.subject.clone(),
                    size_bytes: message.size_bytes,
                    snippet: message.snippet.clone(),
                }),
            )
            .await;
    }

    /// Remove an account's upload placeholder, once nothing points at it.
    ///
    /// A placeholder that still owns attachment rows is kept: an upload in flight must
    /// not lose the row it is checked against.
    pub async fn cleanup_stub(&self, user: UserId) -> Result<(), FerromaError> {
        let Some(mailbox) = self.repos.mailboxes.find_primary(user).await? else {
            return Ok(());
        };
        let Some(folder) = self
            .repos
            .folders
            .find_by_name(mailbox.mailbox_id(), "Drafts")
            .await?
        else {
            return Ok(());
        };
        let rows = self
            .repos
            .messages
            .list_by_folder(folder.folder_id(), 50, 0)
            .await?;
        for message in rows {
            if !crate::state::MessageStub::is_stub(&message) {
                continue;
            }
            let owned = self
                .repos
                .attachments
                .list_by_message(message.message_id())
                .await?;
            if owned.is_empty() {
                let _ = self.repos.messages.hard_delete(message.message_id()).await;
            }
        }
        Ok(())
    }

    /// Record a change-log entry for a message that was inserted elsewhere.
    pub async fn record_created(
        &self,
        user: UserId,
        message: &Message,
    ) -> Result<(), FerromaError> {
        self.sync.record_message_created(user, message).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ferroma_sync::ChangeKind;

    /// A service over repositories that are never queried.
    ///
    /// `PgPool::connect_lazy` registers with the current Tokio runtime, which is why
    /// every test that builds one is a `#[tokio::test]`.
    fn service() -> MessageService {
        let repos = crate::tests_support::lazy_repos();
        MessageService::new(
            repos.clone(),
            Maildir::new(
                std::env::temp_dir(),
                false,
                ferroma_core::config::MailboxLayout::Maildir,
            ),
            Arc::new(AttachmentStore::new(std::env::temp_dir(), false)),
            Arc::new(EventBus::with_defaults()),
            Arc::new(SyncService::new(repos.clone(), 500, 30)),
            Arc::new(Config::default()),
            Arc::new(crate::state::QueueMailSender::new(repos, 12)),
        )
    }

    #[test]
    fn send_request_defaults_are_safe() {
        let request = SendRequest::default();
        assert!(request.to.is_empty());
        assert!(!request.draft);
        assert!(request.from.is_empty());
        assert!(request.attachment_ids.is_empty());
    }

    #[tokio::test]
    async fn utc_day_start_is_midnight_utc() {
        let service = service();

        let now = Utc
            .with_ymd_and_hms(2026, 9, 16, 12, 34, 56)
            .single()
            .expect("valid instant");
        let start = service.utc_day_start(now);
        assert_eq!(
            start,
            Utc.with_ymd_and_hms(2026, 9, 16, 0, 0, 0)
                .single()
                .expect("valid instant")
        );
    }

    #[test]
    fn change_kinds_are_the_documented_wire_names() {
        assert_eq!(
            [
                ChangeKind::MessageCreated.as_str(),
                ChangeKind::MessageUpdated.as_str(),
                ChangeKind::MessageDeleted.as_str(),
                ChangeKind::MessageMoved.as_str(),
            ],
            [
                "message_created",
                "message_updated",
                "message_deleted",
                "message_moved"
            ]
        );
    }

    #[tokio::test]
    async fn the_service_swaps_its_stores() {
        let repos = crate::tests_support::lazy_repos();
        let dir = tempfile::tempdir().expect("temp dir");
        let service = MessageService::new(
            repos.clone(),
            Maildir::new(dir.path().join("a"), false, ferroma_core::config::MailboxLayout::Maildir),
            Arc::new(AttachmentStore::new(dir.path().join("att-a"), false)),
            Arc::new(EventBus::with_defaults()),
            Arc::new(SyncService::new(repos.clone(), 500, 30)),
            Arc::new(Config::default()),
            Arc::new(crate::state::QueueMailSender::new(repos, 12)),
        );

        let swapped = service
            .with_maildir(Maildir::new(
                dir.path().join("b"),
                false,
                ferroma_core::config::MailboxLayout::Maildir,
            ))
            .with_attachments(Arc::new(AttachmentStore::new(dir.path().join("att-b"), false)));
        assert_eq!(swapped.maildir().root(), dir.path().join("b"));
        assert_eq!(swapped.attachments().root(), dir.path().join("att-b"));
        assert_eq!(swapped.repositories().pool().size(), 0);
    }
}
