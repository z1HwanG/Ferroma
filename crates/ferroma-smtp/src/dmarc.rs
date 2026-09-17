//! DMARC: what a domain asks receivers to do when SPF and DKIM both fail
//! (RFC 7489).
//!
//! SPF and DKIM each answer a narrow question — "was this client allowed to use that
//! envelope domain", "did that domain sign these octets". Neither looks at the
//! `From:` header a human actually reads. DMARC is the layer that ties them together:
//!
//! ```text
//!   From: joe@example.com
//!         │
//!         ▼
//!   TXT _dmarc.example.com ──► v=DMARC1; p=quarantine; adkim=s; aspf=r; pct=100
//!         │
//!         ├─ SPF pass under an aligned domain?   ──┐
//!         ├─ DKIM pass under an aligned domain?  ──┴──► pass | fail
//!         │
//!         └─ no record at the exact domain?  ──► try the organizational domain,
//!                                                and use `sp=` if it is there
//! ```
//!
//! # Alignment
//!
//! *[`DmarcAlignment::Relaxed`]* compares organizational domains, so
//! `mail.example.com` aligns with `example.com`. *[`DmarcAlignment::Strict`]*
//! requires the two to be identical. `adkim` governs the DKIM `d=` domain and `aspf`
//! the SPF envelope domain; both default to relaxed.
//!
//! # What is deliberately lenient
//!
//! A record that cannot be understood must fail closed, so a missing or wrong `v=`,
//! a missing or invalid `p=`, an invalid `sp=` and an out-of-range `pct=` are all
//! errors. Tags the RFC registers with a natural default (`adkim`, `aspf`, `fo`,
//! `rf`, `ri`) fall back to that default instead, and unknown tags are ignored —
//! which is exactly what the "ignore unknown tags" rule of RFC 7489 §6.3 asks for.

use std::sync::Arc;

use ferroma_core::config::PolicyConfig;
use ferroma_core::FerromaError;
use futures_util::future::BoxFuture;

use crate::dkim::{DkimResult, DkimVerdict};
use crate::mx::{normalise_name, Resolver};
use crate::spf::{SpfOutcome, SpfResult};

/// Public suffixes that take three labels to reach an organizational domain.
///
/// RFC 7489 §3.2 defines the organizational domain through the Public Suffix List.
/// Vendoring the whole list is not worth its weight for a single comparison, so the
/// common multi-label suffixes are listed explicitly; anything not listed is treated
/// as a single-label suffix, which is the right answer for every `.com`, `.net`,
/// `.org` and country second-level domain that is not in this table.
const TWO_LABEL_SUFFIXES: &[&str] = &[
    "ac.uk", "co.uk", "gov.uk", "ltd.uk", "me.uk", "net.uk", "nhs.uk", "org.uk", "plc.uk", "sch.uk",
    "asn.au", "com.au", "edu.au", "gov.au", "id.au", "net.au", "org.au",
    "ac.nz", "co.nz", "geek.nz", "gen.nz", "govt.nz", "net.nz", "org.nz", "school.nz",
    "ac.jp", "co.jp", "ed.jp", "go.jp", "gr.jp", "lg.jp", "ne.jp", "or.jp",
    "ac.in", "co.in", "edu.in", "firm.in", "gen.in", "gov.in", "ind.in", "net.in", "org.in",
    "com.br", "com.cn", "com.hk", "com.mx", "com.sg", "com.tr", "com.tw", "com.ar", "com.pl",
    "co.il", "co.kr", "co.za", "co.at", "co.hu", "co.id", "co.it", "co.jp", "co.ke", "co.th",
    "edu.cn", "gov.cn", "net.cn", "org.cn", "gov.br", "org.br", "net.br",
];

/// What a domain asks receivers to do with mail that fails DMARC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmarcPolicy {
    /// Take no action; report only.
    None,
    /// Treat the message as suspicious — for Ferroma, file it in `Junk`.
    Quarantine,
    /// Refuse the message.
    Reject,
}

impl DmarcPolicy {
    /// The wire token used in `p=` and `sp=`.
    pub fn as_str(self) -> &'static str {
        match self {
            DmarcPolicy::None => "none",
            DmarcPolicy::Quarantine => "quarantine",
            DmarcPolicy::Reject => "reject",
        }
    }

    /// Parse a policy token. Matching is case-insensitive, as RFC 7489 §6.3 requires
    /// of tag values, and anything else is `None`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" => Some(DmarcPolicy::None),
            "quarantine" => Some(DmarcPolicy::Quarantine),
            "reject" => Some(DmarcPolicy::Reject),
            _ => None,
        }
    }

    /// Whether this policy does anything at all.
    pub fn is_enforcing(self) -> bool {
        !matches!(self, DmarcPolicy::None)
    }
}

impl std::fmt::Display for DmarcPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How strictly an authenticated domain must match the `From:` domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DmarcAlignment {
    /// Compare organizational domains: `mail.example.com` aligns with `example.com`.
    #[default]
    Relaxed,
    /// Require the domains to be identical.
    Strict,
}

impl DmarcAlignment {
    /// Parse `r` or `s`, defaulting to relaxed for anything else.
    ///
    /// The RFC gives both tags a default and does not make an unparsable value fatal,
    /// so an unknown token degrades to the default rather than voiding the record.
    pub fn parse(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("s") {
            DmarcAlignment::Strict
        } else {
            DmarcAlignment::Relaxed
        }
    }

    /// The wire token used in `adkim=` and `aspf=`.
    pub fn as_str(self) -> &'static str {
        match self {
            DmarcAlignment::Relaxed => "r",
            DmarcAlignment::Strict => "s",
        }
    }

    /// Whether an authenticated domain aligns with the `From:` domain.
    pub fn aligns(self, from_domain: &str, authenticated_domain: &str) -> bool {
        let from = normalise_name(from_domain);
        let authenticated = normalise_name(authenticated_domain);
        if from.is_empty() || authenticated.is_empty() {
            return false;
        }
        match self {
            DmarcAlignment::Strict => from == authenticated,
            DmarcAlignment::Relaxed => {
                DmarcChecker::organizational_domain(&from)
                    == DmarcChecker::organizational_domain(&authenticated)
            }
        }
    }
}

/// A parsed `v=DMARC1` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmarcRecord {
    /// `v=` — the record version, `DMARC1`.
    pub version: String,
    /// `p=` — the policy for the domain the record was found at.
    pub policy: DmarcPolicy,
    /// `sp=` — the policy for subdomains, when one is published.
    pub subdomain_policy: Option<DmarcPolicy>,
    /// `rua=` — where aggregate reports are sent.
    pub rua: Vec<String>,
    /// `ruf=` — where failure reports are sent.
    pub ruf: Vec<String>,
    /// `pct=` — the percentage of failing messages the policy applies to.
    pub pct: u8,
    /// `adkim=` — how strictly the DKIM `d=` domain must align.
    pub adkim: DmarcAlignment,
    /// `aspf=` — how strictly the SPF domain must align.
    pub aspf: DmarcAlignment,
    /// `fo=` — which failures produce a report.
    pub fo: String,
    /// `rf=` — the report formats the domain accepts.
    pub rf: Vec<String>,
    /// `ri=` — the report interval in seconds.
    pub ri: u64,
}

impl DmarcRecord {
    /// Parse a record.
    ///
    /// `v=` and `p=` are required, `p=`/`sp=` must name a policy and `pct=` must be
    /// between 0 and 100 (RFC 7489 §6.4); anything else falls back to its default.
    pub fn parse(raw: &str) -> Result<Self, FerromaError> {
        let tags = parse_tags(raw);

        let version = tag(&tags, "v").ok_or_else(|| {
            FerromaError::Invalid("the DMARC record has no v= tag".to_string())
        })?;
        if !version.eq_ignore_ascii_case("DMARC1") {
            return Err(FerromaError::Invalid(format!(
                "unsupported DMARC version {version:?}"
            )));
        }

        let raw_policy = tag(&tags, "p").ok_or_else(|| {
            FerromaError::Invalid("the DMARC record has no p= tag".to_string())
        })?;
        let policy = DmarcPolicy::parse(raw_policy).ok_or_else(|| {
            FerromaError::Invalid(format!("unknown DMARC policy {raw_policy:?}"))
        })?;

        let subdomain_policy = match tag(&tags, "sp") {
            None => None,
            Some(raw) => Some(DmarcPolicy::parse(raw).ok_or_else(|| {
                FerromaError::Invalid(format!("unknown DMARC subdomain policy {raw:?}"))
            })?),
        };

        let pct = match tag(&tags, "pct") {
            None => 100,
            Some(raw) => {
                let value: u16 = raw.trim().parse().map_err(|_| {
                    FerromaError::Invalid(format!("the DMARC pct= value {raw:?} is not a number"))
                })?;
                if value > 100 {
                    return Err(FerromaError::Invalid(format!(
                        "the DMARC pct= value {value} is out of range"
                    )));
                }
                u8::try_from(value).map_err(|_| {
                    FerromaError::Invalid("the DMARC pct= value does not fit a byte".to_string())
                })?
            }
        };

        let ri = tag(&tags, "ri")
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(86_400);

        Ok(DmarcRecord {
            version: version.to_string(),
            policy,
            subdomain_policy,
            rua: comma_list(tag(&tags, "rua").unwrap_or("")),
            ruf: comma_list(tag(&tags, "ruf").unwrap_or("")),
            pct,
            adkim: DmarcAlignment::parse(tag(&tags, "adkim").unwrap_or("")),
            aspf: DmarcAlignment::parse(tag(&tags, "aspf").unwrap_or("")),
            fo: {
                let fo = tag(&tags, "fo").unwrap_or("").trim();
                if fo.is_empty() {
                    "0".to_string()
                } else {
                    fo.to_string()
                }
            },
            rf: {
                let rf = colon_list(tag(&tags, "rf").unwrap_or(""));
                if rf.is_empty() {
                    vec!["afrf".to_string()]
                } else {
                    rf
                }
            },
            ri,
        })
    }

    /// The policy that applies at a subdomain.
    ///
    /// `sp=` when it is published, and `p=` otherwise — and `p=` always for the
    /// domain the record itself lives at.
    pub fn policy_for(&self, is_subdomain: bool) -> DmarcPolicy {
        if is_subdomain {
            self.subdomain_policy.unwrap_or(self.policy)
        } else {
            self.policy
        }
    }

    /// Whether the policy applies to a message sampled at `pct_value`.
    ///
    /// `pct_value` is a uniformly distributed draw in `0..100`; the message is
    /// selected when it falls below `pct=`. `pct=0` therefore disables enforcement
    /// without disabling reporting, and `pct=100` enforces everything.
    pub fn applies_to(&self, pct_value: u8) -> bool {
        u16::from(pct_value) < u16::from(self.pct)
    }

    /// Whether this record asks for any reporting at all.
    pub fn has_reporting(&self) -> bool {
        !self.rua.is_empty() || !self.ruf.is_empty()
    }
}

/// Split a tag list into `(name, value)` pairs, lower-casing the names.
fn parse_tags(raw: &str) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some(eq) = part.find('=') else {
            continue;
        };
        let name = part[..eq].trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let value = part[eq + 1..].trim().trim_matches('"');
        tags.push((name, value.to_string()));
    }
    tags
}

/// The first value for a tag name.
fn tag<'a>(tags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// Split a comma-separated list, dropping empty entries.
fn comma_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Split a colon-separated list, dropping empty entries.
fn colon_list(raw: &str) -> Vec<String> {
    raw.split(':')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// The DMARC result vocabulary of RFC 7489 §11.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmarcResult {
    /// The domain publishes no policy.
    None,
    /// SPF or DKIM aligned with the `From:` domain.
    Pass,
    /// Neither aligned, and the domain has an opinion about that.
    Fail,
    /// A transient failure — usually DNS — left the question unanswered.
    TempError,
    /// The policy could not be evaluated.
    PermError,
}

impl DmarcResult {
    /// The wire token used in `Authentication-Results`.
    pub fn as_str(self) -> &'static str {
        match self {
            DmarcResult::None => "none",
            DmarcResult::Pass => "pass",
            DmarcResult::Fail => "fail",
            DmarcResult::TempError => "temperror",
            DmarcResult::PermError => "permerror",
        }
    }

    /// Whether this verdict is a pass.
    pub fn is_pass(self) -> bool {
        self == DmarcResult::Pass
    }
}

impl std::fmt::Display for DmarcResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of evaluating DMARC for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmarcVerdict {
    /// The verdict.
    pub result: DmarcResult,
    /// The policy that applies: the published policy on a pass, and the *effective*
    /// one on a failure — [`DmarcPolicy::None`] when `pct=` sampled the message out.
    pub policy: Option<DmarcPolicy>,
    /// The `From:` domain that was evaluated.
    pub domain: Option<String>,
    /// Whether SPF passed under an aligned domain.
    pub spf_aligned: bool,
    /// Whether DKIM passed under an aligned domain.
    pub dkim_aligned: bool,
    /// Why the verdict is what it is.
    pub reason: Option<String>,
}

impl DmarcVerdict {
    /// A verdict with nothing aligned.
    pub fn new(result: DmarcResult, reason: impl Into<String>) -> Self {
        DmarcVerdict {
            result,
            policy: None,
            domain: None,
            spf_aligned: false,
            dkim_aligned: false,
            reason: Some(reason.into()),
        }
    }

    /// A verdict about a specific domain.
    pub fn for_domain(result: DmarcResult, domain: impl Into<String>, reason: impl Into<String>) -> Self {
        DmarcVerdict {
            domain: Some(domain.into()),
            ..DmarcVerdict::new(result, reason)
        }
    }

    /// Whether this verdict is a pass.
    pub fn is_pass(&self) -> bool {
        self.result.is_pass()
    }

    /// The policy a delivery layer should apply, if any.
    pub fn enforcing_policy(&self) -> Option<DmarcPolicy> {
        match self.result {
            DmarcResult::Fail => self.policy.filter(|p| p.is_enforcing()),
            _ => None,
        }
    }
}

/// Why a record lookup did not produce a record.
enum LookupFailure {
    /// A transient DNS failure.
    Temp(String),
    /// A permanent problem with the record itself.
    Perm(String),
}

/// Evaluates DMARC for inbound mail.
#[derive(Debug, Clone)]
pub struct DmarcChecker {
    resolver: Arc<dyn Resolver>,
    enabled: bool,
    failure_action: Option<DmarcPolicy>,
}

impl DmarcChecker {
    /// Build a checker honouring `[policy]`.
    pub fn new(resolver: Arc<dyn Resolver>, config: &PolicyConfig) -> Self {
        DmarcChecker {
            resolver,
            enabled: config.dmarc_enabled,
            failure_action: DmarcPolicy::parse(&config.dmarc_failure_action),
        }
    }

    /// The resolver this checker queries.
    pub fn resolver(&self) -> &Arc<dyn Resolver> {
        &self.resolver
    }

    /// Whether `policy.dmarc_enabled` turned evaluation on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The local action configured for a message that fails a `p=reject` policy.
    ///
    /// RFC 7489 asks the receiver to apply the domain's policy; an operator can dial
    /// that down, and `policy.dmarc_failure_action` does so. `None` when the
    /// configured value is not one of `none`, `quarantine` or `reject`.
    pub fn failure_action(&self) -> Option<DmarcPolicy> {
        self.failure_action
    }

    /// Evaluate DMARC for a message.
    ///
    /// `from_domain` comes from the `From:` header; `spf` and `dkim` are the results
    /// already computed for this message. `pct_value` is a uniform draw in `0..100`
    /// used for the `pct=` sampling.
    pub fn check(
        &self,
        from_domain: &str,
        spf: Option<&SpfOutcome>,
        dkim: Option<&DkimVerdict>,
        pct_value: u8,
    ) -> BoxFuture<'_, DmarcVerdict> {
        // Owned inputs keep the returned future borrowing only `self`.
        let from_domain = normalise_name(from_domain);
        let spf = spf.cloned();
        let dkim = dkim.cloned();
        Box::pin(async move {
            self.check_inner(&from_domain, spf.as_ref(), dkim.as_ref(), pct_value)
                .await
        })
    }

    /// The body of [`DmarcChecker::check`].
    async fn check_inner(
        &self,
        from_domain: &str,
        spf: Option<&SpfOutcome>,
        dkim: Option<&DkimVerdict>,
        pct_value: u8,
    ) -> DmarcVerdict {
        if !self.enabled {
            return DmarcVerdict::new(
                DmarcResult::None,
                "DMARC evaluation is disabled by policy.dmarc_enabled",
            );
        }
        if from_domain.is_empty() {
            return DmarcVerdict::new(
                DmarcResult::None,
                "the message has no usable From domain",
            );
        }

        let organizational = DmarcChecker::organizational_domain(from_domain);
        match self.fetch(from_domain).await {
            Ok(Some(record)) => {
                return self.evaluate(from_domain, false, &record, spf, dkim, pct_value)
            }
            Ok(None) => {}
            Err(LookupFailure::Temp(reason)) => {
                return DmarcVerdict::for_domain(DmarcResult::TempError, from_domain, reason)
            }
            Err(LookupFailure::Perm(reason)) => {
                return DmarcVerdict::for_domain(DmarcResult::PermError, from_domain, reason)
            }
        }

        if organizational == from_domain {
            return DmarcVerdict::for_domain(
                DmarcResult::None,
                from_domain,
                format!("no DMARC record at _dmarc.{from_domain}"),
            );
        }
        // RFC 7489 §6.6.3: fall back to the organizational domain, and use `sp=`
        // because the record was not published at the From domain itself.
        match self.fetch(&organizational).await {
            Ok(Some(record)) => self.evaluate(from_domain, true, &record, spf, dkim, pct_value),
            Ok(None) => DmarcVerdict::for_domain(
                DmarcResult::None,
                from_domain,
                format!(
                    "no DMARC record at _dmarc.{from_domain} or _dmarc.{organizational}"
                ),
            ),
            Err(LookupFailure::Temp(reason)) => {
                DmarcVerdict::for_domain(DmarcResult::TempError, from_domain, reason)
            }
            Err(LookupFailure::Perm(reason)) => {
                DmarcVerdict::for_domain(DmarcResult::PermError, from_domain, reason)
            }
        }
    }

    /// Compare the authentication results against one record.
    fn evaluate(
        &self,
        from_domain: &str,
        is_subdomain: bool,
        record: &DmarcRecord,
        spf: Option<&SpfOutcome>,
        dkim: Option<&DkimVerdict>,
        pct_value: u8,
    ) -> DmarcVerdict {
        let spf_aligned = spf
            .filter(|outcome| outcome.result == SpfResult::Pass)
            .and_then(|outcome| outcome.domain.as_deref())
            .map(|domain| record.aspf.aligns(from_domain, domain))
            .unwrap_or(false);
        let dkim_aligned = dkim
            .filter(|verdict| verdict.result == DkimResult::Pass)
            .and_then(|verdict| verdict.domain.as_deref())
            .map(|domain| record.adkim.aligns(from_domain, domain))
            .unwrap_or(false);

        let policy = record.policy_for(is_subdomain);
        let mut verdict = DmarcVerdict {
            result: if spf_aligned || dkim_aligned {
                DmarcResult::Pass
            } else {
                DmarcResult::Fail
            },
            policy: Some(policy),
            domain: Some(from_domain.to_string()),
            spf_aligned,
            dkim_aligned,
            reason: None,
        };

        verdict.reason = Some(if verdict.result == DmarcResult::Pass {
            match (spf_aligned, dkim_aligned) {
                (true, true) => "both SPF and DKIM align with the From domain".to_string(),
                (true, false) => "SPF aligns with the From domain".to_string(),
                _ => "DKIM aligns with the From domain".to_string(),
            }
        } else if !record.applies_to(pct_value) {
            // The message failed, but `pct=` did not select it for enforcement.
            verdict.policy = Some(DmarcPolicy::None);
            format!(
                "neither SPF nor DKIM aligned; pct={} sampled this message out of enforcement",
                record.pct
            )
        } else {
            format!("neither SPF nor DKIM aligned; the domain asks for {policy}")
        });

        verdict
    }

    /// Fetch and parse the record at `_dmarc.<domain>`.
    async fn fetch(
        &self,
        domain: &str,
    ) -> std::result::Result<Option<DmarcRecord>, LookupFailure> {
        let name = format!("_dmarc.{domain}");
        let records = match self.resolver.txt(&name).await {
            Ok(records) => records,
            Err(e) => return Err(LookupFailure::Temp(format!("the DMARC lookup for {name} failed: {e}"))),
        };
        let candidates: Vec<&String> = records
            .iter()
            .filter(|raw| raw.trim_start().to_ascii_lowercase().starts_with("v=dmarc1"))
            .collect();
        match candidates.len() {
            0 => Ok(None),
            1 => {
                let raw = candidates
                    .first()
                    .map(|raw| (*raw).clone())
                    .ok_or_else(|| LookupFailure::Perm(format!("no DMARC record at {name}")))?;
                DmarcRecord::parse(&raw)
                    .map(Some)
                    .map_err(|e| LookupFailure::Perm(format!("the DMARC record at {name} is unusable: {e}")))
            }
            // RFC 7489 §6.6.3: more than one record means no record can be trusted.
            _ => Err(LookupFailure::Perm(format!(
                "{name} publishes {} DMARC records",
                candidates.len()
            ))),
        }
    }

    /// The organizational domain of `domain` (RFC 7489 §3.2).
    ///
    /// The last two labels, or the last three when the last two are a known
    /// multi-label public suffix — `mail.example.co.uk` is `example.co.uk`, not
    /// `co.uk`. A name with fewer than two labels is returned as it is; so is an
    /// address literal, which has no organizational domain at all.
    pub fn organizational_domain(domain: &str) -> String {
        let domain = normalise_name(domain);
        if domain.is_empty() || domain.starts_with('[') {
            return domain;
        }
        let labels: Vec<&str> = domain.split('.').filter(|label| !label.is_empty()).collect();
        if labels.len() <= 2 {
            return labels.join(".");
        }
        let last_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
        if TWO_LABEL_SUFFIXES.contains(&last_two.as_str()) && labels.len() >= 3 {
            return format!("{}.{}", labels[labels.len() - 3], last_two);
        }
        last_two
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mx::MockResolver;

    fn policy_config() -> PolicyConfig {
        PolicyConfig::default()
    }

    fn checker(mock: MockResolver) -> DmarcChecker {
        DmarcChecker::new(Arc::new(mock), &policy_config())
    }

    /// A checker publishing `record` at `_dmarc.example.com`.
    fn with_record(record: &str) -> DmarcChecker {
        with_record_for(record, "example.com")
    }

    fn with_record_for(record: &str, domain: &str) -> DmarcChecker {
        checker(MockResolver::new().with_txt(
            &format!("_dmarc.{domain}"),
            vec![record.to_string()],
        ))
    }

    /// An SPF outcome that passed under `domain`.
    fn spf_pass(domain: &str) -> SpfOutcome {
        SpfOutcome::new(SpfResult::Pass, Some(domain.to_string()), 1)
    }

    /// A DKIM verdict that passed for `domain`.
    fn dkim_pass(domain: &str) -> DkimVerdict {
        DkimVerdict::new(
            DkimResult::Pass,
            Some(domain.to_string()),
            Some("selector".to_string()),
            "verified",
        )
    }

    // ------------------------------------------------------------------
    // Policies and alignment
    // ------------------------------------------------------------------

    #[test]
    fn policies_round_trip_through_their_tokens() {
        for (raw, policy) in [
            ("none", DmarcPolicy::None),
            ("quarantine", DmarcPolicy::Quarantine),
            ("reject", DmarcPolicy::Reject),
            ("REJECT", DmarcPolicy::Reject),
            (" Quarantine ", DmarcPolicy::Quarantine),
        ] {
            assert_eq!(DmarcPolicy::parse(raw), Some(policy));
            assert_eq!(policy.as_str(), raw.trim().to_ascii_lowercase());
        }
        assert_eq!(DmarcPolicy::parse("drop"), None);
        assert_eq!(DmarcPolicy::parse(""), None);
        assert!(DmarcPolicy::Quarantine.is_enforcing());
        assert!(!DmarcPolicy::None.is_enforcing());
        assert_eq!(DmarcPolicy::Reject.to_string(), "reject");
    }

    #[test]
    fn alignment_tokens_default_to_relaxed() {
        assert_eq!(DmarcAlignment::parse("s"), DmarcAlignment::Strict);
        assert_eq!(DmarcAlignment::parse("S"), DmarcAlignment::Strict);
        assert_eq!(DmarcAlignment::parse("r"), DmarcAlignment::Relaxed);
        assert_eq!(DmarcAlignment::parse(""), DmarcAlignment::Relaxed);
        assert_eq!(DmarcAlignment::parse("nonsense"), DmarcAlignment::Relaxed);
        assert_eq!(DmarcAlignment::Relaxed.as_str(), "r");
        assert_eq!(DmarcAlignment::Strict.as_str(), "s");
    }

    #[test]
    fn strict_alignment_requires_an_exact_match() {
        assert!(DmarcAlignment::Strict.aligns("example.com", "example.com"));
        assert!(DmarcAlignment::Strict.aligns("Example.COM", "example.com."));
        assert!(!DmarcAlignment::Strict.aligns("example.com", "mail.example.com"));
        assert!(!DmarcAlignment::Strict.aligns("mail.example.com", "example.com"));
        assert!(!DmarcAlignment::Strict.aligns("example.com", ""));
    }

    #[test]
    fn relaxed_alignment_compares_organizational_domains() {
        assert!(DmarcAlignment::Relaxed.aligns("example.com", "mail.example.com"));
        assert!(DmarcAlignment::Relaxed.aligns("mail.example.com", "example.com"));
        assert!(DmarcAlignment::Relaxed.aligns("a.b.example.com", "c.example.com"));
        assert!(!DmarcAlignment::Relaxed.aligns("example.com", "example.net"));
        assert!(!DmarcAlignment::Relaxed.aligns("example.com", "notexample.com"));
        assert!(!DmarcAlignment::Relaxed.aligns("", "example.com"));
    }

    #[test]
    fn the_organizational_domain_is_the_last_two_labels() {
        assert_eq!(DmarcChecker::organizational_domain("example.com"), "example.com");
        assert_eq!(
            DmarcChecker::organizational_domain("mail.example.com"),
            "example.com"
        );
        assert_eq!(
            DmarcChecker::organizational_domain("a.b.c.example.com"),
            "example.com"
        );
        assert_eq!(DmarcChecker::organizational_domain("EXAMPLE.COM."), "example.com");
    }

    #[test]
    fn the_organizational_domain_knows_multi_label_suffixes() {
        assert_eq!(
            DmarcChecker::organizational_domain("mail.example.co.uk"),
            "example.co.uk"
        );
        assert_eq!(
            DmarcChecker::organizational_domain("example.co.uk"),
            "example.co.uk"
        );
        assert_eq!(
            DmarcChecker::organizational_domain("mail.example.com.au"),
            "example.com.au"
        );
        // A two-label name under a multi-label suffix has no third label to take.
        assert_eq!(DmarcChecker::organizational_domain("co.uk"), "co.uk");
    }

    #[test]
    fn the_organizational_domain_handles_degenerate_names() {
        assert_eq!(DmarcChecker::organizational_domain(""), "");
        assert_eq!(DmarcChecker::organizational_domain("localhost"), "localhost");
        assert_eq!(DmarcChecker::organizational_domain("[192.0.2.1]"), "[192.0.2.1]");
        assert_eq!(DmarcChecker::organizational_domain("..example.com."), "example.com");
    }

    // ------------------------------------------------------------------
    // Record parsing
    // ------------------------------------------------------------------

    #[test]
    fn a_full_record_parses() {
        let record = DmarcRecord::parse(
            "v=DMARC1; p=reject; sp=quarantine; rua=mailto:agg@example.com,mailto:agg2@example.com; \
             ruf=mailto:fail@example.com; pct=50; adkim=s; aspf=r; fo=1:d:s; rf=afrf; ri=3600",
        )
        .expect("parses");
        assert_eq!(record.version, "DMARC1");
        assert_eq!(record.policy, DmarcPolicy::Reject);
        assert_eq!(record.subdomain_policy, Some(DmarcPolicy::Quarantine));
        assert_eq!(
            record.rua,
            vec!["mailto:agg@example.com", "mailto:agg2@example.com"]
        );
        assert_eq!(record.ruf, vec!["mailto:fail@example.com"]);
        assert_eq!(record.pct, 50);
        assert_eq!(record.adkim, DmarcAlignment::Strict);
        assert_eq!(record.aspf, DmarcAlignment::Relaxed);
        assert_eq!(record.fo, "1:d:s");
        assert_eq!(record.rf, vec!["afrf"]);
        assert_eq!(record.ri, 3600);
        assert!(record.has_reporting());
    }

    #[test]
    fn a_minimal_record_parses_with_the_documented_defaults() {
        let record = DmarcRecord::parse("v=DMARC1; p=none").expect("parses");
        assert_eq!(record.policy, DmarcPolicy::None);
        assert_eq!(record.subdomain_policy, None);
        assert!(record.rua.is_empty());
        assert!(record.ruf.is_empty());
        assert_eq!(record.pct, 100);
        assert_eq!(record.adkim, DmarcAlignment::Relaxed);
        assert_eq!(record.aspf, DmarcAlignment::Relaxed);
        assert_eq!(record.fo, "0");
        assert_eq!(record.rf, vec!["afrf"]);
        assert_eq!(record.ri, 86_400);
        assert!(!record.has_reporting());
    }

    #[test]
    fn a_record_without_a_version_is_rejected() {
        assert!(DmarcRecord::parse("p=reject").is_err());
        assert!(DmarcRecord::parse("").is_err());
        assert!(DmarcRecord::parse("   ").is_err());
    }

    #[test]
    fn a_record_with_the_wrong_version_is_rejected() {
        assert!(DmarcRecord::parse("v=DMARC2; p=reject").is_err());
        assert!(DmarcRecord::parse("v=spf1; p=reject").is_err());
        assert!(DmarcRecord::parse("v=dmarc1; p=reject").is_ok());
    }

    #[test]
    fn a_record_without_a_policy_is_rejected() {
        assert!(DmarcRecord::parse("v=DMARC1").is_err());
        assert!(DmarcRecord::parse("v=DMARC1; sp=none").is_err());
    }

    #[test]
    fn an_invalid_policy_is_rejected() {
        assert!(DmarcRecord::parse("v=DMARC1; p=drop").is_err());
        assert!(DmarcRecord::parse("v=DMARC1; p=").is_err());
        assert!(DmarcRecord::parse("v=DMARC1; p=reject; sp=drop").is_err());
    }

    #[test]
    fn an_invalid_percentage_is_rejected() {
        assert!(DmarcRecord::parse("v=DMARC1; p=none; pct=101").is_err());
        assert!(DmarcRecord::parse("v=DMARC1; p=none; pct=-1").is_err());
        assert!(DmarcRecord::parse("v=DMARC1; p=none; pct=half").is_err());
        assert!(DmarcRecord::parse("v=DMARC1; p=none; pct=0").is_ok());
        assert!(DmarcRecord::parse("v=DMARC1; p=none; pct=100").is_ok());
    }

    #[test]
    fn unknown_tags_are_ignored() {
        let record = DmarcRecord::parse("v=DMARC1; p=reject; np=none; psd=nope; x=1")
            .expect("parses");
        assert_eq!(record.policy, DmarcPolicy::Reject);
    }

    #[test]
    fn a_tag_without_a_value_falls_back_to_its_default() {
        let record = DmarcRecord::parse("v=DMARC1; p=none; adkim=; ri=; fo=").expect("parses");
        assert_eq!(record.adkim, DmarcAlignment::Relaxed);
        assert_eq!(record.ri, 86_400);
        assert_eq!(record.fo, "0");
    }

    #[test]
    fn an_unparsable_interval_falls_back_to_the_default() {
        let record = DmarcRecord::parse("v=DMARC1; p=none; ri=soon").expect("parses");
        assert_eq!(record.ri, 86_400);
    }

    #[test]
    fn report_addresses_are_split_and_trimmed() {
        let record = DmarcRecord::parse(
            "v=DMARC1; p=none; rua= mailto:a@example.com , mailto:b@example.com ,",
        )
        .expect("parses");
        assert_eq!(record.rua.len(), 2);
        assert_eq!(record.rua[0], "mailto:a@example.com");
        assert_eq!(record.rua[1], "mailto:b@example.com");
    }

    #[test]
    fn report_formats_are_split_on_colons() {
        let record = DmarcRecord::parse("v=DMARC1; p=none; rf=afrf:iodef").expect("parses");
        assert_eq!(record.rf, vec!["afrf", "iodef"]);
    }

    #[test]
    fn tags_and_values_are_case_insensitive() {
        let record = DmarcRecord::parse("V=DMARC1; P=REJECT; ADKIM=S; SP=QUARANTINE")
            .expect("parses");
        assert_eq!(record.policy, DmarcPolicy::Reject);
        assert_eq!(record.subdomain_policy, Some(DmarcPolicy::Quarantine));
        assert_eq!(record.adkim, DmarcAlignment::Strict);
    }

    #[test]
    fn a_trailing_semicolon_is_tolerated() {
        let record = DmarcRecord::parse("v=DMARC1; p=reject;").expect("parses");
        assert_eq!(record.policy, DmarcPolicy::Reject);
    }

    // ------------------------------------------------------------------
    // Policy selection
    // ------------------------------------------------------------------

    #[test]
    fn the_subdomain_policy_applies_only_to_subdomains() {
        let record =
            DmarcRecord::parse("v=DMARC1; p=reject; sp=quarantine").expect("parses");
        assert_eq!(record.policy_for(false), DmarcPolicy::Reject);
        assert_eq!(record.policy_for(true), DmarcPolicy::Quarantine);
    }

    #[test]
    fn a_missing_subdomain_policy_inherits_the_domain_policy() {
        let record = DmarcRecord::parse("v=DMARC1; p=reject").expect("parses");
        assert_eq!(record.policy_for(true), DmarcPolicy::Reject);
    }

    #[test]
    fn the_percentage_selects_a_share_of_messages() {
        let all = DmarcRecord::parse("v=DMARC1; p=reject; pct=100").expect("parses");
        for value in 0..100u8 {
            assert!(all.applies_to(value));
        }
        let none = DmarcRecord::parse("v=DMARC1; p=reject; pct=0").expect("parses");
        for value in 0..100u8 {
            assert!(!none.applies_to(value));
        }
        let half = DmarcRecord::parse("v=DMARC1; p=reject; pct=50").expect("parses");
        assert!(half.applies_to(0));
        assert!(half.applies_to(49));
        assert!(!half.applies_to(50));
        assert!(!half.applies_to(99));
    }

    // ------------------------------------------------------------------
    // Verdicts
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_domain_with_no_record_is_none() {
        let checker = checker(MockResolver::new());
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::None);
        assert_eq!(verdict.domain.as_deref(), Some("example.com"));
        assert_eq!(verdict.policy, None);
    }

    #[tokio::test]
    async fn a_non_dmarc_txt_record_is_not_a_record() {
        let checker = with_record("v=spf1 -all");
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::None);
    }

    #[tokio::test]
    async fn an_aligned_spf_pass_is_a_dmarc_pass() {
        let checker = with_record("v=DMARC1; p=reject");
        let spf = spf_pass("mail.example.com");
        let verdict = checker.check("example.com", Some(&spf), None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Pass);
        assert!(verdict.spf_aligned);
        assert!(!verdict.dkim_aligned);
        assert_eq!(verdict.policy, Some(DmarcPolicy::Reject));
    }

    #[tokio::test]
    async fn an_aligned_dkim_pass_is_a_dmarc_pass() {
        let checker = with_record("v=DMARC1; p=reject");
        let dkim = dkim_pass("example.com");
        let verdict = checker.check("example.com", None, Some(&dkim), 0).await;
        assert_eq!(verdict.result, DmarcResult::Pass);
        assert!(verdict.dkim_aligned);
        assert!(!verdict.spf_aligned);
    }

    #[tokio::test]
    async fn an_unaligned_spf_pass_is_a_failure() {
        let checker = with_record("v=DMARC1; p=reject");
        let spf = spf_pass("evil.example");
        let verdict = checker.check("example.com", Some(&spf), None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        assert_eq!(verdict.policy, Some(DmarcPolicy::Reject));
    }

    #[tokio::test]
    async fn an_unaligned_dkim_pass_is_a_failure() {
        let checker = with_record("v=DMARC1; p=reject");
        let dkim = dkim_pass("evil.example");
        let verdict = checker.check("example.com", None, Some(&dkim), 0).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
    }

    #[tokio::test]
    async fn a_failed_spf_is_not_an_alignment() {
        let checker = with_record("v=DMARC1; p=reject");
        let spf = SpfOutcome::new(SpfResult::Fail, Some("example.com".to_string()), 1);
        let verdict = checker.check("example.com", Some(&spf), None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        assert!(!verdict.spf_aligned);
    }

    #[tokio::test]
    async fn strict_alignment_rejects_a_subdomain() {
        let checker = with_record("v=DMARC1; p=reject; aspf=s; adkim=s");
        let spf = spf_pass("mail.example.com");
        let dkim = dkim_pass("mail.example.com");
        let verdict = checker
            .check("example.com", Some(&spf), Some(&dkim), 0)
            .await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        let exact = spf_pass("example.com");
        let verdict = checker
            .check("example.com", Some(&exact), None, 0)
            .await;
        assert_eq!(verdict.result, DmarcResult::Pass);
    }

    #[tokio::test]
    async fn relaxed_alignment_accepts_a_subdomain_authenticated_domain() {
        let checker = with_record("v=DMARC1; p=reject; aspf=r");
        let spf = spf_pass("mail.example.com");
        let verdict = checker.check("example.com", Some(&spf), None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Pass);
    }

    #[tokio::test]
    async fn a_verdict_reports_why_it_is_what_it_is() {
        let checker = with_record("v=DMARC1; p=quarantine");
        let spf = spf_pass("example.com");
        let dkim = dkim_pass("example.com");
        let passed = checker.check("example.com", Some(&spf), Some(&dkim), 0).await;
        assert!(passed
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("both SPF and DKIM"));
        let failed = checker.check("example.com", None, None, 0).await;
        assert!(failed
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("quarantine"));
    }

    // ------------------------------------------------------------------
    // pct and sp
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_message_sampled_out_of_pct_is_a_fail_with_no_policy() {
        let checker = with_record("v=DMARC1; p=reject; pct=0");
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        assert_eq!(verdict.policy, Some(DmarcPolicy::None));
        assert_eq!(verdict.enforcing_policy(), None);
        assert!(verdict.reason.as_deref().unwrap_or("").contains("pct=0"));
    }

    #[tokio::test]
    async fn a_message_selected_by_pct_gets_the_whole_policy() {
        let checker = with_record("v=DMARC1; p=reject; pct=10");
        let verdict = checker.check("example.com", None, None, 5).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        assert_eq!(verdict.enforcing_policy(), Some(DmarcPolicy::Reject));
        let outside = checker.check("example.com", None, None, 50).await;
        assert_eq!(outside.enforcing_policy(), None);
    }

    #[tokio::test]
    async fn the_subdomain_policy_is_applied_when_the_record_is_at_the_org_domain() {
        let checker = with_record_for("v=DMARC1; p=reject; sp=quarantine", "example.com");
        let verdict = checker.check("mail.example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        assert_eq!(verdict.policy, Some(DmarcPolicy::Quarantine));
        assert_eq!(verdict.domain.as_deref(), Some("mail.example.com"));
    }

    #[tokio::test]
    async fn a_subdomain_record_wins_over_the_organizational_one() {
        let mock = MockResolver::new()
            .with_txt(
                "_dmarc.mail.example.com",
                vec!["v=DMARC1; p=none".to_string()],
            )
            .with_txt("_dmarc.example.com", vec!["v=DMARC1; p=reject".to_string()]);
        let verdict = checker(mock).check("mail.example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        assert_eq!(verdict.policy, Some(DmarcPolicy::None));
    }

    #[tokio::test]
    async fn the_organizational_fallback_reports_none_when_it_is_empty_too() {
        let checker = checker(MockResolver::new());
        let verdict = checker.check("mail.example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::None);
        assert!(verdict
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("_dmarc.example.com"));
    }

    #[tokio::test]
    async fn an_organizational_fallback_uses_relaxed_alignment_against_the_org_domain() {
        let checker = with_record_for("v=DMARC1; p=reject", "example.com");
        let spf = spf_pass("mail.example.com");
        let verdict = checker.check("mail.example.com", Some(&spf), None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Pass);
    }

    // ------------------------------------------------------------------
    // Failures
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_dns_failure_is_a_temporary_error() {
        let checker = checker(MockResolver::new().with_failure("_dmarc.example.com"));
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::TempError);
    }

    #[tokio::test]
    async fn a_dns_failure_on_the_organizational_lookup_is_a_temporary_error() {
        let checker = checker(MockResolver::new().with_failure("_dmarc.example.com"));
        let verdict = checker.check("mail.example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::TempError);
    }

    #[tokio::test]
    async fn two_records_are_a_permanent_error() {
        let checker = checker(MockResolver::new().with_txt(
            "_dmarc.example.com",
            vec![
                "v=DMARC1; p=none".to_string(),
                "v=DMARC1; p=reject".to_string(),
            ],
        ));
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::PermError);
    }

    #[tokio::test]
    async fn a_malformed_record_is_a_permanent_error() {
        let checker = with_record("v=DMARC1; p=drop");
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::PermError);
    }

    #[tokio::test]
    async fn a_policy_of_none_still_reports_a_failure() {
        let checker = with_record("v=DMARC1; p=none");
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::Fail);
        assert_eq!(verdict.policy, Some(DmarcPolicy::None));
        assert_eq!(verdict.enforcing_policy(), None);
    }

    #[tokio::test]
    async fn a_message_with_no_from_domain_is_none() {
        let checker = with_record("v=DMARC1; p=reject");
        let verdict = checker.check("", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::None);
        assert_eq!(verdict.domain, None);
    }

    #[tokio::test]
    async fn a_disabled_checker_returns_none() {
        let config = PolicyConfig {
            dmarc_enabled: false,
            ..PolicyConfig::default()
        };
        let checker = DmarcChecker::new(
            Arc::new(MockResolver::new().with_txt(
                "_dmarc.example.com",
                vec!["v=DMARC1; p=reject".to_string()],
            )),
            &config,
        );
        assert!(!checker.enabled());
        let verdict = checker.check("example.com", None, None, 0).await;
        assert_eq!(verdict.result, DmarcResult::None);
    }

    // ------------------------------------------------------------------
    // Plumbing
    // ------------------------------------------------------------------

    #[test]
    fn result_tokens_match_rfc_7489() {
        assert_eq!(DmarcResult::None.as_str(), "none");
        assert_eq!(DmarcResult::Pass.as_str(), "pass");
        assert_eq!(DmarcResult::Fail.as_str(), "fail");
        assert_eq!(DmarcResult::TempError.as_str(), "temperror");
        assert_eq!(DmarcResult::PermError.as_str(), "permerror");
        assert_eq!(DmarcResult::Pass.to_string(), "pass");
        assert!(DmarcResult::Pass.is_pass());
    }

    #[test]
    fn a_checker_reports_its_configuration() {
        let config = PolicyConfig {
            dmarc_failure_action: "reject".to_string(),
            ..PolicyConfig::default()
        };
        let checker = DmarcChecker::new(Arc::new(MockResolver::new()), &config);
        assert!(checker.enabled());
        assert_eq!(checker.failure_action(), Some(DmarcPolicy::Reject));
        assert_eq!(checker.resolver().query_count(), 0);
        let odd = PolicyConfig {
            dmarc_failure_action: "nonsense".to_string(),
            ..PolicyConfig::default()
        };
        let checker = DmarcChecker::new(Arc::new(MockResolver::new()), &odd);
        assert_eq!(checker.failure_action(), None);
    }

    #[test]
    fn verdict_helpers_describe_themselves() {
        let pass = DmarcVerdict::for_domain(DmarcResult::Pass, "example.com", "aligned");
        assert!(pass.is_pass());
        assert_eq!(pass.enforcing_policy(), None);
        assert_eq!(pass.domain.as_deref(), Some("example.com"));
        let failure = DmarcVerdict {
            result: DmarcResult::Fail,
            policy: Some(DmarcPolicy::Reject),
            ..DmarcVerdict::new(DmarcResult::Fail, "no alignment")
        };
        assert_eq!(failure.enforcing_policy(), Some(DmarcPolicy::Reject));
        assert!(!DmarcResult::TempError.is_pass());
    }
}
