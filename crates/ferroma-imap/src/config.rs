//! IMAP server configuration.
//!
//! This is the crate's own view of the `[imap]` block in `ferroma.toml` plus the
//! `[tls]` and maildir settings the server needs. It is deliberately a plain
//! struct rather than a `serde` type: the workspace configuration lives in
//! `ferroma-core`, and the binary maps it into this struct once at boot. That
//! keeps `ferroma-imap` testable without a config file.

use std::path::PathBuf;

/// Everything the IMAP listener and its sessions need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapServerConfig {
    /// Address to bind the plaintext listener to.
    pub host: String,
    /// Plaintext port (`143` in production, `0` on a random port in tests).
    pub port: u16,
    /// Implicit-TLS port (`993`), or `0` to disable the listener.
    pub imaps_port: u16,
    /// Text after the `* OK` greeting.
    pub banner: String,
    /// Refuse `LOGIN`/`AUTHENTICATE` on an unencrypted connection.
    pub require_tls_for_login: bool,
    /// Seconds an idle session may live before `* BYE`.
    pub idle_timeout_secs: u64,
    /// Longest `IDLE` a client may hold, in seconds.
    pub max_idle_secs: u64,
    /// Advertise and honour `IDLE`.
    pub enable_idle: bool,
    /// Advertise and honour `MOVE` and `UIDPLUS`.
    pub enable_move: bool,
    /// Largest literal accepted by `APPEND`, in bytes.
    pub max_append_size: u64,
    /// How many messages one `FETCH` may address.
    pub max_fetch_messages: usize,
    /// Serve TLS at all.
    pub tls_enabled: bool,
    /// PEM bundle: leaf certificate followed by intermediates.
    pub tls_cert_path: Option<PathBuf>,
    /// PEM private key (PKCS#8 or PKCS#1).
    pub tls_key_path: Option<PathBuf>,
    /// The Maildir root.
    pub maildir_root: PathBuf,
    /// `fsync` every stored message before its rename is visible.
    pub fsync_on_write: bool,
}

impl Default for ImapServerConfig {
    fn default() -> Self {
        ImapServerConfig {
            host: "0.0.0.0".to_string(),
            port: 143,
            imaps_port: 0,
            banner: "Ferroma IMAP4rev1 ready".to_string(),
            require_tls_for_login: false,
            idle_timeout_secs: 1800,
            max_idle_secs: 1740,
            enable_idle: true,
            enable_move: true,
            max_append_size: 26_214_400,
            max_fetch_messages: 5_000,
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
            maildir_root: PathBuf::from("data/mail"),
            fsync_on_write: false,
        }
    }
}

impl ImapServerConfig {
    /// The address the plaintext listener binds to.
    pub fn listen_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The address the implicit-TLS listener binds to, when enabled.
    pub fn imaps_address(&self) -> Option<String> {
        if self.imaps_port == 0 {
            return None;
        }
        Some(format!("{}:{}", self.host, self.imaps_port))
    }

    /// Whether `STARTTLS` can be offered: TLS must be configured and usable.
    pub fn starttls_available(&self) -> bool {
        self.tls_enabled
            && self.tls_cert_path.is_some()
            && self.tls_key_path.is_some()
    }

    /// Whether the implicit-TLS listener can start.
    pub fn imaps_available(&self) -> bool {
        self.imaps_port != 0 && self.starttls_available()
    }

    /// Validate the configuration, so a typo fails at boot rather than on the
    /// first client.
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 && self.imaps_port == 0 {
            return Err("imap: at least one of port/imaps_port must be set".into());
        }
        if self.imaps_port != 0 && self.imaps_port == self.port {
            return Err("imap: port and imaps_port must differ".into());
        }
        if self.imaps_port != 0 && !self.tls_enabled {
            return Err("imap: imaps_port is set but TLS is disabled".into());
        }
        if self.max_append_size == 0 {
            return Err("imap: max_append_size must be greater than zero".into());
        }
        if self.max_idle_secs == 0 {
            return Err("imap: max_idle_secs must be greater than zero".into());
        }
        if self.idle_timeout_secs < self.max_idle_secs {
            return Err("imap: idle_timeout_secs must be >= max_idle_secs".into());
        }
        if self.max_fetch_messages == 0 {
            return Err("imap: max_fetch_messages must be greater than zero".into());
        }
        if self.starttls_available() {
            for path in [&self.tls_cert_path, &self.tls_key_path].into_iter().flatten() {
                if !path.is_file() {
                    return Err(format!("imap: TLS file `{}` does not exist", path.display()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_specification_section_17() {
        let config = ImapServerConfig::default();
        assert_eq!(config.port, 143);
        assert_eq!(config.imaps_port, 0);
        assert!(!config.tls_enabled);
        assert!(config.enable_idle);
        assert!(config.enable_move);
        assert_eq!(config.max_append_size, 26_214_400);
        assert_eq!(config.listen_address(), "0.0.0.0:143");
        assert!(config.imaps_address().is_none());
    }

    #[test]
    fn imaps_address_follows_the_port() {
        let config = ImapServerConfig {
            imaps_port: 993,
            ..ImapServerConfig::default()
        };
        assert_eq!(config.imaps_address().as_deref(), Some("0.0.0.0:993"));
    }

    #[test]
    fn starttls_requires_configured_files() {
        let mut config = ImapServerConfig::default();
        assert!(!config.starttls_available());
        config.tls_enabled = true;
        assert!(!config.starttls_available());
        config.tls_cert_path = Some(PathBuf::from("/tmp/cert.pem"));
        assert!(!config.starttls_available());
        config.tls_key_path = Some(PathBuf::from("/tmp/key.pem"));
        assert!(config.starttls_available());
    }

    #[test]
    fn imaps_available_needs_a_port_and_files() {
        let mut config = ImapServerConfig::default();
        assert!(!config.imaps_available());
        config.imaps_port = 993;
        assert!(!config.imaps_available());
        config.tls_enabled = true;
        config.tls_cert_path = Some(PathBuf::from("/tmp/cert.pem"));
        config.tls_key_path = Some(PathBuf::from("/tmp/key.pem"));
        assert!(config.imaps_available());
    }

    #[test]
    fn validation_accepts_the_defaults() {
        assert_eq!(ImapServerConfig::default().validate(), Ok(()));
    }

    #[test]
    fn validation_rejects_incoherent_settings() {
        let config = ImapServerConfig {
            port: 0,
            ..ImapServerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ImapServerConfig {
            imaps_port: ImapServerConfig::default().port,
            ..ImapServerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ImapServerConfig {
            imaps_port: 993,
            ..ImapServerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ImapServerConfig {
            max_append_size: 0,
            ..ImapServerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ImapServerConfig {
            max_idle_secs: 0,
            ..ImapServerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ImapServerConfig {
            max_idle_secs: ImapServerConfig::default().idle_timeout_secs + 1,
            ..ImapServerConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ImapServerConfig {
            max_fetch_messages: 0,
            ..ImapServerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_reports_a_missing_tls_file() {
        let mut config = ImapServerConfig {
            tls_enabled: true,
            tls_cert_path: Some(PathBuf::from("definitely/not/here.pem")),
            tls_key_path: Some(PathBuf::from("definitely/not/here.key")),
            ..ImapServerConfig::default()
        };
        let err = config.validate().expect_err("must reject");
        assert!(err.contains("does not exist"));
        config.tls_cert_path = None;
        config.tls_key_path = None;
        assert!(config.validate().is_ok());
    }
}
