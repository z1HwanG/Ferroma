//! Row images of the Ferroma schema.
//!
//! These structs mirror tables one-to-one and derive [`sqlx::FromRow`] so queries
//! stay runtime-checked (no `query!` macros, which would make the build depend on a
//! live database).
//!
//! # Identifiers
//!
//! Row structs carry **raw `i64`** primary and foreign keys, because a row image is
//! a direct projection of the table — including `Option<i64>` columns that cannot
//! carry the typed newtypes. Typed identifiers ([`ferroma_core::UserId`] and
//! friends) are the currency of the *repository* API, which converts at the
//! boundary. Callers therefore get type safety where it matters and a predictable
//! row shape where they need the raw data.
//!
//! # Timestamps
//!
//! Every timestamp is `TIMESTAMPTZ` mapped to `chrono::DateTime<Utc>`.
//!
//! # Documentation
//!
//! `missing_docs` is allowed in this module: these structs are a mechanical mirror
//! of `migrations/0001_initial.sql`, column for column. The schema *is* the
//! documentation — every column there carries a comment explaining its meaning and
//! its constraints. Adding a second, drifting copy of that prose here would be worse
//! than useless. Hand-written logic (the helper `impl` blocks below) is documented
//! and tested as usual.
#![allow(missing_docs)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A login identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct User {
    /// Primary key.
    pub id: i64,
    /// Login address, always lower-case.
    pub email: String,
    /// Argon2id PHC string. Never logged, never serialised into API responses.
    pub password_hash: String,
    /// Optional human name.
    pub display_name: Option<String>,
    /// `false` disables every way of logging in, without deleting data.
    pub enabled: bool,
    /// Grants access to the Admin API.
    pub is_admin: bool,
    /// Total bytes this user's mail may occupy.
    pub quota_bytes: i64,
    /// Cached sum of stored message sizes.
    pub used_bytes: i64,
    /// Consecutive failed logins since the last success.
    pub failed_logins: i32,
    /// Login is refused until this instant (temporary lockout).
    pub locked_until: Option<DateTime<Utc>>,
    /// Last successful login.
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl User {
    /// Typed primary key.
    pub fn user_id(&self) -> ferroma_core::UserId {
        ferroma_core::UserId::new(self.id)
    }

    /// Whether the account may log in right now.
    pub fn is_login_allowed(&self, now: DateTime<Utc>) -> bool {
        self.enabled && self.locked_until.is_none_or(|until| until <= now)
    }

    /// Remaining quota in bytes (never negative).
    pub fn quota_remaining(&self) -> i64 {
        (self.quota_bytes - self.used_bytes).max(0)
    }
}

/// A mail domain managed by this server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Domain {
    pub id: i64,
    /// Lower-case FQDN, e.g. `example.com`.
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    /// Local part that absorbs mail for unknown recipients in this domain.
    pub catch_all: Option<String>,
    /// DKIM selector published at `<selector>._domainkey.<name>`.
    pub dkim_selector: Option<String>,
    /// DKIM private key material (PEM). Excluded from API responses.
    pub dkim_private_key: Option<String>,
    /// The matching public key, for the DNS TXT record the Admin panel shows.
    pub dkim_public_key: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Domain {
    /// Typed primary key.
    pub fn domain_id(&self) -> ferroma_core::DomainId {
        ferroma_core::DomainId::new(self.id)
    }
}

/// An address such as `alice@example.com` — the SMTP delivery target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Mailbox {
    pub id: i64,
    pub user_id: i64,
    pub domain_id: i64,
    /// Lower-case local part.
    pub local_part: String,
    pub display_name: Option<String>,
    pub enabled: bool,
    /// The user's primary address; exactly one per user.
    pub is_primary: bool,
    /// Per-address quota. `None` inherits the owner's quota.
    pub quota_bytes: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Mailbox {
    /// Typed primary key.
    pub fn mailbox_id(&self) -> ferroma_core::MailboxId {
        ferroma_core::MailboxId::new(self.id)
    }

    /// Typed owner.
    pub fn owner(&self) -> ferroma_core::UserId {
        ferroma_core::UserId::new(self.user_id)
    }

    /// The full address, given the domain name it belongs to.
    pub fn address(&self, domain: &str) -> String {
        format!("{}@{}", self.local_part, domain)
    }
}

/// A forwarding alias: `sales@example.com` -> `alice@example.com`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Alias {
    pub id: i64,
    pub domain_id: i64,
    pub local_part: String,
    /// Destination address, or a bare local part meaning "same domain".
    pub target: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// An IMAP folder inside a mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Folder {
    pub id: i64,
    pub mailbox_id: i64,
    /// IMAP name, e.g. `INBOX`, `Sent`, `Archive/2026`.
    pub name: String,
    pub parent_id: Option<i64>,
    /// `\Sent`, `\Drafts`, `\Trash`, `\Junk`, `\Archive`, `\All` or `\Flagged`.
    pub special_use: Option<String>,
    pub subscribed: bool,
    /// IMAP `UIDVALIDITY`; bumped when UIDs are renumbered.
    pub uid_validity: i64,
    /// The next UID that will be handed out.
    pub uid_next: i64,
    /// Highest `MODSEQ` in this folder (CONDSTORE).
    pub highest_modseq: i64,
    pub message_count: i32,
    pub unseen_count: i32,
    pub total_bytes: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Folder {
    /// Typed primary key.
    pub fn folder_id(&self) -> ferroma_core::MailboxId {
        ferroma_core::MailboxId::new(self.id)
    }

    /// The `INBOX` folder is case-insensitive in IMAP; everything else is not.
    pub fn is_inbox(&self) -> bool {
        self.name.eq_ignore_ascii_case("INBOX")
    }
}

/// A stored message. Body bytes live in the Maildir at `storage_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Message {
    pub id: i64,
    pub folder_id: i64,
    /// Denormalised copy of the folder's mailbox, for account-scoped queries.
    pub mailbox_id: i64,
    /// IMAP UID, unique within `folder_id`.
    pub uid: i64,
    /// RFC 5322 `Message-ID` header, if the sender supplied one.
    pub rfc_message_id: Option<String>,
    /// Root of the `References` chain, for conversation grouping.
    pub thread_id: Option<String>,
    pub subject: Option<String>,
    /// Envelope/`From` address.
    pub sender: Option<String>,
    /// Display name that accompanied `sender`.
    pub sender_name: Option<String>,
    /// Short plain-text preview for list views.
    pub snippet: Option<String>,
    pub size_bytes: i64,
    /// Path relative to the Maildir root.
    pub storage_path: String,
    pub checksum_sha256: Option<String>,
    /// Canonical flag string, e.g. `seen flagged $label1`.
    pub flags: String,
    /// IMAP `INTERNALDATE`.
    pub internal_date: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    /// The `Date:` header, which is what the user sees.
    pub sent_at: Option<DateTime<Utc>>,
    pub has_attachments: bool,
    pub attachment_count: i32,
    pub is_draft: bool,
    /// Per-folder modification sequence (CONDSTORE).
    pub modseq: i64,
    /// Soft delete marker.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Set when the row leaves the live set; tombstones keep the id valid.
    pub expunged_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Message {
    /// Typed primary key.
    pub fn message_id(&self) -> ferroma_core::MessageId {
        ferroma_core::MessageId::new(self.id)
    }

    /// The folder this message lives in.
    pub fn folder(&self) -> ferroma_core::MailboxId {
        ferroma_core::MailboxId::new(self.folder_id)
    }

    /// The account this message belongs to.
    pub fn mailbox(&self) -> ferroma_core::MailboxId {
        ferroma_core::MailboxId::new(self.mailbox_id)
    }

    /// Whether the message is still present in its folder.
    pub fn is_live(&self) -> bool {
        self.expunged_at.is_none()
    }
}

/// One `To`/`Cc`/`Bcc`/`Reply-To` entry of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct MessageRecipient {
    pub id: i64,
    pub message_id: i64,
    /// `to`, `cc`, `bcc`, `reply-to` or `sender`.
    pub kind: String,
    pub address: String,
    pub display_name: Option<String>,
    pub ordinal: i32,
}

/// An attachment of a stored message. Bytes live in the attachment store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct AttachmentRow {
    pub id: i64,
    pub message_id: i64,
    pub filename: Option<String>,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_path: String,
    /// `Content-ID` for inline parts, without angle brackets.
    pub content_id: Option<String>,
    pub is_inline: bool,
    pub checksum_sha256: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl AttachmentRow {
    /// Typed primary key.
    pub fn attachment_id(&self) -> ferroma_core::AttachmentId {
        ferroma_core::AttachmentId::new(self.id)
    }
}

/// One recipient of one outbound message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct QueueEntry {
    pub id: i64,
    pub message_id: i64,
    pub user_id: Option<i64>,
    pub sender: String,
    pub recipient: String,
    /// `pending`, `delivering`, `delivered`, `retry`, `failed` or `cancelled`.
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_status_code: Option<i32>,
    pub last_status_text: Option<String>,
    pub remote_mx: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl QueueEntry {
    /// Typed primary key.
    pub fn queue_id(&self) -> ferroma_core::QueueId {
        ferroma_core::QueueId::new(self.id)
    }

    /// Whether the dispatcher should pick this row up.
    pub fn is_due(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "retry")
    }

    /// Attempts left before the message is bounced.
    pub fn attempts_remaining(&self) -> i32 {
        (self.max_attempts - self.attempts).max(0)
    }
}

/// The audit trail of one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct DeliveryAttempt {
    pub id: i64,
    pub queue_id: i64,
    pub attempt: i32,
    pub remote_mx: Option<String>,
    pub status_code: Option<i32>,
    pub status_text: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<i32>,
    pub created_at: DateTime<Utc>,
}

/// A logged-in official client installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Device {
    pub id: i64,
    pub user_id: i64,
    /// Stable identifier generated by the client.
    pub device_uid: String,
    pub name: Option<String>,
    /// `windows`, `linux`, `macos`, `android` or `ios`.
    pub platform: Option<String>,
    pub client_version: Option<String>,
    pub protocol_version: Option<i32>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub last_ip: Option<String>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Device {
    /// Typed primary key.
    pub fn device_id(&self) -> ferroma_core::DeviceId {
        ferroma_core::DeviceId::new(self.id)
    }

    /// Whether the device may still sync.
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// A Webmail/API/client session. Only the token *hash* is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    /// `web`, `api`, `client`, `imap` or `smtp`.
    pub kind: String,
    pub token_hash: String,
    pub device_id: Option<i64>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Session {
    /// Typed primary key.
    pub fn session_id(&self) -> ferroma_core::SessionId {
        ferroma_core::SessionId::new(self.id)
    }

    /// Whether the session is still usable at `now`.
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

/// Per-device, per-folder sync cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ClientSyncState {
    pub id: i64,
    pub device_id: i64,
    pub mailbox_id: i64,
    /// `None` means account level (folder list, settings).
    pub folder_id: Option<i64>,
    pub cursor: i64,
    pub updated_at: DateTime<Utc>,
}

impl ClientSyncState {
    /// The cursor as the API exposes it.
    pub fn cursor_value(&self) -> ferroma_core::Cursor {
        ferroma_core::Cursor(self.cursor)
    }
}

/// A saved draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Draft {
    pub id: i64,
    pub user_id: i64,
    pub mailbox_id: Option<i64>,
    pub folder_id: Option<i64>,
    pub message_id: Option<i64>,
    pub subject: Option<String>,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    /// JSON array of `{address, name}`.
    pub recipients: serde_json::Value,
    /// JSON array of `{filename, content_type, size_bytes, storage_path}`.
    pub attachments: serde_json::Value,
    pub in_reply_to: Option<String>,
    /// JSON array of `Message-ID` strings.
    pub reference_ids: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Draft {
    /// Typed primary key.
    pub fn draft_id(&self) -> ferroma_core::DraftId {
        ferroma_core::DraftId::new(self.id)
    }
}

/// An administrative or security-relevant action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct AuditLog {
    pub id: i64,
    pub actor_user_id: Option<i64>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub details: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// A recorded client operation, for idempotent replay (specification §55).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Operation {
    pub operation_id: String,
    pub user_id: Option<i64>,
    pub kind: String,
    /// `applied` or `failed`.
    pub status: String,
    /// The cached response, replayed verbatim to a retrying client.
    pub result: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Operation {
    /// Typed operation id.
    pub fn op_id(&self) -> ferroma_core::ids::OperationId {
        ferroma_core::ids::OperationId::new(self.operation_id.clone())
    }
}

/// One entry of the sync journal behind incremental sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ChangeLogEntry {
    /// Monotonic sequence number — this is the cursor.
    pub seq: i64,
    pub user_id: i64,
    pub mailbox_id: Option<i64>,
    pub folder_id: Option<i64>,
    /// Deliberately not a foreign key: tombstones outlive rows.
    pub message_id: Option<i64>,
    /// `message_created`, `message_updated`, `message_deleted`, `message_moved`,
    /// `folder_created`, `folder_updated`, `folder_deleted`, `draft_created`,
    /// `draft_updated`, `draft_deleted`.
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl ChangeLogEntry {
    /// The entry's sequence number as a cursor.
    pub fn cursor(&self) -> ferroma_core::Cursor {
        ferroma_core::Cursor(self.seq)
    }
}

/// A login attempt, successful or not. Powers throttling and the audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct LoginAttempt {
    pub id: i64,
    pub email: String,
    pub ip: Option<String>,
    pub kind: String,
    pub success: bool,
    pub created_at: DateTime<Utc>,
}

/// A key/value row of the `settings` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Setting {
    pub key: String,
    pub value: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn sample_user() -> User {
        User {
            id: 7,
            email: "alice@example.com".into(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
            display_name: Some("Alice".into()),
            enabled: true,
            is_admin: false,
            quota_bytes: 1000,
            used_bytes: 250,
            failed_logins: 0,
            locked_until: None,
            last_login_at: None,
            created_at: at(0),
            updated_at: at(0),
        }
    }

    #[test]
    fn user_typed_ids_and_quota_helpers() {
        let u = sample_user();
        assert_eq!(u.user_id(), ferroma_core::UserId::new(7));
        assert_eq!(u.quota_remaining(), 750);
    }

    #[test]
    fn quota_remaining_never_goes_negative() {
        let mut u = sample_user();
        u.used_bytes = 5_000;
        assert_eq!(u.quota_remaining(), 0);
    }

    #[test]
    fn login_allowed_respects_enabled_and_lockout() {
        let now = at(1_000);
        let mut u = sample_user();
        assert!(u.is_login_allowed(now));

        u.locked_until = Some(at(2_000));
        assert!(!u.is_login_allowed(now));
        assert!(u.is_login_allowed(at(2_001)));

        u.locked_until = None;
        u.enabled = false;
        assert!(!u.is_login_allowed(now));
    }

    #[test]
    fn mailbox_address_is_composed_from_parts() {
        let m = Mailbox {
            id: 3,
            user_id: 7,
            domain_id: 1,
            local_part: "alice".into(),
            display_name: None,
            enabled: true,
            is_primary: true,
            quota_bytes: None,
            created_at: at(0),
            updated_at: at(0),
        };
        assert_eq!(m.address("example.com"), "alice@example.com");
        assert_eq!(m.mailbox_id(), ferroma_core::MailboxId::new(3));
        assert_eq!(m.owner(), ferroma_core::UserId::new(7));
    }

    #[test]
    fn folder_knows_when_it_is_inbox() {
        let mut f = Folder {
            id: 1,
            mailbox_id: 3,
            name: "INBOX".into(),
            parent_id: None,
            special_use: None,
            subscribed: true,
            uid_validity: 1,
            uid_next: 1,
            highest_modseq: 1,
            message_count: 0,
            unseen_count: 0,
            total_bytes: 0,
            created_at: at(0),
            updated_at: at(0),
        };
        assert!(f.is_inbox());
        f.name = "inbox".into();
        assert!(f.is_inbox());
        f.name = "Sent".into();
        assert!(!f.is_inbox());
    }

    #[test]
    fn queue_entry_scheduling_helpers() {
        let mut q = QueueEntry {
            id: 1,
            message_id: 10,
            user_id: Some(7),
            sender: "alice@example.com".into(),
            recipient: "bob@example.net".into(),
            status: "retry".into(),
            attempts: 3,
            max_attempts: 12,
            next_attempt_at: Some(at(500)),
            last_attempt_at: Some(at(200)),
            delivered_at: None,
            last_error: Some("421 too many connections".into()),
            last_status_code: Some(421),
            last_status_text: Some("Try again later".into()),
            remote_mx: Some("mx1.example.net".into()),
            created_at: at(0),
            updated_at: at(100),
        };
        assert!(q.is_due());
        assert_eq!(q.attempts_remaining(), 9);
        assert_eq!(q.queue_id(), ferroma_core::QueueId::new(1));

        q.status = "delivered".into();
        assert!(!q.is_due());
        q.status = "failed".into();
        assert!(!q.is_due());

        q.attempts = 99;
        assert_eq!(q.attempts_remaining(), 0);
    }

    #[test]
    fn session_validity_window() {
        let now = at(1_000);
        let mut s = Session {
            id: 1,
            user_id: 7,
            kind: "web".into(),
            token_hash: "abc".into(),
            device_id: None,
            ip: None,
            user_agent: None,
            created_at: at(0),
            last_seen_at: at(0),
            expires_at: at(2_000),
            revoked_at: None,
        };
        assert!(s.is_valid_at(now));
        assert!(!s.is_valid_at(at(3_000)));
        s.revoked_at = Some(at(500));
        assert!(!s.is_valid_at(now));
        assert_eq!(s.session_id(), ferroma_core::SessionId::new(1));
    }

    #[test]
    fn device_and_message_helpers() {
        let mut d = Device {
            id: 2,
            user_id: 7,
            device_uid: "dev-1".into(),
            name: Some("Workstation".into()),
            platform: Some("windows".into()),
            client_version: Some("0.7.0".into()),
            protocol_version: Some(1),
            last_seen_at: None,
            last_ip: None,
            created_at: at(0),
            revoked_at: None,
        };
        assert!(d.is_active());
        d.revoked_at = Some(at(10));
        assert!(!d.is_active());
        assert_eq!(d.device_id(), ferroma_core::DeviceId::new(2));
    }

    #[test]
    fn message_live_and_cursor_helpers() {
        let msg = Message {
            id: 42,
            folder_id: 5,
            mailbox_id: 3,
            uid: 17,
            rfc_message_id: Some("<a@b.c>".into()),
            thread_id: None,
            subject: Some("Invoice".into()),
            sender: Some("bob@example.net".into()),
            sender_name: None,
            snippet: None,
            size_bytes: 2048,
            storage_path: "example.com/alice/Maildir/cur/1:2,S".into(),
            checksum_sha256: None,
            flags: "seen".into(),
            internal_date: at(0),
            received_at: at(0),
            sent_at: None,
            has_attachments: false,
            attachment_count: 0,
            is_draft: false,
            modseq: 4,
            deleted_at: None,
            expunged_at: None,
            created_at: at(0),
            updated_at: at(0),
        };
        assert!(msg.is_live());
        assert_eq!(msg.message_id(), ferroma_core::MessageId::new(42));
        assert_eq!(msg.folder(), ferroma_core::MailboxId::new(5));
        assert_eq!(msg.mailbox(), ferroma_core::MailboxId::new(3));

        let entry = ChangeLogEntry {
            seq: 99,
            user_id: 7,
            mailbox_id: Some(3),
            folder_id: Some(5),
            message_id: Some(42),
            kind: "message_created".into(),
            payload: serde_json::json!({}),
            created_at: at(0),
        };
        assert_eq!(entry.cursor(), ferroma_core::Cursor(99));
    }
}
