//! RFC 5322 message parsing: the header/body split, the MIME tree walk, and the
//! accessors the rest of the platform uses to show a message.
//!
//! Parsing is *total*: any byte string produces a [`ParsedMessage`]. The only
//! errors are the explicit resource limits in [`ParseLimits`], because a mail
//! server that rejects a message it cannot render drops real mail.

use chrono::{DateTime, Utc};
use ferroma_core::{FerromaError, Result, RfcMessageId};

use crate::headers::Headers;
use crate::mime::{
    decode_base64, decode_quoted_printable, ContentType, MimePart, TransferEncoding,
};
use crate::AddressMailbox;

/// Resource limits applied while parsing.
///
/// The defaults mirror `ferroma_core::Limits`: at most 20 nesting levels, 200
/// parts, and 25 MiB per part or per message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParseLimits {
    /// Maximum MIME nesting depth. Exceeding it is a hard error.
    pub max_depth: usize,
    /// Maximum number of parts in the whole tree. Exceeding it is a hard error.
    pub max_parts: usize,
    /// Maximum size of a single part's raw bytes.
    pub max_part_size: usize,
    /// Maximum size of the whole message.
    pub max_message_size: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        ParseLimits {
            max_depth: 20,
            max_parts: 200,
            max_part_size: 26_214_400,
            max_message_size: 26_214_400,
        }
    }
}

impl ParseLimits {
    /// Limits derived from the platform-wide configuration block.
    pub fn from_limits(limits: &ferroma_core::Limits) -> Self {
        let size = limits.max_message_size_usize();
        ParseLimits {
            max_depth: limits.max_mime_depth,
            max_parts: 200,
            max_part_size: size,
            max_message_size: size,
        }
    }
}

/// A parsed message: its top-level headers and its MIME body tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParsedMessage {
    /// The top-level header fields.
    pub headers: Headers,
    /// The root parts of the body. A message with no `Content-Type` has exactly
    /// one `text/plain` part here.
    pub body: Vec<MimePart>,
}

impl ParsedMessage {
    /// Parse a message with [`ParseLimits::default`].
    pub fn parse(raw: &[u8]) -> Result<Self> {
        Self::parse_with_limits(raw, &ParseLimits::default())
    }

    /// Parse a message, refusing to spend more than `limits` on it.
    pub fn parse_with_limits(raw: &[u8], limits: &ParseLimits) -> Result<Self> {
        if raw.len() > limits.max_message_size {
            return Err(FerromaError::LimitExceeded(format!(
                "message is {} bytes, limit is {}",
                raw.len(),
                limits.max_message_size
            )));
        }

        let (header_bytes, body) = split_header_body(raw);
        let headers = Headers::parse(&String::from_utf8_lossy(header_bytes))?;

        let mut state = ParseState { limits, parts: 0 };
        let parts = parse_body_parts(&headers, body, 1, &mut state)?;

        Ok(ParsedMessage { headers, body: parts })
    }

    /// The value of a top-level header field.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// The RFC 2047-decoded `Subject:`.
    pub fn subject(&self) -> Option<String> {
        self.headers.subject()
    }

    /// The `From:` mailboxes.
    pub fn from(&self) -> Vec<AddressMailbox> {
        self.headers.from()
    }

    /// The `To:` mailboxes.
    pub fn to(&self) -> Vec<AddressMailbox> {
        self.headers.to()
    }

    /// The `Cc:` mailboxes.
    pub fn cc(&self) -> Vec<AddressMailbox> {
        self.headers.cc()
    }

    /// The `Reply-To:` mailboxes.
    pub fn reply_to(&self) -> Vec<AddressMailbox> {
        self.headers.reply_to()
    }

    /// The parsed `Date:` in UTC.
    pub fn date(&self) -> Option<DateTime<Utc>> {
        self.headers.date()
    }

    /// The `Message-ID:` value.
    pub fn message_id(&self) -> Option<RfcMessageId> {
        self.headers.message_id()
    }

    /// The first non-attachment `text/plain` part, charset-decoded.
    pub fn text_body(&self) -> Option<String> {
        self.first_body_part("plain")
    }

    /// The first non-attachment `text/html` part, charset-decoded.
    pub fn html_body(&self) -> Option<String> {
        self.first_body_part("html")
    }

    /// Every part that should be shown as an attachment, in document order.
    pub fn attachments(&self) -> Vec<&MimePart> {
        let mut out = Vec::new();
        for root in &self.body {
            root.walk(&mut |part| {
                if part.is_attachment() && !part.is_multipart() {
                    out.push(part);
                }
            });
        }
        out
    }

    /// Whether the message carries any attachment.
    pub fn has_attachments(&self) -> bool {
        !self.attachments().is_empty()
    }

    /// Whether the body is a `multipart/*` structure.
    pub fn is_multipart(&self) -> bool {
        self.body.len() > 1 || self.body.first().map(MimePart::is_multipart).unwrap_or(false)
    }

    /// The root parts of the body.
    pub fn parts(&self) -> &[MimePart] {
        &self.body
    }

    /// A one-line, whitespace-collapsed preview of the text body.
    ///
    /// Long text is cut on a character boundary and marked with `…`, so the
    /// result is always valid UTF-8 and never exceeds `max_chars` characters.
    pub fn snippet(&self, max_chars: usize) -> String {
        let text = self
            .text_body()
            .or_else(|| self.html_body().map(|html| strip_html_tags(&html)))
            .unwrap_or_default();
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        truncate_chars(&collapsed, max_chars)
    }

    /// Find the first non-attachment `text/<subtype>` part, preferring the
    /// `multipart/alternative` branch and never descending into an attached
    /// `message/rfc822`.
    fn first_body_part(&self, subtype: &str) -> Option<String> {
        for root in &self.body {
            if let Some(text) = search_text(root, subtype) {
                return Some(text);
            }
        }
        None
    }
}

/// Depth-first search for the preferred `text/<subtype>` body part.
fn search_text(part: &MimePart, subtype: &str) -> Option<String> {
    // Never look inside an attached message: those bytes are a different email.
    if part.content_type.is_message() {
        return None;
    }
    if part.content_type.type_ == "text"
        && part.content_type.subtype == subtype
        && !part.is_attachment()
    {
        return part.decode_text();
    }
    for child in &part.parts {
        if let Some(found) = search_text(child, subtype) {
            return Some(found);
        }
    }
    None
}

/// Truncate on a character boundary, appending an ellipsis when shortened.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Crude HTML-to-text used only for the snippet of an HTML-only message.
fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Split a raw message into its header block and the raw body bytes.
///
/// The separator is the first empty line, and all three line-ending styles real
/// mail uses are accepted: CRLF CRLF, LF LF and bare CR CR.
///
/// A leading empty line means "no headers, everything is body", which is what a
/// MIME part with no header fields of its own looks like.
fn split_header_body(raw: &[u8]) -> (&[u8], &[u8]) {
    if raw.starts_with(b"\r\n") {
        return (&[], &raw[2..]);
    }
    if raw.starts_with(b"\n") {
        return (&[], &raw[1..]);
    }
    if raw.starts_with(b"\r") {
        return (&[], &raw[1..]);
    }
    let mut i = 0usize;
    while i < raw.len() {
        match raw[i] {
            b'\n' => {
                if i + 1 < raw.len() && raw[i + 1] == b'\n' {
                    return (&raw[..i], &raw[i + 2..]);
                }
                if i + 2 < raw.len() && raw[i + 1] == b'\r' && raw[i + 2] == b'\n' {
                    return (&raw[..i + 1], &raw[i + 3..]);
                }
                i += 1;
            }
            b'\r' => {
                if i + 1 < raw.len() && raw[i + 1] == b'\r' {
                    return (&raw[..i], &raw[i + 2..]);
                }
                if i + 3 < raw.len()
                    && raw[i + 1] == b'\n'
                    && raw[i + 2] == b'\r'
                    && raw[i + 3] == b'\n'
                {
                    return (&raw[..i + 2], &raw[i + 4..]);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    (raw, &[])
}

/// Running parse state: the limits plus the number of parts built so far.
struct ParseState<'a> {
    limits: &'a ParseLimits,
    parts: usize,
}

impl ParseState<'_> {
    /// Count one part, failing when the per-message budget is exhausted.
    fn count(&mut self) -> Result<()> {
        self.parts += 1;
        if self.parts > self.limits.max_parts {
            return Err(FerromaError::LimitExceeded(format!(
                "message has more than {} MIME parts",
                self.limits.max_parts
            )));
        }
        Ok(())
    }
}

/// Build the body parts of a message whose headers are already parsed.
fn parse_body_parts(
    headers: &Headers,
    body: &[u8],
    depth: usize,
    state: &mut ParseState<'_>,
) -> Result<Vec<MimePart>> {
    if depth > state.limits.max_depth {
        return Err(FerromaError::LimitExceeded(format!(
            "MIME nesting deeper than {} levels",
            state.limits.max_depth
        )));
    }

    let content_type = headers
        .get("Content-Type")
        .map(ContentType::parse)
        .unwrap_or_default();

    if content_type.is_multipart() {
        let Some(boundary) = content_type.boundary().map(|b| b.to_string()) else {
            // A multipart with no boundary is unparseable: degrade to text.
            let part = build_leaf(headers, body, &ContentType::default(), state.limits)?;
            state.count()?;
            return Ok(vec![part]);
        };
        let raw_parts = split_multipart(body, &boundary);
        state.count()?; // the container itself
        let mut parts = Vec::with_capacity(raw_parts.len());
        for raw in raw_parts {
            parts.push(parse_entity(raw, depth + 1, state)?);
        }
        return Ok(vec![MimePart {
            headers: headers.clone(),
            content_type,
            encoding: TransferEncoding::SevenBit,
            content: Vec::new(),
            parts,
        }]);
    }

    let part = build_leaf(headers, body, &content_type, state.limits)?;
    state.count()?;
    Ok(vec![part])
}

/// Parse a complete MIME entity (its own headers plus body) from raw bytes.
fn parse_entity(raw: &[u8], depth: usize, state: &mut ParseState<'_>) -> Result<MimePart> {
    if depth > state.limits.max_depth {
        return Err(FerromaError::LimitExceeded(format!(
            "MIME nesting deeper than {} levels",
            state.limits.max_depth
        )));
    }
    let (header_bytes, body) = split_header_body(raw);
    let headers = Headers::parse(&String::from_utf8_lossy(header_bytes))?;
    let content_type = headers
        .get("Content-Type")
        .map(ContentType::parse)
        .unwrap_or_default();

    if content_type.is_multipart() && content_type.boundary().is_some() {
        let boundary = content_type
            .boundary()
            .map(|b| b.to_string())
            .unwrap_or_default();
        let raw_parts = split_multipart(body, &boundary);
        state.count()?;
        let mut parts = Vec::with_capacity(raw_parts.len());
        for raw in raw_parts {
            parts.push(parse_entity(raw, depth + 1, state)?);
        }
        return Ok(MimePart {
            headers,
            content_type,
            encoding: TransferEncoding::SevenBit,
            content: Vec::new(),
            parts,
        });
    }

    let part = build_leaf(&headers, body, &content_type, state.limits)?;
    state.count()?;
    Ok(part)
}

/// Build a leaf part: apply the transfer encoding, then the framing rules.
fn build_leaf(
    headers: &Headers,
    body: &[u8],
    content_type: &ContentType,
    limits: &ParseLimits,
) -> Result<MimePart> {
    let encoding = headers
        .get("Content-Transfer-Encoding")
        .map(TransferEncoding::parse)
        .unwrap_or_default();
    let bounded = if body.len() > limits.max_part_size {
        &body[..limits.max_part_size]
    } else {
        body
    };

    let (content, encoding) = if content_type.is_message() {
        // `message/rfc822` is deliberately *not* decoded: the encapsulated
        // message has to survive byte-for-byte so it can be forwarded again.
        (bounded.to_vec(), TransferEncoding::SevenBit)
    } else {
        let decoded = decode_transfer(bounded, encoding).unwrap_or_else(|err| {
            tracing::debug!(%err, "transfer decoding failed; keeping raw bytes");
            bounded.to_vec()
        });
        (decoded, encoding)
    };

    Ok(MimePart {
        headers: headers.clone(),
        content_type: content_type.clone(),
        encoding,
        content: sanitise_leaf_content(content, limits),
        parts: Vec::new(),
    })
}

/// Apply the transfer encoding to the raw body bytes.
fn decode_transfer(body: &[u8], encoding: TransferEncoding) -> Result<Vec<u8>> {
    match encoding {
        TransferEncoding::Base64 => decode_base64(body),
        TransferEncoding::QuotedPrintable => decode_quoted_printable(body),
        // 7bit/8bit/binary bodies are already the content.
        TransferEncoding::SevenBit | TransferEncoding::EightBit | TransferEncoding::Binary => {
            Ok(body.to_vec())
        }
    }
}

/// Trim the single trailing CRLF that belongs to the boundary, not the content.
fn sanitise_leaf_content(mut content: Vec<u8>, limits: &ParseLimits) -> Vec<u8> {
    if content.ends_with(b"\r\n") {
        content.truncate(content.len() - 2);
    } else if content.ends_with(b"\n") {
        content.truncate(content.len() - 1);
    } else if content.ends_with(b"\r") {
        // A boundary line may be preceded by a bare CR on old systems.
        content.truncate(content.len() - 1);
    }
    if content.len() > limits.max_part_size {
        content.truncate(limits.max_part_size);
    }
    content
}

/// Split a `multipart/*` body into its parts, honouring the terminating
/// `--boundary--`. The preamble and the epilogue are discarded.
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    let mut current: Option<usize> = None; // start of the current part
    let mut pos = 0usize;
    let len = body.len();

    while pos <= len {
        let Some((line_end, next)) = find_line_end(body, pos) else {
            break;
        };
        let line = &body[pos..line_end];
        if is_boundary_line(line, boundary) {
            if let Some(start) = current {
                out.push(&body[start..pos]);
            }
            if is_terminating_boundary(line, boundary) {
                return out;
            }
            current = Some(next);
        }
        pos = next;
    }

    if let Some(start) = current {
        // No terminating boundary: take everything left as the last part.
        out.push(&body[start..]);
    }
    out
}

/// Byte offsets of the end of the line starting at `pos`, and of the next line.
fn find_line_end(body: &[u8], pos: usize) -> Option<(usize, usize)> {
    if pos >= body.len() {
        return None;
    }
    let mut i = pos;
    while i < body.len() {
        match body[i] {
            b'\n' => return Some((i, i + 1)),
            b'\r' => {
                let end = i;
                let next = if i + 1 < body.len() && body[i + 1] == b'\n' {
                    i + 2
                } else {
                    i + 1
                };
                return Some((end, next));
            }
            _ => i += 1,
        }
    }
    Some((body.len(), body.len() + 1))
}

/// Whether a physical line opens a part: `--boundary` with optional trailing WSP.
fn is_boundary_line(line: &[u8], boundary: &str) -> bool {
    let trimmed = trim_ascii_end(line);
    if !trimmed.starts_with(b"--") {
        return false;
    }
    let rest = &trimmed[2..];
    let b = boundary.as_bytes();
    if rest.len() < b.len() || &rest[..b.len()] != b {
        return false;
    }
    let tail = &rest[b.len()..];
    tail.is_empty() || tail == b"--"
}

/// Whether a boundary line is the closing `--boundary--`.
fn is_terminating_boundary(line: &[u8], boundary: &str) -> bool {
    let trimmed = trim_ascii_end(line);
    let mut expected = Vec::with_capacity(boundary.len() + 4);
    expected.extend_from_slice(b"--");
    expected.extend_from_slice(boundary.as_bytes());
    expected.extend_from_slice(b"--");
    trimmed == expected.as_slice()
}

/// Strip trailing ASCII whitespace from a byte slice.
fn trim_ascii_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
        end -= 1;
    }
    &line[..end]
}

/// Decode a part's text using an explicit charset.
///
/// Used by callers that know the charset out of band, for example when the
/// `Content-Type` came from the API rather than from the sender.
pub fn decode_part_text(part: &MimePart, charset: &str) -> String {
    crate::mime::decode_charset(&part.content, charset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A Thunderbird-style `multipart/alternative`, CRLF throughout.
    const THUNDERBIRD: &str = concat!(
        "From: Alice <alice@example.com>\r\n",
        "To: Bob <bob@example.org>\r\n",
        "Subject: =?UTF-8?B?5L2g5aW977yM5LiW55WM?=\r\n",
        "Date: Tue, 16 Sep 2025 12:00:00 +0800\r\n",
        "Message-ID: <tb-1@example.com>\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/alternative; boundary=\"------------TB1\"\r\n",
        "\r\n",
        "This is a multi-part message in MIME format.\r\n",
        "--------------TB1\r\n",
        "Content-Type: text/plain; charset=utf-8; format=flowed\r\n",
        "Content-Transfer-Encoding: 7bit\r\n",
        "\r\n",
        "Hello, World!\r\n",
        "--------------TB1\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "Content-Transfer-Encoding: 7bit\r\n",
        "\r\n",
        "<html><body><p>Hello, <b>World</b>!</p></body></html>\r\n",
        "--------------TB1--\r\n",
    );

    /// An Outlook-style `multipart/related` with an inline image.
    const OUTLOOK: &str = concat!(
        "From: Carol <carol@example.com>\r\n",
        "To: Dave <dave@example.org>\r\n",
        "Subject: Outlook style\r\n",
        "Content-Type: multipart/related; boundary=\"----=_NextPart_000\"\r\n",
        "MIME-Version: 1.0\r\n",
        "\r\n",
        "------=_NextPart_000\r\n",
        "Content-Type: text/html; charset=\"windows-1252\"\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
        "<html><body>caf=E9 <img src=3D\"cid:image001\"></body></html>\r\n",
        "------=_NextPart_000\r\n",
        "Content-Type: image/png; name=\"image001.png\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "Content-ID: <image001@01D2>\r\n",
        "Content-Disposition: inline; filename=\"image001.png\"\r\n",
        "\r\n",
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmM\r\n",
        "IQAAAABJRU5ErkJggg==\r\n",
        "------=_NextPart_000--\r\n",
    );

    /// A message with a folded header and no `Content-Type` at all.
    const NO_CONTENT_TYPE: &str = concat!(
        "From: eve@example.com\r\n",
        "To: frank@example.org\r\n",
        "Subject: folded\r\n",
        " subject line\r\n",
        "Date: Tue, 16 Sep 2025 12:00:00 +0000\r\n",
        "\r\n",
        "Just a plain body.\r\n",
    );

    #[test]
    fn parses_headers_and_single_text_body() {
        let msg = ParsedMessage::parse(NO_CONTENT_TYPE.as_bytes()).unwrap();
        assert_eq!(msg.subject().as_deref(), Some("folded subject line"));
        assert_eq!(msg.text_body().as_deref(), Some("Just a plain body."));
        assert_eq!(msg.html_body(), None);
        assert!(!msg.is_multipart());
        assert!(!msg.has_attachments());
        assert_eq!(msg.parts().len(), 1);
        assert_eq!(msg.parts()[0].content_type.subtype, "plain");
        assert_eq!(msg.header("to"), Some("frank@example.org"));
    }

    #[test]
    fn parses_a_thunderbird_alternative() {
        let msg = ParsedMessage::parse(THUNDERBIRD.as_bytes()).unwrap();
        assert_eq!(msg.subject().as_deref(), Some("你好，世界"));
        assert_eq!(msg.text_body().as_deref(), Some("Hello, World!"));
        assert_eq!(
            msg.html_body().as_deref(),
            Some("<html><body><p>Hello, <b>World</b>!</p></body></html>")
        );
        assert!(msg.is_multipart());
        assert!(!msg.has_attachments());
        assert_eq!(msg.parts().len(), 1);
        assert_eq!(msg.parts()[0].parts.len(), 2);
        assert_eq!(msg.from()[0].address.to_string(), "alice@example.com");
        assert_eq!(msg.to()[0].address.to_string(), "bob@example.org");
        assert_eq!(msg.date().unwrap().to_rfc3339(), "2025-09-16T04:00:00+00:00");
        assert_eq!(msg.message_id().unwrap().inner(), "tb-1@example.com");
    }

    #[test]
    fn parses_an_outlook_related_with_an_inline_image() {
        let msg = ParsedMessage::parse(OUTLOOK.as_bytes()).unwrap();
        assert_eq!(
            msg.html_body().as_deref(),
            Some("<html><body>café <img src=\"cid:image001\"></body></html>")
        );
        let attachments = msg.attachments();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename().as_deref(), Some("image001.png"));
        assert_eq!(attachments[0].content_id(), Some("image001@01D2"));
        assert_eq!(attachments[0].disposition(), Some("inline"));
        // The PNG magic number survives the base64 decode.
        assert_eq!(&attachments[0].content[..4], &[0x89, b'P', b'N', b'G']);
        assert!(msg.text_body().is_none());
    }

    #[test]
    fn parses_a_chinese_utf8_subject() {
        let raw = concat!(
            "Subject: =?UTF-8?B?5Lit5paH5Li76aKY?=\r\n",
            "From: =?UTF-8?Q?=E5=BC=A0=E4=B8=89?= <zhang@example.cn>\r\n",
            "\r\n",
            "body\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.subject().as_deref(), Some("中文主题"));
        assert_eq!(msg.from()[0].name.as_deref(), Some("张三"));
    }

    #[test]
    fn decodes_a_gbk_body() {
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(
            b"Subject: GBK\r\nContent-Type: text/plain; charset=\"GB2312\"\r\n\r\n",
        );
        // 中文 in GBK
        raw.extend_from_slice(&[0xd6, 0xd0, 0xce, 0xc4]);
        raw.extend_from_slice(b"\r\n");
        let msg = ParsedMessage::parse(&raw).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("中文"));
    }

    #[test]
    fn decodes_a_quoted_printable_body_with_soft_breaks() {
        let raw = concat!(
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "This line is long enough that the sender decided to wrap it with a soft=\r\n",
            " break, and here is some =C3=A9 text.\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(
            msg.text_body().as_deref(),
            Some(
                "This line is long enough that the sender decided to wrap it with a soft break, and here is some é text."
            )
        );
    }

    #[test]
    fn decodes_a_base64_attachment_with_crlf_wrapped_lines() {
        let mut raw = String::from(
            "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--B\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\n\r\n",
        );
        raw.push_str(&crate::mime::encode_base64(b"%PDF-1.4 fake pdf bytes"));
        raw.push_str("\r\n--B--\r\n");
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("see attached"));
        let atts = msg.attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename().as_deref(), Some("report.pdf"));
        assert_eq!(atts[0].content, b"%PDF-1.4 fake pdf bytes");
    }

    #[test]
    fn malformed_boundary_degrades_to_text() {
        // The declared boundary never appears in the body.
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=\"NOPE\"\r\n",
            "\r\n",
            "This body has no boundary at all.\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert!(!msg.has_attachments());
        assert_eq!(msg.parts().len(), 1);
    }

    #[test]
    fn multipart_without_a_boundary_parameter_degrades_to_text() {
        let raw = concat!("Content-Type: multipart/mixed\r\n", "\r\n", "body bytes\r\n");
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.parts().len(), 1);
        assert!(!msg.parts()[0].is_multipart());
    }

    #[test]
    fn unterminated_multipart_still_yields_parts() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "one\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "two\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.parts()[0].parts.len(), 2);
    }

    #[test]
    fn handles_lf_only_and_bare_cr_messages() {
        let lf = ParsedMessage::parse(b"Subject: lf\nContent-Type: text/plain\n\nbody\n").unwrap();
        assert_eq!(lf.subject().as_deref(), Some("lf"));
        assert_eq!(lf.text_body().as_deref(), Some("body"));

        let cr = ParsedMessage::parse(b"Subject: cr\r\rbody\r").unwrap();
        assert_eq!(cr.subject().as_deref(), Some("cr"));
        assert_eq!(cr.text_body().as_deref(), Some("body"));
    }

    #[test]
    fn message_with_no_body_at_all() {
        let msg = ParsedMessage::parse(b"Subject: empty\r\n\r\n").unwrap();
        assert_eq!(msg.subject().as_deref(), Some("empty"));
        assert_eq!(msg.text_body().as_deref(), Some(""));
    }

    #[test]
    fn message_with_no_header_body_separator() {
        // Everything is headers; the body is empty rather than an error.
        let msg = ParsedMessage::parse(b"Subject: only headers\r\n").unwrap();
        assert_eq!(msg.subject().as_deref(), Some("only headers"));
        assert!(msg.body.iter().all(|p| p.content.is_empty()));
    }

    #[test]
    fn nested_multipart_is_flattened_into_a_tree() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=OUT\r\n",
            "\r\n",
            "--OUT\r\n",
            "Content-Type: multipart/alternative; boundary=IN\r\n",
            "\r\n",
            "--IN\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "plain\r\n",
            "--IN\r\n",
            "Content-Type: text/html\r\n",
            "\r\n",
            "<p>html</p>\r\n",
            "--IN--\r\n",
            "--OUT\r\n",
            "Content-Type: application/zip; name=\"a.zip\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"a.zip\"\r\n",
            "\r\n",
            "UEsDBA==\r\n",
            "--OUT--\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        let root = &msg.parts()[0];
        assert_eq!(root.parts.len(), 2);
        assert_eq!(root.parts[0].content_type.subtype, "alternative");
        assert_eq!(root.parts[0].parts.len(), 2);
        assert_eq!(msg.text_body().as_deref(), Some("plain"));
        assert_eq!(msg.html_body().as_deref(), Some("<p>html</p>"));
        assert_eq!(msg.attachments()[0].content, b"PK\x03\x04");
        assert_eq!(root.depth(), 3);
    }

    #[test]
    fn attached_message_is_kept_as_one_opaque_part() {
        let inner = "Subject: inner\r\nContent-Type: text/plain\r\n\r\nsecret inner body\r\n";
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nouter\r\n--B\r\nContent-Type: message/rfc822\r\nContent-Disposition: attachment; filename=\"fwd.eml\"\r\n\r\n{inner}--B--\r\n"
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("outer"));
        let atts = msg.attachments();
        assert_eq!(atts.len(), 1);
        assert!(atts[0].content_type.is_message());
        assert!(atts[0].parts.is_empty());
        assert!(String::from_utf8_lossy(&atts[0].content).contains("secret inner body"));
    }

    #[test]
    fn enforces_max_message_size() {
        let limits = ParseLimits {
            max_message_size: 10,
            ..ParseLimits::default()
        };
        let err =
            ParsedMessage::parse_with_limits(b"Subject: way too long\r\n\r\nbody", &limits)
                .unwrap_err();
        assert!(matches!(err, FerromaError::LimitExceeded(_)));
    }

    #[test]
    fn enforces_max_parts() {
        let mut raw = String::from("Content-Type: multipart/mixed; boundary=B\r\n\r\n");
        for i in 0..30 {
            raw.push_str(&format!("--B\r\nContent-Type: text/plain\r\n\r\npart {i}\r\n"));
        }
        raw.push_str("--B--\r\n");
        let limits = ParseLimits {
            max_parts: 5,
            ..ParseLimits::default()
        };
        let err = ParsedMessage::parse_with_limits(raw.as_bytes(), &limits).unwrap_err();
        assert!(matches!(err, FerromaError::LimitExceeded(_)));
        // The same message parses under the default limits.
        assert!(ParsedMessage::parse(raw.as_bytes()).is_ok());
    }

    #[test]
    fn enforces_max_depth() {
        // Six levels of nesting.
        let mut raw = String::new();
        for depth in 0..6 {
            raw.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=B{depth}\r\n\r\n--B{depth}\r\n"
            ));
        }
        raw.push_str("Content-Type: text/plain\r\n\r\ndeep\r\n");
        for depth in (0..6).rev() {
            raw.push_str(&format!("--B{depth}--\r\n"));
        }
        assert!(ParsedMessage::parse(raw.as_bytes()).is_ok());
        let limits = ParseLimits {
            max_depth: 3,
            ..ParseLimits::default()
        };
        let err = ParsedMessage::parse_with_limits(raw.as_bytes(), &limits).unwrap_err();
        assert!(matches!(err, FerromaError::LimitExceeded(_)));
    }

    #[test]
    fn limits_derive_from_platform_limits() {
        let platform = ferroma_core::Limits::default();
        let limits = ParseLimits::from_limits(&platform);
        assert_eq!(limits.max_depth, platform.max_mime_depth);
        assert_eq!(limits.max_message_size, 26_214_400);
        assert_eq!(limits.max_parts, 200);
        assert_eq!(ParseLimits::default().max_depth, 20);
        assert_eq!(ParseLimits::default().max_part_size, 26_214_400);
    }

    #[test]
    fn snippets_collapse_whitespace_and_truncate_on_a_boundary() {
        let raw = "Content-Type: text/plain; charset=utf-8\r\n\r\nHello   there\n\nworld\r\n";
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.snippet(100), "Hello there world");
        assert_eq!(msg.snippet(11), "Hello ther…");
        assert_eq!(msg.snippet(0), "");
        // Multi-byte characters must not be cut in half.
        let cn = "Content-Type: text/plain; charset=utf-8\r\n\r\n你好世界你好世界\r\n";
        let msg = ParsedMessage::parse(cn.as_bytes()).unwrap();
        assert_eq!(msg.snippet(5), "你好世界…");
        assert_eq!(msg.snippet(8), "你好世界你好世界");
    }

    #[test]
    fn snippet_falls_back_to_html() {
        let raw =
            "Content-Type: text/html\r\n\r\n<html><body><p>Hi <b>there</b></p></body></html>\r\n";
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.snippet(50), "Hi there");
    }

    #[test]
    fn attachment_parts_are_not_treated_as_the_body() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n",
            "Content-Disposition: attachment; filename=\"notes.txt\"\r\n",
            "\r\n",
            "I am an attachment.\r\n",
            "--B--\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body(), None);
        assert_eq!(msg.attachments().len(), 1);
    }

    #[test]
    fn whitespace_padded_boundary_lines_are_accepted() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B  \r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "padded\r\n",
            "--B--  \r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("padded"));
    }

    #[test]
    fn boundary_that_prefixes_another_boundary_does_not_confuse_the_split() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "first\r\n",
            "--B2\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "second\r\n",
            "--B--\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        // `--B2` is not a delimiter for boundary `B`, so both texts share a part.
        assert_eq!(msg.parts()[0].parts.len(), 1);
        assert!(msg.text_body().unwrap().contains("first"));
        assert!(msg.text_body().unwrap().contains("second"));
    }

    #[test]
    fn parsed_message_json_round_trip() {
        let msg = ParsedMessage::parse(THUNDERBIRD.as_bytes()).unwrap();
        let json = serde_json::to_string(&msg).unwrap();
        let back: ParsedMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn empty_input_is_an_empty_message() {
        let msg = ParsedMessage::parse(b"").unwrap();
        assert!(msg.headers.is_empty());
        assert_eq!(msg.text_body().as_deref(), Some(""));
        assert_eq!(msg.snippet(10), "");
    }

    #[test]
    fn html_only_message_has_no_plain_body() {
        let raw = "Content-Type: text/html; charset=utf-8\r\n\r\n<p>hi</p>\r\n";
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body(), None);
        assert_eq!(msg.html_body().as_deref(), Some("<p>hi</p>"));
    }

    #[test]
    fn decode_part_text_honours_an_explicit_charset() {
        let part = MimePart::leaf(
            ContentType::parse("text/plain"),
            vec![0xd6, 0xd0, 0xce, 0xc4],
        );
        assert_eq!(decode_part_text(&part, "gbk"), "中文");
    }

    #[test]
    fn crlf_lf_and_cr_separators_find_the_same_split() {
        for raw in [
            &b"Subject: s\r\n\r\nbody\r\n"[..],
            &b"Subject: s\n\nbody\n"[..],
            &b"Subject: s\r\rbody\r"[..],
        ] {
            let msg = ParsedMessage::parse(raw).unwrap();
            assert_eq!(msg.subject().as_deref(), Some("s"));
            assert_eq!(msg.text_body().as_deref(), Some("body"));
        }
    }

    #[test]
    fn deeply_nested_multipart_builds_the_expected_tree() {
        let mut raw = String::new();
        for depth in 0..10 {
            raw.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=D{depth}\r\n\r\n--D{depth}\r\n"
            ));
        }
        raw.push_str("Content-Type: text/plain\r\n\r\ndeep body\r\n");
        for depth in (0..10).rev() {
            raw.push_str(&format!("--D{depth}--\r\n"));
        }
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        let mut node = &msg.parts()[0];
        let mut depth = 1;
        while node.is_multipart() {
            assert_eq!(node.subparts().len(), 1);
            node = &node.subparts()[0];
            depth += 1;
        }
        assert_eq!(depth, 11, "ten containers plus the leaf");
        assert_eq!(msg.text_body().as_deref(), Some("deep body"));
        assert_eq!(msg.parts()[0].depth(), 11);
    }

    #[test]
    fn preamble_and_epilogue_are_discarded() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "This is the preamble and is not a part.\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "the body\r\n",
            "--B--\r\n",
            "This is the epilogue and is not a part.\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.parts()[0].parts.len(), 1);
        assert_eq!(msg.text_body().as_deref(), Some("the body"));
    }

    #[test]
    fn empty_mime_part_is_tolerated() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B\r\n",
            "\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "second\r\n",
            "--B--\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        let parts = msg.parts()[0].subparts();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].content.is_empty());
        // The first (empty) text part wins, because it really does come first.
        assert_eq!(msg.text_body().as_deref(), Some(""));
        assert_eq!(parts[1].content, b"second");
    }

    #[test]
    fn parts_without_a_content_type_default_to_plain_text() {
        let raw =
            "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\n\r\nbare part\r\n--B--\r\n";
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        let part = &msg.parts()[0].subparts()[0];
        assert_eq!(part.content_type.type_, "text");
        assert_eq!(part.content_type.subtype, "plain");
        assert_eq!(part.content, b"bare part");
        assert_eq!(msg.text_body().as_deref(), Some("bare part"));
    }

    #[test]
    fn base64_body_with_invalid_data_degrades_instead_of_failing() {
        let raw = concat!(
            "Content-Type: text/plain\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "!!!not base64 at all!!!\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).expect("must not fail");
        // Whatever came out, the parse succeeded and the part exists.
        assert_eq!(msg.parts().len(), 1);
    }

    #[test]
    fn quoted_printable_body_with_an_invalid_soft_break() {
        let raw = concat!(
            "Content-Type: text/plain\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "ends with a lone equals=\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        // A trailing CRLF is framing, not part of the body, so the `=` has
        // nothing to escape and stays literal.
        assert_eq!(msg.text_body().as_deref(), Some("ends with a lone equals="));
    }

    #[test]
    fn quoted_printable_soft_break_inside_the_body_still_joins() {
        let raw = concat!(
            "Content-Type: text/plain\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "one=\r\n",
            "two\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("onetwo"));
    }

    #[test]
    fn unknown_content_transfer_encoding_is_treated_as_seven_bit() {
        let raw = concat!(
            "Content-Type: text/plain\r\n",
            "Content-Transfer-Encoding: x-uuencode\r\n",
            "\r\n",
            "raw bytes\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("raw bytes"));
        assert_eq!(msg.parts()[0].encoding, TransferEncoding::SevenBit);
    }

    #[test]
    fn attachment_only_message_has_no_body() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"a.pdf\"\r\n",
            "\r\n",
            "JVBERi0xLjQ=\r\n",
            "--B--\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.text_body(), None);
        assert_eq!(msg.html_body(), None);
        assert!(msg.has_attachments());
        assert_eq!(msg.attachments()[0].content, b"%PDF-1.4");
        assert_eq!(msg.attachments()[0].decode_text(), None);
    }

    #[test]
    fn text_attachment_can_still_be_decoded_as_text() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B\r\n",
            "Content-Type: text/csv; charset=utf-8\r\n",
            "Content-Disposition: attachment; filename=\"a.csv\"\r\n",
            "\r\n",
            "a,b\r\n",
            "--B--\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        let att = msg.attachments()[0];
        assert_eq!(att.decode_text().as_deref(), Some("a,b"));
        assert_eq!(msg.text_body(), None);
    }

    #[test]
    fn headers_are_available_on_every_part() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n",
            "\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n",
            "X-Part-Header: value\r\n",
            "\r\n",
            "body\r\n",
            "--B--\r\n",
        );
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        let part = &msg.parts()[0].subparts()[0];
        assert_eq!(part.headers.get("X-Part-Header"), Some("value"));
    }

    #[test]
    fn snippet_truncates_at_exactly_the_requested_length() {
        let raw = "Content-Type: text/plain\r\n\r\nabcdefghij\r\n";
        let msg = ParsedMessage::parse(raw.as_bytes()).unwrap();
        assert_eq!(msg.snippet(1), "…");
        assert_eq!(msg.snippet(5), "abcd…");
        assert_eq!(msg.snippet(10), "abcdefghij");
        assert_eq!(msg.snippet(11), "abcdefghij");
    }

    #[test]
    fn parse_limits_json_round_trip() {
        let limits = ParseLimits::default();
        let json = serde_json::to_string(&limits).unwrap();
        let back: ParseLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(limits, back);
    }

    #[test]
    fn a_message_exactly_at_the_size_limit_is_accepted() {
        let raw = b"Subject: x\r\n\r\n";
        let limits = ParseLimits {
            max_message_size: raw.len(),
            ..ParseLimits::default()
        };
        assert!(ParsedMessage::parse_with_limits(raw, &limits).is_ok());
        let limits = ParseLimits {
            max_message_size: raw.len() - 1,
            ..ParseLimits::default()
        };
        assert!(ParsedMessage::parse_with_limits(raw, &limits).is_err());
    }
}
