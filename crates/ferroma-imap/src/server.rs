//! The IMAP listener: accept loops, TLS, and the per-connection driver.
//!
//! Four things happen here and nowhere else:
//!
//! * **Binding.** A plaintext listener (normally `143`) and, when TLS is
//!   configured, an implicit-TLS listener (`993`).
//! * **`STARTTLS`.** After the upgrade the connection keeps its session but the
//!   session returns to `NotAuthenticated` with the mailbox dropped, as RFC 2595
//!   §3 requires.
//! * **Byte plumbing.** [`TcpInput`] turns a socket into the lines the session
//!   reads, rejecting a line that is not CRLF-terminated and refusing to grow
//!   without bound.
//! * **Structured logs.** Every command is logged with `connection_id`,
//!   `remote_ip`, `user`, `command`, `duration` and `result` — and never with a
//!   password or a message body.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ferroma_core::FerromaError;
use ferroma_events::EventBus;
use ferroma_storage::repository::Repositories;
use ferroma_storage::Maildir;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::auth::ServiceAuthenticator;
use crate::config::ImapServerConfig;
use crate::response::Response;
use crate::session::{
    Authenticator, ImapSession, SessionContext, SessionFlow, SessionInput, SessionOutput,
};

/// The largest command line accepted before CRLF, excluding literals.
///
/// IMAP command lines are short; 8 KiB is generous for `LOGIN`, `SELECT` and a
/// `FETCH` item list, and it stops a peer from making the server buffer
/// megabytes without ever sending a terminator.
pub const MAX_COMMAND_LINE: usize = 8 * 1024;

/// The longest literal `APPEND` may declare when the server config does not say.
pub const MAX_LITERAL: u64 = 26_214_400;

/// A running IMAP server.
pub struct ImapServer {
    config: Arc<ImapServerConfig>,
    repos: Arc<Repositories>,
    maildir: Arc<Maildir>,
    authenticator: Arc<dyn Authenticator>,
    events: Option<Arc<EventBus>>,
    tls: Option<TlsAcceptor>,
    connections: Arc<AtomicU64>,
    next_connection_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for ImapServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapServer")
            .field("config", &self.config)
            .field("tls", &self.tls.is_some())
            .field("connections", &self.connections.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ImapServer {
    /// Build a server over the platform's storage.
    pub fn new(
        config: ImapServerConfig,
        repos: Arc<Repositories>,
        maildir: Arc<Maildir>,
    ) -> Result<Self, FerromaError> {
        config
            .validate()
            .map_err(FerromaError::Config)?;
        let hasher = ferroma_auth::PasswordHasher::default();
        let authenticator = Arc::new(ServiceAuthenticator::new(repos.clone(), hasher));
        Ok(ImapServer {
            config: Arc::new(config),
            repos,
            maildir,
            authenticator,
            events: None,
            tls: None,
            connections: Arc::new(AtomicU64::new(0)),
            next_connection_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Replace the credential checker (tests, and any deployment that
    /// authenticates some other way).
    pub fn with_authenticator(mut self, authenticator: Arc<dyn Authenticator>) -> Self {
        self.authenticator = authenticator;
        self
    }

    /// Attach the event bus, enabling `IDLE` push.
    pub fn with_events(mut self, events: Arc<EventBus>) -> Self {
        self.events = Some(events);
        self
    }

    /// Install a TLS acceptor, enabling `STARTTLS` and the IMAPS listener.
    pub fn with_tls(mut self, acceptor: TlsAcceptor) -> Self {
        self.tls = Some(acceptor);
        self
    }

    /// The server's configuration.
    pub fn config(&self) -> &ImapServerConfig {
        &self.config
    }

    /// How many connections are open right now.
    pub fn connection_count(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    /// Build the per-connection context, sharing everything that outlives one
    /// client.
    fn context(&self, remote_ip: String, connection_id: u64) -> SessionContext {
        let mut context = SessionContext::new(
            self.config.clone(),
            self.repos.clone(),
            self.maildir.clone(),
            self.authenticator.clone(),
        )
        .with_identity(connection_id, remote_ip);
        if let Some(events) = &self.events {
            context = context.with_events(events.clone());
        }
        context
    }

    /// Bind the configured listeners.
    ///
    /// Returns the plaintext listener and, when TLS is configured and
    /// `imaps_port` is set, the implicit-TLS listener.
    pub async fn bind(&self) -> Result<(TcpListener, Option<TcpListener>), FerromaError> {
        let plain = TcpListener::bind(self.config.listen_address())
            .await
            .map_err(|err| {
                FerromaError::Io(io::Error::new(
                    err.kind(),
                    format!("cannot bind {}: {err}", self.config.listen_address()),
                ))
            })?;
        let imaps = match self.config.imaps_address() {
            Some(address) if self.config.imaps_available() && self.tls.is_some() => {
                let listener = TcpListener::bind(&address).await.map_err(|err| {
                    FerromaError::Io(io::Error::new(
                        err.kind(),
                        format!("cannot bind {address}: {err}"),
                    ))
                })?;
                Some(listener)
            }
            _ => None,
        };
        Ok((plain, imaps))
    }

    /// Serve on the configured listeners until `shutdown` fires.
    ///
    /// Both listeners are driven by one `select!` loop, so a shutdown signal
    /// stops accepting immediately.
    pub async fn serve(
        self: Arc<Self>,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), FerromaError> {
        let (plain, imaps) = self.bind().await?;
        let plain_address = plain
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| self.config.listen_address());
        tracing::info!(
            address = %plain_address,
            imaps = self.config.imaps_available(),
            "imap listener started"
        );

        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!("imap listener stopping");
                    return Ok(());
                }
                accepted = plain.accept() => {
                    let (stream, peer) = accepted.map_err(FerromaError::Io)?;
                    self.spawn_connection(stream, peer, false);
                }
                accepted = async {
                    match &imaps {
                        Some(listener) => listener.accept().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let (stream, peer) = accepted.map_err(FerromaError::Io)?;
                    self.spawn_connection(stream, peer, true);
                }
            }
        }
    }

    /// Serve one already-accepted stream. Exposed so tests and the `ferroma`
    /// binary can drive a connection without the accept loop.
    pub async fn serve_stream(
        self: &Arc<Self>,
        stream: TcpStream,
        implicit_tls: bool,
    ) -> Result<(), FerromaError> {
        let peer = stream
            .peer_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_default();
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        self.connections.fetch_add(1, Ordering::Relaxed);
        let result = self
            .handle_connection(stream, peer.clone(), connection_id, implicit_tls)
            .await;
        self.connections.fetch_sub(1, Ordering::Relaxed);
        if let Err(err) = &result {
            tracing::debug!(
                connection_id,
                remote_ip = %peer,
                error = %err,
                "imap connection ended with an error"
            );
        }
        result
    }

    /// Spawn a connection task.
    fn spawn_connection(self: &Arc<Self>, stream: TcpStream, peer: std::net::SocketAddr, implicit_tls: bool) {
        let server = self.clone();
        tokio::spawn(async move {
            let connection_id = server.next_connection_id.fetch_add(1, Ordering::Relaxed);
            server.connections.fetch_add(1, Ordering::Relaxed);
            let result = server
                .handle_connection(stream, peer.to_string(), connection_id, implicit_tls)
                .await;
            server.connections.fetch_sub(1, Ordering::Relaxed);
            if let Err(err) = result {
                tracing::debug!(
                    connection_id,
                    remote_ip = %peer,
                    error = %err,
                    "imap connection ended with an error"
                );
            }
        });
    }

    /// The whole life of one connection.
    async fn handle_connection(
        &self,
        stream: TcpStream,
        remote_ip: String,
        connection_id: u64,
        implicit_tls: bool,
    ) -> Result<(), FerromaError> {
        let _ = stream.set_nodelay(true);
        let context = self.context(remote_ip, connection_id);
        let starttls_available =
            !implicit_tls && self.config.starttls_available() && self.tls.is_some();
        let mut session = ImapSession::new(context, implicit_tls, starttls_available);

        let mut connection = Connection::Plain {
            halves: Some(PlainHalves::new(stream)),
        };

        if implicit_tls {
            let acceptor = self
                .tls
                .clone()
                .ok_or_else(|| FerromaError::Tls("no TLS acceptor is configured".into()))?;
            let Some(plain) = connection.take_plain_socket() else {
                return Ok(());
            };
            let tls = acceptor
                .accept(plain)
                .await
                .map_err(|err| FerromaError::Tls(format!("handshake failed: {err}")))?;
            connection = Connection::Tls {
                halves: Some(TlsHalves::new(tls)),
            };
        }
        // The greeting goes out before anything else.
        connection.greet(&session).await?;

        loop {
            let flow = connection.run_session(&mut session).await?;

            match flow {
                SessionFlow::Close | SessionFlow::Continue => return Ok(()),
                SessionFlow::UpgradeTls => {
                    let Some(acceptor) = self.tls.clone() else {
                        return Ok(());
                    };
                    let Some(plain) = connection.take_plain_socket() else {
                        // Already encrypted: there is nothing to upgrade.
                        return Ok(());
                    };
                    let tls = match acceptor.accept(plain).await {
                        Ok(tls) => tls,
                        Err(err) => {
                            tracing::debug!(error = %err, "imap STARTTLS handshake failed");
                            return Ok(());
                        }
                    };
                    // RFC 2595 §3: the session starts over from NotAuthenticated.
                    session.upgrade_to_tls();
                    connection = Connection::Tls {
                        halves: Some(TlsHalves::new(tls)),
                    };
                }
            }
        }
    }
}

/// The plain socket's two halves, kept whole so `STARTTLS` can rebuild the
/// socket for the TLS layer to take over.
struct PlainHalves {
    read: Option<tokio::net::tcp::OwnedReadHalf>,
    write: Option<tokio::net::tcp::OwnedWriteHalf>,
}

impl PlainHalves {
    fn new(stream: TcpStream) -> Self {
        let (read, write) = stream.into_split();
        PlainHalves {
            read: Some(read),
            write: Some(write),
        }
    }
}

/// An encrypted connection's two halves.
struct TlsHalves {
    reader: Option<tokio::io::ReadHalf<tokio_rustls::server::TlsStream<TcpStream>>>,
    writer: Option<tokio::io::WriteHalf<tokio_rustls::server::TlsStream<TcpStream>>>,
}

impl TlsHalves {
    fn new(stream: tokio_rustls::server::TlsStream<TcpStream>) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        TlsHalves {
            reader: Some(reader),
            writer: Some(writer),
        }
    }
}

/// One connection's transport, either plaintext or TLS.
enum Connection {
    /// A bare TCP stream, split into owned halves.
    Plain {
        /// `None` once the halves have been consumed by a `STARTTLS` upgrade.
        halves: Option<PlainHalves>,
    },
    /// A TLS session over TCP.
    Tls {
        /// `None` once the halves have been consumed.
        halves: Option<TlsHalves>,
    },
}

impl Connection {
    /// Run one command-dispatch pass over this connection.
    ///
    /// The read half is moved into a reader task (which is what keeps the
    /// session's read future `Send`) and handed back when the pass ends, so the
    /// connection keeps its halves across passes.
    async fn run_session(&mut self, session: &mut ImapSession) -> Result<SessionFlow, FerromaError> {
        match self {
            Connection::Plain { halves } => {
                let Some(halves) = halves.as_mut() else {
                    return Ok(SessionFlow::Close);
                };
                let Some(read) = halves.read.take() else {
                    return Ok(SessionFlow::Close);
                };
                let Some(mut write) = halves.write.take() else {
                    return Ok(SessionFlow::Close);
                };
                let mut input = TcpInput::new(read);
                let flow = {
                    // The output holds the write half for this pass only, and the
                    // session flushes after every command: that is what gets a
                    // reply to a waiting client instead of holding the bytes
                    // until the connection ends.
                    let mut output = BufferedOutput::new(Some(&mut write));
                    session.run(&mut input, &mut output).await
                };
                halves.read = input.take_reader().await;
                halves.write = Some(write);
                flow
            }
            Connection::Tls { halves } => {
                let Some(halves) = halves.as_mut() else {
                    return Ok(SessionFlow::Close);
                };
                let Some(read) = halves.reader.take() else {
                    return Ok(SessionFlow::Close);
                };
                let Some(mut write) = halves.writer.take() else {
                    return Ok(SessionFlow::Close);
                };
                let mut input = TcpInput::new(read);
                let flow = {
                    let mut output = BufferedOutput::new(Some(&mut write));
                    session.run(&mut input, &mut output).await
                };
                halves.reader = input.take_reader().await;
                halves.writer = Some(write);
                flow
            }
        }
    }

    /// Write the greeting, before the session loop starts.
    async fn greet(&mut self, session: &ImapSession) -> Result<(), FerromaError> {
        let mut output = BufferedOutput::new(self.writer());
        session.greet(&mut output);
        output.flush().await
    }

    /// The write half, as a [`ResponseWriter`] the session's output can hold.
    fn writer(&mut self) -> Option<&mut dyn ResponseWriter> {
        match self {
            Connection::Plain { halves } => halves
                .as_mut()
                .and_then(|halves| halves.write.as_mut())
                .map(|write| write as &mut dyn ResponseWriter),
            Connection::Tls { halves } => halves
                .as_mut()
                .and_then(|halves| halves.writer.as_mut())
                .map(|write| write as &mut dyn ResponseWriter),
        }
    }



    /// Take the plain socket back, or `None` when this is a TLS connection or
    /// the halves are already gone.
    fn take_plain_socket(&mut self) -> Option<TcpStream> {
        let Connection::Plain { halves } = self else {
            return None;
        };
        let mut taken = halves.take()?;
        let read = taken.read.take()?;
        let write = taken.write.take()?;
        read.reunite(write).ok()
    }
}

/// Anything a connection can push response bytes to.
///
/// A trait object cannot name `AsyncWrite` and `Send` together, so the pair is
/// combined here — that is all this trait is for.
pub trait ResponseWriter: tokio::io::AsyncWrite + Unpin + Send {}

impl<T> ResponseWriter for T where T: tokio::io::AsyncWrite + Unpin + Send {}

/// A [`SessionOutput`] that batches responses and pushes them out on `flush`.
///
/// The session flushes after every command, so a `FETCH` of 5000 messages still
/// costs one `write` per command rather than 5000 — while a client that waits
/// for its reply is never left hanging.
struct BufferedOutput<'a> {
    bytes: Vec<u8>,
    /// `None` once the connection has been taken over; writes then become no-ops.
    writer: Option<&'a mut dyn ResponseWriter>,
}

impl<'a> BufferedOutput<'a> {
    /// Wrap a connection's write half.
    fn new(writer: Option<&'a mut dyn ResponseWriter>) -> Self {
        BufferedOutput {
            bytes: Vec::new(),
            writer,
        }
    }
}

impl SessionOutput for BufferedOutput<'_> {
    fn send(&mut self, response: &Response) {
        self.bytes.extend_from_slice(response.to_wire().as_bytes());
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    async fn flush(&mut self) -> Result<(), FerromaError> {
        // The buffered octets are moved out first so the borrow of `self.writer`
        // and the borrow of `self.bytes` never overlap.
        let bytes = std::mem::take(&mut self.bytes);

        match self.writer.as_mut() {
            Some(writer) => write_out(*writer, &bytes).await.map_err(FerromaError::Io),
            None => Ok(()),
        }
    }
}

/// Write `bytes` to a stream and flush it.
async fn write_out<S: tokio::io::AsyncWrite + Unpin + ?Sized>(
    stream: &mut S,
    bytes: &[u8],
) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    stream.write_all(bytes).await?;
    stream.flush().await
}

/// A line channel's receiving end, as a [`futures_util::Stream`].
///
/// `tokio`'s `mpsc::Receiver` has no `Stream` implementation without the
/// `tokio-stream` crate, and the workspace does not depend on it; this is the
/// four-line adapter instead.
struct LineReceiver<T> {
    inner: tokio::sync::mpsc::Receiver<T>,
}

impl<T> futures_util::Stream for LineReceiver<T> {
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        self.inner.poll_recv(cx)
    }
}

/// Reads CRLF-terminated lines from a stream, one per `read_line`.
///
/// The reader is moved into a task that publishes lines through a bounded
/// channel. That keeps the session's read future `Send` whatever the stream is,
/// and [`TcpInput::take_reader`] takes the stream back afterwards, which is what
/// a `STARTTLS` upgrade needs.
pub struct TcpInput<T> {
    lines: futures_util::stream::BoxStream<'static, Result<Option<Vec<u8>>, FerromaError>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    returned: Option<tokio::sync::oneshot::Receiver<T>>,
    /// The signal that asks the reader task to stop.
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    max_line: usize,
}

impl<T> TcpInput<T>
where
    T: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    /// Wrap a read half.
    pub fn new(reader: T) -> Self {
        TcpInput::with_max_line(reader, MAX_COMMAND_LINE)
    }

    /// Wrap a read half, capping the length of a line.
    pub fn with_max_line(reader: T, max_line: usize) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let (returner, returned) = tokio::sync::oneshot::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(reader_task(reader, sender, returner, stop_rx, max_line));
        TcpInput {
            lines: Box::pin(LineReceiver { inner: receiver }),
            handle: Some(handle),
            returned: Some(returned),
            stop: Some(stop),
            max_line,
        }
    }

    /// The largest line this reader accepts.
    pub fn max_line(&self) -> usize {
        self.max_line
    }

    /// Stop the reader task and take the stream back.
    ///
    /// The reader is asked to stop rather than aborted: it must reach a point
    /// where returning the stream is safe, and an abort would leave it owned by
    /// a cancelled future. The reader selects between a socket read and this
    /// signal, so it notices immediately.
    pub async fn take_reader(&mut self) -> Option<T> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let returned = self.returned.take()?;
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
        returned.await.ok()
    }
}

/// The reader task: one line per channel message, then the reader back.
///
/// It stops on the `stop` signal, on a closed channel, or at end of stream — and
/// in every case it hands the reader back first.
async fn reader_task<T>(
    mut reader: T,
    sender: tokio::sync::mpsc::Sender<Result<Option<Vec<u8>>, FerromaError>>,
    returner: tokio::sync::oneshot::Sender<T>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    max_line: usize,
) where
    T: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    loop {
        let mut buffer: Vec<u8> = Vec::new();
        let outcome = loop {
            let mut byte = [0u8; 1];
            let read = tokio::select! {
                biased;
                _ = &mut stop => {
                    let _ = returner.send(reader);
                    return;
                }
                read = tokio::io::AsyncReadExt::read(&mut reader, &mut byte) => read,
            };
            match read {
                Ok(0) => break Ok(if buffer.is_empty() { None } else { Some(buffer) }),
                Ok(_) if byte[0] == b'\n' => {
                    if buffer.last() == Some(&b'\r') {
                        buffer.pop();
                    }
                    break Ok(Some(buffer));
                }
                Ok(_) => {
                    buffer.push(byte[0]);
                    if buffer.len() > max_line {
                        break Err(FerromaError::Protocol(format!(
                            "command line longer than {max_line} bytes"
                        )));
                    }
                }
                Err(err) => break Err(FerromaError::Io(err)),
            }
        };
        let finished = matches!(outcome, Ok(None) | Err(_));
        if sender.send(outcome).await.is_err() || finished {
            let _ = returner.send(reader);
            return;
        }
    }
}

impl<T> SessionInput for TcpInput<T>
where
    T: Send,
{
    async fn read_line(&mut self) -> Result<Option<Vec<u8>>, FerromaError> {
        match futures_util::StreamExt::next(&mut self.lines).await {
            Some(Ok(line)) => Ok(line),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}



/// Load a TLS acceptor from a PEM certificate bundle and a PEM private key.
///
/// The workspace's rustls build has no PEM parser of its own, so the two PEM
/// blocks are decoded here: a certificate bundle may hold several `CERTIFICATE`
/// blocks and the key may be PKCS#8 (`PRIVATE KEY`) or PKCS#1 (`RSA PRIVATE
/// KEY`).
pub fn tls_acceptor_from_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<TlsAcceptor, FerromaError> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer};

    let certs: Vec<CertificateDer<'static>> = pem_blocks(cert_pem, "CERTIFICATE")
        .into_iter()
        .map(|bytes| CertificateDer::from(bytes).into_owned())
        .collect();
    if certs.is_empty() {
        return Err(FerromaError::Tls(
            "the certificate bundle holds no CERTIFICATE block".into(),
        ));
    }

    let key = match pem_blocks(key_pem, "PRIVATE KEY").into_iter().next() {
        Some(bytes) => PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes)),
        None => match pem_blocks(key_pem, "RSA PRIVATE KEY").into_iter().next() {
            Some(bytes) => PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(bytes)),
            None => match pem_blocks(key_pem, "EC PRIVATE KEY").into_iter().next() {
                Some(_) => {
                    return Err(FerromaError::Tls(
                        "SEC1 (EC PRIVATE KEY) keys are not supported; convert to PKCS#8".into(),
                    ))
                }
                None => {
                    return Err(FerromaError::Tls(
                        "the key file holds no PRIVATE KEY or RSA PRIVATE KEY block".into(),
                    ))
                }
            },
        },
    };

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|err| FerromaError::Tls(format!("certificate or key is unusable: {err}")))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Extract every PEM block with the given label.
fn pem_blocks(pem: &[u8], label: &str) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(pem);
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = text.as_ref();
    while let Some(start) = rest.find(&begin) {
        let after_begin = &rest[start + begin.len()..];
        let Some(stop) = after_begin.find(&end) else {
            break;
        };
        let body: String = after_begin[..stop]
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        if let Some(bytes) = base64_decode(&body) {
            out.push(bytes);
        }
        rest = &after_begin[stop + end.len()..];
    }
    out
}

/// Decode standard base64, ignoring padding problems.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn base64_decodes_a_known_value() {
        assert_eq!(base64_decode("aGVsbG8=").expect("valid"), b"hello".to_vec());
        assert!(base64_decode("!!!!").is_none());
    }

    #[test]
    fn pem_blocks_extract_every_certificate() {
        let pem = b"-----BEGIN CERTIFICATE-----\n\
                    aGVsbG8=\n\
                    -----END CERTIFICATE-----\n\
                    some other text\n\
                    -----BEGIN CERTIFICATE-----\n\
                    d29ybGQ=\n\
                    -----END CERTIFICATE-----\n";
        let blocks = pem_blocks(pem, "CERTIFICATE");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], b"hello".to_vec());
        assert_eq!(blocks[1], b"world".to_vec());
    }

    #[test]
    fn pem_blocks_ignore_a_truncated_block() {
        let pem = b"-----BEGIN CERTIFICATE-----\naGVsbG8=\n";
        assert!(pem_blocks(pem, "CERTIFICATE").is_empty());
    }

    #[test]
    fn pem_blocks_ignore_other_labels() {
        let pem = b"-----BEGIN RSA PRIVATE KEY-----\naGVsbG8=\n-----END RSA PRIVATE KEY-----\n";
        assert!(pem_blocks(pem, "CERTIFICATE").is_empty());
        assert_eq!(pem_blocks(pem, "RSA PRIVATE KEY").len(), 1);
    }

    fn tls_error(cert: &[u8], key: &[u8]) -> FerromaError {
        match tls_acceptor_from_pem(cert, key) {
            Ok(_) => panic!("the PEM must be rejected"),
            Err(err) => err,
        }
    }

    #[test]
    fn tls_acceptor_rejects_a_missing_certificate() {
        let err = tls_error(b"", b"");
        assert!(err.to_string().contains("CERTIFICATE"));
    }

    #[test]
    fn tls_acceptor_rejects_a_missing_key() {
        let cert = b"-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----\n";
        let err = tls_error(cert, b"");
        assert!(err.to_string().contains("PRIVATE KEY"));
    }

    #[test]
    fn tls_acceptor_rejects_a_sec1_key_with_advice() {
        let cert = b"-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----\n";
        let key = b"-----BEGIN EC PRIVATE KEY-----\naGVsbG8=\n-----END EC PRIVATE KEY-----\n";
        let err = tls_error(cert, key);
        assert!(err.to_string().contains("PKCS#8"));
    }

    #[test]
    fn tls_acceptor_rejects_garbage_certificates() {
        let cert = b"-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----\n";
        let key = b"-----BEGIN PRIVATE KEY-----\naGVsbG8=\n-----END PRIVATE KEY-----\n";
        // `hello` is not a DER certificate, so rustls must refuse it.
        assert!(tls_acceptor_from_pem(cert, key).is_err());
    }

    /// Read every line a reader produces, ending with a `None`.
    async fn drain<T>(mut input: TcpInput<T>) -> Result<Vec<Option<Vec<u8>>>, FerromaError>
    where
        T: Send,
    {
        let mut lines = Vec::new();
        loop {
            let line = input.read_line().await?;
            let done = line.is_none();
            lines.push(line);
            if done {
                return Ok(lines);
            }
        }
    }

    #[tokio::test]
    async fn tcp_input_splits_crlf_lines() {
        let (mut client, server) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            let _ = client.write_all(b"a NOOP\r\na LOGOUT\r\n").await;
            let _ = client.shutdown().await;
        });
        let lines = drain(TcpInput::new(server)).await.expect("reads");
        assert_eq!(lines[0].as_deref(), Some(&b"a NOOP"[..]));
        assert_eq!(lines[1].as_deref(), Some(&b"a LOGOUT"[..]));
        assert!(lines[2].is_none());
        writer.await.expect("writer");
    }

    #[tokio::test]
    async fn tcp_input_tolerates_bare_lf() {
        let (mut client, server) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            let _ = client.write_all(b"a NOOP\nb NOOP\n").await;
            let _ = client.shutdown().await;
        });
        let lines = drain(TcpInput::new(server)).await.expect("reads");
        assert_eq!(lines[0].as_deref(), Some(&b"a NOOP"[..]));
        assert_eq!(lines[1].as_deref(), Some(&b"b NOOP"[..]));
        writer.await.expect("writer");
    }

    #[tokio::test]
    async fn tcp_input_returns_a_truncated_final_line() {
        let (mut client, server) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            let _ = client.write_all(b"a NOOP").await;
            let _ = client.shutdown().await;
        });
        let lines = drain(TcpInput::new(server)).await.expect("reads");
        assert_eq!(lines[0].as_deref(), Some(&b"a NOOP"[..]));
        assert!(lines[1].is_none());
        writer.await.expect("writer");
    }

    #[tokio::test]
    async fn tcp_input_rejects_an_over_long_line() {
        let long = format!("a {}\r\n", "x".repeat(MAX_COMMAND_LINE + 10));
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = client.write_all(long.as_bytes()).await;
        });
        let mut input = TcpInput::new(server);
        let err = input.read_line().await.expect_err("must refuse");
        assert!(err.to_string().contains("longer than"));
    }

    #[tokio::test]
    async fn tcp_input_max_line_is_configurable() {
        let (mut client, server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let _ = client.write_all(b"a bbbbbbbb\r\n").await;
        });
        let mut input = TcpInput::with_max_line(server, 4);
        assert_eq!(input.max_line(), 4);
        assert!(input.read_line().await.is_err());
    }

    #[tokio::test]
    async fn tcp_input_take_reader_hands_the_stream_back() {
        let (mut client, server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let _ = client.write_all(b"a NOOP\r\n").await;
        });
        let mut input = TcpInput::new(server);
        let line = input.read_line().await.expect("reads");
        assert_eq!(line.as_deref(), Some(&b"a NOOP"[..]));
        assert!(input.take_reader().await.is_some());
        // A second call finds nothing left to hand back.
        assert!(input.take_reader().await.is_none());
    }

    #[tokio::test]
    async fn buffered_output_buffers_then_flushes() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let mut output = BufferedOutput::new(Some(&mut server));
        output.send(&Response::exists(1));
        output.send_raw(b"* 1 FETCH (UID 1)\r\n");
        assert_eq!(
            output.bytes.len(),
            "* 1 EXISTS\r\n".len() + "* 1 FETCH (UID 1)\r\n".len()
        );
        output.flush().await.expect("flush");
        assert!(output.bytes.is_empty());
        drop(output);
        drop(server);
        let mut buffer = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut client, &mut buffer).await;
        assert_eq!(
            String::from_utf8_lossy(&buffer),
            "* 1 EXISTS\r\n* 1 FETCH (UID 1)\r\n"
        );
    }

    #[tokio::test]
    async fn connection_writes_through_a_plain_tcp_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            let mut connection = Connection::Plain {
                halves: Some(PlainHalves::new(socket)),
            };
            let mut output = BufferedOutput::new(connection.writer());
            output.send(&Response::exists(2));
            output.flush().await.expect("flush");
        });
        let mut client = TcpStream::connect(address).await.expect("connect");
        let mut buffer = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut client, &mut buffer).await;
        assert_eq!(String::from_utf8_lossy(&buffer), "* 2 EXISTS\r\n");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn connection_recovers_its_plain_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let client = tokio::spawn(async move { TcpStream::connect(address).await });
        let (socket, _) = listener.accept().await.expect("accept");
        let mut connection = Connection::Plain {
            halves: Some(PlainHalves::new(socket)),
        };
        let recovered = connection.take_plain_socket().expect("plain socket");
        assert!(recovered.peer_addr().is_ok());
        // A second call has nothing left.
        assert!(connection.take_plain_socket().is_none());
        let _ = client.await;
    }

    #[tokio::test]
    async fn a_tls_connection_has_no_plain_socket() {
        // No handshake is needed to prove the accessor refuses.
        let mut connection = Connection::Tls { halves: None };
        assert!(connection.take_plain_socket().is_none());
            let mut output = BufferedOutput::new(connection.writer());
        output.send(&Response::exists(1));
        output.flush().await.expect("flush is a no-op");
    }
}

#[cfg(test)]
mod probe_transport {
    use super::*;
    use crate::session::SessionOutput;

    #[tokio::test]
    async fn the_greeting_reaches_a_real_client() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = Connection::Plain {
                halves: Some(PlainHalves::new(socket)),
            };
            let mut output = BufferedOutput::new(connection.writer());
            output.send(&Response::exists(7));
            output.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let (read, _write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            reader.read_line(&mut line),
        )
        .await
        .expect("the client must receive the greeting")
        .unwrap();
        assert_eq!(line, "* 7 EXISTS\r\n");
        server.await.unwrap();
    }

}
