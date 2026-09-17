//! A bounded in-process log ring buffer behind `GET /api/v1/logs`.
//!
//! The Admin panel's "System Logs" screen has to work on a host where nothing ships
//! logs anywhere. [`LogBuffer`] is filled by a `tracing` layer
//! ([`LogRingLayer`]) and keeps the most recent [`DEFAULT_CAPACITY`] events in
//! memory. It is **lost on restart**, and the API says so rather than pretending to
//! be a complete log: the response carries `buffer_entries`, `buffer_capacity` and
//! `oldest_at`.
//!
//! # What never enters the buffer
//!
//! * Message bodies and attachment bytes — nothing in this crate ever logs them.
//! * Opaque credentials. The layer runs every rendered value through [`scrub`],
//!   which replaces anything shaped like a `rt_…`/`st_…` token (the shapes
//!   [`ferroma_auth::token::looks_like_opaque_token`] recognises) with
//!   `[redacted]`, so a bearer token that reached a log line by accident does not
//!   become readable by every admin.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tracing::Level;

/// How many entries the ring holds by default.
pub const DEFAULT_CAPACITY: usize = 1000;

/// What a scrubbed credential is replaced by.
pub const REDACTED: &str = "[redacted]";

/// One captured log line.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// When the event happened, UTC.
    pub at: DateTime<Utc>,
    /// `error`, `warn`, `info`, `debug` or `trace`, lower-case.
    pub level: String,
    /// The `tracing` target, e.g. `ferroma_smtp::client`.
    pub target: String,
    /// The rendered message.
    pub message: String,
    /// The structured fields, as strings — the buffer is for humans, not for parsing.
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl LogEntry {
    /// Whether this entry satisfies the `?level=` filter.
    ///
    /// `wanted` is a minimum: asking for `info` also returns `warn` and `error`,
    /// which is what an operator staring at a failure actually wants.
    pub fn at_least(&self, wanted: Level) -> bool {
        level_rank(&self.level) >= level_rank(wanted.as_str())
    }
}

/// Numeric severity of a `tracing` level name, higher being more severe.
pub fn level_rank(name: &str) -> u8 {
    match name.to_ascii_lowercase().as_str() {
        "error" => 4,
        "warn" => 3,
        "info" => 2,
        "debug" => 1,
        "trace" => 0,
        _ => 0,
    }
}

/// Parse a `?level=` value.
pub fn parse_level(raw: &str) -> Option<Level> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Some(Level::ERROR),
        "warn" | "warning" => Some(Level::WARN),
        "info" => Some(Level::INFO),
        "debug" => Some(Level::DEBUG),
        "trace" => Some(Level::TRACE),
        _ => None,
    }
}

/// Replace anything shaped like an opaque Ferroma credential with [`REDACTED`].
///
/// The rule is deliberately about *shape*, not about position: a token is recognised
/// by the `rt_`/`st_` prefix this platform mints and by nothing else, so ordinary
/// prose is untouched. A run of characters is only rewritten when the token is the
/// whole value or follows one of `: = ,` — the separators a `tracing` field renderer
/// uses — which keeps a word like `st_abbey` in a sentence readable while still
/// catching `token=st_abc…`.
pub fn scrub(value: &str) -> String {
    if !value.contains("rt_") && !value.contains("st_") {
        return value.to_string();
    }

    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let rest = &value[index..];
        // A token is recognised at the start of the string or right after one of the
        // separators a field renderer uses. The separator is emitted *before* the check
        // so `"cookie: st_…"` is caught while `"a stanza"` is not.
        let boundary = out.is_empty()
            || matches!(
                out.chars().last(),
                Some(' ') | Some(':') | Some('=') | Some(',') | Some('"') | Some('\'') | Some('(') | Some('[')
            );
        match token_at(rest) {
            Some(len) if boundary => {
                out.push_str(REDACTED);
                index += len;
            }
            _ => {
                let ch = rest.chars().next().unwrap_or(' ');
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    out
}

/// The length of an opaque-token-shaped prefix of `value`, if any.
fn token_at(value: &str) -> Option<usize> {
    let prefix = ["rt_", "st_"]
        .into_iter()
        .find(|prefix| value.starts_with(prefix))?;
    let body: String = value[prefix.len()..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        .collect();
    if body.len() < 8 {
        // Too short to be a token we minted; leave the text alone.
        return None;
    }
    Some(prefix.len() + body.len())
}

/// The ring buffer itself.
#[derive(Debug)]
pub struct LogBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
    floor: Level,
}

impl Default for LogBuffer {
    fn default() -> Self {
        LogBuffer::new(DEFAULT_CAPACITY, Level::WARN)
    }
}

impl LogBuffer {
    /// A buffer holding at most `capacity` entries recorded at `floor` or above.
    pub fn new(capacity: usize, floor: Level) -> Self {
        let capacity = capacity.max(1);
        LogBuffer {
            entries: Mutex::new(VecDeque::with_capacity(capacity.min(256))),
            capacity,
            floor,
        }
    }

    /// Append an entry, evicting the oldest when the ring is full.
    pub fn push(&self, entry: LogEntry) {
        if let Ok(mut guard) = self.entries.lock() {
            while guard.len() >= self.capacity {
                guard.pop_front();
            }
            guard.push_back(entry);
        }
    }

    /// How many entries the ring currently holds.
    pub fn len(&self) -> usize {
        self.entries.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The level the layer captures from.
    pub fn floor(&self) -> Level {
        self.floor
    }

    /// The oldest retained entry's timestamp, when there is one.
    pub fn oldest_at(&self) -> Option<DateTime<Utc>> {
        self.entries
            .lock()
            .ok()
            .and_then(|guard| guard.front().map(|entry| entry.at))
    }

    /// The newest retained entry's timestamp, when there is one.
    pub fn newest_at(&self) -> Option<DateTime<Utc>> {
        self.entries
            .lock()
            .ok()
            .and_then(|guard| guard.back().map(|entry| entry.at))
    }

    /// A snapshot of the entries, oldest first.
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.entries
            .lock()
            .map(|guard| guard.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Apply the documented filters, newest first, and page the result.
    ///
    /// `level` is a *minimum*; `target` is a substring match; `query` is a
    /// case-insensitive substring of the message or any field value; `since` is an
    /// inclusive lower bound on the timestamp.
    pub fn query(&self, filter: &LogFilter) -> (Vec<LogEntry>, i64) {
        let mut matched: Vec<LogEntry> = self
            .snapshot()
            .into_iter()
            .filter(|entry| filter.matches(entry))
            .collect();

        // Newest first, which is how a log panel reads.
        matched.reverse();
        let total = matched.len() as i64;

        let start = filter.offset.max(0) as usize;
        let limit = filter.limit.max(0) as usize;
        let items = matched.into_iter().skip(start).take(limit).collect();
        (items, total)
    }
}

/// The filters `GET /api/v1/logs` accepts.
#[derive(Debug, Clone)]
pub struct LogFilter {
    /// Minimum severity.
    pub level: Level,
    /// Substring of the target.
    pub target: Option<String>,
    /// Case-insensitive substring of the message or a field value.
    pub query: Option<String>,
    /// Only entries at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// Maximum rows.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

impl Default for LogFilter {
    fn default() -> Self {
        LogFilter {
            level: Level::WARN,
            target: None,
            query: None,
            since: None,
            limit: 100,
            offset: 0,
        }
    }
}

impl LogFilter {
    /// Whether an entry survives every configured filter.
    pub fn matches(&self, entry: &LogEntry) -> bool {
        if !entry.at_least(self.level) {
            return false;
        }
        if let Some(target) = self.target.as_deref().filter(|t| !t.is_empty()) {
            if !entry.target.to_ascii_lowercase().contains(&target.to_ascii_lowercase()) {
                return false;
            }
        }
        if let Some(query) = self.query.as_deref().filter(|q| !q.is_empty()) {
            let needle = query.to_ascii_lowercase();
            let in_message = entry.message.to_ascii_lowercase().contains(&needle);
            let in_fields = entry.fields.values().any(|value| {
                value
                    .as_str()
                    .map(|text| text.to_ascii_lowercase().contains(&needle))
                    .unwrap_or(false)
            });
            if !in_message && !in_fields {
                return false;
            }
        }
        if let Some(since) = self.since {
            if entry.at < since {
                return false;
            }
        }
        true
    }
}

/// A shared handle that records into a [`LogBuffer`].
///
/// # Why this is not a `tracing` layer
///
/// The layer that fills this buffer belongs on the *server*'s `tracing` subscriber,
/// next to the stdout layer `ferroma-core`'s logging module installs. Installing it
/// from here would need `tracing-subscriber` as a direct dependency of this crate,
/// which the task's dependency list does not include — so this crate exposes the
/// capture API ([`LogSink::record`], and the plain `Level` filter) and the server
/// wires it wherever it composes its subscriber. Until that wiring exists,
/// `GET /api/v1/logs` answers with an empty buffer plus its `buffer_entries` /
/// `buffer_capacity` / `oldest_at` metadata, which is exactly what the contract says
/// a buffer with nothing in it looks like.
#[derive(Debug, Clone)]
pub struct LogSink {
    buffer: Arc<LogBuffer>,
}

impl LogSink {
    /// Wrap `buffer`.
    pub fn new(buffer: Arc<LogBuffer>) -> Self {
        LogSink { buffer }
    }

    /// The buffer this sink writes into.
    pub fn buffer(&self) -> &Arc<LogBuffer> {
        &self.buffer
    }

    /// Record one event, scrubbing the text on the way in.
    ///
    /// Events below the buffer's floor are dropped, so a caller can hand over
    /// everything and let the sink decide.
    pub fn record(
        &self,
        level: Level,
        target: &str,
        message: &str,
        fields: serde_json::Map<String, serde_json::Value>,
    ) {
        if level > self.buffer.floor() {
            return;
        }
        let mut scrubbed = serde_json::Map::with_capacity(fields.len());
        for (key, value) in fields {
            // Every field is rendered as text: the buffer is read by a human in a
            // panel, and a JSON number would not be searchable by the `?query=` filter.
            let rendered = match value {
                serde_json::Value::String(text) => scrub(&text),
                serde_json::Value::Null => String::new(),
                other => scrub(&other.to_string()),
            };
            scrubbed.insert(key, serde_json::Value::String(rendered));
        }
        self.buffer.push(LogEntry {
            at: Utc::now(),
            level: level.as_str().to_ascii_lowercase(),
            target: scrub(target),
            message: scrub(message),
            fields: scrubbed,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid timestamp")
    }

    fn entry(secs: i64, level: &str, target: &str, message: &str) -> LogEntry {
        LogEntry {
            at: at(secs),
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn the_ring_evicts_the_oldest_entry() {
        let buffer = LogBuffer::new(3, Level::WARN);
        for index in 0..5 {
            buffer.push(entry(index, "warn", "t", &format!("line {index}")));
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.capacity(), 3);
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot[0].message, "line 2");
        assert_eq!(snapshot[2].message, "line 4");
        assert_eq!(buffer.oldest_at(), Some(at(2)));
        assert_eq!(buffer.newest_at(), Some(at(4)));
    }

    #[test]
    fn a_zero_capacity_buffer_still_holds_one_entry() {
        let buffer = LogBuffer::new(0, Level::WARN);
        assert_eq!(buffer.capacity(), 1);
        buffer.push(entry(1, "warn", "t", "only"));
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn an_empty_buffer_reports_nothing() {
        let buffer = LogBuffer::new(4, Level::WARN);
        assert!(buffer.is_empty());
        assert_eq!(buffer.oldest_at(), None);
        assert_eq!(buffer.newest_at(), None);
        assert!(buffer.snapshot().is_empty());
        assert_eq!(buffer.floor(), Level::WARN);
    }

    #[test]
    fn level_is_a_minimum_not_an_equality() {
        assert!(entry(1, "error", "t", "x").at_least(Level::INFO));
        assert!(entry(1, "warn", "t", "x").at_least(Level::WARN));
        assert!(!entry(1, "info", "t", "x").at_least(Level::WARN));
        assert!(entry(1, "info", "t", "x").at_least(Level::INFO));
        assert!(!entry(1, "debug", "t", "x").at_least(Level::INFO));
    }

    #[test]
    fn level_ranks_and_parsing_are_consistent() {
        assert!(level_rank("error") > level_rank("warn"));
        assert!(level_rank("warn") > level_rank("info"));
        assert!(level_rank("info") > level_rank("debug"));
        assert!(level_rank("debug") > level_rank("trace"));
        assert_eq!(level_rank("nonsense"), 0);
        assert_eq!(parse_level("WARN"), Some(Level::WARN));
        assert_eq!(parse_level(" warning "), Some(Level::WARN));
        assert_eq!(parse_level("info"), Some(Level::INFO));
        assert_eq!(parse_level("verbose"), None);
    }

    #[test]
    fn query_filters_by_target_query_and_since() {
        let buffer = LogBuffer::new(16, Level::INFO);
        buffer.push(entry(10, "warn", "ferroma_smtp::client", "deferred 421"));
        buffer.push(entry(20, "error", "ferroma_api::routes", "handler blew up"));
        buffer.push(entry(30, "info", "ferroma_smtp::server", "accepted connection"));

        let mut filter = LogFilter {
            level: Level::INFO,
            ..LogFilter::default()
        };
        let (items, total) = buffer.query(&filter);
        assert_eq!(total, 3);
        // Newest first.
        assert_eq!(items[0].at, at(30));

        filter.target = Some("smtp::client".into());
        let (items, total) = buffer.query(&filter);
        assert_eq!(total, 1);
        assert_eq!(items[0].message, "deferred 421");

        filter.target = None;
        filter.query = Some("421".into());
        let (_, total) = buffer.query(&filter);
        assert_eq!(total, 1);

        filter.query = None;
        filter.since = Some(at(15));
        let (_, total) = buffer.query(&filter);
        assert_eq!(total, 2);
    }

    #[test]
    fn query_pages_after_filtering() {
        let buffer = LogBuffer::new(32, Level::INFO);
        for index in 0..10 {
            buffer.push(entry(index, "warn", "t", &format!("line {index}")));
        }
        let filter = LogFilter {
            level: Level::INFO,
            limit: 3,
            offset: 2,
            ..LogFilter::default()
        };
        let (items, total) = buffer.query(&filter);
        assert_eq!(total, 10);
        assert_eq!(items.len(), 3);
        // Newest first, skipping two.
        assert_eq!(items[0].message, "line 7");

        let beyond = LogFilter {
            limit: 5,
            offset: 100,
            ..filter
        };
        let (items, total) = buffer.query(&beyond);
        assert_eq!(total, 10);
        assert!(items.is_empty());
    }

    #[test]
    fn query_matches_field_values_too() {
        let mut with_field = entry(1, "warn", "t", "delivery deferred");
        with_field
            .fields
            .insert("queue_id".into(), serde_json::Value::from("91"));
        let buffer = LogBuffer::new(4, Level::INFO);
        buffer.push(with_field);

        let filter = LogFilter {
            level: Level::INFO,
            query: Some("91".into()),
            ..LogFilter::default()
        };
        assert_eq!(buffer.query(&filter).1, 1);
    }

    #[test]
    fn scrub_redacts_tokens_and_leaves_prose_alone() {
        assert_eq!(
            scrub("token=rt_ABCDEFGHIJKLMNOP"),
            format!("token={REDACTED}")
        );
        assert_eq!(
            scrub("cookie: st_0123456789abcdef"),
            format!("cookie: {REDACTED}")
        );
        assert_eq!(scrub("st_zzzzzzzzzzzz"), REDACTED);
        assert_eq!(scrub("the status is fine"), "the status is fine");
        // A short lookalike is not a token.
        assert_eq!(scrub("st_ab"), "st_ab");
        // A word containing the prefix inside prose is not rewritten.
        assert_eq!(scrub("a stanza t_st_abcdefghij"), "a stanza t_st_abcdefghij");
    }

    #[test]
    fn scrub_handles_several_tokens_and_clean_values() {
        let scrubbed = scrub("rt_aaaaaaaaaaaa rt_bbbbbbbbbbbb");
        assert_eq!(scrubbed, format!("{REDACTED} {REDACTED}"));
        assert_eq!(scrub(""), "");
        assert!(scrub("nothing secret").contains("nothing"));
    }

    #[test]
    fn entries_serialise_with_the_documented_field_names() {
        let mut item = entry(0, "warn", "ferroma_smtp::client", "deferred");
        item.fields
            .insert("queue_id".into(), serde_json::Value::from("91"));
        let json = serde_json::to_value(&item).expect("entry must serialise");
        assert!(json["at"].is_string());
        assert_eq!(json["level"], "warn");
        assert_eq!(json["target"], "ferroma_smtp::client");
        assert_eq!(json["message"], "deferred");
        assert_eq!(json["fields"]["queue_id"], "91");
    }

    #[test]
    fn recording_a_field_renders_it_as_searchable_text() {
        let buffer = std::sync::Arc::new(LogBuffer::new(8, Level::INFO));
        let sink = LogSink::new(std::sync::Arc::clone(&buffer));
        let mut fields = serde_json::Map::new();
        fields.insert("queue_id".into(), serde_json::Value::from(91));
        fields.insert("remote_mx".into(), serde_json::Value::from("mx1.example.net"));
        sink.record(Level::WARN, "ferroma_smtp::client", "deferred", fields);

        let entries = buffer.snapshot();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].fields["queue_id"], "91");
        assert_eq!(entries[0].level, "warn");

        // ...and the numeric field is findable through the `?query=` filter.
        let filter = LogFilter {
            level: Level::INFO,
            query: Some("91".into()),
            ..LogFilter::default()
        };
        assert_eq!(buffer.query(&filter).1, 1);
    }

    #[test]
    fn a_sink_drops_events_below_the_buffer_floor() {
        let buffer = std::sync::Arc::new(LogBuffer::new(8, Level::WARN));
        let sink = LogSink::new(std::sync::Arc::clone(&buffer));
        sink.record(Level::INFO, "t", "too quiet", serde_json::Map::new());
        assert!(buffer.is_empty());
        sink.record(Level::ERROR, "t", "loud enough", serde_json::Map::new());
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn a_sink_scrubs_tokens_on_the_way_in() {
        let buffer = std::sync::Arc::new(LogBuffer::new(8, Level::WARN));
        let sink = LogSink::new(std::sync::Arc::clone(&buffer));
        let mut fields = serde_json::Map::new();
        fields.insert(
            "token".into(),
            serde_json::Value::from("rt_abcdefghijklmnop"),
        );
        sink.record(Level::WARN, "t", "login used rt_abcdefghijklmnop", fields);
        let entries = buffer.snapshot();
        assert!(!entries[0].message.contains("rt_abcdefghijklmnop"), "{entries:?}");
        assert_eq!(entries[0].fields["token"], REDACTED);
    }

    #[test]
    fn the_filter_defaults_to_warn_and_one_hundred_rows() {
        let filter = LogFilter::default();
        assert_eq!(filter.level, Level::WARN);
        assert_eq!(filter.limit, 100);
        assert_eq!(filter.offset, 0);
        assert!(filter.target.is_none());
    }

    #[test]
    fn default_capacity_matches_the_documented_number() {
        assert_eq!(DEFAULT_CAPACITY, 1000);
        let buffer = LogBuffer::default();
        assert_eq!(buffer.capacity(), DEFAULT_CAPACITY);
        assert_eq!(buffer.floor(), Level::WARN);
    }
}
