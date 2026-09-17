//! Header address lists: display names, quoted strings, comments and groups.
//!
//! RFC 5322 §3.4 defines a `address-list` as a comma-separated sequence of
//! `address` items, where an address is either a `mailbox`
//! (`Display Name <local@domain>`) or a `group` (`Group Name: a@b, c@d;`).
//!
//! Real mail is messier than the grammar: senders quote display names
//! inconsistently, drop the angle brackets, wrap addresses in comments, and
//! sprinkle RFC 2047 encoded words into the display name. [`parse_address_list`]
//! is *forgiving by design*: an entry it cannot make sense of is skipped so the
//! remaining recipients still work.

use std::fmt;

use ferroma_core::EmailAddress;

use crate::headers::decode_encoded_words;

/// A display name paired with a validated address.
///
/// ```text
/// Alice <alice@example.com>
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mailbox {
    /// The display name, if the sender supplied one (already unquoted and
    /// RFC 2047 decoded).
    pub name: Option<String>,
    /// The parsed, normalised address.
    pub address: EmailAddress,
}

impl Mailbox {
    /// A mailbox with no display name.
    pub fn new(address: EmailAddress) -> Self {
        Mailbox {
            name: None,
            address,
        }
    }

    /// A mailbox with a display name.
    pub fn with_name(address: EmailAddress, name: impl Into<String>) -> Self {
        let name = name.into();
        Mailbox {
            name: if name.trim().is_empty() {
                None
            } else {
                Some(name.trim().to_string())
            },
            address,
        }
    }

    /// The header form: `Alice <alice@example.com>`, or the bare address when
    /// there is no display name. Names containing a special are quoted.
    pub fn display(&self) -> String {
        match &self.name {
            None => self.address.to_string(),
            Some(name) => format!("{} <{}>", quote_display_name(name), self.address),
        }
    }
}

impl fmt::Display for Mailbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Parse an RFC 5322 address list into mailboxes.
///
/// Handles `A <a@b>, c@d`, quoted display names that contain commas,
/// RFC 2047 encoded display names, and group syntax (`Group: a@b, c@d;` — the
/// group is flattened and an empty group contributes nothing). Unparsable
/// entries are skipped rather than failing the whole list.
pub fn parse_address_list(raw: &str) -> Vec<Mailbox> {
    let mut out = Vec::new();
    for entry in split_top_level(raw) {
        let entry = strip_comments(entry);
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Group syntax: `name: members;`
        if let Some(colon) = top_level_colon(entry) {
            let inner = entry[colon + 1..].trim();
            let inner = inner.trim_end_matches(';').trim();
            let inner = inner.trim_end_matches(';');
            if !inner.is_empty() {
                out.extend(parse_address_list(inner));
            }
            continue;
        }
        if let Some(mailbox) = parse_single(entry) {
            out.push(mailbox);
        }
    }
    out
}

/// Format a list of mailboxes as a header value: comma + space separated,
/// display names quoted only when they need it.
pub fn format_address_list(list: &[Mailbox]) -> String {
    list.iter()
        .map(Mailbox::display)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Split on commas that are not inside a quoted string, an angle-addr or a comment.
fn split_top_level(raw: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut depth_angle = 0usize;
    let mut depth_comment = 0usize;
    let mut in_quote = false;
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_quote => i += 1,
            b'"' => in_quote = !in_quote,
            b'<' if !in_quote => depth_angle += 1,
            b'>' if !in_quote => depth_angle = depth_angle.saturating_sub(1),
            b'(' if !in_quote => depth_comment += 1,
            b')' if !in_quote => depth_comment = depth_comment.saturating_sub(1),
            b',' | b';' if !in_quote && depth_angle == 0 && depth_comment == 0 => {
                out.push(&raw[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&raw[start..]);
    out
}

/// Byte index of the first colon that is a genuine group separator, if any.
///
/// A colon inside a quoted string, an angle-addr or a comment never counts, and
/// neither does one that appears after an `@` (the local part may contain one in
/// theory, so only a colon *before* any `@` is treated as a group marker).
fn top_level_colon(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut in_quote = false;
    let mut depth_angle = 0usize;
    let mut depth_comment = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_quote => i += 1,
            b'"' => in_quote = !in_quote,
            b'<' if !in_quote => depth_angle += 1,
            b'>' if !in_quote => depth_angle = depth_angle.saturating_sub(1),
            b'(' if !in_quote => depth_comment += 1,
            b')' if !in_quote => depth_comment = depth_comment.saturating_sub(1),
            b'@' if !in_quote && depth_angle == 0 && depth_comment == 0 => return None,
            b':' if !in_quote && depth_angle == 0 && depth_comment == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Parse one `mailbox` production.
fn parse_single(raw: &str) -> Option<Mailbox> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // `<addr>` or `Name <addr>`
    if let Some(open) = raw.find('<') {
        let close = raw.rfind('>').filter(|c| *c > open).unwrap_or(raw.len());
        let addr_part = raw[open + 1..close.min(raw.len())].trim();
        let name_part = raw[..open].trim();
        let address = EmailAddress::parse(addr_part).ok()?;
        return Some(Mailbox {
            name: display_name(name_part),
            address,
        });
    }

    // Bare address, possibly followed by a comment (already stripped upstream).
    let candidate = raw.trim().trim_end_matches(',').trim();
    let address = EmailAddress::parse(candidate).ok()?;
    Some(Mailbox {
        name: None,
        address,
    })
}

/// Turn the raw text before `<` into a display name.
fn display_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let unquoted = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        unescape_quoted(&raw[1..raw.len() - 1])
    } else {
        raw.to_string()
    };
    let decoded = decode_encoded_words(&unquoted);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Remove backslash escapes inside a quoted string.
fn unescape_quoted(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

/// Strip `(...)` comments, honouring quoted strings and nesting.
fn strip_comments(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if in_quote {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quote = false;
            }
            continue;
        }
        match ch {
            '"' if depth == 0 => {
                in_quote = true;
                out.push(ch);
            }
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// RFC 5322 `specials` that force a display name to be quoted.
fn needs_quoting(name: &str) -> bool {
    name.chars()
        .any(|c| "()<>[]:;@\\,.\"".contains(c) || c.is_control())
}

/// Quote a display name when it contains specials, escaping `"` and `\`.
fn quote_display_name(name: &str) -> String {
    let name = if name.is_ascii() {
        name.to_string()
    } else {
        crate::headers::encode_header_value(name)
    };
    if needs_quoting(&name) || name.starts_with(char::is_whitespace) || name.ends_with(char::is_whitespace)
    {
        let mut out = String::with_capacity(name.len() + 2);
        out.push('"');
        for ch in name.chars() {
            if ch == '"' || ch == '\\' {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push('"');
        out
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn addr(s: &str) -> EmailAddress {
        EmailAddress::parse(s).unwrap()
    }

    #[test]
    fn parses_a_simple_pair() {
        let list = parse_address_list("Alice <alice@example.com>, bob@example.com");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name.as_deref(), Some("Alice"));
        assert_eq!(list[0].address.to_string(), "alice@example.com");
        assert_eq!(list[1].name, None);
        assert_eq!(list[1].address.to_string(), "bob@example.com");
    }

    #[test]
    fn parses_a_bare_address() {
        let list = parse_address_list("alice@example.com");
        assert_eq!(list.len(), 1);
        assert!(list[0].name.is_none());
    }

    #[test]
    fn handles_quoted_names_containing_commas() {
        let list = parse_address_list("\"Doe, John\" <john@example.com>, jane@example.com");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name.as_deref(), Some("Doe, John"));
        assert_eq!(list[1].address.to_string(), "jane@example.com");
    }

    #[test]
    fn handles_escaped_quotes_in_names() {
        let list = parse_address_list("\"He said \\\"hi\\\"\" <a@b.com>");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("He said \"hi\""));
    }

    #[test]
    fn decodes_rfc2047_display_names() {
        let list = parse_address_list("=?UTF-8?B?5byg5LiJ?= <zhang@example.cn>");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("张三"));

        let list = parse_address_list("=?UTF-8?Q?Jos=C3=A9?= <jose@example.es>");
        assert_eq!(list[0].name.as_deref(), Some("José"));
    }

    #[test]
    fn flattens_group_syntax() {
        let list = parse_address_list("Team: a@b.com, c@d.com;, e@f.com");
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].address.to_string(), "a@b.com");
        assert_eq!(list[1].address.to_string(), "c@d.com");
        assert_eq!(list[2].address.to_string(), "e@f.com");
    }

    #[test]
    fn empty_groups_contribute_nothing() {
        let list = parse_address_list("Undisclosed recipients:;");
        assert!(list.is_empty());
        let list = parse_address_list("A:;, b@c.com");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address.to_string(), "b@c.com");
    }

    #[test]
    fn groups_with_display_names_inside() {
        let list = parse_address_list("Friends: Alice <a@b.com>, Bob <c@d.com>;");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name.as_deref(), Some("Alice"));
        assert_eq!(list[1].name.as_deref(), Some("Bob"));
    }

    #[test]
    fn strips_comments() {
        let list = parse_address_list("alice@example.com (Alice the Great)");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address.to_string(), "alice@example.com");

        let list = parse_address_list("(leading) bob@example.com");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address.to_string(), "bob@example.com");
    }

    #[test]
    fn skips_unparsable_entries() {
        let list = parse_address_list("not-an-address, good@example.com, @bad.com");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address.to_string(), "good@example.com");
    }

    #[test]
    fn tolerates_odd_whitespace_and_trailing_commas() {
        let list = parse_address_list("  a@b.com ,,  c@d.com , ");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].address.to_string(), "a@b.com");
        assert_eq!(list[1].address.to_string(), "c@d.com");
    }

    #[test]
    fn round_trips_through_format() {
        let list = vec![
            Mailbox::with_name(addr("alice@example.com"), "Alice"),
            Mailbox::new(addr("bob@example.com")),
            Mailbox::with_name(addr("carol@example.com"), "Doe, Carol"),
        ];
        let formatted = format_address_list(&list);
        assert_eq!(
            formatted,
            "Alice <alice@example.com>, bob@example.com, \"Doe, Carol\" <carol@example.com>"
        );
        let back = parse_address_list(&formatted);
        assert_eq!(back, list);
    }

    #[test]
    fn display_and_display_impl_agree() {
        let m = Mailbox::with_name(addr("a@b.com"), "Alice");
        assert_eq!(m.display(), "Alice <a@b.com>");
        assert_eq!(m.to_string(), m.display());
        assert_eq!(Mailbox::new(addr("a@b.com")).display(), "a@b.com");
    }

    #[test]
    fn blank_display_names_become_none() {
        let m = Mailbox::with_name(addr("a@b.com"), "   ");
        assert_eq!(m.name, None);
    }

    #[test]
    fn formats_non_ascii_names_as_encoded_words() {
        let list = vec![Mailbox::with_name(addr("zhang@example.cn"), "张三")];
        let formatted = format_address_list(&list);
        assert!(formatted.starts_with("=?UTF-8?B?"), "got {formatted}");
        let back = parse_address_list(&formatted);
        assert_eq!(back[0].name.as_deref(), Some("张三"));
        assert_eq!(back[0].address.to_string(), "zhang@example.cn");
    }

    #[test]
    fn mailbox_json_round_trip() {
        let m = Mailbox::with_name(addr("a@b.com"), "Alice");
        let json = serde_json::to_string(&m).unwrap();
        let back: Mailbox = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn empty_input_yields_no_mailboxes() {
        assert!(parse_address_list("").is_empty());
        assert!(parse_address_list("   ").is_empty());
        assert!(parse_address_list(",").is_empty());
    }

    #[test]
    fn colon_inside_a_display_name_is_not_a_group() {
        let list = parse_address_list("\"Re: hello\" <a@b.com>");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("Re: hello"));
    }

    #[test]
    fn quoted_local_part_with_a_comma_is_kept_whole() {
        let list = parse_address_list("\"Doe, John\" <\"odd,local\"@example.com>");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address.local_part(), "\"odd,local\"");
    }

    #[test]
    fn comment_before_the_angle_address_is_ignored() {
        let list = parse_address_list("Alice (work) <alice@example.com>");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("Alice"));
    }

    #[test]
    fn semicolon_separated_entries_are_split_too() {
        // Some broken MUAs use `;` where `,` belongs.
        let list = parse_address_list("a@b.com; c@d.com");
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn empty_group_mixed_with_real_recipients() {
        let list = parse_address_list("Undisclosed recipients:;, real@example.com");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address.to_string(), "real@example.com");
    }

    #[test]
    fn format_then_parse_keeps_group_display_names() {
        let list = parse_address_list("Team: Alice <a@b.com>, Bob <c@d.com>;");
        let formatted = format_address_list(&list);
        let back = parse_address_list(&formatted);
        assert_eq!(back, list);
        // The group name itself is dropped, as it carries no addressing meaning.
        assert!(!formatted.contains("Team:"));
    }

    #[test]
    fn address_with_a_trailing_dot_domain_is_normalised() {
        let list = parse_address_list("alice@example.com.");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address.domain(), "example.com");
    }
}
