//! The event vocabulary — *what* happened, *who* it belongs to, and the envelope
//! that carries it over the wire.
//!
//! The specification (§22 *Event Bus*, §23 *WebSocket / realtime events*) splits the
//! realtime surface in two:
//!
//! * **Payloads** ([`Event`] and the structs it wraps) describe a domain fact:
//!   a message arrived, a draft was deleted, a delivery attempt failed.
//! * **Scoping** ([`EventScope`], [`EventEnvelope`]) describes *whose* stream the
//!   fact belongs to, plus the sequencing metadata a client needs to order,
//!   deduplicate and resume events after a reconnect (`seq`, `id`, `at`).
//!
//! # Privacy invariant
//!
//! No payload in this module carries a message body, an attachment, or a
//! credential. Everything is metadata a notification banner or a mailbox-list
//! refresh needs: ids, flags, recipient counts, subjects and a short snippet.
//! [`EventEnvelope::summary`] exists for logs and obeys the same rule.
//!
//! # Wire shape
//!
//! [`EventEnvelope::to_wire`] produces the single JSON frame the WebSocket
//! protocol sends:
//!
//! ```json
//! {
//!   "seq": 42,
//!   "id": "9c0f6a2e-…",
//!   "at": "2026-09-16T12:00:00Z",
//!   "scope": { "user": 7 },
//!   "type": "mail.received",
//!   "mailbox_id": 12,
//!   "message_id": 9001,
//!   "from": "alice@example.com",
//!   "subject": "Lunch?",
//!   "size_bytes": 4211,
//!   "snippet": "Are you free at 12:30…"
//! }
//! ```

use std::fmt;

use chrono::{DateTime, Utc};
use ferroma_core::{DeviceId, DraftId, FerromaError, MailboxId, MessageId, QueueId, Result, UserId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Who an event belongs to. Clients subscribe to their own stream only.
///
/// Scopes are the authorisation boundary of the realtime API: the server filters
/// per subscriber ([`crate::EventFilter`]), so a session authenticated as user *A*
/// can never observe a [`EventScope::User`] belonging to *B*, even though both
/// share one process-wide bus.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    /// A single user's private stream (Webmail session, official client).
    User(UserId),
    /// A mailbox-level stream (used when several sessions watch one folder).
    Mailbox(MailboxId),
    /// Server-wide: admin dashboard, metrics, webhooks.
    System,
}

impl EventScope {
    /// The user this scope belongs to, if it is a per-user stream.
    #[must_use]
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            EventScope::User(id) => Some(*id),
            _ => None,
        }
    }

    /// The mailbox this scope belongs to, if it is a per-mailbox stream.
    #[must_use]
    pub fn mailbox_id(&self) -> Option<MailboxId> {
        match self {
            EventScope::Mailbox(id) => Some(*id),
            _ => None,
        }
    }

    /// `true` for [`EventScope::System`] — the stream shared by admin, metrics and
    /// webhooks.
    #[must_use]
    pub fn is_system(&self) -> bool {
        matches!(self, EventScope::System)
    }
}

impl fmt::Display for EventScope {
    /// Renders as `user:12`, `mailbox:34` or `system` — used in logs and as the
    /// subscriber key in metrics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventScope::User(id) => write!(f, "user:{}", id.get()),
            EventScope::Mailbox(id) => write!(f, "mailbox:{}", id.get()),
            EventScope::System => f.write_str("system"),
        }
    }
}

/// A new message landed in a mailbox.
///
/// This is the event that drives the desktop notification and the unread badge:
/// it deliberately carries a truncated `snippet` and never the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailReceived {
    /// The folder the message was filed into.
    pub mailbox_id: MailboxId,
    /// The stored message.
    pub message_id: MessageId,
    /// The `From` header value, already formatted for display.
    pub from: Option<String>,
    /// The `Subject` header value, already decoded (RFC 2047).
    pub subject: Option<String>,
    /// Size of the stored message in bytes.
    pub size_bytes: i64,
    /// Short, body-free preview text for notification banners.
    pub snippet: Option<String>,
}

/// An outgoing message was accepted for delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSent {
    /// The folder the sent copy was filed into (usually `Sent`).
    pub mailbox_id: MailboxId,
    /// The stored copy of the sent message.
    pub message_id: MessageId,
    /// How many recipients the envelope had.
    pub recipient_count: usize,
    /// `true` when the message went to the queue instead of being delivered inline.
    pub queued: bool,
}

/// A message was removed from a mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailDeleted {
    /// The folder the message was removed from.
    pub mailbox_id: MailboxId,
    /// The message that disappeared.
    pub message_id: MessageId,
    /// `true` for an expunge (gone for good), `false` for a move to `Trash`.
    pub permanent: bool,
}

/// A message's `\Seen` state changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailRead {
    /// The folder holding the message.
    pub mailbox_id: MailboxId,
    /// The message whose seen state changed.
    pub message_id: MessageId,
    /// The new state of `\Seen`.
    pub seen: bool,
}

/// The IMAP flag set of a message changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFlagChanged {
    /// The folder holding the message.
    pub mailbox_id: MailboxId,
    /// The message whose flags changed.
    pub message_id: MessageId,
    /// The DB flag string, e.g. `"seen,flagged"`.
    pub flags: String,
}

/// A message was filed from one folder into another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailMoved {
    /// The folder the message left.
    pub from_mailbox_id: MailboxId,
    /// The folder the message arrived in.
    pub to_mailbox_id: MailboxId,
    /// The message that moved.
    pub message_id: MessageId,
}

/// A draft was created (or autosaved for the first time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftCreated {
    /// The new draft.
    pub draft_id: DraftId,
    /// The folder the draft belongs to, when it is already filed.
    pub mailbox_id: Option<MailboxId>,
    /// Subject typed so far, when the client supplied one.
    pub subject: Option<String>,
}

/// A draft was modified or deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftUpdated {
    /// The draft that changed.
    pub draft_id: DraftId,
    /// `true` when the draft no longer exists (sent or discarded).
    pub deleted: bool,
}

/// The state of one outbound delivery attempt changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryUpdated {
    /// The `mail_queue` row being attempted.
    pub queue_id: QueueId,
    /// The stored message being delivered.
    pub message_id: MessageId,
    /// The recipient address of this attempt (`RCPT TO`).
    pub recipient: String,
    /// Human-readable queue status, e.g. `pending`, `deferred`, `delivered`, `failed`.
    pub status: String,
    /// How many attempts have been made so far.
    pub attempts: u32,
    /// The last failure reason, when the attempt failed. Never contains credentials.
    pub last_error: Option<String>,
}

/// A device (official client installation) was signed out by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevoked {
    /// The revoked device.
    pub device_id: DeviceId,
    /// The user who owned it.
    pub user_id: UserId,
}

/// Every event the platform publishes.
///
/// Serialises with an internal `"type"` tag in `snake_case`
/// (`{"type":"mail_received", …}`); the WebSocket protocol uses the dotted wire
/// name from [`Event::name`] instead, which [`EventEnvelope::to_wire`] applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// See [`MailReceived`].
    MailReceived(MailReceived),
    /// See [`MailSent`].
    MailSent(MailSent),
    /// See [`MailDeleted`].
    MailDeleted(MailDeleted),
    /// See [`MailRead`].
    MailRead(MailRead),
    /// See [`MailFlagChanged`].
    MailFlagChanged(MailFlagChanged),
    /// See [`MailMoved`].
    MailMoved(MailMoved),
    /// See [`DraftCreated`].
    DraftCreated(DraftCreated),
    /// See [`DraftUpdated`].
    DraftUpdated(DraftUpdated),
    /// See [`DeliveryUpdated`].
    DeliveryUpdated(DeliveryUpdated),
    /// See [`DeviceRevoked`].
    DeviceRevoked(DeviceRevoked),
}

impl Event {
    /// The wire name used by the WebSocket protocol (specification §23), e.g. `"mail.received"`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Event::MailReceived(_) => "mail.received",
            Event::MailSent(_) => "mail.sent",
            Event::MailDeleted(_) => "mail.deleted",
            Event::MailRead(_) => "mail.read",
            Event::MailFlagChanged(_) => "mail.flag_changed",
            Event::MailMoved(_) => "mail.moved",
            Event::DraftCreated(_) => "draft.created",
            Event::DraftUpdated(_) => "draft.updated",
            Event::DeliveryUpdated(_) => "delivery.updated",
            Event::DeviceRevoked(_) => "device.revoked",
        }
    }

    /// The mailbox this event concerns, when it has one.
    ///
    /// [`MailMoved`] reports the *destination* mailbox; use pattern matching when
    /// the source folder matters too.
    #[must_use]
    pub fn mailbox_id(&self) -> Option<MailboxId> {
        match self {
            Event::MailReceived(e) => Some(e.mailbox_id),
            Event::MailSent(e) => Some(e.mailbox_id),
            Event::MailDeleted(e) => Some(e.mailbox_id),
            Event::MailRead(e) => Some(e.mailbox_id),
            Event::MailFlagChanged(e) => Some(e.mailbox_id),
            Event::MailMoved(e) => Some(e.to_mailbox_id),
            Event::DraftCreated(e) => e.mailbox_id,
            Event::DraftUpdated(_) | Event::DeliveryUpdated(_) | Event::DeviceRevoked(_) => None,
        }
    }

    /// The message this event concerns, when it has one.
    #[must_use]
    pub fn message_id(&self) -> Option<MessageId> {
        match self {
            Event::MailReceived(e) => Some(e.message_id),
            Event::MailSent(e) => Some(e.message_id),
            Event::MailDeleted(e) => Some(e.message_id),
            Event::MailRead(e) => Some(e.message_id),
            Event::MailFlagChanged(e) => Some(e.message_id),
            Event::MailMoved(e) => Some(e.message_id),
            Event::DeliveryUpdated(e) => Some(e.message_id),
            Event::DraftCreated(_) | Event::DraftUpdated(_) | Event::DeviceRevoked(_) => None,
        }
    }

    /// The queue row this event concerns, when it has one.
    #[must_use]
    pub fn queue_id(&self) -> Option<QueueId> {
        match self {
            Event::DeliveryUpdated(e) => Some(e.queue_id),
            _ => None,
        }
    }

    /// The user this event is *about*, when the payload names one.
    ///
    /// [`DeviceRevoked`] is the only payload that carries a user id today; for
    /// every other event the user comes from the envelope's [`EventScope`].
    #[must_use]
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            Event::DeviceRevoked(e) => Some(e.user_id),
            _ => None,
        }
    }

    /// `true` when the event adds a message to a folder (drives the unread badge).
    #[must_use]
    pub fn is_incoming(&self) -> bool {
        matches!(self, Event::MailReceived(_))
    }

    /// Construct a [`Event::MailReceived`] with only the ids known; the display
    /// fields stay `None` until the caller fills them in.
    #[must_use]
    pub fn mail_received(mailbox_id: MailboxId, message_id: MessageId) -> Self {
        Event::MailReceived(MailReceived {
            mailbox_id,
            message_id,
            from: None,
            subject: None,
            size_bytes: 0,
            snippet: None,
        })
    }

    /// Construct a [`Event::MailSent`] for `recipient_count` recipients.
    #[must_use]
    pub fn mail_sent(
        mailbox_id: MailboxId,
        message_id: MessageId,
        recipient_count: usize,
        queued: bool,
    ) -> Self {
        Event::MailSent(MailSent {
            mailbox_id,
            message_id,
            recipient_count,
            queued,
        })
    }

    /// Construct a [`Event::MailDeleted`]; `permanent` distinguishes expunge from trash.
    #[must_use]
    pub fn mail_deleted(mailbox_id: MailboxId, message_id: MessageId, permanent: bool) -> Self {
        Event::MailDeleted(MailDeleted {
            mailbox_id,
            message_id,
            permanent,
        })
    }

    /// Construct a [`Event::MailRead`].
    #[must_use]
    pub fn mail_read(mailbox_id: MailboxId, message_id: MessageId, seen: bool) -> Self {
        Event::MailRead(MailRead {
            mailbox_id,
            message_id,
            seen,
        })
    }

    /// Construct a [`Event::MailFlagChanged`] from a DB flag string such as
    /// `"seen,flagged"`.
    #[must_use]
    pub fn mail_flag_changed(
        mailbox_id: MailboxId,
        message_id: MessageId,
        flags: impl Into<String>,
    ) -> Self {
        Event::MailFlagChanged(MailFlagChanged {
            mailbox_id,
            message_id,
            flags: flags.into(),
        })
    }

    /// Construct a [`Event::MailMoved`].
    #[must_use]
    pub fn mail_moved(from: MailboxId, to: MailboxId, message_id: MessageId) -> Self {
        Event::MailMoved(MailMoved {
            from_mailbox_id: from,
            to_mailbox_id: to,
            message_id,
        })
    }

    /// Construct a [`Event::DraftCreated`].
    #[must_use]
    pub fn draft_created(draft_id: DraftId, mailbox_id: Option<MailboxId>) -> Self {
        Event::DraftCreated(DraftCreated {
            draft_id,
            mailbox_id,
            subject: None,
        })
    }

    /// Construct a [`Event::DraftUpdated`].
    #[must_use]
    pub fn draft_updated(draft_id: DraftId, deleted: bool) -> Self {
        Event::DraftUpdated(DraftUpdated { draft_id, deleted })
    }

    /// Construct a [`Event::DeliveryUpdated`] with zero attempts and no error.
    #[must_use]
    pub fn delivery_updated(
        queue_id: QueueId,
        message_id: MessageId,
        recipient: impl Into<String>,
        status: impl Into<String>,
    ) -> Self {
        Event::DeliveryUpdated(DeliveryUpdated {
            queue_id,
            message_id,
            recipient: recipient.into(),
            status: status.into(),
            attempts: 0,
            last_error: None,
        })
    }

    /// Construct a [`Event::DeviceRevoked`].
    #[must_use]
    pub fn device_revoked(device_id: DeviceId, user_id: UserId) -> Self {
        Event::DeviceRevoked(DeviceRevoked { device_id, user_id })
    }
}

impl fmt::Display for Event {
    /// The dotted wire name — `mail.received`, `draft.updated`, …
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An event plus the metadata a subscriber needs to order, deduplicate and resume.
///
/// `seq` is assigned by the bus at publish time and is gap-free and strictly
/// monotonic, which is what makes reconnect catch-up
/// ([`EventBus::replay_since`](crate::EventBus::replay_since)) possible: a client
/// stores the last `seq` it processed and asks for everything after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Monotonic sequence number, assigned by the bus on publish.
    pub seq: i64,
    /// Unique id of this event (uuid v4).
    pub id: Uuid,
    /// When the bus accepted it (UTC).
    pub at: DateTime<Utc>,
    /// Who it belongs to.
    pub scope: EventScope,
    /// What happened.
    pub event: Event,
}

impl EventEnvelope {
    /// Wrap `event` for `scope` with the given sequence number.
    ///
    /// Callers normally let [`EventBus::publish`](crate::EventBus::publish) assign
    /// `seq`; this constructor exists for replaying archived events and for tests.
    #[must_use]
    pub fn new(scope: EventScope, event: Event, seq: i64) -> Self {
        EventEnvelope {
            seq,
            id: Uuid::new_v4(),
            at: Utc::now(),
            scope,
            event,
        }
    }

    /// The WebSocket frame payload:
    /// `{"seq":..,"id":..,"at":..,"scope":..,"type":"mail.received", ...fields}`.
    ///
    /// The `type` field carries the dotted wire name from [`Event::name`] rather
    /// than the `snake_case` name serde would use for the bare enum, so the frame
    /// matches specification §23 exactly.
    pub fn to_wire(&self) -> Result<serde_json::Value> {
        let serde_json::Value::Object(fields) = serde_json::to_value(&self.event)? else {
            return Err(FerromaError::Internal(
                "event payload did not serialise to a JSON object".to_string(),
            ));
        };

        let mut frame = serde_json::Map::with_capacity(fields.len() + 5);
        frame.insert("seq".to_string(), serde_json::Value::from(self.seq));
        frame.insert(
            "id".to_string(),
            serde_json::Value::String(self.id.to_string()),
        );
        frame.insert(
            "at".to_string(),
            serde_json::to_value(self.at).unwrap_or(serde_json::Value::Null),
        );
        frame.insert(
            "scope".to_string(),
            serde_json::to_value(&self.scope).unwrap_or(serde_json::Value::Null),
        );
        frame.insert(
            "type".to_string(),
            serde_json::Value::String(self.event.name().to_string()),
        );
        for (key, value) in fields {
            // `type` is authoritative: the dotted wire name wins over the
            // snake_case tag produced by the enum's serde representation.
            if key == "type" {
                continue;
            }
            frame.insert(key, value);
        }

        Ok(serde_json::Value::Object(frame))
    }

    /// The `type` field of the wire frame — a shorthand for `self.event.name()`.
    #[must_use]
    pub fn wire_type(&self) -> &'static str {
        self.event.name()
    }

    /// A short human summary for logs. NEVER include message bodies.
    ///
    /// Keeps logs cheap and GDPR-friendly: ids, flags, counts and statuses only.
    /// Subjects and snippets are intentionally omitted even though the payload
    /// carries a body-free snippet, because logs are retained far longer than the
    /// notification banner the snippet exists for.
    #[must_use]
    pub fn summary(&self) -> String {
        match &self.event {
            Event::MailReceived(e) => format!(
                "mail.received mailbox={} message={} size={}B",
                e.mailbox_id.get(),
                e.message_id.get(),
                e.size_bytes
            ),
            Event::MailSent(e) => format!(
                "mail.sent mailbox={} message={} recipients={} queued={}",
                e.mailbox_id.get(),
                e.message_id.get(),
                e.recipient_count,
                e.queued
            ),
            Event::MailDeleted(e) => format!(
                "mail.deleted mailbox={} message={} permanent={}",
                e.mailbox_id.get(),
                e.message_id.get(),
                e.permanent
            ),
            Event::MailRead(e) => format!(
                "mail.read mailbox={} message={} seen={}",
                e.mailbox_id.get(),
                e.message_id.get(),
                e.seen
            ),
            Event::MailFlagChanged(e) => format!(
                "mail.flag_changed mailbox={} message={} flags=[{}]",
                e.mailbox_id.get(),
                e.message_id.get(),
                e.flags
            ),
            Event::MailMoved(e) => format!(
                "mail.moved from={} to={} message={}",
                e.from_mailbox_id.get(),
                e.to_mailbox_id.get(),
                e.message_id.get()
            ),
            Event::DraftCreated(e) => format!(
                "draft.created draft={} mailbox={}",
                e.draft_id.get(),
                e.mailbox_id
                    .map_or_else(|| "-".to_string(), |id| id.get().to_string())
            ),
            Event::DraftUpdated(e) => format!(
                "draft.updated draft={} deleted={}",
                e.draft_id.get(),
                e.deleted
            ),
            Event::DeliveryUpdated(e) => format!(
                "delivery.updated queue={} message={} status={} attempts={}",
                e.queue_id.get(),
                e.message_id.get(),
                e.status,
                e.attempts
            ),
            Event::DeviceRevoked(e) => format!(
                "device.revoked device={} user={}",
                e.device_id.get(),
                e.user_id.get()
            ),
        }
    }
}

impl fmt::Display for EventEnvelope {
    /// `seq=42 type=mail.received scope=user:7` — the header of a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "seq={} type={} scope={}",
            self.seq,
            self.event.name(),
            self.scope
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mb(raw: i64) -> MailboxId {
        MailboxId::new(raw)
    }

    fn msg(raw: i64) -> MessageId {
        MessageId::new(raw)
    }

    /// One fully-populated sample of every variant, so the per-variant tests
    /// (`name`, `to_wire`, serde round-trip) cannot silently skip a payload type.
    fn samples() -> Vec<Event> {
        vec![
            Event::MailReceived(MailReceived {
                mailbox_id: mb(7),
                message_id: msg(100),
                from: Some("alice@example.com".into()),
                subject: Some("Lunch?".into()),
                size_bytes: 4211,
                snippet: Some("Are you free at 12:30".into()),
            }),
            Event::MailSent(MailSent {
                mailbox_id: mb(8),
                message_id: msg(101),
                recipient_count: 3,
                queued: true,
            }),
            Event::MailDeleted(MailDeleted {
                mailbox_id: mb(9),
                message_id: msg(102),
                permanent: true,
            }),
            Event::MailRead(MailRead {
                mailbox_id: mb(10),
                message_id: msg(103),
                seen: false,
            }),
            Event::MailFlagChanged(MailFlagChanged {
                mailbox_id: mb(11),
                message_id: msg(104),
                flags: "seen,flagged".into(),
            }),
            Event::MailMoved(MailMoved {
                from_mailbox_id: mb(12),
                to_mailbox_id: mb(13),
                message_id: msg(105),
            }),
            Event::DraftCreated(DraftCreated {
                draft_id: DraftId::new(3),
                mailbox_id: Some(mb(14)),
                subject: Some("re: invoice".into()),
            }),
            Event::DraftUpdated(DraftUpdated {
                draft_id: DraftId::new(4),
                deleted: false,
            }),
            Event::DeliveryUpdated(DeliveryUpdated {
                queue_id: QueueId::new(5),
                message_id: msg(106),
                recipient: "bob@example.org".into(),
                status: "deferred".into(),
                attempts: 2,
                last_error: Some("451 4.3.0 try later".into()),
            }),
            Event::DeviceRevoked(DeviceRevoked {
                device_id: DeviceId::new(6),
                user_id: UserId::new(7),
            }),
        ]
    }

    #[test]
    fn every_variant_has_its_spec_wire_name() {
        let names: Vec<&str> = samples().iter().map(Event::name).collect();
        assert_eq!(
            names,
            vec![
                "mail.received",
                "mail.sent",
                "mail.deleted",
                "mail.read",
                "mail.flag_changed",
                "mail.moved",
                "draft.created",
                "draft.updated",
                "delivery.updated",
                "device.revoked",
            ]
        );
        // Display agrees with name().
        for event in samples() {
            assert_eq!(event.to_string(), event.name());
        }
    }

    #[test]
    fn wire_names_are_dotted_and_never_empty() {
        for event in samples() {
            let name = event.name();
            assert!(name.contains('.'), "{name} should be dotted");
            assert!(!name.contains('_') || name.starts_with("mail.flag"), "{name}");
        }
    }

    #[test]
    fn enum_serde_tag_is_snake_case() {
        let json = serde_json::to_value(Event::mail_read(mb(1), msg(2), true)).unwrap();
        assert_eq!(json["type"], "mail_read");
    }

    #[test]
    fn payloads_round_trip_through_json() {
        for event in samples() {
            let json = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(back, event, "round trip failed for {}", event.name());
        }
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let envelope = EventEnvelope::new(EventScope::User(UserId::new(7)), samples().remove(0), 12);
        let json = serde_json::to_string(&envelope).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
        assert_eq!(back.seq, 12);
    }

    #[test]
    fn scope_display_is_stable() {
        assert_eq!(EventScope::User(UserId::new(12)).to_string(), "user:12");
        assert_eq!(EventScope::Mailbox(mb(34)).to_string(), "mailbox:34");
        assert_eq!(EventScope::System.to_string(), "system");
    }

    #[test]
    fn scope_accessors_only_answer_for_their_own_kind() {
        let user = EventScope::User(UserId::new(12));
        assert_eq!(user.user_id(), Some(UserId::new(12)));
        assert_eq!(user.mailbox_id(), None);
        assert!(!user.is_system());

        let mailbox = EventScope::Mailbox(mb(34));
        assert_eq!(mailbox.mailbox_id(), Some(mb(34)));
        assert_eq!(mailbox.user_id(), None);
        assert!(!mailbox.is_system());

        assert!(EventScope::System.is_system());
        assert_eq!(EventScope::System.user_id(), None);
        assert_eq!(EventScope::System.mailbox_id(), None);
    }

    #[test]
    fn scope_serialises_in_a_tagged_shape() {
        assert_eq!(
            serde_json::to_value(EventScope::System).unwrap(),
            serde_json::json!("system")
        );
        assert_eq!(
            serde_json::to_value(EventScope::User(UserId::new(7))).unwrap(),
            serde_json::json!({ "user": 7 })
        );
        assert_eq!(
            serde_json::to_value(EventScope::Mailbox(mb(3))).unwrap(),
            serde_json::json!({ "mailbox": 3 })
        );
        // Round trip through the wire shape as well.
        let back: EventScope =
            serde_json::from_value(serde_json::json!({ "mailbox": 3 })).unwrap();
        assert_eq!(back, EventScope::Mailbox(mb(3)));
    }

    #[test]
    fn id_accessors_report_what_they_know() {
        for event in samples() {
            // mailbox_id/message_id must never panic and must agree with the payload.
            let _ = event.mailbox_id();
            let _ = event.message_id();
            let _ = event.queue_id();
            let _ = event.user_id();
            let _ = event.is_incoming();
        }

        let moved = Event::mail_moved(mb(1), mb(2), msg(3));
        assert_eq!(moved.mailbox_id(), Some(mb(2)), "destination mailbox");
        assert_eq!(moved.message_id(), Some(msg(3)));

        let received = Event::mail_received(mb(1), msg(2));
        assert_eq!(received.mailbox_id(), Some(mb(1)));
        assert_eq!(received.message_id(), Some(msg(2)));
        assert!(received.is_incoming());
        assert!(!Event::draft_updated(DraftId::new(1), false).is_incoming());

        assert_eq!(
            Event::delivery_updated(QueueId::new(9), msg(1), "a@b.c", "pending").queue_id(),
            Some(QueueId::new(9))
        );
        assert_eq!(
            Event::device_revoked(DeviceId::new(2), UserId::new(3)).user_id(),
            Some(UserId::new(3))
        );
        assert_eq!(Event::draft_created(DraftId::new(1), None).mailbox_id(), None);
        assert_eq!(
            Event::draft_created(DraftId::new(1), Some(mb(4))).mailbox_id(),
            Some(mb(4))
        );
    }

    #[test]
    fn helpers_fill_in_sensible_defaults() {
        match Event::mail_received(mb(1), msg(2)) {
            Event::MailReceived(e) => {
                assert_eq!(e.size_bytes, 0);
                assert_eq!(e.from, None);
                assert_eq!(e.subject, None);
                assert_eq!(e.snippet, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        match Event::delivery_updated(QueueId::new(1), msg(2), "bob@example.org", "pending") {
            Event::DeliveryUpdated(e) => {
                assert_eq!(e.attempts, 0);
                assert_eq!(e.last_error, None);
                assert_eq!(e.recipient, "bob@example.org");
                assert_eq!(e.status, "pending");
            }
            other => panic!("wrong variant: {other:?}"),
        }

        assert!(matches!(
            Event::mail_flag_changed(mb(1), msg(2), "seen"),
            Event::MailFlagChanged(MailFlagChanged { ref flags, .. }) if flags == "seen"
        ));
        assert!(matches!(
            Event::mail_sent(mb(1), msg(2), 4, false),
            Event::MailSent(MailSent {
                recipient_count: 4,
                queued: false,
                ..
            })
        ));
        assert!(matches!(
            Event::mail_deleted(mb(1), msg(2), true),
            Event::MailDeleted(MailDeleted { permanent: true, .. })
        ));
        assert!(matches!(
            Event::mail_read(mb(1), msg(2), true),
            Event::MailRead(MailRead { seen: true, .. })
        ));
        assert!(matches!(
            Event::draft_updated(DraftId::new(1), true),
            Event::DraftUpdated(DraftUpdated { deleted: true, .. })
        ));
    }

    #[test]
    fn to_wire_carries_the_dotted_type_and_the_envelope() {
        let envelope = EventEnvelope::new(EventScope::User(UserId::new(7)), samples().remove(0), 42);
        let wire = envelope.to_wire().unwrap();

        assert_eq!(wire["seq"], 42);
        assert_eq!(wire["type"], "mail.received");
        assert_eq!(wire["scope"], serde_json::json!({ "user": 7 }));
        assert_eq!(wire["id"], envelope.id.to_string());
        assert_eq!(wire["mailbox_id"], 7);
        assert_eq!(wire["message_id"], 100);
        assert_eq!(wire["from"], "alice@example.com");
        assert_eq!(wire["subject"], "Lunch?");
        assert_eq!(wire["size_bytes"], 4211);
        assert_eq!(wire["snippet"], "Are you free at 12:30");
        assert!(wire["at"].is_string());
        // No snake_case tag leaks into the frame.
        assert_eq!(wire["type"], "mail.received");
    }

    #[test]
    fn to_wire_shape_is_correct_for_every_variant() {
        let scope = EventScope::System;
        for (index, event) in samples().into_iter().enumerate() {
            let expected = event.name();
            let envelope = EventEnvelope::new(scope.clone(), event, index as i64 + 1);
            let wire = envelope.to_wire().unwrap();

            assert_eq!(wire["type"], expected);
            assert_eq!(wire["seq"], index as i64 + 1);
            assert_eq!(wire["scope"], serde_json::json!("system"));
            assert_eq!(wire["id"], envelope.id.to_string());
            assert!(wire["at"].is_string());
            assert!(wire.as_object().unwrap().len() >= 5, "{expected} lost fields");
        }
    }

    #[test]
    fn to_wire_keeps_every_payload_field() {
        for event in samples() {
            let name = event.name();
            let payload = serde_json::to_value(&event).unwrap();
            let envelope = EventEnvelope::new(EventScope::System, event, 1);
            let wire = envelope.to_wire().unwrap();

            let payload = payload.as_object().unwrap();
            let wire = wire.as_object().unwrap();
            for (key, value) in payload {
                if key == "type" {
                    continue;
                }
                assert_eq!(wire.get(key), Some(value), "{name} lost field {key}");
            }
        }
    }

    #[test]
    fn to_wire_is_valid_json_for_every_variant_after_round_trip() {
        for event in samples() {
            let envelope = EventEnvelope::new(EventScope::System, event, 5);
            let text = serde_json::to_string(&envelope.to_wire().unwrap()).unwrap();
            let reparsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(reparsed["seq"], 5);
        }
    }

    #[test]
    fn summary_never_leaks_a_body_or_a_snippet() {
        let secret = "SUPER-SECRET-BODY-TEXT";
        let event = Event::MailReceived(MailReceived {
            mailbox_id: mb(7),
            message_id: msg(100),
            from: Some("alice@example.com".into()),
            subject: Some(secret.into()),
            size_bytes: 10,
            snippet: Some(secret.into()),
        });
        let envelope = EventEnvelope::new(EventScope::User(UserId::new(7)), event, 3);
        let summary = envelope.summary();

        assert!(!summary.contains(secret), "{summary}");
        assert!(!summary.contains("alice@example.com"), "{summary}");
        assert!(summary.contains("mailbox=7"));
        assert!(summary.contains("message=100"));
        assert!(summary.contains("size=10B"));
    }

    #[test]
    fn summary_is_one_line_for_every_variant() {
        for event in samples() {
            let name = event.name();
            let envelope = EventEnvelope::new(EventScope::System, event, 1);
            let summary = envelope.summary();
            assert!(summary.starts_with(name), "{summary}");
            assert!(!summary.contains('\n'), "{summary}");
            assert!(!summary.is_empty());
            // The dotted name is used, not the snake_case one.
            assert!(!summary.contains(&name.replace('.', "_")), "{summary}");
        }
    }

    #[test]
    fn envelope_display_is_a_log_header() {
        let envelope = EventEnvelope::new(
            EventScope::User(UserId::new(7)),
            Event::mail_received(mb(1), msg(2)),
            42,
        );
        assert_eq!(
            envelope.to_string(),
            "seq=42 type=mail.received scope=user:7"
        );
    }

    #[test]
    fn envelope_new_stamps_a_fresh_uuid_and_timestamp() {
        let a = EventEnvelope::new(EventScope::System, Event::mail_received(mb(1), msg(2)), 1);
        let b = EventEnvelope::new(EventScope::System, Event::mail_received(mb(1), msg(2)), 1);
        assert_ne!(a.id, b.id, "each envelope gets its own uuid");
        assert!(a.at <= Utc::now());
        assert_eq!(a.wire_type(), "mail.received");
    }

    #[test]
    fn wire_frame_has_no_type_collision_with_payload_fields() {
        // No payload struct has a field called `type`, which would have been
        // overwritten by the wire tag. Guard against a future regression.
        for event in samples() {
            let payload = serde_json::to_value(&event).unwrap();
            let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
            assert_eq!(
                keys.iter().filter(|k| k.as_str() == "type").count(),
                1,
                "{keys:?}"
            );
        }
    }
}
