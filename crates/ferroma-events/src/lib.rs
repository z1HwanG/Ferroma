//! Ferroma event bus — the platform's single source of realtime truth.
//!
//! `Mail Core` publishes here; Webmail, the official clients (over WebSocket),
//! the Admin panel, notifications and webhooks consume.
//!
//! Events, per the specification §22:
//! `MailReceived`, `MailSent`, `MailDeleted`, `MailRead`, `MailFlagChanged`,
//! `MailMoved`, `DraftCreated`, `DraftUpdated`, `DeliveryUpdated`,
//! `DeviceRevoked`.
//!
//! # Layout
//!
//! * [`event`] — the vocabulary: [`EventScope`], the [`Event`] payloads and the
//!   [`EventEnvelope`] that carries sequencing metadata and produces the
//!   WebSocket frame ([`EventEnvelope::to_wire`]).
//! * [`bus`] — the [`EventBus`] itself: lock-brief publishing with strictly
//!   monotonic sequence numbers, per-subscriber filtering
//!   ([`EventFilter`]), live delivery through [`Subscription`], and a bounded
//!   replay ring for reconnect catch-up.
//!
//! # Example
//!
//! ```
//! # async fn demo() {
//! use ferroma_core::{MailboxId, MessageId, UserId};
//! use ferroma_events::{Event, EventBus, EventScope};
//!
//! let bus = EventBus::with_defaults();
//! let mut client = bus.subscribe_filtered(EventScope::User(UserId::new(7)).into());
//!
//! bus.publish(
//!     EventScope::User(UserId::new(7)),
//!     Event::mail_received(MailboxId::new(1), MessageId::new(2)),
//! )
//! .await;
//!
//! let envelope = client.recv().await.unwrap();
//! assert_eq!(envelope.event.name(), "mail.received");
//! assert_eq!(envelope.to_wire().unwrap()["type"], "mail.received");
//! # }
//! ```
//!
//! # Guarantees
//!
//! * **Ordering** — `seq` is gap-free and strictly monotonic, even under heavy
//!   concurrency, so `seq` doubles as the reconnect cursor
//!   ([`EventBus::replay_since`]).
//! * **Isolation** — filtering happens per subscriber, so a connection filtered to
//!   one user never receives another user's frames.
//! * **No back-pressure on producers** — a subscriber that stops reading lags and
//!   is told how many events it missed ([`SubscriptionError::Lagged`]) rather than
//!   stalling `Mail Core`.
//! * **No bodies** — no payload (and no [`EventEnvelope::summary`] log line)
//!   contains a message body, attachment or credential.

#![warn(missing_docs)]

pub mod bus;
pub mod event;

pub use bus::{EventBus, EventBusConfig, Subscription};
pub use event::{Event, EventScope};

pub use bus::{EventFilter, EventStream, SubscriptionError};
pub use event::{EventEnvelope, MailReceived};
