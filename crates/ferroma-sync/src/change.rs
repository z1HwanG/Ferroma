//! The change vocabulary: what a client is told changed.
//!
//! One [`SyncChange`] is one entry of the `change_log`, rendered the way
//! [`docs/fcp.md`](../../docs/fcp.md) §3 documents it. The wire shape matters more
//! than the Rust shape: an official client reads `type`, `seq` and the ids, and
//! must be able to apply a page in order and then store `next_cursor`.

use ferroma_core::{Cursor, DraftId, MailboxId, MessageId};
use ferroma_storage::models::ChangeLogEntry;
use serde::{Deserialize, Serialize};

/// The kind of a change. The string form is the `type` field on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A message appeared in a folder.
    MessageCreated,
    /// A message's flags, snippet or storage location changed.
    MessageUpdated,
    /// A message left a folder — `permanent` distinguishes a hard delete (the row
    /// is gone) from a move to Trash.
    MessageDeleted,
    /// A message moved between folders; carries both ids so the client can apply it
    /// even if the two folder streams arrive out of order.
    MessageMoved,
    /// A folder was created.
    FolderCreated,
    /// A folder was renamed or its subscription changed.
    FolderUpdated,
    /// A folder was removed.
    FolderDeleted,
    /// A draft was created.
    DraftCreated,
    /// A draft was updated.
    DraftUpdated,
    /// A draft was deleted.
    DraftDeleted,
}

impl ChangeKind {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::MessageCreated => "message_created",
            ChangeKind::MessageUpdated => "message_updated",
            ChangeKind::MessageDeleted => "message_deleted",
            ChangeKind::MessageMoved => "message_moved",
            ChangeKind::FolderCreated => "folder_created",
            ChangeKind::FolderUpdated => "folder_updated",
            ChangeKind::FolderDeleted => "folder_deleted",
            ChangeKind::DraftCreated => "draft_created",
            ChangeKind::DraftUpdated => "draft_updated",
            ChangeKind::DraftDeleted => "draft_deleted",
        }
    }

    /// Parse a stored name. Unknown names are a corruption, not a client error.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "message_created" => ChangeKind::MessageCreated,
            "message_updated" => ChangeKind::MessageUpdated,
            "message_deleted" => ChangeKind::MessageDeleted,
            "message_moved" => ChangeKind::MessageMoved,
            "folder_created" => ChangeKind::FolderCreated,
            "folder_updated" => ChangeKind::FolderUpdated,
            "folder_deleted" => ChangeKind::FolderDeleted,
            "draft_created" => ChangeKind::DraftCreated,
            "draft_updated" => ChangeKind::DraftUpdated,
            "draft_deleted" => ChangeKind::DraftDeleted,
            _ => return None,
        })
    }

    /// Whether the change concerns a message (and therefore carries `message_id`).
    pub fn is_message(self) -> bool {
        matches!(
            self,
            ChangeKind::MessageCreated
                | ChangeKind::MessageUpdated
                | ChangeKind::MessageDeleted
                | ChangeKind::MessageMoved
        )
    }

    /// Whether the change concerns a folder.
    pub fn is_folder(self) -> bool {
        matches!(
            self,
            ChangeKind::FolderCreated | ChangeKind::FolderUpdated | ChangeKind::FolderDeleted
        )
    }

    /// Whether the change concerns a draft.
    pub fn is_draft(self) -> bool {
        matches!(
            self,
            ChangeKind::DraftCreated | ChangeKind::DraftUpdated | ChangeKind::DraftDeleted
        )
    }
}

/// One change, in the shape a client applies it.
///
/// Fields are optional because the kind decides which of them are meaningful; the
/// client switches on `kind` and ignores the rest. `payload` carries anything the
/// typed fields do not cover, so the vocabulary can grow without a protocol bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncChange {
    /// The cursor value of this change. Clients apply in ascending `seq` order.
    pub seq: i64,
    /// What happened.
    ///
    /// Serialised as `type`, which is the field name [`docs/fcp.md`](../../docs/fcp.md)
    /// §3 documents on the wire. The Rust name stays `kind` because `type` is a
    /// keyword.
    #[serde(rename = "type")]
    pub kind: ChangeKind,
    /// The affected message, when the change concerns one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// The IMAP UID, so a client that also speaks IMAP can correlate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<i64>,
    /// The folder the change happened in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<MailboxId>,
    /// The account the change belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailbox_id: Option<MailboxId>,
    /// The canonical flag string after the change, e.g. `seen flagged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<String>,
    /// For `message_moved`: where it came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_folder_id: Option<MailboxId>,
    /// For `message_moved`: where it went.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_folder_id: Option<MailboxId>,
    /// For folder changes: the folder name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// For `message_deleted`: whether the row is gone rather than trashed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permanent: Option<bool>,
    /// For draft changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_id: Option<DraftId>,
    /// Anything else the change wants to say.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub payload: serde_json::Value,
}

impl SyncChange {
    /// Build the minimal change for a message-level event.
    pub fn message(seq: i64, kind: ChangeKind, message_id: MessageId) -> Self {
        SyncChange {
            seq,
            kind,
            message_id: Some(message_id),
            uid: None,
            folder_id: None,
            mailbox_id: None,
            flags: None,
            from_folder_id: None,
            to_folder_id: None,
            name: None,
            permanent: None,
            draft_id: None,
            payload: serde_json::Value::Null,
        }
    }

    /// Attach a folder.
    pub fn in_folder(mut self, folder_id: MailboxId) -> Self {
        self.folder_id = Some(folder_id);
        self
    }

    /// Attach the account.
    pub fn in_mailbox(mut self, mailbox_id: MailboxId) -> Self {
        self.mailbox_id = Some(mailbox_id);
        self
    }

    /// Attach the IMAP UID.
    pub fn with_uid(mut self, uid: i64) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Attach a flag string.
    pub fn with_flags(mut self, flags: impl Into<String>) -> Self {
        self.flags = Some(flags.into());
        self
    }

    /// Attach a folder name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Attach a payload.
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// This change's cursor.
    pub fn cursor(&self) -> Cursor {
        Cursor(self.seq)
    }

    /// Rebuild a change from a stored `change_log` row.
    ///
    /// The stored row carries the typed ids in its own columns and everything else
    /// in `payload`, so this is the inverse of what [`crate::SyncService`] writes.
    pub fn from_row(row: &ChangeLogEntry) -> Option<Self> {
        let kind = ChangeKind::parse(&row.kind)?;
        let payload = &row.payload;

        let get_i64 = |key: &str| payload.get(key).and_then(serde_json::Value::as_i64);
        let get_str = |key: &str| {
            payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let get_bool = |key: &str| payload.get(key).and_then(serde_json::Value::as_bool);

        Some(SyncChange {
            seq: row.seq,
            kind,
            message_id: row.message_id.map(MessageId::new),
            uid: get_i64("uid"),
            folder_id: row.folder_id.map(MailboxId::new),
            mailbox_id: row.mailbox_id.map(MailboxId::new),
            flags: get_str("flags"),
            from_folder_id: get_i64("from_folder_id").map(MailboxId::new),
            to_folder_id: get_i64("to_folder_id").map(MailboxId::new),
            name: get_str("name"),
            permanent: get_bool("permanent"),
            draft_id: get_i64("draft_id").map(DraftId::new),
            payload: payload.clone(),
        })
    }
}

/// One page of changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPage {
    /// The cursor to store once every change in this page has been applied.
    ///
    /// When `changes` is empty this equals the requested cursor (or the latest
    /// cursor, so a client that has caught up stops asking).
    pub next_cursor: Cursor,
    /// Whether more changes are waiting.
    pub has_more: bool,
    /// The newest cursor the server holds for this user. A client can compare it
    /// with its own to know how far behind it is.
    pub latest_cursor: Cursor,
    /// The changes, ordered by `seq`, ascending and gapless.
    pub changes: Vec<SyncChange>,
}

impl SyncPage {
    /// An empty page that leaves the client where it was.
    pub fn empty(cursor: Cursor, latest: Cursor) -> Self {
        SyncPage {
            next_cursor: cursor,
            has_more: false,
            latest_cursor: latest,
            changes: Vec::new(),
        }
    }

    /// How many changes this page carries.
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Whether the page is empty.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn row(kind: &str, payload: serde_json::Value) -> ChangeLogEntry {
        ChangeLogEntry {
            seq: 42,
            user_id: 7,
            mailbox_id: Some(3),
            folder_id: Some(5),
            message_id: Some(4821),
            kind: kind.to_string(),
            payload,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in [
            ChangeKind::MessageCreated,
            ChangeKind::MessageUpdated,
            ChangeKind::MessageDeleted,
            ChangeKind::MessageMoved,
            ChangeKind::FolderCreated,
            ChangeKind::FolderUpdated,
            ChangeKind::FolderDeleted,
            ChangeKind::DraftCreated,
            ChangeKind::DraftUpdated,
            ChangeKind::DraftDeleted,
        ] {
            assert_eq!(ChangeKind::parse(kind.as_str()), Some(kind), "{kind:?}");
        }
        assert_eq!(ChangeKind::parse("nonsense"), None);
    }

    #[test]
    fn kind_classification() {
        assert!(ChangeKind::MessageMoved.is_message());
        assert!(!ChangeKind::MessageMoved.is_folder());
        assert!(ChangeKind::FolderDeleted.is_folder());
        assert!(ChangeKind::DraftUpdated.is_draft());
        assert!(!ChangeKind::DraftUpdated.is_message());
    }

    #[test]
    fn builders_and_cursor() {
        let change = SyncChange::message(17, ChangeKind::MessageCreated, MessageId::new(9))
            .in_folder(MailboxId::new(5))
            .in_mailbox(MailboxId::new(3))
            .with_uid(117)
            .with_flags("seen")
            .with_name("INBOX");
        assert_eq!(change.cursor(), Cursor(17));
        assert_eq!(change.message_id, Some(MessageId::new(9)));
        assert_eq!(change.uid, Some(117));
        assert_eq!(change.flags.as_deref(), Some("seen"));
    }

    #[test]
    fn wire_shape_matches_the_documented_protocol() {
        let change = SyncChange::message(1836, ChangeKind::MessageCreated, MessageId::new(4821))
            .in_folder(MailboxId::new(5))
            .with_uid(117);
        let json = serde_json::to_value(&change).unwrap();

        assert_eq!(json["seq"], 1836);
        assert_eq!(json["type"], "message_created");
        assert_eq!(json["message_id"], 4821);
        assert_eq!(json["folder_id"], 5);
        assert_eq!(json["uid"], 117);
        // Absent fields must not be serialised as null: the client checks presence.
        assert!(json.get("flags").is_none(), "{json}");
        assert!(json.get("draft_id").is_none(), "{json}");
    }

    #[test]
    fn deleted_change_carries_permanence() {
        let mut change = SyncChange::message(1838, ChangeKind::MessageDeleted, MessageId::new(4712));
        change.permanent = Some(true);
        let json = serde_json::to_value(&change).unwrap();
        assert_eq!(json["type"], "message_deleted");
        assert_eq!(json["permanent"], true);
    }

    #[test]
    fn moved_change_carries_both_folders() {
        let mut change = SyncChange::message(1839, ChangeKind::MessageMoved, MessageId::new(4700));
        change.from_folder_id = Some(MailboxId::new(5));
        change.to_folder_id = Some(MailboxId::new(6));
        let json = serde_json::to_value(&change).unwrap();
        assert_eq!(json["from_folder_id"], 5);
        assert_eq!(json["to_folder_id"], 6);
    }

    #[test]
    fn from_row_rebuilds_every_field() {
        let entry = row(
            "message_moved",
            serde_json::json!({ "uid": 117, "flags": "seen", "from_folder_id": 5, "to_folder_id": 6 }),
        );
        let change = SyncChange::from_row(&entry).unwrap();
        assert_eq!(change.seq, 42);
        assert_eq!(change.kind, ChangeKind::MessageMoved);
        assert_eq!(change.message_id, Some(MessageId::new(4821)));
        assert_eq!(change.mailbox_id, Some(MailboxId::new(3)));
        assert_eq!(change.folder_id, Some(MailboxId::new(5)));
        assert_eq!(change.uid, Some(117));
        assert_eq!(change.flags.as_deref(), Some("seen"));
        assert_eq!(change.from_folder_id, Some(MailboxId::new(5)));
        assert_eq!(change.to_folder_id, Some(MailboxId::new(6)));
    }

    #[test]
    fn from_row_rejects_an_unknown_kind() {
        assert!(SyncChange::from_row(&row("teleported", serde_json::json!({}))).is_none());
    }

    #[test]
    fn from_row_round_trips_through_serde() {
        let entry = row("message_created", serde_json::json!({ "uid": 3 }));
        let change = SyncChange::from_row(&entry).unwrap();
        let json = serde_json::to_string(&change).unwrap();
        let back: SyncChange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, change);
    }

    #[test]
    fn page_helpers() {
        let page = SyncPage::empty(Cursor(10), Cursor(20));
        assert!(page.is_empty());
        assert_eq!(page.len(), 0);
        assert_eq!(page.next_cursor, Cursor(10));
        assert_eq!(page.latest_cursor, Cursor(20));
        assert!(!page.has_more);
    }
}
