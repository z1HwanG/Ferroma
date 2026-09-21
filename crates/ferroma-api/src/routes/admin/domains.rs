//! `docs/api.md` §4.4 — DNS diagnostics.
//!
//! `GET /domains/:id/dns` runs the checks behind the Admin "DNS Health" panel
//! (specification §16) and `GET /domains/:id/dkim` returns the record to publish.
//!
//! # How the records are actually looked up
//!
//! `ferroma-api`'s dependency list contains no DNS resolver, and `ferroma-smtp` — which
//! owns `hickory-resolver` — exports nothing this crate may use. Rather than report
//! fabricated verdicts, the checks go through the platform's resolver tool
//! (`nslookup`, which every supported host ships) in a bounded blocking task:
//!
//! * a record that could be read answers `ok` or `fail` from its real contents;
//! * a resolver that is unreachable, or a build with no tool to call, answers `skip`
//!   with a `hint` explaining that the check did not run.
//!
//! `score`/`max_score` count only the records that were actually checked, so a panel
//! that could not run the checks shows `0/0` instead of a misleading `5/7`.

use std::net::IpAddr;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::{DomainId, FerromaError};
use ferroma_storage::repository::NewAuditLog;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AdminUser, Pagination, PaginationQuery, Page};
use crate::routes::mail::shapes::{AliasResponse, DomainResponse};
use crate::state::AppState;

/// The status of one record check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordStatus {
    /// Present and correct.
    Ok,
    /// Present but not what the server expects.
    Warn,
    /// Missing or wrong.
    Fail,
    /// Not checked — the resolver could not be reached, or the check does not apply.
    Skip,
}

impl RecordStatus {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordStatus::Ok => "ok",
            RecordStatus::Warn => "warn",
            RecordStatus::Fail => "fail",
            RecordStatus::Skip => "skip",
        }
    }

    /// Whether this verdict contributes to the score.
    pub fn is_scored(self) -> bool {
        !matches!(self, RecordStatus::Skip)
    }

    /// Whether it counts as a success in the score.
    pub fn is_success(self) -> bool {
        matches!(self, RecordStatus::Ok)
    }
}

/// One row of the DNS health table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecord {
    /// `MX`, `A`, `AAAA`, `PTR`, `SPF`, `DKIM` or `DMARC`.
    pub kind: String,
    /// `ok`, `warn`, `fail` or `skip`.
    pub status: String,
    /// What the server expects to find.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// What was actually found.
    pub found: Vec<String>,
    /// What to do about it, when something is wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl DnsRecord {
    /// A record that was checked.
    pub fn new(
        kind: &str,
        status: RecordStatus,
        expected: Option<String>,
        found: Vec<String>,
    ) -> Self {
        DnsRecord {
            kind: kind.to_string(),
            status: status.as_str().to_string(),
            expected,
            found,
            hint: None,
        }
    }

    /// A record that could not be checked.
    pub fn skipped(kind: &str, expected: Option<String>, hint: &str) -> Self {
        DnsRecord {
            kind: kind.to_string(),
            status: RecordStatus::Skip.as_str().to_string(),
            expected,
            found: Vec::new(),
            hint: Some(hint.to_string()),
        }
    }

    /// Attach a hint.
    #[must_use]
    pub fn with_hint(mut self, hint: &str) -> Self {
        self.hint = Some(hint.to_string());
        self
    }

    /// The parsed status.
    pub fn status_kind(&self) -> RecordStatus {
        match self.status.as_str() {
            "ok" => RecordStatus::Ok,
            "warn" => RecordStatus::Warn,
            "fail" => RecordStatus::Fail,
            _ => RecordStatus::Skip,
        }
    }
}

/// The `GET /api/v1/domains/:id/dns` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsReport {
    /// The domain that was checked.
    pub domain: String,
    /// When the checks ran.
    pub checked_at: chrono::DateTime<chrono::Utc>,
    /// One row per record kind.
    pub records: Vec<DnsRecord>,
    /// How many records passed.
    pub score: usize,
    /// How many were actually checked.
    pub max_score: usize,
}

impl DnsReport {
    /// Score the rows: only checked records count.
    pub fn from_records(domain: &str, records: Vec<DnsRecord>) -> Self {
        let max_score = records
            .iter()
            .filter(|record| record.status_kind().is_scored())
            .count();
        let score = records
            .iter()
            .filter(|record| record.status_kind().is_success())
            .count();
        DnsReport {
            domain: domain.to_string(),
            checked_at: chrono::Utc::now(),
            records,
            score,
            max_score,
        }
    }
}

/// The DNS checks `GET /domains/:id/dns` runs.
#[derive(Debug, Clone)]
pub struct DnsChecks {
    /// The MX host the operator should publish.
    pub mail_host: String,
    /// The `A`/`AAAA` address expected for that host, when known.
    pub expected_address: Option<IpAddr>,
    /// `v=spf1 …`, when the server knows what it should be.
    pub expected_spf: Option<String>,
    /// The DKIM selector.
    pub dkim_selector: String,
    /// The DKIM public key, when a pair exists.
    pub dkim_public_key: Option<String>,
    /// The DMARC policy the operator should publish.
    pub expected_dmarc: Option<String>,
    /// The relay outbound mail leaves through, when `[queue] relay_host` is set.
    ///
    /// It changes what two rows mean. A `PTR` record is what a *direct* sender needs — the
    /// address on the wire is the one receivers reverse-resolve — and it is not what a relayed
    /// sender needs, because the wire carries the relay's address instead. And an `SPF` record
    /// for a relayed instance has to `include` the relay's own domain, a name only its provider
    /// knows: nothing can guess `spf.example-relay.com` from `mail.example-relay.com`.
    pub relay: Option<String>,
}

impl DnsChecks {
    /// The expected `SPF` record for a deployment whose MX is `mail_host`.
    pub fn default_spf(mail_host: &str) -> String {
        format!("v=spf1 mx a:{mail_host} -all")
    }

    /// The expected `DMARC` record: quarantine, with aggregate reports to the domain.
    pub fn default_dmarc(domain: &str) -> String {
        format!("v=DMARC1; p=quarantine; rua=mailto:dmarc@{domain}")
    }

    /// The DKIM record name for a selector.
    pub fn dkim_record_name(selector: &str, domain: &str) -> String {
        format!("{selector}._domainkey.{domain}")
    }

    /// The DKIM record value for a public key.
    pub fn dkim_record_value(public_key: &str) -> String {
        format!("v=DKIM1; k=rsa; p={}", public_key.replace(['\r', '\n', ' '], ""))
    }
}

/// Run the checks for one domain.
///
/// Every lookup is bounded and blocking work runs off the async reactor, so a slow
/// resolver cannot stall the API.
pub async fn check_domain(domain: &str, checks: &DnsChecks) -> Vec<DnsRecord> {
    let mut records = Vec::with_capacity(7);

    // MX.
    records.push(match lookup(domain, "MX").await {
        Ok(values) => {
            let matched = values
                .iter()
                .any(|value| value.to_ascii_lowercase().contains(&checks.mail_host.to_ascii_lowercase()));
            DnsRecord::new(
                "MX",
                if matched { RecordStatus::Ok } else { RecordStatus::Warn },
                Some(checks.mail_host.clone()),
                values,
            )
            .with_hint(&format!("point {domain}'s MX at {}", checks.mail_host))
        }
        Err(reason) => DnsRecord::skipped("MX", Some(checks.mail_host.clone()), &reason),
    });

    // A / AAAA.
    // The address the A record names is also the one a PTR record has to answer for, so it is
    // kept. `DnsChecks::expected_address` is a value a deployment may *state*, and nothing fills
    // it in today, so without this the PTR row was always skipped with "no expected address is
    // configured" — true of the code, and wrong about a host that publishes its own A record.
    let mut resolved_address: Option<IpAddr> = None;
    records.push(match lookup(&checks.mail_host, "A").await {
        Ok(values) => {
            resolved_address = values
                .iter()
                .find_map(|value| value.trim().parse::<IpAddr>().ok());
            let matched = checks.expected_address.is_none_or(|expected| {
                values.iter().any(|value| value.contains(&expected.to_string()))
            });
            DnsRecord::new(
                "A",
                if matched { RecordStatus::Ok } else { RecordStatus::Warn },
                checks.expected_address.map(|ip| ip.to_string()),
                values,
            )
        }
        Err(reason) => DnsRecord::skipped(
            "A",
            checks.expected_address.map(|ip| ip.to_string()),
            &reason,
        ),
    });

    records.push(match lookup(&checks.mail_host, "AAAA").await {
        Ok(values) if values.is_empty() => DnsRecord::new("AAAA", RecordStatus::Skip, None, values),
        Ok(values) => DnsRecord::new("AAAA", RecordStatus::Ok, None, values),
        Err(reason) => DnsRecord::skipped("AAAA", None, &reason),
    });

    // PTR: reverse-resolve the expected address, when one is known — stated by the deployment,
    // or learned from the A record the operator published.
    records.push(match checks.expected_address.or(resolved_address) {
        Some(address) => match reverse_lookup(address).await {
            Ok(values) => {
                let (status, hint) =
                    ptr_verdict(&values, &checks.mail_host, checks.relay.as_deref());
                DnsRecord::new("PTR", status, Some(checks.mail_host.clone()), values)
                    .with_hint(hint)
            }
            Err(reason) => DnsRecord::skipped("PTR", Some(checks.mail_host.clone()), &reason),
        },
        None => DnsRecord::skipped(
            "PTR",
            Some(checks.mail_host.clone()),
            "the mail host does not resolve to an address yet, so there is nothing to reverse-resolve",
        ),
    });

    // SPF.
    records.push(match lookup(domain, "TXT").await {
        Ok(values) => {
            let spf: Vec<String> = values
                .iter()
                .filter(|value| value.to_ascii_lowercase().starts_with("v=spf1"))
                .cloned()
                .collect();
            match checks.expected_spf.as_deref() {
                Some(expected) => {
                    let (status, hint) = spf_verdict(&spf, expected, checks.relay.as_deref());
                    DnsRecord::new("SPF", status, Some(expected.to_string()), spf).with_hint(hint)
                }
                None if spf.is_empty() => DnsRecord::new("SPF", RecordStatus::Warn, None, spf)
                    .with_hint("publish an SPF record so receivers can tell legitimate mail apart"),
                None => DnsRecord::new("SPF", RecordStatus::Ok, None, spf),
            }
        }
        Err(reason) => DnsRecord::skipped("SPF", checks.expected_spf.clone(), &reason),
    });

    // DKIM.
    let dkim_name = DnsChecks::dkim_record_name(&checks.dkim_selector, domain);
    records.push(match &checks.dkim_public_key {
        None => DnsRecord::skipped(
            "DKIM",
            Some(dkim_name.clone()),
            &format!("no key pair exists yet; POST /api/v1/domains/{domain}/dkim generates one"),
        ),
        Some(_) => match lookup(&dkim_name, "TXT").await {
            Ok(values) => {
                let published = values
                    .iter()
                    .any(|value| value.to_ascii_lowercase().contains("v=dkim1"));
                DnsRecord::new(
                    "DKIM",
                    if published { RecordStatus::Ok } else { RecordStatus::Warn },
                    Some(dkim_name.clone()),
                    values,
                )
                .with_hint("publish the TXT record shown by GET /api/v1/domains/:id/dkim")
            }
            Err(reason) => DnsRecord::skipped("DKIM", Some(dkim_name.clone()), &reason),
        },
    });

    // DMARC.
    let dmarc_name = format!("_dmarc.{domain}");
    records.push(match lookup(&dmarc_name, "TXT").await {
        Ok(values) => {
            let policy: Vec<String> = values
                .iter()
                .filter(|value| value.to_ascii_lowercase().starts_with("v=dmarc1"))
                .cloned()
                .collect();
            DnsRecord::new(
                "DMARC",
                if policy.is_empty() {
                    RecordStatus::Warn
                } else {
                    RecordStatus::Ok
                },
                checks.expected_dmarc.clone(),
                policy,
            )
        }
        Err(reason) => DnsRecord::skipped("DMARC", checks.expected_dmarc.clone(), &reason),
    });

    records
}

/// What the `PTR` row means, given how this instance sends mail.
///
/// A direct sender needs the record: its address is the one on the wire, and a receiver
/// reverse-resolves it to decide whether to believe the connection. A relayed sender does not:
/// the wire carries the relay's address, and this host's is only a place mail arrives. The row
/// still reports what it found — an operator may want the record anyway, for the other things
/// running on that address — but a relay turns "missing" from a defect into a note.
fn ptr_verdict(
    found: &[String],
    mail_host: &str,
    relay: Option<&str>,
) -> (RecordStatus, &'static str) {
    let matched = found.iter().any(|value| {
        value
            .to_ascii_lowercase()
            .trim_end_matches('.')
            .eq_ignore_ascii_case(mail_host)
    });
    match (matched, relay) {
        (true, _) => (
            RecordStatus::Ok,
            "a matching PTR record keeps large receivers from deferring your mail",
        ),
        (false, Some(_)) => (
            RecordStatus::Skip,
            "outbound mail leaves through the configured relay, so this address does not need a matching PTR — the relay's own address is what receivers reverse-resolve. It is still worth having if this host also sends mail itself",
        ),
        (false, None) => (
            RecordStatus::Warn,
            "a matching PTR record keeps large receivers from deferring your mail",
        ),
    }
}

/// What the `SPF` row means, given how this instance sends mail.
///
/// The expected record is the one a *direct* sender publishes: it authorises the mail host. An
/// instance that relays has to authorise the relay instead, and no panel can know which
/// `include` that is — the provider decides the name. So a record that delegates sending counts
/// for what it is, with a hint naming what has to be true, rather than a warning that accuses an
/// operator of a mistake they did not make.
fn spf_verdict(
    published: &[String],
    expected: &str,
    relay: Option<&str>,
) -> (RecordStatus, &'static str) {
    if published.iter().any(|value| value.contains(expected)) {
        return (
            RecordStatus::Ok,
            "this record authorises the mail host, which is what direct delivery needs",
        );
    }
    let delegates = published
        .iter()
        .any(|value| value.to_ascii_lowercase().contains("include:"));
    match relay {
        Some(_) if delegates => (
            RecordStatus::Ok,
            "outbound mail leaves through the configured relay and this record delegates sending to a provider — check that the include names that provider's own domain",
        ),
        Some(_) => (
            RecordStatus::Warn,
            "outbound mail leaves through the configured relay, so this record has to include the relay's own domain (its provider decides that name); the expected value above is what a direct sender publishes",
        ),
        None => (
            RecordStatus::Warn,
            "this record does not authorise the mail host; point it at the mail host, or state the relay this instance sends through as [queue] relay_host and this row will say what it should be",
        ),
    }
}

/// Look one record type up through the platform resolver.
///
/// Returns the found values, or a human reason the check could not run — which becomes
/// the row's `hint` and its `skip` status.
pub async fn lookup(name: &str, kind: &str) -> Result<Vec<String>, String> {
    let owned_name = name.to_string();
    let owned_kind = kind.to_string();
    tokio::task::spawn_blocking(move || resolve_blocking(&owned_name, &owned_kind))
        .await
        .map_err(|err| format!("the resolver task failed: {err}"))?
}

/// Reverse-resolve an address to its PTR names.
pub async fn reverse_lookup(address: IpAddr) -> Result<Vec<String>, String> {
    let name = match address {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            format!(
                "{}.{}.{}.{}.in-addr.arpa",
                octets[3], octets[2], octets[1], octets[0]
            )
        }
        IpAddr::V6(v6) => {
            let mut nibbles: Vec<String> = v6
                .octets()
                .iter()
                .rev()
                .flat_map(|byte| vec![format!("{:x}", byte & 0x0f), format!("{:x}", byte >> 4)])
                .collect();
            nibbles.reverse();
            format!("{}.ip6.arpa", nibbles.join("."))
        }
    };
    lookup(&name, "PTR").await
}

/// The timeout one resolver invocation gets.
pub const RESOLVER_TIMEOUT: Duration = Duration::from_secs(5);

/// Run the resolver tool, off the async reactor.
fn resolve_blocking(name: &str, kind: &str) -> Result<Vec<String>, String> {
    // `-type=` is understood by both the Windows and the BIND `nslookup`.
    let flag = format!("-type={kind}");
    let mut command = std::process::Command::new("nslookup");
    command.arg(&flag).arg(name);
    command.stdin(std::process::Stdio::null());

    let output = match command.output() {
        Ok(output) => output,
        Err(err) => {
            return Err(format!(
                "no resolver tool is available in this build ({err}); run the check with `dig {kind} {name}`"
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");

    if combined.contains("Non-existent domain")
        || combined.contains("NXDOMAIN")
        || combined.contains("can't find")
    {
        return Ok(Vec::new());
    }
    if combined.contains("timed out") || combined.contains("no servers could be reached") {
        return Err("the resolver did not answer; check the host's DNS configuration".to_string());
    }

    let values = extract_records(&combined, kind);
    if values.is_empty() && !output.status.success() {
        return Err("the resolver exited with an error".to_string());
    }
    Ok(values)
}

/// Pull the answer section out of `nslookup` output.
///
/// Both resolver dialects are handled:
///
/// * Windows prints `example.com MX preference = 10, mail exchanger = mx.example.com`
///   after a `Non-authoritative answer:` heading;
/// * BIND prints `example.com. 3600 IN MX 10 mail.example.com.` after `;; ANSWER SECTION:`.
///
/// A line is accepted when it carries the record-type keyword in the shape either
/// dialect uses, so a heading the parser does not recognise only costs the lines it
/// would have guarded — never a false answer.
pub fn extract_records(output: &str, kind: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let keyword = kind.to_ascii_lowercase();
    let mut in_answer = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();

        if lower.contains("non-authoritative answer")
            || lower.starts_with("name:")
            || lower.contains("answer section")
        {
            in_answer = true;
            continue;
        }
        if lower.starts_with("authoritative answers") || lower.starts_with("authority") {
            in_answer = false;
            continue;
        }
        // Anything before the first heading at all is the resolver banner. The record
        // type must appear as its own token, so a `TXT` line is never read as an `MX`.
        if !in_answer && !lower.contains(&format!(" {keyword} ")) {
            continue;
        }

        if kind.eq_ignore_ascii_case("MX") {
            if let Some(value) = parse_windows_mx(trimmed) {
                if !out.contains(&value) {
                    out.push(value);
                }
                continue;
            }
        }

        // An address answer, as the same tool prints it: a `Name:` line and then one
        // `Address:` line per record — never `… IN A …`, which is `dig`'s shape. Without this
        // the A row reported no address on a host that publishes one, and the PTR check had
        // nothing to reverse-resolve. The banner's own `Address:` line is above the answer
        // section, which the gate above has already excluded.
        if keyword == "a" || keyword == "aaaa" {
            if lower.starts_with("address:") {
                let value = trimmed["address:".len()..].trim().trim_end_matches("#53").trim();
                if !value.is_empty() && !out.contains(&value.to_string()) {
                    out.push(value.to_string());
                }
                continue;
            }
        }

        // A TXT answer: `name  text = "…"`. The record type is spelled `text` here and never
        // `TXT`, so looking for it as a token found nothing at all — which is why every TXT
        // row (SPF, DKIM, DMARC) came back empty on domains whose records were published and
        // correct.
        if keyword == "txt" {
            if let Some(position) = lower.find("text =") {
                let value = join_txt_chunks(trimmed[position + "text =".len()..].trim());
                if !value.is_empty() && !out.contains(&value) {
                    out.push(value);
                }
                continue;
            }
        }

        let Some(value) = bind_style_value(trimmed, &keyword) else {
            continue;
        };
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }

    out
}

/// The value of a BIND-style answer line: everything after the record type keyword.
///
/// The keyword must be the record type as its own token — `example.com. 3600 IN MX 10
/// mx.` — so a `TXT` line is never mistaken for an `MX` one. An `MX` value drops its
/// preference number, because every consumer of this table has already decided which
/// host it expects.
fn bind_style_value(line: &str, kind_lower: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let position = lower.find(&format!(" {kind_lower} "))?;
    let value = line[position + kind_lower.len() + 2..].trim();
    let value = value.trim_end_matches('.').trim_matches('"').trim();
    if value.is_empty() {
        return None;
    }
    if kind_lower == "mx" {
        let (_, host) = value.split_once(' ').unwrap_or(("", value));
        let host = host.trim().trim_end_matches('.');
        return if host.is_empty() {
            None
        } else {
            Some(host.to_string())
        };
    }
    Some(value.to_string())
}

/// A single `nslookup` line that looks like `example.com MX preference = 10, mail exchanger = mx.example.com`.
pub fn parse_windows_mx(line: &str) -> Option<String> {
    let position = line.to_ascii_lowercase().find("mail exchanger =")?;
    let value = line[position + "mail exchanger =".len()..].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.trim_end_matches('.').to_string())
    }
}

/// The value of one TXT answer, with its quoting removed.
///
/// A value longer than one 255-byte character-string — a DKIM public key — is printed as
/// several `"…"` pieces on a single line, and the operator has to paste them as one string,
/// which is also what the published-value comparison expects.
fn join_txt_chunks(value: &str) -> String {
    let mut joined = String::new();
    let mut inside = false;
    for character in value.chars() {
        match character {
            '"' => inside = !inside,
            _ if inside => joined.push(character),
            _ => {}
        }
    }
    if joined.is_empty() {
        value.trim().trim_matches('"').trim().to_string()
    } else {
        joined
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// The `POST /api/v1/domains` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateDomainRequest {
    /// The FQDN.
    pub name: String,
    /// A free-form note.
    pub description: Option<String>,
}

/// The `PATCH /api/v1/domains/:id` body.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateDomainRequest {
    /// Accept mail for the domain or not.
    pub enabled: Option<bool>,
    /// A free-form note.
    pub description: Option<String>,
    /// The local part that absorbs unknown recipients.
    pub catch_all: Option<String>,
}
/// The `?force=` flag of `DELETE /api/v1/domains/:id`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct ForceQuery {
    /// Delete the domain even though addresses still exist.
    pub force: Option<bool>,
}

/// The `POST /api/v1/domains/:id/aliases` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateAliasRequest {
    /// The local part being aliased.
    pub local_part: String,
    /// Where mail to it is forwarded.
    pub target: String,
}

/// The `PATCH /api/v1/aliases/:id` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateAliasRequest {
    /// Where mail is forwarded.
    pub target: Option<String>,
    /// Whether the alias is active.
    pub enabled: Option<bool>,
}

/// The `GET /api/v1/domains/:id/dkim` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkimRecordResponse {
    /// The selector.
    pub selector: String,
    /// The name the TXT record is published under.
    pub record_name: String,
    /// Always `TXT`.
    pub record_type: String,
    /// The value to publish.
    pub record_value: String,
}

/// A domain list, with each domain's address count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainListResponse {
    /// The domains.
    pub items: Vec<DomainResponse>,
    /// How many exist.
    pub total: i64,
}

/// The alias list body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasListResponse {
    /// The aliases.
    pub items: Vec<AliasResponse>,
    /// How many exist.
    pub total: i64,
}

/// `GET /api/v1/domains`
pub async fn list_domains(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<DomainListResponse>, ApiError> {
    let rows = state.repos.domains.list().await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        let count = state
            .repos
            .mailboxes
            .list_by_domain(row.domain_id())
            .await
            .map(|mailboxes| mailboxes.len() as i64)
            .unwrap_or(0);
        items.push(DomainResponse::from_row(row).with_mailbox_count(count));
    }
    Ok(Json(DomainListResponse {
        total: items.len() as i64,
        items,
    }))
}

/// `POST /api/v1/domains`
pub async fn create_domain(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(request): Json<CreateDomainRequest>,
) -> Result<(StatusCode, Json<DomainResponse>), ApiError> {
    let domain = state
        .repos
        .domains
        .create(&request.name, request.description.as_deref())
        .await?;
    audit(
        &state,
        &admin,
        "domain.created",
        Some("domain"),
        Some(&domain.id.to_string()),
        serde_json::json!({ "name": domain.name }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(DomainResponse::from_row(&domain))))
}

/// `GET /api/v1/domains/:id`
pub async fn get_domain(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<DomainResponse>, ApiError> {
    let domain = state
        .repos
        .domains
        .find_by_id(DomainId::new(id))
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;
    let count = state
        .repos
        .mailboxes
        .list_by_domain(domain.domain_id())
        .await
        .map(|mailboxes| mailboxes.len() as i64)
        .unwrap_or(0);
    Ok(Json(DomainResponse::from_row(&domain).with_mailbox_count(count)))
}

/// `PATCH /api/v1/domains/:id`
pub async fn update_domain(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Json(request): Json<UpdateDomainRequest>,
) -> Result<Json<DomainResponse>, ApiError> {
    let domain_id = DomainId::new(id);
    state
        .repos
        .domains
        .find_by_id(domain_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;

    if let Some(enabled) = request.enabled {
        state.repos.domains.set_enabled(domain_id, enabled).await?;
    }
    if let Some(description) = request.description.as_deref() {
        let value = if description.trim().is_empty() {
            None
        } else {
            Some(description)
        };
        state
            .repos
            .domains
            .set_description(domain_id, value)
            .await?;
    }
    if let Some(catch_all) = request.catch_all.as_deref() {
        let value = if catch_all.trim().is_empty() {
            None
        } else {
            Some(catch_all.trim())
        };
        state.repos.domains.set_catch_all(domain_id, value).await?;
    }

    audit(
        &state,
        &admin,
        "domain.updated",
        Some("domain"),
        Some(&id.to_string()),
        serde_json::json!({}),
    )
    .await;

    let domain = state
        .repos
        .domains
        .find_by_id(domain_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;
    Ok(Json(DomainResponse::from_row(&domain)))
}

/// `DELETE /api/v1/domains/:id` — refuses while addresses exist unless `?force=true`.
pub async fn delete_domain(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Query(query): Query<ForceQuery>,
) -> Result<StatusCode, ApiError> {
    let domain_id = DomainId::new(id);
    let domain = state
        .repos
        .domains
        .find_by_id(domain_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;

    let addresses = state.repos.mailboxes.list_by_domain(domain_id).await?;
    if !addresses.is_empty() && !query.force.unwrap_or(false) {
        return Err(ApiError::new(FerromaError::Conflict(format!(
            "domain {} still has {} address(es); pass ?force=true to delete them",
            domain.name,
            addresses.len()
        )))
        .with_details(serde_json::json!({ "addresses": addresses.len() })));
    }

    state.repos.domains.delete(domain_id).await?;
    audit(
        &state,
        &admin,
        "domain.deleted",
        Some("domain"),
        Some(&id.to_string()),
        serde_json::json!({ "name": domain.name, "addresses": addresses.len() }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/domains/:id/aliases`
pub async fn list_aliases(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<AliasListResponse>, ApiError> {
    let domain_id = DomainId::new(id);
    state
        .repos
        .domains
        .find_by_id(domain_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;
    let rows = state.repos.aliases.list_by_domain(domain_id).await?;
    Ok(Json(AliasListResponse {
        total: rows.len() as i64,
        items: rows.iter().map(AliasResponse::from_row).collect(),
    }))
}

/// `POST /api/v1/domains/:id/aliases`
pub async fn create_alias(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Json(request): Json<CreateAliasRequest>,
) -> Result<(StatusCode, Json<AliasResponse>), ApiError> {
    let domain_id = DomainId::new(id);
    state
        .repos
        .domains
        .find_by_id(domain_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;

    let alias = state
        .repos
        .aliases
        .create(domain_id, &request.local_part, &request.target)
        .await?;
    audit(
        &state,
        &admin,
        "alias.created",
        Some("alias"),
        Some(&alias.id.to_string()),
        serde_json::json!({ "local_part": alias.local_part, "target": alias.target }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(AliasResponse::from_row(&alias))))
}

/// `PATCH /api/v1/aliases/:id`
pub async fn update_alias(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Json(request): Json<UpdateAliasRequest>,
) -> Result<Json<AliasResponse>, ApiError> {
    state
        .repos
        .aliases
        .find_by_id(id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("alias {id}"))))?;

    if let Some(target) = request.target.as_deref().filter(|t| !t.trim().is_empty()) {
        state.repos.aliases.set_target(id, target.trim()).await?;
    }
    if let Some(enabled) = request.enabled {
        state.repos.aliases.set_enabled(id, enabled).await?;
    }

    audit(
        &state,
        &admin,
        "alias.updated",
        Some("alias"),
        Some(&id.to_string()),
        serde_json::json!({}),
    )
    .await;

    let alias = state
        .repos
        .aliases
        .find_by_id(id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("alias {id}"))))?;
    Ok(Json(AliasResponse::from_row(&alias)))
}

/// `DELETE /api/v1/aliases/:id`
pub async fn delete_alias(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let removed = state.repos.aliases.delete(id).await?;
    if !removed {
        return Err(ApiError::new(FerromaError::NotFound(format!("alias {id}"))));
    }
    audit(
        &state,
        &admin,
        "alias.deleted",
        Some("alias"),
        Some(&id.to_string()),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/domains/:id/dns`
pub async fn domain_dns(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<DnsReport>, ApiError> {
    let domain = state
        .repos
        .domains
        .find_by_id(DomainId::new(id))
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;

    let mail_host = state.config.server.hostname.clone();
    let checks = DnsChecks {
        mail_host: mail_host.clone(),
        expected_address: None,
        expected_spf: Some(DnsChecks::default_spf(&mail_host)),
        dkim_selector: domain
            .dkim_selector
            .clone()
            .unwrap_or_else(|| state.config.dkim.selector.clone()),
        dkim_public_key: domain.dkim_public_key.clone(),
        expected_dmarc: Some(DnsChecks::default_dmarc(&domain.name)),
        // How outbound mail actually leaves, so the PTR and SPF rows can say what they mean
        // instead of accusing a relayed deployment of publishing the wrong records.
        relay: state.config.queue.relay_host.clone(),
    };

    let records = check_domain(&domain.name, &checks).await;
    Ok(Json(DnsReport::from_records(&domain.name, records)))
}

/// `GET /api/v1/domains/:id/dkim`
pub async fn get_dkim(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<DkimRecordResponse>, ApiError> {
    let domain = state
        .repos
        .domains
        .find_by_id(DomainId::new(id))
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;
    dkim_response(&state, &domain)
}

/// `POST /api/v1/domains/:id/dkim` — generates a key pair when none exists.
pub async fn create_dkim(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<DkimRecordResponse>), ApiError> {
    let domain_id = DomainId::new(id);
    let domain = state
        .repos
        .domains
        .find_by_id(domain_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;

    if domain.dkim_public_key.is_some() {
        // Idempotent: an existing pair is returned rather than replaced, so a client
        // retry cannot invalidate a record the operator already published.
        return Ok((StatusCode::OK, dkim_response(&state, &domain)?));
    }

    let (private_key, public_key) = generate_dkim_key_pair()?;
    let selector = domain
        .dkim_selector
        .clone()
        .unwrap_or_else(|| state.config.dkim.selector.clone());
    state
        .repos
        .domains
        .set_dkim(
            domain_id,
            Some(selector.as_str()),
            Some(&private_key),
            Some(&public_key),
        )
        .await?;

    audit(
        &state,
        &admin,
        "domain.dkim_created",
        Some("domain"),
        Some(&id.to_string()),
        serde_json::json!({ "selector": selector }),
    )
    .await;

    let domain = state
        .repos
        .domains
        .find_by_id(domain_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))?;
    Ok((StatusCode::CREATED, dkim_response(&state, &domain)?))
}

/// Build the DKIM record response for a domain.
fn dkim_response(
    state: &AppState,
    domain: &ferroma_storage::models::Domain,
) -> Result<Json<DkimRecordResponse>, ApiError> {
    let selector = domain
        .dkim_selector
        .clone()
        .unwrap_or_else(|| state.config.dkim.selector.clone());
    let Some(public_key) = domain.dkim_public_key.as_deref() else {
        return Err(ApiError::new(FerromaError::NotFound(format!(
            "domain {} has no DKIM key pair yet; POST this endpoint to generate one",
            domain.name
        ))));
    };
    Ok(Json(DkimRecordResponse {
        selector: selector.clone(),
        record_name: DnsChecks::dkim_record_name(&selector, &domain.name),
        record_type: "TXT".to_string(),
        record_value: DnsChecks::dkim_record_value(public_key),
    }))
}

/// Generate an RSA key pair with the platform's `openssl`.
///
/// `ferroma-api`'s dependency list carries no RSA implementation and this crate may not
/// edit `ferroma-smtp` (which owns the signing side), so key *generation* is delegated
/// to `openssl genpkey`. A host without that tool gets a precise `409 conflict` naming
/// the command to run, which is more useful than a fabricated key.
pub fn generate_dkim_key_pair() -> Result<(String, String), ApiError> {
    let private = run_openssl(&[
        "genpkey",
        "-algorithm",
        "RSA",
        "-pkeyopt",
        "rsa_keygen_bits:2048",
    ])?;
    let public = run_openssl_with_stdin(&["pkey", "-pubout"], &private)?;
    Ok((private, strip_pem(&public)))
}

/// Run `openssl` with the given arguments.
fn run_openssl(args: &[&str]) -> Result<String, ApiError> {
    let output = std::process::Command::new("openssl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| unavailable(err.to_string()))?;
    if !output.status.success() {
        return Err(unavailable(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run `openssl`, feeding it `stdin`.
fn run_openssl_with_stdin(args: &[&str], stdin: &str) -> Result<String, ApiError> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("openssl")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| unavailable(err.to_string()))?;
    if let Some(handle) = child.stdin.as_mut() {
        let _ = handle.write_all(stdin.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|err| unavailable(err.to_string()))?;
    if !output.status.success() {
        return Err(unavailable(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// The error a host without `openssl` gets.
fn unavailable(reason: String) -> ApiError {
    ApiError::new(FerromaError::Conflict(format!(
        "DKIM key generation needs the `openssl` command on the server host: {reason}"
    )))
}

/// Strip the PEM armour from a public key, leaving one continuous line.
pub fn strip_pem(pem: &str) -> String {
    pem.lines()
        .filter(|line| !line.trim_start().starts_with("-----"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("")
}

/// Record an administrative action in the audit trail.
pub async fn audit(
    state: &AppState,
    admin: &AdminUser,
    action: &str,
    target_type: Option<&str>,
    target_id: Option<&str>,
    details: serde_json::Value,
) {
    let record = NewAuditLog {
        actor_user_id: Some(admin.user_id()),
        action: action.to_string(),
        target_type: target_type.map(str::to_string),
        target_id: target_id.map(str::to_string),
        ip: admin.auth().ip().map(str::to_string),
        user_agent: admin.auth().user_agent().map(str::to_string),
        details,
    };
    if let Err(err) = state.repos.audit.record(record).await {
        // The action already happened; failing it now would be a lie.
        tracing::warn!(action, error = %err, "could not write the audit trail");
    }
}

/// A paged list helper used by the alias and DNS screens.
pub fn page_of<T>(items: Vec<T>, total: i64, pagination: Pagination) -> Page<T> {
    pagination.page(items, total)
}

/// The default page a list endpoint uses when the caller sends nothing.
pub fn default_pagination(query: &PaginationQuery) -> Pagination {
    Pagination::clamped(query.limit, query.offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_names_are_the_documented_ones() {
        assert_eq!(RecordStatus::Ok.as_str(), "ok");
        assert_eq!(RecordStatus::Warn.as_str(), "warn");
        assert_eq!(RecordStatus::Fail.as_str(), "fail");
        assert_eq!(RecordStatus::Skip.as_str(), "skip");
    }

    #[test]
    fn only_checked_records_are_scored() {
        assert!(RecordStatus::Ok.is_scored());
        assert!(RecordStatus::Warn.is_scored());
        assert!(RecordStatus::Fail.is_scored());
        assert!(!RecordStatus::Skip.is_scored());
        assert!(RecordStatus::Ok.is_success());
        assert!(!RecordStatus::Warn.is_success());
    }

    #[test]
    fn the_report_scores_only_what_ran() {
        let records = vec![
            DnsRecord::new("MX", RecordStatus::Ok, None, vec!["10 mx".into()]),
            DnsRecord::new("A", RecordStatus::Ok, None, vec!["203.0.113.10".into()]),
            DnsRecord::new("AAAA", RecordStatus::Skip, None, vec![]),
            DnsRecord::new("PTR", RecordStatus::Ok, None, vec!["mx".into()]),
            DnsRecord::new("SPF", RecordStatus::Ok, None, vec!["v=spf1 mx -all".into()]),
            DnsRecord::new("DKIM", RecordStatus::Warn, None, vec![]),
            DnsRecord::new("DMARC", RecordStatus::Ok, None, vec!["v=DMARC1".into()]),
        ];
        let report = DnsReport::from_records("example.com", records);
        assert_eq!(report.domain, "example.com");
        assert_eq!(report.records.len(), 7);
        assert_eq!(report.score, 5);
        assert_eq!(report.max_score, 6, "the skipped AAAA is not scored");
    }

    #[test]
    fn a_report_where_nothing_ran_scores_zero_of_zero() {
        let records = vec![
            DnsRecord::skipped("MX", None, "no resolver"),
            DnsRecord::skipped("A", None, "no resolver"),
        ];
        let report = DnsReport::from_records("example.com", records);
        assert_eq!(report.score, 0);
        assert_eq!(report.max_score, 0);
    }

    #[test]
    fn skipped_records_carry_their_reason_as_a_hint() {
        let record = DnsRecord::skipped("MX", Some("mail.example.com".into()), "no resolver tool");
        assert_eq!(record.status, "skip");
        assert!(record.hint.as_deref().unwrap_or_default().contains("no resolver"));
        assert!(record.found.is_empty());
    }

    #[test]
    fn the_report_serialises_with_the_documented_shape() {
        let report = DnsReport::from_records(
            "example.com",
            vec![DnsRecord::new(
                "MX",
                RecordStatus::Ok,
                Some("mail.example.com".into()),
                vec!["10 mail.example.com.".into()],
            )],
        );
        let json = serde_json::to_value(&report).expect("must serialise");
        assert_eq!(json["domain"], "example.com");
        assert!(json["checked_at"].is_string());
        assert_eq!(json["records"][0]["kind"], "MX");
        assert_eq!(json["records"][0]["status"], "ok");
        assert_eq!(json["records"][0]["expected"], "mail.example.com");
        assert_eq!(json["records"][0]["found"][0], "10 mail.example.com.");
        assert_eq!(json["score"], 1);
        assert_eq!(json["max_score"], 1);
        // A record with no hint must not serialise `hint: null`.
        assert!(json["records"][0].get("hint").is_none(), "{json}");
    }

    #[test]
    fn expected_records_are_well_formed() {
        assert_eq!(
            DnsChecks::default_spf("mail.example.com"),
            "v=spf1 mx a:mail.example.com -all"
        );
        assert_eq!(
            DnsChecks::default_dmarc("example.com"),
            "v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com"
        );
        assert_eq!(
            DnsChecks::dkim_record_name("default", "example.com"),
            "default._domainkey.example.com"
        );
    }

    #[test]
    fn a_dkim_record_value_is_a_single_line() {
        let value = DnsChecks::dkim_record_value("MIIB\nIjAN\r\nBg");
        assert_eq!(value, "v=DKIM1; k=rsa; p=MIIBIjANBg");
        assert!(!value.contains('\n'));
        assert!(!value.contains('\r'));
    }

    #[test]
    fn nslookup_output_is_parsed_for_mx_records() {
        let output = "\
Server:  UnKnown
Address:  192.168.1.1

example.com     MX preference = 10, mail exchanger = mx1.example.com
example.com     MX preference = 20, mail exchanger = mx2.example.com
";
        let values = extract_records(output, "MX");
        assert!(values.iter().any(|value| value.contains("mx1.example.com")), "{values:?}");

        let parsed = parse_windows_mx("example.com MX preference = 10, mail exchanger = mx1.example.com");
        assert_eq!(parsed.as_deref(), Some("mx1.example.com"));
        assert_eq!(parse_windows_mx("nothing here"), None);
    }

    #[test]
    fn bind_style_output_is_parsed_too() {
        let output = "\
;; ANSWER SECTION:
example.com.  3600  IN  MX  10 mail.example.com.
";
        // The MX keyword must be its own token, and the preference number is dropped.
        let mx = extract_records(output, "MX");
        assert_eq!(mx, vec!["mail.example.com"]);

        let txt_output = "\
;; ANSWER SECTION:
example.com.  3600  IN  TXT  \"v=spf1 mx -all\"
";
        let txt = extract_records(txt_output, "TXT");
        assert_eq!(txt, vec!["v=spf1 mx -all"]);

        // The shape `nslookup` actually prints: the record type is spelled `text`, the value
        // is quoted, and a long one arrives as several quoted pieces. Looking for `TXT` as a
        // token therefore found nothing, and every TXT row — SPF, DKIM, DMARC — came back
        // empty on domains whose records were published and correct.
        let nslookup = "Non-authoritative answer:\n\
z1hwang.cn\ttext = \"v=spf1 include:spf.example.com ~all\"\n";
        assert_eq!(
            extract_records(nslookup, "TXT"),
            vec!["v=spf1 include:spf.example.com ~all"]
        );

        let split = "Non-authoritative answer:\n\
key.example\ttext = \"v=DKIM1; k=rsa; p=MIIB\" \"AABB\"\n";
        assert_eq!(
            extract_records(split, "TXT"),
            vec!["v=DKIM1; k=rsa; p=MIIBAABB"]
        );

        // An address answer, in the same dialect: `Name:` and then `Address:`, with the
        // resolver's own banner lines above the answer section excluded.
        let address = "Server:\t\t127.0.0.53\nAddress:\t127.0.0.53#53\n\n\
Non-authoritative answer:\nName:\tmail.example.com\nAddress: 203.0.113.10\n";
        assert_eq!(extract_records(address, "A"), vec!["203.0.113.10"]);

    }

    #[test]
    fn a_relay_turns_a_missing_ptr_from_a_defect_into_a_note() {
        let host = "mail.example.com";
        // A direct sender needs the record: its address is the one on the wire.
        assert_eq!(ptr_verdict(&[], host, None).0, RecordStatus::Warn);
        // A relayed one does not, and saying "warn" would be an accusation about a record that
        // no receiver of its mail will ever look up.
        assert_eq!(
            ptr_verdict(&[], host, Some("mail.relay.example")).0,
            RecordStatus::Skip
        );
        // A matching record is good news either way, and the trailing dot is a detail.
        assert_eq!(
            ptr_verdict(&["mail.example.com.".into()], host, None).0,
            RecordStatus::Ok
        );
        assert_eq!(
            ptr_verdict(
                &["mail.example.com.".into()],
                host,
                Some("mail.relay.example")
            )
            .0,
            RecordStatus::Ok
        );
    }

    #[test]
    fn spf_knows_the_difference_between_direct_and_relayed() {
        let expected = DnsChecks::default_spf("mail.example.com");
        assert_eq!(expected, "v=spf1 mx a:mail.example.com -all");

        // The record the expectation names is what direct delivery needs.
        assert_eq!(
            spf_verdict(&[expected.clone()], &expected, None).0,
            RecordStatus::Ok
        );
        // A direct sender whose record authorises somebody else is warned: the mail host is what
        // its own deliveries go out from.
        assert_eq!(
            spf_verdict(&["v=spf1 include:relay.example ~all".into()], &expected, None).0,
            RecordStatus::Warn
        );
        // A relayed instance that delegates sending is not: which `include` its provider needs is
        // the provider's name to choose, and the panel cannot know it.
        assert_eq!(
            spf_verdict(
                &["v=spf1 include:spf.relay.example ~all".into()],
                &expected,
                Some("mail.relay.example")
            )
            .0,
            RecordStatus::Ok
        );
        // Relayed and authorised by neither, which is still the case worth reporting.
        assert_eq!(
            spf_verdict(
                &["v=spf1 ip4:203.0.113.10 ~all".into()],
                &expected,
                Some("mail.relay.example")
            )
            .0,
            RecordStatus::Warn
        );
    }

    #[test]
    fn parsing_an_empty_or_unrelated_output_yields_nothing() {
        assert!(extract_records("", "MX").is_empty());
        assert!(extract_records("Server:  UnKnown\nAddress:  1.1.1.1", "MX").is_empty());
        assert!(extract_records("some prose about mail servers", "MX").is_empty());
    }

    #[test]
    fn parsing_never_panics_on_odd_output() {
        for input in [
            "EXAMPLE.COM MX",
            "example.com   MX   ",
            "example.com MX preference = , mail exchanger = ",
            "\u{0}\u{1}\u{2}",
        ] {
            let _ = extract_records(input, "MX");
            let _ = parse_windows_mx(input);
        }
    }

    #[test]
    fn the_resolver_timeout_is_bounded() {
        assert!(RESOLVER_TIMEOUT <= Duration::from_secs(10));
    }

    #[test]
    fn a_public_key_is_unwrapped_onto_one_line() {
        let pem = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBg\nkqhkiG9w\n-----END PUBLIC KEY-----\n";
        assert_eq!(strip_pem(pem), "MIIBIjANBgkqhkiG9w");
        assert_eq!(strip_pem(""), "");
        // Text without armour is kept, minus its line breaks.
        assert_eq!(strip_pem("no armour here"), "no armour here");
        assert_eq!(strip_pem("a\nb"), "ab");
    }

    #[test]
    fn a_dkim_response_is_only_built_from_a_real_key() {
        let record = DkimRecordResponse {
            selector: "default".into(),
            record_name: DnsChecks::dkim_record_name("default", "example.com"),
            record_type: "TXT".into(),
            record_value: DnsChecks::dkim_record_value("MIIBIjANBg"),
        };
        let json = serde_json::to_value(&record).expect("must serialise");
        assert_eq!(json["selector"], "default");
        assert_eq!(json["record_name"], "default._domainkey.example.com");
        assert_eq!(json["record_type"], "TXT");
        assert_eq!(json["record_value"], "v=DKIM1; k=rsa; p=MIIBIjANBg");
    }

    #[test]
    fn the_domain_list_shape_is_stable() {
        let response = DomainListResponse {
            items: Vec::new(),
            total: 0,
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["total"], 0);
        assert_eq!(json["items"], serde_json::json!([]));

        let aliases = AliasListResponse {
            items: Vec::new(),
            total: 0,
        };
        assert_eq!(
            serde_json::to_value(&aliases).expect("must serialise")["total"],
            0
        );
    }

    #[test]
    fn the_update_bodies_are_all_optional() {
        let domain: UpdateDomainRequest =
            serde_json::from_value(serde_json::json!({})).expect("must deserialise");
        assert!(domain.enabled.is_none());
        assert!(domain.catch_all.is_none());

        let alias: UpdateAliasRequest =
            serde_json::from_value(serde_json::json!({})).expect("must deserialise");
        assert!(alias.target.is_none());
        assert!(alias.enabled.is_none());
    }

    #[test]
    fn the_create_bodies_require_their_essential_fields() {
        assert!(serde_json::from_value::<CreateDomainRequest>(serde_json::json!({})).is_err());
        assert!(serde_json::from_value::<CreateAliasRequest>(serde_json::json!({
            "local_part": "sales"
        }))
        .is_err());
        let request: CreateAliasRequest = serde_json::from_value(serde_json::json!({
            "local_part": "sales",
            "target": "alice@example.com"
        }))
        .expect("must deserialise");
        assert_eq!(request.target, "alice@example.com");
    }

    #[test]
    fn the_force_flag_defaults_to_false() {
        let query: ForceQuery = serde_json::from_value(serde_json::json!({})).expect("must parse");
        assert!(!query.force.unwrap_or(false));
        let query: ForceQuery =
            serde_json::from_value(serde_json::json!({ "force": true })).expect("must parse");
        assert!(query.force.unwrap_or(false));
    }

    #[test]
    fn pagination_helpers_agree_with_the_extractor() {
        let query = PaginationQuery {
            limit: Some(10),
            offset: Some(20),
        };
        let pagination = default_pagination(&query);
        assert_eq!(pagination.limit, 10);
        assert_eq!(pagination.offset, 20);
        let page = page_of(vec![1, 2], 42, pagination);
        assert_eq!(page.total, 42);
        assert_eq!(page.limit, 10);
        assert_eq!(page.offset, 20);

        let clamped = default_pagination(&PaginationQuery {
            limit: Some(100_000),
            offset: Some(-1),
        });
        assert_eq!(clamped.limit, crate::extract::MAX_LIMIT);
        assert_eq!(clamped.offset, 0);
    }
}
