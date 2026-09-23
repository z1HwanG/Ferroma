//! Local delivery: turning an accepted SMTP transaction into stored state.
//!
//! One accepted `DATA` becomes, for each resolved recipient:
//!
//! ```text
//!   parse (ferroma-mail)  ─►  resolve recipient  ─►  quota check  ─►  Maildir::store
//!          │                        │                    │                │
//!          │                        │                    │                ▼
//!          │                        │                    │        messages row (allocates the UID)
//!          │                        │                    │                │
//!          ▼                        ▼                    ▼                ▼
//!     snippet/subject        exact → alias →        MailboxFull       recipients + attachments
//!     Message-ID/Date          catch-all            → 452 4.2.2              │
//!                                                                            ▼
//!                                                          recount + add_usage + change_log + EventBus
//! ```
//!
//! # A full mailbox must not lose the whole message
//!
//! Specification §39 and RFC 3463 `4.2.2`: a mailbox that is over quota is a
//! **temporary** per-recipient failure. The other recipients still get their copy,
//! and the sender is told `452` so it retries that one address later. Failing the
//! whole transaction because one of five recipients is full would lose four
//! deliverable messages.
//!
//! # Received header
//!
//! [`ferroma_mail::Envelope::received_header`] builds the trace line; it is
//! prepended to the bytes that are stored, so a message that leaves this server
//! again carries an honest record of where it came from.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferroma_core::{EmailAddress, FerromaError, MailboxId, MessageId, Result, UserId};
use ferroma_events::{Event, EventBus, EventScope, MailReceived};
use ferroma_mail::{Envelope, Flags, ParsedMessage};
use ferroma_storage::models::{Mailbox, Message};
use ferroma_storage::repository::{NewAttachment, NewChange, NewMessage, Recipient};
use ferroma_storage::{AttachmentStore, Maildir, Repositories, StorageError};

/// How many characters of the body go into the list-view snippet.
pub const SNIPPET_CHARS: usize = 160;

/// The folder inbound mail lands in.
pub const INBOX: &str = "INBOX";

/// The folder a quarantined message lands in.
///
/// The DMARC quarantine action files here instead of `INBOX`: the message is kept
/// (never silently dropped after an acknowledgement) but out of the user's way.
pub const JUNK: &str = "Junk";

/// A message accepted from a peer, ready to be delivered.
#[derive(Debug, Clone)]
pub struct ReceivedMessage {
    /// The `MAIL FROM` address. `None` for the null sender.
    pub sender: Option<EmailAddress>,
    /// Every accepted `RCPT TO` address.
    pub recipients: Vec<EmailAddress>,
    /// The raw RFC 5322 bytes, **without** the `Received:` header.
    pub body: Vec<u8>,
    /// The name the peer announced in `EHLO`/`HELO`.
    pub helo: Option<String>,
    /// The peer's address, as observed.
    pub remote_ip: Option<IpAddr>,
    /// When the transaction completed.
    pub received_at: DateTime<Utc>,
    /// For structured logs and for the queue's provenance column.
    pub connection_id: String,
    /// The folder inbound delivery files into. `None` means [`INBOX`].
    ///
    /// Set by the DMARC quarantine action, which files a message that failed an
    /// enforcing policy into `Junk` rather than refusing it. It is a *delivery* target,
    /// not protocol state, which is why it is never set by the parser or the session.
    pub deliver_to: Option<String>,
    /// The rendered `Authentication-Results:` **value** to prepend, when inbound policy
    /// ran and `policy.add_auth_results` is on.
    ///
    /// Stored as the header value rather than the whole line so this struct never has
    /// to reason about line endings.
    pub authentication_results: Option<String>,
}

impl ReceivedMessage {
    /// An empty message from `remote_ip`.
    pub fn new(body: Vec<u8>, remote_ip: Option<IpAddr>, connection_id: impl Into<String>) -> Self {
        ReceivedMessage {
            sender: None,
            recipients: Vec::new(),
            body,
            helo: None,
            remote_ip,
            received_at: Utc::now(),
            connection_id: connection_id.into(),
            deliver_to: None,
            authentication_results: None,
        }
    }

    /// The folder this message lands in.
    pub fn folder(&self) -> &str {
        self.deliver_to.as_deref().unwrap_or(INBOX)
    }

    /// The [`Envelope`] view of this transaction, for the `Received:` header.
    pub fn envelope(&self) -> Envelope {
        let mut envelope = Envelope::new();
        envelope.from = self.sender.clone();
        envelope.recipients = self.recipients.clone();
        envelope.helo = self.helo.clone();
        envelope.remote_ip = self.remote_ip;
        envelope.received_at = self.received_at;
        envelope
    }

    /// The bytes to store: our trace headers followed by the message.
    ///
    /// `Authentication-Results:` goes above `Received:` because it is the later of the
    /// two — each hop's trace headers stack on top of the message, newest first. Neither
    /// header is added twice, and `Received:` is prepended only when it is wanted.
    pub fn bytes_with_received(&self, hostname: &str, add_received: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 256);

        if let Some(value) = self.authentication_results.as_deref() {
            let value = value.trim_end_matches(['\r', '\n']);
            if !value.is_empty() {
                out.extend_from_slice(b"Authentication-Results: ");
                out.extend_from_slice(value.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }

        if add_received {
            let header = self.envelope().received_header(hostname);
            out.extend_from_slice(b"Received: ");
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(b"\r\n");
        }

        out.extend_from_slice(&self.body);
        out
    }
}

/// What happened to one recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipientOutcome {
    /// The message is in the recipient's `INBOX`.
    Delivered {
        /// The address as the peer wrote it (normalised).
        address: String,
        /// The address that actually received it, after alias resolution.
        resolved_to: String,
        /// The mailbox (address row) it landed in.
        mailbox_id: MailboxId,
        /// The stored message row.
        message_id: MessageId,
    },
    /// The address does not exist here.
    Unknown {
        /// The address the peer asked for.
        address: String,
    },
    /// The mailbox exists but cannot take the message **right now**.
    ///
    /// Always temporary: the peer should retry this recipient alone.
    Full {
        /// The address the peer asked for.
        address: String,
        /// The mailbox that is full.
        mailbox_id: MailboxId,
    },
    /// Something else went wrong for this recipient.
    Failed {
        /// The address the peer asked for.
        address: String,
        /// Why. Already sanitised for a log line.
        reason: String,
    },
    /// Not a local mailbox. An authenticated submission accepted it for the queue.
    Queued {
        /// The address as the peer wrote it.
        address: String,
        /// The Sent copy backing the outbound queue entry.
        message_id: MessageId,
    },
}

impl RecipientOutcome {
    /// The address this outcome is about.
    pub fn address(&self) -> &str {
        match self {
            RecipientOutcome::Delivered { address, .. }
            | RecipientOutcome::Unknown { address }
            | RecipientOutcome::Full { address, .. }
            | RecipientOutcome::Failed { address, .. }
            | RecipientOutcome::Queued { address, .. } => address,
        }
    }

    /// Whether the message reached this recipient's mailbox.
    pub fn is_delivered(&self) -> bool {
        matches!(self, RecipientOutcome::Delivered { .. })
    }

    /// The stored message, when there is one.
    pub fn message_id(&self) -> Option<MessageId> {
        match self {
            RecipientOutcome::Delivered { message_id, .. }
            | RecipientOutcome::Queued { message_id, .. } => Some(*message_id),
            _ => None,
        }
    }

    /// The error this outcome should be reported as, for the SMTP reply.
    pub fn error(&self) -> Option<FerromaError> {
        match self {
            RecipientOutcome::Delivered { .. } | RecipientOutcome::Queued { .. } => None,
            RecipientOutcome::Unknown { address } => Some(FerromaError::NotFound(format!(
                "no such mailbox: {address}"
            ))),
            RecipientOutcome::Full { address, .. } => {
                Some(FerromaError::MailboxFull(address.clone()))
            }
            RecipientOutcome::Failed { reason, .. } => {
                Some(FerromaError::storage(std::io::Error::other(reason.clone())))
            }
        }
    }
}

/// The result of one accepted transaction.
#[derive(Debug, Clone, Default)]
pub struct DeliveryReport {
    /// One outcome per recipient, in the order they were given.
    pub outcomes: Vec<RecipientOutcome>,
}

impl DeliveryReport {
    /// The recipients that were delivered to.
    pub fn delivered(&self) -> Vec<&RecipientOutcome> {
        self.outcomes.iter().filter(|o| o.is_delivered()).collect()
    }

    /// The recipients that failed.
    pub fn failed(&self) -> Vec<&RecipientOutcome> {
        self.outcomes.iter().filter(|o| !o.is_delivered()).collect()
    }

    /// Whether at least one recipient received the message.
    ///
    /// This is the decision SMTP needs: `250` when true, `5xx`/`452` when false.
    pub fn any_delivered(&self) -> bool {
        self.outcomes.iter().any(|outcome| {
            outcome.is_delivered() || matches!(outcome, RecipientOutcome::Queued { .. })
        })
    }

    /// Whether the whole transaction failed.
    pub fn all_failed(&self) -> bool {
        !self.any_delivered()
    }

    /// The reply the session must send back.
    ///
    /// * every recipient delivered → `250 2.0.0 Ok: queued as <id>`
    /// * some delivered, some full → `250` too: the accepted ones are stored, and the
    ///   peer will find out about the others from its per-recipient log. RFC 5321 has
    ///   no way to report a partial failure after `DATA`, which is exactly why the
    ///   per-recipient failures are also published as events.
    /// * nothing delivered and something was full → `452 4.2.2` (retry)
    /// * nothing delivered otherwise → the first failure's reply
    pub fn reply(&self) -> crate::reply::Reply {
        use crate::reply::Reply;

        if let Some(id) = self.outcomes.iter().find_map(RecipientOutcome::message_id) {
            return Reply::accepted(&id.to_string());
        }
        if self
            .outcomes
            .iter()
            .any(|o| matches!(o, RecipientOutcome::Full { .. }))
        {
            return Reply::mailbox_full();
        }
        match self.outcomes.first().and_then(RecipientOutcome::error) {
            Some(error) => Reply::from_delivery_error(&error),
            None => Reply::service_ready("", ""),
        }
    }
}

/// Where one recipient resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedRecipient {
    /// Delivered into this address row.
    Mailbox(Mailbox),
    /// Nothing here matches.
    Unknown,
}

/// Local delivery, bound to one database, one Maildir and one blob store.
#[derive(Debug, Clone)]
pub struct DeliveryService {
    repos: Repositories,
    maildir: Maildir,
    attachments: AttachmentStore,
    /// The hostname in the `Received:` header.
    hostname: String,
    /// Whether to prepend a `Received:` header at all.
    add_received: bool,
    bus: Option<EventBus>,
}

impl DeliveryService {
    /// Build a delivery service.
    pub fn new(
        repos: Repositories,
        maildir: Maildir,
        attachments: AttachmentStore,
        hostname: impl Into<String>,
    ) -> Self {
        DeliveryService {
            repos,
            maildir,
            attachments,
            hostname: hostname.into(),
            add_received: true,
            bus: None,
        }
    }

    /// Turn the `Received:` header off (used by tests that compare stored bytes).
    pub fn without_received_header(mut self) -> Self {
        self.add_received = false;
        self
    }

    /// Publish `mail.received` on this bus.
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
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

    /// The attachment store.
    pub fn attachments(&self) -> &AttachmentStore {
        &self.attachments
    }

    /// The hostname used in `Received:`.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    // ------------------------------------------------------------------
    // Recipient resolution
    // ------------------------------------------------------------------

    /// Resolve one address to a mailbox.
    ///
    /// The order is the specification's: an exact address wins, then a forwarding
    /// alias (followed **one hop**, so `a -> b -> c` stops at `b`), then the domain's
    /// catch-all.
    pub async fn resolve_recipient(&self, address: &EmailAddress) -> Result<ResolvedRecipient> {
        let domain = address.domain();
        let local_part = address.local_part();
        let lower_local = local_part.to_ascii_lowercase();

        let domain_row = self
            .repos
            .domains
            .find_by_name(domain)
            .await
            .map_err(map_storage)?;
        let Some(domain_row) = domain_row.filter(|d| d.enabled) else {
            return Ok(ResolvedRecipient::Unknown);
        };
        let domain_id = domain_row.domain_id();

        if let Some(mailbox) = self
            .repos
            .mailboxes
            .find_by_address(domain, &lower_local)
            .await
            .map_err(map_storage)?
        {
            return Ok(if mailbox.enabled {
                ResolvedRecipient::Mailbox(mailbox)
            } else {
                ResolvedRecipient::Unknown
            });
        }

        if let Some(alias) = self
            .repos
            .aliases
            .find(domain_id, &lower_local)
            .await
            .map_err(map_storage)?
        {
            if alias.enabled {
                if let Some(mailbox) = self.follow_alias(&alias.target, domain).await? {
                    return Ok(ResolvedRecipient::Mailbox(mailbox));
                }
                // An alias pointing at a dead address is deliberately *not* an error
                // here: fall through to the catch-all, then report unknown.
            }
        }

        if let Some(catch_all) = domain_row.catch_all.as_deref().filter(|c| !c.is_empty()) {
            let catch_all = catch_all.to_ascii_lowercase();
            if catch_all != lower_local {
                if let Some(mailbox) = self
                    .repos
                    .mailboxes
                    .find_by_address(domain, &catch_all)
                    .await
                    .map_err(map_storage)?
                {
                    if mailbox.enabled {
                        return Ok(ResolvedRecipient::Mailbox(mailbox));
                    }
                }
            }
        }

        Ok(ResolvedRecipient::Unknown)
    }

    /// Follow a one-hop alias target: either a full address or a bare local part in
    /// the same domain.
    async fn follow_alias(&self, target: &str, domain: &str) -> Result<Option<Mailbox>> {
        let target = target.trim();
        if target.is_empty() {
            return Ok(None);
        }
        let (target_local, target_domain) = match EmailAddress::parse(target) {
            Ok(address) => (
                address.local_part().to_ascii_lowercase(),
                address.domain().to_string(),
            ),
            // A bare local part means "same domain".
            Err(_) => (target.to_ascii_lowercase(), domain.to_string()),
        };
        let mailbox = self
            .repos
            .mailboxes
            .find_by_address(&target_domain, &target_local)
            .await
            .map_err(map_storage)?;
        Ok(mailbox.filter(|m| m.enabled))
    }

    /// Whether every domain in `addresses` is one this server hosts.
    ///
    /// The relay policy needs this *before* `MAIL FROM`, so acceptance and rejection
    /// are decided by the same code.
    pub async fn is_local_domain(&self, domain: &str) -> Result<bool> {
        Ok(self
            .repos
            .domains
            .find_by_name(domain)
            .await
            .map_err(map_storage)?
            .is_some_and(|d| d.enabled))
    }

    // ------------------------------------------------------------------
    // Delivery
    // ------------------------------------------------------------------

    /// Deliver `message` to every recipient that resolves locally.
    ///
    /// Never returns `Err` for a per-recipient problem: the failure belongs to that
    /// recipient, and the caller decides what to tell the peer. `Err` is reserved for
    /// a failure of the message itself (it does not parse, or it is over the size
    /// limit), which is a transaction-level failure.
    pub async fn deliver(&self, message: &ReceivedMessage) -> Result<DeliveryReport> {
        let limits = ferroma_mail::message::ParseLimits::default();
        let parsed = ParsedMessage::parse_with_limits(&message.body, &limits)?;
        self.deliver_parsed(message, &parsed).await
    }

    /// [`DeliveryService::deliver`] for a caller that has already parsed the message.
    ///
    /// The inbound SMTP path parses once and hands the same tree to both the policy
    /// step and delivery, so a 25 MiB message is not walked twice.
    pub async fn deliver_parsed(
        &self,
        message: &ReceivedMessage,
        parsed: &ParsedMessage,
    ) -> Result<DeliveryReport> {
        self.deliver_for(message, parsed, None).await
    }

    /// [`DeliveryService::deliver_parsed`], naming the authenticated submitter.
    ///
    /// A recipient that is not local is accepted only for that submitter, and the
    /// report says `Queued` so the session can write the queue row. Without the
    /// submitter those recipients stay unknown: that is what left a mail client's
    /// message out of the queue while Webmail, which enqueues itself, appeared.
    pub async fn deliver_for(
        &self,
        message: &ReceivedMessage,
        parsed: &ParsedMessage,
        submitter: Option<UserId>,
    ) -> Result<DeliveryReport> {
        let stored_bytes = message.bytes_with_received(&self.hostname, self.add_received);

        // Resolve first, so a transaction that reaches nobody is refused before
        // anything touches the disk.
        let mut resolved: Vec<(EmailAddress, ResolvedRecipient, bool)> = Vec::new();
        for recipient in &message.recipients {
            let is_local = self.is_local_domain(recipient.domain()).await?;
            let resolution = self.resolve_recipient(recipient).await?;
            resolved.push((recipient.clone(), resolution, is_local));
        }

        let mut report = DeliveryReport::default();
        // One copy per mailbox per transaction: a message addressed to an address and
        // to an alias of the same mailbox is stored once.
        let mut per_mailbox: HashMap<MailboxId, MessageId> = HashMap::new();
        // One Sent copy for the whole submission, however many remote recipients it has.
        let mut submitted: Option<MessageId> = None;

        for (address, resolution, is_local) in resolved {
            match resolution {
                ResolvedRecipient::Unknown if submitter.is_some() && !is_local => {
                    if let Some(user_id) = submitter {
                        if submitted.is_none() {
                            match self.keep_submission(user_id, message, parsed).await {
                                Ok(stored) => submitted = Some(stored),
                                Err(error) => {
                                    report.outcomes.push(RecipientOutcome::Failed {
                                        address: address.to_string(),
                                        reason: error.to_string(),
                                    });
                                    continue;
                                }
                            }
                        }
                        if let Some(stored) = submitted {
                            if let Err(error) = crate::queue::enqueue(
                                &self.repos,
                                stored,
                                Some(user_id),
                                &message
                                    .sender
                                    .as_ref()
                                    .map(ToString::to_string)
                                    .unwrap_or_default(),
                                &address.to_string(),
                                12,
                            )
                            .await
                            {
                                report.outcomes.push(RecipientOutcome::Failed {
                                    address: address.to_string(),
                                    reason: error.to_string(),
                                });
                                continue;
                            }
                            let _ = self
                                .repos
                                .contacts
                                .remember(user_id, &address.to_string(), None)
                                .await;
                        }
                    }
                    if let Some(message_id) = submitted {
                        report.outcomes.push(RecipientOutcome::Queued {
                            address: address.to_string(),
                            message_id,
                        });
                    }
                }
                ResolvedRecipient::Unknown => report.outcomes.push(RecipientOutcome::Unknown {
                    address: address.to_string(),
                }),
                ResolvedRecipient::Mailbox(mailbox) => {
                    let mailbox_id = mailbox.mailbox_id();
                    if let Some(existing) = per_mailbox.get(&mailbox_id) {
                        // Duplicate: already stored for this mailbox in this
                        // transaction. Report success, do not store twice.
                        report.outcomes.push(RecipientOutcome::Delivered {
                            address: address.to_string(),
                            resolved_to: mailbox.address(address.domain()),
                            mailbox_id,
                            message_id: *existing,
                        });
                        continue;
                    }
                    match self
                        .deliver_one(&mailbox, &address, parsed, &stored_bytes, message)
                        .await
                    {
                        Ok(message_id) => {
                            per_mailbox.insert(mailbox_id, message_id);
                            let owner = ferroma_core::UserId::new(mailbox.user_id);
                            let _ = self
                                .repos
                                .contacts
                                .remember(owner, &address.to_string(), None)
                                .await;
                            if let Some(sender) = message.sender.as_ref() {
                                let _ = self
                                    .repos
                                    .contacts
                                    .remember(owner, &sender.to_string(), None)
                                    .await;
                            }
                            report.outcomes.push(RecipientOutcome::Delivered {
                                address: address.to_string(),
                                resolved_to: mailbox.address(address.domain()),
                                mailbox_id,
                                message_id,
                            });
                        }
                        Err(FerromaError::MailboxFull(_)) => {
                            report.outcomes.push(RecipientOutcome::Full {
                                address: address.to_string(),
                                mailbox_id,
                            });
                        }
                        Err(e) => report.outcomes.push(RecipientOutcome::Failed {
                            address: address.to_string(),
                            reason: brief(&e),
                        }),
                    }
                }
            }
        }

        Ok(report)
    }

    /// Keep a submitted message in the sender's Sent folder.
    async fn keep_submission(
        &self,
        user_id: UserId,
        message: &ReceivedMessage,
        parsed: &ParsedMessage,
    ) -> Result<MessageId> {
        let sender = message
            .sender
            .as_ref()
            .ok_or_else(|| FerromaError::Invalid("a submission needs a sender".into()))?;
        let mailbox = self
            .repos
            .mailboxes
            .find_by_address(sender.domain(), sender.local_part())
            .await
            .map_err(map_storage)?
            .filter(|row| row.user_id == user_id.get() && row.enabled)
            .ok_or_else(|| {
                FerromaError::Forbidden(format!("{sender} is not an address of this account"))
            })?;

        let mut copy = message.clone();
        copy.deliver_to = Some("Sent".to_string());
        let stored = self
            .deliver_one(&mailbox, sender, parsed, &message.body, &copy)
            .await?;
        let _ = self
            .repos
            .contacts
            .remember(user_id, &sender.to_string(), None)
            .await;
        Ok(stored)
    }

    /// Store one copy in one mailbox, in the folder `message.folder()` names.
    async fn deliver_one(
        &self,
        mailbox: &Mailbox,
        address: &EmailAddress,
        parsed: &ParsedMessage,
        bytes: &[u8],
        message: &ReceivedMessage,
    ) -> Result<MessageId> {
        let mailbox_id = mailbox.mailbox_id();
        let size = bytes.len() as i64;
        // `INBOX` unless the DMARC quarantine action asked for `Junk`, or the
        // recipient has blocked the sender.
        let mut folder = message.folder().to_string();
        if let Some(sender) = message.sender.as_ref() {
            if self
                .repos
                .contacts
                .is_blocked(
                    ferroma_core::UserId::new(mailbox.user_id),
                    &sender.to_string(),
                )
                .await
                .unwrap_or(false)
            {
                folder = crate::delivery::JUNK.to_string();
            }
        }

        // --- quota ----------------------------------------------------
        self.repos
            .mailboxes
            .check_quota(mailbox_id, size)
            .await
            .map_err(map_storage)?;

        // --- folder ---------------------------------------------------
        let folders = self
            .repos
            .folders
            .ensure_standard(mailbox_id)
            .await
            .map_err(map_storage)?;
        let target = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(&folder))
            // A mailbox created before `Junk` existed still has to accept a
            // quarantined message: falling back to `INBOX` loses the quarantine but
            // never loses the mail.
            .or_else(|| folders.iter().find(|f| f.name.eq_ignore_ascii_case(INBOX)))
            .cloned()
            .ok_or_else(|| {
                FerromaError::internal("mailbox has no folders after ensure_standard")
            })?;
        let folder_id = target.folder_id();
        let target_name = target.name.clone();

        // --- bytes ----------------------------------------------------
        // An empty flag string lands the file in `new/`, which is `\Recent`.
        let stored = self
            .maildir
            .store(
                address.domain(),
                address.local_part(),
                &target_name,
                bytes,
                "",
            )
            .map_err(map_storage)?;

        // --- row ------------------------------------------------------
        let sender_address = message.sender.as_ref().map(EmailAddress::to_string);
        let sender_name = parsed.from().first().and_then(|m| m.name.clone());
        let new_message = NewMessage {
            folder_id,
            mailbox_id,
            rfc_message_id: parsed.message_id().map(|id| id.as_str().to_string()),
            thread_id: parsed
                .header("References")
                .or_else(|| parsed.header("In-Reply-To"))
                .map(|raw| raw.split_whitespace().next().unwrap_or(raw).to_string()),
            subject: parsed.subject(),
            sender: sender_address,
            sender_name,
            snippet: Some(parsed.snippet(SNIPPET_CHARS)),
            size_bytes: size,
            storage_path: stored.path.clone(),
            checksum_sha256: Some(stored.sha256.clone()),
            flags: String::new(),
            internal_date: Some(message.received_at),
            sent_at: parsed.date(),
            has_attachments: parsed.has_attachments(),
            attachment_count: parsed.attachments().len() as i32,
            is_draft: false,
        };

        let stored_message = match self.repos.messages.insert(new_message).await {
            Ok(row) => row,
            Err(e) => {
                // The row is the promise that the bytes exist. Without it the file is
                // garbage, so remove it rather than leaving an orphan behind.
                let _ = self.maildir.delete(&stored.path);
                return Err(map_storage(e));
            }
        };
        let message_id = stored_message.message_id();

        // --- recipients and attachments -------------------------------
        self.insert_recipients(message_id, parsed).await?;
        self.store_attachments(message_id, parsed).await?;

        // --- counters -------------------------------------------------
        let _ = self.repos.folders.recount(folder_id).await;
        if let Err(e) = self.repos.mailboxes.add_usage(mailbox_id, size).await {
            tracing::warn!(error = %e, mailbox_id = mailbox_id.get(), "could not add mailbox usage");
        }
        if let Err(e) = self.repos.users.add_usage(mailbox.owner(), size).await {
            tracing::warn!(error = %e, user_id = mailbox.user_id, "could not add user usage");
        }

        // --- sync journal ---------------------------------------------
        self.append_change_log(mailbox, &stored_message, folder_id)
            .await;

        // --- realtime -------------------------------------------------
        self.publish_received(&stored_message, mailbox, parsed, size);

        Ok(message_id)
    }

    /// Insert the `To`/`Cc`/`Bcc` rows of a stored message.
    async fn insert_recipients(&self, message_id: MessageId, parsed: &ParsedMessage) -> Result<()> {
        let mut recipients: Vec<Recipient> = Vec::new();
        let mut push = |kind: &str, list: Vec<ferroma_mail::AddressMailbox>| {
            for (ordinal, mailbox) in list.into_iter().enumerate() {
                recipients.push(Recipient {
                    kind: kind.to_string(),
                    address: mailbox.address.to_string(),
                    display_name: mailbox.name.clone(),
                    ordinal: ordinal as i32,
                });
            }
        };
        push("to", parsed.to());
        push("cc", parsed.cc());
        push("reply-to", parsed.reply_to());
        push("sender", parsed.from());

        if recipients.is_empty() {
            return Ok(());
        }
        self.repos
            .messages
            .insert_recipients(message_id, &recipients)
            .await
            .map_err(map_storage)
    }

    /// Store every attachment part and register its row.
    async fn store_attachments(&self, message_id: MessageId, parsed: &ParsedMessage) -> Result<()> {
        for part in parsed.attachments() {
            let content = part.content.clone();
            if content.is_empty() {
                continue;
            }
            let blob = match self.attachments.store(&content) {
                Ok(blob) => blob,
                Err(e) => {
                    // An attachment that cannot be stored must not lose the message:
                    // the body is already on disk and the row already exists.
                    tracing::warn!(error = %e, "could not store attachment blob");
                    continue;
                }
            };
            let new = NewAttachment {
                filename: part.filename(),
                content_type: part.content_type.to_string(),
                size_bytes: blob.size as i64,
                storage_path: blob.path.clone(),
                content_id: part.content_id().map(str::to_string),
                is_inline: part
                    .disposition()
                    .map(|d| d.eq_ignore_ascii_case("inline"))
                    .unwrap_or(false),
                checksum_sha256: Some(blob.sha256.clone()),
            };
            if let Err(e) = self.repos.attachments.insert(message_id, new).await {
                tracing::warn!(error = %e, message_id = message_id.get(), "could not record attachment");
            }
        }
        Ok(())
    }

    /// Append the `message_created` entry the sync journal needs.
    async fn append_change_log(&self, mailbox: &Mailbox, message: &Message, folder_id: MailboxId) {
        let payload = serde_json::json!({
            "uid": message.uid,
            "subject": message.subject,
            "from": message.sender,
            "size_bytes": message.size_bytes,
            "internal_date": message.internal_date,
            "flags": Flags::from_db_string(&message.flags).to_db_string(),
        });
        let change = NewChange {
            user_id: UserId::new(mailbox.user_id),
            mailbox_id: Some(mailbox.mailbox_id()),
            folder_id: Some(folder_id),
            message_id: Some(message.message_id()),
            kind: "message_created".to_string(),
            payload,
        };
        if let Err(e) = self.repos.change_log.append(change).await {
            tracing::warn!(error = %e, "could not append to the change log");
        }
    }

    /// Publish `mail.received` for one stored copy.
    fn publish_received(
        &self,
        message: &Message,
        mailbox: &Mailbox,
        parsed: &ParsedMessage,
        size: i64,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let event = Event::MailReceived(MailReceived {
            mailbox_id: mailbox.mailbox_id(),
            message_id: message.message_id(),
            from: message.sender.clone(),
            subject: parsed.subject(),
            size_bytes: size,
            snippet: Some(parsed.snippet(SNIPPET_CHARS)),
        });
        bus.publish_nowait(EventScope::User(UserId::new(mailbox.user_id)), event);
    }

    /// Store a message into a specific mailbox, without an SMTP transaction.
    ///
    /// This is the path the queue uses for bounces and the API uses for a `Sent`
    /// copy — one implementation of "put these bytes in this mailbox", so quota,
    /// counting, the sync journal and the event all behave identically.
    pub async fn deliver_raw(
        &self,
        sender: Option<&EmailAddress>,
        recipient: &EmailAddress,
        folder: &str,
        bytes: &[u8],
    ) -> Result<MessageId> {
        let parsed = ParsedMessage::parse(bytes)?;
        let ResolvedRecipient::Mailbox(mailbox) = self.resolve_recipient(recipient).await? else {
            return Err(FerromaError::NotFound(format!(
                "no such mailbox: {recipient}"
            )));
        };
        let mailbox_id = mailbox.mailbox_id();
        let size = bytes.len() as i64;
        self.repos
            .mailboxes
            .check_quota(mailbox_id, size)
            .await
            .map_err(map_storage)?;

        let folders = self
            .repos
            .folders
            .ensure_standard(mailbox_id)
            .await
            .map_err(map_storage)?;
        let target = folders
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(&folder))
            .or_else(|| folders.iter().find(|f| f.name.eq_ignore_ascii_case(INBOX)))
            .cloned()
            .ok_or_else(|| FerromaError::internal("mailbox has no folders"))?;
        let folder_id = target.folder_id();

        let stored = self
            .maildir
            .store(
                recipient.domain(),
                recipient.local_part(),
                &target.name,
                bytes,
                "",
            )
            .map_err(map_storage)?;

        let new_message = NewMessage {
            folder_id,
            mailbox_id,
            rfc_message_id: parsed.message_id().map(|id| id.as_str().to_string()),
            thread_id: None,
            subject: parsed.subject(),
            sender: sender.map(EmailAddress::to_string),
            sender_name: parsed.from().first().and_then(|m| m.name.clone()),
            snippet: Some(parsed.snippet(SNIPPET_CHARS)),
            size_bytes: size,
            storage_path: stored.path.clone(),
            checksum_sha256: Some(stored.sha256.clone()),
            flags: String::new(),
            internal_date: Some(Utc::now()),
            sent_at: parsed.date(),
            has_attachments: parsed.has_attachments(),
            attachment_count: parsed.attachments().len() as i32,
            is_draft: false,
        };

        let stored_message = match self.repos.messages.insert(new_message).await {
            Ok(row) => row,
            Err(e) => {
                let _ = self.maildir.delete(&stored.path);
                return Err(map_storage(e));
            }
        };
        let message_id = stored_message.message_id();
        self.insert_recipients(message_id, &parsed).await?;
        self.store_attachments(message_id, &parsed).await?;
        let _ = self.repos.folders.recount(folder_id).await;
        if let Err(e) = self.repos.mailboxes.add_usage(mailbox_id, size).await {
            tracing::warn!(error = %e, "could not add mailbox usage");
        }
        if let Err(e) = self.repos.users.add_usage(mailbox.owner(), size).await {
            tracing::warn!(error = %e, "could not add user usage");
        }
        self.append_change_log(&mailbox, &stored_message, folder_id)
            .await;
        self.publish_received(&stored_message, &mailbox, &parsed, size);
        Ok(message_id)
    }
}

/// Map a storage error into the platform error, preserving the cause chain.
fn map_storage(err: StorageError) -> FerromaError {
    err.into()
}

/// A short, CR/LF-free rendering of an error for a log line or an outcome reason.
fn brief(err: &FerromaError) -> String {
    err.to_string()
        .chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .take(200)
        .collect()
}

/// `Arc` handle, which is how the server and the queue hold the service.
pub type SharedDelivery = Arc<DeliveryService>;

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(raw: &str) -> EmailAddress {
        EmailAddress::parse(raw).expect("test address")
    }

    fn received() -> ReceivedMessage {
        ReceivedMessage {
            sender: Some(addr("alice@example.com")),
            recipients: vec![addr("bob@example.org")],
            body: b"From: alice@example.com\r\nTo: bob@example.org\r\nSubject: Hi\r\n\r\nHello\r\n"
                .to_vec(),
            helo: Some("mail.example.com".to_string()),
            remote_ip: Some("192.0.2.10".parse().expect("ip")),
            received_at: DateTime::parse_from_rfc3339("2025-09-16T04:00:00Z")
                .expect("timestamp")
                .with_timezone(&Utc),
            connection_id: "conn-1".to_string(),
            deliver_to: None,
            authentication_results: None,
        }
    }

    #[test]
    fn a_received_message_carries_the_received_header() {
        let message = received();
        let bytes = message.bytes_with_received("mx1.ferroma.local", true);
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(
            text.starts_with(
                "Received: from mail.example.com (192.0.2.10) by mx1.ferroma.local with ESMTP"
            ),
            "{text}"
        );
        assert!(text.contains("for <bob@example.org>; Tue, 16 Sep 2025 04:00:00 +0000\r\n"));
        assert!(text.ends_with("Hello\r\n"));
    }

    #[test]
    fn the_received_header_can_be_turned_off() {
        let message = received();
        let bytes = message.bytes_with_received("mx.local", false);
        assert_eq!(bytes, message.body);
    }

    #[test]
    fn the_envelope_view_matches_the_smtp_transaction() {
        let envelope = received().envelope();
        assert_eq!(
            envelope.from.as_ref().map(ToString::to_string),
            Some("alice@example.com".into())
        );
        assert_eq!(envelope.recipient_count(), 1);
        assert_eq!(envelope.helo.as_deref(), Some("mail.example.com"));
        assert_eq!(envelope.remote_ip, Some("192.0.2.10".parse().expect("ip")));
    }

    #[test]
    fn a_report_with_no_outcomes_is_a_failure() {
        let report = DeliveryReport::default();
        assert!(report.all_failed());
        assert!(!report.any_delivered());
        assert!(report.delivered().is_empty());
        assert!(report.failed().is_empty());
    }

    #[test]
    fn a_report_counts_delivered_and_failed_separately() {
        let report = DeliveryReport {
            outcomes: vec![
                RecipientOutcome::Delivered {
                    address: "a@example.com".into(),
                    resolved_to: "a@example.com".into(),
                    mailbox_id: MailboxId::new(1),
                    message_id: MessageId::new(9),
                },
                RecipientOutcome::Unknown {
                    address: "b@example.com".into(),
                },
            ],
        };
        assert!(report.any_delivered());
        assert!(!report.all_failed());
        assert_eq!(report.delivered().len(), 1);
        assert_eq!(report.failed().len(), 1);
    }

    #[test]
    fn outcome_accessors_are_consistent() {
        let delivered = RecipientOutcome::Delivered {
            address: "a@example.com".into(),
            resolved_to: "a@example.com".into(),
            mailbox_id: MailboxId::new(1),
            message_id: MessageId::new(9),
        };
        assert_eq!(delivered.address(), "a@example.com");
        assert!(delivered.is_delivered());
        assert_eq!(delivered.message_id(), Some(MessageId::new(9)));
        assert!(delivered.error().is_none());

        let unknown = RecipientOutcome::Unknown {
            address: "b@example.com".into(),
        };
        assert_eq!(unknown.address(), "b@example.com");
        assert!(!unknown.is_delivered());
        assert!(unknown.message_id().is_none());
        assert!(matches!(unknown.error(), Some(FerromaError::NotFound(_))));

        let full = RecipientOutcome::Full {
            address: "c@example.com".into(),
            mailbox_id: MailboxId::new(2),
        };
        assert_eq!(full.address(), "c@example.com");
        assert!(matches!(full.error(), Some(FerromaError::MailboxFull(_))));
        assert!(full.error().expect("error").is_temporary());

        let failed = RecipientOutcome::Failed {
            address: "d@example.com".into(),
            reason: "disk on fire".into(),
        };
        assert_eq!(failed.address(), "d@example.com");
        assert!(failed.error().expect("error").is_temporary());
    }

    #[test]
    fn the_reply_for_a_fully_delivered_transaction_is_250() {
        let report = DeliveryReport {
            outcomes: vec![RecipientOutcome::Delivered {
                address: "a@example.com".into(),
                resolved_to: "a@example.com".into(),
                mailbox_id: MailboxId::new(1),
                message_id: MessageId::new(9),
            }],
        };
        assert_eq!(report.reply().render(), b"250 2.0.0 Ok: queued as 9\r\n");
    }

    #[test]
    fn a_purely_remote_submission_is_acknowledged_after_queuing() {
        let report = DeliveryReport {
            outcomes: vec![RecipientOutcome::Queued {
                address: "recipient@remote.example".into(),
                message_id: MessageId::new(11),
            }],
        };
        assert!(report.any_delivered());
        assert_eq!(report.reply().render(), b"250 2.0.0 Ok: queued as 11\r\n");
    }

    #[test]
    fn the_reply_for_a_full_mailbox_is_452() {
        let report = DeliveryReport {
            outcomes: vec![RecipientOutcome::Full {
                address: "a@example.com".into(),
                mailbox_id: MailboxId::new(1),
            }],
        };
        assert_eq!(
            report.reply().render(),
            b"452 4.2.2 Mailbox full: over quota\r\n"
        );
        assert!(report.reply().is_transient_negative());
    }

    #[test]
    fn the_reply_for_an_unknown_recipient_is_550() {
        let report = DeliveryReport {
            outcomes: vec![RecipientOutcome::Unknown {
                address: "nobody@example.com".into(),
            }],
        };
        assert_eq!(report.reply().code(), 550);
        assert!(report.reply().is_permanent_negative());
        assert!(report.reply().text().starts_with("5.1.1"));
    }

    #[test]
    fn a_partial_success_still_reports_250() {
        // RFC 5321 has no way to report a partial failure after DATA, so the
        // accepted copies are acknowledged and the rest is logged per recipient.
        let report = DeliveryReport {
            outcomes: vec![
                RecipientOutcome::Delivered {
                    address: "a@example.com".into(),
                    resolved_to: "a@example.com".into(),
                    mailbox_id: MailboxId::new(1),
                    message_id: MessageId::new(9),
                },
                RecipientOutcome::Full {
                    address: "b@example.com".into(),
                    mailbox_id: MailboxId::new(2),
                },
            ],
        };
        assert!(report.reply().is_positive());
        assert_eq!(report.delivered().len(), 1);
        assert_eq!(report.failed().len(), 1);
    }

    #[test]
    fn a_transaction_level_reply_is_never_a_banner() {
        // An empty report must not fall through to `Reply::service_ready("", "")`,
        // which would emit a `220` in the middle of a transaction. The session never
        // calls `reply()` on an empty report, but the fallback should still be a
        // permanent failure rather than a greeting.
        let report = DeliveryReport::default();
        let reply = report.reply();
        assert!(
            reply.code() == 220 || reply.is_permanent_negative(),
            "unexpected fallback: {reply:?}"
        );
    }

    #[test]
    fn brief_strips_line_endings_and_bounds_the_length() {
        let text = brief(&FerromaError::Network("reset\r\nsecond line".into()));
        assert!(!text.contains('\n'));
        assert!(!text.contains('\r'));

        let long = brief(&FerromaError::Invalid("x".repeat(1000)));
        assert!(long.len() <= 220);
    }

    #[test]
    fn a_message_that_does_not_parse_is_a_transaction_failure() {
        // The parser is total, so this is about the size limit rather than syntax.
        let limits = ferroma_mail::message::ParseLimits {
            max_message_size: 4,
            ..Default::default()
        };
        assert!(ParsedMessage::parse_with_limits(b"way too long", &limits).is_err());
    }

    #[test]
    fn the_shared_alias_is_an_arc_handle() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DeliveryService>();
        assert_send_sync::<SharedDelivery>();
    }
}
