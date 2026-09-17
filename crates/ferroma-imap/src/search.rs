//! `SEARCH` (RFC 3501 §6.4.4): the key expression and its evaluation.
//!
//! The expression is evaluated **in Rust**, over rows the session already read
//! from the repository, plus (for `BODY`/`TEXT`) the message bytes from the
//! Maildir. That keeps the search contract in one place: the specification §30
//! server-side search and IMAP `SEARCH` then answer the same question the same
//! way, and the SQL layer never has to grow an IMAP-shaped query language.
//!
//! # The two date families
//!
//! `SINCE`/`BEFORE`/`ON` compare the **internal date** — when the *server*
//! received the message. `SENTSINCE`/`SENTBEFORE`/`SENTON` compare the `Date:`
//! header — when the *sender* says they wrote it. Confusing the two is the
//! classic `SEARCH` bug, so both are tested explicitly.

use chrono::{DateTime, NaiveDate, Utc};

use ferroma_core::FerromaError;
use ferroma_mail::Flags;

use crate::sequence::SequenceSet;

/// The charsets this server accepts on `SEARCH`.
///
/// RFC 3501 requires `US-ASCII`; the specification's mail stack is UTF-8
/// throughout, so `UTF-8` is accepted too. Anything else is answered with
/// `NO [BADCHARSET (US-ASCII UTF-8)]`.
pub const SUPPORTED_CHARSETS: [&str; 2] = ["US-ASCII", "UTF-8"];

/// Whether a client-declared charset is one we can evaluate.
pub fn charset_supported(charset: &str) -> bool {
    SUPPORTED_CHARSETS
        .iter()
        .any(|supported| supported.eq_ignore_ascii_case(charset.trim()))
}

/// A node of the `SEARCH` key grammar.
///
/// `NOT` and `OR` are the only operators; everything else is a leaf predicate.
/// A parenthesised list becomes an [`SearchKey::And`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchKey {
    /// `ALL` — every message.
    All,
    /// `ANSWERED`.
    Answered,
    /// `DELETED`.
    Deleted,
    /// `DRAFT`.
    Draft,
    /// `FLAGGED`.
    Flagged,
    /// `NEW` — `\Recent` and not `\Seen`.
    New,
    /// `OLD` — not `\Recent`.
    Old,
    /// `RECENT`.
    Recent,
    /// `SEEN`.
    Seen,
    /// `UNANSWERED`.
    Unanswered,
    /// `UNDELETED`.
    Undeleted,
    /// `UNDRAFT`.
    Undraft,
    /// `UNFLAGGED`.
    Unflagged,
    /// `UNSEEN`.
    Unseen,
    /// `BCC <string>`.
    Bcc(String),
    /// `BODY <string>`.
    Body(String),
    /// `CC <string>`.
    Cc(String),
    /// `FROM <string>`.
    From(String),
    /// `SUBJECT <string>`.
    Subject(String),
    /// `TEXT <string>`.
    Text(String),
    /// `TO <string>`.
    To(String),
    /// `KEYWORD <flag>`.
    Keyword(String),
    /// `UNKEYWORD <flag>`.
    Unkeyword(String),
    /// `BEFORE <date>` — internal date strictly earlier than the date.
    Before(DateTime<Utc>),
    /// `ON <date>` — internal date on the date.
    On(DateTime<Utc>),
    /// `SINCE <date>` — internal date on or after the date.
    Since(DateTime<Utc>),
    /// `SENTBEFORE <date>` — `Date:` header strictly earlier than the date.
    SentBefore(DateTime<Utc>),
    /// `SENTON <date>` — `Date:` header on the date.
    SentOn(DateTime<Utc>),
    /// `SENTSINCE <date>` — `Date:` header on or after the date.
    SentSince(DateTime<Utc>),
    /// `LARGER <n>`.
    Larger(u64),
    /// `SMALLER <n>`.
    Smaller(u64),
    /// `UID <set>`.
    Uid(SequenceSet),
    /// A bare message set — matched against sequence numbers, or against UIDs
    /// under `UID SEARCH`.
    SequenceSet(SequenceSet),
    /// `HEADER <field> <string>`.
    Header(String, String),
    /// `NOT <key>`.
    Not(Box<SearchKey>),
    /// `OR <key> <key>`.
    Or(Box<SearchKey>, Box<SearchKey>),
    /// A parenthesised list of keys (all must match).
    And(Vec<SearchKey>),
}

impl SearchKey {
    /// Whether evaluating this key needs the message body.
    ///
    /// A session uses this to skip the Maildir read for a purely metadata
    /// search, which is the common case (`UNSEEN`, `SINCE`, `FROM`…).
    pub fn needs_body(&self) -> bool {
        match self {
            SearchKey::Body(_) | SearchKey::Text(_) => true,
            SearchKey::Not(inner) => inner.needs_body(),
            SearchKey::Or(left, right) => left.needs_body() || right.needs_body(),
            SearchKey::And(keys) => keys.iter().any(SearchKey::needs_body),
            _ => false,
        }
    }

    /// Whether evaluating this key needs the message headers.
    pub fn needs_headers(&self) -> bool {
        match self {
            SearchKey::Bcc(_)
            | SearchKey::Cc(_)
            | SearchKey::From(_)
            | SearchKey::Subject(_)
            | SearchKey::To(_)
            | SearchKey::Text(_)
            | SearchKey::Header(_, _)
            | SearchKey::SentBefore(_)
            | SearchKey::SentOn(_)
            | SearchKey::SentSince(_) => true,
            SearchKey::Not(inner) => inner.needs_headers(),
            SearchKey::Or(left, right) => left.needs_headers() || right.needs_headers(),
            SearchKey::And(keys) => keys.iter().any(SearchKey::needs_headers),
            _ => false,
        }
    }

    /// The sequence set this key constrains, if any.
    pub fn sequence_set(&self) -> Option<&SequenceSet> {
        match self {
            SearchKey::Uid(set) | SearchKey::SequenceSet(set) => Some(set),
            _ => None,
        }
    }
}

/// Everything `SEARCH` can ask about one message, gathered once per message.
#[derive(Debug, Clone, Default)]
pub struct MessageFacts {
    /// The message's IMAP sequence number in the selected mailbox (1-based).
    pub seq: u64,
    /// The message's UID.
    pub uid: u64,
    /// `INTERNALDATE` — when the server accepted the message.
    pub internal_date: DateTime<Utc>,
    /// The `Date:` header, when the message has a parseable one.
    pub sent_at: Option<DateTime<Utc>>,
    /// `RFC822.SIZE`.
    pub size: u64,
    /// The stored flag set.
    pub flags: Flags,
    /// The raw stored flag string, when the caller has one.
    ///
    /// `messages.flags` is space-separated, lower-cased and backslash-less, so
    /// it is *not* readable by `Flags::parse`. When this is set the evaluator
    /// uses it in preference to [`MessageFacts::flags`], which lets the session
    /// hand over exactly what the database holds.
    pub raw_flags: Option<String>,
    /// Whether the message is `\Recent` for this session.
    pub recent: bool,
    /// Decoded `From:` addresses, space-joined for substring matching.
    pub from: String,
    /// Decoded `To:` addresses.
    pub to: String,
    /// Decoded `Cc:` addresses.
    pub cc: String,
    /// Decoded `Bcc:` addresses.
    pub bcc: String,
    /// The RFC 2047-decoded `Subject:`.
    pub subject: String,
    /// Every top-level header, `Name: value`, unfolded — what `HEADER` and
    /// `TEXT` search.
    pub header_block: String,
}

impl MessageFacts {
    /// A minimal set of facts, for tests and for callers that only have a row.
    pub fn new(seq: u64, uid: u64, internal_date: DateTime<Utc>, size: u64) -> Self {
        MessageFacts {
            seq,
            uid,
            internal_date,
            size,
            ..MessageFacts::default()
        }
    }
}

/// The one-message evaluation context.
///
/// Bundling the decoded body (and its lower-cased form) here means a `SEARCH`
/// with twenty `BODY` keys over one message decodes the body once, not twenty
/// times, and never allocates per key.
struct Eval<'a> {
    facts: &'a MessageFacts,
    body_text: Option<String>,
    /// Whether a bare message set is matched against UIDs (`UID SEARCH`).
    uid_mode: bool,
    /// The largest sequence number in use, for `*` in a message set.
    max_seq: u64,
    /// The largest UID in use, for `*` in a `UID` set.
    max_uid: u64,
}

/// Which system flag a key asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagKind {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
}

impl Eval<'_> {
    /// Whether a system flag is set, reading the raw stored string when the
    /// caller supplied one.
    ///
    /// `messages.flags` is space-separated, lower-cased and backslash-less, so
    /// `Flags::parse` cannot read it; [`MessageFacts::raw_flags`] carries the
    /// column verbatim and is consulted first.
    fn flag(&self, kind: FlagKind) -> bool {
        if let Some(raw) = self.facts.raw_flags.as_deref() {
            let wanted = match kind {
                FlagKind::Seen => "seen",
                FlagKind::Answered => "answered",
                FlagKind::Flagged => "flagged",
                FlagKind::Deleted => "deleted",
                FlagKind::Draft => "draft",
            };
            return raw
                .split_whitespace()
                .any(|token| token.eq_ignore_ascii_case(wanted));
        }
        let flags = &self.facts.flags;
        match kind {
            FlagKind::Seen => flags.seen(),
            FlagKind::Answered => flags.answered(),
            FlagKind::Flagged => flags.flagged(),
            FlagKind::Deleted => flags.deleted(),
            FlagKind::Draft => flags.draft(),
        }
    }

    /// Whether any flag (system or keyword) carries `name`.
    fn has_flag_name(&self, name: &str) -> bool {
        if let Some(raw) = self.facts.raw_flags.as_deref() {
            let wanted = name.trim().trim_start_matches('\\');
            if raw
                .split_whitespace()
                .any(|token| token.eq_ignore_ascii_case(wanted))
            {
                return true;
            }
        }
        has_keyword(&self.facts.flags, name)
    }

    fn contains(haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.to_lowercase().contains(&needle.to_lowercase())
    }

    fn header_block(&self) -> &str {
        &self.facts.header_block
    }

    fn body_text(&self) -> &str {
        self.body_text.as_deref().unwrap_or_default()
    }

    fn evaluate(&mut self, key: &SearchKey) -> bool {
        match key {
            SearchKey::All => true,
            SearchKey::Answered => self.flag(FlagKind::Answered),
            SearchKey::Deleted => self.flag(FlagKind::Deleted),
            SearchKey::Draft => self.flag(FlagKind::Draft),
            SearchKey::Flagged => self.flag(FlagKind::Flagged),
            SearchKey::Seen => self.flag(FlagKind::Seen),
            SearchKey::Recent => self.facts.recent,
            SearchKey::New => self.facts.recent && !self.flag(FlagKind::Seen),
            SearchKey::Old => !self.facts.recent,
            SearchKey::Unanswered => !self.flag(FlagKind::Answered),
            SearchKey::Undeleted => !self.flag(FlagKind::Deleted),
            SearchKey::Undraft => !self.flag(FlagKind::Draft),
            SearchKey::Unflagged => !self.flag(FlagKind::Flagged),
            SearchKey::Unseen => !self.flag(FlagKind::Seen),
            SearchKey::Keyword(name) => self.has_flag_name(name),
            SearchKey::Unkeyword(name) => !self.has_flag_name(name),
            SearchKey::Subject(needle) => Self::contains(&self.facts.subject, needle),
            SearchKey::From(needle) => Self::contains(&self.facts.from, needle),
            SearchKey::To(needle) => Self::contains(&self.facts.to, needle),
            SearchKey::Cc(needle) => Self::contains(&self.facts.cc, needle),
            SearchKey::Bcc(needle) => Self::contains(&self.facts.bcc, needle),
            SearchKey::Header(field, needle) => {
                header_matches(self.header_block(), field, needle)
            }
            SearchKey::Body(needle) => {
                let body = self.body_text().to_string();
                Self::contains(&body, needle)
            }
            // `TEXT` searches the whole message, header block included.
            SearchKey::Text(needle) => {
                let full = format!("{}\r\n\r\n{}", self.header_block(), self.body_text());
                Self::contains(&full, needle)
            }
            SearchKey::Larger(n) => self.facts.size > *n,
            SearchKey::Smaller(n) => self.facts.size < *n,
            SearchKey::Before(date) => self.facts.internal_date < *date,
            SearchKey::Since(date) => self.facts.internal_date >= *date,
            SearchKey::On(date) => same_day(self.facts.internal_date, *date),
            SearchKey::SentBefore(date) => self.facts.sent_at.is_some_and(|sent| sent < *date),
            SearchKey::SentSince(date) => self.facts.sent_at.is_some_and(|sent| sent >= *date),
            SearchKey::SentOn(date) => self.facts.sent_at.is_some_and(|sent| same_day(sent, *date)),
            SearchKey::Uid(set) => set.contains(self.facts.uid, self.max_uid.max(self.facts.uid)),
            SearchKey::SequenceSet(set) if self.uid_mode => {
                set.contains(self.facts.uid, self.max_uid.max(self.facts.uid))
            }
            SearchKey::SequenceSet(set) => {
                set.contains(self.facts.seq, self.max_seq.max(self.facts.seq))
            }
            SearchKey::Not(inner) => !self.evaluate(inner),
            SearchKey::Or(left, right) => self.evaluate(left) || self.evaluate(right),
            SearchKey::And(keys) => keys.iter().all(|key| self.evaluate(key)),
        }
    }
}

/// Whether a flag set carries a keyword, case-insensitively.
fn has_keyword(flags: &Flags, name: &str) -> bool {
    if flags.has_keyword(name) {
        return true;
    }
    // A client may ask for `\Seen` as a keyword by mistake; treat a system flag
    // name as matching its own bit rather than silently answering "no".
    flags
        .system_flags()
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case(name))
}

/// Whether the header block contains `field` with a value containing `needle`.
fn header_matches(block: &str, field: &str, needle: &str) -> bool {
    for line in block.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case(field) {
            continue;
        }
        if needle.is_empty() || value.to_lowercase().contains(&needle.to_lowercase()) {
            return true;
        }
    }
    false
}

/// Whether two timestamps fall on the same UTC calendar day.
fn same_day(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.date_naive() == b.date_naive()
}

/// Normalise a `SEARCH` date to midnight UTC (the grammar has no time).
pub fn midnight(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_default())
}

/// The full search request: an optional charset and the key expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// The client-declared charset.
    pub charset: Option<String>,
    /// The key expression.
    pub key: Option<SearchKey>,
}

impl SearchRequest {
    /// Validate the request against this server's capabilities.
    ///
    /// Returns the `NO [BADCHARSET …]` error a session should report when the
    /// client asked for a charset we cannot evaluate.
    pub fn validate(&self) -> Result<(), FerromaError> {
        match &self.charset {
            Some(charset) if !charset_supported(charset) => Err(FerromaError::Invalid(format!(
                "BADCHARSET ({})",
                SUPPORTED_CHARSETS.join(" ")
            ))),
            _ => Ok(()),
        }
    }
}

/// Evaluate one key against one message's facts.
///
/// `body` is the raw message (headers plus body), used by `BODY`/`TEXT`. It is
/// optional so a metadata-only search never touches the Maildir.
pub fn matches(
    key: &SearchKey,
    facts: &MessageFacts,
    body: Option<&[u8]>,
    uid_mode: bool,
    max_seq: u64,
    max_uid: u64,
) -> bool {
    let (header_block, body_text) = match body {
        Some(raw) => split_message(raw),
        None => (facts.header_block.clone(), String::new()),
    };
    let mut facts = facts.clone();
    if !header_block.is_empty() {
        facts.header_block = header_block;
    }
    let mut eval = Eval {
        facts: &facts,
        body_text: Some(body_text),
        uid_mode,
        max_seq,
        max_uid,
    };
    eval.evaluate(key)
}

/// Split a raw message into its header block and its decoded body text.
fn split_message(raw: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(raw).into_owned();
    match find_blank_line(&text) {
        Some((header_end, body_start)) => (
            text[..header_end].to_string(),
            text[body_start..].to_string(),
        ),
        None => (text, String::new()),
    }
}

/// Byte offsets of the end of the header block and the start of the body.
fn find_blank_line(text: &str) -> Option<(usize, usize)> {
    if let Some(idx) = text.find("\r\n\r\n") {
        return Some((idx, idx + 4));
    }
    if let Some(idx) = text.find("\n\n") {
        return Some((idx, idx + 2));
    }
    None
}

/// Evaluate a whole key against a slice of messages, returning the matching
/// indices in input order.
pub fn evaluate_all(
    key: &SearchKey,
    messages: &[MessageFacts],
    uid_mode: bool,
) -> Vec<usize> {
    let max_seq = messages.iter().map(|m| m.seq).max().unwrap_or(0);
    let max_uid = messages.iter().map(|m| m.uid).max().unwrap_or(0);
    messages
        .iter()
        .enumerate()
        .filter(|(_, facts)| matches(key, facts, None, uid_mode, max_seq, max_uid))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, hh: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, hh, 0, 0).unwrap()
    }

    fn facts(seq: u64, uid: u64) -> MessageFacts {
        MessageFacts::new(seq, uid, at(2026, 7, 9, 12), 1000)
    }

    fn message() -> MessageFacts {
        let mut m = facts(1, 10);
        m.subject = "Invoice for July".into();
        m.from = "Bob <bob@example.net>".into();
        m.to = "Alice <alice@example.com>".into();
        m.cc = "carol@example.net".into();
        m.bcc = "dave@example.net".into();
        m.header_block = "From: Bob <bob@example.net>\r\nSubject: Invoice for July\r\nDate: Wed, 08 Jul 2026 09:00:00 +0000\r\nX-Tag: alpha\r\n".into();
        m.sent_at = Some(at(2026, 7, 8, 9));
        m
    }

    fn eval(key: &SearchKey) -> bool {
        let m = message();
        matches(key, &m, None, false, 1, 10)
    }

    #[test]
    fn supported_charsets() {
        assert!(charset_supported("US-ASCII"));
        assert!(charset_supported("us-ascii"));
        assert!(charset_supported("UTF-8"));
        assert!(charset_supported(" utf-8 "));
        assert!(!charset_supported("ISO-8859-1"));
        assert!(!charset_supported(""));
    }

    #[test]
    fn request_validation_reports_badcharset() {
        let ok = SearchRequest {
            charset: Some("UTF-8".into()),
            key: Some(SearchKey::All),
        };
        assert!(ok.validate().is_ok());

        let bad = SearchRequest {
            charset: Some("KOI8-R".into()),
            key: Some(SearchKey::All),
        };
        let err = bad.validate().expect_err("must reject");
        assert!(err.to_string().contains("BADCHARSET (US-ASCII UTF-8)"));

        let none = SearchRequest {
            charset: None,
            key: None,
        };
        assert!(none.validate().is_ok());
    }

    #[test]
    fn all_matches_every_message() {
        assert!(eval(&SearchKey::All));
        let list = vec![facts(1, 1), facts(2, 2)];
        assert_eq!(evaluate_all(&SearchKey::All, &list, false), vec![0, 1]);
    }

    #[test]
    fn flag_keys_read_the_flag_set() {
        let mut m = message();
        m.flags = Flags::parse("\\Seen \\Flagged \\Answered \\Draft \\Deleted");
        let max_seq = 1;
        let max_uid = 10;
        for (key, expected) in [
            (SearchKey::Seen, true),
            (SearchKey::Flagged, true),
            (SearchKey::Answered, true),
            (SearchKey::Draft, true),
            (SearchKey::Deleted, true),
            (SearchKey::Unseen, false),
            (SearchKey::Unflagged, false),
            (SearchKey::Unanswered, false),
            (SearchKey::Undraft, false),
            (SearchKey::Undeleted, false),
        ] {
            assert_eq!(
                matches(&key, &m, None, false, max_seq, max_uid),
                expected,
                "{key:?}"
            );
        }
    }

    #[test]
    fn answered_unanswered_flag_keys_work_on_a_plain_stored_flag_string() {
        // `messages.flags` is a space-separated lower-case string, not the
        // comma form `Flags::to_db_string` writes; both must be readable.
        let mut m = message();
        m.flags = crate::session::parse_flags("seen answered");
        assert!(matches(&SearchKey::Answered, &m, None, false, 1, 10));
        assert!(matches(&SearchKey::Seen, &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Deleted, &m, None, false, 1, 10));
    }

    #[test]
    fn recent_family_works_on_the_session_flag() {
        let mut m = message();
        m.recent = true;
        assert!(matches(&SearchKey::Recent, &m, None, false, 1, 10));
        assert!(matches(&SearchKey::New, &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Old, &m, None, false, 1, 10));

        m.flags = crate::session::parse_flags("seen");
        assert!(!matches(&SearchKey::New, &m, None, false, 1, 10));
        assert!(matches(&SearchKey::Recent, &m, None, false, 1, 10));

        m.recent = false;
        assert!(matches(&SearchKey::Old, &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Recent, &m, None, false, 1, 10));
    }

    #[test]
    fn keyword_keys_are_case_insensitive() {
        let mut m = message();
        m.flags = Flags::parse("$Junk");
        assert!(matches(
            &SearchKey::Keyword("$junk".into()),
            &m,
            None,
            false,
            1,
            10
        ));
        assert!(matches(
            &SearchKey::Keyword("$JUNK".into()),
            &m,
            None,
            false,
            1,
            10
        ));
        assert!(matches(
            &SearchKey::Unkeyword("$other".into()),
            &m,
            None,
            false,
            1,
            10
        ));
        assert!(!matches(
            &SearchKey::Unkeyword("$Junk".into()),
            &m,
            None,
            false,
            1,
            10
        ));
    }

    #[test]
    fn a_system_flag_asked_for_as_a_keyword_still_matches() {
        let mut m = message();
        m.flags = Flags::parse("\\Seen");
        assert!(matches(
            &SearchKey::Keyword("\\Seen".into()),
            &m,
            None,
            false,
            1,
            10
        ));
    }

    #[test]
    fn header_string_keys_match_case_insensitively() {
        assert!(eval(&SearchKey::Subject("invoice".into())));
        assert!(eval(&SearchKey::Subject("JULY".into())));
        assert!(!eval(&SearchKey::Subject("August".into())));
        assert!(eval(&SearchKey::From("bob@example.net".into())));
        assert!(eval(&SearchKey::From("BOB".into())));
        assert!(eval(&SearchKey::To("alice".into())));
        assert!(eval(&SearchKey::Cc("carol".into())));
        assert!(eval(&SearchKey::Bcc("dave".into())));
        assert!(!eval(&SearchKey::From("nobody".into())));
    }

    #[test]
    fn an_empty_needle_matches_everything_with_that_field() {
        assert!(eval(&SearchKey::Subject(String::new())));
        assert!(eval(&SearchKey::From(String::new())));
    }

    #[test]
    fn header_key_finds_a_field_by_name_and_value() {
        assert!(eval(&SearchKey::Header("X-Tag".into(), "alpha".into())));
        assert!(eval(&SearchKey::Header("x-tag".into(), "ALPHA".into())));
        assert!(!eval(&SearchKey::Header("X-Tag".into(), "beta".into())));
        assert!(!eval(&SearchKey::Header("X-Missing".into(), "alpha".into())));
        assert!(eval(&SearchKey::Header("X-Tag".into(), String::new())));
    }

    #[test]
    fn body_and_text_search_the_message_bytes() {
        let raw = b"Subject: hi\r\nFrom: bob@example.net\r\n\r\nThe unique body token\r\n";
        let m = message();
        assert!(matches(
            &SearchKey::Body("unique body".into()),
            &m,
            Some(raw),
            false,
            1,
            10
        ));
        assert!(
            !matches(&SearchKey::Body("SUBJECT".into()), &m, Some(raw), false, 1, 10),
            "BODY must not match a header field"
        );
        assert!(matches(
            &SearchKey::Text("Subject: hi".into()),
            &m,
            Some(raw),
            false,
            1,
            10
        ));
        assert!(matches(
            &SearchKey::Text("unique body".into()),
            &m,
            Some(raw),
            false,
            1,
            10
        ));
        assert!(!matches(
            &SearchKey::Body("absent".into()),
            &m,
            Some(raw),
            false,
            1,
            10
        ));
    }

    #[test]
    fn larger_and_smaller_compare_the_stored_size() {
        let mut m = message();
        m.size = 1000;
        assert!(matches(&SearchKey::Larger(999), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Larger(1000), &m, None, false, 1, 10));
        assert!(matches(&SearchKey::Smaller(1001), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Smaller(1000), &m, None, false, 1, 10));
    }

    #[test]
    fn internal_date_family_uses_internal_date() {
        let mut m = message();
        m.internal_date = at(2026, 7, 9, 12);
        m.sent_at = Some(at(2026, 7, 1, 9));

        // Internal-date keys.
        assert!(matches(&SearchKey::Since(at(2026, 7, 9, 0)), &m, None, false, 1, 10));
        assert!(matches(&SearchKey::Since(at(2026, 7, 9, 12)), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Since(at(2026, 7, 10, 0)), &m, None, false, 1, 10));
        assert!(matches(&SearchKey::Before(at(2026, 7, 10, 0)), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Before(at(2026, 7, 9, 12)), &m, None, false, 1, 10));
        assert!(matches(&SearchKey::On(at(2026, 7, 9, 3)), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::On(at(2026, 7, 8, 3)), &m, None, false, 1, 10));
    }

    #[test]
    fn sent_date_family_uses_the_date_header_not_the_internal_date() {
        let mut m = message();
        m.internal_date = at(2026, 7, 9, 12);
        m.sent_at = Some(at(2026, 7, 1, 9));

        // The two families must disagree here: that is the whole point.
        assert!(matches(&SearchKey::SentSince(at(2026, 7, 1, 0)), &m, None, false, 1, 10));
        // The internal date (9 July) is after 1 July, so `SINCE` also matches…
        assert!(matches(&SearchKey::Since(at(2026, 7, 1, 0)), &m, None, false, 1, 10));
        // …but from 2 July on they disagree: the sent date is 1 July.
        assert!(!matches(&SearchKey::SentSince(at(2026, 7, 2, 0)), &m, None, false, 1, 10));
        assert!(matches(&SearchKey::Since(at(2026, 7, 2, 0)), &m, None, false, 1, 10));

        assert!(matches(&SearchKey::SentBefore(at(2026, 7, 2, 0)), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::Before(at(2026, 7, 2, 0)), &m, None, false, 1, 10));
        assert!(matches(&SearchKey::SentOn(at(2026, 7, 1, 23)), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::On(at(2026, 7, 1, 23)), &m, None, false, 1, 10));
    }

    #[test]
    fn a_message_without_a_date_header_never_matches_the_sent_family() {
        let mut m = message();
        m.sent_at = None;
        assert!(!matches(&SearchKey::SentSince(at(2020, 1, 1, 0)), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::SentBefore(at(2030, 1, 1, 0)), &m, None, false, 1, 10));
        assert!(!matches(&SearchKey::SentOn(at(2026, 7, 8, 0)), &m, None, false, 1, 10));
    }

    #[test]
    fn internal_date_keys_always_apply() {
        // `INTERNALDATE` is never absent, so `SINCE 1970` always matches.
        let m = message();
        assert!(matches(&SearchKey::Since(at(1970, 1, 1, 0)), &m, None, false, 1, 10));
    }

    #[test]
    fn uid_key_matches_uids_not_sequence_numbers() {
        let m = facts(1, 99);
        assert!(matches(
            &SearchKey::Uid(SequenceSet::parse("99").unwrap()),
            &m,
            None,
            false,
            1,
            99
        ));
        assert!(!matches(
            &SearchKey::Uid(SequenceSet::parse("1").unwrap()),
            &m,
            None,
            false,
            1,
            99
        ));
    }

    #[test]
    fn a_bare_message_set_matches_sequence_numbers() {
        let m = facts(3, 99);
        assert!(matches(
            &SearchKey::SequenceSet(SequenceSet::parse("1:5").unwrap()),
            &m,
            None,
            false,
            5,
            200
        ));
        assert!(!matches(
            &SearchKey::SequenceSet(SequenceSet::parse("1").unwrap()),
            &m,
            None,
            false,
            5,
            200
        ));
    }

    #[test]
    fn a_bare_message_set_switches_to_uids_under_uid_search() {
        let m = facts(3, 99);
        assert!(!matches(
            &SearchKey::SequenceSet(SequenceSet::parse("3").unwrap()),
            &m,
            None,
            true,
            5,
            200
        ));
        assert!(matches(
            &SearchKey::SequenceSet(SequenceSet::parse("99").unwrap()),
            &m,
            None,
            true,
            5,
            200
        ));
        assert!(matches(
            &SearchKey::SequenceSet(SequenceSet::parse("90:100").unwrap()),
            &m,
            None,
            true,
            5,
            200
        ));
    }

    #[test]
    fn star_in_a_set_resolves_against_the_mailbox_maximum() {
        let m = facts(3, 99);
        assert!(matches(
            &SearchKey::SequenceSet(SequenceSet::parse("*").unwrap()),
            &m,
            None,
            false,
            3,
            99
        ));
        assert!(matches(
            &SearchKey::Uid(SequenceSet::parse("*").unwrap()),
            &m,
            None,
            false,
            3,
            99
        ));
        assert!(!matches(
            &SearchKey::SequenceSet(SequenceSet::parse("*").unwrap()),
            &m,
            None,
            false,
            5,
            99
        ));
    }

    #[test]
    fn not_negates() {
        assert!(!eval(&SearchKey::Not(Box::new(SearchKey::All))));
        assert!(eval(&SearchKey::Not(Box::new(SearchKey::Seen))));
        assert!(!eval(&SearchKey::Not(Box::new(SearchKey::Unseen))));
    }

    #[test]
    fn or_short_circuits_correctly() {
        assert!(eval(&SearchKey::Or(
            Box::new(SearchKey::Seen),
            Box::new(SearchKey::Subject("invoice".into()))
        )));
        assert!(!eval(&SearchKey::Or(
            Box::new(SearchKey::Seen),
            Box::new(SearchKey::Flagged)
        )));
    }

    #[test]
    fn and_requires_every_key() {
        assert!(eval(&SearchKey::And(vec![
            SearchKey::All,
            SearchKey::Subject("invoice".into()),
        ])));
        assert!(!eval(&SearchKey::And(vec![
            SearchKey::All,
            SearchKey::Seen
        ])));
        // An empty conjunction is vacuously true.
        assert!(eval(&SearchKey::And(Vec::new())));
    }

    #[test]
    fn empty_or_set_matches_nothing_rather_than_erroring() {
        let m = message();
        assert!(!matches(
            &SearchKey::SequenceSet(SequenceSet::empty()),
            &m,
            None,
            false,
            1,
            10
        ));
    }

    #[test]
    fn evaluate_all_returns_matching_indices_in_order() {
        let mut list = Vec::new();
        for index in 0..5u64 {
            let mut m = facts(index + 1, (index + 1) * 10);
            m.size = 100 * (index + 1);
            list.push(m);
        }
        let key = SearchKey::Larger(250);
        assert_eq!(evaluate_all(&key, &list, false), vec![2, 3, 4]);
    }

    #[test]
    fn needs_body_and_headers_guide_the_session() {
        assert!(SearchKey::Body("x".into()).needs_body());
        assert!(SearchKey::Text("x".into()).needs_body());
        assert!(!SearchKey::Subject("x".into()).needs_body());
        assert!(SearchKey::From("x".into()).needs_headers());
        assert!(SearchKey::Header("a".into(), "b".into()).needs_headers());
        assert!(SearchKey::SentOn(at(2026, 1, 1, 0)).needs_headers());
        assert!(!SearchKey::Since(at(2026, 1, 1, 0)).needs_headers());
        assert!(SearchKey::Not(Box::new(SearchKey::Body("x".into()))).needs_body());
        assert!(SearchKey::Or(
            Box::new(SearchKey::Seen),
            Box::new(SearchKey::Body("x".into()))
        )
        .needs_body());
        assert!(SearchKey::And(vec![SearchKey::Body("x".into())]).needs_body());
    }

    #[test]
    fn sequence_set_accessor() {
        let key = SearchKey::Uid(SequenceSet::parse("1").unwrap());
        assert!(key.sequence_set().is_some());
        assert!(SearchKey::All.sequence_set().is_none());
    }

    #[test]
    fn split_message_handles_both_line_endings() {
        let (header, body) = split_message(b"Subject: hi\r\n\r\nbody\r\n");
        assert_eq!(header, "Subject: hi");
        assert_eq!(body, "body\r\n");

        let (header, body) = split_message(b"Subject: hi\n\nbody\n");
        assert_eq!(header, "Subject: hi");
        assert_eq!(body, "body\n");

        let (header, body) = split_message(b"no blank line");
        assert_eq!(header, "no blank line");
        assert_eq!(body, "");
    }

    #[test]
    fn same_day_compares_calendar_days_in_utc() {
        assert!(same_day(at(2026, 7, 9, 0), at(2026, 7, 9, 23)));
        assert!(!same_day(at(2026, 7, 9, 23), at(2026, 7, 10, 0)));
    }

    #[test]
    fn midnight_normalises_a_date() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        assert_eq!(midnight(date), at(2026, 7, 9, 0));
    }

    #[test]
    fn header_matching_is_unfolded_line_by_line() {
        let block = "Subject: a long\r\n subject line\r\nX: 1\r\n";
        assert!(header_matches(block, "X", "1"));
        assert!(header_matches(block, "Subject", "long"));
        assert!(!header_matches(block, "Subject", "absent"));
    }

    #[test]
    fn hostile_needles_cannot_panic_the_matcher() {
        let m = message();
        for needle in ["", "\u{0}", "%", "_", "'", "\"", "\\", "日本", &"x".repeat(10_000)] {
            let _ = matches(&SearchKey::Subject(needle.into()), &m, None, false, 1, 10);
            let _ = matches(
                &SearchKey::Header("Subject".into(), needle.into()),
                &m,
                None,
                false,
                1,
                10,
            );
        }
    }

    #[test]
    fn a_deeply_nested_expression_evaluates_without_recursion_trouble() {
        let mut key = SearchKey::All;
        for _ in 0..200 {
            key = SearchKey::Not(Box::new(key));
        }
        let m = message();
        assert!(matches(&key, &m, None, false, 1, 10));
    }
}
