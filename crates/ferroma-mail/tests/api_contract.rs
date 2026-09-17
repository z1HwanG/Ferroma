//! Compile-time proof that `ferroma-mail` exposes exactly the API the rest of
//! the platform is written against.
//!
//! Most of this file is never executed in anger: its value is that it *compiles*.
//! If a signature drifts — a `&str` becomes a `String`, an `Option` gains a
//! layer, a lifetime disappears — this file stops building and the change is
//! caught here instead of in five downstream crates.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use ferroma_core::{EmailAddress, FerromaError, RfcMessageId, Result};

use ferroma_mail::{
    AddressMailbox, Envelope, Flags, Headers, MessageBuilder, ParsedMessage, TransferEncoding,
};
use ferroma_mail::address::{format_address_list, parse_address_list, Mailbox};
use ferroma_mail::headers::{decode_encoded_words, encode_header_value, fold_header_line};
use ferroma_mail::message::ParseLimits;
use ferroma_mail::mime::{
    decode_base64, decode_charset, decode_quoted_printable, encode_base64,
    encode_quoted_printable, ContentType, MimePart,
};

/// `headers.rs` — every signature the contract names.
#[test]
fn headers_api_is_exact() -> Result<()> {
    let mut h: Headers = Headers::new();
    let parsed: Headers = Headers::parse("Subject: hi\r\n")?;
    h.append("X-A", "1");
    h.insert("Subject", "s");
    let _: Option<&str> = h.get("Subject");
    let _: Vec<&str> = h.get_all("Subject");
    let _: bool = h.contains("subject");
    let _: usize = h.len();
    let _: bool = h.is_empty();
    let _: String = h.render();
    let _: String = format!("{h}");
    let _: Option<String> = h.subject();
    let _: Option<DateTime<Utc>> = h.date();
    let _: Option<RfcMessageId> = h.message_id();
    let _: Vec<Mailbox> = h.from();
    let _: Vec<Mailbox> = h.to();
    let _: Vec<Mailbox> = h.cc();
    let _: Vec<Mailbox> = h.reply_to();
    for (name, value) in h.iter() {
        let _: (&str, &str) = (name, value);
    }
    h.remove("X-A");

    let _: String = decode_encoded_words("=?UTF-8?B?SGk=?=");
    let _: String = encode_header_value("hi");
    let _: String = fold_header_line("Subject", "a long value");
    let _ = parsed;
    Ok(())
}

/// `address.rs`.
#[test]
fn address_api_is_exact() -> Result<()> {
    let addr: EmailAddress = EmailAddress::parse("alice@example.com")?;
    let m: Mailbox = Mailbox::new(addr.clone());
    let with_name: Mailbox = Mailbox::with_name(addr, "Alice");
    let _: String = with_name.display();
    let _: String = format!("{with_name}");
    let _: Option<String> = m.name.clone();
    let _: EmailAddress = m.address.clone();
    let list: Vec<Mailbox> = parse_address_list("a@b.com, c@d.com");
    let _: String = format_address_list(&list);
    // `AddressMailbox` is the re-export the rest of the platform imports.
    let alias: AddressMailbox = with_name.clone();
    assert_eq!(alias, with_name);
    Ok(())
}

/// `mime.rs`.
#[test]
fn mime_api_is_exact() {
    let ct: ContentType = ContentType::parse("text/plain; charset=utf-8");
    let _: String = ct.type_.clone();
    let _: String = ct.subtype.clone();
    let _: Vec<(String, String)> = ct.params.clone();
    let _: Option<&str> = ct.param("charset");
    let _: bool = ct.is_multipart();
    let _: bool = ct.is_text();
    let _: bool = ct.is_message();
    let _: Option<&str> = ct.charset();
    let _: Option<&str> = ct.boundary();
    let _: Option<&str> = ct.name();
    let _: String = ct.to_string();
    let _: String = format!("{ct}");
    let _: ContentType = ContentType::default();

    let te: TransferEncoding = TransferEncoding::parse("base64");
    let _: &'static str = te.as_str();
    let _: TransferEncoding = TransferEncoding::default();
    // The enum is `Copy`, so it can be used after being passed by value.
    let copied = te;
    assert_eq!(copied, TransferEncoding::Base64);
    assert_eq!(TransferEncoding::Base64.as_str(), "base64");

    let mut part: MimePart = MimePart::leaf(ct.clone(), b"hello".to_vec());
    part.headers.append("Content-Disposition", "attachment; filename=\"a.txt\"");
    let _: Headers = part.headers.clone();
    let _: ContentType = part.content_type.clone();
    let _: TransferEncoding = part.encoding;
    let _: Vec<u8> = part.content.clone();
    let _: Vec<MimePart> = part.parts.clone();
    let _: bool = part.is_multipart();
    let _: bool = part.is_text();
    let _: bool = part.is_attachment();
    let _: Option<String> = part.filename();
    let _: Option<&str> = part.content_id();
    let _: Option<&str> = part.disposition();
    let _: Option<String> = part.decode_text();
    let _: &[MimePart] = part.subparts();
    part.walk(&mut |_p: &MimePart| {});
    let _: Option<&MimePart> = part.find_first(&|p: &MimePart| p.is_text());

    let _: Vec<u8> = decode_base64(b"SGk=").expect("valid base64");
    let _: String = encode_base64(b"Hi");
    let _: Vec<u8> = decode_quoted_printable(b"Hi").expect("valid qp");
    let _: String = encode_quoted_printable(b"Hi");
    let _: String = decode_charset(b"hi", "utf-8");
}

/// `message.rs`.
#[test]
fn message_api_is_exact() -> Result<()> {
    let raw = b"From: a@b.com\r\nSubject: s\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
    let msg: ParsedMessage = ParsedMessage::parse(raw)?;
    let limits: ParseLimits = ParseLimits::default();
    let _: usize = limits.max_depth;
    let _: usize = limits.max_parts;
    let _: usize = limits.max_part_size;
    let _: usize = limits.max_message_size;
    let _: ParsedMessage = ParsedMessage::parse_with_limits(raw, &limits)?;

    let _: Headers = msg.headers.clone();
    let _: Vec<MimePart> = msg.body.clone();
    let _: Option<&str> = msg.header("Subject");
    let _: Option<String> = msg.subject();
    let _: Vec<Mailbox> = msg.from();
    let _: Vec<Mailbox> = msg.to();
    let _: Vec<Mailbox> = msg.cc();
    let _: Vec<Mailbox> = msg.reply_to();
    let _: Option<DateTime<Utc>> = msg.date();
    let _: Option<RfcMessageId> = msg.message_id();
    let _: Option<String> = msg.text_body();
    let _: Option<String> = msg.html_body();
    let _: Vec<&MimePart> = msg.attachments();
    let _: bool = msg.has_attachments();
    let _: bool = msg.is_multipart();
    let _: &[MimePart] = msg.parts();
    let _: String = msg.snippet(40);
    Ok(())
}

/// `builder.rs`.
#[test]
fn builder_api_is_exact() -> Result<()> {
    let when: DateTime<Utc> = Utc::now();
    let builder: MessageBuilder = MessageBuilder::new()
        .from("Alice <alice@example.com>")
        .to("bob@example.org")
        .cc("carol@example.org")
        .bcc("dave@example.org")
        .reply_to("eve@example.org")
        .subject("hi")
        .text("plain")
        .html("<p>html</p>")
        .header("X-Test", "1")
        .attachment("a.bin", "application/octet-stream", vec![1, 2, 3])
        .in_reply_to("parent@example.com")
        .references(&["one@example.com".to_string(), "two@example.com".to_string()])
        .date(when)
        .message_id(RfcMessageId::new("fixed@example.com"));

    let bytes: Vec<u8> = builder.clone().build()?;
    let reparsed: ParsedMessage = builder.build_message()?;
    assert_eq!(reparsed.text_body().as_deref(), Some("plain"));
    assert!(!bytes.is_empty());

    let _: MessageBuilder = MessageBuilder::default();
    Ok(())
}

/// `envelope.rs`.
#[test]
fn envelope_api_is_exact() -> Result<()> {
    let addr: EmailAddress = EmailAddress::parse("alice@example.com")?;
    let ip: IpAddr = "192.0.2.1".parse().expect("valid ip");
    let mut env: Envelope = Envelope::new()
        .with_from(addr.clone())
        .with_helo("mail.example.com")
        .with_remote_ip(ip);
    env.add_recipient(addr);

    let _: Option<EmailAddress> = env.from.clone();
    let _: Vec<EmailAddress> = env.recipients.clone();
    let _: Option<String> = env.helo.clone();
    let _: Option<IpAddr> = env.remote_ip;
    let _: DateTime<Utc> = env.received_at;
    let _: usize = env.recipient_count();
    let _: String = env.received_header("mx.local");
    let _: Envelope = Envelope::default();
    Ok(())
}

/// `flags.rs`.
#[test]
fn flags_api_is_exact() {
    let mut f: Flags = Flags::new();
    f.set_seen(true);
    f.set_answered(true);
    f.set_flagged(true);
    f.set_deleted(true);
    f.set_draft(true);
    f.set_recent(true);
    let _: bool = f.seen();
    let _: bool = f.answered();
    let _: bool = f.flagged();
    let _: bool = f.deleted();
    let _: bool = f.draft();
    let _: bool = f.recent();
    f.add_keyword("$junk");
    let _: &[String] = f.keywords();
    let _: bool = f.has_keyword("$junk");
    f.remove_keyword("$junk");

    let parsed: Flags = Flags::parse("(\\Seen)");
    let _: String = parsed.to_imap_string();
    let _: String = parsed.to_db_string();
    let _: Flags = Flags::from_db_string("seen");
    let _: bool = parsed.is_empty();
    let _: String = format!("{parsed}");
    let _: Flags = Flags::default();
    assert_eq!(parsed, Flags::from_db_string(&parsed.to_db_string()));
}

/// The crate-level re-exports named in `lib.rs` all resolve.
#[test]
fn module_reexports_resolve() {
    fn assert_is_headers(_: &ferroma_mail::Headers) {}
    fn assert_is_builder(_: &ferroma_mail::MessageBuilder) {}
    fn assert_is_envelope(_: &ferroma_mail::Envelope) {}
    fn assert_is_flags(_: &ferroma_mail::Flags) {}
    fn assert_is_message(_: &ferroma_mail::ParsedMessage) {}
    fn assert_is_content_type(_: &ferroma_mail::ContentType) {}
    fn assert_is_part(_: &ferroma_mail::MimePart) {}
    fn assert_is_encoding(_: ferroma_mail::TransferEncoding) {}
    fn assert_is_mailbox(_: &ferroma_mail::AddressMailbox) {}

    assert_is_headers(&Headers::new());
    assert_is_builder(&MessageBuilder::new());
    assert_is_envelope(&Envelope::new());
    assert_is_flags(&Flags::new());
    assert_is_encoding(TransferEncoding::SevenBit);
    assert_is_mailbox(&Mailbox::new(
        EmailAddress::parse("a@b.com").expect("valid"),
    ));
    let msg = ParsedMessage::parse(b"Content-Type: text/plain\r\n\r\nx\r\n").expect("parses");
    assert_is_message(&msg);
    assert_is_content_type(&msg.parts()[0].content_type);
    assert_is_part(&msg.parts()[0]);
}

/// The error type flowing out is `ferroma_core::FerromaError`, not a local one.
#[test]
fn errors_are_ferroma_errors() -> Result<()> {
    let err: FerromaError = ParsedMessage::parse_with_limits(
        b"x",
        &ParseLimits {
            max_message_size: 0,
            ..ParseLimits::default()
        },
    )
    .expect_err("must be a limit violation");
    assert!(matches!(err, FerromaError::LimitExceeded(_)));

    // `?` carries a `FerromaError` straight through, which is what every caller
    // in the SMTP/IMAP/storage crates relies on.
    let headers: Headers = Headers::parse("Subject: ok\r\n")?;
    assert_eq!(headers.get("Subject"), Some("ok"));
    Ok(())
}
