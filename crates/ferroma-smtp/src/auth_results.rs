//! `Authentication-Results` (RFC 8601): the one header that reports what the
//! receiver's own authentication checks concluded.
//!
//! ```text
//! Authentication-Results: mail.example.com;
//!  spf=pass smtp.mailfrom=example.com smtp.helo=client.example.net;
//!  dkim=pass header.d=example.com;
//!  dmarc=pass header.from=example.com policy.dmarc=reject
//! ```
//!
//! The header is a *statement by this host about this host's own checks*, which is
//! why the authentication service identifier comes first and why nothing a peer sent
//! is ever copied into it verbatim: the values are the verdicts SPF, DKIM and DMARC
//! produced, plus the domains they were produced for.
//!
//! The shape follows `docs/security.md` §8 and RFC 8601 §2.2:
//! `authserv-id [version] 1*( ";" method "=" result [ reason ] *propspec )`, folded
//! at 78 columns.
//!
//! [`AuthResults::render`] and [`AuthResults::parse`] are inverses, and a test asserts
//! it: a header this module writes can be read back by this module unchanged, which is
//! what lets an operator paste one into a bug report and have it understood.

use ferroma_core::{FerromaError, Result};

use crate::dkim::{DkimSignature, DkimVerdict};
use crate::dmarc::{DmarcPolicy, DmarcVerdict};
use crate::spf::SpfOutcome;

/// The column a folded line must not exceed, the CRLF excluded (RFC 5322 §2.1.1).
const MAX_LINE_LENGTH: usize = 78;

/// `"Authentication-Results: "` — the column the value starts in.
const HEADER_PREFIX: &str = "Authentication-Results: ";

/// One method's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthResult {
    /// The method: `spf`, `dkim`, `dmarc`, `iprev`, `auth`…
    pub method: String,
    /// The result token for that method: `pass`, `fail`, `temperror`…
    pub result: String,
    /// The optional human-readable `reason="…"` clause.
    pub reason: Option<String>,
    /// The `ptype.property=value` clauses, in order.
    pub properties: Vec<(String, String)>,
}

impl AuthResult {
    /// A verdict with no reason and no properties.
    pub fn new(method: impl Into<String>, result: impl Into<String>) -> Self {
        AuthResult {
            method: method.into(),
            result: result.into(),
            reason: None,
            properties: Vec::new(),
        }
    }

    /// Add a `ptype.property=value` clause.
    pub fn with_property(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.properties.push((k.into(), v.into()));
        self
    }

    /// Set the `reason="…"` clause, replacing any previous one.
    pub fn with_reason(mut self, r: impl Into<String>) -> Self {
        self.reason = Some(r.into());
        self
    }

    /// The first property with this name, if any.
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The methodspec: `method=result [reason="…"] [k=v …]`.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(32);
        out.push_str(&self.method);
        out.push('=');
        out.push_str(&self.result);
        if let Some(reason) = &self.reason {
            out.push_str(" reason=");
            out.push_str(&quote(reason));
        }
        for (key, value) in &self.properties {
            out.push(' ');
            out.push_str(key);
            out.push('=');
            out.push_str(&render_value(value));
        }
        out
    }
}

/// The `Authentication-Results` header: one authentication service, several verdicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthResults {
    /// The authentication service identifier — this host's name.
    pub authserv_id: String,
    /// The authres version. Version 1 is not written out, as RFC 8601 §2.1 allows.
    pub version: u32,
    /// The verdicts, in the order they should be reported.
    pub results: Vec<AuthResult>,
}

impl AuthResults {
    /// The name of the header this type renders.
    pub fn header_name() -> &'static str {
        "Authentication-Results"
    }

    /// An empty header block attributed to `authserv_id`.
    pub fn new(authserv_id: impl Into<String>) -> Self {
        AuthResults {
            authserv_id: authserv_id.into(),
            version: 1,
            results: Vec::new(),
        }
    }

    /// Append a verdict.
    pub fn add(&mut self, result: AuthResult) {
        self.results.push(result);
    }

    /// The first verdict for `method`.
    pub fn get(&self, method: &str) -> Option<&AuthResult> {
        self.results
            .iter()
            .find(|r| r.method.eq_ignore_ascii_case(method))
    }

    /// Whether any verdict was recorded.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    /// The header **value**: `authserv-id [version] ; method=… …`, folded, no name,
    /// no CRLF.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(128);
        let mut line_length = HEADER_PREFIX.len() + self.authserv_id.len();
        out.push_str(&self.authserv_id);
        if self.version != 1 {
            let version = self.version.to_string();
            out.push(' ');
            out.push_str(&version);
            line_length += 1 + version.len();
        }
        for result in &self.results {
            let spec = result.render();
            if line_length + 2 + spec.len() > MAX_LINE_LENGTH {
                out.push_str(";\r\n ");
                line_length = 1;
            } else {
                out.push_str("; ");
                line_length += 2;
            }
            line_length = push_folded(&mut out, &spec, line_length);
        }
        out
    }

    /// The whole header line, terminated with CRLF.
    pub fn to_header(&self) -> String {
        format!("{}: {}\r\n", Self::header_name(), self.render())
    }

    /// Prepend the header to a raw message, unless it already carries ours.
    ///
    /// Re-authenticating a message that already passed through this host would add a
    /// second, contradictory statement; RFC 8601 §5 warns against exactly that, so a
    /// message with an `Authentication-Results` header from this service is returned
    /// untouched. A header from *another* service is left alone and ours is added.
    pub fn prepend_to(&self, message: &[u8]) -> Vec<u8> {
        if self.already_applied(message) {
            return message.to_vec();
        }
        let header = self.to_header();
        let mut out = Vec::with_capacity(header.len() + message.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(message);
        out
    }

    /// Whether the message already carries a verdict from this service.
    fn already_applied(&self, message: &[u8]) -> bool {
        let block = header_block(message);
        let Ok(headers) = ferroma_mail::Headers::parse(&String::from_utf8_lossy(block)) else {
            return false;
        };
        headers.get_all(Self::header_name()).iter().any(|value| {
            value
                .split_whitespace()
                .next()
                .map(|id| id.trim_end_matches(';').eq_ignore_ascii_case(&self.authserv_id))
                .unwrap_or(false)
        })
    }

    /// Parse a header value (or a whole header line) back into its parts.
    ///
    /// Folding is undone first, so a value read straight out of a message works. A
    /// bare `none` clause — RFC 8601's way of saying "I checked nothing" — parses to
    /// no verdicts rather than an error.
    pub fn parse(raw: &str) -> Result<Self> {
        let unfolded = unfold(raw);
        // Tolerate a caller that handed over the whole header line.
        let body = match unfolded.split_once(':') {
            Some((name, rest)) if name.trim().eq_ignore_ascii_case(Self::header_name()) => rest,
            _ => unfolded.as_str(),
        };

        let mut clauses = body.split(';');
        let first = clauses.next().unwrap_or("").trim();
        let mut words = first.split_whitespace();
        let authserv_id = words
            .next()
            .ok_or_else(|| FerromaError::Parse("the header has no authentication service id".into()))?
            .to_string();
        let version = words
            .next()
            .and_then(|word| word.parse::<u32>().ok())
            .unwrap_or(1);

        let mut results = Vec::new();
        for clause in clauses {
            let clause = clause.trim();
            if clause.is_empty() || clause.eq_ignore_ascii_case("none") {
                continue;
            }
            results.push(parse_method_spec(clause)?);
        }

        Ok(AuthResults {
            authserv_id,
            version,
            results,
        })
    }

    /// Build the standard SPF + DKIM + DMARC header for one message.
    ///
    /// This is the wrapper the SMTP path uses: it takes whatever was actually
    /// evaluated and reports exactly that, in the order the results arrived.
    pub fn from_checks(
        authserv_id: &str,
        spf: Option<&SpfOutcome>,
        mail_from: Option<&str>,
        helo: &str,
        dkim: &[DkimVerdict],
        dmarc: Option<&DmarcVerdict>,
        from_domain: &str,
    ) -> AuthResults {
        let mut builder = AuthResultsBuilder::new(authserv_id);
        if let Some(outcome) = spf {
            builder = builder.spf(outcome, mail_from, helo);
        }
        for verdict in dkim {
            builder = builder.dkim(verdict);
        }
        if let Some(verdict) = dmarc {
            builder = builder.dmarc(verdict, from_domain);
        }
        builder.build()
    }
}

/// Assembles the header from the checks that ran.
#[derive(Debug, Clone)]
pub struct AuthResultsBuilder {
    results: AuthResults,
}

impl AuthResultsBuilder {
    /// A builder for verdicts attributed to `authserv_id`.
    pub fn new(authserv_id: impl Into<String>) -> Self {
        AuthResultsBuilder {
            results: AuthResults::new(authserv_id),
        }
    }

    /// Record an SPF verdict.
    ///
    /// `mail_from` is the envelope sender; the property carries its *domain*, which is
    /// what SPF actually authenticates. The null reverse-path is reported as `<>`,
    /// the way it is spelled on the wire.
    pub fn spf(mut self, outcome: &SpfOutcome, mail_from: Option<&str>, helo: &str) -> Self {
        let mut result = AuthResult::new("spf", outcome.result.as_str())
            .with_property("smtp.mailfrom", mail_from_domain(mail_from));
        let helo = helo.trim();
        if !helo.is_empty() {
            result = result.with_property("smtp.helo", helo);
        }
        if let Some(explanation) = &outcome.explanation {
            result = result.with_reason(explanation.clone());
        }
        self.results.add(result);
        self
    }

    /// Record a DKIM verdict.
    ///
    /// Without the parsed signature there is no `i=` identity and no `b=` value to
    /// report, so only `header.d` is emitted; use
    /// [`AuthResultsBuilder::dkim_with_signature`] when the signature is at hand.
    pub fn dkim(self, verdict: &DkimVerdict) -> Self {
        self.dkim_with_signature(verdict, None)
    }

    /// Record a DKIM verdict, with the signature it came from.
    ///
    /// `header.b` carries the first eight characters of the signature, which is what
    /// RFC 8601 §2.7.3 registers: enough to identify the signature in a log without
    /// repeating it in full.
    pub fn dkim_with_signature(
        mut self,
        verdict: &DkimVerdict,
        signature: Option<&DkimSignature>,
    ) -> Self {
        let mut result = AuthResult::new("dkim", verdict.result.as_str());
        if let Some(domain) = verdict.domain.as_deref() {
            result = result.with_property("header.d", domain);
        }
        if let Some(signature) = signature {
            if let Some(identity) = signature.identity() {
                result = result.with_property("header.i", identity.trim());
            }
            let value = signature.signature();
            if !value.is_empty() {
                let prefix: String = value.chars().take(8).collect();
                result = result.with_property("header.b", prefix);
            }
        }
        if let Some(reason) = verdict.reason.as_deref() {
            result = result.with_reason(reason);
        }
        self.results.add(result);
        self
    }

    /// Record a DMARC verdict.
    pub fn dmarc(mut self, verdict: &DmarcVerdict, from_domain: &str) -> Self {
        let mut result = AuthResult::new("dmarc", verdict.result.as_str());
        let from = if from_domain.trim().is_empty() {
            verdict.domain.clone().unwrap_or_default()
        } else {
            from_domain.trim().to_string()
        };
        if !from.is_empty() {
            result = result.with_property("header.from", from);
        }
        if let Some(policy) = verdict.policy {
            result = result.with_property("policy.dmarc", policy.as_str());
        }
        if let Some(reason) = verdict.reason.as_deref() {
            result = result.with_reason(reason);
        }
        self.results.add(result);
        self
    }

    /// Record a policy chosen locally rather than published by the domain.
    pub fn policy(mut self, policy: DmarcPolicy) -> Self {
        self.results
            .add(AuthResult::new("dmarc", policy.as_str()).with_property("policy.dmarc", policy.as_str()));
        self
    }

    /// Finish and hand back the header.
    pub fn build(self) -> AuthResults {
        self.results
    }
}

/// The domain half of an envelope sender, or `<>` for the null reverse-path.
fn mail_from_domain(mail_from: Option<&str>) -> String {
    let Some(raw) = mail_from else {
        return "<>".to_string();
    };
    let trimmed = raw.trim().trim_start_matches('<').trim_end_matches('>');
    if trimmed.is_empty() {
        return "<>".to_string();
    }
    match ferroma_core::EmailAddress::parse(trimmed) {
        Ok(address) => address.domain().to_string(),
        Err(_) => trimmed.to_string(),
    }
}

/// Append `text`, folding at spaces when the line would overrun.
///
/// CFWS is allowed between property clauses, so a break at a space is always legal.
/// Returns the new column.
fn push_folded(out: &mut String, text: &str, mut line_length: usize) -> usize {
    for (index, word) in text.split(' ').enumerate() {
        let separator = usize::from(index > 0);
        if index > 0 && line_length + 1 + word.len() > MAX_LINE_LENGTH {
            out.push_str("\r\n ");
            line_length = 1;
        } else if index > 0 {
            out.push(' ');
            line_length += 1;
        }
        let _ = separator;
        out.push_str(word);
        line_length += word.len();
    }
    line_length
}

/// Whether a value has to be written as a quoted string.
///
/// RFC 8601 §2.2 allows `token` or `quoted-string`. Only the characters that would
/// break the surrounding grammar force quoting — whitespace, the quote itself, the
/// clause separators — so a mailbox keeps the unquoted spelling every mail system
/// writes.
fn needs_quoting(value: &str) -> bool {
    value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | ';' | '=' | '\\'))
}

/// Render a property value, quoting it only when the grammar demands it.
fn render_value(value: &str) -> String {
    if needs_quoting(value) {
        quote(value)
    } else {
        value.to_string()
    }
}

/// Render a quoted string, escaping the quote and the backslash.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Remove every CR and LF, i.e. RFC 5322 unfolding.
fn unfold(raw: &str) -> String {
    raw.chars().filter(|c| *c != '\r' && *c != '\n').collect()
}

/// The header block of a raw message, terminator included.
fn header_block(raw: &[u8]) -> &[u8] {
    let mut index = 0usize;
    while index < raw.len() {
        if raw[index] == b'\n' {
            let rest = &raw[index + 1..];
            if rest.first() == Some(&b'\n') {
                return &raw[..=index];
            }
            if rest.first() == Some(&b'\r') && rest.get(1) == Some(&b'\n') {
                return &raw[..=index];
            }
        }
        index += 1;
    }
    raw
}

/// Split a methodspec body into whitespace-separated tokens, keeping quoted strings
/// together.
fn split_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Undo [`quote`], if the value is quoted at all.
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(inner) = trimmed.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return trimmed.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for c in inner.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse one `method=result …` clause.
fn parse_method_spec(clause: &str) -> Result<AuthResult> {
    let eq = clause.find('=').ok_or_else(|| {
        FerromaError::Parse(format!("the Authentication-Results clause {clause:?} has no result"))
    })?;
    let method = clause[..eq].trim().to_string();
    if method.is_empty() {
        return Err(FerromaError::Parse(
            "an Authentication-Results clause has no method".to_string(),
        ));
    }

    let mut tokens = split_tokens(clause[eq + 1..].trim()).into_iter();
    let result = tokens.next().ok_or_else(|| {
        FerromaError::Parse(format!("the {method} clause has no result token"))
    })?;
    if result.contains('=') {
        return Err(FerromaError::Parse(format!(
            "the {method} clause has no result token"
        )));
    }

    let mut parsed = AuthResult::new(method, result);
    for token in tokens {
        let Some((key, value)) = token.split_once('=') else {
            return Err(FerromaError::Parse(format!(
                "the Authentication-Results property {token:?} has no value"
            )));
        };
        let value = unquote(value);
        if key.eq_ignore_ascii_case("reason") {
            parsed.reason = Some(value);
        } else {
            parsed.properties.push((key.to_string(), value));
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dkim::DkimResult;
    use crate::dmarc::DmarcResult;
    use crate::spf::SpfResult;

    fn spf_outcome(result: SpfResult, domain: &str) -> SpfOutcome {
        SpfOutcome::new(result, Some(domain.to_string()), 1)
    }

    fn dkim_verdict(result: DkimResult, domain: &str) -> DkimVerdict {
        DkimVerdict::new(
            result,
            Some(domain.to_string()),
            Some("default".to_string()),
            "checked",
        )
    }

    fn dmarc_verdict(result: DmarcResult, policy: Option<DmarcPolicy>) -> DmarcVerdict {
        DmarcVerdict {
            result,
            policy,
            domain: Some("example.com".to_string()),
            spf_aligned: result == DmarcResult::Pass,
            dkim_aligned: false,
            reason: None,
        }
    }

    /// A three-verdict header, the shape the SMTP path builds.
    fn full_header() -> AuthResults {
        let spf = spf_outcome(SpfResult::Pass, "example.com");
        let dkim = dkim_verdict(DkimResult::Pass, "example.com");
        let dmarc = dmarc_verdict(DmarcResult::Pass, Some(DmarcPolicy::Reject));
        AuthResults::from_checks(
            "mail.example.com",
            Some(&spf),
            Some("joe@example.com"),
            "client.example.net",
            &[dkim],
            Some(&dmarc),
            "example.com",
        )
    }

    // ------------------------------------------------------------------
    // Building
    // ------------------------------------------------------------------

    #[test]
    fn a_new_header_is_empty_and_attributed_to_its_service() {
        let results = AuthResults::new("mail.example.com");
        assert_eq!(results.authserv_id, "mail.example.com");
        assert_eq!(results.version, 1);
        assert!(results.is_empty());
        assert_eq!(results.render(), "mail.example.com");
    }

    #[test]
    fn header_name_is_the_rfc_8601_name() {
        assert_eq!(AuthResults::header_name(), "Authentication-Results");
    }

    #[test]
    fn adding_a_result_appends_it() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(AuthResult::new("spf", "pass"));
        results.add(AuthResult::new("dkim", "fail"));
        assert_eq!(results.results.len(), 2);
        assert!(!results.is_empty());
        assert_eq!(results.get("SPF").map(|r| r.result.as_str()), Some("pass"));
        assert_eq!(results.get("dmarc"), None);
    }

    #[test]
    fn a_result_carries_reasons_and_properties() {
        let result = AuthResult::new("spf", "fail")
            .with_property("smtp.mailfrom", "example.com")
            .with_reason("not authorised");
        assert_eq!(result.property("smtp.mailfrom"), Some("example.com"));
        assert_eq!(result.property("SMTP.MAILFROM"), Some("example.com"));
        assert_eq!(result.property("smtp.helo"), None);
        assert_eq!(result.reason.as_deref(), Some("not authorised"));
        assert_eq!(
            result.render(),
            "spf=fail reason=\"not authorised\" smtp.mailfrom=example.com"
        );
    }

    #[test]
    fn a_result_renders_without_a_reason() {
        assert_eq!(AuthResult::new("dkim", "none").render(), "dkim=none");
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    #[test]
    fn the_full_header_has_the_documented_shape() {
        let rendered = full_header().render();
        let unfolded = unfold(&rendered);
        assert_eq!(
            unfolded,
            "mail.example.com; spf=pass smtp.mailfrom=example.com smtp.helo=client.example.net; \
             dkim=pass reason=\"checked\" header.d=example.com; \
             dmarc=pass header.from=example.com policy.dmarc=reject"
        );
        assert!(rendered.starts_with("mail.example.com;"));
    }

    #[test]
    fn the_header_line_carries_the_name_and_a_crlf() {
        let header = full_header().to_header();
        assert!(header.starts_with("Authentication-Results: mail.example.com;"));
        assert!(header.ends_with("\r\n"));
    }

    #[test]
    fn rendering_folds_at_seventy_eight_columns() {
        let rendered = full_header().render();
        assert!(rendered.contains("\r\n "), "{rendered}");
        for line in format!("{HEADER_PREFIX}{rendered}").split("\r\n") {
            assert!(line.len() <= MAX_LINE_LENGTH, "{} columns: {line}", line.len());
        }
    }

    #[test]
    fn folding_breaks_only_at_clause_or_property_boundaries() {
        let rendered = full_header().render();
        // Every continuation line starts with WSP, which is what makes the fold legal.
        for line in rendered.split("\r\n").skip(1) {
            assert!(line.starts_with(' '), "{line:?}");
        }
        // And no fold has swallowed a separator.
        let unfolded = unfold(&rendered);
        assert!(unfolded.contains("; dkim=pass"));
        assert!(unfolded.contains("dmarc=pass header.from=example.com"));
    }

    #[test]
    fn a_value_with_a_space_is_quoted() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(AuthResult::new("spf", "fail").with_property("smtp.helo", "a b"));
        assert_eq!(
            results.render(),
            "mail.example.com; spf=fail smtp.helo=\"a b\""
        );
    }

    #[test]
    fn a_value_with_a_quote_or_backslash_is_escaped() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(AuthResult::new("spf", "fail").with_property("smtp.helo", "a\"b\\c"));
        assert_eq!(
            results.render(),
            "mail.example.com; spf=fail smtp.helo=\"a\\\"b\\\\c\""
        );
    }

    #[test]
    fn a_bare_value_is_not_quoted() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(AuthResult::new("spf", "pass").with_property("smtp.mailfrom", "example.com"));
        assert_eq!(
            results.render(),
            "mail.example.com; spf=pass smtp.mailfrom=example.com"
        );
    }

    #[test]
    fn the_null_reverse_path_is_reported_as_angle_brackets() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(
            AuthResult::new("spf", "none").with_property("smtp.mailfrom", mail_from_domain(None)),
        );
        assert_eq!(
            results.render(),
            "mail.example.com; spf=none smtp.mailfrom=<>"
        );
    }

    #[test]
    fn an_empty_value_is_quoted_rather_than_dropped() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(AuthResult::new("spf", "none").with_property("smtp.helo", ""));
        assert_eq!(results.render(), "mail.example.com; spf=none smtp.helo=\"\"");
    }

    #[test]
    fn a_non_default_version_is_written_out() {
        let mut results = AuthResults::new("mail.example.com");
        results.version = 2;
        results.add(AuthResult::new("spf", "pass"));
        assert_eq!(results.render(), "mail.example.com 2; spf=pass");
    }

    // ------------------------------------------------------------------
    // Parsing
    // ------------------------------------------------------------------

    #[test]
    fn a_rendered_header_re_parses_byte_for_byte() {
        let results = full_header();
        let parsed = AuthResults::parse(&results.render()).expect("parses");
        assert_eq!(parsed, results);
        assert_eq!(parsed.render(), results.render());
    }

    #[test]
    fn a_header_line_re_parses_when_the_name_is_included() {
        let results = full_header();
        let parsed = AuthResults::parse(results.to_header().trim_end()).expect("parses");
        assert_eq!(parsed, results);
    }

    #[test]
    fn a_folded_header_re_parses() {
        let results = full_header();
        let rendered = results.render();
        assert!(rendered.contains("\r\n"));
        let reparsed = AuthResults::parse(&rendered).expect("parses");
        assert_eq!(reparsed.authserv_id, "mail.example.com");
        assert_eq!(reparsed.results.len(), 3);
        assert_eq!(
            reparsed.get("spf").and_then(|r| r.property("smtp.mailfrom")),
            Some("example.com")
        );
    }

    #[test]
    fn a_quoted_reason_with_spaces_survives_the_round_trip() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(
            AuthResult::new("dkim", "fail")
                .with_reason("the body hash does not match the bh= tag")
                .with_property("header.d", "example.com"),
        );
        let parsed = AuthResults::parse(&results.render()).expect("parses");
        assert_eq!(parsed, results);
        assert_eq!(
            parsed.get("dkim").and_then(|r| r.reason.as_deref()),
            Some("the body hash does not match the bh= tag")
        );
    }

    #[test]
    fn a_property_value_containing_an_equals_sign_survives() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(AuthResult::new("auth", "pass").with_property("smtp.auth", "a=b"));
        let parsed = AuthResults::parse(&results.render()).expect("parses");
        assert_eq!(parsed, results);
        assert_eq!(
            parsed.get("auth").and_then(|r| r.property("smtp.auth")),
            Some("a=b")
        );
    }

    #[test]
    fn parsing_accepts_multiple_verdicts_for_one_method() {
        let mut results = AuthResults::new("mail.example.com");
        results.add(AuthResult::new("dkim", "fail").with_property("header.d", "one.example"));
        results.add(AuthResult::new("dkim", "pass").with_property("header.d", "two.example"));
        let parsed = AuthResults::parse(&results.render()).expect("parses");
        assert_eq!(parsed.results.len(), 2);
        assert_eq!(
            parsed.get("dkim").and_then(|r| r.property("header.d")),
            Some("one.example")
        );
    }

    #[test]
    fn parsing_rejects_an_empty_value() {
        assert!(AuthResults::parse("").is_err());
        assert!(AuthResults::parse("   ").is_err());
        assert!(AuthResults::parse("; spf=pass").is_err());
    }

    #[test]
    fn parsing_rejects_a_clause_without_a_result() {
        assert!(AuthResults::parse("mail.example.com; spf").is_err());
        assert!(AuthResults::parse("mail.example.com; =pass").is_err());
        assert!(AuthResults::parse("mail.example.com; spf=smtp.mailfrom=x").is_err());
        assert!(AuthResults::parse("mail.example.com; spf=pass broken").is_err());
    }

    #[test]
    fn a_bare_none_clause_parses_to_no_verdicts() {
        let parsed = AuthResults::parse("mail.example.com; none").expect("parses");
        assert_eq!(parsed.authserv_id, "mail.example.com");
        assert!(parsed.is_empty());
    }

    #[test]
    fn an_explicit_version_is_read() {
        let parsed = AuthResults::parse("mail.example.com 7; spf=pass").expect("parses");
        assert_eq!(parsed.version, 7);
        assert_eq!(parsed.render(), "mail.example.com 7; spf=pass");
    }

    #[test]
    fn an_unparsable_version_falls_back_to_one() {
        let parsed = AuthResults::parse("mail.example.com soon; spf=pass").expect("parses");
        assert_eq!(parsed.version, 1);
    }

    // ------------------------------------------------------------------
    // Prepending
    // ------------------------------------------------------------------

    #[test]
    fn the_header_is_prepended_to_a_message() {
        let message = b"From: joe@example.com\r\nSubject: hi\r\n\r\nbody\r\n".to_vec();
        let with_header = full_header().prepend_to(&message);
        let text = String::from_utf8_lossy(&with_header);
        assert!(text.starts_with("Authentication-Results: mail.example.com;"));
        assert!(with_header.ends_with(&message));
    }

    #[test]
    fn prepending_twice_leaves_one_header() {
        let message = b"From: joe@example.com\r\n\r\nbody\r\n".to_vec();
        let results = full_header();
        let once = results.prepend_to(&message);
        let twice = results.prepend_to(&once);
        assert_eq!(once, twice);
        let text = String::from_utf8_lossy(&twice);
        assert_eq!(text.matches("Authentication-Results:").count(), 1);
    }

    #[test]
    fn another_services_header_does_not_suppress_ours() {
        let message = b"Authentication-Results: other.example; spf=fail\r\nFrom: joe@example.com\r\n\r\nbody\r\n";
        let with_header = full_header().prepend_to(message);
        let text = String::from_utf8_lossy(&with_header);
        assert_eq!(text.matches("Authentication-Results:").count(), 2);
        assert!(text.starts_with("Authentication-Results: mail.example.com;"));
    }

    #[test]
    fn prepending_finds_our_header_anywhere_in_the_block() {
        let message = b"From: joe@example.com\r\nAuthentication-Results: mail.example.com;\r\n spf=pass\r\n\r\nbody\r\n";
        let with_header = full_header().prepend_to(message);
        assert_eq!(with_header, message.to_vec());
    }

    #[test]
    fn prepending_to_a_bare_message_without_headers_still_works() {
        let with_header = full_header().prepend_to(b"not a message at all");
        let text = String::from_utf8_lossy(&with_header);
        assert!(text.starts_with("Authentication-Results: "));
        assert!(text.ends_with("not a message at all"));
    }

    // ------------------------------------------------------------------
    // The builder
    // ------------------------------------------------------------------

    #[test]
    fn the_builder_reports_an_spf_pass() {
        let spf = spf_outcome(SpfResult::Pass, "example.com");
        let results = AuthResultsBuilder::new("mail.example.com")
            .spf(&spf, Some("joe@example.com"), "client.example.net")
            .build();
        assert_eq!(
            unfold(&results.render()),
            "mail.example.com; spf=pass smtp.mailfrom=example.com smtp.helo=client.example.net"
        );
    }

    #[test]
    fn the_builder_omits_an_empty_helo() {
        let spf = spf_outcome(SpfResult::Neutral, "example.com");
        let results = AuthResultsBuilder::new("mail.example.com")
            .spf(&spf, Some("joe@example.com"), "  ")
            .build();
        assert_eq!(
            unfold(&results.render()),
            "mail.example.com; spf=neutral smtp.mailfrom=example.com"
        );
    }

    #[test]
    fn the_builder_reports_the_null_sender() {
        let spf = SpfOutcome::new(SpfResult::None, None, 0);
        let results = AuthResultsBuilder::new("mail.example.com")
            .spf(&spf, None, "client.example.net")
            .build();
        assert!(results.render().contains("smtp.mailfrom=<>"));
    }

    #[test]
    fn the_builder_carries_an_spf_explanation_as_the_reason() {
        let mut spf = spf_outcome(SpfResult::Fail, "example.com");
        spf.explanation = Some("203.0.113.7 is not allowed to send mail".to_string());
        let results = AuthResultsBuilder::new("mail.example.com")
            .spf(&spf, Some("joe@example.com"), "client.example.net")
            .build();
        assert!(results
            .render()
            .contains("reason=\"203.0.113.7 is not allowed to send mail\""));
    }

    #[test]
    fn the_builder_reports_a_dkim_verdict() {
        let dkim = dkim_verdict(DkimResult::Pass, "example.com");
        let results = AuthResultsBuilder::new("mail.example.com")
            .dkim(&dkim)
            .build();
        assert_eq!(
            unfold(&results.render()),
            "mail.example.com; dkim=pass reason=\"checked\" header.d=example.com"
        );
    }

    #[test]
    fn the_builder_reports_a_dkim_signature_when_it_has_one() {
        let signature = DkimSignature::parse(
            "v=1; a=rsa-sha256; d=example.com; s=default; i=@example.com; h=From; \
             bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=; \
             b=AuUoFEfDxTDkHlLXSZEpZj79LICEps6eda7W3deTVFOk4yAUoqOB",
        )
        .expect("parses");
        let verdict = DkimVerdict::new(
            DkimResult::Pass,
            Some("example.com".to_string()),
            Some("default".to_string()),
            "verified",
        );
        let results = AuthResultsBuilder::new("mail.example.com")
            .dkim_with_signature(&verdict, Some(&signature))
            .build();
        let rendered = results.render();
        assert!(rendered.contains("header.d=example.com"), "{rendered}");
        assert!(rendered.contains("header.i=@example.com"), "{rendered}");
        assert!(rendered.contains("header.b=AuUoFEfD"), "{rendered}");
    }

    #[test]
    fn the_builder_reports_a_dmarc_verdict_with_its_policy() {
        let dmarc = dmarc_verdict(DmarcResult::Pass, Some(DmarcPolicy::Reject));
        let results = AuthResultsBuilder::new("mail.example.com")
            .dmarc(&dmarc, "example.com")
            .build();
        assert_eq!(
            unfold(&results.render()),
            "mail.example.com; dmarc=pass header.from=example.com policy.dmarc=reject"
        );
    }

    #[test]
    fn the_builder_falls_back_to_the_verdict_domain_for_header_from() {
        let dmarc = dmarc_verdict(DmarcResult::Fail, None);
        let results = AuthResultsBuilder::new("mail.example.com")
            .dmarc(&dmarc, "")
            .build();
        assert_eq!(
            unfold(&results.render()),
            "mail.example.com; dmarc=fail header.from=example.com"
        );
    }

    #[test]
    fn the_builder_reports_a_locally_chosen_policy() {
        let results = AuthResultsBuilder::new("mail.example.com")
            .policy(DmarcPolicy::Quarantine)
            .build();
        assert_eq!(
            unfold(&results.render()),
            "mail.example.com; dmarc=quarantine policy.dmarc=quarantine"
        );
    }

    #[test]
    fn from_checks_builds_the_whole_header() {
        let results = full_header();
        assert_eq!(results.results.len(), 3);
        assert_eq!(results.results[0].method, "spf");
        assert_eq!(results.results[1].method, "dkim");
        assert_eq!(results.results[2].method, "dmarc");
    }

    #[test]
    fn from_checks_reports_only_what_ran() {
        let results = AuthResults::from_checks("mail.example.com", None, None, "", &[], None, "example.com");
        assert!(results.is_empty());
        assert_eq!(results.render(), "mail.example.com");
    }

    #[test]
    fn from_checks_keeps_every_dkim_verdict() {
        let one = dkim_verdict(DkimResult::Fail, "one.example");
        let two = dkim_verdict(DkimResult::Pass, "two.example");
        let results = AuthResults::from_checks(
            "mail.example.com",
            None,
            None,
            "",
            &[one, two],
            None,
            "example.com",
        );
        assert_eq!(results.results.len(), 2);
        assert!(results.render().contains("header.d=one.example"));
        assert!(results.render().contains("header.d=two.example"));
    }

    #[test]
    fn the_built_header_re_parses_into_the_same_verdicts() {
        let results = full_header();
        let reparsed = AuthResults::parse(results.to_header().trim_end()).expect("parses");
        assert_eq!(reparsed.results, results.results);
        assert_eq!(
            reparsed.get("dmarc").and_then(|r| r.property("policy.dmarc")),
            Some("reject")
        );
        assert_eq!(
            reparsed.get("spf").and_then(|r| r.property("smtp.helo")),
            Some("client.example.net")
        );
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    #[test]
    fn the_mail_from_domain_is_extracted() {
        assert_eq!(mail_from_domain(Some("joe@example.com")), "example.com");
        assert_eq!(mail_from_domain(Some("<joe@Example.COM>")), "example.com");
        assert_eq!(mail_from_domain(Some("<>")), "<>");
        assert_eq!(mail_from_domain(Some("")), "<>");
        assert_eq!(mail_from_domain(None), "<>");
        assert_eq!(mail_from_domain(Some("not-an-address")), "not-an-address");
    }

    #[test]
    fn the_header_block_stops_at_the_first_blank_line() {
        assert_eq!(
            header_block(b"A: b\r\n\r\nbody\r\n"),
            b"A: b\r\n".as_slice()
        );
        assert_eq!(header_block(b"A: b\n\nbody"), b"A: b\n".as_slice());
        assert_eq!(header_block(b"no separator"), b"no separator".as_slice());
    }

    #[test]
    fn tokens_keep_quoted_strings_together() {
        let tokens = split_tokens("pass reason=\"a b c\" smtp.helo=x");
        assert_eq!(tokens, vec!["pass", "reason=\"a b c\"", "smtp.helo=x"]);
    }

    #[test]
    fn unquoting_undoes_quoting() {
        assert_eq!(unquote("\"a b\""), "a b");
        assert_eq!(unquote("\"a\\\"b\""), "a\"b");
        assert_eq!(unquote("\"a\\\\b\""), "a\\b");
        assert_eq!(unquote("bare"), "bare");
        assert_eq!(unquote("\"\""), "");
    }
}
