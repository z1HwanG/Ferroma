//! Small shared protocol helpers: quoting, literals, numbers, dates and atoms.
//!
//! Everything in this module is deliberately total — none of it can panic on
//! peer-controlled input, which is why the width conversions below are written
//! with `saturating_*` rather than `as`.

use std::fmt;

use chrono::{DateTime, Datelike, NaiveDate, SecondsFormat, Timelike, Utc};

/// The maximum length of an IMAP `atom`/`astring` this server will accept, and
/// the largest literal any single command may carry.
///
/// 64 KiB is far above anything a real client sends in a command argument (the
/// largest realistic one is a mailbox name) while still bounding the memory a
/// single hostile command can make us allocate. `APPEND` payloads use
/// [`crate::ImapServerConfig::max_literal_size`] instead.
pub const MAX_ATOM_LEN: usize = 65_536;


/// `true` when `s` can be written as an IMAP atom.
///
/// RFC 3501 `atom-specials` are `(`, `)`, `{`, SP, CTL, the list-wildcards
/// `%`/`*`, the quoted-specials `"`/`\`, and `]` (which is special only inside a
/// `BODY[...]` section name, but is cheaper to exclude everywhere).
pub fn is_atom(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b > 0x20 && b < 0x7f && !matches!(b, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
        })
}

/// `true` when `s` can be written as an IMAP quoted string.
///
/// Quoted strings cannot carry CR, LF or NUL; everything else is escaped by
/// [`quote`].
pub fn is_quotable(s: &str) -> bool {
    !s.contains(['\r', '\n', '\0'])
}

/// Wrap `s` in an IMAP quoted string, escaping `"` and `\`.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// The shortest safe wire form of an `astring`: an atom when possible, a quoted
/// string otherwise.
///
/// This is used for values that are *not* mailbox names (so a literal is never
/// required and NIL is never allowed).
pub fn astring(s: &str) -> String {
    if is_atom(s) {
        s.to_string()
    } else {
        quote(s)
    }
}

/// How a string must be transmitted: as a `nstring` (`NIL` when absent), as an
/// atom, as a quoted string, or as a synchronising literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextForm {
    /// A bare atom.
    Atom,
    /// A `"quoted string"`.
    Quoted,
    /// A `{n}` synchronising literal.
    Literal,
}

/// The wire form an `nstring`/`astring` must take.
pub fn text_form(s: &str) -> TextForm {
    if is_atom(s) {
        TextForm::Atom
    } else if is_quotable(s) && !s.is_ascii() {
        // Quoted strings on the wire are ASCII in IMAP4rev1 unless the client
        // negotiated UTF8=ACCEPT; send anything else as a literal.
        TextForm::Literal
    } else if is_quotable(s) {
        TextForm::Quoted
    } else {
        TextForm::Literal
    }
}

/// An `nstring`: `NIL`, an atom, a quoted string, or a literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NString {
    /// The `NIL` value.
    Nil,
    /// A value, with the form it must be written in.
    Text(String, TextForm),
}

impl NString {
    /// Wrap an `Option<&str>`.
    pub fn from_option(value: Option<&str>) -> Self {
        match value {
            None => NString::Nil,
            Some(text) => NString::of(text),
        }
    }

    /// Wrap a string, choosing the shortest safe form.
    ///
    /// Named `of` rather than `from_str` so it cannot be mistaken for
    /// `std::str::FromStr::from_str`, which would have to report an error it
    /// does not have.
    pub fn of(text: &str) -> Self {
        NString::Text(text.to_string(), text_form(text))
    }

    /// Force the quoted form when the value is present.
    pub fn quoted(value: Option<&str>) -> Self {
        match value {
            None => NString::Nil,
            Some(text) => NString::Text(text.to_string(), TextForm::Quoted),
        }
    }

    /// Whether this is `NIL`.
    pub fn is_nil(&self) -> bool {
        matches!(self, NString::Nil)
    }
}

impl fmt::Display for NString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NString::Nil => f.write_str("NIL"),
            NString::Text(text, TextForm::Atom) => f.write_str(text),
            NString::Text(text, TextForm::Quoted) => f.write_str(&quote(text)),
            NString::Text(text, TextForm::Literal) => {
                write!(f, "{{{}}}\r\n{}", text.len(), text)
            }
        }
    }
}

/// An IMAP `INTERNALDATE`: `"01-Jan-2026 12:34:56 +0000"` (RFC 3501 §7.4.2).
pub fn internal_date(when: DateTime<Utc>) -> String {
    format!(
        "{:02}-{}-{:04} {:02}:{:02}:{:02} +0000",
        when.day(),
        month_name(when.month()),
        when.year(),
        when.hour(),
        when.minute(),
        when.second()
    )
}

/// An IMAP `date` (`dd-Mon-yyyy`), as `SEARCH` date keys use.
pub fn date_only(when: DateTime<Utc>) -> String {
    format!(
        "{:02}-{}-{:04}",
        when.day(),
        month_name(when.month()),
        when.year()
    )
}

/// The three-letter month name IMAP uses.
pub fn month_name(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "Jan",
    }
}

/// Parse an IMAP `date` (`dd-Mon-yyyy`, day may be space-padded or not).
pub fn parse_date(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    let (day, rest) = raw.split_at(raw.find('-')?);
    let (month, year) = rest[1..].split_once('-')?;
    let day: u32 = day.trim().parse().ok()?;
    let year: i32 = year.trim().parse().ok()?;
    let month = month_number(month)?;
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(0, 0, 0)?;
    Some(DateTime::from_naive_utc_and_offset(naive, Utc))
}

/// The month number for a three-letter IMAP month name.
pub fn month_number(name: &str) -> Option<u32> {
    let name = name.trim();
    (1..=12).find(|m| month_name(*m).eq_ignore_ascii_case(name))
}

/// Format an IMAP `date-time` as used by the `ID`/`WITHIN` extensions and by
/// our own diagnostics: RFC 3339 with second precision.
pub fn rfc3339(when: DateTime<Utc>) -> String {
    when.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A `BODY[section]<start.len>` partial specifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partial {
    /// Byte offset of the first octet returned.
    pub start: u64,
    /// Maximum number of octets returned.
    pub len: u64,
}

impl Partial {
    /// Apply the partial to a byte slice, clamped to its bounds.
    ///
    /// Returns an empty slice when the offset is past the end — an out-of-range
    /// partial is not an error in IMAP, it simply yields no data.
    pub fn slice<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        let len = data.len() as u64;
        if self.start >= len {
            return &[];
        }
        let start = self.start.min(len) as usize;
        let end = self.start.saturating_add(self.len).min(len) as usize;
        &data[start..end.max(start)]
    }

    /// The `n` in `<start.len>`, for the `BODY[section]<n>` response suffix.
    pub fn response_len(&self, data_len: usize) -> u64 {
        let start = self.start.min(data_len as u64);
        (data_len as u64 - start).min(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;


    #[test]
    fn atoms_reject_every_atom_special() {
        assert!(is_atom("INBOX"));
        assert!(is_atom("Sent"));
        assert!(is_atom("2026"));
        assert!(!is_atom(""));
        assert!(!is_atom("two words"));
        assert!(!is_atom("a(b"));
        assert!(!is_atom("a)b"));
        assert!(!is_atom("a{b"));
        assert!(!is_atom("a%b"));
        assert!(!is_atom("a*b"));
        assert!(!is_atom("a\"b"));
        assert!(!is_atom("a\\b"));
        assert!(!is_atom("a]b"));
        assert!(!is_atom("a\rb"));
        assert!(!is_atom("naïve"));
    }

    #[test]
    fn quoting_escapes_only_the_two_specials() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(quote("naïve"), "\"naïve\"");
    }

    #[test]
    fn astring_prefers_the_atom_form() {
        assert_eq!(astring("INBOX"), "INBOX");
        assert_eq!(astring("My Folder"), "\"My Folder\"");
        assert_eq!(astring(""), "\"\"");
    }

    #[test]
    fn text_form_routes_non_ascii_to_a_literal() {
        assert_eq!(text_form("INBOX"), TextForm::Atom);
        assert_eq!(text_form("My Folder"), TextForm::Quoted);
        assert_eq!(text_form("naïve"), TextForm::Literal);
        assert_eq!(text_form("with\rnewline"), TextForm::Literal);
    }

    #[test]
    fn nstring_renders_nil_atom_quoted_and_literal() {
        assert_eq!(NString::from_option(None).to_string(), "NIL");
        assert_eq!(NString::from_option(Some("Sent")).to_string(), "Sent");
        assert_eq!(NString::from_option(Some("My Box")).to_string(), "\"My Box\"");
        assert_eq!(NString::from_option(Some("naïve")).to_string(), "{6}\r\nnaïve");
        assert!(NString::from_option(None).is_nil());
    }

    #[test]
    fn internal_date_uses_the_imap_fixed_format() {
        let when = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(internal_date(when), "02-Jan-2026 03:04:05 +0000");
        let when = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();
        assert_eq!(internal_date(when), "31-Dec-2026 23:59:59 +0000");
    }

    #[test]
    fn date_only_and_parsing_round_trip() {
        let when = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
        assert_eq!(date_only(when), "09-Jul-2026");
        let parsed = parse_date("09-Jul-2026").expect("parses");
        assert_eq!(parsed, Utc.with_ymd_and_hms(2026, 7, 9, 0, 0, 0).unwrap());
        assert_eq!(parse_date(" 9-Jul-2026").unwrap().day(), 9);
    }

    #[test]
    fn date_parsing_rejects_garbage() {
        assert!(parse_date("").is_none());
        assert!(parse_date("nonsense").is_none());
        assert!(parse_date("32-Jul-2026").is_none());
        assert!(parse_date("09-Xxx-2026").is_none());
        assert!(parse_date("09-Jul-notayear").is_none());
    }

    #[test]
    fn month_names_cover_every_month() {
        for month in 1..=12u32 {
            let name = month_name(month);
            assert_eq!(name.len(), 3);
            assert_eq!(month_number(name), Some(month));
            assert_eq!(month_number(&name.to_lowercase()), Some(month));
        }
        assert_eq!(month_number("Xxx"), None);
    }

    #[test]
    fn partial_slices_clamp_instead_of_panicking() {
        let data = b"0123456789";
        let p = Partial { start: 2, len: 3 };
        assert_eq!(p.slice(data), b"234");
        assert_eq!(p.response_len(data.len()), 3);

        let p = Partial { start: 8, len: 99 };
        assert_eq!(p.slice(data), b"89");
        assert_eq!(p.response_len(data.len()), 2);

        let p = Partial { start: 10, len: 5 };
        assert!(p.slice(data).is_empty());
        assert_eq!(p.response_len(data.len()), 0);

        let p = Partial {
            start: u64::MAX,
            len: u64::MAX,
        };
        assert!(p.slice(data).is_empty());

        let p = Partial { start: 0, len: u64::MAX };
        assert_eq!(p.slice(data), data);
        assert_eq!(p.response_len(data.len()), 10);
    }

    #[test]
    fn rfc3339_is_second_precision_utc() {
        let when = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(rfc3339(when), "2026-01-02T03:04:05Z");
    }


}
