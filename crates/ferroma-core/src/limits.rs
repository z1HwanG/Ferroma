//! The `[limits]` policy block.
//!
//! These are the numbers from the project specification (§39). They are the
//! platform's *safety rails*: message size, recipient fan-out, connection caps and
//! send-rate throttles. Every one of them is enforced at the protocol edge
//! (SMTP/IMAP) and re-checked in the mail core, so a compromised front end cannot
//! bypass them by talking to the API instead.

use serde::{Deserialize, Serialize};

/// Hard and soft limits applied platform-wide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Largest message accepted or sent, in bytes. Default 25 MiB.
    pub max_message_size: u64,

    /// Maximum `RCPT TO` fan-out per transaction.
    pub max_recipients: usize,

    /// Global cap on simultaneous SMTP connections.
    pub max_connections: usize,

    /// Per-source-IP cap on simultaneous SMTP connections.
    pub max_connections_per_ip: usize,

    /// Inbound SMTP commands accepted per minute per IP.
    pub smtp_rate_limit: u32,

    /// Authenticated submission messages accepted per hour per account.
    pub submission_rate_limit: u32,

    /// Messages per account per day, counted from the queue, not from memory.
    pub daily_send_limit: u32,

    /// Default mailbox quota in bytes. 1 GiB.
    pub mailbox_quota: u64,

    /// Maximum attachments per message.
    pub max_attachments: usize,

    /// Largest single attachment, in bytes.
    pub max_attachment_size: u64,

    /// How long a client may stay connected without progressing, in seconds.
    pub idle_timeout_secs: u64,

    /// How long an SMTP `DATA` phase may take, in seconds.
    pub data_timeout_secs: u64,

    /// How many messages a single IMAP session may fetch in one command.
    pub max_fetch_messages: usize,

    /// Maximum nesting depth accepted by the MIME parser.
    pub max_mime_depth: usize,

    /// Failed logins per account before the account is temporarily locked.
    pub max_failed_logins: u32,

    /// How long a temporary login lock lasts, in seconds.
    pub login_lockout_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_message_size: 26_214_400,
            max_recipients: 100,
            max_connections: 100,
            max_connections_per_ip: 10,
            smtp_rate_limit: 100,
            submission_rate_limit: 50,
            daily_send_limit: 500,
            mailbox_quota: 1_073_741_824,
            max_attachments: 50,
            max_attachment_size: 26_214_400,
            idle_timeout_secs: 300,
            data_timeout_secs: 600,
            max_fetch_messages: 5_000,
            max_mime_depth: 20,
            max_failed_logins: 10,
            login_lockout_secs: 900,
        }
    }
}

impl Limits {
    /// Convenience for tests and for the SMTP size check.
    pub fn max_message_size_usize(&self) -> usize {
        self.max_message_size.min(usize::MAX as u64) as usize
    }

    /// Validate the configuration itself, so a typo in `limits.toml` fails at boot
    /// instead of silently disabling a limit.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_message_size == 0 {
            return Err("limits.max_message_size must be greater than zero".into());
        }
        if self.max_recipients == 0 {
            return Err("limits.max_recipients must be greater than zero".into());
        }
        if self.max_connections_per_ip > self.max_connections {
            return Err(
                "limits.max_connections_per_ip must not exceed limits.max_connections".into(),
            );
        }
        if self.max_attachment_size > self.max_message_size {
            return Err(
                "limits.max_attachment_size must not exceed limits.max_message_size".into(),
            );
        }
        if self.max_mime_depth == 0 || self.max_mime_depth > 100 {
            return Err("limits.max_mime_depth must be between 1 and 100".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_specification() {
        let l = Limits::default();
        assert_eq!(l.max_message_size, 26_214_400);
        assert_eq!(l.max_recipients, 100);
        assert_eq!(l.max_connections, 100);
        assert_eq!(l.max_connections_per_ip, 10);
        assert_eq!(l.smtp_rate_limit, 100);
        assert_eq!(l.submission_rate_limit, 50);
        assert_eq!(l.daily_send_limit, 500);
        assert_eq!(l.mailbox_quota, 1_073_741_824);
        assert!(l.validate().is_ok());
    }

    #[test]
    fn rejects_incoherent_limits() {
        let too_many_per_ip = Limits {
            max_connections_per_ip: Limits::default().max_connections + 1,
            ..Limits::default()
        };
        assert!(too_many_per_ip.validate().is_err());

        let attachment_bigger_than_message = Limits {
            max_attachment_size: Limits::default().max_message_size + 1,
            ..Limits::default()
        };
        assert!(attachment_bigger_than_message.validate().is_err());

        let zero_message_size = Limits {
            max_message_size: 0,
            ..Limits::default()
        };
        assert!(zero_message_size.validate().is_err());
    }

    #[test]
    fn parses_the_toml_block_from_the_spec() {
        let toml_src = r#"
max_message_size = 26214400
max_recipients = 100
max_connections = 100
max_connections_per_ip = 10
smtp_rate_limit = 100
submission_rate_limit = 50
daily_send_limit = 500
mailbox_quota = 1073741824
"#;
        let parsed: Limits = toml::from_str(toml_src).unwrap();
        assert_eq!(parsed, Limits::default());
    }
}
