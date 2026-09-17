//! DKIM: signing outbound mail and verifying inbound mail (RFC 6376).
//!
//! DKIM proves two things about a message: that a domain holding the matching public
//! key signed it, and that the parts it signed have not changed since. Both halves
//! live here.
//!
//! # Canonicalisation
//!
//! A signature covers two hashes:
//!
//! * the **body hash** (`bh=`), taken over the canonicalised body, and
//! * the **header hash**, taken over the headers named by `h=` plus the
//!   `DKIM-Signature` field itself with its `b=` value emptied (RFC 6376 §3.7).
//!
//! [`Canonicalization::Simple`] hashes octets exactly as they appear;
//! [`Canonicalization::Relaxed`] lower-cases field names, unfolds continuations,
//! collapses whitespace runs and strips trailing whitespace. A signature names both,
//! as `c=header/body`.
//!
//! # Why this module keeps raw octets instead of [`ferroma_mail::Headers`]
//!
//! `simple` canonicalisation hashes a header field *exactly as it appears in the
//! message*, folding included, and verification has to delete the `b=` value out of
//! that same raw text. [`ferroma_mail::Headers`] deliberately unfolds on parse, which
//! is right for every other consumer in the platform and wrong here, so this module
//! splits the raw header block itself (`split_fields`) and only decodes a field
//! value into text when it needs to read tags out of it.
//!
//! # Attacker-controlled input
//!
//! Every byte of a message and every TXT record is hostile until proven otherwise.
//! Nothing here panics on malformed input, and nothing logs key material or bodies.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ferroma_core::config::DkimConfig;
use ferroma_core::{FerromaError, Result};
use futures_util::future::BoxFuture;
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs1v15::{Signature as RsaSignature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePublicKey};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};

use crate::mx::{normalise_name, Resolver};

/// The name of the header field a signature travels in.
pub const SIGNATURE_HEADER_NAME: &str = "DKIM-Signature";

/// The name, colon *and* the single space that follows it — what a folded value's
/// column arithmetic has to allow for.
const SIGNATURE_HEADER_PREFIX: &str = "DKIM-Signature: ";

/// [`SIGNATURE_HEADER_PREFIX`]'s length: the column a header value starts in.
const SIGNATURE_PREFIX_LEN: usize = SIGNATURE_HEADER_PREFIX.len();

/// The column a folded line must not exceed, the CRLF excluded (RFC 5322 §2.1.1).
const MAX_FOLD_COLUMN: usize = 78;

/// How many signatures on one message are evaluated before giving up.
///
/// A message may legitimately carry several (a mailing list adds its own on top of
/// the author's); each one costs at most one DNS query, so the cap is what keeps a
/// message with a hundred bogus signatures from becoming a hundred lookups.
const MAX_SIGNATURES_EVALUATED: usize = 8;

// ---------------------------------------------------------------------------
// Canonicalisation
// ---------------------------------------------------------------------------

/// The header and body canonicalisation algorithms (RFC 6376 §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Canonicalization {
    /// Unfold continuations, lower-case field names, collapse whitespace runs and
    /// strip trailing whitespace. The default, and what RFC 6376 recommends.
    #[default]
    Relaxed,
    /// Hash the octets exactly as they appear in the message.
    Simple,
}

impl Canonicalization {
    /// Parse a single token: `"relaxed"` or `"simple"`.
    ///
    /// Anything unrecognised — including an empty string — is
    /// [`Canonicalization::Relaxed`], which is both the RFC recommendation and the
    /// more forgiving of the two.
    pub fn parse(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("simple") {
            Canonicalization::Simple
        } else {
            Canonicalization::Relaxed
        }
    }

    /// Parse a `c=` tag: `"header/body"`, or a single token meaning both.
    ///
    /// A missing half inherits the other, because `c=relaxed` and `c=relaxed/relaxed`
    /// mean the same thing.
    pub fn header_and_body(raw: &str) -> (Self, Self) {
        let trimmed = raw.trim();
        let mut parts = trimmed.splitn(2, '/');
        let header = parts.next().unwrap_or("").trim();
        let body = parts.next().map(str::trim).unwrap_or("");
        match (header.is_empty(), body.is_empty()) {
            (true, true) => (Canonicalization::Relaxed, Canonicalization::Relaxed),
            (true, false) => (Canonicalization::parse(body), Canonicalization::parse(body)),
            (false, true) => (
                Canonicalization::parse(header),
                Canonicalization::parse(header),
            ),
            (false, false) => (
                Canonicalization::parse(header),
                Canonicalization::parse(body),
            ),
        }
    }

    /// The wire token, as it appears in `c=`.
    pub fn as_str(self) -> &'static str {
        match self {
            Canonicalization::Relaxed => "relaxed",
            Canonicalization::Simple => "simple",
        }
    }
}

/// Split a message into its header block (terminator included) and its body.
///
/// The header block ends at the first empty line, spelled `\r\n\r\n` or the bare-`\n`
/// form some old relays emit. A message with no empty line is all headers and has an
/// empty body.
fn split_message(raw: &[u8]) -> (&[u8], &[u8]) {
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'\n' {
            let rest = &raw[i + 1..];
            if rest.first() == Some(&b'\n') {
                return (&raw[..=i], &rest[1..]);
            }
            if rest.first() == Some(&b'\r') && rest.get(1) == Some(&b'\n') {
                return (&raw[..=i], &rest[2..]);
            }
        }
        i += 1;
    }
    (raw, &[])
}

/// Split `data` on `\n`, returning each line without its terminator.
///
/// A trailing `\n` terminates the last line rather than starting an empty one.
fn physical_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, byte) in data.iter().enumerate() {
        if *byte == b'\n' {
            out.push(&data[start..i]);
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// Drop a single trailing `\r`, the terminator of a CRLF line.
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

/// Collapse every run of WSP to one SP and drop trailing WSP: the relaxed body rule,
/// and the second half of the relaxed header rule.
///
/// Leading whitespace survives — in a body it is content — but a *run* of it still
/// collapses to a single space.
fn collapse_wsp(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut pending = false;
    for &byte in line {
        if byte == b' ' || byte == b'\t' {
            pending = true;
            continue;
        }
        if pending {
            out.push(b' ');
            pending = false;
        }
        out.push(byte);
    }
    out
}

/// Canonicalise a body (RFC 6376 §3.4.3 and §3.4.4).
///
/// Both algorithms drop trailing empty lines — `simple` in so many words ("converts
/// `*CRLF` at the end of the body to a single `CRLF`"), `relaxed` by ignoring them.
/// The canonical form therefore always ends with exactly one CRLF, **including for a
/// body that is empty**: a signature over a message with no body covers `CRLF`, not
/// the empty octet string. (The two sections differ by one sentence on that single
/// case; treating them alike keeps the algorithms composable and matches what signers
/// in the wild emit.)
fn canonicalize_body(body: &[u8], canonicalization: Canonicalization) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for line in physical_lines(body) {
        let content = strip_cr(line);
        lines.push(match canonicalization {
            Canonicalization::Simple => content.to_vec(),
            Canonicalization::Relaxed => collapse_wsp(content),
        });
    }
    while lines.last().map(Vec::is_empty).unwrap_or(false) {
        lines.pop();
    }

    let mut out = Vec::new();
    for line in &lines {
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    if out.is_empty() {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Canonicalise one header field, relaxed (RFC 6376 §3.4.2).
///
/// `name` is the field name as written and `value` everything after the colon,
/// folding included. The result is `name:value` with no trailing CRLF.
fn canonicalize_header_relaxed(name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 1 + value.len());
    out.extend(name.iter().map(u8::to_ascii_lowercase));
    out.push(b':');

    let mut pending = false;
    let mut seen_text = false;
    for &byte in value {
        match byte {
            // Unfolding: RFC 5322 removes the CRLF and leaves the WSP that follows it
            // for the collapse below to deal with.
            b'\r' | b'\n' => continue,
            b' ' | b'\t' => {
                if seen_text {
                    pending = true;
                }
                continue;
            }
            _ => {}
        }
        if pending {
            out.push(b' ');
            pending = false;
        }
        out.push(byte);
        seen_text = true;
    }
    out
}

// ---------------------------------------------------------------------------
// Raw message structure
// ---------------------------------------------------------------------------

/// One physical line of a header block.
#[derive(Debug, Clone, Copy)]
struct Line {
    /// Offset of the line's first byte.
    start: usize,
    /// Offset just past the last content byte, before CR/LF.
    content_end: usize,
    /// Offset of the next line's first byte.
    end: usize,
    /// Whether the line starts with WSP and so continues the previous field.
    is_continuation: bool,
}

/// Cut a header block into physical lines.
fn scan_lines(block: &[u8]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut pos = 0usize;
    while pos < block.len() {
        let start = pos;
        let (content_end, end) = match block[pos..].iter().position(|b| *b == b'\n') {
            Some(offset) => {
                let newline = pos + offset;
                let content = if newline > start && block[newline - 1] == b'\r' {
                    newline - 1
                } else {
                    newline
                };
                (content, newline + 1)
            }
            None => (block.len(), block.len()),
        };
        lines.push(Line {
            start,
            content_end,
            end,
            is_continuation: matches!(block.get(start), Some(b' ') | Some(b'\t')),
        });
        if end <= start {
            break;
        }
        pos = end;
    }
    lines
}

/// One header field, kept as bytes so `simple` canonicalisation can reproduce it.
#[derive(Debug, Clone)]
struct RawField {
    /// The field name as written.
    name: String,
    /// The field name as written, byte-exact.
    name_raw: Vec<u8>,
    /// Everything after the colon, folding included.
    value: Vec<u8>,
    /// The whole field including its line terminator, byte-exact.
    raw: Vec<u8>,
}

/// Whether a byte may appear in a field name (RFC 5322 `ftext`).
fn is_ftext(byte: u8) -> bool {
    (33..=126).contains(&byte) && byte != b':'
}

/// Split a header block into fields, folding continuations into their field.
///
/// A line that is not a field is skipped rather than aborting the parse: a message
/// with one bad line must still be verifiable.
fn split_fields(block: &[u8]) -> Vec<RawField> {
    let lines = scan_lines(block);
    let mut fields = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let first = i;
        let mut last = i;
        i += 1;
        while i < lines.len() && lines[i].is_continuation {
            last = i;
            i += 1;
        }

        let start = lines[first].start;
        let content_end = lines[last].content_end;
        let end = lines[last].end;
        let field = &block[start..content_end];
        let Some(colon) = field.iter().position(|b| *b == b':') else {
            continue;
        };
        let name_bytes = &field[..colon];
        if name_bytes.is_empty() || !name_bytes.iter().all(|b| is_ftext(*b)) {
            continue;
        }
        fields.push(RawField {
            name: String::from_utf8_lossy(name_bytes).into_owned(),
            name_raw: name_bytes.to_vec(),
            value: field[colon + 1..].to_vec(),
            raw: block[start..end].to_vec(),
        });
    }
    fields
}

/// Remove every CR and LF from a field value, i.e. RFC 5322 unfolding.
fn unfold_bytes(value: &[u8]) -> Vec<u8> {
    value
        .iter()
        .copied()
        .filter(|b| *b != b'\r' && *b != b'\n')
        .collect()
}

/// Unfold a field value into text, lossily.
///
/// A signature's tags are ASCII; anything else is hostile input that must not panic
/// the verifier.
fn unfold_string(value: &[u8]) -> String {
    String::from_utf8_lossy(&unfold_bytes(value)).into_owned()
}

/// Join a TXT record's character-strings.
///
/// A long `p=` value is stored as several character-strings and presented as
/// `"AAAA" "BBBB"`; the concatenation has no separator (RFC 6376 §3.6.2.2). A value
/// with no quote at all is returned trimmed and otherwise untouched, so the
/// whitespace inside `h=Received : From` survives.
fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if !trimmed.contains('"') {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut in_quotes = false;
    let mut chars = trimmed.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' if in_quotes => {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }
            c if in_quotes => out.push(c),
            // Bytes outside a quoted string separate two character-strings.
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// Split a tag list into `(name, value)` pairs, lower-casing the names.
///
/// Empty segments are skipped, so a trailing `;` and a stray `;;` are both tolerated.
fn parse_tag_list(raw: &str) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some(eq) = part.find('=') else {
            continue;
        };
        let name = part[..eq].trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        tags.push((name, unquote(&part[eq + 1..])));
    }
    tags
}

/// Look a tag up in a parsed list.
fn tag<'a>(tags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Split a colon-separated tag value (`h=`, `t=`, `s=`) into its parts.
fn split_colon(value: &str) -> Vec<String> {
    value
        .split(':')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Remove every WSP from a base64 tag value.
fn squeeze(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Fold a tag list at [`MAX_FOLD_COLUMN`] columns.
///
/// Breaks are taken at tag boundaries first — after a `;`, with the continuation line
/// starting with one space — which is always legal FWS in the tag-list grammar. A tag
/// that is still too long on its own line is broken *inside*, after a `:`: the `h=`
/// tag is a colon-separated list and its grammar allows FWS around each colon, so a
/// fifteen-header list folds instead of running off the line. A base64 tag has no
/// such break point and simply makes a long line, which the caller wraps itself for
/// the one tag (`b=`) where it matters.
fn fold_tags(tags: &[String], prefix_len: usize) -> String {
    let mut out = String::new();
    let mut line_len = prefix_len;
    for (index, tag) in tags.iter().enumerate() {
        if index == 0 {
            push_tag(&mut out, tag, &mut line_len);
        } else if line_len + 2 + tag.len() > MAX_FOLD_COLUMN {
            out.push_str(";\r\n ");
            line_len = 1;
            push_tag(&mut out, tag, &mut line_len);
        } else {
            out.push_str("; ");
            line_len += 2;
            push_tag(&mut out, tag, &mut line_len);
        }
    }
    out
}

/// Append one tag, folding after a `:` when it cannot fit on the current line.
fn push_tag(out: &mut String, tag: &str, line_len: &mut usize) {
    let mut rest = tag;
    while !rest.is_empty() {
        let room = MAX_FOLD_COLUMN.saturating_sub(*line_len);
        if rest.len() <= room {
            out.push_str(rest);
            *line_len += rest.len();
            return;
        }
        // The last ':' that still fits, if any — a char-safe search, because a tag
        // value is attacker-controlled and may not be ASCII.
        let limit = room.min(rest.len());
        let mut split_at = None;
        for (index, c) in rest.char_indices() {
            if index >= limit {
                break;
            }
            if c == ':' {
                split_at = Some(index);
            }
        }
        match split_at {
            Some(index) => {
                out.push_str(&rest[..=index]);
                out.push_str("\r\n ");
                *line_len = 1;
                rest = &rest[index + 1..];
            }
            None => {
                out.push_str(rest);
                *line_len += rest.len();
                return;
            }
        }
    }
}

/// The column the last line of `text` ends at, assuming a `prefix_len`-wide first
/// line.
fn last_line_column(text: &str, prefix_len: usize) -> usize {
    match text.rfind('\n') {
        Some(index) => text.len() - index - 1,
        None => prefix_len + text.len(),
    }
}

/// Append a base64 signature to a folded tag list that ends in `b=`.
///
/// The result is that tag's value, wrapped with FWS so no line runs past
/// [`MAX_FOLD_COLUMN`]. Verification deletes everything after `b=` before hashing, so
/// how this wraps can never change the signature.
fn append_signature(mut folded: String, base64: &str, prefix_len: usize) -> String {
    let mut column = last_line_column(&folded, prefix_len);
    if column + 1 > MAX_FOLD_COLUMN {
        folded.push_str("\r\n ");
        column = 1;
    }
    let mut rest = base64;
    while !rest.is_empty() {
        let available = MAX_FOLD_COLUMN.saturating_sub(column).max(1);
        let take = available.min(rest.len());
        folded.push_str(&rest[..take]);
        rest = &rest[take..];
        column += take;
        if !rest.is_empty() {
            folded.push_str("\r\n ");
            column = 1;
        }
    }
    folded
}

/// Whether a byte may continue a tag name (RFC 6376 `ALNUMPUNC`).
fn is_alnum_punc(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether a byte is folding whitespace: WSP, or the CR/LF of a fold.
///
/// A field value arriving from the wire still carries its folds, so the tag scanner
/// has to step over `CRLF WSP` as readily as over a single space.
fn is_fws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// Delete the value of the `b=` tag from a raw field value, keeping the `;` that ends
/// it (RFC 6376 §3.7: "the value of the `b=` tag ... is deleted").
///
/// Returns `None` when there is no `b=` tag. Everything between `b=` and the next `;`
/// goes, FWS included.
fn strip_signature_value(value: &[u8]) -> Option<Vec<u8>> {
    let mut tag_start = 0usize;
    while tag_start < value.len() {
        let mut cursor = tag_start;
        while matches!(value.get(cursor), Some(b) if is_fws(*b)) {
            cursor += 1;
        }
        let name_start = cursor;
        while matches!(value.get(cursor), Some(b) if is_alnum_punc(*b)) {
            cursor += 1;
        }
        let name = &value[name_start..cursor];
        while matches!(value.get(cursor), Some(b) if is_fws(*b)) {
            cursor += 1;
        }
        if value.get(cursor) == Some(&b'=') {
            let value_start = cursor + 1;
            if name.eq_ignore_ascii_case(b"b") {
                let end = value[value_start..]
                    .iter()
                    .position(|b| *b == b';')
                    .map(|offset| value_start + offset)
                    .unwrap_or(value.len());
                let mut out = Vec::with_capacity(value.len());
                out.extend_from_slice(&value[..value_start]);
                out.extend_from_slice(&value[end..]);
                return Some(out);
            }
        }
        match value[tag_start..].iter().position(|b| *b == b';') {
            Some(offset) => tag_start += offset + 1,
            None => break,
        }
    }
    None
}

/// Canonicalise the signed headers, in `h=` order (RFC 6376 §5.4.2).
///
/// Each name takes the *last unused* instance of that field, working upwards from the
/// bottom of the header block; a name whose field is absent contributes nothing.
fn build_signed_headers(fields: &[RawField], h_list: &[String], canon: Canonicalization) -> Vec<u8> {
    let mut used: HashSet<usize> = HashSet::new();
    let mut out = Vec::new();
    for name in h_list {
        for index in (0..fields.len()).rev() {
            if used.contains(&index) || !fields[index].name.eq_ignore_ascii_case(name) {
                continue;
            }
            used.insert(index);
            match canon {
                Canonicalization::Simple => out.extend_from_slice(&fields[index].raw),
                Canonicalization::Relaxed => {
                    out.extend_from_slice(&canonicalize_header_relaxed(
                        &fields[index].name_raw,
                        &fields[index].value,
                    ));
                    out.extend_from_slice(b"\r\n");
                }
            }
            break;
        }
    }
    out
}

/// Whether `child` is `parent` or a subdomain of it.
fn domain_matches(parent: &str, child: &str) -> bool {
    if child.is_empty() || parent.is_empty() {
        return false;
    }
    child == parent || child.ends_with(&format!(".{parent}"))
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// Decode one PEM block body, whichever label it carries.
fn pem_der(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = pem[start..].find(&end)? + start;
    let body: String = pem[start..stop].chars().filter(|c| !c.is_whitespace()).collect();
    B64.decode(body.as_bytes()).ok()
}

/// An RSA private key, with the domain and selector it belongs to when those are
/// known.
///
/// Deliberately **not** `Debug`-derived: the key must never reach a log line.
#[derive(Clone)]
pub struct DkimKey {
    key: RsaPrivateKey,
    domain: Option<String>,
    selector: Option<String>,
}

impl std::fmt::Debug for DkimKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DkimKey")
            .field("bits", &self.key.n().bits())
            .field("domain", &self.domain)
            .field("selector", &self.selector)
            .finish_non_exhaustive()
    }
}

impl DkimKey {
    /// Parse a PEM private key.
    ///
    /// Both spellings are accepted: PKCS#1 (`-----BEGIN RSA PRIVATE KEY-----`, what
    /// `openssl genrsa` writes) and PKCS#8 (`-----BEGIN PRIVATE KEY-----`).
    pub fn from_pem(pem: &str) -> Result<Self> {
        if let Some(der) = pem_der(pem, "RSA PRIVATE KEY") {
            let key = RsaPrivateKey::from_pkcs1_der(&der).map_err(|e| {
                FerromaError::Invalid(format!("the DKIM private key is not valid PKCS#1: {e}"))
            })?;
            return Ok(DkimKey {
                key,
                domain: None,
                selector: None,
            });
        }
        if let Some(der) = pem_der(pem, "PRIVATE KEY") {
            let key = RsaPrivateKey::from_pkcs8_der(&der).map_err(|e| {
                FerromaError::Invalid(format!("the DKIM private key is not valid PKCS#8: {e}"))
            })?;
            return Ok(DkimKey {
                key,
                domain: None,
                selector: None,
            });
        }
        Err(FerromaError::Invalid(
            "the DKIM private key is not a PEM RSA private key".into(),
        ))
    }

    /// Read and parse a PEM private key from disk.
    ///
    /// Neither the key nor any part of it is logged; only the path, and only in the
    /// error a failed read produces.
    pub fn from_file(path: &Path) -> Result<Self> {
        let pem = std::fs::read_to_string(path).map_err(|e| {
            FerromaError::Config(format!(
                "cannot read the DKIM private key at {}: {e}",
                path.display()
            ))
        })?;
        DkimKey::from_pem(&pem)
    }

    /// The key's public half as the `p=` value of a `v=DKIM1` record: the DER-encoded
    /// `SubjectPublicKeyInfo`, base64.
    pub fn public_key_base64(&self) -> Result<String> {
        let der = self
            .key
            .to_public_key()
            .to_public_key_der()
            .map_err(|e| FerromaError::Internal(format!("cannot encode the DKIM public key: {e}")))?;
        Ok(B64.encode(der.as_bytes()))
    }

    /// The TXT record to publish at `<selector>._domainkey.<domain>`.
    ///
    /// `_selector` names the record's *owner* and is not part of its value; it is in
    /// the signature so the record name and its value are produced in one place. If
    /// the public key cannot be encoded at all, the record comes back with an empty
    /// `p=` — the RFC 6376 §3.6.1 spelling of "revoked", i.e. the safe direction — and
    /// a warning is logged.
    pub fn dns_record(&self, _selector: &str) -> String {
        match self.public_key_base64() {
            Ok(public_key) => format!("v=DKIM1; k=rsa; p={public_key}"),
            Err(e) => {
                tracing::warn!(error = %e, "cannot encode the DKIM public key; publishing a revoked record");
                "v=DKIM1; k=rsa; p=".to_string()
            }
        }
    }

    /// Remember which domain this key signs for.
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(normalise_name(&domain.into()));
        self
    }

    /// Remember which selector this key is published under.
    pub fn with_selector(mut self, selector: impl Into<String>) -> Self {
        self.selector = Some(selector.into().trim().to_string());
        self
    }

    /// The domain this key was told to sign for, if any.
    pub fn domain(&self) -> Option<&str> {
        self.domain.as_deref()
    }

    /// The selector this key was told it is published under, if any.
    pub fn selector(&self) -> Option<&str> {
        self.selector.as_deref()
    }

    /// The modulus size in bits, for an operator sanity check.
    pub fn bits(&self) -> usize {
        self.key.n().bits()
    }
}

/// A parsed `v=DKIM1; k=rsa; p=…` key record (RFC 6376 §3.6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DkimKeyRecord {
    /// `v=` — the record version, `DKIM1` when present.
    pub version: Option<String>,
    /// `k=` — the key type; `"rsa"` when the tag is absent.
    pub key_type: String,
    /// `p=` — the base64 public key. `Some("")` means the key was revoked.
    pub public_key: Option<String>,
    /// `t=` — flags: `y` (testing) and `s` (strict).
    pub flags: Vec<String>,
    /// `h=` — the hash algorithms this key may be used with.
    pub hashes: Vec<String>,
    /// `s=` — the services this key is valid for (`*` or `email`).
    pub services: Vec<String>,
    /// `n=` — a note for humans.
    pub notes: Option<String>,
}

impl DkimKeyRecord {
    /// Parse a key record.
    ///
    /// Unknown tags are ignored, as the grammar requires, and character-strings split
    /// across the record are rejoined. A record whose `v=` is present and is not
    /// `DKIM1` is an error.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut record = DkimKeyRecord {
            version: None,
            key_type: "rsa".to_string(),
            public_key: None,
            flags: Vec::new(),
            hashes: Vec::new(),
            services: Vec::new(),
            notes: None,
        };
        for (name, value) in parse_tag_list(raw) {
            match name.as_str() {
                "v" => {
                    if !value.eq_ignore_ascii_case("DKIM1") {
                        return Err(FerromaError::Invalid(format!(
                            "unsupported DKIM key record version {value:?}"
                        )));
                    }
                    record.version = Some(value);
                }
                "k" => record.key_type = value.to_ascii_lowercase(),
                "p" => record.public_key = Some(value),
                "t" => record.flags = split_colon(&value),
                "h" => record.hashes = split_colon(&value),
                "s" => record.services = split_colon(&value),
                "n" => record.notes = Some(value),
                _ => {}
            }
        }
        Ok(record)
    }

    /// Whether the key has been withdrawn (`p=` present but empty).
    ///
    /// A record with no `p=` at all is a different problem — an incomplete record —
    /// and the verifier reports it as such rather than as a revocation.
    pub fn is_revoked(&self) -> bool {
        matches!(self.public_key.as_deref(), Some(""))
    }

    /// Whether this key may be used with `hash` (RFC 6376 §3.6.1 `h=`).
    ///
    /// An absent `h=` means every algorithm the implementation supports.
    pub fn allows_hash(&self, hash: &str) -> bool {
        self.hashes.is_empty() || self.hashes.iter().any(|h| h.eq_ignore_ascii_case(hash))
    }

    /// Whether this key may be used for `service` (`"email"`).
    ///
    /// An absent `s=` means `*`, i.e. every service.
    pub fn allows_service(&self, service: &str) -> bool {
        self.services.is_empty()
            || self
                .services
                .iter()
                .any(|s| s == "*" || s.eq_ignore_ascii_case(service))
    }

    /// Whether the record is published for testing only (`t=y`).
    pub fn is_testing(&self) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case("y"))
    }

    /// Whether the key may only be used for exact matches of `d=` (`t=s`).
    pub fn is_strict(&self) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case("s"))
    }
}

/// A DER public key, spelled either of the two ways a signer might have published it.
fn parse_rsa_public_key(der: &[u8]) -> Option<RsaPublicKey> {
    RsaPublicKey::from_public_key_der(der)
        .ok()
        .or_else(|| RsaPublicKey::from_pkcs1_der(der).ok())
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// A configured outbound signer.
pub struct DkimSigner {
    key: RsaPrivateKey,
    signing_key: SigningKey<Sha256>,
    selector: String,
    domain: Option<String>,
    enabled: bool,
    header_canon: Canonicalization,
    body_canon: Canonicalization,
    headers_to_sign: Vec<String>,
}

impl std::fmt::Debug for DkimSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DkimSigner")
            .field("selector", &self.selector)
            .field("domain", &self.domain)
            .field("enabled", &self.enabled)
            .field(
                "canonicalization",
                &format!("{}/{}", self.header_canon.as_str(), self.body_canon.as_str()),
            )
            .field("headers_to_sign", &self.headers_to_sign)
            .finish_non_exhaustive()
    }
}

impl DkimSigner {
    /// Build a signer from `[dkim]`, reading the private key from disk.
    ///
    /// An unset or unreadable `private_key_path` is an error: a server that believes it
    /// signs but does not is worse than one that refuses to start.
    pub fn new(config: &DkimConfig) -> Result<Self> {
        let path = config
            .private_key_path
            .as_ref()
            .ok_or_else(|| FerromaError::Config("dkim.private_key_path is not set".into()))?;
        let key = DkimKey::from_file(path)?;
        DkimSigner::from_key(key, config)
    }

    /// Build a signer around an already-parsed key.
    pub fn from_key(key: DkimKey, config: &DkimConfig) -> Result<Self> {
        let selector = config.selector.trim();
        let selector = if selector.is_empty() {
            "default".to_string()
        } else {
            selector.to_string()
        };
        let domain = config
            .domain
            .as_deref()
            .map(normalise_name)
            .filter(|d| !d.is_empty())
            .or_else(|| key.domain.clone());
        let (header_canon, body_canon) =
            Canonicalization::header_and_body(&config.canonicalization);
        let headers_to_sign = config
            .headers_to_sign
            .iter()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .collect();

        let signing_key = SigningKey::<Sha256>::new(key.key.clone());
        Ok(DkimSigner {
            key: key.key,
            signing_key,
            selector,
            domain,
            enabled: config.enabled,
            header_canon,
            body_canon,
            headers_to_sign,
        })
    }

    /// Whether the signer was enabled by configuration.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The DKIM selector.
    pub fn selector(&self) -> &str {
        &self.selector
    }

    /// The `d=` domain, when one is configured.
    pub fn domain(&self) -> Option<&str> {
        self.domain.as_deref()
    }

    /// The header canonicalisation this signer emits.
    pub fn header_canonicalization(&self) -> Canonicalization {
        self.header_canon
    }

    /// The body canonicalisation this signer emits.
    pub fn body_canonicalization(&self) -> Canonicalization {
        self.body_canon
    }

    /// The RSA modulus size in bits, for an operator sanity check.
    pub fn key_bits(&self) -> usize {
        self.key.n().bits()
    }

    /// The body hash (`bh=`) the signer would emit for `message`.
    pub fn body_hash(&self, message: &[u8]) -> Result<String> {
        let (_, body) = split_message(message);
        Ok(B64.encode(Sha256::digest(canonicalize_body(
            body,
            self.body_canon,
        ))))
    }

    /// The full `DKIM-Signature` header **value** for `message`, `b=` included.
    ///
    /// The value is already folded; the caller writes `DKIM-Signature: ` in front of
    /// it. The message must not already carry this signature — the field is hashed as
    /// the last header, which is what RFC 6376 §5.4 requires of the field that is
    /// about to be prepended to it.
    pub fn signature_header(&self, message: &[u8]) -> Result<String> {
        self.signature_value(message, &[])
    }

    /// Sign `message`, returning the `DKIM-Signature:` header value (no name, no CRLF).
    pub fn sign(&self, message: &[u8]) -> Result<String> {
        self.signature_value(message, &[])
    }

    /// Sign `message` and return the whole message with the signature prepended.
    ///
    /// Prepending is the RFC 6376 §3.7 recommendation: it keeps the newest signature
    /// first, which is the order verifiers evaluate.
    pub fn sign_message(&self, message: &[u8]) -> Result<Vec<u8>> {
        let value = self.sign(message)?;
        Ok(prepend_signature(&value, message))
    }

    /// Sign when enabled, and hand the message back untouched when not.
    pub fn maybe_sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        if self.enabled {
            self.sign_message(message)
        } else {
            Ok(message.to_vec())
        }
    }

    /// Build the signature value, with `extra_tags` spliced in before `b=`.
    ///
    /// The extra-tag parameter exists so the body-length rule can be exercised
    /// end to end: an `l=` tag is only meaningful when it is part of what was signed,
    /// so it has to be folded into the hash input rather than pasted onto the finished
    /// header afterwards.
    fn signature_value(&self, message: &[u8], extra_tags: &[String]) -> Result<String> {
        let domain = self.domain.clone().ok_or_else(|| {
            FerromaError::Config(
                "dkim.domain is not set, so the signing domain cannot be chosen".into(),
            )
        })?;
        if domain.is_empty() {
            return Err(FerromaError::Config(
                "dkim.domain is empty, so the signing domain cannot be chosen".into(),
            ));
        }

        let (header_block, body) = split_message(message);
        let fields = split_fields(header_block);
        let h_list = self.select_headers(&fields)?;

        let body_hash = B64.encode(Sha256::digest(canonicalize_body(body, self.body_canon)));
        let mut tags = vec![
            "v=1".to_string(),
            "a=rsa-sha256".to_string(),
            format!(
                "c={}/{}",
                self.header_canon.as_str(),
                self.body_canon.as_str()
            ),
            format!("d={domain}"),
            format!("s={}", self.selector),
            format!("t={}", chrono::Utc::now().timestamp()),
            format!("h={}", h_list.join(":")),
            format!("bh={body_hash}"),
        ];
        tags.extend(extra_tags.iter().cloned());
        tags.push("b=".to_string());
        let folded = fold_tags(&tags, SIGNATURE_PREFIX_LEN);

        // The hash input is the signed headers followed by this very field with an
        // empty `b=` and no trailing CRLF (RFC 6376 §3.7).
        let mut hash_input = build_signed_headers(&fields, &h_list, self.header_canon);
        match self.header_canon {
            Canonicalization::Simple => {
                hash_input.extend_from_slice(SIGNATURE_HEADER_PREFIX.as_bytes());
                hash_input.extend_from_slice(folded.as_bytes());
            }
            Canonicalization::Relaxed => hash_input.extend_from_slice(&canonicalize_header_relaxed(
                SIGNATURE_HEADER_NAME.as_bytes(),
                folded.as_bytes(),
            )),
        }

        let signature = self
            .signing_key
            .try_sign(&hash_input)
            .map_err(|e| FerromaError::Internal(format!("DKIM signing failed: {e}")))?;
        Ok(append_signature(
            folded,
            &B64.encode(signature.to_vec()),
            SIGNATURE_PREFIX_LEN,
        ))
    }

    /// Pick the `h=` list: the configured headers the message actually carries, in
    /// configuration order, each once.
    ///
    /// `From` is mandatory (RFC 6376 §5.4): a signature that does not cover the author
    /// address can be replayed under a different one, which is exactly what DMARC
    /// alignment then cannot see.
    fn select_headers(&self, fields: &[RawField]) -> Result<Vec<String>> {
        let mut chosen: Vec<String> = Vec::new();
        for configured in &self.headers_to_sign {
            if configured.eq_ignore_ascii_case(SIGNATURE_HEADER_NAME) {
                continue;
            }
            if chosen
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(configured))
            {
                continue;
            }
            if fields
                .iter()
                .any(|field| field.name.eq_ignore_ascii_case(configured))
            {
                chosen.push(configured.clone());
            }
        }
        if !chosen.iter().any(|name| name.eq_ignore_ascii_case("from")) {
            return Err(FerromaError::Invalid(
                "the message has no From header field, which DKIM must sign".into(),
            ));
        }
        Ok(chosen)
    }
}

/// Write `DKIM-Signature: <value>\r\n` in front of a message.
fn prepend_signature(value: &str, message: &[u8]) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(message.len() + value.len() + SIGNATURE_HEADER_PREFIX.len() + 2);
    out.extend_from_slice(SIGNATURE_HEADER_PREFIX.as_bytes());
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(message);
    out
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

/// A parsed `DKIM-Signature` header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DkimSignature {
    /// Every tag, in the order it appeared.
    tags: Vec<(String, String)>,
    /// `h=`, split into field names in order.
    headers: Vec<String>,
    /// `b=`, with all FWS removed.
    signature: String,
    /// `bh=`, with all FWS removed.
    body_hash: String,
    /// `d=`, lower-cased and unqualified.
    domain: String,
    /// `s=`, trimmed.
    selector: String,
    /// `a=`, trimmed.
    algorithm: String,
}

impl DkimSignature {
    /// Parse a `DKIM-Signature` header value, folded or already unfolded.
    ///
    /// The tags `v`, `a`, `b`, `bh`, `d`, `h` and `s` are required (RFC 6376 §3.5),
    /// and `v` must be `1`; a value with no tags at all is an error too.
    pub fn parse(raw: &str) -> Result<Self> {
        let tags = parse_tag_list(&unfold_string(raw.as_bytes()));
        if tags.is_empty() {
            return Err(FerromaError::Parse(
                "the DKIM-Signature value has no tags".into(),
            ));
        }
        for required in ["v", "a", "b", "bh", "d", "h", "s"] {
            if tag(&tags, required).is_none() {
                return Err(FerromaError::Parse(format!(
                    "the DKIM-Signature value has no {required}= tag"
                )));
            }
        }
        let version = tag(&tags, "v").unwrap_or("");
        if version != "1" {
            return Err(FerromaError::Parse(format!(
                "unsupported DKIM signature version {version:?}"
            )));
        }
        Ok(DkimSignature {
            headers: tag(&tags, "h").map(split_colon).unwrap_or_default(),
            signature: squeeze(tag(&tags, "b").unwrap_or("")),
            body_hash: squeeze(tag(&tags, "bh").unwrap_or("")),
            domain: normalise_name(tag(&tags, "d").unwrap_or("")),
            selector: tag(&tags, "s").unwrap_or("").trim().to_string(),
            algorithm: tag(&tags, "a").unwrap_or("").trim().to_string(),
            tags,
        })
    }

    /// The `d=` domain, lower-cased and without a trailing dot.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// The `s=` selector.
    pub fn selector(&self) -> &str {
        &self.selector
    }

    /// The `h=` header names, in the order the signer listed them.
    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    /// The `bh=` body hash, FWS removed.
    pub fn body_hash(&self) -> &str {
        &self.body_hash
    }

    /// The `b=` signature, FWS removed.
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// The `l=` body length, when present and numeric.
    pub fn body_length(&self) -> Option<u64> {
        tag(&self.tags, "l").and_then(|raw| raw.trim().parse().ok())
    }

    /// The `(header, body)` canonicalisation from `c=`, defaulting to `simple/simple`
    /// as RFC 6376 §3.5 requires when the tag is absent.
    pub fn canonicalization(&self) -> (Canonicalization, Canonicalization) {
        match tag(&self.tags, "c") {
            Some(raw) => Canonicalization::header_and_body(raw),
            None => (Canonicalization::Simple, Canonicalization::Simple),
        }
    }

    /// The `a=` signing algorithm.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// The `i=` agent or user identifier, when present.
    pub fn identity(&self) -> Option<&str> {
        tag(&self.tags, "i")
    }

    /// The `t=` signature timestamp, when present and numeric.
    pub fn timestamp(&self) -> Option<i64> {
        tag(&self.tags, "t").and_then(|raw| raw.trim().parse().ok())
    }

    /// The `x=` expiration, when present and numeric.
    pub fn expiration(&self) -> Option<i64> {
        tag(&self.tags, "x").and_then(|raw| raw.trim().parse().ok())
    }

    /// The DNS name the public key is published at: `<selector>._domainkey.<domain>`.
    pub fn key_query(&self) -> String {
        format!("{}._domainkey.{}", self.selector, self.domain)
    }

    /// The header value, re-folded at `MAX_FOLD_COLUMN` columns.
    ///
    /// Tag order and values are preserved; only the whitespace between tags is
    /// normalised, so re-rendering never changes what a signature signs.
    pub fn render(&self) -> String {
        let tags: Vec<String> = self
            .tags
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        fold_tags(&tags, SIGNATURE_PREFIX_LEN)
    }
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// A DKIM verdict, as RFC 6376 §6.1 defines the outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkimResult {
    /// The message carries no signature at all.
    None,
    /// The signature verified.
    Pass,
    /// The signature did not verify: the message, or the key, was altered.
    Fail,
    /// The signature is valid but a policy it declares makes it unusable, such as an
    /// `x=` that has passed.
    Policy,
    /// The signature is syntactically valid but could not be evaluated.
    Neutral,
    /// A transient failure, usually DNS.
    TempError,
    /// The signature or the key record is unusable.
    PermError,
}

impl DkimResult {
    /// The wire token used in `Authentication-Results`.
    pub fn as_str(self) -> &'static str {
        match self {
            DkimResult::None => "none",
            DkimResult::Pass => "pass",
            DkimResult::Fail => "fail",
            DkimResult::Policy => "policy",
            DkimResult::Neutral => "neutral",
            DkimResult::TempError => "temperror",
            DkimResult::PermError => "permerror",
        }
    }

    /// Whether this verdict is a pass.
    pub fn is_pass(self) -> bool {
        self == DkimResult::Pass
    }

    /// Whether the message should be treated as unsigned rather than as forged.
    pub fn is_usable(self) -> bool {
        !matches!(self, DkimResult::Fail | DkimResult::Policy)
    }
}

impl std::fmt::Display for DkimResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of verifying one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DkimVerdict {
    /// The verdict.
    pub result: DkimResult,
    /// The `d=` domain of the signature that was evaluated.
    pub domain: Option<String>,
    /// Its `s=` selector.
    pub selector: Option<String>,
    /// Why the verdict is what it is, for `Authentication-Results` and the logs.
    pub reason: Option<String>,
}

impl DkimVerdict {
    /// A verdict attached to one signature.
    pub fn new(
        result: DkimResult,
        domain: Option<String>,
        selector: Option<String>,
        reason: impl Into<String>,
    ) -> Self {
        DkimVerdict {
            result,
            domain,
            selector,
            reason: Some(reason.into()),
        }
    }

    /// A verdict with no signature attached.
    pub fn none(reason: impl Into<String>) -> Self {
        DkimVerdict {
            result: DkimResult::None,
            domain: None,
            selector: None,
            reason: Some(reason.into()),
        }
    }

    /// A transient failure.
    pub fn temp_error(reason: impl Into<String>) -> Self {
        DkimVerdict::new(DkimResult::TempError, None, None, reason)
    }

    /// A permanent failure.
    pub fn perm_error(reason: impl Into<String>) -> Self {
        DkimVerdict::new(DkimResult::PermError, None, None, reason)
    }

    /// Whether this verdict is a pass.
    pub fn is_pass(&self) -> bool {
        self.result.is_pass()
    }
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Verifies inbound DKIM signatures against DNS keys.
#[derive(Debug, Clone)]
pub struct DkimVerifier {
    resolver: Arc<dyn Resolver>,
}

impl DkimVerifier {
    /// Build a verifier over `resolver`.
    pub fn new(resolver: Arc<dyn Resolver>) -> Self {
        DkimVerifier { resolver }
    }

    /// The resolver this verifier queries.
    pub fn resolver(&self) -> &Arc<dyn Resolver> {
        &self.resolver
    }

    /// Verify `message`, returning the first signature that passes.
    ///
    /// A message may carry several signatures; all of them are evaluated and the
    /// verdict is a pass when any of them is. When none passes, the topmost failure is
    /// reported, because that is the trust path the signer most recently created.
    ///
    /// The explicit lifetime is required because the returned future borrows both the
    /// verifier and the message; an elided `'_` would tie it to the verifier alone.
    pub fn verify<'a>(&'a self, message: &'a [u8]) -> BoxFuture<'a, DkimVerdict> {
        Box::pin(async move { self.verify_message(message).await })
    }

    /// The body of [`DkimVerifier::verify`].
    async fn verify_message(&self, message: &[u8]) -> DkimVerdict {
        let (header_block, body) = split_message(message);
        let fields = split_fields(header_block);
        let indices: Vec<usize> = fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.name.eq_ignore_ascii_case(SIGNATURE_HEADER_NAME))
            .map(|(index, _)| index)
            .take(MAX_SIGNATURES_EVALUATED)
            .collect();
        if indices.is_empty() {
            return DkimVerdict::none("the message carries no DKIM-Signature header field");
        }

        let mut first_failure: Option<DkimVerdict> = None;
        for index in indices {
            let verdict = self.verify_one(&fields, index, body).await;
            if verdict.is_pass() {
                return verdict;
            }
            if first_failure.is_none() {
                first_failure = Some(verdict);
            }
        }
        first_failure.unwrap_or_else(|| DkimVerdict::none("no DKIM signature was evaluated"))
    }

    /// Verify the signature in `fields[index]`.
    async fn verify_one(&self, fields: &[RawField], index: usize, body: &[u8]) -> DkimVerdict {
        let field = &fields[index];
        let tags = parse_tag_list(&unfold_string(&field.value));

        let version = tag(&tags, "v").unwrap_or("");
        if version != "1" {
            return DkimVerdict::perm_error(format!("unsupported DKIM version {version:?}"));
        }
        let algorithm = tag(&tags, "a").unwrap_or("").trim();
        if !algorithm.eq_ignore_ascii_case("rsa-sha256") {
            // RFC 8301 deprecates rsa-sha1 outright, so refusing it here is the rule
            // being enforced rather than a gap.
            return DkimVerdict::perm_error(format!("unsupported DKIM algorithm {algorithm:?}"));
        }

        let domain = normalise_name(tag(&tags, "d").unwrap_or(""));
        let selector = tag(&tags, "s").unwrap_or("").trim().to_string();
        if domain.is_empty() || selector.is_empty() {
            return DkimVerdict::perm_error("the DKIM signature has no usable d= or s= tag");
        }
        let body_hash = squeeze(tag(&tags, "bh").unwrap_or(""));
        let signature = squeeze(tag(&tags, "b").unwrap_or(""));
        if body_hash.is_empty() || signature.is_empty() {
            return DkimVerdict::perm_error("the DKIM signature has no usable bh= or b= tag");
        }

        let h_list = tag(&tags, "h").map(split_colon).unwrap_or_default();
        if h_list.is_empty() {
            return DkimVerdict::perm_error("the DKIM signature has an empty h= tag");
        }
        if !h_list.iter().any(|name| name.eq_ignore_ascii_case("from")) {
            return DkimVerdict::perm_error("the DKIM signature does not cover the From field");
        }

        if let Some(identity) = tag(&tags, "i") {
            let identity = identity.trim();
            if !identity.is_empty() {
                let identity_domain = match identity.rsplit_once('@') {
                    Some((_, suffix)) => normalise_name(suffix),
                    None => normalise_name(identity),
                };
                if !domain_matches(&domain, &identity_domain) {
                    return DkimVerdict::new(
                        DkimResult::PermError,
                        Some(domain.clone()),
                        Some(selector.clone()),
                        format!(
                            "the i= identity {identity_domain} is outside the d= domain {domain}"
                        ),
                    );
                }
            }
        }

        let (header_canon, body_canon) = match tag(&tags, "c") {
            Some(raw) => Canonicalization::header_and_body(raw),
            None => (Canonicalization::Simple, Canonicalization::Simple),
        };

        let canonical_body = canonicalize_body(body, body_canon);
        let signed_len = match tag(&tags, "l") {
            None => canonical_body.len(),
            Some(raw) => {
                let Ok(length) = raw.trim().parse::<u64>() else {
                    return DkimVerdict::new(
                        DkimResult::Fail,
                        Some(domain.clone()),
                        Some(selector.clone()),
                        format!("the l= tag {:?} is not a number", raw.trim()),
                    );
                };
                let Ok(length) = usize::try_from(length) else {
                    return DkimVerdict::new(
                        DkimResult::Fail,
                        Some(domain.clone()),
                        Some(selector.clone()),
                        "the l= tag does not fit an address space".to_string(),
                    );
                };
                if length > canonical_body.len() {
                    return DkimVerdict::new(
                        DkimResult::Fail,
                        Some(domain.clone()),
                        Some(selector.clone()),
                        format!(
                            "the l= tag ({length}) is larger than the {}-octet body",
                            canonical_body.len()
                        ),
                    );
                }
                if length < canonical_body.len() {
                    // RFC 6376 §8.2: everything past `l=` is unsigned, so a shorter
                    // `l=` lets an attacker append content to a signed message.
                    return DkimVerdict::new(
                        DkimResult::Fail,
                        Some(domain.clone()),
                        Some(selector.clone()),
                        format!(
                            "the l= tag ({length}) leaves {} octets of the body unsigned",
                            canonical_body.len() - length
                        ),
                    );
                }
                length
            }
        };

        let computed = B64.encode(Sha256::digest(&canonical_body[..signed_len]));
        if computed != body_hash {
            return DkimVerdict::new(
                DkimResult::Fail,
                Some(domain),
                Some(selector),
                "the body hash does not match the bh= tag".to_string(),
            );
        }

        if let Some(expiration) = tag(&tags, "x").and_then(|raw| raw.trim().parse::<i64>().ok()) {
            if expiration < chrono::Utc::now().timestamp() {
                return DkimVerdict::new(
                    DkimResult::Policy,
                    Some(domain),
                    Some(selector),
                    "the signature has expired".to_string(),
                );
            }
        }

        let key_name = format!("{selector}._domainkey.{domain}");
        let records = match self.resolver.txt(&key_name).await {
            Ok(records) => records,
            Err(e) => {
                return DkimVerdict::temp_error(format!(
                    "the DKIM key lookup for {key_name} failed: {e}"
                ))
            }
        };
        let Some(record) = records
            .iter()
            .find(|raw| raw.trim_start().to_ascii_uppercase().starts_with("V=DKIM1"))
        else {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                format!("there is no v=DKIM1 record at {key_name}"),
            );
        };
        let record = match DkimKeyRecord::parse(record) {
            Ok(record) => record,
            Err(e) => {
                return DkimVerdict::new(
                    DkimResult::PermError,
                    Some(domain),
                    Some(selector),
                    format!("the DKIM key record at {key_name} is unusable: {e}"),
                )
            }
        };
        if !record.key_type.eq_ignore_ascii_case("rsa") {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                format!("the DKIM key type {:?} is unsupported", record.key_type),
            );
        }
        if !record.allows_hash("sha256") {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                "the DKIM key record does not permit sha256".to_string(),
            );
        }
        if !record.allows_service("email") {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                "the DKIM key record is not valid for email".to_string(),
            );
        }
        if record.is_testing() {
            // RFC 6376 §3.6.1: a testing key's verdicts must not be enforced.
            tracing::debug!(key = %key_name, "DKIM key is published in testing mode");
        }
        let Some(public_key) = record.public_key.as_deref() else {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                format!("the DKIM key record at {key_name} has no p= tag"),
            );
        };
        if public_key.is_empty() {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                format!("the DKIM key at {key_name} has been revoked"),
            );
        }
        let Ok(der) = B64.decode(public_key.as_bytes()) else {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                format!("the p= value at {key_name} is not base64"),
            );
        };
        let Some(public_key) = parse_rsa_public_key(&der) else {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                format!("the p= value at {key_name} is not an RSA public key"),
            );
        };

        let Some(emptied) = strip_signature_value(&field.value) else {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                "the DKIM-Signature has no b= tag".to_string(),
            );
        };
        let mut hash_input = build_signed_headers(fields, &h_list, header_canon);
        match header_canon {
            Canonicalization::Simple => {
                hash_input.extend_from_slice(&field.name_raw);
                hash_input.push(b':');
                hash_input.extend_from_slice(&emptied);
            }
            Canonicalization::Relaxed => {
                hash_input.extend_from_slice(&canonicalize_header_relaxed(&field.name_raw, &emptied));
            }
        }

        let Ok(raw_signature) = B64.decode(signature.as_bytes()) else {
            return DkimVerdict::new(
                DkimResult::PermError,
                Some(domain),
                Some(selector),
                "the b= value is not base64".to_string(),
            );
        };
        let Ok(signature) = RsaSignature::try_from(raw_signature.as_slice()) else {
            return DkimVerdict::new(
                DkimResult::Fail,
                Some(domain),
                Some(selector),
                "the b= value is not a well-formed RSA signature".to_string(),
            );
        };
        let verifying_key = VerifyingKey::<Sha256>::new(public_key);
        match verifying_key.verify(&hash_input, &signature) {
            Ok(()) => DkimVerdict::new(
                DkimResult::Pass,
                Some(domain),
                Some(selector),
                format!("the signature at {key_name} verified"),
            ),
            Err(_) => DkimVerdict::new(
                DkimResult::Fail,
                Some(domain),
                Some(selector),
                "the header hash does not match the signature".to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mx::MockResolver;

    // ------------------------------------------------------------------
    // RFC 6376 Appendix A.2, A.3 and Appendix C fixtures
    // ------------------------------------------------------------------

    /// The published public key from RFC 6376 Appendix C.
    const APPENDIX_C_PUBLIC_KEY: &str = "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDwIRP/UC3SBsEmGqZ9ZJW3/DkMoGeLnQg1fWn7/zYtIxN2SnFCjxOCKG9v3b4jYfcTNh5ijSsq631uBItLa7od+v/RtdC2UzJ1lWT947qR+Rcac2gbto/NMqJ0fzfVjH4OuKhitdY9tf6mcwGjaNBcWToIMmPSPDdQPNUYckcQ2QIDAQAB";

    /// The matching private key from RFC 6376 Appendix C.
    const APPENDIX_C_PRIVATE_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
MIICXwIBAAKBgQDwIRP/UC3SBsEmGqZ9ZJW3/DkMoGeLnQg1fWn7/zYtIxN2SnFC\n\
jxOCKG9v3b4jYfcTNh5ijSsq631uBItLa7od+v/RtdC2UzJ1lWT947qR+Rcac2gb\n\
to/NMqJ0fzfVjH4OuKhitdY9tf6mcwGjaNBcWToIMmPSPDdQPNUYckcQ2QIDAQAB\n\
AoGBALmn+XwWk7akvkUlqb+dOxyLB9i5VBVfje89Teolwc9YJT36BGN/l4e0l6QX\n\
/1//6DWUTB3KI6wFcm7TWJcxbS0tcKZX7FsJvUz1SbQnkS54DJck1EZO/BLa5ckJ\n\
gAYIaqlA9C0ZwM6i58lLlPadX/rtHb7pWzeNcZHjKrjM461ZAkEA+itss2nRlmyO\n\
n1/5yDyCluST4dQfO8kAB3toSEVc7DeFeDhnC1mZdjASZNvdHS4gbLIA1hUGEF9m\n\
3hKsGUMMPwJBAPW5v/U+AWTADFCS22t72NUurgzeAbzb1HWMqO4y4+9Hpjk5wvL/\n\
eVYizyuce3/fGke7aRYw/ADKygMJdW8H/OcCQQDz5OQb4j2QDpPZc0Nc4QlbvMsj\n\
7p7otWRO5xRa6SzXqqV3+F0VpqvDmshEBkoCydaYwc2o6WQ5EBmExeV8124XAkEA\n\
qZzGsIxVP+sEVRWZmW6KNFSdVUpk3qzK0Tz/WjQMe5z0UunY9Ax9/4PVhp/j61bf\n\
eAYXunajbBSOLlx4D+TunwJBANkPI5S9iylsbLs6NkaMHV6k5ioHBBmgCak95JGX\n\
GMot/L2x0IYyMLAz6oLWh2hm7zwtb0CgOrPo1ke44hFYnfc=\n\
-----END RSA PRIVATE KEY-----\n";

    /// The A.2 body hash, quoted from RFC 6376 §A.2.
    const APPENDIX_A2_BODY_HASH: &str = "2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=";

    /// The A.2 `b=` value, quoted from RFC 6376 §A.2.
    const APPENDIX_A2_SIGNATURE: &str = "AuUoFEfDxTDkHlLXSZEpZj79LICEps6eda7W3deTVFOk4yAUoqOB4nujc7YopdG5dWLSdNg6xNAZpOPr+kHxt1IrE+NahM6L/LbvaHutKVdkLLkpVaVVQPzeRDI009SO2Il5Lu7rDNH6mZckBdrIx0orEtZV4bmp/YzhwvcubU4=";

    /// The header block of the A.2 figure, as the published signature covers it.
    ///
    /// Two details differ from the *printed* figure in RFC 6376 §A.2. Both were
    /// recovered from the signature rather than guessed:
    ///
    /// * the figure carries a display margin; continuation lines are six columns in
    ///   when the signer hashed them;
    /// * the printed body says `game.  Are` with two spaces, which cannot produce the
    ///   `bh=` value the figure prints — see [`APPENDIX_A2_BODY`].
    const APPENDIX_A2_HEADER_BLOCK: &str = concat!(
        "DKIM-Signature: v=1; a=rsa-sha256; s=brisbane; d=example.com;\r\n",
        "      c=simple/simple; q=dns/txt; i=joe@football.example.com;\r\n",
        "      h=Received : From : To : Subject : Date : Message-ID;\r\n",
        "      bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n",
        "      b=AuUoFEfDxTDkHlLXSZEpZj79LICEps6eda7W3deTVFOk4yAUoqOB\r\n",
        "      4nujc7YopdG5dWLSdNg6xNAZpOPr+kHxt1IrE+NahM6L/LbvaHut\r\n",
        "      KVdkLLkpVaVVQPzeRDI009SO2Il5Lu7rDNH6mZckBdrIx0orEtZV\r\n",
        "      4bmp/YzhwvcubU4=;\r\n",
        "Received: from client1.football.example.com  [192.0.2.1]\r\n",
        "      by submitserver.example.com with SUBMISSION;\r\n",
        "      Fri, 11 Jul 2003 21:01:54 -0700 (PDT)\r\n",
        "From: Joe SixPack <joe@football.example.com>\r\n",
        "To: Suzie Q <suzie@shopping.example.net>\r\n",
        "Subject: Is dinner ready?\r\n",
        "Date: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r\n",
        "Message-ID: <20030712040037.46341.5F8J@football.example.com>\r\n",
    );

    /// The A.2 body, exactly as the published `bh=` covers it.
    const APPENDIX_A2_BODY: &str =
        "Hi.\r\n\r\nWe lost the game. Are you hungry yet?\r\n\r\nJoe.\r\n";

    /// The complete signed message of RFC 6376 §A.2.
    fn appendix_a2_message() -> Vec<u8> {
        let mut message = APPENDIX_A2_HEADER_BLOCK.as_bytes().to_vec();
        message.extend_from_slice(b"\r\n");
        message.extend_from_slice(APPENDIX_A2_BODY.as_bytes());
        message
    }

    /// A resolver serving the Appendix C key at `brisbane._domainkey.example.com`.
    fn appendix_c_resolver() -> Arc<dyn Resolver> {
        Arc::new(MockResolver::new().with_txt(
            "brisbane._domainkey.example.com",
            vec![format!("v=DKIM1; k=rsa; p={APPENDIX_C_PUBLIC_KEY}")],
        ))
    }

    /// A signing configuration with everything spelled out.
    fn signing_config(domain: &str, selector: &str, canonicalization: &str) -> DkimConfig {
        DkimConfig {
            enabled: true,
            selector: selector.to_string(),
            private_key_path: None,
            domain: Some(domain.to_string()),
            canonicalization: canonicalization.to_string(),
            headers_to_sign: vec![
                "From".into(),
                "To".into(),
                "Subject".into(),
                "Date".into(),
                "Message-ID".into(),
            ],
            verify_inbound: true,
        }
    }

    /// A signer holding the Appendix C key.
    fn appendix_c_signer(canonicalization: &str) -> DkimSigner {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        DkimSigner::from_key(
            key,
            &signing_config("example.com", "brisbane", canonicalization),
        )
        .expect("the signer builds")
    }

    /// A resolver publishing whatever key `signer` uses.
    fn resolver_for(signer: &DkimSigner) -> Arc<dyn Resolver> {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        Arc::new(MockResolver::new().with_txt(
            &format!("{}._domainkey.example.com", signer.selector()),
            vec![key.dns_record(signer.selector())],
        ))
    }

    // ------------------------------------------------------------------
    // Canonicalisation selection
    // ------------------------------------------------------------------

    #[test]
    fn canonicalization_parses_every_spelling() {
        assert_eq!(Canonicalization::parse("relaxed"), Canonicalization::Relaxed);
        assert_eq!(Canonicalization::parse("simple"), Canonicalization::Simple);
        assert_eq!(Canonicalization::parse("SIMPLE"), Canonicalization::Simple);
        assert_eq!(Canonicalization::parse(" simple "), Canonicalization::Simple);
        assert_eq!(Canonicalization::parse(""), Canonicalization::Relaxed);
        assert_eq!(Canonicalization::parse("nonsense"), Canonicalization::Relaxed);
    }

    #[test]
    fn canonicalization_as_str_round_trips() {
        for name in ["relaxed", "simple"] {
            assert_eq!(Canonicalization::parse(name).as_str(), name);
        }
    }

    #[test]
    fn header_and_body_splits_a_pair() {
        assert_eq!(
            Canonicalization::header_and_body("relaxed/simple"),
            (Canonicalization::Relaxed, Canonicalization::Simple)
        );
        assert_eq!(
            Canonicalization::header_and_body("simple/relaxed"),
            (Canonicalization::Simple, Canonicalization::Relaxed)
        );
        assert_eq!(
            Canonicalization::header_and_body("simple/simple"),
            (Canonicalization::Simple, Canonicalization::Simple)
        );
    }

    #[test]
    fn header_and_body_inherits_a_missing_half() {
        assert_eq!(
            Canonicalization::header_and_body("simple"),
            (Canonicalization::Simple, Canonicalization::Simple)
        );
        assert_eq!(
            Canonicalization::header_and_body("relaxed/"),
            (Canonicalization::Relaxed, Canonicalization::Relaxed)
        );
        assert_eq!(
            Canonicalization::header_and_body("/simple"),
            (Canonicalization::Simple, Canonicalization::Simple)
        );
        assert_eq!(
            Canonicalization::header_and_body(""),
            (Canonicalization::Relaxed, Canonicalization::Relaxed)
        );
    }

    #[test]
    fn canonicalization_defaults_to_relaxed() {
        assert_eq!(Canonicalization::default(), Canonicalization::Relaxed);
    }

    // ------------------------------------------------------------------
    // Header canonicalisation
    // ------------------------------------------------------------------

    #[test]
    fn relaxed_header_canonicalisation_lower_cases_the_name_and_collapses_wsp() {
        let out = canonicalize_header_relaxed(b"SUBJECT", b"  hello   world \t ");
        assert_eq!(out, b"subject:hello world");
    }

    #[test]
    fn relaxed_header_canonicalisation_unfolds_continuations() {
        let out = canonicalize_header_relaxed(b"Received", b" from a\r\n\tby b");
        assert_eq!(out, b"received:from a by b");
    }

    #[test]
    fn relaxed_header_canonicalisation_keeps_inner_colons() {
        let out = canonicalize_header_relaxed(b"Message-ID", b" <a@b>");
        assert_eq!(out, b"message-id:<a@b>");
    }

    #[test]
    fn relaxed_header_canonicalisation_of_a_blank_value_is_just_the_name() {
        assert_eq!(canonicalize_header_relaxed(b"b", b"   "), b"b:");
    }

    #[test]
    fn simple_header_canonicalisation_is_the_raw_field() {
        let block = b"Subject:   hello   world \r\nFrom: a@b\r\n";
        let fields = split_fields(block);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].raw, b"Subject:   hello   world \r\n");
        assert_eq!(fields[0].name, "Subject");
        assert_eq!(fields[0].value, b"   hello   world ");
    }

    #[test]
    fn headers_fold_continuations_into_one_field() {
        let block = b"Received: from a\r\n\tby b\r\nSubject: x\r\n";
        let fields = split_fields(block);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "Received");
        assert_eq!(fields[0].value, b" from a\r\n\tby b");
        assert_eq!(fields[1].name, "Subject");
    }

    #[test]
    fn a_field_without_a_colon_is_skipped_not_fatal() {
        let block = b"garbage line\r\nSubject: x\r\n";
        let fields = split_fields(block);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "Subject");
    }

    #[test]
    fn the_signed_header_list_takes_the_last_instance() {
        let block = b"Received: one\r\nReceived: two\r\nFrom: a@b\r\n";
        let fields = split_fields(block);
        let out = build_signed_headers(&fields, &["Received".to_string()], Canonicalization::Simple);
        assert_eq!(out, b"Received: two\r\n");
    }

    #[test]
    fn a_header_named_twice_takes_two_instances() {
        let block = b"Received: one\r\nReceived: two\r\n";
        let fields = split_fields(block);
        let out = build_signed_headers(
            &fields,
            &["Received".to_string(), "Received".to_string()],
            Canonicalization::Simple,
        );
        assert_eq!(out, b"Received: two\r\nReceived: one\r\n");
    }

    #[test]
    fn an_absent_header_contributes_nothing() {
        let fields = split_fields(b"From: a@b\r\n");
        let out = build_signed_headers(&fields, &["Cc".to_string()], Canonicalization::Simple);
        assert!(out.is_empty());
    }

    // ------------------------------------------------------------------
    // Body canonicalisation
    // ------------------------------------------------------------------

    #[test]
    fn relaxed_body_collapses_and_trims_whitespace() {
        let out = canonicalize_body(b"a  b\t\tc  \r\n", Canonicalization::Relaxed);
        assert_eq!(out, b"a b c\r\n");
    }

    #[test]
    fn relaxed_body_drops_trailing_empty_lines_but_keeps_inner_ones() {
        let out = canonicalize_body(b"a\r\n\r\nb\r\n\r\n\r\n", Canonicalization::Relaxed);
        assert_eq!(out, b"a\r\n\r\nb\r\n");
    }

    #[test]
    fn relaxed_body_keeps_leading_whitespace_but_collapses_the_run() {
        let out = canonicalize_body(b"    indented\r\n", Canonicalization::Relaxed);
        assert_eq!(out, b" indented\r\n");
    }

    #[test]
    fn simple_body_only_drops_trailing_empty_lines() {
        let out = canonicalize_body(b"a  b\r\n\r\n\r\n", Canonicalization::Simple);
        assert_eq!(out, b"a  b\r\n");
    }

    #[test]
    fn a_body_without_a_trailing_crlf_gains_one() {
        assert_eq!(
            canonicalize_body(b"a\r\nb", Canonicalization::Simple),
            b"a\r\nb\r\n"
        );
        assert_eq!(
            canonicalize_body(b"a\r\nb", Canonicalization::Relaxed),
            b"a\r\nb\r\n"
        );
    }

    #[test]
    fn an_empty_body_canonicalises_to_a_single_crlf() {
        assert_eq!(canonicalize_body(b"", Canonicalization::Simple), b"\r\n");
        assert_eq!(canonicalize_body(b"", Canonicalization::Relaxed), b"\r\n");
        assert_eq!(canonicalize_body(b"\r\n\r\n", Canonicalization::Simple), b"\r\n");
    }

    #[test]
    fn simple_and_relaxed_disagree_about_whitespace() {
        let body = b"a  b  \r\n";
        assert_ne!(
            canonicalize_body(body, Canonicalization::Simple),
            canonicalize_body(body, Canonicalization::Relaxed)
        );
    }

    #[test]
    fn a_bare_lf_body_is_handled() {
        assert_eq!(
            canonicalize_body(b"a\nb\n", Canonicalization::Simple),
            b"a\r\nb\r\n"
        );
    }

    // ------------------------------------------------------------------
    // Message splitting
    // ------------------------------------------------------------------

    #[test]
    fn split_message_finds_the_crlf_crlf_boundary() {
        let (headers, body) = split_message(b"A: b\r\n\r\nbody\r\n");
        assert_eq!(headers, b"A: b\r\n");
        assert_eq!(body, b"body\r\n");
    }

    #[test]
    fn split_message_finds_the_bare_lf_boundary() {
        let (headers, body) = split_message(b"A: b\n\nbody");
        assert_eq!(headers, b"A: b\n");
        assert_eq!(body, b"body");
    }

    #[test]
    fn a_message_with_no_blank_line_is_all_headers() {
        let (headers, body) = split_message(b"A: b\r\n");
        assert_eq!(headers, b"A: b\r\n");
        assert!(body.is_empty());
    }

    // ------------------------------------------------------------------
    // The RFC 6376 Appendix A.2 vector
    // ------------------------------------------------------------------

    #[test]
    fn rfc6376_a2_body_hash_is_byte_exact() {
        let signer = appendix_c_signer("simple/simple");
        assert_eq!(
            signer.body_hash(&appendix_a2_message()).expect("hashed"),
            "2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8="
        );
    }

    #[test]
    fn rfc6376_a2_simple_body_canonicalisation_is_the_printed_body() {
        assert_eq!(
            canonicalize_body(APPENDIX_A2_BODY.as_bytes(), Canonicalization::Simple),
            APPENDIX_A2_BODY.as_bytes()
        );
    }

    #[test]
    fn the_rfc6376_a2_figure_typo_body_hashes_differently() {
        // The figure prints two spaces after "game." while the `bh=` it shows was
        // taken over one. Pinning the other value here is what makes the byte-exact
        // assertion above meaningful rather than accidental.
        let typo = b"Hi.\r\n\r\nWe lost the game.  Are you hungry yet?\r\n\r\nJoe.\r\n";
        let hash = B64.encode(Sha256::digest(canonicalize_body(
            typo,
            Canonicalization::Simple,
        )));
        assert_eq!(hash, "4bLNXImK9drULnmePzZNEBleUanJCX5PIsDIFoH4KTQ=");
        assert_ne!(hash, APPENDIX_A2_BODY_HASH);
    }

    #[test]
    fn rfc6376_a2_signature_value_is_reproduced_byte_exactly() {
        // Re-signing the A.2 header hash with the Appendix C private key must return
        // the `b=` value the RFC prints, which pins both the header canonicalisation
        // and the `b=`-deletion rule to the octet.
        let message = appendix_a2_message();
        let (header_block, _) = split_message(&message);
        let fields = split_fields(header_block);
        let h_list: Vec<String> = vec![
            "Received".into(),
            "From".into(),
            "To".into(),
            "Subject".into(),
            "Date".into(),
            "Message-ID".into(),
        ];
        let mut hash_input = build_signed_headers(&fields, &h_list, Canonicalization::Simple);
        let emptied = strip_signature_value(&fields[0].value).expect("the fixture has a b= tag");
        hash_input.extend_from_slice(&fields[0].name_raw);
        hash_input.push(b':');
        hash_input.extend_from_slice(&emptied);

        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let signing_key = SigningKey::<Sha256>::new(key.key.clone());
        let produced = B64.encode(
            signing_key
                .try_sign(&hash_input)
                .expect("signing succeeds")
                .to_vec(),
        );
        assert_eq!(produced, APPENDIX_A2_SIGNATURE);
    }

    #[tokio::test]
    async fn rfc6376_a2_signature_verifies_with_the_published_key() {
        let verifier = DkimVerifier::new(appendix_c_resolver());
        let verdict = verifier.verify(&appendix_a2_message()).await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
        assert_eq!(verdict.domain.as_deref(), Some("example.com"));
        assert_eq!(verdict.selector.as_deref(), Some("brisbane"));
    }

    #[tokio::test]
    async fn a_tampered_a2_body_fails() {
        let mut message = appendix_a2_message();
        let position = message
            .windows(9)
            .position(|w| w == b"the game.")
            .expect("the body marker exists");
        message[position] = b'T';
        let verdict = DkimVerifier::new(appendix_c_resolver())
            .verify(&message)
            .await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
        assert!(verdict.reason.as_deref().unwrap_or("").contains("body hash"));
    }

    #[tokio::test]
    async fn a_tampered_a2_subject_fails() {
        let mut message = appendix_a2_message();
        let position = message
            .windows(14)
            .position(|w| w == b"Subject: Is di")
            .expect("the subject is present");
        message[position + 9] = b'a';
        let verdict = DkimVerifier::new(appendix_c_resolver())
            .verify(&message)
            .await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
    }

    #[test]
    fn the_appendix_c_key_pair_matches() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        assert_eq!(
            key.public_key_base64().expect("encodes"),
            APPENDIX_C_PUBLIC_KEY
        );
        assert_eq!(key.bits(), 1024);
    }

    #[test]
    fn a_key_pem_that_is_not_a_key_is_rejected() {
        assert!(DkimKey::from_pem("not a pem at all").is_err());
        assert!(DkimKey::from_pem(
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n"
        )
        .is_err());
        assert!(DkimKey::from_pem("").is_err());
    }

    #[test]
    fn a_pkcs8_private_key_pem_is_accepted() {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let pkcs8 = key
            .key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("re-encodes as PKCS#8");
        let reparsed = DkimKey::from_pem(&pkcs8).expect("the PKCS#8 spelling parses");
        assert_eq!(
            reparsed.public_key_base64().expect("encodes"),
            APPENDIX_C_PUBLIC_KEY
        );
    }

    #[test]
    fn a_key_file_that_does_not_exist_is_a_configuration_error() {
        let err = DkimKey::from_file(Path::new("no/such/dkim.key")).expect_err("must fail");
        assert!(matches!(err, FerromaError::Config(_)), "{err:?}");
    }

    #[test]
    fn a_key_round_trips_through_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dkim.key");
        std::fs::write(&path, APPENDIX_C_PRIVATE_KEY).expect("write");
        let key = DkimKey::from_file(&path).expect("reads");
        assert_eq!(
            key.public_key_base64().expect("encodes"),
            APPENDIX_C_PUBLIC_KEY
        );
    }

    #[test]
    fn the_dns_record_is_the_published_shape() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let record = key.dns_record("brisbane");
        assert_eq!(record, format!("v=DKIM1; k=rsa; p={APPENDIX_C_PUBLIC_KEY}"));
        let parsed = DkimKeyRecord::parse(&record).expect("the record re-parses");
        assert_eq!(parsed.public_key.as_deref(), Some(APPENDIX_C_PUBLIC_KEY));
    }

    #[test]
    fn the_public_key_is_a_subject_public_key_info() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let der = B64
            .decode(key.public_key_base64().expect("encodes"))
            .expect("base64");
        let parsed = RsaPublicKey::from_public_key_der(&der).expect("the SPKI decodes");
        assert_eq!(parsed.n().bits(), 1024);
    }

    #[test]
    fn a_key_can_carry_its_domain_and_selector() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY)
            .expect("the fixture key parses")
            .with_domain("Example.COM.")
            .with_selector(" brisbane ");
        assert_eq!(key.domain(), Some("example.com"));
        assert_eq!(key.selector(), Some("brisbane"));
    }

    // ------------------------------------------------------------------
    // Key records
    // ------------------------------------------------------------------

    #[test]
    fn a_full_key_record_parses() {
        let record = DkimKeyRecord::parse(
            "v=DKIM1; k=rsa; h=sha256; s=email; t=y:s; n=hello there; p=AAAA",
        )
        .expect("parses");
        assert_eq!(record.version.as_deref(), Some("DKIM1"));
        assert_eq!(record.key_type, "rsa");
        assert_eq!(record.public_key.as_deref(), Some("AAAA"));
        assert_eq!(record.hashes, vec!["sha256"]);
        assert_eq!(record.services, vec!["email"]);
        assert_eq!(record.flags, vec!["y", "s"]);
        assert_eq!(record.notes.as_deref(), Some("hello there"));
        assert!(record.is_testing());
        assert!(record.is_strict());
    }

    #[test]
    fn a_key_record_defaults_the_optional_tags() {
        let record = DkimKeyRecord::parse("v=DKIM1; p=AAAA").expect("parses");
        assert_eq!(record.key_type, "rsa");
        assert!(record.hashes.is_empty());
        assert!(record.allows_hash("sha256"));
        assert!(record.allows_hash("sha1"));
        assert!(record.allows_service("email"));
        assert!(!record.is_testing());
        assert!(!record.is_strict());
    }

    #[test]
    fn a_p_value_split_across_character_strings_is_rejoined() {
        let record =
            DkimKeyRecord::parse("v=DKIM1; k=rsa; p=\"AAAA\" \"BBBB\" \"CCCC\"").expect("parses");
        assert_eq!(record.public_key.as_deref(), Some("AAAABBBBCCCC"));
    }

    #[test]
    fn an_empty_p_value_is_a_revoked_key() {
        let record = DkimKeyRecord::parse("v=DKIM1; k=rsa; p=").expect("parses");
        assert!(record.is_revoked());
        assert_eq!(record.public_key.as_deref(), Some(""));
    }

    #[test]
    fn a_missing_p_value_is_not_a_revocation() {
        let record = DkimKeyRecord::parse("v=DKIM1; k=rsa").expect("parses");
        assert!(!record.is_revoked());
        assert_eq!(record.public_key, None);
    }

    #[test]
    fn a_wrong_key_record_version_is_rejected() {
        assert!(DkimKeyRecord::parse("v=DKIM2; p=AAAA").is_err());
        assert!(DkimKeyRecord::parse("v=dkim1; p=AAAA").is_ok());
    }

    #[test]
    fn unknown_key_record_tags_are_ignored() {
        let record = DkimKeyRecord::parse("v=DKIM1; x=whatever; p=AAAA; zz=1").expect("parses");
        assert_eq!(record.public_key.as_deref(), Some("AAAA"));
    }

    #[test]
    fn a_hash_allow_list_is_honoured() {
        let record = DkimKeyRecord::parse("v=DKIM1; h=sha1; p=AAAA").expect("parses");
        assert!(record.allows_hash("sha1"));
        assert!(!record.allows_hash("sha256"));
    }

    #[test]
    fn a_service_allow_list_is_honoured() {
        let record = DkimKeyRecord::parse("v=DKIM1; s=other; p=AAAA").expect("parses");
        assert!(!record.allows_service("email"));
        let wildcard = DkimKeyRecord::parse("v=DKIM1; s=*; p=AAAA").expect("parses");
        assert!(wildcard.allows_service("email"));
    }

    #[test]
    fn a_key_record_is_read_from_the_first_line_only_when_it_is_one_record() {
        // The resolver hands over one string per record; chunk joining happens there.
        let record = DkimKeyRecord::parse(&format!(
            "v=DKIM1; k=rsa; p={APPENDIX_C_PUBLIC_KEY}"
        ))
        .expect("parses");
        assert_eq!(record.public_key.as_deref(), Some(APPENDIX_C_PUBLIC_KEY));
    }

    // ------------------------------------------------------------------
    // Signature parsing
    // ------------------------------------------------------------------

    fn a2_signature() -> DkimSignature {
        let message = appendix_a2_message();
        let (header_block, _) = split_message(&message);
        let fields = split_fields(header_block);
        let value = String::from_utf8_lossy(&fields[0].value).into_owned();
        DkimSignature::parse(&value).expect("the A.2 signature parses")
    }

    #[test]
    fn a_signature_exposes_its_tags() {
        let signature = a2_signature();
        assert_eq!(signature.domain(), "example.com");
        assert_eq!(signature.selector(), "brisbane");
        assert_eq!(signature.algorithm(), "rsa-sha256");
        assert_eq!(signature.body_hash(), APPENDIX_A2_BODY_HASH);
        assert_eq!(signature.signature(), APPENDIX_A2_SIGNATURE);
        assert_eq!(
            signature.headers(),
            ["Received", "From", "To", "Subject", "Date", "Message-ID"]
        );
        assert_eq!(
            signature.canonicalization(),
            (Canonicalization::Simple, Canonicalization::Simple)
        );
        assert_eq!(signature.identity(), Some("joe@football.example.com"));
        assert_eq!(signature.body_length(), None);
    }

    #[test]
    fn a_signature_knows_where_its_key_lives() {
        assert_eq!(a2_signature().key_query(), "brisbane._domainkey.example.com");
    }

    #[test]
    fn a_signature_renders_and_reparses_identically() {
        let signature = a2_signature();
        let rendered = signature.render();
        let reparsed = DkimSignature::parse(&rendered).expect("the rendering re-parses");
        assert_eq!(reparsed, signature);
        assert_eq!(reparsed.render(), rendered);
    }

    #[test]
    fn a_rendered_signature_folds_long_lines() {
        let signature = DkimSignature::parse(&format!(
            "v=1; a=rsa-sha256; c=relaxed/relaxed; d=example.com; s=selector; \
             h=From:To:Cc:Subject:Date:Message-ID:In-Reply-To:References:MIME-Version:Content-Type; \
             bh={APPENDIX_A2_BODY_HASH}; b={APPENDIX_A2_SIGNATURE}"
        ))
        .expect("parses");
        let rendered = signature.render();
        assert!(rendered.contains("\r\n "));
        for line in rendered.split("\r\n") {
            if line.len() > MAX_FOLD_COLUMN {
                // Only the unbreakable base64 tags may overrun the limit.
                assert!(line.contains("bh=") || line.contains("b="), "{line}");
            }
        }
    }

    #[test]
    fn a_signature_missing_a_required_tag_is_rejected() {
        assert!(
            DkimSignature::parse("v=1; a=rsa-sha256; d=example.com; s=x; h=From; bh=AA").is_err()
        );
        assert!(
            DkimSignature::parse("v=1; a=rsa-sha256; d=example.com; s=x; h=From; b=AA").is_err()
        );
        assert!(DkimSignature::parse("").is_err());
        assert!(DkimSignature::parse("   ").is_err());
    }

    #[test]
    fn a_signature_with_the_wrong_version_is_rejected() {
        assert!(DkimSignature::parse(
            "v=2; a=rsa-sha256; d=example.com; s=x; h=From; bh=AA; b=AA"
        )
        .is_err());
    }

    #[test]
    fn a_signature_body_length_is_read_when_present() {
        let signature = DkimSignature::parse(
            "v=1; a=rsa-sha256; d=example.com; s=x; h=From; l=1234; bh=AA; b=AA",
        )
        .expect("parses");
        assert_eq!(signature.body_length(), Some(1234));
    }

    #[test]
    fn an_unparsable_body_length_reads_as_absent() {
        let signature = DkimSignature::parse(
            "v=1; a=rsa-sha256; d=example.com; s=x; h=From; l=nope; bh=AA; b=AA",
        )
        .expect("parses");
        assert_eq!(signature.body_length(), None);
    }

    #[test]
    fn a_signature_defaults_to_simple_simple() {
        let signature = DkimSignature::parse(
            "v=1; a=rsa-sha256; d=example.com; s=x; h=From; bh=AA; b=AA",
        )
        .expect("parses");
        assert_eq!(
            signature.canonicalization(),
            (Canonicalization::Simple, Canonicalization::Simple)
        );
    }

    #[test]
    fn a_signature_strips_fws_from_its_base64_tags() {
        let signature = DkimSignature::parse(
            "v=1; a=rsa-sha256; d=example.com; s=x; h=From; bh=AA\r\n BB; b=CC\r\n DD",
        )
        .expect("parses");
        assert_eq!(signature.body_hash(), "AABB");
        assert_eq!(signature.signature(), "CCDD");
    }

    #[test]
    fn the_b_equals_value_is_deleted_from_the_raw_field() {
        let message = appendix_a2_message();
        let (header_block, _) = split_message(&message);
        let fields = split_fields(header_block);
        let emptied = strip_signature_value(&fields[0].value).expect("a b= tag exists");
        let text = String::from_utf8_lossy(&emptied);
        assert!(text.ends_with("b=;"), "{text}");
        assert!(!text.contains(APPENDIX_A2_SIGNATURE));
    }

    #[test]
    fn a_field_with_no_b_equals_tag_has_nothing_to_delete() {
        assert_eq!(strip_signature_value(b" v=1; a=rsa-sha256"), None);
    }

    // ------------------------------------------------------------------
    // Signing
    // ------------------------------------------------------------------

    #[test]
    fn a_signer_without_a_key_path_is_a_configuration_error() {
        let config = signing_config("example.com", "brisbane", "relaxed");
        assert!(config.private_key_path.is_none());
        let err = DkimSigner::new(&config).expect_err("must fail");
        assert!(matches!(err, FerromaError::Config(_)), "{err:?}");
    }

    #[test]
    fn a_signer_reads_its_key_from_the_configured_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dkim.key");
        std::fs::write(&path, APPENDIX_C_PRIVATE_KEY).expect("write");
        let mut config = signing_config("example.com", "brisbane", "relaxed/relaxed");
        config.private_key_path = Some(path);
        let signer = DkimSigner::new(&config).expect("builds");
        assert!(signer.enabled());
        assert_eq!(signer.selector(), "brisbane");
        assert_eq!(signer.domain(), Some("example.com"));
        assert_eq!(signer.key_bits(), 1024);
        assert_eq!(signer.header_canonicalization(), Canonicalization::Relaxed);
        assert_eq!(signer.body_canonicalization(), Canonicalization::Relaxed);
    }

    #[test]
    fn an_empty_selector_falls_back_to_default() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let signer = DkimSigner::from_key(key, &signing_config("example.com", "", "relaxed"))
            .expect("builds");
        assert_eq!(signer.selector(), "default");
    }

    #[test]
    fn a_key_domain_is_used_when_the_configuration_has_none() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY)
            .expect("the fixture key parses")
            .with_domain("key.example");
        let mut config = signing_config("example.com", "brisbane", "relaxed");
        config.domain = None;
        let signer = DkimSigner::from_key(key, &config).expect("builds");
        assert_eq!(signer.domain(), Some("key.example"));
    }

    #[test]
    fn a_signer_without_a_domain_cannot_sign() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let mut config = signing_config("example.com", "brisbane", "relaxed");
        config.domain = None;
        let signer = DkimSigner::from_key(key, &config).expect("builds");
        assert_eq!(signer.domain(), None);
        assert!(signer.sign(b"From: a@example.com\r\n\r\nhi\r\n").is_err());
    }

    #[test]
    fn a_message_without_from_cannot_be_signed() {
        let signer = appendix_c_signer("relaxed");
        assert!(signer.sign(b"Subject: x\r\n\r\nhi\r\n").is_err());
    }

    #[test]
    fn the_signed_header_list_is_configuration_order() {
        let signer = appendix_c_signer("relaxed");
        let message = b"From: a@example.com\r\nSubject: s\r\nTo: b@example.net\r\n\r\nhi\r\n";
        let value = signer.sign(message).expect("signs");
        let signature = DkimSignature::parse(&value).expect("parses");
        assert_eq!(signature.headers(), ["From", "To", "Subject"]);
    }

    #[test]
    fn the_emitted_header_names_the_configured_selector_and_domain() {
        let signer = appendix_c_signer("relaxed");
        let value = signer
            .sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let signature = DkimSignature::parse(&value).expect("parses");
        assert_eq!(signature.selector(), "brisbane");
        assert_eq!(signature.domain(), "example.com");
        assert_eq!(signature.algorithm(), "rsa-sha256");
        assert_eq!(
            signature.canonicalization(),
            (Canonicalization::Relaxed, Canonicalization::Relaxed)
        );
        assert!(signature.timestamp().is_some());
        assert!(signature.expiration().is_none());
        assert_eq!(signature.key_query(), "brisbane._domainkey.example.com");
    }

    #[test]
    fn every_emitted_line_stays_within_the_fold_limit() {
        let signer = appendix_c_signer("relaxed");
        let message = b"From: a@example.com\r\nTo: b@example.net\r\nSubject: a fairly long subject line for folding\r\nDate: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r\nMessage-ID: <20030712040037.46341.5F8J@football.example.com>\r\n\r\nhi\r\n";
        let value = signer.sign(message).expect("signs");
        for line in format!("DKIM-Signature: {value}").split("\r\n") {
            assert!(
                line.len() <= MAX_FOLD_COLUMN,
                "{} columns: {line}",
                line.len()
            );
        }
    }

    #[test]
    fn sign_message_prepends_the_header() {
        let signer = appendix_c_signer("relaxed");
        let message = b"From: a@example.com\r\n\r\nhi\r\n";
        let signed = signer.sign_message(message).expect("signs");
        assert!(signed.starts_with(b"DKIM-Signature: v=1;"));
        assert!(signed.ends_with(message));
        let text = String::from_utf8_lossy(&signed);
        assert_eq!(text.matches("DKIM-Signature:").count(), 1);
    }

    #[test]
    fn maybe_sign_leaves_a_disabled_signer_a_no_op() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let mut config = signing_config("example.com", "brisbane", "relaxed");
        config.enabled = false;
        let signer = DkimSigner::from_key(key, &config).expect("builds");
        assert!(!signer.enabled());
        let message = b"From: a@example.com\r\n\r\nhi\r\n";
        assert_eq!(signer.maybe_sign(message).expect("no-op"), message.to_vec());
    }

    #[test]
    fn maybe_sign_signs_when_enabled() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        assert!(signed.starts_with(b"DKIM-Signature: "));
    }

    #[test]
    fn sign_and_signature_header_agree() {
        let signer = appendix_c_signer("simple/simple");
        let message = b"From: a@example.com\r\n\r\nhi\r\n";
        let one = DkimSignature::parse(&signer.sign(message).expect("signs")).expect("parses");
        let two =
            DkimSignature::parse(&signer.signature_header(message).expect("signs")).expect("parses");
        // Only `t=` may differ between two calls.
        assert_eq!(one.headers(), two.headers());
        assert_eq!(one.body_hash(), two.body_hash());
        assert_eq!(one.signature(), two.signature());
    }

    #[test]
    fn the_body_hash_depends_on_the_body_canonicalisation() {
        let relaxed = appendix_c_signer("relaxed/relaxed");
        let simple = appendix_c_signer("simple/simple");
        let message = b"From: a@example.com\r\n\r\nhi  there \r\n";
        assert_ne!(
            relaxed.body_hash(message).expect("hashed"),
            simple.body_hash(message).expect("hashed")
        );
    }

    // ------------------------------------------------------------------
    // Verification, end to end
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_signed_message_verifies() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let signed = signer
            .maybe_sign(b"From: Joe <joe@example.com>\r\nTo: a@example.net\r\nSubject: hi\r\n\r\nbody\r\n")
            .expect("signs");
        let verdict = DkimVerifier::new(resolver_for(&signer)).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
        assert_eq!(verdict.domain.as_deref(), Some("example.com"));
        assert_eq!(verdict.selector.as_deref(), Some("brisbane"));
    }

    #[tokio::test]
    async fn a_signed_message_verifies_with_simple_canonicalisation() {
        let signer = appendix_c_signer("simple/simple");
        let signed = signer
            .maybe_sign(b"From: Joe <joe@example.com>\r\nSubject: hi\r\n\r\nbody  \r\n")
            .expect("signs");
        let verdict = DkimVerifier::new(resolver_for(&signer)).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_message_with_no_signature_is_none() {
        let verdict = DkimVerifier::new(appendix_c_resolver())
            .verify(b"From: a@example.com\r\n\r\nhi\r\n")
            .await;
        assert_eq!(verdict.result, DkimResult::None);
        assert!(verdict.domain.is_none());
    }

    #[tokio::test]
    async fn an_unknown_selector_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> = Arc::new(MockResolver::new());
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_dns_failure_is_a_temporary_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> =
            Arc::new(MockResolver::new().with_failure("brisbane._domainkey.example.com"));
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::TempError, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_revoked_key_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> = Arc::new(MockResolver::new().with_txt(
            "brisbane._domainkey.example.com",
            vec!["v=DKIM1; k=rsa; p=".to_string()],
        ));
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
        assert!(verdict.reason.as_deref().unwrap_or("").contains("revoked"));
    }

    #[tokio::test]
    async fn a_key_record_without_a_public_key_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> = Arc::new(MockResolver::new().with_txt(
            "brisbane._domainkey.example.com",
            vec!["v=DKIM1; k=rsa".to_string()],
        ));
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_non_dkim_txt_record_is_not_a_key() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> = Arc::new(
            MockResolver::new()
                .with_txt("brisbane._domainkey.example.com", vec!["v=spf1 -all".to_string()]),
        );
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_key_record_that_forbids_sha256_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> = Arc::new(MockResolver::new().with_txt(
            "brisbane._domainkey.example.com",
            vec![format!("v=DKIM1; k=rsa; h=sha1; p={APPENDIX_C_PUBLIC_KEY}")],
        ));
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_key_record_whose_p_is_not_base64_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> = Arc::new(MockResolver::new().with_txt(
            "brisbane._domainkey.example.com",
            vec!["v=DKIM1; k=rsa; p=!!!not base64!!!".to_string()],
        ));
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_non_rsa_key_type_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let resolver: Arc<dyn Resolver> = Arc::new(MockResolver::new().with_txt(
            "brisbane._domainkey.example.com",
            vec![format!("v=DKIM1; k=ed25519; p={APPENDIX_C_PUBLIC_KEY}")],
        ));
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn an_unsupported_algorithm_is_a_permanent_error() {
        let message = b"DKIM-Signature: v=1; a=rsa-sha1; d=example.com; s=brisbane;\r\n\
                        h=From; bh=AA; b=AA\r\n\
                        From: a@example.com\r\n\r\nhi\r\n";
        let verdict = DkimVerifier::new(appendix_c_resolver()).verify(message).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
        assert!(verdict.reason.as_deref().unwrap_or("").contains("algorithm"));
    }

    #[tokio::test]
    async fn an_unsupported_version_is_a_permanent_error() {
        let message = b"DKIM-Signature: v=2; a=rsa-sha256; d=example.com; s=brisbane;\r\n\
                        h=From; bh=AA; b=AA\r\n\
                        From: a@example.com\r\n\r\nhi\r\n";
        let verdict = DkimVerifier::new(appendix_c_resolver()).verify(message).await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_signature_that_does_not_cover_from_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed).replace("h=From", "h=Subject");
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
    }

    #[tokio::test]
    async fn an_identity_outside_the_signing_domain_is_a_permanent_error() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed).replacen(
            "s=brisbane;",
            "s=brisbane; i=@evil.example;",
            1,
        );
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        assert_eq!(verdict.result, DkimResult::PermError, "{verdict:?}");
        assert!(verdict.reason.as_deref().unwrap_or("").contains("identity"));
    }

    #[tokio::test]
    async fn an_identity_within_the_signing_domain_is_not_rejected() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed).replacen(
            "s=brisbane;",
            "s=brisbane; i=@mail.example.com;",
            1,
        );
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        // Adding the tag changed the header hash, so the signature now fails — the
        // point is that the i= check itself let the identity through.
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
        assert!(!verdict.reason.as_deref().unwrap_or("").contains("identity"));
    }

    #[tokio::test]
    async fn an_expired_signature_is_a_policy_result() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed).replacen("s=brisbane;", "s=brisbane; x=1;", 1);
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        assert_eq!(verdict.result, DkimResult::Policy, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_future_expiration_is_not_a_policy_result() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nhi\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed).replacen(
            "s=brisbane;",
            "s=brisbane; x=99999999999;",
            1,
        );
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_tampered_signed_header_fails() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\nSubject: hi\r\n\r\nbody\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed).replace("Subject: hi", "Subject: ho");
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
    }

    #[tokio::test]
    async fn an_unsigned_header_may_change_without_breaking_the_signature() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\nSubject: hi\r\n\r\nbody\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed)
            .replace("From: a@example.com", "From: a@example.com\r\nX-Spam: no");
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
    }

    #[tokio::test]
    async fn appending_to_a_signed_body_breaks_the_signature() {
        let signer = appendix_c_signer("relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\nbody\r\n")
            .expect("signs");
        let mut tampered = signed.clone();
        tampered.extend_from_slice(b"and more\r\n");
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(&tampered)
            .await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_message_whose_body_is_replaced_fails() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let signed = signer
            .maybe_sign(b"From: a@example.com\r\n\r\noriginal\r\n")
            .expect("signs");
        let text = String::from_utf8_lossy(&signed).replace("original", "replaced");
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(text.as_bytes())
            .await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
    }

    // ------------------------------------------------------------------
    // The `l=` tag (RFC 6376 §8.2)
    // ------------------------------------------------------------------

    /// Sign `message` with an `l=` tag folded into the signature.
    fn sign_with_body_length(signer: &DkimSigner, message: &[u8], length: &str) -> Vec<u8> {
        let value = signer
            .signature_value(message, &[format!("l={length}")])
            .expect("signs");
        prepend_signature(&value, message)
    }

    #[tokio::test]
    async fn an_l_tag_larger_than_the_body_is_a_failure() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let message = b"From: a@example.com\r\n\r\nbody\r\n";
        let signed = sign_with_body_length(&signer, message, "99999");
        let verdict = DkimVerifier::new(resolver_for(&signer)).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
        assert!(verdict.reason.as_deref().unwrap_or("").contains("larger"));
    }

    #[tokio::test]
    async fn an_l_tag_shorter_than_the_body_is_a_failure() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let message = b"From: a@example.com\r\n\r\nbody\r\n";
        let signed = sign_with_body_length(&signer, message, "2");
        let verdict = DkimVerifier::new(resolver_for(&signer)).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
        assert!(verdict.reason.as_deref().unwrap_or("").contains("unsigned"));
    }

    #[tokio::test]
    async fn an_l_tag_covering_the_whole_body_still_verifies() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let message = b"From: a@example.com\r\n\r\nbody\r\n";
        let canonical = canonicalize_body(b"body\r\n", Canonicalization::Relaxed);
        let signed = sign_with_body_length(&signer, message, &canonical.len().to_string());
        let verdict = DkimVerifier::new(resolver_for(&signer)).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
    }

    #[tokio::test]
    async fn a_non_numeric_l_tag_is_a_failure() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let message = b"From: a@example.com\r\n\r\nbody\r\n";
        let signed = sign_with_body_length(&signer, message, "banana");
        let verdict = DkimVerifier::new(resolver_for(&signer)).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
        assert!(verdict.reason.as_deref().unwrap_or("").contains("not a number"));
    }

    #[tokio::test]
    async fn an_l_tag_that_lets_an_attacker_append_content_fails() {
        // This is the attack RFC 6376 §8.2 describes: sign the body, append whatever
        // you like, and claim the signature only covered the prefix.
        let signer = appendix_c_signer("relaxed/relaxed");
        let message = b"From: a@example.com\r\n\r\nbody\r\n";
        let canonical = canonicalize_body(b"body\r\n", Canonicalization::Relaxed);
        let mut signed =
            sign_with_body_length(&signer, message, &canonical.len().to_string());
        signed.extend_from_slice(b"P.S. send bitcoin\r\n");
        let verdict = DkimVerifier::new(resolver_for(&signer)).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
    }

    // ------------------------------------------------------------------
    // Multiple signatures
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_second_signature_that_verifies_wins() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let message = b"From: a@example.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let signed = signer.maybe_sign(message).expect("signs");
        let mut with_bogus =
            b"DKIM-Signature: v=1; a=rsa-sha256; d=evil.example; s=x; h=From; bh=AA; b=AA\r\n"
                .to_vec();
        with_bogus.extend_from_slice(&signed);
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(&with_bogus)
            .await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
    }

    #[tokio::test]
    async fn when_no_signature_verifies_the_topmost_failure_is_reported() {
        let message = b"DKIM-Signature: v=1; a=rsa-sha256; d=one.example; s=x; h=From; bh=AA; b=AA\r\n\
                        DKIM-Signature: v=1; a=rsa-sha256; d=two.example; s=y; h=From; bh=AA; b=AA\r\n\
                        From: a@example.com\r\n\r\nbody\r\n";
        let verdict = DkimVerifier::new(appendix_c_resolver()).verify(message).await;
        assert_eq!(verdict.result, DkimResult::Fail, "{verdict:?}");
        assert_eq!(verdict.domain.as_deref(), Some("one.example"));
    }

    #[tokio::test]
    async fn a_signature_folded_right_after_the_colon_still_verifies() {
        let signer = appendix_c_signer("relaxed/relaxed");
        let message = b"From: a@example.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let value = signer.sign(message).expect("signs");
        // `DKIM-Signature:` on its own line, the value starting on the continuation.
        // Relaxed canonicalisation drops the leading WSP, so the hash input is the
        // same and the signature still verifies.
        let folded = format!("DKIM-Signature:\r\n {value}\r\n");
        let mut assembled = folded.into_bytes();
        assembled.extend_from_slice(message);
        let verdict = DkimVerifier::new(resolver_for(&signer))
            .verify(&assembled)
            .await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
    }

    // ------------------------------------------------------------------
    // Verdict helpers
    // ------------------------------------------------------------------

    #[test]
    fn result_tokens_match_rfc_6376() {
        assert_eq!(DkimResult::None.as_str(), "none");
        assert_eq!(DkimResult::Pass.as_str(), "pass");
        assert_eq!(DkimResult::Fail.as_str(), "fail");
        assert_eq!(DkimResult::Policy.as_str(), "policy");
        assert_eq!(DkimResult::Neutral.as_str(), "neutral");
        assert_eq!(DkimResult::TempError.as_str(), "temperror");
        assert_eq!(DkimResult::PermError.as_str(), "permerror");
        assert_eq!(DkimResult::Pass.to_string(), "pass");
    }

    #[test]
    fn verdict_helpers_describe_themselves() {
        let none = DkimVerdict::none("no signature");
        assert!(!none.is_pass());
        assert_eq!(none.result, DkimResult::None);
        assert_eq!(DkimVerdict::temp_error("dns").result, DkimResult::TempError);
        assert_eq!(DkimVerdict::perm_error("bad").result, DkimResult::PermError);
        assert!(DkimResult::Neutral.is_usable());
        assert!(!DkimResult::Fail.is_usable());
        assert!(DkimResult::Pass.is_pass());
    }

    #[test]
    fn a_key_never_prints_its_material() {
        let key = DkimKey::from_pem(APPENDIX_C_PRIVATE_KEY).expect("the fixture key parses");
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("MIICXwIBAAKBgQ"));
        assert!(rendered.contains("bits"));
    }

    #[test]
    fn a_signer_never_prints_its_key() {
        let signer = appendix_c_signer("relaxed");
        let rendered = format!("{signer:?}");
        assert!(!rendered.contains("MIICXwIBAAKBgQ"));
        assert!(rendered.contains("brisbane"));
    }

    #[tokio::test]
    async fn a_verification_asks_the_resolver_exactly_once() {
        let mock = MockResolver::new().with_txt(
            "brisbane._domainkey.example.com",
            vec![format!("v=DKIM1; k=rsa; p={APPENDIX_C_PUBLIC_KEY}")],
        );
        let resolver: Arc<dyn Resolver> = Arc::new(mock.clone());
        let verifier = DkimVerifier::new(resolver);
        assert_eq!(verifier.resolver().query_count(), 0);
        let verdict = verifier.verify(&appendix_a2_message()).await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
        assert_eq!(mock.query_count(), 1);
        assert_eq!(mock.queries(), vec!["txt:brisbane._domainkey.example.com"]);
    }

    #[test]
    fn folding_breaks_only_at_tag_boundaries() {
        let tags = vec!["a=1".to_string(), "b=2".to_string(), "c=3".to_string()];
        assert_eq!(fold_tags(&tags, 0), "a=1; b=2; c=3");
        let long = vec!["a=1".to_string(), format!("b={}", "x".repeat(70))];
        let folded = fold_tags(&long, SIGNATURE_PREFIX_LEN);
        assert!(folded.contains(";\r\n b="), "{folded}");
    }

    #[test]
    fn a_very_long_signature_wraps_without_exceeding_the_column_limit() {
        let value = append_signature("b=".to_string(), &"A".repeat(400), 0);
        for line in value.split("\r\n") {
            assert!(line.len() <= MAX_FOLD_COLUMN, "{}", line.len());
        }
    }
}
