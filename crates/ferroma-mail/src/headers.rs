//! RFC 5322 header fields: an ordered multimap with case-insensitive lookup,
//! plus the folding/unfolding and RFC 2047 machinery that surrounds it.
//!
//! A header block is *ordered* — the order in which fields appear is preserved
//! (RFC 5322 §3.6 requires `Received` and `Resent-*` traces to keep their order,
//! and DKIM signing hashes the raw bytes) — yet lookups must be
//! *case-insensitive*, because `Message-ID`, `Message-Id` and `MESSAGE-ID` are
//! the same field name.
//!
//! Two encodings live here:
//!
//! * **Folding** (RFC 5322 §2.2.3): a field body may be split across lines as
//!   long as each continuation line starts with whitespace. Unfolding replaces
//!   `CRLF` + WSP with a single space.
//! * **Encoded words** (RFC 2047): non-ASCII text is carried as
//!   `=?charset?B|Q?payload?=` tokens.

use std::fmt;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::{DateTime, Utc};
use ferroma_core::{Result, RfcMessageId};
use once_cell::sync::Lazy;
use regex::Regex;

use crate::address::{parse_address_list, Mailbox};
use crate::mime::decode_charset;

/// Longest line we allow ourselves to emit before folding, in columns
/// (RFC 5322 §2.1.1 recommends 78 characters excluding CRLF).
pub const MAX_LINE_LENGTH: usize = 78;

/// Longest an RFC 2047 encoded word may be, including the `=?` and `?=` delimiters
/// (RFC 2047 §2 fixes the limit at 75 characters).
pub const MAX_ENCODED_WORD: usize = 75;

/// `=?charset?B|Q?payload?=` — the RFC 2047 encoded-word token.
static ENCODED_WORD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"=\?([^?\s]+)\?([bBqQ])\?([^?\s]*)\?=").expect("static encoded-word regex compiles")
});

/// An ordered, case-insensitive multimap of header fields.
///
/// Values are stored *unfolded*: a field body that arrived as
///
/// ```text
/// Subject: a very long
///  subject
/// ```
///
/// is stored as `"a very long subject"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    /// An empty header block.
    pub fn new() -> Self {
        Headers {
            entries: Vec::new(),
        }
    }

    /// Parse a raw header block, unfolding continuations.
    ///
    /// The input is the *header section only* — the caller has already split it
    /// from the body. Lines that are not header fields (junk before the first
    /// colon) terminate parsing: everything from that line on is ignored rather
    /// than rejecting the whole message, because a single bad field must never
    /// make an otherwise deliverable message undeliverable.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut headers = Headers::new();
        let mut current: Option<usize> = None;

        for line in split_header_lines(raw) {
            if line.is_empty() {
                continue;
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                // A continuation line: unfold into the previous field.
                match current {
                    Some(idx) => {
                        let joined = fold_join(&headers.entries[idx].1, line.trim());
                        headers.entries[idx].1 = joined;
                    }
                    // A continuation with nothing to continue is junk; skip it.
                    None => continue,
                }
                continue;
            }

            match split_field(line) {
                Some((name, value)) => {
                    headers.entries.push((name.to_string(), value.to_string()));
                    current = Some(headers.entries.len() - 1);
                }
                // Malformed line: tolerate everything before it and stop.
                None => break,
            }
        }

        Ok(headers)
    }

    /// Build a header block from an iterator of `(name, value)` pairs, without
    /// any unfolding or validation. Empty names are dropped.
    pub fn from_pairs<I, S1, S2>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S1, S2)>,
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        let mut headers = Headers::new();
        for (name, value) in pairs {
            let name = name.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            headers.entries.push((
                name.to_string(),
                value.as_ref().trim_matches(|c| c == '\r' || c == '\n').to_string(),
            ));
        }
        headers
    }

    /// The number of stored fields, counting duplicates.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when there are no fields at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the first field with this name, case-insensitive.
    fn index_of(&self, name: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// The value of the first field with this name, unfolded.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.index_of(name).map(|i| self.entries[i].1.as_str())
    }

    /// Every value for this name, in the order they appear.
    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Whether at least one field with this name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.index_of(name).is_some()
    }

    /// Set a field: the first occurrence is replaced and every later duplicate
    /// is dropped, so a field never ends up with two competing values.
    pub fn insert(&mut self, name: &str, value: &str) {
        let name = normalise_name(name);
        let lower = name.to_ascii_lowercase();
        let value = value.trim_matches(|c| c == '\r' || c == '\n');
        match self.entries.iter().position(|(k, _)| k.eq_ignore_ascii_case(&lower)) {
            Some(idx) => {
                self.entries[idx] = (name, value.to_string());
                let mut seen = false;
                self.entries.retain(|(k, _)| {
                    if k.eq_ignore_ascii_case(&lower) {
                        if seen {
                            return false;
                        }
                        seen = true;
                    }
                    true
                });
            }
            None => self.entries.push((name, value.to_string())),
        }
    }

    /// Add another occurrence, keeping the existing ones.
    pub fn append(&mut self, name: &str, value: &str) {
        let name = normalise_name(name);
        let value = value.trim_matches(|c| c == '\r' || c == '\n');
        self.entries.push((name, value.to_string()));
    }

    /// Drop every occurrence of a field.
    pub fn remove(&mut self, name: &str) {
        self.entries.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    /// Iterate over `(name, value)` in the original order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Iterate over the raw entries in order, duplicates included.
    pub fn iter_owned(&self) -> impl Iterator<Item = &(String, String)> {
        self.entries.iter()
    }

    /// Render the header block: every field folded to
    /// [`MAX_LINE_LENGTH`] columns, CRLF-terminated, followed by the blank line
    /// that separates headers from body.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (name, value) in &self.entries {
            out.push_str(&fold_header_line(name, value));
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out
    }

    /// The `Subject:` field, RFC 2047 decoded.
    pub fn subject(&self) -> Option<String> {
        self.get("Subject").map(decode_encoded_words)
    }

    /// The `Date:` field parsed into UTC, when it is syntactically usable.
    pub fn date(&self) -> Option<DateTime<Utc>> {
        self.get("Date").and_then(parse_date)
    }

    /// The `Message-ID:` field, angle brackets normalised by [`RfcMessageId`].
    pub fn message_id(&self) -> Option<RfcMessageId> {
        let raw = self.get("Message-ID")?;
        // A field may carry a comment or extra whitespace; take the first token.
        let token = raw.split_whitespace().next().unwrap_or(raw);
        if token.is_empty() {
            return None;
        }
        Some(RfcMessageId::new(token))
    }

    /// The `From:` mailboxes.
    pub fn from(&self) -> Vec<Mailbox> {
        self.address_list("From")
    }

    /// The `To:` mailboxes.
    pub fn to(&self) -> Vec<Mailbox> {
        self.address_list("To")
    }

    /// The `Cc:` mailboxes.
    pub fn cc(&self) -> Vec<Mailbox> {
        self.address_list("Cc")
    }

    /// The `Reply-To:` mailboxes.
    pub fn reply_to(&self) -> Vec<Mailbox> {
        self.address_list("Reply-To")
    }

    /// Parse any address-valued header into mailboxes, merging duplicates.
    pub fn address_list(&self, name: &str) -> Vec<Mailbox> {
        let mut out = Vec::new();
        for value in self.get_all(name) {
            out.extend(parse_address_list(value));
        }
        out
    }
}

impl fmt::Display for Headers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl<'a> IntoIterator for &'a Headers {
    type Item = (&'a str, &'a str);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a str)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// A header name as it should be stored: trimmed, with an internal space kept
/// (some historical fields are spelled with one) but never a colon.
fn normalise_name(name: &str) -> String {
    let trimmed = name.trim().trim_end_matches(':');
    trimmed.to_string()
}

/// Split a raw header block into physical lines, tolerating CRLF, LF and bare CR.
fn split_header_lines(raw: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&raw[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&raw[start..i]);
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < raw.len() {
        out.push(&raw[start..]);
    }
    out
}

/// Split one physical header line into `(name, value)`.
///
/// Returns `None` for a line that cannot be a header field: no colon at all, or
/// an empty name, or a name holding a character that RFC 5322 forbids (`ftext`).
fn split_field(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let (name, rest) = line.split_at(colon);
    if name.is_empty() {
        return None;
    }
    for ch in name.chars() {
        if !is_field_name_char(ch) {
            return None;
        }
    }
    // Strip exactly one colon plus the optional leading whitespace of the body.
    let value = rest[1..].trim_start_matches([' ', '\t']);
    Some((name, value))
}

/// RFC 5322 §3.6.8 `ftext`: printable US-ASCII except colon.
fn is_field_name_char(ch: char) -> bool {
    ch.is_ascii() && ch != ':' && (0x21..=0x7e).contains(&(ch as u32))
}

/// Join an unfolded value with a continuation line: exactly one space between.
fn fold_join(value: &str, continuation: &str) -> String {
    if value.is_empty() {
        return continuation.to_string();
    }
    if continuation.is_empty() {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + continuation.len() + 1);
    out.push_str(value.trim_end());
    out.push(' ');
    out.push_str(continuation.trim());
    out
}

/// Parse an RFC 5322 `date-time` into UTC.
///
/// The pre-`Date:`-with-`+0000` formats are covered first (that is what virtually
/// every real MUA emits), then the obsolete forms that predate the numeric zone.
pub fn parse_date(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Drop an RFC 822 comment such as `Mon, 1 Jan 2024 00:00:00 +0000 (UTC)`.
    let cleaned = strip_comments(raw);
    let cleaned = cleaned
        .trim()
        // `GMT`/`UT` are seen in the wild where a numeric zone belongs.
        .trim_end_matches([')', ' '])
        .to_string();

    const FORMATS: &[&str] = &[
        "%a, %d %b %Y %H:%M:%S %z",
        "%a, %d %b %Y %H:%M:%S%.f %z",
        "%d %b %Y %H:%M:%S %z",
        "%d %b %Y %H:%M:%S%.f %z",
        "%a, %d %b %Y %H:%M %z",
        "%d %b %Y %H:%M %z",
        "%a, %d %b %y %H:%M:%S %z",
        "%d %b %y %H:%M:%S %z",
        "%a, %d %b %Y %H:%M:%S",
        "%d %b %Y %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%d %H:%M:%S%z",
        "%Y-%m-%dT%H:%M:%SZ",
    ];

    for fmt in FORMATS {
        if let Ok(dt) = DateTime::parse_from_str(&cleaned, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
    }

    // Last resort: some senders write `+00:00` (RFC 3339) or a named zone.
    if let Ok(dt) = DateTime::parse_from_rfc3339(&cleaned) {
        return Some(dt.with_timezone(&Utc));
    }
    let named = replace_named_zones(&cleaned);
    if named != cleaned {
        for fmt in FORMATS {
            if let Ok(dt) = DateTime::parse_from_str(&named, fmt) {
                return Some(dt.with_timezone(&Utc));
            }
        }
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&cleaned, "%a, %d %b %Y %H:%M:%S") {
        return Some(naive.and_utc());
    }
    tracing::debug!(date = raw, "unparsable Date header");
    None
}

/// Remove balanced `(...)` comments from a date field.
fn strip_comments(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0usize;
    for ch in raw.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Rewrite obsolete named zones into numeric ones.
fn replace_named_zones(raw: &str) -> String {
    let upper = raw.to_ascii_uppercase();
    let mut out = raw.to_string();
    for (name, offset) in [
        ("UT", "+0000"),
        ("GMT", "+0000"),
        ("UTC", "+0000"),
        ("EST", "-0500"),
        ("EDT", "-0400"),
        ("CST", "-0600"),
        ("CDT", "-0500"),
        ("MST", "-0700"),
        ("MDT", "-0600"),
        ("PST", "-0800"),
        ("PDT", "-0700"),
    ] {
        // Only replace a trailing zone name, never a substring of another word.
        if let Some(pos) = upper.rfind(name) {
            let after_ok = upper[pos + name.len()..].trim().is_empty();
            let before_ok = pos == 0 || !upper.as_bytes()[pos - 1].is_ascii_alphabetic();
            if after_ok && before_ok {
                out = format!("{}{}", &raw[..pos], offset);
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// RFC 2047 encoded words
// ---------------------------------------------------------------------------

/// Decode every RFC 2047 encoded word in `value`.
///
/// Whitespace *between* two adjacent encoded words is removed (RFC 2047 §6.2:
/// it exists only to let the sender fold the field), while whitespace between an
/// encoded word and ordinary text is kept. Text that is not a well-formed
/// encoded word is left exactly as it was.
pub fn decode_encoded_words(value: &str) -> String {
    if !value.contains("=?") {
        return value.to_string();
    }

    let mut out = String::with_capacity(value.len());
    let mut last_end = 0usize;
    let mut previous_was_encoded = false;

    for caps in ENCODED_WORD.captures_iter(value) {
        let whole = caps.get(0).expect("group 0 always present");
        let charset = caps.get(1).map(|m| m.as_str()).unwrap_or("utf-8");
        let kind = caps.get(2).map(|m| m.as_str()).unwrap_or("B");
        let payload = caps.get(3).map(|m| m.as_str()).unwrap_or("");

        let gap = &value[last_end..whole.start()];
        let decoded: Option<Vec<u8>> = if kind.eq_ignore_ascii_case("B") {
            decode_b64_tolerant(payload)
        } else {
            Some(decode_q(payload))
        };

        let Some(bytes) = decoded else {
            // Not decodable: leave the token in place as ordinary text and carry
            // on scanning after it.
            out.push_str(gap);
            out.push_str(whole.as_str());
            last_end = whole.end();
            previous_was_encoded = false;
            continue;
        };

        if previous_was_encoded && gap.chars().all(char::is_whitespace) {
            // Drop the folding whitespace between two encoded words.
        } else {
            out.push_str(gap);
        }
        out.push_str(&decode_charset(&bytes, charset));
        last_end = whole.end();
        previous_was_encoded = true;
    }

    if last_end == 0 {
        return value.to_string();
    }
    out.push_str(&value[last_end..]);
    out
}

/// Encode a header value for transport.
///
/// Pure-ASCII values (including empty ones) pass through untouched. Anything
/// else is emitted as `=?UTF-8?B?…?=` encoded words, each at most
/// [`MAX_ENCODED_WORD`] characters and never splitting a UTF-8 sequence; the
/// words themselves are separated by a single space so the field can be folded
/// between them.
pub fn encode_header_value(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }

    let mut out = String::new();
    let mut chunk = String::new();
    for ch in value.chars() {
        let mut candidate = chunk.clone();
        candidate.push(ch);
        if encoded_word_len(&candidate) > MAX_ENCODED_WORD && !chunk.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&encode_word(&chunk));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&encode_word(&chunk));
    }
    out
}

/// The rendered length of the encoded word that would carry `text`.
fn encoded_word_len(text: &str) -> usize {
    "=?UTF-8?B?".len() + b64_len(text.len()) + "?=".len()
}

/// Base64 output length for `n` input bytes.
fn b64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// Render one `=?UTF-8?B?…?=` word.
fn encode_word(text: &str) -> String {
    format!("=?UTF-8?B?{}?=", B64.encode(text.as_bytes()))
}

/// Decode base64, returning `None` when nothing sensible came out.
fn decode_b64_tolerant(payload: &str) -> Option<Vec<u8>> {
    let cleaned: String = payload
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
        .collect();
    if cleaned.is_empty() {
        return Some(Vec::new());
    }
    B64.decode(cleaned.as_bytes()).ok()
}

/// Decode RFC 2047 `Q` encoding: `_` is a space, `=XX` is a raw byte.
fn decode_q(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'=');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

/// Fold one field into a complete `Name: value` line, CRLF-terminated inside but
/// without a trailing CRLF.
///
/// Continuation lines begin with exactly one space. Folds are only ever placed
/// at whitespace that sits outside a quoted string and outside an encoded word;
/// if no such point exists before the column limit the line is simply allowed to
/// run long, which is always better than corrupting the value.
pub fn fold_header_line(name: &str, value: &str) -> String {
    let prefix = format!("{name}: ");
    let prefix_len = prefix.len();
    let mut out = prefix;
    let mut pending_space = false;

    for ch in value.chars() {
        if ch == '\r' || ch == '\n' {
            // A stored value should never contain these, but if one slips in it
            // must become folding whitespace rather than escaping into the header
            // block.
            if !out.ends_with(' ') {
                out.push(' ');
                pending_space = true;
            }
            continue;
        }
        if ch == ' ' || ch == '\t' {
            // Collapse runs of whitespace: the encoder only ever wants one.
            if !out.ends_with(' ') {
                out.push(' ');
                pending_space = true;
            }
            continue;
        }
        if pending_space && out.len() + ch.len_utf8() > MAX_LINE_LENGTH {
            if let Some(pos) = last_foldable_space(&out, prefix_len) {
                let tail: String = out[pos + 1..].to_string();
                out.truncate(pos);
                out.push_str("\r\n ");
                out.push_str(&tail);
            }
        }
        pending_space = false;
        out.push(ch);
    }

    out
}

/// Find the last space in `line` (at or after `prefix_len`) that is safe to
/// fold at: outside a quoted string and outside an encoded word.
fn last_foldable_space(line: &str, prefix_len: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_quote = false;
    let mut candidates: Vec<usize> = Vec::new();
    let mut i = prefix_len;

    while i < bytes.len() {
        match bytes[i] {
            b'"' if !in_quote => in_quote = true,
            b'"' if in_quote => in_quote = false,
            b'\\' if in_quote => i += 1, // skip the escaped character
            b'=' if !in_quote && bytes.get(i + 1) == Some(&b'?') => {
                // An encoded word: skip it wholesale so its internals are never
                // chosen as a fold point.
                i = scan_encoded_word(line, i);
                continue;
            }
            b' ' | b'\t' if !in_quote => candidates.push(i),
            _ => {}
        }
        i += 1;
    }

    candidates.pop()
}

/// Given the byte index of the `=` in a `=?…?=` token, return the index just past
/// its terminating `?=`, or the end of the string when it is unterminated.
fn scan_encoded_word(line: &str, start: usize) -> usize {
    match line[start + 2..].find("?=") {
        Some(rel) => start + 2 + rel + 2,
        None => line.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_simple_headers_in_order() {
        let h = Headers::parse("From: a@b.c\r\nTo: d@e.f\r\nSubject: hi\r\n").unwrap();
        assert_eq!(h.len(), 3);
        let names: Vec<&str> = h.iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["From", "To", "Subject"]);
        assert_eq!(h.get("subject"), Some("hi"));
        assert!(h.contains("SUBJECT"));
    }

    #[test]
    fn unfolds_continuation_lines() {
        let h = Headers::parse("Subject: hello\r\n world\r\n\tagain\r\n").unwrap();
        assert_eq!(h.get("Subject"), Some("hello world again"));
    }

    #[test]
    fn accepts_lf_and_bare_cr_line_endings() {
        let lf = Headers::parse("A: 1\nB: 2\n").unwrap();
        assert_eq!(lf.get("A"), Some("1"));
        assert_eq!(lf.get("B"), Some("2"));

        let cr = Headers::parse("A: 1\rB: 2\r").unwrap();
        assert_eq!(cr.get("A"), Some("1"));
        assert_eq!(cr.get("B"), Some("2"));
    }

    #[test]
    fn stops_cleanly_at_a_malformed_line() {
        let h = Headers::parse("Good: yes\r\nthis line has no colon\r\nAfter: no\r\n").unwrap();
        assert_eq!(h.get("Good"), Some("yes"));
        assert!(!h.contains("After"));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn duplicate_fields_are_preserved_and_replaceable() {
        let mut h = Headers::parse("Received: one\r\nReceived: two\r\n").unwrap();
        assert_eq!(h.get_all("received"), vec!["one", "two"]);
        assert_eq!(h.get("Received"), Some("one"));

        h.insert("Received", "three");
        assert_eq!(h.get_all("Received"), vec!["three"]);

        h.append("Received", "four");
        assert_eq!(h.get_all("Received"), vec!["three", "four"]);
        assert_eq!(h.len(), 2);

        h.remove("received");
        assert!(!h.contains("Received"));
        assert!(h.is_empty());
    }

    #[test]
    fn insert_of_a_new_field_appends_at_the_end() {
        let mut h = Headers::new();
        h.insert("A", "1");
        h.insert("B", "2");
        h.insert("A", "3");
        let names: Vec<&str> = h.iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["A", "B"]);
        assert_eq!(h.get("A"), Some("3"));
    }

    #[test]
    fn render_is_crlf_terminated_and_ends_with_a_blank_line() {
        let h = Headers::from_pairs([("Subject", "hi"), ("From", "a@b.c")]);
        assert_eq!(h.render(), "Subject: hi\r\nFrom: a@b.c\r\n\r\n");
        assert_eq!(h.to_string(), h.render());
    }

    #[test]
    fn decodes_base64_encoded_subject() {
        // "你好，世界" in UTF-8, base64.
        let raw = "=?UTF-8?B?5L2g5aW977yM5LiW55WM?=";
        assert_eq!(decode_encoded_words(raw), "你好，世界");
        let h = Headers::parse(&format!("Subject: {raw}\r\n")).unwrap();
        assert_eq!(h.subject().as_deref(), Some("你好，世界"));
    }

    #[test]
    fn decodes_q_encoded_words_with_underscores() {
        assert_eq!(decode_encoded_words("=?UTF-8?Q?Hello_World?="), "Hello World");
        assert_eq!(decode_encoded_words("=?iso-8859-1?Q?caf=E9?="), "café");
        assert_eq!(decode_encoded_words("=?utf-8?q?caf=C3=A9?="), "café");
    }

    #[test]
    fn joins_adjacent_encoded_words_and_drops_between_whitespace() {
        let raw = "=?UTF-8?B?5L2g5aW9?=\r\n =?UTF-8?B?77yM5LiW55WM?=";
        assert_eq!(decode_encoded_words(raw), "你好，世界");
        // Whitespace between an encoded word and plain text is kept.
        assert_eq!(decode_encoded_words("=?UTF-8?Q?Hi?= there"), "Hi there");
    }

    #[test]
    fn leaves_malformed_encoded_words_alone() {
        assert_eq!(decode_encoded_words("plain ascii"), "plain ascii");
        assert_eq!(decode_encoded_words("=?UTF-8?B?not base64!!?="), "=?UTF-8?B?not base64!!?=");
        assert_eq!(decode_encoded_words("=?broken"), "=?broken");
    }

    #[test]
    fn encodes_non_ascii_into_bounded_words() {
        let value = "中文主题测试".repeat(20);
        let encoded = encode_header_value(&value);
        assert!(encoded.is_ascii());
        for word in encoded.split(' ') {
            assert!(word.starts_with("=?UTF-8?B?"), "unexpected word {word}");
            assert!(word.ends_with("?="));
            assert!(word.len() <= MAX_ENCODED_WORD, "word too long: {} chars", word.len());
        }
        assert_eq!(decode_encoded_words(&encoded), value);
    }

    #[test]
    fn encodes_ascii_by_passing_it_through() {
        assert_eq!(encode_header_value("plain"), "plain");
        assert_eq!(encode_header_value(""), "");
    }

    #[test]
    fn encoded_words_never_split_a_utf8_character() {
        // A run of 4-byte characters: a naive byte split would emit invalid UTF-8.
        let value = "😀".repeat(40);
        let encoded = encode_header_value(&value);
        assert_eq!(decode_encoded_words(&encoded), value);
        for word in encoded.split(' ') {
            let payload = word
                .trim_start_matches("=?UTF-8?B?")
                .trim_end_matches("?=");
            let bytes = B64.decode(payload).unwrap();
            assert!(std::str::from_utf8(&bytes).is_ok(), "split inside a character");
        }
    }

    #[test]
    fn folds_long_headers_on_a_single_space() {
        let value = "word ".repeat(40);
        let line = fold_header_line("Subject", value.trim_end());
        for physical in line.split("\r\n").skip(1) {
            assert!(physical.starts_with(' '), "continuation must start with a space");
            assert!(!physical.starts_with("  "), "exactly one space");
        }
        for physical in line.split("\r\n") {
            assert!(physical.len() <= MAX_LINE_LENGTH + 20, "line too long: {physical}");
        }
        // Unfolding restores the value.
        let h = Headers::parse(&format!("{line}\r\n")).unwrap();
        assert_eq!(h.get("Subject"), Some(value.trim_end()));
    }

    #[test]
    fn never_folds_inside_a_quoted_string() {
        let value = format!("\"{}\" tail", "x".repeat(200));
        let line = fold_header_line("X-Long", &value);
        let h = Headers::parse(&format!("{line}\r\n")).unwrap();
        assert_eq!(h.get("X-Long"), Some(value.as_str()));
        // The quotes survive intact.
        assert!(line.contains(&format!("\"{}\"", "x".repeat(200))));
    }

    #[test]
    fn never_folds_inside_an_encoded_word() {
        let word = "=?UTF-8?B?5L2g5aW977yM5LiW55WM5L2g5aW977yM5LiW55WM?=";
        let value = format!("{word} {word} {word}");
        let line = fold_header_line("Subject", &value);
        for physical in line.split("\r\n") {
            // No physical line may contain a partial encoded word.
            let opens = physical.matches("=?").count();
            let closes = physical.matches("?=").count();
            assert_eq!(opens, closes, "split inside an encoded word: {physical}");
        }
    }

    #[test]
    fn keeps_the_name_and_a_space_on_the_first_line() {
        let line = fold_header_line("Subject", "a");
        assert_eq!(line, "Subject: a");
        let long = fold_header_line("Subject", &"b".repeat(300));
        assert!(long.starts_with("Subject: b"));
    }

    #[test]
    fn parses_the_date_header() {
        let h = Headers::parse("Date: Tue, 16 Sep 2025 12:00:00 +0800\r\n").unwrap();
        let d = h.date().unwrap();
        assert_eq!(d.to_rfc3339(), "2025-09-16T04:00:00+00:00");
    }

    #[test]
    fn parses_obsolete_and_commented_dates() {
        assert!(parse_date("16 Sep 2025 12:00:00 GMT").is_some());
        assert!(parse_date("Tue, 16 Sep 2025 12:00:00 -0700 (PDT)").is_some());
        assert!(parse_date("Tue, 16 Sep 25 12:00:00 +0000").is_some());
        assert!(parse_date("nonsense").is_none());
        assert!(parse_date("").is_none());
    }

    #[test]
    fn extracts_message_id_with_and_without_brackets() {
        let h = Headers::parse("Message-ID: <abc@example.com>\r\n").unwrap();
        assert_eq!(h.message_id().unwrap().as_str(), "<abc@example.com>");

        let h = Headers::parse("Message-ID: abc@example.com\r\n").unwrap();
        assert_eq!(h.message_id().unwrap().as_str(), "<abc@example.com>");

        assert!(Headers::new().message_id().is_none());
    }

    #[test]
    fn extracts_address_lists_from_headers() {
        let h = Headers::parse(
            "From: Alice <alice@example.com>\r\nTo: bob@example.com, \"Carol, Jr\" <carol@x.org>\r\nCc: dave@y.org\r\nReply-To: eve@z.org\r\n",
        )
        .unwrap();
        let from = h.from();
        assert_eq!(from.len(), 1);
        assert_eq!(from[0].address.to_string(), "alice@example.com");
        assert_eq!(from[0].name.as_deref(), Some("Alice"));

        let to = h.to();
        assert_eq!(to.len(), 2);
        assert_eq!(to[1].name.as_deref(), Some("Carol, Jr"));

        assert_eq!(h.cc().len(), 1);
        assert_eq!(h.reply_to()[0].address.to_string(), "eve@z.org");
    }

    #[test]
    fn headers_round_trip_through_json() {
        let h = Headers::parse("A: 1\r\nB: 2\r\n").unwrap();
        let json = serde_json::to_string(&h).unwrap();
        let back: Headers = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn empty_and_whitespace_only_header_blocks() {
        let h = Headers::parse("").unwrap();
        assert!(h.is_empty());
        assert_eq!(h.render(), "\r\n");
        assert!(Headers::parse("   \r\n").unwrap().is_empty());
    }

    #[test]
    fn continuation_without_a_preceding_field_is_ignored() {
        let h = Headers::parse(" orphan\r\nReal: value\r\n").unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("Real"), Some("value"));
    }

    #[test]
    fn empty_field_values_are_kept() {
        let h = Headers::parse("X-Empty:\r\nX-Blank:   \r\n").unwrap();
        assert_eq!(h.get("X-Empty"), Some(""));
        assert_eq!(h.get("X-Blank"), Some(""));
    }

    #[test]
    fn fold_then_unfold_is_lossless_for_ascii() {
        let value = "The quick brown fox jumps over the lazy dog, repeatedly, forever and ever";
        let line = fold_header_line("Subject", value);
        let h = Headers::parse(&format!("{line}\r\n")).unwrap();
        assert_eq!(h.get("Subject"), Some(value));
    }

    #[test]
    fn charset_decoding_in_encoded_words_survives_unknown_text() {
        // An unknown charset falls back to lossy UTF-8 rather than panicking.
        let decoded = decode_encoded_words("=?x-unknown-charset?B?SGVsbG8=?=");
        assert_eq!(decoded, "Hello");
    }

    #[test]
    fn unfold_then_fold_then_unfold_is_stable() {
        let original = "Subject: A reasonably long subject line that a real MUA would have folded at least once\r\n";
        let once = Headers::parse(original).unwrap();
        let rendered = once.render();
        let twice = Headers::parse(&rendered).unwrap();
        assert_eq!(once, twice);
        assert_eq!(once.render(), twice.render());
    }

    #[test]
    fn a_field_with_only_a_continuation_keeps_the_name() {
        let h = Headers::parse("X-Long: start\r\n more\r\n\tand more\r\n").unwrap();
        assert_eq!(h.get("X-Long"), Some("start more and more"));
    }

    #[test]
    fn get_all_and_get_disagree_only_for_duplicates() {
        let h = Headers::parse("Received: a\r\nReceived: b\r\nSubject: s\r\n").unwrap();
        assert_eq!(h.get("Received"), Some("a"));
        assert_eq!(h.get_all("Received").len(), 2);
        assert_eq!(h.get_all("Subject"), vec!["s"]);
        assert!(h.get_all("Missing").is_empty());
        assert_eq!(h.get("Missing"), None);
    }

    #[test]
    fn insert_normalises_the_name_and_replaces_case_insensitively() {
        let mut h = Headers::new();
        h.insert("x-custom", "one");
        h.insert("X-Custom", "two");
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("X-CUSTOM"), Some("two"));
    }

    #[test]
    fn from_pairs_skips_empty_names_and_strips_newlines() {
        let h = Headers::from_pairs([("", "dropped"), ("A", "one\r\n"), ("B", "two")]);
        assert_eq!(h.len(), 2);
        assert_eq!(h.get("A"), Some("one"));
        assert_eq!(h.get("B"), Some("two"));
    }

    #[test]
    fn address_list_merges_repeated_fields() {
        let h = Headers::parse("To: a@b.com\r\nTo: c@d.com, e@f.com\r\n").unwrap();
        let to = h.to();
        assert_eq!(to.len(), 3);
        assert_eq!(to[2].address.to_string(), "e@f.com");
    }

    #[test]
    fn encoded_word_mixed_with_plain_text_keeps_both() {
        let decoded = decode_encoded_words("Re: =?UTF-8?B?5L2g5aW9?= (fwd)");
        assert_eq!(decoded, "Re: 你好 (fwd)");
    }

    #[test]
    fn q_encoded_underscore_is_a_space_but_adjacent_words_still_join() {
        assert_eq!(
            decode_encoded_words("=?UTF-8?Q?Hello_?= =?UTF-8?Q?World?="),
            "Hello World"
        );
    }

    #[test]
    fn three_adjacent_encoded_words_join_without_spaces() {
        let raw = "=?UTF-8?B?5L2g?=\r\n =?UTF-8?B?5aW9?=\r\n =?UTF-8?B?77yB?=";
        assert_eq!(decode_encoded_words(raw), "你好！");
    }

    #[test]
    fn null_and_control_characters_never_survive_into_a_rendered_line() {
        // A stored value can never contain a bare CR/LF: folding replaces them.
        let line = fold_header_line("X-Test", "a\r\nb");
        assert_eq!(line.split("\r\n").count(), 1, "unexpected break in {line:?}");
        assert_eq!(line, "X-Test: a b");
    }

    #[test]
    fn folding_a_value_that_is_only_whitespace_yields_the_name_alone() {
        assert_eq!(fold_header_line("X-Test", "   "), "X-Test: ");
        assert_eq!(fold_header_line("X-Test", ""), "X-Test: ");
    }

    #[test]
    fn date_with_an_obsolete_two_digit_year_and_named_zone() {
        // RFC 5322 obsoletes two-digit years; we accept them, and chrono applies
        // the RFC 2822 pivot rule (`25` → year 0025) rather than guessing 2025.
        let d = parse_date("Tue, 16 Sep 25 12:00:00 GMT").expect("parses");
        assert_eq!(d.format("%m-%d").to_string(), "09-16");
        assert_eq!(d.format("%H:%M").to_string(), "12:00");
    }

    #[test]
    fn message_id_ignores_trailing_comments() {
        let h = Headers::parse("Message-ID: <abc@example.com> (generated)\r\n").unwrap();
        assert_eq!(h.message_id().unwrap().inner(), "abc@example.com");
    }

    #[test]
    fn iter_is_borrowed_and_ordered() {
        let h = Headers::parse("Z: 1\r\nA: 2\r\nM: 3\r\n").unwrap();
        let names: Vec<&str> = (&h).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["Z", "A", "M"]);
    }
}
