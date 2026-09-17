//! Ferroma IMAP4rev1 server for third-party clients.
//!
//! Target compatibility (specification §12): Thunderbird, Apple Mail, Outlook,
//! iPhone Mail, Android clients — first `CAPABILITY/LOGIN/LIST/SELECT/FETCH/STORE/
//! SEARCH/UID`, then `APPEND/COPY/MOVE/EXPUNGE/IDLE`.
//!
//! # Layout
//!
//! | Module | What it owns |
//! |---|---|
//! | [`parser`] | the IMAP4rev1 command grammar, including literals |
//! | [`sequence`] | `sequence-set` and UID sets |
//! | [`response`] | every response shape and response code |
//! | [`mailbox`] | mailbox names, `INBOX` folding, the `LIST` wildcards |
//! | [`rawmime`] | the raw-bytes MIME view `FETCH` is built on |
//! | [`fetch`] | `ENVELOPE`, `BODYSTRUCTURE`, sections and partials |
//! | [`search`] | the `SEARCH` key grammar and its evaluation |
//! | [`session`] | the RFC 3501 §3 state machine |
//! | [`server`] | the listener, TLS, and the per-connection driver |
//! | [`auth`] | credential verification over `ferroma-auth` |
//! | [`config`] | the server configuration block |
//!
//! # Design notes
//!
//! * **`FETCH` is built from raw octets.** `BODYSTRUCTURE` reports the *encoded*
//!   size of every part and `BODY[1.2]` returns the bytes as they are on disk, so
//!   the code works from [`rawmime::RawMessage`] rather than the decoded
//!   `ferroma-mail` tree. Decoding is for rendering; IMAP needs the wire bytes.
//! * **No peer input can panic.** Every parser path is total and returns a
//!   `BAD`-shaped [`ferroma_core::FerromaError`]; literals are bounded by
//!   [`config::ImapServerConfig::max_append_size`].
//! * **Protocol only.** The session calls `ferroma-storage` repositories and the
//!   `ferroma-mail` core; it never implements message handling of its own.
//! * **Nothing sensitive is logged.** Command logs carry `connection_id`,
//!   `remote_ip`, `user`, `command`, `duration_ms` and `result` — never a
//!   password, a message body or a literal.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use ferroma_imap::{ImapServer, ImapServerConfig};
//! use ferroma_storage::{Maildir, Repositories};
//!
//! # async fn demo(repos: Arc<Repositories>, maildir: Arc<Maildir>) -> Result<(), Box<dyn std::error::Error>> {
//! let server = Arc::new(ImapServer::new(
//!     ImapServerConfig {
//!         port: 143,
//!         ..ImapServerConfig::default()
//!     },
//!     repos,
//!     maildir,
//! )?);
//!
//! // `serve` returns when the shutdown future resolves.
//! server.serve(async {
//!     let _ = tokio::signal::ctrl_c().await;
//! })
//! .await?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

pub mod auth;
pub mod config;
pub mod fetch;
pub mod mailbox;
pub mod parser;
pub mod rawmime;
pub mod response;
pub mod search;
pub mod sequence;
pub mod server;
pub mod session;
pub mod util;

pub use auth::ServiceAuthenticator;
pub use config::ImapServerConfig;
pub use fetch::{
    body, bodystructure, header_block, item_value, render_fields, section_bytes, Envelope,
    EnvelopeAddress, FetchField, FetchValue, MessageMeta,
};
pub use mailbox::{is_inbox, Pattern, DELIMITER, INBOX};
pub use parser::{
    Command, CommandParser, FetchItem, FetchSpec, LiteralError, LiteralSource, ParsedCommand,
    Section, StoreAction,
};
pub use rawmime::{RawMessage, RawPart, Span};
pub use response::{Response, ResponseCode, Status, StatusItem, StatusItems};
pub use search::{
    charset_supported, evaluate_all, matches as search_matches, MessageFacts, SearchKey,
    SearchRequest, SUPPORTED_CHARSETS,
};
pub use sequence::{Bound, SeqRange, SequenceSet};
pub use server::{tls_acceptor_from_pem, ImapServer, MAX_COMMAND_LINE};
pub use session::{
    apply_store, error_response, flags_to_column, Authenticator, CountingOutput, ImapSession,
    MapAuthenticator, ScriptInput, SessionConfig, SessionContext, SessionFlow, SessionInput,
    SessionOutput, SessionState, VecOutput,
};
pub use util::{internal_date, quote, NString, Partial};
