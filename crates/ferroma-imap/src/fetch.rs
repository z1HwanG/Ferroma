//! `FETCH` (RFC 3501 §6.4.5 and §7.4.2): `ENVELOPE`, `BODYSTRUCTURE`, section
//! addressing and partial reads.
//!
//! This is the module clients notice most. Thunderbird decides whether to show a
//! message as text, as HTML or as an attachment from `BODYSTRUCTURE` alone; get
//! one field wrong and the message renders as garbage. So:
//!
//! * `BODYSTRUCTURE` is built from the **raw** message
//!   ([`crate::rawmime`]), so `size` is the octet count of the *encoded* body —
//!   the number of bytes the client is about to receive — not the decoded one.
//! * `BODY[section]` returns the octets of the section as they are on disk, and
//!   a partial (`<start.len>`) is clamped to the stored size, so a read can never
//!   run past the end of the file.
//! * `BODY.PEEK[…]` never sets `\Seen`; `BODY[…]` does. That distinction is
//!   decided by [`crate::parser::FetchItem::sets_seen`] and honoured by the
//!   session, which is the only place that may write a flag.

use chrono::{DateTime, Utc};
use ferroma_mail::address::Mailbox;
use ferroma_mail::headers::Headers;
use ferroma_mail::Flags;

use crate::parser::{FetchItem, Section};
use crate::rawmime::{header_date, RawMessage, RawPart};
use crate::util::{self, internal_date, NString};

/// The `INTERNALDATE` and `RFC822.SIZE` half of a `FETCH` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageMeta {
    /// `INTERNALDATE`.
    pub internal_date: DateTime<Utc>,
    /// `RFC822.SIZE` — the octet count of the stored message.
    pub size: u64,
}

/// One piece of a `FETCH` response: inline text, or a literal's octets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchValue {
    /// Text written directly into the response.
    Text(String),
    /// Octets written as a `{n}\r\n…` literal.
    Literal(Vec<u8>),
}

impl FetchValue {
    /// A literal from bytes.
    pub fn literal(bytes: impl Into<Vec<u8>>) -> Self {
        FetchValue::Literal(bytes.into())
    }

    /// A literal from a string.
    pub fn text_literal(text: impl Into<String>) -> Self {
        FetchValue::Literal(text.into().into_bytes())
    }
}

/// One `FETCH` item's result: the response key and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchField {
    /// The key as it appears in the response (`BODY[TEXT]`, `UID`, …).
    pub key: String,
    /// The value.
    pub value: FetchValue,
}

/// Render a `FETCH` item list into the inside of `* n FETCH ( … )`.
///
/// The result is **bytes**: a `BODY[]` literal is arbitrary binary, so routing it
/// through `String` would corrupt every attachment that is not valid UTF-8.
///
/// Literal lengths are back-patched after the fact, so the `{n}` in front of a
/// section always matches the octets that follow it.
pub fn render_fields(fields: &[FetchField]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    // Where each literal's digits go, and how many octets they stand for.
    let mut patches: Vec<(usize, u64)> = Vec::new();

    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(field.key.as_bytes());
        out.push(b' ');
        match &field.value {
            FetchValue::Text(text) => out.extend_from_slice(text.as_bytes()),
            FetchValue::Literal(bytes) => {
                out.push(b'{');
                // The digits go right after the `{`, so the `}` that follows
                // shifts along with them.
                let after = out.len();
                out.push(b'}');
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(bytes);
                patches.push((after, bytes.len() as u64));
            }
        }
    }

    // Patch from the last literal to the first so the earlier offsets stay
    // valid, and so a literal's octets are never routed through `String` — a
    // `BODY[]` payload is arbitrary binary.
    for (after, length) in patches.into_iter().rev() {
        out.splice(after..after, length.to_string().bytes());
    }

    out
}

/// The response value of one `FETCH` item.
///
/// `Some(None)` means "the item produced nothing" (for example `BODY[2.MIME]` on
/// a message whose part 2 does not exist), which the session reports as `NIL`
/// only for the items where that is legal; `None` means the item is unknown.
pub fn item_value(
    item: &FetchItem,
    meta: &MessageMeta,
    uid: u64,
    flags: &Flags,
    recent: bool,
    message: &RawMessage,
    envelope: &Envelope,
) -> Option<FetchField> {
    let key = item.response_key();
    let value = match item {
        FetchItem::Uid => FetchValue::Text(uid.to_string()),
        FetchItem::Flags => FetchValue::Text(render_flags(flags, recent)),
        FetchItem::InternalDate => FetchValue::Text(util::quote(&internal_date(
            meta.internal_date,
        ))),
        FetchItem::Rfc822Size => FetchValue::Text(meta.size.to_string()),
        FetchItem::Envelope => FetchValue::Text(envelope.render()),
        FetchItem::Body | FetchItem::BodyStructure => {
            FetchValue::Text(bodystructure(message))
        }
        FetchItem::Rfc822 => FetchValue::literal(section_bytes(&Section::All, message)),
        FetchItem::Rfc822Header => FetchValue::literal(header_block(message)),
        FetchItem::Rfc822Text => FetchValue::literal(message.body().to_vec()),
        FetchItem::BodySection(section, partial) | FetchItem::BodyPeekSection(section, partial) => {
            let bytes = section_bytes(section, message);
            let bytes = match partial {
                Some(partial) => partial.slice(&bytes).to_vec(),
                None => bytes,
            };
            FetchValue::literal(bytes)
        }
    };
    Some(FetchField { key, value })
}

/// Render the flags of one message, including `\Recent` when it applies.
pub fn render_flags(flags: &Flags, recent: bool) -> String {
    let mut rendered = flags.clone();
    if recent {
        rendered.set_recent(true);
    }
    rendered.to_imap_string()
}

/// The top-level header block, including the blank line that ends it.
pub fn header_block(message: &RawMessage) -> Vec<u8> {
    message.header_with_blank_line().to_vec()
}

/// The octets a `BODY[section]` (or `RFC822`) names.
pub fn section_bytes(section: &Section, message: &RawMessage) -> Vec<u8> {
    match section {
        Section::All => message.raw().to_vec(),
        Section::Header => header_block(message),
        Section::Text => message.body().to_vec(),
        Section::HeaderFields(names) => {
            filter_headers(message.root(), message.raw(), names, false).into_bytes()
        }
        Section::HeaderFieldsNot(names) => {
            filter_headers(message.root(), message.raw(), names, true).into_bytes()
        }
        Section::Part(path) => message
            .part(path)
            .map(|part| part.owning_bytes(message).to_vec())
            .unwrap_or_default(),
        Section::Mime(path) => message
            .part(path)
            .map(|part| mime_header(part, message))
            .unwrap_or_default(),
        Section::PartHeader(path) => message
            .part(path)
            .map(|part| with_blank_line(part.header_bytes(part.bytes(message.raw())).to_vec()))
            .unwrap_or_default(),
        Section::PartText(path) => message
            .part(path)
            .map(|part| part.body_bytes(message.raw()).to_vec())
            .unwrap_or_default(),
        Section::PartHeaderFields(path, names) => message
            .part(path)
            .map(|part| filter_headers(part, part.bytes(message.raw()), names, false).into_bytes())
            .unwrap_or_default(),
        Section::PartHeaderFieldsNot(path, names) => message
            .part(path)
            .map(|part| filter_headers(part, part.bytes(message.raw()), names, true).into_bytes())
            .unwrap_or_default(),
    }
}

/// A part's own bytes, resolved against the right buffer.
trait OwningBytes {
    /// The part's bytes, including its MIME headers.
    fn owning_bytes(&self, message: &RawMessage) -> Vec<u8>;
}

impl OwningBytes for RawPart {
    fn owning_bytes(&self, message: &RawMessage) -> Vec<u8> {
        let buffer = self.bytes(message.raw());
        let start = self.header.start.min(buffer.len());
        let end = self.body.end.min(buffer.len()).max(start);
        buffer[start..end].to_vec()
    }
}

/// A part's MIME header block, terminated by a blank line.
fn mime_header(part: &RawPart, message: &RawMessage) -> Vec<u8> {
    let buffer = part.bytes(message.raw());
    with_blank_line(part.header.slice(buffer).to_vec())
}

/// Ensure a header block ends with a blank line.
fn with_blank_line(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.is_empty() {
        return bytes;
    }
    if !bytes.ends_with(b"\r\n") {
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\n");
    bytes
}

/// Rebuild a header block containing (or excluding) the named fields.
///
/// Header names are compared case-insensitively and the fields keep the order
/// they have on disk, which is what clients expect for `BODY[HEADER.FIELDS …]`.
fn filter_headers(part: &RawPart, buffer: &[u8], names: &[String], invert: bool) -> String {
    let wanted = |name: &str| {
        let listed = names.iter().any(|want| want.eq_ignore_ascii_case(name));
        if invert {
            !listed
        } else {
            listed
        }
    };

    let mut out = String::new();
    for (name, value) in part.headers.iter() {
        if wanted(name) {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
    }
    let _ = buffer;
    if out.is_empty() {
        // RFC 3501: an empty field list still yields the terminating blank line.
        return "\r\n".to_string();
    }
    out.push_str("\r\n");
    out
}

// ---------------------------------------------------------------------------
// ENVELOPE
// ---------------------------------------------------------------------------

/// One `address` of an `ENVELOPE` address list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeAddress {
    /// The display name, `NIL` when absent.
    pub name: Option<String>,
    /// The `adl` (source route), always `NIL` in practice.
    pub route: Option<String>,
    /// The local part, or the group name when this entry opens a group.
    pub mailbox: Option<String>,
    /// The domain, `NIL` for a group start/end marker.
    pub host: Option<String>,
}

impl EnvelopeAddress {
    /// A normal mailbox address.
    pub fn mailbox(name: Option<String>, mailbox: &str, host: &str) -> Self {
        EnvelopeAddress {
            name,
            route: None,
            mailbox: Some(mailbox.to_string()),
            host: Some(host.to_string()),
        }
    }

    /// The start of a group: `(NIL NIL "group name" NIL)`.
    pub fn group_start(name: &str) -> Self {
        EnvelopeAddress {
            name: None,
            route: None,
            mailbox: Some(name.to_string()),
            host: None,
        }
    }

    /// The end of a group: `(NIL NIL NIL NIL)`.
    pub fn group_end() -> Self {
        EnvelopeAddress {
            name: None,
            route: None,
            mailbox: None,
            host: None,
        }
    }

    /// The wire form of one address.
    pub fn render(&self) -> String {
        format!(
            "({} {} {} {})",
            NString::quoted(self.name.as_deref()),
            NString::quoted(self.route.as_deref()),
            NString::quoted(self.mailbox.as_deref()),
            NString::quoted(self.host.as_deref())
        )
    }
}

/// The `ENVELOPE` of a message (RFC 3501 §7.4.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    /// The `Date:` header, verbatim as an `nstring`.
    pub date: Option<String>,
    /// The `Subject:` header, verbatim (RFC 2047 words are *not* decoded for
    /// `ENVELOPE` — the client does that).
    pub subject: Option<String>,
    /// `From:`.
    pub from: Vec<EnvelopeAddress>,
    /// `Sender:`, defaulting to `From`.
    pub sender: Vec<EnvelopeAddress>,
    /// `Reply-To:`, defaulting to `From`.
    pub reply_to: Vec<EnvelopeAddress>,
    /// `To:`.
    pub to: Vec<EnvelopeAddress>,
    /// `Cc:`.
    pub cc: Vec<EnvelopeAddress>,
    /// `Bcc:`.
    pub bcc: Vec<EnvelopeAddress>,
    /// `In-Reply-To:`.
    pub in_reply_to: Option<String>,
    /// `Message-ID:`.
    pub message_id: Option<String>,
}

impl Envelope {
    /// Build the envelope from a message's top-level headers.
    pub fn from_headers(headers: &Headers) -> Self {
        let from = envelope_addresses(headers, "From");
        let sender = {
            let sender = envelope_addresses(headers, "Sender");
            if sender.is_empty() {
                from.clone()
            } else {
                sender
            }
        };
        let reply_to = {
            let reply_to = envelope_addresses(headers, "Reply-To");
            if reply_to.is_empty() {
                from.clone()
            } else {
                reply_to
            }
        };
        Envelope {
            date: headers.get("Date").map(str::to_string),
            subject: headers.get("Subject").map(str::to_string),
            from,
            sender,
            reply_to,
            to: envelope_addresses(headers, "To"),
            cc: envelope_addresses(headers, "Cc"),
            bcc: envelope_addresses(headers, "Bcc"),
            in_reply_to: headers.get("In-Reply-To").map(str::to_string),
            message_id: headers.get("Message-ID").map(str::to_string),
        }
    }

    /// Build the envelope of a message.
    pub fn of(message: &RawMessage) -> Self {
        Envelope::from_headers(&message.root().headers)
    }

    /// The wire form: `(date subject from sender reply-to to cc bcc in-reply-to
    /// message-id)`.
    pub fn render(&self) -> String {
        format!(
            "({} {} {} {} {} {} {} {} {} {})",
            NString::quoted(self.date.as_deref()),
            NString::quoted(self.subject.as_deref()),
            render_address_list(&self.from),
            render_address_list(&self.sender),
            render_address_list(&self.reply_to),
            render_address_list(&self.to),
            render_address_list(&self.cc),
            render_address_list(&self.bcc),
            NString::quoted(self.in_reply_to.as_deref()),
            NString::quoted(self.message_id.as_deref())
        )
    }
}

/// Render one `ENVELOPE` address list, `NIL` when empty.
pub fn render_address_list(addresses: &[EnvelopeAddress]) -> String {
    if addresses.is_empty() {
        return "NIL".to_string();
    }
    let rendered: Vec<String> = addresses.iter().map(EnvelopeAddress::render).collect();
    format!("({})", rendered.join(" "))
}

/// Parse one address header into `ENVELOPE` addresses, preserving group syntax.
fn envelope_addresses(headers: &Headers, field: &str) -> Vec<EnvelopeAddress> {
    let mut out = Vec::new();
    for raw in headers.get_all(field) {
        parse_envelope_addresses(raw, &mut out);
    }
    out
}

/// Parse an address list, keeping `group` markers.
///
/// `Group: a@b, c@d;` becomes `(NIL NIL "Group" NIL) (a@b) (c@d) (NIL NIL NIL
/// NIL)`, which is what RFC 3501 §7.4.2 specifies — the flattened form clients
/// would otherwise see is not what Thunderbird expects in `ENVELOPE`.
fn parse_envelope_addresses(raw: &str, out: &mut Vec<EnvelopeAddress>) {
    let mut rest = raw.trim();
    if rest.is_empty() {
        return;
    }
    // A single group at the top level: `Name: members;`
    if let Some(colon) = group_colon(rest) {
        let name = rest[..colon].trim();
        let members = rest[colon + 1..].trim();
        let members = members.trim_end_matches(';').trim();
        out.push(EnvelopeAddress::group_start(name));
        if !members.is_empty() {
            for mailbox in ferroma_mail::address::parse_address_list(members) {
                out.push(address_of(&mailbox));
            }
        }
        out.push(EnvelopeAddress::group_end());
        return;
    }
    rest = rest.trim();
    for mailbox in ferroma_mail::address::parse_address_list(rest) {
        out.push(address_of(&mailbox));
    }
}

/// A [`Mailbox`] as an `ENVELOPE` address.
fn address_of(mailbox: &Mailbox) -> EnvelopeAddress {
    EnvelopeAddress::mailbox(
        mailbox.name.clone(),
        mailbox.address.local_part(),
        mailbox.address.domain(),
    )
}

/// The colon that opens a group, or `None`.
fn group_colon(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut angle = 0usize;
    let mut comment = 0usize;
    let mut in_quote = false;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if in_quote => index += 1,
            b'"' => in_quote = !in_quote,
            b'<' if !in_quote => angle += 1,
            b'>' if !in_quote => angle = angle.saturating_sub(1),
            b'(' if !in_quote => comment += 1,
            b')' if !in_quote => comment = comment.saturating_sub(1),
            b'@' if !in_quote && angle == 0 && comment == 0 => return None,
            b':' if !in_quote && angle == 0 && comment == 0 => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// BODYSTRUCTURE
// ---------------------------------------------------------------------------

/// The `BODYSTRUCTURE` of a message (RFC 3501 §7.4.2).
///
/// This is the extensible form: every part carries the `md5`, `disposition`,
/// `language` and `location` extension fields, which is what Thunderbird and
/// Outlook read to decide between "show inline" and "show as attachment".
pub fn bodystructure(message: &RawMessage) -> String {
    render_part(message.root(), message)
}

/// The non-extensible `BODY` form, which is `BODYSTRUCTURE` without the
/// extension fields.
pub fn body(message: &RawMessage) -> String {
    render_part_inner(message.root(), message, false)
}

/// Render one part, extensible.
fn render_part(part: &RawPart, message: &RawMessage) -> String {
    render_part_inner(part, message, true)
}

/// Render one part, with or without the extension fields.
fn render_part_inner(part: &RawPart, message: &RawMessage, extensible: bool) -> String {
    // A `message/rfc822` part is *not* a multipart, even though the entity it
    // encapsulates may be one: RFC 3501 gives it its own body-type.
    if part.is_message() && !part.is_multipart() {
        return render_message_part(part, message, extensible);
    }
    if part.is_multipart() || (!part.parts.is_empty() && part.truncated) {
        let mut out = String::from("(");
        for (index, child) in part.parts.iter().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            out.push_str(&render_part(child, message));
        }
        out.push(' ');
        out.push_str(&quoted_or_nil(Some(&part.content_type.subtype)));
        if extensible {
            // Multipart bodies carry the same four extension fields as leaves
            // (RFC 3501 §7.4.2), so a client parsing the extensible form always
            // finds `md5`, `disposition`, `language` and `location` in place.
            out.push(' ');
            out.push_str(&extension_fields(part));
        }
        out.push(')');
        return out;
    }
    render_leaf_part(part, message, extensible)
}

/// The `message/rfc822` body type: `("message" "rfc822" envelope body lines …)`.
fn render_message_part(part: &RawPart, message: &RawMessage, extensible: bool) -> String {
    let lines = part.content_lines(message.raw());
    let (envelope, nested) = match &part.message {
        Some(nested) => (
            Envelope::from_headers(&nested.headers).render(),
            render_part(nested, message),
        ),
        None => ("NIL".to_string(), "NIL".to_string()),
    };
    let mut out = format!(
        "(message rfc822 {} {} {})",
        envelope, nested, lines
    );
    if extensible {
        // Insert the extension fields before the closing parenthesis.
        out.pop();
        out.push(' ');
        out.push_str(&extension_fields(part));
        out.push(')');
    }
    out
}

/// A leaf body type: `("type" "subtype" params id description encoding size …)`.
fn render_leaf_part(part: &RawPart, message: &RawMessage, extensible: bool) -> String {
    let type_ = quoted_or_nil(Some(&part.content_type.type_));
    let subtype = quoted_or_nil(Some(&part.content_type.subtype));
    let params = render_params(part);
    let id = NString::quoted(part.content_id().as_deref());
    let description = NString::quoted(part.description());
    let encoding = quoted_or_nil(Some(part.encoding.as_str()));
    let size = part.body_bytes(message.raw()).len();

    let mut out = format!(
        "({} {} {} {} {} {} {}",
        type_, subtype, params, id, description, encoding, size
    );
    if part.is_text() {
        out.push(' ');
        out.push_str(&part.content_lines(message.raw()).to_string());
    }
    if extensible {
        out.push(' ');
        out.push_str(&extension_fields(part));
    }
    out.push(')');
    out
}

/// The `md5`, `disposition`, `language` and `location` extension fields.
fn extension_fields(part: &RawPart) -> String {
    let md5 = "NIL";
    let disposition = match part.disposition() {
        Some((type_, params)) => {
            // RFC 3501 §7.4.2: the disposition type is a case-insensitive *atom*
            // and clients compare it upper-cased (`ATTACHMENT`/`INLINE`).
            let list = render_param_list(&params);
            format!("({} {})", quoted_or_nil(Some(&type_.to_ascii_uppercase())), list)
        }
        None => "NIL".to_string(),
    };
    let languages = part.languages();
    let language = match languages.len() {
        0 => "NIL".to_string(),
        1 => util::quote(&languages[0]),
        _ => {
            let rendered: Vec<String> = languages.iter().map(|tag| util::quote(tag)).collect();
            format!("({})", rendered.join(" "))
        }
    };
    let location = NString::quoted(part.location());
    format!("{} {} {} {}", md5, disposition, language, location)
}

/// The `("CHARSET" "utf-8")` parameter list of a leaf part.
fn render_params(part: &RawPart) -> String {
    render_param_list(&part.content_type.params)
}

/// Render a parameter list, `NIL` when empty.
fn render_param_list(params: &[(String, String)]) -> String {
    if params.is_empty() {
        return "NIL".to_string();
    }
    let mut out = String::from("(");
    for (index, (name, value)) in params.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&util::quote(&name.to_ascii_uppercase()));
        out.push(' ');
        out.push_str(&util::quote(value));
    }
    out.push(')');
    out
}

/// A quoted string, or `NIL` when the value is absent.
fn quoted_or_nil(value: Option<&str>) -> String {
    match value {
        None => "NIL".to_string(),
        Some(text) => util::quote(text),
    }
}

/// The `Date:` header of a message, as an `Option`.
pub fn message_date(message: &RawMessage) -> Option<DateTime<Utc>> {
    header_date(&message.root().headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Partial;

    // -----------------------------------------------------------------------
    // Fixtures: the same shapes `ferroma-mail`'s own tests use.
    // -----------------------------------------------------------------------

    /// A Thunderbird `multipart/alternative`: text and HTML, quoted-printable.
    const THUNDERBIRD_ALTERNATIVE: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.net>\r\n\
Subject: =?UTF-8?B?SGVsbG8g4pi6?=\r\n\
Date: Wed, 08 Jul 2026 09:00:00 +0000\r\n\
Message-ID: <tb-1@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=\"------------TB\"\r\n\
\r\n\
--------------TB\r\n\
Content-Type: text/plain; charset=UTF-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Hello=20there\r\n\
--------------TB\r\n\
Content-Type: text/html; charset=UTF-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
<html><body>Hello</body></html>\r\n\
--------------TB--\r\n";

    /// An Outlook `multipart/related` with an inline PNG.
    const OUTLOOK_RELATED: &[u8] = b"From: Carol <carol@example.net>\r\n\
To: Dave <dave@example.com>\r\n\
Subject: Newsletter\r\n\
Date: Thu, 09 Jul 2026 10:30:00 +0000\r\n\
Message-ID: <ol-1@example.net>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/related; type=\"text/html\"; boundary=\"----=_NextPart_OL\"\r\n\
\r\n\
------=_NextPart_OL\r\n\
Content-Type: text/html; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
Content-Location: http://example.net/news.html\r\n\
\r\n\
<html><body><img src=3D\"cid:logo@example.net\"></body></html>\r\n\
------=_NextPart_OL\r\n\
Content-Type: image/png; name=\"logo.png\"\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-ID: <logo@example.net>\r\n\
Content-Disposition: inline; filename=\"logo.png\"\r\n\
Content-Location: http://example.net/logo.png\r\n\
\r\n\
iVBORw0KGgo=\r\n\
------=_NextPart_OL--\r\n";

    /// A message with no `Content-Type` at all.
    const NO_CONTENT_TYPE: &[u8] =
        b"From: eve@example.org\r\nTo: alice@example.com\r\nSubject: plain\r\n\r\nJust text.\r\n";

    fn message(raw: &[u8]) -> RawMessage {
        RawMessage::parse(raw.to_vec())
    }

    fn envelope(raw: &[u8]) -> String {
        Envelope::of(&message(raw)).render()
    }

    fn structure(raw: &[u8]) -> String {
        bodystructure(&message(raw))
    }

    // -----------------------------------------------------------------------
    // ENVELOPE
    // -----------------------------------------------------------------------

    #[test]
    fn envelope_of_a_simple_message() {
        let rendered = envelope(
            b"Date: Wed, 08 Jul 2026 09:00:00 +0000\r\n\
              From: Alice <alice@example.com>\r\n\
              To: Bob <bob@example.net>\r\n\
              Subject: hi\r\n\
              Message-ID: <x@y>\r\n\
              In-Reply-To: <p@q>\r\n\r\nbody",
        );
        assert_eq!(
            rendered,
            "(\"Wed, 08 Jul 2026 09:00:00 +0000\" \"hi\" \
             ((\"Alice\" NIL \"alice\" \"example.com\")) \
             ((\"Alice\" NIL \"alice\" \"example.com\")) \
             ((\"Alice\" NIL \"alice\" \"example.com\")) \
             ((\"Bob\" NIL \"bob\" \"example.net\")) \
             NIL NIL \"<p@q>\" \"<x@y>\")"
        );
    }

    #[test]
    fn envelope_sender_and_reply_to_default_to_from() {
        let rendered = envelope(b"From: a@b\r\n\r\nx");
        // from, sender, reply-to all repeat the From list.
        assert_eq!(
            rendered,
            "(NIL NIL ((NIL NIL \"a\" \"b\")) ((NIL NIL \"a\" \"b\")) ((NIL NIL \"a\" \"b\")) NIL NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn envelope_uses_explicit_sender_and_reply_to_when_present() {
        let rendered = envelope(
            b"From: a@b\r\nSender: s@b\r\nReply-To: r@b\r\n\r\nx",
        );
        assert_eq!(
            rendered,
            "(NIL NIL ((NIL NIL \"a\" \"b\")) ((NIL NIL \"s\" \"b\")) ((NIL NIL \"r\" \"b\")) NIL NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn envelope_renders_multiple_addresses_and_cc_bcc() {
        let rendered = envelope(
            b"From: a@b\r\nTo: One <one@x>, two@y\r\nCc: c@z\r\nBcc: d@w\r\n\r\nx",
        );
        assert_eq!(
            rendered,
            "(NIL NIL ((NIL NIL \"a\" \"b\")) ((NIL NIL \"a\" \"b\")) ((NIL NIL \"a\" \"b\")) \
             ((\"One\" NIL \"one\" \"x\") (NIL NIL \"two\" \"y\")) \
             ((NIL NIL \"c\" \"z\")) ((NIL NIL \"d\" \"w\")) NIL NIL)"
        );
    }

    #[test]
    fn envelope_uses_nil_group_markers_for_a_group_address() {
        let rendered = envelope(b"From: a@b\r\nTo: Friends: one@x, two@y;\r\n\r\nx");
        assert!(
            rendered.contains(
                "((NIL NIL \"Friends\" NIL) (NIL NIL \"one\" \"x\") (NIL NIL \"two\" \"y\") (NIL NIL NIL NIL))"
            ),
            "group must be rendered with NIL markers, got {rendered}"
        );
    }

    #[test]
    fn envelope_escapes_quotes_in_display_names() {
        let rendered = envelope(b"From: \"A \\\"quote\\\" B\" <a@b>\r\n\r\nx");
        assert!(
            rendered.contains("(\"A \\\"quote\\\" B\" NIL \"a\" \"b\")"),
            "got {rendered}"
        );
    }

    #[test]
    fn envelope_keeps_encoded_words_undecoded() {
        // RFC 3501 does not ask the server to decode RFC 2047 words here; the
        // client does, and Thunderbird is unhappy if we pre-decode.
        let rendered = envelope(b"Subject: =?UTF-8?B?SGVsbG8=?=\r\nFrom: a@b\r\n\r\nx");
        assert!(rendered.contains("\"=?UTF-8?B?SGVsbG8=?=\""), "got {rendered}");
    }

    #[test]
    fn envelope_of_a_message_without_headers_is_all_nil() {
        let rendered = envelope(b"\r\njust a body");
        assert_eq!(rendered, "(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)");
    }

    #[test]
    fn envelope_address_helpers_render_correctly() {
        assert_eq!(
            EnvelopeAddress::mailbox(Some("A".into()), "a", "b").render(),
            "(\"A\" NIL \"a\" \"b\")"
        );
        assert_eq!(EnvelopeAddress::group_start("G").render(), "(NIL NIL \"G\" NIL)");
        assert_eq!(EnvelopeAddress::group_end().render(), "(NIL NIL NIL NIL)");
        assert_eq!(render_address_list(&[]), "NIL");
    }

    // -----------------------------------------------------------------------
    // BODYSTRUCTURE
    // -----------------------------------------------------------------------

    #[test]
    fn bodystructure_of_a_simple_text_message() {
        assert_eq!(
            structure(NO_CONTENT_TYPE),
            "(\"text\" \"plain\" NIL NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn bodystructure_of_a_thunderbird_alternative() {
        assert_eq!(
            structure(THUNDERBIRD_ALTERNATIVE),
            "((\"text\" \"plain\" (\"CHARSET\" \"UTF-8\") NIL NIL \"quoted-printable\" 13 1 NIL NIL NIL NIL) \
              (\"text\" \"html\" (\"CHARSET\" \"UTF-8\") NIL NIL \"quoted-printable\" 31 1 NIL NIL NIL NIL) \
              \"alternative\" NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn bodystructure_of_an_outlook_related() {
        assert_eq!(
            structure(OUTLOOK_RELATED),
            "((\"text\" \"html\" (\"CHARSET\" \"utf-8\") NIL NIL \"quoted-printable\" 60 1 NIL NIL NIL \
               \"http://example.net/news.html\") \
              (\"image\" \"png\" (\"NAME\" \"logo.png\") \"logo@example.net\" NIL \"base64\" 12 NIL \
               (\"INLINE\" (\"FILENAME\" \"logo.png\")) NIL \"http://example.net/logo.png\") \
              \"related\" NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn bodystructure_parameters_keep_their_wire_order() {
        let raw = b"Content-Type: text/plain; charset=utf-8; format=flowed\r\n\r\nx".to_vec();
        assert_eq!(
            structure(&raw),
            "(\"text\" \"plain\" (\"CHARSET\" \"utf-8\" \"FORMAT\" \"flowed\") NIL NIL \"7bit\" 1 1 NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn bodystructure_reports_base64_size_as_encoded_octets() {
        // The encoded body is 12 octets even though it decodes to 8.
        let raw = b"Content-Type: application/octet-stream\r\n\
                    Content-Transfer-Encoding: base64\r\n\r\n\
                    iVBORw0KGgo=\r\n"
            .to_vec();
        assert_eq!(
            structure(&raw),
            "(\"application\" \"octet-stream\" NIL NIL NIL \"base64\" 14 NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn bodystructure_marks_an_attachment_with_its_disposition() {
        let raw = b"Content-Type: application/pdf; name=\"a.pdf\"\r\n\
                    Content-Disposition: attachment; filename=\"a.pdf\"\r\n\r\nPDF"
            .to_vec();
        assert_eq!(
            structure(&raw),
            "(\"application\" \"pdf\" (\"NAME\" \"a.pdf\") NIL NIL \"7bit\" 3 NIL \
             (\"ATTACHMENT\" (\"FILENAME\" \"a.pdf\")) NIL NIL)"
        );
    }

    #[test]
    fn bodystructure_renders_language_and_location() {
        let raw = b"Content-Type: text/plain\r\nContent-Language: en\r\n\
                    Content-Location: /a/b\r\n\r\nx"
            .to_vec();
        assert_eq!(
            structure(&raw),
            "(\"text\" \"plain\" NIL NIL NIL \"7bit\" 1 1 NIL NIL \"en\" \"/a/b\")"
        );

        let raw = b"Content-Type: text/plain\r\nContent-Language: en, fr\r\n\r\nx".to_vec();
        assert_eq!(
            structure(&raw),
            "(\"text\" \"plain\" NIL NIL NIL \"7bit\" 1 1 NIL NIL (\"en\" \"fr\") NIL)"
        );
    }

    #[test]
    fn bodystructure_of_message_rfc822_includes_the_nested_envelope_and_body() {
        let raw = b"Content-Type: message/rfc822\r\n\r\n\
                    From: inner@example.net\r\nSubject: inner\r\n\r\ninner body\r\n"
            .to_vec();
        assert_eq!(
            structure(&raw),
            "(message rfc822 (NIL \"inner\" ((NIL NIL \"inner\" \"example.net\")) \
             ((NIL NIL \"inner\" \"example.net\")) ((NIL NIL \"inner\" \"example.net\")) \
             NIL NIL NIL NIL NIL) \
             (\"text\" \"plain\" NIL NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL) 4 NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn bodystructure_of_a_multipart_with_a_nested_multipart() {
        let raw = b"Content-Type: multipart/mixed; boundary=OUT\r\n\r\n\
                    --OUT\r\nContent-Type: multipart/alternative; boundary=IN\r\n\r\n\
                    --IN\r\nContent-Type: text/plain\r\n\r\na\r\n\
                    --IN\r\nContent-Type: text/html\r\n\r\nb\r\n\
                    --IN--\r\n\
                    --OUT\r\nContent-Type: application/pdf\r\n\r\nPDF\r\n\
                    --OUT--\r\n"
            .to_vec();
        assert_eq!(
            structure(&raw),
            "(((\"text\" \"plain\" NIL NIL NIL \"7bit\" 1 1 NIL NIL NIL NIL) \
               (\"text\" \"html\" NIL NIL NIL \"7bit\" 1 1 NIL NIL NIL NIL) \
               \"alternative\" NIL NIL NIL NIL) \
              (\"application\" \"pdf\" NIL NIL NIL \"7bit\" 3 NIL NIL NIL NIL) \
              \"mixed\" NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn the_non_extensible_body_form_has_no_extension_fields() {
        let raw = message(NO_CONTENT_TYPE);
        assert_eq!(body(&raw), "(\"text\" \"plain\" NIL NIL NIL \"7bit\" 12 1)");
    }

    #[test]
    fn bodystructure_is_stable_for_repeated_calls() {
        let raw = message(OUTLOOK_RELATED);
        assert_eq!(bodystructure(&raw), bodystructure(&raw));
    }

    #[test]
    fn param_list_rendering() {
        assert_eq!(render_param_list(&[]), "NIL");
        assert_eq!(
            render_param_list(&[("charset".into(), "utf-8".into())]),
            "(\"CHARSET\" \"utf-8\")"
        );
        assert_eq!(quoted_or_nil(None), "NIL");
        assert_eq!(quoted_or_nil(Some("x")), "\"x\"");
    }

    // -----------------------------------------------------------------------
    // Sections
    // -----------------------------------------------------------------------

    #[test]
    fn body_all_header_and_text_of_a_simple_message() {
        let raw = message(NO_CONTENT_TYPE);
        assert_eq!(section_bytes(&Section::All, &raw), NO_CONTENT_TYPE);
        assert_eq!(
            section_bytes(&Section::Header, &raw),
            b"From: eve@example.org\r\nTo: alice@example.com\r\nSubject: plain\r\n\r\n".to_vec()
        );
        assert_eq!(section_bytes(&Section::Text, &raw), b"Just text.\r\n".to_vec());
    }

    #[test]
    fn body_part_returns_the_encoded_part_with_its_headers() {
        let raw = message(THUNDERBIRD_ALTERNATIVE);
        let part1 = section_bytes(&Section::Part(vec![1]), &raw);
        assert_eq!(
            part1,
            b"Content-Type: text/plain; charset=UTF-8\r\n\
              Content-Transfer-Encoding: quoted-printable\r\n\r\n\
              Hello=20there".to_vec()
        );
        let part2 = section_bytes(&Section::Part(vec![2]), &raw);
        assert!(part2.starts_with(b"Content-Type: text/html"));
        assert!(part2.ends_with(b"<html><body>Hello</body></html>"));
    }

    #[test]
    fn body_mime_returns_only_the_mime_header() {
        let raw = message(THUNDERBIRD_ALTERNATIVE);
        assert_eq!(
            section_bytes(&Section::Mime(vec![1]), &raw),
            b"Content-Type: text/plain; charset=UTF-8\r\n\
              Content-Transfer-Encoding: quoted-printable\r\n\r\n"
                .to_vec()
        );
    }

    #[test]
    fn body_header_fields_selects_and_excludes() {
        let raw = message(NO_CONTENT_TYPE);
        assert_eq!(
            section_bytes(
                &Section::HeaderFields(vec!["SUBJECT".into(), "FROM".into()]),
                &raw
            ),
            b"From: eve@example.org\r\nSubject: plain\r\n\r\n".to_vec()
        );
        assert_eq!(
            section_bytes(&Section::HeaderFields(vec!["subject".into()]), &raw),
            b"Subject: plain\r\n\r\n".to_vec()
        );
        assert_eq!(
            section_bytes(&Section::HeaderFieldsNot(vec!["FROM".into()]), &raw),
            b"To: alice@example.com\r\nSubject: plain\r\n\r\n".to_vec()
        );
    }

    #[test]
    fn header_fields_with_no_match_still_terminates_with_a_blank_line() {
        let raw = message(NO_CONTENT_TYPE);
        assert_eq!(
            section_bytes(&Section::HeaderFields(vec!["X-NOPE".into()]), &raw),
            b"\r\n".to_vec()
        );
    }

    #[test]
    fn body_part_text_and_part_header() {
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n\
                    --B\r\nContent-Type: message/rfc822\r\n\r\n\
                    From: inner@x\r\n\r\ninner\r\n\
                    --B--\r\n"
            .to_vec();
        let message = message(&raw);
        assert_eq!(
            section_bytes(&Section::PartText(vec![1]), &message),
            b"From: inner@x\r\n\r\ninner".to_vec()
        );
        assert_eq!(
            section_bytes(&Section::PartHeader(vec![1]), &message),
            b"Content-Type: message/rfc822\r\n\r\n".to_vec()
        );
    }

    #[test]
    fn sections_on_a_nonexistent_part_are_empty_not_an_error() {
        let raw = message(NO_CONTENT_TYPE);
        assert!(section_bytes(&Section::Part(vec![9]), &raw).is_empty());
        assert!(section_bytes(&Section::Mime(vec![9]), &raw).is_empty());
        assert!(section_bytes(&Section::PartText(vec![9]), &raw).is_empty());
        assert!(section_bytes(&Section::PartHeader(vec![9]), &raw).is_empty());
        assert!(section_bytes(&Section::PartHeaderFields(vec![9], vec!["A".into()]), &raw).is_empty());
    }

    #[test]
    fn nested_part_paths_address_the_right_bytes() {
        let raw = b"Content-Type: multipart/mixed; boundary=OUT\r\n\r\n\
                    --OUT\r\nContent-Type: multipart/alternative; boundary=IN\r\n\r\n\
                    --IN\r\nContent-Type: text/plain\r\n\r\nplain\r\n\
                    --IN\r\nContent-Type: text/html\r\n\r\nhtml\r\n\
                    --IN--\r\n\
                    --OUT--\r\n"
            .to_vec();
        let message = message(&raw);
        assert!(section_bytes(&Section::Part(vec![1, 1]), &message).ends_with(b"plain"));
        assert!(section_bytes(&Section::Part(vec![1, 2]), &message).ends_with(b"html"));
        assert!(section_bytes(&Section::Part(vec![1, 3]), &message).is_empty());
    }

    // -----------------------------------------------------------------------
    // Items and partials
    // -----------------------------------------------------------------------

    fn meta(raw: &[u8]) -> MessageMeta {
        let message = RawMessage::parse(raw.to_vec());
        MessageMeta {
            internal_date: chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap_or_default(),
            size: message.len() as u64,
        }
    }

    fn field(item: FetchItem, raw: &[u8]) -> FetchField {
        let message = message(raw);
        let envelope = Envelope::of(&message);
        item_value(
            &item,
            &meta(raw),
            42,
            &Flags::parse("\\Seen"),
            false,
            &message,
            &envelope,
        )
        .expect("item must have a value")
    }

    #[test]
    fn metadata_items_produce_inline_text() {
        assert_eq!(
            field(FetchItem::Uid, NO_CONTENT_TYPE).value,
            FetchValue::Text("42".into())
        );
        assert_eq!(
            field(FetchItem::Rfc822Size, NO_CONTENT_TYPE).value,
            FetchValue::Text(NO_CONTENT_TYPE.len().to_string())
        );
        assert_eq!(
            field(FetchItem::Flags, NO_CONTENT_TYPE).value,
            FetchValue::Text("(\\Seen)".into())
        );
        assert_eq!(
            field(FetchItem::InternalDate, NO_CONTENT_TYPE).value,
            FetchValue::Text("\"15-Jan-2027 08:00:00 +0000\"".into())
        );
    }

    #[test]
    fn recent_is_added_to_the_rendered_flags() {
        let raw = message(NO_CONTENT_TYPE);
        let envelope = Envelope::of(&raw);
        let value = item_value(
            &FetchItem::Flags,
            &meta(NO_CONTENT_TYPE),
            1,
            &Flags::parse("\\Seen"),
            true,
            &raw,
            &envelope,
        )
        .unwrap();
        assert_eq!(value.value, FetchValue::Text("(\\Seen \\Recent)".into()));
    }

    #[test]
    fn body_items_are_literals() {
        assert!(matches!(
            field(FetchItem::Rfc822, NO_CONTENT_TYPE).value,
            FetchValue::Literal(_)
        ));
        assert!(matches!(
            field(FetchItem::Rfc822Text, NO_CONTENT_TYPE).value,
            FetchValue::Literal(_)
        ));
        assert!(matches!(
            field(FetchItem::Rfc822Header, NO_CONTENT_TYPE).value,
            FetchValue::Literal(_)
        ));
        assert_eq!(field(FetchItem::Rfc822, NO_CONTENT_TYPE).key, "RFC822");
        assert_eq!(
            field(FetchItem::Rfc822Header, NO_CONTENT_TYPE).key,
            "RFC822.HEADER"
        );
    }

    #[test]
    fn a_partial_clamps_to_the_stored_size() {
        let item = FetchItem::BodySection(Section::All, Some(Partial { start: 0, len: 10 }));
        match field(item, NO_CONTENT_TYPE).value {
            FetchValue::Literal(bytes) => assert_eq!(bytes, NO_CONTENT_TYPE[..10].to_vec()),
            other => panic!("expected a literal, got {other:?}"),
        }

        let item = FetchItem::BodySection(
            Section::All,
            Some(Partial {
                start: 1_000,
                len: 10,
            }),
        );
        match field(item, NO_CONTENT_TYPE).value {
            FetchValue::Literal(bytes) => assert!(bytes.is_empty()),
            other => panic!("expected a literal, got {other:?}"),
        }

        let item = FetchItem::BodySection(
            Section::All,
            Some(Partial {
                start: 100,
                len: u64::MAX,
            }),
        );
        match field(item, NO_CONTENT_TYPE).value {
            FetchValue::Literal(bytes) => assert!(bytes.is_empty()),
            other => panic!("expected a literal, got {other:?}"),
        }
    }

    #[test]
    fn the_response_key_of_a_section_keeps_the_partial() {
        let item = FetchItem::BodySection(Section::Text, Some(Partial { start: 0, len: 5 }));
        assert_eq!(item.response_key(), "BODY[TEXT]<0.5>");
        assert_eq!(field(item, NO_CONTENT_TYPE).key, "BODY[TEXT]<0.5>");
    }

    // -----------------------------------------------------------------------
    // Response rendering
    // -----------------------------------------------------------------------

    #[test]
    fn render_fields_backfills_literal_lengths() {
        let fields = vec![
            FetchField {
                key: "UID".into(),
                value: FetchValue::Text("7".into()),
            },
            FetchField {
                key: "BODY[TEXT]".into(),
                value: FetchValue::Literal(b"Just text.\r\n".to_vec()),
            },
        ];
        assert_eq!(
            render_fields(&fields).as_slice(),
            "UID 7 BODY[TEXT] {12}\r\nJust text.\r\n".as_bytes()
        );
    }

    #[test]
    fn render_fields_handles_two_literals_in_one_response() {
        let fields = vec![
            FetchField {
                key: "BODY[HEADER]".into(),
                value: FetchValue::Literal(b"A: 1\r\n\r\n".to_vec()),
            },
            FetchField {
                key: "BODY[TEXT]".into(),
                value: FetchValue::Literal(b"body".to_vec()),
            },
        ];
        assert_eq!(
            render_fields(&fields).as_slice(),
            "BODY[HEADER] {8}\r\nA: 1\r\n\r\n BODY[TEXT] {4}\r\nbody".as_bytes()
        );
    }

    #[test]
    fn render_fields_handles_a_zero_length_literal() {
        let fields = vec![FetchField {
            key: "BODY[TEXT]".into(),
            value: FetchValue::Literal(Vec::new()),
        }];
        assert_eq!(
            render_fields(&fields).as_slice(),
            b"BODY[TEXT] {0}\r\n".as_slice()
        );
    }

    #[test]
    fn render_fields_with_no_literals_is_plain_text() {
        let fields = vec![
            FetchField {
                key: "FLAGS".into(),
                value: FetchValue::Text("(\\Seen)".into()),
            },
            FetchField {
                key: "RFC822.SIZE".into(),
                value: FetchValue::Text("10".into()),
            },
        ];
        assert_eq!(
            render_fields(&fields).as_slice(),
            b"FLAGS (\\Seen) RFC822.SIZE 10".as_slice()
        );
    }

    #[test]
    fn render_fields_of_an_empty_list_is_empty() {
        assert!(render_fields(&[]).is_empty());
    }

    #[test]
    fn render_fields_handles_utf8_literals_by_octet_count() {
        let fields = vec![FetchField {
            key: "BODY[TEXT]".into(),
            value: FetchValue::Literal("naïve".as_bytes().to_vec()),
        }];
        let rendered = render_fields(&fields);
        assert_eq!(rendered.as_slice(), "BODY[TEXT] {6}\r\nnaïve".as_bytes());
    }

    #[test]
    fn render_fields_preserves_non_utf8_literal_bytes() {
        // A binary attachment must survive the response untouched.
        let binary: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0xff, 0xfe, 0x00, 0x01];
        let fields = vec![FetchField {
            key: "BODY[2]".into(),
            value: FetchValue::Literal(binary.clone()),
        }];
        let rendered = render_fields(&fields);
        assert_eq!(rendered.len(), "BODY[2] {8}\r\n".len() + 8);
        assert!(rendered.ends_with(&binary));
    }

    #[test]
    fn fetch_value_constructors() {
        assert_eq!(FetchValue::literal(b"x".to_vec()), FetchValue::Literal(vec![b'x']));
        assert_eq!(
            FetchValue::text_literal("x"),
            FetchValue::Literal(vec![b'x'])
        );
    }

    #[test]
    fn header_block_includes_the_terminating_blank_line() {
        let raw = message(NO_CONTENT_TYPE);
        let header = header_block(&raw);
        assert!(header.ends_with(b"\r\n\r\n"));
        let mut joined = header.clone();
        joined.extend_from_slice(raw.body());
        assert_eq!(joined, NO_CONTENT_TYPE);
    }

    #[test]
    fn with_blank_line_is_idempotent() {
        assert_eq!(with_blank_line(b"A: 1\r\n".to_vec()), b"A: 1\r\n\r\n".to_vec());
        assert_eq!(with_blank_line(b"A: 1".to_vec()), b"A: 1\r\n\r\n".to_vec());
        assert_eq!(with_blank_line(Vec::new()), Vec::<u8>::new());
    }

    #[test]
    fn message_date_reads_the_date_header() {
        assert!(message_date(&message(THUNDERBIRD_ALTERNATIVE)).is_some());
        assert!(message_date(&message(b"From: a@b\r\n\r\nx")).is_none());
    }
}



