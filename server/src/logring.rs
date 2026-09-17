//! The `tracing` layer that fills the in-process ring behind `GET /api/v1/logs`.
//!
//! `ferroma-api` owns the ring, the scrubber and the `LogSink` capture API, but it
//! deliberately does not depend on `tracing-subscriber`: the layer belongs on the
//! server's subscriber, next to the stdout layer `ferroma_core::logging` installs.
//! This module is that layer, and it is the reason the Admin "System Logs" screen
//! shows anything at all — before it existed, the buffer was constructed, exposed and
//! never written to, so the endpoint answered `buffer_entries: 0` forever.
//!
//! # What it captures
//!
//! Every event at or above the buffer's floor. Credentials are not filtered here:
//! `LogSink::record` runs each value through `ferroma_api::logbuf::scrub`, so the one
//! implementation of "this looks like a token" is the one the API tests cover.
//!
//! The `message` field is kept separate from the structured fields because a panel
//! renders it as prose; every other field is stored as text, since the ring is read by
//! a human and searched with `?query=`.

use ferroma_api::LogSink;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// Capture events into a ring buffer.
#[derive(Debug, Clone)]
pub struct LogRingLayer {
    sink: LogSink,
}

impl LogRingLayer {
    /// Capture into `sink`.
    pub fn new(sink: LogSink) -> Self {
        LogRingLayer { sink }
    }
}

impl<S: Subscriber> Layer<S> for LogRingLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        // The floor is the buffer's: a ring configured to hold `warn` and above must
        // not pay for rendering `info` events. `LogSink::record` re-checks, so this is
        // an optimisation as much as a policy.
        if *metadata.level() > self.sink.buffer().floor() {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.sink.record(
            *metadata.level(),
            metadata.target(),
            &visitor.message,
            visitor.fields,
        );
    }
}

/// Collects an event's fields, keeping `message` aside as the rendered line.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl FieldVisitor {
    /// Store one field. `message` is the event's prose; everything else is context.
    fn insert(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = value;
            return;
        }
        self.fields
            .insert(name.to_string(), serde_json::Value::String(value));
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        // Handled separately from `record_debug` so a string is not stored with the
        // quotes and escapes `{:?}` would add: `remote_mx="mx1"` is not searchable.
        self.insert(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field.name(), format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field.name(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field.name(), value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field.name(), value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferroma_api::{LogBuffer, LogSink};
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    /// Run `emit` with the ring layer installed, and hand back what it captured.
    fn capture(floor: Level, emit: impl FnOnce()) -> Vec<ferroma_api::LogEntry> {
        let buffer = Arc::new(LogBuffer::new(32, floor));
        let sink = LogSink::new(Arc::clone(&buffer));
        let subscriber = tracing_subscriber::registry().with(LogRingLayer::new(sink));
        tracing::subscriber::with_default(subscriber, emit);
        buffer.snapshot()
    }

    #[test]
    fn an_event_reaches_the_ring_with_its_message_and_fields() {
        let entries = capture(Level::INFO, || {
            tracing::warn!(queue_id = 91, remote_mx = "mx1.example.net", "delivery deferred");
        });

        assert_eq!(entries.len(), 1, "{entries:?}");
        let entry = &entries[0];
        assert_eq!(entry.level, "warn");
        assert_eq!(entry.message, "delivery deferred");
        assert_eq!(entry.fields["queue_id"], "91");
        // A `&str` field is stored unquoted, so `?query=mx1` can match it.
        assert_eq!(entry.fields["remote_mx"], "mx1.example.net");
        // The target is the emitting module path; the ring keeps it so an operator can
        // filter the panel by subsystem.
        assert!(entry.target.contains("logring"), "target: {}", entry.target);
    }

    #[test]
    fn events_below_the_floor_are_not_captured() {
        let entries = capture(Level::WARN, || {
            tracing::info!("too quiet for this ring");
            tracing::warn!("loud enough");
        });

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].message, "loud enough");
    }

    #[test]
    fn an_info_floor_captures_info() {
        // The floor the server derives from `server.log_level = "info"`: without this,
        // the System Logs screen is blank on a healthy deployment.
        let entries = capture(Level::INFO, || {
            tracing::info!(listener = "mx", "SMTP listener started");
        });

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].level, "info");
        assert_eq!(entries[0].fields["listener"], "mx");
    }

    #[test]
    fn a_token_in_a_field_is_scrubbed_before_it_is_stored() {
        // The layer must not be a way around the scrubber.
        let entries = capture(Level::INFO, || {
            tracing::warn!(token = "st_abcdefghijklmnop", "auth probe");
        });

        assert_eq!(entries.len(), 1, "{entries:?}");
        let stored = entries[0].fields["token"].as_str().unwrap_or_default();
        assert!(!stored.contains("st_abcdefghijklmnop"), "stored: {stored}");
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest_entries() {
        let buffer = Arc::new(LogBuffer::new(3, Level::INFO));
        let sink = LogSink::new(Arc::clone(&buffer));
        let subscriber = tracing_subscriber::registry().with(LogRingLayer::new(sink));
        tracing::subscriber::with_default(subscriber, || {
            for index in 0..6 {
                tracing::warn!(index, "tick");
            }
        });

        let entries = buffer.snapshot();
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert_eq!(entries[2].fields["index"], "5");
    }
}
