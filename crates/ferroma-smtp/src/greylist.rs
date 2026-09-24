//! Greylisting: defer a peer whose triplet has never been seen.
//!
//! The mechanism is one `451 4.7.1` and a row. A real MTA queues the message and
//! comes back; most bulk senders do not, which is the entire value. The costs are
//! a delay for legitimate first-time senders and one row per triplet.
//!
//! # What is never greylisted
//!
//! * an authenticated session — it is a known user, not an unknown peer;
//! * a null envelope sender (`<>`), because a bounce has nowhere to retry from
//!   and would be lost rather than deferred;
//! * a peer the operator listed in `whitelist`.
//!
//! # Failing open
//!
//! A database error accepts the message. Greylisting is an optimisation against
//! bulk senders, never a reason to refuse mail the platform could otherwise take:
//! turning a database blip into `451` for every peer would make Ferroma the
//! outage, not the filter.

use std::net::IpAddr;

use ferroma_core::config::GreylistConfig;
use ferroma_storage::repository::GreylistDecision;

/// Whether a peer address is exempted by the configured whitelist.
///
/// An entry is either one address (`203.0.113.7`) or a block in CIDR form
/// (`203.0.113.0/24`). An entry that parses as neither is ignored rather than
/// treated as a match: a typo in the configuration must not exempt the internet.
pub fn peer_is_whitelisted(peer: IpAddr, whitelist: &[String]) -> bool {
    whitelist.iter().any(|entry| entry_matches(entry, peer))
}

/// Whether one whitelist entry covers `peer`.
fn entry_matches(entry: &str, peer: IpAddr) -> bool {
    let entry = entry.trim();
    if entry.is_empty() {
        return false;
    }
    let Some((base, length)) = entry.split_once('/') else {
        return entry
            .parse::<IpAddr>()
            .map(|parsed| parsed == peer)
            .unwrap_or(false);
    };
    let (Ok(base), Ok(length)) = (base.trim().parse::<IpAddr>(), length.trim().parse::<u8>()) else {
        return false;
    };
    match (base, peer) {
        (IpAddr::V4(base), IpAddr::V4(peer)) if length <= 32 => {
            prefix_matches(&base.octets(), &peer.octets(), length)
        }
        (IpAddr::V6(base), IpAddr::V6(peer)) if length <= 128 => {
            prefix_matches(&base.octets(), &peer.octets(), length)
        }
        _ => false,
    }
}

/// Compare the leading `length` bits of two equal-length octet strings.
fn prefix_matches(base: &[u8], peer: &[u8], length: u8) -> bool {
    let whole = usize::from(length / 8);
    let spare = length % 8;
    if base[..whole] != peer[..whole] {
        return false;
    }
    if spare == 0 {
        return true;
    }
    let mask = 0xFFu8 << (8 - spare);
    base[whole] & mask == peer[whole] & mask
}

/// Why a message was not greylisted, for the structured log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GreylistSkip {
    /// The feature is off in the configuration.
    Disabled,
    /// The session authenticated, so it is a known user.
    Authenticated,
    /// A null envelope sender: a bounce cannot be deferred.
    NullSender,
    /// The operator exempted this peer.
    Whitelisted,
}

/// Whether this transaction is exempt, and why.
pub fn skip_reason(
    config: &GreylistConfig,
    authenticated: bool,
    sender: Option<&str>,
    peer: IpAddr,
) -> Option<GreylistSkip> {
    if !config.enabled {
        return Some(GreylistSkip::Disabled);
    }
    if authenticated {
        return Some(GreylistSkip::Authenticated);
    }
    // RFC 5321 §4.5.5: the null reverse-path is how a bounce is sent, and it has no
    // address to retry from. Deferring it would discard the report instead of
    // delaying it.
    if sender.map(str::trim).is_none_or(|sender| sender.is_empty() || sender == "<>") {
        return Some(GreylistSkip::NullSender);
    }
    if peer_is_whitelisted(peer, &config.whitelist) {
        return Some(GreylistSkip::Whitelisted);
    }
    None
}

/// The delay a newly seen triplet must wait, as a chrono duration.
pub fn delay(config: &GreylistConfig) -> chrono::Duration {
    chrono::Duration::seconds(i64::try_from(config.delay_secs).unwrap_or(i64::MAX))
}

/// Whether a decision lets the message through.
pub fn is_accept(decision: GreylistDecision) -> bool {
    matches!(decision, GreylistDecision::Accept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(raw: &str) -> IpAddr {
        raw.parse().expect("a test address")
    }

    fn whitelist(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    #[test]
    fn a_bare_address_matches_exactly() {
        let list = whitelist(&["203.0.113.7"]);
        assert!(peer_is_whitelisted(ip("203.0.113.7"), &list));
        assert!(!peer_is_whitelisted(ip("203.0.113.8"), &list));
        assert!(!peer_is_whitelisted(ip("203.0.113.7"), &whitelist(&["198.51.100.7"])));
    }

    #[test]
    fn a_block_covers_its_range_and_stops_at_its_edge() {
        let list = whitelist(&["203.0.113.0/24"]);
        assert!(peer_is_whitelisted(ip("203.0.113.0"), &list));
        assert!(peer_is_whitelisted(ip("203.0.113.255"), &list));
        assert!(!peer_is_whitelisted(ip("203.0.114.0"), &list));
    }

    #[test]
    fn a_partial_octet_block_masks_the_right_bits() {
        let list = whitelist(&["203.0.113.128/25"]);
        assert!(peer_is_whitelisted(ip("203.0.113.200"), &list));
        assert!(!peer_is_whitelisted(ip("203.0.113.127"), &list));
    }

    #[test]
    fn ipv6_blocks_are_matched_too() {
        let list = whitelist(&["2001:db8::/32"]);
        assert!(peer_is_whitelisted(ip("2001:db8::1"), &list));
        assert!(!peer_is_whitelisted(ip("2001:db9::1"), &list));
    }

    #[test]
    fn a_nonsense_entry_matches_nothing_rather_than_everything() {
        let list = whitelist(&["", "not-an-address", "203.0.113.0/99", "2001:db8::/200"]);
        assert!(!peer_is_whitelisted(ip("203.0.113.1"), &list));
        assert!(!peer_is_whitelisted(ip("2001:db8::1"), &list));
    }

    #[test]
    fn a_family_mismatch_never_matches() {
        let list = whitelist(&["203.0.113.0/24"]);
        assert!(!peer_is_whitelisted(ip("2001:db8::1"), &list));
    }

    #[test]
    fn every_exemption_names_itself() {
        let enabled = GreylistConfig { enabled: true, ..GreylistConfig::default() };
        let off = GreylistConfig::default();
        assert_eq!(skip_reason(&off, false, Some("a@b.c"), ip("203.0.113.1")), Some(GreylistSkip::Disabled));
        assert_eq!(skip_reason(&enabled, true, Some("a@b.c"), ip("203.0.113.1")), Some(GreylistSkip::Authenticated));
        assert_eq!(skip_reason(&enabled, false, None, ip("203.0.113.1")), Some(GreylistSkip::NullSender));
        assert_eq!(skip_reason(&enabled, false, Some("<>"), ip("203.0.113.1")), Some(GreylistSkip::NullSender));
        assert_eq!(skip_reason(&enabled, false, Some("  "), ip("203.0.113.1")), Some(GreylistSkip::NullSender));
        let listing = GreylistConfig { whitelist: whitelist(&["203.0.113.1"]), ..enabled.clone() };
        assert_eq!(skip_reason(&listing, false, Some("a@b.c"), ip("203.0.113.1")), Some(GreylistSkip::Whitelisted));
        assert_eq!(skip_reason(&enabled, false, Some("a@b.c"), ip("203.0.113.1")), None);
    }

    #[test]
    fn the_configured_delay_is_the_one_used() {
        let config = GreylistConfig { delay_secs: 90, ..GreylistConfig::default() };
        assert_eq!(delay(&config), chrono::Duration::seconds(90));
    }
}
