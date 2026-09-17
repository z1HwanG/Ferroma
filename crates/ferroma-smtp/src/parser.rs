//! The SMTP command parser.
//!
//! One line of input from a peer becomes exactly one [`Command`], or one
//! [`SmtpError`] carrying the reply we must send back. The parser is deliberately
//! hostile-input-first: peers control this byte stream completely, so every branch
//! here either produces a value or a protocol error — never a panic, never an
//! allocation proportional to something the peer did not actually send.
//!
//! # What is accepted
//!
//! * Verbs are case-insensitive (`mail from:` and `MAIL FROM:` are the same).
//! * `MAIL FROM:`/`RCPT TO:` tolerate arbitrary whitespace after the colon and
//!   around the path, and accept the path in angle brackets or bare.
//! * `<>` is the null reverse-path. It is legal, common (bounces are sent with it)
//!   and is preserved as [`MailParams::from`]` == None` so the rest of the platform
//!   can tell "no sender" from "a sender we failed to parse".
//! * ESMTP parameters after the path: `SIZE=`, `BODY=` and `AUTH=` are understood;
//!   unknown `KEY=VALUE` parameters are tolerated and ignored, which is what
//!   RFC 5321 §4.1.1.11 asks of a server that does not implement them.
//! * `AUTH PLAIN` and `AUTH LOGIN` in both the single-step (`AUTH PLAIN <base64>`)
//!   and challenge/response (`AUTH LOGIN`) forms.
//!
//! # What is refused
//!
//! * Lines longer than [`MAX_COMMAND_LINE`] (512 octets of command, 1000 with the
//!   trailing CRLF, per RFC 5321 §4.5.3.1.4) — `500 5.5.6 Line too long`.
//! * Source routes (`<@relay.example:user@example.com>`). Accepting one is how a
//!   server becomes a relay; there is no legitimate use for them in 2026.
//! * `BDAT` (CHUNKING). It is recognised so the refusal is a precise `502`, not a
//!   generic "unknown command".
//! * Any malformed address: parsing delegates to [`ferroma_core::EmailAddress`].

use ferroma_core::EmailAddress;

use crate::reply::Reply;

/// The longest accepted command line *including* the terminating CRLF.
///
/// RFC 5321 §4.5.3.1.4 caps the command itself at 512 octets; the extra room is
/// for the CRLF and for the `MAIL FROM:<...> SIZE=...` form that every real client
/// sends without counting properly.
pub const MAX_COMMAND_LINE: usize = 1000;

/// The longest accepted verb. No SMTP verb is anywhere near this.
const MAX_VERB_LEN: usize = 16;

/// The longest accepted `EHLO`/`HELO` argument (a domain name or address literal).
const MAX_HELO_LEN: usize = 255;

/// A parsed SMTP command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `EHLO <domain>` — extended SMTP greeting.
    Ehlo(String),
    /// `HELO <domain>` — the original, extension-free greeting.
    Helo(String),
    /// `MAIL FROM:<path> [params…]`.
    MailFrom(MailParams),
    /// `RCPT TO:<path> [params…]`.
    RcptTo(RcptParams),
    /// `DATA` — the message follows.
    Data,
    /// `RSET` — abort the current transaction.
    Rset,
    /// `NOOP [string]`.
    Noop(String),
    /// `QUIT`.
    Quit,
    /// `VRFY <string>`.
    Vrfy(String),
    /// `HELP [command]`.
    Help(String),
    /// `AUTH <mechanism> [initial-response]`.
    Auth(AuthParams),
    /// `STARTTLS` — upgrade the connection (RFC 3207).
    StartTls,
    /// `BDAT <n> [LAST]` — recognised so it can be refused precisely.
    Bdat(u64, bool),
}

impl Command {
    /// The verb as the peer wrote it, upper-cased. Handy for structured logging.
    pub fn verb(&self) -> &'static str {
        match self {
            Command::Ehlo(_) => "EHLO",
            Command::Helo(_) => "HELO",
            Command::MailFrom(_) => "MAIL",
            Command::RcptTo(_) => "RCPT",
            Command::Data => "DATA",
            Command::Rset => "RSET",
            Command::Noop(_) => "NOOP",
            Command::Quit => "QUIT",
            Command::Vrfy(_) => "VRFY",
            Command::Help(_) => "HELP",
            Command::Auth(_) => "AUTH",
            Command::StartTls => "STARTTLS",
            Command::Bdat(_, _) => "BDAT",
        }
    }

    /// Whether this command begins a new transaction or continues one.
    ///
    /// Used by the session loop to decide when the [`crate::session::SmtpSession`]
    /// is allowed to reset its envelope.
    pub fn starts_transaction(&self) -> bool {
        matches!(self, Command::MailFrom(_))
    }
}

/// `MAIL FROM` arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailParams {
    /// The reverse-path. `None` is the null sender (`<>`), which is legal.
    pub from: Option<EmailAddress>,
    /// `SIZE=<n>`: the sender's estimate of the message size.
    pub size: Option<u64>,
    /// `BODY=7BIT` / `BODY=8BITMIME`.
    pub body: Option<BodyType>,
    /// `AUTH=<xtext>`: the authenticated identity the sender claims.
    pub auth: Option<String>,
    /// Parameters we did not recognise, kept verbatim for logging.
    pub unknown: Vec<String>,
}

/// The `BODY=` parameter of `MAIL FROM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyType {
    /// `BODY=7BIT` — the message is pure ASCII.
    SevenBit,
    /// `BODY=8BITMIME` — the message carries 8-bit octets (RFC 6152).
    EightBitMime,
}

impl BodyType {
    /// The wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            BodyType::SevenBit => "7BIT",
            BodyType::EightBitMime => "8BITMIME",
        }
    }
}

/// `RCPT TO` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcptParams {
    /// The forward-path. Never `None`: `RCPT TO:<>` is invalid.
    pub to: EmailAddress,
    /// Parameters we did not recognise (`NOTIFY=`, `ORCPT=`, …).
    pub unknown: Vec<String>,
}

/// `AUTH` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthParams {
    /// The SASL mechanism, upper-cased: `PLAIN`, `LOGIN`, …
    pub mechanism: String,
    /// The optional initial response, kept **base64-encoded**.
    ///
    /// It is never decoded here: the raw value is what the auth layer hands to the
    /// SASL implementation, and keeping a password out of an intermediate `String`
    /// that might be logged is the whole point.
    pub initial_response: Option<String>,
}

impl AuthParams {
    /// Whether the client sent an initial response (even if it is the empty `=`).
    pub fn has_initial_response(&self) -> bool {
        self.initial_response.is_some()
    }
}

/// Why a command line could not be turned into a [`Command`].
///
/// Every variant carries the exact reply the server must send, so the session loop
/// never has to invent wording — see [`crate::reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpError {
    /// The reply to send.
    pub reply: Reply,
    /// A machine-readable tag for logs (`syntax`, `line_too_long`, …).
    pub kind: &'static str,
}

impl SmtpError {
    /// Build an error carrying `reply`.
    pub fn new(kind: &'static str, reply: Reply) -> Self {
        SmtpError { kind, reply }
    }

    /// The line exceeded [`MAX_COMMAND_LINE`].
    pub fn line_too_long() -> Self {
        SmtpError::new("line_too_long", Reply::line_too_long())
    }

    /// The command had no verb.
    pub fn empty() -> Self {
        SmtpError::new("empty_command", Reply::syntax_error("empty command"))
    }

    /// Generic syntax error with an operator-readable reason.
    pub fn syntax(reason: &str) -> Self {
        SmtpError::new("syntax", Reply::syntax_error(reason))
    }

    /// A command that needs TLS first.
    pub fn tls_required(reason: &str) -> Self {
        SmtpError::new("tls_required", Reply::tls_required(reason))
    }
}

impl std::fmt::Display for SmtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.reply.text())
    }
}

impl std::error::Error for SmtpError {}

/// Parse one command line.
///
/// `line` may or may not carry its terminating CRLF; both are accepted (and a bare
/// LF, as sent by some broken clients, is tolerated the way real servers do).
pub fn parse_command(line: &[u8]) -> Result<Command, SmtpError> {
    if line.len() > MAX_COMMAND_LINE {
        return Err(SmtpError::line_too_long());
    }

    let trimmed = strip_line_ending(line);

    // A bare CR in the middle is either an injection attempt or a broken client.
    // Either way the command is not what the peer thinks it is: refuse it loudly
    // rather than splitting one line into two commands.
    if trimmed.contains(&b'\r') || trimmed.contains(&b'\n') {
        return Err(SmtpError::syntax("bare CR or LF in command"));
    }
    if trimmed.contains(&0) {
        return Err(SmtpError::syntax("NUL in command"));
    }

    let text = std::str::from_utf8(trimmed).map_err(|_| SmtpError::syntax("command is not UTF-8"))?;
    let text = text.trim_matches(|c: char| c == ' ' || c == '\t');
    if text.is_empty() {
        return Err(SmtpError::empty());
    }

    let (verb, rest) = split_verb(text);
    if verb.len() > MAX_VERB_LEN {
        return Err(SmtpError::syntax("verb too long"));
    }

    match verb.to_ascii_uppercase().as_str() {
        "EHLO" => Ok(Command::Ehlo(required_arg(rest, "EHLO")?)),
        "HELO" => Ok(Command::Helo(required_arg(rest, "HELO")?)),
        "MAIL" => parse_mail(rest),
        "RCPT" => parse_rcpt(rest),
        "DATA" => {
            no_arg(rest, "DATA")?;
            Ok(Command::Data)
        }
        "RSET" => {
            no_arg(rest, "RSET")?;
            Ok(Command::Rset)
        }
        "NOOP" => Ok(Command::Noop(rest.trim().to_string())),
        "QUIT" => {
            no_arg(rest, "QUIT")?;
            Ok(Command::Quit)
        }
        "VRFY" => Ok(Command::Vrfy(required_arg(rest, "VRFY")?)),
        "HELP" => Ok(Command::Help(rest.trim().to_string())),
        "AUTH" => parse_auth(rest),
        "STARTTLS" => {
            no_arg(rest, "STARTTLS")?;
            Ok(Command::StartTls)
        }
        "BDAT" => parse_bdat(rest),
        _ => Err(SmtpError::new(
            "unknown_command",
            Reply::unknown_command(verb),
        )),
    }
}

/// Note the position of the first space/tab: everything before is the verb.
fn split_verb(text: &str) -> (&str, &str) {
    match text.find([' ', '\t']) {
        Some(idx) => (&text[..idx], &text[idx + 1..]),
        None => (text, ""),
    }
}

/// Strip a trailing CRLF, LF or CR.
fn strip_line_ending(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

fn no_arg(rest: &str, verb: &str) -> Result<(), SmtpError> {
    if rest.trim().is_empty() {
        Ok(())
    } else {
        Err(SmtpError::syntax(&format!("{verb} takes no arguments")))
    }
}

fn required_arg(rest: &str, verb: &str) -> Result<String, SmtpError> {
    let arg = rest.trim();
    if arg.is_empty() {
        return Err(SmtpError::syntax(&format!("{verb} requires an argument")));
    }
    if arg.len() > MAX_HELO_LEN {
        return Err(SmtpError::syntax(&format!("{verb} argument is too long")));
    }
    // The greeting travels into `Received:` headers and into the log line.
    if arg.chars().any(|c| c.is_control() || c == '\x00') {
        return Err(SmtpError::syntax(&format!("{verb} argument has control characters")));
    }
    Ok(arg.to_string())
}

/// `MAIL FROM:<path>` — accepts `FROM:` in any case, with or without whitespace.
fn parse_mail(rest: &str) -> Result<Command, SmtpError> {
    let after_from = strip_keyword(rest, "FROM")
        .ok_or_else(|| SmtpError::syntax("MAIL requires FROM:<address>"))?;
    let (path, tail) = split_path(after_from)?;
    let from = parse_reverse_path(path)?;
    let (size, body, auth, unknown) = parse_esmtp_params(tail)?;
    Ok(Command::MailFrom(MailParams {
        from,
        size,
        body,
        auth,
        unknown,
    }))
}

/// `RCPT TO:<path>`.
fn parse_rcpt(rest: &str) -> Result<Command, SmtpError> {
    let after_to =
        strip_keyword(rest, "TO").ok_or_else(|| SmtpError::syntax("RCPT requires TO:<address>"))?;
    let (path, tail) = split_path(after_to)?;
    if path.trim().is_empty() {
        return Err(SmtpError::syntax("RCPT TO requires an address"));
    }
    reject_source_route(path)?;
    let address = EmailAddress::parse(path)
        .map_err(|e| SmtpError::new("bad_recipient", Reply::bad_recipient_address(&e.to_string())))?;
    let (_, _, _, unknown) = parse_esmtp_params(tail)?;
    Ok(Command::RcptTo(RcptParams { to: address, unknown }))
}

/// The reverse path may be empty: `<>` is the null sender.
fn parse_reverse_path(path: &str) -> Result<Option<EmailAddress>, SmtpError> {
    let raw = path.trim();
    if raw.is_empty() {
        // `MAIL FROM:` with nothing at all. RFC 5321 wants `<>`, but every real
        // server accepts the bare form as the null sender.
        return Ok(None);
    }
    if raw == "<>" {
        return Ok(None);
    }
    reject_source_route(raw)?;
    let parsed = EmailAddress::parse(raw)
        .map_err(|e| SmtpError::new("bad_sender", Reply::bad_sender_address(&e.to_string())))?;
    Ok(Some(parsed))
}

/// Refuse `<@relay.example,@other.example:user@example.com>`.
///
/// A source route asks the receiving server to relay through a named path. RFC 5321
/// §4.1.2 still describes the syntax, and §C.1 records that no modern server honours
/// it. Honouring one is an open-relay primitive, so it is refused with a stable
/// error code rather than being silently dropped.
fn reject_source_route(path: &str) -> Result<(), SmtpError> {
    let inner = path.trim().trim_start_matches('<').trim_end_matches('>');
    if inner.starts_with('@') {
        return Err(SmtpError::new(
            "source_route",
            Reply::relay_denied("source routes are not supported"),
        ));
    }
    if let Some(colon) = inner.find(':') {
        // `@a,@b:user@host` — the colon only matters when an `@`-list precedes it.
        if inner[..colon].contains('@') {
            return Err(SmtpError::new(
                "source_route",
                Reply::relay_denied("source routes are not supported"),
            ));
        }
    }
    Ok(())
}

/// Everything after the verb, with a case-insensitive `KEYWORD` and an optional
/// `:` (possibly surrounded by spaces) removed.
fn strip_keyword<'a>(rest: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = rest.trim_start();
    if trimmed.len() < keyword.len() {
        return None;
    }
    let (head, tail) = trimmed.split_at(keyword.len());
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let tail = tail.trim_start();
    match tail.strip_prefix(':') {
        Some(after) => Some(after),
        // `MAIL FROM <addr>` and `MAIL FROM<addr>` are not RFC 5321, but tolerating
        // them costs nothing and some ancient clients emit them.
        None => Some(tail),
    }
}

/// Split `<addr> params` into the path and the parameter tail.
///
/// Handles the bare form (no angle brackets) by taking the first whitespace-delimited
/// token as the path.
fn split_path(input: &str) -> Result<(&str, &str), SmtpError> {
    let trimmed = input.trim_start();
    if let Some(rest) = trimmed.strip_prefix('<') {
        let close = rest
            .find('>')
            .ok_or_else(|| SmtpError::syntax("unterminated `<` in path"))?;
        let (path, tail) = rest.split_at(close);
        // Skip the closing `>`.
        return Ok((path, tail.get(1..).unwrap_or("")));
    }
    match trimmed.find([' ', '\t']) {
        Some(idx) => Ok((&trimmed[..idx], &trimmed[idx..])),
        None => Ok((trimmed, "")),
    }
}

/// The ESMTP parameters we understand: `SIZE=`, `BODY=`, `AUTH=`, plus whatever we
/// did not recognise, in the order they arrived.
type EsmtpParams = (Option<u64>, Option<BodyType>, Option<String>, Vec<String>);

/// Parse the `SIZE=`, `BODY=` and `AUTH=` parameters out of an ESMTP tail.
fn parse_esmtp_params(tail: &str) -> Result<EsmtpParams, SmtpError> {
    let mut size = None;
    let mut body = None;
    let mut auth = None;
    let mut unknown = Vec::new();

    for token in tail.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            return Err(SmtpError::syntax("malformed ESMTP parameter"));
        };
        match key.to_ascii_uppercase().as_str() {
            "SIZE" => {
                let parsed: u64 = value
                    .parse()
                    .map_err(|_| SmtpError::syntax("SIZE must be a non-negative integer"))?;
                size = Some(parsed);
            }
            "BODY" => {
                let parsed = match value.to_ascii_uppercase().as_str() {
                    "7BIT" => BodyType::SevenBit,
                    "8BITMIME" => BodyType::EightBitMime,
                    other => {
                        return Err(SmtpError::new(
                            "bad_body_type",
                            Reply::param_not_implemented(&format!("BODY={other}")),
                        ))
                    }
                };
                body = Some(parsed);
            }
            "AUTH" => {
                if value.is_empty() {
                    return Err(SmtpError::syntax("AUTH requires a value"));
                }
                auth = Some(value.to_string());
            }
            // Unknown parameters are tolerated and recorded (RFC 5321 §4.1.1.11).
            _ => unknown.push(token.to_string()),
        }
    }

    Ok((size, body, auth, unknown))
}

/// `AUTH <mechanism> [initial-response]`.
fn parse_auth(rest: &str) -> Result<Command, SmtpError> {
    let mut parts = rest.split_whitespace();
    let Some(mechanism) = parts.next() else {
        return Err(SmtpError::syntax("AUTH requires a mechanism"));
    };
    if mechanism.len() > MAX_VERB_LEN {
        return Err(SmtpError::syntax("SASL mechanism name is too long"));
    }
    let mechanism = mechanism.to_ascii_uppercase();

    let initial = parts.next();
    if parts.next().is_some() {
        return Err(SmtpError::syntax("AUTH takes at most two arguments"));
    }

    if let Some(response) = initial {
        // Base64 alphabet plus the padding character; `=` alone means "empty".
        if !response
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
        {
            return Err(SmtpError::syntax("AUTH initial response must be base64"));
        }
        if response.len() > 8192 {
            return Err(SmtpError::syntax("AUTH initial response is too long"));
        }
    }

    Ok(Command::Auth(AuthParams {
        mechanism,
        initial_response: initial.map(str::to_string),
    }))
}

/// `BDAT <octets> [LAST]`.
fn parse_bdat(rest: &str) -> Result<Command, SmtpError> {
    let mut parts = rest.split_whitespace();
    let Some(count) = parts.next() else {
        return Err(SmtpError::syntax("BDAT requires a chunk size"));
    };
    let octets: u64 = count
        .parse()
        .map_err(|_| SmtpError::syntax("BDAT chunk size must be a non-negative integer"))?;
    let last = match parts.next() {
        None => false,
        Some(token) if token.eq_ignore_ascii_case("LAST") => true,
        Some(_) => return Err(SmtpError::syntax("BDAT's second argument must be LAST")),
    };
    Ok(Command::Bdat(octets, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command, SmtpError> {
        parse_command(line.as_bytes())
    }

    fn addr(raw: &str) -> EmailAddress {
        EmailAddress::parse(raw).expect("test address must parse")
    }

    // -----------------------------------------------------------------------
    // Greetings
    // -----------------------------------------------------------------------

    #[test]
    fn ehlo_parses_a_domain() {
        assert_eq!(parse("EHLO mail.example.com\r\n"), Ok(Command::Ehlo("mail.example.com".into())));
        assert_eq!(parse("ehlo mail.example.com"), Ok(Command::Ehlo("mail.example.com".into())));
        assert_eq!(parse("EhLo\tmail.example.com\r\n"), Ok(Command::Ehlo("mail.example.com".into())));
    }

    #[test]
    fn ehlo_accepts_an_address_literal() {
        assert_eq!(
            parse("EHLO [192.0.2.1]\r\n"),
            Ok(Command::Ehlo("[192.0.2.1]".into()))
        );
        assert_eq!(parse("EHLO [IPv6:::1]"), Ok(Command::Ehlo("[IPv6:::1]".into())));
    }

    #[test]
    fn ehlo_without_an_argument_is_a_syntax_error() {
        let err = parse("EHLO\r\n").unwrap_err();
        assert_eq!(err.reply, Reply::syntax_error("EHLO requires an argument"));
        assert_eq!(err.kind, "syntax");
    }

    #[test]
    fn helo_parses_like_ehlo_but_stays_distinct() {
        assert_eq!(parse("HELO example.com\r\n"), Ok(Command::Helo("example.com".into())));
        assert!(parse("HELO\r\n").is_err());
    }

    #[test]
    fn greeting_argument_with_control_characters_is_refused() {
        let err = parse("EHLO evil\u{7}host\r\n").unwrap_err();
        assert_eq!(err.kind, "syntax");
    }

    #[test]
    fn greeting_argument_length_is_capped() {
        let long = "a".repeat(MAX_HELO_LEN + 1);
        assert!(parse(&format!("EHLO {long}\r\n")).is_err());
        let ok = "a".repeat(MAX_HELO_LEN);
        assert!(parse(&format!("EHLO {ok}\r\n")).is_ok());
    }

    // -----------------------------------------------------------------------
    // MAIL FROM
    // -----------------------------------------------------------------------

    #[test]
    fn mail_from_accepts_the_canonical_form() {
        let cmd = parse("MAIL FROM:<alice@example.com>\r\n").unwrap();
        match cmd {
            Command::MailFrom(p) => {
                assert_eq!(p.from, Some(addr("alice@example.com")));
                assert!(p.size.is_none() && p.body.is_none() && p.auth.is_none());
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn mail_from_is_case_insensitive_and_whitespace_tolerant() {
        for line in [
            "MAIL FROM:<alice@example.com>",
            "mail from:<alice@example.com>",
            "MaIl FrOm:<alice@example.com>",
            "MAIL FROM: <alice@example.com>",
            "MAIL FROM:  <alice@example.com>  ",
            "MAIL FROM:<alice@example.com> ",
            "MAIL FROM :<alice@example.com>",
            "MAIL FROM\t:<alice@example.com>",
        ] {
            let parsed = parse(line).unwrap_or_else(|e| panic!("{line:?} failed: {e}"));
            match parsed {
                Command::MailFrom(p) => assert_eq!(p.from, Some(addr("alice@example.com")), "{line:?}"),
                other => panic!("{line:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn mail_from_accepts_a_bare_address_without_brackets() {
        match parse("MAIL FROM:alice@example.com\r\n").unwrap() {
            Command::MailFrom(p) => assert_eq!(p.from, Some(addr("alice@example.com"))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_null_sender_is_preserved_as_none() {
        for line in ["MAIL FROM:<>\r\n", "MAIL FROM:<>\n", "MAIL FROM:\r\n", "MAIL FROM:<>"] {
            match parse(line).unwrap_or_else(|e| panic!("{line:?}: {e}")) {
                Command::MailFrom(p) => assert!(p.from.is_none(), "{line:?} must be the null sender"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn the_null_sender_keeps_its_parameters() {
        match parse("MAIL FROM:<> SIZE=0\r\n").unwrap() {
            Command::MailFrom(p) => {
                assert!(p.from.is_none());
                assert_eq!(p.size, Some(0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mail_from_parses_size() {
        match parse("MAIL FROM:<a@example.com> SIZE=26214400\r\n").unwrap() {
            Command::MailFrom(p) => assert_eq!(p.size, Some(26_214_400)),
            other => panic!("{other:?}"),
        }
        assert!(parse("MAIL FROM:<a@example.com> SIZE=-1\r\n").is_err());
        assert!(parse("MAIL FROM:<a@example.com> SIZE=twelve\r\n").is_err());
        assert!(parse("MAIL FROM:<a@example.com> SIZE=99999999999999999999999\r\n").is_err());
    }

    #[test]
    fn mail_from_parses_body() {
        match parse("MAIL FROM:<a@example.com> BODY=8BITMIME\r\n").unwrap() {
            Command::MailFrom(p) => assert_eq!(p.body, Some(BodyType::EightBitMime)),
            other => panic!("{other:?}"),
        }
        match parse("MAIL FROM:<a@example.com> BODY=7BIT\r\n").unwrap() {
            Command::MailFrom(p) => assert_eq!(p.body, Some(BodyType::SevenBit)),
            other => panic!("{other:?}"),
        }
        // RFC 5321 §4.1.1.11: a value we do not implement gets its own reply code
        // (`504`), not a generic syntax error, so the sender knows *what* is wrong.
        let err = parse("MAIL FROM:<a@example.com> BODY=BINARYMIME\r\n").unwrap_err();
        assert_eq!(err.kind, "bad_body_type");
        assert_eq!(err.reply.code(), 504);
        assert!(err.reply.text().contains("BODY=BINARYMIME"));
    }

    #[test]
    fn mail_from_parses_auth_xtext() {
        match parse("MAIL FROM:<a@example.com> AUTH=alice@example.com\r\n").unwrap() {
            Command::MailFrom(p) => assert_eq!(p.auth.as_deref(), Some("alice@example.com")),
            other => panic!("{other:?}"),
        }
        assert!(parse("MAIL FROM:<a@example.com> AUTH=\r\n").is_err());
    }

    #[test]
    fn mail_from_keeps_unknown_parameters_for_logging() {
        match parse("MAIL FROM:<a@example.com> SIZE=10 FOO=bar\r\n").unwrap() {
            Command::MailFrom(p) => {
                assert_eq!(p.size, Some(10));
                assert_eq!(p.unknown, vec!["FOO=bar".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        // A parameter with no `=` at all is a syntax error, not an unknown parameter:
        // a sender that meant `SMTPUTF8` and forgot the value must be told.
        let err = parse("MAIL FROM:<a@example.com> SMTPUTF8\r\n").unwrap_err();
        assert_eq!(err.kind, "syntax");
    }

    #[test]
    fn mail_from_requires_the_from_keyword() {
        assert!(parse("MAIL <alice@example.com>\r\n").is_err());
        assert!(parse("MAIL\r\n").is_err());
        assert!(parse("MAIL TO:<alice@example.com>\r\n").is_err());
    }

    #[test]
    fn mail_from_with_an_unterminated_bracket_is_refused() {
        let err = parse("MAIL FROM:<alice@example.com\r\n").unwrap_err();
        assert_eq!(err.kind, "syntax");
        assert_eq!(err.reply.code(), 501);
    }

    #[test]
    fn mail_from_with_a_malformed_address_is_refused() {
        for line in [
            "MAIL FROM:<alice>\r\n",
            "MAIL FROM:<alice@>\r\n",
            "MAIL FROM:<alice @example.com>\r\n",
            "MAIL FROM:<alice@exa mple.com>\r\n",
            "MAIL FROM:<alice@[192.0.2.1]>\r\n",
        ] {
            let err = match parse(line) {
                Ok(v) => panic!("{line:?} should have failed, got {v:?}"),
                Err(e) => e,
            };
            assert_eq!(err.kind, "bad_sender", "{line:?}");
            assert_eq!(err.reply.code(), 501, "{line:?}");
        }
    }

    #[test]
    fn source_routes_are_refused_outright() {
        for line in [
            "MAIL FROM:<@a.example,@b.example:user@example.com>\r\n",
            "RCPT TO:<@relay.example:user@example.com>\r\n",
            "MAIL FROM:<@relay.example:user@example.com>\r\n",
            // A bare `@domain` is not a source route either, but it is certainly not
            // an address, and refusing it as a route is the safe reading.
            "MAIL FROM:<@example.com>\r\n",
        ] {
            let err = match parse(line) {
                Ok(v) => panic!("{line:?} should have been refused, got {v:?}"),
                Err(e) => e,
            };
            assert_eq!(err.kind, "source_route", "{line:?}");
            assert_eq!(err.reply.code(), 550, "{line:?}");
            assert!(err.reply.text().contains("Relaying denied"), "{}", err.reply.text());
        }
    }

    #[test]
    fn a_quoted_local_part_containing_a_colon_is_not_a_source_route() {
        // `"a:b"@example.com` is a legal address; only a leading `@`-list is a route.
        match parse("RCPT TO:<\"a:b\"@example.com>\r\n").unwrap() {
            Command::RcptTo(p) => assert_eq!(p.to.local_part(), "\"a:b\""),
            other => panic!("{other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // RCPT TO
    // -----------------------------------------------------------------------

    #[test]
    fn rcpt_to_accepts_the_canonical_form() {
        match parse("RCPT TO:<bob@example.org>\r\n").unwrap() {
            Command::RcptTo(p) => assert_eq!(p.to, addr("bob@example.org")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rcpt_to_is_case_insensitive_and_whitespace_tolerant() {
        for line in [
            "RCPT TO:<bob@example.org>",
            "rcpt to:<bob@example.org>",
            "RCPT TO: <bob@example.org>",
            "RCPT  TO:<bob@example.org>",
            "RCPT TO:<bob@example.org> NOTIFY=SUCCESS",
        ] {
            let parsed = parse(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
            match parsed {
                Command::RcptTo(p) => assert_eq!(p.to, addr("bob@example.org"), "{line:?}"),
                other => panic!("{line:?} => {other:?}"),
            }
        }
    }

    #[test]
    fn rcpt_to_never_accepts_the_null_path() {
        assert!(parse("RCPT TO:<>\r\n").is_err());
        assert!(parse("RCPT TO:\r\n").is_err());
    }

    #[test]
    fn rcpt_to_requires_the_to_keyword() {
        assert!(parse("RCPT <bob@example.org>\r\n").is_err());
        assert!(parse("RCPT FROM:<bob@example.org>\r\n").is_err());
    }

    #[test]
    fn rcpt_to_collects_unknown_parameters() {
        match parse("RCPT TO:<bob@example.org> NOTIFY=FAILURE ORCPT=rfc822;bob@example.org\r\n")
            .unwrap()
        {
            Command::RcptTo(p) => {
                assert_eq!(p.unknown.len(), 2);
                assert!(p.unknown[0].starts_with("NOTIFY="));
            }
            other => panic!("{other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Data phase and transaction control
    // -----------------------------------------------------------------------

    #[test]
    fn simple_verbs_parse() {
        assert_eq!(parse("DATA\r\n"), Ok(Command::Data));
        assert_eq!(parse("data"), Ok(Command::Data));
        assert_eq!(parse("RSET\r\n"), Ok(Command::Rset));
        assert_eq!(parse("rset\r\n"), Ok(Command::Rset));
        assert_eq!(parse("QUIT\r\n"), Ok(Command::Quit));
        assert_eq!(parse("NOOP\r\n"), Ok(Command::Noop(String::new())));
        assert_eq!(parse("NOOP ping\r\n"), Ok(Command::Noop("ping".into())));
        assert_eq!(parse("HELP\r\n"), Ok(Command::Help(String::new())));
        assert_eq!(parse("HELP MAIL\r\n"), Ok(Command::Help("MAIL".into())));
        assert_eq!(parse("VRFY alice\r\n"), Ok(Command::Vrfy("alice".into())));
        assert_eq!(parse("STARTTLS\r\n"), Ok(Command::StartTls));
    }

    #[test]
    fn verbs_that_take_no_arguments_refuse_them() {
        for line in ["DATA now\r\n", "RSET all\r\n", "QUIT please\r\n", "STARTTLS now\r\n"] {
            assert!(parse(line).is_err(), "{line:?} must be refused");
        }
    }

    #[test]
    fn vrfy_requires_an_argument() {
        assert!(parse("VRFY\r\n").is_err());
        assert!(parse("VRFY   \r\n").is_err());
    }

    #[test]
    fn bdat_is_parsed_so_it_can_be_refused_precisely() {
        assert_eq!(parse("BDAT 100\r\n"), Ok(Command::Bdat(100, false)));
        assert_eq!(parse("BDAT 100 LAST\r\n"), Ok(Command::Bdat(100, true)));
        assert_eq!(parse("bdat 0 last\r\n"), Ok(Command::Bdat(0, true)));
        assert!(parse("BDAT\r\n").is_err());
        assert!(parse("BDAT abc\r\n").is_err());
        assert!(parse("BDAT 100 MIDDLE\r\n").is_err());
    }

    // -----------------------------------------------------------------------
    // AUTH
    // -----------------------------------------------------------------------

    #[test]
    fn auth_plain_with_and_without_an_initial_response() {
        assert_eq!(
            parse("AUTH PLAIN\r\n"),
            Ok(Command::Auth(AuthParams {
                mechanism: "PLAIN".into(),
                initial_response: None
            }))
        );
        assert_eq!(
            parse("AUTH PLAIN AGpvaG4AZG9l\r\n"),
            Ok(Command::Auth(AuthParams {
                mechanism: "PLAIN".into(),
                initial_response: Some("AGpvaG4AZG9l".into())
            }))
        );
        assert_eq!(
            parse("auth plain =\r\n"),
            Ok(Command::Auth(AuthParams {
                mechanism: "PLAIN".into(),
                initial_response: Some("=".into())
            }))
        );
    }

    #[test]
    fn auth_login_with_an_initial_username() {
        match parse("AUTH LOGIN dXNlcg==\r\n").unwrap() {
            Command::Auth(p) => {
                assert_eq!(p.mechanism, "LOGIN");
                assert_eq!(p.initial_response.as_deref(), Some("dXNlcg=="));
                assert!(p.has_initial_response());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn auth_mechanism_is_upper_cased() {
        match parse("auth login\r\n").unwrap() {
            Command::Auth(p) => assert_eq!(p.mechanism, "LOGIN"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn auth_requires_a_mechanism() {
        assert!(parse("AUTH\r\n").is_err());
        assert!(parse("AUTH   \r\n").is_err());
    }

    #[test]
    fn auth_refuses_a_non_base64_initial_response() {
        assert!(parse("AUTH PLAIN not base64!!\r\n").is_err());
        assert!(parse("AUTH PLAIN abc$def\r\n").is_err());
    }

    #[test]
    fn auth_refuses_a_third_argument() {
        assert!(parse("AUTH PLAIN AGpvaG4AZG9l extra\r\n").is_err());
    }

    #[test]
    fn auth_refuses_an_absurdly_long_initial_response() {
        let long = "A".repeat(9000);
        assert!(parse(&format!("AUTH PLAIN {long}\r\n")).is_err());
    }

    // -----------------------------------------------------------------------
    // Hostile input
    // -----------------------------------------------------------------------

    #[test]
    fn an_embedded_nul_is_refused() {
        let err = parse_command(b"MAIL FROM:<a@example.com>\x00EVIL\r\n").unwrap_err();
        assert_eq!(err.kind, "syntax");
    }

    #[test]
    fn an_embedded_bare_cr_is_refused() {
        // A peer trying to smuggle a second command into one line.
        let err = parse_command(b"NOOP\rX-Evil: 1\r\n").unwrap_err();
        assert_eq!(err.kind, "syntax");
    }

    #[test]
    fn an_embedded_bare_lf_inside_a_line_is_refused() {
        let err = parse_command(b"NOOP\nQUIT\r\n").unwrap_err();
        assert_eq!(err.kind, "syntax");
    }

    #[test]
    fn a_command_terminated_with_a_bare_lf_is_accepted() {
        // Broken clients do this constantly; real servers tolerate it.
        assert_eq!(parse_command(b"NOOP\n"), Ok(Command::Noop(String::new())));
        assert_eq!(parse_command(b"QUIT\n"), Ok(Command::Quit));
        assert_eq!(parse_command(b"QUIT\r"), Ok(Command::Quit));
    }

    #[test]
    fn eight_bit_bytes_in_a_command_are_refused_not_panicked_on() {
        let raw = b"MAIL FROM:<\xff\xfe\xfd@example.com>\r\n";
        let err = parse_command(raw).unwrap_err();
        assert_eq!(err.kind, "syntax");
        assert_eq!(err.reply.code(), 501);
    }

    #[test]
    fn eight_bit_bytes_inside_a_utf8_greeting_are_accepted() {
        // SMTPUTF8 greets with a UTF-8 name; that is valid UTF-8, so it parses.
        assert_eq!(
            parse("EHLO mail.bücher.example\r\n"),
            Ok(Command::Ehlo("mail.bücher.example".into()))
        );
    }

    #[test]
    fn an_empty_line_is_a_syntax_error() {
        assert_eq!(parse("\r\n").unwrap_err().kind, "empty_command");
        assert_eq!(parse("   \r\n").unwrap_err().kind, "empty_command");
        assert_eq!(parse("").unwrap_err().kind, "empty_command");
    }

    #[test]
    fn a_line_at_the_limit_is_accepted_and_one_byte_over_is_not() {
        let padding = "x".repeat(MAX_COMMAND_LINE - "NOOP ".len());
        let ok = format!("NOOP {padding}");
        assert_eq!(ok.len(), MAX_COMMAND_LINE);
        assert!(parse_command(ok.as_bytes()).is_ok());

        let too_long = format!("NOOP {padding}x");
        let err = parse_command(too_long.as_bytes()).unwrap_err();
        assert_eq!(err.kind, "line_too_long");
        assert_eq!(err.reply, Reply::line_too_long());
    }

    #[test]
    fn a_ten_thousand_byte_line_is_refused_quickly() {
        let huge = format!("MAIL FROM:<{}@example.com>\r\n", "a".repeat(10_000));
        let err = parse_command(huge.as_bytes()).unwrap_err();
        assert_eq!(err.kind, "line_too_long");
        assert_eq!(err.reply.code(), 500);
        assert_eq!(err.reply.text(), "5.5.6 Line too long");
    }

    #[test]
    fn a_ten_thousand_byte_line_with_no_verb_is_still_refused_as_too_long() {
        let err = parse_command(&vec![b'X'; 10_000]).unwrap_err();
        assert_eq!(err.kind, "line_too_long");
    }

    #[test]
    fn an_over_long_verb_is_refused() {
        let verb = "A".repeat(MAX_VERB_LEN + 1);
        let err = parse(&format!("{verb} arg\r\n")).unwrap_err();
        assert_eq!(err.kind, "syntax");
    }

    #[test]
    fn unknown_commands_get_the_unknown_command_reply() {
        let err = parse("XYZZY\r\n").unwrap_err();
        assert_eq!(err.kind, "unknown_command");
        assert_eq!(err.reply.code(), 500);
        assert_eq!(err.reply.text(), "5.5.2 Command unrecognized: XYZZY");
    }

    #[test]
    fn a_command_with_a_huge_prefix_of_spaces_is_handled() {
        let line = format!("{}NOOP\r\n", " ".repeat(500));
        assert_eq!(parse(&line), Ok(Command::Noop(String::new())));
    }

    #[test]
    fn deeply_nested_brackets_do_not_confuse_the_path_splitter() {
        // `<a<b@example.com>` — the local part validation rejects it, but the
        // splitter itself must not mis-slice the line.
        assert!(parse("MAIL FROM:<a<b@example.com>\r\n").is_err());
        assert!(parse("RCPT TO:<<bob@example.org>>\r\n").is_err());
    }

    #[test]
    fn parameters_after_the_null_sender_without_a_space_are_still_parsed() {
        // `<>SIZE=10` has no separating space. The path splitter ends the path at the
        // closing bracket, so the size parameter is read rather than swallowed into
        // the address — the lenient reading, which is what a real server does.
        match parse("MAIL FROM:<>SIZE=10\r\n").unwrap() {
            Command::MailFrom(p) => {
                assert!(p.from.is_none());
                assert_eq!(p.size, Some(10));
            }
            other => panic!("{other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

    #[test]
    fn verbs_are_reported_for_logging() {
        assert_eq!(parse("EHLO x\r\n").unwrap().verb(), "EHLO");
        assert_eq!(parse("MAIL FROM:<a@example.com>\r\n").unwrap().verb(), "MAIL");
        assert_eq!(parse("RCPT TO:<a@example.com>\r\n").unwrap().verb(), "RCPT");
        assert_eq!(parse("DATA\r\n").unwrap().verb(), "DATA");
        assert_eq!(parse("STARTTLS\r\n").unwrap().verb(), "STARTTLS");
    }

    #[test]
    fn only_mail_from_starts_a_transaction() {
        assert!(parse("MAIL FROM:<a@example.com>\r\n").unwrap().starts_transaction());
        for line in ["RCPT TO:<a@example.com>\r\n", "DATA\r\n", "RSET\r\n", "NOOP\r\n"] {
            assert!(!parse(line).unwrap().starts_transaction(), "{line:?}");
        }
    }

    #[test]
    fn body_type_round_trips_through_its_wire_token() {
        assert_eq!(BodyType::SevenBit.as_str(), "7BIT");
        assert_eq!(BodyType::EightBitMime.as_str(), "8BITMIME");
    }

    #[test]
    fn line_endings_are_stripped_from_every_shape() {
        for line in ["QUIT", "QUIT\r", "QUIT\n", "QUIT\r\n"] {
            assert_eq!(parse_command(line.as_bytes()), Ok(Command::Quit), "{line:?}");
        }
    }

    #[test]
    fn errors_are_displayable() {
        let err = parse("XYZZY\r\n").unwrap_err();
        assert!(format!("{err}").contains("unknown_command"));
        assert!(std::error::Error::source(&err).is_none());
    }
}
