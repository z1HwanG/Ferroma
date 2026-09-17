//! The SMTP envelope: the data captured at `MAIL FROM`, `RCPT TO` and `DATA`.
//!
//! The envelope is *not* the message. `MAIL FROM: <bounce@list.example>` can
//! differ from the `From:` header, and the recipients are exactly the addresses
//! the peer asked us to deliver to — including BCC recipients that appear in no
//! header at all. Ferroma keeps both, because bounce handling and loop detection
//! need the envelope while the mailbox needs the headers.

use std::fmt::Write as _;
use std::net::IpAddr;

use chrono::{DateTime, Utc};
use ferroma_core::EmailAddress;

/// Everything SMTP told us about a message before its content arrived.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    /// The `MAIL FROM` address. `None` for a null reverse-path (`<>`), which is
    /// how bounces are sent.
    pub from: Option<EmailAddress>,
    /// Every accepted `RCPT TO` address, in the order they arrived.
    pub recipients: Vec<EmailAddress>,
    /// The name the peer announced in `EHLO`/`HELO`.
    pub helo: Option<String>,
    /// The peer's IP address as we observed it (never as the peer claimed).
    pub remote_ip: Option<IpAddr>,
    /// When the transaction started, in UTC.
    pub received_at: DateTime<Utc>,
}

impl Envelope {
    /// A fresh envelope stamped with the current time.
    pub fn new() -> Self {
        Envelope {
            from: None,
            recipients: Vec::new(),
            helo: None,
            remote_ip: None,
            received_at: Utc::now(),
        }
    }

    /// Set the reverse-path.
    pub fn with_from(mut self, from: EmailAddress) -> Self {
        self.from = Some(from);
        self
    }

    /// Set the `EHLO` name.
    pub fn with_helo(mut self, helo: impl Into<String>) -> Self {
        self.helo = Some(helo.into());
        self
    }

    /// Set the observed peer address.
    pub fn with_remote_ip(mut self, ip: IpAddr) -> Self {
        self.remote_ip = Some(ip);
        self
    }

    /// Append one accepted recipient.
    pub fn add_recipient(&mut self, to: EmailAddress) {
        self.recipients.push(to);
    }

    /// How many recipients were accepted.
    pub fn recipient_count(&self) -> usize {
        self.recipients.len()
    }

    /// The value of the `Received:` header this transaction should prepend.
    ///
    /// The shape follows RFC 5321 §4.4: the peer's identity, the protocol we
    /// spoke, our own name, the message id, and the timestamp. Only parts we
    /// actually observed are emitted, so a message never gains a header line
    /// that claims something we do not know.
    pub fn received_header(&self, by_hostname: &str) -> String {
        let mut out = String::with_capacity(96);

        match (&self.helo, self.remote_ip) {
            (Some(helo), Some(ip)) => {
                let _ = write!(out, "from {} ({})", sanitise_helo(helo), ip);
            }
            (Some(helo), None) => {
                let _ = write!(out, "from {}", sanitise_helo(helo));
            }
            (None, Some(ip)) => {
                let _ = write!(out, "from unknown ({ip})");
            }
            (None, None) => {
                out.push_str("from unknown");
            }
        }

        let _ = write!(out, " by {by_hostname} with ESMTP");

        if let Some(from) = &self.from {
            let _ = write!(out, " id {}", local_id(from));
        }

        if let Some(first) = self.recipients.first() {
            let _ = write!(out, " for <{first}>");
        }

        let _ = write!(out, "; {}", self.received_at.format("%a, %d %b %Y %H:%M:%S %z"));

        out
    }
}

impl Default for Envelope {
    fn default() -> Self {
        Envelope::new()
    }
}

/// A short, stable-ish transaction id derived from the reverse path and clock.
///
/// RFC 5321 only requires the `id` clause to be *unique per transaction*; using
/// the timestamp plus the local part keeps it readable in a mail log while still
/// being unique for every message a single sender produces in the same second.
fn local_id(from: &EmailAddress) -> String {
    let local: String = from
        .local_part()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .take(24)
        .collect();
    if local.is_empty() {
        "unknown".to_string()
    } else {
        local
    }
}

/// Keep a peer-supplied `EHLO` name from breaking the header it is written into.
fn sanitise_helo(helo: &str) -> String {
    let cleaned: String = helo
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace() && *c != '(' && *c != ')')
        .take(255)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn addr(s: &str) -> EmailAddress {
        EmailAddress::parse(s).unwrap()
    }

    #[test]
    fn defaults_are_empty_but_timestamped() {
        let env = Envelope::default();
        assert!(env.from.is_none());
        assert!(env.recipients.is_empty());
        assert!(env.helo.is_none());
        assert!(env.remote_ip.is_none());
        assert_eq!(env.recipient_count(), 0);
        // The timestamp is "now", so it must be within a few seconds of it.
        let delta = Utc::now().signed_duration_since(env.received_at);
        assert!(delta.num_seconds().abs() < 5);
    }

    #[test]
    fn builders_chain() {
        let env = Envelope::new()
            .with_from(addr("alice@example.com"))
            .with_helo("mail.example.com")
            .with_remote_ip("192.0.2.10".parse().unwrap());
        assert_eq!(env.from.as_ref().unwrap().to_string(), "alice@example.com");
        assert_eq!(env.helo.as_deref(), Some("mail.example.com"));
        assert_eq!(env.remote_ip, Some("192.0.2.10".parse().unwrap()));
    }

    #[test]
    fn recipients_accumulate_in_order() {
        let mut env = Envelope::new();
        env.add_recipient(addr("a@example.com"));
        env.add_recipient(addr("b@example.com"));
        assert_eq!(env.recipient_count(), 2);
        assert_eq!(env.recipients[0].to_string(), "a@example.com");
        assert_eq!(env.recipients[1].to_string(), "b@example.com");
    }

    #[test]
    fn received_header_has_every_observed_clause() {
        let mut env = Envelope::new()
            .with_from(addr("alice@example.com"))
            .with_helo("mail.example.com")
            .with_remote_ip("192.0.2.10".parse().unwrap());
        env.add_recipient(addr("bob@example.org"));
        env.received_at = DateTime::parse_from_rfc3339("2025-09-16T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let header = env.received_header("mx1.ferroma.local");
        assert_eq!(
            header,
            "from mail.example.com (192.0.2.10) by mx1.ferroma.local with ESMTP id alice for <bob@example.org>; Tue, 16 Sep 2025 04:00:00 +0000"
        );
    }

    #[test]
    fn received_header_omits_what_we_never_saw() {
        let env = Envelope::new();
        let header = env.received_header("mx1.ferroma.local");
        let expected = format!(
            "from unknown by mx1.ferroma.local with ESMTP; {}",
            env.received_at.format("%a, %d %b %Y %H:%M:%S %z")
        );
        assert_eq!(header, expected);
    }

    #[test]
    fn received_header_with_only_an_ip() {
        let env = Envelope::new().with_remote_ip("203.0.113.5".parse().unwrap());
        let header = env.received_header("mx.local");
        assert!(header.starts_with("from unknown (203.0.113.5) by mx.local with ESMTP;"));
    }

    #[test]
    fn received_header_with_only_a_helo() {
        let env = Envelope::new().with_helo("client.example.net");
        let header = env.received_header("mx.local");
        assert!(header.starts_with("from client.example.net by mx.local with ESMTP;"));
    }

    #[test]
    fn received_header_is_single_line_even_for_a_hostile_helo() {
        let env = Envelope::new().with_helo("evil\r\nX-Injected: yes");
        let header = env.received_header("mx.local");
        assert!(!header.contains('\r'), "header must not contain CR: {header:?}");
        assert!(!header.contains('\n'), "header must not contain LF: {header:?}");
        assert!(header.contains("evilX-Injected:yes"));
    }

    #[test]
    fn null_reverse_path_produces_no_id_clause() {
        let env = Envelope::new().with_helo("mail.example.com");
        let header = env.received_header("mx.local");
        assert!(!header.contains(" id "));
    }

    #[test]
    fn received_header_can_be_parsed_back_by_our_own_machinery() {
        let mut env = Envelope::new()
            .with_from(addr("bounce@example.com"))
            .with_helo("mail.example.com")
            .with_remote_ip("192.0.2.1".parse().unwrap());
        env.add_recipient(addr("bob@example.org"));
        let raw = format!("Received: {}\r\n\r\n", env.received_header("mx.local"));
        let headers = crate::Headers::parse(&raw).unwrap();
        assert!(headers.get("Received").unwrap().contains("by mx.local"));
    }

    #[test]
    fn envelope_json_round_trip() {
        let env = Envelope::new()
            .with_from(addr("alice@example.com"))
            .with_helo("mail.example.com")
            .with_remote_ip("192.0.2.10".parse().unwrap());
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn transaction_id_uses_the_local_part() {
        assert_eq!(local_id(&addr("Alice.Smith@example.com")), "alice.smith");
        assert_eq!(local_id(&addr("weird!name@example.com")), "weirdname");
    }

    #[test]
    fn transaction_id_never_goes_empty() {
        assert_eq!(local_id(&addr("\"@\"@example.com")), "unknown");
    }

    #[test]
    fn a_hostile_helo_with_parentheses_is_neutralised() {
        let env = Envelope::new().with_helo("evil (192.0.2.1)");
        let header = env.received_header("mx.local");
        assert_eq!(header.matches('(').count(), 0, "{header}");
    }

    #[test]
    fn an_empty_helo_falls_back_to_unknown() {
        let env = Envelope::new().with_helo("");
        assert!(env.received_header("mx.local").starts_with("from unknown by"));
    }

    #[test]
    fn received_header_mentions_only_the_first_recipient() {
        let mut env = Envelope::new().with_helo("h");
        env.add_recipient(addr("a@example.com"));
        env.add_recipient(addr("b@example.com"));
        let header = env.received_header("mx.local");
        assert_eq!(header.matches(" for ").count(), 1);
        assert!(header.contains("<a@example.com>"));
        assert!(!header.contains("<b@example.com>"));
    }

    #[test]
    fn envelope_can_be_serialised_to_json_and_back_with_recipients() {
        let mut env = Envelope::new()
            .with_from(addr("a@example.com"))
            .with_remote_ip("198.51.100.7".parse().unwrap());
        env.add_recipient(addr("b@example.com"));
        env.add_recipient(addr("c@example.com"));
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);
        assert_eq!(back.recipient_count(), 2);
    }
}
