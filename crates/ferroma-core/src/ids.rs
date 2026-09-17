//! Typed identifiers.
//!
//! Every database primary key is a `BIGSERIAL` in the schema, but inside Rust we
//! never pass raw `i64`s around: a [`UserId`] cannot be handed to a function that
//! wants a [`MailboxId`]. The `ids` are `#[repr(transparent)]` newtypes, so they
//! are free at runtime and map to `i64` columns directly.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! numeric_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        #[repr(transparent)]
        pub struct $name(pub i64);

        impl $name {
            /// Wrap a raw database value.
            #[inline]
            pub const fn new(raw: i64) -> Self {
                $name(raw)
            }

            /// The raw database value.
            #[inline]
            pub const fn get(self) -> i64 {
                self.0
            }

            /// `false` when the id has not been assigned yet (not yet inserted).
            #[inline]
            pub const fn is_assigned(self) -> bool {
                self.0 > 0
            }
        }

        impl From<i64> for $name {
            #[inline]
            fn from(raw: i64) -> Self {
                $name(raw)
            }
        }

        impl From<$name> for i64 {
            #[inline]
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::FerromaError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                s.trim()
                    .parse::<i64>()
                    .map($name)
                    .map_err(|_| $crate::FerromaError::Parse(concat!("invalid ", $prefix, " id: ").to_string() + s))
            }
        }
    };
}

numeric_id!(
    /// A row of `users`.
    UserId,
    "user"
);
numeric_id!(
    /// A row of `domains`.
    DomainId,
    "domain"
);
numeric_id!(
    /// A row of `mailboxes`. Every mailbox is a folder owned by a user.
    MailboxId,
    "mailbox"
);
numeric_id!(
    /// A row of `messages` — the server-side, authoritative copy of an email.
    MessageId,
    "message"
);
numeric_id!(
    /// A row of `attachments`.
    AttachmentId,
    "attachment"
);
numeric_id!(
    /// A row of `mail_queue`.
    QueueId,
    "queue"
);
numeric_id!(
    /// A row of `sessions` (Webmail / API sessions).
    SessionId,
    "session"
);
numeric_id!(
    /// A row of `devices` — one per logged-in official client installation.
    DeviceId,
    "device"
);
numeric_id!(
    /// A row of `drafts`.
    DraftId,
    "draft"
);
numeric_id!(
    /// A row of `audit_logs`.
    AuditLogId,
    "audit"
);

/// The RFC 5322 `Message-ID` header value, e.g. `<20260916.1200.abc@example.com>`.
///
/// Distinct from [`MessageId`], which is the numeric row id of the stored copy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RfcMessageId(String);

impl RfcMessageId {
    /// Wrap an already-parsed header value. Angle brackets are added when missing.
    pub fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let trimmed = raw.trim();
        if trimmed.starts_with('<') && trimmed.ends_with('>') {
            RfcMessageId(trimmed.to_string())
        } else {
            RfcMessageId(format!("<{trimmed}>"))
        }
    }

    /// Generate a fresh, globally unique `Message-ID` for `domain`.
    ///
    /// Shape: `<ulid-ish.timestamp.random@domain>` — deterministic enough to sort,
    /// random enough to never collide, and valid per RFC 5322 `msg-id`.
    pub fn generate(domain: &str) -> Self {
        use rand::Rng;
        let now = chrono::Utc::now();
        let mut rng = rand::thread_rng();
        let rand_part: u64 = rng.gen();
        RfcMessageId(format!(
            "<{}.{:016x}.{:08x}@{}>",
            now.timestamp(),
            rand_part,
            std::process::id(),
            domain
        ))
    }

    /// The value without angle brackets.
    pub fn inner(&self) -> &str {
        self.0.trim_start_matches('<').trim_end_matches('>')
    }

    /// The value including angle brackets, as it appears in a header.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The domain part after `@`, when present.
    pub fn domain(&self) -> Option<&str> {
        self.inner().rsplit_once('@').map(|(_, d)| d)
    }
}

impl fmt::Display for RfcMessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A client-generated idempotency key, e.g. `op_9f2c…`.
///
/// The server records every applied operation so that a retried request (the
/// client crashed, the network dropped, the response was lost) never executes twice.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    pub fn new(raw: impl Into<String>) -> Self {
        OperationId(raw.into())
    }

    /// Generate a new random operation id with the `op_` prefix used across the API.
    pub fn generate() -> Self {
        OperationId(format!("op_{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque pagination/synchronisation cursor.
///
/// The sync engine stores a cursor per mailbox and asks only for what changed.
/// The wire format is a monotonic sequence number rendered as a string so the
/// mechanism can be swapped without breaking older clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub i64);

impl Cursor {
    /// The cursor a client sends on its very first sync.
    pub const ZERO: Cursor = Cursor(0);

    pub fn as_str(&self) -> String {
        self.0.to_string()
    }

    pub fn parse(raw: &str) -> crate::Result<Self> {
        raw.trim()
            .parse::<i64>()
            .map(Cursor)
            .map_err(|_| crate::FerromaError::Parse(format!("invalid sync cursor: {raw}")))
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_types_and_round_trip() {
        let u = UserId::new(7);
        assert_eq!(u.get(), 7);
        assert!(u.is_assigned());
        assert_eq!(UserId::from(7i64), u);
        assert_eq!(i64::from(u), 7);
        assert_eq!("42".parse::<MailboxId>().unwrap(), MailboxId(42));
        assert!("nope".parse::<MailboxId>().is_err());
    }

    #[test]
    fn rfc_message_id_normalises_brackets() {
        assert_eq!(RfcMessageId::new("a@b.c").as_str(), "<a@b.c>");
        assert_eq!(RfcMessageId::new("<a@b.c>").as_str(), "<a@b.c>");
        assert_eq!(RfcMessageId::new("<a@b.c>").domain(), Some("b.c"));
    }

    #[test]
    fn generated_message_ids_are_unique_and_well_formed() {
        let a = RfcMessageId::generate("example.com");
        let b = RfcMessageId::generate("example.com");
        assert_ne!(a, b);
        assert!(a.as_str().starts_with('<'));
        assert!(a.as_str().ends_with("@example.com>"));
        assert_eq!(a.domain(), Some("example.com"));
    }

    #[test]
    fn cursor_parses_and_rejects() {
        assert_eq!(Cursor::parse("17").unwrap(), Cursor(17));
        assert!(Cursor::parse("abc").is_err());
        assert_eq!(Cursor::ZERO.as_str(), "0");
    }
}
