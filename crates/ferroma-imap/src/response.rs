//! IMAP responses: untagged data, tagged completions, continuations and
//! response codes (RFC 3501 §7).
//!
//! Every response this server sends is built through a typed constructor in this
//! module — no call site concatenates a response string by hand. That is what
//! keeps `\r\n` universal, quoting correct, and the optional `[RESPONSE-CODE]`
//! in the right place.
//!
//! # Shape
//!
//! ```text
//! * 12 EXISTS                     untagged data
//! * OK [UNSEEN 3] Message 3 is first unseen
//! + Ready for literal data        continuation
//! a001 OK FETCH completed         tagged completion
//! ```

use std::fmt;

use crate::util::{self, NString};

/// A response code: the bracketed `[ ... ]` part of a status response.
///
/// RFC 3501 §7.1 defines these; §7.1 also allows a server-defined
/// `ATOM [SP text]` code, which [`ResponseCode::Other`] covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseCode {
    /// `[ALERT]` — a human-readable message the client must show.
    Alert,
    /// `[BADCHARSET (…)]` — the charsets the server supports.
    BadCharset(Vec<String>),
    /// `[CAPABILITY …]` — the capability list, echoed in the greeting.
    Capability(Vec<String>),
    /// `[PARSE]` — the message could not be parsed (extension).
    Parse,
    /// `[PERMANENTFLAGS (…)]` — flags that can be set permanently.
    PermanentFlags(Vec<String>),
    /// `[READ-ONLY]` — the mailbox is open read-only.
    ReadOnly,
    /// `[READ-WRITE]` — the mailbox is open read-write.
    ReadWrite,
    /// `[TRYCREATE]` — the destination mailbox does not exist.
    TryCreate,
    /// `[UIDNEXT n]` — the next UID the mailbox will hand out.
    UidNext(u64),
    /// `[UIDVALIDITY n]` — the mailbox's UID validity value.
    UidValidity(u64),
    /// `[UNSEEN n]` — the sequence number of the first unseen message.
    Unseen(u64),
    /// `[OVERQUOTA]` — the mailbox quota is exhausted (RFC 9208 style).
    OverQuota,
    /// `[NONEXISTENT]` — the referenced mailbox does not exist.
    NonExistent,
    /// `[ALREADYEXISTS]` — the mailbox already exists.
    AlreadyExists,
    /// `[CLOSED]` — the session's selected mailbox was closed.
    Closed,
    /// `[UIDNOTSTICKY]` — the mailbox does not support persistent UIDs.
    UidNotSticky,
    /// `[APPENDUID uidvalidity uid]` — result of `APPEND` (RFC 4315).
    AppendUid(u64, u64),
    /// `[COPYUID uidvalidity src-uids dst-uids]` — result of `COPY`/`MOVE`.
    CopyUid(u64, String, String),
    /// `[HIGHESTMODSEQ n]` — the highest modification sequence.
    HighestModSeq(u64),
    /// A server-defined code.
    Other(String),
}

impl fmt::Display for ResponseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResponseCode::Alert => f.write_str("ALERT"),
            ResponseCode::BadCharset(charsets) => {
                write!(f, "BADCHARSET ({})", charsets.join(" "))
            }
            ResponseCode::Capability(caps) => write!(f, "CAPABILITY {}", caps.join(" ")),
            ResponseCode::Parse => f.write_str("PARSE"),
            ResponseCode::PermanentFlags(flags) => {
                write!(f, "PERMANENTFLAGS ({})", flags.join(" "))
            }
            ResponseCode::ReadOnly => f.write_str("READ-ONLY"),
            ResponseCode::ReadWrite => f.write_str("READ-WRITE"),
            ResponseCode::TryCreate => f.write_str("TRYCREATE"),
            ResponseCode::UidNext(next) => write!(f, "UIDNEXT {next}"),
            ResponseCode::UidValidity(validity) => write!(f, "UIDVALIDITY {validity}"),
            ResponseCode::Unseen(seq) => write!(f, "UNSEEN {seq}"),
            ResponseCode::OverQuota => f.write_str("OVERQUOTA"),
            ResponseCode::NonExistent => f.write_str("NONEXISTENT"),
            ResponseCode::AlreadyExists => f.write_str("ALREADYEXISTS"),
            ResponseCode::Closed => f.write_str("CLOSED"),
            ResponseCode::UidNotSticky => f.write_str("UIDNOTSTICKY"),
            ResponseCode::AppendUid(validity, uid) => write!(f, "APPENDUID {validity} {uid}"),
            ResponseCode::CopyUid(validity, src, dst) => {
                write!(f, "COPYUID {validity} {src} {dst}")
            }
            ResponseCode::HighestModSeq(modseq) => write!(f, "HIGHESTMODSEQ {modseq}"),
            ResponseCode::Other(text) => f.write_str(text),
        }
    }
}

/// An untagged status prefix: `* OK`, `* NO`, `* BAD`, `* BYE`, `* PREAUTH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `OK` — success.
    Ok,
    /// `NO` — a failure the client caused (non-fatal).
    No,
    /// `BAD` — a protocol error.
    Bad,
    /// `BYE` — the connection is closing.
    Bye,
    /// `PREAUTH` — already authenticated by external means.
    PreAuth,
}

impl Status {
    /// The wire keyword.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::No => "NO",
            Status::Bad => "BAD",
            Status::Bye => "BYE",
            Status::PreAuth => "PREAUTH",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One mailbox/folder state, as `STATUS` and `LIST` report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusItems {
    /// `MESSAGES` — total messages.
    pub messages: u64,
    /// `RECENT` — messages with the `\Recent` flag.
    pub recent: u64,
    /// `UIDNEXT` — next UID.
    pub uidnext: u64,
    /// `UIDVALIDITY` — UID validity value.
    pub uidvalidity: u64,
    /// `UNSEEN` — number of unseen messages.
    pub unseen: u64,
    /// `SIZE` — total size in bytes (RFC 8438).
    pub size: Option<u64>,
}

impl StatusItems {
    /// Render the items in the fixed order RFC 3501 §7.2.1 uses, restricted to
    /// the ones the client actually asked for.
    pub fn render(&self, wanted: &[StatusItem]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for item in wanted {
            match item {
                StatusItem::Messages => parts.push(format!("MESSAGES {}", self.messages)),
                StatusItem::Recent => parts.push(format!("RECENT {}", self.recent)),
                StatusItem::UidNext => parts.push(format!("UIDNEXT {}", self.uidnext)),
                StatusItem::UidValidity => parts.push(format!("UIDVALIDITY {}", self.uidvalidity)),
                StatusItem::Unseen => parts.push(format!("UNSEEN {}", self.unseen)),
                StatusItem::Size => {
                    if let Some(size) = self.size {
                        parts.push(format!("SIZE {size}"));
                    }
                }
            }
        }
        parts.join(" ")
    }
}

/// One item of a `STATUS` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusItem {
    /// `MESSAGES`.
    Messages,
    /// `RECENT`.
    Recent,
    /// `UIDNEXT`.
    UidNext,
    /// `UIDVALIDITY`.
    UidValidity,
    /// `UNSEEN`.
    Unseen,
    /// `SIZE` (RFC 8438).
    Size,
}

/// A complete response, ready to be written to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// `* <data>` — untagged data.
    Untagged(String),
    /// `<tag> OK/NO/BAD [code] text`.
    Tagged {
        /// The command tag.
        tag: String,
        /// The completion status.
        status: Status,
        /// The optional response code.
        code: Option<ResponseCode>,
        /// The human-readable text.
        text: String,
    },
    /// `+ text` — a continuation request.
    Continuation(String),
}

impl Response {
    /// An untagged line, given the part after `* `.
    pub fn untagged(body: impl Into<String>) -> Self {
        Response::Untagged(body.into())
    }

    /// `* OK [code] text`.
    pub fn untagged_ok(text: impl Into<String>, code: Option<ResponseCode>) -> Self {
        Response::untagged(render_status(Status::Ok, code.as_ref(), &text.into()))
    }

    /// `* NO [code] text`.
    pub fn untagged_no(text: impl Into<String>, code: Option<ResponseCode>) -> Self {
        Response::untagged(render_status(Status::No, code.as_ref(), &text.into()))
    }

    /// `* BAD [code] text`.
    pub fn untagged_bad(text: impl Into<String>, code: Option<ResponseCode>) -> Self {
        Response::untagged(render_status(Status::Bad, code.as_ref(), &text.into()))
    }

    /// `* BYE [code] text`.
    pub fn bye(text: impl Into<String>, code: Option<ResponseCode>) -> Self {
        Response::untagged(render_status(Status::Bye, code.as_ref(), &text.into()))
    }

    /// `* PREAUTH text`.
    pub fn preauth(text: impl Into<String>) -> Self {
        Response::untagged(format!("PREAUTH {}", text.into()))
    }

    /// `<tag> OK [code] text`.
    pub fn tagged_ok(tag: &str, text: impl Into<String>, code: Option<ResponseCode>) -> Self {
        Response::Tagged {
            tag: tag.to_string(),
            status: Status::Ok,
            code,
            text: text.into(),
        }
    }

    /// `<tag> NO [code] text`.
    pub fn tagged_no(tag: &str, text: impl Into<String>, code: Option<ResponseCode>) -> Self {
        Response::Tagged {
            tag: tag.to_string(),
            status: Status::No,
            code,
            text: text.into(),
        }
    }

    /// `<tag> BAD [code] text`.
    pub fn tagged_bad(tag: &str, text: impl Into<String>, code: Option<ResponseCode>) -> Self {
        Response::Tagged {
            tag: tag.to_string(),
            status: Status::Bad,
            code,
            text: text.into(),
        }
    }

    /// `+ text`.
    pub fn continuation(text: impl Into<String>) -> Self {
        Response::Continuation(text.into())
    }

    // -----------------------------------------------------------------------
    // Untagged data constructors.
    // -----------------------------------------------------------------------

    /// `* <n> EXISTS`.
    pub fn exists(count: usize) -> Self {
        Response::untagged(format!("{count} EXISTS"))
    }

    /// `* <n> RECENT`.
    pub fn recent(count: usize) -> Self {
        Response::untagged(format!("{count} RECENT"))
    }

    /// `* <n> EXPUNGE`.
    pub fn expunge(seq: usize) -> Self {
        Response::untagged(format!("{seq} EXPUNGE"))
    }

    /// `* <n> FETCH (<items>)`.
    pub fn fetch(seq: usize, items: &str) -> Self {
        Response::untagged(format!("{seq} FETCH ({items})"))
    }

    /// `* <n> FETCH (UID <uid> <items>)`.
    pub fn uid_fetch(seq: usize, uid: u64, items: &str) -> Self {
        if items.is_empty() {
            Response::untagged(format!("{seq} FETCH (UID {uid})"))
        } else {
            Response::untagged(format!("{seq} FETCH (UID {uid} {items})"))
        }
    }

    /// `* FLAGS (…)`.
    pub fn flags(flags: &[String]) -> Self {
        Response::untagged(format!("FLAGS ({})", flags.join(" ")))
    }

    /// `* OK [PERMANENTFLAGS (…)] text`.
    pub fn permanent_flags(flags: &[String]) -> Self {
        Response::untagged_ok(
            "Permanent flags",
            Some(ResponseCode::PermanentFlags(flags.to_vec())),
        )
    }

    /// `* OK [UIDVALIDITY n] text`.
    pub fn uid_validity(validity: u64) -> Self {
        Response::untagged_ok("UIDVALIDITY", Some(ResponseCode::UidValidity(validity)))
    }

    /// `* OK [UIDNEXT n] text`.
    pub fn uid_next(next: u64) -> Self {
        Response::untagged_ok("UIDNEXT", Some(ResponseCode::UidNext(next)))
    }

    /// `* OK [UNSEEN n] text`.
    pub fn unseen(seq: u64) -> Self {
        Response::untagged_ok("UNSEEN", Some(ResponseCode::Unseen(seq)))
    }

    /// `* LIST (attrs) "/" name`.
    ///
    /// The name is rendered with the shortest safe form: an atom where legal, a
    /// quoted string for names with spaces, and a synchronising literal for names
    /// that cannot be quoted.
    pub fn list(attributes: &[&str], delimiter: Option<char>, name: &str) -> Self {
        Response::untagged(format!(
            "LIST ({}) {} {}",
            attributes.join(" "),
            delimiter_nstring(delimiter),
            NString::of(&crate::mailbox::canonical(name))
        ))
    }

    /// `* LSUB (attrs) "/" name`.
    pub fn lsub(attributes: &[&str], delimiter: Option<char>, name: &str) -> Self {
        Response::untagged(format!(
            "LSUB ({}) {} {}",
            attributes.join(" "),
            delimiter_nstring(delimiter),
            NString::of(&crate::mailbox::canonical(name))
        ))
    }

    /// `* <n> STATUS name (items)`.
    pub fn status(seq: usize, name: &str, items: &str) -> Self {
        Response::untagged(format!(
            "{seq} STATUS {} ({items})",
            NString::of(&crate::mailbox::canonical(name))
        ))
    }

    /// `* STATUS name (items)` — the untagged form `STATUS` actually uses.
    pub fn mailbox_status(name: &str, items: &str) -> Self {
        Response::untagged(format!(
            "STATUS {} ({items})",
            NString::of(&crate::mailbox::canonical(name))
        ))
    }

    /// `* SEARCH n n n` (or `* SEARCH` with no results).
    pub fn search(ids: &[u64]) -> Self {
        let mut body = String::from("SEARCH");
        for id in ids {
            body.push(' ');
            body.push_str(&id.to_string());
        }
        Response::untagged(body)
    }

    /// `* NAMESPACE (("" "/")) NIL NIL` (RFC 2342).
    ///
    /// Rust does not allow two methods with the same name in one `impl` block,
    /// so this is the only `namespace` constructor.
    pub fn namespace(
        personal: &[(&str, char)],
        other: &[(&str, char)],
        shared: &[(&str, char)],
    ) -> Self {
        Response::untagged(format!(
            "NAMESPACE {} {} {}",
            render_namespace(personal),
            render_namespace(other),
            render_namespace(shared)
        ))
    }

    /// `* CAPABILITY …`.
    pub fn capability(capabilities: &[String]) -> Self {
        Response::untagged(format!("CAPABILITY {}", capabilities.join(" ")))
    }

    /// `* BYE [ALERT] text` — the server is disconnecting.
    pub fn bye_alert(text: impl Into<String>) -> Self {
        Response::bye(text, Some(ResponseCode::Alert))
    }

    /// The complete wire form, ending in CRLF.
    pub fn to_wire(&self) -> String {
        let mut out = String::new();
        match self {
            Response::Untagged(body) => {
                out.push_str("* ");
                out.push_str(body);
            }
            Response::Tagged {
                tag,
                status,
                code,
                text,
            } => {
                out.push_str(tag);
                out.push(' ');
                out.push_str(&render_status(*status, code.as_ref(), text));
            }
            Response::Continuation(text) => {
                out.push_str("+ ");
                out.push_str(text);
            }
        }
        out.push_str("\r\n");
        out
    }

    /// The response's status keyword, for structured logging.
    pub fn status_keyword(&self) -> &'static str {
        match self {
            Response::Untagged(_) => "*",
            Response::Continuation(_) => "+",
            Response::Tagged { status, .. } => status.as_str(),
        }
    }
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_wire())
    }
}

/// Render the `<STATUS> [<code>] <text>` tail both tagged and untagged
/// responses share.
fn render_status(status: Status, code: Option<&ResponseCode>, text: &str) -> String {
    let mut out = status.as_str().to_string();
    if let Some(code) = code {
        out.push(' ');
        out.push('[');
        out.push_str(&code.to_string());
        out.push(']');
    }
    if !text.is_empty() {
        out.push(' ');
        out.push_str(text);
    }
    out
}

/// The delimiter as an `nstring`: `"/"` or `NIL` when the namespace is flat.
pub fn delimiter_nstring(delimiter: Option<char>) -> String {
    match delimiter {
        Some(ch) => util::quote(&ch.to_string()),
        None => "NIL".to_string(),
    }
}

/// Render one `NAMESPACE` component list: `NIL` when empty.
fn render_namespace(entries: &[(&str, char)]) -> String {
    if entries.is_empty() {
        return "NIL".to_string();
    }
    let rendered: Vec<String> = entries
        .iter()
        .map(|(prefix, delimiter)| {
            format!(
                "({} {})",
                util::quote(prefix),
                util::quote(&delimiter.to_string())
            )
        })
        .collect();
    format!("({})", rendered.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_codes_render_exactly() {
        assert_eq!(ResponseCode::Alert.to_string(), "ALERT");
        assert_eq!(
            ResponseCode::Capability(vec!["IMAP4rev1".into(), "IDLE".into()]).to_string(),
            "CAPABILITY IMAP4rev1 IDLE"
        );
        assert_eq!(
            ResponseCode::PermanentFlags(vec!["\\Seen".into(), "\\Deleted".into()]).to_string(),
            "PERMANENTFLAGS (\\Seen \\Deleted)"
        );
        assert_eq!(ResponseCode::ReadOnly.to_string(), "READ-ONLY");
        assert_eq!(ResponseCode::ReadWrite.to_string(), "READ-WRITE");
        assert_eq!(ResponseCode::TryCreate.to_string(), "TRYCREATE");
        assert_eq!(ResponseCode::UidNext(7).to_string(), "UIDNEXT 7");
        assert_eq!(ResponseCode::UidValidity(99).to_string(), "UIDVALIDITY 99");
        assert_eq!(ResponseCode::Unseen(2).to_string(), "UNSEEN 2");
        assert_eq!(ResponseCode::OverQuota.to_string(), "OVERQUOTA");
        assert_eq!(ResponseCode::Parse.to_string(), "PARSE");
        assert_eq!(
            ResponseCode::BadCharset(vec!["US-ASCII".into(), "UTF-8".into()]).to_string(),
            "BADCHARSET (US-ASCII UTF-8)"
        );
        assert_eq!(ResponseCode::AppendUid(1, 5).to_string(), "APPENDUID 1 5");
        assert_eq!(
            ResponseCode::CopyUid(1, "1:3".into(), "5:7".into()).to_string(),
            "COPYUID 1 1:3 5:7"
        );
        assert_eq!(ResponseCode::HighestModSeq(4).to_string(), "HIGHESTMODSEQ 4");
        assert_eq!(ResponseCode::Other("X-THING".into()).to_string(), "X-THING");
        assert_eq!(ResponseCode::NonExistent.to_string(), "NONEXISTENT");
        assert_eq!(ResponseCode::AlreadyExists.to_string(), "ALREADYEXISTS");
        assert_eq!(ResponseCode::Closed.to_string(), "CLOSED");
        assert_eq!(ResponseCode::UidNotSticky.to_string(), "UIDNOTSTICKY");
    }

    #[test]
    fn status_keywords_are_the_five_rfc_names() {
        assert_eq!(Status::Ok.as_str(), "OK");
        assert_eq!(Status::No.as_str(), "NO");
        assert_eq!(Status::Bad.as_str(), "BAD");
        assert_eq!(Status::Bye.as_str(), "BYE");
        assert_eq!(Status::PreAuth.as_str(), "PREAUTH");
        assert_eq!(Status::Ok.to_string(), "OK");
    }

    #[test]
    fn untagged_lines_start_with_star_and_end_with_crlf() {
        assert_eq!(Response::exists(3).to_wire(), "* 3 EXISTS\r\n");
        assert_eq!(Response::recent(1).to_wire(), "* 1 RECENT\r\n");
        assert_eq!(Response::expunge(4).to_wire(), "* 4 EXPUNGE\r\n");
        assert_eq!(Response::search(&[]).to_wire(), "* SEARCH\r\n");
        assert_eq!(Response::search(&[1, 4, 9]).to_wire(), "* SEARCH 1 4 9\r\n");
    }

    #[test]
    fn fetch_lines_render_flags_and_uid() {
        assert_eq!(
            Response::fetch(1, "FLAGS (\\Seen)").to_wire(),
            "* 1 FETCH (FLAGS (\\Seen))\r\n"
        );
        assert_eq!(
            Response::uid_fetch(2, 17, "FLAGS (\\Seen)").to_wire(),
            "* 2 FETCH (UID 17 FLAGS (\\Seen))\r\n"
        );
        assert_eq!(Response::uid_fetch(2, 17, "").to_wire(), "* 2 FETCH (UID 17)\r\n");
    }

    #[test]
    fn flags_and_permanent_flags_render_the_list() {
        let flags = vec!["\\Answered".to_string(), "\\Flagged".to_string()];
        assert_eq!(Response::flags(&flags).to_wire(), "* FLAGS (\\Answered \\Flagged)\r\n");
        assert_eq!(
            Response::permanent_flags(&flags).to_wire(),
            "* OK [PERMANENTFLAGS (\\Answered \\Flagged)] Permanent flags\r\n"
        );
    }

    #[test]
    fn select_metadata_responses_match_rfc_3501() {
        assert_eq!(
            Response::uid_validity(1234).to_wire(),
            "* OK [UIDVALIDITY 1234] UIDVALIDITY\r\n"
        );
        assert_eq!(
            Response::uid_next(4321).to_wire(),
            "* OK [UIDNEXT 4321] UIDNEXT\r\n"
        );
        assert_eq!(Response::unseen(5).to_wire(), "* OK [UNSEEN 5] UNSEEN\r\n");
    }

    #[test]
    fn tagged_responses_render_tag_status_code_text() {
        assert_eq!(
            Response::tagged_ok("a001", "LOGIN completed", None).to_wire(),
            "a001 OK LOGIN completed\r\n"
        );
        assert_eq!(
            Response::tagged_no("a002", "No such mailbox", Some(ResponseCode::NonExistent))
                .to_wire(),
            "a002 NO [NONEXISTENT] No such mailbox\r\n"
        );
        assert_eq!(
            Response::tagged_bad("a003", "Unknown command", None).to_wire(),
            "a003 BAD Unknown command\r\n"
        );
        assert_eq!(
            Response::tagged_ok("a004", "", Some(ResponseCode::ReadOnly)).to_wire(),
            "a004 OK [READ-ONLY]\r\n"
        );
    }

    #[test]
    fn tagged_ok_with_trycreate_and_overquota() {
        assert_eq!(
            Response::tagged_no("a1", "Mailbox does not exist", Some(ResponseCode::TryCreate))
                .to_wire(),
            "a1 NO [TRYCREATE] Mailbox does not exist\r\n"
        );
        assert_eq!(
            Response::tagged_no("a2", "Quota exceeded", Some(ResponseCode::OverQuota)).to_wire(),
            "a2 NO [OVERQUOTA] Quota exceeded\r\n"
        );
    }

    #[test]
    fn continuation_and_bye() {
        assert_eq!(Response::continuation("Ready for literal data").to_wire(), "+ Ready for literal data\r\n");
        assert_eq!(Response::continuation("").to_wire(), "+ \r\n");
        assert_eq!(Response::bye("bye", None).to_wire(), "* BYE bye\r\n");
        assert_eq!(
            Response::bye_alert("server shutting down").to_wire(),
            "* BYE [ALERT] server shutting down\r\n"
        );
        assert_eq!(Response::preauth("trusted").to_wire(), "* PREAUTH trusted\r\n");
    }

    #[test]
    fn capabability_line_is_untagged() {
        let caps = vec!["IMAP4rev1".to_string(), "LITERAL+".to_string()];
        assert_eq!(
            Response::capability(&caps).to_wire(),
            "* CAPABILITY IMAP4rev1 LITERAL+\r\n"
        );
    }

    #[test]
    fn list_quotes_only_when_needed() {
        assert_eq!(
            Response::list(&["\\HasNoChildren"], Some('/'), "INBOX").to_wire(),
            "* LIST (\\HasNoChildren) \"/\" INBOX\r\n"
        );
        assert_eq!(
            Response::list(&[], Some('/'), "My Folder").to_wire(),
            "* LIST () \"/\" \"My Folder\"\r\n"
        );
        assert_eq!(
            Response::list(&["\\Noselect"], Some('/'), "").to_wire(),
            "* LIST (\\Noselect) \"/\" \"\"\r\n"
        );
        assert_eq!(
            Response::list(&[], None, "Flat").to_wire(),
            "* LIST () NIL Flat\r\n"
        );
    }

    #[test]
    fn list_canonicalises_inbox_but_leaves_other_names_alone() {
        assert_eq!(
            Response::list(&[], Some('/'), "inbox").to_wire(),
            "* LIST () \"/\" INBOX\r\n"
        );
        assert_eq!(
            Response::list(&[], Some('/'), "sent").to_wire(),
            "* LIST () \"/\" sent\r\n"
        );
    }

    #[test]
    fn list_uses_a_literal_for_non_ascii_names() {
        // `日本語` is 9 UTF-8 bytes, so `{9}` is correct on the wire.
        assert_eq!(
            Response::list(&[], Some('/'), "日本語").to_wire(),
            "* LIST () \"/\" {9}\r\n日本語\r\n"
        );
    }

    #[test]
    fn list_uses_a_literal_for_names_with_newlines() {
        // A name with CR/LF can only be transmitted as a literal.
        assert_eq!(
            Response::list(&[], Some('/'), "a\r\nb").to_wire(),
            "* LIST () \"/\" {4}\r\na\r\nb\r\n"
        );
    }

    #[test]
    fn lsub_mirrors_list() {
        assert_eq!(
            Response::lsub(&["\\HasChildren"], Some('/'), "Archive").to_wire(),
            "* LSUB (\\HasChildren) \"/\" Archive\r\n"
        );
    }

    #[test]
    fn status_renders_the_requested_items_in_order() {
        let items = StatusItems {
            messages: 12,
            recent: 1,
            uidnext: 40,
            uidvalidity: 99,
            unseen: 3,
            size: Some(4096),
        };
        assert_eq!(
            items.render(&[
                StatusItem::Messages,
                StatusItem::Recent,
                StatusItem::UidNext,
                StatusItem::UidValidity,
                StatusItem::Unseen
            ]),
            "MESSAGES 12 RECENT 1 UIDNEXT 40 UIDVALIDITY 99 UNSEEN 3"
        );
        // The order the client asked for is the order it gets back.
        assert_eq!(items.render(&[StatusItem::Size]), "SIZE 4096");
    }

    #[test]
    fn status_omits_size_when_unknown() {
        let items = StatusItems {
            messages: 0,
            recent: 0,
            uidnext: 1,
            uidvalidity: 1,
            unseen: 0,
            size: None,
        };
        assert_eq!(items.render(&[StatusItem::Size]), "");
    }

    #[test]
    fn mailbox_status_line_is_untagged() {
        assert_eq!(
            Response::mailbox_status("INBOX", "MESSAGES 2 UNSEEN 1").to_wire(),
            "* STATUS INBOX (MESSAGES 2 UNSEEN 1)\r\n"
        );
        assert_eq!(
            Response::mailbox_status("My Box", "MESSAGES 2").to_wire(),
            "* STATUS \"My Box\" (MESSAGES 2)\r\n"
        );
    }

    #[test]
    fn namespace_renders_the_personal_namespace() {
        assert_eq!(
            Response::namespace(&[("", '/')], &[], &[]).to_wire(),
            "* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n"
        );
        assert_eq!(Response::namespace(&[], &[], &[]).to_wire(), "* NAMESPACE NIL NIL NIL\r\n");
    }

    #[test]
    fn delimiter_nstring_covers_both_forms() {
        assert_eq!(delimiter_nstring(Some('/')), "\"/\"");
        assert_eq!(delimiter_nstring(None), "NIL");
    }

    #[test]
    fn status_keyword_reports_the_class() {
        assert_eq!(Response::exists(1).status_keyword(), "*");
        assert_eq!(Response::continuation("x").status_keyword(), "+");
        assert_eq!(Response::tagged_ok("a", "x", None).status_keyword(), "OK");
        assert_eq!(Response::tagged_no("a", "x", None).status_keyword(), "NO");
        assert_eq!(Response::tagged_bad("a", "x", None).status_keyword(), "BAD");
    }

    #[test]
    fn display_matches_to_wire() {
        let response = Response::exists(2);
        assert_eq!(response.to_string(), response.to_wire());
    }

    #[test]
    fn untagged_ok_no_bad_include_the_code() {
        assert_eq!(
            Response::untagged_ok("Welcome", None).to_wire(),
            "* OK Welcome\r\n"
        );
        assert_eq!(
            Response::untagged_ok("Welcome", Some(ResponseCode::ReadOnly)).to_wire(),
            "* OK [READ-ONLY] Welcome\r\n"
        );
        assert_eq!(
            Response::untagged_no("Nothing here", Some(ResponseCode::NonExistent)).to_wire(),
            "* NO [NONEXISTENT] Nothing here\r\n"
        );
        assert_eq!(
            Response::untagged_bad("Syntax error", Some(ResponseCode::Parse)).to_wire(),
            "* BAD [PARSE] Syntax error\r\n"
        );
    }

    #[test]
    fn every_line_ends_with_crlf_and_never_with_a_bare_lf() {
        let lines = [
            Response::exists(1),
            Response::recent(1),
            Response::expunge(1),
            Response::fetch(1, "FLAGS ()"),
            Response::search(&[1]),
            Response::list(&[], Some('/'), "x"),
            Response::mailbox_status("INBOX", "MESSAGES 0"),
            Response::capability(&["IMAP4rev1".into()]),
            Response::continuation("go"),
            Response::bye("bye", None),
            Response::tagged_ok("t", "done", None),
            Response::tagged_no("t", "no", None),
            Response::tagged_bad("t", "bad", None),
        ];
        for line in lines {
            let wire = line.to_wire();
            assert!(wire.ends_with("\r\n"), "`{wire}` must end with CRLF");
            assert_eq!(
                wire.matches('\n').count(),
                wire.matches("\r\n").count(),
                "`{wire}` must not contain a bare LF"
            );
        }
    }
}
