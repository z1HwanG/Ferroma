//! A raw-bytes MIME view of a stored message, used by `FETCH`.
//!
//! `ferroma-mail`'s [`ferroma_mail::MimePart`] tree is *decoded*: transfer
//! encodings are applied and charsets resolved. That is exactly right for
//! rendering a message in Webmail, and exactly wrong for IMAP, where:
//!
//! * `BODY[1.2]` must return the **encoded** octets of a part, including its
//!   MIME headers, byte for byte as they are on disk;
//! * `BODYSTRUCTURE` reports the **encoded** size of each part, and clients
//!   compare that against the number of octets they receive;
//! * `RFC822.SIZE` is the octet count of the file.
//!
//! So FETCH works from this module instead: a tree whose nodes are *byte ranges*
//! of the original message. Nothing is copied except the parts a client actually
//! asks for, and no range can ever run past the end of the file.

use ferroma_mail::headers::{parse_date, Headers};
use ferroma_mail::mime::{decode_base64, decode_quoted_printable, TransferEncoding};

/// Maximum nesting depth for the structural walk. A message deeper than this is
/// treated as a leaf: IMAP must never fail a `FETCH` because of a hostile
/// `Content-Type` chain.
pub const MAX_STRUCTURE_DEPTH: usize = 32;

/// Maximum number of parts in one message's structural tree.
pub const MAX_STRUCTURE_PARTS: usize = 1024;

/// A known byte range of the original message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// First byte.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
}

impl Span {
    /// A span from `start` to `end`.
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// The number of bytes in the span.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes of the span within `raw`, clamped to the buffer.
    ///
    /// Clamping is what guarantees a `BODY[...]` request can never read past the
    /// file even if the structure and the buffer disagree.
    pub fn slice<'a>(&self, raw: &'a [u8]) -> &'a [u8] {
        let start = self.start.min(raw.len());
        let end = self.end.min(raw.len()).max(start);
        &raw[start..end]
    }
}

/// One MIME entity: its own header block and its body (or its children).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPart {
    /// The entity's own header block, CRLF-terminated, without the blank line.
    pub header: Span,
    /// The entity's body: everything after the blank line up to where the next
    /// boundary starts.
    pub body: Span,
    /// The parsed header fields.
    pub headers: Headers,
    /// `Content-Type`, parsed leniently (a missing one is `text/plain`).
    pub content_type: ContentTypeView,
    /// `Content-Transfer-Encoding`, parsed leniently.
    pub encoding: TransferEncoding,
    /// Child parts, for a `multipart/*` entity.
    pub parts: Vec<RawPart>,
    /// The encapsulated message of a `message/rfc822` part, if it has one.
    ///
    /// IMAP asks for the *envelope* and *body structure* of an encapsulated
    /// message, so the nested entity is parsed here rather than on every fetch.
    pub message: Option<Box<RawPart>>,
    /// A private buffer, set only for a nested message that had to be decoded
    /// out of its transfer encoding before it could be parsed.
    ///
    /// A nested node's [`Span`]s are meaningless without the buffer they index,
    /// so an encoded `message/rfc822` carries its own copy here and
    /// [`RawPart::bytes`] resolves against it.
    pub owned: Option<std::sync::Arc<[u8]>>,
    /// Whether the walk stopped descending here because of a limit.
    pub truncated: bool,
}

impl RawPart {
    /// Whether this entity is `multipart/*`.
    pub fn is_multipart(&self) -> bool {
        self.content_type.type_.eq_ignore_ascii_case("multipart")
    }

    /// Whether this entity is `text/*`.
    pub fn is_text(&self) -> bool {
        self.content_type.type_.eq_ignore_ascii_case("text")
    }

    /// Whether this entity is `message/rfc822` (or `message/global`).
    pub fn is_message(&self) -> bool {
        is_message_type(&self.content_type)
    }

    /// The `Content-ID` without angle brackets.
    pub fn content_id(&self) -> Option<String> {
        self.headers.get("Content-ID").map(|raw| {
            raw.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    }

    /// The `Content-Description`.
    pub fn description(&self) -> Option<&str> {
        self.headers.get("Content-Description")
    }

    /// The file name, from `Content-Disposition` or `Content-Type`'s `name`.
    pub fn filename(&self) -> Option<String> {
        if let Some(raw) = self.headers.get("Content-Disposition") {
            if let Some(name) = parameter_of(raw, "filename") {
                return Some(name);
            }
        }
        self.content_type.param("name").map(str::to_string)
    }

    /// The `Content-Disposition` value: `(type, params)`.
    pub fn disposition(&self) -> Option<(String, Vec<(String, String)>)> {
        let raw = self.headers.get("Content-Disposition")?;
        let (type_, params) = split_type_and_params(raw);
        Some((type_.to_ascii_lowercase(), params))
    }

    /// Every `Content-Language` tag.
    pub fn languages(&self) -> Vec<String> {
        self.headers
            .get_all("Content-Language")
            .into_iter()
            .flat_map(|raw| {
                raw.split(',')
                    .map(|tag| tag.trim().to_string())
                    .filter(|tag| !tag.is_empty())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// `Content-Location`.
    pub fn location(&self) -> Option<&str> {
        self.headers.get("Content-Location").map(str::trim)
    }

    /// The number of lines the decoded content has.
    ///
    /// For a leaf, the octet count of the *encoded* body (what the client will
    /// receive); for a `message/rfc822` part, its encapsulated structure. This is
    /// what RFC 3501 §7.4.2 calls `size` and `lines`.
    pub fn content_lines(&self, raw: &[u8]) -> usize {
        let body = self.body.slice(raw);
        let decoded = decode_body(body, self.encoding);
        count_lines(&decoded)
    }

    /// The bytes this node's [`Span`]s index into: the node's own buffer when it
    /// has one, otherwise the whole message.
    pub fn bytes<'a>(&'a self, raw: &'a [u8]) -> &'a [u8] {
        match &self.owned {
            Some(buffer) => buffer.as_ref(),
            None => raw,
        }
    }

    /// This node's own header block, as raw bytes.
    pub fn header_bytes<'a>(&'a self, raw: &'a [u8]) -> &'a [u8] {
        let buffer = self.bytes(raw);
        self.header.slice(buffer)
    }

    /// This node's body, as raw bytes.
    pub fn body_bytes<'a>(&'a self, raw: &'a [u8]) -> &'a [u8] {
        let buffer = self.bytes(raw);
        self.body.slice(buffer)
    }
}

/// Count the lines of a byte run: the number of line terminators, plus one when
/// the run does not end with one and is not empty.
pub fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut lines = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines += 1;
                index += 1;
            }
            b'\r' => {
                lines += 1;
                index += if bytes.get(index + 1) == Some(&b'\n') { 2 } else { 1 };
            }
            _ => index += 1,
        }
    }
    if !matches!(bytes.last(), Some(b'\n') | Some(b'\r')) {
        lines += 1;
    }
    lines
}

/// Apply a transfer encoding to a body run for the purpose of counting lines.
fn decode_body(body: &[u8], encoding: TransferEncoding) -> Vec<u8> {
    match encoding {
        TransferEncoding::Base64 => decode_base64(body).unwrap_or_else(|_| body.to_vec()),
        TransferEncoding::QuotedPrintable => {
            decode_quoted_printable(body).unwrap_or_else(|_| body.to_vec())
        }
        _ => body.to_vec(),
    }
}

/// A lenient `Content-Type` view: type, subtype and raw parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentTypeView {
    /// The primary type, lower-cased (`text`).
    pub type_: String,
    /// The subtype, lower-cased (`plain`).
    pub subtype: String,
    /// Parameters in the order they were written, lower-cased names.
    pub params: Vec<(String, String)>,
}

impl Default for ContentTypeView {
    fn default() -> Self {
        ContentTypeView {
            type_: "text".to_string(),
            subtype: "plain".to_string(),
            params: Vec::new(),
        }
    }
}

impl ContentTypeView {
    /// Parse a `Content-Type` value.
    pub fn parse(raw: &str) -> Self {
        let (type_, params) = split_type_and_params(raw);
        let (primary, subtype) = match type_.split_once('/') {
            Some((primary, subtype)) => (primary, subtype),
            None => {
                if type_.is_empty() {
                    ("text", "plain")
                } else {
                    // `Content-Type: text` — treat the lone token as the type.
                    (type_.as_str(), "")
                }
            }
        };
        ContentTypeView {
            type_: primary.trim().to_ascii_lowercase(),
            subtype: subtype.trim().to_ascii_lowercase(),
            params,
        }
    }

    /// A parameter value, case-insensitively named.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The `boundary` parameter.
    pub fn boundary(&self) -> Option<&str> {
        self.param("boundary")
    }

    /// Whether the type is `multipart`.
    pub fn is_multipart(&self) -> bool {
        self.type_.eq_ignore_ascii_case("multipart")
    }
}

/// Split a header value into its leading token and its parameters.
fn split_type_and_params(raw: &str) -> (String, Vec<(String, String)>) {
    let mut params = Vec::new();
    // The type token ends at the first `;` (or at the end).
    let (type_, rest) = match raw.split_once(';') {
        Some((type_, rest)) => (type_.trim().to_string(), rest),
        None => (raw.trim().to_string(), ""),
    };

    let bytes = rest.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        // Skip separators and whitespace.
        while index < bytes.len() && (bytes[index] == b';' || bytes[index].is_ascii_whitespace()) {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name_start = index;
        while index < bytes.len() && bytes[index] != b'=' && bytes[index] != b';' {
            index += 1;
        }
        let name = rest[name_start..index].trim().to_ascii_lowercase();
        if index >= bytes.len() || bytes[index] != b'=' {
            if name.is_empty() {
                continue;
            }
            params.push((name, String::new()));
            continue;
        }
        index += 1; // past `=`
        let value = if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            let mut out = String::new();
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' if index + 1 < bytes.len() => {
                        out.push(bytes[index + 1] as char);
                        index += 2;
                    }
                    b'"' => {
                        index += 1;
                        break;
                    }
                    byte => {
                        out.push(byte as char);
                        index += 1;
                    }
                }
            }
            out
        } else {
            let value_start = index;
            while index < bytes.len() && bytes[index] != b';' {
                index += 1;
            }
            rest[value_start..index].trim().to_string()
        };
        if !name.is_empty() {
            params.push((name, value));
        }
    }
    (type_, params)
}

/// One parameter of a header value, e.g. `filename` of `Content-Disposition`.
fn parameter_of(raw: &str, name: &str) -> Option<String> {
    let (_, params) = split_type_and_params(raw);
    params
        .into_iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

/// A parsed message: the raw bytes plus the structural tree over them.
#[derive(Debug, Clone)]
pub struct RawMessage {
    raw: Vec<u8>,
    root: RawPart,
}

impl RawMessage {
    /// Parse a stored message.
    ///
    /// Parsing is total: any byte string produces a tree. Only the structural
    /// limits can stop the walk, and they degrade to a leaf rather than erroring.
    pub fn parse(raw: Vec<u8>) -> Self {
        let whole = Span::new(0, raw.len());
        let (header, body) = split_header_body(&raw, whole);
        let headers = parse_headers(header.slice(&raw));
        let content_type = content_type_of(&headers);
        let encoding = encoding_of(&headers);
        let mut budget = MAX_STRUCTURE_PARTS;
        let is_multipart = content_type.is_multipart();
        let parts = if is_multipart {
            content_type
                .boundary()
                .map(|boundary| {
                    split_multipart(raw.as_slice(), body, boundary, 1, &mut budget)
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // A whole message that is itself a `message/rfc822` still has to report
        // the encapsulated message's envelope and structure.
        let message = if !is_multipart && is_message_type(&content_type) && budget > 0 {
            budget -= 1;
            Some(Box::new(parse_nested(raw.as_slice(), body, encoding, 1, &mut budget)))
        } else {
            None
        };
        let root = RawPart {
            header,
            body,
            headers,
            content_type,
            encoding,
            parts,
            message,
            owned: None,
            truncated: false,
        };
        RawMessage { raw, root }
    }

    /// The whole message, byte for byte.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// The top-level entity.
    pub fn root(&self) -> &RawPart {
        &self.root
    }

    /// The top-level header block, including its trailing blank line.
    ///
    /// RFC 3501 guarantees `BODY[HEADER]` ends with the blank line that separates
    /// the headers from the body, so clients can concatenate it with
    /// `BODY[TEXT]`.
    pub fn header_with_blank_line(&self) -> &[u8] {
        let start = self.root.header.start;
        let end = self.root.body.start;
        Span::new(start, end).slice(&self.raw)
    }

    /// The top-level body.
    pub fn body(&self) -> &[u8] {
        self.root.body.slice(&self.raw)
    }

    /// The whole message length.
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Whether the message is empty.
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Resolve a numeric part path (`1.2.3`) to a node.
    pub fn part(&self, path: &[u32]) -> Option<&RawPart> {
        let mut current = &self.root;
        for (depth, index) in path.iter().enumerate() {
            if *index == 0 {
                return None;
            }
            let child = current.parts.get((*index - 1) as usize)?;
            if depth < path.len() - 1 && child.parts.is_empty() {
                return None;
            }
            current = child;
        }
        Some(current)
    }

    /// The `1`-based index path of a node, if it is reachable from the root.
    pub fn path_of(&self, target: &RawPart) -> Option<Vec<u32>> {
        fn walk(part: &RawPart, target: &RawPart, path: &mut Vec<u32>) -> bool {
            if std::ptr::eq(part, target) {
                return true;
            }
            for (index, child) in part.parts.iter().enumerate() {
                path.push(index as u32 + 1);
                if walk(child, target, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
        let mut path = Vec::new();
        if walk(&self.root, target, &mut path) {
            Some(path)
        } else {
            None
        }
    }
}

/// Find the header/body split of an entity bounded by `span`.
///
/// `span.end` is where the entity stops — for a part of a multipart message that
/// is the position of the CRLF that precedes the next boundary line, so the
/// boundary itself is never part of the entity. The root message's span ends at
/// the end of the file.
///
/// Accepts CRLF, bare LF and bare CR terminators, exactly like
/// [`ferroma_mail::ParsedMessage`] does, so a message stored with either style
/// splits the same way in both views.
pub fn split_header_body(raw: &[u8], span: Span) -> (Span, Span) {
    let end = span.end.min(raw.len());
    let start = span.start.min(end);
    let tail = &raw[start..end];
    // A leading empty line means "no headers, everything is the body".
    if tail.starts_with(b"\r\n") {
        return (Span::new(start, start), Span::new(start + 2, end));
    }
    if tail.starts_with(b"\n") {
        return (Span::new(start, start), Span::new(start + 1, end));
    }

    let mut index = 0usize;
    while index < tail.len() {
        match tail[index] {
            b'\n' => {
                if tail.get(index + 1) == Some(&b'\n') {
                    return (
                        Span::new(start, start + index),
                        Span::new(start + index + 2, end),
                    );
                }
                if tail.get(index + 1) == Some(&b'\r') && tail.get(index + 2) == Some(&b'\n') {
                    return (
                        Span::new(start, start + index + 1),
                        Span::new(start + index + 3, end),
                    );
                }
                index += 1;
            }
            b'\r' => {
                if tail.get(index + 1) == Some(&b'\r') {
                    return (
                        Span::new(start, start + index),
                        Span::new(start + index + 2, end),
                    );
                }
                if tail.get(index + 1) == Some(&b'\n') && tail.get(index + 2) == Some(&b'\r')
                    && tail.get(index + 3) == Some(&b'\n') {
                        return (
                            Span::new(start, start + index + 2),
                            Span::new(start + index + 4, end),
                        );
                    }
                index += 1;
            }
            _ => index += 1,
        }
    }
    // No blank line: the whole entity is headers and the body is empty.
    (Span::new(start, end), Span::new(end, end))
}

/// Split a multipart body into its child entities.
fn split_multipart(
    raw: &[u8],
    body: Span,
    boundary: &str,
    depth: usize,
    budget: &mut usize,
) -> Vec<RawPart> {
    let mut out = Vec::new();
    if depth > MAX_STRUCTURE_DEPTH || boundary.is_empty() {
        return out;
    }
    let marker = format!("--{boundary}");

    for span in boundary_spans(raw, body, &marker) {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        out.push(parse_entity(raw, span, depth, budget));
    }
    out
}

/// The byte spans of the child entities of a multipart body.
fn boundary_spans(raw: &[u8], body: Span, marker: &str) -> Vec<Span> {
    let bytes = body.slice(raw);
    let base = body.start;
    let mut spans: Vec<Span> = Vec::new();
    let mut current: Option<usize> = None;
    let mut index = 0usize;

    while index <= bytes.len() {
        let Some((line_start, line_end, next)) = next_line(bytes, index) else {
            break;
        };
        let line = &bytes[line_start..line_end];
        if is_boundary_line(line, marker) {
            if let Some(start) = current {
                spans.push(Span::new(base + start, base + entity_end(bytes, line_start)));
            }
            if is_terminating_boundary(line, marker) {
                return spans;
            }
            current = Some(next);
        }
        index = next;
    }
    if let Some(start) = current {
        spans.push(Span::new(base + start, base + bytes.len()));
    }
    spans
}

/// Byte offsets of a line: its content, and the start of the next line.
fn next_line(bytes: &[u8], from: usize) -> Option<(usize, usize, usize)> {
    if from >= bytes.len() {
        // The final line, when the buffer does not end with a terminator.
        return None;
    }
    let mut index = from;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => return Some((from, index, index + 1)),
            b'\r' => {
                let next = if bytes.get(index + 1) == Some(&b'\n') {
                    index + 2
                } else {
                    index + 1
                };
                return Some((from, index, next));
            }
            _ => index += 1,
        }
    }
    Some((from, bytes.len(), bytes.len()))
}

/// The end of an entity that runs up to the CRLF before the boundary line at
/// `line_start`, with that CRLF removed.
fn entity_end(bytes: &[u8], line_start: usize) -> usize {
    if line_start >= 2 && &bytes[line_start - 2..line_start] == b"\r\n" {
        line_start - 2
    } else if line_start >= 1 && (bytes[line_start - 1] == b'\n' || bytes[line_start - 1] == b'\r')
    {
        line_start - 1
    } else {
        line_start
    }
}

/// Whether a line opens a MIME part for `marker`.
fn is_boundary_line(line: &[u8], marker: &str) -> bool {
    let trimmed = trim_ascii_end(line);
    if !trimmed.starts_with(marker.as_bytes()) {
        return false;
    }
    let mut tail = &trimmed[marker.len()..];
    if tail.starts_with(b"--") {
        tail = &tail[2..];
    }
    tail.iter().all(|b| *b == b' ' || *b == b'\t')
}


/// Whether a boundary line is the closing `--boundary--`.
fn is_terminating_boundary(line: &[u8], marker: &str) -> bool {
    let trimmed = trim_ascii_end(line);
    match trimmed.strip_prefix(marker.as_bytes()) {
        Some(tail) => tail.starts_with(b"--"),
        None => false,
    }
}

/// Trim trailing spaces, tabs and CR from a line.
fn trim_ascii_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
        end -= 1;
    }
    &line[..end]
}

/// Whether a content type is an encapsulated-message type.
fn is_message_type(content_type: &ContentTypeView) -> bool {
    content_type.type_.eq_ignore_ascii_case("message")
        && (content_type.subtype.eq_ignore_ascii_case("rfc822")
            || content_type.subtype.eq_ignore_ascii_case("global"))
}

/// Parse one entity, recursing into a nested multipart.
fn parse_entity(raw: &[u8], span: Span, depth: usize, budget: &mut usize) -> RawPart {
    let (header, body) = split_header_body(raw, span);
    let header = Span::new(header.start.min(span.end), header.end.min(span.end));
    let headers = parse_headers(header.slice(raw));
    let content_type = content_type_of(&headers);
    let encoding = encoding_of(&headers);
    let is_multipart = content_type.is_multipart();
    let is_message = is_message_type(&content_type);

    let parts = if is_multipart && *budget > 0 && depth < MAX_STRUCTURE_DEPTH {
        match content_type.boundary() {
            Some(boundary) => split_multipart(raw, body, boundary, depth + 1, budget),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // An encapsulated message: the part's body *is* a whole RFC 5322 message,
    // whose envelope and structure `FETCH` has to report.
    let message = if is_message && !is_multipart && *budget > 0 && depth < MAX_STRUCTURE_DEPTH {
        *budget -= 1;
        Some(Box::new(parse_nested(raw, body, encoding, depth, budget)))
    } else {
        None
    };

    RawPart {
        header,
        body,
        headers,
        content_type,
        encoding,
        parts,
        message,
        owned: None,
        truncated: is_multipart && depth >= MAX_STRUCTURE_DEPTH,
    }
}

/// Parse the encapsulated message of a `message/rfc822` part.
///
/// When the part is 7bit/8bit/binary the nested message is a byte range of the
/// original buffer, so it is parsed in place. A transfer-encoded part has to be
/// decoded into an owned buffer first, and the nested node then keeps that
/// buffer in [`RawPart::owned`] so its spans stay valid.
fn parse_nested(
    raw: &[u8],
    body: Span,
    encoding: TransferEncoding,
    depth: usize,
    budget: &mut usize,
) -> RawPart {
    match encoding {
        TransferEncoding::SevenBit
        | TransferEncoding::EightBit
        | TransferEncoding::Binary => {
            let (header, nested_body) = split_header_body(raw, body);
            let header = Span::new(header.start.min(body.end), header.end.min(body.end));
            let headers = parse_headers(header.slice(raw));
            let content_type = content_type_of(&headers);
            let nested_encoding = encoding_of(&headers);
            let parts = if content_type.is_multipart() && *budget > 0 {
                match content_type.boundary() {
                    Some(boundary) => {
                        split_multipart(raw, nested_body, boundary, depth + 1, budget)
                    }
                    None => Vec::new(),
                }
            } else {
                Vec::new()
            };
            RawPart {
                header,
                body: nested_body,
                headers,
                content_type,
                encoding: nested_encoding,
                parts,
                message: None,
                owned: None,
                truncated: false,
            }
        }
        _ => {
            let decoded = decode_body(body.slice(raw), encoding);
            let buffer: std::sync::Arc<[u8]> = decoded.into();
            let mut nested = RawMessage::parse(buffer.as_ref().to_vec()).root;
            nested.owned = Some(buffer);
            nested
        }
    }
}

/// Parse a header block, never failing.
fn parse_headers(block: &[u8]) -> Headers {
    Headers::parse(&String::from_utf8_lossy(block)).unwrap_or_default()
}

/// The entity's content type, defaulting to `text/plain`.
fn content_type_of(headers: &Headers) -> ContentTypeView {
    headers
        .get("Content-Type")
        .map(ContentTypeView::parse)
        .unwrap_or_default()
}

/// The entity's transfer encoding, defaulting to `7bit`.
fn encoding_of(headers: &Headers) -> TransferEncoding {
    headers
        .get("Content-Transfer-Encoding")
        .map(TransferEncoding::parse)
        .unwrap_or_default()
}

/// The `Date:` header of a message, for `ENVELOPE` and `SENTON`.
pub fn header_date(headers: &Headers) -> Option<chrono::DateTime<chrono::Utc>> {
    headers.get("Date").and_then(parse_date)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = b"From: bob@example.net\r\nTo: alice@example.com\r\nSubject: hi\r\n\r\nHello there\r\n";

    #[test]
    fn a_simple_message_has_no_children_and_the_expected_spans() {
        let message = RawMessage::parse(SIMPLE.to_vec());
        assert_eq!(message.root().parts.len(), 0);
        assert_eq!(
            message.root().header.slice(message.raw()),
            b"From: bob@example.net\r\nTo: alice@example.com\r\nSubject: hi\r\n"
        );
        assert_eq!(message.body(), b"Hello there\r\n");
        assert_eq!(message.len(), SIMPLE.len());
        assert!(!message.is_empty());
    }

    #[test]
    fn body_header_and_text_concatenate_to_the_whole_message() {
        let message = RawMessage::parse(SIMPLE.to_vec());
        let mut joined = message.header_with_blank_line().to_vec();
        joined.extend_from_slice(message.body());
        assert_eq!(joined, SIMPLE);
        assert!(message.header_with_blank_line().ends_with(b"\r\n\r\n"));
    }

    #[test]
    fn a_message_with_no_blank_line_is_all_header() {
        let message = RawMessage::parse(b"From: a@b\r\n".to_vec());
        assert_eq!(message.body(), b"");
        assert_eq!(message.header_with_blank_line(), b"From: a@b\r\n");
    }

    #[test]
    fn a_message_starting_with_a_blank_line_has_no_headers() {
        let message = RawMessage::parse(b"\r\njust a body\r\n".to_vec());
        assert_eq!(message.root().header.len(), 0);
        assert_eq!(message.body(), b"just a body\r\n");
    }

    #[test]
    fn bare_lf_and_bare_cr_messages_split_too() {
        let lf = RawMessage::parse(b"From: a@b\n\nbody\n".to_vec());
        assert_eq!(lf.body(), b"body\n");
        assert_eq!(lf.root().headers.get("From"), Some("a@b"));

        let cr = RawMessage::parse(b"From: a@b\r\rbody\r".to_vec());
        assert_eq!(cr.body(), b"body\r");
    }

    #[test]
    fn content_type_defaults_to_text_plain() {
        let message = RawMessage::parse(SIMPLE.to_vec());
        assert_eq!(message.root().content_type.type_, "text");
        assert_eq!(message.root().content_type.subtype, "plain");
        assert_eq!(message.root().encoding, TransferEncoding::SevenBit);
        assert!(message.root().is_text());
        assert!(!message.root().is_multipart());
    }

    #[test]
    fn content_type_parsing_handles_parameters() {
        let view = ContentTypeView::parse("multipart/mixed; boundary=\"a b\"; charset=utf-8");
        assert_eq!(view.type_, "multipart");
        assert_eq!(view.subtype, "mixed");
        assert_eq!(view.boundary(), Some("a b"));
        assert_eq!(view.param("CHARSET"), Some("utf-8"));
        assert!(view.is_multipart());

        let bare = ContentTypeView::parse("text/plain");
        assert!(bare.params.is_empty());

        let upper = ContentTypeView::parse("TEXT/HTML; CHARSET=UTF-8");
        assert_eq!(upper.type_, "text");
        assert_eq!(upper.subtype, "html");
        assert_eq!(upper.param("charset"), Some("UTF-8"));

        let empty = ContentTypeView::parse("");
        assert_eq!(empty, ContentTypeView::default());
    }

    #[test]
    fn content_type_with_only_a_primary_type_is_tolerated() {
        let view = ContentTypeView::parse("text");
        assert_eq!(view.type_, "text");
        assert_eq!(view.subtype, "");
    }

    fn multipart_with(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut message = String::from(
            "From: a@b\r\nSubject: multi\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"BND\"\r\n\r\n",
        );
        for (header, body) in parts {
            message.push_str("--BND\r\n");
            message.push_str(header);
            message.push_str("\r\n\r\n");
            message.push_str(body);
            message.push_str("\r\n");
        }
        message.push_str("--BND--\r\n");
        message.into_bytes()
    }

    #[test]
    fn multipart_splits_into_children() {
        let raw = multipart_with(&[
            ("Content-Type: text/plain", "first"),
            ("Content-Type: text/html", "<p>second</p>"),
        ]);
        let message = RawMessage::parse(raw.clone());
        assert!(message.root().is_multipart());
        assert_eq!(message.root().parts.len(), 2);

        let first = &message.root().parts[0];
        assert_eq!(first.body.slice(&raw), b"first");
        assert_eq!(first.content_type.subtype, "plain");
        assert_eq!(first.headers.get("Content-Type"), Some("text/plain"));

        let second = &message.root().parts[1];
        assert_eq!(second.body.slice(&raw), b"<p>second</p>");
        assert_eq!(second.content_type.subtype, "html");
    }

    #[test]
    fn multipart_parts_addressable_by_path() {
        let raw = multipart_with(&[
            ("Content-Type: text/plain", "first"),
            ("Content-Type: text/html", "second"),
        ]);
        let message = RawMessage::parse(raw);
        assert!(message.part(&[1]).is_some());
        assert!(message.part(&[2]).is_some());
        assert!(message.part(&[3]).is_none());
        assert!(message.part(&[0]).is_none());
        assert!(message.part(&[1, 1]).is_none());
        assert!(message.part(&[]).is_some());
    }

    #[test]
    fn nested_multipart_paths_resolve() {
        let raw = b"Content-Type: multipart/mixed; boundary=OUT\r\n\r\n\
                    --OUT\r\nContent-Type: multipart/alternative; boundary=IN\r\n\r\n\
                    --IN\r\nContent-Type: text/plain\r\n\r\nplain\r\n\
                    --IN\r\nContent-Type: text/html\r\n\r\n<p>html</p>\r\n\
                    --IN--\r\n\
                    --OUT\r\nContent-Type: application/pdf\r\n\r\nPDF\r\n\
                    --OUT--\r\n"
            .to_vec();
        let message = RawMessage::parse(raw.clone());
        assert_eq!(message.root().parts.len(), 2);
        assert_eq!(message.part(&[1]).unwrap().parts.len(), 2);
        assert_eq!(message.part(&[1, 1]).unwrap().body.slice(&raw), b"plain");
        assert_eq!(
            message.part(&[1, 2]).unwrap().body.slice(&raw),
            b"<p>html</p>"
        );
        assert_eq!(message.part(&[2]).unwrap().body.slice(&raw), b"PDF");
        assert!(message.part(&[1, 3]).is_none());
        assert!(message.part(&[3]).is_none());
    }

    #[test]
    fn multipart_without_a_boundary_degrades_to_a_leaf() {
        let raw = b"Content-Type: multipart/mixed\r\n\r\nnot really multipart\r\n".to_vec();
        let message = RawMessage::parse(raw);
        assert!(message.root().is_multipart());
        assert!(message.root().parts.is_empty());
    }

    #[test]
    fn a_terminating_boundary_stops_the_walk() {
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n\
                    --B\r\nContent-Type: text/plain\r\n\r\none\r\n\
                    --B--\r\n-Not a boundary\r\n"
            .to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().parts.len(), 1);
    }

    #[test]
    fn a_missing_terminating_boundary_still_yields_the_last_part() {
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n\
                    --B\r\nContent-Type: text/plain\r\n\r\none\r\n"
            .to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().parts.len(), 1);
        assert_eq!(message.root().parts[0].body.slice(message.raw()), b"one\r\n");
    }

    #[test]
    fn a_preamble_before_the_first_boundary_is_ignored() {
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n\
                    This is the preamble.\r\n\
                    --B\r\nContent-Type: text/plain\r\n\r\none\r\n\
                    --B--\r\nepilogue\r\n"
            .to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().parts.len(), 1);
        assert_eq!(message.root().parts[0].body.slice(message.raw()), b"one");
    }

    #[test]
    fn boundary_matching_is_exact_and_not_a_prefix() {
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n\
                    --BB\r\nContent-Type: text/plain\r\n\r\nwrong\r\n\
                    --B\r\nContent-Type: text/plain\r\n\r\nright\r\n\
                    --B--\r\n"
            .to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().parts.len(), 1);
        assert_eq!(message.root().parts[0].body.slice(message.raw()), b"right");
    }

    #[test]
    fn boundary_lines_tolerate_trailing_whitespace() {
        // The marker is the whole boundary, `--` included — that is what
        // `split_multipart` builds before calling this.
        assert!(is_boundary_line(b"--B  \t", "--B"));
        assert!(is_boundary_line(b"--B", "--B"));
        assert!(is_boundary_line(b"--B--", "--B"));
        assert!(is_boundary_line(b"--B--  ", "--B"));
        assert!(!is_boundary_line(b"--BB", "--B"));
        assert!(!is_boundary_line(b"---B", "--B"));
        assert!(!is_boundary_line(b"B", "--B"));
        assert!(is_terminating_boundary(b"--B--", "--B"));
        assert!(!is_terminating_boundary(b"--B", "--B"));
    }
    #[test]
    fn content_id_is_stripped_of_angle_brackets() {
        let raw = b"Content-Type: image/png\r\nContent-ID: <abc@def>\r\n\r\nx".to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().content_id().as_deref(), Some("abc@def"));
    }

    #[test]
    fn filename_comes_from_disposition_then_content_type() {
        let raw =
            b"Content-Type: application/pdf; name=\"from-type.pdf\"\r\n\
              Content-Disposition: attachment; filename=\"from-disp.pdf\"\r\n\r\nx"
                .to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().filename().as_deref(), Some("from-disp.pdf"));

        let raw = b"Content-Type: application/pdf; name=bare.pdf\r\n\r\nx".to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().filename().as_deref(), Some("bare.pdf"));

        let message = RawMessage::parse(b"Content-Type: text/plain\r\n\r\nx".to_vec());
        assert!(message.root().filename().is_none());
    }

    #[test]
    fn disposition_language_and_location_are_exposed() {
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\
                    Content-Disposition: inline; filename=x.txt\r\n\
                    Content-Language: en, fr\r\n\
                    Content-Location: http://example.com/x\r\n\
                    Content-Description: a note\r\n\r\nbody"
            .to_vec();
        let message = RawMessage::parse(raw);
        let (type_, params) = message.root().disposition().unwrap();
        assert_eq!(type_, "inline");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].0, "filename");
        assert_eq!(message.root().languages(), vec!["en", "fr"]);
        assert_eq!(message.root().location(), Some("http://example.com/x"));
        assert_eq!(message.root().description(), Some("a note"));
    }

    #[test]
    fn quoted_parameters_support_escapes() {
        let (_, params) = split_type_and_params("attachment; filename=\"a\\\"b.txt\"");
        assert_eq!(params[0].1, "a\"b.txt");
    }

    #[test]
    fn parameter_without_a_value_is_tolerated() {
        let (_, params) = split_type_and_params("text/plain; charset");
        assert_eq!(params, vec![("charset".to_string(), String::new())]);
    }

    #[test]
    fn count_lines_matches_the_usual_definitions() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"one"), 1);
        assert_eq!(count_lines(b"one\r\n"), 1);
        assert_eq!(count_lines(b"one\r\ntwo"), 2);
        assert_eq!(count_lines(b"one\r\ntwo\r\n"), 2);
        assert_eq!(count_lines(b"one\ntwo\nthree\n"), 3);
        assert_eq!(count_lines(b"\r\n"), 1);
        assert_eq!(count_lines(b"a\rb"), 2);
    }

    #[test]
    fn content_lines_counts_decoded_lines_for_base64() {
        // base64("one\ntwo\n") == b25lCnR3bwo=
        let raw = b"Content-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\nb25lCnR3bwo="
            .to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().content_lines(message.raw()), 2);
    }

    #[test]
    fn content_lines_counts_encoded_lines_for_7bit() {
        let raw = b"Content-Type: text/plain\r\n\r\none\r\ntwo\r\n".to_vec();
        let message = RawMessage::parse(raw);
        assert_eq!(message.root().content_lines(message.raw()), 2);
    }

    #[test]
    fn paths_can_be_recovered_from_a_node_reference() {
        let raw = multipart_with(&[
            ("Content-Type: text/plain", "a"),
            ("Content-Type: text/html", "b"),
        ]);
        let message = RawMessage::parse(raw);
        let second = &message.root().parts[1];
        assert_eq!(message.path_of(second), Some(vec![2]));
        assert_eq!(message.path_of(message.root()), Some(Vec::new()));
    }

    #[test]
    fn spans_clamp_instead_of_panicking() {
        let raw = b"0123456789";
        assert_eq!(Span::new(2, 4).slice(raw), b"23");
        assert_eq!(Span::new(8, 99).slice(raw), b"89");
        assert_eq!(Span::new(99, 200).slice(raw), b"");
        assert_eq!(Span::new(5, 2).slice(raw), b"");
        assert_eq!(Span::new(0, 10).len(), 10);
        assert!(Span::new(3, 3).is_empty());
    }

    #[test]
    fn hostile_structure_inputs_never_panic() {
        let inputs: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"\r\n".to_vec(),
            b"--B\r\n".to_vec(),
            b"Content-Type: multipart/mixed; boundary=\r\n\r\n--\r\n".to_vec(),
            b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\n--B\r\n--B\r\n".to_vec(),
            {
                // 60 levels of nesting: the walk must stop at the depth cap.
                let mut message = String::new();
                for level in 0..60 {
                    message.push_str(&format!(
                        "Content-Type: multipart/mixed; boundary=B{level}\r\n\r\n--B{level}\r\n"
                    ));
                }
                message.push_str("leaf\r\n");
                message.into_bytes()
            },
            {
                // 5000 sibling parts: the walk must stop at the part cap.
                let mut message =
                    String::from("Content-Type: multipart/mixed; boundary=B\r\n\r\n");
                for _ in 0..5000 {
                    message.push_str("--B\r\nContent-Type: text/plain\r\n\r\nx\r\n");
                }
                message.push_str("--B--\r\n");
                message.into_bytes()
            },
            vec![0xff; 4096],
            b"Content-Type: text/plain; charset=\"\xff\xfe\"\r\n\r\n\xff".to_vec(),
        ];
        for input in inputs {
            let message = RawMessage::parse(input);
            // Only the requirement that it returns at all, and that every span
            // is inside the buffer that node indexes into.
            let mut stack = vec![message.root()];
            while let Some(part) = stack.pop() {
                let buffer_len = part.bytes(message.raw()).len();
                assert!(part.body.end <= buffer_len);
                assert!(part.header.end <= buffer_len);
                stack.extend(part.parts.iter());
                if let Some(nested) = &part.message {
                    stack.push(nested);
                }
            }
        }
    }

    #[test]
    fn deep_nesting_is_capped() {
        let mut message = String::new();
        for level in 0..60 {
            message.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=B{level}\r\n\r\n--B{level}\r\n"
            ));
        }
        message.push_str("leaf\r\n");
        let parsed = RawMessage::parse(message.into_bytes());

        let mut depth = 0usize;
        let mut current = parsed.root();
        while let Some(child) = current.parts.first() {
            depth += 1;
            current = child;
            assert!(depth <= MAX_STRUCTURE_DEPTH + 1, "walk must stop");
        }
        assert!(depth <= MAX_STRUCTURE_DEPTH + 1);
    }

    #[test]
    fn sibling_count_is_capped() {
        let mut message = String::from("Content-Type: multipart/mixed; boundary=B\r\n\r\n");
        for _ in 0..5000 {
            message.push_str("--B\r\nContent-Type: text/plain\r\n\r\nx\r\n");
        }
        message.push_str("--B--\r\n");
        let parsed = RawMessage::parse(message.into_bytes());
        assert!(parsed.root().parts.len() <= MAX_STRUCTURE_PARTS);
    }

    #[test]
    fn header_date_reads_the_date_field() {
        let message = RawMessage::parse(
            b"Date: Wed, 08 Jul 2026 09:00:00 +0000\r\n\r\nx".to_vec(),
        );
        let date = header_date(&message.root().headers).expect("date must parse");
        assert_eq!(date.to_string(), "2026-07-08 09:00:00 UTC");

        let message = RawMessage::parse(b"From: a@b\r\n\r\nx".to_vec());
        assert!(header_date(&message.root().headers).is_none());
    }

    #[test]
    fn message_rfc822_is_detected() {
        let raw = b"Content-Type: message/rfc822\r\n\r\nFrom: a@b\r\n\r\nx".to_vec();
        let message = RawMessage::parse(raw);
        assert!(message.root().is_message());
        assert!(!message.root().is_text());
    }
}

#[cfg(test)]
mod probe {
    use super::*;
    #[test]
    fn probe_boundary() {
        println!("A={}", is_boundary_line(b"--B", "B"));
        println!("B={}", is_boundary_line(b"--B  \t", "B"));
        println!("C={}", is_terminating_boundary(b"--B--", "B"));
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\none\r\n--B--\r\n".to_vec();
        let message = RawMessage::parse(raw);
        println!("PARTS={}", message.root().parts.len());
        println!("BODY={:?}", String::from_utf8_lossy(message.body()));
    }
}
