//! Inbound authentication policy: SPF, DKIM and DMARC, applied to accepted mail.
//!
//! ```text
//!   DATA complete
//!        │
//!        ├─ SPF    envelope sender vs. the observed peer address   (policy.spf_enabled)
//!        ├─ DKIM   the signature over the bytes as received        (dkim.verify_inbound)
//!        ├─ DMARC  alignment against the From: domain              (policy.dmarc_enabled)
//!        │
//!        ├─ Authentication-Results: …                              (policy.add_auth_results)
//!        │
//!        └─ PolicyAction: Accept | Quarantine | Reject
//! ```
//!
//! # Two rules that decide everything here
//!
//! **1. Never lose mail because a lookup failed.** A DNS timeout is not "no record".
//! [`SpfResult::TempError`](crate::spf::SpfResult::TempError),
//! [`DkimResult::TempError`](crate::dkim::DkimResult::TempError) and
//! [`DmarcResult::TempError`](crate::dmarc::DmarcResult::TempError) all mean *we could
//! not tell*, which is a different thing from *this domain fails*. A transient result
//! anywhere in the chain therefore suppresses the enforcement action entirely: the
//! message is accepted, the header says `spf=temperror` (the honest report), and the
//! rejection is never taken. Rejecting on a resolver outage would turn our own broken
//! DNS into silent mail loss for every domain that publishes `p=reject`.
//!
//! **2. `reject` is decided before the `250`.** [`InboundPolicy::evaluate`] runs at the
//! end of `DATA` and *before* anything is stored, so a rejection is a `550` the peer
//! has not been contradicted about. Silently filing a rejected message after
//! acknowledging it would be the kind of dishonesty that makes a mail server
//! untrustworthy; the alternative — quarantine — is a *delivery* choice and files the
//! message into `Junk`.
//!
//! # Which switch adds the header
//!
//! `policy.add_auth_results` — the only switch for it. (The redundant
//! `[dkim] add_auth_results` key that used to shadow it has been removed from
//! `DkimConfig`: the header reports all three methods, not just DKIM, so it belongs
//! under `[policy]`.)

use std::sync::Arc;
use std::time::Duration;

use ferroma_core::config::Config;
use ferroma_mail::ParsedMessage;

use crate::auth_results::AuthResults;
use crate::delivery::ReceivedMessage;
use crate::dkim::{DkimResult, DkimVerdict, DkimVerifier};
use crate::dmarc::{DmarcChecker, DmarcPolicy, DmarcResult, DmarcVerdict};
use crate::mx::Resolver;
use crate::spf::{SpfChecker, SpfOutcome, SpfResult};

/// The absolute ceiling on one message's policy evaluation.
///
/// Not the *budget* — [`policy_budget`] derives that from `[dns]`. This is the hard cap
/// for a configuration generous to the point of being a liability.
pub const POLICY_TIMEOUT_CAP: Duration = Duration::from_secs(60);

/// The floor under the derived budget.
///
/// A configuration cannot make the deadline so short that a single slow-but-alive
/// resolver fails every message.
pub const POLICY_TIMEOUT_FLOOR: Duration = Duration::from_secs(2);

/// How long the policy step may take, derived from `[dns]`.
///
/// The cost of a DNS-bound step is `time × packets × lookups`, so the budget is exactly
/// that, clamped between [`POLICY_TIMEOUT_FLOOR`] and [`POLICY_TIMEOUT_CAP`].
///
/// A fixed 60-second deadline is what this replaces: with `[dns] timeout_secs = 1,
/// attempts = 1` an operator has said "be impatient", and a step that then sat on a
/// black-holed lookup for a minute per message turned inbound mail off.
///
/// # `attempts` counts retries, not packets
///
/// `ResolverOpts::attempts` is documented as "number of retries after lookup failure", so
/// `attempts = 1` puts **two** datagrams on the wire and costs two timeouts. The `+ 1` is
/// not padding: leaving it out under-counts the worst case by a factor of two, which is
/// the difference between a derived budget and a guess.
///
/// The lookup count is the worst case the configuration allows: SPF's own record, plus
/// its `spf_max_lookups` mechanisms, plus the DKIM key record, plus the DMARC record and
/// its organizational-domain fallback. The short-circuit in
/// [`InboundPolicy::evaluate_inner`] normally stops long before it.
pub fn policy_budget(config: &Config) -> Duration {
    let packets_per_lookup = config.dns.attempts.max(1).saturating_add(1) as u64;
    let per_lookup_secs = config
        .dns
        .timeout_secs
        .max(1)
        .saturating_mul(packets_per_lookup);
    let lookups = 4u64.saturating_add(config.policy.spf_max_lookups as u64);
    Duration::from_secs(per_lookup_secs.saturating_mul(lookups))
        .clamp(POLICY_TIMEOUT_FLOOR, POLICY_TIMEOUT_CAP)
}

/// What the delivery layer should do with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolicyAction {
    /// Deliver normally.
    #[default]
    Accept,
    /// Deliver, but into `Junk` rather than `INBOX`.
    Quarantine,
    /// Refuse the message. Only ever decided before the `250`.
    Reject,
}

impl PolicyAction {
    /// A short name for logs and for the `policy.dmarc` property.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyAction::Accept => "accept",
            PolicyAction::Quarantine => "quarantine",
            PolicyAction::Reject => "reject",
        }
    }

    /// Whether this action refuses the message outright.
    pub fn is_reject(self) -> bool {
        matches!(self, PolicyAction::Reject)
    }
}

/// Everything the policy step learned about one message.
#[derive(Debug, Clone, Default)]
pub struct InboundVerdict {
    /// The SPF result, when SPF ran.
    pub spf: Option<SpfOutcome>,
    /// One entry per DKIM signature evaluated.
    pub dkim: Vec<DkimVerdict>,
    /// The DMARC result, when DMARC ran.
    pub dmarc: Option<DmarcVerdict>,
    /// What delivery should do.
    pub action: PolicyAction,
    /// Why the action is what it is, for the log line.
    pub reason: Option<String>,
    /// The rendered `Authentication-Results` header value, when one was produced.
    pub authentication_results: Option<String>,
}

impl InboundVerdict {
    /// A verdict that changes nothing: the message is accepted, unmodified.
    pub fn accepted(reason: impl Into<String>) -> Self {
        InboundVerdict {
            action: PolicyAction::Accept,
            reason: Some(reason.into()),
            ..InboundVerdict::default()
        }
    }

    /// Whether the message is refused.
    pub fn is_reject(&self) -> bool {
        self.action.is_reject()
    }

    /// Whether the DMARC check produced a definite failure.
    pub fn dmarc_failed(&self) -> bool {
        self.dmarc
            .as_ref()
            .is_some_and(|v| v.result == DmarcResult::Fail)
    }

    /// Whether any check could not reach a decision because of a lookup failure.
    ///
    /// When this is true the enforcement action is suppressed — see the module docs.
    pub fn is_transient(&self) -> bool {
        let spf_transient = self
            .spf
            .as_ref()
            .is_some_and(|o| o.result == SpfResult::TempError);
        let dkim_transient = self
            .dkim
            .iter()
            .any(|v| v.result == DkimResult::TempError);
        let dmarc_transient = self
            .dmarc
            .as_ref()
            .is_some_and(|v| v.result == DmarcResult::TempError);
        spf_transient || dkim_transient || dmarc_transient
    }

    /// The `Authentication-Results` header value, when one was produced.
    pub fn header_value(&self) -> Option<&str> {
        self.authentication_results.as_deref()
    }
}

/// The inbound authentication policy, bound to one resolver and one configuration.
#[derive(Debug, Clone)]
pub struct InboundPolicy {
    spf: SpfChecker,
    dmarc: DmarcChecker,
    verifier: DkimVerifier,
    spf_enabled: bool,
    dkim_enabled: bool,
    add_auth_results: bool,
    authserv_id: String,
    timeout: Duration,
}

impl InboundPolicy {
    /// Build the evaluator for `config`, over `resolver`.
    pub fn new(config: &Config, resolver: Arc<dyn Resolver>) -> Self {
        InboundPolicy {
            spf: SpfChecker::new(Arc::clone(&resolver), &config.policy),
            dmarc: DmarcChecker::new(Arc::clone(&resolver), &config.policy),
            verifier: DkimVerifier::new(resolver),
            spf_enabled: config.policy.spf_enabled,
            dkim_enabled: config.dkim.verify_inbound,
            add_auth_results: config.policy.add_auth_results,
            authserv_id: config.server.hostname.clone(),
            // Derived from `[dns]`, not a constant: an impatient resolver configuration
            // must produce an impatient policy step.
            timeout: policy_budget(config),
        }
    }

    /// Override the evaluation deadline. Tests use this to prove the timeout path.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Whether the evaluator will report `Authentication-Results`.
    pub fn adds_auth_results(&self) -> bool {
        self.add_auth_results
    }

    /// Whether the evaluator will do any work at all.
    pub fn is_enabled(&self) -> bool {
        self.spf_enabled || self.dkim_enabled || self.dmarc.enabled()
    }

    /// The evaluation deadline.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The `From:` domain of a parsed message, which is what DMARC aligns against.
    ///
    /// The *first* `From:` address, per RFC 7489 §6.6.1: a message with several From
    /// addresses has no usable domain for DMARC, so anything ambiguous yields `None`
    /// and DMARC is skipped rather than guessed.
    pub fn from_domain(parsed: &ParsedMessage) -> Option<String> {
        let from = parsed.from();
        if from.len() != 1 {
            return None;
        }
        let domain = from[0].address.domain().to_string();
        if domain.is_empty() {
            None
        } else {
            Some(domain)
        }
    }

    /// Evaluate the policy for one accepted message.
    ///
    /// Never returns `Err`: a policy that cannot decide must not be able to lose mail,
    /// so every failure inside this function becomes an `Accept` with a reason.
    pub async fn evaluate(&self, message: &ReceivedMessage, parsed: &ParsedMessage) -> InboundVerdict {
        match tokio::time::timeout(self.timeout, self.evaluate_inner(message, parsed)).await {
            Ok(verdict) => verdict,
            Err(_) => {
                tracing::warn!(
                    connection_id = %message.connection_id,
                    timeout_secs = self.timeout.as_secs(),
                    result = "policy_timeout",
                    "inbound policy evaluation timed out; accepting without an Authentication-Results header"
                );
                InboundVerdict::accepted("the policy evaluation timed out")
            }
        }
    }

    /// The body of [`InboundPolicy::evaluate`].
    async fn evaluate_inner(
        &self,
        message: &ReceivedMessage,
        parsed: &ParsedMessage,
    ) -> InboundVerdict {
        // Rendered once: SPF is evaluated on it and the header reports it as
        // `smtp.mailfrom`.
        let envelope_sender = message.sender.as_ref().map(ToString::to_string);
        let helo = message.helo.as_deref().unwrap_or("");

        // --- SPF ------------------------------------------------------
        let spf = if self.spf_enabled {
            match message.remote_ip {
                Some(ip) => Some(
                    self.spf
                        .check(ip, envelope_sender.as_deref(), helo)
                        .await,
                ),
                // No observed address means we cannot evaluate SPF at all — not that the
                // sender fails it.
                None => None,
            }
        } else {
            None
        };

        // Fail open *fast*. A `TempError` here is a DNS lookup that did not come back at
        // all — a black-holed nameserver, a broken resolver, a network with no DNS — and
        // the stages that follow would ask the same resolver the same questions and pay
        // the same timeout for each. One failed probe is enough to know the answer, so
        // the remaining stages are reported as `temperror` without being attempted.
        //
        // This is what keeps a dead resolver cheap: `[dns] timeout_secs = 1` costs about
        // a second per message, not one timeout per stage.
        let mut resolver_unavailable = spf
            .as_ref()
            .is_some_and(|outcome| outcome.result == SpfResult::TempError);

        // --- DKIM -----------------------------------------------------
        // Verification runs over the bytes *as received*, before we add our own trace
        // headers: adding a header would invalidate a `simple` canonicalisation.
        let dkim = if !self.dkim_enabled {
            Vec::new()
        } else if resolver_unavailable {
            vec![DkimVerdict::temp_error(
                "not attempted: an earlier DNS lookup did not complete",
            )]
        } else {
            let verdict = self.verifier.verify(&message.body).await;
            if verdict.result == DkimResult::TempError {
                resolver_unavailable = true;
            }
            match verdict.result {
                // "this message carries no signature" is not a finding worth reporting.
                DkimResult::None => Vec::new(),
                _ => vec![verdict],
            }
        };

        // --- DMARC ----------------------------------------------------
        let from_domain = Self::from_domain(parsed);
        let dmarc = match from_domain.as_deref() {
            Some(domain) if resolver_unavailable => {
                // The domain is named in the verdict so the header still attributes the
                // `temperror` to the right place.
                Some(DmarcVerdict::for_domain(
                    DmarcResult::TempError,
                    domain,
                    "not attempted: an earlier DNS lookup did not complete",
                ))
            }
            Some(domain) => {
                // `pct=` is a uniform draw per message, which is what RFC 7489 §6.6.4
                // asks for.
                let pct = {
                    use rand::Rng;
                    rand::thread_rng().gen_range(0..100u8)
                };
                Some(
                    self.dmarc
                        .check(domain, spf.as_ref(), dkim.first(), pct)
                        .await,
                )
            }
            None => Some(DmarcVerdict::new(
                DmarcResult::None,
                "the message has no single usable From domain",
            )),
        };

        // --- the action ----------------------------------------------
        let (action, reason) = decide(
            spf.as_ref(),
            &dkim,
            dmarc.as_ref(),
            self.dmarc.failure_action(),
        );

        // --- the header ----------------------------------------------
        let authentication_results = if self.add_auth_results {
            let header = AuthResults::from_checks(
                &self.authserv_id,
                spf.as_ref(),
                envelope_sender.as_deref(),
                helo,
                &dkim,
                dmarc.as_ref(),
                from_domain.as_deref().unwrap_or(""),
            );
            if header.is_empty() {
                None
            } else {
                Some(header.render())
            }
        } else {
            None
        };

        InboundVerdict {
            spf,
            dkim,
            dmarc,
            action,
            reason,
            authentication_results,
        }
    }
}

/// Decide what to do, per RFC 7489 and the two rules in the module docs.
///
/// `local` is `policy.dmarc_failure_action`: the operator's dial-down. The action taken
/// is never *harsher* than what the domain published, so a domain asking for
/// `quarantine` is never rejected because of a local setting.
fn decide(
    spf: Option<&SpfOutcome>,
    dkim: &[DkimVerdict],
    dmarc: Option<&DmarcVerdict>,
    local: Option<DmarcPolicy>,
) -> (PolicyAction, Option<String>) {
    // Rule 1: a lookup we could not complete is not a verdict. The header still reports
    // `temperror`, but nothing is enforced.
    let transient = spf.is_some_and(|o| o.result == SpfResult::TempError)
        || dkim.iter().any(|v| v.result == DkimResult::TempError)
        || dmarc.is_some_and(|v| v.result == DmarcResult::TempError);
    if transient {
        return (
            PolicyAction::Accept,
            Some(
                "a DNS lookup failed, so the DMARC result is not reliable and no \
                 enforcement action was taken"
                    .to_string(),
            ),
        );
    }

    let Some(verdict) = dmarc else {
        return (PolicyAction::Accept, None);
    };
    // `enforcing_policy` is `Some` only for a definite `Fail` under `p=reject` or
    // `p=quarantine`; a `pct=` sample-out and a `TempError` both return `None`.
    let Some(published) = verdict.enforcing_policy() else {
        return (PolicyAction::Accept, None);
    };

    let effective = match local {
        Some(local) => less_severe(published, local),
        None => published,
    };
    let action = match effective {
        DmarcPolicy::Reject => PolicyAction::Reject,
        DmarcPolicy::Quarantine => PolicyAction::Quarantine,
        DmarcPolicy::None => PolicyAction::Accept,
    };
    let reason = match action {
        PolicyAction::Reject => Some(format!(
            "DMARC failed and the effective policy is reject (published {}, local {})",
            published.as_str(),
            local.map(DmarcPolicy::as_str).unwrap_or("unset")
        )),
        PolicyAction::Quarantine => Some(format!(
            "DMARC failed and the effective policy is quarantine (published {}, local {})",
            published.as_str(),
            local.map(DmarcPolicy::as_str).unwrap_or("unset")
        )),
        PolicyAction::Accept => None,
    };
    (action, reason)
}

/// The less severe of two policies: `None` < `Quarantine` < `Reject`.
fn less_severe(a: DmarcPolicy, b: DmarcPolicy) -> DmarcPolicy {
    fn rank(p: DmarcPolicy) -> u8 {
        match p {
            DmarcPolicy::None => 0,
            DmarcPolicy::Quarantine => 1,
            DmarcPolicy::Reject => 2,
        }
    }
    if rank(a) <= rank(b) {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mx::{MockResolver, MxHost};
use ferroma_core::FerromaError;
    use crate::spf::SpfChecker;
    use ferroma_core::config::PolicyConfig;
    use ferroma_core::EmailAddress;
    use std::net::IpAddr;

    fn config() -> Config {
        let mut config = Config::default();
        config.server.hostname = "mx.test".to_string();
        config.policy.spf_enabled = true;
        config.policy.dmarc_enabled = true;
        config.policy.add_auth_results = true;
        config.dkim.verify_inbound = true;
        config
    }

    fn ip(raw: &str) -> IpAddr {
        raw.parse().expect("test ip")
    }

    fn parsed(raw: &str) -> ParsedMessage {
        ParsedMessage::parse(raw.as_bytes()).expect("parse")
    }

    fn message(raw: &str, sender: Option<&str>, from_ip: Option<&str>) -> ReceivedMessage {
        message_bytes(raw.as_bytes(), sender, from_ip)
    }

    /// [`message`] for a payload that is not `&str` — a signed message, for instance.
    fn message_bytes(
        raw: &[u8],
        sender: Option<&str>,
        from_ip: Option<&str>,
    ) -> ReceivedMessage {
        let mut message = ReceivedMessage::new(raw.to_vec(), None, "conn-test");
        message.sender = sender.map(|s| EmailAddress::parse(s).expect("address"));
        message.remote_ip = from_ip.map(ip);
        message.helo = Some("client.example.net".to_string());
        message
    }

    fn policy(resolver: MockResolver, config: &Config) -> InboundPolicy {
        InboundPolicy::new(config, Arc::new(resolver))
    }

    /// A genuinely signed message plus its parsed form.
    ///
    /// `MockResolver` answers DNS from a script, so for the short-circuit tests the
    /// signature has to be real: a hand-written `DKIM-Signature` fails to parse and the
    /// verifier never reaches the lookup those tests are about.
    struct SignedMessage {
        raw: Vec<u8>,
        parsed: ParsedMessage,
        /// The `v=DKIM1; …` record for `sel._domainkey.example.net`.
        record: String,
    }

    fn signed_message() -> SignedMessage {
        use crate::dkim::{DkimKey, DkimSigner};
        use ferroma_core::config::DkimConfig;
        use rsa::pkcs8::EncodePrivateKey;

        let config = DkimConfig {
            enabled: true,
            selector: "sel".to_string(),
            private_key_path: None,
            domain: Some("example.net".to_string()),
            canonicalization: "relaxed".to_string(),
            headers_to_sign: vec!["From".to_string(), "Subject".to_string()],
            verify_inbound: true,
        };
        // 1024 bits: this is a test, and generation speed matters.
        let mut rng = rand::thread_rng();
        let key = rsa::RsaPrivateKey::new(&mut rng, 1024).expect("generate an RSA key");
        let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).expect("PEM");
        let dkim_key = DkimKey::from_pem(&pem).expect("parse the key");
        let record = format!(
            "v=DKIM1; k=rsa; p={}",
            dkim_key.public_key_base64().expect("public key")
        );
        let signer = DkimSigner::from_key(dkim_key, &config).expect("signer");

        let body = b"From: alice@example.net\r\nSubject: signed\r\n\r\nbody\r\n";
        let raw = signer.sign_message(body).expect("sign");
        let parsed = ParsedMessage::parse(&raw).expect("parse");
        SignedMessage { raw, parsed, record }
    }

    // ------------------------------------------------------------------
    // The action decision, in isolation
    // ------------------------------------------------------------------

    #[test]
    fn no_dmarc_verdict_means_no_action() {
        let (action, reason) = decide(None, &[], None, None);
        assert_eq!(action, PolicyAction::Accept);
        assert!(reason.is_none());
    }

    #[test]
    fn a_dmarc_pass_takes_no_action() {
        let verdict = DmarcVerdict::for_domain(DmarcResult::Pass, "example.com", "aligned");
        let (action, _) = decide(None, &[], Some(&verdict), Some(DmarcPolicy::Reject));
        assert_eq!(action, PolicyAction::Accept, "a pass is never rejected");
    }

    #[test]
    fn a_dmarc_failure_under_p_none_takes_no_action() {
        let mut verdict = DmarcVerdict::for_domain(DmarcResult::Fail, "example.com", "unaligned");
        verdict.policy = Some(DmarcPolicy::None);
        let (action, _) = decide(None, &[], Some(&verdict), Some(DmarcPolicy::Reject));
        assert_eq!(action, PolicyAction::Accept, "p=none asks for nothing");
    }

    #[test]
    fn a_dmarc_failure_under_p_reject_rejects_when_local_policy_agrees() {
        let mut verdict = DmarcVerdict::for_domain(DmarcResult::Fail, "example.com", "unaligned");
        verdict.policy = Some(DmarcPolicy::Reject);
        let (action, reason) = decide(None, &[], Some(&verdict), Some(DmarcPolicy::Reject));
        assert_eq!(action, PolicyAction::Reject);
        assert!(reason.expect("a reason").contains("reject"));
    }

    #[test]
    fn a_local_policy_can_dial_a_reject_down_to_quarantine() {
        let mut verdict = DmarcVerdict::for_domain(DmarcResult::Fail, "example.com", "unaligned");
        verdict.policy = Some(DmarcPolicy::Reject);
        let (action, reason) = decide(None, &[], Some(&verdict), Some(DmarcPolicy::Quarantine));
        assert_eq!(action, PolicyAction::Quarantine);
        assert!(reason.expect("a reason").contains("quarantine"));
    }

    #[test]
    fn a_local_policy_can_dial_a_reject_down_to_nothing() {
        let mut verdict = DmarcVerdict::for_domain(DmarcResult::Fail, "example.com", "unaligned");
        verdict.policy = Some(DmarcPolicy::Reject);
        let (action, _) = decide(None, &[], Some(&verdict), Some(DmarcPolicy::None));
        assert_eq!(action, PolicyAction::Accept);
    }

    #[test]
    fn a_local_policy_never_makes_a_message_harsher_than_the_domain_asked() {
        // The domain published `quarantine`; a local `reject` must not escalate it.
        let mut verdict = DmarcVerdict::for_domain(DmarcResult::Fail, "example.com", "unaligned");
        verdict.policy = Some(DmarcPolicy::Quarantine);
        let (action, _) = decide(None, &[], Some(&verdict), Some(DmarcPolicy::Reject));
        assert_eq!(action, PolicyAction::Quarantine);
    }

    #[test]
    fn an_unparseable_local_policy_falls_back_to_the_published_one() {
        let mut verdict = DmarcVerdict::for_domain(DmarcResult::Fail, "example.com", "unaligned");
        verdict.policy = Some(DmarcPolicy::Reject);
        let (action, _) = decide(None, &[], Some(&verdict), None);
        assert_eq!(action, PolicyAction::Reject);
    }

    /// The rule the brief is most emphatic about: a lookup failure must never become a
    /// rejection.
    #[test]
    fn a_transient_result_suppresses_enforcement_at_every_stage() {
        let mut verdict = DmarcVerdict::for_domain(DmarcResult::Fail, "example.com", "unaligned");
        verdict.policy = Some(DmarcPolicy::Reject);

        // SPF timed out.
        let spf = SpfOutcome::new(SpfResult::TempError, Some("example.com".into()), 1);
        let (action, reason) = decide(Some(&spf), &[], Some(&verdict), Some(DmarcPolicy::Reject));
        assert_eq!(action, PolicyAction::Accept, "an SPF timeout must not reject");
        assert!(reason.expect("a reason").contains("DNS lookup failed"));

        // DKIM timed out.
        let dkim = DkimVerdict::temp_error("dns timeout");
        let (action, _) = decide(None, &[dkim], Some(&verdict), Some(DmarcPolicy::Reject));
        assert_eq!(action, PolicyAction::Accept, "a DKIM timeout must not reject");

        // DMARC itself timed out.
        let transient = DmarcVerdict::for_domain(DmarcResult::TempError, "example.com", "timeout");
        let (action, _) = decide(None, &[], Some(&transient), Some(DmarcPolicy::Reject));
        assert_eq!(action, PolicyAction::Accept, "a DMARC timeout must not reject");
    }

    #[test]
    fn less_severe_orders_the_policies() {
        assert_eq!(less_severe(DmarcPolicy::Reject, DmarcPolicy::None), DmarcPolicy::None);
        assert_eq!(
            less_severe(DmarcPolicy::Quarantine, DmarcPolicy::Reject),
            DmarcPolicy::Quarantine
        );
        assert_eq!(less_severe(DmarcPolicy::Reject, DmarcPolicy::Reject), DmarcPolicy::Reject);
        assert_eq!(less_severe(DmarcPolicy::None, DmarcPolicy::Reject), DmarcPolicy::None);
    }

    // ------------------------------------------------------------------
    // Verdict helpers
    // ------------------------------------------------------------------

    #[test]
    fn an_accepted_verdict_is_the_default_shape() {
        let verdict = InboundVerdict::default();
        assert_eq!(verdict.action, PolicyAction::Accept);
        assert!(!verdict.is_reject());
        assert!(!verdict.is_transient());
        assert!(verdict.header_value().is_none());
        assert!(InboundVerdict::accepted("because").reason.is_some());
    }

    #[test]
    fn a_verdict_knows_a_definite_dmarc_failure_from_a_transient_one() {
        let failing = InboundVerdict {
            dmarc: Some(DmarcVerdict::new(DmarcResult::Fail, "x")),
            ..InboundVerdict::default()
        };
        assert!(failing.dmarc_failed());
        assert!(!failing.is_transient());

        let transient = InboundVerdict {
            dmarc: Some(DmarcVerdict::new(DmarcResult::TempError, "x")),
            ..InboundVerdict::default()
        };
        assert!(!transient.dmarc_failed());
        assert!(transient.is_transient());
    }

    #[test]
    fn action_names_are_stable() {
        assert_eq!(PolicyAction::Accept.as_str(), "accept");
        assert_eq!(PolicyAction::Quarantine.as_str(), "quarantine");
        assert_eq!(PolicyAction::Reject.as_str(), "reject");
        assert!(PolicyAction::Reject.is_reject());
        assert!(!PolicyAction::Quarantine.is_reject());
    }

    // ------------------------------------------------------------------
    // The From: domain
    // ------------------------------------------------------------------

    #[test]
    fn the_from_domain_comes_from_the_single_from_address() {
        let parsed = parsed("From: alice@Example.COM\r\nTo: bob@mx.test\r\n\r\nbody\r\n");
        assert_eq!(InboundPolicy::from_domain(&parsed).as_deref(), Some("example.com"));
    }

    #[test]
    fn a_message_without_a_from_header_has_no_dmarc_domain() {
        let parsed = parsed("To: bob@mx.test\r\n\r\nbody\r\n");
        assert!(InboundPolicy::from_domain(&parsed).is_none());
    }

    #[test]
    fn several_from_addresses_yield_no_dmarc_domain() {
        // RFC 7489 §6.6.1: several From addresses give no usable domain, so DMARC is
        // skipped rather than guessed.
        let parsed = parsed("From: a@x.test, b@y.test\r\n\r\nbody\r\n");
        assert!(InboundPolicy::from_domain(&parsed).is_none());
    }

    // ------------------------------------------------------------------
    // Evaluation against a mock resolver
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn an_spf_failure_is_reported_and_the_message_is_still_accepted_under_p_none() {
        let mut config = config();
        config.policy.dmarc_failure_action = "none".to_string();
        // The domain publishes SPF that does not include the peer address, and a DMARC
        // record asking for nothing.
        let resolver = MockResolver::new()
            .with_txt("example.net", vec!["v=spf1 ip4:198.51.100.1 -all".to_string()])
            .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]);
        let policy = policy(resolver, &config);

        let raw = "From: alice@example.net\r\nTo: bob@mx.test\r\nSubject: hi\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;

        assert_eq!(verdict.spf.as_ref().expect("SPF ran").result, SpfResult::Fail);
        assert_eq!(
            verdict.action,
            PolicyAction::Accept,
            "p=none must not reject, even though SPF failed"
        );
        let header = verdict.header_value().expect("a header");
        assert!(header.contains("spf=fail"), "{header}");
        assert!(header.contains("smtp.mailfrom=example.net"), "{header}");
    }

    #[tokio::test]
    async fn an_spf_pass_is_reported_as_pass() {
        let config = config();
        let resolver = MockResolver::new()
            .with_txt("example.net", vec!["v=spf1 ip4:203.0.113.0/24 -all".to_string()])
            .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]);
        let policy = policy(resolver, &config);

        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert_eq!(verdict.spf.as_ref().expect("SPF ran").result, SpfResult::Pass);
        assert!(verdict.header_value().expect("a header").contains("spf=pass"));
    }

    #[tokio::test]
    async fn a_message_with_no_spf_record_reports_none_and_accepts() {
        let config = config();
        let policy = policy(MockResolver::new(), &config);
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert_eq!(verdict.spf.as_ref().expect("SPF ran").result, SpfResult::None);
        assert_eq!(verdict.action, PolicyAction::Accept);
    }

    /// The headline requirement: a resolver that always errors must neither reject the
    /// message nor report a misleading `fail`.
    #[tokio::test]
    async fn a_resolver_that_always_errors_accepts_the_message_and_reports_temperror() {
        let mut config = config();
        config.policy.dmarc_failure_action = "reject".to_string();
        // Every lookup fails: the resolvers below have no answers and are marked
        // failing, which is what `MockResolver` returns for a broken resolver.
        let resolver = MockResolver::new()
            .with_failure("example.net")
            .with_failure("_dmarc.example.net")
            .with_failure("example.net._domainkey")
            .with_failure("alice._domainkey.example.net");
        let policy = policy(resolver, &config);

        let raw = "From: alice@example.net\r\nTo: bob@mx.test\r\nSubject: hi\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;

        assert!(
            !verdict.is_reject(),
            "a DNS failure must never reject: {:?}",
            verdict.action
        );
        assert_eq!(verdict.action, PolicyAction::Accept);
        assert!(verdict.is_transient(), "the failure must be visible as transient");

        let header = verdict.header_value().expect("a header is still produced");
        assert!(
            header.contains("temperror"),
            "the header must report temperror, not a misleading fail: {header}"
        );
        assert!(!header.contains("spf=fail"), "{header}");
        assert!(!header.contains("dmarc=fail"), "{header}");
    }

    #[tokio::test]
    async fn a_dmarc_reject_failure_is_a_rejection_when_local_policy_agrees() {
        let mut config = config();
        config.policy.dmarc_failure_action = "reject".to_string();
        // No SPF, and DMARC says reject: nothing aligns, so this is a definite failure.
        let resolver = MockResolver::new()
            .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=reject".to_string()]);
        let policy = policy(resolver, &config);

        let raw = "From: alice@example.net\r\nTo: bob@mx.test\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;

        assert!(verdict.dmarc_failed(), "{:?}", verdict.dmarc);
        assert_eq!(verdict.action, PolicyAction::Reject);
        assert!(verdict.header_value().expect("a header").contains("dmarc=fail"));
    }

    #[tokio::test]
    async fn a_dmarc_reject_failure_is_only_quarantined_when_local_policy_says_so() {
        let mut config = config();
        config.policy.dmarc_failure_action = "quarantine".to_string();
        let resolver = MockResolver::new()
            .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=reject".to_string()]);
        let policy = policy(resolver, &config);
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert_eq!(verdict.action, PolicyAction::Quarantine);
        assert!(!verdict.is_reject());
    }

    #[tokio::test]
    async fn dmarc_disabled_reports_none() {
        let mut config = config();
        config.policy.dmarc_enabled = false;
        let policy = policy(MockResolver::new(), &config);
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert_eq!(
            verdict.dmarc.as_ref().expect("a verdict").result,
            DmarcResult::None
        );
        assert_eq!(verdict.action, PolicyAction::Accept);
    }

    #[tokio::test]
    async fn spf_disabled_skips_the_spf_check_entirely() {
        let mut config = config();
        config.policy.spf_enabled = false;
        let policy = policy(MockResolver::new(), &config);
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert!(verdict.spf.is_none(), "{:?}", verdict.spf);
        assert!(!verdict.header_value().expect("a header").contains("spf="));
    }

    #[tokio::test]
    async fn a_missing_remote_address_skips_spf_rather_than_failing_it() {
        let config = config();
        let policy = policy(MockResolver::new(), &config);
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), None), &parsed(raw))
            .await;
        assert!(verdict.spf.is_none());
        assert_eq!(verdict.action, PolicyAction::Accept);
    }

    #[tokio::test]
    async fn a_null_sender_reports_no_spf_result() {
        let config = config();
        let policy = policy(MockResolver::new(), &config);
        let raw = "From: MAILER-DAEMON@mx.test\r\n\r\nbounce\r\n";
        let verdict = policy.evaluate(&message(raw, None, Some("203.0.113.7")), &parsed(raw)).await;
        let spf = verdict.spf.as_ref().expect("SPF ran");
        assert_eq!(spf.result, SpfResult::None);
        assert!(verdict
            .header_value()
            .expect("a header")
            .contains("smtp.mailfrom=<>"));
    }

    #[tokio::test]
    async fn dkim_verification_is_skipped_when_the_config_says_so() {
        let mut config = config();
        config.dkim.verify_inbound = false;
        let policy = policy(MockResolver::new(), &config);
        let raw = "From: alice@example.net\r\nDKIM-Signature: v=1; a=rsa-sha256; d=x.test; s=s; bh=a; b=b\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert!(verdict.dkim.is_empty(), "{:?}", verdict.dkim);
    }

    #[tokio::test]
    async fn an_unsigned_message_produces_no_dkim_result() {
        let config = config();
        let policy = policy(MockResolver::new(), &config);
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert!(
            verdict.dkim.is_empty(),
            "an unsigned message is not a finding: {:?}",
            verdict.dkim
        );
    }

    // ------------------------------------------------------------------
    // The header switch
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn the_header_is_omitted_when_policy_add_auth_results_is_off() {
        let mut config = config();
        config.policy.add_auth_results = false;
        // `[dkim] add_auth_results` is deliberately *not* consulted; both default to
        // true, so honouring it would leave the operator no way to turn the header off.
        let policy = policy(MockResolver::new(), &config);
        assert!(!policy.adds_auth_results());

        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert!(verdict.header_value().is_none());
        // The verdicts are still computed and logged.
        assert!(verdict.spf.is_some());
    }

    #[tokio::test]
    async fn the_header_names_this_server_as_the_authserv_id() {
        let config = config();
        let policy = policy(
            MockResolver::new().with_txt("example.net", vec!["v=spf1 -all".to_string()]),
            &config,
        );
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(&message(raw, Some("alice@example.net"), Some("203.0.113.7")), &parsed(raw))
            .await;
        assert!(
            verdict
                .header_value()
                .expect("a header")
                .starts_with("mx.test;"),
            "{:?}",
            verdict.header_value()
        );
    }

    // ------------------------------------------------------------------
    // The timeout
    // ------------------------------------------------------------------

    /// A resolver that never answers.
    ///
    /// `MockResolver` returns an immediately-ready future, so it cannot exercise the
    /// timeout: the whole evaluation would finish inside the first poll. This one stays
    /// pending forever, which is what a resolver pointed at a black hole looks like.
    #[derive(Debug)]
    struct HangingResolver;

    impl Resolver for HangingResolver {
        fn mx(&self, _domain: &str) -> futures_util::future::BoxFuture<'_, Result<Vec<MxHost>, FerromaError>> {
            Box::pin(std::future::pending())
        }

        fn addresses(&self, _name: &str) -> futures_util::future::BoxFuture<'_, Result<Vec<IpAddr>, FerromaError>> {
            Box::pin(std::future::pending())
        }

        fn txt(&self, _name: &str) -> futures_util::future::BoxFuture<'_, Result<Vec<String>, FerromaError>> {
            Box::pin(std::future::pending())
        }

        fn ptr(&self, _ip: IpAddr) -> futures_util::future::BoxFuture<'_, Result<Vec<String>, FerromaError>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn an_evaluation_that_times_out_accepts_without_a_header() {
        let config = config();
        // A resolver that never answers plus a short deadline: the evaluation is
        // abandoned, and abandoning it must not cost the message.
        let policy = InboundPolicy::new(&config, Arc::new(HangingResolver))
            .with_timeout(Duration::from_millis(50));
        assert_eq!(policy.timeout(), Duration::from_millis(50));

        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(
                &message(raw, Some("alice@example.net"), Some("203.0.113.7")),
                &parsed(raw),
            )
            .await;

        assert_eq!(verdict.action, PolicyAction::Accept, "a hang must not reject");
        assert!(
            verdict.header_value().is_none(),
            "a timed-out evaluation has no verdicts to report"
        );
        assert!(verdict.reason.expect("a reason").contains("timed out"));
    }

    // ------------------------------------------------------------------
    // Configuration wiring
    // ------------------------------------------------------------------

    #[test]
    fn the_evaluator_reports_whether_it_will_do_anything() {
        let mut config = config();
        assert!(InboundPolicy::new(&config, Arc::new(MockResolver::new())).is_enabled());

        config.policy.spf_enabled = false;
        config.dkim.verify_inbound = false;
        config.policy.dmarc_enabled = false;
        assert!(!InboundPolicy::new(&config, Arc::new(MockResolver::new())).is_enabled());
    }

    #[test]
    fn the_configured_failure_action_reaches_the_evaluator() {
        let config = Config {
            policy: ferroma_core::config::PolicyConfig {
                dmarc_failure_action: "quarantine".to_string(),
                ..config().policy
            },
            ..config()
        };
        let policy = InboundPolicy::new(&config, Arc::new(MockResolver::new()));
        assert_eq!(policy.dmarc.failure_action(), Some(DmarcPolicy::Quarantine));
    }

    // ------------------------------------------------------------------
    // The budget
    // ------------------------------------------------------------------

    /// The regression test for the fixed-60-second deadline.
    ///
    /// A policy step whose cost is bound by `[dns]` must shrink when `[dns]` shrinks.
    #[test]
    fn the_budget_follows_the_dns_configuration() {
        let impatient = Config {
            dns: ferroma_core::config::DnsConfig {
                timeout_secs: 1,
                attempts: 1,
                ..config().dns
            },
            ..config()
        };
        let patient = Config {
            dns: ferroma_core::config::DnsConfig {
                timeout_secs: 5,
                attempts: 3,
                ..config().dns
            },
            ..config()
        };

        let quick = policy_budget(&impatient);
        let slow = policy_budget(&patient);
        assert!(
            quick < slow,
            "an impatient DNS configuration must produce a shorter budget: {quick:?} vs {slow:?}"
        );
        // 1s x (attempts 1 + 1 packet) x (4 + spf_max_lookups 10) lookups.
        assert_eq!(quick, Duration::from_secs(28), "{quick:?}");
        // The default is generous enough that the cap is what applies.
        assert_eq!(slow, POLICY_TIMEOUT_CAP, "{slow:?}");
        assert!(quick < Duration::from_secs(60), "60s must not be the effective budget");
    }

    #[test]
    fn the_budget_is_never_zero_and_never_unbounded() {
        let tiny = Config {
            dns: ferroma_core::config::DnsConfig {
                timeout_secs: 0,
                attempts: 0,
                ..config().dns
            },
            policy: ferroma_core::config::PolicyConfig {
                spf_max_lookups: 0,
                ..config().policy
            },
            ..config()
        };
        let budget = policy_budget(&tiny);
        assert!(budget >= POLICY_TIMEOUT_FLOOR, "{budget:?}");
        assert!(budget <= POLICY_TIMEOUT_CAP, "{budget:?}");
        // Zero means "one attempt", so the smallest budget is 1s × 2 packets × 4 lookups.
        assert_eq!(budget, Duration::from_secs(8), "{budget:?}");

        let huge = Config {
            dns: ferroma_core::config::DnsConfig {
                timeout_secs: u64::MAX,
                attempts: usize::MAX,
                ..config().dns
            },
            ..config()
        };
        assert_eq!(policy_budget(&huge), POLICY_TIMEOUT_CAP, "and never exceeds the cap");
    }

    #[test]
    fn the_evaluator_takes_its_timeout_from_the_budget() {
        let config = Config {
            dns: ferroma_core::config::DnsConfig {
                timeout_secs: 1,
                attempts: 1,
                ..config().dns
            },
            ..config()
        };
        let policy = InboundPolicy::new(&config, Arc::new(MockResolver::new()));
        assert_eq!(policy.timeout(), policy_budget(&config));
        assert_eq!(policy.timeout(), Duration::from_secs(28));
    }

    // ------------------------------------------------------------------
    // The short-circuit
    // ------------------------------------------------------------------

    /// When the first lookup cannot be answered, the rest are not attempted.
    ///
    /// Asserted on the resolver's own query count, which is the only way to see that the
    /// work was skipped rather than merely bounded.
    #[tokio::test]
    async fn a_dead_resolver_costs_one_lookup_not_one_per_stage() {
        let config = config();
        let signing = signed_message();
        let resolver = Arc::new(
            MockResolver::new()
                .with_failure("example.net")
                .with_failure("_dmarc.example.net")
                .with_failure("sel._domainkey.example.net"),
        );
        let policy = InboundPolicy::new(&config, resolver.clone());

        let verdict = policy
            .evaluate(
                &message_bytes(&signing.raw, Some("alice@example.net"), Some("203.0.113.7")),
                &signing.parsed,
            )
            .await;

        assert_eq!(verdict.action, PolicyAction::Accept);
        assert!(verdict.is_transient());
        // Exactly one DNS query: SPF's. DKIM and DMARC were reported as `temperror`
        // without asking the resolver anything.
        assert_eq!(
            resolver.query_count(),
            1,
            "queries: {:?}",
            resolver.queries()
        );
        // The fixture is a real signature over a real key, so the DKIM step *would* have
        // queried had it been attempted.
        assert!(signing.record.starts_with("v=DKIM1; k=rsa; p="));
        let header = verdict.header_value().expect("a header");
        assert!(header.contains("spf=temperror"), "{header}");
        assert!(header.contains("dkim=temperror"), "{header}");
        assert!(header.contains("dmarc=temperror"), "{header}");
    }

    #[tokio::test]
    async fn a_healthy_resolver_still_attempts_every_stage() {
        let config = config();
        let resolver = Arc::new(
            MockResolver::new()
                .with_txt("example.net", vec!["v=spf1 ip4:203.0.113.0/24 -all".to_string()])
                .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]),
        );
        let policy = InboundPolicy::new(&config, resolver.clone());

        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let verdict = policy
            .evaluate(
                &message(raw, Some("alice@example.net"), Some("203.0.113.7")),
                &parsed(raw),
            )
            .await;

        assert_eq!(verdict.action, PolicyAction::Accept);
        assert!(!verdict.is_transient());
        // SPF's record and DMARC's, and no short-circuit.
        assert_eq!(resolver.query_count(), 2, "queries: {:?}", resolver.queries());
        let header = verdict.header_value().expect("a header");
        assert!(header.contains("spf=pass"), "{header}");
        assert!(header.contains("dmarc=pass"), "{header}");
    }

    #[tokio::test]
    async fn the_short_circuit_also_fires_when_spf_is_disabled() {
        // With SPF off, DKIM is the probe: a `TempError` on its key record must stop the
        // DMARC lookup. The message is *really* signed, so the verifier genuinely reaches
        // the DNS step rather than bailing out on a malformed header.
        let signing = signed_message();
        let config = Config {
            policy: ferroma_core::config::PolicyConfig {
                spf_enabled: false,
                ..config().policy
            },
            ..config()
        };
        let resolver = Arc::new(
            MockResolver::new()
                .with_failure("example.net")
                .with_failure("sel._domainkey.example.net"),
        );
        let policy = InboundPolicy::new(&config, resolver.clone());

        let verdict = policy
            .evaluate(
                &message_bytes(
                    &signing.raw,
                    Some("alice@example.net"),
                    Some("203.0.113.7"),
                ),
                &signing.parsed,
            )
            .await;

        assert_eq!(verdict.action, PolicyAction::Accept);
        assert!(
            verdict.is_transient(),
            "the key lookup failed transiently: {:?}",
            verdict.dkim
        );
        assert!(
            !resolver.queries().iter().any(|q| q.contains("_dmarc")),
            "DMARC must not have been attempted: {:?}",
            resolver.queries()
        );
    }

    #[tokio::test]
    async fn a_message_with_no_signature_costs_no_dkim_lookup() {
        let config = config();
        let resolver = Arc::new(
            MockResolver::new()
                .with_txt("example.net", vec!["v=spf1 ip4:203.0.113.0/24 -all".to_string()])
                .with_txt("_dmarc.example.net", vec!["v=DMARC1; p=none".to_string()]),
        );
        let policy = InboundPolicy::new(&config, resolver.clone());
        let raw = "From: alice@example.net\r\n\r\nbody\r\n";
        let _ = policy
            .evaluate(
                &message(raw, Some("alice@example.net"), Some("203.0.113.7")),
                &parsed(raw),
            )
            .await;
        assert!(
            !resolver.queries().iter().any(|q| q.contains("_domainkey")),
            "an unsigned message must not look a key record up: {:?}",
            resolver.queries()
        );
    }

    #[test]
    fn the_cap_and_floor_are_sane() {
        assert!(POLICY_TIMEOUT_FLOOR >= Duration::from_secs(1));
        assert!(POLICY_TIMEOUT_FLOOR < POLICY_TIMEOUT_CAP);
        assert!(POLICY_TIMEOUT_CAP <= Duration::from_secs(120));
    }

    #[test]
    fn spf_checker_and_dmarc_checker_share_one_resolver() {
        let resolver = Arc::new(
            MockResolver::new().with_txt("example.net", vec!["v=spf1 -all".to_string()]),
        );
        let config = config();
        let policy = InboundPolicy::new(&config, resolver);
        // Both checkers see the same answers, which is what makes the resolver-level
        // cache effective across SPF and DMARC.
        assert!(Arc::ptr_eq(policy.spf.resolver(), policy.dmarc.resolver()));
    }

    #[test]
    fn the_spf_checker_honours_the_lookup_budget() {
        let policy_config = PolicyConfig {
            spf_max_lookups: 3,
            ..PolicyConfig::default()
        };
        let checker = SpfChecker::new(Arc::new(MockResolver::new()), &policy_config);
        assert_eq!(checker.max_lookups(), 3);
    }
}
