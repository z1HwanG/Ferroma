//! The IMAP session state machine (RFC 3501 §3).
//!
//! ```text
//!          +----------------------+
//!          |  NotAuthenticated    |
//!          +----------+-----------+
//!             LOGIN / AUTHENTICATE
//!                     v
//!          +----------------------+     CLOSE / UNSELECT
//!          |    Authenticated     |<----------------+
//!                     v                            |
//!          +----------------------+                |
//!          |       Selected       |----------------+
//!          +----------+-----------+
//!                   LOGOUT
//!                     v
//!                 Logout
//! ```
//!
//! Every command is checked against the current state *first*: `FETCH` in
//! `Authenticated` is a `BAD` (the client forgot to `SELECT`), `SELECT` in
//! `NotAuthenticated` is a `BAD` too, and no command may ever take the session to
//! a state the RFC does not allow.
//!
//! The module is deliberately I/O-minimal: it reads through [`SessionInput`] and
//! writes through [`SessionOutput`], which is what lets the whole state machine
//! be unit-tested without a socket while [`crate::server`] connects those to a
//! TCP or TLS stream.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferroma_core::{FerromaError, MailboxId, MessageId, UserId};
use ferroma_events::{Event, EventBus, EventScope, SubscriptionError};
use ferroma_mail::{Flags, ParsedMessage};
use ferroma_storage::models::{Folder, Message};
use ferroma_storage::repository::NewMessage;
use ferroma_storage::{Maildir, Repositories};

use crate::config::ImapServerConfig;
use crate::fetch::{self, Envelope, MessageMeta};
use crate::mailbox as mbox;
use crate::parser::{
    Command, CommandParser, FetchSpec, LiteralError, LiteralSource, StoreAction,
};
use crate::rawmime::RawMessage;
use crate::response::{Response, ResponseCode, StatusItem, StatusItems};
use crate::search::{self, MessageFacts, SearchKey};
use crate::sequence::SequenceSet;

/// Cap on how many body bytes a `SEARCH BODY`/`TEXT` will read from the Maildir.
///
/// A search must never let one enormous message make the server allocate
/// unbounded memory; beyond the cap the message is searched for its header
/// fields only.
pub const SEARCH_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// How the session loop should continue after one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFlow {
    /// Keep reading commands.
    Continue,
    /// The client (or the server) ended the session.
    Close,
    /// The client asked for `STARTTLS`; the server must upgrade the stream.
    UpgradeTls,
}

/// The session's state, per RFC 3501 §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Before `LOGIN`/`AUTHENTICATE`.
    NotAuthenticated,
    /// Logged in, no mailbox selected.
    Authenticated,
    /// A mailbox is open.
    Selected,
    /// `LOGOUT` was received.
    Logout,
}

impl SessionState {
    /// The wire name, for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::NotAuthenticated => "not-authenticated",
            SessionState::Authenticated => "authenticated",
            SessionState::Selected => "selected",
            SessionState::Logout => "logout",
        }
    }
}

/// Somewhere for the session to write its responses.
pub trait SessionOutput {
    /// Write one response, including its CRLF.
    fn send(&mut self, response: &Response);

    /// Write an already-rendered wire line.
    fn send_raw(&mut self, bytes: &[u8]);

    /// Push everything written so far to the peer.
    ///
    /// The session flushes after every command — and after every `IDLE` push —
    /// so an implementation that buffers must actually write here. Holding the
    /// bytes until the connection ends would leave a client waiting for a reply
    /// it has already been promised.
    ///
    /// The default is a no-op, which is right for an in-memory sink.
    fn flush(&mut self) -> impl std::future::Future<Output = Result<(), FerromaError>> + Send {
        async { Ok(()) }
    }
}

/// Somewhere for the session to read lines from.
pub trait SessionInput {
    /// The next CRLF-terminated line, without the CRLF.
    ///
    /// `None` means the peer closed the connection.
    fn read_line(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, FerromaError>> + Send;

    /// Stop reading.
    ///
    /// A `STARTTLS` upgrade has to take the socket back from its reader before
    /// the TLS layer can have it. The default is a no-op, which is right for
    /// every in-memory reader.
    fn shutdown(&mut self) -> impl std::future::Future<Output = Result<(), FerromaError>> + Send {
        async { Ok(()) }
    }
}

/// A [`SessionOutput`] that appends to a buffer — the workhorse of the unit
/// tests, and also a way to build a whole command's responses before flushing.
#[derive(Debug, Default, Clone)]
pub struct VecOutput {
    /// Everything written so far, as wire bytes.
    pub bytes: Vec<u8>,
}

impl VecOutput {
    /// A fresh, empty sink.
    pub fn new() -> Self {
        VecOutput::default()
    }

    /// The responses written so far, split into lines.
    pub fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.bytes)
            .split("\r\n")
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Everything written so far as text with `\n` separators, for assertions.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).replace("\r\n", "\n")
    }

    /// Forget everything written so far.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

impl SessionOutput for VecOutput {
    fn send(&mut self, response: &Response) {
        self.bytes.extend_from_slice(response.to_wire().as_bytes());
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
}

/// A [`SessionOutput`] that counts what passes through it.
///
/// Used so every command's structured log line can report how many untagged
/// responses it produced without threading a counter through every handler.
pub struct CountingOutput<'a, O: SessionOutput + Send> {
    inner: &'a mut O,
    /// How many responses have been written.
    pub count: usize,
}

impl<'a, O: SessionOutput + Send> CountingOutput<'a, O> {
    /// Wrap `inner`.
    pub fn new(inner: &'a mut O) -> Self {
        CountingOutput { inner, count: 0 }
    }

    /// The wrapped sink.
    pub fn inner(&mut self) -> &mut O {
        self.inner
    }
}

impl<O: SessionOutput + Send> SessionOutput for CountingOutput<'_, O> {
    fn send(&mut self, response: &Response) {
        self.count += 1;
        self.inner.send(response);
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.count += 1;
        self.inner.send_raw(bytes);
    }

    async fn flush(&mut self) -> Result<(), FerromaError> {
        self.inner.flush().await
    }
}

/// A [`SessionInput`] that serves an in-memory script.
#[derive(Debug, Default, Clone)]
pub struct ScriptInput {
    /// Lines still to be read.
    pub lines: std::collections::VecDeque<Vec<u8>>,
}

impl ScriptInput {
    /// Build from lines, in order.
    pub fn new<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Vec<u8>>,
    {
        ScriptInput {
            lines: lines.into_iter().map(Into::into).collect(),
        }
    }
}

impl SessionInput for ScriptInput {
    async fn read_line(&mut self) -> Result<Option<Vec<u8>>, FerromaError> {
        Ok(self.lines.pop_front())
    }
}

/// Checks a user name and password.
///
/// Implemented by [`crate::auth::ServiceAuthenticator`] over `ferroma-auth`, and
/// by trivial maps in tests. Keeping it a trait is what lets the session be
/// unit-tested without a database.
pub trait Authenticator: Send + Sync {
    /// Verify credentials. `Ok(Some(user))` on success, `Ok(None)` when the
    /// credentials are wrong.
    #[allow(clippy::type_complexity)]
    fn authenticate<'a>(
        &'a self,
        user: &'a str,
        password: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<UserId>, FerromaError>> + Send + 'a>,
    >;
}

/// An in-memory `address -> password` authenticator.
#[derive(Debug, Clone, Default)]
pub struct MapAuthenticator {
    /// `(lower-cased address, password, user id)` triples.
    pub accounts: Vec<(String, String, i64)>,
}

impl MapAuthenticator {
    /// An authenticator with one account.
    pub fn single(address: &str, password: &str, user: i64) -> Self {
        MapAuthenticator {
            accounts: vec![(address.to_ascii_lowercase(), password.to_string(), user)],
        }
    }
}

impl Authenticator for MapAuthenticator {
    fn authenticate<'a>(
        &'a self,
        user: &'a str,
        password: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<UserId>, FerromaError>> + Send + 'a>,
    > {
        let user = user.trim().to_ascii_lowercase();
        let password = password.to_string();
        Box::pin(async move {
            Ok(self
                .accounts
                .iter()
                .find(|(address, secret, _)| *address == user && *secret == password)
                .map(|(_, _, id)| UserId::new(*id)))
        })
    }
}

/// Split `local@domain` into its two halves, lower-cased.
pub fn split_address(address: &str) -> Option<(String, String)> {
    let lowered = address.trim().to_ascii_lowercase();
    let (local, domain) = lowered.split_once('@')?;
    if local.is_empty() || domain.is_empty() {
        return None;
    }
    Some((local.to_string(), domain.to_string()))
}

/// One message in the selected mailbox.
#[derive(Debug, Clone)]
struct SelectedMessage {
    /// The database row id.
    id: MessageId,
    /// The IMAP UID.
    uid: i64,
    /// Path of the body file, relative to the Maildir root.
    storage_path: String,
    /// The stored flag string (`seen flagged`).
    flags: String,
    /// `INTERNALDATE`.
    internal_date: DateTime<Utc>,
    /// The RFC 5322 `Date:` header.
    sent_at: Option<DateTime<Utc>>,
    /// Size in octets.
    size: i64,
    /// Whether the message still sits in the Maildir `new/` directory, which is
    /// how `\Recent` is derived.
    recent: bool,
    /// Decoded header summary, filled in from the file when `SEARCH` needs it.
    headers: Option<MessageHeaderSummary>,
}

impl SelectedMessage {
    fn from_row(message: &Message) -> Self {
        SelectedMessage {
            id: message.message_id(),
            uid: message.uid,
            storage_path: message.storage_path.clone(),
            flags: message.flags.clone(),
            internal_date: message.internal_date,
            sent_at: message.sent_at,
            size: message.size_bytes,
            recent: is_recent(&message.storage_path),
            headers: None,
        }
    }

    /// The stored flags as a [`Flags`] value.
    fn flag_set(&self) -> Flags {
        parse_flags(&self.flags)
    }
}

/// The header-derived fields `SEARCH` and `ENVELOPE` need.
#[derive(Debug, Clone, Default)]
struct MessageHeaderSummary {
    from: String,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    header_block: String,
}

/// The mailbox a session has selected.
#[derive(Debug, Clone)]
struct Selected {
    /// The folder row.
    folder: Folder,
    /// The owning address (denormalised onto the messages).
    mailbox_id: MailboxId,
    /// The messages, in UID (and therefore sequence) order.
    messages: Vec<SelectedMessage>,
    /// `true` for `EXAMINE`: every mutating command must fail.
    read_only: bool,
}

/// Configuration for one session, derived from [`ImapServerConfig`].
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The greeting banner.
    pub banner: String,
    /// Whether the connection is already encrypted.
    pub tls: bool,
    /// Refuse `LOGIN`/`AUTHENTICATE` on an unencrypted connection.
    pub require_tls_for_login: bool,
    /// Advertise `IDLE`.
    pub enable_idle: bool,
    /// Advertise `MOVE`.
    pub enable_move: bool,
    /// Largest `APPEND` accepted, in bytes.
    pub max_append_size: u64,
    /// Longest literal the parser accepts.
    pub max_literal_size: u64,
    /// Seconds an `IDLE` may last before the server says `BYE`.
    pub max_idle_secs: u64,
    /// Whether `STARTTLS` may be issued on this connection.
    pub starttls_available: bool,
}

impl SessionConfig {
    /// Derive the session settings from the server configuration.
    pub fn from_server(config: &ImapServerConfig, tls: bool, starttls_available: bool) -> Self {
        SessionConfig {
            banner: config.banner.clone(),
            tls,
            require_tls_for_login: config.require_tls_for_login,
            enable_idle: config.enable_idle,
            enable_move: config.enable_move,
            max_append_size: config.max_append_size,
            max_literal_size: config.max_append_size.max(crate::parser::DEFAULT_MAX_LITERAL),
            max_idle_secs: config.max_idle_secs,
            starttls_available,
        }
    }

    /// The capability list for this session's connection.
    pub fn capabilities(&self) -> Vec<String> {
        let mut caps = vec!["IMAP4rev1".to_string()];
        if self.starttls_available && !self.tls {
            caps.push("STARTTLS".to_string());
        }
        if !self.tls && self.require_tls_for_login {
            caps.push("LOGINDISABLED".to_string());
        }
        caps.push("AUTH=PLAIN".to_string());
        if self.enable_idle {
            caps.push("IDLE".to_string());
        }
        caps.push("UIDPLUS".to_string());
        if self.enable_move {
            caps.push("MOVE".to_string());
        }
        caps.push("UNSELECT".to_string());
        caps.push("NAMESPACE".to_string());
        caps.push("LITERAL+".to_string());
        caps.push("CHILDREN".to_string());
        caps
    }
}

/// Everything a session needs that outlives it.
#[derive(Clone)]
pub struct SessionContext {
    /// Server configuration.
    pub config: Arc<ImapServerConfig>,
    /// Database repositories.
    pub repos: Arc<Repositories>,
    /// The Maildir holding the message bodies.
    pub maildir: Arc<Maildir>,
    /// Credential verification.
    pub authenticator: Arc<dyn Authenticator>,
    /// Where mailbox changes are published and `IDLE` listens.
    pub events: Option<Arc<EventBus>>,
    /// This connection's id, for structured logs.
    pub connection_id: u64,
    /// The peer address, for structured logs.
    pub remote_ip: String,
}

impl std::fmt::Debug for SessionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContext")
            .field("connection_id", &self.connection_id)
            .field("remote_ip", &self.remote_ip)
            .finish_non_exhaustive()
    }
}

impl SessionContext {
    /// Build a context from its parts.
    pub fn new(
        config: Arc<ImapServerConfig>,
        repos: Arc<Repositories>,
        maildir: Arc<Maildir>,
        authenticator: Arc<dyn Authenticator>,
    ) -> Self {
        SessionContext {
            config,
            repos,
            maildir,
            authenticator,
            events: None,
            connection_id: 0,
            remote_ip: String::new(),
        }
    }

    /// Attach an event bus, enabling `IDLE` push.
    pub fn with_events(mut self, events: Arc<EventBus>) -> Self {
        self.events = Some(events);
        self
    }

    /// Set the log identity.
    pub fn with_identity(mut self, connection_id: u64, remote_ip: impl Into<String>) -> Self {
        self.connection_id = connection_id;
        self.remote_ip = remote_ip.into();
        self
    }
}

/// One IMAP session.
pub struct ImapSession {
    /// Shared, outliving state.
    context: SessionContext,
    /// Parser state.
    parser: CommandParser,
    /// Session settings.
    config: SessionConfig,
    /// Current state.
    state: SessionState,
    /// The authenticated account.
    user: Option<UserId>,
    /// The account's primary address, `local@domain`.
    address: Option<String>,
    /// The address's local part (a Maildir path component).
    local_part: Option<String>,
    /// The address's domain (a Maildir path component).
    domain: Option<String>,
    /// The selected mailbox.

    selected: Option<Selected>,
    /// The event subscription an `IDLE` is listening on.
    ///
    /// It is created when `IDLE` is *accepted*, not when the wait begins: an
    /// event published between the `+ idling` continuation and the first wait
    /// would otherwise be lost, and the client would never be told.
    idle: Option<ferroma_events::Subscription>,
    /// Lines left over from a literal's tail.
    ///
    /// A literal may sit in the middle of an argument list and its octets are
    /// not line-terminated, so whatever the client wrote after them belongs to
    /// the same command line. That remainder is queued here and read before the
    /// socket does.
    pending: std::collections::VecDeque<Vec<u8>>,
}


impl std::fmt::Debug for ImapSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapSession")
            .field("state", &self.state)
            .field("user", &self.user)
            .field("connection_id", &self.context.connection_id)
            .finish_non_exhaustive()
    }
}

impl ImapSession {
    /// A new session.
    pub fn new(context: SessionContext, tls: bool, starttls_available: bool) -> Self {
        let config = SessionConfig::from_server(&context.config, tls, starttls_available);
        let parser = CommandParser::with_max_literal(config.max_literal_size);
        ImapSession {
            context,
            parser,
            config,
            state: SessionState::NotAuthenticated,
            user: None,
            address: None,
            local_part: None,
            domain: None,
            selected: None,
            idle: None,
            pending: std::collections::VecDeque::new(),
        }
    }

    /// The session's current state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// The authenticated account, once there is one.
    pub fn user(&self) -> Option<UserId> {
        self.user
    }

    /// The session's settings.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The selected folder, once there is one.
    pub fn selected_folder(&self) -> Option<&Folder> {
        self.selected.as_ref().map(|selected| &selected.folder)
    }

    /// The session's capabilities.
    pub fn capabilities(&self) -> Vec<String> {
        self.config.capabilities()
    }

    /// Record that the connection is now encrypted (after `STARTTLS`).
    ///
    /// RFC 2595 requires the session to return to `NotAuthenticated` and drop
    /// the selected mailbox: everything the client sent before the handshake is
    /// untrusted, including its identity.
    pub fn upgrade_to_tls(&mut self) {
        self.config.tls = true;
        self.config.starttls_available = false;
        self.state = SessionState::NotAuthenticated;
        self.selected = None;
        self.user = None;
        self.address = None;
    }

    /// Send the greeting: `* OK [CAPABILITY …] <banner>`.
    pub fn greet<O: SessionOutput + Send>(&self, out: &mut O) {
        out.send(&Response::untagged_ok(
            self.config.banner.clone(),
            Some(ResponseCode::Capability(self.capabilities())),
        ));
    }

    /// Tell the client the server is going away.
    pub fn shutdown<O: SessionOutput + Send>(&self, out: &mut O) {
        out.send(&Response::bye_alert("server shutting down"));
    }

    /// Run the read/dispatch loop until the client leaves.
    pub async fn run<I, O>(&mut self, input: &mut I, out: &mut O) -> Result<SessionFlow, FerromaError>
    where
        I: SessionInput + Send,
        O: SessionOutput + Send,
    {
        loop {
            // A literal's tail is a command line in its own right, and it must
            // be consumed before the socket is read again — otherwise the
            // client's next command would be taken as a fresh line while the
            // tail was still pending.
            let line = match self.pending.pop_front() {
                Some(line) => line,
                None => match input.read_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        self.state = SessionState::Logout;
                        return Ok(SessionFlow::Close);
                    }
                    Err(err) => return Err(err),
                },
            };

            let started = std::time::Instant::now();

            let flow = self.handle_line(line, input, out).await?;
            // The reply must reach the client before the next read — and before
            // the connection closes, which is what makes `LOGOUT` deliver its
            // `* BYE` and its tagged `OK`. A client that waits for the reply
            // would otherwise deadlock on a buffered one.
            out.flush().await?;
            match flow {
                SessionFlow::Continue => {
                    // `IDLE` is the one command that keeps the read loop: the
                    // client is waiting for pushes rather than sending anything,
                    // so the server drives the wait — and the timeout — here.
                    if self.parser.is_idling() {
                        let flow = self.serve_idle(input, out, started).await?;
                        // The `DONE` completion — or the `* BYE` of an over-long
                        // idle — has to reach the client too.
                        out.flush().await?;
                        match flow {
                            SessionFlow::Continue => {}
                            other => return Ok(other),
                        }
                    }
                }
                other => return Ok(other),
            }
        }
    }

    /// Handle one raw command line (already stripped of its CRLF).
    pub async fn handle_line<I, O>(
        &mut self,
        line: Vec<u8>,
        input: &mut I,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError>
    where
        I: SessionInput + Send,
        O: SessionOutput + Send,
    {
        let started = std::time::Instant::now();
        let text = String::from_utf8_lossy(&line).into_owned();
        if text.trim().is_empty() {
            return Ok(SessionFlow::Continue);
        }


        // Literals are the one place the parser has to come back to the socket:
        // it asks for `n` octets, this reader writes the `+` continuation and
        // reads them.
        let parsed = {
            let mut literals = SocketLiterals {
                input,
                out,
                pending: &mut self.pending,
            };
            self.parser.parse(&text, &mut literals).await
        };
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(err) => {
                let tag = if self.parser.tag().is_empty() {
                    "*".to_string()
                } else {
                    self.parser.tag().to_string()
                };
                out.send(&Response::tagged_bad(&tag, describe_error(&err), None));
                self.log_command("(parse)", started, "BAD");
                return Ok(SessionFlow::Continue);
            }
        };

        let mut counter = CountingOutput::new(out);
        let flow = match self.dispatch(&parsed.tag, &parsed.command, &mut counter).await {
            Ok(flow) => flow,
            Err(err) => {
                // A failure inside one command must never kill the connection:
                // the client gets a NO/BAD and carries on.
                let (response, result) = error_response(&parsed.tag, &err);
                counter.send(&response);
                self.log_command(parsed.command.name(), started, result);
                return Ok(SessionFlow::Continue);
            }
        };
        let count = counter.count;
        self.log_command_with(parsed.command.name(), started, "OK", count);
        Ok(flow)
    }

    /// Log one command with its structured fields.
    fn log_command(&self, command: &str, started: std::time::Instant, result: &'static str) {
        self.log_command_with(command, started, result, 0);
    }

    fn log_command_with(
        &self,
        command: &str,
        started: std::time::Instant,
        result: &'static str,
        responses: usize,
    ) {
        tracing::info!(
            connection_id = self.context.connection_id,
            remote_ip = %self.context.remote_ip,
            user = self.address.as_deref().unwrap_or("-"),
            command = command,
            duration_ms = started.elapsed().as_millis() as u64,
            result = result,
            responses = responses,
            "imap command"
        );
    }

    // -----------------------------------------------------------------------
    // Dispatch and state checks
    // -----------------------------------------------------------------------

    /// Check the state machine, then run the command.
    async fn dispatch<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        command: &Command,
        out: &mut CountingOutput<'_, O>,
    ) -> Result<SessionFlow, FerromaError> {
        let name = command.name();
        let inner = command.inner();

        // `CAPABILITY`, `NOOP` and `LOGOUT` are legal in every state.
        match inner {
            Command::Capability => {
                out.send(&Response::capability(&self.capabilities()));
                out.send(&Response::tagged_ok(tag, "CAPABILITY completed", None));
                return Ok(SessionFlow::Continue);
            }
            Command::Noop => {
                out.send(&Response::tagged_ok(tag, "NOOP completed", None));
                return Ok(SessionFlow::Continue);
            }
            Command::Logout => {
                out.send(&Response::bye("LOGOUT", None));
                out.send(&Response::tagged_ok(tag, "LOGOUT completed", None));
                self.state = SessionState::Logout;
                self.selected = None;
                return Ok(SessionFlow::Close);
            }
            _ => {}
        }

        if self.state == SessionState::Logout {
            return Err(FerromaError::Protocol("session is logging out".into()));
        }

        match inner {
            Command::StartTls => self.command_starttls(tag, out),
            Command::Login { user, password } => self.command_login(tag, user, password, out).await,
            Command::Authenticate {
                mechanism,
                initial_response,
            } => {
                self.command_authenticate(tag, mechanism, initial_response.clone(), out)
                    .await
            }
            Command::Idle => self.command_idle(tag, out),
            _ if self.state == SessionState::NotAuthenticated => Err(FerromaError::Protocol(
                format!("{name} is not allowed before LOGIN"),
            )),
            Command::Select(name) => self.open_mailbox(tag, name, false, out).await,
            Command::Examine(name) => self.open_mailbox(tag, name, true, out).await,
            Command::Create(name) => self.command_create(tag, name, out).await,
            Command::Delete(name) => self.command_delete(tag, name, out).await,
            Command::Rename { from, to } => self.command_rename(tag, from, to, out).await,
            Command::Subscribe(name) => self.command_subscribe(tag, name, true, out).await,
            Command::Unsubscribe(name) => self.command_subscribe(tag, name, false, out).await,
            Command::List { reference, pattern } => {
                self.command_list(tag, reference, pattern, false, out).await
            }
            Command::Lsub { reference, pattern } => {
                self.command_list(tag, reference, pattern, true, out).await
            }
            Command::Status { mailbox, items } => self.command_status(tag, mailbox, items, out).await,
            Command::Append {
                mailbox,
                flags,
                internal_date,
                message,
            } => {
                self.command_append(
                    tag,
                    mailbox,
                    flags,
                    *internal_date,
                    message.clone(),
                    out,
                )
                .await
            }
            Command::Namespace => {
                out.send(&Response::namespace(&[("", mbox::DELIMITER)], &[], &[]));
                out.send(&Response::tagged_ok(tag, "NAMESPACE completed", None));
                Ok(SessionFlow::Continue)
            }
            Command::Id => {
                out.send(&Response::untagged("ID NIL"));
                out.send(&Response::tagged_ok(tag, "ID completed", None));
                Ok(SessionFlow::Continue)
            }
            Command::Enable(_) => {
                out.send(&Response::tagged_ok(tag, "ENABLE completed", None));
                Ok(SessionFlow::Continue)
            }
            Command::Unselect => {
                self.require_selected(name)?;
                self.selected = None;
                self.state = SessionState::Authenticated;
                out.send(&Response::tagged_ok(tag, "UNSELECT completed", None));
                Ok(SessionFlow::Continue)
            }
            Command::Check => {
                self.require_selected(name)?;
                out.send(&Response::tagged_ok(tag, "CHECK completed", None));
                Ok(SessionFlow::Continue)
            }
            Command::Close => self.command_close(tag, out).await,
            Command::Expunge => self.command_expunge(tag, None, out).await,
            Command::UidExpunge(set) => self.command_expunge(tag, Some(set), out).await,
            Command::Search { charset, key } => {
                self.command_search(tag, charset, key, command.is_uid(), out)
                    .await
            }
            Command::Fetch { set, spec } => {
                self.command_fetch(tag, set, spec, command.is_uid(), out).await
            }
            Command::Store {
                set,
                action,
                silent,
                flags,
            } => {
                self.command_store(tag, set, *action, *silent, flags, command.is_uid(), out)
                    .await
            }
            Command::Copy { set, mailbox } => {
                self.command_copy(tag, set, mailbox, false, command.is_uid(), out)
                    .await
            }
            Command::Move { set, mailbox } => {
                if !self.config.enable_move {
                    return Err(FerromaError::Unsupported(
                        "MOVE is not enabled on this server".into(),
                    ));
                }
                self.command_copy(tag, set, mailbox, true, command.is_uid(), out)
                    .await
            }
            Command::Noop | Command::Logout | Command::Capability | Command::Uid(_) => {
                // `NOOP`, `LOGOUT` and `CAPABILITY` were answered at the top of
                // this function; `LOGIN`, `AUTHENTICATE` and `IDLE` have their
                // own arms above; and `UID` was unwrapped by `Command::inner`.
                // Every remaining variant is named, so a new command cannot be
                // silently forgotten here.
                unreachable!("handled above")
            }
        }
    }

    /// The selected mailbox, or a `BAD`-shaped error.
    fn require_selected(&self, command: &str) -> Result<&Selected, FerromaError> {
        self.selected.as_ref().ok_or_else(|| {
            FerromaError::Protocol(format!("{command} needs a selected mailbox"))
        })
    }

    /// The selected mailbox, mutable, refusing writes in `EXAMINE`.
    fn require_writable(&mut self, command: &str) -> Result<&mut Selected, FerromaError> {
        let selected = self.selected.as_mut().ok_or_else(|| {
            FerromaError::Protocol(format!("{command} needs a selected mailbox"))
        })?;
        if selected.read_only {
            return Err(FerromaError::Forbidden(format!(
                "{command} is not allowed on a read-only mailbox"
            )));
        }
        Ok(selected)
    }

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------

    fn command_starttls<O: SessionOutput + Send>(
        &self,
        tag: &str,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        if self.config.tls {
            out.send(&Response::tagged_bad(
                tag,
                "STARTTLS is not allowed on an encrypted connection",
                None,
            ));
            return Ok(SessionFlow::Continue);
        }
        if !self.config.starttls_available {
            out.send(&Response::tagged_no(
                tag,
                "TLS is not configured on this server",
                None,
            ));
            return Ok(SessionFlow::Continue);
        }
        out.send(&Response::tagged_ok(tag, "Begin TLS negotiation now", None));
        Ok(SessionFlow::UpgradeTls)
    }

    async fn command_login<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        user: &str,
        password: &str,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        if self.state != SessionState::NotAuthenticated {
            return Err(FerromaError::Protocol("already authenticated".into()));
        }
        if !self.config.tls && self.config.require_tls_for_login {
            out.send(&Response::tagged_no(
                tag,
                "LOGIN is disabled until the connection is encrypted",
                None,
            ));
            return Ok(SessionFlow::Continue);
        }
        // Never log the password, and never let it into an error message.
        let outcome = self.context.authenticator.authenticate(user, password).await;
        match outcome {
            Ok(Some(user_id)) => {
                self.complete_login(user_id, user).await?;
                out.send(&Response::tagged_ok(tag, "LOGIN completed", None));
                Ok(SessionFlow::Continue)
            }
            Ok(None) => {
                tracing::warn!(
                    connection_id = self.context.connection_id,
                    remote_ip = %self.context.remote_ip,
                    "imap login failed"
                );
                out.send(&Response::tagged_no(tag, "Authentication failed", None));
                Ok(SessionFlow::Continue)
            }
            Err(err) => {
                tracing::warn!(
                    connection_id = self.context.connection_id,
                    remote_ip = %self.context.remote_ip,
                    error = %err,
                    "imap login error"
                );
                out.send(&Response::tagged_no(tag, "Authentication failed", None));
                Ok(SessionFlow::Continue)
            }
        }
    }

    async fn command_authenticate<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        mechanism: &str,
        initial: Option<Vec<u8>>,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let _ = initial;
        if self.state != SessionState::NotAuthenticated {
            return Err(FerromaError::Protocol("already authenticated".into()));
        }
        if !mechanism.eq_ignore_ascii_case("PLAIN") {
            out.send(&Response::tagged_no(
                tag,
                "Unsupported authentication mechanism",
                None,
            ));
            return Ok(SessionFlow::Continue);
        }
        if !self.config.tls && self.config.require_tls_for_login {
            out.send(&Response::tagged_no(
                tag,
                "AUTHENTICATE is disabled until the connection is encrypted",
                None,
            ));
            return Ok(SessionFlow::Continue);
        }
        Err(FerromaError::Unsupported(
            "AUTHENTICATE PLAIN needs the connection's literal reader".into(),
        ))
    }

    /// Record a successful login and resolve the account's Maildir components.
    async fn complete_login(
        &mut self,
        user_id: UserId,
        supplied_address: &str,
    ) -> Result<(), FerromaError> {
        self.user = Some(user_id);
        self.state = SessionState::Authenticated;
        let address = self.resolve_address(user_id, supplied_address).await?;
        let (local, domain) = split_address(&address).ok_or_else(|| {
            FerromaError::Invalid(format!("account address `{address}` is malformed"))
        })?;
        self.address = Some(address);
        self.local_part = Some(local);
        self.domain = Some(domain);
        Ok(())
    }

    /// The address whose Maildir belongs to this account.
    ///
    /// The first address of the account wins (the schema guarantees at most one
    /// primary address).
    async fn resolve_address(
        &self,
        user_id: UserId,
        fallback: &str,
    ) -> Result<String, FerromaError> {
        let mailboxes = self
            .context
            .repos
            .mailboxes
            .list_by_user_with_domain(user_id)
            .await
            .map_err(FerromaError::storage)?;
        if let Some(found) = mailboxes
            .iter()
            .find(|entry| entry.mailbox.is_primary)
            .or_else(|| mailboxes.first())
        {
            return Ok(found.address());
        }
        Ok(fallback.trim().to_ascii_lowercase())
    }

    /// The account's primary address row, creating a default one if the account
    /// has none yet.
    async fn primary_mailbox(&self) -> Result<ferroma_storage::models::Mailbox, FerromaError> {
        let user_id = self
            .user
            .ok_or_else(|| FerromaError::Unauthorized("not logged in".into()))?;
        let mailboxes = self
            .context
            .repos
            .mailboxes
            .list_by_user(user_id)
            .await
            .map_err(FerromaError::storage)?;
        mailboxes
            .into_iter()
            .find(|mailbox| mailbox.is_primary)
            .or(None)
            .ok_or_else(|| {
                FerromaError::NotFound(format!("no address is configured for user {user_id}"))
            })
    }

    // -----------------------------------------------------------------------
    // Mailbox lifecycle
    // -----------------------------------------------------------------------

    async fn open_mailbox<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        name: &str,
        read_only: bool,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let canonical = mbox::canonical(name);
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        let Some(folder) = self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &canonical)
            .await
            .map_err(FerromaError::storage)?
        else {
            // A failed SELECT must leave the session as it was, per RFC 3501.
            out.send(&Response::tagged_no(
                tag,
                format!("Mailbox `{canonical}` does not exist"),
                Some(ResponseCode::NonExistent),
            ));
            return Ok(SessionFlow::Continue);
        };

        let rows = self
            .context
            .repos
            .messages
            .list_unexpunged(folder.folder_id())
            .await
            .map_err(FerromaError::storage)?;
        let messages: Vec<SelectedMessage> =
            rows.iter().map(SelectedMessage::from_row).collect();

        let exists = messages.len();
        let recent = messages.iter().filter(|m| m.recent).count();
        let first_unseen = messages
            .iter()
            .position(|m| !m.flag_set().seen())
            .map(|index| index as u64 + 1);

        self.selected = Some(Selected {
            folder: folder.clone(),
            mailbox_id,
            messages,
            read_only,
        });
        self.state = SessionState::Selected;

        out.send(&Response::untagged_ok(
            format!("{} is the delimiter", mbox::DELIMITER),
            None,
        ));
        out.send(&Response::exists(exists));
        out.send(&Response::recent(recent));
        out.send(&Response::flags(&session_flags()));
        out.send(&Response::permanent_flags(&permanent_flags()));
        out.send(&Response::uid_validity(folder.uid_validity.max(0) as u64));
        out.send(&Response::uid_next(
            self.selected
                .as_ref()
                .map(|s| s.folder.uid_next.max(1) as u64)
                .unwrap_or(1),
        ));
        if let Some(seq) = first_unseen {
            out.send(&Response::unseen(seq));
        }
        out.send(&Response::tagged_ok(
            tag,
            if read_only {
                "[READ-ONLY] EXAMINE completed"
            } else {
                "[READ-WRITE] SELECT completed"
            },
            Some(if read_only {
                ResponseCode::ReadOnly
            } else {
                ResponseCode::ReadWrite
            }),
        ));
        Ok(SessionFlow::Continue)
    }

    async fn command_create<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        name: &str,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let canonical = mbox::canonical(name);
        if let Err(err) = mbox::validate_new_name(&canonical) {
            out.send(&Response::tagged_no(tag, err.message(), None));
            return Ok(SessionFlow::Continue);
        }
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        if self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &canonical)
            .await
            .map_err(FerromaError::storage)?
            .is_some()
        {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox already exists",
                Some(ResponseCode::AlreadyExists),
            ));
            return Ok(SessionFlow::Continue);
        }
        // The parent's row id, so the folder records the hierarchy both in its own
        // `/`-separated name and in `parent_id`. `None` for a top-level folder.
        let mut parent_id = None;
        if let Some(parent) = mbox::parent(&canonical) {
            match self
                .context
                .repos
                .folders
                .find_by_name(mailbox_id, parent)
                .await
                .map_err(FerromaError::storage)?
            {
                Some(folder) => parent_id = Some(folder.folder_id()),
                None => {
                    // RFC 3501: creating a child of a missing parent needs TRYCREATE.
                    out.send(&Response::tagged_no(
                        tag,
                        "Parent mailbox does not exist",
                        Some(ResponseCode::TryCreate),
                    ));
                    return Ok(SessionFlow::Continue);
                }
            }
        }

        let special_use = special_use_for(&canonical);
        self.context
            .repos
            .folders
            .create_in(mailbox_id, &canonical, parent_id, special_use)
            .await
            .map_err(FerromaError::storage)?;
        self.create_disk_folder(&canonical)?;
        out.send(&Response::tagged_ok(tag, "CREATE completed", None));
        Ok(SessionFlow::Continue)
    }

    /// Create the Maildir directory for a new folder, best effort.
    fn create_disk_folder(&self, name: &str) -> Result<(), FerromaError> {
        let (Some(domain), Some(local)) = (self.domain.as_deref(), self.local_part.as_deref())
        else {
            return Ok(());
        };
        self.context
            .maildir
            .create_folder(domain, local, name)
            .map_err(FerromaError::storage)
    }

    async fn command_delete<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        name: &str,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let canonical = mbox::canonical(name);
        if mbox::is_inbox(&canonical) {
            out.send(&Response::tagged_no(tag, "INBOX cannot be deleted", None));
            return Ok(SessionFlow::Continue);
        }
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        let Some(folder) = self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &canonical)
            .await
            .map_err(FerromaError::storage)?
        else {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox does not exist",
                Some(ResponseCode::NonExistent),
            ));
            return Ok(SessionFlow::Continue);
        };

        // `\Noselect` in the LIST response means the folder has children, which
        // IMAP forbids deleting.
        let folders = self
            .context
            .repos
            .folders
            .list(mailbox_id)
            .await
            .map_err(FerromaError::storage)?;
        if folders
            .iter()
            .any(|other| mbox::is_child_of(&canonical, &other.name))
        {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox has children and cannot be deleted",
                None,
            ));
            return Ok(SessionFlow::Continue);
        }

        let removed = self
            .context
            .repos
            .folders
            .delete(folder.folder_id())
            .await
            .map_err(FerromaError::storage)?;
        if !removed {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox does not exist",
                Some(ResponseCode::NonExistent),
            ));
            return Ok(SessionFlow::Continue);
        }
        if let (Some(domain), Some(local)) = (self.domain.as_deref(), self.local_part.as_deref()) {
            // A failure to remove the directory must not fail the command: the
            // database is authoritative for what exists.
            let _ = self.context.maildir.delete_folder(domain, local, &canonical);
        }
        out.send(&Response::tagged_ok(tag, "DELETE completed", None));
        Ok(SessionFlow::Continue)
    }

    async fn command_rename<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        from: &str,
        to: &str,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let (from, to) = (mbox::canonical(from), mbox::canonical(to));
        if let Err(err) = mbox::validate_new_name(&to) {
            out.send(&Response::tagged_no(tag, err.message(), None));
            return Ok(SessionFlow::Continue);
        }
        if mbox::is_inbox(&from) {
            // RFC 3501 lets INBOX be renamed: every message moves to the new
            // name. That is a data migration, not a rename, so we answer NO and
            // let the client COPY + DELETE instead.
            out.send(&Response::tagged_no(
                tag,
                "INBOX cannot be renamed; copy the messages and create a new folder",
                None,
            ));
            return Ok(SessionFlow::Continue);
        }
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        let Some(folder) = self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &from)
            .await
            .map_err(FerromaError::storage)?
        else {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox does not exist",
                Some(ResponseCode::NonExistent),
            ));
            return Ok(SessionFlow::Continue);
        };
        if self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &to)
            .await
            .map_err(FerromaError::storage)?
            .is_some()
        {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox already exists",
                Some(ResponseCode::AlreadyExists),
            ));
            return Ok(SessionFlow::Continue);
        }

        self.context
            .repos
            .folders
            .rename(folder.folder_id(), &to)
            .await
            .map_err(FerromaError::storage)?;
        if let (Some(domain), Some(local)) = (self.domain.as_deref(), self.local_part.as_deref()) {
            let _ = self
                .context
                .maildir
                .rename_folder(domain, local, &from, &to);
        }
        out.send(&Response::tagged_ok(tag, "RENAME completed", None));
        Ok(SessionFlow::Continue)
    }

    async fn command_subscribe<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        name: &str,
        subscribe: bool,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let canonical = mbox::canonical(name);
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        let Some(folder) = self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &canonical)
            .await
            .map_err(FerromaError::storage)?
        else {
            // RFC 3501 makes subscribing to a missing folder legal — it is a
            // promise about the future, not a claim about the present.
            out.send(&Response::tagged_ok(
                tag,
                if subscribe {
                    "SUBSCRIBE completed"
                } else {
                    "UNSUBSCRIBE completed"
                },
                None,
            ));
            return Ok(SessionFlow::Continue);
        };
        self.context
            .repos
            .folders
            .set_subscribed(folder.folder_id(), subscribe)
            .await
            .map_err(FerromaError::storage)?;
        out.send(&Response::tagged_ok(
            tag,
            if subscribe {
                "SUBSCRIBE completed"
            } else {
                "UNSUBSCRIBE completed"
            },
            None,
        ));
        Ok(SessionFlow::Continue)
    }

    async fn command_list<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        reference: &str,
        pattern: &str,
        subscribed_only: bool,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let verb = if subscribed_only { "LSUB" } else { "LIST" };
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        let folders = if subscribed_only {
            self.context
                .repos
                .folders
                .list_subscribed(mailbox_id)
                .await
                .map_err(FerromaError::storage)?
        } else {
            self.context
                .repos
                .folders
                .list(mailbox_id)
                .await
                .map_err(FerromaError::storage)?
        };

        let query = mbox::Pattern::new(reference, pattern);
        if query.is_root_query() {
            // `LIST "" ""` asks for the delimiter and the root name.
            out.send(&Response::list(&["\\Noselect"], Some(mbox::DELIMITER), ""));
            out.send(&Response::tagged_ok(
                tag,
                format!("{verb} completed"),
                None,
            ));
            return Ok(SessionFlow::Continue);
        }

        for folder in &folders {
            if !query.matches(&folder.name) {
                continue;
            }
            let mut attributes: Vec<&str> = Vec::new();
            if folders
                .iter()
                .any(|other| mbox::is_child_of(&folder.name, &other.name))
            {
                attributes.push("\\HasChildren");
            } else {
                attributes.push("\\HasNoChildren");
            }
            if let Some(special_use) = folder.special_use.as_deref() {
                attributes.push(special_use);
            }
            if subscribed_only {
                out.send(&Response::lsub(
                    &attributes,
                    Some(mbox::DELIMITER),
                    &folder.name,
                ));
            } else {
                out.send(&Response::list(
                    &attributes,
                    Some(mbox::DELIMITER),
                    &folder.name,
                ));
            }
        }
        out.send(&Response::tagged_ok(tag, format!("{verb} completed"), None));
        Ok(SessionFlow::Continue)
    }

    async fn command_status<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        name: &str,
        items: &[StatusItem],
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let canonical = mbox::canonical(name);
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        let Some(folder) = self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &canonical)
            .await
            .map_err(FerromaError::storage)?
        else {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox does not exist",
                Some(ResponseCode::NonExistent),
            ));
            return Ok(SessionFlow::Continue);
        };

        let messages = self
            .context
            .repos
            .messages
            .list_unexpunged(folder.folder_id())
            .await
            .map_err(FerromaError::storage)?;
        let recent = messages
            .iter()
            .filter(|message| is_recent(&message.storage_path))
            .count() as u64;
        let unseen = messages
            .iter()
            .filter(|message| !parse_flags(&message.flags).seen())
            .count() as u64;
        let total_bytes: u64 = messages
            .iter()
            .map(|message| message.size_bytes.max(0) as u64)
            .sum();

        let status = StatusItems {
            messages: messages.len() as u64,
            recent,
            uidnext: folder.uid_next.max(1) as u64,
            uidvalidity: folder.uid_validity.max(0) as u64,
            unseen,
            size: Some(total_bytes),
        };
        out.send(&Response::mailbox_status(
            &canonical,
            &status.render(items),
        ));
        out.send(&Response::tagged_ok(tag, "STATUS completed", None));
        Ok(SessionFlow::Continue)
    }

    // -----------------------------------------------------------------------
    // APPEND
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn command_append<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        name: &str,
        flags: &[String],
        internal_date: Option<DateTime<Utc>>,
        message: Vec<u8>,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        if message.len() as u64 > self.config.max_append_size {
            // RFC 3501 §6.3.11: a message that is too big is a NO with the
            // `TOOBIG` code, and the literal was already consumed.
            out.send(&Response::tagged_no(
                tag,
                "Message is too large to append",
                Some(ResponseCode::Other("TOOBIG".to_string())),
            ));
            return Ok(SessionFlow::Continue);
        }
        let canonical = mbox::canonical(name);
        let mailbox = self.primary_mailbox().await?;
        let mailbox_id = mailbox.mailbox_id();
        let Some(folder) = self
            .context
            .repos
            .folders
            .find_by_name(mailbox_id, &canonical)
            .await
            .map_err(FerromaError::storage)?
        else {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox does not exist",
                Some(ResponseCode::TryCreate),
            ));
            return Ok(SessionFlow::Continue);
        };

        // The quota is the account's; `check_quota` raises `LimitExceeded`.
        if let Err(err) = self
            .context
            .repos
            .mailboxes
            .check_quota(mailbox_id, message.len() as i64)
            .await
        {
            out.send(&Response::tagged_no(
                tag,
                "Mailbox is over quota",
                Some(ResponseCode::OverQuota),
            ));
            let _ = err;
            return Ok(SessionFlow::Continue);
        }

        let parsed = ParsedMessage::parse(&message).ok();
        let flag_set = Flags::parse(&flags.join(" "));
        let flag_string = flags_to_column(&flag_set);
        let (Some(domain), Some(local)) = (self.domain.clone(), self.local_part.clone()) else {
            return Err(FerromaError::Unauthorized("not logged in".into()));
        };

        self.create_disk_folder(&canonical)?;
        let stored = self
            .context
            .maildir
            .store(&domain, &local, &canonical, &message, &flag_string)
            .map_err(FerromaError::storage)?;

        let sender = parsed.as_ref().and_then(|p| p.from().first().cloned());
        let subject = parsed.as_ref().and_then(ParsedMessage::subject);
        let sent_at = parsed.as_ref().and_then(ParsedMessage::date);
        let has_attachments = parsed
            .as_ref()
            .map(ParsedMessage::has_attachments)
            .unwrap_or(false);
        let attachment_count = parsed
            .as_ref()
            .map(|p| p.attachments().len() as i32)
            .unwrap_or(0);
        let snippet = parsed
            .as_ref()
            .map(|p| p.snippet(160))
            .filter(|s| !s.is_empty());

        let inserted = self
            .context
            .repos
            .messages
            .insert(NewMessage {
                folder_id: folder.folder_id(),
                mailbox_id,
                rfc_message_id: parsed
                    .as_ref()
                    .and_then(ParsedMessage::message_id)
                    .map(|id| id.to_string()),
                thread_id: None,
                subject,
                sender: sender.as_ref().map(|m| m.address.to_string()),
                sender_name: sender.as_ref().and_then(|m| m.name.clone()),
                snippet,
                size_bytes: message.len() as i64,
                storage_path: stored.path.clone(),
                checksum_sha256: Some(stored.sha256.clone()),
                flags: flag_string.clone(),
                internal_date,
                sent_at,
                has_attachments,
                attachment_count,
                is_draft: flag_set.draft(),
            })
            .await
            .map_err(FerromaError::storage)?;

        // Keep the write-time account usage in step with the new message.
        let _ = self
            .context
            .repos
            .mailboxes
            .add_usage(mailbox_id, message.len() as i64)
            .await;

        self.publish(
            EventScope::User(self.user.unwrap_or_else(|| UserId::new(0))),
            Event::mail_received(folder.folder_id(), inserted.message_id()),
        )
        .await;

        if let Some(selected) = self.selected.as_mut() {
            if selected.folder.id == folder.id {
                selected.messages.push(SelectedMessage::from_row(&inserted));
                selected.folder.message_count = selected.messages.len() as i32;
            }
        }

        out.send(&Response::tagged_ok(
            tag,
            format!(
                "[APPENDUID {} {}] APPEND completed",
                folder.uid_validity.max(0),
                inserted.uid
            ),
            Some(ResponseCode::AppendUid(
                folder.uid_validity.max(0) as u64,
                inserted.uid.max(0) as u64,
            )),
        ));
        Ok(SessionFlow::Continue)
    }

    // -----------------------------------------------------------------------
    // CLOSE / EXPUNGE
    // -----------------------------------------------------------------------

    /// `CLOSE`: expunge the `\Deleted` messages without reporting them, and
    /// return to the authenticated state.
    async fn command_close<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let harmful = {
            let selected = self.require_selected("CLOSE")?;
            !selected.read_only
        };
        if harmful {
            let doomed: Vec<MessageId> = {
                let selected = self.require_selected("CLOSE")?;
                selected
                    .messages
                    .iter()
                    .filter(|message| message.flag_set().deleted())
                    .map(|message| message.id)
                    .collect()
            };
            for id in doomed {
                self.expunge_one(id).await?;
            }
        }
        self.selected = None;
        self.state = SessionState::Authenticated;
        out.send(&Response::tagged_ok(tag, "CLOSE completed", None));
        Ok(SessionFlow::Continue)
    }

    /// Remove one message's row and body file.
    ///
    /// The row is hard-deleted rather than tombstoned: `MessagesRepository::expunge`
    /// tombstones *every* `\Deleted` message of a folder, which would expunge
    /// messages the client did not ask about. A targeted read-then-delete is the
    /// only way to honour `EXPUNGE`'s and `UID EXPUNGE`'s contract.
    async fn expunge_one(&mut self, id: MessageId) -> Result<(), FerromaError> {
        let Some(row) = self
            .context
            .repos
            .messages
            .hard_delete(id)
            .await
            .map_err(FerromaError::storage)?
        else {
            return Ok(());
        };
        self.remove_body_file(&row.storage_path);
        let _ = self
            .context
            .repos
            .mailboxes
            .add_usage(MailboxId::new(row.mailbox_id), -row.size_bytes.max(0))
            .await;
        Ok(())
    }

    /// `EXPUNGE` / `UID EXPUNGE <set>`.
    async fn command_expunge<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        uids: Option<&SequenceSet>,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        // Collect the targets with their *current* sequence numbers, then walk
        // them in descending order: RFC 3501 §7.4.1 requires the `* n EXPUNGE`
        // responses to be emitted that way, because the client decrements its
        // own numbering by one for each EXPUNGE it has already processed.
        let mut targets: Vec<(usize, MessageId)> = {
            let selected = self.require_writable("EXPUNGE")?;
            let max_uid = selected
                .messages
                .iter()
                .map(|m| m.uid.max(0) as u64)
                .max()
                .unwrap_or(0);
            selected
                .messages
                .iter()
                .enumerate()
                .filter(|(_, message)| message.flag_set().deleted())
                .filter(|(_, message)| match uids {
                    Some(set) => set.contains(message.uid.max(0) as u64, max_uid),
                    None => true,
                })
                .map(|(index, message)| (index, message.id))
                .collect()
        };
        targets.sort_unstable_by_key(|(index, _)| std::cmp::Reverse(*index));

        for (index, id) in targets {
            self.expunge_one(id).await?;
            if let Some(selected) = self.selected.as_mut() {
                if index < selected.messages.len() && selected.messages[index].id == id {
                    selected.messages.remove(index);
                } else if let Some(position) = selected.messages.iter().position(|m| m.id == id) {
                    // Defensive: the list changed under us, so fall back to the
                    // message's actual position rather than reporting a wrong
                    // sequence number.
                    selected.messages.remove(position);
                    out.send(&Response::expunge(position + 1));
                    continue;
                }
                out.send(&Response::expunge(index + 1));
            }
            self.publish(
                EventScope::User(self.user.unwrap_or_else(|| UserId::new(0))),
                Event::mail_deleted(
                    self.selected
                        .as_ref()
                        .map(|s| s.folder.folder_id())
                        .unwrap_or_else(|| MailboxId::new(0)),
                    id,
                    true,
                ),
            )
            .await;
        }
        if let Some(selected) = self.selected.as_mut() {
            selected.folder.message_count = selected.messages.len() as i32;
        }
        out.send(&Response::tagged_ok(tag, "EXPUNGE completed", None));
        Ok(SessionFlow::Continue)
    }

    // -----------------------------------------------------------------------
    // SEARCH
    // -----------------------------------------------------------------------

    async fn command_search<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        charset: &Option<String>,
        key: &Option<SearchKey>,
        uid_mode: bool,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        if let Some(charset) = charset {
            if !search::charset_supported(charset) {
                out.send(&Response::tagged_no(
                    tag,
                    "Unsupported charset",
                    Some(ResponseCode::BadCharset(
                        search::SUPPORTED_CHARSETS
                            .iter()
                            .map(|c| c.to_string())
                            .collect(),
                    )),
                ));
                return Ok(SessionFlow::Continue);
            }
        }
        let key = key.clone().unwrap_or(SearchKey::All);
        let (max_seq, max_uid, snapshot) = {
            let selected = self.require_selected("SEARCH")?;
            (
                selected.messages.len() as u64,
                selected
                    .messages
                    .iter()
                    .map(|m| m.uid.max(0) as u64)
                    .max()
                    .unwrap_or(0),
                selected.messages.clone(),
            )
        };
        let needs_body = key.needs_body();
        let needs_headers = key.needs_headers();

        let mut hits: Vec<u64> = Vec::new();
        for (index, message) in snapshot.iter().enumerate() {
            let facts = self.message_facts(index, message, needs_headers).await?;
            let body = if needs_body {
                self.read_body(message).await
            } else {
                None
            };
            if search::matches(&key, &facts, body.as_deref(), uid_mode, max_seq, max_uid) {
                hits.push(if uid_mode {
                    message.uid.max(0) as u64
                } else {
                    index as u64 + 1
                });
            }
        }

        out.send(&Response::search(&hits));
        out.send(&Response::tagged_ok(tag, "SEARCH completed", None));
        Ok(SessionFlow::Continue)
    }

    /// The search facts for one message, reading its headers if needed.
    async fn message_facts(
        &mut self,
        index: usize,
        message: &SelectedMessage,
        need_headers: bool,
    ) -> Result<MessageFacts, FerromaError> {
        let mut summary = message.headers.clone();
        if need_headers && summary.is_none() {
            summary = Some(self.read_header_summary(message).await);
            if let Some(selected) = self.selected.as_mut() {
                if let Some(slot) = selected.messages.get_mut(index) {
                    slot.headers = summary.clone();
                }
            }
        }
        let summary = summary.unwrap_or_default();
        let mut facts = MessageFacts::new(
            index as u64 + 1,
            message.uid.max(0) as u64,
            message.internal_date,
            message.size.max(0) as u64,
        );
        facts.flags = message.flag_set();
        facts.recent = message.recent;
        facts.sent_at = message.sent_at;
        facts.from = summary.from;
        facts.to = summary.to;
        facts.cc = summary.cc;
        facts.bcc = summary.bcc;
        facts.subject = summary.subject;
        facts.header_block = summary.header_block;
        Ok(facts)
    }

    /// Read a message's header fields from its body file.
    async fn read_header_summary(&self, message: &SelectedMessage) -> MessageHeaderSummary {
        let Some(raw) = self.read_body(message).await else {
            return MessageHeaderSummary::default();
        };
        let parsed = match ParsedMessage::parse(&raw) {
            Ok(parsed) => parsed,
            Err(_) => return MessageHeaderSummary::default(),
        };
        let join = |list: Vec<ferroma_mail::address::Mailbox>| {
            list.iter()
                .map(|mailbox| mailbox.display())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let header_block = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(index) => String::from_utf8_lossy(&raw[..index + 2]).into_owned(),
            None => String::from_utf8_lossy(&raw).into_owned(),
        };
        MessageHeaderSummary {
            from: join(parsed.from()),
            to: join(parsed.to()),
            cc: join(parsed.cc()),
            bcc: parsed
                .headers
                .get("Bcc")
                .map(str::to_string)
                .unwrap_or_default(),
            subject: parsed.subject().unwrap_or_default(),
            header_block,
        }
    }

    /// Read a message's body, capped at [`SEARCH_BODY_LIMIT`].
    async fn read_body(&self, message: &SelectedMessage) -> Option<Vec<u8>> {
        let maildir = self.context.maildir.clone();
        let path = message.storage_path.clone();
        match maildir.read_prefix(&path, SEARCH_BODY_LIMIT) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                tracing::warn!(
                    connection_id = self.context.connection_id,
                    message_id = %message.id,
                    error = %err,
                    "imap could not read a message body"
                );
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // FETCH
    // -----------------------------------------------------------------------

    async fn command_fetch<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        set: &SequenceSet,
        spec: &FetchSpec,
        uid_mode: bool,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let mut implicit_seen = false;
        let selected = self.require_selected("FETCH")?;
        let max = if uid_mode {
            selected.messages.iter().map(|m| m.uid.max(0) as u64).max().unwrap_or(0)
        } else {
            selected.messages.len() as u64
        };

        let items = spec.items();
        let indices: Vec<usize> = selected
            .messages
            .iter()
            .enumerate()
            .filter(|(index, message)| {
                let value = if uid_mode {
                    message.uid.max(0) as u64
                } else {
                    *index as u64 + 1
                };
                set.contains(value, max)
            })
            .map(|(index, _)| index)
            .collect();

        if indices.len() > self.context.config.max_fetch_messages {
            out.send(&Response::tagged_no(
                tag,
                "Too many messages in one FETCH",
                Some(ResponseCode::Other("TOOMANY".to_string())),
            ));
            return Ok(SessionFlow::Continue);
        }

        let mut mark_seen: Vec<MessageId> = Vec::new();
        for index in indices {
            let (message, flags, raw) = {
                let selected = self.require_selected("FETCH")?;
                let message = selected.messages[index].clone();
                let flags = message.flag_set();
                let raw = self.load_message(&message);
                (message, flags, raw)
            };
            let Some(raw) = raw else {
                out.send(&Response::fetch(
                    index + 1,
                    "FLAGS ()  /* message body is missing */",
                ));
                continue;
            };
            let meta = MessageMeta {
                internal_date: message.internal_date,
                // `RFC822.SIZE` is the *stored* size (the row's `size_bytes`),
                // not the size of whatever the file holds now: a client compares
                // this number against the octets it receives, and the row is
                // the contract.
                size: message.size.max(0) as u64,
            };
            let envelope = Envelope::of(&raw);
            let mut fields = Vec::with_capacity(items.len());
            for item in items {
                if item.sets_seen() && !flags.seen() {
                    implicit_seen = true;
                    mark_seen.push(message.id);
                }
                if let Some(field) = fetch::item_value(
                    item,
                    &meta,
                    message.uid.max(0) as u64,
                    &flags,
                    message.recent,
                    &raw,
                    &envelope,
                ) {
                    fields.push(field);
                }
            }
            let body = fetch::render_fields(&fields);
            let mut line = format!("* {} FETCH (", index + 1).into_bytes();
            line.extend_from_slice(&body);
            line.extend_from_slice(b")\r\n");
            out.send_raw(&line);
        }

        // A `BODY[...]` (not `BODY.PEEK[...]`) sets `\Seen`. The flag change is
        // persisted here, which is the only place IMAP writes a flag as a side
        // effect of a read.
        if !mark_seen.is_empty() {
            if let Err(err) = self.persist_seen(&mark_seen).await {
                tracing::warn!(
                    connection_id = self.context.connection_id,
                    error = %err,
                    "imap could not persist \\Seen"
                );
            }
        }
        let _ = implicit_seen;

        out.send(&Response::tagged_ok(tag, "FETCH completed", None));
        Ok(SessionFlow::Continue)
    }

    /// Load and parse one message's body file.
    fn load_message(&self, message: &SelectedMessage) -> Option<RawMessage> {
        match self.context.maildir.read(&message.storage_path) {
            Ok(bytes) => Some(RawMessage::parse(bytes)),
            Err(err) => {
                tracing::warn!(
                    connection_id = self.context.connection_id,
                    message_id = %message.id,
                    error = %err,
                    "imap could not read a message body"
                );
                None
            }
        }
    }

    /// Add `\Seen` to messages, in the database and in their Maildir names.
    async fn persist_seen(&mut self, ids: &[MessageId]) -> Result<(), FerromaError> {
        for id in ids {
            self.context
                .repos
                .messages
                .mark_seen(*id, true)
                .await
                .map_err(FerromaError::storage)?;
            if let Some(selected) = self.selected.as_mut() {
                if let Some(message) = selected.messages.iter_mut().find(|m| m.id == *id) {
                    let mut flags = message.flag_set();
                    flags.set_seen(true);
                    message.flags = flags_to_column(&flags);
                    let updated = self
                        .context
                        .maildir
                        .set_flags(&message.storage_path, &message.flags);
                    if let Ok(path) = updated {
                        message.storage_path = path;
                    }
                }
            }
            self.publish(
                EventScope::User(self.user.unwrap_or_else(|| UserId::new(0))),
                Event::mail_read(
                    self.selected
                        .as_ref()
                        .map(|s| s.folder.folder_id())
                        .unwrap_or_else(|| MailboxId::new(0)),
                    *id,
                    true,
                ),
            )
            .await;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // STORE
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn command_store<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        set: &SequenceSet,
        action: StoreAction,
        silent: bool,
        flags: &[String],
        uid_mode: bool,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let incoming = Flags::parse(&flags.join(" "));
        let (targets, folder_id, max) = {
            let selected = self.require_writable("STORE")?;
            let max = if uid_mode {
                selected.messages.iter().map(|m| m.uid.max(0) as u64).max().unwrap_or(0)
            } else {
                selected.messages.len() as u64
            };
            let targets: Vec<usize> = selected
                .messages
                .iter()
                .enumerate()
                .filter(|(index, message)| {
                    let value = if uid_mode {
                        message.uid.max(0) as u64
                    } else {
                        *index as u64 + 1
                    };
                    set.contains(value, max)
                })
                .map(|(index, _)| index)
                .collect();
            (targets, selected.folder.folder_id(), max)
        };
        let _ = max;

        for index in targets {
            let (id, uid, storage_path, before, recent) = {
                let selected = self.require_selected("STORE")?;
                let message = &selected.messages[index];
                (
                    message.id,
                    message.uid,
                    message.storage_path.clone(),
                    message.flags.clone(),
                    message.recent,
                )
            };
            let updated = apply_store(&before, &incoming, action);
            if updated == before {
                // Nothing changed: still report the flags unless `.SILENT`.
                if !silent {
                    out.send(&Response::fetch(
                        index + 1,
                        &format!(
                            "UID {} FLAGS {}",
                            uid,
                            fetch::render_flags(&parse_flags(&before), recent)
                        ),
                    ));
                }
                continue;
            }
            self.context
                .repos
                .messages
                .set_flags(id, &updated)
                .await
                .map_err(FerromaError::storage)?;
            let new_path = self
                .context
                .maildir
                .set_flags(&storage_path, &updated)
                .unwrap_or_else(|_| storage_path.clone());
            if new_path != storage_path {
                self.context
                    .repos
                    .messages
                    .set_storage_path(id, &new_path)
                    .await
                    .map_err(FerromaError::storage)?;
            }
            if let Some(selected) = self.selected.as_mut() {
                if let Some(message) = selected.messages.get_mut(index) {
                    message.flags = updated.clone();
                    message.storage_path = new_path;
                }
            }
            self.publish(
                EventScope::User(self.user.unwrap_or_else(|| UserId::new(0))),
                Event::mail_flag_changed(folder_id, id, updated.clone()),
            )
            .await;
            if !silent {
                out.send(&Response::fetch(
                    index + 1,
                    &format!(
                        "UID {} FLAGS {}",
                        uid,
                        fetch::render_flags(&parse_flags(&updated), recent)
                    ),
                ));
            }
        }

        out.send(&Response::tagged_ok(tag, "STORE completed", None));
        Ok(SessionFlow::Continue)
    }

    // -----------------------------------------------------------------------
    // COPY / MOVE
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn command_copy<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        set: &SequenceSet,
        destination: &str,
        move_messages: bool,
        uid_mode: bool,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        let verb = if move_messages { "MOVE" } else { "COPY" };
        let canonical = mbox::canonical(destination);
        let (source_folder, source_mailbox, targets) = {
            let selected = self.require_selected(verb)?;
            if selected.read_only {
                // `EXAMINE` opens the mailbox read-only, and RFC 3501 §6.3.2
                // forbids *any* change to it — which includes the `\Recent`
                // bookkeeping a `COPY` implies. A client that wants to file
                // mail out of a read-only view has to `SELECT` it first.
                return Err(FerromaError::Forbidden(format!(
                    "{verb} is not allowed on a read-only mailbox"
                )));
            }

            let max = if uid_mode {
                selected.messages.iter().map(|m| m.uid.max(0) as u64).max().unwrap_or(0)
            } else {
                selected.messages.len() as u64
            };
            let targets: Vec<SelectedMessage> = selected
                .messages
                .iter()
                .enumerate()
                .filter(|(index, message)| {
                    let value = if uid_mode {
                        message.uid.max(0) as u64
                    } else {
                        *index as u64 + 1
                    };
                    set.contains(value, max)
                })
                .map(|(_, message)| message.clone())
                .collect();
            (selected.folder.clone(), selected.mailbox_id, targets)
        };
        if targets.is_empty() {
            out.send(&Response::tagged_ok(tag, format!("{verb} completed"), None));
            return Ok(SessionFlow::Continue);
        }

        let Some(destination_folder) = self
            .context
            .repos
            .folders
            .find_by_name(source_mailbox, &canonical)
            .await
            .map_err(FerromaError::storage)?
        else {
            out.send(&Response::tagged_no(
                tag,
                "Destination mailbox does not exist",
                Some(ResponseCode::TryCreate),
            ));
            return Ok(SessionFlow::Continue);
        };

        let (Some(domain), Some(local)) = (self.domain.clone(), self.local_part.clone()) else {
            return Err(FerromaError::Unauthorized("not logged in".into()));
        };
        self.create_disk_folder(&canonical)?;
        self.create_disk_folder(&source_folder.name)?;

        let mut source_uids: Vec<u64> = Vec::new();
        let mut dest_uids: Vec<u64> = Vec::new();
        let mut moved_ids: Vec<MessageId> = Vec::new();

        for message in &targets {
            let row = self
                .context
                .repos
                .messages
                .require_by_id(message.id)
                .await
                .map_err(FerromaError::storage)?;
            let copied_row = if move_messages {
                self.context
                    .repos
                    .messages
                    .move_to_folder(
                        message.id,
                        destination_folder.folder_id(),
                        source_mailbox,
                    )
                    .await
                    .map_err(FerromaError::storage)?
            } else {
                self.context
                    .repos
                    .messages
                    .copy_to_folder(
                        message.id,
                        destination_folder.folder_id(),
                        source_mailbox,
                    )
                    .await
                    .map_err(FerromaError::storage)?
            };

            // The body file moves to the destination folder's Maildir. The
            // database row is authoritative, and both `store` and `delete` are
            // idempotent, so a partial failure here cannot corrupt the mailbox.
            let bytes = self.context.maildir.read(&row.storage_path).ok();
            let new_path = match bytes {
                Some(bytes) => self
                    .context
                    .maildir
                    .store(&domain, &local, &canonical, &bytes, &copied_row.flags)
                    .ok()
                    .map(|stored| stored.path),
                None => None,
            };
            match new_path {
                Some(path) => {
                    let _ = self
                        .context
                        .repos
                        .messages
                        .set_storage_path(copied_row.message_id(), &path)
                        .await;
                    if move_messages {
                        let _ = self.context.maildir.delete(&row.storage_path);
                    }
                }
                None => {
                    // No bytes to copy: the row would point at a missing file, so
                    // undo the row rather than leave a dangling reference.
                    let _ = self
                        .context
                        .repos
                        .messages
                        .hard_delete(copied_row.message_id())
                        .await;
                    continue;
                }
            }

            source_uids.push(message.uid.max(0) as u64);
            dest_uids.push(copied_row.uid.max(0) as u64);
            if move_messages {
                moved_ids.push(message.id);
            }
            self.publish(
                EventScope::User(self.user.unwrap_or_else(|| UserId::new(0))),
                Event::mail_moved(
                    source_folder.folder_id(),
                    destination_folder.folder_id(),
                    copied_row.message_id(),
                ),
            )
            .await;
        }

        if move_messages && !moved_ids.is_empty() {
            // A MOVE expunges the source copies. `EXPUNGE` responses are emitted
            // in descending sequence order (RFC 3501 §7.4.1) so the client can
            // decrement its numbering as it processes them.
            let mut indices: Vec<usize> = {
                let selected = self.require_selected(verb)?;
                selected
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| moved_ids.contains(&message.id))
                    .map(|(index, _)| index)
                    .collect()
            };
            indices.sort_unstable_by(|a, b| b.cmp(a));
            for index in indices {
                // The row was *moved*, not copied: it now lives in the
                // destination folder, so only this session's view of the source
                // mailbox changes here. Deleting the row would delete the very
                // message the MOVE just delivered.
                if let Some(selected) = self.selected.as_mut() {
                    // The stored index is only valid while the list is unchanged,
                    // and we remove exactly the entry we read it from.
                    if index < selected.messages.len() {
                        selected.messages.remove(index);
                    }
                    out.send(&Response::expunge(index + 1));
                }
            }
            if let Some(selected) = self.selected.as_mut() {
                selected.folder.message_count = selected.messages.len() as i32;
            }
        }

        if !source_uids.is_empty() {
            out.send(&Response::untagged_ok(
                format!(
                    "[COPYUID {} {} {}] {verb} completed",
                    destination_folder.uid_validity.max(0),
                    compress_set(&source_uids),
                    compress_set(&dest_uids)
                ),
                Some(ResponseCode::CopyUid(
                    destination_folder.uid_validity.max(0) as u64,
                    compress_set(&source_uids),
                    compress_set(&dest_uids),
                )),
            ));
        }
        out.send(&Response::tagged_ok(tag, format!("{verb} completed"), None));
        Ok(SessionFlow::Continue)
    }

    // -----------------------------------------------------------------------
    // IDLE
    // -----------------------------------------------------------------------

    fn command_idle<O: SessionOutput + Send>(
        &mut self,
        tag: &str,
        out: &mut O,
    ) -> Result<SessionFlow, FerromaError> {
        if !self.config.enable_idle {
            out.send(&Response::tagged_no(tag, "IDLE is not enabled", None));
            return Ok(SessionFlow::Continue);
        }

        let Some(bus) = self.context.events.clone() else {
            out.send(&Response::tagged_no(
                tag,
                "IDLE needs the event bus, which is not configured",
                None,
            ));
            return Ok(SessionFlow::Continue);
        };
        // Subscribe *before* the continuation goes out, so a change published
        // while the client is still reading `+ idling` cannot be missed.
        let scope = EventScope::User(self.user.unwrap_or_else(|| UserId::new(0)));
        self.idle = Some(bus.subscribe_filtered(scope.into()));
        out.send(&Response::continuation("idling"));

        self.parser.begin_idle();
        Ok(SessionFlow::Continue)
    }

    /// Re-read the selected mailbox and report what changed.
    ///
    /// This is what turns an `IDLE` wake-up into a response a client can act on:
    /// `* n EXISTS` and `* n RECENT` are the only two untagged replies a client
    /// needs to resynchronise, and they must carry the *current* counts — so the
    /// mailbox is re-read rather than the counts being adjusted by guesswork.
    async fn refresh_selected<O: SessionOutput + Send>(
        &mut self,
        out: &mut O,
    ) -> Result<(), FerromaError> {
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        let folder_id = selected.folder.folder_id();
        let rows = self
            .context
            .repos
            .messages
            .list_unexpunged(folder_id)
            .await
            .map_err(FerromaError::storage)?;
        let messages: Vec<SelectedMessage> =
            rows.iter().map(SelectedMessage::from_row).collect();
        let exists = messages.len();
        let recent = messages.iter().filter(|m| m.recent).count();
        if let Some(selected) = self.selected.as_mut() {
            selected.messages = messages;
            selected.folder.message_count = exists as i32;
        }
        out.send(&Response::exists(exists));
        // `RECENT` is only sent when it is non-zero: an unconditional
        // `* 0 RECENT` would reset the client's recent count on every push.
        if recent > 0 {
            out.send(&Response::recent(recent));
        }
        Ok(())
    }

    /// Serve an `IDLE` until the client sends `DONE`.
    ///
    /// Pushes mailbox changes as the event bus publishes them, which is what
    /// makes Thunderbird feel instant, and ends the IDLE with `* BYE` when the
    /// client holds it longer than `imap.max_idle_secs`.
    async fn serve_idle<I, O>(
        &mut self,
        input: &mut I,
        out: &mut O,
        started: std::time::Instant,
    ) -> Result<SessionFlow, FerromaError>
    where
        I: SessionInput + Send,
        O: SessionOutput + Send,
    {
        let tag = self.parser.tag().to_string();
        self.parser.end_idle();

        // The subscription was created when `IDLE` was accepted, so nothing
        // published since then can have been missed.
        let Some(mut subscription) = self.idle.take() else {
            out.send(&Response::tagged_ok(&tag, "IDLE terminated", None));
            return Ok(SessionFlow::Continue);
        };
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(self.config.max_idle_secs.max(1));

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // RFC 2177 lets a server end an over-long IDLE; a `BYE` tells the
                // client to reconnect rather than leaving it hanging.
                out.send(&Response::bye("IDLE timed out", None));
                self.state = SessionState::Logout;
                self.log_command("IDLE", started, "BYE");
                return Ok(SessionFlow::Close);
            }
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {
                    out.send(&Response::bye("IDLE timed out", None));
                    self.state = SessionState::Logout;
                    self.log_command("IDLE", started, "BYE");
                    return Ok(SessionFlow::Close);
                }
                event = subscription.recv() => {
                    match event {
                        Ok(envelope) => {
                            // A new message in the selected mailbox is reported
                            // the way a client expects it — `* n EXISTS` — which
                            // means re-reading the mailbox so `n` is the truth
                            // rather than a guess. Anything else can be reported
                            // from the event alone.
                            let new_in_selected = match (&envelope.event, self.selected.as_ref()) {
                                (Event::MailReceived(payload), Some(selected)) => {
                                    payload.mailbox_id == selected.folder.folder_id()
                                }
                                _ => false,
                            };
                            if new_in_selected {
                                self.refresh_selected(out).await?;
                                out.flush().await?;
                                continue;
                            }
                            let selected = self.selected.as_ref().map(|s| s.folder.folder_id());
                            if let Some(response) = idle_response(&envelope.event, selected) {
                                out.send(&response);
                                // An `IDLE` push is only useful if it goes out
                                // now, not when the client eventually says DONE.
                                out.flush().await?;
                            }
                        }
                        Err(SubscriptionError::Lagged(skipped)) => {
                            tracing::warn!(
                                connection_id = self.context.connection_id,
                                skipped,
                                "imap idle subscription lagged"
                            );
                            if let Some(selected) = self.selected.as_ref() {
                                out.send(&Response::exists(selected.messages.len()));
                            }
                        }
                        Err(SubscriptionError::Closed) => {
                            out.send(&Response::bye("event bus closed", None));
                            self.state = SessionState::Logout;
                            return Ok(SessionFlow::Close);
                        }
                        Err(SubscriptionError::Timeout) => {}
                    }
                }
                read = input.read_line() => {
                    match read {
                        Ok(Some(next)) => {
                            let text = String::from_utf8_lossy(&next);
                            if CommandParser::is_done(&text) {
                                self.log_command("IDLE", started, "OK");
                                out.send(&Response::tagged_ok(&tag, "IDLE terminated", None));
                                return Ok(SessionFlow::Continue);
                            }
                            out.send(&Response::tagged_bad(&tag, "expected DONE while idling", None));
                            self.log_command("IDLE", started, "BAD");
                            return Ok(SessionFlow::Continue);
                        }
                        Ok(None) => {
                            self.state = SessionState::Logout;
                            return Ok(SessionFlow::Close);
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

    /// Remove a message's body file, best effort.
    fn remove_body_file(&self, path: &str) {
        if let Err(err) = self.context.maildir.delete(path) {
            tracing::warn!(
                connection_id = self.context.connection_id,
                error = %err,
                "imap could not delete a message body"
            );
        }
    }

    /// Publish an event, if a bus is configured.
    async fn publish(&self, scope: EventScope, event: Event) {
        if let Some(bus) = &self.context.events {
            bus.publish(scope, event).await;
        }
    }
}

/// `\Recent` is derived from the Maildir `new/` directory.
fn is_recent(storage_path: &str) -> bool {
    let parts: Vec<&str> = storage_path.split(['/', '\\']).collect();
    parts
        .iter()
        .rev()
        .nth(1)
        .map(|parent| *parent == "new")
        .unwrap_or(false)
}

/// The flag string as the `messages.flags` column stores it.
///
/// The column is space-separated and lower-cased (that is what the storage layer
/// documents and what its `string_to_array(lower(flags), ' ')` queries expect),
/// which is *not* the same as `Flags::to_db_string`'s comma form.
pub fn flags_to_column(flags: &Flags) -> String {
    let mut parts: Vec<String> = Vec::new();
    for name in flags.system_flags() {
        parts.push(name.trim_start_matches('\\').to_ascii_lowercase());
    }
    for keyword in flags.keywords() {
        parts.push(keyword.clone());
    }
    parts.join(" ")
}

/// Parse a flag string in either the wire form (`\Seen`) or the storage form
/// (`seen`).
///
/// `messages.flags` holds space-separated, lower-cased, backslash-less names
/// while clients send `\Seen`; reading both with one helper is what keeps a
/// `STORE` from silently treating a system flag as a keyword.
pub fn parse_flags(raw: &str) -> Flags {
    let mut flags = Flags::new();
    for token in raw.split_whitespace() {
        match token.to_ascii_lowercase().as_str() {
            "seen" | "\\seen" => flags.set_seen(true),
            "answered" | "\\answered" => flags.set_answered(true),
            "flagged" | "\\flagged" => flags.set_flagged(true),
            "deleted" | "\\deleted" => flags.set_deleted(true),
            "draft" | "\\draft" => flags.set_draft(true),
            "recent" | "\\recent" => flags.set_recent(true),
            other => flags.add_keyword(other),
        }
    }
    flags
}

/// Apply a `STORE` action to a stored flag string.
pub fn apply_store(before: &str, incoming: &Flags, action: StoreAction) -> String {
    let mut current = parse_flags(before);
    match action {
        StoreAction::Replace => current = incoming.clone(),
        StoreAction::Add => {
            for name in incoming.system_flags() {
                match name {
                    "\\Seen" => current.set_seen(true),
                    "\\Answered" => current.set_answered(true),
                    "\\Flagged" => current.set_flagged(true),
                    "\\Deleted" => current.set_deleted(true),
                    "\\Draft" => current.set_draft(true),
                    "\\Recent" => current.set_recent(true),
                    _ => {}
                }
            }
            for keyword in incoming.keywords() {
                current.add_keyword(keyword);
            }
        }
        StoreAction::Remove => {
            for name in incoming.system_flags() {
                match name {
                    "\\Seen" => current.set_seen(false),
                    "\\Answered" => current.set_answered(false),
                    "\\Flagged" => current.set_flagged(false),
                    "\\Deleted" => current.set_deleted(false),
                    "\\Draft" => current.set_draft(false),
                    "\\Recent" => current.set_recent(false),
                    _ => {}
                }
            }
            for keyword in incoming.keywords() {
                current.remove_keyword(keyword);
            }
        }
    }
    flags_to_column(&current)
}

/// The flags a mailbox can hold.
pub fn session_flags() -> Vec<String> {
    ["\\Answered", "\\Flagged", "\\Deleted", "\\Seen", "\\Draft"]
        .iter()
        .map(|flag| (*flag).to_string())
        .collect()
}

/// The flags a client may set permanently.
pub fn permanent_flags() -> Vec<String> {
    let mut flags = session_flags();
    flags.push("\\*".to_string());
    flags
}

/// The `special_use` marker for a freshly created standard folder.
fn special_use_for(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "sent" => Some("\\Sent"),
        "drafts" => Some("\\Drafts"),
        "trash" => Some("\\Trash"),
        "junk" | "spam" => Some("\\Junk"),
        "archive" => Some("\\Archive"),
        _ => None,
    }
}

/// Compress sorted UIDs into the `1:3,5` form `COPYUID` uses.
pub fn compress_set(values: &[u64]) -> String {
    let mut sorted: Vec<u64> = values.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            index += 1;
            end = sorted[index];
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}:{end}"));
        }
        index += 1;
    }
    parts.join(",")
}

/// Turn a session error into the response a client should see.
pub fn error_response(tag: &str, err: &FerromaError) -> (Response, &'static str) {
    let text = describe_error(err);
    match err {
        FerromaError::Parse(_) | FerromaError::Protocol(_) => {
            (Response::tagged_bad(tag, text, None), "BAD")
        }
        FerromaError::Unauthorized(_) | FerromaError::Forbidden(_) => {
            (Response::tagged_no(tag, text, None), "NO")
        }
        FerromaError::NotFound(_) => (
            Response::tagged_no(tag, text, Some(ResponseCode::NonExistent)),
            "NO",
        ),
        FerromaError::Unsupported(_) => (
            Response::tagged_no(tag, text, Some(ResponseCode::Other("CANNOT".into()))),
            "NO",
        ),
        FerromaError::LimitExceeded(_) => (
            Response::tagged_no(tag, text, Some(ResponseCode::OverQuota)),
            "NO",
        ),
        FerromaError::Conflict(_) => (
            Response::tagged_no(tag, text, Some(ResponseCode::AlreadyExists)),
            "NO",
        ),
        _ => (Response::tagged_no(tag, text, None), "NO"),
    }
}

/// The unsolicited response an event should produce inside `IDLE`.
///
/// Only changes inside the *selected* mailbox matter to the client; a message
/// arriving in another folder is left for the next `SELECT`.
pub fn idle_response(event: &Event, selected: Option<MailboxId>) -> Option<Response> {
    let in_selected = |mailbox: MailboxId| match selected {
        Some(current) => current == mailbox,
        None => false,
    };
    match event {
        Event::MailReceived(payload) if in_selected(payload.mailbox_id) => {
            Some(Response::untagged_ok("new message", None))
        }
        Event::MailFlagChanged(payload) if in_selected(payload.mailbox_id) => {
            Some(Response::untagged(format!(
                "FETCH (FLAGS ({}))",
                payload.flags
            )))
        }
        Event::MailDeleted(payload) if in_selected(payload.mailbox_id) => {
            Some(Response::untagged_ok("message removed", None))
        }
        _ => None,
    }
}

/// The user-facing text of an error, flattened onto one line.
pub fn describe_error(err: &FerromaError) -> String {
    err.to_string().replace(['\r', '\n'], " ")
}

/// The literal reader a session hands to the parser.
///
/// It holds only shared borrows of the input and output, so the future it
/// produces is `Send` even though the session itself is not — the parser needs
/// no access to the session while it is reading octets.
struct SocketLiterals<'a, I: ?Sized, O: ?Sized> {
    input: &'a mut I,
    out: &'a mut O,
    /// Where the text that follows a mid-line literal's octets is queued.
    pending: &'a mut std::collections::VecDeque<Vec<u8>>,
}

impl<I, O> LiteralSource for SocketLiterals<'_, I, O>
where
    I: SessionInput + Send,
    O: SessionOutput + Send,
{
    async fn read_literal(
        &mut self,
        declared: u64,
        synchronizing: bool,
    ) -> Result<Vec<u8>, LiteralError> {
        if synchronizing {
            self.out
                .send(&Response::continuation("Ready for literal data"));
            // The continuation must reach the client *before* the octets are
            // read: a client that honours RFC 3501 waits for it, so buffering it
            // until the command completes would deadlock both sides.
            self.out
                .flush()
                .await
                .map_err(|err| LiteralError::Io(err.to_string()))?;
        }
        let mut buffer: Vec<u8> = Vec::with_capacity(declared.min(64 * 1024) as usize);
        while (buffer.len() as u64) < declared {
            let remaining = declared - buffer.len() as u64;
            let line = self
                .input
                .read_line()
                .await
                .map_err(|err| LiteralError::Io(err.to_string()))?;
            let Some(line) = line else {
                return Err(LiteralError::Io(
                    "connection closed while reading a literal".into(),
                ));
            };
            let available = line.len() as u64;
            if available < remaining {
                // Not the last line of the literal: its CRLF belongs to the
                // octets, so put it back.
                buffer.extend_from_slice(&line);
                buffer.extend_from_slice(b"\r\n");
            } else {
                // The octets end inside this line, so what follows them is the
                // rest of the command line — `RENAME {3} Old New` puts `New`
                // there. Queue it so the session reads it as its next line.
                let take = remaining as usize;
                buffer.extend_from_slice(&line[..take]);
                let tail = line[take..].to_vec();
                if !tail.is_empty() {
                    self.pending.push_front(tail);
                }
            }
        }
        buffer.truncate(declared as usize);
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_states_have_wire_names() {
        assert_eq!(SessionState::NotAuthenticated.as_str(), "not-authenticated");
        assert_eq!(SessionState::Authenticated.as_str(), "authenticated");
        assert_eq!(SessionState::Selected.as_str(), "selected");
        assert_eq!(SessionState::Logout.as_str(), "logout");
    }

    #[tokio::test]
    async fn script_input_and_vec_output_work() {
        let mut input = ScriptInput::new(["a NOOP"]);
        let mut out = VecOutput::new();
        out.send(&Response::tagged_ok("a", "done", None));
        assert_eq!(out.lines(), vec!["a OK done"]);
        assert_eq!(out.text(), "a OK done\n");
        out.clear();
        assert!(out.lines().is_empty());
        let line = input.read_line().await.unwrap();
        assert_eq!(line.as_deref(), Some(&b"a NOOP"[..]));
        let line = input.read_line().await.unwrap();
        assert!(line.is_none());
    }

    #[test]
    fn counting_output_counts() {
        let mut sink = VecOutput::new();
        {
            let mut counter = CountingOutput::new(&mut sink);
            counter.send(&Response::exists(1));
            counter.send(&Response::recent(1));
            assert_eq!(counter.count, 2);
        }
        assert_eq!(sink.lines().len(), 2);
    }

    #[test]
    fn split_address_splits_and_normalises() {
        let (local, domain) = split_address("Alice@Example.COM").unwrap();
        assert_eq!(local, "alice");
        assert_eq!(domain, "example.com");
        assert!(split_address("no-at-sign").is_none());
        assert!(split_address("@domain").is_none());
        assert!(split_address("local@").is_none());
    }

    #[tokio::test]
    async fn map_authenticator_accepts_and_rejects() {
        let auth = MapAuthenticator::single("Alice@Example.com", "hunter2", 7);
        let ok = auth.authenticate("alice@example.com", "hunter2").await;
        assert_eq!(ok.unwrap(), Some(UserId::new(7)));
        let bad = auth.authenticate("alice@example.com", "wrong").await;
        assert_eq!(bad.unwrap(), None);
        let unknown = auth.authenticate("bob@example.com", "x").await;
        assert_eq!(unknown.unwrap(), None);
    }

    #[test]
    fn describe_error_flattens_newlines() {
        let err = FerromaError::Parse("bad\r\nthing".into());
        assert!(!describe_error(&err).contains('\n'));
    }

    #[test]
    fn error_response_maps_each_error_class() {
        let (response, result) = error_response("a", &FerromaError::Parse("x".into()));
        assert_eq!(result, "BAD");
        assert!(response.to_wire().starts_with("a BAD"));

        let (_response, result) = error_response("a", &FerromaError::Protocol("x".into()));
        assert_eq!(result, "BAD");

        let (_, result) = error_response("a", &FerromaError::Unauthorized("x".into()));
        assert_eq!(result, "NO");
        let (response, _) = error_response("a", &FerromaError::Unauthorized("x".into()));
        assert!(!response.to_wire().contains("["));

        let (response, _) = error_response("a", &FerromaError::NotFound("x".into()));
        assert!(response.to_wire().contains("[NONEXISTENT]"));

        let (response, _) = error_response("a", &FerromaError::LimitExceeded("x".into()));
        assert!(response.to_wire().contains("[OVERQUOTA]"));

        let (response, _) = error_response("a", &FerromaError::Conflict("x".into()));
        assert!(response.to_wire().contains("[ALREADYEXISTS]"));

        let (response, _) = error_response("a", &FerromaError::Unsupported("x".into()));
        assert!(response.to_wire().contains("[CANNOT]"));

        let (response, _) = error_response("a", &FerromaError::Internal("x".into()));
        assert!(response.to_wire().starts_with("a NO"));
    }

    #[test]
    fn flags_to_column_is_space_separated_and_lowercase() {
        let flags = Flags::parse("\\Seen \\Flagged $Label1");
        assert_eq!(flags_to_column(&flags), "seen flagged $label1");
        assert_eq!(flags_to_column(&Flags::new()), "");
    }

    #[test]
    fn apply_store_replaces_adds_and_removes() {
        let incoming = Flags::parse("\\Seen \\Flagged");
        assert_eq!(apply_store("deleted", &incoming, StoreAction::Replace), "seen flagged");
        assert_eq!(
            apply_store("deleted", &incoming, StoreAction::Add),
            "seen flagged deleted"
        );
        assert_eq!(
            apply_store("seen flagged deleted", &incoming, StoreAction::Remove),
            "deleted"
        );
        // Removing something that was never set changes nothing.
        assert_eq!(
            apply_store("seen", &Flags::parse("\\Draft"), StoreAction::Remove),
            "seen"
        );
    }

    #[test]
    fn apply_store_handles_keywords() {
        let incoming = Flags::parse("$Junk");
        assert_eq!(apply_store("seen", &incoming, StoreAction::Add), "seen $junk");
        assert_eq!(apply_store("seen $junk", &incoming, StoreAction::Remove), "seen");
        assert_eq!(apply_store("seen", &incoming, StoreAction::Replace), "$junk");
    }

    #[test]
    fn apply_store_round_trips_through_the_column_format() {
        for before in ["", "seen", "seen flagged", "deleted $label1"] {
            let incoming = Flags::parse("\\Answered");
            let after = apply_store(before, &incoming, StoreAction::Add);
            assert!(parse_flags(&after).answered());
            // The result must itself be readable, which is the property that
            // matters for a reconnect.
            assert_eq!(flags_to_column(&parse_flags(&after)), after);
        }
    }

    #[test]
    fn session_flags_are_the_five_settable_system_flags() {
        let flags = session_flags();
        assert_eq!(flags.len(), 5);
        assert!(flags.contains(&"\\Seen".to_string()));
        assert!(!flags.contains(&"\\Recent".to_string()));
    }

    #[test]
    fn permanent_flags_include_the_keyword_wildcard() {
        let flags = permanent_flags();
        assert!(flags.contains(&"\\*".to_string()));
        assert!(flags.contains(&"\\Deleted".to_string()));
    }

    #[test]
    fn special_use_for_the_standard_folders() {
        assert_eq!(special_use_for("Sent"), Some("\\Sent"));
        assert_eq!(special_use_for("sent"), Some("\\Sent"));
        assert_eq!(special_use_for("Drafts"), Some("\\Drafts"));
        assert_eq!(special_use_for("Trash"), Some("\\Trash"));
        assert_eq!(special_use_for("Junk"), Some("\\Junk"));
        assert_eq!(special_use_for("Archive"), Some("\\Archive"));
        assert_eq!(special_use_for("INBOX"), None);
        assert_eq!(special_use_for("Custom"), None);
    }

    #[test]
    fn compress_set_finds_runs() {
        assert_eq!(compress_set(&[]), "");
        assert_eq!(compress_set(&[1]), "1");
        assert_eq!(compress_set(&[1, 2, 3]), "1:3");
        assert_eq!(compress_set(&[1, 3, 4, 5, 9]), "1,3:5,9");
        assert_eq!(compress_set(&[5, 4, 3, 3]), "3:5");
    }

    #[test]
    fn is_recent_reads_the_maildir_subdirectory() {
        assert!(is_recent("example.com/alice/Maildir/new/1.msg"));
        assert!(!is_recent("example.com/alice/Maildir/cur/1:2,S"));
        assert!(!is_recent("new"));
        assert!(!is_recent(""));
        assert!(is_recent("example.com/alice/Maildir/.Archive/new/1"));
    }

    #[test]
    fn idle_response_only_reports_the_selected_mailbox() {
        let scope_mailbox = MailboxId::new(4);
        let other = MailboxId::new(9);
        let received = Event::mail_received(scope_mailbox, MessageId::new(1));
        assert!(idle_response(&received, Some(scope_mailbox)).is_some());
        assert!(idle_response(&received, Some(other)).is_none());
        assert!(idle_response(&received, None).is_none());

        let flags = Event::mail_flag_changed(scope_mailbox, MessageId::new(1), "seen");
        let response = idle_response(&flags, Some(scope_mailbox)).expect("must report");
        assert!(response.to_wire().contains("FLAGS (seen)"));

        let deleted = Event::mail_deleted(scope_mailbox, MessageId::new(1), true);
        assert!(idle_response(&deleted, Some(other)).is_none());
        assert!(idle_response(&deleted, Some(scope_mailbox)).is_some());
    }

    #[test]
    fn idle_response_ignores_unrelated_events() {
        let event = Event::device_revoked(
            ferroma_core::DeviceId::new(1),
            UserId::new(2),
        );
        assert!(idle_response(&event, Some(MailboxId::new(1))).is_none());
    }

    #[test]
    fn session_config_capabilities_reflect_tls_and_features() {
        let plain = SessionConfig {
            banner: "b".into(),
            tls: false,
            require_tls_for_login: false,
            enable_idle: true,
            enable_move: true,
            max_append_size: 1024,
            max_literal_size: 1024,
            max_idle_secs: 60,
            starttls_available: true,
        };
        let caps = plain.capabilities();
        assert!(caps.contains(&"IMAP4rev1".to_string()));
        assert!(caps.contains(&"STARTTLS".to_string()));
        assert!(caps.contains(&"IDLE".to_string()));
        assert!(caps.contains(&"MOVE".to_string()));
        assert!(caps.contains(&"UIDPLUS".to_string()));
        assert!(caps.contains(&"UNSELECT".to_string()));
        assert!(caps.contains(&"NAMESPACE".to_string()));
        assert!(caps.contains(&"LITERAL+".to_string()));
        assert!(caps.contains(&"CHILDREN".to_string()));
        assert!(caps.contains(&"AUTH=PLAIN".to_string()));
        assert!(!caps.contains(&"LOGINDISABLED".to_string()));

        let secure = SessionConfig {
            tls: true,
            ..plain.clone()
        };
        let caps = secure.capabilities();
        assert!(!caps.contains(&"STARTTLS".to_string()));

        let strict = SessionConfig {
            tls: false,
            require_tls_for_login: true,
            starttls_available: false,
            ..plain.clone()
        };
        let caps = strict.capabilities();
        assert!(caps.contains(&"LOGINDISABLED".to_string()));
        assert!(!caps.contains(&"STARTTLS".to_string()));

        let no_extras = SessionConfig {
            enable_idle: false,
            enable_move: false,
            ..plain
        };
        let caps = no_extras.capabilities();
        assert!(!caps.contains(&"IDLE".to_string()));
        assert!(!caps.contains(&"MOVE".to_string()));
    }

    #[test]
    fn session_config_from_server_uses_the_configured_sizes() {
        let config = ImapServerConfig {
            banner: "hello".into(),
            max_append_size: 4096,
            max_idle_secs: 90,
            ..ImapServerConfig::default()
        };
        let session = SessionConfig::from_server(&config, false, true);
        assert_eq!(session.banner, "hello");
        assert_eq!(session.max_append_size, 4096);
        assert_eq!(session.max_idle_secs, 90);
        // The literal ceiling is at least the parser default, so a mailbox name
        // literal still fits even with a tiny APPEND limit.
        assert!(session.max_literal_size >= crate::parser::DEFAULT_MAX_LITERAL);

    }
}
