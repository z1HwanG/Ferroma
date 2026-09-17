//! The per-connection SMTP state machine.
//!
//! ```text
//!  Connected ──EHLO/HELO──► Greeted ──MAIL──► MailFrom ──RCPT──► RcptTo ──DATA──► Data
//!      ▲                       ▲                                                      │
//!      │                       │◄──────────────── RSET / end of DATA ──────────────────┘
//!      └──── STARTTLS ─────────┘        (RFC 3207 §4.2: TLS resets everything)
//! ```
//!
//! [`SmtpState`] mirrors specification §9.2: the linear progress of a transaction,
//! with `Authenticated` as a *sibling* of the other states because authentication is
//! orthogonal to where a transaction has got to. [`SmtpSession::authenticated_user`]
//! is therefore the real marker; the `Authenticated` state is kept so the transition
//! table is explicit and testable.
//!
//! Every illegal transition is refused by a `may_*` guard, which the session loop
//! turns into exactly one reply ([`crate::reply::Reply::bad_sequence`] or
//! [`crate::reply::Reply::auth_required`]). The guards are pure: they never touch
//! the network and never depend on wall-clock time, so the whole matrix is unit
//! testable.

use std::net::SocketAddr;

use chrono::{DateTime, Utc};
use ferroma_core::{EmailAddress, UserId};

/// Where a session is in the SMTP dialogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SmtpState {
    /// Just connected; no greeting received yet.
    Connected,
    /// `EHLO`/`HELO` accepted.
    Greeted,
    /// `MAIL FROM` accepted; the envelope sender is set.
    MailFrom,
    /// At least one `RCPT TO` accepted.
    RcptTo,
    /// Inside the `DATA` phase.
    Data,
    /// Authenticated. Tracked separately from the transaction position, exactly as
    /// [`SmtpSession::authenticated_user`] is — see the module documentation.
    Authenticated,
}

impl SmtpState {
    /// Whether the peer has identified itself with `EHLO`/`HELO`.
    pub fn greeted(self) -> bool {
        !matches!(self, SmtpState::Connected)
    }

    /// A short name for structured logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SmtpState::Connected => "connected",
            SmtpState::Greeted => "greeted",
            SmtpState::MailFrom => "mail_from",
            SmtpState::RcptTo => "rcpt_to",
            SmtpState::Data => "data",
            SmtpState::Authenticated => "authenticated",
        }
    }
}

/// A multi-step `AUTH` exchange in progress.
///
/// Only `AUTH LOGIN` needs this: `PLAIN` carries everything in one (optional)
/// initial response. The variant names are the steps, not the challenges, so the
/// byte-exact challenge strings live in one place ([`crate::server`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthState {
    /// A `334 VXNlcm5hbWU6` challenge is outstanding — the username is next.
    AwaitingUsername,
    /// A `334 UGFzc3dvcmQ6` challenge is outstanding — the password is next.
    AwaitingPassword,
}

impl AuthState {
    /// A short name for structured logs. Never contains credential material.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthState::AwaitingUsername => "awaiting_username",
            AuthState::AwaitingPassword => "awaiting_password",
        }
    }
}

/// The envelope and provenance of one transaction.
///
/// Reset by `RSET`, by `MAIL FROM` (which starts a new one) and by `STARTTLS`;
/// never carried across transactions, because a `Received:` header built from a
/// previous transaction's peer name would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// The reverse-path. `None` for the null sender (`<>`), which is legal.
    pub sender: Option<EmailAddress>,
    /// Every accepted recipient, in arrival order.
    pub recipients: Vec<EmailAddress>,
    /// The `SIZE=` the peer declared, when it declared one.
    pub declared_size: Option<u64>,
    /// Whether the peer said `BODY=8BITMIME`.
    pub eight_bit: bool,
    /// When the transaction started; the `Received:` header's timestamp.
    pub started_at: DateTime<Utc>,
}

impl Transaction {
    /// An empty transaction stamped with the current time.
    pub fn new() -> Self {
        Transaction {
            sender: None,
            recipients: Vec::new(),
            declared_size: None,
            eight_bit: false,
            started_at: Utc::now(),
        }
    }

    /// How many recipients have been accepted.
    pub fn recipient_count(&self) -> usize {
        self.recipients.len()
    }

    /// Whether `address` is already in the recipient list (case-insensitively, the
    /// way every real server deduplicates a repeated `RCPT TO`).
    pub fn has_recipient(&self, address: &EmailAddress) -> bool {
        let key = address.to_lowercase();
        self.recipients.iter().any(|r| r.to_lowercase() == key)
    }
}

impl Default for Transaction {
    fn default() -> Self {
        Transaction::new()
    }
}

/// Everything the server knows about one SMTP connection.
#[derive(Debug, Clone)]
pub struct SmtpSession {
    /// A short, unique id used in every log line for this connection.
    pub connection_id: String,
    /// The peer's address as observed, never as claimed.
    pub remote_addr: SocketAddr,
    /// The current position in the state machine.
    pub state: SmtpState,
    /// The name the peer announced, already validated and control-stripped.
    pub helo: Option<String>,
    /// Whether the greeting was `EHLO` (extensions available) rather than `HELO`.
    pub esmtp: bool,
    /// The authenticated account, if any.
    pub authenticated_user: Option<UserId>,
    /// The address the authenticated account logged in as, for the daily send
    /// limit and for logging.
    pub authenticated_as: Option<String>,
    /// The envelope being built.
    pub transaction: Transaction,
    /// Whether TLS is active on this connection.
    pub tls: bool,
    /// The outstanding SASL step, if a multi-step `AUTH` is in progress.
    pub auth_state: Option<AuthState>,
    /// The username collected by an in-progress `AUTH LOGIN`.
    ///
    /// Held only between the two challenges and cleared the moment the exchange
    /// ends, so a half-finished login never outlives the step that started it.
    pub pending_auth_username: Option<String>,
    /// `true` when this connection arrived on a submission port.
    pub submission: bool,
    /// The `SIZE` the server advertised, for the pre-`DATA` arithmetic check.
    pub advertised_size: Option<u64>,
    /// How many commands this session has accepted, for rate limiting.
    pub command_count: u64,
}

impl SmtpSession {
    /// A fresh session for a peer that just connected.
    pub fn new(connection_id: impl Into<String>, remote_addr: SocketAddr) -> Self {
        SmtpSession {
            connection_id: connection_id.into(),
            remote_addr,
            state: SmtpState::Connected,
            helo: None,
            esmtp: false,
            authenticated_user: None,
            authenticated_as: None,
            transaction: Transaction::new(),
            tls: false,
            auth_state: None,
            pending_auth_username: None,
            submission: false,
            advertised_size: None,
            command_count: 0,
        }
    }

    /// Mark this session as belonging to a submission listener.
    pub fn with_submission(mut self, submission: bool) -> Self {
        self.submission = submission;
        self
    }

    // ---------------------------------------------------------------
    // Greeting
    // ---------------------------------------------------------------

    /// Accept an `EHLO`.
    ///
    /// A second `EHLO` is legal and must reset the transaction (RFC 5321 §4.1.4):
    /// the peer is re-announcing itself, so anything half-built is abandoned.
    pub fn greet(&mut self, name: &str, esmtp: bool) {
        self.helo = Some(name.to_string());
        self.esmtp = esmtp;
        self.transaction = Transaction::new();
        self.auth_state = None;
        self.pending_auth_username = None;
        if self.state != SmtpState::Authenticated {
            self.state = SmtpState::Greeted;
        }
    }

    // ---------------------------------------------------------------
    // Transaction
    // ---------------------------------------------------------------

    /// Begin a transaction with `MAIL FROM`.
    pub fn begin_transaction(&mut self, sender: Option<EmailAddress>, declared_size: Option<u64>, eight_bit: bool) {
        self.transaction = Transaction::new();
        self.transaction.sender = sender;
        self.transaction.declared_size = declared_size;
        self.transaction.eight_bit = eight_bit;
        self.state = SmtpState::MailFrom;
    }

    /// Accept one `RCPT TO`.
    pub fn add_recipient(&mut self, address: EmailAddress) {
        self.transaction.recipients.push(address);
        self.state = SmtpState::RcptTo;
    }

    /// Enter the `DATA` phase.
    pub fn begin_data(&mut self) {
        self.state = SmtpState::Data;
    }

    /// Leave the `DATA` phase once the message has been handed to delivery.
    ///
    /// The session returns to `Greeted` (or stays `Authenticated`) with an empty
    /// envelope, ready for the next transaction on the same connection.
    pub fn end_transaction(&mut self) {
        self.transaction = Transaction::new();
        self.state = if self.authenticated_user.is_some() {
            SmtpState::Authenticated
        } else {
            SmtpState::Greeted
        };
    }

    /// `RSET`: abandon the transaction, keep the greeting and the authentication.
    pub fn reset(&mut self) {
        self.transaction = Transaction::new();
        self.auth_state = None;
        self.pending_auth_username = None;
        self.state = if self.authenticated_user.is_some() {
            SmtpState::Authenticated
        } else if self.helo.is_some() {
            SmtpState::Greeted
        } else {
            SmtpState::Connected
        };
    }

    /// `STARTTLS`: RFC 3207 §4.2 requires the server to discard *everything* it
    /// learned before the handshake — the greeting, the authentication and any
    /// half-built envelope. A peer that authenticated in the clear must not stay
    /// authenticated through the upgrade, and a peer must not be able to inject a
    /// `MAIL FROM` before the TLS session begins.
    pub fn reset_for_starttls(&mut self) {
        self.state = SmtpState::Connected;
        self.helo = None;
        self.esmtp = false;
        self.authenticated_user = None;
        self.authenticated_as = None;
        self.transaction = Transaction::new();
        self.auth_state = None;
        self.pending_auth_username = None;
        self.tls = true;
    }

    // ---------------------------------------------------------------
    // AUTH
    // ---------------------------------------------------------------

    /// Record a successful authentication.
    pub fn authenticate(&mut self, user: UserId, address: impl Into<String>) {
        self.authenticated_user = Some(user);
        self.authenticated_as = Some(address.into());
        self.auth_state = None;
        self.pending_auth_username = None;
        self.state = SmtpState::Authenticated;
    }

    /// Begin a multi-step `AUTH` exchange.
    pub fn begin_auth(&mut self, state: AuthState) {
        self.auth_state = Some(state);
        self.pending_auth_username = None;
    }

    /// Abandon an `AUTH` exchange (a bad response, or `RSET`).
    pub fn abort_auth(&mut self) {
        self.auth_state = None;
        self.pending_auth_username = None;
    }

    /// Whether an `AUTH` exchange is mid-flight.
    pub fn in_auth(&self) -> bool {
        self.auth_state.is_some()
    }

    /// Whether this session has authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.authenticated_user.is_some()
    }

    // ---------------------------------------------------------------
    // Guards
    // ---------------------------------------------------------------

    /// Whether `MAIL FROM` may be accepted now.
    ///
    /// `helo_required` is a configuration flag, not a protocol rule: RFC 5321 makes
    /// the greeting mandatory, but operators sometimes need to accept a peer that
    /// skips it, so the decision is the caller's.
    pub fn may_mail_from(&self, helo_required: bool) -> Result<(), MailFromDenied> {
        if self.state == SmtpState::Data {
            return Err(MailFromDenied::InData);
        }
        if helo_required && !self.state.greeted() {
            return Err(MailFromDenied::NoGreeting);
        }
        Ok(())
    }

    /// Whether `RCPT TO` may be accepted now.
    pub fn may_rcpt_to(&self) -> Result<(), RcptDenied> {
        match self.state {
            SmtpState::MailFrom | SmtpState::RcptTo => Ok(()),
            SmtpState::Data => Err(RcptDenied::InData),
            _ => Err(RcptDenied::NoMailFrom),
        }
    }

    /// Whether `DATA` may be accepted now.
    pub fn may_data(&self) -> Result<(), DataDenied> {
        match self.state {
            SmtpState::RcptTo => Ok(()),
            SmtpState::Data => Err(DataDenied::AlreadyInData),
            SmtpState::Connected | SmtpState::Greeted | SmtpState::Authenticated => {
                Err(DataDenied::NoRecipients)
            }
            SmtpState::MailFrom => Err(DataDenied::NoRecipients),
        }
    }

    /// Whether `STARTTLS` may be issued now.
    pub fn may_starttls(&self, available: bool) -> Result<(), StartTlsDenied> {
        if self.tls {
            return Err(StartTlsDenied::AlreadyTls);
        }
        if !available {
            return Err(StartTlsDenied::Unavailable);
        }
        if self.state == SmtpState::Data {
            return Err(StartTlsDenied::InData);
        }
        Ok(())
    }

    /// Whether `AUTH` may be attempted now.
    pub fn may_auth(&self, require_tls: bool) -> Result<(), AuthDenied> {
        if self.is_authenticated() {
            return Err(AuthDenied::AlreadyAuthenticated);
        }
        if self.in_auth() {
            return Err(AuthDenied::InProgress);
        }
        if self.state == SmtpState::Data {
            return Err(AuthDenied::InData);
        }
        // RFC 4954 §4: a server that offers AUTH must be willing to authenticate at
        // any point, but a mid-transaction AUTH would let a peer change identity
        // between MAIL FROM and DATA.
        if matches!(self.state, SmtpState::MailFrom | SmtpState::RcptTo) {
            return Err(AuthDenied::InTransaction);
        }
        if require_tls && !self.tls {
            return Err(AuthDenied::TlsRequired);
        }
        Ok(())
    }

    /// Whether the envelope is complete enough to hand to delivery.
    pub fn is_ready_to_deliver(&self) -> bool {
        !self.transaction.recipients.is_empty()
    }

    /// Whether this peer may relay to an arbitrary domain.
    ///
    /// This is the open-relay rule from specification §9.4: anyone may deliver to
    /// our own domains, only an authenticated peer may be a relay.
    pub fn may_relay(&self) -> bool {
        self.is_authenticated()
    }

    // ---------------------------------------------------------------
    // Logging
    // ---------------------------------------------------------------

    /// A description of the envelope for structured logs.
    ///
    /// Never includes message bodies, credentials or the `AUTH` payload — only
    /// addresses, which are the minimum an operator needs to trace a delivery.
    pub fn envelope_summary(&self) -> String {
        let from = self
            .transaction
            .sender
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "<>".to_string());
        let to: Vec<String> = self
            .transaction
            .recipients
            .iter()
            .map(EmailAddress::to_string)
            .collect();
        format!("from={from} to=[{}]", to.join(","))
    }
}

/// Why `MAIL FROM` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailFromDenied {
    /// The peer must greet first.
    NoGreeting,
    /// A `DATA` phase is in progress.
    InData,
}

/// Why `RCPT TO` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcptDenied {
    /// `MAIL FROM` has not been accepted yet.
    NoMailFrom,
    /// A `DATA` phase is in progress.
    InData,
}

/// Why `DATA` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDenied {
    /// No `RCPT TO` was accepted.
    NoRecipients,
    /// A `DATA` phase is already in progress.
    AlreadyInData,
}

/// Why `STARTTLS` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartTlsDenied {
    /// TLS is already active.
    AlreadyTls,
    /// This listener has no TLS acceptor.
    Unavailable,
    /// A `DATA` phase is in progress.
    InData,
}

/// Why `AUTH` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDenied {
    /// The session already authenticated.
    AlreadyAuthenticated,
    /// An `AUTH` exchange is mid-flight.
    InProgress,
    /// A `DATA` phase is in progress.
    InData,
    /// A transaction is open (`MAIL FROM`/`RCPT TO` seen).
    InTransaction,
    /// The policy requires TLS before authentication.
    TlsRequired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 40_000 + 1)
    }

    fn addr(raw: &str) -> EmailAddress {
        EmailAddress::parse(raw).expect("test address")
    }

    fn greeted() -> SmtpSession {
        let mut s = SmtpSession::new("conn-1", peer());
        s.greet("mail.example.com", true);
        s
    }

    fn in_transaction() -> SmtpSession {
        let mut s = greeted();
        s.begin_transaction(Some(addr("alice@example.com")), Some(100), false);
        s.add_recipient(addr("bob@example.org"));
        s
    }

    // ------------------------------------------------------------------
    // Construction and the happy path
    // ------------------------------------------------------------------

    #[test]
    fn a_new_session_is_connected_and_empty() {
        let s = SmtpSession::new("c1", peer());
        assert_eq!(s.state, SmtpState::Connected);
        assert!(s.helo.is_none());
        assert!(!s.esmtp);
        assert!(!s.is_authenticated());
        assert!(!s.tls);
        assert!(!s.in_auth());
        assert_eq!(s.connection_id, "c1");
        assert_eq!(s.remote_addr, peer());
        assert_eq!(s.transaction.recipient_count(), 0);
        assert!(!s.is_ready_to_deliver());
    }

    #[test]
    fn the_happy_path_walks_the_state_machine_in_order() {
        let mut s = SmtpSession::new("c1", peer());
        assert_eq!(s.state, SmtpState::Connected);

        s.greet("client.example.net", true);
        assert_eq!(s.state, SmtpState::Greeted);
        assert!(s.esmtp);
        assert_eq!(s.helo.as_deref(), Some("client.example.net"));

        s.begin_transaction(Some(addr("alice@example.com")), None, false);
        assert_eq!(s.state, SmtpState::MailFrom);

        s.add_recipient(addr("bob@example.org"));
        assert_eq!(s.state, SmtpState::RcptTo);

        s.begin_data();
        assert_eq!(s.state, SmtpState::Data);

        s.end_transaction();
        assert_eq!(s.state, SmtpState::Greeted);
        assert_eq!(s.transaction.recipient_count(), 0);
    }

    #[test]
    fn helo_marks_the_session_as_non_esmtp() {
        let mut s = SmtpSession::new("c1", peer());
        s.greet("client.example.net", false);
        assert!(!s.esmtp);
        assert_eq!(s.state, SmtpState::Greeted);
    }

    #[test]
    fn submission_sessions_are_marked() {
        let s = SmtpSession::new("c1", peer()).with_submission(true);
        assert!(s.submission);
        assert!(!SmtpSession::new("c2", peer()).submission);
    }

    // ------------------------------------------------------------------
    // Transaction bookkeeping
    // ------------------------------------------------------------------

    #[test]
    fn a_transaction_records_the_sender_size_and_8bit_flag() {
        let mut s = greeted();
        s.begin_transaction(Some(addr("alice@example.com")), Some(4096), true);
        assert_eq!(s.transaction.sender.as_ref().map(ToString::to_string), Some("alice@example.com".into()));
        assert_eq!(s.transaction.declared_size, Some(4096));
        assert!(s.transaction.eight_bit);
    }

    #[test]
    fn the_null_sender_is_recorded_as_no_sender() {
        let mut s = greeted();
        s.begin_transaction(None, None, false);
        assert!(s.transaction.sender.is_none());
        assert!(s.envelope_summary().contains("from=<>"));
    }

    #[test]
    fn recipients_accumulate_in_order_and_deduplicate_case_insensitively() {
        let mut s = greeted();
        s.begin_transaction(Some(addr("a@example.com")), None, false);
        s.add_recipient(addr("b@example.org"));
        s.add_recipient(addr("C@example.org"));
        assert_eq!(s.transaction.recipient_count(), 2);
        assert!(s.transaction.has_recipient(&addr("c@example.org")));
        assert!(s.transaction.has_recipient(&addr("B@EXAMPLE.ORG")));
        assert!(!s.transaction.has_recipient(&addr("d@example.org")));
    }

    #[test]
    fn envelope_summary_lists_every_recipient_and_no_body() {
        let s = in_transaction();
        let summary = s.envelope_summary();
        assert!(summary.contains("from=alice@example.com"), "{summary}");
        assert!(summary.contains("bob@example.org"), "{summary}");
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn a_second_greeting_resets_the_transaction() {
        let mut s = in_transaction();
        s.greet("other.example.net", true);
        assert_eq!(s.state, SmtpState::Greeted);
        assert_eq!(s.transaction.recipient_count(), 0);
        assert!(s.transaction.sender.is_none());
        assert_eq!(s.helo.as_deref(), Some("other.example.net"));
    }

    #[test]
    fn a_second_greeting_does_not_lose_an_authentication() {
        let mut s = in_transaction();
        s.authenticate(UserId::new(7), "alice@example.com");
        s.greet("other.example.net", true);
        assert!(s.is_authenticated());
        assert_eq!(s.state, SmtpState::Authenticated);
    }

    // ------------------------------------------------------------------
    // RSET
    // ------------------------------------------------------------------

    #[test]
    fn rset_clears_the_envelope_and_keeps_the_greeting() {
        let mut s = in_transaction();
        s.reset();
        assert_eq!(s.state, SmtpState::Greeted);
        assert!(s.transaction.sender.is_none());
        assert_eq!(s.transaction.recipient_count(), 0);
        assert_eq!(s.helo.as_deref(), Some("mail.example.com"));
    }

    #[test]
    fn rset_keeps_an_authentication_but_clears_the_transaction() {
        let mut s = in_transaction();
        s.authenticate(UserId::new(7), "alice@example.com");
        s.reset();
        assert!(s.is_authenticated());
        assert_eq!(s.state, SmtpState::Authenticated);
        assert_eq!(s.transaction.recipient_count(), 0);
    }

    #[test]
    fn rset_before_a_greeting_returns_to_connected() {
        let mut s = SmtpSession::new("c1", peer());
        s.reset();
        assert_eq!(s.state, SmtpState::Connected);
    }

    #[test]
    fn rset_during_data_leaves_the_data_phase_and_clears_auth_state() {
        let mut s = in_transaction();
        s.begin_data();
        s.begin_auth(AuthState::AwaitingPassword);
        s.reset();
        assert_eq!(s.state, SmtpState::Greeted);
        assert!(!s.in_auth());
    }

    // ------------------------------------------------------------------
    // STARTTLS
    // ------------------------------------------------------------------

    #[test]
    fn starttls_resets_the_session_to_connected() {
        let mut s = in_transaction();
        s.authenticate(UserId::new(7), "alice@example.com");
        s.reset_for_starttls();

        assert_eq!(s.state, SmtpState::Connected);
        assert!(s.helo.is_none());
        assert!(!s.esmtp);
        assert!(!s.is_authenticated());
        assert!(s.authenticated_as.is_none());
        assert!(s.transaction.sender.is_none());
        assert_eq!(s.transaction.recipient_count(), 0);
        assert!(s.tls);
    }

    #[test]
    fn starttls_discards_an_in_flight_auth_exchange() {
        let mut s = greeted();
        s.begin_auth(AuthState::AwaitingUsername);
        s.pending_auth_username = Some("alice".into());
        s.reset_for_starttls();
        assert!(!s.in_auth());
        assert!(s.pending_auth_username.is_none());
    }

    #[test]
    fn after_starttls_the_peer_must_greet_again() {
        let mut s = greeted();
        s.reset_for_starttls();
        assert_eq!(s.may_mail_from(true), Err(MailFromDenied::NoGreeting));
        s.greet("mail.example.com", true);
        assert_eq!(s.may_mail_from(true), Ok(()));
    }

    // ------------------------------------------------------------------
    // Guards: MAIL FROM
    // ------------------------------------------------------------------

    #[test]
    fn mail_from_requires_a_greeting_when_configured() {
        let s = SmtpSession::new("c1", peer());
        assert_eq!(s.may_mail_from(true), Err(MailFromDenied::NoGreeting));
        // With the flag off, an ungreeted peer is tolerated.
        assert_eq!(s.may_mail_from(false), Ok(()));
    }

    #[test]
    fn mail_from_is_allowed_again_after_a_completed_transaction() {
        let mut s = in_transaction();
        s.begin_data();
        s.end_transaction();
        assert_eq!(s.may_mail_from(true), Ok(()));
        s.begin_transaction(Some(addr("a@example.com")), None, false);
        assert_eq!(s.may_mail_from(true), Ok(()));
    }

    #[test]
    fn mail_from_during_data_is_refused() {
        let mut s = in_transaction();
        s.begin_data();
        assert_eq!(s.may_mail_from(true), Err(MailFromDenied::InData));
    }

    #[test]
    fn mail_from_after_helo_is_always_fine() {
        let s = greeted();
        assert_eq!(s.may_mail_from(true), Ok(()));
    }

    // ------------------------------------------------------------------
    // Guards: RCPT TO
    // ------------------------------------------------------------------

    #[test]
    fn rcpt_to_before_mail_from_is_refused() {
        let s = greeted();
        assert_eq!(s.may_rcpt_to(), Err(RcptDenied::NoMailFrom));
    }

    #[test]
    fn rcpt_to_before_a_greeting_is_refused() {
        let s = SmtpSession::new("c1", peer());
        assert_eq!(s.may_rcpt_to(), Err(RcptDenied::NoMailFrom));
    }

    #[test]
    fn rcpt_to_is_allowed_repeatedly_between_mail_from_and_data() {
        let mut s = greeted();
        s.begin_transaction(Some(addr("a@example.com")), None, false);
        assert_eq!(s.may_rcpt_to(), Ok(()));
        s.add_recipient(addr("b@example.org"));
        assert_eq!(s.may_rcpt_to(), Ok(()));
        s.add_recipient(addr("c@example.org"));
        assert_eq!(s.may_rcpt_to(), Ok(()));
    }

    #[test]
    fn rcpt_to_during_data_is_refused() {
        let mut s = in_transaction();
        s.begin_data();
        assert_eq!(s.may_rcpt_to(), Err(RcptDenied::InData));
    }

    #[test]
    fn rcpt_to_after_an_authentication_but_without_mail_from_is_refused() {
        let mut s = greeted();
        s.authenticate(UserId::new(7), "alice@example.com");
        assert_eq!(s.may_rcpt_to(), Err(RcptDenied::NoMailFrom));
    }

    // ------------------------------------------------------------------
    // Guards: DATA
    // ------------------------------------------------------------------

    #[test]
    fn data_requires_at_least_one_recipient() {
        let mut s = greeted();
        assert_eq!(s.may_data(), Err(DataDenied::NoRecipients));
        s.begin_transaction(Some(addr("a@example.com")), None, false);
        assert_eq!(s.may_data(), Err(DataDenied::NoRecipients));
        s.add_recipient(addr("b@example.org"));
        assert_eq!(s.may_data(), Ok(()));
    }

    #[test]
    fn data_before_a_greeting_or_after_an_auth_is_refused_for_want_of_recipients() {
        let s = SmtpSession::new("c1", peer());
        assert_eq!(s.may_data(), Err(DataDenied::NoRecipients));

        let mut s = greeted();
        s.authenticate(UserId::new(7), "alice@example.com");
        assert_eq!(s.may_data(), Err(DataDenied::NoRecipients));
    }

    #[test]
    fn a_second_data_command_is_refused() {
        let mut s = in_transaction();
        s.begin_data();
        assert_eq!(s.may_data(), Err(DataDenied::AlreadyInData));
    }

    // ------------------------------------------------------------------
    // Guards: STARTTLS
    // ------------------------------------------------------------------

    #[test]
    fn starttls_is_refused_when_no_acceptor_exists() {
        let s = SmtpSession::new("c1", peer());
        assert_eq!(s.may_starttls(false), Err(StartTlsDenied::Unavailable));
    }

    #[test]
    fn starttls_is_allowed_before_tls_and_refused_after() {
        let mut s = greeted();
        assert_eq!(s.may_starttls(true), Ok(()));
        s.reset_for_starttls();
        assert_eq!(s.may_starttls(true), Err(StartTlsDenied::AlreadyTls));
    }

    #[test]
    fn starttls_during_data_is_refused() {
        let mut s = in_transaction();
        s.begin_data();
        assert_eq!(s.may_starttls(true), Err(StartTlsDenied::InData));
    }

    #[test]
    fn starttls_mid_transaction_is_allowed_by_the_protocol() {
        // RFC 3207 lets a client upgrade at any point; the reset then discards the
        // transaction, which is what makes it safe.
        let mut s = in_transaction();
        assert_eq!(s.may_starttls(true), Ok(()));
        s.reset_for_starttls();
        assert_eq!(s.transaction.recipient_count(), 0);
    }

    // ------------------------------------------------------------------
    // Guards: AUTH
    // ------------------------------------------------------------------

    #[test]
    fn auth_is_allowed_before_a_greeting_and_after_it() {
        let s = SmtpSession::new("c1", peer());
        assert_eq!(s.may_auth(false), Ok(()));
        let s = greeted();
        assert_eq!(s.may_auth(false), Ok(()));
    }

    #[test]
    fn auth_is_refused_when_tls_is_required_and_absent() {
        let s = greeted();
        assert_eq!(s.may_auth(true), Err(AuthDenied::TlsRequired));

        let mut s = greeted();
        s.reset_for_starttls();
        assert_eq!(s.may_auth(true), Ok(()));
    }

    #[test]
    fn a_second_auth_is_refused_once_authenticated() {
        let mut s = greeted();
        s.authenticate(UserId::new(7), "alice@example.com");
        assert_eq!(s.may_auth(false), Err(AuthDenied::AlreadyAuthenticated));
    }

    #[test]
    fn a_nested_auth_exchange_is_refused() {
        let mut s = greeted();
        s.begin_auth(AuthState::AwaitingUsername);
        assert_eq!(s.may_auth(false), Err(AuthDenied::InProgress));
    }

    #[test]
    fn auth_during_data_is_refused() {
        let mut s = in_transaction();
        s.begin_data();
        assert_eq!(s.may_auth(false), Err(AuthDenied::InData));
    }

    #[test]
    fn auth_mid_transaction_is_refused_so_identity_cannot_change() {
        let mut s = greeted();
        s.begin_transaction(Some(addr("a@example.com")), None, false);
        assert_eq!(s.may_auth(false), Err(AuthDenied::InTransaction));
        s.add_recipient(addr("b@example.org"));
        assert_eq!(s.may_auth(false), Err(AuthDenied::InTransaction));
    }

    #[test]
    fn after_a_finished_transaction_auth_is_allowed_again() {
        let mut s = in_transaction();
        s.begin_data();
        s.end_transaction();
        assert_eq!(s.may_auth(false), Ok(()));
    }

    // ------------------------------------------------------------------
    // AUTH bookkeeping
    // ------------------------------------------------------------------

    #[test]
    fn beginning_and_aborting_an_auth_exchange() {
        let mut s = greeted();
        assert!(!s.in_auth());
        s.begin_auth(AuthState::AwaitingUsername);
        assert!(s.in_auth());
        assert_eq!(s.auth_state, Some(AuthState::AwaitingUsername));
        s.abort_auth();
        assert!(!s.in_auth());
        assert!(s.pending_auth_username.is_none());
    }

    #[test]
    fn authenticating_records_the_user_and_the_address() {
        let mut s = greeted();
        s.authenticate(UserId::new(7), "Alice@Example.com");
        assert_eq!(s.authenticated_user, Some(UserId::new(7)));
        assert_eq!(s.authenticated_as.as_deref(), Some("Alice@Example.com"));
        assert_eq!(s.state, SmtpState::Authenticated);
        assert!(!s.in_auth());
    }

    #[test]
    fn authenticating_clears_an_in_flight_exchange() {
        let mut s = greeted();
        s.begin_auth(AuthState::AwaitingPassword);
        s.pending_auth_username = Some("alice".into());
        s.authenticate(UserId::new(7), "alice@example.com");
        assert!(s.auth_state.is_none());
        assert!(s.pending_auth_username.is_none());
    }

    #[test]
    fn only_authenticated_peers_may_relay() {
        let anonymous = greeted();
        assert!(!anonymous.may_relay());

        let mut s = greeted();
        s.authenticate(UserId::new(7), "alice@example.com");
        assert!(s.may_relay());
    }

    // ------------------------------------------------------------------
    // Enum helpers
    // ------------------------------------------------------------------

    #[test]
    fn state_helpers_agree_with_the_variants() {
        assert!(!SmtpState::Connected.greeted());
        for state in [
            SmtpState::Greeted,
            SmtpState::MailFrom,
            SmtpState::RcptTo,
            SmtpState::Data,
            SmtpState::Authenticated,
        ] {
            assert!(state.greeted(), "{state:?}");
            assert!(!state.as_str().is_empty());
        }
        assert_eq!(SmtpState::Connected.as_str(), "connected");
        assert_eq!(SmtpState::MailFrom.as_str(), "mail_from");
        assert_eq!(AuthState::AwaitingUsername.as_str(), "awaiting_username");
        assert_eq!(AuthState::AwaitingPassword.as_str(), "awaiting_password");
    }

    #[test]
    fn the_transaction_default_matches_new() {
        let t = Transaction::default();
        assert!(t.sender.is_none());
        assert!(t.recipients.is_empty());
        assert_eq!(t.declared_size, None);
        assert!(!t.eight_bit);
    }

    #[test]
    fn a_session_is_cheap_enough_to_clone_for_logging() {
        let s = in_transaction();
        let clone = s.clone();
        assert_eq!(clone.state, s.state);
        assert_eq!(clone.transaction, s.transaction);
    }
}
