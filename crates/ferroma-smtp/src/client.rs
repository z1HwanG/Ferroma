//! The outbound SMTP client: deliver one message to one recipient.
//!
//! ```text
//!   queue.rs ──► SmtpClient::deliver
//!                    │
//!                    ├─ MX lookup            (mx.rs, via the caller's targets)
//!                    ├─ connect_timeout      TCP, then optional implicit TLS
//!                    ├─ read banner, EHLO    (HELO fallback for old servers)
//!                    ├─ STARTTLS             opportunistic by default
//!                    ├─ MAIL FROM / RCPT TO / DATA / QUIT
//!                    └─ DeliveryOutcome      Delivered | Temporary | Permanent
//! ```
//!
//! # The classification rule
//!
//! The outcome decides whether a message is retried or bounced, so getting it
//! wrong either loses mail or spams a remote server forever:
//!
//! * **`Delivered`** — the remote server accepted the message (`2xx` after `DATA`).
//! * **`Temporary`** — a `4xx` reply, a connection failure, or a timeout. *Every*
//!   transport failure is temporary: a timeout never means "this message is
//!   undeliverable", only "try again later". Treating one as permanent is how a
//!   mail server silently drops mail during a network blip.
//! * **`Permanent`** — a `5xx` reply. The remote server has made a decision.
//!
//! # TLS
//!
//! [`TlsPolicy::Opportunistic`] is the default and the right answer for a real MTA:
//! upgrade when the peer offers it, and deliver in the clear when it does not, the
//! way RFC 7435 describes. [`TlsPolicy::Required`] refuses to send in the clear and
//! reports a **temporary** failure instead — a policy failure must not bounce mail.
//! Certificate verification is on by default, and turning it off logs loudly at
//! `warn` on every connection.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ferroma_core::{FerromaError, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_rustls::TlsConnector;

use crate::mx::MxHost;

/// How much of one remote reply we are willing to read.
const MAX_REPLY_BYTES: usize = 8192;

/// The largest `DATA` payload we will send, as a sanity bound. The queue enforces
/// the real limit; this only stops a corrupt row from streaming forever.
const MAX_DATA_BYTES: usize = 64 * 1024 * 1024;

/// When to use TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Try `STARTTLS` when the peer advertises it; deliver in the clear otherwise
    /// (RFC 7435). The default, and right for almost every deployment.
    Opportunistic,
    /// Require an encrypted channel. A peer that does not offer `STARTTLS`, or whose
    /// handshake fails, produces a **temporary** failure — never a bounce.
    Required,
    /// TLS from the first byte (SMTPS). What a relay on port 465 expects.
    Implicit,
    /// Never use TLS. Only for an internal relay on a trusted network.
    Disabled,
}

impl TlsPolicy {
    /// A short name for logs and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            TlsPolicy::Opportunistic => "opportunistic",
            TlsPolicy::Required => "required",
            TlsPolicy::Implicit => "implicit",
            TlsPolicy::Disabled => "disabled",
        }
    }

    /// Whether an unencrypted channel is acceptable.
    pub fn is_required(self) -> bool {
        matches!(self, TlsPolicy::Required | TlsPolicy::Implicit)
    }

    /// Whether TLS may be used at all.
    pub fn allows_tls(self) -> bool {
        !matches!(self, TlsPolicy::Disabled)
    }

    /// Whether the handshake happens before the greeting rather than through
    /// `STARTTLS`.
    pub fn is_implicit(self) -> bool {
        matches!(self, TlsPolicy::Implicit)
    }

    /// From a configuration string: `starttls`, `implicit`, `none`.
    ///
    /// `None` for anything else, so a caller can reject a typo at boot rather than
    /// silently downgrade to cleartext.
    pub fn from_config_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "starttls" | "required" => Some(TlsPolicy::Required),
            "implicit" | "smtps" => Some(TlsPolicy::Implicit),
            "none" | "disabled" | "cleartext" => Some(TlsPolicy::Disabled),
            "opportunistic" => Some(TlsPolicy::Opportunistic),
            _ => None,
        }
    }
}

/// Credentials for `AUTH` at a relay.
///
/// Its `Debug` is hand-written: a configuration dump that prints a password is a
/// leak, and this struct travels inside [`SmtpClientConfig`], which is `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpCredentials {
    /// The account name the relay expects.
    pub username: String,
    /// Its password.
    pub password: String,
}

impl std::fmt::Debug for SmtpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpCredentials")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

/// Client tuning.
#[derive(Debug, Clone)]
pub struct SmtpClientConfig {
    /// The name we announce in `EHLO`, and the name the remote server sees on our
    /// `Received:` header if it logs one.
    pub hostname: String,
    /// The TCP port to connect to. `25` everywhere except in tests, which point the
    /// client at an ephemeral port.
    pub port: u16,
    /// The TLS policy.
    pub tls: TlsPolicy,
    /// Whether to verify the remote certificate. Turning this off is a security
    /// downgrade and is logged at `warn` on every connection.
    pub verify_certificates: bool,
    /// Credentials for `AUTH`, when the peer is a relay that requires them.
    pub auth: Option<SmtpCredentials>,
    /// Send credentials even when the channel is not encrypted. Off by default:
    /// `AUTH` before `STARTTLS` hands the password to anyone on the path, and no
    /// hosting provider's submission service needs it.
    pub allow_cleartext_auth: bool,
    /// Deadline for the TCP connect and the TLS handshake.
    pub connect_timeout: Duration,
    /// Deadline for each reply.
    pub read_timeout: Duration,
    /// Deadline for the whole session.
    pub session_timeout: Duration,
}

impl Default for SmtpClientConfig {
    fn default() -> Self {
        SmtpClientConfig {
            hostname: "localhost".to_string(),
            port: 25,
            tls: TlsPolicy::Opportunistic,
            verify_certificates: true,
            auth: None,
            allow_cleartext_auth: false,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(300),
        }
    }
}

impl SmtpClientConfig {
    /// Build from the configuration tree.
    pub fn from_config(config: &ferroma_core::config::Config) -> Self {
        SmtpClientConfig {
            hostname: config.server.hostname.clone(),
            port: 25,
            tls: TlsPolicy::Opportunistic,
            verify_certificates: true,
            auth: None,
            allow_cleartext_auth: false,
            connect_timeout: Duration::from_secs(config.queue.connect_timeout_secs.max(1)),
            read_timeout: Duration::from_secs(config.queue.connect_timeout_secs.max(1) * 2),
            session_timeout: Duration::from_secs(config.queue.delivery_timeout_secs.max(1)),
        }
    }

    /// Build the client for the configured outbound relay, when there is one.
    ///
    /// `None` when `[queue] relay_host` is unset, which is the direct-to-MX case.
    /// An unparseable `relay_tls` is rejected by `Config::validate`, so reaching
    /// here with one is impossible — but the fallback is `starttls`, never `none`.
    pub fn for_relay(config: &ferroma_core::config::Config) -> Option<(String, SmtpClientConfig)> {
        if !config.queue.relay_configured() {
            return None;
        }
        let host = config.queue.relay_host.clone().unwrap_or_default();
        let tls = TlsPolicy::from_config_name(&config.queue.relay_tls).unwrap_or(TlsPolicy::Required);
        let auth = match (&config.queue.relay_username, &config.queue.relay_password) {
            (Some(username), Some(password)) => Some(SmtpCredentials {
                username: username.clone(),
                password: password.clone(),
            }),
            _ => None,
        };
        let mut client = SmtpClientConfig::from_config(config);
        client.port = config.queue.relay_port;
        client.tls = tls;
        client.auth = auth;
        Some((host, client))
    }

    /// Require TLS.
    pub fn with_tls_required(mut self) -> Self {
        self.tls = TlsPolicy::Required;
        self
    }

    /// Connect to a non-standard port, for tests and for an internal relay.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Turn certificate verification off. Logs a warning at construction.
    pub fn without_certificate_verification(mut self) -> Self {
        tracing::warn!(
            "outbound SMTP certificate verification disabled: deliveries are encrypted but not authenticated"
        );
        self.verify_certificates = false;
        self
    }
}

/// The result of one delivery attempt to one recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The remote server accepted the message.
    Delivered {
        /// The final SMTP reply code (always `2xx`).
        code: Option<u16>,
        /// The final reply text.
        text: String,
        /// The host that accepted it.
        host: String,
    },
    /// The message may be delivered later. Retry.
    Temporary {
        /// The reply code, when there was one. `None` for a transport failure.
        code: Option<u16>,
        /// Why. Already sanitised for a log line.
        text: String,
        /// The host we were talking to, when we got that far.
        host: Option<String>,
    },
    /// The remote server refused the message for good. Bounce.
    Permanent {
        /// The reply code (always `5xx`).
        code: Option<u16>,
        /// The reply text.
        text: String,
        /// The host that refused it.
        host: Option<String>,
    },
}

impl DeliveryOutcome {
    /// The remote reply code, when there was one.
    pub fn code(&self) -> Option<u16> {
        match self {
            DeliveryOutcome::Delivered { code, .. }
            | DeliveryOutcome::Temporary { code, .. }
            | DeliveryOutcome::Permanent { code, .. } => *code,
        }
    }

    /// The reply text or the transport error.
    pub fn text(&self) -> &str {
        match self {
            DeliveryOutcome::Delivered { text, .. }
            | DeliveryOutcome::Temporary { text, .. }
            | DeliveryOutcome::Permanent { text, .. } => text,
        }
    }

    /// The host involved, when we got far enough to name one.
    pub fn host(&self) -> Option<&str> {
        match self {
            DeliveryOutcome::Delivered { host, .. } => Some(host),
            DeliveryOutcome::Temporary { host, .. }
            | DeliveryOutcome::Permanent { host, .. } => host.as_deref(),
        }
    }

    /// Whether the message was accepted.
    pub fn is_delivered(&self) -> bool {
        matches!(self, DeliveryOutcome::Delivered { .. })
    }

    /// Whether the queue should retry.
    pub fn is_temporary(&self) -> bool {
        matches!(self, DeliveryOutcome::Temporary { .. })
    }

    /// Whether the queue should give up and bounce.
    pub fn is_permanent(&self) -> bool {
        matches!(self, DeliveryOutcome::Permanent { .. })
    }

    /// The status string the queue writes and `delivery.updated` carries.
    pub fn status(&self) -> &'static str {
        match self {
            DeliveryOutcome::Delivered { .. } => "delivered",
            DeliveryOutcome::Temporary { .. } => "retry",
            DeliveryOutcome::Permanent { .. } => "failed",
        }
    }

    /// The equivalent [`FerromaError`], for callers that work in errors.
    pub fn as_error(&self) -> Option<FerromaError> {
        match self {
            DeliveryOutcome::Delivered { .. } => None,
            DeliveryOutcome::Temporary { text, .. } => FerromaError::Network(text.clone()).into(),
            DeliveryOutcome::Permanent { text, .. } => Some(FerromaError::Forbidden(text.clone())),
        }
    }

    /// A `Temporary` outcome for a transport-level failure.
    pub fn transport_failure(text: impl Into<String>, host: Option<&str>) -> Self {
        DeliveryOutcome::Temporary {
            code: None,
            text: sanitise(&text.into()),
            host: host.map(str::to_string),
        }
    }
}

/// Classify an SMTP reply code into an outcome, per RFC 5321 §4.2.1.
///
/// This is the single place the 2xx/4xx/5xx → delivered/retry/fail mapping lives,
/// so a caller can test the classification without a socket.
pub fn classify_reply(code: u16, text: &str, host: Option<&str>) -> DeliveryOutcome {
    let text = sanitise(text);
    if (200..300).contains(&code) {
        DeliveryOutcome::Delivered {
            code: Some(code),
            text,
            host: host.unwrap_or_default().to_string(),
        }
    } else if (400..500).contains(&code) {
        DeliveryOutcome::Temporary {
            code: Some(code),
            text,
            host: host.map(str::to_string),
        }
    } else {
        DeliveryOutcome::Permanent {
            code: Some(code),
            text,
            host: host.map(str::to_string),
        }
    }
}

/// Keep a remote reply from breaking our log lines or our database.
fn sanitise(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| *c != '\r' && *c != '\n' && *c != '\0')
        .collect();
    if cleaned.len() > 400 {
        cleaned.chars().take(400).collect()
    } else {
        cleaned
    }
}

/// The outbound SMTP client.
#[derive(Debug, Clone)]
pub struct SmtpClient {
    config: SmtpClientConfig,
}

impl SmtpClient {
    /// Build a client.
    pub fn new(config: SmtpClientConfig) -> Self {
        if !config.verify_certificates {
            tracing::warn!(
                hostname = %config.hostname,
                "outbound SMTP certificate verification is disabled"
            );
        }
        SmtpClient { config }
    }

    /// The configuration this client uses.
    pub fn config(&self) -> &SmtpClientConfig {
        &self.config
    }

    /// Deliver one message to one recipient at one host.
    ///
    /// Never returns `Err`: every failure *is* a delivery outcome, because "could
    /// not connect" and "the server said 550" both have to be recorded against the
    /// queue row rather than thrown away.
    pub async fn deliver(
        &self,
        host: &MxHost,
        addresses: &[IpAddr],
        sender: &str,
        recipient: &str,
        message: &[u8],
    ) -> DeliveryOutcome {
        let deadline = tokio::time::Instant::now() + self.config.session_timeout;
        let mut last: Option<DeliveryOutcome> = None;

        for address in addresses {
            let socket = SocketAddr::new(*address, self.config.port);
            let outcome = match tokio::time::timeout_at(deadline, self.attempt(socket, host, sender, recipient, message))
                .await
            {
                Ok(outcome) => outcome,
                Err(_) => DeliveryOutcome::transport_failure(
                    "the delivery attempt exceeded its deadline",
                    Some(&host.host),
                ),
            };

            match outcome {
                DeliveryOutcome::Temporary { .. } => {
                    // Try the next address of the same MX host: a dead IPv6 route
                    // should not cost us the delivery.
                    last = Some(outcome);
                }
                other => return other,
            }
        }

        last.unwrap_or_else(|| {
            DeliveryOutcome::transport_failure(
                format!("{} has no usable address", host.host),
                Some(&host.host),
            )
        })
    }

    /// One attempt against one socket address.
    async fn attempt(
        &self,
        socket: SocketAddr,
        host: &MxHost,
        sender: &str,
        recipient: &str,
        message: &[u8],
    ) -> DeliveryOutcome {
        let hostname = host.host.as_str();
        let stream = match tokio::time::timeout(
            self.config.connect_timeout,
            TcpStream::connect(socket),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                return DeliveryOutcome::transport_failure(
                    format!("cannot connect to {socket}: {e}"),
                    Some(hostname),
                )
            }
            Err(_) => {
                return DeliveryOutcome::transport_failure(
                    format!("connecting to {socket} timed out"),
                    Some(hostname),
                )
            }
        };
        let _ = stream.set_nodelay(true);

        let mut session = Session::new(stream, hostname, self.config.clone());
        session.run(sender, recipient, message).await
    }
}

// ---------------------------------------------------------------------------
// One client session
// ---------------------------------------------------------------------------

/// The stream a session reads and writes.
type Stream = Box<dyn AsyncStream>;

/// The traits a session stream must provide.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// State for one connected attempt.
struct Session {
    stream: Stream,
    config: SmtpClientConfig,
    host: String,
    /// The `EHLO` extension lines, upper-cased keywords only.
    extensions: Vec<String>,
    /// Whether the channel is encrypted — set by the implicit handshake or by
    /// `STARTTLS`, and consulted before any credential is sent.
    encrypted: bool,
}

impl Session {
    fn new<S: AsyncStream + 'static>(stream: S, host: &str, config: SmtpClientConfig) -> Self {
        Session {
            stream: Box::new(stream),
            config,
            host: host.to_string(),
            extensions: Vec::new(),
            encrypted: false,
        }
    }

    /// Run the whole dialogue.
    async fn run(&mut self, sender: &str, recipient: &str, message: &[u8]) -> DeliveryOutcome {
        // --- implicit TLS ---------------------------------------------
        // `SMTPS`: the handshake happens before the server greets us, and there is
        // no plaintext channel to fall back to.
        if self.config.tls.is_implicit() {
            if let Err(outcome) = self.handshake().await {
                return outcome;
            }
            self.encrypted = true;
        }

        // --- banner ---------------------------------------------------
        let banner = match self.read_reply().await {
            Ok(reply) => reply,
            Err(outcome) => return outcome,
        };
        if banner.0 != 220 {
            return classify_reply(banner.0, &banner.1, Some(&self.host));
        }

        // --- EHLO, falling back to HELO -------------------------------
        let ehlo = match self.command(&format!("EHLO {}", self.config.hostname)).await {
            Ok(reply) => reply,
            Err(outcome) => return outcome,
        };
        match ehlo.0 {
            250 => self.extensions = parse_extensions(&ehlo.1),
            // An old server that does not know EHLO: RFC 5321 §4.1.1.1 says fall
            // back to HELO rather than give up.
            _ => {
                let helo = match self.command(&format!("HELO {}", self.config.hostname)).await {
                    Ok(reply) => reply,
                    Err(outcome) => return outcome,
                };
                if helo.0 != 250 {
                    return classify_reply(helo.0, &helo.1, Some(&self.host));
                }
                self.extensions.clear();
            }
        }

        // --- STARTTLS -------------------------------------------------
        if self.config.tls.allows_tls() {
            let offers_starttls = self.has_extension("STARTTLS");
            if offers_starttls {
                match self.starttls().await {
                    Ok(()) => {}
                    Err(outcome) => return outcome,
                }
            } else if self.config.tls.is_required() {
                // A policy failure must be temporary: the message is fine, the
                // channel is not.
                return DeliveryOutcome::transport_failure(
                    format!(
                        "{} does not offer STARTTLS and the TLS policy requires it",
                        self.host
                    ),
                    Some(&self.host),
                );
            }
        }

        // --- envelope -------------------------------------------------
        // A relay that requires credentials is the only peer this client ever
        // authenticates to; an MX host never asks.
        if let Some(credentials) = self.config.auth.clone() {
            if !self.encrypted && !self.config.allow_cleartext_auth {
                return DeliveryOutcome::transport_failure(
                    format!(
                        "{} wants credentials but the channel is not encrypted; \
                         set queue.relay_tls to \"starttls\" or \"implicit\"",
                        self.host
                    ),
                    Some(&self.host),
                );
            }
            if let Err(outcome) = self.authenticate(&credentials).await {
                return outcome;
            }
        }

        let mail = match self.command(&format!("MAIL FROM:<{sender}>")).await {
            Ok(reply) => reply,
            Err(outcome) => return outcome,
        };
        if !(200..300).contains(&mail.0) {
            return classify_reply(mail.0, &mail.1, Some(&self.host));
        }

        let rcpt = match self.command(&format!("RCPT TO:<{recipient}>")).await {
            Ok(reply) => reply,
            Err(outcome) => return outcome,
        };
        if !(200..300).contains(&rcpt.0) {
            return classify_reply(rcpt.0, &rcpt.1, Some(&self.host));
        }

        let data = match self.command("DATA").await {
            Ok(reply) => reply,
            Err(outcome) => return outcome,
        };
        if data.0 != 354 {
            return classify_reply(data.0, &data.1, Some(&self.host));
        }

        // --- body -----------------------------------------------------
        if message.len() > MAX_DATA_BYTES {
            return DeliveryOutcome::transport_failure(
                "message is too large for the outbound client",
                Some(&self.host),
            );
        }
        if let Err(outcome) = self.write_body(message).await {
            return outcome;
        }

        let accepted = match self.read_reply().await {
            Ok(reply) => reply,
            Err(outcome) => return outcome,
        };

        // `QUIT` is polite: a server that logs it can tell a clean transaction from
        // a hung-up client. Its failure must not change the outcome.
        let _ = self.command("QUIT").await;

        classify_reply(accepted.0, &accepted.1, Some(&self.host))
    }

    /// Whether the peer advertised `name`.
    fn has_extension(&self, name: &str) -> bool {
        let wanted = name.to_ascii_uppercase();
        self.extensions.iter().any(|line| {
            line.split_whitespace()
                .next()
                .map(|keyword| keyword.eq_ignore_ascii_case(&wanted))
                .unwrap_or(false)
        })
    }

    /// Perform the `STARTTLS` upgrade in place.
    async fn starttls(&mut self) -> std::result::Result<(), DeliveryOutcome> {
        let reply = self.command("STARTTLS").await?;
        if reply.0 != 220 {
            if self.config.tls.is_required() {
                return Err(classify_reply(reply.0, &reply.1, Some(&self.host)));
            }
            return Err(DeliveryOutcome::transport_failure(
                format!("{} refused STARTTLS: {}", self.host, reply.1),
                Some(&self.host),
            ));
        }

        self.handshake().await?;
        self.encrypted = true;

        // RFC 3207 §4.2: the session state is reset, so ask again.
        let ehlo = self
            .command(&format!("EHLO {}", self.config.hostname))
            .await?;
        if ehlo.0 != 250 {
            return Err(classify_reply(ehlo.0, &ehlo.1, Some(&self.host)));
        }
        self.extensions = parse_extensions(&ehlo.1);
        Ok(())
    }

    /// The TLS handshake itself, on whatever the current stream is.
    ///
    /// Shared by `STARTTLS` and by implicit TLS, which differ only in when it
    /// happens and whether the dialogue restarts afterwards.
    async fn handshake(&mut self) -> std::result::Result<(), DeliveryOutcome> {
        // Take the plaintext stream back so the connector can own it.
        let stream = std::mem::replace(&mut self.stream, Box::new(tokio::io::empty()));
        let connector = match self.connector() {
            Ok(connector) => connector,
            Err(e) => {
                return Err(DeliveryOutcome::transport_failure(
                    format!("cannot build the TLS client: {e}"),
                    Some(&self.host),
                ))
            }
        };
        let server_name = match ServerName::try_from(self.host.clone()) {
            Ok(name) => name,
            Err(_) => {
                return Err(DeliveryOutcome::transport_failure(
                    format!("{} is not a valid TLS server name", self.host),
                    Some(&self.host),
                ))
            }
        };

        match tokio::time::timeout(self.config.connect_timeout, connector.connect(server_name, stream)).await {
            Ok(Ok(tls)) => {
                self.stream = Box::new(tls);
                Ok(())
            }
            Ok(Err(e)) => Err(DeliveryOutcome::transport_failure(
                format!("TLS handshake with {} failed: {e}", self.host),
                Some(&self.host),
            )),
            Err(_) => Err(DeliveryOutcome::transport_failure(
                format!("TLS handshake with {} timed out", self.host),
                Some(&self.host),
            )),
        }
    }

    /// Authenticate to a relay.
    ///
    /// `AUTH PLAIN` with an initial response, falling back to `AUTH LOGIN`. A
    /// rejection is a **temporary** failure, deliberately: a wrong password is a
    /// configuration mistake, and bouncing every queued message over it destroys
    /// mail that a corrected password would have delivered.
    async fn authenticate(
        &mut self,
        credentials: &SmtpCredentials,
    ) -> std::result::Result<(), DeliveryOutcome> {
        if !self.has_extension("AUTH") {
            return Err(DeliveryOutcome::transport_failure(
                format!(
                    "{} does not advertise AUTH but credentials are configured",
                    self.host
                ),
                Some(&self.host),
            ));
        }

        use base64::Engine as _;
        let engine = base64::engine::general_purpose::STANDARD;
        // `AUTH` with no mechanism list is legal; PLAIN is what relays support.
        let login = self.has_extension("LOGIN") && !self.has_extension("PLAIN");

        let reply = if login {
            let first = self
                .command(&format!("AUTH LOGIN {}", engine.encode(&credentials.username)))
                .await?;
            if first.0 != 334 {
                return Err(auth_failure(&self.host, first.0, &first.1));
            }
            self.command(&engine.encode(&credentials.password)).await?
        } else {
            let payload = engine.encode(format!(
                "\0{}\0{}",
                credentials.username, credentials.password
            ));
            self.command(&format!("AUTH PLAIN {payload}")).await?
        };

        if reply.0 != 235 {
            return Err(auth_failure(&self.host, reply.0, &reply.1));
        }
        Ok(())
    }

    /// Build the TLS connector for this delivery.
    fn connector(&self) -> Result<TlsConnector> {
        if self.config.verify_certificates {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(TlsConnector::from(Arc::new(config)))
        } else {
            // `with_custom_certificate_verifier` replaces the whole verifier, so the
            // danger must be explicit at every call site — which is why this branch
            // is a single function and the warning is logged here.
            let config = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification))
                .with_no_client_auth();
            Ok(TlsConnector::from(Arc::new(config)))
        }
    }

    /// Send one command and read its reply.
    async fn command(&mut self, line: &str) -> std::result::Result<(u16, String), DeliveryOutcome> {
        let bytes = format!("{line}\r\n");
        self.write_all(bytes.as_bytes()).await?;
        self.read_reply().await
    }

    /// Read a reply, following an SMTP continuation line (`250-…`).
    async fn read_reply(&mut self) -> std::result::Result<(u16, String), DeliveryOutcome> {
        let mut collected: Vec<String> = Vec::new();
        let mut code = 0u16;
        loop {
            let line = match tokio::time::timeout(self.config.read_timeout, self.read_line()).await {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => {
                    return Err(DeliveryOutcome::transport_failure(
                        format!("{} closed the connection", self.host),
                        Some(&self.host),
                    ))
                }
                Ok(Err(e)) => {
                    return Err(DeliveryOutcome::transport_failure(
                        format!("reading from {} failed: {e}", self.host),
                        Some(&self.host),
                    ))
                }
                Err(_) => {
                    return Err(DeliveryOutcome::transport_failure(
                        format!("{} did not reply in time", self.host),
                        Some(&self.host),
                    ))
                }
            };

            let (line_code, separator, rest) = match split_reply(&line) {
                Some(parts) => parts,
                None => {
                    return Err(DeliveryOutcome::transport_failure(
                        format!("{} sent a malformed reply", self.host),
                        Some(&self.host),
                    ))
                }
            };
            if code == 0 {
                code = line_code;
            }
            collected.push(rest.to_string());
            if separator == ' ' {
                break;
            }
            if collected.len() > 64 {
                return Err(DeliveryOutcome::transport_failure(
                    format!("{} sent an endless reply", self.host),
                    Some(&self.host),
                ));
            }
        }
        Ok((code, collected.join(" ")))
    }

    /// Read one CRLF-terminated line.
    async fn read_line(&mut self) -> std::io::Result<Option<String>> {
        use tokio::io::AsyncReadExt;
        let mut buffer: Vec<u8> = Vec::with_capacity(128);
        let mut byte = [0u8; 1];
        loop {
            if buffer.len() > MAX_REPLY_BYTES {
                return Ok(Some(String::from_utf8_lossy(&buffer).into_owned()));
            }
            let read = self.stream.read(&mut byte).await?;
            if read == 0 {
                return if buffer.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
                };
            }
            if byte[0] == b'\n' {
                return Ok(Some(String::from_utf8_lossy(&buffer).into_owned()));
            }
            buffer.push(byte[0]);
        }
    }

    /// Write the message with dot-stuffing, then the terminator.
    async fn write_body(&mut self, message: &[u8]) -> std::result::Result<(), DeliveryOutcome> {
        let stuffed = dot_stuff(message);
        self.write_all(&stuffed).await?;
        self.write_all(b".\r\n").await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> std::result::Result<(), DeliveryOutcome> {
        use tokio::io::AsyncWriteExt;
        match tokio::time::timeout(self.config.read_timeout, self.stream.write_all(bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(DeliveryOutcome::transport_failure(
                    format!("writing to {} failed: {e}", self.host),
                    Some(&self.host),
                ))
            }
            Err(_) => {
                return Err(DeliveryOutcome::transport_failure(
                    format!("writing to {} timed out", self.host),
                    Some(&self.host),
                ))
            }
        }
        match tokio::time::timeout(self.config.read_timeout, self.stream.flush()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(DeliveryOutcome::transport_failure(
                format!("flushing to {} failed: {e}", self.host),
                Some(&self.host),
            )),
            Err(_) => Err(DeliveryOutcome::transport_failure(
                format!("flushing to {} timed out", self.host),
                Some(&self.host),
            )),
        }
    }
}

/// Split `250-Text` / `250 Text` into its code, separator and text.
fn split_reply(line: &str) -> Option<(u16, char, &str)> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.len() < 3 {
        return None;
    }
    let code: u16 = line[..3].parse().ok()?;
    let separator = line[3..].chars().next().unwrap_or(' ');
    Some((code, separator, line[4.min(line.len())..].trim_end()))
}

/// The extension keywords of a multi-line `EHLO` reply.
///
/// The first line is the greeting ("mx.example.com greets you"), so it is dropped
/// when it does not look like a keyword — otherwise `SIZE 100` would be mistaken for
/// an extension named after the hostname.
fn parse_extensions(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|token| token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '='))
        .map(str::to_string)
        .collect()
}

/// A rejected `AUTH`, as a **temporary** failure.
///
/// Deliberately not `classify_reply`: a `535` there would be permanent, and a
/// mistyped password would bounce the whole queue instead of waiting for the
/// operator to fix it. The reply text never contains the password — it is the
/// server's own words.
fn auth_failure(host: &str, code: u16, text: &str) -> DeliveryOutcome {
    DeliveryOutcome::transport_failure(
        format!("{host} rejected AUTH: {code} {text}"),
        Some(host),
    )
}

/// Dot-stuff a message body and guarantee it ends with CRLF.
///
/// RFC 5321 §4.5.2: a line starting with `.` gains one more, and the body must end
/// with CRLF so the terminator is unambiguous.
pub fn dot_stuff(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 16);
    let mut rest = message;
    let mut last_ended_with_newline = false;

    while !rest.is_empty() {
        let (line, remainder) = match rest.iter().position(|b| *b == b'\n') {
            Some(index) => (&rest[..=index], &rest[index + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        let content = strip_eol(line);
        if content.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(content);
        out.extend_from_slice(b"\r\n");
        last_ended_with_newline = true;
        rest = remainder;
    }

    if !last_ended_with_newline {
        // An empty message still needs a CRLF so `.\r\n` terminates it.
        out.extend_from_slice(b"\r\n");
    }
    out
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

/// A certificate verifier that accepts anything.
///
/// Only reachable through [`SmtpClientConfig::without_certificate_verification`],
/// which logs a warning at construction and at every connection. It exists because
/// some operators need to talk to an internal relay with a self-signed certificate,
/// and the alternative — a silent `native-tls` style "insecure" flag — is worse.
#[derive(Debug)]
struct NoVerification;

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    /// The signature schemes a certificate may use.
    ///
    /// Listed explicitly rather than derived from the process-wide crypto provider:
    /// a `warn`-level operator decision to skip verification must not also depend on
    /// which provider happens to be installed.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn host(name: &str) -> MxHost {
        MxHost::new(10, name)
    }

    // ------------------------------------------------------------------
    // Classification
    // ------------------------------------------------------------------

    #[test]
    fn a_2xx_reply_is_a_delivery() {
        let outcome = classify_reply(250, "2.0.0 Ok: queued as ABC", Some("mx.example.net"));
        assert!(outcome.is_delivered());
        assert_eq!(outcome.code(), Some(250));
        assert_eq!(outcome.host(), Some("mx.example.net"));
        assert_eq!(outcome.status(), "delivered");
        assert!(outcome.as_error().is_none());
    }

    #[test]
    fn a_4xx_reply_is_temporary() {
        for code in [421, 450, 451, 452] {
            let outcome = classify_reply(code, "try later", Some("mx.example.net"));
            assert!(outcome.is_temporary(), "{code} must be temporary");
            assert!(!outcome.is_permanent(), "{code} must not be permanent");
            assert_eq!(outcome.status(), "retry");
            assert!(outcome.as_error().expect("error").is_temporary());
        }
    }

    #[test]
    fn a_5xx_reply_is_permanent() {
        for code in [500, 550, 551, 552, 553, 554] {
            let outcome = classify_reply(code, "no thanks", Some("mx.example.net"));
            assert!(outcome.is_permanent(), "{code} must be permanent");
            assert!(!outcome.is_temporary(), "{code} must not be temporary");
            assert_eq!(outcome.status(), "failed");
            assert!(!outcome.as_error().expect("error").is_temporary());
        }
    }

    #[test]
    fn a_transport_failure_is_always_temporary() {
        let outcome = DeliveryOutcome::transport_failure("connection reset", Some("mx.example.net"));
        assert!(outcome.is_temporary());
        assert!(!outcome.is_permanent(), "a network blip must never bounce mail");
        assert_eq!(outcome.code(), None);
        assert_eq!(outcome.host(), Some("mx.example.net"));
    }

    #[test]
    fn a_reply_with_crlf_is_sanitised_into_the_outcome() {
        let outcome = classify_reply(550, "evil\r\n250 Ok", Some("mx"));
        assert!(!outcome.text().contains('\n'));
        assert!(!outcome.text().contains('\r'));
    }

    #[test]
    fn a_very_long_reply_text_is_bounded() {
        let outcome = classify_reply(550, &"x".repeat(5000), Some("mx"));
        assert!(outcome.text().len() <= 400);
    }

    #[test]
    fn classification_without_a_host_still_works() {
        let outcome = classify_reply(250, "ok", None);
        assert_eq!(outcome.host(), Some(""));
    }

    // ------------------------------------------------------------------
    // Reply parsing
    // ------------------------------------------------------------------

    #[test]
    fn a_single_line_reply_parses() {
        assert_eq!(split_reply("250 Ok"), Some((250, ' ', "Ok")));
        assert_eq!(split_reply("250 Ok\r\n"), Some((250, ' ', "Ok")));
        assert_eq!(split_reply("354 End data"), Some((354, ' ', "End data")));
    }

    #[test]
    fn a_continuation_line_parses_with_its_separator() {
        assert_eq!(split_reply("250-SIZE 100"), Some((250, '-', "SIZE 100")));
        assert_eq!(split_reply("250-PIPELINING\r\n"), Some((250, '-', "PIPELINING")));
    }

    #[test]
    fn a_reply_without_text_parses() {
        assert_eq!(split_reply("250"), Some((250, ' ', "")));
        assert_eq!(split_reply("250 \r\n"), Some((250, ' ', "")));
    }

    #[test]
    fn a_malformed_reply_is_rejected() {
        assert_eq!(split_reply(""), None);
        assert_eq!(split_reply("25"), None);
        assert_eq!(split_reply("abc Ok"), None);
    }

    #[test]
    fn extension_keywords_are_extracted_from_a_multi_line_reply() {
        let text = "mx.example.net greets you SIZE 10485760 PIPELINING STARTTLS AUTH";
        let extensions = parse_extensions(text);
        assert!(extensions.iter().any(|e| e == "STARTTLS"), "{extensions:?}");
        assert!(extensions.iter().any(|e| e == "PIPELINING"));
    }

    #[test]
    fn extension_keywords_are_recognised_case_insensitively() {
        let extensions = parse_extensions("mx greets you SIZE 100 STARTTLS 8BITMIME");
        assert!(extensions.iter().any(|e| e == "SIZE"), "{extensions:?}");
        assert!(extensions.iter().any(|e| e == "STARTTLS"));
        assert!(extensions.iter().any(|e| e == "8BITMIME"));
        // A greeting line with dots is not an extension keyword.
        assert!(!extensions.iter().any(|e| e.contains('.')), "{extensions:?}");
    }

    #[test]
    fn has_extension_matches_on_the_keyword_only() {
        let session = |extensions: Vec<String>| Session {
            stream: Box::new(tokio::io::empty()),
            config: SmtpClientConfig::default(),
            host: "mx.test".to_string(),
            extensions,
            encrypted: false,
        };
        assert!(session(vec!["SIZE 100".into(), "STARTTLS".into()]).has_extension("STARTTLS"));
        assert!(session(vec!["starttls".into()]).has_extension("STARTTLS"));
        assert!(!session(vec!["SIZE 100".into()]).has_extension("STARTTLS"));
        // A keyword that merely *contains* the name must not match.
        assert!(!session(vec!["NOTSTARTTLS".into()]).has_extension("STARTTLS"));
    }

    // ------------------------------------------------------------------
    // Dot stuffing
    // ------------------------------------------------------------------

    #[test]
    fn a_leading_dot_is_stuffed() {
        assert_eq!(dot_stuff(b".hidden\r\n"), b"..hidden\r\n");
    }

    #[test]
    fn a_body_without_leading_dots_is_unchanged() {
        let body = b"From: a@b\r\nSubject: hi\r\n\r\nhello\r\n";
        assert_eq!(dot_stuff(body), body);
    }

    #[test]
    fn a_body_without_a_final_crlf_gains_one() {
        assert_eq!(dot_stuff(b"no newline"), b"no newline\r\n");
    }

    #[test]
    fn an_empty_body_becomes_a_single_crlf() {
        assert_eq!(dot_stuff(b""), b"\r\n");
    }

    #[test]
    fn every_line_gets_a_crlf() {
        assert_eq!(dot_stuff(b"a\nb\n"), b"a\r\nb\r\n");
    }

    #[test]
    fn a_line_of_only_a_dot_is_stuffed() {
        assert_eq!(dot_stuff(b".\r\n"), b"..\r\n");
    }

    #[test]
    fn stuffing_then_unstuffing_round_trips() {
        let original = b"From: a@b\r\n\r\n.hidden\r\n..double\r\nnormal\r\n";
        let stuffed = dot_stuff(original);
        // Undo it the way the receiving server would.
        let mut restored: Vec<u8> = Vec::new();
        for line in stuffed.split_inclusive(|b| *b == b'\n') {
            let content = strip_eol(line);
            let content = if content.first() == Some(&b'.') {
                &content[1..]
            } else {
                content
            };
            restored.extend_from_slice(content);
            restored.extend_from_slice(b"\r\n");
        }
        assert_eq!(restored, original);
    }

    #[test]
    fn strip_eol_handles_every_ending() {
        assert_eq!(strip_eol(b"x\r\n"), b"x");
        assert_eq!(strip_eol(b"x\n"), b"x");
        assert_eq!(strip_eol(b"x"), b"x");
        assert_eq!(strip_eol(b""), b"");
    }

    // ------------------------------------------------------------------
    // TLS policy
    // ------------------------------------------------------------------

    #[test]
    fn the_tls_policy_reports_itself() {
        assert_eq!(TlsPolicy::Opportunistic.as_str(), "opportunistic");
        assert_eq!(TlsPolicy::Required.as_str(), "required");
        assert_eq!(TlsPolicy::Disabled.as_str(), "disabled");
        assert!(TlsPolicy::Required.is_required());
        assert!(!TlsPolicy::Opportunistic.is_required());
        assert!(TlsPolicy::Opportunistic.allows_tls());
        assert!(!TlsPolicy::Disabled.allows_tls());
    }

    #[test]
    fn the_default_config_is_opportunistic_with_verification_on() {
        let config = SmtpClientConfig::default();
        assert_eq!(config.tls, TlsPolicy::Opportunistic);
        assert!(config.verify_certificates);
        assert!(config.connect_timeout > Duration::ZERO);
    }

    #[test]
    fn the_config_builders_flip_the_right_switches() {
        let config = SmtpClientConfig::default().with_tls_required();
        assert_eq!(config.tls, TlsPolicy::Required);
        let config = SmtpClientConfig::default().without_certificate_verification();
        assert!(!config.verify_certificates);
    }

    #[test]
    fn the_config_can_be_built_from_the_tree() {
        let mut tree = ferroma_core::config::Config::default();
        tree.server.hostname = "mx.example.com".into();
        tree.queue.connect_timeout_secs = 11;
        tree.queue.delivery_timeout_secs = 222;
        let config = SmtpClientConfig::from_config(&tree);
        assert_eq!(config.hostname, "mx.example.com");
        assert_eq!(config.connect_timeout, Duration::from_secs(11));
        assert_eq!(config.session_timeout, Duration::from_secs(222));
    }

    #[test]
    fn a_client_exposes_its_configuration() {
        let client = SmtpClient::new(SmtpClientConfig::default());
        assert!(client.config().verify_certificates);
        assert_eq!(client.config().port, 25);
    }

    #[test]
    fn the_port_can_be_overridden() {
        let config = SmtpClientConfig::default().with_port(2525);
        assert_eq!(config.port, 2525);
        assert_eq!(SmtpClientConfig::from_config(&ferroma_core::config::Config::default()).port, 25);
    }

    // ------------------------------------------------------------------
    // In-process test server
    // ------------------------------------------------------------------

    /// What the scripted server should do for one connection.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Script {
        /// A clean transaction ending in `250`.
        Accept,
        /// `RCPT TO` is deferred with `450`.
        DeferRcpt,
        /// `RCPT TO` is refused with `550`.
        RejectRcpt,
        /// Accept, then drop the connection in the middle of `DATA`.
        DropInData,
        /// Advertise `STARTTLS`, then answer `220` and speak garbage instead of
        /// completing a handshake.
        FakeStartTls,
        /// A server that does not know `EHLO`.
        HeloOnly,
        /// Advertise `AUTH PLAIN LOGIN` and accept whatever is offered — a relay
        /// that takes credentials.
        AuthRequired,
        /// Advertise `AUTH` and refuse the credentials with `535`.
        AuthRejected,
    }

    /// A hand-written SMTP server for one connection.
    ///
    /// Bound to `127.0.0.1:0`; returns the address it is listening on.
    async fn scripted_server(script: Script) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let handle = tokio::spawn(async move {
            let mut transcript = Vec::new();
            let (mut socket, _) = listener.accept().await.expect("accept");
            socket
                .write_all(b"220 mx.test ESMTP ready\r\n")
                .await
                .expect("banner");

            let mut line = String::new();
            let mut in_data = false;
            loop {
                line.clear();
                match read_line(&mut socket, &mut line).await {
                    Some(0) | None => break,
                    Some(_) => {}
                }
                let trimmed = line.trim_end().to_string();
                if in_data {
                    transcript.push(format!("data:{trimmed}"));
                    if trimmed == "." {
                        socket
                            .write_all(b"250 2.0.0 Ok: queued as TEST\r\n")
                            .await
                            .expect("accept data");
                        in_data = false;
                    }
                    continue;
                }
                transcript.push(trimmed.clone());

                if trimmed.to_ascii_uppercase().starts_with("EHLO") {
                    if script == Script::HeloOnly {
                        socket
                            .write_all(b"500 5.5.1 Command unrecognized\r\n")
                            .await
                            .expect("ehlo");
                        continue;
                    }
                    let reply = match script {
                        Script::FakeStartTls => {
                            "250-mx.test greets you\r\n250-SIZE 10485760\r\n250 STARTTLS\r\n"
                        }
                        Script::AuthRequired | Script::AuthRejected => {
                            "250-mx.test greets you\r\n250-SIZE 10485760\r\n250 AUTH PLAIN LOGIN\r\n"
                        }
                        _ => "250-mx.test greets you\r\n250-SIZE 10485760\r\n250 PIPELINING\r\n",
                    };
                    socket.write_all(reply.as_bytes()).await.expect("ehlo");
                } else if trimmed.to_ascii_uppercase().starts_with("HELO") {
                    socket.write_all(b"250 mx.test\r\n").await.expect("helo");
                } else if trimmed.to_ascii_uppercase().starts_with("STARTTLS") {
                    socket
                        .write_all(b"220 2.0.0 Ready to start TLS\r\n")
                        .await
                        .expect("starttls");
                    if script == Script::FakeStartTls {
                        // Not a TLS record: the client's handshake must fail, which is
                        // what proves it really tried.
                        socket
                            .write_all(b"this is not a TLS record\r\n")
                            .await
                            .expect("garbage");
                        break;
                    }
                } else if trimmed.to_ascii_uppercase().starts_with("AUTH ") {
                    match script {
                        Script::AuthRejected => socket
                            .write_all(b"535 5.7.8 Authentication credentials invalid\r\n")
                            .await
                            .expect("auth"),
                        _ => socket
                            .write_all(b"235 2.7.0 Authentication successful\r\n")
                            .await
                            .expect("auth"),
                    }
                } else if trimmed.to_ascii_uppercase().starts_with("MAIL FROM") {
                    socket.write_all(b"250 2.1.0 Ok\r\n").await.expect("mail");
                } else if trimmed.to_ascii_uppercase().starts_with("RCPT TO") {
                    match script {
                        Script::DeferRcpt => {
                            socket
                                .write_all(b"450 4.2.0 Mailbox busy, try later\r\n")
                                .await
                                .expect("rcpt")
                        }
                        Script::RejectRcpt => {
                            socket
                                .write_all(b"550 5.1.1 User unknown\r\n")
                                .await
                                .expect("rcpt")
                        }
                        _ => socket.write_all(b"250 2.1.5 Ok\r\n").await.expect("rcpt"),
                    }
                } else if trimmed.eq_ignore_ascii_case("DATA") {
                    if script == Script::DropInData {
                        break;
                    }
                    socket
                        .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                        .await
                        .expect("data");
                    in_data = true;
                } else if trimmed.eq_ignore_ascii_case("QUIT") {
                    let _ = socket.write_all(b"221 2.0.0 Bye\r\n").await;
                    break;
                } else {
                    socket.write_all(b"500 5.5.2 Unknown\r\n").await.expect("unknown");
                }
            }
            
            transcript
        });
        (address, handle)
    }

    /// Read one CRLF-terminated line, returning the number of bytes read.
    async fn read_line(
        socket: &mut tokio::net::TcpStream,
        out: &mut String,
    ) -> Option<usize> {
        use tokio::io::AsyncReadExt;
        let mut total = 0usize;
        let mut byte = [0u8; 1];
        loop {
            match socket.read(&mut byte).await {
                Ok(0) => return if total == 0 { None } else { Some(total) },
                Ok(_) => {
                    total += 1;
                    out.push(byte[0] as char);
                    if byte[0] == b'\n' {
                        return Some(total);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    async fn deliver_to(script: Script, config: SmtpClientConfig) -> DeliveryOutcome {
        let (server, _handle) = scripted_server(script).await;
        let client = SmtpClient::new(config.with_port(server.port()));
        client
            .deliver(
                &host("mx.test"),
                &[server.ip()],
                "alice@example.com",
                "bob@example.net",
                b"From: alice@example.com\r\nTo: bob@example.net\r\nSubject: hi\r\n\r\nbody\r\n",
            )
            .await
    }

    /// Deliver with a credentialled client, and hand back what the server saw.
    async fn deliver_with_auth(
        script: Script,
        config: SmtpClientConfig,
    ) -> (DeliveryOutcome, Vec<String>) {
        let (server, handle) = scripted_server(script).await;
        let client = SmtpClient::new(config.with_port(server.port()));
        let outcome = client
            .deliver(
                &host("relay.test"),
                &[server.ip()],
                "alice@example.com",
                "bob@example.net",
                b"Subject: hi\r\n\r\nbody\r\n",
            )
            .await;
        (outcome, handle.await.expect("server task"))
    }

    fn relay_credentials() -> SmtpCredentials {
        SmtpCredentials {
            username: "apikey".into(),
            password: "s3cret".into(),
        }
    }

    /// A client for a relay, with the cleartext guard relaxed: the scripted server
    /// speaks no TLS, and the guard has its own test below.
    fn relay_config() -> SmtpClientConfig {
        SmtpClientConfig {
            auth: Some(relay_credentials()),
            allow_cleartext_auth: true,
            tls: TlsPolicy::Disabled,
            ..SmtpClientConfig::default()
        }
    }

    #[tokio::test]
    async fn credentials_are_sent_with_auth_plain_before_the_envelope() {
        let (outcome, transcript) = deliver_with_auth(Script::AuthRequired, relay_config()).await;
        assert!(outcome.is_delivered(), "{outcome:?}");

        use base64::Engine as _;
        let expected = base64::engine::general_purpose::STANDARD.encode("\0apikey\0s3cret");
        let position = transcript
            .iter()
            .position(|line| line.starts_with("AUTH "))
            .expect("AUTH was sent");
        assert_eq!(transcript[position], format!("AUTH PLAIN {expected}"));
        // A relay expects it after `EHLO` and before `MAIL FROM`.
        let mail = transcript
            .iter()
            .position(|line| line.starts_with("MAIL FROM"))
            .expect("envelope");
        assert!(position < mail, "{transcript:?}");
    }

    #[tokio::test]
    async fn credentials_are_never_sent_over_an_unencrypted_channel() {
        let config = SmtpClientConfig {
            auth: Some(relay_credentials()),
            tls: TlsPolicy::Disabled,
            ..SmtpClientConfig::default()
        };
        let (outcome, transcript) = deliver_with_auth(Script::AuthRequired, config).await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert!(outcome.text().contains("not encrypted"), "{outcome:?}");
        assert!(
            !transcript.iter().any(|line| line.starts_with("AUTH")),
            "nothing may be sent before the channel is encrypted: {transcript:?}"
        );
    }

    #[tokio::test]
    async fn a_rejected_auth_is_temporary_and_never_echoes_the_password() {
        let (outcome, _) = deliver_with_auth(Script::AuthRejected, relay_config()).await;
        assert!(
            outcome.is_temporary(),
            "a wrong password must not bounce the queue: {outcome:?}"
        );
        assert!(outcome.text().contains("rejected AUTH"), "{outcome:?}");
        assert!(!outcome.text().contains("s3cret"), "{outcome:?}");
    }

    #[tokio::test]
    async fn implicit_tls_starts_before_the_greeting() {
        // `implicit` against a server that speaks plaintext: the handshake cannot
        // succeed, and the failure must be temporary — never a bounce.
        let config = SmtpClientConfig {
            tls: TlsPolicy::Implicit,
            ..SmtpClientConfig::default()
        };
        let outcome = deliver_to(Script::Accept, config).await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert!(outcome.text().to_lowercase().contains("tls"), "{outcome:?}");
    }

    #[test]
    fn tls_policy_names_round_trip_for_a_relay() {
        assert_eq!(TlsPolicy::from_config_name("starttls"), Some(TlsPolicy::Required));
        assert_eq!(TlsPolicy::from_config_name("implicit"), Some(TlsPolicy::Implicit));
        assert_eq!(TlsPolicy::from_config_name("none"), Some(TlsPolicy::Disabled));
        assert_eq!(TlsPolicy::from_config_name("IMPLICIT"), Some(TlsPolicy::Implicit));
        assert_eq!(TlsPolicy::from_config_name("perhaps"), None);
        assert!(TlsPolicy::Implicit.is_implicit() && TlsPolicy::Implicit.is_required());
    }

    #[test]
    fn the_relay_client_is_built_from_the_queue_block() {
        let mut config = ferroma_core::config::Config::default();
        assert!(SmtpClientConfig::for_relay(&config).is_none());

        config.queue.relay_host = Some("smtp.relay.example".into());
        config.queue.relay_port = 465;
        config.queue.relay_tls = "implicit".into();
        config.queue.relay_username = Some("apikey".into());
        config.queue.relay_password = Some("s3cret".into());

        let (relay_host, client) = SmtpClientConfig::for_relay(&config).expect("a relay client");
        assert_eq!(relay_host, "smtp.relay.example");
        assert_eq!(client.port, 465);
        assert_eq!(client.tls, TlsPolicy::Implicit);
        assert_eq!(client.auth.as_ref().map(|c| c.username.as_str()), Some("apikey"));
        // The password must not be printable, not here and not from the client.
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("s3cret"), "{rendered}");
    }

    #[tokio::test]
    async fn a_clean_delivery_is_reported_as_delivered() {        let outcome = deliver_to(Script::Accept, SmtpClientConfig::default()).await;
        assert!(outcome.is_delivered(), "{outcome:?}");
        assert_eq!(outcome.code(), Some(250));
        assert!(outcome.text().contains("Ok: queued as TEST"), "{outcome:?}");
        assert_eq!(outcome.host(), Some("mx.test"));
    }

    #[tokio::test]
    async fn a_450_on_rcpt_is_temporary() {
        let outcome = deliver_to(Script::DeferRcpt, SmtpClientConfig::default()).await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert_eq!(outcome.code(), Some(450));
        assert!(outcome.text().contains("Mailbox busy"));
    }

    #[tokio::test]
    async fn a_550_on_rcpt_is_permanent() {
        let outcome = deliver_to(Script::RejectRcpt, SmtpClientConfig::default()).await;
        assert!(outcome.is_permanent(), "{outcome:?}");
        assert_eq!(outcome.code(), Some(550));
        assert!(outcome.text().contains("User unknown"));
    }

    #[tokio::test]
    async fn a_server_that_drops_the_connection_mid_data_is_temporary() {
        let outcome = deliver_to(Script::DropInData, SmtpClientConfig::default()).await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert_eq!(outcome.code(), None);
        assert!(!outcome.is_permanent(), "a dropped connection must never bounce mail");
    }

    #[tokio::test]
    async fn starttls_is_attempted_when_the_server_advertises_it() {
        // The scripted server answers `220` and then sends a non-TLS record, so the
        // handshake cannot succeed. A temporary failure proves the client really
        // tried to upgrade rather than ignoring the advertisement.
        let outcome = deliver_to(Script::FakeStartTls, SmtpClientConfig::default()).await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert!(
            outcome.text().to_lowercase().contains("tls"),
            "expected a TLS failure, got {outcome:?}"
        );
        assert!(!outcome.is_permanent(), "a TLS failure must not bounce mail");
    }

    #[tokio::test]
    async fn a_required_tls_policy_fails_temporarily_when_starttls_is_absent() {
        let outcome = deliver_to(
            Script::Accept,
            SmtpClientConfig::default().with_tls_required(),
        )
        .await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert!(outcome.text().contains("does not offer STARTTLS"), "{outcome:?}");
        assert!(!outcome.is_permanent(), "a policy failure must not bounce mail");
    }

    #[tokio::test]
    async fn a_disabled_tls_policy_delivers_in_the_clear() {
        let config = SmtpClientConfig {
            tls: TlsPolicy::Disabled,
            ..SmtpClientConfig::default()
        };
        let outcome = deliver_to(Script::Accept, config).await;
        assert!(outcome.is_delivered(), "{outcome:?}");
    }

    #[tokio::test]
    async fn the_client_falls_back_to_helo_for_a_server_without_ehlo() {
        let config = SmtpClientConfig {
            tls: TlsPolicy::Disabled,
            ..SmtpClientConfig::default()
        };
        let outcome = deliver_to(Script::HeloOnly, config).await;
        assert!(outcome.is_delivered(), "{outcome:?}");
    }

    #[tokio::test]
    async fn the_transcript_shows_a_well_formed_dialogue() {
        let (address, handle) = scripted_server(Script::Accept).await;
        let client = SmtpClient::new(SmtpClientConfig::default().with_port(address.port()));
        let outcome = client
            .deliver(
                &host("mx.test"),
                &[address.ip()],
                "alice@example.com",
                "bob@example.net",
                b"From: alice@example.com\r\nSubject: hi\r\n\r\nbody\r\n",
            )
            .await;
        assert!(outcome.is_delivered(), "{outcome:?}");

        let transcript = handle.await.expect("join");
        assert!(transcript[0].starts_with("EHLO "), "{transcript:?}");
        assert_eq!(transcript[1], "MAIL FROM:<alice@example.com>");
        assert_eq!(transcript[2], "RCPT TO:<bob@example.net>");
        assert_eq!(transcript[3], "DATA");
        assert!(transcript.iter().any(|line| line == "data:From: alice@example.com"));
        assert!(transcript.iter().any(|line| line == "data:."), "{transcript:?}");
        assert_eq!(transcript.last().map(String::as_str), Some("QUIT"));
    }

    #[tokio::test]
    async fn a_leading_dot_body_is_stuffed_on_the_wire() {
        let (address, handle) = scripted_server(Script::Accept).await;
        let client = SmtpClient::new(SmtpClientConfig::default().with_port(address.port()));
        let outcome = client
            .deliver(
                &host("mx.test"),
                &[address.ip()],
                "alice@example.com",
                "bob@example.net",
                b"From: a@b\r\n\r\n.hidden\r\n",
            )
            .await;
        assert!(outcome.is_delivered(), "{outcome:?}");
        let transcript = handle.await.expect("join");
        assert!(
            transcript.iter().any(|line| line == "data:..hidden"),
            "the body must be dot-stuffed on the wire: {transcript:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_temporary() {
        // Bind and immediately drop, so the port is closed.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        drop(listener);

        let client = SmtpClient::new(SmtpClientConfig::default().with_port(address.port()));
        let outcome = client
            .deliver(
                &host("mx.test"),
                &[address.ip()],
                "alice@example.com",
                "bob@example.net",
                b"From: a@b\r\n\r\nbody\r\n",
            )
            .await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert!(outcome.text().contains("cannot connect") || outcome.text().contains("timed out"));
    }

    #[tokio::test]
    async fn a_host_with_no_addresses_is_temporary() {
        let client = SmtpClient::new(SmtpClientConfig::default());
        let outcome = client
            .deliver(
                &host("mx.test"),
                &[],
                "alice@example.com",
                "bob@example.net",
                b"body\r\n",
            )
            .await;
        assert!(outcome.is_temporary());
        assert!(outcome.text().contains("no usable address"), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_short_connect_timeout_produces_a_temporary_failure() {
        // 203.0.113.0/24 is TEST-NET-3: reserved, never routed, so the connect hangs.
        let config = SmtpClientConfig {
            connect_timeout: Duration::from_millis(50),
            session_timeout: Duration::from_millis(200),
            ..SmtpClientConfig::default()
        };
        let client = SmtpClient::new(config);
        let outcome = client
            .deliver(
                &host("mx.test"),
                &["203.0.113.1".parse().expect("ip")],
                "alice@example.com",
                "bob@example.net",
                b"body\r\n",
            )
            .await;
        assert!(outcome.is_temporary(), "{outcome:?}");
        assert!(!outcome.is_permanent());
    }

    #[tokio::test]
    async fn the_client_tries_every_address_of_a_host() {
        // The first address is unroutable; the second is the live test server. The
        // delivery must succeed, which is the whole point of iterating addresses.
        let (address, _handle) = scripted_server(Script::Accept).await;
        let mut config = SmtpClientConfig::default().with_port(address.port());
        config.connect_timeout = Duration::from_millis(200);
        let client = SmtpClient::new(config);
        let outcome = client
            .deliver(
                &host("mx.test"),
                &["203.0.113.1".parse().expect("ip"), address.ip()],
                "alice@example.com",
                "bob@example.net",
                b"From: a@b\r\n\r\nbody\r\n",
            )
            .await;
        assert!(outcome.is_delivered(), "{outcome:?}");
    }
}
