//! `tracing` initialisation, shared by the server, the client and the test suite.
//!
//! Ferroma emits structured logs with `tracing`. The two supported renderings are
//! plain text (for humans reading `docker compose logs`) and newline-delimited
//! JSON (for Loki/ELK). Both carry the same fields, including the SMTP session
//! context described in the specification: `connection_id`, `remote_ip`, `helo`,
//! `authenticated_user`, `sender`, `recipient`, `message_id`, `result`, `duration`.
//!
//! Passwords, AUTH tokens, private keys and full message bodies are never logged.

use std::io::IsTerminal;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;
use crate::Result;

/// Third-party crates that are chatty at `debug`/`trace`; kept at `warn` unless
/// the operator explicitly asks for them.
const NOISY_DEFAULTS: &str = "hyper=warn,h2=warn,sqlx=warn,hickory_resolver=warn,hickory_proto=warn,rustls=warn,tokio_tungstenite=warn";

/// Build the filter, appending the noisy-crate defaults only when the operator's
/// directive does not already mention them.
pub fn build_filter(directive: &str) -> EnvFilter {
    let directive = directive.trim();
    let directive = if directive.is_empty() { "info" } else { directive };

    let mut combined = directive.to_string();
    for entry in NOISY_DEFAULTS.split(',') {
        let target = entry.split('=').next().unwrap_or_default();
        if !directive.contains(target) {
            combined.push(',');
            combined.push_str(entry);
        }
    }

    EnvFilter::try_new(&combined).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Install the global subscriber.
///
/// Calling this more than once in a process is harmless: subsequent calls are
/// ignored, which keeps `cargo test` (many test threads, one process) quiet.
pub fn init(level: &str, format: LogFormat) -> Result<()> {
    match format {
        LogFormat::Json => install(
            tracing_subscriber::registry()
                .with(build_filter(level))
                .with(json_layer()),
        ),
        LogFormat::Text => install(
            tracing_subscriber::registry()
                .with(build_filter(level))
                .with(text_layer()),
        ),
    }
}

/// Install an already-composed subscriber as the global one.
///
/// The same idempotence as [`init`]: a process that already has a subscriber keeps it.
pub fn install<S>(subscriber: S) -> Result<()>
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    let _ = subscriber.try_init();
    Ok(())
}

/// The newline-delimited JSON stdout layer.
///
/// Generic over the subscriber it will be layered onto, and returned as an opaque
/// **concrete** type rather than a trait object: `SubscriberExt::with` needs each layer
/// to implement `Layer` for the subscriber accumulated so far, and a layer typed
/// against [`Registry`](tracing_subscriber::registry::Registry) alone — boxed or not —
/// cannot be appended to a chain that already carries the filter. The server composes
/// `filter → this → the in-process log ring` (see `server/src/logring.rs`), which is
/// what `GET /api/v1/logs` reads.
pub fn json_layer<S>() -> impl tracing_subscriber::Layer<S> + Send + Sync
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(true)
        .with_target(true)
        .with_level(true)
        .with_writer(std::io::stdout)
}

/// The human-readable text stdout layer, with colour when stdout is a terminal.
///
/// Concrete and generic for the same reasons as [`json_layer`].
pub fn text_layer<S>() -> impl tracing_subscriber::Layer<S> + Send + Sync
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let ansi = std::io::stdout().is_terminal();
    tracing_subscriber::fmt::layer()
        .with_ansi(ansi)
        .with_target(true)
        .with_level(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_writer(std::io::stdout)
}

/// Install a subscriber suitable for tests: quiet by default, overridable with
/// `RUST_LOG` or `FERROMA_LOG_LEVEL`.
pub fn init_for_tests() {
    let level = std::env::var("RUST_LOG")
        .or_else(|_| std::env::var("FERROMA_LOG_LEVEL"))
        .unwrap_or_else(|_| "warn".to_string());
    let _ = init(&level, LogFormat::Text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_defaults_to_info_on_garbage() {
        let f = build_filter("  ");
        assert!(format!("{f}").contains("info"));

        // An unparsable directive falls back rather than panicking.
        let f = build_filter("!!!not a directive!!!");
        assert!(format!("{f}").contains("info"));
    }

    #[test]
    fn filter_keeps_the_operator_directive_and_quiets_noisy_crates() {
        let f = build_filter("ferroma_smtp=debug");
        let rendered = format!("{f}");
        assert!(rendered.contains("ferroma_smtp=debug"), "{rendered}");
        assert!(rendered.contains("sqlx=warn"), "{rendered}");
    }

    #[test]
    fn operator_can_opt_back_into_noisy_crate_logs() {
        let f = build_filter("sqlx=debug");
        let rendered = format!("{f}");
        assert!(rendered.contains("sqlx=debug"), "{rendered}");
        // ...and we did not append a second, conflicting directive for it.
        assert_eq!(rendered.matches("sqlx=").count(), 1, "{rendered}");
    }

    #[test]
    fn init_is_idempotent() {
        init("info", LogFormat::Text).unwrap();
        init("info", LogFormat::Json).unwrap();
        tracing::info!("logging smoke test");
    }
}
