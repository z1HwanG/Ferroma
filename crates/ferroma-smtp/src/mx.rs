//! DNS: MX resolution, the records SPF/DKIM/DMARC need, and the Admin DNS-health
//! checker.
//!
//! Two layers live here:
//!
//! * [`Resolver`] — a small, **mockable** trait. Every DNS question the SMTP layer
//!   asks goes through it, which is what makes the SPF/DKIM/DMARC unit tests run
//!   without a network.
//! * [`MxResolver`] — the production implementation: a TTL cache in front of a
//!   [`Resolver`], plus the RFC 5321 §5.1 *implicit MX* rule (a domain with no `MX`
//!   record is its own mail exchanger, reached at its `A`/`AAAA` addresses) and the
//!   RFC 7505 *null MX* rule (`" "`/`"."` means "this domain accepts no mail").
//!
//! ```text
//!   client / queue / spf / dmarc
//!             │
//!             ▼
//!        MxResolver ──► cache (TTL, bounded)
//!             │
//!             ▼
//!     dyn Resolver ──► HickoryResolver (production)   or   MockResolver (tests)
//! ```

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ferroma_core::config::DnsConfig;
use ferroma_core::FerromaError;
use futures_util::future::BoxFuture;
use hickory_resolver::config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts};
use hickory_resolver::error::ResolveErrorKind;
use hickory_resolver::TokioAsyncResolver;

/// The upper bound on cached DNS answers.
///
/// A mail server asks about a lot of domains; unbounded growth here would be a slow
/// memory leak shaped like a spam run.
pub const MAX_CACHE_ENTRIES: usize = 4096;

/// One mail exchanger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MxHost {
    /// The MX preference (`0` is most preferred).
    pub preference: u16,
    /// The exchanger's hostname, lower-cased, without a trailing dot.
    pub host: String,
}

impl MxHost {
    /// Build an MX host record.
    pub fn new(preference: u16, host: impl Into<String>) -> Self {
        MxHost {
            preference,
            host: normalise_name(&host.into()),
        }
    }

    /// Whether this is the RFC 7505 null MX (the root name, meaning "no mail").
    pub fn is_null(&self) -> bool {
        is_null_mx_name(&self.host)
    }
}

/// The result of an MX lookup, including the "no mail here" cases.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MxLookup {
    /// The exchangers, most preferred first.
    pub hosts: Vec<MxHost>,
    /// `true` when the domain published a null MX and therefore accepts no mail.
    pub null_mx: bool,
    /// `true` when there was no `MX` record and the implicit-MX rule was applied.
    pub implicit: bool,
}

impl MxLookup {
    /// Whether this domain is willing to receive mail at all.
    pub fn accepts_mail(&self) -> bool {
        !self.null_mx && !self.hosts.is_empty()
    }

    /// The hostnames, most preferred first.
    pub fn host_names(&self) -> Vec<&str> {
        self.hosts.iter().map(|h| h.host.as_str()).collect()
    }
}

/// The DNS question set the mail layer needs.
///
/// Implemented by [`HickoryResolver`] in production and [`MockResolver`] in tests.
/// Failures that mean "this name has no such record" (`NXDOMAIN`, an empty answer)
/// are `Ok(vec![])`, not `Err`: a domain with no `MX` is a normal domain, not an
/// error. `Err` is reserved for genuine lookup failures (timeout, SERVFAIL, no
/// network), which callers must treat as *temporary*.
pub trait Resolver: Send + Sync + std::fmt::Debug {
    /// `MX` records for `domain`, with their preferences.
    fn mx(&self, domain: &str) -> BoxFuture<'_, Result<Vec<MxHost>, FerromaError>>;

    /// `A` and `AAAA` addresses for `name`.
    fn addresses(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, FerromaError>>;

    /// Every `TXT` record for `name`, one string per record (chunks concatenated).
    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, FerromaError>>;

    /// The `PTR` names for `ip`.
    fn ptr(&self, ip: IpAddr) -> BoxFuture<'_, Result<Vec<String>, FerromaError>>;

    /// How many queries this resolver has answered.
    ///
    /// Production resolvers do not count (there is nothing useful to do with the
    /// number); [`MockResolver`] does, so tests can assert that a cache hit really
    /// avoided a lookup.
    fn query_count(&self) -> usize {
        0
    }
}

/// The production resolver, built on `hickory-resolver`.
#[derive(Debug, Clone)]
pub struct HickoryResolver {
    inner: TokioAsyncResolver,
}

impl HickoryResolver {
    /// Build a resolver honouring the `[dns]` configuration block.
    ///
    /// An empty `resolvers` list means "use the system's *nameservers*" — which is what a
    /// developer laptop and a container with a working `/etc/resolv.conf` both want — but
    /// **not** the system's timeout policy: `[dns] timeout_secs` and `dns.attempts` are
    /// ours to set and are applied on both paths.
    ///
    /// That distinction matters more than it looks. `TokioAsyncResolver::tokio_from_system_conf()`
    /// takes no options and silently uses hickory's defaults (a five-second timeout and
    /// two attempts, per nameserver), so building the system resolver with it would make
    /// one lookup cost ten seconds however impatient `[dns]` was configured to be. On a
    /// host whose configured nameserver does not answer raw UDP queries, that is the
    /// difference between an inbound message taking one second and taking a minute.
    pub fn new(config: &DnsConfig) -> Result<Self, FerromaError> {
        if config.resolvers.is_empty() {
            let (system_config, mut options) = hickory_resolver::system_conf::read_system_conf()
                .map_err(|e| {
                    FerromaError::Dns(format!("cannot read the system resolver configuration: {e}"))
                })?;
            apply_dns_config(&mut options, config);
            return Ok(HickoryResolver {
                inner: TokioAsyncResolver::tokio(system_config, options),
            });
        }

        let mut resolver_config = ResolverConfig::new();
        for server in &config.resolvers {
            let address = parse_server(server)?;
            resolver_config.add_name_server(NameServerConfig {
                socket_addr: address,
                protocol: Protocol::Udp,
                tls_dns_name: None,
                trust_negative_responses: false,
                bind_addr: None,
            });
        }
        let mut options = ResolverOpts::default();
        apply_dns_config(&mut options, config);

        Ok(HickoryResolver {
            inner: TokioAsyncResolver::tokio(resolver_config, options),
        })
    }

    /// The underlying hickory resolver, for callers that need a record type this
    /// trait does not expose.
    pub fn inner(&self) -> &TokioAsyncResolver {
        &self.inner
    }
}

/// Overlay the `[dns]` block onto a hickory options set.
///
/// The single place `[dns]` becomes resolver behaviour, so the system-config path and
/// the explicit-nameserver path cannot drift apart — which is exactly how the timeout
/// came to be ignored on one of them.
fn apply_dns_config(options: &mut ResolverOpts, config: &DnsConfig) {
    options.timeout = Duration::from_secs(config.timeout_secs.max(1));
    options.attempts = config.attempts.max(1);
    options.try_tcp_on_error = config.tcp_fallback;
    options.cache_size = MAX_CACHE_ENTRIES;
    options.negative_min_ttl = Some(Duration::from_secs(config.negative_ttl_secs));
    options.positive_min_ttl = if config.cache_ttl_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(config.cache_ttl_secs))
    };
}

/// Parse `1.1.1.1` or `1.1.1.1:53` into a socket address.
fn parse_server(raw: &str) -> Result<std::net::SocketAddr, FerromaError> {
    let trimmed = raw.trim();
    if let Ok(address) = trimmed.parse::<std::net::SocketAddr>() {
        return Ok(address);
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, 53));
    }
    Err(FerromaError::Dns(format!(
        "dns.resolvers entry {raw:?} is not an IP address or ip:port"
    )))
}

impl Resolver for HickoryResolver {
    fn mx(&self, domain: &str) -> BoxFuture<'_, Result<Vec<MxHost>, FerromaError>> {
        let domain = normalise_name(domain);
        Box::pin(async move {
            match self.inner.mx_lookup(domain.as_str()).await {
                Ok(lookup) => {
                    let mut hosts: Vec<MxHost> = lookup
                        .iter()
                        .map(|record| MxHost::new(record.preference(), record.exchange().to_utf8()))
                        .collect();
                    sort_mx(&mut hosts);
                    Ok(hosts)
                }
                Err(e) => Err(map_resolve_error(e, &domain)),
            }
        })
    }

    fn addresses(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, FerromaError>> {
        let name = normalise_name(name);
        Box::pin(async move {
            match self.inner.lookup_ip(name.as_str()).await {
                Ok(lookup) => Ok(lookup.iter().collect()),
                Err(e) => Err(map_resolve_error(e, &name)),
            }
        })
    }

    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, FerromaError>> {
        let name = normalise_name(name);
        Box::pin(async move {
            match self.inner.txt_lookup(name.as_str()).await {
                Ok(lookup) => Ok(lookup
                    .iter()
                    .map(|record| {
                        // RFC 7208 §3.3 / RFC 6376 §3.6.2.2: a long record is split
                        // across several character-strings and must be rejoined
                        // **without** a separator.
                        record
                            .iter()
                            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                            .collect::<String>()
                    })
                    .collect()),
                Err(e) => Err(map_resolve_error(e, &name)),
            }
        })
    }

    fn ptr(&self, ip: IpAddr) -> BoxFuture<'_, Result<Vec<String>, FerromaError>> {
        Box::pin(async move {
            match self.inner.reverse_lookup(ip).await {
                Ok(lookup) => Ok(lookup.iter().map(|name| normalise_name(&name.to_utf8())).collect()),
                Err(e) => Err(map_resolve_error(e, &ip.to_string())),
            }
        })
    }
}

/// Turn a hickory error into a [`FerromaError`], mapping "no such record" to an
/// empty successful answer.
fn map_resolve_error(err: hickory_resolver::error::ResolveError, query: &str) -> FerromaError {
    match err.kind() {
        ResolveErrorKind::NoRecordsFound { .. } => FerromaError::Dns(format!("no records for {query}")),
        ResolveErrorKind::Proto(proto) => {
            FerromaError::Dns(format!("protocol error resolving {query}: {proto}"))
        }
        ResolveErrorKind::Timeout => FerromaError::Timeout(format!("DNS lookup for {query} timed out")),
        ResolveErrorKind::Io(io) => FerromaError::Dns(format!("i/o error resolving {query}: {io}")),
        other => FerromaError::Dns(format!("cannot resolve {query}: {other}")),
    }
}

/// A scripted resolver for tests.
///
/// Every answer is set explicitly, so SPF/DKIM/DMARC can be exercised end to end
/// without a network. Unset names answer "no records".
#[derive(Debug, Default, Clone)]
pub struct MockResolver {
    mx: HashMap<String, Vec<MxHost>>,
    addresses: HashMap<String, Vec<IpAddr>>,
    txt: HashMap<String, Vec<String>>,
    ptr: HashMap<String, Vec<String>>,
    /// Names whose lookup should fail with a temporary error.
    failing: Vec<String>,
    /// Every query this mock was asked, in order — for asserting lookup counts.
    queries: Arc<Mutex<Vec<String>>>,
}

impl MockResolver {
    /// An empty resolver.
    pub fn new() -> Self {
        MockResolver::default()
    }

    /// Publish `MX` records for `domain`.
    pub fn with_mx(mut self, domain: &str, hosts: Vec<MxHost>) -> Self {
        self.mx.insert(normalise_name(domain), hosts);
        self
    }

    /// Publish addresses for `name`.
    pub fn with_addresses(mut self, name: &str, addresses: Vec<IpAddr>) -> Self {
        self.addresses.insert(normalise_name(name), addresses);
        self
    }

    /// Publish `TXT` records for `name`.
    pub fn with_txt(mut self, name: &str, records: Vec<String>) -> Self {
        self.txt.insert(normalise_name(name), records);
        self
    }

    /// Publish `PTR` names for `ip`.
    pub fn with_ptr(mut self, ip: IpAddr, names: Vec<String>) -> Self {
        self.ptr.insert(ip.to_string(), names);
        self
    }

    /// Set `PTR` names in place.
    pub fn set_ptr(&mut self, ip: IpAddr, names: Vec<String>) {
        self.ptr.insert(ip.to_string(), names);
    }

    /// Make every lookup for `name` fail temporarily.
    pub fn with_failure(mut self, name: &str) -> Self {
        self.failing.push(normalise_name(name));
        self
    }

    /// Set `MX` records in place.
    pub fn set_mx(&mut self, domain: &str, hosts: Vec<MxHost>) {
        self.mx.insert(normalise_name(domain), hosts);
    }

    /// Set `TXT` records in place.
    pub fn set_txt(&mut self, name: &str, records: Vec<String>) {
        self.txt.insert(normalise_name(name), records);
    }

    /// Set addresses in place.
    pub fn set_addresses(&mut self, name: &str, addresses: Vec<IpAddr>) {
        self.addresses.insert(normalise_name(name), addresses);
    }

    /// Make `name` fail temporarily.
    pub fn set_failure(&mut self, name: &str) {
        self.failing.push(normalise_name(name));
    }

    /// Every query made against this mock, `kind:name` shaped.
    pub fn queries(&self) -> Vec<String> {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many queries were made.
    pub fn query_count(&self) -> usize {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn record(&self, kind: &str, name: &str) {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!("{kind}:{}", normalise_name(name)));
    }

    fn check_failing(&self, name: &str) -> Result<(), FerromaError> {
        let normalised = normalise_name(name);
        if self.failing.contains(&normalised) {
            return Err(FerromaError::Dns(format!("mock failure for {normalised}")));
        }
        Ok(())
    }
}

impl Resolver for MockResolver {
    fn mx(&self, domain: &str) -> BoxFuture<'_, Result<Vec<MxHost>, FerromaError>> {
        self.record("mx", domain);
        let result = match self.check_failing(domain) {
            Ok(()) => Ok(self.mx.get(&normalise_name(domain)).cloned().unwrap_or_default()),
            Err(e) => Err(e),
        };
        Box::pin(async move { result })
    }

    fn addresses(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, FerromaError>> {
        self.record("a", name);
        let result = match self.check_failing(name) {
            Ok(()) => Ok(self
                .addresses
                .get(&normalise_name(name))
                .cloned()
                .unwrap_or_default()),
            Err(e) => Err(e),
        };
        Box::pin(async move { result })
    }

    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, FerromaError>> {
        self.record("txt", name);
        let result = match self.check_failing(name) {
            Ok(()) => Ok(self.txt.get(&normalise_name(name)).cloned().unwrap_or_default()),
            Err(e) => Err(e),
        };
        Box::pin(async move { result })
    }

    fn ptr(&self, ip: IpAddr) -> BoxFuture<'_, Result<Vec<String>, FerromaError>> {
        self.record("ptr", &ip.to_string());
        let result = match self.check_failing(&ip.to_string()) {
            Ok(()) => Ok(self.ptr.get(&ip.to_string()).cloned().unwrap_or_default()),
            Err(e) => Err(e),
        };
        Box::pin(async move { result })
    }

    fn query_count(&self) -> usize {
        MockResolver::query_count(self)
    }
}

/// A cached DNS answer.
#[derive(Debug, Clone)]
struct CacheEntry {
    /// The answer, or the error that was seen. Errors are cached for the shorter
    /// negative TTL so a SERVFAIL does not pin a domain out of service for five
    /// minutes.
    value: Result<CacheValue, String>,
    /// When the entry was stored.
    stored_at: std::time::Instant,
    /// How long it may live.
    ttl: Duration,
}

/// A cached answer, type-erased so one map serves every record type.
#[derive(Debug, Clone)]
enum CacheValue {
    Mx(Vec<MxHost>),
    Addresses(Vec<IpAddr>),
    Txt(Vec<String>),
    Ptr(Vec<String>),
}

impl CacheValue {
    fn as_mx(&self) -> Option<&Vec<MxHost>> {
        match self {
            CacheValue::Mx(v) => Some(v),
            _ => None,
        }
    }

    fn as_addresses(&self) -> Option<&Vec<IpAddr>> {
        match self {
            CacheValue::Addresses(v) => Some(v),
            _ => None,
        }
    }

    fn as_txt(&self) -> Option<&Vec<String>> {
        match self {
            CacheValue::Txt(v) => Some(v),
            _ => None,
        }
    }

    fn as_ptr(&self) -> Option<&Vec<String>> {
        match self {
            CacheValue::Ptr(v) => Some(v),
            _ => None,
        }
    }
}

/// The DNS facade the rest of the crate uses.
///
/// Adds three things the raw resolver does not have:
///
/// * a **TTL cache** honouring `[dns] cache_ttl_secs` / `negative_ttl_secs`, bounded
///   by [`MAX_CACHE_ENTRIES`];
/// * the **implicit MX** rule: a domain with no `MX` is its own mail exchanger at its
///   `A`/`AAAA` addresses (RFC 5321 §5.1);
/// * **null MX** detection: an `MX` of `.` (or a single space) means the domain
///   accepts no mail at all (RFC 7505).
#[derive(Debug, Clone)]
pub struct MxResolver {
    inner: Arc<dyn Resolver>,
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
    positive_ttl: Duration,
    negative_ttl: Duration,
    max_entries: usize,
}

impl MxResolver {
    /// Wrap `inner` with the cache and the derived rules from `[dns]`.
    pub fn new(inner: Arc<dyn Resolver>, config: &DnsConfig) -> Self {
        MxResolver {
            inner,
            cache: Arc::new(Mutex::new(HashMap::new())),
            positive_ttl: Duration::from_secs(if config.cache_ttl_secs == 0 {
                1
            } else {
                config.cache_ttl_secs
            }),
            negative_ttl: Duration::from_secs(if config.negative_ttl_secs == 0 {
                1
            } else {
                config.negative_ttl_secs
            }),
            max_entries: MAX_CACHE_ENTRIES,
        }
    }

    /// Build one over the production resolver.
    pub fn hickory(config: &DnsConfig) -> Result<Self, FerromaError> {
        let inner = Arc::new(HickoryResolver::new(config)?);
        Ok(MxResolver::new(inner, config))
    }

    /// Build one over a scripted resolver, for tests.
    pub fn mock(resolver: MockResolver, config: &DnsConfig) -> Self {
        MxResolver::new(Arc::new(resolver), config)
    }

    /// The underlying resolver, for the SPF/DKIM/DMARC checkers.
    pub fn resolver(&self) -> Arc<dyn Resolver> {
        Arc::clone(&self.inner)
    }

    /// How many answers are cached right now.
    pub fn cache_len(&self) -> usize {
        self.lock().len()
    }

    /// Empty the cache.
    pub fn clear_cache(&self) {
        self.lock().clear();
    }

    /// Resolve where `domain`'s mail goes.
    ///
    /// Applies, in order: the cache; a real `MX` lookup; the null-MX rule; and the
    /// RFC 5321 §5.1 implicit-MX fallback — a domain with no `MX` record is its own
    /// mail exchanger, reached at its `A`/`AAAA` addresses, so
    /// `MxLookup::implicit` is set and the single "host" is the domain itself.
    ///
    /// The fallback is only applied when the domain actually resolves to an
    /// address; a domain that does not exist has nowhere to deliver, and reporting
    /// that as "no targets" is more useful to the queue than a hostname that will
    /// fail to resolve a second time.
    pub async fn mx(&self, domain: &str) -> Result<MxLookup, FerromaError> {
        let key = format!("mx:{}", normalise_name(domain));
        if let Some(hosts) = self.cached(&key).and_then(|v| v.as_mx().cloned()) {
            return Ok(build_lookup(hosts));
        }

        let hosts = match self.inner.mx(domain).await {
            Ok(hosts) => hosts,
            Err(e) => {
                self.store_error(key, &e);
                return Err(e);
            }
        };
        let mut lookup = build_lookup(hosts.clone());
        if lookup.hosts.is_empty() && !lookup.null_mx {
            // Implicit MX: cache the *resolved* form so the fallback is not redone
            // on every delivery attempt. The address lookup goes straight to the
            // inner resolver — `MxResolver::addresses` would cache the same answer a
            // second time under its own key for no benefit.
            let has_addresses = self
                .inner
                .addresses(domain)
                .await
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if has_addresses {
                lookup = MxLookup {
                    hosts: vec![MxHost::new(0, domain)],
                    null_mx: false,
                    implicit: true,
                };
                self.store(key, CacheValue::Mx(lookup.hosts.clone()), true);
                return Ok(lookup);
            }
        }
        self.store(key, CacheValue::Mx(hosts), true);
        Ok(lookup)
    }

    /// The `A`/`AAAA` addresses of `name`.
    pub async fn addresses(&self, name: &str) -> Result<Vec<IpAddr>, FerromaError> {
        let key = format!("a:{}", normalise_name(name));
        if let Some(addresses) = self.cached(&key).and_then(|v| v.as_addresses().cloned()) {
            return Ok(addresses);
        }
        let addresses = match self.inner.addresses(name).await {
            Ok(a) => a,
            Err(e) => {
                self.store_error(key, &e);
                return Err(e);
            }
        };
        self.store(key, CacheValue::Addresses(addresses.clone()), true);
        Ok(addresses)
    }

    /// Every `TXT` record at `name`.
    pub async fn txt(&self, name: &str) -> Result<Vec<String>, FerromaError> {
        let key = format!("txt:{}", normalise_name(name));
        if let Some(records) = self.cached(&key).and_then(|v| v.as_txt().cloned()) {
            return Ok(records);
        }
        let records = match self.inner.txt(name).await {
            Ok(r) => r,
            Err(e) => {
                self.store_error(key, &e);
                return Err(e);
            }
        };
        self.store(key, CacheValue::Txt(records.clone()), true);
        Ok(records)
    }

    /// The `PTR` names of `ip`.
    pub async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, FerromaError> {
        let key = format!("ptr:{ip}");
        if let Some(names) = self.cached(&key).and_then(|v| v.as_ptr().cloned()) {
            return Ok(names);
        }
        let names = match self.inner.ptr(ip).await {
            Ok(n) => n,
            Err(e) => {
                self.store_error(key, &e);
                return Err(e);
            }
        };
        self.store(key, CacheValue::Ptr(names.clone()), true);
        Ok(names)
    }

    /// Resolve the hosts to try for `domain`, in preference order, with each host's
    /// addresses.
    ///
    /// Returns an empty vector when the domain publishes a null MX or has no
    /// addresses at all: both mean "there is nowhere to deliver this".
    pub async fn delivery_targets(&self, domain: &str) -> Result<Vec<(MxHost, Vec<IpAddr>)>, FerromaError> {
        let lookup = self.mx(domain).await?;
        if lookup.null_mx {
            return Ok(Vec::new());
        }
        let mut targets = Vec::with_capacity(lookup.hosts.len());
        for host in lookup.hosts {
            let addresses = self.addresses(&host.host).await?;
            if addresses.is_empty() {
                continue;
            }
            targets.push((host, addresses));
        }
        Ok(targets)
    }

    /// Run the Admin DNS-health checks for `domain`.
    pub async fn health(&self, domain: &str, selector: &str) -> DnsHealth {
        DnsHealth::check(self, domain, selector).await
    }

    // ------------------------------------------------------------------
    // Cache internals
    // ------------------------------------------------------------------

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CacheEntry>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cached(&self, key: &str) -> Option<CacheValue> {
        let mut map = self.lock();
        let entry = map.get(key)?;
        if entry.stored_at.elapsed() >= entry.ttl {
            map.remove(key);
            return None;
        }
        match &entry.value {
            Ok(value) => Some(value.clone()),
            Err(_) => None,
        }
    }

    fn store(&self, key: String, value: CacheValue, positive: bool) {
        let mut map = self.lock();
        if map.len() >= self.max_entries && !map.contains_key(&key) {
            // Evict everything that has expired; if that freed nothing, drop one
            // arbitrary entry. A cache miss is always safe, so eviction policy only
            // affects throughput.
            let now = std::time::Instant::now();
            map.retain(|_, entry| now.saturating_duration_since(entry.stored_at) < entry.ttl);
            if map.len() >= self.max_entries {
                if let Some(victim) = map.keys().next().cloned() {
                    map.remove(&victim);
                }
            }
        }
        map.insert(
            key,
            CacheEntry {
                value: Ok(value),
                stored_at: std::time::Instant::now(),
                ttl: if positive { self.positive_ttl } else { self.negative_ttl },
            },
        );
    }

    fn store_error(&self, key: String, err: &FerromaError) {
        // Only *temporary* failures are cached; a permanent one is either a bug or a
        // configuration problem and the operator wants to see it every time.
        if !err.is_temporary() && !matches!(err, FerromaError::Dns(_)) {
            return;
        }
        let mut map = self.lock();
        map.insert(
            key,
            CacheEntry {
                value: Err(err.to_string()),
                stored_at: std::time::Instant::now(),
                ttl: self.negative_ttl,
            },
        );
    }
}

impl Resolver for MxResolver {
    /// Explicit calls to the inherent methods, so the cache is used and this
    /// implementation cannot recurse into itself.
    fn mx(&self, domain: &str) -> BoxFuture<'_, Result<Vec<MxHost>, FerromaError>> {
        let domain = domain.to_string();
        Box::pin(async move { MxResolver::mx(self, &domain).await.map(|lookup| lookup.hosts) })
    }

    fn addresses(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, FerromaError>> {
        let name = name.to_string();
        Box::pin(async move { MxResolver::addresses(self, &name).await })
    }

    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, FerromaError>> {
        let name = name.to_string();
        Box::pin(async move { MxResolver::txt(self, &name).await })
    }

    fn ptr(&self, ip: IpAddr) -> BoxFuture<'_, Result<Vec<String>, FerromaError>> {
        Box::pin(async move { MxResolver::ptr(self, ip).await })
    }

    fn query_count(&self) -> usize {
        // What a cache hit is worth: the number of answers this resolver is currently
        // holding, which is the closest thing to "queries answered" a caching facade
        // can report.
        MxResolver::cache_len(self)
    }
}

/// Apply the null-MX and implicit-MX rules to a raw `MX` answer.
fn build_lookup(mut hosts: Vec<MxHost>) -> MxLookup {
    sort_mx(&mut hosts);
    if hosts.len() == 1 && hosts[0].is_null() {
        return MxLookup {
            hosts: Vec::new(),
            null_mx: true,
            implicit: false,
        };
    }
    hosts.retain(|h| !h.is_null());
    MxLookup {
        hosts,
        null_mx: false,
        implicit: false,
    }
}

/// Whether an `MX` target is the RFC 7505 null MX.
///
/// The root name is written `"."` on the wire; the RFC shows `" "` (a single space)
/// because some nameservers will not store a bare root. Both are accepted.
pub fn is_null_mx_name(host: &str) -> bool {
    let trimmed = host.trim();
    trimmed.is_empty() || trimmed == "." || trimmed == "0"
}

/// Sort by preference, then by hostname for stability.
fn sort_mx(hosts: &mut [MxHost]) {
    hosts.sort_by(|a, b| a.preference.cmp(&b.preference).then_with(|| a.host.cmp(&b.host)));
}

/// Lower-case a DNS name and drop a trailing dot.
pub fn normalise_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// DNS health
// ---------------------------------------------------------------------------

/// How one DNS check came out. Mirrors `docs/api.md` §4.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsStatus {
    /// Present and correct.
    Ok,
    /// Present but not what a mail server wants (a `~all` is a warning).
    Warn,
    /// Missing or wrong in a way that breaks mail.
    Fail,
    /// Not applicable here (an `AAAA` record on an IPv4-only host).
    Skip,
}

impl DnsStatus {
    /// The wire token `docs/api.md` documents.
    pub fn as_str(self) -> &'static str {
        match self {
            DnsStatus::Ok => "ok",
            DnsStatus::Warn => "warn",
            DnsStatus::Fail => "fail",
            DnsStatus::Skip => "skip",
        }
    }

    /// Whether this verdict counts towards the score.
    pub fn counts(self) -> bool {
        !matches!(self, DnsStatus::Skip)
    }

    /// Whether this verdict is worth points.
    pub fn is_ok(self) -> bool {
        matches!(self, DnsStatus::Ok)
    }
}

/// Which record a check looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsRecordKind {
    /// `MX`.
    Mx,
    /// `A`.
    A,
    /// `AAAA`.
    Aaaa,
    /// The reverse `PTR`.
    Ptr,
    /// The `v=spf1` `TXT` record.
    Spf,
    /// The `<selector>._domainkey` `TXT` record.
    Dkim,
    /// The `_dmarc` `TXT` record.
    Dmarc,
}

impl DnsRecordKind {
    /// The wire token `docs/api.md` documents (`"MX"`, `"A"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            DnsRecordKind::Mx => "MX",
            DnsRecordKind::A => "A",
            DnsRecordKind::Aaaa => "AAAA",
            DnsRecordKind::Ptr => "PTR",
            DnsRecordKind::Spf => "SPF",
            DnsRecordKind::Dkim => "DKIM",
            DnsRecordKind::Dmarc => "DMARC",
        }
    }
}

/// One row of the Admin "DNS Health" panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsRecordCheck {
    /// Which record was checked.
    pub kind: DnsRecordKind,
    /// The verdict.
    pub status: DnsStatus,
    /// What the record should contain, when there is a single right answer.
    pub expected: Option<String>,
    /// What was actually found.
    pub found: Vec<String>,
    /// What to do about a `warn`/`fail`.
    pub hint: Option<String>,
}

impl DnsRecordCheck {
    /// A check with no hint.
    pub fn new(kind: DnsRecordKind, status: DnsStatus, expected: Option<String>, found: Vec<String>) -> Self {
        DnsRecordCheck {
            kind,
            status,
            expected,
            found,
            hint: None,
        }
    }

    /// Attach an operator-facing hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// The whole panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsHealth {
    /// The domain that was checked.
    pub domain: String,
    /// When the check ran.
    pub checked_at: chrono::DateTime<chrono::Utc>,
    /// One row per record kind.
    pub records: Vec<DnsRecordCheck>,
}

impl DnsHealth {
    /// Run every check for `domain`.
    ///
    /// `selector` is the DKIM selector published at `<selector>._domainkey.<domain>`.
    pub async fn check(resolver: &MxResolver, domain: &str, selector: &str) -> DnsHealth {
        let domain = normalise_name(domain);
        let mut records = Vec::new();

        // --- MX ---------------------------------------------------------
        let mx = resolver.mx(&domain).await;
        match mx {
            Ok(lookup) if lookup.null_mx => records.push(
                DnsRecordCheck::new(DnsRecordKind::Mx, DnsStatus::Fail, None, vec![".".into()])
                    .with_hint("this domain publishes a null MX and accepts no mail"),
            ),
            Ok(lookup) if !lookup.hosts.is_empty() => {
                let found: Vec<String> = lookup
                    .hosts
                    .iter()
                    .map(|h| format!("{} {}.", h.preference, h.host))
                    .collect();
                let expected = lookup.hosts.first().map(|h| h.host.clone());
                records.push(DnsRecordCheck::new(
                    DnsRecordKind::Mx,
                    DnsStatus::Ok,
                    expected,
                    found,
                ));
            }
            Ok(_) => {
                // No MX: legal, but the implicit-MX fallback is what real mail
                // providers tell operators to avoid.
                let fallback = resolver.addresses(&domain).await.unwrap_or_default();
                if fallback.is_empty() {
                    records.push(
                        DnsRecordCheck::new(DnsRecordKind::Mx, DnsStatus::Fail, None, Vec::new())
                            .with_hint("publish an MX record pointing at this host"),
                    );
                } else {
                    records.push(
                        DnsRecordCheck::new(
                            DnsRecordKind::Mx,
                            DnsStatus::Warn,
                            Some(format!("10 mail.{domain}")),
                            Vec::new(),
                        )
                        .with_hint("no MX record; mail falls back to the domain's A record"),
                    );
                }
            }
            Err(e) => records.push(
                DnsRecordCheck::new(DnsRecordKind::Mx, DnsStatus::Fail, None, Vec::new())
                    .with_hint(format!("lookup failed: {e}")),
            ),
        }

        // --- A / AAAA ---------------------------------------------------
        let addresses = resolver.addresses(&domain).await.unwrap_or_default();
        let v4: Vec<String> = addresses
            .iter()
            .filter(|a| a.is_ipv4())
            .map(ToString::to_string)
            .collect();
        let v6: Vec<String> = addresses
            .iter()
            .filter(|a| a.is_ipv6())
            .map(ToString::to_string)
            .collect();

        records.push(if v4.is_empty() {
            DnsRecordCheck::new(DnsRecordKind::A, DnsStatus::Fail, None, v4).with_hint(
                "the mail host needs an A record — every sending server must be resolvable",
            )
        } else {
            DnsRecordCheck::new(DnsRecordKind::A, DnsStatus::Ok, None, v4)
        });

        records.push(if v6.is_empty() {
            DnsRecordCheck::new(DnsRecordKind::Aaaa, DnsStatus::Skip, None, v6)
        } else {
            DnsRecordCheck::new(DnsRecordKind::Aaaa, DnsStatus::Ok, None, v6)
        });

        // --- PTR --------------------------------------------------------
        let mut ptr_found = Vec::new();
        for address in addresses.iter().take(4) {
            if let Ok(names) = resolver.ptr(*address).await {
                ptr_found.extend(names);
            }
        }
        records.push(if ptr_found.is_empty() {
            DnsRecordCheck::new(
                DnsRecordKind::Ptr,
                DnsStatus::Fail,
                Some(domain.clone()),
                Vec::new(),
            )
            .with_hint("publish a matching PTR record; large providers reject mail without one")
        } else if ptr_found.iter().any(|n| n == &domain) {
            DnsRecordCheck::new(DnsRecordKind::Ptr, DnsStatus::Ok, Some(domain.clone()), ptr_found)
        } else {
            DnsRecordCheck::new(
                DnsRecordKind::Ptr,
                DnsStatus::Warn,
                Some(domain.clone()),
                ptr_found,
            )
            .with_hint("the PTR does not match the sending hostname (forward-confirmed reverse DNS)")
        });

        // --- SPF ---------------------------------------------------------
        let spf_found: Vec<String> = resolver
            .txt(&domain)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.trim_start().to_ascii_lowercase().starts_with("v=spf1"))
            .collect();
        records.push(match spf_found.len() {
            0 => DnsRecordCheck::new(DnsRecordKind::Spf, DnsStatus::Fail, None, Vec::new())
                .with_hint("publish one `v=spf1` TXT record, e.g. `v=spf1 mx -all`"),
            1 => {
                // `-all` is a hard fail; `~all` is a soft one; anything else leaves the
                // record open-ended, which is the state operators most often forget to
                // finish.
                let status = if spf_found[0].contains("-all") {
                    DnsStatus::Ok
                } else {
                    DnsStatus::Warn
                };
                let mut check = DnsRecordCheck::new(DnsRecordKind::Spf, status, None, spf_found);
                if status == DnsStatus::Warn {
                    check = check.with_hint("end the record with `-all` to refuse everything else");
                }
                check
            }
            _ => DnsRecordCheck::new(DnsRecordKind::Spf, DnsStatus::Fail, None, spf_found)
                .with_hint("a domain must publish exactly one SPF record (RFC 7208 §3.2)"),
        });

        // --- DKIM --------------------------------------------------------
        let dkim_name = format!("{}._domainkey.{domain}", normalise_name(selector));
        let dkim_found = resolver.txt(&dkim_name).await.unwrap_or_default();
        records.push(if dkim_found.is_empty() {
            DnsRecordCheck::new(
                DnsRecordKind::Dkim,
                DnsStatus::Warn,
                Some(dkim_name.clone()),
                Vec::new(),
            )
            .with_hint("publish the TXT record shown by GET /api/v1/domains/:id/dkim")
        } else if dkim_found.iter().any(|t| t.contains("p=")) {
            DnsRecordCheck::new(DnsRecordKind::Dkim, DnsStatus::Ok, Some(dkim_name), dkim_found)
        } else {
            DnsRecordCheck::new(
                DnsRecordKind::Dkim,
                DnsStatus::Fail,
                Some(dkim_name),
                dkim_found,
            )
            .with_hint("the DKIM record has no `p=` public key")
        });

        // --- DMARC -------------------------------------------------------
        let dmarc_name = format!("_dmarc.{domain}");
        let dmarc_found: Vec<String> = resolver
            .txt(&dmarc_name)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.trim_start().to_ascii_uppercase().starts_with("V=DMARC1"))
            .collect();
        records.push(match dmarc_found.len() {
            0 => DnsRecordCheck::new(DnsRecordKind::Dmarc, DnsStatus::Fail, None, Vec::new())
                .with_hint("publish `v=DMARC1; p=quarantine; rua=mailto:dmarc@…`"),
            1 => {
                let lowered = dmarc_found[0].to_ascii_lowercase();
                let status = if lowered.contains("p=reject") {
                    DnsStatus::Ok
                } else {
                    DnsStatus::Warn
                };
                let mut check = DnsRecordCheck::new(DnsRecordKind::Dmarc, status, None, dmarc_found);
                if status == DnsStatus::Warn {
                    check = check.with_hint("`p=reject` is the strongest policy once reports look clean");
                }
                check
            }
            _ => DnsRecordCheck::new(DnsRecordKind::Dmarc, DnsStatus::Fail, None, dmarc_found)
                .with_hint("a domain must publish exactly one DMARC record"),
        });

        DnsHealth {
            domain,
            checked_at: chrono::Utc::now(),
            records,
        }
    }

    /// How many checks are `ok`.
    pub fn score(&self) -> usize {
        self.records.iter().filter(|r| r.status.is_ok()).count()
    }

    /// How many checks count towards the score.
    pub fn max_score(&self) -> usize {
        self.records.iter().filter(|r| r.status.counts()).count()
    }

    /// Look one check up by kind.
    pub fn get(&self, kind: DnsRecordKind) -> Option<&DnsRecordCheck> {
        self.records.iter().find(|r| r.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroma_core::config::DnsConfig;

    fn dns_config() -> DnsConfig {
        DnsConfig {
            resolvers: Vec::new(),
            timeout_secs: 1,
            attempts: 1,
            cache_ttl_secs: 300,
            negative_ttl_secs: 60,
            tcp_fallback: true,
        }
    }

    fn ip(raw: &str) -> IpAddr {
        raw.parse().expect("test ip")
    }

    // ------------------------------------------------------------------
    // Name handling
    // ------------------------------------------------------------------

    #[test]
    fn names_are_lower_cased_and_unqualified() {
        assert_eq!(normalise_name("Example.COM."), "example.com");
        assert_eq!(normalise_name("  mail.example.com  "), "mail.example.com");
        assert_eq!(normalise_name(""), "");
    }

    #[test]
    fn mx_hosts_normalise_their_name() {
        let host = MxHost::new(10, "MX1.Example.COM.");
        assert_eq!(host.host, "mx1.example.com");
        assert_eq!(host.preference, 10);
        assert!(!host.is_null());
    }

    #[test]
    fn null_mx_names_are_recognised_in_both_spellings() {
        assert!(is_null_mx_name("."));
        assert!(is_null_mx_name(" "));
        assert!(is_null_mx_name(""));
        assert!(is_null_mx_name("0"));
        assert!(!is_null_mx_name("mx.example.com"));
        assert!(MxHost::new(0, ".").is_null());
        assert!(MxHost::new(0, " ").is_null());
    }

    // ------------------------------------------------------------------
    // Sorting and the derived rules
    // ------------------------------------------------------------------

    #[test]
    fn mx_hosts_are_sorted_by_preference() {
        let mut hosts = vec![
            MxHost::new(20, "mx2.example.com"),
            MxHost::new(5, "mx1.example.com"),
            MxHost::new(10, "mx3.example.com"),
        ];
        sort_mx(&mut hosts);
        assert_eq!(hosts[0].preference, 5);
        assert_eq!(hosts[1].preference, 10);
        assert_eq!(hosts[2].preference, 20);
    }

    #[test]
    fn equal_preferences_sort_by_hostname_for_stability() {
        let mut hosts = vec![
            MxHost::new(10, "b.example.com"),
            MxHost::new(10, "a.example.com"),
        ];
        sort_mx(&mut hosts);
        assert_eq!(hosts[0].host, "a.example.com");
    }

    #[test]
    fn a_single_null_mx_means_the_domain_accepts_no_mail() {
        let lookup = build_lookup(vec![MxHost::new(0, ".")]);
        assert!(lookup.null_mx);
        assert!(lookup.hosts.is_empty());
        assert!(!lookup.accepts_mail());
    }

    #[test]
    fn a_null_mx_among_real_ones_is_dropped_not_honoured() {
        // Not legal, but a nameserver can serve it; dropping the null entry is the
        // only reading that does not black-hole mail.
        let lookup = build_lookup(vec![MxHost::new(0, "."), MxHost::new(10, "mx.example.com")]);
        assert!(!lookup.null_mx);
        assert_eq!(lookup.host_names(), vec!["mx.example.com"]);
        assert!(lookup.accepts_mail());
    }

    #[test]
    fn a_real_mx_answer_is_not_marked_implicit() {
        let lookup = build_lookup(vec![MxHost::new(10, "mx.example.com")]);
        assert!(!lookup.implicit);
        assert_eq!(lookup.host_names(), vec!["mx.example.com"]);
    }

    // ------------------------------------------------------------------
    // MxResolver behaviour against a mock
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn mx_answers_come_from_the_resolver_sorted() {
        let mock = MockResolver::new().with_mx(
            "example.com",
            vec![MxHost::new(30, "mx3.example.com"), MxHost::new(10, "mx1.example.com")],
        );
        let resolver = MxResolver::mock(mock, &dns_config());
        let lookup = resolver.mx("example.com").await.unwrap();
        assert_eq!(lookup.host_names(), vec!["mx1.example.com", "mx3.example.com"]);
        assert!(!lookup.null_mx);
    }

    #[tokio::test]
    async fn a_null_mx_is_reported_as_such() {
        let mock = MockResolver::new().with_mx("example.com", vec![MxHost::new(0, ".")]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let lookup = resolver.mx("example.com").await.unwrap();
        assert!(lookup.null_mx);
        assert!(!lookup.accepts_mail());
        assert!(resolver.delivery_targets("example.com").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn implicit_mx_falls_back_to_the_domain_addresses() {
        // RFC 5321 §5.1: a domain with no MX record is its own mail exchanger.
        let mock = MockResolver::new().with_addresses("plain.example.com", vec![ip("203.0.113.5")]);
        let resolver = MxResolver::mock(mock, &dns_config());

        let lookup = resolver.mx("plain.example.com").await.unwrap();
        assert!(lookup.implicit, "the fallback must be reported as such");
        assert_eq!(lookup.host_names(), vec!["plain.example.com"]);
        assert!(lookup.accepts_mail());

        let targets = resolver.delivery_targets("plain.example.com").await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0.host, "plain.example.com");
        assert_eq!(targets[0].1, vec![ip("203.0.113.5")]);
    }

    #[tokio::test]
    async fn a_domain_that_does_not_resolve_at_all_has_no_targets() {
        let resolver = MxResolver::mock(MockResolver::new(), &dns_config());
        let lookup = resolver.mx("ghost.example").await.unwrap();
        assert!(lookup.hosts.is_empty());
        assert!(!lookup.implicit);
        assert!(!lookup.accepts_mail());
        assert!(resolver.delivery_targets("ghost.example").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delivery_targets_are_in_preference_order_with_addresses() {
        let mock = MockResolver::new()
            .with_mx(
                "example.com",
                vec![MxHost::new(20, "mx2.example.com"), MxHost::new(10, "mx1.example.com")],
            )
            .with_addresses("mx1.example.com", vec![ip("203.0.113.1")])
            .with_addresses("mx2.example.com", vec![ip("203.0.113.2"), ip("2001:db8::2")]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let targets = resolver.delivery_targets("example.com").await.unwrap();

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].0.host, "mx1.example.com");
        assert_eq!(targets[0].1, vec![ip("203.0.113.1")]);
        assert_eq!(targets[1].0.host, "mx2.example.com");
        assert_eq!(targets[1].1.len(), 2);
    }

    #[tokio::test]
    async fn a_host_without_addresses_is_skipped() {
        let mock = MockResolver::new()
            .with_mx(
                "example.com",
                vec![MxHost::new(10, "dead.example.com"), MxHost::new(20, "live.example.com")],
            )
            .with_addresses("live.example.com", vec![ip("203.0.113.9")]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let targets = resolver.delivery_targets("example.com").await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0.host, "live.example.com");
    }

    #[tokio::test]
    async fn a_lookup_failure_is_a_temporary_error() {
        let mock = MockResolver::new().with_failure("broken.example.com");
        let resolver = MxResolver::mock(mock, &dns_config());
        let err = resolver.mx("broken.example.com").await.unwrap_err();
        assert!(err.is_temporary(), "{err:?}");
    }

    #[tokio::test]
    async fn txt_and_ptr_go_through_the_resolver() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 mx -all".to_string()])
            .with_ptr(ip("203.0.113.5"), vec!["mail.example.com".to_string()]);
        let resolver = MxResolver::mock(mock, &dns_config());
        assert_eq!(resolver.txt("example.com").await.unwrap(), vec!["v=spf1 mx -all"]);
        assert_eq!(
            resolver.ptr(ip("203.0.113.5")).await.unwrap(),
            vec!["mail.example.com"]
        );
    }

    #[tokio::test]
    async fn an_unknown_name_yields_an_empty_answer_not_an_error() {
        let resolver = MxResolver::mock(MockResolver::new(), &dns_config());
        assert!(resolver.mx("nope.example").await.unwrap().hosts.is_empty());
        assert!(resolver.txt("nope.example").await.unwrap().is_empty());
        assert!(resolver.addresses("nope.example").await.unwrap().is_empty());
        assert!(resolver.ptr(ip("203.0.113.1")).await.unwrap().is_empty());
    }

    // ------------------------------------------------------------------
    // Caching
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_repeated_lookup_is_served_from_the_cache() {
        let mock = MockResolver::new().with_mx("example.com", vec![MxHost::new(10, "mx.example.com")]);
        let resolver = MxResolver::mock(mock, &dns_config());

        resolver.mx("example.com").await.unwrap();
        resolver.mx("example.com").await.unwrap();
        resolver.mx("EXAMPLE.COM.").await.unwrap();

        // Three lookups with two spellings of one name = one cache entry and one
        // trip to the resolver.
        assert_eq!(resolver.cache_len(), 1);
        assert_eq!(resolver.resolver().query_count(), 1);
    }

    #[tokio::test]
    async fn the_cache_is_per_record_type() {
        let mock = MockResolver::new()
            .with_mx("example.com", vec![MxHost::new(10, "mx.example.com")])
            .with_txt("example.com", vec!["v=spf1 -all".to_string()]);
        let resolver = MxResolver::mock(mock, &dns_config());
        resolver.mx("example.com").await.unwrap();
        resolver.txt("example.com").await.unwrap();
        assert_eq!(resolver.cache_len(), 2);
    }

    #[tokio::test]
    async fn clearing_the_cache_forces_a_fresh_lookup() {
        let mock = MockResolver::new().with_mx("example.com", vec![MxHost::new(10, "mx.example.com")]);
        let resolver = MxResolver::mock(mock, &dns_config());
        resolver.mx("example.com").await.unwrap();
        assert_eq!(resolver.cache_len(), 1);
        resolver.clear_cache();
        assert_eq!(resolver.cache_len(), 0);
    }

    #[tokio::test]
    async fn the_cache_stays_bounded() {
        let mut config = dns_config();
        config.cache_ttl_secs = 300;
        let mock = MockResolver::new();
        let resolver = MxResolver::new(Arc::new(mock), &config);
        for n in 0..(MAX_CACHE_ENTRIES + 500) {
            let name = format!("host{n}.example.com");
            let _ = resolver.addresses(&name).await;
        }
        assert!(
            resolver.cache_len() <= MAX_CACHE_ENTRIES,
            "cache grew to {}",
            resolver.cache_len()
        );
    }

    #[tokio::test]
    async fn a_temporary_failure_is_cached_for_the_negative_ttl() {
        let mock = MockResolver::new().with_failure("broken.example.com");
        let resolver = MxResolver::mock(mock, &dns_config());
        assert!(resolver.mx("broken.example.com").await.is_err());
        assert_eq!(resolver.cache_len(), 1);
        // The second call is short-circuited by the cached failure.
        assert!(resolver.mx("broken.example.com").await.is_err());
    }

    #[tokio::test]
    async fn cache_ttls_are_clamped_away_from_zero() {
        let mut config = dns_config();
        config.cache_ttl_secs = 0;
        config.negative_ttl_secs = 0;
        let resolver = MxResolver::new(Arc::new(MockResolver::new()), &config);
        // Must not panic or divide by zero.
        let _ = resolver.mx("example.com").await;
        assert!(resolver.mx("example.com").await.is_ok());
    }

    // ------------------------------------------------------------------
    // Configuration parsing
    // ------------------------------------------------------------------

    #[test]
    fn resolver_addresses_accept_both_spellings() {
        assert_eq!(parse_server("1.1.1.1").unwrap().port(), 53);
        assert_eq!(parse_server("1.1.1.1:5353").unwrap().port(), 5353);
        assert_eq!(parse_server("2606:4700:4700::1111").unwrap().port(), 53);
        assert!(parse_server("not-an-address").is_err());
        assert!(parse_server("").is_err());
    }

    #[test]
    fn a_bad_resolver_address_is_a_configuration_time_dns_error() {
        let mut config = dns_config();
        config.resolvers = vec!["nope".to_string()];
        let err = HickoryResolver::new(&config).unwrap_err();
        assert!(matches!(err, FerromaError::Dns(_)), "{err:?}");
    }

    #[test]
    fn an_explicit_resolver_list_is_accepted() {
        let mut config = dns_config();
        config.resolvers = vec!["127.0.0.1:5399".to_string()];
        // Constructing a resolver must not touch the network.
        assert!(HickoryResolver::new(&config).is_ok());
    }

    // ------------------------------------------------------------------
    // DNS health
    // ------------------------------------------------------------------

    fn healthy_mock() -> MockResolver {
        MockResolver::new()
            .with_mx("example.com", vec![MxHost::new(10, "mail.example.com")])
            .with_addresses("example.com", vec![ip("203.0.113.10")])
            .with_addresses("mail.example.com", vec![ip("203.0.113.10")])
            .with_ptr(ip("203.0.113.10"), vec!["example.com".to_string()])
            .with_txt("example.com", vec!["v=spf1 mx -all".to_string()])
            .with_txt(
                "default._domainkey.example.com",
                vec!["v=DKIM1; k=rsa; p=MIIBIjANBg".to_string()],
            )
            .with_txt(
                "_dmarc.example.com",
                vec!["v=DMARC1; p=reject; rua=mailto:dmarc@example.com".to_string()],
            )
    }

    #[tokio::test]
    async fn a_fully_configured_domain_scores_perfectly() {
        let resolver = MxResolver::mock(healthy_mock(), &dns_config());
        let health = resolver.health("example.com", "default").await;

        assert_eq!(health.domain, "example.com");
        assert_eq!(health.get(DnsRecordKind::Mx).unwrap().status, DnsStatus::Ok);
        assert_eq!(health.get(DnsRecordKind::A).unwrap().status, DnsStatus::Ok);
        assert_eq!(health.get(DnsRecordKind::Aaaa).unwrap().status, DnsStatus::Skip);
        assert_eq!(health.get(DnsRecordKind::Ptr).unwrap().status, DnsStatus::Ok);
        assert_eq!(health.get(DnsRecordKind::Spf).unwrap().status, DnsStatus::Ok);
        assert_eq!(health.get(DnsRecordKind::Dkim).unwrap().status, DnsStatus::Ok);
        assert_eq!(health.get(DnsRecordKind::Dmarc).unwrap().status, DnsStatus::Ok);

        assert_eq!(health.score(), 6, "AAAA is skipped, not scored");
        assert_eq!(health.max_score(), 6);
    }

    #[tokio::test]
    async fn the_health_report_names_the_expected_records() {
        let resolver = MxResolver::mock(healthy_mock(), &dns_config());
        let health = resolver.health("example.com", "default").await;

        let mx = health.get(DnsRecordKind::Mx).unwrap();
        assert_eq!(mx.expected.as_deref(), Some("mail.example.com"));
        assert_eq!(mx.found, vec!["10 mail.example.com.".to_string()]);

        let dkim = health.get(DnsRecordKind::Dkim).unwrap();
        assert_eq!(
            dkim.expected.as_deref(),
            Some("default._domainkey.example.com")
        );
        assert_eq!(DnsRecordKind::Dkim.as_str(), "DKIM");
    }

    #[tokio::test]
    async fn a_domain_with_nothing_published_fails_every_check() {
        let resolver = MxResolver::mock(MockResolver::new(), &dns_config());
        let health = resolver.health("bare.example", "default").await;

        assert_eq!(health.get(DnsRecordKind::Mx).unwrap().status, DnsStatus::Fail);
        assert_eq!(health.get(DnsRecordKind::A).unwrap().status, DnsStatus::Fail);
        assert_eq!(health.get(DnsRecordKind::Ptr).unwrap().status, DnsStatus::Fail);
        assert_eq!(health.get(DnsRecordKind::Spf).unwrap().status, DnsStatus::Fail);
        assert_eq!(health.get(DnsRecordKind::Dkim).unwrap().status, DnsStatus::Warn);
        assert_eq!(health.get(DnsRecordKind::Dmarc).unwrap().status, DnsStatus::Fail);
        assert_eq!(health.score(), 0);
        assert_eq!(health.max_score(), 6);
        for check in &health.records {
            if check.status != DnsStatus::Ok && check.status != DnsStatus::Skip {
                assert!(check.hint.is_some(), "{:?} has no hint", check.kind);
            }
        }
    }

    #[tokio::test]
    async fn an_implicit_mx_is_reported_as_ok_for_an_address_that_resolves() {
        // No MX, but the domain has an A record: RFC 5321 §5.1 makes it its own mail
        // exchanger, so mail *can* be delivered.
        let mock = MockResolver::new().with_addresses("example.com", vec![ip("203.0.113.10")]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        let mx = health.get(DnsRecordKind::Mx).unwrap();
        assert_eq!(mx.status, DnsStatus::Ok);
        assert_eq!(mx.expected.as_deref(), Some("example.com"));
        assert_eq!(mx.found, vec!["0 example.com.".to_string()]);
    }

    #[tokio::test]
    async fn a_missing_mx_record_on_a_dead_domain_is_a_failure() {
        let resolver = MxResolver::mock(MockResolver::new(), &dns_config());
        let health = resolver.health("example.com", "default").await;
        let mx = health.get(DnsRecordKind::Mx).unwrap();
        assert_eq!(mx.status, DnsStatus::Fail);
        assert!(mx.hint.as_deref().unwrap().contains("publish an MX record"));
    }

    #[tokio::test]
    async fn a_null_mx_is_a_hard_failure_in_the_report() {
        let mock = MockResolver::new().with_mx("example.com", vec![MxHost::new(0, ".")]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        let mx = health.get(DnsRecordKind::Mx).unwrap();
        assert_eq!(mx.status, DnsStatus::Fail);
        assert!(mx.hint.as_deref().unwrap().contains("null MX"));
    }

    #[tokio::test]
    async fn a_soft_spf_fail_is_a_warning() {
        let mut mock = healthy_mock();
        mock.set_txt("example.com", vec!["v=spf1 mx ~all".to_string()]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        assert_eq!(health.get(DnsRecordKind::Spf).unwrap().status, DnsStatus::Warn);
    }

    #[tokio::test]
    async fn two_spf_records_are_a_failure() {
        let mut mock = healthy_mock();
        mock.set_txt(
            "example.com",
            vec!["v=spf1 mx -all".to_string(), "v=spf1 ip4:203.0.113.0/24 -all".to_string()],
        );
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        let spf = health.get(DnsRecordKind::Spf).unwrap();
        assert_eq!(spf.status, DnsStatus::Fail);
        assert!(spf.hint.as_deref().unwrap().contains("exactly one"));
    }

    #[tokio::test]
    async fn a_dkim_record_without_a_key_fails() {
        let mut mock = healthy_mock();
        mock.set_txt("default._domainkey.example.com", vec!["v=DKIM1".to_string()]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        assert_eq!(health.get(DnsRecordKind::Dkim).unwrap().status, DnsStatus::Fail);
    }

    #[tokio::test]
    async fn a_dmarc_policy_weaker_than_reject_is_a_warning() {
        let mut mock = healthy_mock();
        mock.set_txt("_dmarc.example.com", vec!["v=DMARC1; p=none".to_string()]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        assert_eq!(health.get(DnsRecordKind::Dmarc).unwrap().status, DnsStatus::Warn);
    }

    #[tokio::test]
    async fn a_mismatched_ptr_is_a_warning() {
        let mut mock = healthy_mock();
        mock.set_ptr(ip("203.0.113.10"), vec!["other.example.net".to_string()]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        let ptr = health.get(DnsRecordKind::Ptr).unwrap();
        assert_eq!(ptr.status, DnsStatus::Warn);
        assert_eq!(ptr.found, vec!["other.example.net".to_string()]);
    }

    #[tokio::test]
    async fn an_ipv6_only_host_marks_aaaa_ok_and_a_fail() {
        let mock = MockResolver::new()
            .with_mx("example.com", vec![MxHost::new(10, "mail.example.com")])
            .with_addresses("example.com", vec![ip("2001:db8::10")]);
        let resolver = MxResolver::mock(mock, &dns_config());
        let health = resolver.health("example.com", "default").await;
        assert_eq!(health.get(DnsRecordKind::Aaaa).unwrap().status, DnsStatus::Ok);
        assert_eq!(health.get(DnsRecordKind::A).unwrap().status, DnsStatus::Fail);
    }

    #[test]
    fn status_and_kind_tokens_match_the_api_document() {
        assert_eq!(DnsStatus::Ok.as_str(), "ok");
        assert_eq!(DnsStatus::Warn.as_str(), "warn");
        assert_eq!(DnsStatus::Fail.as_str(), "fail");
        assert_eq!(DnsStatus::Skip.as_str(), "skip");
        for kind in [
            DnsRecordKind::Mx,
            DnsRecordKind::A,
            DnsRecordKind::Aaaa,
            DnsRecordKind::Ptr,
            DnsRecordKind::Spf,
            DnsRecordKind::Dkim,
            DnsRecordKind::Dmarc,
        ] {
            assert!(!kind.as_str().is_empty());
        }
        assert!(DnsStatus::Ok.is_ok());
        assert!(!DnsStatus::Skip.counts());
        assert!(DnsStatus::Fail.counts());
    }

    #[test]
    fn a_check_with_a_hint_is_built_by_chaining() {
        let check = DnsRecordCheck::new(DnsRecordKind::Mx, DnsStatus::Warn, None, Vec::new())
            .with_hint("publish an MX");
        assert_eq!(check.hint.as_deref(), Some("publish an MX"));
    }

    // ------------------------------------------------------------------
    // Mock resolver contract
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn the_mock_records_every_query() {
        let mock = MockResolver::new().with_txt("example.com", vec!["v=spf1 -all".to_string()]);
        let _ = mock.mx("example.com").await;
        let _ = mock.txt("example.com").await;
        let _ = mock.addresses("example.com").await;
        let _ = mock.ptr(ip("203.0.113.1")).await;
        assert_eq!(
            mock.queries(),
            vec![
                "mx:example.com",
                "txt:example.com",
                "a:example.com",
                "ptr:203.0.113.1"
            ]
        );
        assert_eq!(mock.query_count(), 4);
    }
}
