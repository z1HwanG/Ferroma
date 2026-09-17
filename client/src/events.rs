//! The realtime event socket: framing, routing and reconnection
//! (`docs/fcp.md` §8, specification §23).
//!
//! ```text
//! GET /api/v1/client/events?cursor=1841      Upgrade: websocket
//!   {"type":"hello","protocol_version":1,"heartbeat_secs":30,"last_seq":1841}
//!   {"seq":1842,"id":"0f4c…","at":"…","scope":"user:7","type":"mail.received", …}
//! ```
//!
//! Three pieces, deliberately separable:
//!
//! * [`parse_frame`] turns one text frame into a [`ServerFrame`]. It is total —
//!   it never panics, whatever a hostile or buggy server sends — and everything
//!   it cannot place becomes [`ServerFrame::Unknown`] rather than an error, so a
//!   server that grows a new event kind cannot wedge an older client.
//! * [`EventRouter`] turns a frame into the [`RouterAction`]s the client owes.
//!   It is pure, synchronous and clock-free, so every routing rule is a unit test.
//! * [`EventClient`] and [`EventConnection`] own the socket: connect, ping/pong,
//!   read, close. The socket is the only part that needs a runtime.
//!
//! # The cursor is the source of truth, not the socket
//!
//! §8 states it plainly, and this module is built around it. A frame may be
//! duplicated, replayed after a reconnect, or arrive out of order; a gap in the
//! sequence means the bus dropped events for this subscriber. In every one of
//! those cases the answer is the same: run a sync from the **stored cursor**, and
//! treat what the socket carried as a hint that something happened. That is why
//! [`EventRouter`] reports [`RouterAction::SyncNow`] for a non-increasing `seq`
//! and for `{"replay_gap":true}`, and why [`EventConnection::should_ping`] is
//! only a liveness probe and never a reason to trust the stream.
//!
//! # What needs no TLS of its own
//!
//! The connect feature of `tokio-tungstenite` is deliberately off in this
//! workspace (the crate is built with `handshake` and `rustls-tls-webpki-roots`
//! only, so `connect_async` does not exist). The TCP connection is therefore
//! dialled here, and the handshake is done over that connected stream:
//! `client_async` for `ws://`, `client_async_tls_with_config` for `wss://`. Both
//! live behind one private enum, so nothing else in the module cares which one it
//! got.

use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::{ClientRequestBuilder, Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::api::CLIENT_PROTOCOL_VERSION;
use crate::error::{ClientError, ClientResult};

/// The heartbeat assumed when the server's `hello` does not name one (§8: 30 s).
pub const DEFAULT_HEARTBEAT_SECS: u64 = 30;

/// The frame the client sends when it has been silent for a heartbeat (§8).
///
/// It is the application-level ping the server parses as `{"type":"ping"}`, not
/// the WebSocket control frame of the same name.
pub const PING_FRAME: &str = r#"{"type":"ping"}"#;

/// The frame that answers a server `ping` (§8).
pub const PONG_FRAME: &str = r#"{"type":"pong"}"#;

/// Every event name §8 defines, in the order the document lists them.
///
/// A `type` outside this list is not an error: it becomes
/// [`ServerFrame::Unknown`] and routes to [`RouterAction::Ignore`], because a
/// newer server may add events and an older client must keep its cursor moving.
pub const EVENT_KINDS: [&str; 10] = [
    "mail.received",
    "mail.sent",
    "mail.deleted",
    "mail.read",
    "mail.flag_changed",
    "mail.moved",
    "draft.created",
    "draft.updated",
    "delivery.updated",
    "device.revoked",
];

/// How long dialling the socket may take before it is a timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the WebSocket handshake may take once the TCP connection is up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// The exponent `delay` stops at, so a hostile `attempt` cannot overflow.
const MAX_BACKOFF_EXPONENT: u32 = 16;

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Bounded exponential backoff with optional jitter (`docs/fcp.md` §11).
///
/// Applied to reconnect attempts, one schedule per account, so a server that is
/// down for maintenance does not turn every client in the world into a
/// thundering herd the moment it comes back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Backoff {
    /// The delay before the first reconnect attempt.
    pub base: Duration,
    /// The ceiling for a single delay.
    pub max: Duration,
    /// The multiplier applied per attempt. `2.0` unless the caller changes it.
    pub factor: f64,
    /// The fraction of the delay that is randomised, `0.0`..=`1.0`. `0.0` makes
    /// [`Backoff::delay`] deterministic.
    pub jitter: f64,
}

impl Backoff {
    /// A backoff with `factor = 2.0` and `jitter = 0.25`.
    pub fn new(base: Duration, max: Duration) -> Self {
        Backoff {
            base,
            max,
            factor: 2.0,
            jitter: 0.25,
        }
    }

    /// A backoff that never randomises — deterministic, which is what the tests
    /// (and anyone reproducing a reconnect storm from a log) need.
    pub fn without_jitter(base: Duration, max: Duration) -> Self {
        Backoff {
            base,
            max,
            factor: 2.0,
            jitter: 0.0,
        }
    }

    /// The delay before reconnect attempt `attempt` (1-based, so `delay(1)` is
    /// the wait after the first failure).
    ///
    /// Exponential in [`factor`](Backoff::factor) and capped at
    /// [`max`](Backoff::max); randomised by ± [`jitter`](Backoff::jitter) when
    /// jitter is positive. With jitter the result may exceed `max` by that
    /// fraction — the cap bounds the schedule, not each individual sleep.
    ///
    /// `attempt == 0` is treated as the first attempt. A non-finite or degenerate
    /// `factor`/`jitter` falls back to the documented values rather than
    /// producing a panic or a zero delay.
    pub fn delay(&self, attempt: u32) -> Duration {
        let base_ms = millis(self.base);
        let max_ms = millis(self.max);
        let factor = if self.factor.is_finite() && self.factor >= 1.0 {
            self.factor
        } else {
            2.0
        };

        let exponent = attempt.saturating_sub(1).min(MAX_BACKOFF_EXPONENT);
        let scaled = (base_ms as f64) * factor.powi(exponent as i32);
        let mut millis = if scaled.is_finite() && scaled < max_ms as f64 {
            scaled as u64
        } else {
            max_ms
        };
        if millis > max_ms {
            millis = max_ms;
        }

        let jitter = if self.jitter.is_finite() {
            self.jitter.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if jitter > 0.0 && millis > 0 {
            let spread = (millis as f64 * jitter) as u64;
            if spread > 0 {
                let roll = rand::random::<u64>() % (spread.saturating_mul(2).saturating_add(1));
                millis = millis.saturating_sub(spread).saturating_add(roll);
            }
        }

        Duration::from_millis(millis)
    }
}

impl Default for Backoff {
    /// One second, doubling to a ceiling of sixty seconds, with jitter.
    fn default() -> Self {
        Backoff::new(Duration::from_secs(1), Duration::from_secs(60))
    }
}

/// A [`Duration`] in whole milliseconds, saturating at `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// The first frame of every session (§8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The protocol version the server will speak.
    pub protocol_version: u32,
    /// How often the server pings — and how often the client should.
    pub heartbeat_secs: u64,
    /// The newest sequence the bus had when the socket opened; `None` on an idle
    /// bus. It is *not* a cursor: the stored cursor is what a reconnect sends.
    pub last_seq: Option<i64>,
}

impl Hello {
    /// The heartbeat as a [`Duration`], never zero.
    pub fn heartbeat(&self) -> Duration {
        Duration::from_secs(self.heartbeat_secs.max(1))
    }
}

/// One event as it arrived (§8).
///
/// Every field but [`kind`](EventFrame::kind) is optional, because a server that
/// grows a shorter payload must not break a client that reads a longer one. The
/// untouched JSON is kept in [`raw`](EventFrame::raw) so the notification and
/// outbox layers can read fields this struct does not name.
#[derive(Debug, Clone, PartialEq)]
pub struct EventFrame {
    /// The global, per-user sequence number.
    pub seq: Option<i64>,
    /// The event id, for de-duplication.
    pub id: Option<String>,
    /// When the server published it (RFC 3339, UTC).
    pub at: Option<String>,
    /// The scope it was published to, e.g. `user:7`.
    pub scope: Option<String>,
    /// The dotted wire name, e.g. `mail.received`.
    pub kind: String,
    /// The mailbox it concerns.
    pub mailbox_id: Option<i64>,
    /// The message it concerns.
    pub message_id: Option<i64>,
    /// The `From:` header, already formatted for display.
    pub from: Option<String>,
    /// The decoded subject.
    pub subject: Option<String>,
    /// A short, body-free preview for the notification banner.
    pub snippet: Option<String>,
    /// Whether the bus flagged a replay gap on this frame.
    pub replay_gap: bool,
    /// The frame exactly as it arrived.
    pub raw: Value,
}

/// One frame off the socket.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    /// The opening frame; the connection is usable from here on.
    Hello(Hello),
    /// One event. Boxed: it is by far the largest variant.
    Event(Box<EventFrame>),
    /// A liveness probe; answer it with [`EventConnection::send_pong`].
    Ping,
    /// The answer to our own [`EventConnection::send_ping`].
    Pong,
    /// The bus dropped events for this subscriber: sync now (§8).
    ReplayGap,
    /// A frame this client build does not understand. Never an error.
    Unknown(Value),
}

/// Parse one text frame from the realtime socket.
///
/// Accepts `{"type":"hello",…}`, `{"type":"ping"}`, `{"type":"pong"}`,
/// `{"replay_gap":true}` (with or without an enclosing `type`), one of the
/// [`EVENT_KINDS`], a bare `"ping"`/`"pong"` string, and anything else JSON —
/// which becomes [`ServerFrame::Unknown`] and never an error. A frame that is not
/// JSON at all is [`ClientError::Parse`].
///
/// It never panics, whatever the peer sends: no indexing, no `unwrap`, no
/// assumption about the shape of a field beyond "it is there and it is a string
/// or a number".
pub fn parse_frame(text: &str) -> ClientResult<ServerFrame> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ClientError::parse("the realtime socket sent an empty frame"));
    }
    // Some proxies and test harnesses rewrite the JSON away entirely.
    if trimmed.eq_ignore_ascii_case("ping") {
        return Ok(ServerFrame::Ping);
    }
    if trimmed.eq_ignore_ascii_case("pong") {
        return Ok(ServerFrame::Pong);
    }

    let value: Value = serde_json::from_str(trimmed).map_err(|err| {
        ClientError::parse(format!(
            "the realtime socket sent a frame that is not JSON: {err}"
        ))
    })?;

    if let Some(frame) = bare_string_frame(&value) {
        return Ok(frame);
    }

    if value.is_object() {
        // `replay_gap` wins over `type`: §8 says the frame may arrive with or
        // without an enclosing type, and the flag is the signal.
        if value
            .get("replay_gap")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(ServerFrame::ReplayGap);
        }

        if let Some(kind) = value.get("type").and_then(Value::as_str) {
            match kind {
                "hello" => return Ok(ServerFrame::Hello(hello_from(&value))),
                "ping" => return Ok(ServerFrame::Ping),
                "pong" => return Ok(ServerFrame::Pong),
                // The server's own name for the frame, which is a signal whether or
                // not it also carries the boolean.
                "replay_gap" => return Ok(ServerFrame::ReplayGap),
                _ if is_event_kind(kind) => {
                    return Ok(ServerFrame::Event(Box::new(event_frame_from(&value))))
                }
                _ => {}
            }
        }
    }

    Ok(ServerFrame::Unknown(value))
}

/// A bare `"ping"` / `"pong"` JSON *string* frame.
fn bare_string_frame(value: &Value) -> Option<ServerFrame> {
    let text = value.as_str()?;
    if text.eq_ignore_ascii_case("ping") {
        Some(ServerFrame::Ping)
    } else if text.eq_ignore_ascii_case("pong") {
        Some(ServerFrame::Pong)
    } else {
        None
    }
}

/// Whether `kind` is one of the event names §8 defines.
pub fn is_event_kind(kind: &str) -> bool {
    EVENT_KINDS.contains(&kind)
}

/// Read the `hello` fields, falling back to the documented defaults.
fn hello_from(value: &Value) -> Hello {
    Hello {
        protocol_version: value
            .get("protocol_version")
            .and_then(Value::as_u64)
            .map(|version| version as u32)
            .unwrap_or(CLIENT_PROTOCOL_VERSION),
        heartbeat_secs: value
            .get("heartbeat_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_HEARTBEAT_SECS),
        last_seq: value.get("last_seq").and_then(Value::as_i64),
    }
}

/// Read an event frame, keeping the raw JSON alongside the named fields.
fn event_frame_from(value: &Value) -> EventFrame {
    EventFrame {
        seq: value.get("seq").and_then(Value::as_i64),
        id: string_field(value, "id"),
        at: string_field(value, "at"),
        scope: string_field(value, "scope"),
        kind: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        mailbox_id: value.get("mailbox_id").and_then(Value::as_i64),
        message_id: value.get("message_id").and_then(Value::as_i64),
        from: string_field(value, "from"),
        subject: string_field(value, "subject"),
        snippet: string_field(value, "snippet"),
        replay_gap: value
            .get("replay_gap")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        raw: value.clone(),
    }
}

/// A string field of a frame, when the server sent one.
fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// What the client owes after one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterAction {
    /// Show a desktop notification (subject to the account's pause settings).
    Notify,
    /// Sync from the stored cursor before trusting anything else.
    SyncNow,
    /// Re-read the folder tree.
    RefreshFolders,
    /// Reload the drafts pane.
    ReloadDrafts,
    /// This device was revoked: clear the tokens and the cache, and sign out.
    SessionExpired,
    /// Nothing to do — the frame carries no client obligation.
    Ignore,
}

/// Turns frames into actions.
///
/// Deliberately small and synchronous: it holds the two counters the UI wants
/// (`events_seen`, the high-water `seq`) and no I/O, so the routing table of §8
/// is covered by tests that need no server and no clock.
#[derive(Debug, Clone, Default)]
pub struct EventRouter {
    events_seen: u64,
    last_seq: Option<i64>,
}

impl EventRouter {
    /// A router that has seen nothing.
    pub fn new() -> Self {
        EventRouter::default()
    }

    /// The last sequence seen on the socket.
    ///
    /// The cursor, not the socket, is the source of truth (§8), so this is only
    /// used to pass `cursor` on reconnect. It is a high-water mark: a frame whose
    /// `seq` goes backwards never moves it down, because a cursor that moves
    /// backwards would replay events forever.
    pub fn last_seq(&self) -> Option<i64> {
        self.last_seq
    }

    /// How many event frames this router has seen — every
    /// [`ServerFrame::Event`], including one whose kind this build does not route
    /// (which still counts, and still routes to [`RouterAction::Ignore`]).
    pub fn events_seen(&self) -> u64 {
        self.events_seen
    }

    /// The actions `frame` calls for, in the order the client should take them.
    ///
    /// * `hello` → [`RouterAction::RefreshFolders`]: the connection is usable, and
    ///   the folders are what the UI needs before the first sync lands.
    /// * `mail.received` → [`RouterAction::Notify`] + [`RouterAction::SyncNow`].
    /// * `mail.sent` / `mail.deleted` / `mail.moved` / `mail.flag_changed` /
    ///   `mail.read` → [`RouterAction::SyncNow`]. They do **not** notify: the user
    ///   caused them, or they are consequences of something already on screen.
    /// * `draft.created` / `draft.updated` → [`RouterAction::ReloadDrafts`].
    /// * `device.revoked` → [`RouterAction::SessionExpired`].
    /// * `delivery.updated` → [`RouterAction::Notify`]: the outbox shows it, so no
    ///   sync is forced for it.
    /// * anything else → [`RouterAction::Ignore`], including [`ServerFrame::Unknown`].
    /// * `{"replay_gap":true}` → [`RouterAction::SyncNow`].
    /// * a `seq` that is not strictly greater than the previous one is a duplicate
    ///   or an out-of-order replay, and is a gap signal too → [`RouterAction::SyncNow`],
    ///   added once, after the kind's own actions.
    pub fn observe(&mut self, frame: &ServerFrame) -> Vec<RouterAction> {
        let event = match frame {
            ServerFrame::Event(event) => event,
            ServerFrame::Hello(_) => return vec![RouterAction::RefreshFolders],
            ServerFrame::ReplayGap => return vec![RouterAction::SyncNow],
            ServerFrame::Ping | ServerFrame::Pong | ServerFrame::Unknown(_) => {
                return vec![RouterAction::Ignore]
            }
        };

        self.events_seen = self.events_seen.saturating_add(1);

        let mut out_of_order = false;
        if let Some(seq) = event.seq {
            if matches!(self.last_seq, Some(previous) if seq <= previous) {
                out_of_order = true;
            } else {
                self.last_seq = Some(seq);
            }
        }

        let mut actions = actions_for_kind(&event.kind);
        if out_of_order {
            push_unique(&mut actions, RouterAction::SyncNow);
        }
        actions
    }
}

/// The routing table of §8, by event kind.
fn actions_for_kind(kind: &str) -> Vec<RouterAction> {
    match kind {
        "mail.received" => vec![RouterAction::Notify, RouterAction::SyncNow],
        "mail.sent" | "mail.deleted" | "mail.moved" | "mail.flag_changed" | "mail.read" => {
            vec![RouterAction::SyncNow]
        }
        "draft.created" | "draft.updated" => vec![RouterAction::ReloadDrafts],
        "device.revoked" => vec![RouterAction::SessionExpired],
        "delivery.updated" => vec![RouterAction::Notify],
        _ => vec![RouterAction::Ignore],
    }
}

/// Append `action` unless the list already asks for it.
fn push_unique(actions: &mut Vec<RouterAction>, action: RouterAction) {
    if !actions.contains(&action) {
        actions.push(action);
    }
}

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// Everything [`EventClient::connect`] needs (§8).
#[derive(Clone, PartialEq)]
pub struct EventStreamConfig {
    /// The full socket URL, normally from [`EventClient::event_url`].
    pub url: String,
    /// The bearer token the handshake authenticates with.
    pub token: String,
    /// The last sequence the client applied; sent as `?cursor=` on reconnect so
    /// the bus can replay what was missed.
    pub cursor: Option<String>,
    /// The heartbeat to assume until the server's `hello` says otherwise.
    pub heartbeat_secs: u64,
    /// The reconnect schedule.
    pub backoff: Backoff,
}

impl EventStreamConfig {
    /// A configuration for `url` with the documented heartbeat and backoff.
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        EventStreamConfig {
            url: url.into(),
            token: token.into(),
            cursor: None,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            backoff: Backoff::default(),
        }
    }

    /// The same configuration, resuming from `cursor`.
    pub fn resuming_from(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

/// The token is never rendered, not even by `{:?}` (AGENTS.md rule 7).
impl std::fmt::Debug for EventStreamConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStreamConfig")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("cursor", &self.cursor)
            .field("heartbeat_secs", &self.heartbeat_secs)
            .field("backoff", &self.backoff)
            .finish()
    }
}

/// The stateless half of the realtime client.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventClient;

impl EventClient {
    /// The §8 URL: `<base>/events?cursor=<c>`.
    ///
    /// `api_base` is the FCP client base — [`crate::Discovery::fcp_base_url`] —
    /// not the site root; a trailing `/` is ignored and the cursor is
    /// percent-encoded, so an opaque cursor cannot inject a query parameter.
    pub fn event_url(api_base: &str, cursor: Option<&str>) -> String {
        let base = api_base.trim_end_matches('/');
        match cursor.filter(|cursor| !cursor.is_empty()) {
            Some(cursor) => format!("{base}/events?cursor={}", crate::util::urlencode(cursor)),
            None => format!("{base}/events"),
        }
    }

    /// Connect, authenticate with the bearer token, and return the open socket.
    ///
    /// The handshake carries `Authorization: Bearer <token>` alongside the
    /// WebSocket headers, which is how §8 authenticates the socket. The server's
    /// `hello` is *not* consumed here: [`EventConnection::next_frame`] hands it to
    /// the caller like any other frame.
    pub async fn connect(config: &EventStreamConfig) -> ClientResult<EventConnection> {
        let url = url_with_cursor(&config.url, config.cursor.as_deref());
        let socket = open_socket(&url, &config.token).await?;
        Ok(EventConnection::start(socket, config.heartbeat_secs))
    }
}

/// The URL to dial: `url`, with `cursor` appended when the config names one the
/// URL does not already carry.
fn url_with_cursor(url: &str, cursor: Option<&str>) -> String {
    match cursor.filter(|cursor| !cursor.is_empty()) {
        Some(cursor) if !url.contains("cursor=") => {
            let separator = if url.contains('?') { '&' } else { '?' };
            format!("{url}{separator}cursor={}", crate::util::urlencode(cursor))
        }
        _ => url.to_string(),
    }
}

/// An open realtime socket.
///
/// Owns the connection and the heartbeat clock; it never syncs, never notifies
/// and never touches the cache. The caller reads frames, routes them through
/// [`EventRouter`], and decides what to do about a disconnect — including the
/// [`Backoff`] wait before the next attempt.
pub struct EventConnection {
    socket: Option<Socket>,
    heartbeat: Duration,
    last_inbound: DateTime<Utc>,
}

impl EventConnection {
    /// Wrap an already-handshaken socket.
    fn start(socket: Socket, heartbeat_secs: u64) -> Self {
        EventConnection {
            socket: Some(socket),
            heartbeat: Duration::from_secs(heartbeat_secs.max(1)),
            last_inbound: Utc::now(),
        }
    }

    /// A connection with no socket at all, so the heartbeat clock can be tested
    /// without a server. Test-only: a real connection always has a socket.
    #[cfg(test)]
    fn detached(heartbeat_secs: u64, now: DateTime<Utc>) -> Self {
        EventConnection {
            socket: None,
            heartbeat: Duration::from_secs(heartbeat_secs.max(1)),
            last_inbound: now,
        }
    }

    /// Whether the socket is still open.
    pub fn is_open(&self) -> bool {
        self.socket.is_some()
    }

    /// The heartbeat currently in force.
    pub fn heartbeat(&self) -> Duration {
        self.heartbeat
    }

    /// Send the `pong` that answers a server `ping` (§8).
    pub async fn send_pong(&mut self) -> ClientResult<()> {
        self.send_text(PONG_FRAME).await
    }

    /// Send the `ping` that says "still here" after a silent heartbeat (§8).
    pub async fn send_ping(&mut self) -> ClientResult<()> {
        self.send_text(PING_FRAME).await
    }

    /// Read the next frame, or `None` when the socket closed.
    ///
    /// Control frames are handled, not surfaced: a WebSocket `ping` becomes
    /// [`ServerFrame::Ping`] (and is answered by [`EventConnection::send_pong`]),
    /// a `close` ends the stream with `None`, and a binary frame that happens to
    /// be valid UTF-8 JSON is parsed like a text frame — a proxy that rewrites the
    /// opcode must not cost the client an event. A frame that is not valid UTF-8,
    /// or that is not JSON, is skipped or reported as [`ClientError::Parse`]; it
    /// never panics.
    ///
    /// A `hello` frame replaces the heartbeat with the one the server asked for.
    pub async fn next_frame(&mut self) -> ClientResult<Option<ServerFrame>> {
        loop {
            let next = match self.socket.as_mut() {
                Some(socket) => socket.next_message().await,
                None => return Ok(None),
            };

            let message = match next {
                None => {
                    self.socket = None;
                    return Ok(None);
                }
                Some(Err(err)) => {
                    self.socket = None;
                    return Err(ClientError::WebSocket(format!(
                        "the realtime socket failed: {err}"
                    )));
                }
                Some(Ok(message)) => message,
            };

            // Anything that arrives is evidence of life, even if we skip it.
            self.note_inbound(Utc::now());

            match message {
                Message::Text(text) => {
                    let frame = parse_frame(&text)?;
                    self.adopt_heartbeat(&frame);
                    return Ok(Some(frame));
                }
                Message::Binary(bytes) => match std::str::from_utf8(&bytes) {
                    Ok(text) => {
                        let frame = parse_frame(text)?;
                        self.adopt_heartbeat(&frame);
                        return Ok(Some(frame));
                    }
                    // A binary frame is not an FCP frame. Ignore it and keep
                    // reading rather than dropping a healthy connection.
                    Err(_) => continue,
                },
                Message::Ping(_) => return Ok(Some(ServerFrame::Ping)),
                Message::Pong(_) => return Ok(Some(ServerFrame::Pong)),
                Message::Close(_) => {
                    self.socket = None;
                    return Ok(None);
                }
                Message::Frame(_) => continue,
            }
        }
    }

    /// Close cleanly, sending the WebSocket close handshake.
    ///
    /// Idempotent: closing an already-closed connection is a success, because the
    /// caller's intent — "this socket is done" — is satisfied either way.
    pub async fn close(&mut self) -> ClientResult<()> {
        let Some(mut socket) = self.socket.take() else {
            return Ok(());
        };
        socket
            .close(None)
            .await
            .map_err(|err| ClientError::WebSocket(format!("closing the realtime socket failed: {err}")))
    }

    /// Whether `heartbeat_secs` have passed since the last inbound frame, so the
    /// caller knows to send `ping` (§8).
    ///
    /// Purely a clock comparison: a `now` that is *before* the last inbound frame
    /// (a clock that stepped backwards, or a caller that passed a stale `now`)
    /// answers `false`, so a bad clock cannot turn into a ping storm.
    pub fn should_ping(&self, now: DateTime<Utc>) -> bool {
        match now.signed_duration_since(self.last_inbound).to_std() {
            Ok(elapsed) => elapsed >= self.heartbeat,
            Err(_) => false,
        }
    }

    /// Record that a frame arrived at `now`, restarting the heartbeat clock.
    pub fn note_inbound(&mut self, now: DateTime<Utc>) {
        self.last_inbound = now;
    }

    /// Take the heartbeat the server asked for in its `hello`.
    fn adopt_heartbeat(&mut self, frame: &ServerFrame) {
        if let ServerFrame::Hello(hello) = frame {
            self.heartbeat = hello.heartbeat();
        }
    }

    /// Send one text frame.
    async fn send_text(&mut self, text: &str) -> ClientResult<()> {
        let socket = self.socket.as_mut().ok_or_else(|| {
            ClientError::WebSocket("the realtime socket is not open".to_string())
        })?;
        socket
            .send_message(Message::text(text))
            .await
            .map_err(|err| ClientError::WebSocket(format!("sending on the realtime socket failed: {err}")))
    }
}

/// The token is never rendered (AGENTS.md rule 7).
impl std::fmt::Debug for EventConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventConnection")
            .field("open", &self.socket.is_some())
            .field("heartbeat", &self.heartbeat)
            .field("last_inbound", &self.last_inbound)
            .finish()
    }
}

/// The two shapes a connected WebSocket can have here.
///
/// `ws://` and `wss://` hand back different stream types, and this module wants
/// one. The TLS variant is a [`MaybeTlsStream`] because the handshake helper
/// decides from the URL whether to actually wrap the socket in TLS.
type WsStream<S> = WebSocketStream<S>;

enum Socket {
    /// A plain `ws://` socket — a loopback development server, or a terminator
    /// that has already done the TLS.
    Plain(Box<WsStream<TcpStream>>),
    /// A `wss://` socket, wrapped in rustls by the handshake.
    ///
    /// Boxed because a TLS stream is much larger than a plain one and a `Socket`
    /// lives inside every connection.
    Tls(Box<WsStream<MaybeTlsStream<TcpStream>>>),
}

impl Socket {
    /// The next message, or `None` once the stream ends.
    async fn next_message(&mut self) -> Option<Result<Message, WsError>> {
        match self {
            Socket::Plain(socket) => socket.next().await,
            Socket::Tls(socket) => socket.next().await,
        }
    }

    /// Send one message.
    #[allow(clippy::result_large_err)] // tungstenite's own error type; boxing is not ours to choose.
    async fn send_message(&mut self, message: Message) -> Result<(), WsError> {
        match self {
            Socket::Plain(socket) => socket.send(message).await,
            Socket::Tls(socket) => socket.send(message).await,
        }
    }

    /// Start the close handshake.
    ///
    /// The TLS arm reaches through the box explicitly: `SinkExt::close` (which
    /// takes no frame) would otherwise be picked for `Box<WebSocketStream>`,
    /// silently sending an empty close frame instead of the one asked for.
    #[allow(clippy::result_large_err)] // tungstenite's own error type.
    async fn close(
        &mut self,
        frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame<'static>>,
    ) -> Result<(), WsError> {
        match self {
            Socket::Plain(socket) => socket.as_mut().close(frame).await,
            Socket::Tls(socket) => socket.as_mut().close(frame).await,
        }
    }
}

/// Dial `url`, perform the WebSocket handshake, and return the open socket.
///
/// This is the one function in the module that needs a runtime and a network. The
/// TCP connection is dialled here because this workspace builds `tokio-tungstenite`
/// without its `connect` feature — `connect_async` does not exist — so the
/// handshake is done over a stream we opened ourselves: `client_async` for
/// `ws://`, `client_async_tls_with_config` for `wss://` (rustls, webpki roots).
///
/// The request carries `Authorization: Bearer <token>` together with the standard
/// WebSocket headers, which `ClientRequestBuilder` generates.
async fn open_socket(url: &str, token: &str) -> ClientResult<Socket> {
    let parsed = Url::parse(url)
        .map_err(|err| ClientError::invalid(format!("not a realtime URL: {url:?} ({err})")))?;
    let scheme = parsed.scheme();
    if scheme != "ws" && scheme != "wss" {
        return Err(ClientError::invalid(format!(
            "a realtime URL must be ws:// or wss://, not {scheme}://"
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| ClientError::invalid(format!("the realtime URL {url:?} has no host")))?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);

    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| ClientError::Timeout(format!("connecting to {host}:{port} timed out")))?
    .map_err(|err| ClientError::Network(format!("connecting to {host}:{port} failed: {err}")))?;

    let uri: Uri = url
        .parse()
        .map_err(|err| ClientError::invalid(format!("not a realtime URL: {url:?} ({err})")))?;
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {token}"))
        .with_header("X-Ferroma-Client", crate::api::client_version_string());

    let handshake = async move {
        if scheme == "wss" {
            let (socket, _response) =
                tokio_tungstenite::client_async_tls_with_config(request, stream, None, None)
                    .await
                    .map_err(|err| {
                        ClientError::WebSocket(format!("the realtime TLS handshake failed: {err}"))
                    })?;
            Ok::<Socket, ClientError>(Socket::Tls(Box::new(socket)))
        } else {
            let (socket, _response) = tokio_tungstenite::client_async(request, stream)
                .await
                .map_err(|err| {
                    ClientError::WebSocket(format!("the realtime handshake failed: {err}"))
                })?;
            Ok::<Socket, ClientError>(Socket::Plain(Box::new(socket)))
        }
    };

    tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| ClientError::Timeout(format!("the handshake with {url} timed out")))?
        .map_err(|err| ClientError::WebSocket(format!("the realtime handshake failed: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    /// Parse a frame that is expected to be valid.
    fn frame(text: &str) -> ServerFrame {
        parse_frame(text).expect("frame")
    }

    /// An event frame, for the router tests.
    fn event(text: &str) -> ServerFrame {
        frame(text)
    }

    // -- parse_frame -------------------------------------------------------

    #[test]
    fn a_hello_frame_is_parsed() {
        let parsed = frame(
            r#"{"type":"hello","protocol_version":1,"heartbeat_secs":30,"last_seq":1841}"#,
        );
        assert_eq!(
            parsed,
            ServerFrame::Hello(Hello {
                protocol_version: 1,
                heartbeat_secs: 30,
                last_seq: Some(1841),
            })
        );
        if let ServerFrame::Hello(hello) = parsed {
            assert_eq!(hello.heartbeat(), Duration::from_secs(30));
        } else {
            panic!("expected a hello");
        }
    }

    #[test]
    fn a_hello_frame_falls_back_to_the_documented_defaults() {
        assert_eq!(
            frame(r#"{"type":"hello","last_seq":null}"#),
            ServerFrame::Hello(Hello {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
                last_seq: None,
            })
        );
        assert_eq!(
            frame(r#"{"type":"hello"}"#),
            ServerFrame::Hello(Hello {
                protocol_version: 1,
                heartbeat_secs: 30,
                last_seq: None,
            })
        );
    }

    #[test]
    fn ping_and_pong_frames_are_parsed() {
        assert_eq!(frame(r#"{"type":"ping"}"#), ServerFrame::Ping);
        assert_eq!(frame(r#"{"type":"pong"}"#), ServerFrame::Pong);
        // Whitespace around a frame is not significant.
        assert_eq!(frame("  {\"type\":\"ping\"}\n"), ServerFrame::Ping);
    }

    #[test]
    fn a_full_mail_received_frame_keeps_every_field() {
        let parsed = frame(
            r#"{"seq":1842,"id":"0f4c8f1e","at":"2026-09-16T12:00:00Z","scope":"user:7",
                "type":"mail.received","mailbox_id":3,"message_id":4822,
                "from":"bob@example.net","subject":"Re: Invoice","snippet":"Thanks, got it."}"#,
        );
        let event = match parsed {
            ServerFrame::Event(event) => event,
            other => panic!("expected an event frame, got {other:?}"),
        };

        assert_eq!(event.seq, Some(1842));
        assert_eq!(event.id.as_deref(), Some("0f4c8f1e"));
        assert_eq!(event.at.as_deref(), Some("2026-09-16T12:00:00Z"));
        assert_eq!(event.scope.as_deref(), Some("user:7"));
        assert_eq!(event.kind, "mail.received");
        assert_eq!(event.mailbox_id, Some(3));
        assert_eq!(event.message_id, Some(4822));
        assert_eq!(event.from.as_deref(), Some("bob@example.net"));
        assert_eq!(event.subject.as_deref(), Some("Re: Invoice"));
        assert_eq!(event.snippet.as_deref(), Some("Thanks, got it."));
        assert!(!event.replay_gap);
        // The raw frame is kept for the layers that read fields this struct does
        // not name.
        assert_eq!(event.raw["message_id"], serde_json::json!(4822));
    }

    #[test]
    fn an_event_without_the_optional_fields_still_parses() {
        let event = match frame(r#"{"type":"draft.updated","seq":9}"#) {
            ServerFrame::Event(event) => event,
            other => panic!("expected an event frame, got {other:?}"),
        };
        assert_eq!(event.kind, "draft.updated");
        assert_eq!(event.seq, Some(9));
        assert_eq!(event.id, None);
        assert_eq!(event.at, None);
        assert_eq!(event.scope, None);
        assert_eq!(event.mailbox_id, None);
        assert_eq!(event.message_id, None);
        assert_eq!(event.from, None);
        assert_eq!(event.subject, None);
        assert_eq!(event.snippet, None);
        assert_eq!(event.raw["type"], serde_json::json!("draft.updated"));
    }

    #[test]
    fn a_replay_gap_frame_is_recognised_with_and_without_a_type() {
        assert_eq!(frame(r#"{"replay_gap":true}"#), ServerFrame::ReplayGap);
        assert_eq!(
            frame(r#"{"type":"replay_gap","replay_gap":true}"#),
            ServerFrame::ReplayGap
        );
        // The server's own name for the frame is a signal on its own…
        assert_eq!(frame(r#"{"type":"replay_gap"}"#), ServerFrame::ReplayGap);
        // …and the flag is the signal, whatever else the frame carries.
        assert_eq!(
            frame(r#"{"seq":1843,"type":"mail.received","replay_gap":true}"#),
            ServerFrame::ReplayGap
        );
        // `false` is not a gap.
        assert!(matches!(
            frame(r#"{"replay_gap":false}"#),
            ServerFrame::Unknown(_)
        ));
    }

    #[test]
    fn an_unknown_type_is_unknown_not_an_error() {
        assert_eq!(
            frame(r#"{"type":"team.invited","seq":5}"#),
            ServerFrame::Unknown(serde_json::json!({"type": "team.invited", "seq": 5}))
        );
        // A JSON object with no `type` at all is unknown too.
        assert!(matches!(frame(r#"{"seq":5}"#), ServerFrame::Unknown(_)));
        // …and so is a JSON scalar that is neither ping nor pong.
        assert!(matches!(frame("42"), ServerFrame::Unknown(_)));
        assert!(matches!(frame("null"), ServerFrame::Unknown(_)));
        assert!(matches!(frame(r#""hello""#), ServerFrame::Unknown(_)));
    }

    #[test]
    fn a_frame_that_is_not_json_is_a_parse_error() {
        for bad in ["{ this is not json", "not json at all", "{\"seq\":}"] {
            let err = parse_frame(bad).expect_err("not JSON");
            assert!(matches!(err, ClientError::Parse(_)), "{bad}: {err:?}");
        }
        let err = parse_frame("   ").expect_err("an empty frame");
        assert!(matches!(err, ClientError::Parse(_)));
    }

    #[test]
    fn a_json_array_is_unknown_and_does_not_panic() {
        assert_eq!(
            frame(r#"[{"type":"ping"},1,null]"#),
            ServerFrame::Unknown(serde_json::json!([{"type": "ping"}, 1, null]))
        );
    }

    #[test]
    fn a_bare_ping_string_is_accepted() {
        // The JSON string the protocol allows…
        assert_eq!(frame(r#""ping""#), ServerFrame::Ping);
        assert_eq!(frame(r#""pong""#), ServerFrame::Pong);
        // …and the unquoted word some proxies leave behind.
        assert_eq!(frame("ping"), ServerFrame::Ping);
        assert_eq!(frame("PONG"), ServerFrame::Pong);
    }

    #[test]
    fn the_event_kinds_are_the_documented_ten() {
        assert_eq!(EVENT_KINDS.len(), 10);
        for kind in EVENT_KINDS {
            assert!(is_event_kind(kind), "{kind}");
            assert!(matches!(
                frame(&format!(r#"{{"type":"{kind}","seq":1}}"#)),
                ServerFrame::Event(_)
            ));
        }
        assert!(!is_event_kind("mail.updated"));
        assert!(!is_event_kind(""));
    }

    // -- EventRouter -------------------------------------------------------

    #[test]
    fn hello_refreshes_the_folders() {
        let mut router = EventRouter::new();
        assert_eq!(
            router.observe(&frame(r#"{"type":"hello","heartbeat_secs":30}"#)),
            vec![RouterAction::RefreshFolders]
        );
        // A hello is not an event: it must not touch the counters.
        assert_eq!(router.events_seen(), 0);
        assert_eq!(router.last_seq(), None);
    }

    #[test]
    fn mail_received_notifies_and_syncs() {
        let mut router = EventRouter::new();
        assert_eq!(
            router.observe(&event(
                r#"{"seq":1842,"type":"mail.received","mailbox_id":3,"message_id":4822}"#
            )),
            vec![RouterAction::Notify, RouterAction::SyncNow]
        );
        assert_eq!(router.events_seen(), 1);
        assert_eq!(router.last_seq(), Some(1842));
    }

    #[test]
    fn mail_state_changes_only_sync() {
        for kind in [
            "mail.sent",
            "mail.deleted",
            "mail.moved",
            "mail.flag_changed",
            "mail.read",
        ] {
            let mut router = EventRouter::new();
            let actions = router.observe(&event(&format!(r#"{{"seq":10,"type":"{kind}"}}"#)));
            assert_eq!(actions, vec![RouterAction::SyncNow], "{kind}");
            assert!(
                !actions.contains(&RouterAction::Notify),
                "{kind} must not ring the bell: the user caused it"
            );
        }
    }

    #[test]
    fn drafts_reload_the_drafts_pane() {
        for kind in ["draft.created", "draft.updated"] {
            let mut router = EventRouter::new();
            assert_eq!(
                router.observe(&event(&format!(r#"{{"seq":3,"type":"{kind}"}}"#))),
                vec![RouterAction::ReloadDrafts],
                "{kind}"
            );
        }
    }

    #[test]
    fn device_revoked_expires_the_session() {
        let mut router = EventRouter::new();
        assert_eq!(
            router.observe(&event(r#"{"seq":7,"type":"device.revoked","scope":"user:7"}"#)),
            vec![RouterAction::SessionExpired]
        );
    }

    #[test]
    fn delivery_updated_notifies_without_forcing_a_sync() {
        let mut router = EventRouter::new();
        assert_eq!(
            router.observe(&event(r#"{"seq":8,"type":"delivery.updated"}"#)),
            vec![RouterAction::Notify]
        );
    }

    #[test]
    fn anything_else_is_ignored() {
        let mut router = EventRouter::new();
        assert_eq!(
            router.observe(&frame(r#"{"seq":1,"type":"team.invited"}"#)),
            vec![RouterAction::Ignore]
        );
        assert_eq!(
            router.observe(&frame(r#"{"unexpected":true}"#)),
            vec![RouterAction::Ignore]
        );
        assert_eq!(router.observe(&ServerFrame::Ping), vec![RouterAction::Ignore]);
        assert_eq!(router.observe(&ServerFrame::Pong), vec![RouterAction::Ignore]);
    }

    #[test]
    fn a_replay_gap_syncs_immediately() {
        let mut router = EventRouter::new();
        assert_eq!(
            router.observe(&frame(r#"{"replay_gap":true}"#)),
            vec![RouterAction::SyncNow]
        );
        assert_eq!(router.observe(&ServerFrame::ReplayGap), vec![RouterAction::SyncNow]);
    }

    #[test]
    fn the_router_advances_its_sequence_high_water_mark() {
        let mut router = EventRouter::new();
        assert_eq!(router.last_seq(), None);

        router.observe(&event(r#"{"seq":1842,"type":"mail.received"}"#));
        assert_eq!(router.last_seq(), Some(1842));
        router.observe(&event(r#"{"seq":1843,"type":"mail.read"}"#));
        assert_eq!(router.last_seq(), Some(1843));

        // A frame without a `seq` routes by kind and leaves the cursor alone.
        assert_eq!(
            router.observe(&event(r#"{"type":"draft.updated"}"#)),
            vec![RouterAction::ReloadDrafts]
        );
        assert_eq!(router.last_seq(), Some(1843));
    }

    #[test]
    fn a_duplicate_or_regressing_seq_syncs_again() {
        let mut router = EventRouter::new();
        let first = event(r#"{"seq":10,"type":"draft.updated"}"#);
        assert_eq!(router.observe(&first), vec![RouterAction::ReloadDrafts]);
        assert_eq!(router.last_seq(), Some(10));

        // The same frame twice is a duplicate: re-sync rather than trust it. The
        // sync is reported once, after the kind's own action.
        assert_eq!(
            router.observe(&first),
            vec![RouterAction::ReloadDrafts, RouterAction::SyncNow]
        );
        assert_eq!(router.last_seq(), Some(10));

        // A seq that went backwards must never move the watermark back.
        assert_eq!(
            router.observe(&event(r#"{"seq":4,"type":"draft.updated"}"#)),
            vec![RouterAction::ReloadDrafts, RouterAction::SyncNow]
        );
        assert_eq!(router.last_seq(), Some(10));

        // Forward again: no gap.
        assert_eq!(
            router.observe(&event(r#"{"seq":11,"type":"draft.updated"}"#)),
            vec![RouterAction::ReloadDrafts]
        );
        assert_eq!(router.last_seq(), Some(11));

        // A duplicate of an event that already syncs does not report it twice.
        let received = event(r#"{"seq":11,"type":"mail.received"}"#);
        assert_eq!(
            router.observe(&received),
            vec![RouterAction::Notify, RouterAction::SyncNow]
        );
    }

    #[test]
    fn the_router_counts_every_event_frame() {
        let mut router = EventRouter::new();
        router.observe(&frame(r#"{"type":"hello"}"#));
        router.observe(&ServerFrame::Ping);
        router.observe(&ServerFrame::ReplayGap);
        router.observe(&event(r#"{"seq":1,"type":"mail.received"}"#));
        router.observe(&event(r#"{"seq":2,"type":"mail.read"}"#));
        // Not one of the documented kinds: parsed as unknown, so not an event.
        assert_eq!(
            router.observe(&frame(r#"{"seq":3,"type":"team.invited"}"#)),
            vec![RouterAction::Ignore]
        );
        assert_eq!(router.events_seen(), 2);

        // An event frame whose kind the routing table does not know still counts,
        // and still routes to `Ignore`.
        let mut unrouted = event(r#"{"seq":4,"type":"mail.read"}"#);
        if let ServerFrame::Event(inner) = &mut unrouted {
            inner.kind = "mail.updated".to_string();
        }
        assert_eq!(router.observe(&unrouted), vec![RouterAction::Ignore]);
        assert_eq!(router.events_seen(), 3);
    }

    // -- Backoff -----------------------------------------------------------

    #[test]
    fn backoff_is_exponential_and_capped() {
        let backoff = Backoff::without_jitter(Duration::from_millis(100), Duration::from_millis(1000));
        assert_eq!(backoff.delay(1), Duration::from_millis(100));
        assert_eq!(backoff.delay(2), Duration::from_millis(200));
        assert_eq!(backoff.delay(3), Duration::from_millis(400));
        assert_eq!(backoff.delay(4), Duration::from_millis(800));
        assert_eq!(backoff.delay(5), Duration::from_millis(1000));
        assert_eq!(backoff.delay(64), Duration::from_millis(1000));
        // Attempt 0 is the first attempt, and a base above the ceiling is capped.
        assert_eq!(backoff.delay(0), Duration::from_millis(100));
        let inverted = Backoff::without_jitter(Duration::from_secs(30), Duration::from_secs(2));
        assert_eq!(inverted.delay(1), Duration::from_secs(2));
    }

    #[test]
    fn jitter_stays_inside_the_band() {
        let backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(10));
        assert_eq!(backoff.factor, 2.0);
        assert_eq!(backoff.jitter, 0.25);
        for _ in 0..200 {
            let millis = backoff.delay(1).as_millis() as u64;
            assert!((75..=125).contains(&millis), "out of band: {millis}");
        }
        for _ in 0..200 {
            let millis = backoff.delay(3).as_millis() as u64;
            assert!((300..=500).contains(&millis), "out of band: {millis}");
        }
        // A degenerate jitter cannot produce a panic or an unbounded delay.
        let broken = Backoff {
            base: Duration::from_millis(10),
            max: Duration::from_millis(20),
            factor: f64::NAN,
            jitter: f64::INFINITY,
        };
        assert!(broken.delay(1) <= Duration::from_millis(20));
    }

    #[test]
    fn the_default_backoff_is_one_to_sixty_seconds() {
        let backoff = Backoff::default();
        assert_eq!(backoff, Backoff::new(Duration::from_secs(1), Duration::from_secs(60)));
        assert_eq!(backoff.base, Duration::from_secs(1));
        assert_eq!(backoff.max, Duration::from_secs(60));
        assert_eq!(backoff.factor, 2.0);
        assert_eq!(backoff.jitter, 0.25);
    }

    // -- URL and the heartbeat clock ---------------------------------------

    #[test]
    fn the_event_url_carries_the_cursor() {
        assert_eq!(
            EventClient::event_url("https://mail.example.com/api/v1/client", Some("1841")),
            "https://mail.example.com/api/v1/client/events?cursor=1841"
        );
        assert_eq!(
            EventClient::event_url("https://mail.example.com/api/v1/client", None),
            "https://mail.example.com/api/v1/client/events"
        );
        // A trailing slash is not doubled, and an empty cursor is no cursor.
        assert_eq!(
            EventClient::event_url("https://mail.example.com/api/v1/client/", Some("")),
            "https://mail.example.com/api/v1/client/events"
        );
        // An opaque cursor cannot smuggle in a second query parameter.
        assert_eq!(
            EventClient::event_url("https://mail.example.com", Some("1&limit=999")),
            "https://mail.example.com/events?cursor=1%26limit%3D999"
        );
        assert_eq!(
            EventClient::event_url("http://127.0.0.1:8080/api/v1/client", Some("0")),
            "http://127.0.0.1:8080/api/v1/client/events?cursor=0"
        );
    }

    #[test]
    fn the_connect_url_takes_the_cursor_from_the_config() {
        assert_eq!(
            url_with_cursor("wss://mail.example.com/api/v1/client/events", Some("42")),
            "wss://mail.example.com/api/v1/client/events?cursor=42"
        );
        // A cursor already in the URL is not added twice.
        assert_eq!(
            url_with_cursor(
                "wss://mail.example.com/api/v1/client/events?cursor=42",
                Some("7")
            ),
            "wss://mail.example.com/api/v1/client/events?cursor=42"
        );
        assert_eq!(
            url_with_cursor("wss://mail.example.com/api/v1/client/events", None),
            "wss://mail.example.com/api/v1/client/events"
        );

        let config = EventStreamConfig::new("wss://mail.example.com/events", "s3cr3t-bearer-value")
            .resuming_from("1841");
        assert_eq!(config.cursor.as_deref(), Some("1841"));
        assert_eq!(config.heartbeat_secs, DEFAULT_HEARTBEAT_SECS);
        assert_eq!(config.backoff, Backoff::default());
        // The bearer token never reaches a log line, not even through `{:?}`:
        // the field *name* is printed, the secret must not be.
        assert!(!format!("{config:?}").contains("s3cr3t-bearer-value"));
    }

    #[test]
    fn the_heartbeat_clock_tracks_inbound_frames() {
        let now = Utc::now();
        let mut connection = EventConnection::detached(30, now);

        assert_eq!(connection.heartbeat(), Duration::from_secs(30));
        assert!(!connection.is_open());
        // Nothing has arrived yet: the clock starts when the connection does.
        assert!(!connection.should_ping(now));
        assert!(!connection.should_ping(now + ChronoDuration::seconds(29)));
        assert!(connection.should_ping(now + ChronoDuration::seconds(30)));
        assert!(connection.should_ping(now + ChronoDuration::seconds(3600)));

        // A frame resets the clock.
        let later = now + ChronoDuration::minutes(5);
        connection.note_inbound(later);
        assert!(!connection.should_ping(later));
        assert!(!connection.should_ping(later + ChronoDuration::seconds(29)));
        assert!(connection.should_ping(later + ChronoDuration::seconds(30)));

        // A clock that stepped backwards must not become a ping storm.
        assert!(!connection.should_ping(now));
    }

    #[test]
    fn the_hello_heartbeat_replaces_the_configured_one() {
        let now = Utc::now();
        let mut connection = EventConnection::detached(DEFAULT_HEARTBEAT_SECS, now);
        assert!(!connection.should_ping(now + ChronoDuration::seconds(20)));

        connection.adopt_heartbeat(&frame(r#"{"type":"hello","heartbeat_secs":5}"#));
        assert_eq!(connection.heartbeat(), Duration::from_secs(5));
        assert!(connection.should_ping(now + ChronoDuration::seconds(5)));

        // A server asking for zero still gets a sane one-second heartbeat.
        connection.adopt_heartbeat(&frame(r#"{"type":"hello","heartbeat_secs":0}"#));
        assert_eq!(connection.heartbeat(), Duration::from_secs(1));

        // Any other frame leaves it alone.
        connection.adopt_heartbeat(&frame(r#"{"seq":1,"type":"mail.read"}"#));
        assert_eq!(connection.heartbeat(), Duration::from_secs(1));
    }

    #[test]
    fn the_client_frames_match_the_server_parser() {
        assert_eq!(PING_FRAME, r#"{"type":"ping"}"#);
        assert_eq!(PONG_FRAME, r#"{"type":"pong"}"#);
        assert_eq!(
            serde_json::from_str::<Value>(PING_FRAME)
                .expect("ping json")["type"],
            serde_json::json!("ping")
        );
        assert_eq!(
            serde_json::from_str::<Value>(PONG_FRAME)
                .expect("pong json")["type"],
            serde_json::json!("pong")
        );
    }
}
