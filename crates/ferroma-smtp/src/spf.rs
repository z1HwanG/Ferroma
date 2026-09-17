//! SPF: evaluating a domain's sender policy against a connecting client (RFC 7208).
//!
//! ```text
//!   SpfChecker::check(client_ip, MAIL FROM, EHLO name)
//!        │
//!        ├─ TXT <sender domain>            ──► exactly one v=spf1 record
//!        │        │
//!        │        └─ terms, left to right  ──► all / ip4 / ip6 / a / mx / ptr /
//!        │                                     exists / include / redirect
//!        └─ SpfOutcome { result, domain, explanation, lookups }
//! ```
//!
//! # The two things that make SPF dangerous
//!
//! * **Lookup amplification.** A record can chain `include` terms and force an
//!   unbounded number of DNS queries. RFC 7208 §4.6.4 caps the terms that cause a
//!   query at ten, and [`SpfChecker`] counts every one of them — `include`, `a`,
//!   `mx`, `ptr`, `exists` and `redirect`. Crossing the cap is a `permerror`, not a
//!   timeout.
//! * **Macro expansion.** `%{s}`, `%{l}` and friends splice attacker-influenced text
//!   into a DNS name. Expansion is bounded by length, rejects unknown and upper-case
//!   macro letters, and never builds a name longer than 253 octets.
//!
//! Everything DNS-facing goes through [`Resolver`], so the whole of RFC 7208 is
//! exercised in tests against [`MockResolver`](crate::mx::MockResolver) with no
//! network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ferroma_core::config::PolicyConfig;
use ferroma_core::{EmailAddress, FerromaError, Result};
use futures_util::future::BoxFuture;

use crate::mx::{normalise_name, Resolver};

/// The deepest an `include`/`redirect` chain is followed before it is a `permerror`.
///
/// The lookup counter already stops a runaway chain; this is the belt to its braces,
/// and it keeps the stack of a self-referential record bounded.
const MAX_DEPTH: usize = 20;

/// The longest a domain produced by macro expansion may be (RFC 1035 §2.3.4).
const MAX_DOMAIN_LEN: usize = 253;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The qualifier in front of a mechanism: what a match means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpfQualifier {
    /// `+` — authorised. The default when no qualifier is written.
    #[default]
    Pass,
    /// `-` — not authorised.
    Fail,
    /// `~` — probably not authorised, but don't be harsh about it.
    SoftFail,
    /// `?` — no assertion either way.
    Neutral,
}

impl SpfQualifier {
    /// The character that spells this qualifier in a record.
    pub fn as_char(self) -> char {
        match self {
            SpfQualifier::Pass => '+',
            SpfQualifier::Fail => '-',
            SpfQualifier::SoftFail => '~',
            SpfQualifier::Neutral => '?',
        }
    }

    /// Parse the character that spells a qualifier.
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            '+' => Some(SpfQualifier::Pass),
            '-' => Some(SpfQualifier::Fail),
            '~' => Some(SpfQualifier::SoftFail),
            '?' => Some(SpfQualifier::Neutral),
            _ => None,
        }
    }

    /// A word for the qualifier, for logs and explanations.
    pub fn as_str(self) -> &'static str {
        match self {
            SpfQualifier::Pass => "pass",
            SpfQualifier::Fail => "fail",
            SpfQualifier::SoftFail => "softfail",
            SpfQualifier::Neutral => "neutral",
        }
    }
}

/// One mechanism, with whatever arguments it carried.
///
/// The `ip4` and `ip6` variants carry their prefix length as well as their address
/// because RFC 7208 §5.6 gives both a CIDR suffix (`ip4:198.51.100.0/24`) and a
/// default (`/32` and `/128`); there is nowhere else to keep it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpfMechanism {
    /// `all` — matches everything.
    All,
    /// `ip4:<address>[/<prefix>]`.
    Ip4(IpAddr, u8),
    /// `ip6:<address>[/<prefix>]`.
    Ip6(IpAddr, u8),
    /// `a[:<domain>][/<v4 prefix>][//<v6 prefix>]`.
    A {
        /// The domain to look up; the current domain when absent.
        domain: Option<String>,
        /// The IPv4 prefix length, when written.
        cidr4: Option<u8>,
        /// The IPv6 prefix length, when written.
        cidr6: Option<u8>,
    },
    /// `mx[:<domain>][/<v4 prefix>][//<v6 prefix>]`.
    Mx {
        /// The domain whose exchangers to look up; the current domain when absent.
        domain: Option<String>,
        /// The IPv4 prefix length, when written.
        cidr4: Option<u8>,
        /// The IPv6 prefix length, when written.
        cidr6: Option<u8>,
    },
    /// `ptr[:<domain>]`.
    Ptr {
        /// The domain the reverse name must live under; the current domain when absent.
        domain: Option<String>,
    },
    /// `exists:<domain>` — matches when the name has an address.
    Exists {
        /// The macro string to expand into a name.
        domain: String,
    },
    /// `include:<domain>` — matches when that domain's policy authorises the client.
    Include {
        /// The domain whose policy to evaluate.
        domain: String,
    },
}

impl SpfMechanism {
    /// The mechanism's name as written in a record.
    pub fn name(&self) -> &'static str {
        match self {
            SpfMechanism::All => "all",
            SpfMechanism::Ip4(..) => "ip4",
            SpfMechanism::Ip6(..) => "ip6",
            SpfMechanism::A { .. } => "a",
            SpfMechanism::Mx { .. } => "mx",
            SpfMechanism::Ptr { .. } => "ptr",
            SpfMechanism::Exists { .. } => "exists",
            SpfMechanism::Include { .. } => "include",
        }
    }

    /// Whether evaluating this mechanism needs a DNS query, and so spends one of the
    /// ten RFC 7208 §4.6.4 lookups.
    pub fn costs_a_lookup(&self) -> bool {
        !matches!(self, SpfMechanism::All | SpfMechanism::Ip4(..) | SpfMechanism::Ip6(..))
    }
}

/// One mechanism and its qualifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpfTerm {
    /// What a match means.
    pub qualifier: SpfQualifier,
    /// What is matched against.
    pub mechanism: SpfMechanism,
}

/// A parsed `v=spf1` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpfRecord {
    /// The version token, `v=spf1`.
    pub version: String,
    /// The mechanisms, in the order they were written.
    pub terms: Vec<SpfTerm>,
    /// The `redirect=` modifier, when present.
    pub redirect: Option<String>,
    /// The `exp=` modifier, when present.
    pub explanation: Option<String>,
}

impl SpfRecord {
    /// Parse a record.
    ///
    /// The first token must be `v=spf1`. Unknown mechanisms, malformed CIDR lengths,
    /// a repeated `redirect=` or `exp=`, and a modifier with an empty value are all
    /// errors — RFC 7208 §4.6.4 makes every one of them a `permerror`, and a record
    /// that cannot be understood must not be silently treated as "no policy".
    pub fn parse(raw: &str) -> Result<Self> {
        let mut tokens = raw.split_whitespace();
        let version = tokens.next().ok_or_else(|| {
            FerromaError::Invalid("the SPF record is empty".to_string())
        })?;
        if !version.eq_ignore_ascii_case("v=spf1") {
            return Err(FerromaError::Invalid(format!(
                "the SPF record does not begin with v=spf1 (found {version:?})"
            )));
        }

        let mut record = SpfRecord {
            version: version.to_string(),
            terms: Vec::new(),
            redirect: None,
            explanation: None,
        };

        for token in tokens {
            if let Some((name, value)) = split_modifier(token) {
                let value = value.trim();
                if value.is_empty() {
                    return Err(FerromaError::Invalid(format!(
                        "the {name}= modifier has no value"
                    )));
                }
                match name.as_str() {
                    "redirect" => {
                        if record.redirect.is_some() {
                            return Err(FerromaError::Invalid(
                                "the SPF record has two redirect= modifiers".to_string(),
                            ));
                        }
                        record.redirect = Some(value.to_string());
                    }
                    "exp" => {
                        if record.explanation.is_some() {
                            return Err(FerromaError::Invalid(
                                "the SPF record has two exp= modifiers".to_string(),
                            ));
                        }
                        record.explanation = Some(value.to_string());
                    }
                    // RFC 7208 §6: unrecognised modifiers MUST be ignored.
                    _ => {}
                }
                continue;
            }
            record.terms.push(parse_term(token)?);
        }

        Ok(record)
    }

    /// The `redirect=` target, when present.
    pub fn redirect(&self) -> Option<&str> {
        self.redirect.as_deref()
    }

    /// The `exp=` explanation template, when present.
    pub fn explanation(&self) -> Option<&str> {
        self.explanation.as_deref()
    }

    /// How many of the terms cost one of the ten RFC 7208 §4.6.4 lookups.
    pub fn lookup_terms(&self) -> usize {
        self.terms
            .iter()
            .filter(|term| term.mechanism.costs_a_lookup())
            .count()
            + usize::from(self.redirect.is_some())
    }
}

/// Whether a token is a modifier, and if so its name and value.
///
/// A modifier is `name=value` and a mechanism never contains `=` before its first
/// `:` or `/`, so the shape is unambiguous.
fn split_modifier(token: &str) -> Option<(String, &str)> {
    let eq = token.find('=')?;
    if let Some(boundary) = token.find([':', '/']) {
        if boundary < eq {
            return None;
        }
    }
    let name = token[..eq].trim().to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    Some((name, &token[eq + 1..]))
}

/// Split a mechanism token into its name and the rest.
fn split_mechanism(token: &str) -> (&str, &str) {
    let end = token
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(token.len());
    (&token[..end], &token[end..])
}

/// Parse one mechanism term, with its qualifier.
fn parse_term(token: &str) -> Result<SpfTerm> {
    let (qualifier, rest) = match token.chars().next() {
        Some(c) if SpfQualifier::from_char(c).is_some() => (
            SpfQualifier::from_char(c).unwrap_or_default(),
            &token[c.len_utf8()..],
        ),
        _ => (SpfQualifier::Pass, token),
    };
    if rest.is_empty() {
        return Err(FerromaError::Invalid(format!(
            "the SPF term {token:?} is only a qualifier"
        )));
    }

    let (name, arguments) = split_mechanism(rest);
    let name_lower = name.to_ascii_lowercase();
    let mechanism = match name_lower.as_str() {
        "all" => {
            if !arguments.is_empty() {
                return Err(FerromaError::Invalid(format!(
                    "the all mechanism takes no arguments (found {arguments:?})"
                )));
            }
            SpfMechanism::All
        }
        "ip4" => {
            let (address, prefix) = parse_ip_argument(arguments, "ip4")?;
            match address {
                IpAddr::V4(address) => {
                    if prefix > 32 {
                        return Err(FerromaError::Invalid(format!(
                            "the ip4 prefix /{prefix} is out of range"
                        )));
                    }
                    SpfMechanism::Ip4(IpAddr::V4(address), prefix)
                }
                IpAddr::V6(_) => {
                    return Err(FerromaError::Invalid(
                        "ip4 must carry an IPv4 address".to_string(),
                    ))
                }
            }
        }
        "ip6" => {
            let (address, prefix) = parse_ip_argument(arguments, "ip6")?;
            match address {
                IpAddr::V6(address) => {
                    if prefix > 128 {
                        return Err(FerromaError::Invalid(format!(
                            "the ip6 prefix /{prefix} is out of range"
                        )));
                    }
                    SpfMechanism::Ip6(IpAddr::V6(address), prefix)
                }
                IpAddr::V4(_) => {
                    return Err(FerromaError::Invalid(
                        "ip6 must carry an IPv6 address".to_string(),
                    ))
                }
            }
        }
        "a" | "mx" => {
            let (domain, cidr4, cidr6) = parse_domain_and_cidrs(arguments, &name_lower)?;
            if name_lower == "a" {
                SpfMechanism::A {
                    domain,
                    cidr4,
                    cidr6,
                }
            } else {
                SpfMechanism::Mx {
                    domain,
                    cidr4,
                    cidr6,
                }
            }
        }
        "ptr" => {
            let domain = plain_domain_argument(arguments, "ptr")?;
            SpfMechanism::Ptr { domain }
        }
        "exists" => {
            let domain = plain_domain_argument(arguments, "exists")?.ok_or_else(|| {
                FerromaError::Invalid("the exists mechanism needs a domain".to_string())
            })?;
            SpfMechanism::Exists { domain }
        }
        "include" => {
            let domain = plain_domain_argument(arguments, "include")?.ok_or_else(|| {
                FerromaError::Invalid("the include mechanism needs a domain".to_string())
            })?;
            SpfMechanism::Include { domain }
        }
        _ => {
            return Err(FerromaError::Invalid(format!(
                "unknown SPF mechanism {name:?}"
            )))
        }
    };

    Ok(SpfTerm {
        qualifier,
        mechanism,
    })
}

/// Parse `:1.2.3.4[/24]` into an address and a prefix length.
fn parse_ip_argument(arguments: &str, name: &str) -> Result<(IpAddr, u8)> {
    let body = arguments.strip_prefix(':').ok_or_else(|| {
        FerromaError::Invalid(format!("the {name} mechanism needs an address after a colon"))
    })?;
    let (address, prefix) = match body.split_once('/') {
        Some((address, prefix)) => {
            let prefix: u8 = prefix.trim().parse().map_err(|_| {
                FerromaError::Invalid(format!("the {name} prefix {prefix:?} is not a number"))
            })?;
            (address, Some(prefix))
        }
        None => (body, None),
    };
    let address: IpAddr = address.trim().parse().map_err(|_| {
        FerromaError::Invalid(format!("{address:?} is not an IP address"))
    })?;
    let default = if address.is_ipv4() { 32 } else { 128 };
    Ok((address, prefix.unwrap_or(default)))
}

/// Parse `[:domain][/24][//64]` for `a` and `mx`.
fn parse_domain_and_cidrs(
    arguments: &str,
    name: &str,
) -> Result<(Option<String>, Option<u8>, Option<u8>)> {
    let mut rest = arguments;
    let mut domain = None;
    if let Some(after) = rest.strip_prefix(':') {
        let end = after.find('/').unwrap_or(after.len());
        let value = after[..end].trim();
        if value.is_empty() {
            return Err(FerromaError::Invalid(format!(
                "the {name} mechanism has an empty domain"
            )));
        }
        domain = Some(value.to_string());
        rest = &after[end..];
    }

    if rest.is_empty() {
        return Ok((domain, None, None));
    }
    let Some(after) = rest.strip_prefix('/') else {
        return Err(FerromaError::Invalid(format!(
            "unexpected {rest:?} after the {name} mechanism"
        )));
    };

    // dual-cidr-length = [ "/" ip4-cidr ] [ "/" "/" ip6-cidr ]: `a/24`, `a//64` and
    // `a/24//64` are the three legal spellings.
    let mut cidr4 = None;
    let mut cidr6 = None;
    if let Some(ipv6) = after.strip_prefix('/') {
        cidr6 = Some(parse_prefix(ipv6, 128, name)?);
    } else {
        let end = after.find('/').unwrap_or(after.len());
        cidr4 = Some(parse_prefix(&after[..end], 32, name)?);
        let tail = &after[end..];
        if !tail.is_empty() {
            let Some(ipv6) = tail.strip_prefix("//") else {
                return Err(FerromaError::Invalid(format!(
                    "the {name} mechanism has a malformed CIDR suffix ({rest:?})"
                )));
            };
            cidr6 = Some(parse_prefix(ipv6, 128, name)?);
        }
    }
    Ok((domain, cidr4, cidr6))
}

/// Parse a CIDR length, rejecting anything that is not digits within `max`.
fn parse_prefix(raw: &str, max: u8, name: &str) -> Result<u8> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return Err(FerromaError::Invalid(format!(
            "the {name} mechanism has a malformed CIDR length ({raw:?})"
        )));
    }
    let value: u8 = raw.parse().map_err(|_| {
        FerromaError::Invalid(format!("the {name} prefix {raw:?} is out of range"))
    })?;
    if value > max {
        return Err(FerromaError::Invalid(format!(
            "the {name} prefix /{value} is out of range"
        )));
    }
    Ok(value)
}

/// Parse `[:domain]` for `ptr`, `exists` and `include`.
fn plain_domain_argument(arguments: &str, name: &str) -> Result<Option<String>> {
    if arguments.is_empty() {
        return Ok(None);
    }
    let body = arguments.strip_prefix(':').ok_or_else(|| {
        FerromaError::Invalid(format!("unexpected {arguments:?} after the {name} mechanism"))
    })?;
    let body = body.trim();
    if body.is_empty() {
        return Err(FerromaError::Invalid(format!(
            "the {name} mechanism has an empty domain"
        )));
    }
    if body.contains('/') {
        return Err(FerromaError::Invalid(format!(
            "the {name} mechanism takes no CIDR length"
        )));
    }
    Ok(Some(body.to_string()))
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// The SPF result vocabulary of RFC 7208 §2.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpfResult {
    /// The domain publishes no policy, or does not exist.
    None,
    /// The policy makes no assertion about this client.
    Neutral,
    /// The client is authorised.
    Pass,
    /// The client is not authorised.
    Fail,
    /// The client is probably not authorised.
    SoftFail,
    /// A transient failure — usually DNS — left the question unanswered.
    TempError,
    /// The policy could not be evaluated at all.
    PermError,
}

impl SpfResult {
    /// The wire token used in `Authentication-Results` and `Received-SPF`.
    pub fn as_str(self) -> &'static str {
        match self {
            SpfResult::None => "none",
            SpfResult::Neutral => "neutral",
            SpfResult::Pass => "pass",
            SpfResult::Fail => "fail",
            SpfResult::SoftFail => "softfail",
            SpfResult::TempError => "temperror",
            SpfResult::PermError => "permerror",
        }
    }

    /// Whether this result authorises the client.
    pub fn is_pass(self) -> bool {
        self == SpfResult::Pass
    }

    /// The result a qualifier produces when its mechanism matches.
    pub fn from_qualifier(qualifier: SpfQualifier) -> Self {
        match qualifier {
            SpfQualifier::Pass => SpfResult::Pass,
            SpfQualifier::Fail => SpfResult::Fail,
            SpfQualifier::SoftFail => SpfResult::SoftFail,
            SpfQualifier::Neutral => SpfResult::Neutral,
        }
    }
}

impl std::fmt::Display for SpfResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything one SPF check produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpfOutcome {
    /// The result.
    pub result: SpfResult,
    /// The domain whose policy was evaluated.
    pub domain: Option<String>,
    /// The expanded `exp=` text, when the result is [`SpfResult::Fail`].
    pub explanation: Option<String>,
    /// How many DNS-lookup terms were evaluated.
    pub lookups: usize,
}

impl SpfOutcome {
    /// An outcome with no explanation.
    pub fn new(result: SpfResult, domain: Option<String>, lookups: usize) -> Self {
        SpfOutcome {
            result,
            domain,
            explanation: None,
            lookups,
        }
    }

    /// Whether the client was authorised.
    pub fn is_pass(&self) -> bool {
        self.result.is_pass()
    }
}

// ---------------------------------------------------------------------------
// Evaluation state
// ---------------------------------------------------------------------------

/// The counters and the include stack, shared by a whole check.
///
/// Interior mutability rather than `&mut` because the recursion is asynchronous:
/// each nested `evaluate` borrows the same state immutably, which is what lets the
/// compiler accept the recursive boxed future at all.
#[derive(Debug, Default)]
struct EvalState {
    /// How many lookup terms have been charged.
    lookups: AtomicUsize,
    /// The domains currently being evaluated, innermost last.
    stack: Mutex<Vec<String>>,
}

impl EvalState {
    /// How many lookups have been charged.
    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::Relaxed)
    }

    /// Take one of the ten lookups. `false` means the limit is spent.
    fn charge(&self, limit: usize) -> bool {
        let taken = self.lookups.fetch_add(1, Ordering::Relaxed) + 1;
        taken <= limit
    }

    /// Put `domain` on the stack, unless it is already there.
    fn enter(&self, domain: &str) -> bool {
        let mut stack = self.stack.lock().unwrap_or_else(PoisonError::into_inner);
        if stack.iter().any(|seen| seen == domain) {
            return false;
        }
        stack.push(domain.to_string());
        true
    }

    /// Take `domain` off the stack.
    fn leave(&self, domain: &str) {
        let mut stack = self.stack.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(position) = stack.iter().rposition(|seen| seen == domain) {
            stack.remove(position);
        }
    }
}

/// The values the macros expand against (RFC 7208 §7).
struct MacroContext<'a> {
    /// The connecting client's address.
    ip: IpAddr,
    /// The whole reverse-path, `local@domain`.
    sender: &'a str,
    /// The reverse-path's local part.
    local: &'a str,
    /// The reverse-path's domain — the domain being checked at the top level.
    sender_domain: &'a str,
    /// The domain of the record currently being evaluated.
    current_domain: String,
    /// The name the client announced in EHLO.
    helo: &'a str,
}

/// The parts of one check that never change as `include` and `redirect` recurse.
#[derive(Debug, Clone, Copy)]
struct CheckRequest<'a> {
    /// The connecting client's address.
    ip: IpAddr,
    /// The whole reverse-path.
    sender: &'a str,
    /// The reverse-path's local part.
    local: &'a str,
    /// The reverse-path's domain.
    sender_domain: &'a str,
    /// The name the client announced in EHLO.
    helo: &'a str,
}

impl<'a> CheckRequest<'a> {
    /// The macro context for a record published at `current_domain`.
    fn macros(&self, current_domain: &str) -> MacroContext<'a> {
        MacroContext {
            ip: self.ip,
            sender: self.sender,
            local: self.local,
            sender_domain: self.sender_domain,
            current_domain: current_domain.to_string(),
            helo: self.helo,
        }
    }
}

// ---------------------------------------------------------------------------
// The checker
// ---------------------------------------------------------------------------

/// Evaluates SPF for inbound mail.
#[derive(Debug, Clone)]
pub struct SpfChecker {
    resolver: Arc<dyn Resolver>,
    max_lookups: usize,
}

impl SpfChecker {
    /// Build a checker honouring `policy.spf_max_lookups`.
    pub fn new(resolver: Arc<dyn Resolver>, config: &PolicyConfig) -> Self {
        SpfChecker {
            resolver,
            max_lookups: config.spf_max_lookups.max(1),
        }
    }

    /// Override the RFC 7208 §4.6.4 lookup limit (the default is ten).
    pub fn with_max_lookups(mut self, n: usize) -> Self {
        self.max_lookups = n.max(1);
        self
    }

    /// The resolver this checker queries.
    pub fn resolver(&self) -> &Arc<dyn Resolver> {
        &self.resolver
    }

    /// The lookup limit in force.
    pub fn max_lookups(&self) -> usize {
        self.max_lookups
    }

    /// Evaluate SPF for a connection.
    ///
    /// `ip` is the connecting client, `mail_from` the envelope sender (`None`, or the
    /// empty string, for the null reverse-path `<>`, which has no domain to check and
    /// yields [`SpfResult::None`]), and `helo` the name the client announced.
    pub fn check(&self, ip: IpAddr, mail_from: Option<&str>, helo: &str) -> BoxFuture<'_, SpfOutcome> {
        // Owned copies let the returned future borrow only `self`, which keeps the
        // signature free of a second lifetime.
        let mail_from = mail_from.map(str::to_string);
        let helo = helo.trim().to_string();
        Box::pin(async move { self.check_inner(ip, mail_from.as_deref(), &helo).await })
    }

    /// The body of [`SpfChecker::check`].
    async fn check_inner(&self, ip: IpAddr, mail_from: Option<&str>, helo: &str) -> SpfOutcome {
        let Some(raw_sender) = mail_from else {
            return SpfOutcome::new(SpfResult::None, None, 0);
        };
        let sender = raw_sender.trim().trim_start_matches('<').trim_end_matches('>');
        if sender.is_empty() {
            // The null reverse-path: RFC 7208 §2.4 has no domain to check.
            return SpfOutcome::new(SpfResult::None, None, 0);
        }
        let Ok(address) = EmailAddress::parse(sender) else {
            // An address literal, or something else with no domain: nothing to check.
            return SpfOutcome::new(SpfResult::None, None, 0);
        };
        let sender_domain = normalise_name(address.domain());
        if sender_domain.is_empty() {
            return SpfOutcome::new(SpfResult::None, None, 0);
        }

        let state = EvalState::default();
        let request = CheckRequest {
            ip,
            sender,
            local: address.local_part(),
            sender_domain: &sender_domain,
            helo,
        };
        self.evaluate(&request, sender_domain.clone(), &state, 0).await
    }

    /// Evaluate one domain's record, recursing for `include` and `redirect`.
    fn evaluate<'a>(
        &'a self,
        request: &'a CheckRequest<'a>,
        domain: String,
        state: &'a EvalState,
        depth: usize,
    ) -> BoxFuture<'a, SpfOutcome> {
        Box::pin(async move {
            if depth > MAX_DEPTH {
                return SpfOutcome::new(SpfResult::PermError, Some(domain), state.lookups());
            }
            if !state.enter(&domain) {
                // A record that includes itself, directly or through a cycle.
                return SpfOutcome::new(SpfResult::PermError, Some(domain), state.lookups());
            }
            let outcome = self
                .evaluate_record(request, domain.clone(), state, depth)
                .await;
            state.leave(&domain);
            outcome
        })
    }

    /// Evaluate the record published at `domain`.
    async fn evaluate_record(
        &self,
        request: &CheckRequest<'_>,
        domain: String,
        state: &EvalState,
        depth: usize,
    ) -> SpfOutcome {
        let records = match self.resolver.txt(&domain).await {
            Ok(records) => records,
            Err(_) => {
                return SpfOutcome::new(SpfResult::TempError, Some(domain), state.lookups());
            }
        };
        let spf_records: Vec<&String> = records
            .iter()
            .filter(|raw| raw.trim_start().to_ascii_lowercase().starts_with("v=spf1"))
            .collect();

        match spf_records.len() {
            0 => return SpfOutcome::new(SpfResult::None, Some(domain), state.lookups()),
            // RFC 7208 §4.5: more than one record is a permerror.
            1 => {}
            _ => return SpfOutcome::new(SpfResult::PermError, Some(domain), state.lookups()),
        }
        let raw = match spf_records.first() {
            Some(raw) => (*raw).clone(),
            None => return SpfOutcome::new(SpfResult::None, Some(domain), state.lookups()),
        };
        let record = match SpfRecord::parse(&raw) {
            Ok(record) => record,
            Err(_) => return SpfOutcome::new(SpfResult::PermError, Some(domain), state.lookups()),
        };

        let context = request.macros(&domain);
        let ip = request.ip;

        for term in &record.terms {
            let matched: bool = match &term.mechanism {
                SpfMechanism::All => true,
                SpfMechanism::Ip4(address, prefix) | SpfMechanism::Ip6(address, prefix) => {
                    match (ip, address) {
                        (IpAddr::V4(client), IpAddr::V4(network)) => {
                            in_cidr4(client, *network, *prefix)
                        }
                        (IpAddr::V6(client), IpAddr::V6(network)) => {
                            in_cidr6(client, *network, *prefix)
                        }
                        _ => false,
                    }
                }
                SpfMechanism::A {
                    domain: target,
                    cidr4,
                    cidr6,
                } => {
                    if !state.charge(self.max_lookups) {
                        return SpfOutcome::new(
                            SpfResult::PermError,
                            Some(domain),
                            state.lookups(),
                        );
                    }
                    let target = match self.mechanism_domain(target, &context).await {
                        Ok(target) => target,
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::PermError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    };
                    match self.resolver.addresses(&target).await {
                        Ok(addresses) => matches_addresses(ip, &addresses, *cidr4, *cidr6),
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::TempError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    }
                }
                SpfMechanism::Mx {
                    domain: target,
                    cidr4,
                    cidr6,
                } => {
                    if !state.charge(self.max_lookups) {
                        return SpfOutcome::new(
                            SpfResult::PermError,
                            Some(domain),
                            state.lookups(),
                        );
                    }
                    let target = match self.mechanism_domain(target, &context).await {
                        Ok(target) => target,
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::PermError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    };
                    let hosts = match self.resolver.mx(&target).await {
                        Ok(hosts) => hosts,
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::TempError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    };
                    // RFC 7208 §5.7: the implicit-MX rule of RFC 5321 is NOT applied
                    // here; a domain with no MX records simply does not match.
                    let mut found = false;
                    for host in &hosts {
                        match self.resolver.addresses(&host.host).await {
                            Ok(addresses) => {
                                if matches_addresses(ip, &addresses, *cidr4, *cidr6) {
                                    found = true;
                                    break;
                                }
                            }
                            Err(_) => {
                                return SpfOutcome::new(
                                    SpfResult::TempError,
                                    Some(domain),
                                    state.lookups(),
                                )
                            }
                        }
                    }
                    found
                }
                SpfMechanism::Ptr { domain: target } => {
                    if !state.charge(self.max_lookups) {
                        return SpfOutcome::new(
                            SpfResult::PermError,
                            Some(domain),
                            state.lookups(),
                        );
                    }
                    let target = match self.mechanism_domain(target, &context).await {
                        Ok(target) => target,
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::PermError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    };
                    let names = match self.resolver.ptr(ip).await {
                        Ok(names) => names,
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::TempError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    };
                    let mut found = false;
                    for name in &names {
                        if !domain_matches(&target, name) {
                            continue;
                        }
                        // Forward-confirmed reverse DNS: the name only counts when it
                        // resolves back to the client.
                        match self.resolver.addresses(name).await {
                            Ok(addresses) => {
                                if addresses.contains(&ip) {
                                    found = true;
                                    break;
                                }
                            }
                            // A failure here just means this name does not validate.
                            Err(_) => continue,
                        }
                    }
                    found
                }
                SpfMechanism::Exists { domain: target } => {
                    if !state.charge(self.max_lookups) {
                        return SpfOutcome::new(
                            SpfResult::PermError,
                            Some(domain),
                            state.lookups(),
                        );
                    }
                    let expanded = match self.expand(target, &context).await {
                        Ok(name) => name,
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::PermError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    };
                    if expanded.is_empty() {
                        return SpfOutcome::new(
                            SpfResult::PermError,
                            Some(domain),
                            state.lookups(),
                        );
                    }
                    match self.resolver.addresses(&expanded).await {
                        Ok(addresses) => !addresses.is_empty(),
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::TempError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    }
                }
                SpfMechanism::Include { domain: target } => {
                    if !state.charge(self.max_lookups) {
                        return SpfOutcome::new(
                            SpfResult::PermError,
                            Some(domain),
                            state.lookups(),
                        );
                    }
                    let expanded = match self.expand(target, &context).await {
                        Ok(name) => name,
                        Err(_) => {
                            return SpfOutcome::new(
                                SpfResult::PermError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    };
                    if expanded.is_empty() {
                        return SpfOutcome::new(
                            SpfResult::PermError,
                            Some(domain),
                            state.lookups(),
                        );
                    }
                    let inner = self
                        .evaluate(request, normalise_name(&expanded), state, depth + 1)
                        .await;
                    match inner.result {
                        // RFC 7208 §5.2: only a pass makes the mechanism match, and
                        // a domain with no record at all is a permerror, not "none".
                        SpfResult::Pass => true,
                        SpfResult::Fail | SpfResult::SoftFail | SpfResult::Neutral => false,
                        SpfResult::TempError => {
                            return SpfOutcome::new(
                                SpfResult::TempError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                        SpfResult::PermError | SpfResult::None => {
                            return SpfOutcome::new(
                                SpfResult::PermError,
                                Some(domain),
                                state.lookups(),
                            )
                        }
                    }
                }
            };

            if matched {
                let result = SpfResult::from_qualifier(term.qualifier);
                let mut outcome = SpfOutcome::new(result, Some(domain), state.lookups());
                if result == SpfResult::Fail {
                    outcome.explanation = self.explanation(&record, &context).await;
                }
                return outcome;
            }
        }

        if let Some(target) = record.redirect() {
            if !state.charge(self.max_lookups) {
                return SpfOutcome::new(SpfResult::PermError, Some(domain), state.lookups());
            }
            let expanded = match self.expand(target, &context).await {
                Ok(name) => name,
                Err(_) => {
                    return SpfOutcome::new(SpfResult::PermError, Some(domain), state.lookups())
                }
            };
            if expanded.is_empty() {
                return SpfOutcome::new(SpfResult::PermError, Some(domain), state.lookups());
            }
            let inner = self
                .evaluate(request, normalise_name(&expanded), state, depth + 1)
                .await;
            // RFC 7208 §6.1: a redirect target with no record is a permerror.
            let result = match inner.result {
                SpfResult::None => SpfResult::PermError,
                other => other,
            };
            let mut outcome = SpfOutcome::new(result, Some(domain), state.lookups());
            if result == SpfResult::Fail {
                outcome.explanation = inner.explanation;
            }
            return outcome;
        }

        // No term matched and there is no redirect: RFC 7208 §4.6.1.
        SpfOutcome::new(SpfResult::Neutral, Some(domain), state.lookups())
    }

    /// The domain a mechanism looks at, expanded and defaulted to the current domain.
    async fn mechanism_domain(
        &self,
        target: &Option<String>,
        context: &MacroContext<'_>,
    ) -> std::result::Result<String, String> {
        match target {
            Some(template) => Ok(normalise_name(&self.expand(template, context).await?)),
            None => Ok(normalise_name(&context.current_domain)),
        }
    }

    /// The expanded `exp=` text, if the record has one and it resolves.
    ///
    /// A failure here never changes the result: the explanation is decoration.
    async fn explanation(
        &self,
        record: &SpfRecord,
        context: &MacroContext<'_>,
    ) -> Option<String> {
        let template = record.explanation()?;
        let name = self.expand(template, context).await.ok()?;
        if name.is_empty() {
            return None;
        }
        let records = self.resolver.txt(&name).await.ok()?;
        let first = records.first()?;
        self.expand(first, context).await.ok()
    }

    /// Expand a macro string (RFC 7208 §7).
    ///
    /// Returns `Err` with a reason for anything the RFC makes a `permerror`: an
    /// unknown escape, an unterminated `%{`, an upper-case macro letter, an unknown
    /// macro letter, or an expansion longer than a DNS name.
    async fn expand(
        &self,
        template: &str,
        context: &MacroContext<'_>,
    ) -> std::result::Result<String, String> {
        let mut out = String::with_capacity(template.len());
        let mut chars = template.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('_') => out.push(' '),
                Some('-') => out.push_str("%20"),
                Some('{') => {
                    let mut spec = String::new();
                    let mut closed = false;
                    for inner in chars.by_ref() {
                        if inner == '}' {
                            closed = true;
                            break;
                        }
                        spec.push(inner);
                    }
                    if !closed {
                        return Err("a macro is missing its closing brace".to_string());
                    }
                    out.push_str(&self.expand_macro(&spec, context).await?);
                }
                Some(other) => return Err(format!("the escape %{other} is not defined")),
                None => return Err("the record ends with a bare %".to_string()),
            }
        }
        if out.len() > MAX_DOMAIN_LEN {
            return Err("the expansion is longer than a domain name".to_string());
        }
        Ok(out)
    }

    /// Expand one `%{...}` body.
    async fn expand_macro(
        &self,
        spec: &str,
        context: &MacroContext<'_>,
    ) -> std::result::Result<String, String> {
        let mut chars = spec.chars();
        let letter = chars
            .next()
            .ok_or_else(|| "a macro has no name".to_string())?;
        if letter.is_ascii_uppercase() {
            return Err("macro letters are lower case".to_string());
        }

        let mut digits = String::new();
        let mut reverse = false;
        for c in chars {
            if c.is_ascii_digit() {
                if reverse {
                    return Err("a macro transformer has digits after the r".to_string());
                }
                digits.push(c);
            } else if c == 'r' {
                reverse = true;
            } else {
                return Err(format!("the macro transformer {c:?} is not defined"));
            }
        }

        let value = match letter {
            's' => context.sender.to_string(),
            'l' => context.local.to_string(),
            'o' => context.sender_domain.to_string(),
            'd' => context.current_domain.to_string(),
            'i' => ip_macro(context.ip),
            'c' => context.ip.to_string(),
            'h' => context.helo.to_string(),
            'v' => {
                if context.ip.is_ipv4() {
                    "in-addr".to_string()
                } else {
                    "ip6".to_string()
                }
            }
            'p' => self.validated_client_name(context.ip).await,
            _ => return Err(format!("the macro letter {letter:?} is not defined")),
        };

        let mut parts: Vec<&str> = value.split('.').collect();
        if let Ok(keep) = digits.parse::<usize>() {
            if keep == 0 {
                parts.clear();
            } else if parts.len() > keep {
                parts.drain(..parts.len() - keep);
            }
        }
        if reverse {
            parts.reverse();
        }
        Ok(parts.join("."))
    }

    /// The forward-confirmed reverse name of `ip`, or the empty string (RFC 7208 §7.3).
    ///
    /// `%{p}` is the one macro that costs a query of its own; RFC 7208 tells authors
    /// not to use it, and this returns nothing rather than failing when it cannot be
    /// validated.
    async fn validated_client_name(&self, ip: IpAddr) -> String {
        let Ok(names) = self.resolver.ptr(ip).await else {
            return String::new();
        };
        for name in names {
            if let Ok(addresses) = self.resolver.addresses(&name).await {
                if addresses.contains(&ip) {
                    return name;
                }
            }
        }
        String::new()
    }
}

/// The `%{i}` form of an address: dotted quad, or dot-separated nibbles for IPv6.
fn ip_macro(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            let octets = address.octets();
            let mut nibbles = Vec::with_capacity(32);
            for octet in octets {
                nibbles.push(format!("{:x}", octet >> 4));
                nibbles.push(format!("{:x}", octet & 0x0f));
            }
            nibbles.join(".")
        }
    }
}

/// Whether `child` equals `parent` or lives under it.
fn domain_matches(parent: &str, child: &str) -> bool {
    if parent.is_empty() || child.is_empty() {
        return false;
    }
    let child = normalise_name(child);
    child == parent || child.ends_with(&format!(".{parent}"))
}

/// Whether `client` is inside `network/prefix`.
fn in_cidr4(client: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let prefix = prefix.min(32);
    let mask = u32::MAX << (32 - u32::from(prefix));
    (u32::from(client) & mask) == (u32::from(network) & mask)
}

/// Whether `client` is inside `network/prefix`.
fn in_cidr6(client: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let prefix = prefix.min(128);
    let mask = u128::MAX << (128 - u32::from(prefix));
    (u128::from(client) & mask) == (u128::from(network) & mask)
}

/// Whether the client is among `addresses`, honouring the dual CIDR lengths.
///
/// The defaults are the single-address lengths: `/32` for IPv4 and `/128` for IPv6.
fn matches_addresses(
    client: IpAddr,
    addresses: &[IpAddr],
    cidr4: Option<u8>,
    cidr6: Option<u8>,
) -> bool {
    addresses.iter().any(|candidate| match (client, candidate) {
        (IpAddr::V4(client), IpAddr::V4(network)) => {
            in_cidr4(client, *network, cidr4.unwrap_or(32))
        }
        (IpAddr::V6(client), IpAddr::V6(network)) => {
            in_cidr6(client, *network, cidr6.unwrap_or(128))
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mx::MockResolver;

    fn ip(raw: &str) -> IpAddr {
        raw.parse().expect("test address")
    }

    fn policy_config() -> PolicyConfig {
        PolicyConfig::default()
    }

    fn checker(mock: MockResolver) -> SpfChecker {
        SpfChecker::new(Arc::new(mock), &policy_config())
    }

    /// A checker whose mock publishes `records` for `example.com`.
    fn with_record(records: &[&str]) -> SpfChecker {
        let mock = MockResolver::new().with_txt(
            "example.com",
            records.iter().map(|r| (*r).to_string()).collect(),
        );
        checker(mock)
    }

    async fn run(checker: &SpfChecker, ip: &str, sender: &str) -> SpfOutcome {
        checker
            .check(ip_of(ip), Some(sender), "mail.example.com")
            .await
    }

    fn ip_of(raw: &str) -> IpAddr {
        raw.parse().expect("test address")
    }

    // ------------------------------------------------------------------
    // Qualifiers
    // ------------------------------------------------------------------

    #[test]
    fn qualifiers_round_trip_through_their_characters() {
        for (c, qualifier) in [
            ('+', SpfQualifier::Pass),
            ('-', SpfQualifier::Fail),
            ('~', SpfQualifier::SoftFail),
            ('?', SpfQualifier::Neutral),
        ] {
            assert_eq!(SpfQualifier::from_char(c), Some(qualifier));
            assert_eq!(qualifier.as_char(), c);
        }
        assert_eq!(SpfQualifier::from_char('x'), None);
        assert_eq!(SpfQualifier::from_char(' '), None);
    }

    #[test]
    fn qualifier_names_match_their_results() {
        assert_eq!(SpfQualifier::Pass.as_str(), "pass");
        assert_eq!(SpfQualifier::Fail.as_str(), "fail");
        assert_eq!(SpfQualifier::SoftFail.as_str(), "softfail");
        assert_eq!(SpfQualifier::Neutral.as_str(), "neutral");
        assert_eq!(
            SpfResult::from_qualifier(SpfQualifier::SoftFail),
            SpfResult::SoftFail
        );
    }

    #[test]
    fn result_tokens_match_rfc_7208() {
        assert_eq!(SpfResult::None.as_str(), "none");
        assert_eq!(SpfResult::Neutral.as_str(), "neutral");
        assert_eq!(SpfResult::Pass.as_str(), "pass");
        assert_eq!(SpfResult::Fail.as_str(), "fail");
        assert_eq!(SpfResult::SoftFail.as_str(), "softfail");
        assert_eq!(SpfResult::TempError.as_str(), "temperror");
        assert_eq!(SpfResult::PermError.as_str(), "permerror");
        assert_eq!(SpfResult::Pass.to_string(), "pass");
        assert!(SpfResult::Pass.is_pass());
        assert!(!SpfResult::Fail.is_pass());
    }

    // ------------------------------------------------------------------
    // Record parsing
    // ------------------------------------------------------------------

    #[test]
    fn a_simple_record_parses() {
        let record = SpfRecord::parse("v=spf1 mx -all").expect("parses");
        assert_eq!(record.version, "v=spf1");
        assert_eq!(record.terms.len(), 2);
        assert_eq!(record.terms[0].mechanism, SpfMechanism::Mx {
            domain: None,
            cidr4: None,
            cidr6: None
        });
        assert_eq!(record.terms[0].qualifier, SpfQualifier::Pass);
        assert_eq!(record.terms[1].mechanism, SpfMechanism::All);
        assert_eq!(record.terms[1].qualifier, SpfQualifier::Fail);
        assert_eq!(record.redirect(), None);
        assert_eq!(record.explanation(), None);
    }

    #[test]
    fn the_version_token_is_case_insensitive_and_whitespace_tolerant() {
        let record = SpfRecord::parse("  V=SPF1   -all  ").expect("parses");
        assert_eq!(record.terms.len(), 1);
    }

    #[test]
    fn a_record_that_does_not_begin_with_v_spf1_is_rejected() {
        assert!(SpfRecord::parse("").is_err());
        assert!(SpfRecord::parse("v=spf2 -all").is_err());
        assert!(SpfRecord::parse("-all").is_err());
        assert!(SpfRecord::parse("v=spf1x -all").is_err());
    }

    #[test]
    fn every_qualifier_is_parsed() {
        let record = SpfRecord::parse("v=spf1 +all -all ~all ?all").expect("parses");
        let qualifiers: Vec<SpfQualifier> = record.terms.iter().map(|t| t.qualifier).collect();
        assert_eq!(
            qualifiers,
            vec![
                SpfQualifier::Pass,
                SpfQualifier::Fail,
                SpfQualifier::SoftFail,
                SpfQualifier::Neutral
            ]
        );
    }

    #[test]
    fn an_unqualified_mechanism_is_a_pass() {
        let record = SpfRecord::parse("v=spf1 all").expect("parses");
        assert_eq!(record.terms[0].qualifier, SpfQualifier::Pass);
    }

    #[test]
    fn ip4_and_ip6_mechanisms_parse_with_their_prefixes() {
        let record = SpfRecord::parse("v=spf1 ip4:198.51.100.0/24 ip6:2001:db8::/32 -all")
            .expect("parses");
        assert_eq!(
            record.terms[0].mechanism,
            SpfMechanism::Ip4(ip("198.51.100.0"), 24)
        );
        assert_eq!(
            record.terms[1].mechanism,
            SpfMechanism::Ip6(ip("2001:db8::"), 32)
        );
    }

    #[test]
    fn an_ip_mechanism_defaults_to_a_host_prefix() {
        let record = SpfRecord::parse("v=spf1 ip4:198.51.100.7 ip6:2001:db8::1").expect("parses");
        assert_eq!(
            record.terms[0].mechanism,
            SpfMechanism::Ip4(ip("198.51.100.7"), 32)
        );
        assert_eq!(
            record.terms[1].mechanism,
            SpfMechanism::Ip6(ip("2001:db8::1"), 128)
        );
    }

    #[test]
    fn a_and_mx_parse_the_dual_cidr_lengths() {
        let record =
            SpfRecord::parse("v=spf1 a:example.net/24//64 mx/24 mx//64 -all").expect("parses");
        assert_eq!(
            record.terms[0].mechanism,
            SpfMechanism::A {
                domain: Some("example.net".to_string()),
                cidr4: Some(24),
                cidr6: Some(64)
            }
        );
        assert_eq!(
            record.terms[1].mechanism,
            SpfMechanism::Mx {
                domain: None,
                cidr4: Some(24),
                cidr6: None
            }
        );
        assert_eq!(
            record.terms[2].mechanism,
            SpfMechanism::Mx {
                domain: None,
                cidr4: None,
                cidr6: Some(64)
            }
        );
        let both = SpfRecord::parse("v=spf1 a/24//64").expect("parses");
        assert_eq!(
            both.terms[0].mechanism,
            SpfMechanism::A {
                domain: None,
                cidr4: Some(24),
                cidr6: Some(64)
            }
        );
    }

    #[test]
    fn ptr_exists_and_include_parse() {
        let record =
            SpfRecord::parse("v=spf1 ptr ptr:example.net exists:%{i}.example.com include:_spf.example.net")
                .expect("parses");
        assert_eq!(
            record.terms[0].mechanism,
            SpfMechanism::Ptr { domain: None }
        );
        assert_eq!(
            record.terms[1].mechanism,
            SpfMechanism::Ptr {
                domain: Some("example.net".to_string())
            }
        );
        assert_eq!(
            record.terms[2].mechanism,
            SpfMechanism::Exists {
                domain: "%{i}.example.com".to_string()
            }
        );
        assert_eq!(
            record.terms[3].mechanism,
            SpfMechanism::Include {
                domain: "_spf.example.net".to_string()
            }
        );
    }

    #[test]
    fn redirect_and_exp_modifiers_parse() {
        let record = SpfRecord::parse("v=spf1 redirect=_spf.example.net exp=explain.example.net")
            .expect("parses");
        assert!(record.terms.is_empty());
        assert_eq!(record.redirect(), Some("_spf.example.net"));
        assert_eq!(record.explanation(), Some("explain.example.net"));
    }

    #[test]
    fn unknown_modifiers_are_ignored() {
        let record = SpfRecord::parse("v=spf1 -all whatever=1").expect("parses");
        assert_eq!(record.terms.len(), 1);
        assert_eq!(record.redirect(), None);
    }

    #[test]
    fn a_repeated_modifier_is_rejected() {
        assert!(SpfRecord::parse("v=spf1 redirect=a.example redirect=b.example").is_err());
        assert!(SpfRecord::parse("v=spf1 exp=a.example exp=b.example").is_err());
    }

    #[test]
    fn a_modifier_with_no_value_is_rejected() {
        assert!(SpfRecord::parse("v=spf1 redirect=").is_err());
        assert!(SpfRecord::parse("v=spf1 exp=").is_err());
    }

    #[test]
    fn unknown_mechanisms_are_rejected() {
        assert!(SpfRecord::parse("v=spf1 bogus -all").is_err());
        assert!(SpfRecord::parse("v=spf1 ip5:1.2.3.4 -all").is_err());
    }

    #[test]
    fn malformed_arguments_are_rejected() {
        assert!(SpfRecord::parse("v=spf1 all:foo").is_err());
        assert!(SpfRecord::parse("v=spf1 ip4:not-an-address").is_err());
        assert!(SpfRecord::parse("v=spf1 ip4:198.51.100.0/33").is_err());
        assert!(SpfRecord::parse("v=spf1 ip6:2001:db8::/129").is_err());
        assert!(SpfRecord::parse("v=spf1 ip4:2001:db8::/24").is_err());
        assert!(SpfRecord::parse("v=spf1 ip6:198.51.100.0/24").is_err());
        assert!(SpfRecord::parse("v=spf1 a/33").is_err());
        assert!(SpfRecord::parse("v=spf1 a//129").is_err());
        assert!(SpfRecord::parse("v=spf1 include").is_err());
        assert!(SpfRecord::parse("v=spf1 exists").is_err());
        assert!(SpfRecord::parse("v=spf1 ptr:nope/x").is_err());
        assert!(SpfRecord::parse("v=spf1 -").is_err());
        assert!(SpfRecord::parse("v=spf1").is_ok());
    }

    #[test]
    fn the_lookup_terms_are_counted_as_rfc_7208_says() {
        let record = SpfRecord::parse("v=spf1 ip4:1.2.3.4 a mx ptr exists:x include:y -all")
            .expect("parses");
        assert_eq!(record.lookup_terms(), 5);
        let redirect = SpfRecord::parse("v=spf1 ip4:1.2.3.4 redirect=x").expect("parses");
        assert_eq!(redirect.lookup_terms(), 1);
        assert!(!SpfMechanism::All.costs_a_lookup());
        assert!(!SpfMechanism::Ip4(ip("1.2.3.4"), 32).costs_a_lookup());
        assert!(SpfMechanism::Exists { domain: "x".into() }.costs_a_lookup());
    }

    #[test]
    fn mechanism_names_match_their_variants() {
        assert_eq!(SpfMechanism::All.name(), "all");
        assert_eq!(SpfMechanism::Ip4(ip("1.2.3.4"), 32).name(), "ip4");
        assert_eq!(SpfMechanism::Ip6(ip("::1"), 128).name(), "ip6");
        assert_eq!(
            SpfMechanism::A {
                domain: None,
                cidr4: None,
                cidr6: None
            }
            .name(),
            "a"
        );
        assert_eq!(
            SpfMechanism::Mx {
                domain: None,
                cidr4: None,
                cidr6: None
            }
            .name(),
            "mx"
        );
        assert_eq!(SpfMechanism::Ptr { domain: None }.name(), "ptr");
        assert_eq!(SpfMechanism::Exists { domain: "x".into() }.name(), "exists");
        assert_eq!(SpfMechanism::Include { domain: "x".into() }.name(), "include");
    }

    // ------------------------------------------------------------------
    // CIDR arithmetic
    // ------------------------------------------------------------------

    #[test]
    fn ipv4_cidr_matching_is_exact() {
        assert!(in_cidr4(
            Ipv4Addr::new(198, 51, 100, 7),
            Ipv4Addr::new(198, 51, 100, 0),
            24
        ));
        assert!(!in_cidr4(
            Ipv4Addr::new(198, 51, 101, 7),
            Ipv4Addr::new(198, 51, 100, 0),
            24
        ));
        assert!(in_cidr4(
            Ipv4Addr::new(198, 51, 100, 7),
            Ipv4Addr::new(198, 51, 100, 7),
            32
        ));
        assert!(in_cidr4(
            Ipv4Addr::new(8, 8, 8, 8),
            Ipv4Addr::new(1, 1, 1, 1),
            0
        ));
    }

    #[test]
    fn ipv6_cidr_matching_is_exact() {
        assert!(in_cidr6(
            "2001:db8::5".parse().expect("address"),
            "2001:db8::".parse().expect("address"),
            32
        ));
        assert!(!in_cidr6(
            "2001:db9::5".parse().expect("address"),
            "2001:db8::".parse().expect("address"),
            32
        ));
        assert!(in_cidr6(
            "2001:db8::5".parse().expect("address"),
            "::".parse().expect("address"),
            0
        ));
    }

    #[test]
    fn address_matching_honours_the_dual_cidr_lengths() {
        let addresses = vec![ip("198.51.100.4"), ip("2001:db8::4")];
        assert!(matches_addresses(ip("198.51.100.4"), &addresses, None, None));
        assert!(!matches_addresses(ip("198.51.100.5"), &addresses, None, None));
        assert!(matches_addresses(
            ip("198.51.100.5"),
            &addresses,
            Some(24),
            None
        ));
        assert!(matches_addresses(
            ip("2001:db8::9"),
            &addresses,
            None,
            Some(64)
        ));
        assert!(!matches_addresses(ip("203.0.113.1"), &addresses, Some(24), Some(64)));
        // An IPv4 client never matches an IPv6 record, or the reverse.
        assert!(!matches_addresses(ip("198.51.100.4"), &[ip("2001:db8::4")], None, None));
    }

    // ------------------------------------------------------------------
    // End-to-end evaluation
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_null_sender_is_none() {
        let checker = with_record(&["v=spf1 -all"]);
        let outcome = checker.check(ip("203.0.113.7"), None, "mail.example.com").await;
        assert_eq!(outcome.result, SpfResult::None);
        assert_eq!(outcome.domain, None);
        let empty = checker
            .check(ip("203.0.113.7"), Some("<>"), "mail.example.com")
            .await;
        assert_eq!(empty.result, SpfResult::None);
    }

    #[tokio::test]
    async fn a_sender_without_a_domain_is_none() {
        let checker = with_record(&["v=spf1 -all"]);
        for sender in ["nodomain", "@", "postmaster@[192.0.2.1]"] {
            let outcome = run(&checker, "203.0.113.7", sender).await;
            assert_eq!(outcome.result, SpfResult::None, "{sender}");
        }
    }

    #[tokio::test]
    async fn a_domain_with_no_record_is_none() {
        let checker = checker(MockResolver::new());
        let outcome = run(&checker, "203.0.113.7", "joe@example.com").await;
        assert_eq!(outcome.result, SpfResult::None);
        assert_eq!(outcome.domain.as_deref(), Some("example.com"));
    }

    #[tokio::test]
    async fn a_soft_and_hard_fail_are_reported_as_such() {
        let soft = run(&with_record(&["v=spf1 ~all"]), "203.0.113.7", "joe@example.com").await;
        assert_eq!(soft.result, SpfResult::SoftFail);
        let hard = run(&with_record(&["v=spf1 -all"]), "203.0.113.7", "joe@example.com").await;
        assert_eq!(hard.result, SpfResult::Fail);
    }

    #[tokio::test]
    async fn an_ip4_mechanism_matches_exactly() {
        let checker = with_record(&["v=spf1 ip4:203.0.113.7 -all"]);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Pass
        );
        assert_eq!(
            run(&checker, "203.0.113.8", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn an_ip4_cidr_mechanism_matches_a_network() {
        let checker = with_record(&["v=spf1 ip4:203.0.113.0/24 -all"]);
        assert!(run(&checker, "203.0.113.200", "joe@example.com")
            .await
            .is_pass());
        assert_eq!(
            run(&checker, "203.0.114.1", "joe@example.com").await.result,
            SpfResult::Fail
        );
        // An IPv6 client cannot match an ip4 term.
        assert_eq!(
            run(&checker, "2001:db8::1", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn an_ip6_cidr_mechanism_matches_a_prefix() {
        let checker = with_record(&["v=spf1 ip6:2001:db8::/32 -all"]);
        assert!(run(&checker, "2001:db8:1::9", "joe@example.com")
            .await
            .is_pass());
        assert_eq!(
            run(&checker, "2001:db9::9", "joe@example.com").await.result,
            SpfResult::Fail
        );
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn an_a_mechanism_uses_the_current_domain() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 a -all".to_string()])
            .with_addresses("example.com", vec![ip("203.0.113.7")]);
        let outcome = run(&checker(mock), "203.0.113.7", "joe@example.com").await;
        assert_eq!(outcome.result, SpfResult::Pass);
    }

    #[tokio::test]
    async fn an_a_mechanism_can_name_another_domain() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 a:mail.example.net -all".to_string()])
            .with_addresses("mail.example.net", vec![ip("203.0.113.7")]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn an_a_mechanism_matches_a_cidr() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 a/24 -all".to_string()])
            .with_addresses("example.com", vec![ip("203.0.113.0")]);
        assert!(run(&checker(mock), "203.0.113.55", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn an_mx_mechanism_matches_an_exchanger_address() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 mx -all".to_string()])
            .with_mx(
                "example.com",
                vec![crate::mx::MxHost::new(10, "mx1.example.com")],
            )
            .with_addresses("mx1.example.com", vec![ip("203.0.113.7")]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn an_mx_mechanism_does_not_apply_the_implicit_mx_rule() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 mx -all".to_string()])
            // The domain has an address but no MX record; RFC 7208 §5.7 says that
            // must not match.
            .with_addresses("example.com", vec![ip("203.0.113.7")]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn an_exists_mechanism_matches_a_resolvable_name() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 exists:ping.example.net -all".to_string()])
            .with_addresses("ping.example.net", vec![ip("203.0.113.1")]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn an_exists_mechanism_without_a_name_does_not_match() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 exists:gone.example.net -all".to_string()]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn an_exists_mechanism_expands_its_macros() {
        let mock = MockResolver::new()
            .with_txt(
                "example.com",
                vec!["v=spf1 exists:%{i}.example.net -all".to_string()],
            )
            .with_addresses("203.0.113.7.example.net", vec![ip("203.0.113.1")]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn a_ptr_mechanism_needs_forward_confirmation() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 ptr -all".to_string()])
            .with_ptr(ip("203.0.113.7"), vec!["mail.example.com".to_string()])
            .with_addresses("mail.example.com", vec![ip("203.0.113.7")]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn a_ptr_mechanism_rejects_a_name_that_does_not_resolve_back() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 ptr -all".to_string()])
            .with_ptr(ip("203.0.113.7"), vec!["mail.example.com".to_string()])
            .with_addresses("mail.example.com", vec![ip("203.0.113.99")]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn a_ptr_mechanism_rejects_a_name_outside_the_domain() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 ptr -all".to_string()])
            .with_ptr(ip("203.0.113.7"), vec!["mail.other.example".to_string()])
            .with_addresses("mail.other.example", vec![ip("203.0.113.7")]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn a_ptr_mechanism_can_name_the_domain_it_validates_against() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 ptr:example.net -all".to_string()])
            .with_ptr(ip("203.0.113.7"), vec!["mail.example.net".to_string()])
            .with_addresses("mail.example.net", vec![ip("203.0.113.7")]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn an_include_that_passes_matches() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 include:_spf.example.net -all".to_string()])
            .with_txt("_spf.example.net", vec!["v=spf1 ip4:203.0.113.7 -all".to_string()]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn an_include_that_fails_does_not_match() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 include:_spf.example.net -all".to_string()])
            .with_txt("_spf.example.net", vec!["v=spf1 ip4:198.51.100.1 -all".to_string()]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn an_include_of_a_domain_with_no_record_is_a_permanent_error() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 include:empty.example.net -all".to_string()]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    #[tokio::test]
    async fn a_nested_include_chain_resolves() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 include:a.example.net -all".to_string()])
            .with_txt("a.example.net", vec!["v=spf1 include:b.example.net -all".to_string()])
            .with_txt("b.example.net", vec!["v=spf1 ip4:203.0.113.7 -all".to_string()]);
        let outcome = run(&checker(mock), "203.0.113.7", "joe@example.com").await;
        assert_eq!(outcome.result, SpfResult::Pass);
        assert_eq!(outcome.lookups, 2);
    }

    #[tokio::test]
    async fn a_recursive_include_is_a_permanent_error() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 include:example.com -all".to_string()]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    #[tokio::test]
    async fn an_indirect_include_cycle_is_a_permanent_error() {
        let mock = MockResolver::new()
            .with_txt("a.example", vec!["v=spf1 include:b.example -all".to_string()])
            .with_txt("b.example", vec!["v=spf1 include:a.example -all".to_string()]);
        let outcome = checker(mock)
            .check(ip("203.0.113.7"), Some("joe@a.example"), "mail.a.example")
            .await;
        assert_eq!(outcome.result, SpfResult::PermError);
    }

    #[tokio::test]
    async fn a_redirect_replaces_the_result() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 redirect=_spf.example.net".to_string()])
            .with_txt("_spf.example.net", vec!["v=spf1 ip4:203.0.113.7 -all".to_string()]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn a_redirect_to_a_domain_with_no_record_is_a_permanent_error() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 redirect=empty.example.net".to_string()]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    #[tokio::test]
    async fn a_redirect_is_ignored_when_a_term_matched() {
        let mock = MockResolver::new()
            .with_txt(
                "example.com",
                vec!["v=spf1 ip4:203.0.113.7 redirect=broken.example.net".to_string()],
            );
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn a_record_with_no_match_is_neutral() {
        let checker = with_record(&["v=spf1 ip4:198.51.100.1"]);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Neutral
        );
    }

    #[tokio::test]
    async fn a_record_with_no_terms_is_neutral() {
        let checker = with_record(&["v=spf1"]);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Neutral
        );
    }

    #[tokio::test]
    async fn two_records_are_a_permanent_error() {
        let checker = with_record(&["v=spf1 -all", "v=spf1 +all"]);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    #[tokio::test]
    async fn non_spf_txt_records_are_ignored() {
        let checker = with_record(&["google-site-verification=abc", "v=spf1 -all"]);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::Fail
        );
    }

    #[tokio::test]
    async fn a_malformed_record_is_a_permanent_error() {
        let checker = with_record(&["v=spf1 bogus -all"]);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    #[tokio::test]
    async fn a_dns_failure_on_the_record_is_a_temporary_error() {
        let mock = MockResolver::new().with_failure("example.com");
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::TempError
        );
    }

    #[tokio::test]
    async fn a_dns_failure_inside_a_term_is_a_temporary_error() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 a:broken.example.net -all".to_string()])
            .with_failure("broken.example.net");
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::TempError
        );
    }

    // ------------------------------------------------------------------
    // The lookup limit
    // ------------------------------------------------------------------

    /// A record with `count` includes, each of which resolves to `-all`.
    fn include_chain(count: usize) -> MockResolver {
        let mut mock = MockResolver::new();
        let mut record = String::from("v=spf1");
        for index in 0..count {
            record.push_str(&format!(" include:i{index}.example.net"));
        }
        record.push_str(" -all");
        mock = mock.with_txt("example.com", vec![record]);
        for index in 0..count {
            mock = mock.with_txt(
                &format!("i{index}.example.net"),
                vec!["v=spf1 ip4:198.51.100.1 -all".to_string()],
            );
        }
        mock
    }

    #[tokio::test]
    async fn ten_lookups_are_allowed() {
        let outcome = run(
            &checker(include_chain(10)),
            "203.0.113.7",
            "joe@example.com",
        )
        .await;
        assert_eq!(outcome.result, SpfResult::Fail);
        assert_eq!(outcome.lookups, 10);
    }

    #[tokio::test]
    async fn eleven_lookups_are_a_permanent_error() {
        let outcome = run(
            &checker(include_chain(11)),
            "203.0.113.7",
            "joe@example.com",
        )
        .await;
        assert_eq!(outcome.result, SpfResult::PermError);
        assert_eq!(outcome.lookups, 11);
    }

    #[tokio::test]
    async fn the_lookup_limit_can_be_lowered() {
        let checker = checker(include_chain(3)).with_max_lookups(2);
        assert_eq!(checker.max_lookups(), 2);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    #[tokio::test]
    async fn the_lookup_limit_comes_from_the_policy_configuration() {
        let config = PolicyConfig {
            spf_max_lookups: 4,
            ..PolicyConfig::default()
        };
        let checker = SpfChecker::new(Arc::new(include_chain(5)), &config);
        assert_eq!(checker.max_lookups(), 4);
        assert_eq!(
            run(&checker, "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    #[tokio::test]
    async fn the_lookup_count_is_reported() {
        let mock = MockResolver::new()
            .with_txt(
                "example.com",
                vec!["v=spf1 a:one.example mx:two.example ip4:198.51.100.1 -all".to_string()],
            )
            .with_addresses("one.example", vec![ip("203.0.113.7")]);
        let outcome = run(&checker(mock), "203.0.113.7", "joe@example.com").await;
        assert_eq!(outcome.result, SpfResult::Pass);
        assert_eq!(outcome.lookups, 1);
    }

    // ------------------------------------------------------------------
    // Macros
    // ------------------------------------------------------------------

    fn macro_context() -> MacroContext<'static> {
        MacroContext {
            ip: ip_of("203.0.113.7"),
            sender: "joe@example.com",
            local: "joe",
            sender_domain: "example.com",
            current_domain: "example.com".to_string(),
            helo: "mail.example.com",
        }
    }

    #[tokio::test]
    async fn every_macro_letter_expands() {
        let checker = checker(MockResolver::new());
        let context = macro_context();
        assert_eq!(
            checker.expand("%{s}", &context).await.expect("expands"),
            "joe@example.com"
        );
        assert_eq!(checker.expand("%{l}", &context).await.expect("expands"), "joe");
        assert_eq!(
            checker.expand("%{o}", &context).await.expect("expands"),
            "example.com"
        );
        assert_eq!(
            checker.expand("%{d}", &context).await.expect("expands"),
            "example.com"
        );
        assert_eq!(
            checker.expand("%{i}", &context).await.expect("expands"),
            "203.0.113.7"
        );
        assert_eq!(
            checker.expand("%{c}", &context).await.expect("expands"),
            "203.0.113.7"
        );
        assert_eq!(
            checker.expand("%{h}", &context).await.expect("expands"),
            "mail.example.com"
        );
        assert_eq!(
            checker.expand("%{v}", &context).await.expect("expands"),
            "in-addr"
        );
    }

    #[tokio::test]
    async fn the_ip_version_macro_follows_the_client() {
        let checker = checker(MockResolver::new());
        let mut context = macro_context();
        context.ip = ip_of("2001:db8::1");
        assert_eq!(checker.expand("%{v}", &context).await.expect("expands"), "ip6");
        assert!(checker
            .expand("%{i}", &context)
            .await
            .expect("expands")
            .starts_with("2.0.0.1.0.d.b.8"));
    }

    #[tokio::test]
    async fn the_reverse_transformer_reverses_the_parts() {
        let checker = checker(MockResolver::new());
        let context = macro_context();
        assert_eq!(
            checker.expand("%{ir}", &context).await.expect("expands"),
            "7.113.0.203"
        );
        assert_eq!(
            checker.expand("%{dr}", &context).await.expect("expands"),
            "com.example"
        );
        assert_eq!(
            checker.expand("%{lr}", &context).await.expect("expands"),
            "joe"
        );
    }

    #[tokio::test]
    async fn the_digits_transformer_keeps_the_rightmost_parts() {
        let checker = checker(MockResolver::new());
        let context = macro_context();
        let mut domain = macro_context();
        domain.current_domain = "a.b.c.example.com".to_string();
        assert_eq!(
            checker.expand("%{d2}", &domain).await.expect("expands"),
            "example.com"
        );
        assert_eq!(
            checker.expand("%{d3}", &domain).await.expect("expands"),
            "c.example.com"
        );
        assert_eq!(
            checker.expand("%{d1}", &domain).await.expect("expands"),
            "com"
        );
        assert_eq!(checker.expand("%{d0}", &context).await.expect("expands"), "");
        assert_eq!(
            checker.expand("%{d2r}", &domain).await.expect("expands"),
            "com.example"
        );
    }

    #[tokio::test]
    async fn the_ip_macro_uses_nibbles_for_ipv6() {
        assert_eq!(ip_macro(ip_of("203.0.113.7")), "203.0.113.7");
        let nibbles = ip_macro(ip_of("2001:db8::1"));
        assert_eq!(nibbles.split('.').count(), 32);
        assert!(nibbles.starts_with("2.0.0.1.0.d.b.8"));
        assert!(nibbles.ends_with("0.1"));
    }

    #[tokio::test]
    async fn a_literal_percent_and_the_space_and_plus_escapes_expand() {
        let checker = checker(MockResolver::new());
        let context = macro_context();
        assert_eq!(checker.expand("100%%", &context).await.expect("expands"), "100%");
        assert_eq!(checker.expand("a%_b", &context).await.expect("expands"), "a b");
        assert_eq!(
            checker.expand("a%-b", &context).await.expect("expands"),
            "a%20b"
        );
    }

    #[tokio::test]
    async fn an_unknown_macro_letter_is_an_error() {
        let checker = checker(MockResolver::new());
        let context = macro_context();
        assert!(checker.expand("%{z}", &context).await.is_err());
        assert!(checker.expand("%{S}", &context).await.is_err());
        assert!(checker.expand("%{ix}", &context).await.is_err());
        assert!(checker.expand("%{s", &context).await.is_err());
        assert!(checker.expand("%", &context).await.is_err());
        assert!(checker.expand("%q", &context).await.is_err());
        assert!(checker.expand("%{}", &context).await.is_err());
    }

    #[tokio::test]
    async fn an_over_long_expansion_is_an_error() {
        let checker = checker(MockResolver::new());
        let long = "a".repeat(300);
        let context = MacroContext {
            ip: ip_of("203.0.113.7"),
            sender: "joe@example.com",
            local: "joe",
            sender_domain: "example.com",
            current_domain: long,
            helo: "mail.example.com",
        };
        assert!(checker.expand("%{d}", &context).await.is_err());
    }

    #[tokio::test]
    async fn the_p_macro_resolves_the_forward_confirmed_client_name() {
        let mock = MockResolver::new()
            .with_ptr(ip("203.0.113.7"), vec!["mail.example.com".to_string()])
            .with_addresses("mail.example.com", vec![ip("203.0.113.7")]);
        let checker = checker(mock);
        assert_eq!(
            checker
                .expand("%{p}", &macro_context())
                .await
                .expect("expands"),
            "mail.example.com"
        );
    }

    #[tokio::test]
    async fn the_p_macro_is_empty_without_a_validated_name() {
        let unresolved = checker(MockResolver::new());
        assert_eq!(
            unresolved
                .expand("%{p}", &macro_context())
                .await
                .expect("expands"),
            ""
        );
        // A PTR that does not resolve back to the client is not valid either.
        let mock = MockResolver::new()
            .with_ptr(ip("203.0.113.7"), vec!["mail.example.com".to_string()])
            .with_addresses("mail.example.com", vec![ip("198.51.100.1")]);
        assert_eq!(
            checker(mock)
                .expand("%{p}", &macro_context())
                .await
                .expect("expands"),
            ""
        );
    }

    #[tokio::test]
    async fn a_macro_is_expanded_inside_a_mechanism_argument() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 a:%{d} -all".to_string()])
            .with_addresses("example.com", vec![ip("203.0.113.7")]);
        assert!(run(&checker(mock), "203.0.113.7", "joe@example.com")
            .await
            .is_pass());
    }

    #[tokio::test]
    async fn a_malformed_macro_in_a_mechanism_is_a_permanent_error() {
        let mock = MockResolver::new()
            .with_txt("example.com", vec!["v=spf1 a:%{Z} -all".to_string()])
            .with_addresses("example.com", vec![ip("203.0.113.7")]);
        assert_eq!(
            run(&checker(mock), "203.0.113.7", "joe@example.com").await.result,
            SpfResult::PermError
        );
    }

    // ------------------------------------------------------------------
    // The exp= explanation
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_fail_fetches_and_expands_the_explanation() {
        let mock = MockResolver::new()
            .with_txt(
                "example.com",
                vec!["v=spf1 -all exp=explain.example.net".to_string()],
            )
            .with_txt(
                "explain.example.net",
                vec!["%{i} is not allowed to send mail for %{d}".to_string()],
            );
        let outcome = run(&checker(mock), "203.0.113.7", "joe@example.com").await;
        assert_eq!(outcome.result, SpfResult::Fail);
        assert_eq!(
            outcome.explanation.as_deref(),
            Some("203.0.113.7 is not allowed to send mail for example.com")
        );
    }

    #[tokio::test]
    async fn a_pass_does_not_fetch_the_explanation() {
        let mock = MockResolver::new()
            .with_txt(
                "example.com",
                vec!["v=spf1 +all exp=explain.example.net".to_string()],
            )
            .with_txt("explain.example.net", vec!["never used".to_string()]);
        let outcome = run(&checker(mock), "203.0.113.7", "joe@example.com").await;
        assert_eq!(outcome.result, SpfResult::Pass);
        assert_eq!(outcome.explanation, None);
    }

    #[tokio::test]
    async fn an_unresolvable_explanation_leaves_the_result_alone() {
        let mock = MockResolver::new().with_txt(
            "example.com",
            vec!["v=spf1 -all exp=gone.example.net".to_string()],
        );
        let outcome = run(&checker(mock), "203.0.113.7", "joe@example.com").await;
        assert_eq!(outcome.result, SpfResult::Fail);
        assert_eq!(outcome.explanation, None);
    }

    // ------------------------------------------------------------------
    // Plumbing
    // ------------------------------------------------------------------

    #[test]
    fn a_checker_reports_itself() {
        let checker = with_record(&["v=spf1 -all"]);
        assert_eq!(checker.max_lookups(), 10);
        assert_eq!(checker.resolver().query_count(), 0);
    }

    #[test]
    fn a_zero_lookup_limit_is_clamped_to_one() {
        let checker = with_record(&["v=spf1 -all"]).with_max_lookups(0);
        assert_eq!(checker.max_lookups(), 1);
    }

    #[tokio::test]
    async fn the_checked_domain_is_the_sender_domain() {
        let mock = MockResolver::new().with_txt(
            "example.com",
            vec!["v=spf1 ip4:203.0.113.7 -all".to_string()],
        );
        let outcome = run(&checker(mock), "203.0.113.7", "Joe@Example.COM").await;
        assert_eq!(outcome.domain.as_deref(), Some("example.com"));
        assert!(outcome.is_pass());
    }

    #[tokio::test]
    async fn the_envelope_sender_is_read_from_an_angle_bracketed_path() {
        let mock = MockResolver::new().with_txt(
            "example.com",
            vec!["v=spf1 ip4:203.0.113.7 -all".to_string()],
        );
        let outcome = checker(mock)
            .check(ip("203.0.113.7"), Some("<joe@example.com>"), "mail.example.com")
            .await;
        assert!(outcome.is_pass());
    }

    #[test]
    fn outcome_helpers_describe_themselves() {
        let outcome = SpfOutcome::new(SpfResult::Pass, Some("example.com".into()), 3);
        assert!(outcome.is_pass());
        assert_eq!(outcome.lookups, 3);
        assert_eq!(outcome.explanation, None);
    }
}
