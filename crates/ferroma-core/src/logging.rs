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
    let filter = build_filter(level);

    match format {
        LogFormat::Json => {
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(true)
                .with_target(true)
                .with_level(true)
                .with_writer(std::io::stdout);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(layer)
                .try_init();
        }
        LogFormat::Text => {
            let ansi = std::io::stdout().is_terminal();
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(ansi)
                .with_target(true)
                .with_level(true)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_writer(std::io::stdout);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(layer)
                .try_init();
        }
    }

    Ok(())
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
