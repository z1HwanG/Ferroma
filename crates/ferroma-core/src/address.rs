//! RFC 5321 email addresses.
//!
//! Ferroma never stores a mailbox as a bare `String`. SMTP envelopes, IMAP
//! `LOGIN`, the REST API and the mail queue all speak [`EmailAddress`], which
//! guarantees that whatever the peer sent was at least syntactically valid and
//! normalised before it reached the database.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{FerromaError, Result};

/// Maximum length of the local part (`local-part`), RFC 5321 §4.5.3.1.1.
pub const MAX_LOCAL_PART_LEN: usize = 64;
/// Maximum length of a single DNS label.
pub const MAX_DOMAIN_LABEL_LEN: usize = 63;
/// Maximum length of a fully-qualified domain name.
pub const MAX_DOMAIN_LEN: usize = 255;
/// Characters allowed unquoted in a local part (`atext` plus `.`), RFC 5322 §3.2.3.
const ATEXT: &str = "!#$%&'*+-/=?^_`{|}~";

/// A validated `local-part@domain` pair.
///
/// The domain is always stored lower-cased (DNS is case-insensitive). The local
/// part preserves the case the peer used, because RFC 5321 says it *may* be
/// case-sensitive; use [`EmailAddress::to_lowercase`] for the case-insensitive
/// lookups that virtually every real deployment wants.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EmailAddress {
    local: String,
    domain: String,
}

impl EmailAddress {
    /// Build an address from an already-split local part and domain.
    pub fn new(local: impl Into<String>, domain: impl Into<String>) -> Result<Self> {
        let local = local.into();
        let domain = normalise_domain(&domain.into());
        validate_local_part(&local)?;
        validate_domain(&domain)?;
        Ok(EmailAddress { local, domain })
    }

    /// Parse `local@domain`, tolerating surrounding whitespace and angle brackets.
    ///
    /// Display names (`Alice <alice@example.com>`) are **not** accepted here —
    /// [`ferroma_mail`] owns header parsing and extracts the address first.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut s = raw.trim();
        if s.starts_with('<') && s.ends_with('>') && s.len() >= 2 {
            s = &s[1..s.len() - 1];
        }
        // A stray display name or an unbalanced bracket is a hard error: silently
        // guessing here is how open relays and mis-deliveries start.
        if s.contains('<') || s.contains('>') {
            return Err(FerromaError::Invalid(format!("malformed address: {raw}")));
        }

        let (local, domain) = s
            .rsplit_once('@')
            .ok_or_else(|| FerromaError::Invalid(format!("address has no domain: {raw}")))?;
        if local.is_empty() {
            return Err(FerromaError::Invalid(format!("address has no local part: {raw}")));
        }
        Self::new(local, domain)
    }

    /// The local part (`alice` in `alice@example.com`).
    pub fn local_part(&self) -> &str {
        &self.local
    }

    /// The domain (`example.com`), always lower-cased.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Borrow the two halves.
    pub fn parts(&self) -> (&str, &str) {
        (&self.local, &self.domain)
    }

    /// Consume the address, yielding `(local_part, domain)`.
    pub fn into_parts(self) -> (String, String) {
        (self.local, self.domain)
    }

    /// Case-insensitive form, used for database lookups and rate-limit keys.
    pub fn to_lowercase(&self) -> String {
        format!("{}@{}", self.local.to_ascii_lowercase(), self.domain)
    }

    /// Whether this address lives in `domain` (case-insensitive).
    pub fn is_in_domain(&self, domain: &str) -> bool {
        self.domain == normalise_domain(domain)
    }

    /// A stable, filesystem-safe key: `alice@example.com` -> `alice_at_example.com`
    /// is *not* used for storage paths (see `ferroma-storage`), but this is handy
    /// for log correlation and cache keys.
    pub fn as_key(&self) -> String {
        self.to_lowercase()
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.local, self.domain)
    }
}

impl FromStr for EmailAddress {
    type Err = FerromaError;

    fn from_str(s: &str) -> Result<Self> {
        EmailAddress::parse(s)
    }
}

/// Lower-case a domain and strip a single trailing dot (`example.com.` is valid DNS).
pub fn normalise_domain(domain: &str) -> String {
    let d = domain.trim().trim_end_matches('.');
    d.to_ascii_lowercase()
}

/// Validate a local part: dot-atom or quoted-string, length-limited.
pub fn validate_local_part(local: &str) -> Result<()> {
    if local.is_empty() {
        return Err(FerromaError::Invalid("empty local part".into()));
    }
    if local.len() > MAX_LOCAL_PART_LEN {
        return Err(FerromaError::Invalid(format!(
            "local part longer than {MAX_LOCAL_PART_LEN} bytes"
        )));
    }

    // Quoted local part: "john..doe"@example.com — accepted verbatim.
    if local.starts_with('"') && local.ends_with('"') && local.len() >= 2 {
        let inner = &local[1..local.len() - 1];
        if inner.chars().any(|c| c == '\r' || c == '\n' || c == '\0') {
            return Err(FerromaError::Invalid("control character in quoted local part".into()));
        }
        return Ok(());
    }

    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err(FerromaError::Invalid(format!("invalid dot placement in local part: {local}")));
    }

    for ch in local.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '.' || ATEXT.contains(ch);
        if !ok {
            return Err(FerromaError::Invalid(format!(
                "invalid character {ch:?} in local part {local:?}"
            )));
        }
    }
    Ok(())
}

/// Validate a domain: one or more labels, each alphanumeric/hyphen, length-limited.
pub fn validate_domain(domain: &str) -> Result<()> {
    if domain.is_empty() {
        return Err(FerromaError::Invalid("empty domain".into()));
    }
    if domain.len() > MAX_DOMAIN_LEN {
        return Err(FerromaError::Invalid(format!("domain longer than {MAX_DOMAIN_LEN} bytes")));
    }

    // A domain literal such as [192.0.2.1] is legal SMTP but never a local mailbox domain.
    if domain.starts_with('[') {
        return Err(FerromaError::Invalid("domain literals are not valid mailbox domains".into()));
    }

    for label in domain.split('.') {
        if label.is_empty() {
            return Err(FerromaError::Invalid(format!("empty label in domain {domain}")));
        }
        if label.len() > MAX_DOMAIN_LABEL_LEN {
            return Err(FerromaError::Invalid(format!("label too long in domain {domain}")));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(FerromaError::Invalid(format!("label starts or ends with hyphen in {domain}")));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(FerromaError::Invalid(format!("invalid character in domain {domain}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_addresses() {
        let a = EmailAddress::parse("alice@example.com").unwrap();
        assert_eq!(a.local_part(), "alice");
        assert_eq!(a.domain(), "example.com");
        assert_eq!(a.to_string(), "alice@example.com");
        assert_eq!(a.to_lowercase(), "alice@example.com");
    }

    #[test]
    fn normalises_domain_case_and_trailing_dot() {
        let a = EmailAddress::parse("Alice@Example.COM.").unwrap();
        assert_eq!(a.local_part(), "Alice");
        assert_eq!(a.domain(), "example.com");
        assert_eq!(a.to_lowercase(), "alice@example.com");
    }

    #[test]
    fn accepts_angle_brackets_and_whitespace() {
        let a = EmailAddress::parse("  <bob@example.org>  ").unwrap();
        assert_eq!(a.to_string(), "bob@example.org");
    }

    #[test]
    fn accepts_dotted_and_plus_tagged_local_parts() {
        assert!(EmailAddress::parse("john.doe@example.com").is_ok());
        assert!(EmailAddress::parse("alice+newsletters@example.com").is_ok());
        assert!(EmailAddress::parse("\"john..doe\"@example.com").is_ok());
        assert!(EmailAddress::parse("o'brien@example.com").is_ok());
    }

    #[test]
    fn rejects_malformed_addresses() {
        for bad in [
            "alice",
            "@example.com",
            "alice@",
            "alice@@example.com",
            ".alice@example.com",
            "alice.@example.com",
            "al.ice..x@example.com",
            "Alice <alice@example.com>",
            "ali ce@example.com",
            "alice@exa mple.com",
            "alice@-example.com",
            "alice@example-.com",
            "alice@example..com",
            "alice@[192.0.2.1]",
        ] {
            assert!(EmailAddress::parse(bad).is_err(), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn enforces_length_limits() {
        let long_local = "a".repeat(MAX_LOCAL_PART_LEN + 1);
        assert!(EmailAddress::parse(&format!("{long_local}@example.com")).is_err());
        let ok_local = "a".repeat(MAX_LOCAL_PART_LEN);
        assert!(EmailAddress::parse(&format!("{ok_local}@example.com")).is_ok());

        let long_label = "a".repeat(MAX_DOMAIN_LABEL_LEN + 1);
        assert!(EmailAddress::parse(&format!("alice@{long_label}.com")).is_err());
    }

    #[test]
    fn domain_membership_is_case_insensitive() {
        let a = EmailAddress::parse("alice@Example.com").unwrap();
        assert!(a.is_in_domain("example.com"));
        assert!(a.is_in_domain("EXAMPLE.COM."));
        assert!(!a.is_in_domain("example.org"));
    }

    #[test]
    fn parse_from_str_and_serde_round_trip() {
        let a: EmailAddress = "alice@example.com".parse().unwrap();
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, "{\"local\":\"alice\",\"domain\":\"example.com\"}");
        let back: EmailAddress = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn address_key_is_stable_for_lookups() {
        let a = EmailAddress::parse("Alice@Example.com").unwrap();
        assert_eq!(a.as_key(), "alice@example.com");
    }
}
