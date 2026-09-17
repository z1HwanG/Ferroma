//! The MIME layer: `Content-Type`, transfer encodings, charsets and the
//! recursive [`MimePart`] tree.
//!
//! Everything here is *tolerant*. A mail server does not get to reject a message
//! because a sender's MUA emitted a `Content-Type` that no grammar accepts — it
//! has to carry the bytes to the mailbox and still show something sensible. So
//! `ContentType::parse` falls back to `text/plain`, unknown transfer encodings
//! become `7bit`, and unknown charsets decode as lossy UTF-8.

use std::fmt;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ferroma_core::{FerromaError, Result};

use crate::headers::Headers;

// `Content-Type`/`Content-Disposition` parameter values decoded from RFC 2231.
//
// The parameter list on `ContentType` stores exactly what the sender wrote (that
// is part of the wire format, and `Display` has to reproduce it), but
// `ContentType::param` is specified to hand back a `&str`. Percent- and
// charset-decoded values therefore live in this per-thread cache, keyed by the
// parameter name plus the raw parameters it was built from, so re-decoding the
// same header never allocates twice and stale keys are simply never read again.
thread_local! {
    static RFC2231_CACHE: std::cell::RefCell<std::collections::HashMap<String, &'static str>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Look up an already-decoded RFC 2231 parameter value.
fn rfc2231_cache(key: &str) -> Option<&'static str> {
    RFC2231_CACHE.with(|c| c.borrow().get(key).copied())
}

/// A parsed `Content-Type` field: `type/subtype; param=value; …`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentType {
    /// The primary type, lower-cased (`text`, `multipart`, …).
    pub type_: String,
    /// The subtype, lower-cased (`plain`, `alternative`, …).
    pub subtype: String,
    /// Parameters in the order they appeared, names lower-cased, values raw.
    pub params: Vec<(String, String)>,
}

impl ContentType {
    /// Parse a `Content-Type` field body.
    ///
    /// Garbage, an empty string or a missing subtype all yield
    /// [`ContentType::default`] (`text/plain; charset=us-ascii`).
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return ContentType::default();
        }

        let (head, params) = split_params(trimmed);
        let head = head.trim();
        let (type_, subtype) = match head.split_once('/') {
            Some((t, s)) => (t.trim(), s.trim()),
            None => ("text", head),
        };
        let type_ = sanitise_token(type_);
        let subtype = sanitise_token(subtype);
        if type_.is_empty() || subtype.is_empty() {
            return ContentType::default();
        }

        ContentType {
            type_,
            subtype,
            params,
        }
    }

    /// Look up a parameter by name, case-insensitively.
    ///
    /// This returns a *borrowed* value, so it serves the plain `name=value` form
    /// — plus the RFC 2231 single-segment form `name*=`, whose value it decodes on
    /// demand and memoises. Parameters split across several RFC 2231 segments are
    /// reassembled by [`ContentType::param_owned`], which is what
    /// [`ContentType::name`] uses.
    pub fn param(&self, name: &str) -> Option<&str> {
        if let Some((_, v)) = self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            return Some(v.as_str());
        }
        let lower = name.to_ascii_lowercase();
        let single = format!("{lower}*");
        let (_, raw) = self.params.iter().find(|(k, _)| *k == single)?;
        let key = format!("{single}:{}:{raw}", self.params.len());
        match rfc2231_cache(&key) {
            Some(cached) => Some(cached),
            None => {
                let decoded = decode_charset(&percent_decode(raw), "utf-8");
                let leaked: &'static str = Box::leak(decoded.into_boxed_str());
                RFC2231_CACHE.with(|c| c.borrow_mut().insert(key, leaked));
                Some(leaked)
            }
        }
    }

    /// Reassemble an RFC 2231 parameter, allocating the decoded result.
    ///
    /// Handles the extended form `name*=charset'lang'percent-encoded` and the
    /// continued form `name*0*=…; name*1*=…; name*2=…`, concatenating the
    /// segments in numeric order before percent- and charset-decoding.
    pub fn param_owned(&self, name: &str) -> Option<String> {
        let lower = name.to_ascii_lowercase();

        // Plain form, when there is no RFC 2231 form competing with it.
        let plain = self.params.iter().find(|(k, _)| *k == lower);

        let mut segments: Vec<(usize, bool, &str)> = Vec::new();
        for (k, v) in &self.params {
            if let Some(rest) = k.strip_prefix(&lower) {
                if let Some((idx, extended)) = parse_continuation_key(rest) {
                    segments.push((idx, extended.is_some(), v.as_str()));
                }
            }
        }

        if segments.is_empty() {
            return plain.map(|(_, v)| v.clone());
        }

        segments.sort_by_key(|(idx, _, _)| *idx);
        let mut charset: Option<String> = None;
        let mut bytes: Vec<u8> = Vec::new();
        for (idx, extended, value) in segments {
            if extended {
                let value = if idx == 0 {
                    // `charset'language'percent-encoded-value`
                    match value.find('\'') {
                        Some(first) => {
                            let after = &value[first + 1..];
                            let payload = match after.find('\'') {
                                Some(second) => &after[second + 1..],
                                None => after,
                            };
                            let cs = &value[..first];
                            if !cs.is_empty() {
                                charset = Some(cs.to_string());
                            }
                            payload
                        }
                        None => value,
                    }
                } else {
                    value
                };
                bytes.extend(percent_decode(value));
            } else if charset.is_none() {
                bytes.extend(value.as_bytes());
            } else {
                bytes.extend(percent_decode(value));
            }
        }
        Some(match charset {
            Some(cs) => decode_charset(&bytes, &cs),
            None => decode_charset(&bytes, "utf-8"),
        })
    }

    /// The `charset` parameter, lower-cased, when present.
    ///
    /// Parameter names are matched case-insensitively and the value is normalised
    /// to lower case, so `CHARSET=UTF-8` and `charset=utf-8` are indistinguishable
    /// to callers. Charset labels are case-insensitive (RFC 2978), and returning
    /// one canonical spelling keeps comparisons and cache keys simple.
    pub fn charset(&self) -> Option<&str> {
        let raw = self.param("charset")?;
        if raw.chars().all(|c| !c.is_ascii_uppercase()) {
            return Some(raw);
        }
        let key = format!("__charset_lower:{}:{raw}", self.params.len());
        if let Some(cached) = rfc2231_cache(&key) {
            return Some(cached);
        }
        let leaked: &'static str = Box::leak(raw.to_ascii_lowercase().into_boxed_str());
        RFC2231_CACHE.with(|c| c.borrow_mut().insert(key, leaked));
        Some(leaked)
    }

    /// The `boundary` parameter, when present.
    pub fn boundary(&self) -> Option<&str> {
        self.param("boundary")
    }

    /// The `name` parameter (RFC 2231 *and* RFC 2047 decoded — the latter because
    /// some MUAs encode `name=` as an encoded word instead), when present.
    pub fn name(&self) -> Option<&str> {
        // A plain `name=` may still be an RFC 2047 encoded word.
        if let Some((_, v)) = self.params.iter().find(|(k, _)| k == "name") {
            if !v.contains("=?") {
                return Some(v.as_str());
            }
            let key = format!("name2047:{}:{v}", self.params.len());
            if let Some(cached) = rfc2231_cache(&key) {
                return Some(cached);
            }
            let decoded =
                Box::leak(crate::headers::decode_encoded_words(v).into_boxed_str()) as &'static str;
            RFC2231_CACHE.with(|c| c.borrow_mut().insert(key, decoded));
            return Some(decoded);
        }
        let has_rfc2231 = self
            .params
            .iter()
            .any(|(k, _)| k == "name*" || k.starts_with("name*0"));
        if !has_rfc2231 {
            return None;
        }
        let key = format!(
            "name2231:{}:{}",
            self.params.len(),
            self.params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<String>()
        );
        match rfc2231_cache(&key) {
            Some(cached) => Some(cached),
            None => {
                let decoded = self.param_owned("name")?;
                let leaked: &'static str = Box::leak(decoded.into_boxed_str());
                RFC2231_CACHE.with(|c| c.borrow_mut().insert(key, leaked));
                Some(leaked)
            }
        }
    }

    /// The RFC 2047-decoded form of a parameter value, for MUAs that encode
    /// `name=`/`filename=` that way instead of using RFC 2231.
    pub fn decoded_param(&self, name: &str) -> Option<String> {
        let raw = self.param(name)?;
        if raw.contains("=?") {
            Some(crate::headers::decode_encoded_words(raw))
        } else {
            Some(raw.to_string())
        }
    }

    /// Whether this is a `multipart/*` type.
    pub fn is_multipart(&self) -> bool {
        self.type_ == "multipart"
    }

    /// Whether this is a `text/*` type.
    pub fn is_text(&self) -> bool {
        self.type_ == "text"
    }

    /// Whether this is a `message/*` type (`message/rfc822`, `message/delivery-status`).
    pub fn is_message(&self) -> bool {
        self.type_ == "message"
    }

    /// Render `type/subtype; k=v`, quoting parameter values that need it.
    fn render(&self) -> String {
        let mut out = format!("{}/{}", self.type_, self.subtype);
        for (k, v) in &self.params {
            out.push_str("; ");
            out.push_str(k);
            out.push('=');
            out.push_str(&render_param_value(v));
        }
        out
    }
}

impl fmt::Display for ContentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl Default for ContentType {
    fn default() -> Self {
        ContentType {
            type_: "text".into(),
            subtype: "plain".into(),
            params: vec![("charset".into(), "us-ascii".into())],
        }
    }
}

#[allow(clippy::inherent_to_string, clippy::inherent_to_string_shadow_display)]
impl ContentType {
    /// The rendered form, exactly as [`std::fmt::Display`] writes it.
    ///
    /// The inherent method exists because the API contract fixes that name;
    /// [`std::fmt::Display`] is implemented identically for `{}` formatting, so
    /// the shadowing the lint warns about never changes behaviour here.
    #[allow(clippy::inherent_to_string, clippy::inherent_to_string_shadow_display)]
    pub fn to_string(&self) -> String {
        self.render()
    }
}

/// `Content-Transfer-Encoding` values that Ferroma understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TransferEncoding {
    /// `7bit` — US-ASCII, short lines.
    SevenBit,
    /// `8bit` — arbitrary octets, short lines.
    EightBit,
    /// `binary` — arbitrary octets, no line-length promises.
    Binary,
    /// `base64`.
    Base64,
    /// `quoted-printable`.
    QuotedPrintable,
}

impl TransferEncoding {
    /// Parse the field body case-insensitively; anything unknown becomes `7bit`.
    pub fn parse(raw: &str) -> Self {
        let token = raw
            .trim()
            .trim_end_matches(';')
            .split([';', ' ', '\t'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match token.as_str() {
            "base64" => TransferEncoding::Base64,
            "quoted-printable" => TransferEncoding::QuotedPrintable,
            "8bit" => TransferEncoding::EightBit,
            "binary" => TransferEncoding::Binary,
            _ => TransferEncoding::SevenBit,
        }
    }

    /// The canonical spelling used in a header.
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferEncoding::SevenBit => "7bit",
            TransferEncoding::EightBit => "8bit",
            TransferEncoding::Binary => "binary",
            TransferEncoding::Base64 => "base64",
            TransferEncoding::QuotedPrintable => "quoted-printable",
        }
    }
}

// The explicit form spells out the documented default (`7bit`), which is worth
// more here than deriving it from the first variant.
#[allow(clippy::derivable_impls)]
impl Default for TransferEncoding {
    fn default() -> Self {
        TransferEncoding::SevenBit
    }
}

impl fmt::Display for TransferEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One node of a parsed MIME tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MimePart {
    /// The part's own header fields.
    pub headers: Headers,
    /// The parsed `Content-Type` (defaulted to `text/plain; charset=us-ascii`).
    pub content_type: ContentType,
    /// The parsed `Content-Transfer-Encoding` (defaulted to `7bit`).
    pub encoding: TransferEncoding,
    /// Decoded bytes for leaf parts; empty for `multipart/*` containers.
    pub content: Vec<u8>,
    /// Children of a `multipart/*` part, in document order.
    pub parts: Vec<MimePart>,
}

impl MimePart {
    /// Build a leaf part from decoded bytes and a content type.
    pub fn leaf(content_type: ContentType, content: Vec<u8>) -> Self {
        MimePart {
            headers: Headers::new(),
            content_type,
            encoding: TransferEncoding::SevenBit,
            content,
            parts: Vec::new(),
        }
    }

    /// Build a multipart container around `parts`.
    pub fn multipart(subtype: &str, boundary: &str, parts: Vec<MimePart>) -> Self {
        let content_type = ContentType {
            type_: "multipart".into(),
            subtype: subtype.to_ascii_lowercase(),
            params: vec![("boundary".into(), boundary.to_string())],
        };
        MimePart {
            headers: Headers::new(),
            content_type,
            encoding: TransferEncoding::SevenBit,
            content: Vec::new(),
            parts,
        }
    }

    /// Whether this part is a `multipart/*` container.
    pub fn is_multipart(&self) -> bool {
        self.content_type.is_multipart()
    }

    /// Whether this part carries text.
    pub fn is_text(&self) -> bool {
        self.content_type.is_text()
    }

    /// Whether this part should be presented to the user as an attachment:
    /// `Content-Disposition: attachment`, or any disposition carrying a
    /// filename, or the content type carrying a `name` parameter.
    pub fn is_attachment(&self) -> bool {
        if let Some(disposition) = self.disposition() {
            if disposition.eq_ignore_ascii_case("attachment") {
                return true;
            }
        }
        if self.disposition_filename().is_some() {
            return true;
        }
        self.content_type_has_name() && !self.is_multipart()
    }

    /// The part's filename: `Content-Disposition`'s `filename` first, then the
    /// `Content-Type`'s `name`.
    pub fn filename(&self) -> Option<String> {
        self.disposition_filename()
            .or_else(|| self.decoded_content_type_name())
    }

    /// The `Content-ID` with angle brackets stripped.
    pub fn content_id(&self) -> Option<&str> {
        let raw = self.headers.get("Content-ID")?.trim();
        let stripped = raw
            .strip_prefix('<')
            .and_then(|rest| rest.strip_suffix('>'))
            .unwrap_or(raw);
        if stripped.is_empty() {
            None
        } else {
            Some(stripped)
        }
    }

    /// The `Content-Disposition` type (`inline`, `attachment`), lower-cased.
    pub fn disposition(&self) -> Option<&str> {
        let raw = self.headers.get("Content-Disposition")?;
        let token = raw.split(';').next()?.trim();
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    }

    /// The parsed `Content-Disposition` header as a parameter list, so that
    /// RFC 2231 decoding can be reused for `filename*`.
    fn disposition_params(&self) -> ContentType {
        let raw = self.headers.get("Content-Disposition").unwrap_or("");
        let (_, params) = split_params(raw);
        ContentType {
            type_: "application".into(),
            subtype: "octet-stream".into(),
            params,
        }
    }

    /// The `filename` parameter of `Content-Disposition`, RFC 2231/2047 decoded.
    pub fn disposition_filename(&self) -> Option<String> {
        let disposition = self.disposition_params();
        let value = disposition
            .decoded_param("filename")
            .or_else(|| disposition.param_owned("filename"))?;
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    /// The `name` parameter of `Content-Type`, decoded, for filename purposes.
    fn decoded_content_type_name(&self) -> Option<String> {
        if let Some(decoded) = self.content_type.decoded_param("name") {
            return if decoded.is_empty() { None } else { Some(decoded) };
        }
        let value = self.content_type.param_owned("name")?;
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    /// Whether the `Content-Type` carries a `name` parameter at all.
    fn content_type_has_name(&self) -> bool {
        self.content_type
            .params
            .iter()
            .any(|(k, _)| k == "name" || k.starts_with("name*"))
    }

    /// Decode [`MimePart::content`] using the part's charset.
    ///
    /// Returns `None` for parts that are not `text/*`.
    pub fn decode_text(&self) -> Option<String> {
        if !self.is_text() {
            return None;
        }
        let charset = self
            .content_type
            .charset()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "us-ascii".to_string());
        Some(decode_charset(&self.content, &charset))
    }

    /// Visit this part and every descendant, parents before children.
    pub fn walk<'a>(&'a self, visit: &mut dyn FnMut(&'a MimePart)) {
        visit(self);
        for child in &self.parts {
            child.walk(visit);
        }
    }

    /// The first part (self or a descendant, parents first) matching `predicate`.
    pub fn find_first<'a>(&'a self, predicate: &dyn Fn(&MimePart) -> bool) -> Option<&'a MimePart> {
        if predicate(self) {
            return Some(self);
        }
        for child in &self.parts {
            if let Some(found) = child.find_first(predicate) {
                return Some(found);
            }
        }
        None
    }

    /// Direct children.
    pub fn subparts(&self) -> &[MimePart] {
        &self.parts
    }

    /// Total number of nodes in this subtree, including itself.
    pub fn part_count(&self) -> usize {
        1 + self.parts.iter().map(MimePart::part_count).sum::<usize>()
    }

    /// Nesting depth of this subtree: a leaf is `1`.
    pub fn depth(&self) -> usize {
        1 + self.parts.iter().map(MimePart::depth).max().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Transfer encodings
// ---------------------------------------------------------------------------

/// Decode base64 tolerantly: whitespace and characters outside the base64
/// alphabet are ignored, and missing padding is inferred.
pub fn decode_base64(input: &[u8]) -> Result<Vec<u8>> {
    let mut cleaned: Vec<u8> = Vec::with_capacity(input.len());
    for &b in input {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'=' => cleaned.push(b),
            b'-' => cleaned.push(b'+'),
            b'_' => cleaned.push(b'/'),
            _ => {}
        }
    }
    if cleaned.is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(decoded) = B64.decode(&cleaned) {
        return Ok(decoded);
    }
    // Retry with inferred padding, then with a truncation to a whole group.
    let mut padded = cleaned.clone();
    let rem = padded.len() % 4;
    if rem != 0 {
        padded.extend(std::iter::repeat_n(b'=', 4 - rem));
        if let Ok(decoded) = B64.decode(&padded) {
            return Ok(decoded);
        }
    }
    let truncated = &padded[..padded.len() - padded.len() % 4];
    B64.decode(truncated)
        .map_err(|e| FerromaError::Parse(format!("invalid base64: {e}")))
}

/// Encode bytes as base64, wrapped at 76 columns with CRLF separators and no
/// trailing line break.
pub fn encode_base64(input: &[u8]) -> String {
    let encoded = B64.encode(input);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2 + 2);
    for (i, chunk) in encoded.as_bytes().chunks(76).enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        // SAFETY-free: base64 output is always ASCII.
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    out
}

/// Decode quoted-printable: `=XX` escapes, `=` + line-break soft line breaks, and
/// trailing whitespace that a transport stripped is dropped (RFC 2045 §6.7 rule
/// 3: whitespace at the end of an encoded line is not significant).
///
/// Decoding never fails: an escape that is not valid hexadecimal is kept as a
/// literal `=`, which is what a lenient reader has to do with the mail it meets.
pub fn decode_quoted_printable(input: &[u8]) -> Result<Vec<u8>> {
    // Step 1: split into physical lines and drop their trailing whitespace.
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < input.len() {
        match input[i] {
            b'\n' => {
                lines.push(&input[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(&input[start..i]);
                i += 1;
                if i < input.len() && input[i] == b'\n' {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < input.len() {
        lines.push(&input[start..]);
    }

    let mut out = Vec::with_capacity(input.len());
    let last = lines.len().saturating_sub(1);

    // Step 2: decode each line, honouring soft breaks.
    for (index, line) in lines.iter().enumerate() {
        let mut end = line.len();
        while end > 0 && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
            end -= 1;
        }
        let line = &line[..end];

        // A soft break is `=` immediately before the line ending. The final line
        // has no ending (the framing CRLF was stripped by the parser), so a `=`
        // there is literal.
        let soft_break = index < last && line.last() == Some(&b'=');
        let body = if soft_break {
            &line[..line.len() - 1]
        } else {
            line
        };

        let mut j = 0usize;
        while j < body.len() {
            let b = body[j];
            if b == b'=' && j + 2 < body.len() {
                if let (Some(hi), Some(lo)) = (hex_val(body[j + 1]), hex_val(body[j + 2])) {
                    out.push(hi * 16 + lo);
                    j += 3;
                    continue;
                }
            }
            out.push(b);
            j += 1;
        }

        if index < last && !soft_break {
            out.push(b'\r');
            out.push(b'\n');
        }
    }

    Ok(out)
}

/// Encode bytes as quoted-printable with soft line breaks.
///
/// `=` and every control character (except TAB and the CRLF pairs of the input)
/// are escaped; a trailing space or tab before a line break is escaped too, since
/// a literal one would be stripped in transit. No line ever exceeds 76 octets,
/// counting the `=` that marks a soft break — a space that would land in the last
/// three columns is encoded as `=20` so that the break can follow it. Printable
/// ASCII except `=` passes through unchanged, so an ASCII message stays readable.
/// Line endings are always written as CRLF.
pub fn encode_quoted_printable(input: &[u8]) -> String {
    /// Hard column limit for QP lines (RFC 2045 §6.7 says at most 76 octets).
    const QP_LIMIT: usize = 76;

    let mut out = String::with_capacity(input.len() + input.len() / 4);
    let mut line_len = 0usize; // octets already on the current line

    let mut i = 0usize;
    while i < input.len() {
        let b = input[i];

        // Line breaks are written as CRLF; any of CR, LF or CRLF in the input
        // counts as one break.
        if b == b'\r' || b == b'\n' {
            let consumed = if b == b'\r' && input.get(i + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            out.push_str("\r\n");
            line_len = 0;
            i += consumed;
            continue;
        }

        let (token, literal_space) = qp_token(b, i, input);
        i += 1;

        if literal_space {
            // A soft break needs a column of its own. If a literal space would land
            // in the columns the break has to use, break *first* and let the space
            // start the next line — emitting `=20` just before the break would make
            // the decoder read it back as a real (non-framing) space.
            if line_len + token.len() + 3 > QP_LIMIT {
                out.push_str("=\r\n");
                line_len = 0;
            }
            out.push_str(&token);
            line_len += token.len();
            // A space at the very end of the input would be stripped in transit.
            if line_len + 3 > QP_LIMIT {
                out.push_str("=20");
                line_len += 3;
            }
            continue;
        }

        // Leave room for the `=` of a soft break.
        if line_len + token.len() > QP_LIMIT - 1 {
            out.push_str("=\r\n");
            line_len = 0;
        }
        out.push_str(&token);
        line_len += token.len();
    }

    out
}

/// Render one byte as a QP token.
///
/// The second element is `true` when the token is a *literal* space, which the
/// caller has to keep away from a line boundary. A space that this function
/// already encoded as `=20` reports `false`.
fn qp_token(b: u8, i: usize, input: &[u8]) -> (String, bool) {
    if matches!(b, 0x21..=0x7e) && b != b'=' {
        return ((b as char).to_string(), false);
    }
    if b == b' ' {
        let next_breaks = match input.get(i + 1) {
            None => true,
            Some(&n) => n == b'\r' || n == b'\n',
        };
        if !next_breaks {
            return (" ".to_string(), true);
        }
        return ("=20".to_string(), false);
    }
    if b == b'\t' {
        let next_breaks = match input.get(i + 1) {
            None => true,
            Some(&n) => n == b'\r' || n == b'\n',
        };
        if !next_breaks {
            return ("\t".to_string(), true);
        }
    }
    (format!("={b:02X}"), false)
}

/// Decode bytes in `charset` into a Rust `String`.
///
/// Covers the charsets a mail server actually meets — `utf-8`, `us-ascii`,
/// `iso-8859-1`, `windows-1252`, `gbk`/`gb2312`/`gb18030`, `big5`, `shift_jis`
/// and their aliases. Anything else is decoded as lossy UTF-8, and malformed
/// input never fails: it becomes U+FFFD.
pub fn decode_charset(bytes: &[u8], charset: &str) -> String {
    let label = charset
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_ascii_lowercase();
    let label = label.split([';', ' ']).next().unwrap_or("").trim();

    // Fast paths that never allocate a replacement string.
    if matches!(label, "" | "utf-8" | "utf8" | "us-ascii" | "ascii" | "ansi_x3.4-1968" | "646") {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let encoding = match label {
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "iso_8859-1" | "8859-1" | "cp819" => {
            Some(encoding_rs::WINDOWS_1252)
        }
        "windows-1252" | "cp1252" | "x-cp1252" => Some(encoding_rs::WINDOWS_1252),
        "gbk" | "gb2312" | "gb18030" | "x-gbk" | "csgb2312" | "cp936" | "ms936" => {
            Some(encoding_rs::GBK)
        }
        "big5" | "big-5" | "csbig5" | "cp950" | "x-x-big5" => Some(encoding_rs::BIG5),
        "shift_jis" | "shift-jis" | "sjis" | "x-sjis" | "cp932" | "ms_kanji" | "windows-31j" => {
            Some(encoding_rs::SHIFT_JIS)
        }
        "euc-kr" | "euckr" | "ks_c_5601-1987" | "cp949" => Some(encoding_rs::EUC_KR),
        "iso-8859-2" | "latin2" => Some(encoding_rs::ISO_8859_2),
        "iso-8859-5" => Some(encoding_rs::ISO_8859_5),
        "iso-8859-7" => Some(encoding_rs::ISO_8859_7),
        "iso-8859-15" => Some(encoding_rs::ISO_8859_15),
        "koi8-r" => Some(encoding_rs::KOI8_R),
        "windows-1251" | "cp1251" => Some(encoding_rs::WINDOWS_1251),
        "utf-16le" => Some(encoding_rs::UTF_16LE),
        "utf-16be" => Some(encoding_rs::UTF_16BE),
        _ => None,
    };

    match encoding {
        Some(enc) => enc.decode(bytes).0.into_owned(),
        None => {
            tracing::debug!(charset = charset, "unknown charset, decoding as UTF-8");
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// Internals shared with the other modules
// ---------------------------------------------------------------------------

/// Split a `Content-Type`/`Content-Disposition` body into its bare value and its
/// parameters. Parameter names are lower-cased; values keep their original case
/// with surrounding quotes removed.
pub(crate) fn split_params(raw: &str) -> (String, Vec<(String, String)>) {
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut escaped = false;

    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quote => {
                current.push(ch);
                escaped = true;
            }
            '"' => {
                in_quote = !in_quote;
                current.push(ch);
            }
            ';' if !in_quote => {
                segments.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    segments.push(current);

    let head = segments.first().cloned().unwrap_or_default();
    let mut params: Vec<(String, String)> = Vec::new();
    for segment in segments.iter().skip(1) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some(eq) = segment.find('=') else {
            continue;
        };
        let key = segment[..eq].trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let value = unquote(segment[eq + 1..].trim());
        // RFC 2231 continuations of the same parameter are merged later by
        // `param_owned`, so keep every segment as its own entry.
        params.push((key, value));
    }

    (head, params)
}

/// Remove the surrounding quotes of a parameter value and resolve `\` escapes.
pub(crate) fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escaped = false;
        for ch in inner.chars() {
            if escaped {
                out.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        raw.to_string()
    }
}

/// Quote a parameter value when RFC 2045 `tspecials` demand it.
pub(crate) fn render_param_value(value: &str) -> String {
    let needs = value.is_empty()
        || value
            .chars()
            .any(|c| "()<>@,;:\\\"/[]?=".contains(c) || c.is_control() || c == ' ');
    if !needs {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Recognise the `*0*` / `*1` / `*3*` suffix of an RFC 2231 continued parameter.
///
/// Returns the segment index and, for an extended (`*`) segment, its raw value.
fn parse_continuation_key(rest: &str) -> Option<(usize, Option<String>)> {
    if rest.is_empty() {
        return None;
    }
    if let Some(idx) = rest.strip_prefix('*') {
        // `name*` — a single, extended segment.
        let idx: usize = idx.parse().unwrap_or(0);
        return Some((idx, Some(String::new())));
    }
    let star = rest.strip_prefix('*');
    let digits = star.unwrap_or(rest);
    let (num, extended) = match digits.find('*') {
        Some(pos) => (&digits[..pos], true),
        None => (digits, false),
    };
    let idx: usize = num.parse().ok()?;
    Some((idx, if extended { Some(String::new()) } else { None }))
}

/// Lower-case a MIME token and strip anything that cannot appear in one.
fn sanitise_token(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"');
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || "!#$%&'*+-.^_`{|}~".contains(ch) {
            out.push(ch.to_ascii_lowercase());
        } else {
            break;
        }
    }
    out
}

/// Percent-decode an RFC 2231 payload.
pub(crate) fn percent_decode(raw: &str) -> Vec<u8> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Hex digit value, for `=XX` and `%XX` escapes.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_a_simple_content_type() {
        let ct = ContentType::parse("text/plain; charset=utf-8");
        assert_eq!(ct.type_, "text");
        assert_eq!(ct.subtype, "plain");
        assert_eq!(ct.charset(), Some("utf-8"));
        assert!(ct.is_text());
        assert!(!ct.is_multipart());
    }

    #[test]
    fn parses_quoted_parameters_with_semicolons() {
        let ct = ContentType::parse("multipart/mixed; boundary=\"a;b;c\"; charset=\"UTF-8\"");
        assert!(ct.is_multipart());
        assert_eq!(ct.boundary(), Some("a;b;c"));
        // Parameter names are case-insensitive.
        assert_eq!(ct.param("CHARSET"), Some("UTF-8"));
    }

    #[test]
    fn falls_back_to_text_plain_for_garbage() {
        assert_eq!(ContentType::parse(""), ContentType::default());
        assert_eq!(ContentType::parse("   "), ContentType::default());
        for garbage in ["///", "text/", "  /  ", "\"\"\"", "; charset=x"] {
            let ct = ContentType::parse(garbage);
            assert_eq!(ct.type_, "text", "input {garbage:?}");
            assert!(!ct.subtype.is_empty());
        }
    }

    #[test]
    fn accepts_the_obsolete_x_type_shorthand() {
        // `Content-Type: text` (no slash) is seen from ancient MUAs.
        let ct = ContentType::parse("text");
        assert_eq!(ct.type_, "text");
        assert_eq!(ct.subtype, "text");
    }

    #[test]
    fn normalises_case_and_padding() {
        let ct = ContentType::parse("  TEXT / HTML ;  CHARSET = UTF-8  ");
        assert_eq!(ct.type_, "text");
        assert_eq!(ct.subtype, "html");
        assert_eq!(ct.charset(), Some("utf-8"));
        // The stored parameter keeps the sender's spelling; `charset()` normalises.
        assert_eq!(ct.param("charset"), Some("UTF-8"));
    }

    #[test]
    fn renders_back_to_a_header_value() {
        let ct = ContentType::parse("multipart/mixed; boundary=abc");
        assert_eq!(ct.to_string(), "multipart/mixed; boundary=abc");
        assert_eq!(format!("{ct}"), "multipart/mixed; boundary=abc");

        let quoting = ContentType {
            type_: "application".into(),
            subtype: "octet-stream".into(),
            params: vec![("name".into(), "my file.txt".into())],
        };
        assert_eq!(quoting.to_string(), "application/octet-stream; name=\"my file.txt\"");
        assert_eq!(ContentType::parse(&quoting.to_string()), quoting);
    }

    #[test]
    fn default_is_plain_ascii_text() {
        let d = ContentType::default();
        assert_eq!(d.type_, "text");
        assert_eq!(d.subtype, "plain");
        assert_eq!(d.charset(), Some("us-ascii"));
    }

    #[test]
    fn decodes_rfc2231_encoded_and_continued_parameters() {
        let ct = ContentType::parse(
            "application/pdf; name*0*=utf-8''%E4%B8%AD%E6%96%87; name*1*=%2Etxt",
        );
        assert_eq!(ct.param_owned("name").as_deref(), Some("中文.txt"));

        let ct = ContentType::parse("text/plain; name*=\"utf-8''na%C3%AFve.txt\"");
        assert_eq!(ct.name(), Some("naïve.txt"));
    }

    #[test]
    fn decodes_rfc2047_parameter_values() {
        // Some MUAs RFC 2047-encode `name=` instead of using RFC 2231.
        let ct = ContentType::parse("application/pdf; name=\"=?UTF-8?B?5paH5Lu2LnBkZg==?=\"");
        assert_eq!(ct.name(), Some("文件.pdf"));
    }

    #[test]
    fn transfer_encoding_parsing_is_case_insensitive() {
        assert_eq!(TransferEncoding::parse("BASE64"), TransferEncoding::Base64);
        assert_eq!(
            TransferEncoding::parse("quoted-printable"),
            TransferEncoding::QuotedPrintable
        );
        assert_eq!(TransferEncoding::parse(" 8bit "), TransferEncoding::EightBit);
        assert_eq!(TransferEncoding::parse("binary"), TransferEncoding::Binary);
        assert_eq!(TransferEncoding::parse("7bit"), TransferEncoding::SevenBit);
        assert_eq!(TransferEncoding::parse("x-uuencode"), TransferEncoding::SevenBit);
        assert_eq!(TransferEncoding::parse(""), TransferEncoding::SevenBit);
        assert_eq!(TransferEncoding::default(), TransferEncoding::SevenBit);
        assert_eq!(TransferEncoding::QuotedPrintable.as_str(), "quoted-printable");
        assert_eq!(TransferEncoding::Base64.to_string(), "base64");
    }

    #[test]
    fn base64_round_trip_with_wrapping() {
        let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let encoded = encode_base64(&data);
        assert!(encoded.contains("\r\n"));
        for line in encoded.split("\r\n") {
            assert!(line.len() <= 76);
        }
        assert_eq!(decode_base64(encoded.as_bytes()).unwrap(), data);
    }

    #[test]
    fn base64_decoding_ignores_whitespace_and_junk() {
        let cleaned = decode_base64(b"SGVs bG8s\r\nIFdv cmxk\n").unwrap();
        assert_eq!(String::from_utf8(cleaned).unwrap(), "Hello, World");
        assert_eq!(decode_base64(b"").unwrap(), Vec::<u8>::new());
        assert_eq!(decode_base64(b"\r\n\r\n").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn base64_decoding_tolerates_missing_padding() {
        assert_eq!(decode_base64(b"SGVsbG8").unwrap(), b"Hello");
        assert_eq!(decode_base64(b"SGVsbG8=").unwrap(), b"Hello");
    }

    #[test]
    fn quoted_printable_round_trip() {
        let text = "Hello, World! This is a fairly long line of plain ASCII text that should be wrapped by the encoder.\r\nSecond line with 中文 and = signs.";
        let encoded = encode_quoted_printable(text.as_bytes());
        for line in encoded.split("\r\n") {
            assert!(line.len() <= 76, "line too long ({}): {line}", line.len());
        }
        let decoded = decode_quoted_printable(encoded.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), text);
    }

    #[test]
    fn quoted_printable_lines_never_exceed_76_octets() {
        // Squeeze the encoder from every direction: spaces at every column, a
        // long run of ASCII, and multi-byte text, all of which change the token
        // width and therefore where the soft breaks land.
        for pad in 0..80usize {
            let text = format!("{}tail {}", "x".repeat(pad), "y".repeat(200));
            let encoded = encode_quoted_printable(text.as_bytes());
            for line in encoded.split("\r\n") {
                assert!(
                    line.len() <= 76,
                    "pad {pad}: line of {} octets: {line:?}",
                    line.len()
                );
            }
            let decoded = decode_quoted_printable(encoded.as_bytes()).unwrap();
            assert_eq!(String::from_utf8(decoded).unwrap(), text, "pad {pad}");
        }
    }
    #[test]
    fn quoted_printable_escapes_equals_and_controls() {
        let encoded = encode_quoted_printable(b"a=b\tc\x01d");
        assert_eq!(encoded, "a=3Db\tc=01d");
        assert_eq!(decode_quoted_printable(encoded.as_bytes()).unwrap(), b"a=b\tc\x01d");
    }

    #[test]
    fn quoted_printable_soft_line_breaks() {
        let decoded = decode_quoted_printable(b"Hello =\r\nWorld").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "Hello World");

        let decoded = decode_quoted_printable(b"Hello =\nWorld").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "Hello World");
    }

    #[test]
    fn quoted_printable_decodes_utf8_sequences() {
        let input = b"=E4=BD=A0=E5=A5=BD";
        let decoded = decode_quoted_printable(input).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "你好");
    }

    #[test]
    fn quoted_printable_trailing_space_is_encoded() {
        let encoded = encode_quoted_printable(b"trailing \r\nnext");
        assert!(encoded.starts_with("trailing=20"), "got {encoded}");
        assert_eq!(
            decode_quoted_printable(encoded.as_bytes()).unwrap(),
            b"trailing \r\nnext"
        );
    }

    #[test]
    fn quoted_printable_keeps_lone_equals() {
        let decoded = decode_quoted_printable(b"a=zb").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "a=zb");
        let decoded = decode_quoted_printable(b"trailing=").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "trailing=");
    }

    #[test]
    fn charset_decoding_covers_common_encodings() {
        assert_eq!(decode_charset("héllo".as_bytes(), "utf-8"), "héllo");
        assert_eq!(decode_charset(b"caf\xe9", "iso-8859-1"), "café");
        assert_eq!(decode_charset(b"caf\xe9", "windows-1252"), "café");
        // GBK for 中文
        assert_eq!(decode_charset(b"\xd6\xd0\xce\xc4", "gbk"), "中文");
        assert_eq!(decode_charset(b"\xd6\xd0\xce\xc4", "GB2312"), "中文");
        assert_eq!(decode_charset(b"\xd6\xd0\xce\xc4", "gb18030"), "中文");
        // Big5 for 中文
        assert_eq!(decode_charset(b"\xa4\xa4\xa4\xe5", "big5"), "中文");
        // Shift_JIS for こんにちは
        assert_eq!(
            decode_charset(b"\x82\xb1\x82\xf1\x82\xc9\x82\xbf\x82\xcd", "shift_jis"),
            "こんにちは"
        );
        // Unknown charsets fall back to lossy UTF-8 rather than failing.
        assert_eq!(decode_charset(b"caf\xc3\xa9", "x-nonsense"), "café");
        assert_eq!(decode_charset(b"\xff\xfe", "utf-8"), "\u{fffd}\u{fffd}");
        assert_eq!(decode_charset(b"", "utf-8"), "");
        assert_eq!(decode_charset(b"plain", "us-ascii"), "plain");
    }

    #[test]
    fn part_predicates() {
        let mut part = MimePart::leaf(ContentType::parse("text/plain; charset=utf-8"), b"hi".to_vec());
        assert!(part.is_text());
        assert!(!part.is_multipart());
        assert!(!part.is_attachment());

        part.headers.append("Content-Disposition", "attachment; filename=\"a.txt\"");
        assert!(part.is_attachment());
        assert_eq!(part.filename().as_deref(), Some("a.txt"));
        assert_eq!(part.disposition(), Some("attachment"));

        let mut inline = MimePart::leaf(ContentType::parse("image/png; name=\"pic.png\""), vec![]);
        inline.headers.append("Content-Disposition", "inline");
        assert!(inline.is_attachment(), "an inline part with a name is an attachment");
        assert_eq!(inline.filename().as_deref(), Some("pic.png"));

        let mut bare = MimePart::leaf(ContentType::parse("image/png"), vec![]);
        bare.headers.append("Content-Disposition", "inline");
        assert!(!bare.is_attachment());
    }

    #[test]
    fn content_id_strips_angle_brackets() {
        let mut part = MimePart::leaf(ContentType::parse("image/png"), vec![]);
        part.headers.append("Content-ID", "<image001@01D2.abc>");
        assert_eq!(part.content_id(), Some("image001@01D2.abc"));
        part.headers.insert("Content-ID", "  plain@id  ");
        assert_eq!(part.content_id(), Some("plain@id"));
        part.headers.insert("Content-ID", "<>");
        assert_eq!(part.content_id(), None);
    }

    #[test]
    fn decode_text_is_charset_aware() {
        let mut part = MimePart::leaf(ContentType::parse("text/plain; charset=gbk"), b"\xd6\xd0\xce\xc4".to_vec());
        part.encoding = TransferEncoding::SevenBit;
        assert_eq!(part.decode_text().as_deref(), Some("中文"));

        let binary = MimePart::leaf(ContentType::parse("application/octet-stream"), vec![]);
        assert_eq!(binary.decode_text(), None);
    }

    #[test]
    fn walk_and_find_first_are_pre_order() {
        let inner = MimePart::multipart(
            "alternative",
            "b2",
            vec![
                MimePart::leaf(ContentType::parse("text/plain"), b"plain".to_vec()),
                MimePart::leaf(ContentType::parse("text/html"), b"<p>html</p>".to_vec()),
            ],
        );
        let root = MimePart::multipart(
            "mixed",
            "b1",
            vec![inner, MimePart::leaf(ContentType::parse("application/pdf"), vec![1, 2, 3])],
        );

        let mut visited = Vec::new();
        root.walk(&mut |p| visited.push(p.content_type.to_string()));
        assert_eq!(
            visited,
            vec![
                "multipart/mixed; boundary=b1",
                "multipart/alternative; boundary=b2",
                "text/plain",
                "text/html",
                "application/pdf",
            ]
        );

        let html = root
            .find_first(&|p| p.content_type.subtype == "html")
            .expect("html part");
        assert_eq!(html.content, b"<p>html</p>");
        assert!(root.find_first(&|p| p.content_type.subtype == "zip").is_none());
        assert_eq!(root.subparts().len(), 2);
        assert_eq!(root.part_count(), 5);
        assert_eq!(root.depth(), 3);
    }

    #[test]
    fn mime_part_json_round_trip() {
        let part = MimePart::multipart(
            "alternative",
            "b",
            vec![MimePart::leaf(ContentType::parse("text/plain"), b"hi".to_vec())],
        );
        let json = serde_json::to_string(&part).unwrap();
        let back: MimePart = serde_json::from_str(&json).unwrap();
        assert_eq!(part, back);
    }

    #[test]
    fn content_type_json_round_trip() {
        let ct = ContentType::parse("multipart/mixed; boundary=xyz");
        let json = serde_json::to_string(&ct).unwrap();
        let back: ContentType = serde_json::from_str(&json).unwrap();
        assert_eq!(ct, back);
    }

    #[test]
    fn content_type_with_no_parameters() {
        let ct = ContentType::parse("application/json");
        assert!(ct.params.is_empty());
        assert_eq!(ct.to_string(), "application/json");
        assert_eq!(ct.param("charset"), None);
        assert_eq!(ct.boundary(), None);
        assert_eq!(ct.name(), None);
    }

    #[test]
    fn parameter_without_a_value_is_dropped() {
        let ct = ContentType::parse("text/plain; charset");
        assert_eq!(ct.param("charset"), None);
        assert_eq!(ct.to_string(), "text/plain");
    }

    #[test]
    fn empty_parameter_value_is_kept_and_quoted_on_output() {
        let ct = ContentType::parse("text/plain; charset=");
        assert_eq!(ct.charset(), Some(""));
        assert_eq!(ct.to_string(), "text/plain; charset=\"\"");
    }

    #[test]
    fn quoted_parameter_value_with_escapes_round_trips() {
        let ct = ContentType::parse("application/octet-stream; name=\"a\\\"b.txt\"");
        assert_eq!(ct.param("name"), Some("a\"b.txt"));
        let rendered = ct.to_string();
        assert_eq!(ContentType::parse(&rendered), ct);
    }

    #[test]
    fn charset_lookup_is_case_insensitive_and_ignores_quotes() {
        for raw in [
            "text/plain; charset=utf-8",
            "text/plain; charset=\"utf-8\"",
            "text/plain; CHARSET=UTF-8",
            "text/plain;charset=utf-8",
        ] {
            let ct = ContentType::parse(raw);
            assert_eq!(ct.charset(), Some("utf-8"), "input {raw}");
        }
        // The value is normalised too, and the decoder accepts any casing.
        assert_eq!(
            decode_charset("héllo".as_bytes(), ContentType::parse("text/plain; charset=UTF-8").charset().unwrap()),
            "héllo"
        );
    }

    #[test]
    fn message_type_predicates() {
        assert!(ContentType::parse("message/rfc822").is_message());
        assert!(ContentType::parse("message/delivery-status").is_message());
        assert!(!ContentType::parse("text/plain").is_message());
    }

    #[test]
    fn base64_of_an_empty_payload() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(decode_base64(b"").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn base64_rejects_nothing_but_never_panics() {
        // Garbage in, empty or prefix out — but never an unwrap/panic.
        let _ = decode_base64(b"!!!!");
        let _ = decode_base64(b"====");
        let _ = decode_base64(b"\xff\xfe\xfd");
        assert!(decode_base64(b"").is_ok());
    }

    #[test]
    fn quoted_printable_leaves_a_clean_ascii_line_alone() {
        assert_eq!(encode_quoted_printable(b"Hello World"), "Hello World");
        assert_eq!(encode_quoted_printable(b""), "");
    }

    #[test]
    fn quoted_printable_decodes_lowercase_hex_escapes() {
        assert_eq!(
            decode_quoted_printable(b"caf=c3=a9").unwrap(),
            "café".as_bytes()
        );
    }

    #[test]
    fn quoted_printable_ignores_a_bare_carriage_return_as_a_break() {
        assert_eq!(decode_quoted_printable(b"a=\rb").unwrap(), b"ab");
    }

    #[test]
    fn decode_charset_accepts_aliases_and_trims_quotes() {
        assert_eq!(decode_charset(b"caf\xe9", "\"ISO-8859-1\""), "café");
        assert_eq!(decode_charset(b"caf\xe9", " latin1 "), "café");
        assert_eq!(decode_charset(b"\xd6\xd0\xce\xc4", "GB18030"), "中文");
        assert_eq!(decode_charset(b"\xd6\xd0\xce\xc4", "cp936"), "中文");
        assert_eq!(decode_charset(b"\xa4\xa4\xa4\xe5", "cp950"), "中文");
    }

    #[test]
    fn walk_visits_every_node_including_an_empty_tree() {
        let leaf = MimePart::leaf(ContentType::parse("text/plain"), b"x".to_vec());
        let mut count = 0;
        leaf.walk(&mut |_| count += 1);
        assert_eq!(count, 1);

        let empty = MimePart::multipart("mixed", "b", Vec::new());
        assert_eq!(empty.part_count(), 1);
        assert_eq!(empty.depth(), 1);
        assert!(empty.subparts().is_empty());
    }

    #[test]
    fn leaf_and_multipart_constructors_set_sensible_headers() {
        let leaf = MimePart::leaf(ContentType::parse("text/plain"), b"hi".to_vec());
        assert_eq!(leaf.encoding, TransferEncoding::SevenBit);
        assert!(leaf.parts.is_empty());
        assert!(leaf.headers.is_empty());

        let multi = MimePart::multipart("Alternative", "BOUND", vec![leaf]);
        assert_eq!(multi.content_type.subtype, "alternative");
        assert_eq!(multi.content_type.boundary(), Some("BOUND"));
        assert!(multi.content.is_empty());
        assert_eq!(multi.subparts().len(), 1);
        assert!(multi.is_multipart());
    }

    #[test]
    fn find_first_returns_the_node_itself_when_it_matches() {
        let part = MimePart::leaf(ContentType::parse("text/plain"), b"hi".to_vec());
        assert!(part.find_first(&|p| p.is_text()).is_some());
    }

    #[test]
    fn filename_prefers_content_disposition_over_content_type() {
        let mut part = MimePart::leaf(
            ContentType::parse("application/pdf; name=\"type-name.pdf\""),
            vec![],
        );
        assert_eq!(part.filename().as_deref(), Some("type-name.pdf"));
        part.headers
            .append("Content-Disposition", "attachment; filename=\"disp-name.pdf\"");
        assert_eq!(part.filename().as_deref(), Some("disp-name.pdf"));
    }

    #[test]
    fn filename_understands_rfc2231_continuations() {
        let mut part = MimePart::leaf(ContentType::parse("application/pdf"), vec![]);
        part.headers.append(
            "Content-Disposition",
            "attachment; filename*0*=utf-8''%E6%8A%A5%E5%91%8A; filename*1*=.pdf",
        );
        assert_eq!(part.filename().as_deref(), Some("报告.pdf"));
    }

    #[test]
    fn part_without_a_disposition_is_not_an_attachment() {
        let mut part = MimePart::leaf(ContentType::parse("text/plain"), b"hi".to_vec());
        assert_eq!(part.disposition(), None);
        assert_eq!(part.disposition_filename(), None);
        assert!(!part.is_attachment());

        part.headers.append("Content-Disposition", "inline");
        assert_eq!(part.disposition(), Some("inline"));
        assert!(!part.is_attachment());
    }

    #[test]
    fn multipart_parts_are_never_attachments() {
        let multi = MimePart::multipart("mixed", "b", Vec::new());
        assert!(!multi.is_attachment());
    }
}
