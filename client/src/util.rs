//! Small shared helpers: timestamps, hex digests, byte formatting.
//!
//! Nothing here talks to the network or the database, so every function is
//! trivially testable and free of surprises.

use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};

/// The current UTC time as an RFC 3339 string, the format every timestamp column
/// in the cache uses (`docs/api.md` §1.7).
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Render a timestamp the way the wire does (`2026-09-16T12:00:00Z`).
pub fn to_rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Parse an RFC 3339 timestamp. Returns `None` rather than failing, because the
/// value comes from a server that may be buggy — and a bad timestamp must never
/// take the client down.
pub fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// Lowercase hex of an arbitrary byte slice.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// URL-encode a single query-string value (RFC 3986 unreserved characters pass
/// through unchanged, everything else becomes `%XX`).
///
/// `reqwest`'s `query()` would do this for us, but a few endpoints take the
/// search string unparsed, and building it by hand must not let a `&` or a space
/// through.
pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A human-readable byte count, used by the storage pane and the CLI.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Escape a value for use inside an SQLite `LIKE` pattern.
///
/// `%` and `_` are wildcards; a user searching for `100%` must not match the
/// whole cache. The escaped pattern is meant to be used with
/// `ESCAPE '\'` in the statement.
pub fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escape a value for use inside an FTS5 string literal (double quotes are the
/// only special character there).
pub fn escape_fts(value: &str) -> String {
    value.replace('"', "\"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_through_rfc3339() {
        let now = now_rfc3339();
        assert!(now.ends_with('Z'), "{now}");
        let parsed = parse_rfc3339(&now).expect("parses");
        assert_eq!(to_rfc3339(parsed), now);
    }

    #[test]
    fn a_bad_timestamp_is_none_not_a_panic() {
        assert!(parse_rfc3339("not a date").is_none());
        assert!(parse_rfc3339("").is_none());
        assert!(parse_rfc3339("2026-13-45T99:99:99Z").is_none());
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn urlencode_escapes_the_query_delimiters() {
        assert_eq!(urlencode("from:bob subject:invoice"), "from%3Abob%20subject%3Ainvoice");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("safe-._~"), "safe-._~");
    }

    #[test]
    fn human_bytes_is_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn like_escapes_the_wildcards() {
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn fts_escapes_only_double_quotes() {
        assert_eq!(escape_fts("say \"hi\""), "say \"\"hi\"\"");
        assert_eq!(escape_fts("plain"), "plain");
    }
}
