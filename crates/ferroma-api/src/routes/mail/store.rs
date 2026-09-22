//! Turning a request into stored mail.
//!
//! Both the management surface and the Client API funnel their send path through
//! here, so a message composed in Webmail and one composed in the desktop client
//! produce exactly the same bytes, the same rows and the same queue entries.
//!
//! The sequence for a non-draft send is:
//!
//! 1. resolve the sender address and check the caller owns it;
//! 2. validate the recipient list against `limits.max_recipients`;
//! 3. build the RFC 5322 bytes with [`ferroma_mail::MessageBuilder`];
//! 4. write them into the sender's `Sent` folder through the Maildir;
//! 5. insert the `messages` row, its recipients and its attachment rows;
//! 6. record the sync change and publish `mail.sent`;
//! 7. hand the message to [`crate::state::MailSender`], which writes one
//!    `mail_queue` row per recipient.
//!
//! A draft stops after step 5 and lands in `Drafts` with `\Draft` instead, which is
//! what `POST /api/v1/messages { "draft": true }` means.
//!
//! # HTML
//!
//! `docs/fcp.md` §5 says `html_body` is sanitised server-side. [`sanitize_html`] does
//! that: a small allow-list sanitiser that removes `<script>`/`<style>` bodies, every
//! `on*` handler, and `javascript:` URLs. It is deliberately conservative — a mail
//! client is a hostile-content renderer, and the cost of dropping a fancy attribute is
//! far lower than the cost of a stored XSS in Webmail.

use ferroma_core::{EmailAddress, FerromaError, RfcMessageId, UserId};
use ferroma_mail::MessageBuilder;
use ferroma_storage::models::{AttachmentRow, Mailbox, Message};
use ferroma_storage::repository::{NewAttachment, NewMessage, Recipient};
use ferroma_storage::Repositories;
use sha2::{Digest, Sha256};

/// One attachment the caller asked to include: already stored in the blob store.
#[derive(Debug, Clone)]
pub struct ComposeAttachment {
    /// The attachment row this blob belongs to, if it already has one.
    pub id: Option<i64>,
    /// The file name the recipient sees.
    pub filename: String,
    /// The MIME type.
    pub content_type: String,
    /// The bytes.
    pub data: Vec<u8>,
    /// The blob's path in the store, when it was already stored by an upload.
    pub storage_path: Option<String>,
}

impl ComposeAttachment {
    /// The path the blob occupies, computing the content address when the upload did
    /// not already store it.
    pub fn path(&self) -> String {
        self.storage_path
            .clone()
            .unwrap_or_else(|| attachment_storage_path(self))
    }
}

/// A fully-described outgoing message.
#[derive(Debug, Clone, Default)]
pub struct OutgoingMessage {
    /// The envelope/`From` address.
    pub from: String,
    /// Display name that goes with [`OutgoingMessage::from`].
    pub from_name: Option<String>,
    /// `To` recipients.
    pub to: Vec<String>,
    /// `Cc` recipients.
    pub cc: Vec<String>,
    /// `Bcc` recipients (they are queued, but the header is still written so an
    /// IMAP copy of a draft keeps them).
    pub bcc: Vec<String>,
    /// Subject.
    pub subject: String,
    /// Plain-text body.
    pub text: Option<String>,
    /// HTML body.
    pub html: Option<String>,
    /// Attachments already in the blob store.
    pub attachments: Vec<ComposeAttachment>,
    /// The `Message-ID` being replied to.
    pub in_reply_to: Option<String>,
    /// The reference chain.
    pub references: Vec<String>,
    /// A `Reply-To` header.
    pub reply_to: Option<String>,
}

impl OutgoingMessage {
    /// The addresses the message must be delivered to: `To` + `Cc` + `Bcc`.
    pub fn envelope_recipients(&self) -> Vec<String> {
        let mut out = Vec::new();
        for value in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            for mailbox in ferroma_mail::address::parse_address_list(value) {
                let address = mailbox.address.to_string();
                if !out
                    .iter()
                    .any(|seen: &String| seen.eq_ignore_ascii_case(&address))
                {
                    out.push(address);
                }
            }
        }
        out
    }

    /// Whether the message has any body at all.
    pub fn has_body(&self) -> bool {
        self.text.as_deref().is_some_and(|text| !text.is_empty())
            || self.html.as_deref().is_some_and(|html| !html.is_empty())
    }

    /// Build the RFC 5322 bytes.
    pub fn build(&self, domain: &str) -> Result<Vec<u8>, FerromaError> {
        let mut builder = MessageBuilder::new().domain(domain);

        let from = match self.from_name.as_deref() {
            Some(name) if !name.trim().is_empty() => {
                format!("{} <{}>", name.trim(), self.from)
            }
            _ => self.from.clone(),
        };
        builder = builder.from(from);

        for value in &self.to {
            builder = builder.to(value);
        }
        for value in &self.cc {
            builder = builder.cc(value);
        }
        for value in &self.bcc {
            builder = builder.bcc(value);
        }
        if let Some(reply_to) = self.reply_to.as_deref().filter(|v| !v.trim().is_empty()) {
            builder = builder.reply_to(reply_to);
        }

        builder = builder.subject(&self.subject);
        if let Some(text) = self.text.as_deref().filter(|t| !t.is_empty()) {
            builder = builder.text(text);
        }
        if let Some(html) = self.html.as_deref().filter(|h| !h.is_empty()) {
            builder = builder.html(&sanitize_html(html));
        }
        if let Some(in_reply_to) = self.in_reply_to.as_deref().filter(|v| !v.trim().is_empty()) {
            builder = builder.in_reply_to(in_reply_to);
        }
        if !self.references.is_empty() {
            builder = builder.references(&self.references);
        }
        for attachment in &self.attachments {
            builder = builder.attachment(
                &attachment.filename,
                &attachment.content_type,
                attachment.data.clone(),
            );
        }

        builder.build()
    }
}

/// The result of storing a composed message.
#[derive(Debug, Clone)]
pub struct StoredOutgoing {
    /// The row that was created.
    pub message: Message,
    /// The attachment rows that were created.
    pub attachments: Vec<AttachmentRow>,
}

/// Insert a message into the Maildir and the database.
///
/// `folder` is the IMAP folder name the copy lands in (`Sent` for a sent message,
/// `Drafts` for a draft) and `flags` is the canonical flag string it carries.
#[allow(clippy::too_many_arguments)]
pub async fn store_message(
    repos: &Repositories,
    maildir: &ferroma_storage::Maildir,
    domain: &str,
    mailbox: &Mailbox,
    folder: &str,
    flags: &str,
    is_draft: bool,
    outgoing: &OutgoingMessage,
    body: &ParsedBody,
) -> Result<StoredOutgoing, FerromaError> {
    let bytes = outgoing.build(domain)?;
    let stored = maildir.store(domain, &mailbox.local_part, folder, &bytes, flags)?;

    let folder_row = repos
        .folders
        .find_by_name(mailbox.mailbox_id(), folder)
        .await?
        .ok_or_else(|| FerromaError::NotFound(format!("folder {folder}")))?;

    let message = repos
        .messages
        .insert(NewMessage {
            folder_id: folder_row.folder_id(),
            mailbox_id: mailbox.mailbox_id(),
            rfc_message_id: body.message_id.clone(),
            thread_id: body.thread_id.clone(),
            subject: Some(outgoing.subject.clone()).filter(|s| !s.is_empty()),
            sender: Some(outgoing.from.clone()),
            sender_name: outgoing.from_name.clone(),
            snippet: body.snippet.clone(),
            size_bytes: stored.size as i64,
            storage_path: stored.path,
            checksum_sha256: Some(stored.sha256),
            flags: flags.to_string(),
            internal_date: None,
            sent_at: body.sent_at,
            has_attachments: !outgoing.attachments.is_empty(),
            attachment_count: outgoing.attachments.len() as i32,
            is_draft,
        })
        .await?;

    repos
        .messages
        .insert_recipients(message.message_id(), &body.recipients)
        .await?;

    let mut attachment_rows = Vec::with_capacity(outgoing.attachments.len());
    for attachment in &outgoing.attachments {
        // An attachment that was uploaded before the message existed already has a
        // row, hanging from the uploader's placeholder. Re-point it by replacing the
        // row rather than mutating it in place: the repository exposes insert/delete,
        // and this crate deliberately has no direct database access for an `UPDATE`.
        if let Some(existing_id) = attachment.id {
            repos
                .attachments
                .delete(ferroma_core::AttachmentId::new(existing_id))
                .await?;
        }

        let blob = repos
            .attachments
            .insert(
                message.message_id(),
                NewAttachment {
                    filename: Some(attachment.filename.clone()),
                    content_type: attachment.content_type.clone(),
                    size_bytes: attachment.data.len() as i64,
                    storage_path: attachment.path(),
                    content_id: None,
                    is_inline: false,
                    checksum_sha256: Some(content_digest(&attachment.data)),
                },
            )
            .await?;
        attachment_rows.push(blob);
    }

    Ok(StoredOutgoing {
        message,
        attachments: attachment_rows,
    })
}

/// Lower-case hex SHA-256 of some bytes.
pub fn content_digest(data: &[u8]) -> String {
    hex_lower(&Sha256::digest(data))
}

/// The blob path an attachment is stored at.
///
/// The send path re-stores every attachment into the blob store so the copy that goes
/// out is content-addressed like everything else the platform keeps.
fn attachment_storage_path(attachment: &ComposeAttachment) -> String {
    use sha2::{Digest, Sha256};
    let digest = hex_lower(&Sha256::digest(&attachment.data));
    format!("{}/{}/{}", &digest[0..2], &digest[2..4], digest)
}

/// Lower-case hex of a digest.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The parsed facts a stored message carries into its row.
#[derive(Debug, Clone, Default)]
pub struct ParsedBody {
    /// The RFC 5322 `Message-ID` the builder minted.
    pub message_id: Option<String>,
    /// The conversation root.
    pub thread_id: Option<String>,
    /// The `Date:` header.
    pub sent_at: Option<chrono::DateTime<chrono::Utc>>,
    /// A short, body-free preview.
    pub snippet: Option<String>,
    /// The stored recipients.
    pub recipients: Vec<Recipient>,
}

/// Parse the bytes we just built, to fill the row's denormalised columns.
pub fn describe_built_message(bytes: &[u8], outgoing: &OutgoingMessage) -> ParsedBody {
    let parsed = ferroma_mail::ParsedMessage::parse(bytes).ok();
    let message_id = parsed
        .as_ref()
        .and_then(|message| message.message_id())
        .map(|id| id.as_str().to_string());
    let thread_id = outgoing
        .references
        .first()
        .cloned()
        .or_else(|| outgoing.in_reply_to.clone())
        .or_else(|| message_id.clone());
    let sent_at = parsed.as_ref().and_then(|message| message.date());
    let snippet = parsed
        .as_ref()
        .map(|message| message.snippet(180))
        .filter(|snippet| !snippet.trim().is_empty());

    let mut recipients: Vec<Recipient> = Vec::new();
    let mut push = |kind: &str, values: &[String]| {
        let mut ordinal = 0i32;
        for value in values {
            for mailbox in ferroma_mail::address::parse_address_list(value) {
                recipients.push(Recipient {
                    kind: kind.to_string(),
                    address: mailbox.address.to_string(),
                    display_name: mailbox.name.clone(),
                    ordinal,
                });
                ordinal += 1;
            }
        }
    };
    push("to", &outgoing.to);
    push("cc", &outgoing.cc);
    push("bcc", &outgoing.bcc);

    ParsedBody {
        message_id,
        thread_id,
        sent_at,
        snippet,
        recipients,
    }
}

/// Validate a `from` address and confirm the caller owns it.
pub async fn resolve_sender(
    repos: &Repositories,
    from: &str,
    user: UserId,
) -> Result<(Mailbox, String), FerromaError> {
    let address = EmailAddress::parse(from)
        .map_err(|_| FerromaError::Invalid(format!("from is not a valid address: {from}")))?;
    let mailbox = repos
        .mailboxes
        .find_by_address(address.domain(), address.local_part())
        .await?;
    match mailbox {
        Some(mailbox) if mailbox.user_id == user.get() => {
            let domain = domain_name(repos, mailbox.domain_id).await?;
            Ok((mailbox, domain))
        }
        // Somebody else's address: the same answer as an address that does not exist.
        _ => Err(FerromaError::NotFound(format!("no such mailbox {from}"))),
    }
}

/// The name of a domain row.
pub async fn domain_name(repos: &Repositories, domain_id: i64) -> Result<String, FerromaError> {
    let domain = repos
        .domains
        .find_by_id(ferroma_core::DomainId::new(domain_id))
        .await?
        .ok_or_else(|| FerromaError::NotFound(format!("domain {domain_id}")))?;
    Ok(domain.name)
}

/// Assemble an [`OutgoingMessage`] from a request and its resolved attachments.
pub fn outgoing_from(
    from: String,
    request: &crate::service::SendRequest,
    attachments: Vec<ComposeAttachment>,
) -> OutgoingMessage {
    OutgoingMessage {
        from,
        from_name: request.from_name.clone(),
        to: request.to.clone(),
        cc: request.cc.clone(),
        bcc: request.bcc.clone(),
        subject: request.subject.clone(),
        text: request.text.clone(),
        html: request.html.clone(),
        attachments,
        in_reply_to: request.in_reply_to.clone(),
        references: request.references.clone(),
        reply_to: request.reply_to.clone(),
    }
}

/// The `Message-ID` of a stored message, as the raw header carries it.
pub fn message_id_header(message: &Message) -> Option<String> {
    message.rfc_message_id.clone()
}

/// Mint a fresh `Message-ID` for a domain.
pub fn fresh_message_id(domain: &str) -> RfcMessageId {
    RfcMessageId::generate(domain)
}

/// Read the threading headers out of a stored message's bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadingHeaders {
    /// The `Message-ID` header.
    pub message_id: Option<String>,
    /// The `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// The `References` header, oldest first.
    pub references: Vec<String>,
}

/// Extract the threading headers a reply needs.
///
/// `docs/api.md` §5.2: a client that does not receive `message_id_header` must send
/// its reply **without** threading headers rather than inventing one, so this returns
/// `None`s rather than synthesising anything.
pub fn threading_headers(raw: &[u8]) -> ThreadingHeaders {
    let Ok(parsed) = ferroma_mail::ParsedMessage::parse(raw) else {
        return ThreadingHeaders::default();
    };
    let message_id = parsed.message_id().map(|id| id.as_str().to_string());
    let in_reply_to = parsed
        .header("In-Reply-To")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let references = parsed
        .header("References")
        .map(split_message_ids)
        .unwrap_or_default();
    ThreadingHeaders {
        message_id,
        in_reply_to,
        references,
    }
}

/// Split a `References` header into individual `<…>` ids.
pub fn split_message_ids(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in raw.chars() {
        match ch {
            '<' => {
                depth += 1;
                current.clear();
                current.push(ch);
            }
            '>' if depth > 0 => {
                current.push(ch);
                out.push(current.clone());
                current.clear();
                depth -= 1;
            }
            _ if depth > 0 => current.push(ch),
            _ => {}
        }
    }
    // A malformed header with a bare id and no brackets still names a message.
    if out.is_empty() {
        for token in raw.split_whitespace() {
            let token = token.trim();
            if token.contains('@') {
                out.push(token.to_string());
            }
        }
    }
    out
}

/// The tags [`sanitize_html`] leaves in place.
const ALLOWED_TAGS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "samp",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
    "var",
];

/// Attributes kept when their tag is kept. Everything not listed is dropped.
const ALLOWED_ATTRIBUTES: &[&str] = &[
    "alt", "class", "colspan", "dir", "height", "href", "lang", "rel", "rowspan", "src", "style",
    "title", "width",
];

/// Sanitise an HTML body for storage.
///
/// The rules, in order:
///
/// * the contents of `<script>`, `<style>`, `<iframe>`, `<object>`, `<embed>` and
///   `<template>` are removed **with** their bodies — dropping only the tags would
///   leave script source visible as text;
/// * a tag that is not in [`ALLOWED_TAGS`] is dropped but its text is kept;
/// * an attribute that is not in [`ALLOWED_ATTRIBUTES`], or whose name starts with
///   `on`, is dropped — those are the event handlers;
/// * a `href`/`src` whose scheme is `javascript:`, `vbscript:` or `data:` (other than
///   a `data:image/...`) is dropped along with the attribute.
///
/// It is not a full HTML parser — it is a filter that only ever removes, never
/// rewrites, so it cannot introduce markup that was not already there.
pub fn sanitize_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes: Vec<char> = input.chars().collect();
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] != '<' {
            // A stray `<` is text, not markup.
            out.push(bytes[index]);
            index += 1;
            continue;
        }

        let Some(end) = find_tag_end(&bytes, index) else {
            out.push(bytes[index]);
            index += 1;
            continue;
        };

        let raw_tag: String = bytes[index + 1..end].iter().collect();
        let closing = raw_tag.starts_with('/');
        let self_closing = raw_tag.ends_with('/');
        let name: String = raw_tag
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split(|ch: char| ch.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();

        if name.is_empty() {
            // `<>` is not a tag; keep it as text so nothing is silently eaten.
            out.push_str(&format!("<{}>", raw_tag));
            index = end + 1;
            continue;
        }

        if DROPPED_WITH_BODY.contains(&name.as_str()) {
            index = skip_element(&bytes, end + 1, &name);
            continue;
        }

        if !ALLOWED_TAGS.contains(&name.as_str()) {
            // Drop the tag itself, keep whatever it wrapped.
            index = end + 1;
            continue;
        }

        out.push('<');
        if closing {
            out.push('/');
        }
        out.push_str(&name);

        if !closing {
            out.push_str(&render_attributes(&raw_tag, &name));
            if self_closing || is_void(&name) {
                out.push_str(" /");
            }
        }
        out.push('>');
        index = end + 1;
    }

    out
}

/// Elements whose entire body is dropped, not just their tags.
const DROPPED_WITH_BODY: &[&str] = &[
    "script", "style", "iframe", "object", "embed", "template", "noscript", "head",
];

/// Void elements, which never take a closing tag.
const VOID_ELEMENTS: &[&str] = &["br", "hr", "img", "col"];

/// Whether a tag is a void element.
fn is_void(name: &str) -> bool {
    VOID_ELEMENTS.contains(&name)
}

/// The index of the `>` that closes the tag starting at `start`.
fn find_tag_end(chars: &[char], start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut index = start + 1;
    while index < chars.len() {
        let ch = chars[index];
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '>' => return Some(index),
            None => {}
        }
        index += 1;
    }
    None
}

/// Skip past `</name>` starting at `from`.
///
/// A missing closer must not swallow the rest of the document. HTML permits `</head>`
/// to be omitted — the parser ends the head at `<body>` — and mail is full of HTML that
/// omits it. Dropping everything from `<head>` to the end emptied the body of any such
/// message, which the reader then reported as "this message has no body". So a `<head>`
/// without a closer is ended at `<body>` when there is one, and otherwise the remainder
/// is kept: showing a stray title is recoverable, showing nothing is not.
fn skip_element(chars: &[char], from: usize, name: &str) -> usize {
    let closer = format!("</{name}");
    let text: String = chars[from..]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    match text.find(&closer) {
        Some(offset) => {
            // Advance past the closer's own `>`.
            let after = from + offset;
            match find_tag_end(chars, after) {
                Some(end) => end + 1,
                None => chars.len(),
            }
        }
        None => {
            if name == "head" {
                if let Some(offset) = text.find("<body") {
                    return from + offset;
                }
            }
            // Unterminated: drop the opening tag only, never the document behind it.
            from
        }
    }
}

/// Re-render the attributes worth keeping.
fn render_attributes(raw_tag: &str, element: &str) -> String {
    let mut out = String::new();
    let mut rest = raw_tag;
    // Skip the tag name.
    if let Some(position) = rest.find(|ch: char| ch.is_whitespace()) {
        rest = &rest[position..];
    } else {
        return out;
    }

    for attribute in split_attributes(rest) {
        let (name, value) = match attribute.split_once('=') {
            Some((name, value)) => (name.trim().to_ascii_lowercase(), Some(value.trim())),
            None => (attribute.trim().to_ascii_lowercase(), None),
        };
        if name.is_empty() || name.starts_with("on") {
            continue;
        }
        if !ALLOWED_ATTRIBUTES.contains(&name.as_str()) {
            continue;
        }
        if matches!(name.as_str(), "href" | "src" | "style") {
            if let Some(value) = value {
                if name == "style" {
                    // A `style` value that carries a script URL is not a style.
                    if value.to_ascii_lowercase().contains("javascript:")
                        || value.to_ascii_lowercase().contains("expression(")
                    {
                        continue;
                    }
                } else if !safe_url(value) {
                    continue;
                }
            }
        }
        match value {
            Some(value) => out.push_str(&format!(" {name}={value}")),
            None => out.push_str(&format!(" {name}")),
        }
    }
    let _ = element;
    out
}

/// Split an attribute list on whitespace that is outside quotes.
fn split_attributes(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match quote {
            Some(open) if ch == open => {
                quote = None;
                current.push(ch);
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            None if ch.is_whitespace() => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_string());
                }
                current.clear();
            }
            None => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Whether a URL is safe to keep in an `href`/`src`.
pub fn safe_url(value: &str) -> bool {
    let trimmed = value.trim().trim_matches('"').trim_matches('\'').trim();
    let lower = trimmed.to_ascii_lowercase();
    // Strip the whitespace and control characters a filter-bypass relies on.
    let compact: String = lower
        .chars()
        .filter(|ch| !ch.is_whitespace() && !ch.is_control())
        .collect();
    if compact.starts_with("data:") {
        // Only images; a `data:text/html` URL is a script in disguise.
        return compact.starts_with("data:image/")
            && !compact.contains("image/svg")
            && !compact.contains("script");
    }
    !(compact.starts_with("javascript:")
        || compact.starts_with("vbscript:")
        || compact.starts_with("file:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_recipients_deduplicate_case_insensitively() {
        let message = OutgoingMessage {
            from: "alice@example.com".into(),
            to: vec!["Bob <bob@example.net>, carol@example.org".into()],
            cc: vec!["BOB@example.net".into()],
            bcc: vec!["dan@example.io".into()],
            ..OutgoingMessage::default()
        };
        let recipients = message.envelope_recipients();
        assert_eq!(
            recipients,
            vec!["bob@example.net", "carol@example.org", "dan@example.io"]
        );
    }

    #[test]
    fn envelope_recipients_ignore_unparsable_entries() {
        let message = OutgoingMessage {
            to: vec!["not an address, bob@example.net".into()],
            ..OutgoingMessage::default()
        };
        assert_eq!(message.envelope_recipients(), vec!["bob@example.net"]);
    }

    #[test]
    fn a_message_with_no_recipients_has_an_empty_envelope() {
        let message = OutgoingMessage::default();
        assert!(message.envelope_recipients().is_empty());
        assert!(!message.has_body());
    }

    #[test]
    fn has_body_ignores_empty_strings() {
        let mut message = OutgoingMessage::default();
        assert!(!message.has_body());
        message.text = Some(String::new());
        assert!(!message.has_body());
        message.text = Some("hi".into());
        assert!(message.has_body());
        message.html = Some("<p>hi</p>".into());
        assert!(message.has_body());
    }

    #[test]
    fn built_bytes_are_a_parseable_message() {
        let message = OutgoingMessage {
            from: "alice@example.com".into(),
            from_name: Some("Alice".into()),
            to: vec!["bob@example.net".into()],
            subject: "Invoice".into(),
            text: Some("Hi Bob".into()),
            ..OutgoingMessage::default()
        };
        let bytes = message.build("example.com").expect("build must succeed");
        let parsed = ferroma_mail::ParsedMessage::parse(&bytes).expect("must parse back");
        assert_eq!(parsed.subject().as_deref(), Some("Invoice"));
        assert!(parsed.message_id().is_some());
        assert!(String::from_utf8_lossy(&bytes).contains("Alice <alice@example.com>"));
    }

    #[test]
    fn built_message_sanitises_html_and_keeps_the_text_alternative() {
        let message = OutgoingMessage {
            from: "alice@example.com".into(),
            to: vec!["bob@example.net".into()],
            subject: "both".into(),
            text: Some("plain".into()),
            html: Some("<p>hi<script>alert(1)</script></p>".into()),
            ..OutgoingMessage::default()
        };
        let bytes = message.build("example.com").expect("build must succeed");
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(!text.contains("alert(1)"), "{text}");
        assert!(text.contains("plain"));
    }

    #[test]
    fn describe_fills_the_denormalised_columns() {
        let message = OutgoingMessage {
            from: "alice@example.com".into(),
            to: vec!["Bob <bob@example.net>".into()],
            cc: vec!["carol@example.org".into()],
            subject: "Hi".into(),
            text: Some("Hello there, this is the body".into()),
            in_reply_to: Some("<parent@example.com>".into()),
            ..OutgoingMessage::default()
        };
        let bytes = message.build("example.com").expect("build");
        let body = describe_built_message(&bytes, &message);
        assert!(body.message_id.is_some());
        assert_eq!(body.thread_id.as_deref(), Some("<parent@example.com>"));
        assert!(body.snippet.is_some());
        assert_eq!(body.recipients.len(), 2);
        assert_eq!(body.recipients[0].kind, "to");
        assert_eq!(body.recipients[0].address, "bob@example.net");
        assert_eq!(body.recipients[0].display_name.as_deref(), Some("Bob"));
        assert_eq!(body.recipients[1].kind, "cc");
    }

    #[test]
    fn describe_falls_back_to_the_new_message_id_for_the_thread() {
        let message = OutgoingMessage {
            from: "alice@example.com".into(),
            to: vec!["bob@example.net".into()],
            subject: "fresh".into(),
            text: Some("body".into()),
            ..OutgoingMessage::default()
        };
        let bytes = message.build("example.com").expect("build");
        let body = describe_built_message(&bytes, &message);
        assert_eq!(body.thread_id, body.message_id);
    }

    #[test]
    fn sanitize_drops_script_bodies_entirely() {
        let cleaned = sanitize_html("<p>before</p><script>alert('xss')</script><p>after</p>");
        assert!(!cleaned.contains("alert"), "{cleaned}");
        assert!(!cleaned.contains("script"), "{cleaned}");
        assert!(cleaned.contains("before"));
        assert!(cleaned.contains("after"));
    }

    #[test]
    fn sanitize_drops_event_handlers_and_javascript_urls() {
        let cleaned = sanitize_html(
            r#"<a href="javascript:alert(1)" onclick="steal()">click</a><img src="x" onerror="boom()">"#,
        );
        assert!(!cleaned.contains("onclick"), "{cleaned}");
        assert!(!cleaned.contains("onerror"), "{cleaned}");
        assert!(!cleaned.contains("javascript:"), "{cleaned}");
        assert!(cleaned.contains("click"));
    }

    #[test]
    fn sanitize_keeps_ordinary_markup_intact() {
        let input = r#"<p class="lead">Hi <strong>Alice</strong></p><a href="https://example.com">link</a>"#;
        let cleaned = sanitize_html(input);
        assert!(cleaned.contains("<strong>Alice</strong>"), "{cleaned}");
        assert!(
            cleaned.contains(r#"href="https://example.com""#),
            "{cleaned}"
        );
    }

    #[test]
    fn sanitize_drops_style_and_iframe_bodies_too() {
        let cleaned = sanitize_html(
            "<style>body{background:url(javascript:x)}</style><iframe src=\"https://evil\"></iframe>ok",
        );
        assert!(!cleaned.contains("background"), "{cleaned}");
        assert!(!cleaned.contains("iframe"), "{cleaned}");
        assert!(cleaned.contains("ok"));
    }

    #[test]
    fn sanitize_never_eats_plain_text_containing_angle_brackets() {
        assert_eq!(sanitize_html("2 < 3 and 5 > 4"), "2 < 3 and 5 > 4");
        assert_eq!(sanitize_html("no tags here"), "no tags here");
        assert_eq!(sanitize_html(""), "");
    }

    #[test]
    fn sanitize_keeps_an_unclosed_tag_from_swallowing_the_message() {
        let cleaned = sanitize_html("<b>bold but never closed");
        assert!(cleaned.contains("bold but never closed"), "{cleaned}");
    }

    #[test]
    fn sanitize_survives_an_html_mail_that_omits_its_head_closer() {
        // HTML permits `</head>` to be omitted; the head ends at `<body>`. Treating the
        // missing closer as "drop the rest of the document" emptied the body of every
        // such message, and the reader reported it as having no body at all.
        let cleaned = sanitize_html(
            "<html><head><style>.x{color:red}</style><body><p>Body after an unclosed head.</p></body></html>",
        );
        assert!(
            cleaned.contains("Body after an unclosed head."),
            "the body was swallowed: {cleaned:?}"
        );
        assert!(
            !cleaned.contains("color:red"),
            "the CSS leaked: {cleaned:?}"
        );
    }

    #[test]
    fn sanitize_does_not_drop_a_document_after_an_unterminated_style() {
        let cleaned = sanitize_html("<style>p{color:red}<p>kept</p>");
        assert!(cleaned.contains("kept"), "{cleaned}");
    }

    #[test]
    fn sanitize_drops_unknown_tags_but_keeps_their_text() {
        let cleaned = sanitize_html("<blink>hello</blink>");
        assert_eq!(cleaned, "hello");
    }

    #[test]
    fn data_urls_are_only_kept_for_images() {
        assert!(safe_url("data:image/png;base64,AAAA"));
        assert!(!safe_url("data:text/html;base64,PHNjcmlwdD4="));
        assert!(!safe_url("data:image/svg+xml,<svg onload=alert(1)>"));
        assert!(!safe_url("javascript:alert(1)"));
        assert!(!safe_url("JaVaScRiPt:alert(1)"));
        assert!(!safe_url("java\tscript:alert(1)"));
        assert!(!safe_url("vbscript:msgbox(1)"));
        assert!(!safe_url("file:///etc/passwd"));
        assert!(safe_url("https://example.com/x"));
        assert!(safe_url("/relative/path"));
        assert!(safe_url("mailto:bob@example.net"));
    }

    #[test]
    fn message_ids_split_out_of_a_references_header() {
        let ids = split_message_ids("<a@b.c> <d@e.f>\r\n <g@h.i>");
        assert_eq!(ids, vec!["<a@b.c>", "<d@e.f>", "<g@h.i>"]);
        assert!(split_message_ids("").is_empty());
        assert_eq!(
            split_message_ids("bare@example.com"),
            vec!["bare@example.com"]
        );
    }

    #[test]
    fn threading_headers_come_from_the_stored_bytes() {
        let raw = concat!(
            "From: a@b.c\r\n",
            "Message-ID: <me@example.com>\r\n",
            "In-Reply-To: <parent@example.com>\r\n",
            "References: <root@example.com> <parent@example.com>\r\n",
            "\r\nbody\r\n"
        );
        let headers = threading_headers(raw.as_bytes());
        assert_eq!(headers.message_id.as_deref(), Some("<me@example.com>"));
        assert_eq!(headers.in_reply_to.as_deref(), Some("<parent@example.com>"));
        assert_eq!(
            headers.references,
            vec!["<root@example.com>", "<parent@example.com>"]
        );
    }

    #[test]
    fn threading_headers_never_invent_a_message_id() {
        let raw = "From: a@b.c\r\nSubject: no id\r\n\r\nbody\r\n";
        let headers = threading_headers(raw.as_bytes());
        assert!(headers.message_id.is_none());
        assert!(headers.in_reply_to.is_none());
        assert!(headers.references.is_empty());
        // Garbage in, empty out — never a panic.
        assert_eq!(
            threading_headers(b"\xff\xfe not a message").message_id,
            None
        );
    }

    #[test]
    fn attachment_paths_are_sharded_like_the_store_wants() {
        let attachment = ComposeAttachment {
            id: None,
            filename: "a.bin".into(),
            content_type: "application/octet-stream".into(),
            data: b"0123456789".to_vec(),
            storage_path: None,
        };
        let path = attachment.path();
        let parts: Vec<&str> = path.split('/').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 2);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 64);
        assert!(path.ends_with(&parts[2]));
        // Deterministic, so two identical attachments share one blob.
        assert_eq!(path, attachment.path());

        // An attachment an upload already stored keeps that path.
        let stored = ComposeAttachment {
            storage_path: Some("ab/cd/keep-me".into()),
            ..attachment
        };
        assert_eq!(stored.path(), "ab/cd/keep-me");
    }

    #[test]
    fn fresh_message_ids_are_unique_and_well_formed() {
        let a = fresh_message_id("example.com");
        let b = fresh_message_id("example.com");
        assert_ne!(a, b);
        assert!(a.as_str().ends_with("@example.com>"));
    }

    #[test]
    fn message_id_header_is_read_from_the_row() {
        let message = Message {
            id: 1,
            folder_id: 1,
            mailbox_id: 1,
            uid: 1,
            rfc_message_id: Some("<a@b.c>".into()),
            thread_id: None,
            subject: None,
            sender: None,
            sender_name: None,
            snippet: None,
            size_bytes: 0,
            storage_path: "x".into(),
            checksum_sha256: None,
            flags: String::new(),
            internal_date: chrono::Utc::now(),
            received_at: chrono::Utc::now(),
            sent_at: None,
            has_attachments: false,
            attachment_count: 0,
            is_draft: false,
            modseq: 1,
            deleted_at: None,
            expunged_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert_eq!(message_id_header(&message).as_deref(), Some("<a@b.c>"));
    }
}
