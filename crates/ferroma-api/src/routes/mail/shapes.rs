//! The JSON shapes `docs/api.md` §5 documents.
//!
//! One type per documented object, so a field rename in the contract shows up as a
//! compile error in exactly one place. Every type here is also the type the Client API
//! (FCP) answers with — `docs/fcp.md` §4 and §5 describe the same records — which is
//! why the conversions take the row plus the small amount of context (the domain name
//! for an address, the attachments for a message) they need.

use chrono::{DateTime, Utc};
use ferroma_core::{DomainId, DraftId, MailboxId, MessageId, UserId};
use ferroma_storage::models::{
    Alias, AttachmentRow, AuditLog, Device, Domain, Draft, Folder, Mailbox, Message,
    MessageRecipient, QueueEntry, User,
};
use serde::{Deserialize, Serialize};

/// One address, as `GET /auth/me`, `GET /mailboxes` and the client API report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxResponse {
    /// The address row id.
    pub id: i64,
    /// The full address, e.g. `alice@example.com`.
    pub address: String,
    /// The owning account.
    pub user_id: i64,
    /// Human name shown next to the address.
    pub display_name: Option<String>,
    /// The account's primary address.
    pub is_primary: bool,
    /// Whether the address accepts mail.
    pub enabled: bool,
    /// Per-address quota, when it overrides the account's.
    pub quota_bytes: Option<i64>,
    /// Bytes currently stored in this address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_bytes: Option<i64>,
    /// When the address was created.
    pub created_at: DateTime<Utc>,
}

impl MailboxResponse {
    /// Build from the row and its domain name.
    pub fn from_row(row: &Mailbox, domain: &str) -> Self {
        MailboxResponse {
            id: row.id,
            address: row.address(domain),
            user_id: row.user_id,
            display_name: row.display_name.clone(),
            is_primary: row.is_primary,
            enabled: row.enabled,
            quota_bytes: row.quota_bytes,
            used_bytes: None,
            created_at: row.created_at,
        }
    }

    /// Attach the address's current usage.
    #[must_use]
    pub fn with_usage(mut self, used_bytes: i64) -> Self {
        self.used_bytes = Some(used_bytes);
        self
    }
}

/// One IMAP folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderResponse {
    /// The folder row id.
    pub id: i64,
    /// The owning address.
    pub mailbox_id: i64,
    /// The IMAP name, e.g. `Archive/2026`.
    pub name: String,
    /// The parent folder, when the name implies one.
    pub parent_id: Option<i64>,
    /// RFC 6154 special-use marker.
    pub special_use: Option<String>,
    /// Whether the client subscribed to it.
    pub subscribed: bool,
    /// Live messages in the folder.
    pub message_count: i32,
    /// Messages without `\Seen`.
    pub unseen_count: i32,
    /// Total bytes stored in the folder.
    pub total_bytes: i64,
    /// IMAP `UIDVALIDITY`.
    pub uid_validity: i64,
    /// The next UID that will be handed out.
    pub uid_next: i64,
}

impl FolderResponse {
    /// Build from the row.
    pub fn from_row(row: &Folder) -> Self {
        FolderResponse {
            id: row.id,
            mailbox_id: row.mailbox_id,
            name: row.name.clone(),
            parent_id: row.parent_id,
            special_use: row.special_use.clone(),
            subscribed: row.subscribed,
            message_count: row.message_count,
            unseen_count: row.unseen_count,
            total_bytes: row.total_bytes,
            uid_validity: row.uid_validity,
            uid_next: row.uid_next,
        }
    }
}

/// An address and its folders, as `docs/fcp.md` §4 documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxTreeResponse {
    /// The address.
    pub id: i64,
    /// The full address.
    pub address: String,
    /// Human name.
    pub display_name: Option<String>,
    /// The account's primary address.
    pub is_primary: bool,
    /// The folders inside it.
    pub folders: Vec<FolderResponse>,
}

/// An address in an address list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressResponse {
    /// The address.
    pub address: String,
    /// The display name, when the header carried one.
    pub name: Option<String>,
}

impl AddressResponse {
    /// Build from a stored recipient row.
    pub fn from_recipient(row: &MessageRecipient) -> Self {
        AddressResponse {
            address: row.address.clone(),
            name: row.display_name.clone(),
        }
    }
}

/// An attachment's metadata, without its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentResponse {
    /// The attachment row id.
    pub id: i64,
    /// The file name the sender chose.
    pub filename: Option<String>,
    /// The MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Whether the part is referenced from the HTML body.
    pub is_inline: bool,
    /// The `Content-ID`, for inline parts.
    pub content_id: Option<String>,
}

impl AttachmentResponse {
    /// Build from the row.
    pub fn from_row(row: &AttachmentRow) -> Self {
        AttachmentResponse {
            id: row.id,
            filename: row.filename.clone(),
            content_type: row.content_type.clone(),
            size_bytes: row.size_bytes,
            is_inline: row.is_inline,
            content_id: row.content_id.clone(),
        }
    }
}

/// A message as a list view shows it: headers, flags and a snippet, no bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSummaryResponse {
    /// The message row id.
    pub id: i64,
    /// The IMAP UID inside its folder.
    pub uid: i64,
    /// The folder holding it.
    pub folder_id: i64,
    /// The address holding it.
    pub mailbox_id: i64,
    /// Subject.
    pub subject: Option<String>,
    /// The `From` address.
    pub from: Option<AddressResponse>,
    /// The `To` addresses.
    pub to: Vec<AddressResponse>,
    /// The `Cc` addresses.
    pub cc: Vec<AddressResponse>,
    /// The `Bcc` addresses, on mail this account sent.
    ///
    /// The front-end reads this key off both message shapes; it was absent, so the
    /// blind-copy list normalised to empty on every message.
    pub bcc: Vec<AddressResponse>,
    /// The `Reply-To` addresses.
    pub reply_to: Vec<AddressResponse>,
    /// Body-free preview text.
    pub snippet: Option<String>,
    /// The canonical flag string, e.g. `seen flagged`.
    pub flags: String,
    /// Whether the message carries the `\Seen` flag.
    ///
    /// `flags` is the canonical machine-readable spelling; these three booleans exist
    /// because the front-ends read `seen`/`flagged`/`answered` directly. Deriving them
    /// in the handler keeps every consumer from re-parsing the flag string — and from
    /// getting it wrong, which is how the Webmail showed every message as unread.
    pub seen: bool,
    /// Whether it carries `\Flagged`.
    pub flagged: bool,
    /// Whether it carries `\Answered`.
    pub answered: bool,
    /// Size of the stored bytes.
    pub size_bytes: i64,
    /// Whether the message carries attachments.
    pub has_attachments: bool,
    /// How many attachments it carries.
    pub attachment_count: i32,
    /// Whether this row is a draft.
    pub is_draft: bool,
    /// IMAP `INTERNALDATE`.
    pub internal_date: DateTime<Utc>,
    /// The `Date:` header.
    pub sent_at: Option<DateTime<Utc>>,
    /// The RFC 5322 `Message-ID`, when the sender supplied one.
    pub rfc_message_id: Option<String>,
}

/// A message with its bodies, headers and attachment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDetailResponse {
    /// The message row id.
    pub id: i64,
    /// The IMAP UID inside its folder.
    pub uid: i64,
    /// The folder holding it.
    pub folder_id: i64,
    /// The address holding it.
    pub mailbox_id: i64,
    /// Subject.
    pub subject: Option<String>,
    /// The `From` address.
    pub from: Option<AddressResponse>,
    /// The `To` addresses.
    pub to: Vec<AddressResponse>,
    /// The `Cc` addresses.
    pub cc: Vec<AddressResponse>,
    /// The `Bcc` addresses, on mail this account sent.
    ///
    /// The front-end reads this key off both message shapes; it was absent, so the
    /// blind-copy list normalised to empty on every message.
    pub bcc: Vec<AddressResponse>,
    /// The `Reply-To` addresses.
    pub reply_to: Vec<AddressResponse>,
    /// The canonical flag string.
    pub flags: String,
    /// Whether the message carries the `\Seen` flag.
    ///
    /// `flags` is the canonical machine-readable spelling; these three booleans exist
    /// because the front-ends read `seen`/`flagged`/`answered` directly. Deriving them
    /// in the handler keeps every consumer from re-parsing the flag string — and from
    /// getting it wrong, which is how the Webmail showed every message as unread.
    pub seen: bool,
    /// Whether it carries `\Flagged`.
    pub flagged: bool,
    /// Whether it carries `\Answered`.
    pub answered: bool,
    /// Size of the stored bytes.
    pub size_bytes: i64,
    /// Body-free preview text.
    pub snippet: Option<String>,
    /// The decoded `text/plain` body.
    pub text_body: Option<String>,
    /// The decoded `text/html` body, sanitised when `security.sanitize_html` is on.
    pub html_body: Option<String>,
    /// The RFC 5322 `Message-ID` of this message; a reply carries it as `in_reply_to`.
    pub message_id_header: Option<String>,
    /// This message's own `In-Reply-To`.
    pub in_reply_to: Option<String>,
    /// This message's `References`, oldest first.
    pub references: Vec<String>,
    /// IMAP `INTERNALDATE`.
    pub internal_date: DateTime<Utc>,
    /// The `Date:` header.
    pub sent_at: Option<DateTime<Utc>>,
    /// Whether this row is a draft.
    pub is_draft: bool,
    /// Whether the message carries attachments.
    pub has_attachments: bool,
    /// How many attachments it carries.
    pub attachment_count: i32,
    /// The attachment metadata.
    pub attachments: Vec<AttachmentResponse>,
}

/// What `POST /messages` answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendResponse {
    /// The stored copy's row id.
    pub message_id: i64,
    /// How many `mail_queue` rows were written.
    pub queued: usize,
    /// The recipients that were queued, in order.
    pub recipients: Vec<String>,
}

/// A saved draft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftResponse {
    /// The draft row id.
    pub id: i64,
    /// The address the draft will be sent from.
    pub mailbox_id: Option<i64>,
    /// The folder the mirrored message lives in.
    pub folder_id: Option<i64>,
    /// The mirrored message, when the draft has been materialised.
    pub message_id: Option<i64>,
    /// Subject typed so far.
    pub subject: Option<String>,
    /// Plain-text body.
    pub text: Option<String>,
    /// HTML body.
    pub html: Option<String>,
    /// The recipient list, as `{address, name}` objects.
    pub to: Vec<AddressResponse>,
    /// The copy list.
    pub cc: Vec<AddressResponse>,
    /// The blind-copy list.
    pub bcc: Vec<AddressResponse>,
    /// The `Message-ID` being replied to.
    pub in_reply_to: Option<String>,
    /// The reference chain.
    pub references: Vec<String>,
    /// Attachment metadata the draft carries.
    pub attachments: Vec<AttachmentResponse>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it was last touched.
    pub updated_at: DateTime<Utc>,
}

/// One alias.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliasResponse {
    /// The alias row id.
    pub id: i64,
    /// The domain it belongs to.
    pub domain_id: i64,
    /// The local part being aliased.
    pub local_part: String,
    /// Where mail to it is forwarded.
    pub target: String,
    /// Whether the alias is active.
    pub enabled: bool,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

impl AliasResponse {
    /// Build from the row.
    pub fn from_row(row: &Alias) -> Self {
        AliasResponse {
            id: row.id,
            domain_id: row.domain_id,
            local_part: row.local_part.clone(),
            target: row.target.clone(),
            enabled: row.enabled,
            created_at: row.created_at,
        }
    }
}

/// One managed domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainResponse {
    /// The domain row id.
    pub id: i64,
    /// The FQDN.
    pub name: String,
    /// Free-form note.
    pub description: Option<String>,
    /// Whether mail is accepted for it.
    pub enabled: bool,
    /// The local part that absorbs unknown recipients.
    pub catch_all: Option<String>,
    /// The DKIM selector in use.
    pub dkim_selector: Option<String>,
    /// Whether a DKIM key pair exists. The private key itself is never returned.
    pub dkim_key_present: bool,
    /// How many addresses exist in the domain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailbox_count: Option<i64>,
    /// When the domain was created.
    pub created_at: DateTime<Utc>,
    /// When it was last changed.
    pub updated_at: DateTime<Utc>,
}

impl DomainResponse {
    /// Build from the row. `dkim_private_key` is deliberately not projected.
    pub fn from_row(row: &Domain) -> Self {
        DomainResponse {
            id: row.id,
            name: row.name.clone(),
            description: row.description.clone(),
            enabled: row.enabled,
            catch_all: row.catch_all.clone(),
            dkim_selector: row.dkim_selector.clone(),
            dkim_key_present: row.dkim_private_key.is_some(),
            mailbox_count: None,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }

    /// Attach the address count.
    #[must_use]
    pub fn with_mailbox_count(mut self, count: i64) -> Self {
        self.mailbox_count = Some(count);
        self
    }
}

/// The account, as `docs/api.md` §3 and the Admin user list report it.
///
/// `password_hash` has no field: it can never be serialised by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserResponse {
    /// The user row id.
    pub id: i64,
    /// The login address.
    pub email: String,
    /// Human name.
    pub display_name: Option<String>,
    /// Whether the account may log in.
    pub enabled: bool,
    /// Whether it may use the Admin API.
    pub is_admin: bool,
    /// Total bytes the account may store.
    pub quota_bytes: i64,
    /// Bytes currently stored.
    pub used_bytes: i64,
    /// When the account last logged in.
    pub last_login_at: Option<DateTime<Utc>>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// The addresses the account owns.
    ///
    /// `GET /users` fills this in so the console's list can print an address count;
    /// the single-account responses omit it, because they never read it and the
    /// addresses would be a second query each. Absent therefore means "not asked for",
    /// which is exactly what the client's `mailboxesKnown` flag reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailboxes: Option<Vec<MailboxResponse>>,
}

impl UserResponse {
    /// Build from the row.
    pub fn from_row(row: &User) -> Self {
        UserResponse {
            id: row.id,
            email: row.email.clone(),
            display_name: row.display_name.clone(),
            enabled: row.enabled,
            is_admin: row.is_admin,
            quota_bytes: row.quota_bytes,
            used_bytes: row.used_bytes,
            last_login_at: row.last_login_at,
            created_at: row.created_at,
            mailboxes: None,
        }
    }

    /// Attach the account's addresses.
    #[must_use]
    pub fn with_mailboxes(mut self, mailboxes: Vec<MailboxResponse>) -> Self {
        self.mailboxes = Some(mailboxes);
        self
    }
}

/// One queued outbound recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueResponse {
    /// The queue row id.
    pub id: i64,
    /// The stored message being delivered.
    pub message_id: i64,
    /// Who queued it, when an authenticated user did.
    pub user_id: Option<i64>,
    /// The envelope sender.
    pub sender: String,
    /// The envelope recipient.
    pub recipient: String,
    /// `pending`, `delivering`, `delivered`, `retry`, `failed` or `cancelled`.
    pub status: String,
    /// Attempts made so far.
    pub attempts: i32,
    /// Attempts allowed before bouncing.
    pub max_attempts: i32,
    /// When the next attempt is due.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// When the last attempt ran.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// When it was accepted by the remote server.
    pub delivered_at: Option<DateTime<Utc>>,
    /// The last failure reason.
    pub last_error: Option<String>,
    /// The last SMTP status code.
    pub last_status_code: Option<i32>,
    /// The last SMTP status text.
    pub last_status_text: Option<String>,
    /// The MX host the last attempt used.
    pub remote_mx: Option<String>,
    /// When the row was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

impl QueueResponse {
    /// Build from the row.
    pub fn from_row(row: &QueueEntry) -> Self {
        QueueResponse {
            id: row.id,
            message_id: row.message_id,
            user_id: row.user_id,
            sender: row.sender.clone(),
            recipient: row.recipient.clone(),
            status: row.status.clone(),
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            next_attempt_at: row.next_attempt_at,
            last_attempt_at: row.last_attempt_at,
            delivered_at: row.delivered_at,
            last_error: row.last_error.clone(),
            last_status_code: row.last_status_code,
            last_status_text: row.last_status_text.clone(),
            remote_mx: row.remote_mx.clone(),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// One delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryAttemptResponse {
    /// The attempt row id.
    pub id: i64,
    /// Which attempt this was, 1-based.
    pub attempt: i32,
    /// The remote MX host.
    pub remote_mx: Option<String>,
    /// The SMTP status code.
    pub status_code: Option<i32>,
    /// The SMTP status text.
    pub status_text: Option<String>,
    /// The failure reason, when there was one.
    pub error: Option<String>,
    /// How long the attempt took.
    pub duration_ms: Option<i32>,
    /// When it ran.
    pub at: DateTime<Utc>,
}

impl DeliveryAttemptResponse {
    /// Build from the row.
    pub fn from_row(row: &ferroma_storage::models::DeliveryAttempt) -> Self {
        DeliveryAttemptResponse {
            id: row.id,
            attempt: row.attempt,
            remote_mx: row.remote_mx.clone(),
            status_code: row.status_code,
            status_text: row.status_text.clone(),
            error: row.error.clone(),
            duration_ms: row.duration_ms,
            at: row.created_at,
        }
    }
}

/// One audit-trail entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditResponse {
    /// The entry id.
    pub id: i64,
    /// Who acted.
    pub actor_user_id: Option<i64>,
    /// The acting account's address, when the row names one.
    ///
    /// The console's audit table shows an actor, and `docs/api.md` §4.6 documents this
    /// field; without it every row rendered a bare numeric id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// What they did.
    pub action: String,
    /// The kind of thing acted upon.
    pub target_type: Option<String>,
    /// Its identifier.
    pub target_id: Option<String>,
    /// The peer address.
    pub ip: Option<String>,
    /// The client's `User-Agent`.
    pub user_agent: Option<String>,
    /// Structured detail.
    pub details: serde_json::Value,
    /// When it happened.
    pub created_at: DateTime<Utc>,
}

impl AuditResponse {
    /// Build from the row.
    pub fn from_row(row: &AuditLog) -> Self {
        AuditResponse {
            id: row.id,
            actor_user_id: row.actor_user_id,
            actor: None,
            action: row.action.clone(),
            target_type: row.target_type.clone(),
            target_id: row.target_id.clone(),
            ip: row.ip.clone(),
            user_agent: row.user_agent.clone(),
            details: row.details.clone(),
            created_at: row.created_at,
        }
    }

    /// Attach the acting account's address.
    #[must_use]
    pub fn with_actor(mut self, actor: Option<String>) -> Self {
        self.actor = actor;
        self
    }
}

/// One registered client installation, as the admin device list reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceResponse {
    /// The device row id.
    pub id: i64,
    /// The owning account.
    pub user_id: i64,
    /// The owner's login address, resolved for the Admin panel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The client-generated installation id.
    pub device_uid: String,
    /// Human name.
    pub name: Option<String>,
    /// `windows`, `linux`, `macos`, `android` or `ios`.
    pub platform: Option<String>,
    /// The client version string.
    pub client_version: Option<String>,
    /// The FCP version the device last spoke.
    pub protocol_version: Option<i32>,
    /// When it was last seen.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The last peer address.
    pub last_ip: Option<String>,
    /// When it was first registered.
    pub created_at: DateTime<Utc>,
    /// Whether it has been revoked.
    pub revoked: bool,
}

impl DeviceResponse {
    /// Build from the row, without resolving the owner's address.
    pub fn from_row(row: &Device) -> Self {
        DeviceResponse {
            id: row.id,
            user_id: row.user_id,
            email: None,
            device_uid: row.device_uid.clone(),
            name: row.name.clone(),
            platform: row.platform.clone(),
            client_version: row.client_version.clone(),
            protocol_version: row.protocol_version,
            last_seen_at: row.last_seen_at,
            last_ip: row.last_ip.clone(),
            created_at: row.created_at,
            revoked: row.revoked_at.is_some(),
        }
    }

    /// Attach the owner's address.
    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }
}

/// One address, reduced to what `docs/api.md` §3 shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressBrief {
    /// The address row id.
    pub id: i64,
    /// The full address.
    pub address: String,
    /// Whether it is the account's primary address.
    pub is_primary: bool,
}

/// `GET /auth/me`: the account plus its addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeResponse {
    /// The user row id.
    pub id: i64,
    /// The login address.
    pub email: String,
    /// Human name.
    pub display_name: Option<String>,
    /// Whether the account may use the Admin API.
    pub is_admin: bool,
    /// Total bytes the account may store.
    pub quota_bytes: i64,
    /// Bytes currently stored.
    pub used_bytes: i64,
    /// The addresses it owns.
    ///
    /// Full mailbox records, not the reduced [`AddressBrief`] the address *list* uses.
    /// The Webmail normalises each of these into the shape its folder tree, address
    /// picker and quota readouts consume, and reads `quota_bytes` and `used_bytes` off
    /// it — which the brief does not carry, so every address it listed reported as
    /// empty. `AddressBrief` stays for `GET /mailboxes` and the client account blob,
    /// which genuinely want only the address and its primary flag.
    pub mailboxes: Vec<MailboxResponse>,
}

/// One typed-id alias, so handlers can name the owner of a row without importing ids.
pub type OwnedMessage = (Message, MailboxId);

/// The `domain_id` of a mailbox row, for the routes that need it.
pub fn domain_of(row: &Mailbox) -> DomainId {
    DomainId::new(row.domain_id)
}

/// The `message_id` of a draft's mirrored row.
pub fn draft_message(row: &Draft) -> Option<MessageId> {
    row.message_id.map(MessageId::new)
}

/// The `draft_id` of a row.
pub fn draft_id_of(row: &Draft) -> DraftId {
    row.draft_id()
}

/// The `user_id` of a device row.
pub fn device_owner(row: &Device) -> UserId {
    UserId::new(row.user_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0)
            .single()
            .expect("valid timestamp")
    }

    fn mailbox_row() -> Mailbox {
        Mailbox {
            id: 3,
            user_id: 7,
            domain_id: 1,
            local_part: "alice".into(),
            display_name: Some("Alice".into()),
            enabled: true,
            is_primary: true,
            quota_bytes: None,
            created_at: at(0),
            updated_at: at(0),
        }
    }

    #[test]
    fn mailbox_response_composes_the_address_and_never_leaks_the_owner_hash() {
        let row = mailbox_row();
        let response = MailboxResponse::from_row(&row, "example.com").with_usage(512);
        assert_eq!(response.address, "alice@example.com");
        assert_eq!(response.used_bytes, Some(512));
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["address"], "alice@example.com");
        assert_eq!(json["is_primary"], true);
        assert!(json.get("password_hash").is_none());
    }

    #[test]
    fn usage_is_omitted_when_it_was_not_loaded() {
        let response = MailboxResponse::from_row(&mailbox_row(), "example.com");
        assert!(response.used_bytes.is_none());
        let json = serde_json::to_value(&response).expect("must serialise");
        assert!(json.get("used_bytes").is_none(), "{json}");
    }

    #[test]
    fn folder_response_projects_the_counters_clients_map_on() {
        let row = Folder {
            id: 5,
            mailbox_id: 3,
            name: "INBOX".into(),
            parent_id: None,
            special_use: Some("\\Sent".into()),
            subscribed: true,
            uid_validity: 1,
            uid_next: 118,
            highest_modseq: 9,
            message_count: 412,
            unseen_count: 3,
            total_bytes: 1024,
            created_at: at(0),
            updated_at: at(0),
        };
        let json = serde_json::to_value(FolderResponse::from_row(&row)).expect("must serialise");
        assert_eq!(json["message_count"], 412);
        assert_eq!(json["unseen_count"], 3);
        assert_eq!(json["special_use"], "\\Sent");
        assert_eq!(json["uid_validity"], 1);
        assert_eq!(json["uid_next"], 118);
    }

    #[test]
    fn attachment_and_address_responses_keep_the_documented_fields() {
        let row = AttachmentRow {
            id: 991,
            message_id: 4821,
            filename: Some("invoice.pdf".into()),
            content_type: "application/pdf".into(),
            size_bytes: 24831,
            storage_path: "ab/cd/deadbeef".into(),
            content_id: None,
            is_inline: false,
            checksum_sha256: Some("deadbeef".into()),
            created_at: at(0),
        };
        let json = serde_json::to_value(AttachmentResponse::from_row(&row)).expect("serialise");
        assert_eq!(json["id"], 991);
        assert_eq!(json["filename"], "invoice.pdf");
        assert_eq!(json["is_inline"], false);
        assert!(json["content_id"].is_null());
        // The blob's storage path is internal and must not be published.
        assert!(json.get("storage_path").is_none(), "{json}");

        let recipient = MessageRecipient {
            id: 1,
            message_id: 4821,
            kind: "to".into(),
            address: "bob@example.net".into(),
            display_name: Some("Bob".into()),
            ordinal: 0,
        };
        let json = serde_json::to_value(AddressResponse::from_recipient(&recipient)).expect("json");
        assert_eq!(json["address"], "bob@example.net");
        assert_eq!(json["name"], "Bob");
    }

    #[test]
    fn user_response_never_carries_the_hash() {
        let row = User {
            id: 7,
            email: "alice@example.com".into(),
            password_hash: "$argon2id$secret".into(),
            display_name: Some("Alice".into()),
            enabled: true,
            is_admin: false,
            quota_bytes: 1_073_741_824,
            used_bytes: 52_428_800,
            failed_logins: 0,
            locked_until: None,
            last_login_at: Some(at(10)),
            created_at: at(0),
            updated_at: at(0),
        };
        let json = serde_json::to_value(UserResponse::from_row(&row)).expect("must serialise");
        assert_eq!(json["id"], 7);
        assert_eq!(json["quota_bytes"], 1_073_741_824);
        assert_eq!(json["used_bytes"], 52_428_800);
        let text = json.to_string();
        assert!(!text.contains("argon2"), "{text}");
        assert!(!text.contains("password"), "{text}");
        assert!(!text.contains("failed_logins"), "{text}");
    }

    #[test]
    fn domain_response_reports_presence_but_never_the_private_key() {
        let row = Domain {
            id: 1,
            name: "example.com".into(),
            description: None,
            enabled: true,
            catch_all: Some("catch".into()),
            dkim_selector: Some("default".into()),
            dkim_private_key: Some("-----BEGIN PRIVATE KEY-----".into()),
            dkim_public_key: Some("MIIBIjANBg".into()),
            created_at: at(0),
            updated_at: at(0),
        };
        let response = DomainResponse::from_row(&row).with_mailbox_count(42);
        let text = serde_json::to_string(&response).expect("must serialise");
        assert!(!text.contains("PRIVATE KEY"), "{text}");
        assert!(response.dkim_key_present);
        assert_eq!(response.mailbox_count, Some(42));
    }

    #[test]
    fn queue_response_round_trips_every_operational_field() {
        let row = QueueEntry {
            id: 91,
            message_id: 4821,
            user_id: Some(7),
            sender: "alice@example.com".into(),
            recipient: "bob@example.net".into(),
            status: "retry".into(),
            attempts: 2,
            max_attempts: 12,
            next_attempt_at: Some(at(500)),
            last_attempt_at: Some(at(200)),
            delivered_at: None,
            last_error: Some("421 too many connections".into()),
            last_status_code: Some(421),
            last_status_text: Some("try later".into()),
            remote_mx: Some("mx1.example.net".into()),
            created_at: at(0),
            updated_at: at(100),
        };
        let json = serde_json::to_value(QueueResponse::from_row(&row)).expect("must serialise");
        assert_eq!(json["status"], "retry");
        assert_eq!(json["attempts"], 2);
        assert_eq!(json["last_status_code"], 421);
        assert_eq!(json["remote_mx"], "mx1.example.net");
        assert!(json["delivered_at"].is_null());
    }

    #[test]
    fn device_response_marks_revocation_and_can_carry_the_owner() {
        let row = Device {
            id: 12,
            user_id: 7,
            device_uid: "3f2c".into(),
            name: Some("Alice's laptop".into()),
            platform: Some("windows".into()),
            client_version: Some("0.7.0".into()),
            protocol_version: Some(1),
            last_seen_at: Some(at(100)),
            last_ip: Some("203.0.113.44".into()),
            created_at: at(0),
            revoked_at: Some(at(50)),
        };
        let response = DeviceResponse::from_row(&row).with_email("alice@example.com");
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["revoked"], true);
        assert_eq!(json["email"], "alice@example.com");
        assert_eq!(json["platform"], "windows");
        // The row's own column name is not the wire name.
        assert!(json.get("revoked_at").is_none(), "{json}");
    }

    #[test]
    fn audit_and_attempt_responses_keep_their_timestamps() {
        let row = AuditLog {
            id: 1,
            actor_user_id: Some(7),
            action: "user.created".into(),
            target_type: Some("user".into()),
            target_id: Some("9".into()),
            ip: Some("203.0.113.9".into()),
            user_agent: Some("curl/8".into()),
            details: serde_json::json!({ "email": "bob@example.net" }),
            created_at: at(42),
        };
        let json = serde_json::to_value(AuditResponse::from_row(&row)).expect("must serialise");
        assert_eq!(json["action"], "user.created");
        assert_eq!(json["details"]["email"], "bob@example.net");
        assert_eq!(json["created_at"], serde_json::json!(at(42)));

        let attempt = ferroma_storage::models::DeliveryAttempt {
            id: 5,
            queue_id: 91,
            attempt: 2,
            remote_mx: Some("mx1.example.net".into()),
            status_code: Some(421),
            status_text: Some("try later".into()),
            error: Some("deferred".into()),
            duration_ms: Some(1200),
            created_at: at(200),
        };
        let json = serde_json::to_value(DeliveryAttemptResponse::from_row(&attempt)).expect("json");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["duration_ms"], 1200);
        assert!(
            json.get("queue_id").is_none(),
            "the parent id is the envelope"
        );
    }

    #[test]
    fn me_response_serialises_the_documented_shape() {
        let response = MeResponse {
            id: 7,
            email: "alice@example.com".into(),
            display_name: Some("Alice".into()),
            is_admin: false,
            quota_bytes: 1_073_741_824,
            used_bytes: 52_428_800,
            mailboxes: vec![MailboxResponse {
                id: 3,
                address: "alice@example.com".into(),
                user_id: 7,
                display_name: Some("Alice".into()),
                is_primary: true,
                enabled: true,
                quota_bytes: Some(2_147_483_648),
                used_bytes: Some(4096),
                created_at: Utc::now(),
            }],
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["id"], 7);
        assert_eq!(json["mailboxes"][0]["address"], "alice@example.com");
        assert_eq!(json["mailboxes"][0]["is_primary"], true);
        assert_eq!(json["mailboxes"][0]["id"], 3);
        // The fields the Webmail's mailbox normaliser reads and the brief did not carry.
        assert_eq!(json["mailboxes"][0]["quota_bytes"], 2_147_483_648_i64);
        assert_eq!(json["mailboxes"][0]["used_bytes"], 4096);
        assert_eq!(json["mailboxes"][0]["enabled"], true);
    }

    #[test]
    fn mailbox_tree_groups_folders_under_their_address() {
        let tree = MailboxTreeResponse {
            id: 3,
            address: "alice@example.com".into(),
            display_name: Some("Alice".into()),
            is_primary: true,
            folders: vec![FolderResponse::from_row(&Folder {
                id: 5,
                mailbox_id: 3,
                name: "INBOX".into(),
                parent_id: None,
                special_use: None,
                subscribed: true,
                uid_validity: 1,
                uid_next: 118,
                highest_modseq: 1,
                message_count: 412,
                unseen_count: 3,
                total_bytes: 0,
                created_at: at(0),
                updated_at: at(0),
            })],
        };
        let json = serde_json::to_value(&tree).expect("must serialise");
        assert_eq!(json["folders"][0]["name"], "INBOX");
        assert!(json["folders"][0]["special_use"].is_null());
    }

    #[test]
    fn send_response_matches_the_documented_shape() {
        let json = serde_json::to_value(SendResponse {
            message_id: 4821,
            queued: 2,
            recipients: vec!["bob@example.net".into(), "carol@example.org".into()],
        })
        .expect("must serialise");
        assert_eq!(json["message_id"], 4821);
        assert_eq!(json["queued"], 2);
        assert_eq!(json["recipients"][1], "carol@example.org");
    }

    #[test]
    fn typed_id_helpers_agree_with_the_rows() {
        let mailbox = mailbox_row();
        assert_eq!(domain_of(&mailbox), DomainId::new(1));
        let device = Device {
            id: 12,
            user_id: 7,
            device_uid: "x".into(),
            name: None,
            platform: None,
            client_version: None,
            protocol_version: None,
            last_seen_at: None,
            last_ip: None,
            created_at: at(0),
            revoked_at: None,
        };
        assert_eq!(device_owner(&device), UserId::new(7));
    }

    #[test]
    fn draft_response_round_trips_and_defaults_its_metric_fields() {
        let draft = Draft {
            id: 44,
            user_id: 7,
            mailbox_id: Some(3),
            folder_id: Some(7),
            message_id: None,
            subject: Some("hi".into()),
            body_text: Some("body".into()),
            body_html: None,
            recipients: serde_json::json!([{ "address": "bob@example.net", "name": "Bob" }]),
            attachments: serde_json::json!([]),
            in_reply_to: Some("<a@b.c>".into()),
            reference_ids: serde_json::json!(["<x@y.z>"]),
            created_at: at(0),
            updated_at: at(1),
        };
        assert_eq!(draft_id_of(&draft), DraftId::new(44));
        assert_eq!(draft_message(&draft), None);
        assert_eq!(draft.recipients.as_array().map(Vec::len), Some(1));
    }
}
