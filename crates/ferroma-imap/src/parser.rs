//! The IMAP4rev1 command parser (RFC 3501 §9).
//!
//! # Why this is not a one-line `split_whitespace`
//!
//! IMAP is not a line protocol in the way SMTP is: a command argument may be a
//! **literal**, `{n}`, whose `n` octets follow the CRLF *outside* the line. A
//! client may also use `LITERAL+` (`{n+}`) to send the octets without waiting
//! for the server's `+` continuation. Getting this wrong is the classic failure
//! mode of hand-written IMAP servers, so the parser here is an incremental
//! reader: [`CommandParser`] is handed the line it just read, and the instant it
//! needs literal octets it asks the [`LiteralSource`] for them, which is what
//! writes the `+` continuation in production.
//!
//! Everything else — atoms, quoted strings, parenthesised lists, `NIL`,
//! sequence sets, `FETCH` item lists with macros and partials, `STORE` action
//! syntax, `SEARCH` keys — is parsed into a typed [`Command`]. Every failure is a
//! `BAD`-shaped [`FerromaError`]; no input, however hostile, can panic this
//! module.

use std::fmt;

use ferroma_core::FerromaError;

use crate::search::SearchKey;
use crate::sequence::SequenceSet;
use crate::util::{self, MAX_ATOM_LEN};

/// Longest accepted command tag (RFC 3501 §9 allows 1–30).
pub const MAX_TAG_LEN: usize = 30;

/// Default ceiling for one literal, in bytes.
pub const DEFAULT_MAX_LITERAL: u64 = 1_048_576;

/// What a `STORE` does to a message's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreAction {
    /// `FLAGS` — replace the whole flag set.
    Replace,
    /// `+FLAGS` — add.
    Add,
    /// `-FLAGS` — remove.
    Remove,
}

impl StoreAction {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            StoreAction::Replace => "FLAGS",
            StoreAction::Add => "+FLAGS",
            StoreAction::Remove => "-FLAGS",
        }
    }
}

/// One section of a message a `BODY[...]`/`RFC822.*` item asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Section {
    /// `BODY[]` — the whole message.
    All,
    /// `BODY[HEADER]` — the top-level header block.
    Header,
    /// `BODY[HEADER.FIELDS (…)]`.
    HeaderFields(Vec<String>),
    /// `BODY[HEADER.FIELDS.NOT (…)]`.
    HeaderFieldsNot(Vec<String>),
    /// `BODY[TEXT]` — the body of the top-level message.
    Text,
    /// `BODY[n]` — the raw bytes of part `n`.
    Part(Vec<u32>),
    /// `BODY[n.MIME]` — the MIME header of part `n`.
    Mime(Vec<u32>),
    /// `BODY[n.HEADER]` — the header of an encapsulated `message/rfc822` part.
    PartHeader(Vec<u32>),
    /// `BODY[n.TEXT]`.
    PartText(Vec<u32>),
    /// `BODY[n.HEADER.FIELDS (…)]`.
    PartHeaderFields(Vec<u32>, Vec<String>),
    /// `BODY[n.HEADER.FIELDS.NOT (…)]`.
    PartHeaderFieldsNot(Vec<u32>, Vec<String>),
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = |parts: &[u32]| {
            parts
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(".")
        };
        let fields = |names: &[String]| names.join(" ");
        match self {
            Section::All => f.write_str(""),
            Section::Header => f.write_str("HEADER"),
            Section::HeaderFields(names) => write!(f, "HEADER.FIELDS ({})", fields(names)),
            Section::HeaderFieldsNot(names) => {
                write!(f, "HEADER.FIELDS.NOT ({})", fields(names))
            }
            Section::Text => f.write_str("TEXT"),
            Section::Part(parts) => f.write_str(&path(parts)),
            Section::Mime(parts) => write!(f, "{}.MIME", path(parts)),
            Section::PartHeader(parts) => write!(f, "{}.HEADER", path(parts)),
            Section::PartText(parts) => write!(f, "{}.TEXT", path(parts)),
            Section::PartHeaderFields(parts, names) => {
                write!(f, "{}.HEADER.FIELDS ({})", path(parts), fields(names))
            }
            Section::PartHeaderFieldsNot(parts, names) => {
                write!(f, "{}.HEADER.FIELDS.NOT ({})", path(parts), fields(names))
            }
        }
    }
}

impl Section {
    /// Whether this section names a nested MIME part.
    pub fn part_path(&self) -> Option<&[u32]> {
        match self {
            Section::Part(p)
            | Section::Mime(p)
            | Section::PartHeader(p)
            | Section::PartText(p)
            | Section::PartHeaderFields(p, _)
            | Section::PartHeaderFieldsNot(p, _) => Some(p),
            _ => None,
        }
    }
}

/// One item of a `FETCH` data list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchItem {
    /// `UID`.
    Uid,
    /// `FLAGS`.
    Flags,
    /// `INTERNALDATE`.
    InternalDate,
    /// `RFC822.SIZE`.
    Rfc822Size,
    /// `ENVELOPE`.
    Envelope,
    /// `BODY` (non-extensible `BODYSTRUCTURE`).
    Body,
    /// `BODYSTRUCTURE`.
    BodyStructure,
    /// `BODY[section]`, optionally with a `<start.len>` partial.
    BodySection(Section, Option<util::Partial>),
    /// `BODY.PEEK[section]` — like `BODY[section]` but must not set `\Seen`.
    BodyPeekSection(Section, Option<util::Partial>),
    /// `RFC822`.
    Rfc822,
    /// `RFC822.HEADER`.
    Rfc822Header,
    /// `RFC822.TEXT`.
    Rfc822Text,
}

impl FetchItem {
    /// Whether evaluating this item sets `\Seen` implicitly.
    ///
    /// `BODY[...]` and `RFC822`/`RFC822.TEXT` do; `BODY.PEEK[...]` and the
    /// metadata items do not (RFC 3501 §6.4.5).
    pub fn sets_seen(&self) -> bool {
        matches!(
            self,
            FetchItem::BodySection(..) | FetchItem::Rfc822 | FetchItem::Rfc822Text
        )
    }

    /// The response key this item is reported under.
    pub fn response_key(&self) -> String {
        match self {
            FetchItem::Uid => "UID".to_string(),
            FetchItem::Flags => "FLAGS".to_string(),
            FetchItem::InternalDate => "INTERNALDATE".to_string(),
            FetchItem::Rfc822Size => "RFC822.SIZE".to_string(),
            FetchItem::Envelope => "ENVELOPE".to_string(),
            FetchItem::Body => "BODY".to_string(),
            FetchItem::BodyStructure => "BODYSTRUCTURE".to_string(),
            FetchItem::BodySection(section, partial) => {
                render_section_key("BODY", section, *partial)
            }
            FetchItem::BodyPeekSection(section, partial) => {
                render_section_key("BODY", section, *partial)
            }
            FetchItem::Rfc822 => "RFC822".to_string(),
            FetchItem::Rfc822Header => "RFC822.HEADER".to_string(),
            FetchItem::Rfc822Text => "RFC822.TEXT".to_string(),
        }
    }

    /// The equivalent item with `BODY[...]` turned into `BODY.PEEK[...]`, used
    /// by `UID FETCH` and by internal callers that must not disturb `\Seen`.
    pub fn peek(&self) -> FetchItem {
        match self {
            FetchItem::BodySection(section, partial) => {
                FetchItem::BodyPeekSection(section.clone(), *partial)
            }
            other => other.clone(),
        }
    }

    /// Whether this item's value is a literal (so it is written as `{n}\r\n…`).
    pub fn is_literal_valued(&self) -> bool {
        matches!(
            self,
            FetchItem::BodySection(..)
                | FetchItem::BodyPeekSection(..)
                | FetchItem::Rfc822
                | FetchItem::Rfc822Text
                | FetchItem::Rfc822Header
        )
    }
}

/// Render `BODY[section]<start.len>`.
fn render_section_key(name: &str, section: &Section, partial: Option<util::Partial>) -> String {
    let mut out = String::with_capacity(name.len() + 16);
    out.push_str(name);
    out.push('[');
    out.push_str(&section.to_string());
    out.push(']');
    if let Some(partial) = partial {
        out.push('<');
        out.push_str(&partial.start.to_string());
        out.push('.');
        out.push_str(&partial.len.to_string());
        out.push('>');
    }
    out
}

/// A `FETCH` argument: either the shorthand macro or an explicit item list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchSpec {
    /// `ALL`, `FAST` or `FULL`, expanded into their item lists.
    Macro(Vec<FetchItem>),
    /// An explicit `(item item …)` list.
    Items(Vec<FetchItem>),
}

impl FetchSpec {
    /// The items to evaluate, in order.
    pub fn items(&self) -> &[FetchItem] {
        match self {
            FetchSpec::Macro(items) | FetchSpec::Items(items) => items,
        }
    }

    /// `ALL` — everything a normal client needs to list a mailbox.
    pub fn all() -> Vec<FetchItem> {
        vec![
            FetchItem::Flags,
            FetchItem::InternalDate,
            FetchItem::Rfc822Size,
            FetchItem::Envelope,
        ]
    }

    /// `FAST` — no envelope.
    pub fn fast() -> Vec<FetchItem> {
        vec![
            FetchItem::Flags,
            FetchItem::InternalDate,
            FetchItem::Rfc822Size,
        ]
    }

    /// `FULL` — `ALL` plus the body.
    pub fn full() -> Vec<FetchItem> {
        let mut items = FetchSpec::all();
        items.push(FetchItem::Body);
        items
    }
}

/// The destination of a `COPY`/`MOVE`, plus its `UID` mode.
pub type MailboxName = String;

/// A parsed IMAP command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `CAPABILITY`.
    Capability,
    /// `NOOP`.
    Noop,
    /// `LOGOUT`.
    Logout,
    /// `STARTTLS`.
    StartTls,
    /// `LOGIN user pass`.
    Login {
        /// The user name (an IMAP `astring`).
        user: String,
        /// The password (an IMAP `astring`).
        password: String,
    },
    /// `AUTHENTICATE PLAIN [initial-response]`.
    Authenticate {
        /// The SASL mechanism, upper-cased.
        mechanism: String,
        /// The optional `SASL-IR` initial response, already base64-decoded.
        initial_response: Option<Vec<u8>>,
    },
    /// `SELECT mailbox`.
    Select(MailboxName),
    /// `EXAMINE mailbox`.
    Examine(MailboxName),
    /// `CREATE mailbox`.
    Create(MailboxName),
    /// `DELETE mailbox`.
    Delete(MailboxName),
    /// `RENAME from to`.
    Rename {
        /// The existing name.
        from: MailboxName,
        /// The new name.
        to: MailboxName,
    },
    /// `SUBSCRIBE mailbox`.
    Subscribe(MailboxName),
    /// `UNSUBSCRIBE mailbox`.
    Unsubscribe(MailboxName),
    /// `LIST reference pattern`.
    List {
        /// The reference name.
        reference: String,
        /// The pattern.
        pattern: String,
    },
    /// `LSUB reference pattern`.
    Lsub {
        /// The reference name.
        reference: String,
        /// The pattern.
        pattern: String,
    },
    /// `STATUS mailbox (items)`.
    Status {
        /// The mailbox name.
        mailbox: MailboxName,
        /// The requested items.
        items: Vec<crate::response::StatusItem>,
    },
    /// `APPEND mailbox [(flags)] [date] {literal}`.
    Append {
        /// The destination mailbox.
        mailbox: MailboxName,
        /// The flags to store, as an IMAP flag list.
        flags: Vec<String>,
        /// The optional `INTERNALDATE`.
        internal_date: Option<chrono::DateTime<chrono::Utc>>,
        /// The message bytes.
        message: Vec<u8>,
    },
    /// `CHECK`.
    Check,
    /// `CLOSE`.
    Close,
    /// `EXPUNGE`.
    Expunge,
    /// `UID EXPUNGE <set>` (RFC 4315) — expunge only these UIDs.
    UidExpunge(SequenceSet),
    /// `SEARCH [CHARSET c] keys`.
    Search {
        /// The charset the client declared, if any.
        charset: Option<String>,
        /// The key expression.
        key: Option<SearchKey>,
    },
    /// `FETCH set spec`.
    Fetch {
        /// The message set.
        set: SequenceSet,
        /// The items to fetch.
        spec: FetchSpec,
    },
    /// `STORE set action flags`.
    Store {
        /// The message set.
        set: SequenceSet,
        /// Replace/add/remove.
        action: StoreAction,
        /// `true` for the `.SILENT` variant (no untagged `FETCH` in reply).
        silent: bool,
        /// The flags.
        flags: Vec<String>,
    },
    /// `COPY set mailbox`.
    Copy {
        /// The message set.
        set: SequenceSet,
        /// The destination.
        mailbox: MailboxName,
    },
    /// `MOVE set mailbox`.
    Move {
        /// The message set.
        set: SequenceSet,
        /// The destination.
        mailbox: MailboxName,
    },
    /// `UID <command>`.
    Uid(Box<Command>),
    /// `IDLE`.
    Idle,
    /// `UNSELECT`.
    Unselect,
    /// `NAMESPACE`.
    Namespace,
    /// `ID …` — accepted and answered with an empty `ID` list (RFC 2971).
    Id,
    /// `ENABLE …` — accepted, answering `OK` and enabling nothing.
    Enable(Vec<String>),
}

impl Command {
    /// The command's name, for structured logging and `BAD` messages.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Capability => "CAPABILITY",
            Command::Noop => "NOOP",
            Command::Logout => "LOGOUT",
            Command::StartTls => "STARTTLS",
            Command::Login { .. } => "LOGIN",
            Command::Authenticate { .. } => "AUTHENTICATE",
            Command::Select(_) => "SELECT",
            Command::Examine(_) => "EXAMINE",
            Command::Create(_) => "CREATE",
            Command::Delete(_) => "DELETE",
            Command::Rename { .. } => "RENAME",
            Command::Subscribe(_) => "SUBSCRIBE",
            Command::Unsubscribe(_) => "UNSUBSCRIBE",
            Command::List { .. } => "LIST",
            Command::Lsub { .. } => "LSUB",
            Command::Status { .. } => "STATUS",
            Command::Append { .. } => "APPEND",
            Command::Check => "CHECK",
            Command::Close => "CLOSE",
            Command::Expunge => "EXPUNGE",
            Command::UidExpunge(_) => "UID EXPUNGE",
            Command::Search { .. } => "SEARCH",
            Command::Fetch { .. } => "FETCH",
            Command::Store { .. } => "STORE",
            Command::Copy { .. } => "COPY",
            Command::Move { .. } => "MOVE",
            Command::Uid(inner) => match inner.as_ref() {
                Command::Fetch { .. } => "UID FETCH",
                Command::Store { .. } => "UID STORE",
                Command::Search { .. } => "UID SEARCH",
                Command::Copy { .. } => "UID COPY",
                Command::Move { .. } => "UID MOVE",
                Command::Expunge | Command::UidExpunge(_) => "UID EXPUNGE",
                _ => "UID",
            },
            Command::Idle => "IDLE",
            Command::Unselect => "UNSELECT",
            Command::Namespace => "NAMESPACE",
            Command::Id => "ID",
            Command::Enable(_) => "ENABLE",
        }
    }

    /// Whether this is a `UID`-prefixed command.
    pub fn is_uid(&self) -> bool {
        matches!(self, Command::Uid(_))
    }

    /// The inner command of a `UID` command, or the command itself.
    pub fn inner(&self) -> &Command {
        match self {
            Command::Uid(inner) => inner,
            other => other,
        }
    }

    /// Whether the command carries a literal whose octets were already consumed
    /// from the wire (so a logging layer must not print them).
    pub fn carries_message_body(&self) -> bool {
        matches!(self, Command::Append { .. } | Command::Login { .. } | Command::Authenticate { .. })
    }
}

/// A supplied literal, as read from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralError {
    /// The client refused the literal (sent a tagged command instead).
    Declined,
    /// The octets did not arrive.
    Io(String),
}

/// Where the parser gets a literal's octets from.
///
/// The production implementation is the session loop, which writes the
/// `+ Ready for literal data` continuation and then reads `declared` octets from
/// the socket. Tests implement it from an in-memory byte queue.
///
/// The trait requires `Send` because a literal read happens inside the
/// connection task: a session over a socket has to keep its whole future `Send`.
pub trait LiteralSource: Send {
    /// The octets of one literal. `synchronizing` is `false` for `LITERAL+`.
    fn read_literal(
        &mut self,
        declared: u64,
        synchronizing: bool,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, LiteralError>> + Send;
}

/// How the parse ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// A complete command.
    Command,
    /// The client answered `DONE` while idling.
    IdleDone,
}

/// The parser's public result for one parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    /// The command tag.
    pub tag: String,
    /// The command.
    pub command: Command,
}

/// The incremental command parser.
///
/// One instance per connection. Feed it each line the client sends; it returns a
/// [`ParsedCommand`] or a `BAD` error, and never loses its place.
#[derive(Debug, Clone)]
pub struct CommandParser {
    tag: String,
    line: String,
    pos: usize,
    max_literal: u64,
    idling: bool,
}

impl Default for CommandParser {
    fn default() -> Self {
        CommandParser::new()
    }
}

impl CommandParser {
    /// A parser with the default literal ceiling.
    pub fn new() -> Self {
        CommandParser {
            tag: String::new(),
            line: String::new(),
            pos: 0,
            max_literal: DEFAULT_MAX_LITERAL,
            idling: false,
        }
    }

    /// A parser with an explicit literal ceiling (`APPEND` uses the configured
    /// `imap.max_append_size`).
    pub fn with_max_literal(max_literal: u64) -> Self {
        CommandParser {
            max_literal,
            ..CommandParser::new()
        }
    }

    /// The tag of the command currently being parsed.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Raise the literal ceiling.
    pub fn set_max_literal(&mut self, max_literal: u64) {
        self.max_literal = max_literal;
    }

    /// The literal ceiling.
    pub fn max_literal(&self) -> u64 {
        self.max_literal
    }

    /// The byte offset of the cursor inside the current line.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Whether the parser is inside an `IDLE`.
    pub fn is_idling(&self) -> bool {
        self.idling
    }

    /// Switch into `IDLE` mode: subsequent lines are scanned for `DONE`.
    pub fn begin_idle(&mut self) {
        self.idling = true;
        self.line.clear();
        self.pos = 0;
    }

    /// Switch out of `IDLE` mode.
    pub fn end_idle(&mut self) {
        self.idling = false;
    }

    /// The parser's error for a line received while idling that is not `DONE`.
    ///
    /// RFC 2177 says anything other than `DONE` is an error; in practice clients
    /// only ever send `DONE`, and a client that does not gets its `IDLE`
    /// terminated with `BAD` (never a hang).
    pub fn idle_error(&self, line: &str) -> FerromaError {
        bad(format!(
            "expected DONE while idling, got `{}`",
            line.chars().take(40).collect::<String>()
        ))
    }

    /// Whether `line` is the `DONE` that ends an `IDLE`.
    pub fn is_done(line: &str) -> bool {
        line.trim().eq_ignore_ascii_case("DONE")
    }

    /// Parse one command from `line`, pulling literals through `source`.
    ///
    /// `line` must be the *content* of a CRLF-terminated line, without the CRLF.
    #[allow(clippy::type_complexity)]
    pub async fn parse<L: LiteralSource>(
        &mut self,
        line: &str,
        source: &mut L,
    ) -> Result<ParsedCommand, FerromaError> {
        if self.idling {
            return Err(self.idle_error(line));
        }
        self.line = line.to_string();
        self.pos = 0;
        self.tag.clear();

        if !line.is_ascii() {
            return Err(bad("command line contains non-ASCII bytes outside a literal"));
        }

        self.tag = self.read_tag()?;
        let name = self.read_atom_upper()?;
        let command = self
            .parse_command_body(&name, source)
            .await
            .map_err(|err| self.annotate(err, &name))?;
        self.expect_end(&name)?;
        Ok(ParsedCommand {
            tag: self.tag.clone(),
            command,
        })
    }

    /// Parse a command that has no tag and no literals (used by tests and by
    /// callers that already hold a command line).
    pub fn parse_command_only(&mut self, line: &str) -> Result<Command, FerromaError> {
        self.line = line.to_string();
        self.pos = 0;
        self.tag = "x".to_string();
        if !line.is_ascii() {
            return Err(bad("command line contains non-ASCII bytes"));
        }
        let name = self.read_atom_upper()?;
        let parsed = self.parse_command_body_sync(&name)?;
        self.expect_end(&name)?;
        Ok(parsed)
    }

    // -----------------------------------------------------------------------
    // Wire helpers.
    // -----------------------------------------------------------------------

    /// A parse error that names the command it came from.
    fn annotate(&self, err: FerromaError, name: &str) -> FerromaError {
        match err {
            FerromaError::Parse(message) => bad(format!("{name}: {message}")),
            other => other,
        }
    }

    fn bad_here(&self, message: impl Into<String>) -> FerromaError {
        bad(format!(
            "{} (at byte {})",
            message.into(),
            self.pos
        ))
    }

    fn peek(&self) -> Option<char> {
        self.line[self.pos..].chars().next()
    }

    fn peek_byte(&self) -> Option<u8> {
        self.line.as_bytes().get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    /// Consume one SP, at least one.
    fn read_sp(&mut self) -> Result<(), FerromaError> {
        if self.peek_byte() == Some(b' ') {
            self.skip_sp();
            Ok(())
        } else {
            Err(self.bad_here("expected a space"))
        }
    }

    /// Consume any run of SPs (possibly none).
    fn skip_sp(&mut self) {
        while self.peek_byte() == Some(b' ') {
            self.pos += 1;
        }
    }

    /// Consume the optional SP at the end of a command.
    fn skip_opt_sp(&mut self) {
        self.skip_sp();
    }

    /// `true` when the cursor is at the end of the line.
    fn at_end(&self) -> bool {
        self.pos >= self.line.len()
    }

    /// Fail unless the cursor is at the end of the line.
    fn expect_end(&self, name: &str) -> Result<(), FerromaError> {
        if self.at_end() {
            return Ok(());
        }
        let rest: String = self.line[self.pos..].chars().take(32).collect();
        Err(bad(format!("{name}: unexpected trailing input `{rest}`")))
    }

    /// Read the 1–30 character command tag.
    fn read_tag(&mut self) -> Result<String, FerromaError> {
        let start = self.pos;
        while let Some(byte) = self.peek_byte() {
            if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let tag = &self.line[start..self.pos];
        if tag.is_empty() {
            return Err(self.bad_here("missing command tag"));
        }
        if tag.len() > MAX_TAG_LEN {
            return Err(bad(format!(
                "command tag is longer than {MAX_TAG_LEN} characters"
            )));
        }
        if self.peek_byte() != Some(b' ') {
            return Err(self.bad_here("expected a space after the command tag"));
        }
        self.pos += 1;
        Ok(tag.to_string())
    }

    /// Read a bare atom and upper-case it (for command names).
    fn read_atom_upper(&mut self) -> Result<String, FerromaError> {
        let start = self.pos;
        while let Some(byte) = self.peek_byte() {
            if byte == b' ' || byte == b'(' || byte == b')' || byte == b'{' {
                break;
            }
            if byte == b'"' || !(0x20..0x7f).contains(&byte) {
                break;
            }
            self.pos += 1;
        }
        let atom = &self.line[start..self.pos];
        if atom.is_empty() {
            return Err(self.bad_here("expected an atom"));
        }
        if atom.len() > MAX_ATOM_LEN {
            return Err(bad("atom is longer than the maximum accepted length"));
        }
        Ok(atom.to_ascii_uppercase())
    }

    /// Read a bare atom without case folding.
    fn read_atom(&mut self) -> Result<String, FerromaError> {
        let start = self.pos;
        while let Some(byte) = self.peek_byte() {
            if matches!(byte, b' ' | b'(' | b')' | b'{' | b'"' | b'%' | b'*' | b'\\' | b']') {
                break;
            }
            if !(0x20..0x7f).contains(&byte) {
                break;
            }
            self.pos += 1;
        }
        let atom = &self.line[start..self.pos];
        if atom.is_empty() {
            return Err(self.bad_here("expected an atom"));
        }
        if atom.len() > MAX_ATOM_LEN {
            return Err(bad("atom is longer than the maximum accepted length"));
        }
        Ok(atom.to_string())
    }

    /// Read a quoted string, decoding `\"` and `\\`.
    fn read_quoted(&mut self) -> Result<String, FerromaError> {
        // The opening quote.
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(bad("unterminated quoted string"));
            };
            match ch {
                '"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                '\\' => {
                    self.pos += 1;
                    let Some(escaped) = self.bump() else {
                        return Err(bad("unterminated escape in quoted string"));
                    };
                    if escaped != '"' && escaped != '\\' {
                        return Err(bad(format!(
                            "unsupported escape `\\{escaped}` in quoted string"
                        )));
                    }
                    out.push(escaped);
                    if out.len() > MAX_ATOM_LEN {
                        return Err(bad("quoted string is longer than the maximum accepted length"));
                    }
                }
                '\r' | '\n' | '\0' => {
                    return Err(bad("quoted string contains a control character"));
                }
                _ => {
                    self.pos += ch.len_utf8();
                    out.push(ch);
                    if out.len() > MAX_ATOM_LEN {
                        return Err(bad("quoted string is longer than the maximum accepted length"));
                    }
                }
            }
        }
    }

    /// Parse a `{n}` or `{n+}` literal prefix at the cursor, if present.
    ///
    /// Returns `(declared length, synchronizing)`. A `{` that is not followed by
    /// digits is a syntax error, matching the grammar (`literal` is the only
    /// production that starts with `{`).
    fn try_literal_prefix(&mut self) -> Result<Option<(u64, bool)>, FerromaError> {
        if self.peek_byte() != Some(b'{') {
            return Ok(None);
        }
        let mut pos = self.pos + 1;
        let bytes = self.line.as_bytes();
        let mut value: u64 = 0;
        let mut digits = 0usize;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(bytes[pos] - b'0')))
                .ok_or_else(|| bad("literal length overflows"))?;
            digits += 1;
            pos += 1;
            if digits > 20 {
                return Err(bad("literal length has too many digits"));
            }
        }
        if digits == 0 {
            return Err(bad("malformed literal: expected `{<number>}`"));
        }
        let mut synchronizing = true;
        if pos < bytes.len() && bytes[pos] == b'+' {
            synchronizing = false;
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b'}' {
            return Err(bad("malformed literal: missing `}`"));
        }
        // RFC 3501 §4.3: the `{n}` on the line is replaced by the `n` octets
        // that follow the line, and *then* the rest of the command line
        // continues — so a literal may legitimately sit in the middle of an
        // argument list (`RENAME {3} Old New`). The cursor therefore stops just
        // past the `}`, and the `LiteralSource` hands the text that follows the
        // octets back as the next line.
        if value > self.max_literal {
            return Err(bad(format!(
                "literal of {value} bytes exceeds the maximum of {}",
                self.max_literal
            )));
        }
        self.pos = pos + 1;
        Ok(Some((value, synchronizing)))
    }

    /// Read an `astring`: atom, quoted string, or literal.
    async fn read_astring<L: LiteralSource>(&mut self, source: &mut L) -> Result<String, FerromaError> {
        let form = self.assert_string_start()?;
        let bytes = match form {
            StrStart::Quoted => return self.read_quoted(),
            StrStart::Literal => self.read_literal_bytes(source).await?,
            StrStart::Atom => self.read_atom()?.into_bytes(),
        };
        Ok(decode_name(&bytes))
    }

    /// Read a `string` that is always quoted or literal, preserving raw bytes.
    async fn read_raw_string<L: LiteralSource>(
        &mut self,
        source: &mut L,
    ) -> Result<Vec<u8>, FerromaError> {
        let form = self.assert_string_start()?;
        match form {
            StrStart::Quoted => Ok(self.read_quoted()?.into_bytes()),
            StrStart::Literal => self.read_literal_bytes(source).await,
            StrStart::Atom => Err(self.bad_here("expected a quoted string or literal")),
        }
    }

    /// Classify the next string token and advance past its opening delimiter.
    fn assert_string_start(&mut self) -> Result<StrStart, FerromaError> {
        match self.peek_byte() {
            Some(b'"') => Ok(StrStart::Quoted),
            Some(b'{') => Ok(StrStart::Literal),
            Some(byte)
                // `%` and `*` are legal starting bytes: a `LIST`/`LSUB` pattern
                // is an atom whose alphabet includes the list wildcards, and
                // `read_mailbox_atom` consumes them. Every other atom reader
                // stops at those bytes on its own.
                if byte > 0x20 && byte < 0x7f && !matches!(byte, b'(' | b')' | b'\\' | b']') =>
            {
                Ok(StrStart::Atom)
            }
            Some(_) => Err(self.bad_here("expected a string")),
            None => Err(self.bad_here("expected a string, found the end of the line")),
        }
    }

    /// Read a literal: emit the continuation (through `source`), then take the
    /// declared octets.
    async fn read_literal_bytes<L: LiteralSource>(
        &mut self,
        source: &mut L,
    ) -> Result<Vec<u8>, FerromaError> {
        let Some((declared, synchronizing)) = self.try_literal_prefix()? else {
            return Err(bad("expected a literal"));
        };
        let bytes = source
            .read_literal(declared, synchronizing)
            .await
            .map_err(|err| match err {
                LiteralError::Declined => bad("the client declined the literal"),
                LiteralError::Io(message) => FerromaError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    message,
                )),
            })?;
        if bytes.len() as u64 != declared {
            return Err(bad(format!(
                "literal declared {declared} bytes but {} arrived",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// [`CommandParser::starts_with_token`], ignoring case.
    fn starts_with_token_ci(&self, token: &str) -> bool {
        let rest = &self.line[self.pos..];
        if rest.len() < token.len() {
            return false;
        }
        let (head, tail) = rest.split_at(token.len());
        if !head.eq_ignore_ascii_case(token) {
            return false;
        }
        match tail.chars().next() {
            None => true,
            Some(ch) => !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' && ch != '.',
        }
    }

    /// Whether the cursor is at the literal token `token`, as a whole word.
    fn starts_with_token(&self, token: &str) -> bool {
        let rest = &self.line[self.pos..];
        if !rest.starts_with(token) {
            return false;
        }
        match rest[token.len()..].chars().next() {
            None => true,
            Some(ch) => !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' && ch != '.',
        }
    }

    /// Read a non-negative decimal number.
    fn read_number(&mut self) -> Result<u64, FerromaError> {
        let start = self.pos;
        while matches!(self.peek_byte(), Some(b) if b.is_ascii_digit()) {
            self.pos += 1;
        }
        let text = &self.line[start..self.pos];
        if text.is_empty() {
            return Err(self.bad_here("expected a number"));
        }
        if text.len() > 20 {
            return Err(bad("number is too long"));
        }
        text.parse()
            .map_err(|_| bad(format!("`{text}` is not a valid number")))
    }

    // -----------------------------------------------------------------------
    // The grammar.
    // -----------------------------------------------------------------------

    async fn parse_command_body<L: LiteralSource>(
        &mut self,
        name: &str,
        source: &mut L,
    ) -> Result<Command, FerromaError> {
        match name {
            "SELECT" => {
                self.read_sp()?;
                Ok(Command::Select(self.read_mailbox(source).await?))
            }
            "EXAMINE" => {
                self.read_sp()?;
                Ok(Command::Examine(self.read_mailbox(source).await?))
            }
            "CREATE" => {
                self.read_sp()?;
                Ok(Command::Create(self.read_mailbox(source).await?))
            }
            "DELETE" => {
                self.read_sp()?;
                Ok(Command::Delete(self.read_mailbox(source).await?))
            }
            "SUBSCRIBE" => {
                self.read_sp()?;
                Ok(Command::Subscribe(self.read_mailbox(source).await?))
            }
            "UNSUBSCRIBE" => {
                self.read_sp()?;
                Ok(Command::Unsubscribe(self.read_mailbox(source).await?))
            }
            "RENAME" => {
                self.read_sp()?;
                let from = self.read_mailbox(source).await?;
                self.read_sp()?;
                let to = self.read_mailbox(source).await?;
                Ok(Command::Rename { from, to })
            }
            "LIST" => {
                self.read_sp()?;
                let reference = self.read_mailbox_pattern(source).await?;
                self.read_sp()?;
                let pattern = self.read_mailbox_pattern(source).await?;
                Ok(Command::List { reference, pattern })
            }
            "LSUB" => {
                self.read_sp()?;
                let reference = self.read_mailbox_pattern(source).await?;
                self.read_sp()?;
                let pattern = self.read_mailbox_pattern(source).await?;
                Ok(Command::Lsub { reference, pattern })
            }
            "APPEND" => self.parse_append(source).await,
            "AUTHENTICATE" => self.parse_authenticate(source).await,
            "LOGIN" => {
                self.read_sp()?;
                let user = self.read_astring(source).await?;
                self.read_sp()?;
                let password = self.read_astring(source).await?;
                Ok(Command::Login { user, password })
            }
            "FETCH" => {
                self.read_sp()?;
                let set = self.read_sequence_set()?;
                self.read_sp()?;
                let spec = self.parse_fetch_spec_sync()?;
                Ok(Command::Fetch { set, spec })
            }
            "SEARCH" => {
                self.skip_opt_sp();
                self.parse_search_tail().map(|(charset, key)| Command::Search { charset, key })
            }
            _ => self.parse_command_body_sync(name),
        }
    }

    /// The commands that cannot contain a literal.
    fn parse_command_body_sync(&mut self, name: &str) -> Result<Command, FerromaError> {
        match name {
            "CAPABILITY" => Ok(Command::Capability),
            "NOOP" => Ok(Command::Noop),
            "LOGOUT" => Ok(Command::Logout),
            "STARTTLS" => Ok(Command::StartTls),
            "CHECK" => Ok(Command::Check),
            "CLOSE" => Ok(Command::Close),
            "EXPUNGE" => Ok(Command::Expunge),
            "IDLE" => Ok(Command::Idle),
            "UNSELECT" => Ok(Command::Unselect),
            "NAMESPACE" => Ok(Command::Namespace),
            "STATUS" => {
                self.read_sp()?;
                let mailbox = self.read_mailbox_sync()?;
                self.read_sp()?;
                let items = self.parse_status_items()?;
                Ok(Command::Status { mailbox, items })
            }
            "STORE" => {
                self.read_sp()?;
                let set = self.read_sequence_set()?;
                self.read_sp()?;
                let (action, silent) = self.parse_store_action()?;
                self.read_sp()?;
                let flags = self.parse_flag_list()?;
                Ok(Command::Store {
                    set,
                    action,
                    silent,
                    flags,
                })
            }
            "COPY" => {
                self.read_sp()?;
                let set = self.read_sequence_set()?;
                self.read_sp()?;
                let mailbox = self.read_mailbox_sync()?;
                Ok(Command::Copy { set, mailbox })
            }
            "MOVE" => {
                self.read_sp()?;
                let set = self.read_sequence_set()?;
                self.read_sp()?;
                let mailbox = self.read_mailbox_sync()?;
                Ok(Command::Move { set, mailbox })
            }
            "UID" => {
                self.read_sp()?;
                let inner_name = self.read_atom_upper()?;
                match inner_name.as_str() {
                    "FETCH" => {
                        self.read_sp()?;
                        let set = self.read_sequence_set()?;
                        self.read_sp()?;
                        let spec = self.parse_fetch_spec_sync()?;
                        Ok(Command::Uid(Box::new(Command::Fetch { set, spec })))
                    }
                    "SEARCH" => {
                        self.skip_opt_sp();
                        let (charset, key) = self.parse_search_tail()?;
                        Ok(Command::Uid(Box::new(Command::Search { charset, key })))
                    }
                    "STORE" => {
                        self.read_sp()?;
                        let set = self.read_sequence_set()?;
                        self.read_sp()?;
                        let (action, silent) = self.parse_store_action()?;
                        self.read_sp()?;
                        let flags = self.parse_flag_list()?;
                        Ok(Command::Uid(Box::new(Command::Store {
                            set,
                            action,
                            silent,
                            flags,
                        })))
                    }
                    "COPY" => {
                        self.read_sp()?;
                        let set = self.read_sequence_set()?;
                        self.read_sp()?;
                        let mailbox = self.read_mailbox_sync()?;
                        Ok(Command::Uid(Box::new(Command::Copy { set, mailbox })))
                    }
                    "MOVE" => {
                        self.read_sp()?;
                        let set = self.read_sequence_set()?;
                        self.read_sp()?;
                        let mailbox = self.read_mailbox_sync()?;
                        Ok(Command::Uid(Box::new(Command::Move { set, mailbox })))
                    }
                    "EXPUNGE" => {
                        // RFC 4315: `UID EXPUNGE <set>` restricts the expunge to
                        // those UIDs; a bare `UID EXPUNGE` is not legal but is
                        // treated as the unrestricted form.
                        self.skip_opt_sp();
                        if self.at_end() {
                            return Ok(Command::Uid(Box::new(Command::Expunge)));
                        }
                        let set = self.read_sequence_set()?;
                        Ok(Command::Uid(Box::new(Command::UidExpunge(set))))
                    }
                    other => Err(bad(format!("UID {other} is not a supported command"))),
                }
            }
            "ID" => {
                self.skip_opt_sp();
                // The argument list is a parenthesised list or NIL; both are
                // consumed without being used.
                if self.starts_with_token("NIL") {
                    self.pos += 3;
                } else if self.peek_byte() == Some(b'(') {
                    self.skip_parenthesised()?;
                }
                Ok(Command::Id)
            }
            "ENABLE" => {
                let mut names = Vec::new();
                while !self.at_end() {
                    self.read_sp()?;
                    if self.at_end() {
                        break;
                    }
                    names.push(self.read_arg_sync()?);
                }
                Ok(Command::Enable(names))
            }
            other => Err(bad(format!("unknown command `{other}`"))),
        }
    }

    /// `APPEND mailbox [(flags)] [date] {literal}`.
    async fn parse_append<L: LiteralSource>(&mut self, source: &mut L) -> Result<Command, FerromaError> {
        self.read_sp()?;
        let mailbox = match self.assert_string_start()? {
            StrStart::Quoted => self.read_quoted()?,
            StrStart::Atom => crate::mailbox::canonical(&self.read_atom()?),
            StrStart::Literal => {
                let bytes = self.read_literal_bytes(source).await?;
                crate::mailbox::canonical(&decode_name(&bytes))
            }
        };

        let mut flags = Vec::new();
        let mut internal_date = None;

        if self.peek_byte() == Some(b' ') {
            self.skip_sp();
            if self.peek_byte() == Some(b'(') {
                flags = self.parse_flag_list()?;
                self.skip_opt_sp();
            }
            if self.peek_byte() == Some(b'"') {
                let raw = self.read_quoted()?;
                internal_date = Some(parse_internal_date(&raw)?);
                self.skip_opt_sp();
            }
        }

        let message = self.read_raw_string(source).await?;
        Ok(Command::Append {
            mailbox,
            flags,
            internal_date,
            message,
        })
    }

    /// `AUTHENTICATE mechanism [initial-response]`.
    async fn parse_authenticate<L: LiteralSource>(
        &mut self,
        source: &mut L,
    ) -> Result<Command, FerromaError> {
        self.read_sp()?;
        let mechanism = self.read_atom_upper()?;
        let mut initial_response = None;
        if !self.at_end() {
            self.read_sp()?;
            let raw = match self.assert_string_start()? {
                StrStart::Quoted => self.read_quoted()?.into_bytes(),
                StrStart::Literal => self.read_literal_bytes(source).await?,
                StrStart::Atom => self.read_atom()?.into_bytes(),
            };
            if raw == b"=" {
                initial_response = Some(Vec::new());
            } else {
                initial_response = Some(base64_decode(&raw)?);
            }
        }
        Ok(Command::Authenticate {
            mechanism,
            initial_response,
        })
    }

    /// Parse `FLAGS`, `+FLAGS`, `-FLAGS` and their `.SILENT` forms.
    fn parse_store_action(&mut self) -> Result<(StoreAction, bool), FerromaError> {
        let sign = match self.peek_byte() {
            Some(b'+') => {
                self.pos += 1;
                Some(StoreAction::Add)
            }
            Some(b'-') => {
                self.pos += 1;
                Some(StoreAction::Remove)
            }
            _ => None,
        };
        let start = self.pos;
        while matches!(self.peek_byte(), Some(b) if b.is_ascii_alphabetic() || b == b'.') {
            self.pos += 1;
        }
        let word = self.line[start..self.pos].to_ascii_uppercase();
        let action = match (sign, word.as_str()) {
            (None, "FLAGS") => StoreAction::Replace,
            (Some(action), "FLAGS") => action,
            (None, "FLAGS.SILENT") => StoreAction::Replace,
            (Some(action), "FLAGS.SILENT") => action,
            _ => return Err(self.bad_here("unsupported STORE item")),
        };
        self.pos = start + word.len();
        let silent = word.ends_with(".SILENT");
        Ok((action, silent))
    }

    /// Parse a `(flag flag …)` list, tolerating a missing pair of parentheses.
    fn parse_flag_list(&mut self) -> Result<Vec<String>, FerromaError> {
        let parenthesised = self.peek_byte() == Some(b'(');
        if parenthesised {
            self.pos += 1;
        }
        let mut flags = Vec::new();
        loop {
            self.skip_sp();
            if parenthesised && self.peek_byte() == Some(b')') {
                self.pos += 1;
                break;
            }
            if !parenthesised && self.at_end() {
                break;
            }
            if !parenthesised && self.peek_byte() == Some(b')') {
                return Err(self.bad_here("unbalanced `)` in flag list"));
            }
            let start = self.pos;
            if self.peek_byte() == Some(b'\\') {
                self.pos += 1;
            }
            while matches!(self.peek_byte(), Some(b) if b > 0x20 && b < 0x7f && !matches!(b, b'(' | b')' | b'"' | b'\\' | b'{')) {
                self.pos += 1;
            }
            if self.pos == start {
                return Err(self.bad_here("expected a flag"));
            }
            let flag = self.line[start..self.pos].to_string();
            if flag.len() > MAX_ATOM_LEN {
                return Err(bad("flag name is too long"));
            }
            flags.push(flag);
            if flags.len() > 128 {
                return Err(bad("too many flags in one command"));
            }
        }
        if flags.is_empty() {
            return Err(bad("a flag list must not be empty"));
        }
        Ok(flags)
    }

    /// Parse a `(item item …)` `STATUS` list.
    fn parse_status_items(&mut self) -> Result<Vec<crate::response::StatusItem>, FerromaError> {
        if self.peek_byte() != Some(b'(') {
            return Err(self.bad_here("expected `(` before the STATUS item list"));
        }
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_sp();
            if self.peek_byte() == Some(b')') {
                self.pos += 1;
                break;
            }
            let name = self.read_atom_upper()?;
            match name.as_str() {
                "MESSAGES" => items.push(crate::response::StatusItem::Messages),
                "RECENT" => items.push(crate::response::StatusItem::Recent),
                "UIDNEXT" => items.push(crate::response::StatusItem::UidNext),
                "UIDVALIDITY" => items.push(crate::response::StatusItem::UidValidity),
                "UNSEEN" => items.push(crate::response::StatusItem::Unseen),
                "SIZE" => items.push(crate::response::StatusItem::Size),
                other => {
                    return Err(bad(format!("unsupported STATUS item `{other}`")));
                }
            }
            if items.len() > 32 {
                return Err(bad("too many STATUS items"));
            }
        }
        if items.is_empty() {
            return Err(bad("STATUS needs at least one item"));
        }
        Ok(items)
    }

    /// Read a sequence set, stopping at the first byte that cannot be part of one.
    fn read_sequence_set(&mut self) -> Result<SequenceSet, FerromaError> {
        let start = self.pos;
        while matches!(self.peek_byte(), Some(b) if b.is_ascii_digit() || matches!(b, b'*' | b':' | b','))
        {
            self.pos += 1;
        }
        let text = &self.line[start..self.pos];
        if text.is_empty() {
            return Err(self.bad_here("expected a message set"));
        }
        SequenceSet::parse(text)
    }

    /// Read a mailbox name: quoted, literal, or atom.
    async fn read_mailbox<L: LiteralSource>(
        &mut self,
        source: &mut L,
    ) -> Result<String, FerromaError> {
        match self.assert_string_start()? {
            StrStart::Quoted => Ok(crate::mailbox::canonical(&self.read_quoted()?)),
            StrStart::Atom => Ok(crate::mailbox::canonical(&self.read_mailbox_atom()?)),
            StrStart::Literal => {
                let bytes = self.read_literal_bytes(source).await?;
                Ok(crate::mailbox::canonical(&decode_name(&bytes)))
            }
        }
    }

    /// Read a `LIST`/`LSUB` reference or pattern.
    ///
    /// A pattern may carry the `%`/`*` list wildcards that an ordinary mailbox
    /// name may not, so the atom alphabet is wider here.
    async fn read_mailbox_pattern<L: LiteralSource>(
        &mut self,
        source: &mut L,
    ) -> Result<String, FerromaError> {
        match self.assert_string_start()? {
            StrStart::Quoted => self.read_quoted(),
            StrStart::Atom => self.read_mailbox_atom(),
            StrStart::Literal => {
                let bytes = self.read_literal_bytes(source).await?;
                Ok(decode_name(&bytes))
            }
        }
    }

    /// Read a mailbox atom, whose alphabet includes the `LIST` wildcards.
    fn read_mailbox_atom(&mut self) -> Result<String, FerromaError> {
        let start = self.pos;
        while matches!(self.peek_byte(), Some(b) if b > 0x20 && b < 0x7f && !matches!(b, b'(' | b')' | b'{' | b'"' | b'\\' | b']')) {
            self.pos += 1;
        }
        let atom = &self.line[start..self.pos];
        if atom.is_empty() {
            return Err(self.bad_here("expected a mailbox name"));
        }
        if atom.len() > MAX_ATOM_LEN {
            return Err(bad("mailbox name is longer than the maximum accepted length"));
        }
        Ok(atom.to_string())
    }

    /// Read a mailbox name (atom or quoted string — no literals here).
    fn read_mailbox_sync(&mut self) -> Result<String, FerromaError> {
        match self.assert_string_start()? {
            StrStart::Quoted => Ok(crate::mailbox::canonical(&self.read_quoted()?)),
            StrStart::Atom => Ok(crate::mailbox::canonical(&self.read_atom()?)),
            StrStart::Literal => Err(self.bad_here("a literal is not allowed here")),
        }
    }

    /// Read an atom-or-quoted argument without literals.
    fn read_arg_sync(&mut self) -> Result<String, FerromaError> {
        match self.assert_string_start()? {
            StrStart::Quoted => self.read_quoted(),
            StrStart::Atom => self.read_atom(),
            StrStart::Literal => Err(self.bad_here("a literal is not allowed here")),
        }
    }

    /// Consume a balanced parenthesised list without interpreting it.
    fn skip_parenthesised(&mut self) -> Result<(), FerromaError> {
        if self.peek_byte() != Some(b'(') {
            return Err(self.bad_here("expected `(`"));
        }
        let mut depth = 0usize;
        let mut in_quote = false;
        while let Some(ch) = self.bump() {
            match ch {
                '\\' if in_quote => {
                    self.bump();
                }
                '"' => in_quote = !in_quote,
                '(' if !in_quote => depth += 1,
                ')' if !in_quote => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Err(bad("unbalanced parentheses"))
    }

    /// Parse the `[CHARSET c] keys` tail of a `SEARCH`.
    fn parse_search_tail(&mut self) -> Result<(Option<String>, Option<SearchKey>), FerromaError> {
        let mut charset = None;
        if self.starts_with_token_ci("CHARSET") {
            self.pos += "CHARSET".len();
            self.skip_sp();
            let name = self.read_arg_sync()?;
            charset = Some(name.to_ascii_uppercase());
        }
        self.skip_sp();
        if self.at_end() {
            // `SEARCH` with no keys is illegal: RFC 3501 requires at least one.
            return Err(bad("SEARCH needs at least one key"));
        }
        let key = self.parse_search_key()?;
        Ok((charset, Some(key)))
    }

    /// Parse one search key (recursive descent over the RFC 3501 `search-key`
    /// grammar).
    fn parse_search_key(&mut self) -> Result<SearchKey, FerromaError> {
        self.skip_sp();
        if self.peek_byte() == Some(b'(') {
            self.pos += 1;
            let mut keys = Vec::new();
            loop {
                self.skip_sp();
                if self.peek_byte() == Some(b')') {
                    self.pos += 1;
                    break;
                }
                if self.at_end() {
                    return Err(bad("unbalanced parentheses in SEARCH"));
                }
                keys.push(self.parse_search_key()?);
                if keys.len() > 1000 {
                    return Err(bad("SEARCH expression is too deeply nested"));
                }
            }
            if keys.is_empty() {
                // RFC 3501: `()` is not a legal key, but treating it as "match
                // nothing" is friendlier than an error and cannot be exploited.
                return Ok(SearchKey::SequenceSet(SequenceSet::empty()));
            }
            return Ok(if keys.len() == 1 {
                keys.remove(0)
            } else {
                SearchKey::And(keys)
            });
        }

        if self.starts_with_token("NOT") {
            self.pos += 3;
            self.skip_sp();
            let inner = self.parse_search_key()?;
            return Ok(SearchKey::Not(Box::new(inner)));
        }
        if self.starts_with_token("OR") {
            self.pos += 2;
            self.skip_sp();
            let left = self.parse_search_key()?;
            self.skip_sp();
            let right = self.parse_search_key()?;
            return Ok(SearchKey::Or(Box::new(left), Box::new(right)));
        }

        // A message set is `n`, `n:m`, `n,*`, `*`, … — anything that starts with
        // a digit or `*` is a set and must not be read as a keyword.
        if matches!(self.peek_byte(), Some(b) if b.is_ascii_digit() || b == b'*') {
            let set = self.read_sequence_set()?;
            return Ok(SearchKey::SequenceSet(set));
        }

        let name = self.read_atom_upper()?;
        let key = match name.as_str() {
            "ALL" => SearchKey::All,
            "ANSWERED" => SearchKey::Answered,
            "DELETED" => SearchKey::Deleted,
            "DRAFT" => SearchKey::Draft,
            "FLAGGED" => SearchKey::Flagged,
            "NEW" => SearchKey::New,
            "OLD" => SearchKey::Old,
            "RECENT" => SearchKey::Recent,
            "SEEN" => SearchKey::Seen,
            "UNANSWERED" => SearchKey::Unanswered,
            "UNDELETED" => SearchKey::Undeleted,
            "UNDRAFT" => SearchKey::Undraft,
            "UNFLAGGED" => SearchKey::Unflagged,
            "UNSEEN" => SearchKey::Unseen,
            "BCC" => SearchKey::Bcc(self.read_search_string()?),
            "BODY" => SearchKey::Body(self.read_search_string()?),
            "CC" => SearchKey::Cc(self.read_search_string()?),
            "FROM" => SearchKey::From(self.read_search_string()?),
            "SUBJECT" => SearchKey::Subject(self.read_search_string()?),
            "TEXT" => SearchKey::Text(self.read_search_string()?),
            "TO" => SearchKey::To(self.read_search_string()?),
            "KEYWORD" => SearchKey::Keyword(self.read_search_flag()?),
            "UNKEYWORD" => SearchKey::Unkeyword(self.read_search_flag()?),
            "BEFORE" => SearchKey::Before(self.read_search_date()?),
            "ON" => SearchKey::On(self.read_search_date()?),
            "SINCE" => SearchKey::Since(self.read_search_date()?),
            "SENTBEFORE" => SearchKey::SentBefore(self.read_search_date()?),
            "SENTON" => SearchKey::SentOn(self.read_search_date()?),
            "SENTSINCE" => SearchKey::SentSince(self.read_search_date()?),
            "LARGER" => SearchKey::Larger(self.read_search_number()?),
            "SMALLER" => SearchKey::Smaller(self.read_search_number()?),
            "UID" => {
                self.skip_sp();
                SearchKey::Uid(self.read_sequence_set()?)
            }
            "HEADER" => {
                self.skip_sp();
                let field = self.read_search_string_raw()?;
                self.skip_sp();
                let value = self.read_search_string_raw()?;
                SearchKey::Header(field, value)
            }
            other => {
                return Err(bad(format!("unknown SEARCH key `{other}`")));
            }
        };
        Ok(key)
    }

    /// A SEARCH string argument: atom, quoted string or literal (no literals in
    /// the synchronous grammar — `SEARCH` is parsed from a line that may contain
    /// literals, but SEARCH literals are vanishingly rare and the grammar here
    /// accepts the atom and quoted forms).
    fn read_search_string(&mut self) -> Result<String, FerromaError> {
        self.skip_sp();
        self.read_search_string_raw()
    }

    fn read_search_string_raw(&mut self) -> Result<String, FerromaError> {
        match self.assert_string_start()? {
            StrStart::Quoted => self.read_quoted(),
            StrStart::Atom => self.read_atom(),
            StrStart::Literal => Err(self.bad_here("a literal is not allowed in SEARCH")),
        }
    }

    fn read_search_flag(&mut self) -> Result<String, FerromaError> {
        self.skip_sp();
        let start = self.pos;
        if self.peek_byte() == Some(b'\\') {
            self.pos += 1;
        }
        while matches!(self.peek_byte(), Some(b) if b > 0x20 && b < 0x7f && !matches!(b, b'(' | b')' | b'"')) {
            self.pos += 1;
        }
        let flag = self.line[start..self.pos].to_string();
        if flag.is_empty() {
            return Err(self.bad_here("expected a flag name"));
        }
        Ok(flag)
    }

    fn read_search_date(&mut self) -> Result<chrono::DateTime<chrono::Utc>, FerromaError> {
        self.skip_sp();
        let raw = self.read_search_string_raw()?;
        util::parse_date(&raw).ok_or_else(|| bad(format!("`{raw}` is not a valid date")))
    }

    fn read_search_number(&mut self) -> Result<u64, FerromaError> {
        self.skip_sp();
        self.read_number()
    }

    /// Parse a `FETCH` item list.
    ///
    /// `FETCH` item names are atoms; the only place a literal could appear is
    /// inside a `BODY[HEADER.FIELDS (…)]` list, which no real client sends, so
    /// the item grammar is literals-free by construction.
    fn parse_fetch_spec_sync(&mut self) -> Result<FetchSpec, FerromaError> {
        let parenthesised = self.peek_byte() == Some(b'(');
        if !parenthesised {
            let name = self.read_fetch_atom()?;
            return match name.to_ascii_uppercase().as_str() {
                "ALL" => Ok(FetchSpec::Macro(FetchSpec::all())),
                "FAST" => Ok(FetchSpec::Macro(FetchSpec::fast())),
                "FULL" => Ok(FetchSpec::Macro(FetchSpec::full())),
                _ => Ok(FetchSpec::Items(vec![self.parse_fetch_item_name(&name)?])),
            };
        }

        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_sp();
            if self.peek_byte() == Some(b')') {
                self.pos += 1;
                break;
            }
            if self.at_end() {
                return Err(bad("unterminated FETCH item list"));
            }
            if items.len() > 128 {
                return Err(bad("FETCH item list is too long"));
            }
            items.push(self.parse_fetch_item()?);
        }
        if items.is_empty() {
            return Err(bad("FETCH needs at least one item"));
        }
        Ok(FetchSpec::Items(items))
    }

    /// Read a `FETCH` item name: an atom, up to (but not including) a `[`.
    fn read_fetch_atom(&mut self) -> Result<String, FerromaError> {
        let start = self.pos;
        while matches!(self.peek_byte(), Some(b) if b > 0x20 && b < 0x7f && !matches!(b, b'(' | b')' | b'{' | b'"' | b'[')) {
            self.pos += 1;
        }
        let atom = self.line[start..self.pos].to_string();
        if atom.is_empty() {
            return Err(self.bad_here("expected a FETCH item"));
        }
        if atom.len() > MAX_ATOM_LEN {
            return Err(bad("FETCH item name is too long"));
        }
        Ok(atom)
    }

    /// One `FETCH` item, including any `[section]` and `<start.len>` partial.
    fn parse_fetch_item(&mut self) -> Result<FetchItem, FerromaError> {
        let name = self.read_fetch_atom()?;
        self.parse_fetch_item_name(&name)
    }

    /// Turn an already-read `FETCH` item name into a [`FetchItem`].
    ///
    /// Any trailing `[section]` / `<start.len>` is still unconsumed on the
    /// cursor when this is called.
    fn parse_fetch_item_name(&mut self, name: &str) -> Result<FetchItem, FerromaError> {
        let upper = name.to_ascii_uppercase();

        // The `BODY`/`BODY.PEEK` family is the only one that takes a section.
        let body_prefix = if upper == "BODY" {
            Some(("BODY", false))
        } else if upper == "BODY.PEEK" {
            Some(("BODY.PEEK", true))
        } else {
            None
        };
        if let Some((_, peek)) = body_prefix {
            if self.peek_byte() == Some(b'[') {
                let section = self.parse_bracketed_section()?;
                let partial = self.try_partial()?;
                return Ok(if peek {
                    FetchItem::BodyPeekSection(section, partial)
                } else {
                    FetchItem::BodySection(section, partial)
                });
            }
            if peek {
                return Err(bad("BODY.PEEK needs a section specification"));
            }
            return Ok(FetchItem::Body);
        }

        match upper.as_str() {
            "UID" => Ok(FetchItem::Uid),
            "FLAGS" => Ok(FetchItem::Flags),
            "INTERNALDATE" => Ok(FetchItem::InternalDate),
            "RFC822.SIZE" => Ok(FetchItem::Rfc822Size),
            "ENVELOPE" => Ok(FetchItem::Envelope),
            "BODYSTRUCTURE" => Ok(FetchItem::BodyStructure),
            "RFC822" => Ok(FetchItem::Rfc822),
            "RFC822.HEADER" => Ok(FetchItem::Rfc822Header),
            "RFC822.TEXT" => Ok(FetchItem::Rfc822Text),
            other => Err(bad(format!("unknown FETCH item `{other}`"))),
        }
    }

    /// Parse `[ ... ]`, the section specification.
    fn parse_bracketed_section(&mut self) -> Result<Section, FerromaError> {
        if self.peek_byte() != Some(b'[') {
            return Err(self.bad_here("expected `[`"));
        }
        self.pos += 1;
        let inner = self.read_section_body()?;
        if self.peek_byte() != Some(b']') {
            return Err(self.bad_here("expected `]`"));
        }
        self.pos += 1;
        Ok(inner)
    }

    /// Parse the content between the `[` and `]` of a section.
    fn read_section_body(&mut self) -> Result<Section, FerromaError> {
        if self.peek_byte() == Some(b']') {
            return Ok(Section::All);
        }
        // A leading numeric part path, if any. Each component is a run of digits
        // read directly: `read_number` is not usable here because a path is
        // followed by `.MIME` / `.HEADER` / … rather than by whitespace.
        let mut path: Vec<u32> = Vec::new();
        if matches!(self.peek_byte(), Some(b) if b.is_ascii_digit()) {
            loop {
                let start = self.pos;
                while matches!(self.peek_byte(), Some(b) if b.is_ascii_digit()) {
                    self.pos += 1;
                }
                let digits = &self.line[start..self.pos];
                if digits.len() > 10 {
                    return Err(bad("message part number is out of range"));
                }
                let number: u32 = digits
                    .parse()
                    .map_err(|_| bad("message part number is out of range"))?;
                if number == 0 {
                    return Err(bad("message part numbers start at 1"));
                }
                path.push(number);
                if path.len() > 16 {
                    return Err(bad("message part path is too deep"));
                }
                if self.peek_byte() == Some(b'.')
                    && matches!(self.line.as_bytes().get(self.pos + 1), Some(b) if b.is_ascii_digit())
                {
                    // A `.` followed by a digit continues the path (`1.2`).
                    self.pos += 1;
                } else {
                    // A `.` followed by anything else starts a section
                    // keyword (`1.MIME`, `1.HEADER.FIELDS`); it is consumed
                    // below, by the keyword scan.
                    break;
                }
            }
        }

        if self.peek_byte() == Some(b']') {
            if path.is_empty() {
                return Ok(Section::All);
            }
            return Ok(Section::Part(path));
        }

        // A `.` separates a part path from its section keyword (`1.MIME`).
        if self.peek_byte() == Some(b'.') {
            self.pos += 1;
        }
        let start = self.pos;
        while matches!(self.peek_byte(), Some(b) if b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            self.pos += 1;
        }
        let keyword = self.line[start..self.pos].to_ascii_uppercase();
        if keyword.is_empty() {
            return Err(self.bad_here("malformed section specification"));
        }

        let fields = if keyword == "HEADER.FIELDS" || keyword == "HEADER.FIELDS.NOT" {
            Some(self.parse_header_field_list()?)
        } else {
            None
        };

        match (keyword.as_str(), path.is_empty(), fields) {
            ("", _, _) => Ok(Section::Part(path)),
            ("HEADER", true, _) => Ok(Section::Header),
            ("TEXT", true, _) => Ok(Section::Text),
            ("HEADER.FIELDS", true, Some(names)) => Ok(Section::HeaderFields(names)),
            ("HEADER.FIELDS.NOT", true, Some(names)) => Ok(Section::HeaderFieldsNot(names)),
            ("MIME", false, _) => Ok(Section::Mime(path)),
            ("HEADER", false, _) => Ok(Section::PartHeader(path)),
            ("TEXT", false, _) => Ok(Section::PartText(path)),
            ("HEADER.FIELDS", false, Some(names)) => Ok(Section::PartHeaderFields(path, names)),
            ("HEADER.FIELDS.NOT", false, Some(names)) => {
                Ok(Section::PartHeaderFieldsNot(path, names))
            }
            ("MIME", true, _) => Err(bad("MIME needs a message part number")),
            (other, _, _) => Err(bad(format!("unsupported section `{other}`"))),
        }
    }

    /// Parse the `(FIELD FIELD …)` list of `HEADER.FIELDS`.
    fn parse_header_field_list(&mut self) -> Result<Vec<String>, FerromaError> {
        self.skip_sp();
        if self.peek_byte() != Some(b'(') {
            return Err(self.bad_here("expected `(` before the header field list"));
        }
        self.pos += 1;
        let mut names = Vec::new();
        loop {
            self.skip_sp();
            if self.peek_byte() == Some(b')') {
                self.pos += 1;
                break;
            }
            if self.at_end() {
                return Err(bad("unterminated header field list"));
            }
            let start = self.pos;
            while matches!(self.peek_byte(), Some(b) if b > 0x20 && b < 0x7f && !matches!(b, b'(' | b')' | b'"')) {
                self.pos += 1;
            }
            if self.pos == start {
                return Err(self.bad_here("expected a header field name"));
            }
            names.push(self.line[start..self.pos].to_string());
            if names.len() > 64 {
                return Err(bad("too many header field names"));
            }
        }
        if names.is_empty() {
            return Err(bad("HEADER.FIELDS needs at least one field name"));
        }
        Ok(names)
    }

    /// Parse an optional `<start.len>` partial suffix.
    fn try_partial(&mut self) -> Result<Option<util::Partial>, FerromaError> {
        if self.peek_byte() != Some(b'<') {
            return Ok(None);
        }
        self.pos += 1;
        let start = self.read_number()?;
        if self.peek_byte() != Some(b'.') {
            return Err(self.bad_here("expected `.` in a partial specifier"));
        }
        self.pos += 1;
        let len = self.read_number()?;
        if self.peek_byte() != Some(b'>') {
            return Err(self.bad_here("expected `>` at the end of a partial specifier"));
        }
        self.pos += 1;
        Ok(Some(util::Partial { start, len }))
    }
}

/// Which string production the cursor is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrStart {
    Atom,
    Quoted,
    Literal,
}

/// Turn a raw mailbox-name byte string into text.
///
/// IMAP mailbox names are *modified UTF-8*: 8-bit bytes that are not valid UTF-8
/// are decoded with replacement, which is what every server does in practice and
/// what [`crate::mailbox`] then treats as an opaque name.
fn decode_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Decode standard or URL-safe base64 (the SASL `base64` production).
fn base64_decode(raw: &[u8]) -> Result<Vec<u8>, FerromaError> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let text: String = raw
        .iter()
        .map(|b| *b as char)
        .filter(|c| !c.is_whitespace())
        .collect();
    engine
        .decode(text.as_bytes())
        .map_err(|_| bad("invalid base64 in the SASL response"))
}

/// Parse an `APPEND` `date-time`: `"01-Jan-2026 12:34:56 +0000"`.
fn parse_internal_date(raw: &str) -> Result<chrono::DateTime<chrono::Utc>, FerromaError> {
    use chrono::{NaiveDateTime, TimeZone};
    let raw = raw.trim();
    let (date, rest) = raw
        .split_once(' ')
        .ok_or_else(|| bad(format!("`{raw}` is not an INTERNALDATE")))?;
    let day = util::parse_date(date).ok_or_else(|| bad(format!("`{date}` is not a valid date")))?;
    let (time, zone) = rest
        .trim()
        .split_once(' ')
        .ok_or_else(|| bad(format!("`{raw}` is missing a time zone")))?;
    let naive_time = chrono::NaiveTime::parse_from_str(time.trim(), "%H:%M:%S")
        .map_err(|_| bad(format!("`{time}` is not a valid time")))?;
    let offset = parse_zone_offset(zone.trim())
        .ok_or_else(|| bad(format!("`{zone}` is not a valid time zone")))?;
    let naive = NaiveDateTime::new(day.date_naive(), naive_time);
    let fixed = chrono::FixedOffset::east_opt(offset)
        .ok_or_else(|| bad(format!("time zone `{zone}` is out of range")))?;
    fixed
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok_or_else(|| bad("ambiguous INTERNALDATE"))
}

/// Parse `+HHMM` / `-HHMM` into an offset in seconds.
fn parse_zone_offset(raw: &str) -> Option<i32> {
    let bytes = raw.as_bytes();
    if bytes.len() != 5 {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    if !bytes[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let hours: i32 = raw[1..3].parse().ok()?;
    let minutes: i32 = raw[3..5].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// A `BAD`-shaped parse error.
pub(crate) fn bad(message: impl Into<String>) -> FerromaError {
    FerromaError::Parse(message.into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sequence::Bound;
    use std::collections::VecDeque;

    /// An in-memory [`LiteralSource`] that records the literals the parser asked
    /// for, so tests can assert on the continuation behaviour.
    pub(crate) struct FakeLiterals {
        queue: VecDeque<Vec<u8>>,
        /// `(declared, synchronizing)` for every literal requested.
        pub(crate) requested: Vec<(u64, bool)>,
    }

    impl FakeLiterals {
        pub(crate) fn new(literals: impl IntoIterator<Item = Vec<u8>>) -> Self {
            FakeLiterals {
                queue: literals.into_iter().collect(),
                requested: Vec::new(),
            }
        }

        pub(crate) fn empty() -> Self {
            FakeLiterals::new(Vec::new())
        }
    }

    impl LiteralSource for FakeLiterals {
        async fn read_literal(
            &mut self,
            declared: u64,
            synchronizing: bool,
        ) -> Result<Vec<u8>, LiteralError> {
            self.requested.push((declared, synchronizing));
            self.queue
                .pop_front()
                .ok_or_else(|| LiteralError::Io("no literal queued".into()))
        }
    }

    async fn parse(line: &str) -> Result<ParsedCommand, FerromaError> {
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::empty();
        parser.parse(line, &mut source).await
    }

    async fn parse_with(line: &str, literals: Vec<Vec<u8>>) -> Result<ParsedCommand, FerromaError> {
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(literals);
        parser.parse(line, &mut source).await
    }

    async fn command(line: &str) -> Command {
        match parse(line).await {
            Ok(parsed) => parsed.command,
            Err(err) => panic!("`{line}` must parse, got: {err}"),
        }
    }

    async fn command_err(line: &str) -> FerromaError {
        match parse(line).await {
            Ok(_) => panic!("`{line}` must not parse"),
            Err(err) => err,
        }
    }

    // -- tags ---------------------------------------------------------------

    #[tokio::test]
    async fn tags_accept_the_legal_alphabet() {
        for tag in [
            "a",
            "A1",
            "a001",
            "tag-1",
            "tag_1",
            "123456789012345678901234567890",
        ] {
            assert_eq!(
                command(&format!("{tag} NOOP")).await,
                Command::Noop,
                "tag `{tag}`"
            );
        }
    }

    #[tokio::test]
    async fn tags_reject_illegal_alphabet() {
        for line in [
            "a.b NOOP",
            "a+b NOOP",
            "a/b NOOP",
            "a? NOOP",
            "* NOOP",
            "+ NOOP",
            "a b NOOP",
            " NOOP",
        ] {
            assert!(
                matches!(command_err(line).await, FerromaError::Parse(_)),
                "`{line}` must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn tag_longer_than_thirty_is_rejected() {
        let tag = "t".repeat(31);
        let err = command_err(&format!("{tag} NOOP")).await;
        assert!(err.to_string().contains("longer than 30"));
    }

    #[tokio::test]
    async fn crlf_injection_in_a_tag_is_rejected() {
        // The line is already split on CRLF by the reader, so an embedded CR is
        // a byte that cannot appear in a tag and must be refused.
        assert!(matches!(
            command_err("a\rx NOOP").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a\nx NOOP").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a\x00b NOOP").await,
            FerromaError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn the_parsed_tag_is_preserved_exactly() {
        let parsed = parse("A1b2 NOOP").await.expect("must parse");
        assert_eq!(parsed.tag, "A1b2");
    }

    // -- simple commands ----------------------------------------------------

    #[tokio::test]
    async fn simple_commands_parse() {
        assert_eq!(command("a CAPABILITY").await, Command::Capability);
        assert_eq!(command("a NOOP").await, Command::Noop);
        assert_eq!(command("a LOGOUT").await, Command::Logout);
        assert_eq!(command("a STARTTLS").await, Command::StartTls);
        assert_eq!(command("a CHECK").await, Command::Check);
        assert_eq!(command("a CLOSE").await, Command::Close);
        assert_eq!(command("a EXPUNGE").await, Command::Expunge);
        assert_eq!(command("a IDLE").await, Command::Idle);
        assert_eq!(command("a UNSELECT").await, Command::Unselect);
        assert_eq!(command("a NAMESPACE").await, Command::Namespace);
        assert_eq!(command("a ID NIL").await, Command::Id);
        assert_eq!(
            command("a ENABLE UTF8=ACCEPT").await,
            Command::Enable(vec!["UTF8=ACCEPT".into()])
        );
    }

    #[tokio::test]
    async fn command_names_are_case_insensitive() {
        assert_eq!(command("a noop").await, Command::Noop);
        assert_eq!(command("a NoOp").await, Command::Noop);
        assert_eq!(
            command("a select INBOX").await,
            Command::Select("INBOX".into())
        );
    }

    #[tokio::test]
    async fn unknown_commands_are_rejected() {
        let err = command_err("a FROBNICATE").await;
        assert!(err.to_string().contains("unknown command"));
        assert!(matches!(err, FerromaError::Parse(_)));
    }

    #[tokio::test]
    async fn trailing_garbage_is_rejected() {
        for line in [
            "a NOOP extra",
            "a LOGOUT now",
            "a CAPABILITY x",
            "a EXPUNGE 1",
        ] {
            assert!(
                command_err(line).await.to_string().contains("trailing"),
                "`{line}` must be rejected for trailing input"
            );
        }
    }

    // -- strings ------------------------------------------------------------

    #[tokio::test]
    async fn login_accepts_atoms_quotes_and_escapes() {
        assert_eq!(
            command("a LOGIN alice secret").await,
            Command::Login {
                user: "alice".into(),
                password: "secret".into()
            }
        );
        assert_eq!(
            command("a LOGIN \"alice@example.com\" \"p a s s\"").await,
            Command::Login {
                user: "alice@example.com".into(),
                password: "p a s s".into()
            }
        );
        assert_eq!(
            command(r#"a LOGIN "a\"b" "c\\d""#).await,
            Command::Login {
                user: "a\"b".into(),
                password: "c\\d".into()
            }
        );
    }

    #[tokio::test]
    async fn login_rejects_bad_escapes_and_unterminated_quotes() {
        assert!(command_err(r#"a LOGIN "a\b" x"#)
            .await
            .to_string()
            .contains("escape"));
        assert!(command_err("a LOGIN \"unterminated secret")
            .await
            .to_string()
            .contains("unterminated"));
        assert!(command_err("a LOGIN").await.to_string().contains("space"));
        assert!(command_err("a LOGIN alice").await.to_string().contains("space"));
    }

    #[tokio::test]
    async fn quoted_strings_reject_embedded_control_characters() {
        assert!(matches!(
            command_err("a LOGIN \"a\rb\" x").await,
            FerromaError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn non_ascii_outside_a_literal_is_rejected() {
        assert!(matches!(
            command_err("a SELECT naïve").await,
            FerromaError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn a_100kb_atom_is_rejected_not_allocated() {
        let line = format!("a SELECT {}", "x".repeat(100 * 1024));
        let err = command_err(&line).await;
        assert!(err.to_string().contains("longer than"));
    }

    #[tokio::test]
    async fn unbalanced_parentheses_are_rejected() {
        assert!(command_err("a STATUS INBOX (MESSAGES")
            .await
            .to_string()
            .contains("STATUS"));
        assert!(command_err("a FETCH 1 (UID FLAGS")
            .await
            .to_string()
            .contains("unterminated"));
        assert!(matches!(
            command_err("a FETCH 1 BODY[HEADER.FIELDS (SUBJECT").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a ID (a b").await,
            FerromaError::Parse(_)
        ));
    }

    // -- sequence sets ------------------------------------------------------

    #[tokio::test]
    async fn sequence_sets_parse_in_every_command() {
        match command("a FETCH 1,3:5,7:* (UID)").await {
            Command::Fetch { set, .. } => {
                assert_eq!(set.ranges().len(), 3);
                assert_eq!(set.ranges()[0], crate::sequence::SeqRange::single(1));
                assert_eq!(set.ranges()[1].from, Bound::Number(3));
                assert_eq!(set.ranges()[1].to, Bound::Number(5));
                assert_eq!(set.ranges()[2].to, Bound::Star);
            }
            other => panic!("expected FETCH, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn star_in_a_bad_position_is_rejected() {
        assert!(matches!(
            command_err("a FETCH *: (UID)").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a FETCH 1:*:2 (UID)").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a FETCH 1,,2 (UID)").await,
            FerromaError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn negative_and_u32_max_numbers() {
        assert!(matches!(
            command_err("a FETCH -1 (UID)").await,
            FerromaError::Parse(_)
        ));
        match command("a FETCH 4294967295 (UID)").await {
            Command::Fetch { set, .. } => {
                assert_eq!(
                    set.ranges()[0],
                    crate::sequence::SeqRange::single(4_294_967_295)
                );
            }
            other => panic!("expected FETCH, got {other:?}"),
        }
        assert!(matches!(
            command("a FETCH 9999999999999 (UID)").await,
            Command::Fetch { .. }
        ));
    }

    // -- SELECT/EXAMINE/CREATE/... ------------------------------------------

    #[tokio::test]
    async fn mailbox_commands_parse_and_canonicalise_inbox() {
        assert_eq!(
            command("a SELECT inbox").await,
            Command::Select("INBOX".into())
        );
        assert_eq!(
            command("a EXAMINE INBOX").await,
            Command::Examine("INBOX".into())
        );
        assert_eq!(
            command("a SELECT \"My Folder\"").await,
            Command::Select("My Folder".into())
        );
        assert_eq!(
            command("a CREATE Archive/2026").await,
            Command::Create("Archive/2026".into())
        );
        assert_eq!(command("a DELETE Old").await, Command::Delete("Old".into()));
        assert_eq!(
            command("a RENAME Old New").await,
            Command::Rename {
                from: "Old".into(),
                to: "New".into()
            }
        );
        assert_eq!(
            command("a SUBSCRIBE Sent").await,
            Command::Subscribe("Sent".into())
        );
        assert_eq!(
            command("a UNSUBSCRIBE Sent").await,
            Command::Unsubscribe("Sent".into())
        );
    }

    #[tokio::test]
    async fn rename_needs_two_mailboxes() {
        assert!(matches!(
            command_err("a RENAME Only").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a RENAME").await,
            FerromaError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn list_and_lsub_accept_empty_and_wildcard_arguments() {
        assert_eq!(
            command("a LIST \"\" \"*\"").await,
            Command::List {
                reference: "".into(),
                pattern: "*".into()
            }
        );
        assert_eq!(
            command("a LIST \"\" \"\"").await,
            Command::List {
                reference: "".into(),
                pattern: "".into()
            }
        );
        assert_eq!(
            command("a LSUB \"\" %").await,
            Command::Lsub {
                reference: "".into(),
                pattern: "%".into()
            }
        );
    }

    #[tokio::test]
    async fn status_items_parse() {
        match command("a STATUS INBOX (MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN)").await {
            Command::Status { mailbox, items } => {
                assert_eq!(mailbox, "INBOX");
                assert_eq!(items.len(), 5);
                assert_eq!(items[0], crate::response::StatusItem::Messages);
                assert_eq!(items[4], crate::response::StatusItem::Unseen);
            }
            other => panic!("expected STATUS, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_rejects_unknown_and_empty_item_lists() {
        assert!(matches!(
            command_err("a STATUS INBOX (MESSAGES BOGUS)").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a STATUS INBOX ()").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a STATUS INBOX MESSAGES").await,
            FerromaError::Parse(_)
        ));
    }

    // -- AUTHENTICATE -------------------------------------------------------

    #[tokio::test]
    async fn authenticate_plain_without_an_initial_response() {
        assert_eq!(
            command("a AUTHENTICATE PLAIN").await,
            Command::Authenticate {
                mechanism: "PLAIN".into(),
                initial_response: None
            }
        );
        assert_eq!(
            command("a authenticate plain").await,
            Command::Authenticate {
                mechanism: "PLAIN".into(),
                initial_response: None
            }
        );
    }

    #[tokio::test]
    async fn authenticate_plain_with_a_base64_initial_response() {
        // base64("alice\0alice\0secret")
        let encoded = "YWxpY2UAYWxpY2UAc2VjcmV0";
        match command(&format!("a AUTHENTICATE PLAIN {encoded}")).await {
            Command::Authenticate {
                mechanism,
                initial_response,
            } => {
                assert_eq!(mechanism, "PLAIN");
                assert_eq!(
                    initial_response.expect("must decode"),
                    b"alice\0alice\0secret".to_vec()
                );
            }
            other => panic!("expected AUTHENTICATE, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn authenticate_accepts_the_empty_initial_response_and_rejects_bad_base64() {
        match command("a AUTHENTICATE PLAIN =").await {
            Command::Authenticate {
                initial_response, ..
            } => assert_eq!(initial_response, Some(Vec::new())),
            other => panic!("expected AUTHENTICATE, got {other:?}"),
        }
        assert!(command_err("a AUTHENTICATE PLAIN !!!not base64!!!")
            .await
            .to_string()
            .contains("base64"));
    }

    // -- FETCH items --------------------------------------------------------

    #[tokio::test]
    async fn fetch_macros_expand() {
        match command("a FETCH 1 ALL").await {
            Command::Fetch { spec, .. } => {
                assert_eq!(
                    spec.items(),
                    &[
                        FetchItem::Flags,
                        FetchItem::InternalDate,
                        FetchItem::Rfc822Size,
                        FetchItem::Envelope
                    ]
                );
            }
            other => panic!("expected FETCH, got {other:?}"),
        }
        match command("a FETCH 1 FAST").await {
            Command::Fetch { spec, .. } => assert_eq!(spec.items().len(), 3),
            other => panic!("expected FETCH, got {other:?}"),
        }
        match command("a FETCH 1 FULL").await {
            Command::Fetch { spec, .. } => {
                assert_eq!(spec.items().len(), 5);
                assert_eq!(spec.items()[4], FetchItem::Body);
            }
            other => panic!("expected FETCH, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_macros_are_case_insensitive() {
        assert!(matches!(
            command("a FETCH 1 all").await,
            Command::Fetch { .. }
        ));
        assert!(matches!(
            command("a FETCH 1 fast").await,
            Command::Fetch { .. }
        ));
        assert!(matches!(
            command("a FETCH 1 full").await,
            Command::Fetch { .. }
        ));
    }

    #[tokio::test]
    async fn fetch_single_item_without_parentheses() {
        match command("a FETCH 1 UID").await {
            Command::Fetch { spec, .. } => assert_eq!(spec.items(), &[FetchItem::Uid]),
            other => panic!("expected FETCH, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_item_list_parses_every_item() {
        let line = "a FETCH 1 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODY BODYSTRUCTURE RFC822 RFC822.HEADER RFC822.TEXT)";
        match command(line).await {
            Command::Fetch { spec, .. } => {
                assert_eq!(spec.items().len(), 10);
                assert_eq!(spec.items()[0], FetchItem::Uid);
                assert_eq!(spec.items()[7], FetchItem::Rfc822);
                assert_eq!(spec.items()[8], FetchItem::Rfc822Header);
                assert_eq!(spec.items()[9], FetchItem::Rfc822Text);
            }
            other => panic!("expected FETCH, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_body_sections_parse() {
        let cases = [
            ("BODY[]", Section::All),
            ("BODY[TEXT]", Section::Text),
            ("BODY[HEADER]", Section::Header),
            ("BODY[1]", Section::Part(vec![1])),
            ("BODY[1.2.3]", Section::Part(vec![1, 2, 3])),
            ("BODY[1.MIME]", Section::Mime(vec![1])),
            ("BODY[2.HEADER]", Section::PartHeader(vec![2])),
            ("BODY[2.TEXT]", Section::PartText(vec![2])),
            (
                "BODY[HEADER.FIELDS (SUBJECT FROM)]",
                Section::HeaderFields(vec!["SUBJECT".into(), "FROM".into()]),
            ),
            (
                "BODY[HEADER.FIELDS.NOT (RECEIVED)]",
                Section::HeaderFieldsNot(vec!["RECEIVED".into()]),
            ),
            (
                "BODY[1.HEADER.FIELDS (CONTENT-TYPE)]",
                Section::PartHeaderFields(vec![1], vec!["CONTENT-TYPE".into()]),
            ),
            (
                "BODY[1.HEADER.FIELDS.NOT (CONTENT-TYPE)]",
                Section::PartHeaderFieldsNot(vec![1], vec!["CONTENT-TYPE".into()]),
            ),
        ];
        for (item, expected) in cases {
            match command(&format!("a FETCH 1 {item}")).await {
                Command::Fetch { spec, .. } => {
                    assert_eq!(spec.items().len(), 1, "{item}");
                    match &spec.items()[0] {
                        FetchItem::BodySection(section, partial) => {
                            assert_eq!(section, &expected, "{item}");
                            assert!(partial.is_none());
                        }
                        other => panic!("{item}: expected a body section, got {other:?}"),
                    }
                }
                other => panic!("expected FETCH, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_peek_is_marked_and_does_not_set_seen() {
        match command("a FETCH 1 BODY.PEEK[]").await {
            Command::Fetch { spec, .. } => match &spec.items()[0] {
                FetchItem::BodyPeekSection(section, partial) => {
                    assert_eq!(section, &Section::All);
                    assert!(partial.is_none());
                    assert!(!spec.items()[0].sets_seen());
                }
                other => panic!("expected BODY.PEEK, got {other:?}"),
            },
            other => panic!("expected FETCH, got {other:?}"),
        }
        match command("a FETCH 1 BODY[]").await {
            Command::Fetch { spec, .. } => assert!(spec.items()[0].sets_seen()),
            other => panic!("expected FETCH, got {other:?}"),
        }
        match command("a FETCH 1 RFC822").await {
            Command::Fetch { spec, .. } => assert!(spec.items()[0].sets_seen()),
            other => panic!("expected FETCH, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_partials_parse() {
        match command("a FETCH 1 BODY[]<0.1024>").await {
            Command::Fetch { spec, .. } => match &spec.items()[0] {
                FetchItem::BodySection(section, partial) => {
                    assert_eq!(section, &Section::All);
                    assert_eq!(*partial, Some(util::Partial { start: 0, len: 1024 }));
                }
                other => panic!("expected a body section, got {other:?}"),
            },
            other => panic!("expected FETCH, got {other:?}"),
        }
        match command("a FETCH 1 BODY.PEEK[TEXT]<100.50>").await {
            Command::Fetch { spec, .. } => match &spec.items()[0] {
                FetchItem::BodyPeekSection(section, partial) => {
                    assert_eq!(section, &Section::Text);
                    assert_eq!(*partial, Some(util::Partial { start: 100, len: 50 }));
                }
                other => panic!("expected BODY.PEEK, got {other:?}"),
            },
            other => panic!("expected FETCH, got {other:?}"),
        }
        match command("a FETCH 1 BODY[1.MIME]<0.20>").await {
            Command::Fetch { spec, .. } => match &spec.items()[0] {
                FetchItem::BodySection(section, partial) => {
                    assert_eq!(section, &Section::Mime(vec![1]));
                    assert_eq!(*partial, Some(util::Partial { start: 0, len: 20 }));
                }
                other => panic!("expected a body section, got {other:?}"),
            },
            other => panic!("expected FETCH, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_rejects_malformed_sections_and_partials() {
        for line in [
            "a FETCH 1 BODY[",
            "a FETCH 1 BODY[]<0>",
            "a FETCH 1 BODY[]<0.>",
            "a FETCH 1 BODY[]<.5>",
            "a FETCH 1 BODY[HEADER.FIELDS]",
            "a FETCH 1 BODY[HEADER.FIELDS ()]",
            "a FETCH 1 BODY[MIME]",
            "a FETCH 1 BODY[BOGUS]",
            "a FETCH 1 BOGUS",
            "a FETCH 1 ()",
        ] {
            assert!(
                matches!(command_err(line).await, FerromaError::Parse(_)),
                "`{line}` must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn fetch_item_response_keys_are_canonical() {
        let cases = [
            ("BODY[]", "BODY[]"),
            ("BODY[TEXT]", "BODY[TEXT]"),
            (
                "BODY[HEADER.FIELDS (SUBJECT)]",
                "BODY[HEADER.FIELDS (SUBJECT)]",
            ),
            ("BODY.PEEK[1]", "BODY[1]"),
            ("BODY[1.MIME]<0.10>", "BODY[1.MIME]<0.10>"),
        ];
        for (item, expected) in cases {
            match command(&format!("a FETCH 1 {item}")).await {
                Command::Fetch { spec, .. } => {
                    assert_eq!(spec.items()[0].response_key(), expected, "{item}");
                }
                other => panic!("expected FETCH, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_peek_conversion() {
        match command("a FETCH 1 BODY[TEXT]").await {
            Command::Fetch { spec, .. } => {
                assert!(matches!(
                    spec.items()[0].peek(),
                    FetchItem::BodyPeekSection(Section::Text, None)
                ));
                assert_eq!(spec.items()[0].peek().response_key(), "BODY[TEXT]");
            }
            other => panic!("expected FETCH, got {other:?}"),
        }
    }

    // -- STORE --------------------------------------------------------------

    #[tokio::test]
    async fn store_actions_parse() {
        let cases = [
            ("FLAGS", StoreAction::Replace, false),
            ("FLAGS.SILENT", StoreAction::Replace, true),
            ("+FLAGS", StoreAction::Add, false),
            ("+FLAGS.SILENT", StoreAction::Add, true),
            ("-FLAGS", StoreAction::Remove, false),
            ("-FLAGS.SILENT", StoreAction::Remove, true),
        ];
        for (text, action, silent) in cases {
            match command(&format!("a STORE 1 {text} (\\Seen)")).await {
                Command::Store {
                    action: parsed_action,
                    silent: parsed_silent,
                    flags,
                    ..
                } => {
                    assert_eq!(parsed_action, action, "{text}");
                    assert_eq!(parsed_silent, silent, "{text}");
                    assert_eq!(flags, vec!["\\Seen".to_string()], "{text}");
                }
                other => panic!("expected STORE, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn store_actions_are_case_insensitive() {
        match command("a STORE 1 +flags.silent (\\Deleted)").await {
            Command::Store { action, silent, .. } => {
                assert_eq!(action, StoreAction::Add);
                assert!(silent);
            }
            other => panic!("expected STORE, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn store_flag_lists_accept_keywords_and_reject_garbage() {
        match command("a STORE 1 FLAGS (\\Seen $Label1 \\Flagged)").await {
            Command::Store { flags, .. } => {
                assert_eq!(flags, vec!["\\Seen", "$Label1", "\\Flagged"]);
            }
            other => panic!("expected STORE, got {other:?}"),
        }
        assert!(matches!(
            command_err("a STORE 1 BOGUS (\\Seen)").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a STORE 1 FLAGS (\\Seen").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a STORE 1 FLAGS ()").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a STORE 1 FLAGS \\Seen extra)").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a STORE 1 FLAGS").await,
            FerromaError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn store_accepts_an_unparenthesised_flag_list() {
        match command("a STORE 1 +FLAGS \\Seen").await {
            Command::Store { flags, .. } => assert_eq!(flags, vec!["\\Seen"]),
            other => panic!("expected STORE, got {other:?}"),
        }
    }

    // -- SEARCH -------------------------------------------------------------

    #[tokio::test]
    async fn search_keys_parse() {
        let cases = [
            "ALL",
            "ANSWERED",
            "DELETED",
            "DRAFT",
            "FLAGGED",
            "NEW",
            "OLD",
            "RECENT",
            "SEEN",
            "UNANSWERED",
            "UNDELETED",
            "UNDRAFT",
            "UNFLAGGED",
            "UNSEEN",
        ];
        for key in cases {
            assert!(
                matches!(
                    command(&format!("a SEARCH {key}")).await,
                    Command::Search { .. }
                ),
                "`{key}` must parse"
            );
        }
    }

    #[tokio::test]
    async fn search_string_keys_parse() {
        for key in ["BCC", "BODY", "CC", "FROM", "SUBJECT", "TEXT", "TO"] {
            assert!(matches!(
                command(&format!("a SEARCH {key} needle")).await,
                Command::Search { .. }
            ));
            assert!(matches!(
                command(&format!("a SEARCH {key} \"two words\"")).await,
                Command::Search { .. }
            ));
        }
    }

    #[tokio::test]
    async fn search_date_keys_parse_imap_dates() {
        for key in ["BEFORE", "ON", "SINCE", "SENTBEFORE", "SENTON", "SENTSINCE"] {
            assert!(
                matches!(
                    command(&format!("a SEARCH {key} 09-Jul-2026")).await,
                    Command::Search { .. }
                ),
                "`{key}` must parse"
            );
            assert!(
                matches!(
                    command_err(&format!("a SEARCH {key} nonsense")).await,
                    FerromaError::Parse(_)
                ),
                "`{key}` must reject a bad date"
            );
        }
    }

    #[tokio::test]
    async fn search_numeric_and_uid_keys_parse() {
        assert!(matches!(
            command("a SEARCH LARGER 1000").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH SMALLER 10").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH UID 1:5").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH 1:5").await,
            Command::Search { .. }
        ));
        assert!(matches!(command("a SEARCH *").await, Command::Search { .. }));
    }

    #[tokio::test]
    async fn search_keyword_and_header_keys_parse() {
        assert!(matches!(
            command("a SEARCH KEYWORD $Junk").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH UNKEYWORD \\Seen").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH HEADER Message-ID <a@b>").await,
            Command::Search { .. }
        ));
    }

    #[tokio::test]
    async fn search_not_or_and_parentheses() {
        assert!(matches!(
            command("a SEARCH NOT SEEN").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH OR SEEN FLAGGED").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH (SEEN FLAGGED)").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH NOT (OR SEEN FLAGGED)").await,
            Command::Search { .. }
        ));
        assert!(matches!(
            command("a SEARCH OR NOT SEEN NOT FLAGGED").await,
            Command::Search { .. }
        ));
    }

    #[tokio::test]
    async fn search_charset_is_captured() {
        match command("a SEARCH CHARSET UTF-8 ALL").await {
            Command::Search { charset, .. } => assert_eq!(charset.as_deref(), Some("UTF-8")),
            other => panic!("expected SEARCH, got {other:?}"),
        }
        match command("a SEARCH charset us-ascii TEXT hi").await {
            Command::Search { charset, .. } => assert_eq!(charset.as_deref(), Some("US-ASCII")),
            other => panic!("expected SEARCH, got {other:?}"),
        }
        match command("a SEARCH ALL").await {
            Command::Search { charset, .. } => assert!(charset.is_none()),
            other => panic!("expected SEARCH, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_rejects_garbage() {
        for line in [
            "a SEARCH",
            "a SEARCH CHARSET",
            "a SEARCH BOGUSKEY",
            "a SEARCH (SEEN",
            "a SEARCH OR SEEN",
            "a SEARCH NOT",
            "a SEARCH HEADER",
        ] {
            assert!(
                matches!(command_err(line).await, FerromaError::Parse(_)),
                "`{line}` must be rejected"
            );
        }
    }

    // -- COPY / MOVE / UID --------------------------------------------------

    #[tokio::test]
    async fn copy_move_and_uid_commands_parse() {
        assert_eq!(
            command("a COPY 1:3 Sent").await,
            Command::Copy {
                set: SequenceSet::parse("1:3").expect("valid"),
                mailbox: "Sent".into()
            }
        );
        assert_eq!(
            command("a MOVE 1 Trash").await,
            Command::Move {
                set: SequenceSet::parse("1").expect("valid"),
                mailbox: "Trash".into()
            }
        );
        assert!(command("a UID FETCH 1 (UID)").await.is_uid());
        assert!(command("a UID STORE 1 +FLAGS (\\Seen)").await.is_uid());
        assert!(command("a UID SEARCH ALL").await.is_uid());
        assert!(command("a UID COPY 1 Sent").await.is_uid());
        assert!(command("a UID MOVE 1 Trash").await.is_uid());
        assert!(command("a UID EXPUNGE").await.is_uid());
        assert!(command("a UID EXPUNGE 1:3").await.is_uid());
    }

    #[tokio::test]
    async fn uid_command_names_are_prefixed() {
        assert_eq!(command("a UID FETCH 1 (UID)").await.name(), "UID FETCH");
        assert_eq!(command("a UID SEARCH ALL").await.name(), "UID SEARCH");
        assert_eq!(
            command("a UID STORE 1 +FLAGS (\\Seen)").await.name(),
            "UID STORE"
        );
        assert_eq!(command("a UID COPY 1 Sent").await.name(), "UID COPY");
        assert_eq!(command("a UID MOVE 1 Trash").await.name(), "UID MOVE");
        assert_eq!(command("a UID EXPUNGE").await.name(), "UID EXPUNGE");
    }

    #[tokio::test]
    async fn uid_rejects_unsupported_inner_commands() {
        assert!(command_err("a UID NOOP")
            .await
            .to_string()
            .contains("not a supported command"));
        assert!(matches!(
            command_err("a UID").await,
            FerromaError::Parse(_)
        ));
    }

    #[tokio::test]
    async fn inner_unwraps_uid_commands() {
        let parsed = command("a UID FETCH 1 (UID)").await;
        assert!(matches!(parsed.inner(), Command::Fetch { .. }));
        assert!(!parsed.inner().is_uid());
    }

    // -- literals -----------------------------------------------------------

    #[tokio::test]
    async fn literal_at_the_start_of_the_arguments() {
        let parsed = parse_with("a SELECT {5}", vec![b"INBOX".to_vec()])
            .await
            .expect("literal select must parse");
        assert_eq!(parsed.command, Command::Select("INBOX".into()));
    }

    #[tokio::test]
    async fn literal_in_the_middle_of_the_arguments() {
        let parsed = parse_with("a RENAME {3} New", vec![b"Old".to_vec()])
            .await
            .expect("literal rename must parse");
        assert_eq!(
            parsed.command,
            Command::Rename {
                from: "Old".into(),
                to: "New".into()
            }
        );
    }

    #[tokio::test]
    async fn two_literals_in_one_command() {
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(vec![b"Old".to_vec(), b"New".to_vec()]);
        let parsed = parser
            .parse("a RENAME {3} {3}", &mut source)
            .await
            .expect("two literals must parse");
        assert_eq!(
            parsed.command,
            Command::Rename {
                from: "Old".into(),
                to: "New".into()
            }
        );
        assert_eq!(source.requested, vec![(3, true), (3, true)]);
    }

    #[tokio::test]
    async fn a_literal_may_contain_the_text_of_the_command_itself() {
        // The literal's octets are never re-scanned: `a SELECT INBOX` inside a
        // literal stays inside the literal.
        let payload = b"a SELECT INBOX\r\n".to_vec();
        let line = format!("a CREATE {{{}}}", payload.len());
        let parsed = parse_with(&line, vec![payload.clone()])
            .await
            .expect("must parse");
        assert_eq!(
            parsed.command,
            Command::Create(String::from_utf8_lossy(&payload).into_owned())
        );
    }

    #[tokio::test]
    async fn a_zero_length_literal_is_accepted() {
        let parsed = parse_with("a CREATE {0}", vec![Vec::new()])
            .await
            .expect("zero literal must parse");
        assert_eq!(parsed.command, Command::Create(String::new()));
    }

    #[tokio::test]
    async fn literal_plus_is_non_synchronising() {
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(vec![b"INBOX".to_vec()]);
        let parsed = parser
            .parse("a SELECT {5+}", &mut source)
            .await
            .expect("LITERAL+ must parse");
        assert_eq!(parsed.command, Command::Select("INBOX".into()));
        assert_eq!(source.requested, vec![(5, false)]);
    }

    #[tokio::test]
    async fn an_oversized_literal_is_rejected_before_it_is_read() {
        let mut parser = CommandParser::with_max_literal(16);
        let mut source = FakeLiterals::new(vec![vec![b'x'; 1024]]);
        let err = parser
            .parse("a SELECT {1024}", &mut source)
            .await
            .expect_err("oversized literal must be refused");
        assert!(err.to_string().contains("exceeds the maximum"));
        assert!(source.requested.is_empty());
    }

    /// A source that always returns fewer bytes than declared.
    struct ShortLiterals;

    impl LiteralSource for ShortLiterals {
        async fn read_literal(
            &mut self,
            _declared: u64,
            _synchronizing: bool,
        ) -> Result<Vec<u8>, LiteralError> {
            Ok(b"abc".to_vec())
        }
    }

    #[tokio::test]
    async fn a_literal_declaring_more_bytes_than_it_sends_is_an_error() {
        let mut parser = CommandParser::new();
        // The source returns 3 bytes for a declared length of 10 — a truncated
        // read must be a BAD, not a hang or a panic.
        let err = parser
            .parse("a SELECT {10}", &mut ShortLiterals)
            .await
            .expect_err("short literal must fail");
        assert!(err.to_string().contains("declared 10"));
    }

    /// A source that refuses every literal.
    struct Declining;

    impl LiteralSource for Declining {
        async fn read_literal(
            &mut self,
            _declared: u64,
            _synchronizing: bool,
        ) -> Result<Vec<u8>, LiteralError> {
            Err(LiteralError::Declined)
        }
    }

    #[tokio::test]
    async fn a_client_that_declines_the_literal_is_an_error() {
        let mut parser = CommandParser::new();
        let err = parser
            .parse("a SELECT {5}", &mut Declining)
            .await
            .expect_err("declined literal must fail");
        assert!(err.to_string().contains("declined"));
    }

    #[tokio::test]
    async fn malformed_literal_prefixes_are_rejected() {
        for line in [
            "a SELECT {",
            "a SELECT {}",
            "a SELECT {5",
            "a SELECT {+}",
            "a SELECT {-5}",
            "a SELECT {{5}}",
            "a SELECT {99999999999999999999999999}",
        ] {
            assert!(
                matches!(command_err(line).await, FerromaError::Parse(_)),
                "`{line}` must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn a_literal_may_be_followed_by_more_of_the_command_line() {
        // RFC 3501 §4.3: the octets replace the `{n}`, and the rest of the
        // line continues after them — `RENAME {3} Old New` is legal.
        let parsed = parse_with("a RENAME {3} New", vec![b"Old".to_vec()])
            .await
            .expect("a mid-line literal must parse");
        assert_eq!(
            parsed.command,
            Command::Rename {
                from: "Old".into(),
                to: "New".into()
            }
        );
    }


    #[tokio::test]
    async fn append_with_a_literal_carries_the_message() {
        let message = b"Subject: hi\r\n\r\nbody\r\n".to_vec();
        let line = format!("a APPEND INBOX {{{}}}", message.len());
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(vec![message.clone()]);
        let parsed = parser.parse(&line, &mut source).await.expect("must parse");
        match parsed.command {
            Command::Append {
                mailbox,
                flags,
                internal_date,
                message: parsed_message,
            } => {
                assert_eq!(mailbox, "INBOX");
                assert!(flags.is_empty());
                assert!(internal_date.is_none());
                assert_eq!(parsed_message, message);
            }
            other => panic!("expected APPEND, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_with_flags_and_internal_date() {
        let message = b"x".to_vec();
        let line = format!(
            "a APPEND \"My Box\" (\\Seen \\Draft) \"09-Jul-2026 12:34:56 +0000\" {{{}}}",
            message.len()
        );
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(vec![message.clone()]);
        let parsed = parser.parse(&line, &mut source).await.expect("must parse");
        match parsed.command {
            Command::Append {
                mailbox,
                flags,
                internal_date,
                message: parsed_message,
            } => {
                assert_eq!(mailbox, "My Box");
                assert_eq!(flags, vec!["\\Seen", "\\Draft"]);
                let when = internal_date.expect("date must parse");
                assert_eq!(when.to_string(), "2026-07-09 12:34:56 UTC");
                assert_eq!(parsed_message, message);
            }
            other => panic!("expected APPEND, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_with_only_an_internal_date() {
        let line = "a APPEND INBOX \"09-Jul-2026 12:34:56 +0000\" {1}";
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(vec![b"x".to_vec()]);
        let parsed = parser.parse(line, &mut source).await.expect("must parse");
        match parsed.command {
            Command::Append {
                flags,
                internal_date,
                ..
            } => {
                assert!(flags.is_empty());
                assert!(internal_date.is_some());
            }
            other => panic!("expected APPEND, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_with_a_negative_timezone_offset_is_converted_to_utc() {
        let line = "a APPEND INBOX \"09-Jul-2026 12:00:00 -0500\" {1}";
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(vec![b"x".to_vec()]);
        let parsed = parser.parse(line, &mut source).await.expect("must parse");
        match parsed.command {
            Command::Append { internal_date, .. } => {
                let when = internal_date.expect("date must parse");
                assert_eq!(when.to_string(), "2026-07-09 17:00:00 UTC");
            }
            other => panic!("expected APPEND, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_rejects_a_malformed_internal_date() {
        for line in [
            "a APPEND INBOX \"nonsense\" {1}",
            "a APPEND INBOX \"09-Jul-2026\" {1}",
            "a APPEND INBOX \"09-Jul-2026 12:34:56\" {1}",
            "a APPEND INBOX \"09-Jul-2026 12:34:56 +99\" {1}",
        ] {
            assert!(
                matches!(command_err(line).await, FerromaError::Parse(_)),
                "`{line}` must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn append_without_a_literal_is_rejected() {
        assert!(matches!(
            command_err("a APPEND INBOX").await,
            FerromaError::Parse(_)
        ));
        assert!(matches!(
            command_err("a APPEND INBOX ()").await,
            FerromaError::Parse(_)
        ));
    }

    #[test]
    fn timezone_offsets_parse() {
        assert_eq!(parse_zone_offset("+0000"), Some(0));
        assert_eq!(parse_zone_offset("+0100"), Some(3600));
        assert_eq!(parse_zone_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_zone_offset("0000"), None);
        assert_eq!(parse_zone_offset("+000"), None);
        assert_eq!(parse_zone_offset("+00a0"), None);
    }

    // -- IDLE ---------------------------------------------------------------

    #[tokio::test]
    async fn idle_detection_and_mode() {
        let mut parser = CommandParser::new();
        assert!(!parser.is_idling());
        parser.begin_idle();
        assert!(parser.is_idling());
        assert!(CommandParser::is_done("DONE"));
        assert!(CommandParser::is_done("done"));
        assert!(CommandParser::is_done("  DONE "));
        assert!(!CommandParser::is_done("DONE."));
        assert!(!CommandParser::is_done("NOOP"));

        let mut source = FakeLiterals::empty();
        let err = parser
            .parse("a NOOP", &mut source)
            .await
            .expect_err("a command while idling must fail");
        assert!(err.to_string().contains("DONE"));

        parser.end_idle();
        assert!(!parser.is_idling());
        assert!(parser.parse("a NOOP", &mut source).await.is_ok());
    }

    // -- parser state -------------------------------------------------------

    #[tokio::test]
    async fn the_parser_can_be_reused_for_many_commands() {
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::empty();
        for line in ["a1 CAPABILITY", "a2 SELECT INBOX", "a3 FETCH 1 (UID)"] {
            let parsed = parser.parse(line, &mut source).await.expect("must parse");
            assert_eq!(parsed.tag, line.split(' ').next().unwrap_or_default());
        }
    }

    #[tokio::test]
    async fn a_failed_parse_does_not_poison_the_next_one() {
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::empty();
        assert!(parser.parse("a BOGUS", &mut source).await.is_err());
        assert!(parser.parse("a NOOP", &mut source).await.is_ok());
    }

    #[test]
    fn parse_outcome_enum_is_public() {
        assert_ne!(ParseOutcome::Command, ParseOutcome::IdleDone);
    }

    #[tokio::test]
    async fn carries_message_body_is_reported() {
        let message = b"x".to_vec();
        let line = format!("a APPEND INBOX {{{}}}", message.len());
        let mut parser = CommandParser::new();
        let mut source = FakeLiterals::new(vec![message]);
        let parsed = parser.parse(&line, &mut source).await.expect("must parse");
        assert!(parsed.command.carries_message_body());
        assert!(!Command::Noop.carries_message_body());
        assert!(Command::Login {
            user: "a".into(),
            password: "b".into()
        }
        .carries_message_body());
    }

    #[test]
    fn section_and_fetch_item_display_are_canonical() {
        assert_eq!(Section::All.to_string(), "");
        assert_eq!(Section::Header.to_string(), "HEADER");
        assert_eq!(Section::Text.to_string(), "TEXT");
        assert_eq!(Section::Part(vec![1, 2]).to_string(), "1.2");
        assert_eq!(Section::Mime(vec![1]).to_string(), "1.MIME");
        assert_eq!(Section::PartHeader(vec![2]).to_string(), "2.HEADER");
        assert_eq!(Section::PartText(vec![2]).to_string(), "2.TEXT");
        assert_eq!(
            Section::HeaderFields(vec!["SUBJECT".into(), "FROM".into()]).to_string(),
            "HEADER.FIELDS (SUBJECT FROM)"
        );
        assert_eq!(
            Section::HeaderFieldsNot(vec!["RECEIVED".into()]).to_string(),
            "HEADER.FIELDS.NOT (RECEIVED)"
        );
        assert_eq!(
            Section::PartHeaderFields(vec![1], vec!["A".into()]).to_string(),
            "1.HEADER.FIELDS (A)"
        );
        assert_eq!(
            Section::PartHeaderFieldsNot(vec![1], vec!["A".into()]).to_string(),
            "1.HEADER.FIELDS.NOT (A)"
        );
        assert_eq!(Section::Part(vec![3]).part_path(), Some(&[3u32][..]));
        assert_eq!(Section::All.part_path(), None);
    }

    #[test]
    fn store_action_as_str_matches_the_grammar() {
        assert_eq!(StoreAction::Replace.as_str(), "FLAGS");
        assert_eq!(StoreAction::Add.as_str(), "+FLAGS");
        assert_eq!(StoreAction::Remove.as_str(), "-FLAGS");
    }

    #[test]
    fn is_literal_valued_covers_the_body_items() {
        assert!(FetchItem::Rfc822.is_literal_valued());
        assert!(FetchItem::Rfc822Text.is_literal_valued());
        assert!(FetchItem::Rfc822Header.is_literal_valued());
        assert!(FetchItem::BodySection(Section::All, None).is_literal_valued());
        assert!(!FetchItem::Envelope.is_literal_valued());
        assert!(!FetchItem::Flags.is_literal_valued());
    }

    #[test]
    fn decode_name_replaces_invalid_utf8() {
        assert_eq!(decode_name(b"INBOX"), "INBOX");
        assert_eq!(decode_name(&[0xff, 0xfe]), "\u{fffd}\u{fffd}");
    }

    #[test]
    fn base64_decode_accepts_whitespace_and_rejects_garbage() {
        assert_eq!(base64_decode(b"YWJj").expect("valid"), b"abc".to_vec());
        assert_eq!(base64_decode(b"YW Jj").expect("valid"), b"abc".to_vec());
        assert!(base64_decode(b"!!!!").is_err());
    }

    #[tokio::test]
    async fn a_hostile_command_survey() {
        // Nothing here may panic; each line either parses or is a Parse error.
        let hostile = [
            "",
            " ",
            "a",
            "a ",
            "a  NOOP",
            "a\tNOOP",
            "a NOOP\r\nNOOP",
            "a LOGIN \"\" \"\"",
            "a SELECT \"\\",
            "a FETCH 1 BODY[[[]]]",
            "a FETCH 1 BODY[1.1.1.1.1.1.1.1.1.1.1.1.1.1.1.1.1.1]",
            "a STORE 1 +FLAGS (\"unclosed)",
            "a APPEND INBOX () ",
            "a APPEND INBOX {0+}",
            "a SEARCH ()",
            "a SEARCH ((((((",
            "a UID",
            "a UID UID",
            "a LIST",
            "a LIST \"\"",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa NOOP",
        ];
        for line in hostile {
            // The only requirement is that nothing panics.
            let mut parser = CommandParser::new();
            let mut source = FakeLiterals::new(vec![Vec::new()]);
            let _ = parser.parse(line, &mut source).await;
        }
    }
}

