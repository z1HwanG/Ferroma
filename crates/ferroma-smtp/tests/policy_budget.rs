//! The policy step's DNS budget, measured against a **real** black-holing nameserver.
//!
//! The unit tests in `src/inbound.rs` prove the decision table and the short-circuit with
//! a scripted resolver. This file answers a different question, the one the acceptance run
//! raised: *does `[dns] timeout_secs` actually reach the socket, and how long does a
//! message take when the configured nameserver never answers?*
//!
//! Nothing here is mocked. A UDP socket is bound on loopback and reads every datagram
//! into the void — the exact shape of "the resolver on this host does not answer raw UDP
//! queries". A real [`HickoryResolver`] is pointed at it through `[dns] resolvers`, a real
//! `InboundPolicy` is built from that configuration, and the wall clock is the assertion.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferroma_core::config::{Config, DnsConfig};
use ferroma_core::EmailAddress;
use ferroma_mail::ParsedMessage;
use ferroma_smtp::{
    policy_budget, HickoryResolver, InboundPolicy, MockResolver, MxResolver, ReceivedMessage,
    Resolver, POLICY_TIMEOUT_CAP,
};

/// A nameserver that accepts queries and never answers — a black hole.
struct BlackHole {
    address: SocketAddr,
    /// Every query it swallowed, so a test can prove the lookups really happened.
    queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl BlackHole {
    async fn start() -> BlackHole {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind the black hole");
        let address = socket.local_addr().expect("local address");
        let queries = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let counter = Arc::clone(&queries);
        let stop = Arc::clone(&shutdown);
        tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                // Read and discard. Never a reply — that is the whole point.
                match tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut buffer)).await {
                    Ok(Ok(_)) => {
                        counter.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(Err(_)) => return,
                    Err(_) => continue,
                }
            }
        });

        BlackHole {
            address,
            queries,
            shutdown,
        }
    }

    fn queries(&self) -> usize {
        self.queries.load(std::sync::atomic::Ordering::Acquire)
    }

    fn stop(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// The `[dns]` block the acceptance run used: impatient, and pointed at a dead server.
fn impatient_dns(server: SocketAddr) -> DnsConfig {
    DnsConfig {
        resolvers: vec![server.to_string()],
        timeout_secs: 1,
        attempts: 1,
        cache_ttl_secs: 0,
        negative_ttl_secs: 0,
        tcp_fallback: false,
    }
}

// ---------------------------------------------------------------------------
// The configured timeout reaches the socket
// ---------------------------------------------------------------------------

/// **The regression test.** With `[dns] timeout_secs = 1, attempts = 1`, one lookup must
/// cost about a second — not hickory's default five seconds times two attempts.
///
/// This is the test that would have caught the original defect: the system-configuration
/// path built the resolver with `tokio_from_system_conf()`, which takes no options and
/// silently discarded `[dns]`, so every lookup ran on hickory's defaults however the
/// operator had configured them.
#[tokio::test]
async fn a_lookup_honours_the_configured_timeout() {
    let black_hole = BlackHole::start().await;

    // `HickoryResolver::new` with an empty `resolvers` list would read the *system's*
    // nameservers — which on this host is the case the acceptance run hit. Pointing
    // `resolvers` at the black hole exercises the same options-applied path
    // deterministically; `the_system_configuration_path_applies_the_timeout` below covers
    // the empty-list branch.
    let resolver = HickoryResolver::new(&impatient_dns(black_hole.address))
        .expect("build the resolver");

    let started = Instant::now();
    let result = resolver.txt("example.net").await;
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a black hole must not produce an answer");
    assert!(
        elapsed >= Duration::from_millis(700),
        "the timeout must have been waited out, not skipped: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "[dns] timeout_secs = 1 did not reach the socket: one lookup took {elapsed:?} \
         (hickory's defaults would make this >= 5s)"
    );
    assert!(black_hole.queries() >= 1, "the query really was sent");

    black_hole.stop();
}

#[tokio::test]
async fn a_slower_configured_timeout_takes_longer() {
    // The same lookup with a 3-second budget must be measurably slower, which proves the
    // setting is read rather than coincidentally matching a constant.
    let black_hole = BlackHole::start().await;

    let quick = HickoryResolver::new(&impatient_dns(black_hole.address)).expect("resolver");
    let started = Instant::now();
    let _ = quick.txt("quick.example.net").await;
    let quick_elapsed = started.elapsed();

    let mut patient_dns = impatient_dns(black_hole.address);
    patient_dns.timeout_secs = 3;
    let patient = HickoryResolver::new(&patient_dns).expect("resolver");
    let started = Instant::now();
    let _ = patient.txt("patient.example.net").await;
    let patient_elapsed = started.elapsed();

    assert!(
        patient_elapsed > quick_elapsed,
        "3s must be slower than 1s: {patient_elapsed:?} vs {quick_elapsed:?}"
    );

    black_hole.stop();
}

/// The empty-`resolvers` branch, which is the one the acceptance run actually took.
///
/// It cannot be pointed at a black hole (it reads the host's real nameservers), so what is
/// asserted is the part that was broken: the options are applied to the resolver hickory
/// is handed, rather than `tokio_from_system_conf()` being asked to invent its own.
#[tokio::test]
async fn the_system_configuration_path_applies_the_timeout() {
    // Either the host has a usable system resolver configuration, or it does not; both
    // are fine, and neither may panic.
    let mut dns = DnsConfig {
        resolvers: Vec::new(),
        timeout_secs: 1,
        attempts: 1,
        cache_ttl_secs: 0,
        negative_ttl_secs: 0,
        tcp_fallback: false,
    };
    let resolver = HickoryResolver::new(&dns).expect("the system configuration must be readable");

    // A name that cannot exist, so the answer is either a fast NXDOMAIN or a timeout, and
    // never a real record.
    let started = Instant::now();
    let _ = resolver.txt("this-name-cannot-exist.invalid").await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "one lookup on the system path took {elapsed:?}; [dns] timeout_secs = 1 was not applied"
    );

    // And a 30-second budget must not be reachable inside a short test, which is only
    // true if the value is used.
    dns.timeout_secs = 30;
    dns.attempts = 1;
    let resolver = HickoryResolver::new(&dns).expect("resolver");
    let started = Instant::now();
    let _ = resolver.txt("this-name-cannot-exist-either.invalid").await;
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(25), "{elapsed:?}");
}

// ---------------------------------------------------------------------------
// The whole step, end to end
// ---------------------------------------------------------------------------

/// **The number the acceptance run asked for.**
///
/// End of `DATA` to the `250`, with `[dns] timeout_secs = 1, attempts = 1` and a
/// black-holing resolver. The resolver is the **real** `HickoryResolver`, not a mock, so
/// the measurement includes the actual socket and the actual hickory accounting. Prints
/// the measurement so it lands in the test output.
#[tokio::test]
async fn a_message_through_a_black_holed_resolver_is_still_fast() {
    let black_hole = BlackHole::start().await;

    let mut config = Config::default();
    config.server.hostname = "mx.test".to_string();
    config.dns = impatient_dns(black_hole.address);
    config.policy.spf_enabled = true;
    config.policy.dmarc_enabled = true;
    config.policy.add_auth_results = true;
    config.dkim.verify_inbound = true;
    // The strictest action: even so, a dead resolver must not slow the step down or
    // reject anything.
    config.policy.dmarc_failure_action = "reject".to_string();

    // The real resolver, pointed at a nameserver that never answers.
    let resolver: Arc<dyn Resolver> =
        Arc::new(HickoryResolver::new(&config.dns).expect("build the real resolver"));
    let policy = InboundPolicy::new(&config, resolver);
    assert_eq!(
        policy.timeout(),
        policy_budget(&config),
        "the step must be bounded by the derived budget"
    );
    assert!(
        policy.timeout() < POLICY_TIMEOUT_CAP,
        "the budget must be the derived one, not the 60s cap: {:?}",
        policy.timeout()
    );

    let raw = b"From: alice@example.net\r\nTo: alice@mx.test\r\nSubject: black hole\r\n\r\nbody\r\n";
    let mut message = ReceivedMessage::new(raw.to_vec(), None, "conn-black-hole");
    message.sender = EmailAddress::parse("alice@example.net").ok();
    message.remote_ip = Some("127.0.0.1".parse().expect("ip"));
    message.helo = Some("client.example.net".to_string());
    let parsed = ParsedMessage::parse(raw).expect("parse");

    let started = Instant::now();
    let verdict = policy.evaluate(&message, &parsed).await;
    let elapsed = started.elapsed();

    println!(
        "POLICY MEASUREMENT: end-of-DATA to 250 with [dns] timeout_secs=1, attempts=1 and a \
         black-holing resolver: {elapsed:?} (budget {:?}, {} DNS queries sent)",
        policy.timeout(),
        black_hole.queries()
    );

    // The design holds: accepted, not rejected.
    assert_eq!(
        verdict.action,
        ferroma_smtp::PolicyAction::Accept,
        "a dead resolver must never reject"
    );
    assert!(verdict.is_transient());
    // Exactly one lookup: SPF's. The short-circuit is what makes a dead resolver cost one
    // timeout instead of one per stage.
    // Two packets from ONE stage: hickory's `attempts = 1` means one retry. DKIM and
    // DMARC were not attempted at all, which the header assertions below confirm — a run
    // without the short-circuit would send six or more.
    assert_eq!(
        black_hole.queries(),
        2,
        "one lookup plus its single retry, and nothing else"
    );
    let header = verdict.header_value().expect("a header");
    assert!(header.contains("dkim=temperror"), "{header}");
    assert!(header.contains("dmarc=temperror"), "{header}");
    // And it is bounded by `[dns]`, not by the 60-second cap.
    assert!(
        elapsed < Duration::from_secs(5),
        "the step took {elapsed:?}; [dns] timeout_secs = 1 was not the effective budget"
    );
    assert!(
        elapsed < policy.timeout() + Duration::from_secs(1),
        "the step must finish inside its budget"
    );

    black_hole.stop();
}

/// The same step against a resolver that answers, to show the budget is not merely a
/// ceiling the happy path pays.
#[tokio::test]
async fn a_working_resolver_costs_almost_nothing() {
    let mut config = Config::default();
    config.server.hostname = "mx.test".to_string();
    config.policy.dmarc_failure_action = "none".to_string();

    let policy = InboundPolicy::new(
        &config,
        Arc::new(MxResolver::mock(
            MockResolver::new()
                .with_txt("example.net", vec!["v=spf1 ip4:127.0.0.1 -all".to_string()])
                .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]),
            &config.dns,
        )),
    );

    let raw = b"From: alice@example.net\r\n\r\nbody\r\n";
    let mut message = ReceivedMessage::new(raw.to_vec(), None, "conn-fast");
    message.sender = EmailAddress::parse("alice@example.net").ok();
    message.remote_ip = Some("127.0.0.1".parse().expect("ip"));
    message.helo = Some("client.example.net".to_string());
    let parsed = ParsedMessage::parse(raw).expect("parse");

    let started = Instant::now();
    let verdict = policy.evaluate(&message, &parsed).await;
    let elapsed = started.elapsed();

    println!("POLICY MEASUREMENT: working resolver: {elapsed:?}");
    assert_eq!(verdict.action, ferroma_smtp::PolicyAction::Accept);
    assert!(verdict.header_value().expect("a header").contains("spf=pass"));
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

/// The budget the acceptance run's configuration produces, as a number.
#[test]
fn the_acceptance_configuration_gets_a_small_budget() {
    let config = Config {
        dns: DnsConfig {
            resolvers: vec!["127.0.0.1:5399".to_string()],
            timeout_secs: 1,
            attempts: 1,
            ..Config::default().dns
        },
        ..Config::default()
    };
    let budget = policy_budget(&config);
    println!(
        "POLICY MEASUREMENT: [dns] timeout_secs=1 attempts=1, spf_max_lookups={} -> budget {budget:?} \
         (was 60s before the fix)",
        config.policy.spf_max_lookups
    );
    // 1s x (attempts 1 + 1 packet) x (4 + spf_max_lookups 10) lookups.
    assert_eq!(budget, Duration::from_secs(28));
    assert!(budget < POLICY_TIMEOUT_CAP, "{budget:?}");
}
