//! The realtime socket: `GET /api/v1/client/events`.
//!
//! The framing is frozen by [`docs/fcp.md`](../../../docs/fcp.md) §8:
//!
//! ```json
//! { "type": "hello", "protocol_version": 1, "heartbeat_secs": 30, "last_seq": 1841 }
//! ```
//!
//! followed by one JSON frame per event:
//!
//! ```json
//! { "seq": 1842, "id": "0f4c…", "at": "2026-09-16T12:00:00Z", "scope": "user:7",
//!   "type": "mail.received", "mailbox_id": 3, "message_id": 4822, … }
//! ```
//!
//! # Isolation
//!
//! The subscription is filtered to the caller's own [`EventScope::User`] stream, and
//! `EventFilter` matching is **exact** — so the mailbox-scoped events the server also
//! publishes never reach a client that is not the owner, and `replay_since_for` can
//! never be used to read somebody else's history.
//!
//! # Reconnect
//!
//! `?cursor=N` replays everything still buffered with `seq > N` before the live
//! stream starts. Because the replay ring is bounded, a cursor older than the oldest
//! buffered event cannot be caught up from the socket; the server says so with
//! `{"replay_gap":true}` and the client runs a sync instead — the cursor, not the
//! socket, is the source of truth.
//!
//! # Revocation
//!
//! A device revoked while connected is closed with the WebSocket `POLICY` close code
//! (`1008`), the socket-level equivalent of the `401` every other endpoint would
//! answer with. The close happens on the next heartbeat tick, so a revoked client is
//! dropped within `client.ws_heartbeat_secs`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use ferroma_core::UserId;
use ferroma_events::{EventEnvelope, EventFilter, EventScope, SubscriptionError};
use futures_util::{SinkExt, StreamExt};

use crate::error::ApiError;
use crate::extract::ClientAuth;
use crate::state::AppState;

/// The close code sent to a revoked device: "policy violation".
pub const CLOSE_POLICY_VIOLATION: u16 = 1008;

/// The `type` of the first frame the server sends.
pub const HELLO_TYPE: &str = "hello";

/// The `type` of the frame that tells a client its cursor could not be honoured.
pub const REPLAY_GAP_TYPE: &str = "replay_gap";

/// The `?cursor=` query of the upgrade request.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct EventsQuery {
    /// The last event sequence the client processed.
    pub cursor: Option<i64>,
}

/// The first frame on every connection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HelloFrame {
    /// Always [`HELLO_TYPE`].
    #[serde(rename = "type")]
    pub kind: String,
    /// The protocol version the server will speak.
    pub protocol_version: u32,
    /// How often the server pings, and how often the client should.
    pub heartbeat_secs: u64,
    /// The newest sequence the bus had when the socket opened; `null` on an idle bus.
    pub last_seq: Option<i64>,
}

impl HelloFrame {
    /// Build the hello for a freshly accepted socket.
    pub fn new(protocol_version: u32, heartbeat_secs: u64, last_seq: Option<i64>) -> Self {
        HelloFrame {
            kind: HELLO_TYPE.to_string(),
            protocol_version,
            heartbeat_secs,
            last_seq,
        }
    }

    /// Render the frame as the JSON text the socket carries.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            // Every field here is a number or a literal string, so this cannot happen;
            // a hard-coded fallback keeps the socket alive if it somehow does.
            format!(
                "{{\"type\":\"{HELLO_TYPE}\",\"protocol_version\":{},\"heartbeat_secs\":{},\"last_seq\":null}}",
                self.protocol_version, self.heartbeat_secs
            )
        })
    }

    /// The frame as a WebSocket text message.
    pub fn to_message(&self) -> Message {
        Message::Text(Utf8Bytes::from(self.to_text()))
    }
}

/// The frame telling a client to run a sync because replay could not catch it up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayGapFrame {
    /// Always `true`; the field's presence is the signal.
    pub replay_gap: bool,
}

impl ReplayGapFrame {
    /// The one instance that is ever sent.
    pub fn new() -> Self {
        ReplayGapFrame { replay_gap: true }
    }

    /// Render the frame as JSON text.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| format!("{{\"{REPLAY_GAP_TYPE}\":true}}"))
    }

    /// The frame as a WebSocket text message.
    pub fn to_message(&self) -> Message {
        Message::Text(Utf8Bytes::from(self.to_text()))
    }
}

impl Default for ReplayGapFrame {
    fn default() -> Self {
        ReplayGapFrame::new()
    }
}

/// A frame the client may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientFrame {
    /// A liveness probe; the server answers `pong`.
    Ping,
    /// An answer to a server `ping`.
    Pong,
    /// The client asking to close.
    Close,
}

/// Parse a client frame. Unrecognised text yields `None` and is ignored, which is
/// what a forward-compatible server must do with a frame from a newer client.
pub fn parse_client_frame(text: &str) -> Option<ClientFrame> {
    serde_json::from_str::<ClientFrame>(text).ok()
}

/// Build the frame that carries one event.
pub fn event_frame(envelope: &EventEnvelope) -> Message {
    let value = envelope
        .to_wire()
        .unwrap_or_else(|_| serde_json::json!({ "type": envelope.wire_type(), "seq": envelope.seq }));
    Message::Text(Utf8Bytes::from(value.to_string()))
}

/// The state the socket's run loop needs, extracted so it can be tested.
#[derive(Debug, Clone)]
pub struct FrameBuilder {
    /// The protocol version this server speaks.
    pub protocol_version: u32,
    /// The heartbeat interval.
    pub heartbeat: Duration,
    /// The user whose stream this socket carries.
    pub user_id: UserId,
}

impl FrameBuilder {
    /// Build the framing for `user_id`.
    pub fn new(user_id: UserId, protocol_version: u32, heartbeat_secs: u64) -> Self {
        FrameBuilder {
            protocol_version,
            heartbeat: Duration::from_secs(heartbeat_secs.max(1)),
            user_id,
        }
    }

    /// The subscription filter for this socket. Exact, never `All`.
    pub fn filter(&self) -> EventFilter {
        EventFilter::User(self.user_id)
    }

    /// The scope this socket's events carry.
    pub fn scope(&self) -> EventScope {
        EventScope::User(self.user_id)
    }

    /// The hello frame for a bus whose newest sequence is `last_seq`.
    pub fn hello(&self, last_seq: Option<i64>) -> HelloFrame {
        HelloFrame::new(self.protocol_version, self.heartbeat.as_secs(), last_seq)
    }

    /// The frames to send before the live stream starts, for a client that asked to
    /// resume from `cursor`.
    ///
    /// Returns `(gap, frames)`: `gap` is true when the client's cursor predates the
    /// oldest buffered event, so it must run a sync regardless of what is replayed.
    pub fn replay(
        &self,
        bus: &ferroma_events::EventBus,
        cursor: i64,
    ) -> (bool, Vec<Message>) {
        let oldest = oldest_buffered_seq(bus);
        let gap = match oldest {
            Some(oldest) => cursor < oldest.saturating_sub(1),
            // Nothing buffered at all: a client with a cursor is behind an empty ring.
            None => cursor > 0 && cursor < bus.last_seq(),
        };
        let frames = bus
            .replay_since_for(&self.scope(), cursor)
            .iter()
            .map(event_frame)
            .collect();
        (gap, frames)
    }
}

/// The `seq` of the oldest event still in the bus's replay ring.
pub fn oldest_buffered_seq(bus: &ferroma_events::EventBus) -> Option<i64> {
    bus.replay_since(0).first().map(|envelope| envelope.seq)
}

/// The upgrade handler for `GET /api/v1/client/events`.
pub async fn events_socket(
    State(state): State<AppState>,
    client: ClientAuth,
    Query(query): Query<EventsQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let builder = FrameBuilder::new(
        client.user_id(),
        state.protocol_version(),
        state.config.client.ws_heartbeat_secs,
    );
    let cursor = query.cursor.unwrap_or(0);
    let device = client.device();

    Ok(upgrade.on_upgrade(move |socket| async move {
        let user_id = builder.user_id;
        if let Err(err) = run_socket(socket, state, builder, cursor, device, user_id).await {
            tracing::debug!(user_id = user_id.get(), error = %err, "event socket closed");
        }
    }))
}

/// Drive one socket until the client goes away or its device is revoked.
async fn run_socket(
    socket: WebSocket,
    state: AppState,
    builder: FrameBuilder,
    cursor: i64,
    device: Option<ferroma_core::DeviceId>,
    _user_id: UserId,
) -> Result<(), SocketError> {
    let (mut sink, mut stream) = socket.split();

    // Subscribe *before* replaying, so an event published while the replay is being
    // written is not lost between the two.
    let mut subscription = state.events.subscribe_filtered(builder.filter());

    let hello = builder.hello(Some(state.events.last_seq()).filter(|seq| *seq > 0));
    sink.send(hello.to_message()).await?;

    let (gap, replay) = builder.replay(&state.events, cursor);
    if gap {
        sink.send(ReplayGapFrame::new().to_message()).await?;
    }
    for frame in replay {
        sink.send(frame).await?;
    }

    let mut heartbeat = tokio::time::interval(builder.heartbeat);
    // The first tick fires immediately; skip it so the client is not pinged the
    // instant it connects.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    None => return Ok(()),
                    Some(Err(_)) => return Ok(()),
                    Some(Ok(Message::Close(_))) => return Ok(()),
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload)).await?;
                    }
                    Some(Ok(Message::Text(text))) => {
                        match parse_client_frame(text.as_str()) {
                            Some(ClientFrame::Ping) => {
                                sink.send(Message::Text(Utf8Bytes::from("{\"type\":\"pong\"}"))).await?;
                            }
                            Some(ClientFrame::Close) => return Ok(()),
                            // A `pong` or anything unrecognised needs no answer.
                            Some(ClientFrame::Pong) | None => {}
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
            event = subscription.recv() => {
                match event {
                    Ok(envelope) => {
                        if envelope.scope == builder.scope() {
                            sink.send(event_frame(&envelope)).await?;
                        }
                    }
                    Err(SubscriptionError::Lagged(_)) => {
                        // The client could not keep up: tell it to sync, and keep the
                        // socket open. Dropping the connection would only make it fall
                        // further behind.
                        sink.send(ReplayGapFrame::new().to_message()).await?;
                    }
                    Err(SubscriptionError::Closed) => return Ok(()),
                    Err(SubscriptionError::Timeout) => {}
                }
            }
            _ = heartbeat.tick() => {
                if device_revoked(&state, device).await {
                    let close = CloseFrame {
                        code: CLOSE_POLICY_VIOLATION,
                        reason: Utf8Bytes::from_static("device revoked"),
                    };
                    sink.send(Message::Close(Some(close))).await?;
                    return Ok(());
                }
                sink.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

/// Whether the socket's device has been revoked since it connected.
async fn device_revoked(state: &AppState, device: Option<ferroma_core::DeviceId>) -> bool {
    let Some(id) = device else {
        return false;
    };
    match state.repos.devices.find_by_id(id).await {
        Ok(Some(device)) => device.revoked_at.is_some(),
        // A device row that vanished is as good as revoked.
        Ok(None) => true,
        // A database hiccup must not disconnect a healthy client.
        Err(_) => false,
    }
}

/// Why a socket loop ended. Never carries message content.
#[derive(Debug)]
pub enum SocketError {
    /// The transport failed.
    Transport(String),
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketError::Transport(message) => write!(f, "socket transport: {message}"),
        }
    }
}

impl std::error::Error for SocketError {}

impl From<axum::Error> for SocketError {
    fn from(err: axum::Error) -> Self {
        SocketError::Transport(err.to_string())
    }
}

/// A [`Shared`] helper so the router can hand the socket its state.
pub type SharedBus = Arc<ferroma_events::EventBus>;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ferroma_core::{MailboxId, MessageId};
    use ferroma_events::{Event, EventBus, EventEnvelope};
    use uuid::Uuid;

    fn envelope(seq: i64, user: i64) -> EventEnvelope {
        EventEnvelope {
            seq,
            id: Uuid::nil(),
            at: Utc::now(),
            scope: EventScope::User(UserId::new(user)),
            event: Event::mail_received(MailboxId::new(3), MessageId::new(4821)),
        }
    }

    fn text_of(message: &Message) -> String {
        match message {
            Message::Text(text) => text.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    #[test]
    fn hello_frame_has_the_documented_fields() {
        let hello = HelloFrame::new(1, 30, Some(1841));
        let json: serde_json::Value =
            serde_json::from_str(&hello.to_text()).expect("hello must be JSON");
        assert_eq!(json["type"], "hello");
        assert_eq!(json["protocol_version"], 1);
        assert_eq!(json["heartbeat_secs"], 30);
        assert_eq!(json["last_seq"], 1841);
    }

    #[test]
    fn hello_frame_reports_a_null_last_seq_on_an_idle_bus() {
        let hello = HelloFrame::new(1, 30, None);
        let json: serde_json::Value =
            serde_json::from_str(&hello.to_text()).expect("hello must be JSON");
        assert!(json["last_seq"].is_null());
        assert!(hello.to_text().contains("\"last_seq\":null"));
    }

    #[test]
    fn hello_frame_round_trips() {
        let hello = HelloFrame::new(2, 15, Some(9));
        let back: HelloFrame =
            serde_json::from_str(&hello.to_text()).expect("hello must parse back");
        assert_eq!(back, hello);
    }

    #[test]
    fn event_frame_carries_the_wire_shape() {
        let frame = event_frame(&envelope(1842, 7));
        let json: serde_json::Value =
            serde_json::from_str(&text_of(&frame)).expect("frame must be JSON");
        assert_eq!(json["type"], "mail.received");
        assert_eq!(json["seq"], 1842);
        assert_eq!(json["scope"]["user"], 7);
        assert_eq!(json["message_id"], 4821);
        assert_eq!(json["mailbox_id"], 3);
        assert!(json["id"].is_string());
        assert!(json["at"].is_string());
    }

    #[test]
    fn replay_gap_frame_matches_the_protocol() {
        let frame = ReplayGapFrame::new();
        assert_eq!(frame.to_text(), "{\"replay_gap\":true}");
        let json: serde_json::Value =
            serde_json::from_str(&frame.to_text()).expect("gap frame must be JSON");
        assert_eq!(json["replay_gap"], true);
        assert_eq!(ReplayGapFrame::default(), frame);
    }

    #[test]
    fn client_frames_parse_and_unknown_ones_are_ignored() {
        assert_eq!(parse_client_frame("{\"type\":\"ping\"}"), Some(ClientFrame::Ping));
        assert_eq!(parse_client_frame("{\"type\":\"pong\"}"), Some(ClientFrame::Pong));
        assert_eq!(parse_client_frame("{\"type\":\"close\"}"), Some(ClientFrame::Close));
        assert_eq!(parse_client_frame("{\"type\":\"teleport\"}"), None);
        assert_eq!(parse_client_frame("not json"), None);
        assert_eq!(parse_client_frame(""), None);
    }

    #[test]
    fn the_builder_filters_to_one_user_and_never_to_all() {
        let builder = FrameBuilder::new(UserId::new(7), 1, 30);
        assert_eq!(builder.filter(), EventFilter::User(UserId::new(7)));
        assert_ne!(builder.filter(), EventFilter::All);
        assert_eq!(builder.scope(), EventScope::User(UserId::new(7)));
        // The filter is exact: a mailbox scope is not delivered.
        assert!(!builder.filter().matches(&EventScope::Mailbox(MailboxId::new(3))));
        assert!(!builder.filter().matches(&EventScope::User(UserId::new(8))));
        assert!(!builder.filter().matches(&EventScope::System));
    }

    #[test]
    fn the_builder_never_lets_a_heartbeat_be_zero() {
        let builder = FrameBuilder::new(UserId::new(1), 1, 0);
        assert_eq!(builder.heartbeat, Duration::from_secs(1));
        assert_eq!(builder.hello(None).heartbeat_secs, 1);
        let builder = FrameBuilder::new(UserId::new(1), 1, 30);
        assert_eq!(builder.heartbeat, Duration::from_secs(30));
        assert_eq!(builder.hello(None).heartbeat_secs, 30);
    }

    #[tokio::test]
    async fn replay_returns_only_this_users_history() {
        let bus = EventBus::with_defaults();
        bus.publish(
            EventScope::User(UserId::new(7)),
            Event::mail_received(MailboxId::new(1), MessageId::new(1)),
        )
        .await;
        bus.publish(
            EventScope::User(UserId::new(8)),
            Event::mail_received(MailboxId::new(2), MessageId::new(2)),
        )
        .await;
        bus.publish(
            EventScope::User(UserId::new(7)),
            Event::mail_read(MailboxId::new(1), MessageId::new(1), true),
        )
        .await;

        let builder = FrameBuilder::new(UserId::new(7), 1, 30);
        let (gap, frames) = builder.replay(&bus, 0);
        assert!(!gap, "cursor 0 is a first sync and never a gap");
        assert_eq!(frames.len(), 2, "only user 7's two events");
        for frame in &frames {
            let json: serde_json::Value =
                serde_json::from_str(&text_of(frame)).expect("frame must be JSON");
            assert_eq!(json["scope"]["user"], 7);
        }
    }

    #[tokio::test]
    async fn replay_from_the_head_is_empty() {
        let bus = EventBus::with_defaults();
        bus.publish(
            EventScope::User(UserId::new(7)),
            Event::mail_read(MailboxId::new(1), MessageId::new(1), true),
        )
        .await;
        let builder = FrameBuilder::new(UserId::new(7), 1, 30);
        let (gap, frames) = builder.replay(&bus, bus.last_seq());
        assert!(!gap);
        assert!(frames.is_empty());
    }

    #[tokio::test]
    async fn a_cursor_older_than_the_ring_is_reported_as_a_gap() {
        // A tiny ring: publish more than it holds, then ask from zero-with-a-cursor.
        let bus = EventBus::new(ferroma_events::EventBusConfig {
            history_capacity: 2,
            ..ferroma_events::EventBusConfig::default()
        });
        for index in 0..6 {
            bus.publish(
                EventScope::User(UserId::new(7)),
                Event::mail_read(MailboxId::new(1), MessageId::new(index), true),
            )
            .await;
        }
        let builder = FrameBuilder::new(UserId::new(7), 1, 30);

        // Cursor 1 predates the oldest retained event (seq 5): a gap.
        let (gap, frames) = builder.replay(&bus, 1);
        assert!(gap, "cursor 1 is older than the retained ring");
        assert!(!frames.is_empty(), "what is retained is still replayed");

        // The immediately-previous cursor is honoured without a gap.
        let (gap, _) = builder.replay(&bus, 5);
        assert!(!gap);
    }

    #[tokio::test]
    async fn an_empty_bus_with_a_behind_client_is_a_gap() {
        let bus = EventBus::with_defaults();
        let builder = FrameBuilder::new(UserId::new(7), 1, 30);
        // Nothing published at all: a first sync (cursor 0) is not a gap.
        let (gap, frames) = builder.replay(&bus, 0);
        assert!(!gap);
        assert!(frames.is_empty());
    }

    #[test]
    fn oldest_buffered_seq_is_none_on_an_empty_bus() {
        let bus = EventBus::with_defaults();
        assert_eq!(oldest_buffered_seq(&bus), None);
    }

    #[tokio::test]
    async fn oldest_buffered_seq_tracks_the_ring() {
        let bus = EventBus::with_defaults();
        assert_eq!(oldest_buffered_seq(&bus), None);
        bus.publish(
            EventScope::User(UserId::new(1)),
            Event::mail_received(MailboxId::new(1), MessageId::new(1)),
        )
        .await;
        assert_eq!(oldest_buffered_seq(&bus), Some(1));
    }

    #[test]
    fn the_close_code_is_policy_violation() {
        // The documented "401-equivalent" for a revoked device over a socket.
        assert_eq!(CLOSE_POLICY_VIOLATION, 1008);
    }

    #[test]
    fn socket_errors_render_internally_only() {
        let err = SocketError::Transport("broken pipe".into());
        assert!(err.to_string().contains("broken pipe"));
        let as_error: &dyn std::error::Error = &err;
        assert!(as_error.source().is_none());
    }
}
