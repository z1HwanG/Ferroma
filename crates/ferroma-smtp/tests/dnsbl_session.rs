//! The DNS block list, where it meets the SMTP session.
//!
//! # What this file can and cannot reach
//!
//! A test here connects over a loopback socket, and a loopback peer is **never** looked
//! up — that rule is the reason a deployment does not get its resolver blocked by its
//! own block list, and faking a public source address would mean either root-level
//! interface configuration or a lie in the test.
//!
//! So the two ends of the wiring are covered separately and honestly: the lookup and its
//! decisions exhaustively in `ferroma_smtp::dnsbl`'s unit tests, and here the pieces the
//! session itself owns — the reply a refused peer receives, the quarantine a listed peer
//! earns, and the fact that the transaction carries it only for the transaction it
//! belongs to.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use ferroma_smtp::dnsbl::{DnsblChecker, DnsblVerdict, SkipReason};
use ferroma_smtp::mx::{MockResolver, Resolver};
use ferroma_smtp::reply::Reply;
use ferroma_smtp::session::SmtpSession;

/// A configuration that lists one zone and quarantines.
fn config() -> ferroma_core::config::DnsblConfig {
    ferroma_core::config::DnsblConfig {
        enabled: true,
        zones: vec!["zen.example.test".to_string()],
        action: "quarantine".to_string(),
        allowlist: Vec::new(),
        // No caching, so each assertion below is one observable lookup.
        cache_ttl_secs: 0,
        cache_capacity: 16,
    }
}

#[test]
fn a_listed_peer_is_refused_with_a_permanent_reply() {
    let reply = Reply::block_listed("zen.example.test (127.0.0.2)");
    assert_eq!(reply.code(), 550, "a listing is not a temporary condition");
    let text = reply.to_string();
    assert!(text.contains("5.7.1"), "the enhanced code names the class: {text}");
    assert!(text.contains("zen.example.test"), "the reply names the zone: {text}");
}

#[tokio::test]
async fn the_public_lookup_decides_and_the_quarantine_follows_from_it() {
    let resolver = Arc::new(MockResolver::new().with_addresses(
        "34.216.184.93.zen.example.test",
        vec!["127.0.0.2".parse().unwrap()],
    ));
    let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &config());

    let verdict = checker.check("93.184.216.34".parse().unwrap()).await;
    assert!(verdict.is_listed(), "{verdict:?}");

    // The session-side half: a listed peer's transaction remembers why, and the
    // delivery that reads it puts the message in `Junk`.
    let mut session = SmtpSession::new("test-connection", "93.184.216.34:25".parse().unwrap());
    session.begin_transaction(None, None, false);
    assert!(session.transaction.block_listed_by.is_none());
    session.mark_block_listed(verdict.describe());
    assert_eq!(
        session.transaction.block_listed_by.as_deref(),
        Some("listed by zen.example.test (127.0.0.2)")
    );
}

/// The ordering that a first implementation of this got wrong.
#[test]
fn a_new_transaction_does_not_inherit_a_listing() {
    let mut session = SmtpSession::new("test-connection", "93.184.216.34:25".parse().unwrap());
    session.begin_transaction(None, None, false);
    session.mark_block_listed("listed by zen.example.test (127.0.0.2)".to_string());
    assert!(session.transaction.block_listed_by.is_some());

    // `begin_transaction` replaces the transaction wholesale. Marking before it would
    // set the flag on the object that is about to be discarded — which is exactly the
    // bug this asserts against, because the peer would then be quarantined on a later,
    // unrelated message from the same connection.
    session.begin_transaction(None, None, false);
    assert!(
        session.transaction.block_listed_by.is_none(),
        "a fresh transaction must not inherit the previous one's listing"
    );
}

#[tokio::test]
async fn a_loopback_peer_is_never_looked_up_however_the_zone_answers() {
    // The zone is configured to list the loopback address, which a real one would never
    // do. The point is that the question is not asked: the lookup count stays at zero.
    let resolver = Arc::new(MockResolver::new().with_addresses(
        "1.0.0.127.zen.example.test",
        vec!["127.0.0.2".parse().unwrap()],
    ));
    let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &config());

    assert_eq!(
        checker.check("127.0.0.1".parse().unwrap()).await,
        DnsblVerdict::Skipped(SkipReason::NotEligible)
    );
    assert_eq!(resolver.query_count(), 0, "a loopback peer costs no query");
}

#[test]
fn every_peer_a_test_can_connect_from_is_ineligible() {
    // The addresses a test harness ends up with: loopback, and the private ranges a
    // container gets. All of them are skipped by design, which is why this file asserts
    // the skip rather than pretending to exercise a listing over a socket.
    for address in ["127.0.0.1", "::1", "172.17.0.2", "192.168.65.1", "10.0.0.5"] {
        let ip: IpAddr = address.parse().unwrap();
        assert!(!ferroma_smtp::dnsbl::is_public(ip), "{address}");
    }
    let socket: SocketAddr = "127.0.0.1:25".parse().unwrap();
    assert_eq!(socket.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
}
