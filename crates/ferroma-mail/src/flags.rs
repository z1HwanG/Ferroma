//! IMAP flags: the six system flags plus arbitrary keywords.
//!
//! RFC 3501 §2.3.2 defines `\Seen`, `\Answered`, `\Flagged`, `\Deleted`,
//! `\Draft` and `\Recent`. Everything else a client sends is a *keyword*
//! (`$Junk`, `$label1`, …), which Ferroma stores verbatim: two clients that
//! agree on a keyword can use it, and one that does not simply ignores it.

use std::fmt;

/// The six system flags, as a bitset plus a list of keywords.
///
/// The bitset keeps the system flags cheap (one `u8`) while the keyword list
/// keeps custom flags open-ended, which is exactly how IMAP treats them.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Flags {
    /// Bit 0 `\Seen`, 1 `\Answered`, 2 `\Flagged`, 3 `\Deleted`, 4 `\Draft`,
    /// 5 `\Recent`.
    bits: u8,
    /// Custom keywords such as `$Junk`, in insertion order.
    keywords: Vec<String>,
}

/// Canonical system-flag names, index == bit position.
const SYSTEM_FLAGS: [&str; 6] = [
    "\\Seen",
    "\\Answered",
    "\\Flagged",
    "\\Deleted",
    "\\Draft",
    "\\Recent",
];

impl Flags {
    /// No flags set.
    pub fn new() -> Self {
        Flags {
            bits: 0,
            keywords: Vec::new(),
        }
    }

    /// Read a system flag by bit position.
    fn bit(&self, index: u8) -> bool {
        self.bits & (1 << index) != 0
    }

    /// Write a system flag by bit position.
    fn set_bit(&mut self, index: u8, on: bool) {
        if on {
            self.bits |= 1 << index;
        } else {
            self.bits &= !(1 << index);
        }
    }

    /// Whether `\Seen` is set.
    pub fn seen(&self) -> bool {
        self.bit(0)
    }

    /// Set or clear `\Seen`.
    pub fn set_seen(&mut self, on: bool) {
        self.set_bit(0, on);
    }

    /// Whether `\Answered` is set.
    pub fn answered(&self) -> bool {
        self.bit(1)
    }

    /// Set or clear `\Answered`.
    pub fn set_answered(&mut self, on: bool) {
        self.set_bit(1, on);
    }

    /// Whether `\Flagged` is set.
    pub fn flagged(&self) -> bool {
        self.bit(2)
    }

    /// Set or clear `\Flagged`.
    pub fn set_flagged(&mut self, on: bool) {
        self.set_bit(2, on);
    }

    /// Whether `\Deleted` is set.
    pub fn deleted(&self) -> bool {
        self.bit(3)
    }

    /// Set or clear `\Deleted`.
    pub fn set_deleted(&mut self, on: bool) {
        self.set_bit(3, on);
    }

    /// Whether `\Draft` is set.
    pub fn draft(&self) -> bool {
        self.bit(4)
    }

    /// Set or clear `\Draft`.
    pub fn set_draft(&mut self, on: bool) {
        self.set_bit(4, on);
    }

    /// Whether `\Recent` is set.
    pub fn recent(&self) -> bool {
        self.bit(5)
    }

    /// Set or clear `\Recent`.
    pub fn set_recent(&mut self, on: bool) {
        self.set_bit(5, on);
    }

    /// The keywords, in insertion order.
    ///
    /// Values are lower-cased and deduplicated; see [`Flags::add_keyword`].
    pub fn keywords(&self) -> &[String] {
        &self.keywords
    }

    /// Add a keyword unless an equal (case-insensitive) keyword is present.
    ///
    /// Keywords are stored lower-cased: IMAP compares them case-insensitively,
    /// and keeping one canonical spelling makes [`Flags::to_db_string`] (and
    /// therefore every cache key derived from it) deterministic. A leading `\`
    /// is stripped, because `\Seen`-style names are system flags and must not be
    /// smuggled in as keywords.
    pub fn add_keyword(&mut self, keyword: &str) {
        let cleaned = keyword.trim().trim_start_matches('\\').to_lowercase();
        if cleaned.is_empty() {
            return;
        }
        if self.keywords.contains(&cleaned) {
            return;
        }
        self.keywords.push(cleaned);
    }

    /// Remove a keyword, case-insensitively.
    pub fn remove_keyword(&mut self, keyword: &str) {
        let cleaned = keyword.trim().trim_start_matches('\\').to_lowercase();
        self.keywords.retain(|k| *k != cleaned);
    }

    /// Whether a keyword is present, case-insensitively.
    pub fn has_keyword(&self, keyword: &str) -> bool {
        let cleaned = keyword.trim().trim_start_matches('\\').to_lowercase();
        self.keywords.contains(&cleaned)
    }

    /// Parse an IMAP flag list.
    ///
    /// Accepts `(\Seen \Flagged $Label1)`, the bare form without parentheses,
    /// doubled spaces, and quoted keywords. Unknown `\Foo` names are stored as
    /// keywords so a client's private flags survive a round trip.
    pub fn parse(raw: &str) -> Self {
        let mut flags = Flags::new();
        for token in tokenize_flags(raw) {
            let mut matched = false;
            for (index, name) in SYSTEM_FLAGS.iter().enumerate() {
                if token.eq_ignore_ascii_case(name) {
                    flags.set_bit(index as u8, true);
                    matched = true;
                    break;
                }
            }
            if !matched {
                flags.add_keyword(&token);
            }
        }
        flags
    }

    /// Render the IMAP flag list: `(\Seen \Flagged "$Label1")`.
    ///
    /// Keywords are quoted only when they contain a character IMAP would
    /// otherwise mis-read.
    pub fn to_imap_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (index, name) in SYSTEM_FLAGS.iter().enumerate() {
            if self.bit(index as u8) {
                parts.push((*name).to_string());
            }
        }
        for keyword in &self.keywords {
            parts.push(quote_keyword(keyword));
        }
        format!("({})", parts.join(" "))
    }

    /// Render the database form: `seen,flagged,$Label1`.
    ///
    /// System flags come first as lower-cased bare names in a fixed order, then
    /// the keywords sorted lexicographically, so the same flag set always
    /// produces the same string (which keeps cache keys and dedup stable).
    pub fn to_db_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (index, name) in SYSTEM_FLAGS.iter().enumerate() {
            if self.bit(index as u8) {
                parts.push(name[1..].to_ascii_lowercase());
            }
        }
        let mut keywords: Vec<String> = self.keywords.clone();
        keywords.sort_unstable();
        keywords.dedup();
        parts.extend(keywords);
        parts.join(",")
    }

    /// Read back what [`Flags::to_db_string`] wrote.
    ///
    /// Tolerates stray whitespace and empty fields, so a hand-edited row still
    /// loads.
    pub fn from_db_string(raw: &str) -> Self {
        let mut flags = Flags::new();
        for field in raw.split(',') {
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            let mut matched = false;
            for (index, name) in SYSTEM_FLAGS.iter().enumerate() {
                if field.eq_ignore_ascii_case(&name[1..]) {
                    flags.set_bit(index as u8, true);
                    matched = true;
                    break;
                }
            }
            if !matched {
                flags.add_keyword(field);
            }
        }
        flags
    }

    /// Whether nothing at all is set.
    pub fn is_empty(&self) -> bool {
        self.bits == 0 && self.keywords.is_empty()
    }

    /// The set system flags as canonical names, in bit order.
    pub fn system_flags(&self) -> Vec<&'static str> {
        SYSTEM_FLAGS
            .iter()
            .enumerate()
            .filter(|(index, _)| self.bit(*index as u8))
            .map(|(_, name)| *name)
            .collect()
    }
}

impl fmt::Display for Flags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_imap_string())
    }
}

/// Split an IMAP flag list into tokens, honouring quoted keywords.
fn tokenize_flags(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut escaped = false;

    for ch in raw.chars() {
        if in_quote {
            if escaped {
                current.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quote = false;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '(' | ')' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            '"' => in_quote = true,
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Quote a keyword when IMAP requires it.
fn quote_keyword(keyword: &str) -> String {
    let needs = keyword.is_empty()
        || keyword
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"\\(){}%*".contains(c));
    if !needs {
        return keyword.to_string();
    }
    let mut out = String::with_capacity(keyword.len() + 2);
    out.push('"');
    for ch in keyword.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn new_has_nothing_set() {
        let f = Flags::new();
        assert!(f.is_empty());
        assert!(!f.seen());
        assert!(!f.answered());
        assert!(!f.flagged());
        assert!(!f.deleted());
        assert!(!f.draft());
        assert!(!f.recent());
        assert!(f.keywords().is_empty());
        assert_eq!(f, Flags::default());
        assert_eq!(f.to_imap_string(), "()");
        assert_eq!(f.to_db_string(), "");
    }

    #[test]
    fn every_system_flag_round_trips() {
        let mut f = Flags::new();
        f.set_seen(true);
        f.set_answered(true);
        f.set_flagged(true);
        f.set_deleted(true);
        f.set_draft(true);
        f.set_recent(true);
        assert!(f.seen() && f.answered() && f.flagged() && f.deleted() && f.draft() && f.recent());
        assert!(!f.is_empty());
        assert_eq!(
            f.to_imap_string(),
            "(\\Seen \\Answered \\Flagged \\Deleted \\Draft \\Recent)"
        );
        assert_eq!(
            f.to_db_string(),
            "seen,answered,flagged,deleted,draft,recent"
        );

        f.set_answered(false);
        assert!(!f.answered());
        assert_eq!(f.to_db_string(), "seen,flagged,deleted,draft,recent");
    }

    #[test]
    fn parses_a_typical_imap_flag_list() {
        let f = Flags::parse("(\\Seen \\Flagged $Label1)");
        assert!(f.seen());
        assert!(f.flagged());
        assert!(!f.answered());
        assert_eq!(f.keywords(), &["$label1".to_string()]);
        assert!(f.has_keyword("$label1"), "keywords are case-insensitive");
        assert!(f.has_keyword("$LABEL1"));
    }

    #[test]
    fn parses_without_parentheses_and_with_extra_spaces() {
        let f = Flags::parse("   \\Seen    \\Draft   ");
        assert!(f.seen());
        assert!(f.draft());
        assert_eq!(f.keywords().len(), 0);
        assert_eq!(Flags::parse(""), Flags::new());
        assert_eq!(Flags::parse("()"), Flags::new());
        assert_eq!(Flags::parse("( )"), Flags::new());
    }

    #[test]
    fn parse_is_case_insensitive_for_system_flags() {
        let f = Flags::parse("(\\seEN \\FLAGGED)");
        assert!(f.seen());
        assert!(f.flagged());
        assert!(f.keywords().is_empty());
    }

    #[test]
    fn parses_quoted_keywords() {
        let f = Flags::parse("(\\Seen \"my label\" $Junk)");
        assert!(f.seen());
        assert!(f.has_keyword("my label"));
        assert!(f.has_keyword("$Junk"));
        assert_eq!(f.keywords(), &["my label".to_string(), "$junk".to_string()]);
        // And they render back quoted, preserving the space.
        assert_eq!(f.to_imap_string(), "(\\Seen \"my label\" $junk)");
        let back = Flags::parse(&f.to_imap_string());
        assert_eq!(back, f);
    }

    #[test]
    fn unknown_backslash_flags_become_keywords() {
        let f = Flags::parse("(\\Seen \\Forwarded)");
        assert!(f.seen());
        assert!(f.has_keyword("Forwarded"));
        assert_eq!(f.to_imap_string(), "(\\Seen forwarded)");
    }

    #[test]
    fn keywords_are_deduplicated_case_insensitively() {
        let mut f = Flags::new();
        f.add_keyword("$Junk");
        f.add_keyword("$junk");
        assert_eq!(f.keywords().len(), 1);
        assert_eq!(f.keywords()[0], "$junk");

        f.remove_keyword("$JUNK");
        assert!(!f.has_keyword("$junk"));
        assert!(f.is_empty());
    }

    #[test]
    fn adding_an_empty_keyword_is_a_no_op() {
        let mut f = Flags::new();
        f.add_keyword("   ");
        f.add_keyword("\\");
        assert!(f.is_empty());
    }

    #[test]
    fn db_round_trip_preserves_everything() {
        let raw = "seen,flagged,$label1,$label2";
        let f = Flags::from_db_string(raw);
        assert!(f.seen());
        assert!(f.flagged());
        assert!(f.has_keyword("$Label1"));
        assert_eq!(f.to_db_string(), raw);
    }

    #[test]
    fn db_round_trip_is_property_style_over_many_combinations() {
        // Every one of the 64 system-flag combinations, crossed with several
        // keyword sets.
        let keyword_sets: [&[&str]; 4] = [
            &[],
            &["$Junk"],
            &["$Junk", "$Label1"],
            &["$Label1", "$Label2", "$Label3"],
        ];
        for mask in 0u8..64 {
            for keywords in keyword_sets {
                let mut f = Flags::new();
                for bit in 0..6u8 {
                    let on = mask & (1 << bit) != 0;
                    match bit {
                        0 => f.set_seen(on),
                        1 => f.set_answered(on),
                        2 => f.set_flagged(on),
                        3 => f.set_deleted(on),
                        4 => f.set_draft(on),
                        _ => f.set_recent(on),
                    }
                }
                for keyword in keywords {
                    f.add_keyword(keyword);
                }

                let db = f.to_db_string();
                let back = Flags::from_db_string(&db);
                assert_eq!(back, f, "mask {mask} keywords {keywords:?} via {db}");

                // Rendering is deterministic and the IMAP form also round-trips.
                assert_eq!(f.to_db_string(), db);
                let imap = f.to_imap_string();
                assert_eq!(Flags::parse(&imap), f, "imap {imap}");
            }
        }
    }

    #[test]
    fn db_form_is_sorted_and_lowercase() {
        let mut f = Flags::new();
        f.add_keyword("$Zebra");
        f.add_keyword("$apple");
        f.add_keyword("$Mango");
        f.set_seen(true);
        assert_eq!(f.to_db_string(), "seen,$apple,$mango,$zebra");
    }

    #[test]
    fn db_parsing_tolerates_whitespace_and_empty_fields() {
        let f = Flags::from_db_string(" seen , , flagged ,, $Junk ");
        assert!(f.seen());
        assert!(f.flagged());
        assert!(f.has_keyword("$Junk"));
        assert_eq!(f.to_db_string(), "seen,flagged,$junk");
    }

    #[test]
    fn display_matches_the_imap_form() {
        let f = Flags::parse("(\\Seen \\Answered)");
        assert_eq!(f.to_string(), f.to_imap_string());
        assert_eq!(f.to_string(), "(\\Seen \\Answered)");
    }

    #[test]
    fn system_flags_are_listed_in_bit_order() {
        let mut f = Flags::new();
        f.set_recent(true);
        f.set_flagged(true);
        assert_eq!(f.system_flags(), vec!["\\Flagged", "\\Recent"]);
    }

    #[test]
    fn json_round_trip() {
        let f = Flags::parse("(\\Seen $Junk)");
        let json = serde_json::to_string(&f).unwrap();
        let back: Flags = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn keyword_with_specials_gets_quoted() {
        let mut f = Flags::new();
        f.add_keyword("has space");
        assert_eq!(f.to_imap_string(), "(\"has space\")");
        assert_eq!(Flags::parse(&f.to_imap_string()), f);
    }

    #[test]
    fn keyword_with_a_quote_is_escaped() {
        let mut f = Flags::new();
        f.add_keyword("say\"hi");
        let rendered = f.to_imap_string();
        assert_eq!(rendered, "(\"say\\\"hi\")");
        assert_eq!(Flags::parse(&rendered), f);
    }

    #[test]
    fn every_system_flag_maps_to_its_own_bit() {
        let names = [
            "\\Seen",
            "\\Answered",
            "\\Flagged",
            "\\Deleted",
            "\\Draft",
            "\\Recent",
        ];
        for (index, name) in names.iter().enumerate() {
            let f = Flags::parse(name);
            assert_eq!(f.system_flags(), vec![*name], "{name}");
            // Only the bit for this flag is set.
            assert_eq!(f.to_db_string(), name[1..].to_lowercase());
            assert_eq!(f.to_imap_string(), format!("({name})"));
            let _ = index;
        }
    }

    #[test]
    fn parse_then_render_is_idempotent() {
        let f = Flags::parse("(\\Seen \\Answered $junk \"two words\")");
        let once = f.to_imap_string();
        let twice = Flags::parse(&once).to_imap_string();
        assert_eq!(once, twice);
    }

    #[test]
    fn db_string_and_imap_string_agree_about_the_flag_set() {
        let f = Flags::from_db_string("seen,draft,$junk");
        let from_imap = Flags::parse(&f.to_imap_string());
        assert_eq!(from_imap, f);
        assert_eq!(from_imap.to_db_string(), f.to_db_string());
    }

    #[test]
    fn flags_are_comparable_and_cloneable() {
        let a = Flags::parse("(\\Seen $junk)");
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, Flags::parse("(\\Seen)"));
    }
}
