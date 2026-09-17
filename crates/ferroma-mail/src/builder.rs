//! Assembling outgoing messages.
//!
//! [`MessageBuilder`] turns "a subject, a body and some attachments" into the
//! exact bytes an SMTP `DATA` phase should carry. The rules it implements:
//!
//! * text only → `text/plain`; HTML only → `text/html`; both →
//!   `multipart/alternative` (plain first, as RFC 2046 §5.1.4 requires).
//! * any attachment → the whole thing is wrapped in `multipart/mixed`.
//! * a body that is not pure US-ASCII is encoded as **quoted-printable** with
//!   `Content-Transfer-Encoding: quoted-printable`; an ASCII body is sent
//!   verbatim as `7bit`. (Quoted-printable is chosen over base64 for bodies
//!   because an ASCII-only message stays readable in transit and in logs, and
//!   because the encoder guarantees the 998-octet line limit.)
//! * attachments are always base64, wrapped at 76 columns.
//! * `MIME-Version`, `Date` and `Message-ID` are filled in when the caller did
//!   not supply them.
//! * non-ASCII `Subject` and display names become RFC 2047 encoded words.
//!
//! Everything is emitted with CRLF line endings, and
//! [`MessageBuilder::build_message`] proves the result round-trips through
//! [`crate::message::ParsedMessage`].

use chrono::{DateTime, SecondsFormat, Utc};
use ferroma_core::{Result, RfcMessageId};

use crate::address::{format_address_list, parse_address_list, Mailbox};
use crate::headers::{encode_header_value, fold_header_line, Headers};
use crate::message::ParsedMessage;
use crate::mime::{encode_base64, encode_quoted_printable, ContentType};

/// The longest line a generated body is allowed to contain, in octets.
///
/// RFC 5322 §2.1.1 permits 998; we stay well below it, which keeps the message
/// safe through the transit agents that fold or re-wrap long lines. A body that
/// already contains a longer line is sent quoted-printable instead of `7bit`.
pub const MAX_BODY_LINE: usize = 78;

/// One attachment queued on the builder.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    /// The filename the recipient should see.
    pub filename: String,
    /// The MIME type, e.g. `application/pdf`.
    pub content_type: String,
    /// The raw, undecoded bytes.
    pub data: Vec<u8>,
}

impl Attachment {
    /// Queue an attachment.
    pub fn new(filename: impl Into<String>, content_type: impl Into<String>, data: Vec<u8>) -> Self {
        Attachment {
            filename: filename.into(),
            content_type: content_type.into(),
            data,
        }
    }
}

/// A fluent builder for RFC 5322 messages.
///
/// ```
/// use ferroma_mail::MessageBuilder;
///
/// # fn main() -> Result<(), ferroma_core::FerromaError> {
/// let raw = MessageBuilder::new()
///     .from("Alice <alice@example.com>")
///     .to("bob@example.org")
///     .subject("Hello")
///     .text("Hi there")
///     .build()?;
///
/// assert!(raw.ends_with(b"\r\n"));
/// assert!(String::from_utf8_lossy(&raw).contains("Content-Type: text/plain; charset=utf-8"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct MessageBuilder {
    from: Vec<String>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    reply_to: Vec<String>,
    subject: Option<String>,
    text: Option<String>,
    html: Option<String>,
    headers: Headers,
    attachments: Vec<Attachment>,
    in_reply_to: Option<String>,
    references: Vec<String>,
    date: Option<DateTime<Utc>>,
    message_id: Option<RfcMessageId>,
    /// Domain used to mint a `Message-ID` when the caller did not supply one.
    domain: Option<String>,
}

impl MessageBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        MessageBuilder::default()
    }

    /// Set the `From` address (as a header value: a display name is allowed).
    pub fn from(mut self, value: impl AsRef<str>) -> Self {
        self.from.push(value.as_ref().to_string());
        self
    }

    /// Add a `To` recipient.
    pub fn to(mut self, value: impl AsRef<str>) -> Self {
        self.to.push(value.as_ref().to_string());
        self
    }

    /// Add a `Cc` recipient.
    pub fn cc(mut self, value: impl AsRef<str>) -> Self {
        self.cc.push(value.as_ref().to_string());
        self
    }

    /// Add a `Bcc` recipient.
    ///
    /// The header is written but, for a message built for one Bcc recipient,
    /// callers that want the classic "hide the copies from each other" behaviour
    /// should send one copy per recipient and strip the field themselves; the
    /// builder keeps the field because it is also the only place a *saved*
    /// draft records those recipients.
    pub fn bcc(mut self, value: impl AsRef<str>) -> Self {
        self.bcc.push(value.as_ref().to_string());
        self
    }

    /// Add a `Reply-To` address.
    pub fn reply_to(mut self, value: impl AsRef<str>) -> Self {
        self.reply_to.push(value.as_ref().to_string());
        self
    }

    /// Set the subject (RFC 2047 encoded on output when it is not ASCII).
    pub fn subject(mut self, value: &str) -> Self {
        self.subject = Some(value.to_string());
        self
    }

    /// Set the plain-text body.
    pub fn text(mut self, value: &str) -> Self {
        self.text = Some(value.to_string());
        self
    }

    /// Set the HTML body.
    pub fn html(mut self, value: &str) -> Self {
        self.html = Some(value.to_string());
        self
    }

    /// Set an arbitrary header field, replacing any previous value.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Queue an attachment; its bytes are base64-encoded on output.
    pub fn attachment(mut self, filename: &str, content_type: &str, data: Vec<u8>) -> Self {
        self.attachments
            .push(Attachment::new(filename, content_type, data));
        self
    }

    /// Set `In-Reply-To` from a message id (angle brackets are added if missing).
    pub fn in_reply_to(mut self, value: &str) -> Self {
        self.in_reply_to = Some(normalise_msgid(value));
        self
    }

    /// Set `References` from a thread of message ids.
    pub fn references(mut self, ids: &[String]) -> Self {
        self.references = ids.iter().map(|id| normalise_msgid(id)).collect();
        self
    }

    /// Set `Date` explicitly instead of using the current time.
    pub fn date(mut self, when: DateTime<Utc>) -> Self {
        self.date = Some(when);
        self
    }

    /// Set `Message-ID` explicitly instead of generating one.
    pub fn message_id(mut self, id: RfcMessageId) -> Self {
        self.message_id = Some(id);
        self
    }

    /// The domain used to mint a `Message-ID` when none was supplied.
    pub fn domain(mut self, value: &str) -> Self {
        let value = value.trim();
        if !value.is_empty() {
            self.domain = Some(value.to_string());
        }
        self
    }

    /// Build the complete message: headers, a blank line, and the body, all with
    /// CRLF line endings and a trailing CRLF.
    pub fn build(self) -> Result<Vec<u8>> {
        let (body, content_type, transfer_encoding) = self.final_body();
        let headers = self.build_headers(&content_type, transfer_encoding.as_deref());

        let mut out = headers.render().into_bytes();
        out.extend_from_slice(body.as_bytes());
        if !out.ends_with(b"\r\n") {
            out.extend_from_slice(b"\r\n");
        }
        Ok(out)
    }

    /// The body as it will be sent, plus the `Content-Type` (and optional
    /// `Content-Transfer-Encoding`) that belongs in the message header.
    ///
    /// For a single leaf the entity's own header block is lifted out into the
    /// message header. When there are attachments the body keeps that block,
    /// because it becomes the first part of a `multipart/mixed` container, and the
    /// container's `Content-Type` is what the message header gets instead.
    fn final_body(&self) -> (String, String, Option<String>) {
        let body = self.build_body();

        if self.attachments.is_empty() {
            let (content_type, encoding) = body_top_headers(&body);
            if let Some(rest) = strip_header_block(&body) {
                return (rest, content_type, encoding);
            }
            return (body, content_type, encoding);
        }

        let boundary = self.boundary_for(0);
        let mut out = format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n");
        out.push_str(&format!("--{boundary}\r\n"));
        out.push_str(&body);
        if !out.ends_with("\r\n") {
            out.push_str("\r\n");
        }
        for attachment in &self.attachments {
            out.push_str(&format!("--{boundary}\r\n"));
            out.push_str(&render_attachment(attachment));
        }
        out.push_str(&format!("--{boundary}--\r\n"));
        (
            out,
            format!("multipart/mixed; boundary=\"{boundary}\""),
            None,
        )
    }

    /// Build the message and immediately parse it back.
    ///
    /// This is the self-check the API layer uses before queueing a message: if
    /// the bytes we produced cannot be read by our own parser, the message is
    /// not fit to send.
    pub fn build_message(self) -> Result<ParsedMessage> {
        let raw = self.build()?;
        ParsedMessage::parse(&raw)
    }

    /// The addresses the message should be delivered to: `To` + `Cc` + `Bcc`.
    pub fn recipients(&self) -> Vec<Mailbox> {
        let mut out = Vec::new();
        for value in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            out.extend(parse_address_list(value));
        }
        out
    }

    /// Render the body, choosing the right MIME structure.
    ///
    /// The result is a complete entity: it starts with the body's own
    /// `Content-Type` header block and the blank line that ends it, so the top
    /// level can lift those header lines out and put them in the message header,
    /// or wrap the whole entity in `multipart/mixed` for attachments.
    fn build_body(&self) -> String {
        let mut leaves: Vec<(String, Vec<u8>)> = Vec::new();

        if let Some(text) = &self.text {
            leaves.push(self.render_text_leaf("plain", text));
        }
        if let Some(html) = &self.html {
            leaves.push(self.render_text_leaf("html", html));
        }
        if leaves.is_empty() {
            // A message with no body is still a valid message: send an empty
            // text/plain part so the structure is well-formed.
            leaves.push(self.render_text_leaf("plain", ""));
        }

        if leaves.len() == 1 {
            let (header, bytes) = leaves.remove(0);
            return render_leaf(&header, &bytes);
        }

        let boundary = self.boundary_for(1);
        let mut out =
            format!("Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n");
        for (header, bytes) in &leaves {
            out.push_str(&format!("--{boundary}\r\n"));
            out.push_str(&render_leaf(header, bytes));
        }
        out.push_str(&format!("--{boundary}--\r\n"));
        out
    }

    /// Render one text leaf: its header lines plus its encoded bytes.
    ///
    /// A body goes out as `7bit` only when it is US-ASCII *and* every line already
    /// fits [`MAX_BODY_LINE`]. Long lines — which RFC 5322 §2.1.1 forbids in a
    /// `7bit` body — and non-ASCII text both go through the quoted-printable
    /// encoder, which is what keeps the 998-octet limit honest.
    fn render_text_leaf(&self, subtype: &str, text: &str) -> (String, Vec<u8>) {
        let normalized = normalise_newlines(text);
        if normalized.is_ascii() && has_short_lines(&normalized) {
            let header = format!(
                "Content-Type: text/{subtype}; charset=utf-8\r\nContent-Transfer-Encoding: 7bit"
            );
            (header, normalized.into_bytes())
        } else {
            let header = format!(
                "Content-Type: text/{subtype}; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable"
            );
            let encoded = encode_quoted_printable(normalized.as_bytes());
            (header, encoded.into_bytes())
        }
    }

    /// A boundary that cannot occur in any part of the message.
    fn boundary_for(&self, salt: u64) -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for _ in 0..64 {
            let candidate = format!(
                "=_ferroma_{:016x}_{:08x}_{}",
                rng.gen::<u64>(),
                rng.gen::<u32>(),
                salt
            );
            if !self.content_contains(&candidate) {
                return candidate;
            }
        }
        // Astronomically unlikely; still deterministic and syntactically valid.
        format!("=_ferroma_fallback_{salt}")
    }

    /// Whether any queued content already contains `needle`.
    fn content_contains(&self, needle: &str) -> bool {
        let texts = [self.text.as_deref(), self.html.as_deref()];
        if texts.iter().flatten().any(|t| t.contains(needle)) {
            return true;
        }
        self.attachments.iter().any(|a| {
            a.filename.contains(needle) || a.content_type.contains(needle)
        })
    }

    /// Render the full header block, including the body's `Content-Type`.
    fn build_headers(&self, content_type: &str, transfer_encoding: Option<&str>) -> Headers {
        let mut headers = Headers::new();

        for value in &self.from {
            for mailbox in parse_address_list(value) {
                headers.append("From", &mailbox.display());
            }
            if parse_address_list(value).is_empty() {
                headers.append("From", value);
            }
        }
        append_address_header(&mut headers, "To", &self.to);
        append_address_header(&mut headers, "Cc", &self.cc);
        append_address_header(&mut headers, "Bcc", &self.bcc);
        append_address_header(&mut headers, "Reply-To", &self.reply_to);

        if let Some(subject) = &self.subject {
            headers.insert("Subject", &encode_header_value(subject));
        }

        // Caller-supplied headers come next so that they can override the
        // automatic ones, except for the structural headers below.
        for (name, value) in self.headers.iter() {
            if is_structural(name) {
                continue;
            }
            headers.insert(name, value);
        }

        if let Some(in_reply_to) = &self.in_reply_to {
            headers.insert("In-Reply-To", in_reply_to);
        }
        if !self.references.is_empty() {
            headers.insert("References", &self.references.join(" "));
        }

        if !headers.contains("Date") {
            let when = self.date.unwrap_or_else(Utc::now);
            headers.insert("Date", &render_date(when));
        }
        if !headers.contains("Message-ID") {
            let id = match &self.message_id {
                Some(id) => id.clone(),
                None => RfcMessageId::generate(&self.message_domain()),
            };
            headers.insert("Message-ID", id.as_str());
        }
        if !headers.contains("MIME-Version") {
            headers.insert("MIME-Version", "1.0");
        }

        // The body's own Content-Type and transfer encoding describe the message.
        headers.insert("Content-Type", content_type);
        match transfer_encoding {
            Some(encoding) => headers.insert("Content-Transfer-Encoding", encoding),
            None => headers.remove("Content-Transfer-Encoding"),
        }

        headers
    }

    /// The domain a generated `Message-ID` should use: the explicitly configured
    /// one, else the domain of the first `From` address, else `ferroma.local`.
    fn message_domain(&self) -> String {
        if let Some(domain) = &self.domain {
            if !domain.is_empty() {
                return domain.clone();
            }
        }
        if let Some(first) = self.from.first() {
            if let Some(mailbox) = parse_address_list(first).first() {
                return mailbox.address.domain().to_string();
            }
        }
        "ferroma.local".to_string()
    }
}

/// Append an address-valued header, normalising display names.
fn append_address_header(headers: &mut Headers, name: &str, values: &[String]) {
    for value in values {
        let list = parse_address_list(value);
        if list.is_empty() {
            // Keep what the caller wrote so nothing is silently dropped.
            headers.append(name, value);
        } else {
            headers.append(name, &format_address_list(&list));
        }
    }
}

/// Headers the builder always computes itself.
fn is_structural(name: &str) -> bool {
    ["Content-Type", "Content-Transfer-Encoding", "MIME-Version", "Date", "Message-ID"]
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
}

/// Render a leaf entity: its header lines, a blank line, and the body.
fn render_leaf(header: &str, bytes: &[u8]) -> String {
    let mut out = String::new();
    for line in header.split("\r\n") {
        if let Some((name, value)) = line.split_once(": ") {
            out.push_str(&fold_header_line(name, value));
            out.push_str("\r\n");
        }
    }
    out.push_str("\r\n");
    out.push_str(&String::from_utf8_lossy(bytes));
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out
}

/// Render an attachment as a MIME entity.
fn render_attachment(attachment: &Attachment) -> String {
    let content_type = if attachment.content_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        attachment.content_type.trim().to_string()
    };
    let content_type = ContentType::parse(&content_type);
    let mut out = String::new();

    if content_type.type_ == "text" {
        // A text attachment is still an attachment; keep the charset explicit.
        out.push_str(&format!(
            "Content-Type: {}; charset=utf-8\r\n",
            content_type.to_string()
        ));
    } else {
        out.push_str(&format!(
            "Content-Type: {}; name=\"{}\"\r\n",
            content_type.to_string(),
            escape_param(&attachment.filename)
        ));
    }
    out.push_str("Content-Transfer-Encoding: base64\r\n");
    out.push_str(&format!(
        "Content-Disposition: attachment; filename=\"{}\"\r\n",
        escape_param(&attachment.filename)
    ));
    out.push_str("\r\n");
    out.push_str(&encode_base64(&attachment.data));
    out.push_str("\r\n");
    out
}

/// Escape a parameter value for use inside a quoted string.
fn escape_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '\r' | '\n' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

/// Remove the leading `Content-…` header block (and the blank line that ends it)
/// from a single-part entity, returning `None` when the entity is not a leaf
/// (a `multipart/*` entity has no header block to lift out).
fn strip_header_block(entity: &str) -> Option<String> {
    if !entity.starts_with("Content-") {
        return None;
    }
    // Only a leaf carries `Content-Type`/`Content-Transfer-Encoding` here; a
    // multipart body starts with `Content-Type: multipart/…` and keeps its block.
    let first_line = entity.split("\r\n").next().unwrap_or("");
    if first_line
        .to_ascii_lowercase()
        .starts_with("content-type: multipart/")
    {
        return None;
    }
    let pos = entity.find("\r\n\r\n")?;
    Some(entity[pos + 4..].to_string())
}

/// The `Content-Type` (and optional transfer encoding) the body starts with.
fn body_top_headers(body: &str) -> (String, Option<String>) {
    let headers = body.split("\r\n\r\n").next().unwrap_or("");
    // A headerless body (which cannot happen with our own writers) means text.
    if !headers.starts_with("Content-") {
        return ("text/plain; charset=utf-8".to_string(), None);
    }
    let mut content_type = None;
    let mut encoding = None;
    for line in headers.split("\r\n").map(|l| l.trim()) {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("Content-Type") {
                content_type = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("Content-Transfer-Encoding") {
                encoding = Some(value.to_string());
            }
        }
    }
    (
        content_type.unwrap_or_else(|| "text/plain; charset=utf-8".to_string()),
        encoding,
    )
}

/// Whether every line of `text` is short enough for a `7bit` body.
fn has_short_lines(text: &str) -> bool {
    text.split("\r\n").all(|line| line.len() <= MAX_BODY_LINE)
}

/// Convert every line ending in `text` to CRLF.
fn normalise_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                out.push_str("\r\n");
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                out.push_str("\r\n");
                i += 1;
            }
            _ => {
                // Copy whole characters so multi-byte text stays intact.
                let start = i;
                let ch_len = utf8_len(bytes[i]);
                i = (i + ch_len).min(bytes.len());
                out.push_str(&text[start..i]);
            }
        }
    }
    out
}

/// Length of the UTF-8 sequence starting with `first`.
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // A continuation byte on its own (impossible in a `&str`) or an invalid
        // lead: step one byte so we never loop forever.
        _ => 1,
    }
}

/// Normalise a message id to the `<...>` form.
fn normalise_msgid(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') && trimmed.len() >= 2 {
        trimmed.to_string()
    } else {
        format!("<{trimmed}>")
    }
}

/// Render a `Date` header per RFC 5322 §3.3.
fn render_date(when: DateTime<Utc>) -> String {
    let _ = SecondsFormat::Secs;
    when.format("%a, %d %b %Y %H:%M:%S %z").to_string()
}

/// The longest body line the builder will emit verbatim, in octets.
///
/// Exposed so the storage layer and the SMTP `DATA` writer can validate the same
/// invariant the builder guarantees.
pub fn max_body_line() -> usize {
    MAX_BODY_LINE
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn text_only_message_is_text_plain() {
        let msg = MessageBuilder::new()
            .from("Alice <alice@example.com>")
            .to("bob@example.org")
            .subject("Hello")
            .text("Hi there")
            .build_message()
            .unwrap();

        assert_eq!(msg.subject().as_deref(), Some("Hello"));
        assert_eq!(msg.text_body().as_deref(), Some("Hi there"));
        assert_eq!(msg.html_body(), None);
        assert!(!msg.is_multipart());
        assert_eq!(msg.header("MIME-Version"), Some("1.0"));
        assert!(msg.header("Date").is_some());
        assert!(msg.message_id().is_some());
        let ct = msg.parts()[0].content_type.to_string();
        assert_eq!(ct, "text/plain; charset=utf-8");
        assert_eq!(msg.parts()[0].encoding, crate::mime::TransferEncoding::SevenBit);
    }

    #[test]
    fn html_only_message_is_text_html() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("Hi")
            .html("<p>Hi <b>there</b></p>")
            .build_message()
            .unwrap();
        assert_eq!(msg.html_body().as_deref(), Some("<p>Hi <b>there</b></p>"));
        assert_eq!(msg.text_body(), None);
        assert!(!msg.is_multipart());
        assert_eq!(msg.parts()[0].content_type.subtype, "html");
        assert_eq!(msg.header("Content-Type"), Some("text/html; charset=utf-8"));
    }

    #[test]
    fn text_and_html_become_alternative_with_plain_first() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("Both")
            .text("plain body")
            .html("<p>html body</p>")
            .build_message()
            .unwrap();

        assert!(msg.is_multipart());
        let root = &msg.parts()[0];
        assert_eq!(root.content_type.to_string().split(';').next().unwrap(), "multipart/alternative");
        assert_eq!(root.parts.len(), 2);
        assert_eq!(root.parts[0].content_type.subtype, "plain");
        assert_eq!(root.parts[1].content_type.subtype, "html");
        assert_eq!(msg.text_body().as_deref(), Some("plain body"));
        assert_eq!(msg.html_body().as_deref(), Some("<p>html body</p>"));
    }

    #[test]
    fn an_attachment_wraps_the_body_in_multipart_mixed() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("With attachment")
            .text("see attached")
            .attachment("notes.txt", "text/plain", b"attachment body".to_vec())
            .build_message()
            .unwrap();

        assert!(msg.is_multipart());
        let root = &msg.parts()[0];
        assert!(root.is_multipart());
        assert_eq!(root.parts.len(), 2);
        assert_eq!(msg.text_body().as_deref(), Some("see attached"));
        let atts = msg.attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename().as_deref(), Some("notes.txt"));
        assert_eq!(atts[0].content, b"attachment body");
        assert_eq!(atts[0].encoding, crate::mime::TransferEncoding::Base64);
    }

    #[test]
    fn binary_attachment_round_trips_byte_for_byte() {
        let data: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("Binary")
            .text("body")
            .attachment("blob.bin", "application/octet-stream", data.clone())
            .build_message()
            .unwrap();
        let atts = msg.attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].content, data);
    }

    #[test]
    fn non_ascii_text_body_uses_quoted_printable_and_survives() {
        let body = "你好，世界！This line is long enough to require a soft line break somewhere in the middle of it.";
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("中文")
            .text(body)
            .build_message()
            .unwrap();
        assert_eq!(msg.text_body().as_deref(), Some(body));
        assert_eq!(msg.subject().as_deref(), Some("中文"));
        assert_eq!(msg.parts()[0].encoding, crate::mime::TransferEncoding::QuotedPrintable);
    }

    #[test]
    fn subject_is_rfc2047_encoded_only_when_needed() {
        let plain = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("Plain ascii")
            .text("x")
            .build()
            .unwrap();
        let raw = String::from_utf8(plain).unwrap();
        assert!(raw.contains("Subject: Plain ascii\r\n"));

        let encoded = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("中文主题")
            .text("x")
            .build()
            .unwrap();
        let raw = String::from_utf8(encoded).unwrap();
        assert!(raw.contains("Subject: =?UTF-8?B?"), "got {raw}");
        assert!(!raw.contains("Subject: 中文"));
    }

    #[test]
    fn display_names_are_encoded_and_quoted_as_needed() {
        let msg = MessageBuilder::new()
            .from("张三 <zhang@example.cn>")
            .to("\"Doe, John\" <john@example.com>")
            .subject("s")
            .text("b")
            .build_message()
            .unwrap();

        let from = msg.from();
        assert_eq!(from[0].name.as_deref(), Some("张三"));
        assert_eq!(from[0].address.to_string(), "zhang@example.cn");
        let to = msg.to();
        assert_eq!(to[0].name.as_deref(), Some("Doe, John"));
    }

    #[test]
    fn every_required_header_is_present() {
        let raw = MessageBuilder::new()
            .from("Alice <alice@example.com>")
            .to("bob@example.org")
            .subject("Headers")
            .text("body")
            .build()
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        for expected in [
            "From: Alice <alice@example.com>\r\n",
            "To: bob@example.org\r\n",
            "Subject: Headers\r\n",
            "MIME-Version: 1.0\r\n",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text}");
        }
        assert!(text.contains("\r\nDate: "));
        assert!(text.contains("\r\nMessage-ID: <"));
        assert!(text.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(text.contains("Content-Transfer-Encoding: 7bit\r\n"));
        assert!(text.ends_with("\r\n"));
    }

    #[test]
    fn explicit_date_and_message_id_are_respected() {
        let when = DateTime::parse_from_rfc3339("2020-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("b")
            .date(when)
            .message_id(RfcMessageId::new("fixed@example.com"))
            .build_message()
            .unwrap();
        assert_eq!(msg.date().unwrap(), when);
        assert_eq!(msg.message_id().unwrap().inner(), "fixed@example.com");
    }

    #[test]
    fn thread_headers_are_normalised() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("re")
            .text("b")
            .in_reply_to("parent@example.com")
            .references(&["a@example.com".to_string()])
            .build_message()
            .unwrap();
        assert_eq!(msg.header("In-Reply-To"), Some("<parent@example.com>"));

        let msg = MessageBuilder::new()
            .subject("re")
            .text("b")
            .in_reply_to("<already@bracketed>")
            .references(&["<one@x>".to_string(), "two@x".to_string()])
            .build_message()
            .unwrap();
        assert_eq!(msg.header("In-Reply-To"), Some("<already@bracketed>"));
        assert_eq!(msg.header("References"), Some("<one@x> <two@x>"));
    }

    #[test]
    fn custom_headers_are_written_and_can_be_overridden() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .header("X-Mailer", "Ferroma Test")
            .header("X-Mailer", "Ferroma Test 2")
            .header("Content-Type", "application/ignored")
            .text("b")
            .build_message()
            .unwrap();
        assert_eq!(msg.header("X-Mailer"), Some("Ferroma Test 2"));
        // Structural headers cannot be forged.
        assert_eq!(msg.parts()[0].content_type.type_, "text");
    }

    #[test]
    fn multiple_recipients_accumulate_across_calls() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .to("c@example.org, d@example.org")
            .cc("e@example.org")
            .build_message()
            .unwrap();
        assert_eq!(msg.to().len(), 3);
        assert_eq!(msg.cc().len(), 1);
        // Repeated `to()` calls are appended as further `To` fields, which is
        // legal and is what lets `get_all` see every recipient.
        let all = msg.headers.get_all("To").join(", ");
        assert_eq!(all.matches(',').count(), 2);
        assert_eq!(msg.header("To"), Some("b@example.org"));
    }

    #[test]
    fn recipients_helper_covers_to_cc_and_bcc() {
        let builder = MessageBuilder::new()
            .to("b@example.org")
            .cc("c@example.org")
            .bcc("d@example.org");
        let recipients = builder.recipients();
        assert_eq!(recipients.len(), 3);
        assert_eq!(recipients[2].address.to_string(), "d@example.org");
    }

    #[test]
    fn output_uses_crlf_throughout() {
        let raw = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("crlf")
            .text("one\ntwo\r\nthree")
            .build()
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(!text.replace("\r\n", "").contains('\n'), "bare LF present");
        let msg = ParsedMessage::parse(text.as_bytes()).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("one\r\ntwo\r\nthree"));
    }

    #[test]
    fn empty_builder_still_produces_a_parseable_message() {
        let msg = MessageBuilder::new().build_message().unwrap();
        assert_eq!(msg.text_body().as_deref(), Some(""));
        assert!(msg.header("Message-ID").is_some());
        assert!(msg.header("MIME-Version").is_some());
    }

    #[test]
    fn generated_message_id_uses_the_from_domain() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("b")
            .build_message()
            .unwrap();
        assert_eq!(msg.message_id().unwrap().domain(), Some("example.com"));
    }

    #[test]
    fn explicit_domain_wins_for_generated_ids() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .domain("mail.ferroma.test")
            .subject("s")
            .text("b")
            .build_message()
            .unwrap();
        assert_eq!(
            msg.message_id().unwrap().domain(),
            Some("mail.ferroma.test")
        );
    }

    #[test]
    fn body_lines_stay_within_the_limit() {
        let long = "lorem ipsum dolor sit amet ".repeat(40);
        let long = long.trim_end();
        let raw = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("long")
            .text(long)
            .attachment("a.bin", "application/octet-stream", vec![0u8; 500])
            .build()
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        for line in text.split("\r\n") {
            if line.len() > 998 {
                panic!("line of {} octets", line.len());
            }
        }
        let msg = ParsedMessage::parse(text.as_bytes()).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some(long));
    }

    #[test]
    fn boundaries_never_appear_in_the_content() {
        // The body deliberately contains the literal text a boundary starts with.
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("boundary")
            .text("=_ferroma_ boundary lookalike -- \r\n")
            .html("<p>=_ferroma_</p>")
            .attachment("=_ferroma_x.bin", "application/octet-stream", vec![1, 2, 3])
            .build_message()
            .unwrap();
        // The framing CRLF that precedes a boundary belongs to the boundary, so it
        // is not part of the decoded content.
        assert_eq!(
            msg.text_body().as_deref(),
            Some("=_ferroma_ boundary lookalike -- ")
        );
        assert_eq!(msg.attachments().len(), 1);
        assert_eq!(msg.attachments()[0].content, vec![1, 2, 3]);
    }

    #[test]
    fn attachment_filenames_with_specials_round_trip() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("b")
            .attachment("报告 \"final\".pdf", "application/pdf", b"%PDF".to_vec())
            .build_message()
            .unwrap();
        assert_eq!(
            msg.attachments()[0].filename().as_deref(),
            Some("报告 \"final\".pdf")
        );
    }

    #[test]
    fn text_attachments_keep_their_own_content_type() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("body")
            .attachment("data.csv", "text/csv", b"a,b,c\r\n1,2,3".to_vec())
            .build_message()
            .unwrap();
        let atts = msg.attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].content_type.type_, "text");
        assert_eq!(atts[0].content_type.subtype, "csv");
        assert_eq!(atts[0].content, b"a,b,c\r\n1,2,3");
    }

    #[test]
    fn blank_content_type_falls_back_to_octet_stream() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("body")
            .attachment("x.bin", "  ", vec![9])
            .build_message()
            .unwrap();
        let atts = msg.attachments();
        assert_eq!(atts[0].content_type.type_, "application");
        assert_eq!(atts[0].content_type.subtype, "octet-stream");
    }

    #[test]
    fn round_trip_text_only_is_lossless() {
        let body = "Line one\r\nLine two with trailing spaces   \r\n\r\nLine four";
        let raw = MessageBuilder::new()
            .from("Alice <alice@example.com>")
            .to("bob@example.org")
            .subject("Round trip")
            .text(body)
            .build()
            .unwrap();
        let msg = ParsedMessage::parse(&raw).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some(body));
        assert_eq!(msg.subject().as_deref(), Some("Round trip"));
        assert_eq!(msg.from()[0].display(), "Alice <alice@example.com>");
    }

    #[test]
    fn round_trip_alternative_is_lossless() {
        let text = "plain 中文 body";
        let html = "<html><body><p>html 中文 body</p></body></html>";
        let raw = MessageBuilder::new()
            .from("Alice <alice@example.com>")
            .to("bob@example.org")
            .subject("你好")
            .text(text)
            .html(html)
            .build()
            .unwrap();
        let msg = ParsedMessage::parse(&raw).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some(text));
        assert_eq!(msg.html_body().as_deref(), Some(html));
        assert_eq!(msg.subject().as_deref(), Some("你好"));
    }

    #[test]
    fn round_trip_with_attachment_is_lossless() {
        let payload: Vec<u8> = (0u8..=255).rev().collect();
        let raw = MessageBuilder::new()
            .from("Alice <alice@example.com>")
            .to("bob@example.org")
            .subject("attachment round trip")
            .text("body text")
            .html("<p>body html</p>")
            .attachment("payload.bin", "application/octet-stream", payload.clone())
            .attachment("notes.txt", "text/plain", b"notes".to_vec())
            .build()
            .unwrap();
        let msg = ParsedMessage::parse(&raw).unwrap();
        assert_eq!(msg.text_body().as_deref(), Some("body text"));
        assert_eq!(msg.html_body().as_deref(), Some("<p>body html</p>"));
        let atts = msg.attachments();
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].filename().as_deref(), Some("payload.bin"));
        assert_eq!(atts[0].content, payload);
        assert_eq!(atts[1].filename().as_deref(), Some("notes.txt"));
        assert_eq!(atts[1].content, b"notes");
    }

    #[test]
    fn round_trip_mixed_attachment_without_html_body() {
        let raw = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("only text")
            .attachment("a.bin", "application/octet-stream", vec![7, 7, 7])
            .build()
            .unwrap();
        let msg = ParsedMessage::parse(&raw).unwrap();
        let root = &msg.parts()[0];
        assert_eq!(root.parts.len(), 2);
        assert_eq!(root.parts[0].content_type.subtype, "plain");
        assert_eq!(root.parts[1].content_type.subtype, "octet-stream");
    }

    #[test]
    fn build_message_matches_build_plus_parse() {
        let when = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let builder = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("self check")
            .text("body")
            .date(when)
            .message_id(RfcMessageId::new("self@example.com"));
        let raw = builder.clone().build().unwrap();
        let direct = builder.build_message().unwrap();
        assert_eq!(direct, ParsedMessage::parse(&raw).unwrap());
    }

    #[test]
    fn max_body_line_is_exposed() {
        assert_eq!(max_body_line(), MAX_BODY_LINE);
        assert_eq!(MAX_BODY_LINE, 78);
    }

    #[test]
    fn a_line_exactly_at_the_limit_stays_seven_bit() {
        // 78 octets is exactly the limit, so no encoding is needed.
        let body = "a".repeat(MAX_BODY_LINE);
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text(&body)
            .build_message()
            .unwrap();
        assert_eq!(
            msg.parts()[0].encoding,
            crate::mime::TransferEncoding::SevenBit
        );
        assert_eq!(msg.text_body().as_deref(), Some(body.as_str()));

        // One octet more and the quoted-printable encoder takes over.
        let body = "a".repeat(MAX_BODY_LINE + 1);
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text(&body)
            .build_message()
            .unwrap();
        assert_eq!(
            msg.parts()[0].encoding,
            crate::mime::TransferEncoding::QuotedPrintable
        );
        assert_eq!(msg.text_body().as_deref(), Some(body.as_str()));
    }

    #[test]
    fn normalise_newlines_handles_all_three_styles() {
        assert_eq!(normalise_newlines("a\nb"), "a\r\nb");
        assert_eq!(normalise_newlines("a\r\nb"), "a\r\nb");
        assert_eq!(normalise_newlines("a\rb"), "a\r\nb");
        assert_eq!(normalise_newlines(""), "");
        assert_eq!(normalise_newlines("中文\n😀"), "中文\r\n😀");
    }

    #[test]
    fn header_folding_applies_to_long_subjects() {
        let subject = "word ".repeat(60);
        let raw = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject(subject.trim())
            .text("b")
            .build()
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        let msg = ParsedMessage::parse(text.as_bytes()).unwrap();
        assert_eq!(msg.subject().as_deref(), Some(subject.trim()));
    }

    #[test]
    fn attachment_with_an_empty_filename_is_still_an_attachment() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("b")
            .attachment("", "application/octet-stream", vec![1])
            .build_message()
            .unwrap();
        assert_eq!(msg.attachments().len(), 1);
    }

    #[test]
    fn builder_default_is_empty() {
        let builder = MessageBuilder::default();
        assert!(builder.recipients().is_empty());
        assert!(builder.text.is_none());
        assert!(builder.html.is_none());
    }

    #[test]
    fn reply_to_and_bcc_headers_are_written() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .reply_to("noreply@example.com")
            .bcc("secret@example.org")
            .subject("s")
            .text("b")
            .build_message()
            .unwrap();
        assert_eq!(msg.reply_to()[0].address.to_string(), "noreply@example.com");
        assert_eq!(msg.header("Bcc"), Some("secret@example.org"));
    }

    #[test]
    fn multiple_attachments_keep_their_order() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text("b")
            .attachment("one.txt", "text/plain", b"1".to_vec())
            .attachment("two.bin", "application/octet-stream", b"2".to_vec())
            .attachment("three.txt", "text/plain", b"3".to_vec())
            .build_message()
            .unwrap();
        let names: Vec<String> = msg
            .attachments()
            .iter()
            .map(|a| a.filename().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["one.txt", "two.bin", "three.txt"]);
    }

    #[test]
    fn empty_text_body_still_produces_a_text_leaf() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("empty")
            .text("")
            .build_message()
            .unwrap();
        assert_eq!(msg.text_body().as_deref(), Some(""));
        assert_eq!(msg.header("Content-Type"), Some("text/plain; charset=utf-8"));
    }

    #[test]
    fn html_attachment_mixed_with_alternative_body() {
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("mixed")
            .text("plain")
            .html("<p>html</p>")
            .attachment("x.bin", "application/octet-stream", vec![1])
            .build_message()
            .unwrap();
        let root = &msg.parts()[0];
        assert_eq!(root.parts.len(), 2);
        assert!(root.parts[0].is_multipart(), "first part is the alternative");
        assert_eq!(msg.text_body().as_deref(), Some("plain"));
        assert_eq!(msg.html_body().as_deref(), Some("<p>html</p>"));
        assert_eq!(msg.attachments().len(), 1);
    }

    #[test]
    fn a_body_of_only_long_lines_is_quoted_printable() {
        let long_line = "y".repeat(3000);
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("long line")
            .text(&long_line)
            .build_message()
            .unwrap();
        assert_eq!(
            msg.parts()[0].encoding,
            crate::mime::TransferEncoding::QuotedPrintable
        );
        assert_eq!(msg.text_body().as_deref(), Some(long_line.as_str()));
    }

    #[test]
    fn crlf_body_is_sent_unchanged_as_seven_bit() {
        let body = "line one\r\nline two\r\nline three";
        let msg = MessageBuilder::new()
            .from("a@example.com")
            .to("b@example.org")
            .subject("s")
            .text(body)
            .build_message()
            .unwrap();
        assert_eq!(
            msg.parts()[0].encoding,
            crate::mime::TransferEncoding::SevenBit
        );
        assert_eq!(msg.text_body().as_deref(), Some(body));
    }

    #[test]
    fn recipients_helper_handles_unparsable_values() {
        let builder = MessageBuilder::new()
            .to("not an address")
            .to("good@example.com");
        let recipients = builder.recipients();
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].address.to_string(), "good@example.com");
    }

    #[test]
    fn build_is_deterministic_when_the_id_and_date_are_fixed() {
        let when = DateTime::parse_from_rfc3339("2024-05-05T05:05:05Z")
            .unwrap()
            .with_timezone(&Utc);
        let make = || {
            MessageBuilder::new()
                .from("a@example.com")
                .to("b@example.org")
                .subject("stable")
                .text("body")
                .date(when)
                .message_id(RfcMessageId::new("fixed@example.com"))
                .build()
                .unwrap()
        };
        assert_eq!(make(), make());
    }
}
