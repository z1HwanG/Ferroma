//! Ferroma mail core — RFC 5322 messages and MIME.
//!
//! Modules (see the project specification §8 and §14):
//!
//! * [`headers`] — header parsing, folding/unfolding, RFC 2047 encoded words.
//! * [`mime`] — the MIME tree: `Content-Type` parsing, transfer encodings, charsets.
//! * [`message`] — [`message::ParsedMessage`], the server's view of one email.
//! * [`envelope`] — SMTP envelope data captured at `MAIL FROM` / `RCPT TO`.
//! * [`builder`] — assembling outgoing messages (text, HTML, attachments).
//! * [`flags`] — IMAP flags (`\Seen`, `\Flagged`, …) and custom keywords.
//! * [`address`] — header address lists (display names, groups).

#![warn(missing_docs)]

pub mod address;
pub mod builder;
pub mod envelope;
pub mod flags;
pub mod headers;
pub mod message;
pub mod mime;

pub use address::Mailbox as AddressMailbox;
pub use builder::MessageBuilder;
pub use envelope::Envelope;
pub use flags::Flags;
pub use headers::Headers;
pub use message::ParsedMessage;
pub use mime::{ContentType, MimePart, TransferEncoding};
