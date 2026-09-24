//! Ferroma SMTP — both directions.
//!
//! # Inbound
//!
//! [`server`] owns the listener and the per-connection loop; [`parser`] turns a line
//! of bytes into a [`parser::Command`]; [`session`] is the state machine that decides
//! whether that command is legal *now*; [`reply`] is the only place a reply is
//! worded or CRLF-terminated; [`delivery`] turns an accepted message into stored
//! state.
//!
//! ```text
//!   TCP ─► ConnectionLimiter ─► parser::parse_command ─► session guards
//!                                                             │
//!                          reply::Reply ◄──────────────────────┤
//!                                                             │
//!                                       delivery::DeliveryService ─► Maildir + PostgreSQL
//! ```
//!
//! # Outbound
//!
//! [`mx`] resolves where a domain's mail goes; [`client`] speaks SMTP to it;
//! [`queue`] is the worker pool that turns `mail_queue` rows into attempts, retries
//! and bounces.
//!
//! # Policy
//!
//! [`dkim`] signs what we send and verifies what we receive, [`spf`] and [`dmarc`]
//! evaluate inbound policy, and [`auth_results`] writes the single
//! `Authentication-Results:` header that reports all three. Everything DNS-facing
//! goes through the [`mx::Resolver`] trait, so SPF/DKIM/DMARC are unit-testable
//! without a network.
//!
//! # Ground rules
//!
//! * No `unwrap()` outside tests. Peers control every byte that reaches the parser
//!   and every byte of MIME that reaches the mail core.
//! * Input is strict (`\r\n` expected, bare `\n` tolerated the way real servers do
//!   it) and output is always `\r\n`.
//! * Nothing in this crate logs a message body, a password or a private key.

#![warn(missing_docs)]

pub mod auth_results;
pub mod client;
pub mod connection;
pub mod delivery;
pub mod dkim;
pub mod dmarc;
pub mod inbound;
pub mod greylist;
pub mod mx;
pub mod parser;
pub mod queue;
pub mod reply;
pub mod server;
pub mod session;
pub mod spf;

/// The base64 engine the SASL path uses.
///
/// Re-exported so integration tests and downstream callers can build an `AUTH`
/// payload without taking their own dependency on `base64`, and so they encode
/// exactly the way the server decodes:
///
/// ```no_run
/// use ferroma_smtp::base64::{engine::general_purpose::STANDARD, Engine as _};
///
/// let payload = STANDARD.encode("\0alice@example.com\0secret");
/// assert!(!payload.is_empty());
/// ```
pub use ::base64;

pub use auth_results::{AuthResult, AuthResults, AuthResultsBuilder};
pub use client::{classify_reply, DeliveryOutcome, SmtpClient, SmtpClientConfig, TlsPolicy};
pub use connection::{ConnectionLimiter, ConnectionPermit, LimitKind, RateLimited};
pub use delivery::{
    DeliveryReport, DeliveryService, ReceivedMessage, RecipientOutcome, ResolvedRecipient,
    SharedDelivery, INBOX, JUNK,
};
pub use dkim::{
    Canonicalization, DkimKey, DkimKeyRecord, DkimResult, DkimSignature, DkimSigner, DkimVerdict,
    DkimVerifier,
};
pub use dmarc::{DmarcAlignment, DmarcChecker, DmarcPolicy, DmarcRecord, DmarcResult, DmarcVerdict};
pub use inbound::{
    policy_budget, InboundPolicy, InboundVerdict, PolicyAction, POLICY_TIMEOUT_CAP,
    POLICY_TIMEOUT_FLOOR,
};
pub use mx::{
    DnsHealth, DnsRecordCheck, DnsRecordKind, DnsStatus, HickoryResolver, MockResolver, MxHost,
    MxLookup, MxResolver, Resolver,
};
pub use parser::{parse_command, AuthParams, BodyType, Command, MailParams, RcptParams, SmtpError};
pub use queue::{enqueue, next_attempt_at, QueueConfigView, QueueWorker, QueueWorkerHandle};
pub use reply::Reply;
pub use server::{
    ehlo_extensions, tls_acceptor, tls_acceptor_from_files, AsyncReadWrite, ListenerKind,
    SessionResult, SmtpListener, SmtpServer, SmtpServerConfig, SmtpServerHandle,
    AUTH_LOGIN_PASSWORD_CHALLENGE, AUTH_LOGIN_USERNAME_CHALLENGE,
};
pub use session::{AuthState, SmtpSession, SmtpState, Transaction};
pub use spf::{SpfChecker, SpfMechanism, SpfOutcome, SpfQualifier, SpfRecord, SpfResult};
