//! The SMTP listener and the per-connection session loop.
//!
//! ```text
//!   TcpListener ─► ConnectionLimiter ─► 220 banner ─► read → parse → dispatch → reply
//!                        │                                    │
//!                        │                                    ▼
//!                        │                        session guards (session.rs)
//!                        │                                    │
//!                        ▼                                    ▼
//!                 ConnectionPermit                    delivery.rs / repos
//! ```
//!
//! # What the listener guarantees
//!
//! * **Never an open relay.** An unauthenticated peer may only `RCPT` an address in
//!   a domain this server hosts. Everything else is `550 5.7.1 Relaying denied`,
//!   decided *before* `DATA`, so a rejected relay never costs us a message body.
//! * **`EHLO` advertises only what is enabled.** `AUTH` appears only when an auth
//!   service exists and the TLS policy allows it; `STARTTLS` only when a TLS acceptor
//!   exists and TLS is not already active.
//! * **`STARTTLS` resets the session** (RFC 3207 §4.2). The greeting, the
//!   authentication and any half-built envelope are discarded, the peer must `EHLO`
//!   again, and any bytes it sent before the handshake abort the upgrade — that is
//!   the request-smuggling primitive RFC 3207 §6 warns about.
//! * **Every command is bounded** by a timeout, and the whole `DATA` phase by
//!   another. A peer that connects and says nothing costs one permit for
//!   `command_timeout`, not a task forever.
//! * **Structured logs only.** `connection_id`, `remote_ip`, `helo`,
//!   `authenticated_user`, `sender`, `recipient`, `message_id`, `result`,
//!   `duration` — never a body, a password or a token.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferroma_core::config::Config;
use ferroma_core::{FerromaError, UserId};
use ferroma_events::EventBus;
use ferroma_mail::ParsedMessage;
use ferroma_storage::Repositories;
use futures_util::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use crate::connection::ConnectionLimiter;
use crate::inbound::{InboundVerdict, PolicyAction};
use crate::mx::Resolver;
use crate::delivery::{DeliveryService, ReceivedMessage, RecipientOutcome};
use crate::parser::{self, BodyType, Command, SmtpError};
use crate::reply::Reply;
use crate::session::{AuthDenied, AuthState, MailFromDenied, RcptDenied, SmtpSession};

/// The byte-exact `AUTH LOGIN` username challenge (`base64("Username:")`).
pub const AUTH_LOGIN_USERNAME_CHALLENGE: &str = "VXNlcm5hbWU6";
/// The byte-exact `AUTH LOGIN` password challenge (`base64("Password:")`).
pub const AUTH_LOGIN_PASSWORD_CHALLENGE: &str = "UGFzc3dvcmQ6";

/// Which listener a connection arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    /// Port 25 — inbound mail from other servers.
    Mx,
    /// Port 587 — authenticated submission.
    Submission,
    /// Port 465 — implicit TLS.
    Smtps,
}

impl ListenerKind {
    /// A short name for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ListenerKind::Mx => "mx",
            ListenerKind::Submission => "submission",
            ListenerKind::Smtps => "smtps",
        }
    }

    /// Whether this listener exists to accept mail *for* this server (as opposed to
    /// accepting mail *from* a user).
    pub fn is_inbound(self) -> bool {
        matches!(self, ListenerKind::Mx)
    }
}

/// Everything the server needs that is not in [`Config`].
///
/// The repositories, Maildir and attachment store are handed in through
/// [`DeliveryService`] rather than built here, so the whole process shares one pool
/// and one content-addressed blob store.
#[derive(Clone)]
pub struct SmtpServerConfig {
    /// The full configuration tree.
    pub config: Config,
    /// Storage handles, for recipient resolution and the daily send limit.
    pub repos: Repositories,
    /// Where accepted messages go.
    pub delivery: Arc<DeliveryService>,
    /// The password verifier for `AUTH`. `None` disables `AUTH` entirely.
    pub auth: Option<Arc<ferroma_auth::AuthService>>,
    /// TLS for `STARTTLS` and implicit TLS. `None` disables both.
    pub tls: Option<TlsAcceptor>,
    /// Where lifecycle events are published.
    pub events: Option<EventBus>,
    /// The DNS resolver for the inbound SPF/DKIM/DMARC policy step.
    ///
    /// `None` disables that step entirely: without a resolver there is nothing to
    /// evaluate against, and synthesising one from `[dns]` at boot can fail on a host
    /// with no usable `/etc/resolv.conf`. Pass the same `Arc<MxResolver>` the queue
    /// uses, so the DNS cache is shared between inbound policy and outbound delivery.
    pub resolver: Option<Arc<dyn Resolver>>,
}

impl std::fmt::Debug for SmtpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpServerConfig")
            .field("hostname", &self.config.server.hostname)
            .field("port", &self.config.smtp.port)
            .field("tls", &self.tls.is_some())
            .field("auth", &self.auth.is_some())
            .field("resolver", &self.resolver.is_some())
            .finish_non_exhaustive()
    }
}

impl SmtpServerConfig {
    /// Build from a configuration tree, a repository handle and a delivery service.
    ///
    /// The inbound policy step is off until [`SmtpServerConfig::with_resolver`] supplies
    /// a resolver.
    pub fn new(config: Config, repos: Repositories, delivery: Arc<DeliveryService>) -> Self {
        SmtpServerConfig {
            config,
            repos,
            delivery,
            auth: None,
            tls: None,
            events: None,
            resolver: None,
        }
    }

    /// Attach the authentication service, enabling `AUTH`.
    pub fn with_auth(mut self, auth: Arc<ferroma_auth::AuthService>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Attach a TLS acceptor, enabling `STARTTLS` and implicit TLS.
    pub fn with_tls(mut self, acceptor: TlsAcceptor) -> Self {
        self.tls = Some(acceptor);
        self
    }

    /// Attach an event bus.
    pub fn with_events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    /// Attach the DNS resolver, enabling inbound SPF/DKIM/DMARC evaluation and the
    /// `Authentication-Results` header.
    pub fn with_resolver(mut self, resolver: Arc<dyn Resolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// The hostname this server identifies itself as.
    pub fn hostname(&self) -> &str {
        &self.config.server.hostname
    }
}

/// One listening socket, with the role it plays.
pub struct SmtpListener {
    listener: TcpListener,
    kind: ListenerKind,
    implicit_tls: bool,
}

impl std::fmt::Debug for SmtpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpListener")
            .field("kind", &self.kind)
            .field("implicit_tls", &self.implicit_tls)
            .field("local_addr", &self.listener.local_addr().ok())
            .finish()
    }
}

impl SmtpListener {
    /// Bind one address.
    pub async fn bind(
        address: SocketAddr,
        kind: ListenerKind,
        implicit_tls: bool,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        Ok(SmtpListener {
            listener,
            kind,
            implicit_tls,
        })
    }

    /// The address actually bound — useful when the port was `0`.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Which listener this is.
    pub fn kind(&self) -> ListenerKind {
        self.kind
    }

    /// Whether every connection is upgraded to TLS before the banner.
    pub fn implicit_tls(&self) -> bool {
        self.implicit_tls
    }

    /// Stop listening. In-flight sessions keep running until they finish.
    pub fn close(&self) {
        // Dropping the `TcpListener` is what releases the port; there is nothing to
        // do here beyond documenting that fact for callers that hold one directly.
    }
}

/// A handle to a running server.
pub struct SmtpServerHandle {
    shutdown: watch::Sender<bool>,
    listener_tasks: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    local_addrs: Vec<SocketAddr>,
    connection_limiter: ConnectionLimiter,
    hostname: String,
}

impl std::fmt::Debug for SmtpServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpServerHandle")
            .field("local_addrs", &self.local_addrs)
            .field("hostname", &self.hostname)
            .finish_non_exhaustive()
    }
}

impl SmtpServerHandle {
    /// The addresses the server is listening on.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    /// How many connections are open right now — what `/health` reports.
    pub fn active_connections(&self) -> usize {
        self.connection_limiter.active_connections()
    }

    /// The limiter, for the health endpoint and for tests.
    pub fn connection_limiter(&self) -> &ConnectionLimiter {
        &self.connection_limiter
    }

    /// The hostname in the banner.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Ask the accept loops to stop. In-flight sessions finish their current command.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Stop accept loops and wait until every listening socket has been released.
    ///
    /// Connection tasks are intentionally not awaited: a transaction that is already
    /// receiving `DATA` is allowed to finish, while idle command reads see the same
    /// shutdown signal and close promptly.
    pub async fn shutdown_and_wait(&self) {
        self.shutdown();
        let tasks = self
            .listener_tasks
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default();
        for task in tasks {
            let _ = task.await;
        }
    }

    /// A receiver that resolves when shutdown was requested.
    ///
    /// # Call this before you spawn the task that awaits it
    ///
    /// The receiver is **moved** into whatever waits on it — `shutdown.changed().await`
    /// takes `&mut self` — so calling this *after* a `tokio::spawn` that already
    /// captured its own receiver either fails to compile or leaves the new receiver
    /// waiting on a signal that has already fired. Take it first:
    ///
    /// ```no_run
    /// # async fn demo(handle: ferroma_smtp::SmtpServerHandle) {
    /// let mut shutdown = handle.subscribe_shutdown();
    /// tokio::spawn(async move {
    ///     let _ = shutdown.changed().await;
    /// });
    /// # }
    /// ```
    ///
    /// A receiver created *after* [`SmtpServerHandle::shutdown`] still resolves
    /// immediately, because `watch` remembers the last value — so the ordering hazard
    /// is about *which* receiver the waiter holds, not about missing the signal.
    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }
}

/// The SMTP server.
#[derive(Debug)]
pub struct SmtpServer {
    config: SmtpServerConfig,
    limiter: ConnectionLimiter,
}

impl SmtpServer {
    /// Build a server from its configuration.
    pub fn new(config: SmtpServerConfig) -> Self {
        let limiter = ConnectionLimiter::new(&config.config.limits);
        SmtpServer { config, limiter }
    }

    /// The connection limiter this server enforces.
    pub fn limiter(&self) -> &ConnectionLimiter {
        &self.limiter
    }

    /// The configuration this server was built with.
    pub fn config(&self) -> &SmtpServerConfig {
        &self.config
    }

    /// Bind the ports the configuration enables.
    ///
    /// Follows `[smtp]`: `port`, then `submission_port`, then `smtps_port` when it is
    /// not `0` and `[tls]` is configured. A port of `0` means "listener disabled".
    pub async fn bind(&self) -> Result<Vec<SmtpListener>, FerromaError> {
        let smtp = &self.config.config.smtp;
        let mut listeners = Vec::new();

        for (port, kind, implicit) in [
            (smtp.port, ListenerKind::Mx, false),
            (smtp.submission_port, ListenerKind::Submission, false),
            (smtp.smtps_port, ListenerKind::Smtps, true),
        ] {
            if port == 0 {
                continue;
            }
            let address = listener_address(&smtp.host, port)?;
            listeners.push(self.bind_one(address, kind, implicit).await?);
        }

        if listeners.is_empty() {
            return Err(FerromaError::Config(
                "no SMTP listener is enabled: check [smtp] port/submission_port/smtps_port".into(),
            ));
        }
        Ok(listeners)
    }

    /// Bind one listener explicitly.
    ///
    /// This is what tests use (with port `0`, so the OS picks a free port) and what
    /// the `ferroma` binary uses for an operator-configured extra listener.
    pub async fn bind_one(
        &self,
        address: SocketAddr,
        kind: ListenerKind,
        implicit_tls: bool,
    ) -> Result<SmtpListener, FerromaError> {
        if implicit_tls && self.config.tls.is_none() {
            return Err(FerromaError::Config(
                "an implicit-TLS listener needs [tls] to be configured".into(),
            ));
        }
        SmtpListener::bind(address, kind, implicit_tls)
            .await
            .map_err(|e| FerromaError::Config(format!("cannot bind SMTP listener on {address}: {e}")))
    }

    /// Bind the configured ports and start serving, returning a shutdown handle.
    pub async fn start(self) -> Result<SmtpServerHandle, FerromaError> {
        let listeners = self.bind().await?;
        Ok(self.start_with(listeners))
    }

    /// Serve already-bound listeners, returning a shutdown handle.
    ///
    /// Splitting "bind" from "serve" is what lets a caller learn the ephemeral port
    /// before the accept loops start — and what makes the integration tests
    /// deterministic.
    pub fn start_with(self, listeners: Vec<SmtpListener>) -> SmtpServerHandle {
        let local_addrs: Vec<SocketAddr> = listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let listener_tasks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handle = SmtpServerHandle {
            shutdown: shutdown_tx,
            listener_tasks: std::sync::Arc::clone(&listener_tasks),
            local_addrs,
            connection_limiter: self.limiter.clone(),
            hostname: self.config.hostname().to_string(),
        };

        let context = Arc::new(SessionContext::new(
            Arc::new(self.config),
            self.limiter,
        ));
        let mut tasks = Vec::new();
        for listener in listeners {
            let context = Arc::clone(&context);
            let mut shutdown = shutdown_rx.clone();
            tasks.push(tokio::spawn(async move {
                run_listener(listener, context, &mut shutdown).await;
            }));
        }
        if let Ok(mut guard) = listener_tasks.lock() {
            guard.extend(tasks);
        }
        handle
    }

    /// Bind, serve, and wait for shutdown — the "run this server" entry point.
    pub async fn serve(self) -> Result<(), FerromaError> {
        let handle = self.start().await?;
        let mut shutdown = handle.subscribe_shutdown();
        let _ = shutdown.changed().await;
        Ok(())
    }
}

/// Build a TLS acceptor for `STARTTLS` and implicit TLS from PEM material.
///
/// `cert_pem` is the leaf certificate followed by any intermediates; `key_pem` is a
/// PKCS#8, PKCS#1 or SEC1 private key. `min_version` is `[tls] min_version` — `"1.2"` or
/// `"1.3"`, anything else being treated as `"1.2"`.
///
/// This lives in the SMTP crate rather than in the binary because this is the crate that
/// hands the acceptor to `tokio-rustls`; a second copy elsewhere would be a second place
/// for the minimum version to drift.
pub fn tls_acceptor(
    cert_pem: &[u8],
    key_pem: &[u8],
    min_version: &str,
) -> Result<TlsAcceptor, FerromaError> {
    let mut cert_reader = std::io::BufReader::new(cert_pem);
    let certs: Vec<rustls_pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| FerromaError::Tls(format!("cannot read the certificate chain: {e}")))?;
    if certs.is_empty() {
        return Err(FerromaError::Tls(
            "no certificate found in the PEM bundle".into(),
        ));
    }

    let mut key_reader = std::io::BufReader::new(key_pem);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| FerromaError::Tls(format!("cannot read the private key: {e}")))?
        .ok_or_else(|| FerromaError::Tls("no private key found in the PEM file".into()))?;

    // An operator on `min_version = "1.3"` must not silently accept a 1.2 client, so the
    // version list is narrowed at construction — `ServerConfig::versions` is not
    // settable after the fact.
    let versions: &[&'static tokio_rustls::rustls::SupportedProtocolVersion] =
        if min_version == "1.3" {
            &[&tokio_rustls::rustls::version::TLS13]
        } else {
            &[
                &tokio_rustls::rustls::version::TLS13,
                &tokio_rustls::rustls::version::TLS12,
            ]
        };

    let config = tokio_rustls::rustls::ServerConfig::builder_with_protocol_versions(versions)
        .with_no_client_auth()
        // `with_single_cert` checks that the key matches the leaf certificate, so a
        // mismatched pair fails here rather than on the first handshake.
        .with_single_cert(certs, key)
        .map_err(|e| FerromaError::Tls(format!("the certificate and key do not match: {e}")))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Load a TLS acceptor from a certificate chain file and a private key file.
pub fn tls_acceptor_from_files(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
    min_version: &str,
) -> Result<TlsAcceptor, FerromaError> {
    let cert_pem = std::fs::read(cert_path).map_err(|e| {
        FerromaError::Tls(format!(
            "cannot read the certificate at {}: {e}",
            cert_path.display()
        ))
    })?;
    let key_pem = std::fs::read(key_path).map_err(|e| {
        FerromaError::Tls(format!(
            "cannot read the private key at {}: {e}",
            key_path.display()
        ))
    })?;
    // The key's *contents* are never logged, here or anywhere else.
    tls_acceptor(&cert_pem, &key_pem, min_version)
}

/// `host:port` for one listener.
fn listener_address(host: &str, port: u16) -> Result<SocketAddr, FerromaError> {
    if host == "localhost" {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    let ip = host
        .parse::<std::net::IpAddr>()
        .map_err(|_| FerromaError::Config(format!("smtp.host {host:?} is not an IP address")))?;
    Ok(SocketAddr::new(ip, port))
}

/// Accept connections until shutdown.
async fn run_listener(
    listener: SmtpListener,
    context: Arc<SessionContext>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let kind = listener.kind;
    let implicit_tls = listener.implicit_tls;
    let Ok(address) = listener.listener.local_addr() else {
        tracing::error!(listener = kind.as_str(), "listener has no local address");
        return;
    };
    tracing::info!(
        listener = kind.as_str(),
        address = %address,
        tls = implicit_tls,
        "SMTP listener started"
    );

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tracing::info!(listener = kind.as_str(), "SMTP listener stopping");
                    return;
                }
            }
            accepted = listener.listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let context = Arc::clone(&context);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, peer, kind, implicit_tls, context, shutdown).await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(listener = kind.as_str(), error = %e, "accept failed");
                        // A transient accept failure (EMFILE, ECONNABORTED) must not
                        // spin this loop; give the OS a moment to recover.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
}

/// Everything a session needs that outlives one connection.
struct SessionContext {
    config: Arc<SmtpServerConfig>,
    limiter: ConnectionLimiter,
    /// The inbound SPF/DKIM/DMARC evaluator, when a resolver was supplied.
    ///
    /// Built once here rather than per connection, so its resolver cache is shared by
    /// every session.
    policy: Option<crate::inbound::InboundPolicy>,
}

impl SessionContext {
    /// Build the shared context for one server.
    fn new(config: Arc<SmtpServerConfig>, limiter: ConnectionLimiter) -> Self {
        let policy = config.resolver.as_ref().map(|resolver| {
            crate::inbound::InboundPolicy::new(&config.config, Arc::clone(resolver))
        });
        SessionContext {
            config,
            limiter,
            policy,
        }
    }

    fn hostname(&self) -> &str {
        &self.config.config.server.hostname
    }

    fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.config.config.smtp.command_timeout_secs.max(1))
    }

    fn data_timeout(&self) -> Duration {
        Duration::from_secs(self.config.config.smtp.data_timeout_secs.max(1))
    }

    fn max_message_size(&self) -> u64 {
        self.config.config.limits.max_message_size
    }
}

/// Admit one connection, then run its session (upgrading to TLS when asked).
async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    kind: ListenerKind,
    implicit_tls: bool,
    context: Arc<SessionContext>,
    mut shutdown: watch::Receiver<bool>,
) {
    let connection_id = new_connection_id();
    let started = Instant::now();

    let _permit = match context.limiter.try_acquire(peer.ip()) {
        Ok(permit) => permit,
        Err(limited) => {
            tracing::warn!(
                connection_id = %connection_id,
                remote_ip = %peer.ip(),
                limit = limited.kind.as_str(),
                result = "refused",
                "connection refused by the limiter"
            );
            let mut stream = stream;
            let _ = tokio::io::AsyncWriteExt::write_all(
                &mut stream,
                &Reply::too_many_connections().render(),
            )
            .await;
            let _ = tokio::io::AsyncWriteExt::flush(&mut stream).await;
            return;
        }
    };

    let _ = stream.set_nodelay(true);

    let outcome = if implicit_tls {
        match context.config.tls.as_ref() {
            Some(acceptor) => match acceptor.accept(stream).await {
                Ok(tls) => {
                    let mut io = SessionIo::new(SharedStream::from_tls(tls));
                    // An implicit-TLS connection is a *new* connection, so it gets the
                    // banner before anything else.
                    if greet(&mut io, &context, &connection_id).await.is_err() {
                        return finish(connection_id, peer, kind, started, SessionResult::Disconnected);
                    }
                    let mut session = SmtpSession::new(connection_id.clone(), peer);
                    session.tls = true;
                    run_session(&mut io, kind, &mut session, &context, &connection_id, &mut shutdown).await
                }
                Err(e) => {
                    tracing::info!(
                        connection_id = %connection_id,
                        remote_ip = %peer.ip(),
                        error = %e,
                        result = "tls_failed",
                        "implicit TLS handshake failed"
                    );
                    SessionResult::TlsFailed
                }
            },
            None => {
                let mut io = SessionIo::new(SharedStream::from_tcp(stream));
                let _ = write_reply(&mut io, &Reply::tls_unavailable()).await;
                SessionResult::TlsFailed
            }
        }
    } else {
        let mut io = SessionIo::new(SharedStream::from_tcp(stream));
        if greet(&mut io, &context, &connection_id).await.is_err() {
            return finish(connection_id, peer, kind, started, SessionResult::Disconnected);
        }
        let mut session = SmtpSession::new(connection_id.clone(), peer)
            .with_submission(kind == ListenerKind::Submission);
        let mut outcome = run_session(&mut io, kind, &mut session, &context, &connection_id, &mut shutdown).await;

        // STARTTLS: hand the socket to the acceptor, then continue as a *new*
        // session. `reset_for_starttls` has already discarded everything the peer
        // said in the clear.
        if matches!(outcome, SessionResult::Upgrade) {
            let Some(acceptor) = context.config.tls.clone() else {
                return finish(connection_id, peer, kind, started, SessionResult::Internal);
            };
            match io.into_inner() {
                Ok(stream) => {
                    let plain: TcpStream = match stream.into_plain() {
                        Some(plain) => plain,
                        None => {
                            tracing::warn!(
                                connection_id = %connection_id,
                                "STARTTLS is only supported on a plaintext connection"
                            );
                            return finish(
                                connection_id,
                                peer,
                                kind,
                                started,
                                SessionResult::TlsFailed,
                            );
                        }
                    };
                    match acceptor.accept(plain).await {
                        Ok(tls) => {
                            tracing::info!(
                                connection_id = %connection_id,
                                remote_ip = %peer.ip(),
                                result = "starttls",
                                "STARTTLS completed"
                            );
                            let mut upgraded = SmtpSession::new(connection_id.clone(), peer)
                                .with_submission(kind == ListenerKind::Submission);
                            upgraded.tls = true;
                            // RFC 3207 §4.2: no second `220`. The connection is already
                            // open; the upgrade is not a new one. The peer's next command
                            // is the `EHLO` the RFC requires it to send.
                            let mut io = SessionIo::new(SharedStream::from_tls(tls));
                            outcome = run_session(&mut io, kind, &mut upgraded, &context, &connection_id, &mut shutdown)
                                .await;
                        }
                        Err(e) => {
                            tracing::info!(
                                connection_id = %connection_id,
                                remote_ip = %peer.ip(),
                                error = %e,
                                result = "tls_failed",
                                "STARTTLS handshake failed"
                            );
                            outcome = SessionResult::TlsFailed;
                        }
                    }
                }
                Err(e) => {
                    // The peer pipelined bytes before the handshake. RFC 3207 §6:
                    // abort rather than upgrade.
                    tracing::warn!(
                        connection_id = %connection_id,
                        remote_ip = %peer.ip(),
                        error = %e,
                        result = "tls_aborted",
                        "STARTTLS aborted: buffered data before the handshake"
                    );
                    outcome = SessionResult::TlsFailed;
                }
            }
        }
        outcome
    };

    finish(connection_id, peer, kind, started, outcome);
}

/// One structured log line for the end of a session.
fn finish(
    connection_id: String,
    peer: SocketAddr,
    kind: ListenerKind,
    started: Instant,
    outcome: SessionResult,
) {
    tracing::info!(
        connection_id = %connection_id,
        remote_ip = %peer.ip(),
        listener = kind.as_str(),
        result = outcome.as_str(),
        duration = ?started.elapsed(),
        "SMTP session finished"
    );
}

/// How a session ended, for the log line and for the connection wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionResult {
    /// The peer said `QUIT`.
    Quit,
    /// The peer went away without `QUIT`.
    Disconnected,
    /// No command arrived within the timeout.
    Timeout,
    /// A TLS handshake failed.
    TlsFailed,
    /// The peer sent a line longer than any command may be.
    LineTooLong,
    /// An administrator disabled the listener while this session was idle.
    Disabled,
    /// The peer asked for `STARTTLS`; the caller must perform the handshake.
    Upgrade,
    /// An internal failure ended the session.
    Internal,
}

impl SessionResult {
    /// A short name for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionResult::Quit => "quit",
            SessionResult::Disconnected => "disconnected",
            SessionResult::Timeout => "timeout",
            SessionResult::TlsFailed => "tls_failed",
            SessionResult::LineTooLong => "line_too_long",
            SessionResult::Disabled => "disabled",
            SessionResult::Upgrade => "starttls",
            SessionResult::Internal => "internal",
        }
    }

    /// Whether the session ended because the peer asked for TLS.
    pub fn is_upgrade(self) -> bool {
        matches!(self, SessionResult::Upgrade)
    }
}

/// A unique id for one connection.
///
/// Random rather than sequential: two servers behind one load balancer must not
/// produce colliding ids in a shared log store, and an id that leaks no counter is
/// also one an attacker cannot use to count our traffic.
fn new_connection_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let value: u64 = rng.gen();
    format!("{value:016x}")
}

// ---------------------------------------------------------------------------
// Stream handling
// ---------------------------------------------------------------------------

/// The traits a connection stream must provide.
///
/// `futures_util` rather than `tokio::io`, because the session loop needs
/// [`futures_util::io::ReadHalf`]/[`futures_util::io::WriteHalf`] and
/// [`futures_util::io::BufReader`]. Every tokio stream is adapted at the two
/// construction sites ([`SharedStream::from_tcp`] and [`SharedStream::from_tls`]), so
/// the session loop sees exactly one shape of stream no matter how the connection was
/// secured.
pub trait AsyncReadWrite:
    futures_util::AsyncRead
    + futures_util::AsyncWrite
    + Send
    + Unpin
    + std::any::Any
    + std::fmt::Debug
{
    /// Narrow this stream back to the concrete kind it was built from.
    ///
    /// `None` means the connection is already encrypted, which `STARTTLS` treats as a
    /// protocol error rather than a silent downgrade.
    fn into_plain(self: Box<Self>) -> Option<TcpStream>;
}

impl<T> AsyncReadWrite for T where
    T: futures_util::AsyncRead
        + futures_util::AsyncWrite
        + Send
        + Unpin
        + std::any::Any
        + std::fmt::Debug
{
    fn into_plain(self: Box<Self>) -> Option<TcpStream> {
        let any: Box<dyn std::any::Any> = self;
        any.downcast::<TokioCompat<TcpStream>>()
            .ok()
            .map(|boxed| boxed.0)
    }
}

/// A `Debug` wrapper that adapts a *tokio* stream to the *futures* I/O traits.
///
/// The two trait sets are *not* identical — tokio reads into a `ReadBuf` and answers
/// `Poll<Result<(), _>>`, futures read into a `&mut [u8]` and answer the byte count —
/// so this is a real (if small) adapter rather than a delegation. `futures_util`'s own
/// `Compat` lives behind its `compat` feature, which this workspace does not enable.
pub struct TokioCompat<T>(pub T);

impl<T> std::fmt::Debug for TokioCompat<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokioCompat")
    }
}

impl<T: tokio::io::AsyncRead + Unpin> futures_util::AsyncRead for TokioCompat<T> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut read_buf = tokio::io::ReadBuf::new(buf);
        match std::pin::Pin::new(&mut self.get_mut().0).poll_read(cx, &mut read_buf) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(read_buf.filled().len())),
        }
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> futures_util::AsyncWrite for TokioCompat<T> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// One connection's byte stream, behind a mutex so it can be given back to the TLS
/// acceptor by `STARTTLS` and still be read and written through shared halves.
///
/// `futures_util`'s `ReadHalf`/`WriteHalf` take the stream by value; the mutex is what
/// lets the session hold both halves *and* hand the socket to a TLS acceptor later
/// without inventing a stream type for every combination of plaintext and TLS.
#[derive(Debug)]
pub struct SharedStream(Arc<tokio::sync::Mutex<BoxedStream>>);

/// A boxed connection stream: a TCP socket or a TLS session over one.
pub type BoxedStream = Box<dyn AsyncReadWrite>;

impl SharedStream {
    /// Wrap an already-adapted stream.
    pub fn new(stream: BoxedStream) -> Self {
        SharedStream(Arc::new(tokio::sync::Mutex::new(stream)))
    }

    /// Wrap a TCP socket.
    pub fn from_tcp(stream: TcpStream) -> Self {
        SharedStream::new(Box::new(TokioCompat(stream)))
    }

    /// Wrap a TLS session.
    pub fn from_tls(stream: tokio_rustls::server::TlsStream<TcpStream>) -> Self {
        SharedStream::new(Box::new(TokioCompat(stream)))
    }

    /// A second handle on the same stream.
    pub fn handle(&self) -> Self {
        SharedStream(Arc::clone(&self.0))
    }

    /// Take the stream out, for a TLS upgrade.
    ///
    /// Fails when the stream is not exclusively owned — i.e. when a read is still in
    /// flight, which would mean the session is mid-command.
    pub fn take(&self) -> Result<BoxedStream, FerromaError> {
        let mut guard = self
            .0
            .try_lock()
            .map_err(|_| FerromaError::Protocol("the connection is still busy".into()))?;
        let stream = std::mem::replace(&mut *guard, Box::new(TokioCompat(tokio::io::empty())));
        Ok(stream)
    }

    /// Take the stream out and narrow it to a `TcpStream`.
    ///
    /// This is what `STARTTLS` needs: an implicit-TLS connection can never be
    /// upgraded again, so only the plaintext case can produce the socket the acceptor
    /// wants, and anything else is a protocol error rather than a silent downgrade.
    pub fn compact(&self) -> Result<TcpStream, FerromaError> {
        let stream = self.take()?;
        match stream.into_plain() {
            Some(plain) => Ok(plain),
            None => Err(FerromaError::Protocol(
                "STARTTLS requires an unencrypted connection".into(),
            )),
        }
    }
}

impl futures_util::AsyncRead for SharedStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let inner = Arc::clone(&self.0);
        let mut stream = match inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
        };
        std::pin::Pin::new(&mut *stream).poll_read(cx, buf)
    }
}

impl futures_util::AsyncWrite for SharedStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let inner = Arc::clone(&self.0);
        let mut stream = match inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
        };
        std::pin::Pin::new(&mut *stream).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let inner = Arc::clone(&self.0);
        let mut stream = match inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
        };
        std::pin::Pin::new(&mut *stream).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let inner = Arc::clone(&self.0);
        let mut stream = match inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
        };
        std::pin::Pin::new(&mut *stream).poll_close(cx)
    }
}

/// A buffered reader and a writer over one connection.
pub struct SessionIo {
    reader: BufReader<futures_util::io::ReadHalf<SharedStream>>,
    writer: futures_util::io::WriteHalf<SharedStream>,
    /// The same stream, for the `STARTTLS` upgrade.
    stream: SharedStream,
}

impl SessionIo {
    /// Wrap a stream.
    pub fn new(stream: SharedStream) -> Self {
        let (read, write) = futures_util::io::AsyncReadExt::split(stream.handle());
        SessionIo {
            reader: BufReader::new(read),
            writer: write,
            stream,
        }
    }

    /// Read one line, up to `max` bytes, with a deadline.
    ///
    /// Returns `Ok(None)` on a clean end of stream. A line that reaches `max`
    /// without a newline is returned as-is, so the caller answers `500 Line too
    /// long` instead of buffering a peer's whole memory into ours.
    pub async fn read_line(
        &mut self,
        max: usize,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, FerromaError> {
        let mut buffer = Vec::with_capacity(256);
        match tokio::time::timeout(timeout, self.fill_line(&mut buffer, max)).await {
            Ok(Ok(())) => {
                if buffer.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(buffer))
                }
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(FerromaError::Timeout("no command arrived in time".into())),
        }
    }

    /// The read loop behind [`SessionIo::read_line`], split out so the whole thing
    /// can be wrapped in one timeout.
    ///
    /// `poll_fill_buf` is driven by hand rather than through
    /// `AsyncBufReadExt::fill_buf`, because the borrowed slice it returns cannot cross
    /// the `consume` that has to follow it. Copying one chunk at a time keeps the
    /// borrow local and the loop straightforward.
    async fn fill_line(&mut self, buffer: &mut Vec<u8>, max: usize) -> Result<(), FerromaError> {
        use futures_util::io::AsyncBufRead;
        loop {
            let mut chunk = [0u8; 4096];
            let read = std::future::poll_fn(|cx| {
                let available = std::task::ready!(
                    std::pin::Pin::new(&mut self.reader).poll_fill_buf(cx)
                )?;
                if available.is_empty() {
                    return std::task::Poll::Ready(Ok(0usize));
                }
                let newline = available.iter().position(|b| *b == b'\n');
                let take = match newline {
                    Some(index) => index + 1,
                    None => available.len(),
                }
                // Never read past the caller's cap: a peer that sends 4 KiB with no
                // newline must be answered `500 Line too long` after `max` bytes, not
                // buffered whole.
                .min(chunk.len())
                .min(max.saturating_sub(buffer.len()).max(1));
                chunk[..take].copy_from_slice(&available[..take]);
                std::task::Poll::Ready(Ok(take))
            })
            .await
            .map_err(FerromaError::Io)?;

            if read == 0 {
                return Ok(());
            }
            let complete = chunk[read - 1] == b'\n';
            std::pin::Pin::new(&mut self.reader).consume(read);
            buffer.extend_from_slice(&chunk[..read]);
            if complete || buffer.len() >= max {
                return Ok(());
            }
        }
    }

    /// Write bytes.
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), FerromaError> {
        use futures_util::io::AsyncWriteExt;
        self.writer
            .write_all(bytes)
            .await
            .map_err(FerromaError::Io)
    }

    /// Flush.
    pub async fn flush(&mut self) -> Result<(), FerromaError> {
        use futures_util::io::AsyncWriteExt;
        self.writer.flush().await.map_err(FerromaError::Io)
    }

    /// Whether bytes are still buffered from the peer.
    ///
    /// `STARTTLS` must fail when this is true: leftover plaintext read before the
    /// handshake is exactly the request-smuggling primitive RFC 3207 §6 warns about.
    pub fn has_buffered_input(&self) -> bool {
        !self.reader.buffer().is_empty()
    }

    /// Give the stream back so it can be handed to a TLS acceptor.
    ///
    /// Fails when the reader still holds buffered bytes.
    pub fn into_inner(self) -> Result<BoxedStream, FerromaError> {
        if self.has_buffered_input() {
            return Err(FerromaError::Protocol(
                "client sent data before the TLS handshake".into(),
            ));
        }
        let SessionIo { reader, writer, stream } = self;
        // Drop the halves so the session's own handle is the only one left.
        drop(reader);
        drop(writer);
        stream.take()
    }
}

// ---------------------------------------------------------------------------
// The session loop
// ---------------------------------------------------------------------------

/// Write the `220` banner.
///
/// Called once per *connection* — after an implicit-TLS handshake, or immediately on a
/// plaintext one. It is deliberately **not** part of [`run_session`]: RFC 3207 §4.2 says
/// a `STARTTLS` upgrade continues the same connection, so re-entering the session loop
/// after the handshake must not greet the peer a second time. (A real MTA reads a
/// duplicate `220` as an out-of-order reply and hangs up.)
async fn greet(
    io: &mut SessionIo,
    context: &SessionContext,
    connection_id: &str,
) -> Result<(), FerromaError> {
    let smtp = &context.config.config.smtp;
    let banner = Reply::service_ready(context.hostname(), &smtp.banner);
    if let Err(e) = write_reply(io, &banner).await {
        tracing::debug!(connection_id = %connection_id, error = %e, "could not write the banner");
        return Err(e);
    }
    Ok(())
}

/// Run one SMTP session until the peer leaves, the connection breaks, or the peer
/// asks for `STARTTLS`.
async fn run_session(
    io: &mut SessionIo,
    kind: ListenerKind,
    session: &mut SmtpSession,
    context: &SessionContext,
    connection_id: &str,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionResult {
    let peer = session.remote_addr;
    let started = Instant::now();

    tracing::info!(
        connection_id = %connection_id,
        remote_ip = %peer.ip(),
        listener = kind.as_str(),
        result = "connected",
        "SMTP connection accepted"
    );

    loop {
        // A multi-step AUTH exchange owns the next line and answers with the SASL
        // wording rather than "unknown command".
        if session.in_auth() {
            match read_command_until(io, context.command_timeout(), shutdown).await {
                CommandWait::Input(Ok(Some(line))) => match handle_auth_response(io, session, &line, context).await {
                    Ok(()) => continue,
                    Err(e) => return internal(connection_id, peer, e),
                },
                CommandWait::Input(Ok(None)) => {
                tracing::info!(
                    connection_id = %connection_id,
                    remote_ip = %peer.ip(),
                    authenticated = session.is_authenticated(),
                    result = "disconnected",
                    "SMTP peer closed the connection"
                );
                return SessionResult::Disconnected;
            },
                CommandWait::Input(Err(e)) if is_timeout(&e) => {
                    let _ = write_reply(io, &Reply::timeout()).await;
                    return SessionResult::Timeout;
                }
                CommandWait::Input(Err(e)) => return internal(connection_id, peer, e),
                CommandWait::Disabled => {
                    let _ = write_reply(io, &Reply::service_disabled()).await;
                    return SessionResult::Disabled;
                }
            }
        }

        let line = match read_command_until(io, context.command_timeout(), shutdown).await {
            CommandWait::Input(Ok(Some(line))) => line,
            CommandWait::Input(Ok(None)) => {
                tracing::info!(
                    connection_id = %connection_id,
                    remote_ip = %peer.ip(),
                    authenticated = session.is_authenticated(),
                    result = "disconnected",
                    "SMTP peer closed the connection"
                );
                return SessionResult::Disconnected;
            },
            CommandWait::Input(Err(e)) if is_timeout(&e) => {
                let _ = write_reply(io, &Reply::timeout()).await;
                return SessionResult::Timeout;
            }
            CommandWait::Input(Err(e)) => return internal(connection_id, peer, e),
            CommandWait::Disabled => {
                let _ = write_reply(io, &Reply::service_disabled()).await;
                return SessionResult::Disabled;
            }
        };

        // Rate limit before doing any work for this command.
        if let Err(limited) = context.limiter.check_command_rate(peer.ip()) {
            tracing::warn!(
                connection_id = %connection_id,
                remote_ip = %peer.ip(),
                limit = limited.kind.as_str(),
                result = "rate_limited",
                "command rate limit reached"
            );
            let _ = write_reply(io, &Reply::rate_limited("Too many commands")).await;
            return SessionResult::Disconnected;
        }

        session.command_count += 1;

        let command = match parser::parse_command(&line) {
            Ok(command) => command,
            Err(error) => {
                if is_fatal_parse_error(&error) {
                    let _ = write_reply(io, &error.reply).await;
                    tracing::warn!(
                        connection_id = %connection_id,
                        remote_ip = %peer.ip(),
                        reason = error.kind,
                        result = "line_too_long",
                        "peer sent an over-long command; closing"
                    );
                    return SessionResult::LineTooLong;
                }
                session.abort_auth();
                if write_reply(io, &error.reply).await.is_err() {
                    return SessionResult::Disconnected;
                }
                tracing::debug!(
                    connection_id = %connection_id,
                    remote_ip = %peer.ip(),
                    reason = error.kind,
                    result = "rejected",
                    "command refused"
                );
                continue;
            }
        };

        let verb = command.verb();
        // A client that authenticates and then leaves without MAIL FROM produces no other
        // record of what it sent. The verb is enough; arguments can carry a password.
        if session.is_authenticated() {
            tracing::info!(
                connection_id = %connection_id,
                remote_ip = %peer.ip(),
                verb,
                "SMTP command after authentication"
            );
        }
        let reply_or_upgrade = dispatch(io, kind, session, &command, context, connection_id).await;

        match reply_or_upgrade {
            Dispatch::Reply(reply) => {
                if write_reply(io, &reply).await.is_err() {
                    return SessionResult::Disconnected;
                }
                tracing::debug!(connection_id = %connection_id, verb, result = "answered", "command handled");
            }
            Dispatch::Close(reply) => {
                let _ = write_reply(io, &reply).await;
                tracing::info!(
                    connection_id = %connection_id,
                    remote_ip = %peer.ip(),
                    helo = session.helo.as_deref().unwrap_or("-"),
                    authenticated_user = session.authenticated_as.as_deref().unwrap_or("-"),
                    result = "quit",
                    duration = ?started.elapsed(),
                    "SMTP session closed by the peer"
                );
                return SessionResult::Quit;
            }
            Dispatch::Data => {
                match handle_data(io, session, context, kind, connection_id).await {
                    Ok(Some(reply)) => {
                        if write_reply(io, &reply).await.is_err() {
                            return SessionResult::Disconnected;
                        }
                    }
                    Ok(None) => return SessionResult::Disconnected,
                    Err(e) => return internal(connection_id, peer, e),
                }
            }
            Dispatch::StartTls => {
                if write_reply(io, &Reply::ready_to_start_tls()).await.is_err() {
                    return SessionResult::Disconnected;
                }
                session.reset_for_starttls();
                tracing::info!(
                    connection_id = %connection_id,
                    remote_ip = %peer.ip(),
                    result = "starttls",
                    "STARTTLS accepted; session reset"
                );
                return SessionResult::Upgrade;
            }
            Dispatch::Auth => match handle_auth(io, session, &command, context).await {
                Ok(()) => {}
                Err(e) => return internal(connection_id, peer, e),
            },
        }
    }
}

/// What the dispatcher wants the loop to do next.
enum Dispatch {
    /// Answer and keep reading.
    Reply(Reply),
    /// Answer, then end the session.
    Close(Reply),
    /// Enter the `DATA` phase.
    Data,
    /// Answer, then upgrade the socket to TLS.
    StartTls,
    /// `AUTH` writes its own replies.
    Auth,
}

/// Route one parsed command.
async fn dispatch(
    _io: &mut SessionIo,
    kind: ListenerKind,
    session: &mut SmtpSession,
    command: &Command,
    context: &SessionContext,
    connection_id: &str,
) -> Dispatch {
    let smtp = &context.config.config.smtp;
    match command {
        Command::Ehlo(name) => {
            session.greet(name, true);
            session.advertised_size = if smtp.advertise_size {
                Some(context.max_message_size())
            } else {
                None
            };
            let extensions = ehlo_extensions(
                smtp,
                &context.config.config.limits,
                context.config.auth.is_some(),
                context.config.tls.is_some(),
                session.tls,
            );
            Dispatch::Reply(Reply::ehlo(context.hostname(), &extensions))
        }
        Command::Helo(name) => {
            session.greet(name, false);
            Dispatch::Reply(Reply::helo(context.hostname()))
        }
        Command::MailFrom(params) => Dispatch::Reply(
            handle_mail_from(session, params, context, kind).await,
        ),
        Command::RcptTo(params) => {
            Dispatch::Reply(handle_rcpt_to(session, params, context, connection_id).await)
        }
        Command::Data => Dispatch::Data,
        Command::Rset => {
            session.reset();
            Dispatch::Reply(Reply::ok())
        }
        Command::Noop(_) => Dispatch::Reply(Reply::ok()),
        Command::Vrfy(_) => Dispatch::Reply(Reply::cannot_vrfy()),
        Command::Help(arg) => {
            let text = if arg.is_empty() {
                "Commands: EHLO HELO MAIL RCPT DATA RSET NOOP QUIT VRFY HELP AUTH STARTTLS"
            } else {
                "See RFC 5321 for the full command reference"
            };
            Dispatch::Reply(Reply::help(text))
        }
        Command::Auth(_) => Dispatch::Auth,
        Command::StartTls => match session.may_starttls(context.config.tls.is_some()) {
            Ok(()) => Dispatch::StartTls,
            Err(_) if session.tls => Dispatch::Reply(Reply::already_tls()),
            Err(_) => Dispatch::Reply(Reply::tls_unavailable()),
        },
        // A recognised-but-unsupported verb is a `502` and the session continues:
        // CHUNKING is refused, not fatal, so a client that offered it can fall back
        // to `DATA` on the same connection.
        Command::Bdat(_, _) => Dispatch::Reply(Reply::not_implemented("BDAT")),
        Command::Quit => Dispatch::Close(Reply::closing()),
    }
}

/// Result of waiting for the next SMTP command while a listener may be disabled.
enum CommandWait {
    /// The peer supplied a line, disconnected, or hit a read error.
    Input(Result<Option<Vec<u8>>, FerromaError>),
    /// The Admin listener switch asked idle sessions to close.
    Disabled,
}

/// Read one command, or notice that the listener was disabled while it was idle.
async fn read_command_until(
    io: &mut SessionIo,
    timeout: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> CommandWait {
    tokio::select! {
        result = io.read_line(parser::MAX_COMMAND_LINE + 1, timeout) => CommandWait::Input(result),
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                CommandWait::Disabled
            } else {
                CommandWait::Input(io.read_line(parser::MAX_COMMAND_LINE + 1, timeout).await)
            }
        }
    }
}

/// Log an internal failure and produce the session result.
fn internal(connection_id: &str, peer: SocketAddr, error: FerromaError) -> SessionResult {
    tracing::warn!(
        connection_id = %connection_id,
        remote_ip = %peer.ip(),
        error = %error,
        result = "error",
        "SMTP session ended with an error"
    );
    SessionResult::Internal
}

/// `true` when the error is the per-command deadline.
fn is_timeout(err: &FerromaError) -> bool {
    matches!(err, FerromaError::Timeout(_))
}

/// Whether a parse error is severe enough to end the session.
///
/// A peer that streams an over-long line is not speaking SMTP; letting it keep
/// sending costs us bandwidth and log volume for nothing.
fn is_fatal_parse_error(error: &SmtpError) -> bool {
    error.kind == "line_too_long"
}

/// The `EHLO` extension list, containing exactly what is enabled.
///
/// A free function of the configuration and the current TLS state, so the whole
/// advertisement matrix is unit-testable without a database or a socket.
pub fn ehlo_extensions(
    smtp: &ferroma_core::config::SmtpConfig,
    limits: &ferroma_core::Limits,
    auth_available: bool,
    tls_available: bool,
    tls_active: bool,
) -> Vec<String> {
    let mut extensions: Vec<String> = Vec::new();

    if smtp.advertise_size {
        extensions.push(format!("SIZE {}", limits.max_message_size));
    }
    if smtp.advertise_extensions {
        extensions.push("8BITMIME".to_string());
        extensions.push("PIPELINING".to_string());
        extensions.push("ENHANCEDSTATUSCODES".to_string());
        extensions.push("SMTPUTF8".to_string());
    }
    extensions.push("HELP".to_string());

    if auth_available && (!smtp.require_tls_for_auth || tls_active) {
        extensions.push("AUTH PLAIN LOGIN".to_string());
    }
    if tls_available && !tls_active {
        extensions.push("STARTTLS".to_string());
    }
    extensions
}

/// The `MAIL FROM` handler.
async fn handle_mail_from(
    session: &mut SmtpSession,
    params: &parser::MailParams,
    context: &SessionContext,
    kind: ListenerKind,
) -> Reply {
    let smtp = &context.config.config.smtp;
    if let Err(denied) = session.may_mail_from(smtp.helo_required) {
        return match denied {
            MailFromDenied::NoGreeting => Reply::bad_sequence_because("send EHLO first"),
            MailFromDenied::InData => Reply::bad_sequence(),
        };
    }

    // A submission listener requires authentication before any envelope is built.
    if smtp.require_auth_on_submission && kind == ListenerKind::Submission && !session.is_authenticated()
    {
        return Reply::auth_required_because("this is the submission port");
    }

    // The `SIZE` the peer declared, checked before we accept a byte of body.
    if smtp.advertise_size {
        if let Some(declared) = params.size {
            if declared > context.max_message_size() {
                return Reply::size_exceeded(context.max_message_size());
            }
        }
    }

    session.begin_transaction(
        params.from.clone(),
        params.size,
        params.body == Some(BodyType::EightBitMime),
    );
    Reply::ok()
}

/// The `RCPT TO` handler: resolution **and** the open-relay policy.
async fn handle_rcpt_to(
    session: &mut SmtpSession,
    params: &parser::RcptParams,
    context: &SessionContext,
    connection_id: &str,
) -> Reply {
    if let Err(denied) = session.may_rcpt_to() {
        return match denied {
            RcptDenied::NoMailFrom => Reply::bad_sequence_because("send MAIL FROM first"),
            RcptDenied::InData => Reply::bad_sequence(),
        };
    }

    let limits = &context.config.config.limits;
    if session.transaction.recipient_count() >= limits.max_recipients {
        return Reply::too_many_recipients(limits.max_recipients);
    }

    // A repeated `RCPT TO` is a success, not a second copy.
    if session.transaction.has_recipient(&params.to) {
        return Reply::ok();
    }

    let is_local = match context.config.delivery.is_local_domain(params.to.domain()).await {
        Ok(value) => value,
        Err(e) => return Reply::from_error(&e),
    };

    if !is_local {
        // The open-relay rule: only an authenticated peer may relay.
        if !session.may_relay() {
            tracing::warn!(
                connection_id = %connection_id,
                remote_ip = %session.remote_addr.ip(),
                sender = %session
                    .transaction
                    .sender
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "<>".to_string()),
                recipient = %params.to,
                result = "relay_denied",
                "relay attempt refused"
            );
            return Reply::relay_denied("not a local domain");
        }
        session.add_recipient(params.to.clone());
        return Reply::ok();
    }

    match context.config.delivery.resolve_recipient(&params.to).await {
        Ok(crate::delivery::ResolvedRecipient::Mailbox(_)) => {
            // An unknown address is refused outright, so greylisting only ever sees
            // recipients that exist. Deferring mail to a mailbox that does not is a
            // delay with nothing behind it.
            if let Some(reply) = greet_or_defer(session, params, context, connection_id).await {
                return reply;
            }
            session.add_recipient(params.to.clone());
            Reply::ok()
        }
        Ok(crate::delivery::ResolvedRecipient::Unknown) => {
            Reply::user_unknown(&params.to.to_string())
        }
        Err(e) => Reply::from_error(&e),
    }
}

/// Defer an unknown peer once, or return `None` to accept the recipient.
///
/// Fails open: a database error accepts. Greylisting exists to filter bulk senders,
/// never to become the reason a peer cannot deliver mail.
async fn greet_or_defer(
    session: &SmtpSession,
    params: &parser::RcptParams,
    context: &SessionContext,
    connection_id: &str,
) -> Option<Reply> {
    let config = &context.config.config.policy.greylist;
    let peer = session.remote_addr.ip();
    let sender = session
        .transaction
        .sender
        .as_ref()
        .map(ToString::to_string);
    if let Some(reason) = crate::greylist::skip_reason(
        config,
        session.is_authenticated(),
        sender.as_deref(),
        peer,
    ) {
        tracing::trace!(
            connection_id = %connection_id,
            remote_ip = %peer,
            ?reason,
            "greylist skipped"
        );
        return None;
    }

    let recipient = params.to.to_string();
    let key = sender.unwrap_or_default();
    match context
        .config
        .delivery
        .repositories()
        .greylist
        .check_and_record(&peer.to_string(), &key, &recipient, crate::greylist::delay(config))
        .await
    {
        Ok(decision) if crate::greylist::is_accept(decision) => {
            tracing::debug!(
                connection_id = %connection_id,
                remote_ip = %peer,
                recipient = %recipient,
                result = "greylist_accepted",
                "peer known"
            );
            None
        }
        Ok(_) => {
            tracing::info!(
                connection_id = %connection_id,
                remote_ip = %peer,
                recipient = %recipient,
                delay_secs = config.delay_secs,
                result = "greylisted",
                "unknown peer deferred"
            );
            Some(Reply::try_again_later(
                "greylisting: this sender and recipient pair has not been seen before, please retry shortly",
            ))
        }
        Err(error) => {
            // Fail open, loudly: the peer keeps its mail, and the operator learns
            // that the greylist is not doing its job.
            tracing::warn!(
                connection_id = %connection_id,
                remote_ip = %peer,
                %error,
                "greylist check failed; accepting without deferral"
            );
            None
        }
    }
}

/// The `DATA` handler: read to the terminator, unstuff, deliver.
///
/// Returns `Ok(None)` when the peer disconnected mid-message.
async fn handle_data(
    io: &mut SessionIo,
    session: &mut SmtpSession,
    context: &SessionContext,
    kind: ListenerKind,
    connection_id: &str,
) -> Result<Option<Reply>, FerromaError> {
    if session.may_data().is_err() {
        return Ok(Some(Reply::bad_sequence_because(
            "send MAIL FROM and RCPT TO first",
        )));
    }

    // The submission throttle applies to the *message*, not the command, and the
    // daily figure is counted from the queue rather than from memory.
    if session.is_authenticated() && kind == ListenerKind::Submission {
        if let Err(limited) = context.limiter.check_message_rate(session.remote_addr.ip()) {
            tracing::warn!(
                connection_id = %connection_id,
                remote_ip = %session.remote_addr.ip(),
                limit = limited.kind.as_str(),
                result = "rate_limited",
                "submission rate limit reached"
            );
            return Ok(Some(Reply::submission_rate_limited()));
        }
        if let Some(user_id) = session.authenticated_user {
            let since = chrono::Utc::now() - chrono::Duration::hours(24);
            match context.config.repos.queue.count_sent_since(user_id, since).await {
                Ok(sent) if sent >= i64::from(context.config.config.limits.daily_send_limit) => {
                    return Ok(Some(Reply::daily_send_limit_reached(
                        context.config.config.limits.daily_send_limit,
                    )));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not check the daily send limit"),
            }
        }
    }

    session.begin_data();
    write_reply(io, &Reply::start_mail_input()).await?;

    let body = match read_data(io, context.max_message_size(), context.data_timeout()).await {
        Ok(Some(body)) => body,
        Ok(None) => return Ok(None),
        Err(DataError::TooLarge) => {
            session.end_transaction();
            return Ok(Some(Reply::size_exceeded(context.max_message_size())));
        }
        Err(DataError::Timeout) => {
            session.end_transaction();
            return Ok(Some(Reply::temporary_failure(
                "timed out reading the message",
            )));
        }
        Err(DataError::Io(e)) => return Err(e),
    };

    let mut message = ReceivedMessage::new(body, Some(session.remote_addr.ip()), connection_id);
    message.sender = session.transaction.sender.clone();
    message.recipients = session.transaction.recipients.clone();
    message.helo = session.helo.clone();
    message.received_at = session.transaction.started_at;

    // The structured fields the specification asks for, on one line per transaction.
    // Addresses only: a body never reaches a log.
    tracing::info!(
        connection_id = %connection_id,
        remote_ip = %session.remote_addr.ip(),
        helo = session.helo.as_deref().unwrap_or("-"),
        authenticated_user = session.authenticated_as.as_deref().unwrap_or("-"),
        sender = %message
            .sender
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "<>".to_string()),
        recipient = %message
            .recipients
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        size_bytes = message.body.len(),
        result = "received",
        "message accepted for delivery"
    );

    // ---------------------------------------------------------------
    // Inbound authentication policy (SPF / DKIM / DMARC).
    //
    // It runs *here* — after DATA and before anything is stored — and that placement is
    // the whole reason a `reject` is safe: the peer has not been told `250` yet, so
    // refusing the message is honest. Evaluating after delivery would mean either
    // contradicting an acknowledgement or silently discarding a message we accepted.
    // ---------------------------------------------------------------
    let parsed = match ParsedMessage::parse_with_limits(
        &message.body,
        &ferroma_mail::message::ParseLimits::default(),
    ) {
        Ok(parsed) => parsed,
        Err(e) => {
            session.end_transaction();
            tracing::warn!(
                connection_id = %connection_id,
                remote_ip = %session.remote_addr.ip(),
                error = %e,
                result = "unparseable",
                "the message could not be parsed"
            );
            return Ok(Some(Reply::from_error(&e)));
        }
    };

    // Authenticated submissions must not be evaluated by inbound SPF/DMARC: the
    // submitting client is not the domain's published MX.
    if let Some(policy) = context.policy.as_ref().filter(|_| !session.is_authenticated()) {
        let verdict = policy.evaluate(&message, &parsed).await;
        log_policy(connection_id, &session.remote_addr, &verdict);
        if verdict.is_reject() {
            // Nothing has been stored, so this reply is the whole story.
            let domain = verdict
                .dmarc
                .as_ref()
                .and_then(|v| v.domain.clone())
                .unwrap_or_else(|| session.helo.clone().unwrap_or_else(|| "the sender".into()));
            session.end_transaction();
            return Ok(Some(Reply::dmarc_rejected(&domain)));
        }
        if verdict.action == PolicyAction::Quarantine {
            message.deliver_to = Some(crate::delivery::JUNK.to_string());
        }
        message.authentication_results = verdict.header_value().map(str::to_string);
    }

    let reply = match context
        .config
        .delivery
        .deliver_for(&message, &parsed, session.authenticated_user)
        .await
    {
        Ok(report) => {
            log_report(connection_id, context.hostname(), &report);
            report.reply()
        }
        Err(e) => {
            tracing::warn!(
                connection_id = %connection_id,
                remote_ip = %session.remote_addr.ip(),
                error = %e,
                result = "delivery_failed",
                "message could not be delivered"
            );
            Reply::from_error(&e)
        }
    };

    session.end_transaction();
    Ok(Some(reply))
}

/// Why reading a `DATA` payload stopped.
#[derive(Debug)]
enum DataError {
    /// The payload exceeded the configured limit.
    TooLarge,
    /// The peer went quiet before the terminating line.
    Timeout,
    /// The socket failed.
    Io(FerromaError),
}

/// Read a `DATA` payload until `\r\n.\r\n`.
async fn read_data(
    io: &mut SessionIo,
    max_size: u64,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, DataError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let max_size = max_size.min(usize::MAX as u64) as usize;
    let mut raw: Vec<u8> = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(DataError::Timeout);
        }
        let line = match io.read_line(64 * 1024, remaining).await {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(None),
            Err(FerromaError::Timeout(_)) => return Err(DataError::Timeout),
            Err(e) => return Err(DataError::Io(e)),
        };

        if is_terminator(&line) {
            return Ok(Some(unstuff(&raw)));
        }

        // Bounded *before* appending, so a peer cannot make us allocate more than the
        // configured limit. The check is on the raw wire bytes, which can only shrink
        // when dot-stuffing is undone — erring on the strict side is what the `SIZE`
        // extension we advertised promised.
        if raw.len().saturating_add(line.len()) > max_size {
            return Err(DataError::TooLarge);
        }
        raw.extend_from_slice(&line);
    }
}

/// Whether a line is the `DATA` terminator (a lone `.`).
fn is_terminator(line: &[u8]) -> bool {
    strip_eol(line) == b"."
}

/// Strip one trailing CRLF, LF or CR.
fn strip_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

/// Undo dot-stuffing and normalise line endings to CRLF.
///
/// Bare `\n` is normalised because a stored message with bare LF breaks every
/// downstream parser, and refusing a message a broken client *did* manage to send
/// would lose mail.
pub fn unstuff(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 2);
    let mut rest = raw;
    while !rest.is_empty() {
        let (line, remainder) = match rest.iter().position(|b| *b == b'\n') {
            Some(index) => (&rest[..index + 1], &rest[index + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        let content = strip_eol(line);
        let content = if content.first() == Some(&b'.') {
            &content[1..]
        } else {
            content
        };
        out.extend_from_slice(content);
        out.extend_from_slice(b"\r\n");
        rest = remainder;
    }
    out
}

/// The `AUTH` command handler.
async fn handle_auth(
    io: &mut SessionIo,
    session: &mut SmtpSession,
    command: &Command,
    context: &SessionContext,
) -> Result<(), FerromaError> {
    let Command::Auth(params) = command else {
        return Ok(());
    };
    let smtp = &context.config.config.smtp;

    if let Err(denied) = session.may_auth(smtp.require_tls_for_auth) {
        let reply = match denied {
            AuthDenied::AlreadyAuthenticated => Reply::already_authenticated(),
            AuthDenied::TlsRequired => Reply::auth_requires_tls(),
            AuthDenied::InProgress => Reply::bad_sequence_because("AUTH is already in progress"),
            AuthDenied::InData => Reply::bad_sequence(),
            AuthDenied::InTransaction => {
                Reply::bad_sequence_because("AUTH is not allowed inside a transaction")
            }
        };
        return write_reply(io, &reply).await;
    }

    if context.config.auth.is_none() {
        return write_reply(io, &Reply::auth_mechanism_unsupported()).await;
    }

    match params.mechanism.as_str() {
        "PLAIN" => {
            let Some(initial) = params.initial_response.as_deref() else {
                // An empty `334` asks the client for the payload on the next line,
                // which the connection loop routes back through
                // `handle_auth_response` only when `auth_state` is set.
                session.begin_auth(AuthState::AwaitingPassword);
                return write_reply(io, &Reply::auth_challenge("")).await;
            };
            if initial == "=" {
                return write_reply(io, &Reply::auth_failed()).await;
            }
            match decode_sasl(initial) {
                Some(bytes) => match split_plain(&bytes) {
                    Some((user, password)) => {
                        finish_auth(io, session, &user, &password, context).await
                    }
                    None => {
                        write_reply(io, &Reply::auth_bad_exchange("malformed PLAIN response")).await
                    }
                },
                None => write_reply(io, &Reply::auth_bad_exchange("invalid base64")).await,
            }
        }
        "LOGIN" => {
            if let Some(initial) = params.initial_response.as_deref() {
                let Some(bytes) = decode_sasl(initial) else {
                    return write_reply(io, &Reply::auth_bad_exchange("invalid base64")).await;
                };
                let Some(username) = String::from_utf8(bytes).ok().filter(|u| !u.is_empty()) else {
                    return write_reply(io, &Reply::auth_bad_exchange("empty username")).await;
                };
                session.begin_auth(AuthState::AwaitingPassword);
                session.pending_auth_username = Some(username);
                return write_reply(io, &Reply::auth_challenge(AUTH_LOGIN_PASSWORD_CHALLENGE)).await;
            }
            session.begin_auth(AuthState::AwaitingUsername);
            write_reply(io, &Reply::auth_challenge(AUTH_LOGIN_USERNAME_CHALLENGE)).await
        }
        _ => write_reply(io, &Reply::auth_mechanism_unsupported()).await,
    }
}

/// One line of a multi-step SASL exchange.
async fn handle_auth_response(
    io: &mut SessionIo,
    session: &mut SmtpSession,
    line: &[u8],
    context: &SessionContext,
) -> Result<(), FerromaError> {
    let line = strip_eol(line);
    // `*` is the SASL cancel token (RFC 4954 §4).
    if line == b"*" {
        session.abort_auth();
        return write_reply(io, &Reply::auth_failed()).await;
    }
    let Some(text) = std::str::from_utf8(line).ok().map(str::trim) else {
        session.abort_auth();
        return write_reply(io, &Reply::auth_bad_exchange("response is not UTF-8")).await;
    };
    let Some(bytes) = decode_sasl(text) else {
        session.abort_auth();
        return write_reply(io, &Reply::auth_bad_exchange("invalid base64")).await;
    };

    match session.auth_state {
        Some(AuthState::AwaitingUsername) => {
            let Some(username) = String::from_utf8(bytes).ok().filter(|u| !u.is_empty()) else {
                session.abort_auth();
                return write_reply(io, &Reply::auth_bad_exchange("empty username")).await;
            };
            session.pending_auth_username = Some(username);
            session.auth_state = Some(AuthState::AwaitingPassword);
            write_reply(io, &Reply::auth_challenge(AUTH_LOGIN_PASSWORD_CHALLENGE)).await
        }
        Some(AuthState::AwaitingPassword) => {
            let password = String::from_utf8(bytes).unwrap_or_default();
            let username = session.pending_auth_username.clone().unwrap_or_default();
            finish_auth(io, session, &username, &password, context).await
        }
        None => write_reply(io, &Reply::bad_sequence()).await,
    }
}

/// Verify a username/password pair and answer `235` or `535`.
async fn finish_auth(
    io: &mut SessionIo,
    session: &mut SmtpSession,
    username: &str,
    password: &str,
    context: &SessionContext,
) -> Result<(), FerromaError> {
    let Some(auth) = context.config.auth.as_ref() else {
        session.abort_auth();
        return write_reply(io, &Reply::auth_mechanism_unsupported()).await;
    };

    let email = username.trim().to_ascii_lowercase();

    // One credential check for both kinds of secret: the account password, or an
    // application password for an account whose second factor this protocol cannot
    // carry. `authenticate_client` refuses a password-only login once a second
    // factor is enforced, which is the whole point of enabling it.
    let authenticated_user = match auth.authenticate_client(&email, password).await {
        Ok(user_id) => user_id,
        Err(e) => {
            session.abort_auth();
            tracing::warn!(error = %e, "could not look up the account for AUTH");
            return write_reply(io, &Reply::auth_temporary_failure()).await;
        }
    };

    if authenticated_user.is_some() {
        match authenticated_user {
            Some(user_id) => {
                session.authenticate(user_id, email.clone());
                // `AUTH PLAIN` with the payload on the same line never called
                // `begin_auth`, but a client that pipelined `MAIL FROM` behind it
                // is still read as the SASL response unless this is cleared. The
                // message is then never accepted, and the client disconnects.
                session.abort_auth();
                // A successful AUTH clears any transaction state that preceded it.
                session.reset();
                tracing::info!(
                    connection_id = %session.connection_id,
                    remote_ip = %session.remote_addr.ip(),
                    authenticated_user = %email,
                    result = "authenticated",
                    "SMTP AUTH succeeded"
                );
                write_reply(io, &Reply::auth_successful()).await
            }
            None => {
                session.abort_auth();
                write_reply(io, &Reply::auth_temporary_failure()).await
            }
        }
    } else {
        tracing::warn!(
            connection_id = %session.connection_id,
            remote_ip = %session.remote_addr.ip(),
            result = "auth_failed",
            "SMTP AUTH failed"
        );
        session.abort_auth();
        write_reply(io, &Reply::auth_failed()).await
    }
}

/// Decode a base64 SASL response. The result is never logged.
fn decode_sasl(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

/// Split an `AUTH PLAIN` payload: `authzid NUL authcid NUL passwd`.
///
/// The password is everything after the *second* NUL, so a password containing a NUL
/// byte survives.
fn split_plain(payload: &[u8]) -> Option<(String, String)> {
    let first = payload.iter().position(|b| *b == 0)?;
    let rest = &payload[first + 1..];
    let second = rest.iter().position(|b| *b == 0)?;
    let username = String::from_utf8(rest[..second].to_vec()).ok()?;
    if username.is_empty() {
        return None;
    }
    let password = String::from_utf8(rest[second + 1..].to_vec()).ok()?;
    Some((username, password))
}

/// Log one delivery report with the fields the specification asks for.
///
/// Never logs message content: only addresses, ids and outcomes.
fn log_report(connection_id: &str, hostname: &str, report: &crate::delivery::DeliveryReport) {
    for outcome in &report.outcomes {
        match outcome {
            RecipientOutcome::Delivered {
                address,
                resolved_to,
                mailbox_id,
                message_id,
            } => tracing::info!(
                connection_id = %connection_id,
                recipient = %address,
                resolved_to = %resolved_to,
                mailbox_id = mailbox_id.get(),
                message_id = message_id.get(),
                result = "delivered",
                "message delivered locally"
            ),
            RecipientOutcome::Unknown { address } => tracing::info!(
                connection_id = %connection_id,
                recipient = %address,
                result = "user_unknown",
                "recipient does not exist"
            ),
            RecipientOutcome::Full { address, mailbox_id } => tracing::info!(
                connection_id = %connection_id,
                recipient = %address,
                mailbox_id = mailbox_id.get(),
                result = "mailbox_full",
                "recipient mailbox is over quota"
            ),
            RecipientOutcome::Queued { address, .. } => tracing::info!(
                connection_id = %connection_id,
                recipient = %address,
                result = "queued",
                "recipient accepted for the outbound queue"
            ),
            RecipientOutcome::Failed { address, reason } => tracing::warn!(
                connection_id = %connection_id,
                recipient = %address,
                result = "failed",
                reason = %reason,
                "recipient could not be delivered to"
            ),
        }
    }
    tracing::debug!(
        connection_id = %connection_id,
        hostname = %hostname,
        delivered = report.delivered().len(),
        failed = report.failed().len(),
        "delivery report"
    );
}

/// Log one inbound-policy verdict.
///
/// Reports the method results, the action and the reason — never a header value, a
/// signature or any message content.
fn log_policy(connection_id: &str, peer: &SocketAddr, verdict: &InboundVerdict) {
    tracing::info!(
        connection_id = %connection_id,
        remote_ip = %peer.ip(),
        spf = verdict.spf.as_ref().map(|o| o.result.as_str()).unwrap_or("-"),
        dkim = %if verdict.dkim.is_empty() {
            "-".to_string()
        } else {
            verdict
                .dkim
                .iter()
                .map(|v| v.result.as_str())
                .collect::<Vec<_>>()
                .join(",")
        },
        dmarc = verdict
            .dmarc
            .as_ref()
            .map(|v| v.result.as_str())
            .unwrap_or("-"),
        action = verdict.action.as_str(),
        reason = verdict.reason.as_deref().unwrap_or("-"),
        result = "policy",
        "inbound authentication policy evaluated"
    );
}

/// Write a reply through a [`SessionIo`].
async fn write_reply(io: &mut SessionIo, reply: &Reply) -> Result<(), FerromaError> {
    let bytes = reply.render();
    for chunk in bytes.chunks(64 * 1024) {
        io.write_all(chunk).await?;
    }
    io.flush().await
}

/// The user id an authenticated session belongs to.
pub fn authenticated_user(session: &SmtpSession) -> Option<UserId> {
    session.authenticated_user
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_command;
    use ferroma_core::config::SmtpConfig;
    use ferroma_core::Limits;

    fn session(tls: bool) -> SmtpSession {
        let mut session =
            SmtpSession::new("c1", "192.0.2.10:40000".parse().expect("socket address"));
        session.tls = tls;
        session
    }

    fn extensions(auth: bool, tls_available: bool, tls_active: bool) -> Vec<String> {
        ehlo_extensions(
            &SmtpConfig::default(),
            &Limits::default(),
            auth,
            tls_available,
            tls_active,
        )
    }

    // ------------------------------------------------------------------
    // AUTH challenges
    // ------------------------------------------------------------------

    #[test]
    fn the_login_challenges_are_the_byte_exact_rfc_strings() {
        assert_eq!(AUTH_LOGIN_USERNAME_CHALLENGE, "VXNlcm5hbWU6");
        assert_eq!(AUTH_LOGIN_PASSWORD_CHALLENGE, "UGFzc3dvcmQ6");
        assert_eq!(
            Reply::auth_challenge(AUTH_LOGIN_USERNAME_CHALLENGE).render(),
            b"334 VXNlcm5hbWU6\r\n"
        );
        assert_eq!(
            Reply::auth_challenge(AUTH_LOGIN_PASSWORD_CHALLENGE).render(),
            b"334 UGFzc3dvcmQ6\r\n"
        );
    }

    #[test]
    fn the_login_challenges_decode_to_the_documented_prompts() {
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(AUTH_LOGIN_USERNAME_CHALLENGE)
                .expect("base64"),
            b"Username:"
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(AUTH_LOGIN_PASSWORD_CHALLENGE)
                .expect("base64"),
            b"Password:"
        );
    }

    // ------------------------------------------------------------------
    // EHLO advertisement
    // ------------------------------------------------------------------

    #[test]
    fn ehlo_advertises_the_default_extensions() {
        let list = extensions(true, true, false);
        assert!(list.contains(&"SIZE 26214400".to_string()), "{list:?}");
        assert!(list.contains(&"8BITMIME".to_string()));
        assert!(list.contains(&"PIPELINING".to_string()));
        assert!(list.contains(&"ENHANCEDSTATUSCODES".to_string()));
        assert!(list.contains(&"SMTPUTF8".to_string()));
        assert!(list.contains(&"HELP".to_string()));
    }

    #[test]
    fn ehlo_hides_starttls_when_no_acceptor_exists() {
        let list = extensions(true, false, false);
        assert!(!list.contains(&"STARTTLS".to_string()), "{list:?}");
    }

    #[test]
    fn ehlo_hides_starttls_once_tls_is_active() {
        let list = extensions(true, true, true);
        assert!(!list.contains(&"STARTTLS".to_string()), "{list:?}");
        assert!(list.contains(&"AUTH PLAIN LOGIN".to_string()));
    }

    #[test]
    fn ehlo_hides_auth_when_no_auth_service_exists() {
        let list = extensions(false, true, false);
        assert!(!list.iter().any(|e| e.starts_with("AUTH ")), "{list:?}");
    }

    #[test]
    fn ehlo_hides_auth_on_plaintext_when_tls_is_required_for_it() {
        let smtp = SmtpConfig {
            require_tls_for_auth: true,
            ..SmtpConfig::default()
        };
        let plaintext = ehlo_extensions(&smtp, &Limits::default(), true, true, false);
        assert!(
            !plaintext.iter().any(|e| e.starts_with("AUTH ")),
            "{plaintext:?}"
        );
        let encrypted = ehlo_extensions(&smtp, &Limits::default(), true, true, true);
        assert!(
            encrypted.contains(&"AUTH PLAIN LOGIN".to_string()),
            "{encrypted:?}"
        );
    }

    #[test]
    fn ehlo_omits_the_extension_block_when_it_is_turned_off() {
        let smtp = SmtpConfig {
            advertise_size: false,
            advertise_extensions: false,
            ..SmtpConfig::default()
        };
        let list = ehlo_extensions(&smtp, &Limits::default(), false, false, false);
        assert_eq!(list, vec!["HELP".to_string()]);
    }

    #[test]
    fn the_size_extension_uses_the_configured_maximum() {
        let limits = Limits {
            max_message_size: 1024,
            ..Limits::default()
        };
        let list = ehlo_extensions(&SmtpConfig::default(), &limits, false, false, false);
        assert!(list.contains(&"SIZE 1024".to_string()), "{list:?}");
    }

    #[test]
    fn the_ehlo_block_renders_with_the_hostname_first() {
        let list = extensions(true, true, false);
        let reply = Reply::ehlo("mx.example.com", &list);
        let rendered = reply.render();
        assert!(
            rendered.starts_with(b"250-mx.example.com greets you\r\n"),
            "{rendered:?}"
        );
        assert!(rendered.ends_with(b"\r\n"));
        assert_eq!(reply.code(), 250);
    }

    #[test]
    fn the_banner_uses_the_configured_hostname_and_text() {
        let smtp = SmtpConfig::default();
        let reply = Reply::service_ready("mx.example.com", &smtp.banner);
        assert!(
            reply.render().starts_with(b"220 mx.example.com "),
            "{:?}",
            reply.render()
        );
    }

    // ------------------------------------------------------------------
    // DATA framing
    // ------------------------------------------------------------------

    #[test]
    fn the_terminator_is_a_lone_dot() {
        assert!(is_terminator(b".\r\n"));
        assert!(is_terminator(b".\n"));
        assert!(is_terminator(b"."));
        assert!(!is_terminator(b"..\r\n"));
        assert!(!is_terminator(b".x\r\n"));
        assert!(!is_terminator(b"\r\n"));
        assert!(!is_terminator(b""));
    }

    #[test]
    fn dot_stuffing_is_undone() {
        assert_eq!(unstuff(b"..leading dot\r\n"), b".leading dot\r\n");
    }

    #[test]
    fn a_line_of_only_dots_loses_exactly_one() {
        assert_eq!(unstuff(b"...\r\n"), b"..\r\n");
    }

    #[test]
    fn bare_lf_is_normalised_to_crlf() {
        assert_eq!(unstuff(b"a\nb\n"), b"a\r\nb\r\n");
    }

    #[test]
    fn an_empty_payload_unstuffs_to_nothing() {
        assert_eq!(unstuff(b""), b"");
    }

    #[test]
    fn a_payload_without_a_final_newline_still_gains_one() {
        assert_eq!(unstuff(b"no newline"), b"no newline\r\n");
    }

    #[test]
    fn a_multi_line_payload_round_trips() {
        let raw = b"From: a@b\r\nSubject: hi\r\n\r\nbody one\r\nbody two\r\n";
        assert_eq!(unstuff(raw), raw);
    }

    #[test]
    fn a_dot_stuffed_body_is_stored_unstuffed() {
        assert_eq!(
            unstuff(b"From: a@b\r\n\r\n..hidden\r\n"),
            b"From: a@b\r\n\r\n.hidden\r\n"
        );
    }

    #[test]
    fn strip_eol_handles_every_ending() {
        assert_eq!(strip_eol(b"x\r\n"), b"x");
        assert_eq!(strip_eol(b"x\n"), b"x");
        assert_eq!(strip_eol(b"x\r"), b"x");
        assert_eq!(strip_eol(b"x"), b"x");
        assert_eq!(strip_eol(b""), b"");
    }

    // ------------------------------------------------------------------
    // SASL
    // ------------------------------------------------------------------

    #[test]
    fn plain_payloads_are_split_on_nul() {
        assert_eq!(
            split_plain(b"\x00alice@example.com\x00secret"),
            Some(("alice@example.com".to_string(), "secret".to_string()))
        );
    }

    #[test]
    fn plain_accepts_an_authzid() {
        assert_eq!(
            split_plain(b"admin\x00alice@example.com\x00secret"),
            Some(("alice@example.com".to_string(), "secret".to_string()))
        );
    }

    #[test]
    fn plain_rejects_a_missing_component() {
        assert_eq!(split_plain(b"\x00alice@example.com"), None);
        assert_eq!(split_plain(b"alice@example.com"), None);
        assert_eq!(split_plain(b""), None);
        assert_eq!(
            split_plain(b"\x00\x00secret"),
            None,
            "an empty username is not valid"
        );
    }

    #[test]
    fn plain_keeps_a_password_containing_a_nul() {
        assert_eq!(
            split_plain(b"\x00alice@example.com\x00a\x00b"),
            Some(("alice@example.com".to_string(), "a\u{0}b".to_string()))
        );
    }

    #[test]
    fn sasl_decoding_accepts_valid_base64_and_rejects_garbage() {
        assert_eq!(
            decode_sasl("AGFsaWNlAHNlY3JldA=="),
            Some(b"\x00alice\x00secret".to_vec())
        );
        assert_eq!(decode_sasl("not base64!"), None);
        assert_eq!(decode_sasl(""), Some(Vec::new()));
    }

    #[test]
    fn sasl_decoding_tolerates_surrounding_whitespace() {
        assert_eq!(decode_sasl("  AGZvbwBiYXI=  "), Some(b"\x00foo\x00bar".to_vec()));
    }

    // ------------------------------------------------------------------
    // SessionIo
    // ------------------------------------------------------------------

    /// A `SessionIo` over a duplex pipe preloaded with `payload`.
    fn duplex(payload: &[u8]) -> SessionIo {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let payload = payload.to_vec();
        tokio::spawn(async move {
            let mut client = client;
            let _ = tokio::io::AsyncWriteExt::write_all(&mut client, &payload).await;
            let _ = tokio::io::AsyncWriteExt::flush(&mut client).await;
            // Hold the write half open so the reader never sees an early EOF.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        SessionIo::new(SharedStream::new(Box::new(TokioCompat(server))))
    }

    #[tokio::test]
    async fn read_line_returns_one_line_including_its_crlf() {
        let mut io = duplex(b"EHLO mail.example.com\r\nNOOP\r\n");
        let first = io
            .read_line(1024, Duration::from_secs(1))
            .await
            .expect("read")
            .expect("a line");
        assert_eq!(first, b"EHLO mail.example.com\r\n");
        let second = io
            .read_line(1024, Duration::from_secs(1))
            .await
            .expect("read")
            .expect("a line");
        assert_eq!(second, b"NOOP\r\n");
    }

    #[tokio::test]
    async fn read_line_accepts_a_bare_lf() {
        let mut io = duplex(b"QUIT\n");
        let line = io
            .read_line(1024, Duration::from_secs(1))
            .await
            .expect("read")
            .expect("a line");
        assert_eq!(line, b"QUIT\n");
    }

    #[tokio::test]
    async fn read_line_stops_at_the_limit_without_waiting_for_a_newline() {
        let mut io = duplex(&vec![b'A'; 4096]);
        let line = io
            .read_line(64, Duration::from_secs(1))
            .await
            .expect("read")
            .expect("a line");
        assert_eq!(line.len(), 64);
        assert!(line.iter().all(|b| *b == b'A'));
    }

    #[tokio::test]
    async fn read_line_reports_a_timeout() {
        let (client, server) = tokio::io::duplex(1024);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(client);
        });
        let mut io = SessionIo::new(SharedStream::new(Box::new(TokioCompat(server))));
        let err = io
            .read_line(1024, Duration::from_millis(30))
            .await
            .expect_err("must time out");
        assert!(is_timeout(&err), "{err:?}");
        handle.abort();
    }

    #[tokio::test]
    async fn read_line_reports_a_clean_end_of_stream() {
        let (client, server) = tokio::io::duplex(1024);
        drop(client);
        let mut io = SessionIo::new(SharedStream::new(Box::new(TokioCompat(server))));
        assert!(io
            .read_line(1024, Duration::from_secs(1))
            .await
            .expect("read")
            .is_none());
    }

    #[tokio::test]
    async fn into_inner_refuses_when_plaintext_is_buffered() {
        let mut io = duplex(b"MAIL FROM:<a@b.c>\r\nNOOP\r\n");
        let _ = io
            .read_line(1024, Duration::from_secs(1))
            .await
            .expect("read");
        // The second line arrived in the same read, so it is still in the buffer —
        // exactly the request-smuggling shape RFC 3207 §6 warns about.
        assert!(io.has_buffered_input());
        let err = io.into_inner().expect_err("must refuse");
        assert!(matches!(err, FerromaError::Protocol(_)), "{err:?}");
    }

    #[tokio::test]
    async fn into_inner_succeeds_for_a_bare_starttls() {
        let mut io = duplex(b"STARTTLS\r\n");
        let line = io
            .read_line(1024, Duration::from_secs(1))
            .await
            .expect("read")
            .expect("a line");
        assert_eq!(line, b"STARTTLS\r\n");
        assert!(!io.has_buffered_input());
        assert!(io.into_inner().is_ok(), "a clean upgrade must be possible");
    }

    #[tokio::test]
    async fn replies_are_written_verbatim() {
        let (client, server) = tokio::io::duplex(4096);
        let mut io = SessionIo::new(SharedStream::new(Box::new(TokioCompat(server))));
        write_reply(&mut io, &Reply::ok()).await.expect("write");
        let mut client = client;
        let mut buffer = vec![0u8; 64];
        let read = tokio::io::AsyncReadExt::read(&mut client, &mut buffer)
            .await
            .expect("read");
        assert_eq!(&buffer[..read], b"250 2.1.0 Ok\r\n");
    }

    #[tokio::test]
    async fn a_multi_line_reply_is_written_in_one_piece() {
        let (client, server) = tokio::io::duplex(4096);
        let mut io = SessionIo::new(SharedStream::new(Box::new(TokioCompat(server))));
        let reply = Reply::ehlo("mx.example.com", &extensions(true, true, false));
        write_reply(&mut io, &reply).await.expect("write");
        let mut client = client;
        let mut buffer = vec![0u8; 1024];
        let read = tokio::io::AsyncReadExt::read(&mut client, &mut buffer)
            .await
            .expect("read");
        let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
        assert!(
            text.starts_with("250-mx.example.com greets you\r\n"),
            "{text}"
        );
        assert!(text.ends_with("\r\n"), "{text}");
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    #[test]
    fn listener_addresses_accept_localhost_and_literals() {
        assert_eq!(
            listener_address("localhost", 2525).expect("address"),
            "127.0.0.1:2525".parse().expect("socket address")
        );
        assert_eq!(
            listener_address("0.0.0.0", 25).expect("address"),
            "0.0.0.0:25".parse().expect("socket address")
        );
        assert_eq!(
            listener_address("::1", 25).expect("address"),
            "[::1]:25".parse().expect("socket address")
        );
    }

    #[test]
    fn a_non_ip_host_is_a_configuration_error() {
        let err = listener_address("mail.example.com", 25).expect_err("must fail");
        assert!(matches!(err, FerromaError::Config(_)), "{err:?}");
    }

    #[test]
    fn connection_ids_are_unique_and_hex() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let id = new_connection_id();
            assert_eq!(id.len(), 16);
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(seen.insert(id));
        }
    }

    #[test]
    fn timeouts_are_recognised() {
        assert!(is_timeout(&FerromaError::Timeout("x".into())));
        assert!(!is_timeout(&FerromaError::Network("x".into())));
    }

    #[test]
    fn only_an_over_long_line_is_a_fatal_parse_error() {
        let long = parse_command(&vec![b'A'; 5000]).expect_err("must fail");
        assert!(is_fatal_parse_error(&long));
        let syntax = parse_command(b"XYZZY\r\n").expect_err("must fail");
        assert!(!is_fatal_parse_error(&syntax));
    }

    #[test]
    fn session_results_have_stable_names() {
        for result in [
            SessionResult::Quit,
            SessionResult::Disconnected,
            SessionResult::Timeout,
            SessionResult::TlsFailed,
            SessionResult::LineTooLong,
            SessionResult::Upgrade,
            SessionResult::Internal,
        ] {
            assert!(!result.as_str().is_empty());
        }
        assert!(SessionResult::Upgrade.is_upgrade());
        assert!(!SessionResult::Quit.is_upgrade());
    }

    #[test]
    fn listener_kinds_report_their_role() {
        assert!(ListenerKind::Mx.is_inbound());
        assert!(!ListenerKind::Submission.is_inbound());
        assert!(!ListenerKind::Smtps.is_inbound());
        assert_eq!(ListenerKind::Smtps.as_str(), "smtps");
        assert_eq!(ListenerKind::Submission.as_str(), "submission");
    }

    #[tokio::test]
    async fn binding_port_zero_picks_an_ephemeral_port() {
        let listener = SmtpListener::bind(
            "127.0.0.1:0".parse().expect("address"),
            ListenerKind::Mx,
            false,
        )
        .await
        .expect("bind");
        let address = listener.local_addr().expect("local address");
        assert_ne!(address.port(), 0);
        assert_eq!(listener.kind(), ListenerKind::Mx);
        assert!(!listener.implicit_tls());
        listener.close();
    }

    #[test]
    fn helper_accessors_are_exposed_for_callers() {
        let mut session = session(false);
        assert_eq!(authenticated_user(&session), None);
        session.authenticate(UserId::new(5), "alice@example.com");
        assert_eq!(authenticated_user(&session), Some(UserId::new(5)));
    }

    #[test]
    fn the_report_logging_is_body_free() {
        // Pins the shape of the call: `log_report` takes only the report, which
        // carries addresses and ids and never message content.
        let report = crate::delivery::DeliveryReport::default();
        log_report("c1", "mx.example.com", &report);
    }
}
