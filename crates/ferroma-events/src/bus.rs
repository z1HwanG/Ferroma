//! The bus: live fan-out, per-subscriber filtering and replay.
//!
//! # Shape
//!
//! One process-wide [`EventBus`] sits between `Mail Core` and every consumer
//! (Webmail, official clients over WebSocket, Admin, notifications, webhooks).
//! Publishing is cheap and lock-brief, so a producer never waits for a slow
//! consumer:
//!
//! ```text
//!   Mail Core
//!       │  publish(scope, event)          ┌─ one mutex ─────────────────┐
//!       ├────────────────────────────────► │ seq:  i64  (last_seq += 1)  │
//!       │                                  │ ring: VecDeque<Envelope>    │
//!       │                                  └────────────┬────────────────┘
//!       │                                               │ broadcast::send
//!       │                                               ▼
//!       │                                  broadcast channel (bounded, drop-oldest)
//!       │                                               │
//!       │                            ┌──────────────────┴──────────────────┐
//!       │                            ▼                                     ▼
//!       └─ replay_since(after) ─► [ replay ring ]              Subscription (live)
//!                                                               ├─ EventFilter::matches
//!                                                               └─ recv / try_recv / Stream
//! ```
//!
//! * **Live delivery** uses [`tokio::sync::broadcast`], so a slow subscriber
//!   falls behind and gets [`SubscriptionError::Lagged`] instead of blocking the
//!   publisher — the key property the bus must guarantee.
//! * **Replay** keeps the last [`EventBusConfig::history_capacity`] envelopes in a
//!   ring buffer, so a reconnecting WebSocket client can ask for everything after
//!   the last `seq` it processed ([`EventBus::replay_since`]).
//! * **Filtering** happens per subscriber ([`EventFilter`]), which is the
//!   authorisation boundary: a subscriber filtered to `User(7)` never observes a
//!   frame belonging to anybody else.
//!
//! # Concurrency contract
//!
//! `seq` is strictly monotonic with no duplicates and no gaps, even when many
//! tasks publish at once: the counter and the replay ring are mutated under one
//! mutex and the broadcast happens while that mutex is held, so subscribers also
//! observe events in `seq` order.
//!
//! # Lifetime
//!
//! [`EventBus`] is a cloneable handle over shared state. The broadcast channel
//! closes — and every [`Subscription`] ends — only once the last handle **and**
//! every subscription have been dropped. A subscription deliberately does not own
//! the bus (that would be a subscription → sender → receiver cycle that never
//! closes); it shares only the process-wide lag counter.
//!
//! ```no_run
//! # async fn demo() {
//! use ferroma_events::{Event, EventBus, EventScope};
//! use ferroma_core::{MailboxId, MessageId, UserId};
//!
//! let bus = EventBus::with_defaults();
//! let mut sub = bus.subscribe_filtered(EventScope::User(UserId::new(7)).into());
//!
//! bus.publish(
//!     EventScope::User(UserId::new(7)),
//!     Event::mail_received(MailboxId::new(1), MessageId::new(2)),
//! )
//! .await;
//!
//! // Catch a client up after a reconnect.
//! let missed = bus.replay_since_for(&EventScope::User(UserId::new(7)), 0);
//! assert_eq!(missed.len(), 1);
//! # let _ = sub;
//! # }
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use ferroma_core::{MailboxId, UserId};
use futures_util::Stream;
use tokio::sync::broadcast;

use crate::event::{Event, EventEnvelope, EventScope};

/// Bus tuning knobs.
///
/// The defaults ([`EventBusConfig::default`]) are sized for a single Ferroma
/// server: roughly a megabyte of replay history and half a thousand pending
/// frames per subscriber before it is considered lagging.
#[derive(Debug, Clone)]
pub struct EventBusConfig {
    /// How many events to keep for replay after a client reconnects.
    pub history_capacity: usize,
    /// Per-subscriber queue depth before a slow subscriber starts lagging.
    pub channel_capacity: usize,
    /// Drop the oldest events for a lagging subscriber instead of blocking the publisher.
    ///
    /// **This is the supported (and default) mode.** The bus is built on
    /// `tokio::sync::broadcast`, whose ring buffer semantics *are* drop-oldest:
    /// the flag therefore documents the contract rather than switching strategies.
    /// Setting it to `false` asks for publisher back-pressure, which this bus does
    /// not implement — a stateful per-subscriber queue with `send().await` would
    /// make [`EventBus::publish`] able to stall `Mail Core` on one slow WebSocket
    /// client, and would leave [`EventBus::publish_nowait`] with no non-blocking
    /// equivalent. A `false` value is accepted but behaves as `true`; see the
    /// crate's known limitations.
    pub drop_on_lag: bool,
}

impl Default for EventBusConfig {
    /// `history_capacity` 1024, `channel_capacity` 512, `drop_on_lag` `true`.
    fn default() -> Self {
        EventBusConfig {
            history_capacity: 1024,
            channel_capacity: 512,
            drop_on_lag: true,
        }
    }
}

impl EventBusConfig {
    /// Clamp the capacities into a range the bus can actually honour.
    ///
    /// `tokio::sync::broadcast` panics on a zero capacity, and a zero-length
    /// history would silently break reconnect catch-up, so a misconfigured
    /// deployment gets a usable bus instead of a crash on startup.
    fn sanitised(&self) -> EventBusConfig {
        EventBusConfig {
            history_capacity: self.history_capacity.max(1),
            channel_capacity: self.channel_capacity.max(1),
            drop_on_lag: self.drop_on_lag,
        }
    }
}

/// Shared state behind one mutex: the replay ring and the sequence counter.
///
/// Keeping both together is what makes `seq` gap-free — a publisher cannot take
/// a sequence number without also committing its envelope to the ring.
#[derive(Debug)]
struct Shared {
    /// Replay ring, oldest first. Bounded by [`EventBusConfig::history_capacity`].
    history: VecDeque<EventEnvelope>,
    /// The `seq` of the most recently published event (`0` before the first).
    last_seq: i64,
}

/// The reference-counted guts of an [`EventBus`].
#[derive(Debug)]
struct Inner {
    /// Effective (sanitised) configuration.
    config: EventBusConfig,
    /// Live fan-out. Every subscriber gets a receiver from this sender.
    sender: broadcast::Sender<EventEnvelope>,
    /// Sequence + replay ring, guarded by one mutex.
    shared: Mutex<Shared>,
    /// Events dropped because some subscriber lagged, since process start.
    ///
    /// Held behind its own `Arc` (not inside `Inner`) so that a [`Subscription`]
    /// can report process-wide lag *without* keeping the `Sender` alive: a
    /// subscription that owned an `Arc<Inner>` would form a
    /// subscription → sender → receiver cycle, and the channel would never close.
    lagged: Arc<AtomicU64>,
}

/// The platform event bus.
///
/// Cheap to clone (it is an `Arc` handle) and safe to share across tasks, so the
/// whole server can hold one and treat it as a global.
#[derive(Clone)]
pub struct EventBus {
    /// Shared guts; the last handle dropped closes the broadcast channel, which
    /// ends every [`Subscription`].
    inner: Arc<Inner>,
}

impl fmt::Debug for EventBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventBus")
            .field("last_seq", &self.last_seq())
            .field("subscribers", &self.subscriber_count())
            .field("lagged", &self.lagged_total())
            .finish_non_exhaustive()
    }
}

impl Default for EventBus {
    /// Same as [`EventBus::with_defaults`].
    fn default() -> Self {
        EventBus::with_defaults()
    }
}

impl EventBus {
    /// Build a bus with an explicit configuration.
    ///
    /// Zero capacities are clamped up to one; see [`EventBusConfig::sanitised`].
    #[must_use]
    pub fn new(config: EventBusConfig) -> Self {
        let config = config.sanitised();
        let (sender, _receiver) = broadcast::channel(config.channel_capacity);

        EventBus {
            inner: Arc::new(Inner {
                config,
                sender,
                shared: Mutex::new(Shared {
                    history: VecDeque::new(),
                    last_seq: 0,
                }),
                lagged: Arc::new(AtomicU64::new(0)),
            }),
        }
    }

    /// Build a bus with [`EventBusConfig::default`].
    #[must_use]
    pub fn with_defaults() -> Self {
        EventBus::new(EventBusConfig::default())
    }

    /// The effective configuration (after clamping).
    #[must_use]
    pub fn config(&self) -> &EventBusConfig {
        &self.inner.config
    }

    /// Subscribe to every event.
    ///
    /// Equivalent to `subscribe_filtered(EventFilter::All)`. This is what an
    /// in-process consumer such as the notification or webhook worker uses; a
    /// *client* connection must use [`EventBus::subscribe_filtered`] instead.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        self.subscribe_filtered(EventFilter::All)
    }

    /// Subscribe with a server-side filter, so a client only receives its own stream.
    #[must_use]
    pub fn subscribe_filtered(&self, filter: EventFilter) -> Subscription {
        Subscription {
            receiver: self.inner.sender.subscribe(),
            filter,
            // Only the lag counter is shared with the subscription, never the bus
            // itself — otherwise the subscription would own the sender and the
            // channel could never close.
            lagged_total: Arc::clone(&self.inner.lagged),
            lagged: 0,
        }
    }

    /// Publish and await delivery into the broadcast channel. Assigns `seq`/`id`/`at`.
    ///
    /// # Blocking behaviour
    ///
    /// With [`EventBusConfig::drop_on_lag`] set (the default) this never waits on
    /// a subscriber: `tokio::sync::broadcast` drops the oldest queued frames of a
    /// lagging receiver. It pauses only for the brief internal mutex, which is
    /// never held across an `await`.
    pub async fn publish(&self, scope: EventScope, event: Event) -> EventEnvelope {
        self.publish_nowait(scope, event)
    }

    /// Publish from a non-async context (never blocks; same as `publish` but infallible).
    ///
    /// Used by the storage layer and by signal handlers, where an event is the
    /// side effect of work already done and must not be able to fail or stall.
    pub fn publish_nowait(&self, scope: EventScope, event: Event) -> EventEnvelope {
        let envelope = {
            let mut shared = self
                .inner
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            shared.last_seq += 1;
            let envelope = EventEnvelope::new(scope, event, shared.last_seq);

            if self.inner.config.history_capacity > 0 {
                shared.history.push_back(envelope.clone());
                while shared.history.len() > self.inner.config.history_capacity {
                    shared.history.pop_front();
                }
            }

            // The broadcast is issued while the lock is held (and it never blocks
            // unless `drop_on_lag` is off), so subscribers observe `seq` order.
            // `send` only reports "no live receivers"; the envelope is in the ring.
            let _live_receivers = self.inner.sender.send(envelope.clone());

            envelope
        };

        tracing::trace!(
            seq = envelope.seq,
            event = envelope.event.name(),
            scope = %envelope.scope,
            "event published"
        );

        envelope
    }

    /// Events with `seq > after`, for WebSocket catch-up on reconnect.
    ///
    /// `after = 0` yields everything still buffered; a value at or beyond the head
    /// yields an empty vector. Because the ring is bounded, a client whose cursor
    /// is older than the oldest buffered event gets what is still available — the
    /// caller should fall back to a full mailbox sync in that case (detectable by
    /// comparing `after` with the first returned `seq`).
    #[must_use]
    pub fn replay_since(&self, after: i64) -> Vec<EventEnvelope> {
        let shared = self
            .inner
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        shared
            .history
            .iter()
            .filter(|envelope| envelope.seq > after)
            .cloned()
            .collect()
    }

    /// Replay filtered to one scope's owner.
    ///
    /// A `User(7)` scope yields only events whose scope is that same user; a
    /// `System` scope yields only system-wide events. This is what a WebSocket
    /// reconnect path calls, so it can never be used to read somebody else's
    /// history.
    #[must_use]
    pub fn replay_since_for(&self, scope: &EventScope, after: i64) -> Vec<EventEnvelope> {
        let filter = EventFilter::from(scope.clone());
        self.replay_since(after)
            .into_iter()
            .filter(|envelope| filter.matches(&envelope.scope))
            .collect()
    }

    /// How many live subscriptions exist right now.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner.sender.receiver_count()
    }

    /// The `seq` of the most recently published event (`0` when nothing has been published).
    #[must_use]
    pub fn last_seq(&self) -> i64 {
        let shared = self
            .inner
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.last_seq
    }

    /// How many envelopes the replay ring currently holds.
    #[must_use]
    pub fn history_len(&self) -> usize {
        let shared = self
            .inner
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.history.len()
    }

    /// Number of events dropped because a subscriber lagged.
    ///
    /// Counted per lagging receiver: one slow WebSocket connection falling 300
    /// frames behind adds 300. Surfaced as a metric so an operator can see a
    /// client that cannot keep up.
    #[must_use]
    pub fn lagged_total(&self) -> u64 {
        self.inner.lagged.load(Ordering::Relaxed)
    }

}

/// Server-side subscription filter.
///
/// Applied inside [`Subscription`] as events are pulled, so a filtered
/// subscriber never even materialises another user's envelope in application code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventFilter {
    /// Everything, including other users' streams. For in-process workers only.
    All,
    /// One user's private stream.
    User(UserId),
    /// One mailbox's stream.
    Mailbox(MailboxId),
    /// Server-wide events only.
    System,
}

impl EventFilter {
    /// Whether an event with this scope should be delivered to the subscriber.
    ///
    /// Matching is exact: a user filter does not match a mailbox scope (the
    /// server resolves mailbox scopes to their owner before subscribing) and a
    /// mailbox filter does not match a user scope.
    #[must_use]
    pub fn matches(&self, scope: &EventScope) -> bool {
        match (self, scope) {
            (EventFilter::All, _) => true,
            (EventFilter::User(wanted), EventScope::User(actual)) => wanted == actual,
            (EventFilter::Mailbox(wanted), EventScope::Mailbox(actual)) => wanted == actual,
            (EventFilter::System, EventScope::System) => true,
            _ => false,
        }
    }
}

impl From<EventScope> for EventFilter {
    /// The filter that delivers exactly the given scope.
    fn from(scope: EventScope) -> Self {
        match scope {
            EventScope::User(id) => EventFilter::User(id),
            EventScope::Mailbox(id) => EventFilter::Mailbox(id),
            EventScope::System => EventFilter::System,
        }
    }
}

impl From<&EventScope> for EventFilter {
    /// The filter that delivers exactly the given scope.
    fn from(scope: &EventScope) -> Self {
        EventFilter::from(scope.clone())
    }
}

impl fmt::Display for EventFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventFilter::All => f.write_str("all"),
            EventFilter::User(id) => write!(f, "user:{}", id.get()),
            EventFilter::Mailbox(id) => write!(f, "mailbox:{}", id.get()),
            EventFilter::System => f.write_str("system"),
        }
    }
}

/// A live subscription. Dropping it unsubscribes.
///
/// Backed by a [`tokio::sync::broadcast`] receiver, so it has its own bounded
/// queue: if the consumer stops reading while others publish, the oldest frames
/// are dropped and [`Subscription::recv`] reports [`SubscriptionError::Lagged`]
/// with the number skipped.
pub struct Subscription {
    /// The live feed from the bus.
    receiver: broadcast::Receiver<EventEnvelope>,
    /// Server-side filter applied as frames are pulled.
    filter: EventFilter,
    /// Process-wide lag counter, shared with the bus for metrics.
    lagged_total: Arc<AtomicU64>,
    /// Frames this subscription itself missed because it lagged.
    lagged: u64,
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("filter", &self.filter)
            .field("lagged", &self.lagged)
            .field("total_lagged", &self.lagged_total.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Subscription {
    /// Next event, waiting for it. Returns `Err(SubscriptionError::Lagged(n))` when
    /// the subscriber fell behind (the bus dropped `n` events).
    pub async fn recv(&mut self) -> std::result::Result<EventEnvelope, SubscriptionError> {
        loop {
            match self.receiver.recv().await {
                Ok(envelope) => {
                    if self.filter.matches(&envelope.scope) {
                        return Ok(envelope);
                    }
                    // Filtered out: keep waiting without yielding it upstream.
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Err(self.on_lag(n));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(SubscriptionError::Closed);
                }
            }
        }
    }

    /// Next event with a timeout.
    ///
    /// Returns [`SubscriptionError::Timeout`] when `timeout` elapses first. A
    /// timeout is not a failure: the subscription stays open and the caller
    /// typically sends a WebSocket ping and calls `recv_timeout` again.
    pub async fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::result::Result<EventEnvelope, SubscriptionError> {
        match tokio::time::timeout(timeout, self.recv()).await {
            Ok(result) => result,
            Err(_) => Err(SubscriptionError::Timeout),
        }
    }

    /// Non-blocking poll.
    ///
    /// Returns [`SubscriptionError::Timeout`] when nothing is queued right now
    /// (the in-process equivalent of "try again later"), which makes it usable in
    /// a synchronous drain loop.
    pub fn try_recv(&mut self) -> std::result::Result<EventEnvelope, SubscriptionError> {
        loop {
            match self.receiver.try_recv() {
                Ok(envelope) => {
                    if self.filter.matches(&envelope.scope) {
                        return Ok(envelope);
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    return Err(self.on_lag(n));
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    return Err(SubscriptionError::Timeout);
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(SubscriptionError::Closed);
                }
            }
        }
    }

    /// Drain everything queued right now without blocking.
    ///
    /// Stops at the first lag or empty queue, so a consumer that has fallen
    /// behind can notice and resynchronise.
    #[must_use]
    pub fn drain(&mut self) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        while let Ok(envelope) = self.try_recv() {
            out.push(envelope);
        }
        out
    }

    /// Make this subscription an async `Stream` of `EventEnvelope` (skip lag errors,
    /// which the caller surfaces through [`Subscription::lagged`] instead).
    ///
    /// The returned [`EventStream`] *is* the subscription (it polls
    /// [`Subscription::recv`] internally), so the lag tally stays reachable via
    /// [`EventStream::lagged`] after the conversion.
    #[must_use]
    pub fn into_stream(self) -> EventStream {
        EventStream { subscription: self }
    }

    /// Total events this subscription missed because it lagged.
    #[must_use]
    pub fn lagged(&self) -> u64 {
        self.lagged
    }

    /// The filter this subscription was created with.
    ///
    /// Named `event_filter` rather than `filter` so that
    /// `futures_util::StreamExt::filter` stays callable on a subscription.
    #[must_use]
    pub fn event_filter(&self) -> &EventFilter {
        &self.filter
    }

    /// `true` once the bus has been dropped and no further events can arrive.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }

    /// Count a lag reported by `broadcast`, bumping the per-subscription and
    /// process-wide counters.
    fn on_lag(&mut self, n: u64) -> SubscriptionError {
        self.lagged += n;
        self.lagged_total.fetch_add(n, Ordering::Relaxed);
        tracing::debug!(
            filter = %self.filter,
            dropped = n,
            total = self.lagged,
            "event subscriber lagged; frames dropped"
        );
        SubscriptionError::Lagged(n)
    }
}

impl Stream for Subscription {
    type Item = EventEnvelope;

    /// Yields the next matching envelope, skipping lag reports.
    ///
    /// The stream ends (returns `None`) when the bus is dropped. A subscriber that
    /// lagged can still inspect the total through [`Subscription::lagged`] — but
    /// note that consuming the stream moves the subscription; use
    /// [`Subscription::recv`] directly when the lag count matters.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let polled = {
                let mut recv = std::pin::pin!(this.recv());
                recv.as_mut().poll(cx)
            };

            match polled {
                Poll::Ready(Ok(envelope)) => return Poll::Ready(Some(envelope)),
                // Lag is reported through `Subscription::lagged`, not as an item.
                Poll::Ready(Err(SubscriptionError::Lagged(_))) => {}
                // The bus is gone: end the stream.
                Poll::Ready(Err(SubscriptionError::Closed)) => return Poll::Ready(None),
                // `recv` has no deadline, so this cannot happen; keep waiting.
                Poll::Ready(Err(SubscriptionError::Timeout)) => {}
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// An [`EventEnvelope`] stream produced by [`Subscription::into_stream`].
///
/// Lag reports are swallowed here; the subscription's own counter keeps the
/// tally (see [`Subscription::lagged`]).
#[derive(Debug)]
pub struct EventStream {
    /// The wrapped subscription.
    subscription: Subscription,
}

impl EventStream {
    /// Total events this stream missed because it lagged.
    #[must_use]
    pub fn lagged(&self) -> u64 {
        self.subscription.lagged()
    }

    /// The filter this stream was created with.
    ///
    /// Named `event_filter` rather than `filter` on purpose: this type is a
    /// `Stream`, and `futures_util::StreamExt::filter` would otherwise be shadowed
    /// by the inherent method.
    #[must_use]
    pub fn event_filter(&self) -> &EventFilter {
        self.subscription.event_filter()
    }

    /// Borrow the underlying subscription.
    #[must_use]
    pub fn subscription(&self) -> &Subscription {
        &self.subscription
    }
}

impl Stream for EventStream {
    type Item = EventEnvelope;

    /// Yields matching envelopes; lag reports are skipped, and the stream ends
    /// when the bus is dropped.
    ///
    /// Polling builds a fresh [`Subscription::recv`] future per attempt. That is
    /// sound because `tokio::sync::broadcast`'s receive is cancel-safe: dropping a
    /// half-polled future never consumes a frame.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let subscription = &mut self.get_mut().subscription;
        loop {
            let polled = {
                let mut recv = std::pin::pin!(subscription.recv());
                recv.as_mut().poll(cx)
            };

            match polled {
                Poll::Ready(Ok(envelope)) => return Poll::Ready(Some(envelope)),
                // Lag is surfaced through `EventStream::lagged` instead.
                Poll::Ready(Err(SubscriptionError::Lagged(_))) => {}
                // The bus is gone: end the stream.
                Poll::Ready(Err(SubscriptionError::Closed)) => return Poll::Ready(None),
                // Unreachable through `recv`, which has no deadline of its own.
                Poll::Ready(Err(SubscriptionError::Timeout)) => {}
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Why a [`Subscription`] failed to produce an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionError {
    /// The subscriber fell behind and the bus dropped `n` events for it.
    ///
    /// Recoverable: resynchronise from the mailbox (or via `replay_since`) and
    /// keep reading. The count is the number of frames missed.
    Lagged(u64),
    /// The bus was dropped; no further events can arrive.
    Closed,
    /// Nothing was available before the deadline (or in the queue, for `try_recv`).
    Timeout,
}

impl fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubscriptionError::Lagged(n) => write!(f, "subscription lagged, {n} event(s) dropped"),
            SubscriptionError::Closed => f.write_str("event bus closed"),
            SubscriptionError::Timeout => f.write_str("no event available before the timeout"),
        }
    }
}

impl std::error::Error for SubscriptionError {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use futures_util::StreamExt;
    use tokio::task::JoinSet;

    use super::*;
    use crate::event::MailReceived;

    fn user(raw: i64) -> EventScope {
        EventScope::User(UserId::new(raw))
    }

    fn mailbox(raw: i64) -> EventScope {
        EventScope::Mailbox(MailboxId::new(raw))
    }

    fn received(mailbox_id: i64, message_id: i64) -> Event {
        Event::mail_received(MailboxId::new(mailbox_id), ferroma_core::MessageId::new(message_id))
    }

    fn tiny_bus(channel_capacity: usize) -> EventBus {
        EventBus::new(EventBusConfig {
            history_capacity: 8,
            channel_capacity,
            drop_on_lag: true,
        })
    }

    #[test]
    fn default_config_matches_the_spec() {
        let config = EventBusConfig::default();
        assert_eq!(config.history_capacity, 1024);
        assert_eq!(config.channel_capacity, 512);
        assert!(config.drop_on_lag);
    }

    #[test]
    fn zero_capacities_are_clamped_not_fatal() {
        let bus = EventBus::new(EventBusConfig {
            history_capacity: 0,
            channel_capacity: 0,
            drop_on_lag: true,
        });
        assert_eq!(bus.config().history_capacity, 1);
        assert_eq!(bus.config().channel_capacity, 1);
        let mut sub = bus.subscribe();
        bus.publish_nowait(EventScope::System, received(1, 1));
        assert_eq!(sub.try_recv().unwrap().seq, 1);
    }

    #[test]
    fn from_scope_builds_the_matching_filter() {
        assert_eq!(EventFilter::from(user(7)), EventFilter::User(UserId::new(7)));
        assert_eq!(
            EventFilter::from(mailbox(3)),
            EventFilter::Mailbox(MailboxId::new(3))
        );
        assert_eq!(EventFilter::from(EventScope::System), EventFilter::System);
        assert_eq!(EventFilter::from(&user(7)), EventFilter::User(UserId::new(7)));
    }

    #[test]
    fn filter_matching_is_exact_and_never_crosses_streams() {
        let all = EventFilter::All;
        assert!(all.matches(&user(1)));
        assert!(all.matches(&mailbox(1)));
        assert!(all.matches(&EventScope::System));

        let u7 = EventFilter::User(UserId::new(7));
        assert!(u7.matches(&user(7)));
        assert!(!u7.matches(&user(8)));
        assert!(!u7.matches(&mailbox(7)));
        assert!(!u7.matches(&EventScope::System));

        let m7 = EventFilter::Mailbox(MailboxId::new(7));
        assert!(m7.matches(&mailbox(7)));
        assert!(!m7.matches(&mailbox(8)));
        assert!(!m7.matches(&user(7)));
        assert!(!m7.matches(&EventScope::System));

        let sys = EventFilter::System;
        assert!(sys.matches(&EventScope::System));
        assert!(!sys.matches(&user(7)));
        assert!(!sys.matches(&mailbox(7)));
    }

    #[test]
    fn filter_display_is_stable() {
        assert_eq!(EventFilter::All.to_string(), "all");
        assert_eq!(EventFilter::User(UserId::new(7)).to_string(), "user:7");
        assert_eq!(EventFilter::Mailbox(MailboxId::new(3)).to_string(), "mailbox:3");
        assert_eq!(EventFilter::System.to_string(), "system");
    }

    #[tokio::test]
    async fn publish_assigns_seq_id_and_timestamp() {
        let bus = EventBus::with_defaults();
        let before = chrono::Utc::now();
        let envelope = bus.publish(user(1), received(1, 1)).await;

        assert_eq!(envelope.seq, 1);
        assert_eq!(envelope.scope, user(1));
        assert_eq!(envelope.event.name(), "mail.received");
        assert!(envelope.at >= before);
        assert!(envelope.at <= chrono::Utc::now());

        let second = bus.publish_nowait(user(1), received(1, 2));
        assert_eq!(second.seq, 2);
        assert_ne!(second.id, envelope.id, "ids must be unique");
    }

    #[tokio::test]
    async fn last_seq_and_history_len_track_publishing() {
        let bus = tiny_bus(16);
        assert_eq!(bus.last_seq(), 0);
        assert_eq!(bus.history_len(), 0);

        for i in 1..=3 {
            bus.publish(EventScope::System, received(1, i)).await;
        }
        assert_eq!(bus.last_seq(), 3);
        assert_eq!(bus.history_len(), 3);
    }

    #[tokio::test]
    async fn history_ring_stays_bounded() {
        let bus = tiny_bus(64);
        let capacity = bus.config().history_capacity;
        for i in 1..=50 {
            bus.publish(EventScope::System, received(1, i)).await;
        }
        assert_eq!(bus.history_len(), capacity);
        let replayed = bus.replay_since(0);
        assert_eq!(replayed.len(), capacity);
        // The oldest entries were evicted, the newest kept.
        assert_eq!(replayed.last().unwrap().seq, 50);
        assert_eq!(replayed.first().unwrap().seq, 50 - capacity as i64 + 1);
    }

    #[tokio::test]
    async fn seq_is_strictly_monotonic_under_concurrency() {
        let bus = EventBus::with_defaults();
        const TASKS: usize = 8;
        const PER_TASK: usize = 50;

        let mut set = JoinSet::new();
        for task in 0..TASKS {
            let bus = bus.clone();
            set.spawn(async move {
                let mut seqs = Vec::with_capacity(PER_TASK);
                for i in 0..PER_TASK {
                    let envelope = bus
                        .publish(user(task as i64), received(task as i64, i as i64))
                        .await;
                    seqs.push(envelope.seq);
                }
                seqs
            });
        }

        let mut all = Vec::with_capacity(TASKS * PER_TASK);
        while let Some(result) = set.join_next().await {
            all.extend(result.unwrap());
        }

        all.sort_unstable();
        let expected: Vec<i64> = (1..=(TASKS * PER_TASK) as i64).collect();
        assert_eq!(all, expected, "no duplicates, no gaps, starts at 1");
        assert_eq!(bus.last_seq(), (TASKS * PER_TASK) as i64);
    }

    #[tokio::test]
    async fn a_subscriber_sees_every_seq_exactly_once_under_concurrency() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        const TASKS: usize = 6;
        const PER_TASK: usize = 25;

        let mut set = JoinSet::new();
        for _ in 0..TASKS {
            let bus = bus.clone();
            set.spawn(async move {
                for i in 0..PER_TASK {
                    bus.publish(user(1), received(1, i as i64)).await;
                }
            });
        }

        let collector = tokio::spawn(async move {
            let mut seqs = Vec::new();
            while seqs.len() < TASKS * PER_TASK {
                match sub.recv().await {
                    Ok(envelope) => seqs.push(envelope.seq),
                    Err(SubscriptionError::Lagged(_)) => continue,
                    Err(other) => panic!("unexpected: {other}"),
                }
            }
            seqs
        });

        while set.join_next().await.is_some() {}
        let seqs = collector.await.unwrap();

        let expected: Vec<i64> = (1..=(TASKS * PER_TASK) as i64).collect();
        assert_eq!(seqs, expected, "delivery order matches seq order");
    }

    #[tokio::test]
    async fn replay_since_boundaries() {
        let bus = EventBus::with_defaults();
        for i in 1..=5 {
            bus.publish(EventScope::System, received(1, i)).await;
        }

        // Zero / negative: everything buffered.
        assert_eq!(bus.replay_since(0).len(), 5);
        assert_eq!(bus.replay_since(-10).len(), 5);

        // Mid-head: strictly greater than `after`.
        let mid = bus.replay_since(3);
        assert_eq!(mid.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);

        // Exactly at head: nothing new.
        assert!(bus.replay_since(5).is_empty());

        // Beyond head: nothing, and no panic.
        assert!(bus.replay_since(999).is_empty());
        assert!(bus.replay_since(i64::MAX).is_empty());
    }

    #[tokio::test]
    async fn replay_on_an_empty_bus_is_empty() {
        let bus = EventBus::with_defaults();
        assert!(bus.replay_since(0).is_empty());
        assert!(bus.replay_since_for(&EventScope::System, 0).is_empty());
    }

    #[tokio::test]
    async fn replay_since_for_only_returns_the_owners_stream() {
        let bus = EventBus::with_defaults();
        bus.publish(user(7), received(1, 1)).await;
        bus.publish(user(8), received(2, 2)).await;
        bus.publish(mailbox(3), received(3, 3)).await;
        bus.publish(EventScope::System, received(4, 4)).await;
        bus.publish(user(7), received(5, 5)).await;

        let mine = bus.replay_since_for(&user(7), 0);
        assert_eq!(mine.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 5]);

        let theirs = bus.replay_since_for(&user(8), 0);
        assert_eq!(theirs.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);

        let mailbox_events = bus.replay_since_for(&mailbox(3), 0);
        assert_eq!(mailbox_events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3]);

        let system = bus.replay_since_for(&EventScope::System, 0);
        assert_eq!(system.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4]);

        // Combined with `after`.
        assert_eq!(bus.replay_since_for(&user(7), 1).len(), 1);
    }

    #[tokio::test]
    async fn filtered_subscription_is_isolated_from_other_users() {
        let bus = EventBus::with_defaults();
        let mut alice = bus.subscribe_filtered(EventFilter::User(UserId::new(7)));
        let mut bob = bus.subscribe_filtered(EventFilter::User(UserId::new(8)));
        let mut everyone = bus.subscribe();

        bus.publish(user(8), received(1, 10)).await;
        bus.publish(user(7), received(1, 11)).await;
        bus.publish(user(9), received(1, 12)).await;
        bus.publish(user(7), received(1, 13)).await;

        let mine = alice.drain();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|e| e.scope == user(7)));
        assert_eq!(mine.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 4]);

        let theirs = bob.drain();
        assert_eq!(theirs.len(), 1);
        assert!(theirs.iter().all(|e| e.scope == user(8)));

        let all = everyone.drain();
        assert_eq!(all.len(), 4);
    }

    #[tokio::test]
    async fn filtered_subscription_skips_other_scopes_while_blocked_on_recv() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe_filtered(EventFilter::Mailbox(MailboxId::new(3)));

        bus.publish(user(1), received(1, 1)).await;
        bus.publish(EventScope::System, received(1, 2)).await;
        bus.publish(mailbox(4), received(4, 3)).await;
        bus.publish(mailbox(3), received(3, 4)).await;

        // `recv` must skip past the three frames that do not match, not return them.
        let envelope = sub.recv().await.unwrap();
        assert_eq!(envelope.scope, mailbox(3));
        assert_eq!(envelope.seq, 4);
        assert!(sub.try_recv().is_err());
    }

    #[tokio::test]
    async fn system_filter_only_matches_system_events() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe_filtered(EventFilter::System);

        bus.publish(user(1), received(1, 1)).await;
        bus.publish(EventScope::System, received(1, 2)).await;

        let envelope = sub.recv().await.unwrap();
        assert_eq!(envelope.scope, EventScope::System);
        assert_eq!(envelope.seq, 2);
    }

    #[tokio::test]
    async fn recv_waits_for_the_next_event() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        let publisher = bus.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            publisher.publish(EventScope::System, received(1, 1)).await;
        });

        let envelope = sub.recv().await.unwrap();
        assert_eq!(envelope.seq, 1);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn recv_timeout_reports_timeout_without_closing() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();

        let err = sub
            .recv_timeout(Duration::from_millis(20))
            .await
            .expect_err("no event was published");
        assert_eq!(err, SubscriptionError::Timeout);

        // Still usable afterwards.
        bus.publish(EventScope::System, received(1, 1)).await;
        assert_eq!(
            sub.recv_timeout(Duration::from_millis(500)).await.unwrap().seq,
            1
        );
    }

    #[tokio::test]
    async fn try_recv_reports_timeout_when_the_queue_is_empty() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        assert_eq!(sub.try_recv().unwrap_err(), SubscriptionError::Timeout);

        bus.publish(EventScope::System, received(1, 1)).await;
        assert_eq!(sub.try_recv().unwrap().seq, 1);
        assert_eq!(sub.try_recv().unwrap_err(), SubscriptionError::Timeout);
    }

    #[tokio::test]
    async fn lagging_subscriber_is_reported_not_fatal_for_the_publisher() {
        let bus = tiny_bus(4);
        let mut slow = bus.subscribe();

        // Publish well past the queue depth without ever reading, and assert the
        // publisher never blocks (this `await` returning *is* the assertion).
        for i in 1..=20 {
            bus.publish(EventScope::System, received(1, i)).await;
        }

        let err = slow.try_recv().unwrap_err();
        match err {
            SubscriptionError::Lagged(n) => assert!(n >= 15, "expected a big drop, got {n}"),
            other => panic!("expected Lagged, got {other}"),
        }
        assert!(slow.lagged() >= 15);
        assert!(bus.lagged_total() >= 15);

        // After reporting the lag the subscription keeps working from the head.
        let envelope = slow.recv().await.unwrap();
        assert!(envelope.seq > 20 - 4, "head of the queue, got {}", envelope.seq);
    }

    #[tokio::test]
    async fn one_slow_subscriber_does_not_starve_a_fast_one() {
        // Depth-4 queues, 30 events: neither subscriber can hold the whole run, so
        // the slow one is guaranteed to lag while the fast one keeps up.
        let bus = tiny_bus(4);
        let mut slow = bus.subscribe();
        let mut fast = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);

        let mut seen = Vec::new();
        for i in 1..=30 {
            bus.publish(EventScope::System, received(1, i)).await;
            // Read on every publish: the fast subscriber never falls behind.
            if let Ok(envelope) = fast.try_recv() {
                seen.push(envelope.seq);
            }
        }

        assert_eq!(
            seen,
            (1..=30).collect::<Vec<_>>(),
            "the fast subscriber kept up and saw every event, in order"
        );
        assert_eq!(fast.lagged(), 0);

        // The slow subscriber, which never read while 30 events went past a
        // depth-4 queue, lost almost all of them — and the publisher was never
        // blocked by it (this test completing is the proof).
        let mut recovered_by_slow = 0;
        loop {
            match slow.try_recv() {
                Ok(_) => recovered_by_slow += 1,
                Err(SubscriptionError::Lagged(n)) => {
                    assert!(n >= 1);
                    continue;
                }
                Err(_) => break,
            }
        }
        assert!(
            slow.lagged() >= 25,
            "the slow subscriber dropped almost everything, got {}",
            slow.lagged()
        );
        assert!(recovered_by_slow <= 5, "only the tail survived");

        // And the replay ring still holds the tail of the run, which is what a
        // reconnecting WebSocket client resumes from. It is bounded by
        // `history_capacity` (8 here), so an old cursor must fall back to a full
        // mailbox sync — detectable by comparing the cursor with the first
        // replayed `seq`.
        let replayed: Vec<i64> = bus.replay_since(0).iter().map(|e| e.seq).collect();
        assert_eq!(replayed, (23..=30).collect::<Vec<_>>());
        assert!(bus.replay_since(29).iter().all(|e| e.seq == 30));
        assert!(bus.lagged_total() > 0);
    }

    #[tokio::test]
    async fn lag_is_counted_per_subscription_and_in_total() {
        let bus = tiny_bus(2);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        for i in 1..=10 {
            bus.publish(EventScope::System, received(1, i)).await;
        }

        let lag_a = match a.try_recv().unwrap_err() {
            SubscriptionError::Lagged(n) => n,
            other => panic!("expected Lagged, got {other}"),
        };
        let lag_b = match b.try_recv().unwrap_err() {
            SubscriptionError::Lagged(n) => n,
            other => panic!("expected Lagged, got {other}"),
        };

        assert_eq!(a.lagged(), lag_a);
        assert_eq!(b.lagged(), lag_b);
        assert_eq!(bus.lagged_total(), lag_a + lag_b);
        assert!(lag_a >= 8);
    }

    #[tokio::test]
    async fn drop_on_lag_false_still_never_blocks_the_publisher() {
        // Documented limitation: `drop_on_lag: false` is accepted but the bus
        // keeps broadcast's drop-oldest behaviour, because a blocking publisher
        // queue would let one slow WebSocket client stall Mail Core. What matters
        // is that publishing 20 events into a depth-2 queue still completes.
        let bus = EventBus::new(EventBusConfig {
            history_capacity: 8,
            channel_capacity: 2,
            drop_on_lag: false,
        });
        let mut slow = bus.subscribe();

        for i in 1..=20 {
            bus.publish(EventScope::System, received(1, i)).await;
        }
        assert_eq!(bus.last_seq(), 20);
        assert!(matches!(
            slow.try_recv().unwrap_err(),
            SubscriptionError::Lagged(_)
        ));
    }

    #[tokio::test]
    async fn lagged_total_is_zero_on_a_healthy_bus() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        for i in 1..=5 {
            bus.publish(EventScope::System, received(1, i)).await;
            let _ = sub.try_recv();
        }
        assert_eq!(bus.lagged_total(), 0);
        assert_eq!(sub.lagged(), 0);
    }

    #[tokio::test]
    async fn subscriber_count_tracks_live_receivers() {
        let bus = EventBus::with_defaults();
        assert_eq!(bus.subscriber_count(), 0);

        let first = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        let second = bus.subscribe_filtered(EventFilter::System);
        assert_eq!(bus.subscriber_count(), 2);

        drop(second);
        assert_eq!(bus.subscriber_count(), 1);

        drop(first);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn converting_to_a_stream_keeps_the_subscription_counted() {
        let bus = EventBus::with_defaults();
        let stream = bus.subscribe().into_stream();
        assert_eq!(bus.subscriber_count(), 1);
        drop(stream);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn dropped_subscription_stops_receiving() {
        let bus = EventBus::with_defaults();
        let mut keeper = bus.subscribe();
        let doomed = bus.subscribe();
        drop(doomed);
        assert_eq!(bus.subscriber_count(), 1);

        bus.publish(EventScope::System, received(1, 1)).await;
        assert_eq!(keeper.recv().await.unwrap().seq, 1);
    }

    #[tokio::test]
    async fn dropping_the_bus_closes_subscriptions() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        assert!(!sub.is_closed());

        // A subscription never owns the sender (that would be a
        // subscription → sender → receiver cycle), so dropping the bus really does
        // close the channel.
        drop(bus);
        assert_eq!(sub.recv().await.unwrap_err(), SubscriptionError::Closed);
        assert_eq!(sub.try_recv().unwrap_err(), SubscriptionError::Closed);
        assert!(sub.is_closed());
    }

    #[tokio::test]
    async fn a_subscription_outlives_the_handle_it_came_from() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe_filtered(EventFilter::System);
        let publisher = bus.clone();
        drop(bus);

        // Still open: the clone is alive and publishing works.
        assert!(!sub.is_closed());
        publisher.publish(EventScope::System, received(1, 1)).await;
        assert_eq!(sub.recv().await.unwrap().seq, 1);
    }

    #[tokio::test]
    async fn stream_ends_when_the_bus_is_dropped() {
        let bus = EventBus::with_defaults();
        let stream = bus.subscribe().into_stream();
        bus.publish_nowait(EventScope::System, received(1, 1));
        drop(bus);

        let collected: Vec<EventEnvelope> = stream.collect().await;
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].seq, 1);
    }

    #[tokio::test]
    async fn stream_yields_matching_events_in_order() {
        let bus = EventBus::with_defaults();
        let sub = bus.subscribe_filtered(EventFilter::User(UserId::new(7)));
        for i in 1..=4 {
            bus.publish(user(7), received(1, i)).await;
            bus.publish(user(8), received(2, i)).await;
        }
        drop(bus);

        let stream = sub.into_stream();
        let collected: Vec<EventEnvelope> = stream.collect().await;
        assert_eq!(collected.len(), 4);
        assert!(collected.iter().all(|e| e.scope == user(7)));
        assert!(collected.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[tokio::test]
    async fn stream_skips_lag_reports() {
        let bus = tiny_bus(2);
        let sub = bus.subscribe();
        for i in 1..=10 {
            bus.publish(EventScope::System, received(1, i)).await;
        }
        let stream = sub.into_stream();
        drop(bus);

        // Never yields an error, and never yields more than capacity + what was
        // left in flight.
        let collected: Vec<EventEnvelope> = stream.collect().await;
        assert!(collected.len() <= 10);
        assert!(!collected.is_empty());
    }

    #[tokio::test]
    async fn drain_collects_everything_queued() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        for i in 1..=3 {
            bus.publish(EventScope::System, received(1, i)).await;
        }
        let drained = sub.drain();
        assert_eq!(drained.len(), 3);
        assert!(sub.drain().is_empty());
    }

    #[tokio::test]
    async fn publish_nowait_works_from_a_sync_context() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        let envelope = bus.publish_nowait(user(1), Event::mail_sent(
            MailboxId::new(2),
            ferroma_core::MessageId::new(3),
            2,
            true,
        ));

        assert_eq!(envelope.seq, 1);
        assert_eq!(envelope.event.name(), "mail.sent");
        assert_eq!(sub.try_recv().unwrap().seq, 1);
        assert_eq!(bus.last_seq(), 1);
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_is_fine() {
        let bus = EventBus::with_defaults();
        assert_eq!(bus.subscriber_count(), 0);
        let envelope = bus.publish(EventScope::System, received(1, 1)).await;
        assert_eq!(envelope.seq, 1);
        assert_eq!(bus.replay_since(0).len(), 1, "still replayable");
    }

    #[tokio::test]
    async fn payloads_are_carried_verbatim_and_body_free() {
        let bus = EventBus::with_defaults();
        let mut sub = bus.subscribe();
        let payload = MailReceived {
            mailbox_id: MailboxId::new(1),
            message_id: ferroma_core::MessageId::new(2),
            from: Some("alice@example.com".into()),
            subject: Some("hi".into()),
            size_bytes: 10,
            snippet: Some("hello".into()),
        };
        bus.publish(user(1), Event::MailReceived(payload.clone())).await;

        let envelope = sub.recv().await.unwrap();
        assert_eq!(envelope.event, Event::MailReceived(payload));
        assert!(!envelope.summary().contains("hello"));
    }

    #[tokio::test]
    async fn debug_impls_do_not_panic() {
        let bus = tiny_bus(4);
        let sub = bus.subscribe();
        let _ = format!("{bus:?}");
        let _ = format!("{sub:?}");
        let _ = format!("{:?}", bus.config());
        let _ = format!("{:?}", SubscriptionError::Lagged(1));
        let _ = format!("{:?}", EventFilter::All);
    }

    #[test]
    fn subscription_error_messages_are_useful() {
        assert_eq!(
            SubscriptionError::Lagged(3).to_string(),
            "subscription lagged, 3 event(s) dropped"
        );
        assert_eq!(SubscriptionError::Closed.to_string(), "event bus closed");
        assert!(SubscriptionError::Timeout.to_string().contains("timeout"));
        // It is a real `std::error::Error`.
        let boxed: Box<dyn std::error::Error> = Box::new(SubscriptionError::Closed);
        assert_eq!(boxed.to_string(), "event bus closed");
    }

    #[tokio::test]
    async fn subscription_event_filter_accessor() {
        let bus = EventBus::with_defaults();
        let sub = bus.subscribe_filtered(EventFilter::User(UserId::new(7)));
        assert_eq!(sub.event_filter(), &EventFilter::User(UserId::new(7)));
    }

    #[tokio::test]
    async fn event_stream_accessors() {
        let bus = EventBus::with_defaults();
        let stream = bus.subscribe_filtered(EventFilter::System).into_stream();
        assert_eq!(stream.event_filter(), &EventFilter::System);
        assert_eq!(stream.lagged(), 0);
        assert_eq!(stream.subscription().event_filter(), &EventFilter::System);
    }

    #[tokio::test]
    async fn bus_is_cloneable_and_shares_state() {
        let bus = EventBus::with_defaults();
        let clone = bus.clone();
        let mut sub = clone.subscribe();

        bus.publish(EventScope::System, received(1, 1)).await;
        assert_eq!(sub.recv().await.unwrap().seq, 1);
        assert_eq!(clone.last_seq(), 1);
        assert_eq!(bus.last_seq(), 1);
    }

    #[tokio::test]
    async fn default_impl_matches_with_defaults() {
        let bus = EventBus::default();
        assert_eq!(bus.config().history_capacity, 1024);
        assert_eq!(bus.config().channel_capacity, 512);
    }

    #[tokio::test]
    async fn high_volume_single_task_keeps_the_ring_bounded_and_ordered() {
        let bus = EventBus::new(EventBusConfig {
            history_capacity: 16,
            channel_capacity: 1024,
            drop_on_lag: true,
        });
        let mut sub = bus.subscribe();
        let published = Arc::new(AtomicUsize::new(0));

        for i in 1..=100 {
            bus.publish(EventScope::System, received(1, i)).await;
            published.fetch_add(1, Ordering::Relaxed);
        }

        assert_eq!(bus.last_seq(), 100);
        assert_eq!(bus.history_len(), 16);
        assert_eq!(published.load(Ordering::Relaxed), 100);

        let head = sub.drain();
        assert_eq!(head.len(), 100, "channel capacity 1024 held everything");
        assert!(head.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
    }
}
