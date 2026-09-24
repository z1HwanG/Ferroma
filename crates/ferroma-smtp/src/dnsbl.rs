//! DNS block lists: asking a third party what it thinks of a peer's address.
//!
//! The mechanism is old and simple — reverse the address, append the zone, and an `A`
//! record in `127.0.0.0/8` means "listed" — and the ways to get it wrong are what this
//! module is careful about.
//!
//! * **Private addresses are never queried.** RFC 5782 §2.4 says so, and every public
//!   zone's terms say so more forcefully: querying about `192.0.2.x` or a loopback
//!   address is how a host gets its DNS blocked. A private peer is `Skipped`, not
//!   `NotListed`, because the difference matters in a log.
//! * **Anything other than a listing is not a listing.** `NXDOMAIN`, a timeout, a
//!   resolver failure, a `SERVFAIL` — all of them mean "no answer", and mail is never
//!   refused over an answer nobody gave. This is the only safe direction: a block list
//!   that is down must not become an outage here.
//! * **An answer is believed only from the zone that was asked.** The response must be
//!   an address in `127.0.0.0/8`; anything else — a CNAME chain out of the zone, an
//!   address someone published by accident — is not a listing.
//!
//! # The allowlist is why this is usable
//!
//! Block lists are shared, and shared lists list whole hosting ranges when they mean to
//! list one sender. The allowlist is the escape hatch for a relay or a partner, and it
//! is consulted before any query is made.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroma_core::config::DnsblConfig;

use crate::mx::Resolver;

/// What a check concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsblVerdict {
    /// The peer is on one of the configured lists.
    Listed {
        /// The zone that listed it.
        zone: String,
        /// The codes the zone returned, which name the reason (`127.0.0.2` is a
        /// spam source, `127.0.0.4` is a policy block, and so on, per zone).
        codes: Vec<IpAddr>,
    },
    /// Every configured zone was consulted and none listed the peer.
    Clear,
    /// The check did not happen, and the reason.
    Skipped(SkipReason),
}

/// Why a peer was not looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The feature is off.
    Disabled,
    /// No zones are configured.
    NoZones,
    /// The address is private, loopback, link-local or otherwise not routable, or the
    /// operator exempted it.
    NotEligible,
    /// The lookups all failed. Not a listing, and not a reason to refuse mail.
    Unavailable,
}

impl DnsblVerdict {
    /// Whether this is a listing.
    pub fn is_listed(&self) -> bool {
        matches!(self, DnsblVerdict::Listed { .. })
    }

    /// One line for a structured log.
    pub fn describe(&self) -> String {
        match self {
            DnsblVerdict::Listed { zone, codes } => {
                let codes: Vec<String> = codes.iter().map(ToString::to_string).collect();
                format!("listed by {zone} ({})", codes.join(", "))
            }
            DnsblVerdict::Clear => "clear".to_string(),
            DnsblVerdict::Skipped(SkipReason::Disabled) => "skipped: disabled".to_string(),
            DnsblVerdict::Skipped(SkipReason::NoZones) => "skipped: no zones configured".to_string(),
            DnsblVerdict::Skipped(SkipReason::NotEligible) => {
                "skipped: address is not eligible".to_string()
            }
            DnsblVerdict::Skipped(SkipReason::Unavailable) => {
                "skipped: the zone did not answer".to_string()
            }
        }
    }
}

/// One cached verdict.
#[derive(Debug, Clone)]
struct CacheEntry {
    verdict: DnsblVerdict,
    stored_at: Instant,
}

/// The checker: a resolver, a configuration, and a small cache.
pub struct DnsblChecker {
    resolver: Arc<dyn Resolver>,
    config: DnsblConfig,
    cache: Mutex<HashMap<IpAddr, CacheEntry>>,
}

impl std::fmt::Debug for DnsblChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DnsblChecker")
            .field("zones", &self.config.zones)
            .field("action", &self.config.action)
            .finish_non_exhaustive()
    }
}

impl DnsblChecker {
    /// Build a checker over `resolver`.
    pub fn new(resolver: Arc<dyn Resolver>, config: &DnsblConfig) -> Self {
        DnsblChecker {
            resolver,
            config: config.clone(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The configuration in use.
    pub fn config(&self) -> &DnsblConfig {
        &self.config
    }

    /// Whether a listed peer's mail is refused rather than quarantined.
    pub fn rejects(&self) -> bool {
        self.config.rejects()
    }

    /// Check one peer address.
    pub async fn check(&self, ip: IpAddr) -> DnsblVerdict {
        if let Some(reason) = self.skip_reason(ip) {
            return DnsblVerdict::Skipped(reason);
        }
        if let Some(verdict) = self.cached(ip) {
            return verdict;
        }

        let mut answered_any = false;
        let mut verdict = DnsblVerdict::Clear;
        for zone in &self.config.zones {
            let name = query_name(ip, zone);
            match self.resolver.addresses(&name).await {
                Ok(addresses) => {
                    answered_any = true;
                    // Only `127.0.0.0/8` is a listing. A response outside it is a name
                    // that resolves to something else entirely — a wildcard or a
                    // misconfigured zone — and believing it would refuse mail over
                    // nothing.
                    let codes: Vec<IpAddr> = addresses
                        .into_iter()
                        .filter(|address| match address {
                            IpAddr::V4(v4) => v4.octets()[0] == 127,
                            IpAddr::V6(_) => false,
                        })
                        .collect();
                    if !codes.is_empty() {
                        verdict = DnsblVerdict::Listed {
                            zone: zone.clone(),
                            codes,
                        };
                        break;
                    }
                }
                Err(error) => {
                    // A failed lookup is named in the log and then ignored. Refusing mail
                    // because a block list is unreachable would make every zone a
                    // single point of failure for the whole deployment.
                    tracing::warn!(zone = %zone, name = %name, %error, "a DNSBL lookup failed");
                }
            }
        }

        if !answered_any && verdict == DnsblVerdict::Clear {
            verdict = DnsblVerdict::Skipped(SkipReason::Unavailable);
        }
        self.remember(ip, verdict.clone());
        verdict
    }

    /// Why this address is not worth querying, if it is not.
    fn skip_reason(&self, ip: IpAddr) -> Option<SkipReason> {
        if !self.config.enabled {
            return Some(SkipReason::Disabled);
        }
        if self.config.zones.is_empty() {
            return Some(SkipReason::NoZones);
        }
        if !is_public(ip) || crate::greylist::peer_is_whitelisted(ip, &self.config.allowlist) {
            return Some(SkipReason::NotEligible);
        }
        None
    }

    /// A verdict from the cache, if one is still fresh.
    fn cached(&self, ip: IpAddr) -> Option<DnsblVerdict> {
        if self.config.cache_ttl_secs == 0 {
            return None;
        }
        let ttl = Duration::from_secs(self.config.cache_ttl_secs);
        let cache = self.cache.lock().ok()?;
        let entry = cache.get(&ip)?;
        if entry.stored_at.elapsed() < ttl {
            Some(entry.verdict.clone())
        } else {
            None
        }
    }

    /// Store a verdict, dropping expired entries first when the map is full.
    fn remember(&self, ip: IpAddr, verdict: DnsblVerdict) {
        if self.config.cache_ttl_secs == 0 {
            return;
        }
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        if cache.len() >= self.config.cache_capacity {
            let ttl = Duration::from_secs(self.config.cache_ttl_secs);
            cache.retain(|_, entry| entry.stored_at.elapsed() < ttl);
            // Still full: everything left is fresh. Dropping the whole map is a coarser
            // eviction than a real LRU and costs at most one round of lookups, which is
            // the honest trade for a cache nobody should ever fill.
            if cache.len() >= self.config.cache_capacity {
                cache.clear();
            }
        }
        cache.insert(
            ip,
            CacheEntry {
                verdict,
                stored_at: Instant::now(),
            },
        );
    }
}

/// The name to query for `ip` in `zone` (RFC 5782 §2.1, §2.2).
///
/// IPv4 reverses the four octets; IPv6 reverses the thirty-two nibbles. Both are
/// lowercase, because a DNS name is case-insensitive and a mixed-case query is a
/// needless difference in a cache key.
pub fn query_name(ip: IpAddr, zone: &str) -> String {
    let zone = zone.trim().trim_end_matches('.').to_ascii_lowercase();
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            format!(
                "{}.{}.{}.{}.{zone}",
                octets[3], octets[2], octets[1], octets[0]
            )
        }
        IpAddr::V6(v6) => {
            let mut labels: Vec<String> = Vec::with_capacity(32);
            for byte in v6.octets().iter().rev() {
                labels.push(format!("{:x}", byte & 0x0F));
                labels.push(format!("{:x}", byte >> 4));
            }
            format!("{}.{zone}", labels.join("."))
        }
    }
}

/// Whether an address is one a public block list can be asked about.
///
/// Everything that is not globally routable is excluded, which covers the ranges RFC
/// 5782 §2.4 names and the ones that came after it: loopback, the three private IPv4
/// ranges, link-local, carrier-grade NAT, benchmarking, documentation, multicast,
/// broadcast, unspecified, unique-local IPv6 and IPv6 link-local.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || v4.is_unspecified()
            {
                return false;
            }
            // 100.64.0.0/10 carrier-grade NAT, which `is_private` does not cover.
            if octets[0] == 100 && (64..128).contains(&octets[1]) {
                return false;
            }
            // 192.0.0.0/24 IETF protocol assignments and 198.18.0.0/15 benchmarking.
            if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
                return false;
            }
            if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
                return false;
            }
            // 240.0.0.0/4 reserved, and the IPv4-mapped IPv6 range is handled by the
            // caller passing the mapped address through as IPv4.
            if octets[0] >= 240 {
                return false;
            }
            true
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(mapped));
            }
            // `is_unique_local` covers fc00::/7 and `is_unicast_link_local` fe80::/10.
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mx::MockResolver;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn config() -> DnsblConfig {
        DnsblConfig {
            enabled: true,
            zones: vec!["zen.example.test".to_string()],
            action: "quarantine".to_string(),
            allowlist: Vec::new(),
            cache_ttl_secs: 900,
            cache_capacity: 16,
        }
    }

    #[test]
    fn an_ipv4_address_is_reversed_and_suffixed() {
        assert_eq!(
            query_name("93.184.216.34".parse().unwrap(), "zen.example.test"),
            "34.216.184.93.zen.example.test"
        );
        // A trailing dot and mixed case are normalised, because two spellings of one
        // zone are two cache keys.
        assert_eq!(
            query_name("93.184.216.34".parse().unwrap(), "ZEN.Example.Test."),
            "34.216.184.93.zen.example.test"
        );
    }

    #[test]
    fn an_ipv6_address_is_nibble_reversed() {
        let name = query_name(
            "2001:db8::1".parse::<Ipv6Addr>().unwrap().into(),
            "zen.example.test",
        );
        // Thirty-two nibbles, least significant first: the address ends in ...::1, so
        // the name starts with that nibble.
        assert!(name.starts_with("1.0.0.0."), "{name}");
        assert!(name.ends_with(".zen.example.test"), "{name}");
        assert_eq!(name.split('.').count(), 32 + 3, "{name}");
    }

    #[test]
    fn private_and_reserved_addresses_are_not_eligible() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.10.10",
            "100.64.0.1",
            "192.0.0.1",
            "198.18.0.1",
            "240.0.0.1",
            // Documentation ranges: not routable, so a block list could never say
            // anything true about them.
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:10.0.0.1",
        ] {
            let ip: IpAddr = private.parse().unwrap();
            assert!(!is_public(ip), "{private} must not be queried");
        }
        for public in ["93.184.216.34", "8.8.8.8", "1.1.1.1", "2001:4860::1", "::ffff:8.8.8.8"] {
            let ip: IpAddr = public.parse().unwrap();
            assert!(is_public(ip), "{public} is routable");
        }
    }

    #[tokio::test]
    async fn a_127_answer_is_a_listing() {
        let resolver = Arc::new(MockResolver::new().with_addresses(
            "34.216.184.93.zen.example.test",
            vec!["127.0.0.2".parse().unwrap()],
        ));
        let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &config());
        let verdict = checker.check("93.184.216.34".parse().unwrap()).await;
        match &verdict {
            DnsblVerdict::Listed { zone, codes } => {
                assert_eq!(zone, "zen.example.test");
                assert_eq!(codes, &vec!["127.0.0.2".parse::<IpAddr>().unwrap()]);
            }
            other => panic!("expected a listing, got {other:?}"),
        }
        assert!(verdict.is_listed());
        assert_eq!(resolver.queries(), vec!["a:34.216.184.93.zen.example.test"]);
    }

    #[tokio::test]
    async fn no_answer_is_not_a_listing() {
        // NXDOMAIN, which a resolver reports as an empty answer.
        let resolver = Arc::new(MockResolver::new());
        let checker = DnsblChecker::new(resolver as Arc<dyn Resolver>, &config());
        assert_eq!(
            checker.check("93.184.216.34".parse().unwrap()).await,
            DnsblVerdict::Clear
        );
    }

    #[tokio::test]
    async fn a_failing_zone_is_not_a_listing() {
        // The zone times out. Refusing mail here would make every block list an outage.
        let resolver = Arc::new(MockResolver::new().with_failure("34.216.184.93.zen.example.test"));
        let checker = DnsblChecker::new(resolver as Arc<dyn Resolver>, &config());
        assert_eq!(
            checker.check("93.184.216.34".parse().unwrap()).await,
            DnsblVerdict::Skipped(SkipReason::Unavailable)
        );
    }

    #[tokio::test]
    async fn an_answer_outside_the_loopback_range_is_not_a_listing() {
        // A wildcard or a misconfigured zone answers with a real address. Believing it
        // would refuse mail over nothing at all.
        let resolver = Arc::new(MockResolver::new().with_addresses(
            "34.216.184.93.zen.example.test",
            vec!["198.51.100.7".parse().unwrap(), "::1".parse().unwrap()],
        ));
        let checker = DnsblChecker::new(resolver as Arc<dyn Resolver>, &config());
        assert_eq!(
            checker.check("93.184.216.34".parse().unwrap()).await,
            DnsblVerdict::Clear
        );
    }

    #[tokio::test]
    async fn the_second_listing_zone_is_not_asked_after_the_first_hits() {
        let mut many = config();
        many.zones = vec!["first.example.test".into(), "second.example.test".into()];
        let resolver = Arc::new(MockResolver::new().with_addresses(
            "34.216.184.93.first.example.test",
            vec!["127.0.0.4".parse().unwrap()],
        ));
        let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &many);
        assert!(checker.check("93.184.216.34".parse().unwrap()).await.is_listed());
        assert_eq!(resolver.query_count(), 1, "the first listing decides");
    }

    #[tokio::test]
    async fn a_private_peer_is_never_queried() {
        let resolver = Arc::new(MockResolver::new());
        let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &config());
        assert_eq!(
            checker.check("192.168.1.5".parse().unwrap()).await,
            DnsblVerdict::Skipped(SkipReason::NotEligible)
        );
        assert_eq!(resolver.query_count(), 0, "a private address costs no query");
        // The skip reason is distinct from "clear": an operator reading logs has to be
        // able to tell "not listed" from "not asked".
        assert_ne!(
            DnsblVerdict::Skipped(SkipReason::NotEligible),
            DnsblVerdict::Clear
        );
    }

    #[tokio::test]
    async fn an_allowlisted_peer_is_never_queried() {
        let mut listed = config();
        listed.allowlist = vec!["93.184.216.0/24".to_string()];
        let resolver = Arc::new(MockResolver::new().with_addresses(
            "34.216.184.93.zen.example.test",
            vec!["127.0.0.2".parse().unwrap()],
        ));
        let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &listed);
        assert_eq!(
            checker.check("93.184.216.34".parse().unwrap()).await,
            DnsblVerdict::Skipped(SkipReason::NotEligible)
        );
        assert_eq!(resolver.query_count(), 0);
    }

    #[tokio::test]
    async fn a_verdict_is_cached_for_its_ttl() {
        let resolver = Arc::new(MockResolver::new().with_addresses(
            "34.216.184.93.zen.example.test",
            vec!["127.0.0.2".parse().unwrap()],
        ));
        let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &config());
        for _ in 0..5 {
            checker.check("93.184.216.34".parse().unwrap()).await;
        }
        assert_eq!(resolver.query_count(), 1, "one lookup for five connections");

        // A zero TTL means every check is a query, which is what the tests above rely on.
        let mut uncached = config();
        uncached.cache_ttl_secs = 0;
        let fresh_resolver = Arc::new(MockResolver::new().with_addresses(
            "34.216.184.93.zen.example.test",
            vec!["127.0.0.2".parse().unwrap()],
        ));
        let uncached_checker =
            DnsblChecker::new(Arc::clone(&fresh_resolver) as Arc<dyn Resolver>, &uncached);
        for _ in 0..3 {
            uncached_checker.check("93.184.216.34".parse().unwrap()).await;
        }
        assert_eq!(fresh_resolver.query_count(), 3);
    }

    #[tokio::test]
    async fn the_cache_does_not_grow_past_its_capacity() {
        let mut small = config();
        small.cache_capacity = 4;
        let checker = DnsblChecker::new(Arc::new(MockResolver::new()) as Arc<dyn Resolver>, &small);
        for last in 1..=20u8 {
            let ip: IpAddr = format!("93.184.216.{last}").parse().unwrap();
            checker.check(ip).await;
        }
        let cache = checker.cache.lock().unwrap();
        assert!(cache.len() <= 4, "cache grew to {}", cache.len());
    }

    #[tokio::test]
    async fn a_disabled_or_zoneless_checker_asks_nothing() {
        let resolver = Arc::new(MockResolver::new());
        let mut off = config();
        off.enabled = false;
        let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &off);
        assert_eq!(
            checker.check("93.184.216.34".parse().unwrap()).await,
            DnsblVerdict::Skipped(SkipReason::Disabled)
        );

        let mut zoneless = config();
        zoneless.zones.clear();
        let checker = DnsblChecker::new(Arc::clone(&resolver) as Arc<dyn Resolver>, &zoneless);
        assert_eq!(
            checker.check("93.184.216.34".parse().unwrap()).await,
            DnsblVerdict::Skipped(SkipReason::NoZones)
        );
        assert_eq!(resolver.query_count(), 0);
    }

    #[test]
    fn the_action_decides_between_quarantine_and_refusal() {
        let mut config = config();
        assert!(!config.rejects(), "quarantine is the default");
        config.action = "REJECT".into();
        assert!(config.rejects(), "the action is matched case-insensitively");
    }

    #[test]
    fn a_configuration_that_cannot_work_is_refused() {
        let mut enabled_without_zones = config();
        enabled_without_zones.zones.clear();
        assert!(enabled_without_zones.validate().is_err());

        let mut bad_action = config();
        bad_action.action = "drop".into();
        assert!(bad_action.validate().is_err());

        let mut blank_zone = config();
        blank_zone.zones = vec!["  ".into()];
        assert!(blank_zone.validate().is_err());

        // Disabled is always valid: nothing is looked up, so nothing can be wrong.
        let off = DnsblConfig::default();
        assert!(off.validate().is_ok());
        assert!(config().validate().is_ok());
    }

    #[test]
    fn a_reserved_ipv4_range_is_recognised_by_its_octets() {
        // These are the ranges `Ipv4Addr`'s own predicates do not cover, and the ones a
        // transcription error in the list above would silently allow.
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 255))));
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(192, 0, 0, 255))));
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(198, 19, 255, 255))));
        assert!(is_public(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        assert!(is_public(IpAddr::V4(Ipv4Addr::new(198, 20, 0, 1))));
        // TEST-NET-1 is documentation, not private, and still not routable — the
        // distinction matters because a block list cannot be asked about it either.
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))));
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(192, 0, 0, 1))), "192.0.0.0/24 is reserved");
    }
}
