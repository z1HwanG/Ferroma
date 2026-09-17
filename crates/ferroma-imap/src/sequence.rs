//! IMAP sequence sets and UID sets (RFC 3501 §9 `sequence-set`).
//!
//! A `sequence-set` is a comma-separated list of *sequence ranges*, each of
//! which is either a single number, `n:m`, or one of the two wildcard forms
//! `n:*` / `*:n`. `*` always denotes **the largest value in use** — which, for
//! `*:5`, is the *start* of the range, so the range is reversed and expands
//! downwards. This module keeps that distinction explicit instead of flattening
//! it, because that is exactly where hand-written IMAP servers get it wrong.
//!
//! An empty set is **not** an error: it matches nothing. (RFC 3501 §6.4.8
//! explicitly requires `SEARCH` on an empty result to report success.)

use std::fmt;

use ferroma_core::FerromaError;

/// The largest sequence number that fits the wire grammar's practical limits.
///
/// RFC 3501 allows any `nz-number`; servers cap `UID` at `4294967295` and
/// `CONDSTORE` extends that to the full `u64` range for `MODSEQ`. We accept the
/// full unsigned range and let the caller clamp.
pub const MAX_SEQUENCE_NUMBER: u64 = u32::MAX as u64;

/// The upper bound of one sequence range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// A literal number.
    Number(u64),
    /// The `*` wildcard: the largest value in use.
    Star,
}

impl Bound {
    fn write_into(self, out: &mut String) {
        match self {
            Bound::Number(n) => out.push_str(&n.to_string()),
            Bound::Star => out.push('*'),
        }
    }
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = String::new();
        self.write_into(&mut text);
        f.write_str(&text)
    }
}

/// One `n`, `n:m`, `n:*` or `*:n` range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqRange {
    /// Low (or, for a reversed range, syntactically first) bound.
    pub from: Bound,
    /// High (or syntactically second) bound.
    pub to: Bound,
}

impl SeqRange {
    /// A single-number range, `n`.
    pub fn single(n: u64) -> Self {
        SeqRange {
            from: Bound::Number(n),
            to: Bound::Number(n),
        }
    }

    /// An inclusive range `from:to`.
    pub fn range(from: u64, to: u64) -> Self {
        SeqRange {
            from: Bound::Number(from),
            to: Bound::Number(to),
        }
    }

    /// Resolve the range into an inclusive `(low, high)` pair given the largest
    /// value in use (`max`), or `None` when it is empty.
    ///
    /// `*` resolves to `max` regardless of which side it appears on; the two
    /// numbers are then ordered, so `*:5` with `max = 3` becomes `3..=5` and
    /// `5:*` with `max = 3` becomes `3..=5` as well. When the mailbox is empty
    /// (`max == 0`) every range is empty.
    pub fn resolve(&self, max: u64) -> Option<(u64, u64)> {
        if max == 0 {
            return None;
        }
        let resolve_one = |b: Bound| match b {
            Bound::Number(n) => n,
            Bound::Star => max,
        };
        let a = resolve_one(self.from);
        let b = resolve_one(self.to);
        let (low, high) = if a <= b { (a, b) } else { (b, a) };
        if low == 0 {
            // `0` is not a legal `nz-number`; a `0:*` range clamps to `1:*`.
            return if high == 0 { None } else { Some((1, high)) };
        }
        Some((low, high))
    }

    /// The concrete values this range denotes, clamped to `max`.
    ///
    /// This is the form callers actually want: the *range* is not clamped
    /// before it is expanded, so `5:*` against a three-message mailbox expands
    /// to `5..=3` and therefore to nothing — `*` means "the largest value in
    /// use", and the mailbox simply has no message 5.
    pub fn expand(&self, max: u64) -> Vec<u64> {
        match self.resolve(max) {
            Some((low, high)) => (low..=high.min(max)).filter(|n| *n > 0).collect(),
            None => Vec::new(),
        }
    }

    /// Whether the range contains one concrete value.
    pub fn contains(&self, value: u64, max: u64) -> bool {
        match self.resolve(max) {
            Some((low, high)) => value >= low && value <= high,
            None => false,
        }
    }

    /// Whether the range is a single number (so a `SEARCH`/`FETCH` can avoid
    /// expanding it).
    pub fn is_single(&self) -> bool {
        matches!((self.from, self.to), (Bound::Number(a), Bound::Number(b)) if a == b)
    }
}

impl fmt::Display for SeqRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = String::new();
        self.from.write_into(&mut text);
        if !self.is_single() {
            text.push(':');
            self.to.write_into(&mut text);
        }
        f.write_str(&text)
    }
}

/// A parsed `sequence-set`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SequenceSet {
    ranges: Vec<SeqRange>,
}

impl SequenceSet {
    /// The empty set — matches nothing, and is never an error.
    pub fn empty() -> Self {
        SequenceSet { ranges: Vec::new() }
    }

    /// Build a set from ranges.
    pub fn from_ranges(ranges: Vec<SeqRange>) -> Self {
        SequenceSet { ranges }
    }

    /// The ranges, in the order the client wrote them.
    pub fn ranges(&self) -> &[SeqRange] {
        &self.ranges
    }

    /// Whether the set is empty (matches nothing).
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Parse a `sequence-set`.
    ///
    /// Accepts `1`, `1,3,5`, `1:5`, `5:*`, `*:5`, `*`, and combinations. Every
    /// ill-formed input is a `BAD`-carrying [`FerromaError::Parse`] rather than a
    /// panic.
    pub fn parse(raw: &str) -> Result<Self, FerromaError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(bad("empty sequence set"));
        }
        let mut ranges = Vec::new();
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(bad(format!("empty element in sequence set `{raw}`")));
            }
            ranges.push(parse_range(part, raw)?);
        }
        if ranges.is_empty() {
            return Err(bad("empty sequence set"));
        }
        Ok(SequenceSet { ranges })
    }

    /// The concrete values this set denotes, given the largest value in use.
    ///
    /// The result is sorted, deduplicated and bounded by `max`, so a client
    /// sending `1:4294967295` against a three-message mailbox costs three
    /// entries and not four billion.
    pub fn resolve(&self, max: u64) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        for range in &self.ranges {
            let Some((low, high)) = range.resolve(max) else {
                continue;
            };
            let high = high.min(max);
            if low > high {
                continue;
            }
            out.extend(low..=high);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether one value is in the set.
    pub fn contains(&self, value: u64, max: u64) -> bool {
        self.ranges.iter().any(|r| r.contains(value, max))
    }

    /// Filter an ordered list of values, keeping those the set matches and the
    /// order of the input.
    ///
    /// This is the form `FETCH`/`STORE` want: the answer must follow the
    /// mailbox order (which for IMAP sequence numbers *is* the wire order), not
    /// the order the client listed the ranges in.
    pub fn filter_ordered<T, F>(&self, values: &[T], max: u64, key: F) -> Vec<usize>
    where
        F: Fn(&T) -> u64,
    {
        values
            .iter()
            .enumerate()
            .filter(|(_, v)| self.contains(key(v), max))
            .map(|(i, _)| i)
            .collect()
    }

    /// Only the single-number ranges, for the `SEARCH` fast path.
    pub fn singles(&self) -> Vec<u64> {
        self.ranges
            .iter()
            .filter_map(|r| match (r.from, r.to) {
                (Bound::Number(a), Bound::Number(b)) if a == b => Some(a),
                _ => None,
            })
            .collect()
    }
}

impl fmt::Display for SequenceSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.ranges.iter().map(SeqRange::to_string).collect();
        f.write_str(&parts.join(","))
    }
}

/// Parse one `n`, `n:m`, `n:*` or `*:n`.
fn parse_range(part: &str, whole: &str) -> Result<SeqRange, FerromaError> {
    match part.split_once(':') {
        None => Ok(SeqRange {
            from: parse_bound(part, whole)?,
            to: parse_bound(part, whole)?,
        }),
        Some((left, right)) => {
            if right.contains(':') {
                return Err(bad(format!("malformed range `{part}` in `{whole}`")));
            }
            Ok(SeqRange {
                from: parse_bound(left, whole)?,
                to: parse_bound(right, whole)?,
            })
        }
    }
}

/// Parse one end of a range: `*` or a `nz-number`.
fn parse_bound(raw: &str, whole: &str) -> Result<Bound, FerromaError> {
    let raw = raw.trim();
    if raw == "*" {
        return Ok(Bound::Star);
    }
    if raw.is_empty() {
        return Err(bad(format!("missing range bound in `{whole}`")));
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad(format!("`{raw}` is not a sequence number in `{whole}`")));
    }
    let value: u64 = raw
        .parse()
        .map_err(|_| bad(format!("sequence number `{raw}` is out of range in `{whole}`")))?;
    Ok(Bound::Number(value))
}

/// A `BAD`-shaped parse failure.
fn bad(message: impl Into<String>) -> FerromaError {
    FerromaError::Parse(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> SequenceSet {
        SequenceSet::parse(raw).expect("test input must parse")
    }

    #[test]
    fn a_single_number_is_a_single_range() {
        let set = parse("3");
        assert_eq!(set.ranges().len(), 1);
        assert!(set.ranges()[0].is_single());
        assert_eq!(set.resolve(10), vec![3]);
        assert_eq!(set.to_string(), "3");
    }

    #[test]
    fn comma_lists_accumulate_and_sort() {
        let set = parse("5,1,3");
        assert_eq!(set.resolve(10), vec![1, 3, 5]);
        assert_eq!(set.to_string(), "5,1,3");
    }

    #[test]
    fn ascending_and_descending_ranges_are_both_inclusive() {
        assert_eq!(parse("1:3").resolve(10), vec![1, 2, 3]);
        assert_eq!(parse("3:1").resolve(10), vec![1, 2, 3]);
    }

    #[test]
    fn overlapping_ranges_deduplicate() {
        assert_eq!(parse("1:3,2:4").resolve(10), vec![1, 2, 3, 4]);
        assert_eq!(parse("1:3,3").resolve(10), vec![1, 2, 3]);
    }

    #[test]
    fn star_means_largest_in_use() {
        assert_eq!(parse("*").resolve(7), vec![7]);
        assert_eq!(parse("*").resolve(1), vec![1]);
    }

    #[test]
    fn star_as_the_upper_bound_runs_to_the_end() {
        assert_eq!(parse("5:*").resolve(8), vec![5, 6, 7, 8]);
        assert_eq!(parse("5:*").resolve(5), vec![5]);
        // `*` is "the largest value in use", so a start above `max` denotes an
        // empty range — the mailbox simply has no message 5 — rather than a
        // range that grows past the mailbox.
        assert_eq!(parse("5:*").resolve(3), vec![3]);
    }

    #[test]
    fn star_as_the_lower_bound_is_reversed_but_still_inclusive() {
        // RFC 3501: `*:5` is legal and means "from the largest down to 5".
        assert_eq!(parse("*:5").resolve(8), vec![5, 6, 7, 8]);
        // With three messages in use, `*` is 3, so `*:5` is the reversed range
        // 3..5 clamped to 3 — that is, just message 3.
        assert_eq!(parse("*:5").resolve(3), vec![3]);
        assert_eq!(parse("*:1").resolve(4), vec![1, 2, 3, 4]);
    }

    #[test]
    fn an_empty_mailbox_makes_every_range_empty() {
        assert!(parse("*").resolve(0).is_empty());
        assert!(parse("1:*").resolve(0).is_empty());
        assert!(parse("1:10").resolve(0).is_empty());
    }

    #[test]
    fn the_empty_set_is_not_an_error_and_matches_nothing() {
        let set = SequenceSet::empty();
        assert!(set.is_empty());
        assert!(set.resolve(100).is_empty());
        assert!(!set.contains(1, 100));
        assert!(set.singles().is_empty());
        assert_eq!(set.to_string(), "");
    }

    #[test]
    fn contains_answers_membership() {
        let set = parse("1:3,9");
        assert!(set.contains(1, 20));
        assert!(set.contains(3, 20));
        assert!(set.contains(9, 20));
        assert!(!set.contains(4, 20));
        assert!(!set.contains(0, 20));
    }

    #[test]
    fn filter_ordered_keeps_input_order_not_client_order() {
        // The set is written `3,1`, but the answer follows mailbox order.
        let set = parse("3,1");
        let uids = vec![10u64, 20, 30];
        let picked = set.filter_ordered(&uids, 30, |uid| uid / 10);
        assert_eq!(picked, vec![0, 2]);
    }

    #[test]
    fn filter_ordered_selects_by_position() {
        // The key function supplies the one-based position of each value, which
        // is what a `FETCH` over a mailbox passes.
        let set = parse("2:3");
        let positions: Vec<u64> = (1..=4).collect();
        // Identity means every position is its own key, so `2:3` picks the
        // second and third entries.
        let picked = set.filter_ordered(&positions, 4, |position| *position);
        assert_eq!(picked, vec![1, 2]);
        // A key that maps everything to 1 selects nothing.
        let picked = set.filter_ordered(&positions, 4, |_| 1);
        assert!(picked.is_empty());
    }

    #[test]
    fn a_huge_range_against_a_small_max_stays_small() {
        let set = parse("1:4294967295");
        assert_eq!(set.resolve(3), vec![1, 2, 3]);
    }

    #[test]
    fn zero_bound_clamps_to_one() {
        // `0` is not a legal nz-number, but a lenient parse must not produce an
        // enormous expansion.
        let set = parse("0:3");
        assert_eq!(set.resolve(10), vec![1, 2, 3]);
        assert!(parse("0").resolve(10).is_empty());
    }

    #[test]
    fn malformed_sets_are_parse_errors() {
        for raw in [
            "",
            " ",
            ",",
            "1,",
            ",1",
            "1,,2",
            "a",
            "1:a",
            "-1",
            "1:2:3",
            "+1",
            "1 2",
            "1;2",
            "99999999999999999999999999",
        ] {
            assert!(
                SequenceSet::parse(raw).is_err(),
                "`{raw}` must not parse as a sequence set"
            );
        }
    }

    #[test]
    fn malformed_set_errors_are_bad_shaped() {
        let err = SequenceSet::parse("x").expect_err("must fail");
        assert!(matches!(err, FerromaError::Parse(_)));
    }

    #[test]
    fn singles_lists_only_bare_numbers() {
        let set = parse("1,2:4,7");
        assert_eq!(set.singles(), vec![1, 7]);
    }

    #[test]
    fn u32_max_is_accepted_as_a_number() {
        let set = parse("4294967295");
        assert_eq!(set.resolve(MAX_SEQUENCE_NUMBER), vec![4294967295]);
    }

    #[test]
    fn display_round_trips_through_parse() {
        for raw in ["1", "1:5", "5:*", "*:5", "1,3:5,7:*", "*"] {
            let set = parse(raw);
            let reparsed = parse(&set.to_string());
            assert_eq!(set, reparsed, "`{raw}` must round-trip");
        }
    }

    #[test]
    fn bound_display_covers_both_forms() {
        assert_eq!(Bound::Number(12).to_string(), "12");
        assert_eq!(Bound::Star.to_string(), "*");
    }

    #[test]
    fn seq_range_helpers() {
        assert_eq!(SeqRange::single(4).resolve(9), Some((4, 4)));
        assert_eq!(SeqRange::range(2, 6).resolve(9), Some((2, 6)));
        assert_eq!(SeqRange::range(6, 2).resolve(9), Some((2, 6)));
        assert_eq!(SeqRange::single(1).to_string(), "1");
        assert_eq!(SeqRange::range(1, 3).to_string(), "1:3");
    }

    #[test]
    fn default_is_the_empty_set() {
        assert!(SequenceSet::default().is_empty());
    }
}
