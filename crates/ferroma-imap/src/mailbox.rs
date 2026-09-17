//! IMAP mailbox names: `INBOX` case folding, the `/` hierarchy delimiter, name
//! validation, and the `LIST`/`LSUB` wildcard matcher.
//!
//! The rules implemented here are the ones RFC 3501 §5.1 and §6.3.8 state, plus
//! the practical ones every client depends on:
//!
//! * `INBOX` is the **only** case-insensitive name. `inbox` and `Inbox` both
//!   refer to it, and every other name is compared byte-for-byte.
//! * The hierarchy delimiter is `/`.
//! * In a `LIST` pattern, `*` matches zero or more characters **including** the
//!   delimiter, while `%` matches zero or more characters **excluding** it. That
//!   difference is what makes `LIST "" "%"` list only top-level folders and
//!   `LIST "" "*"` list the whole tree.
//! * `LIST "" ""` is a special case: the pattern is the empty name, which asks
//!   for the delimiter and the root — answered as `* LIST (\Noselect) "/" ""`.

/// The hierarchy delimiter (mailbox names are stored `/`-separated).
pub const DELIMITER: char = '/';

/// The one case-insensitive mailbox name.
pub const INBOX: &str = "INBOX";

/// The root name, returned for `LIST "" ""`.
pub const ROOT: &str = "";

/// Whether `name` is `INBOX`, ignoring case.
pub fn is_inbox(name: &str) -> bool {
    name.eq_ignore_ascii_case(INBOX)
}

/// Whether `name` denotes `INBOX` *or* something below it (`INBOX/Sub`).
pub fn is_under_inbox(name: &str) -> bool {
    name.split(DELIMITER).next().map(is_inbox).unwrap_or(false)
}

/// Canonical spelling of a mailbox name.
///
/// Only the `INBOX` component is folded (to upper case, the form RFC 3501 §5.1
/// says a server should use on the wire); everything else is preserved exactly.
pub fn canonical(name: &str) -> String {
    if is_inbox(name) {
        return INBOX.to_string();
    }
    match name.split_once(DELIMITER) {
        Some((first, rest)) if is_inbox(first) => format!("{INBOX}{DELIMITER}{rest}"),
        _ => name.to_string(),
    }
}

/// Whether two mailbox names denote the same mailbox.
pub fn same_mailbox(a: &str, b: &str) -> bool {
    is_inbox(a) && is_inbox(b) || a == b
}

/// Why a mailbox name is unacceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// The name is empty.
    Empty,
    /// The name is longer than [`MAX_NAME_LEN`] bytes.
    TooLong,
    /// The name contains a control character (`\0`, CR or LF).
    ControlCharacter,
    /// The name contains a wildcard, which is only legal in a `LIST` pattern.
    Wildcard,
    /// The name begins, ends, or doubles the hierarchy delimiter.
    BadDelimiter,
}

impl NameError {
    /// A human-readable explanation, suitable for a `NO` response.
    pub fn message(self) -> &'static str {
        match self {
            NameError::Empty => "mailbox name must not be empty",
            NameError::TooLong => "mailbox name is too long",
            NameError::ControlCharacter => "mailbox name contains a control character",
            NameError::Wildcard => "mailbox name must not contain a wildcard",
            NameError::BadDelimiter => "mailbox name has an empty hierarchy component",
        }
    }
}

/// Longest accepted mailbox name, in bytes.
///
/// RFC 3501 only requires that a name fit in a `mailbox` (`astring`), but every
/// real server caps it; this also bounds the Maildir path derived from it.
pub const MAX_NAME_LEN: usize = 512;

/// Validate a mailbox name a client wants to *create*.
pub fn validate_new_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong);
    }
    if name.chars().any(|c| c == '\0' || c == '\r' || c == '\n') {
        return Err(NameError::ControlCharacter);
    }
    if name.contains(['%', '*']) {
        return Err(NameError::Wildcard);
    }
    if name.starts_with(DELIMITER) || name.ends_with(DELIMITER) || name.contains("//") {
        return Err(NameError::BadDelimiter);
    }
    Ok(())
}

/// The parent of a hierarchical name, or `None` at the top level.
pub fn parent(name: &str) -> Option<&str> {
    name.rfind(DELIMITER).map(|idx| &name[..idx])
}

/// The last component of a hierarchical name.
pub fn leaf(name: &str) -> &str {
    match name.rfind(DELIMITER) {
        Some(idx) => &name[idx + 1..],
        None => name,
    }
}

/// Whether `child` is strictly below `ancestor` in the hierarchy.
pub fn is_child_of(ancestor: &str, child: &str) -> bool {
    let prefix_len = ancestor.len();
    child.len() > prefix_len + 1
        && child.starts_with(ancestor)
        && child.as_bytes()[prefix_len] == DELIMITER as u8
}

/// A `LIST`/`LSUB` pattern, with the reference name it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    /// The reference name the client sent (echoed, and applied by
    /// [`Pattern::effective`]).
    pub reference: String,
    /// The client's pattern, verbatim.
    pub pattern: String,
}

impl Pattern {
    /// Combine a reference name and a pattern the way RFC 3501 §6.3.8 defines.
    ///
    /// The reference is only meaningful when it ends with the delimiter (or is
    /// `INBOX`, which the server interprets relative to the inbox); otherwise
    /// clients expect it to be ignored, which is what every server does in
    /// practice.
    pub fn new(reference: &str, pattern: &str) -> Self {
        Pattern {
            reference: reference.to_string(),
            pattern: pattern.to_string(),
        }
    }

    /// The pattern to match against, with the reference applied.
    pub fn effective(&self) -> String {
        let reference = canonical(&self.reference);
        if reference.is_empty() {
            return self.pattern.clone();
        }
        if reference.ends_with(DELIMITER) {
            return format!("{reference}{}", self.pattern);
        }
        if is_inbox(&reference) {
            return format!("{INBOX}{DELIMITER}{}", self.pattern);
        }
        self.pattern.clone()
    }

    /// Whether this is the `LIST "" ""` special case.
    pub fn is_root_query(&self) -> bool {
        self.reference.is_empty() && self.pattern.is_empty()
    }

    /// Whether `name` matches this pattern.
    pub fn matches(&self, name: &str) -> bool {
        let effective = self.effective();
        let name = canonical(name);
        if is_inbox(&effective) {
            return is_inbox(&name);
        }
        wildcard_match(&effective, &name)
    }

    /// Whether any pattern in `patterns` matches `name`.
    pub fn any_matches(patterns: &[Pattern], name: &str) -> bool {
        patterns.iter().any(|p| p.matches(name))
    }
}

/// Match a `LIST` pattern against a mailbox name.
///
/// `*` matches any run of characters (delimiter included); `%` matches any run
/// that does not contain the delimiter. Matching is byte-wise, which is what
/// IMAP requires for the non-`INBOX` names (they are compared literally).
///
/// The implementation is iterative with an explicit backtracking point per `*`
/// or `%`, so a pathological pattern such as `"**********x"` cannot blow the
/// stack or take exponential time.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let pat = pattern.as_bytes();
    let text = name.as_bytes();
    let delim = DELIMITER as u8;

    let mut p = 0usize; // index into `pat`
    let mut t = 0usize; // index into `text`
    // Where to resume after a failed match: the pattern index just past the last
    // wildcard, and the text index that wildcard consumed up to.
    let mut star_p: Option<usize> = None;
    let mut star_t = 0usize;
    let mut star_crosses_delimiter = true;

    loop {
        if p < pat.len() {
            match pat[p] {
                b'*' | b'%' => {
                    star_crosses_delimiter = pat[p] == b'*';
                    // Collapse a run of wildcards: `**` is `*`, `*%` is `*`.
                    let mut next = p + 1;
                    while next < pat.len() && matches!(pat[next], b'*' | b'%') {
                        star_crosses_delimiter =
                            star_crosses_delimiter || pat[next] == b'*';
                        next += 1;
                    }
                    star_p = Some(next);
                    star_t = t;
                    p = next;
                    continue;
                }
                literal => {
                    if t < text.len() && text[t] == literal {
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
            }
        } else if t >= text.len() {
            return true;
        }

        // Mismatch (or pattern exhausted with text left): backtrack to the last
        // wildcard and let it consume one more byte.
        match star_p {
            Some(next) => {
                if star_t >= text.len() {
                    return false;
                }
                if !star_crosses_delimiter && text[star_t] == delim {
                    return false;
                }
                star_t += 1;
                t = star_t;
                p = next;
            }
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_is_the_only_case_insensitive_name() {
        assert!(is_inbox("INBOX"));
        assert!(is_inbox("inbox"));
        assert!(is_inbox("InBoX"));
        assert!(!is_inbox("Sent"));
        assert!(!is_inbox("INBOXES"));
        assert!(!is_inbox(""));
        assert!(same_mailbox("inbox", "INBOX"));
        assert!(same_mailbox("Sent", "Sent"));
        assert!(!same_mailbox("Sent", "sent"));
    }

    #[test]
    fn canonical_folds_only_the_inbox_component() {
        assert_eq!(canonical("inbox"), "INBOX");
        assert_eq!(canonical("inbox/Sub"), "INBOX/Sub");
        assert_eq!(canonical("Sent"), "Sent");
        assert_eq!(canonical("sent"), "sent");
        assert_eq!(canonical("Archive/2026"), "Archive/2026");
        assert_eq!(canonical(""), "");
    }

    #[test]
    fn is_under_inbox_checks_the_first_component() {
        assert!(is_under_inbox("INBOX"));
        assert!(is_under_inbox("inbox/Sub"));
        assert!(!is_under_inbox("INBOXES/x"));
        assert!(!is_under_inbox("Sent"));
    }

    #[test]
    fn parent_and_leaf_split_on_the_delimiter() {
        assert_eq!(parent("Archive/2026/Q1"), Some("Archive/2026"));
        assert_eq!(parent("Archive"), None);
        assert_eq!(leaf("Archive/2026"), "2026");
        assert_eq!(leaf("Archive"), "Archive");
    }

    #[test]
    fn is_child_of_requires_a_delimiter_boundary() {
        assert!(is_child_of("Archive", "Archive/2026"));
        assert!(!is_child_of("Archive", "Archive2026"));
        assert!(!is_child_of("Archive", "Archive"));
        assert!(!is_child_of("", "Archive"));
    }

    #[test]
    fn validation_accepts_ordinary_names() {
        assert_eq!(validate_new_name("INBOX"), Ok(()));
        assert_eq!(validate_new_name("Sent"), Ok(()));
        assert_eq!(validate_new_name("Archive/2026/Q1"), Ok(()));
        assert_eq!(validate_new_name("日本語"), Ok(()));
        assert_eq!(validate_new_name("two words"), Ok(()));
    }

    #[test]
    fn validation_rejects_hostile_names() {
        assert_eq!(validate_new_name(""), Err(NameError::Empty));
        assert_eq!(
            validate_new_name("bad\0name"),
            Err(NameError::ControlCharacter)
        );
        assert_eq!(
            validate_new_name("bad\r\nname"),
            Err(NameError::ControlCharacter)
        );
        assert_eq!(validate_new_name("wild*card"), Err(NameError::Wildcard));
        assert_eq!(validate_new_name("wild%card"), Err(NameError::Wildcard));
        assert_eq!(validate_new_name("/leading"), Err(NameError::BadDelimiter));
        assert_eq!(validate_new_name("trailing/"), Err(NameError::BadDelimiter));
        assert_eq!(validate_new_name("a//b"), Err(NameError::BadDelimiter));
        assert_eq!(
            validate_new_name(&"x".repeat(MAX_NAME_LEN + 1)),
            Err(NameError::TooLong)
        );
    }

    #[test]
    fn every_name_error_has_a_message() {
        for err in [
            NameError::Empty,
            NameError::TooLong,
            NameError::ControlCharacter,
            NameError::Wildcard,
            NameError::BadDelimiter,
        ] {
            assert!(!err.message().is_empty());
        }
    }

    #[test]
    fn pattern_applies_a_trailing_delimiter_reference() {
        let p = Pattern::new("Archive/", "*");
        assert_eq!(p.effective(), "Archive/*");
        assert!(p.matches("Archive/2026"));
        assert!(!p.matches("Sent"));
    }

    #[test]
    fn pattern_ignores_a_non_delimiter_reference() {
        let p = Pattern::new("Archive", "*");
        assert_eq!(p.effective(), "*");
        assert!(p.matches("Sent"));
    }

    #[test]
    fn pattern_treats_an_inbox_reference_as_a_prefix() {
        let p = Pattern::new("inbox", "%");
        assert_eq!(p.effective(), "INBOX/%");
        assert!(p.matches("INBOX/Sub"));
        assert!(!p.matches("Sent"));
    }

    #[test]
    fn root_query_is_detected() {
        assert!(Pattern::new("", "").is_root_query());
        assert!(!Pattern::new("", "*").is_root_query());
        assert!(!Pattern::new("INBOX", "").is_root_query());
    }

    #[test]
    fn star_enumerates_the_whole_tree() {
        let p = Pattern::new("", "*");
        assert!(p.matches("INBOX"));
        assert!(p.matches("Sent"));
        assert!(p.matches("Archive/2026"));
        assert!(p.matches("a/b/c/d"));
        assert!(p.matches(""));
    }

    #[test]
    fn percent_never_crosses_the_delimiter() {
        let p = Pattern::new("", "%");
        assert!(p.matches("INBOX"));
        assert!(p.matches("Sent"));
        assert!(!p.matches("Archive/2026"));
        assert!(!p.matches("a/b"));
    }

    #[test]
    fn percent_matches_a_partial_component() {
        let p = Pattern::new("", "%/2026");
        assert!(p.matches("Archive/2026"));
        assert!(!p.matches("a/b/2026"));
    }

    #[test]
    fn literal_patterns_match_exactly() {
        let p = Pattern::new("", "Sent");
        assert!(p.matches("Sent"));
        assert!(!p.matches("Sent/2026"));
        assert!(!p.matches("sent"));
    }

    #[test]
    fn pattern_table() {
        // (pattern, name, expected)
        let cases: &[(&str, &str, bool)] = &[
            ("*", "INBOX", true),
            ("*", "a/b", true),
            ("*", "", true),
            ("%", "INBOX", true),
            ("%", "a/b", false),
            ("INBOX", "INBOX", true),
            ("INBOX", "inbox", true),
            ("INBOX", "INBOX/Sub", false),
            ("INBOX/%", "INBOX/Sub", true),
            ("INBOX/%", "INBOX/a/b", false),
            ("INBOX/*", "INBOX/a/b", true),
            ("Archive/%", "Archive/2026", true),
            ("Archive/%", "Archive/2026/Q1", false),
            ("Archive/*", "Archive/2026/Q1", true),
            ("A*", "Archive/2026", true),
            ("A%C", "Archive/2026", false),
            ("*2026", "Archive/2026", true),
            ("*2026", "Archive/2025", false),
            ("*/*", "a/b", true),
            ("*/*", "a", false),
            ("%/%", "a/b", true),
            ("%/%", "a/b/c", false),
            ("a*b", "ab", true),
            ("a*b", "axxxb", true),
            ("a*b", "axxx", false),
            ("a%b", "ab", true),
            ("a%b", "a/b", false),
            ("**", "anything/at/all", true),
            ("%%", "a/b", false),
            ("*%x", "a/b/x", true),
            ("", "", true),
            ("", "INBOX", false),
        ];
        for (pattern, name, expected) in cases {
            let p = Pattern::new("", pattern);
            assert_eq!(
                p.matches(name),
                *expected,
                "pattern `{pattern}` against `{name}`"
            );
        }
    }

    #[test]
    fn wildcard_match_is_not_exponential() {
        // A pathological pattern must return promptly rather than backtrack
        // forever.
        let pattern = format!("{}b", "*".repeat(40));
        let name = "a".repeat(2000);
        assert!(!wildcard_match(&pattern, &name));
        assert!(wildcard_match(&pattern, &format!("{}b", "a".repeat(2000))));
    }

    #[test]
    fn any_matches_accepts_any_of_several_patterns() {
        let patterns = vec![Pattern::new("", "INBOX"), Pattern::new("", "Archive/%")];
        assert!(Pattern::any_matches(&patterns, "inbox"));
        assert!(Pattern::any_matches(&patterns, "Archive/2026"));
        assert!(!Pattern::any_matches(&patterns, "Sent"));
    }
}
