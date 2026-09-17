//! The complete runtime configuration.
//!
//! # Layering
//!
//! Configuration is built in three layers, later layers winning:
//!
//! 1. **Embedded defaults** — [`Config::default_toml`], compiled into the binary so
//!    Ferroma can always start with sane values.
//! 2. **A TOML file** — `config/ferroma.toml` by default, or `--config <path>`.
//! 3. **Environment variables** — two forms:
//!    * Generic, fully-qualified: `FERROMA__SERVER__HOSTNAME`, `FERROMA__SMTP__PORT`
//!      (double underscore separates the path segments).
//!    * Shorthand aliases for the ones operators actually type every day:
//!      `DATABASE_URL`, `FERROMA_HOSTNAME`, `FERROMA_API_PORT`, …
//!
//! Environment overrides are type-coerced against the default value at that path,
//! so `FERROMA__SMTP__PORT=2525` becomes an integer and
//! `FERROMA__TLS__ENABLED=true` becomes a boolean.
//!
//! Unknown keys are rejected (`deny_unknown_fields`) — a typo in `ferroma.toml`
//! should stop the server at boot, not silently leave a limit disabled.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::limits::Limits;
use crate::{FerromaError, Result};

/// How log lines are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, one line per event. Best for `docker compose logs`.
    #[default]
    Text,
    /// Newline-delimited JSON. Best for log shipping and Loki/ELK.
    Json,
}

/// `[server]` — identity, data location and process-level settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Product name shown in banners and the admin UI.
    pub name: String,
    /// FQDN this server identifies itself as (SMTP `EHLO`, `Message-ID`, `Received`).
    pub hostname: String,
    /// Root of all Ferroma state: maildir, attachments, TLS material, backups.
    pub data_dir: PathBuf,
    /// `tracing` filter directive, e.g. `info` or `ferroma_smtp=debug,info`.
    pub log_level: String,
    /// Text or JSON log rendering.
    pub log_format: LogFormat,
    /// Tokio worker threads. `0` means "as many as there are CPUs".
    pub worker_threads: usize,
    /// How long to wait for in-flight work during shutdown, in seconds.
    pub shutdown_timeout_secs: u64,
    /// Reject startup when the configuration is inconsistent.
    pub strict_config: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            name: "Ferroma".into(),
            hostname: "localhost".into(),
            data_dir: default_data_dir(),
            log_level: "info".into(),
            log_format: LogFormat::Text,
            worker_threads: 0,
            shutdown_timeout_secs: 30,
            strict_config: true,
        }
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

/// `[database]` — PostgreSQL connection and pool settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// `postgres://user:pass@host:5432/dbname`.
    pub url: String,
    /// Upper bound on pooled connections.
    pub max_connections: u32,
    /// Connections kept warm even when idle.
    pub min_connections: u32,
    /// Seconds to wait for a connection from the pool before failing. This also
    /// bounds opening a brand-new connection.
    pub acquire_timeout_secs: u64,
    /// Idle connections are closed after this many seconds.
    pub idle_timeout_secs: u64,
    /// Lifetime cap for a pooled connection, in seconds. PostgreSQL and proxies
    /// (pgbouncer) both punish immortal connections.
    pub max_lifetime_secs: u64,
    /// Apply `migrations/` at startup.
    pub run_migrations: bool,
    /// Log every SQL statement. Development only — it prints message subjects.
    pub log_statements: bool,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            url: "postgres://ferroma:ferroma@localhost:5432/ferroma".into(),
            max_connections: 20,
            min_connections: 2,
            acquire_timeout_secs: 10,
            idle_timeout_secs: 600,
            max_lifetime_secs: 1800,
            run_migrations: true,
            log_statements: false,
        }
    }
}

impl DatabaseConfig {
    /// Whether the configured URL actually points at `localhost`/loopback.
    pub fn is_local(&self) -> bool {
        self.url.contains("@localhost")
            || self.url.contains("@127.0.0.1")
            || self.url.contains("@[::1]")
    }
}

/// `[smtp]` — receiving, submission and implicit-TLS listeners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmtpConfig {
    /// Run any SMTP listener at all.
    pub enabled: bool,
    /// Address to bind. `0.0.0.0` in Docker, `127.0.0.1` for local development.
    pub host: String,
    /// Inbound MX port (plaintext + STARTTLS).
    pub port: u16,
    /// Authenticated submission port (STARTTLS expected).
    pub submission_port: u16,
    /// Implicit TLS port (SMTPS). `0` disables the listener.
    pub smtps_port: u16,
    /// Text after the `220` greeting.
    pub banner: String,
    /// Require `EHLO`/`HELO` before `MAIL FROM`.
    pub helo_required: bool,
    /// Refuse `AUTH` unless the connection is encrypted.
    pub require_tls_for_auth: bool,
    /// On submission ports, require successful authentication before `MAIL FROM`.
    pub require_auth_on_submission: bool,
    /// Advertise and honour the `SIZE` extension.
    pub advertise_size: bool,
    /// Seconds a client may take to send the next command.
    pub command_timeout_secs: u64,
    /// Seconds a client may take to finish `DATA`.
    pub data_timeout_secs: u64,
    /// Add a `Received:` header to inbound mail.
    pub add_received_header: bool,
    /// Emit `DSN`/`ENHANCEDSTATUSCODES`/`8BITMIME`/`PIPELINING`/`SMTPUTF8` in `EHLO`.
    pub advertise_extensions: bool,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        SmtpConfig {
            enabled: true,
            host: "0.0.0.0".into(),
            port: 25,
            submission_port: 587,
            // Implicit TLS is off until `[tls]` is configured; production enables 465.
            smtps_port: 0,
            banner: "Ferroma ESMTP ready".into(),
            helo_required: true,
            require_tls_for_auth: false,
            require_auth_on_submission: true,
            advertise_size: true,
            command_timeout_secs: 300,
            data_timeout_secs: 600,
            add_received_header: true,
            advertise_extensions: true,
        }
    }
}

/// `[imap]` — IMAP4rev1 listeners for third-party clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImapConfig {
    pub enabled: bool,
    pub host: String,
    /// Plaintext port, normally upgraded with `STARTTLS`.
    pub port: u16,
    /// Implicit TLS port. `0` disables the listener.
    pub imaps_port: u16,
    /// Text after the `* OK` greeting.
    pub banner: String,
    /// Refuse `LOGIN` on an unencrypted connection.
    pub require_tls_for_login: bool,
    /// Seconds an idle session may live before `* BYE`.
    pub idle_timeout_secs: u64,
    /// Longest `IDLE` a client may hold, in seconds (RFC 2177 recommends 29 min).
    pub max_idle_secs: u64,
    /// Advertise `IDLE`.
    pub enable_idle: bool,
    /// Advertise `MOVE` / `UIDPLUS`.
    pub enable_move: bool,
    /// Largest literal accepted by `APPEND`, in bytes.
    pub max_append_size: u64,
}

impl Default for ImapConfig {
    fn default() -> Self {
        ImapConfig {
            enabled: true,
            host: "0.0.0.0".into(),
            port: 143,
            // Implicit TLS is off until `[tls]` is configured; production enables 993.
            imaps_port: 0,
            banner: "Ferroma IMAP4rev1 ready".into(),
            require_tls_for_login: false,
            idle_timeout_secs: 1800,
            max_idle_secs: 1740,
            enable_idle: true,
            enable_move: true,
            max_append_size: 26_214_400,
        }
    }
}

/// `[queue]` — the outbound mail queue and its retry policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    /// Run the delivery workers in this process.
    pub enabled: bool,
    /// Concurrent delivery tasks.
    pub workers: usize,
    /// Attempts before a message is bounced as permanently failed.
    pub max_attempts: u32,
    /// Backoff schedule in seconds; the last entry repeats once exhausted.
    pub retry_schedule_secs: Vec<u64>,
    /// How often the dispatcher looks for due messages, in seconds.
    pub poll_interval_secs: u64,
    /// Seconds allowed for one delivery attempt end-to-end.
    pub delivery_timeout_secs: u64,
    /// Seconds allowed for a single outbound socket operation.
    pub connect_timeout_secs: u64,
    /// Simultaneous connections to a single remote MX host.
    pub max_connections_per_host: usize,
    /// Generate a bounce message when delivery finally fails.
    pub bounce_on_failure: bool,
    /// Keep delivered queue rows for this many days before pruning.
    pub retention_days: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            enabled: true,
            workers: 4,
            max_attempts: 12,
            retry_schedule_secs: vec![60, 300, 900, 3600, 21_600, 86_400],
            poll_interval_secs: 10,
            delivery_timeout_secs: 300,
            connect_timeout_secs: 30,
            max_connections_per_host: 4,
            bounce_on_failure: true,
            retention_days: 30,
        }
    }
}

impl QueueConfig {
    /// Backoff for attempt `n` (1-based), saturating at the end of the schedule.
    pub fn backoff_for_attempt(&self, attempt: u32) -> u64 {
        if self.retry_schedule_secs.is_empty() {
            return 60;
        }
        let idx = attempt.saturating_sub(1) as usize;
        let idx = idx.min(self.retry_schedule_secs.len() - 1);
        self.retry_schedule_secs[idx]
    }
}

/// `[tls]` — certificates for SMTP/IMAP/HTTPS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// Serve TLS at all. When false, only plaintext listeners are started.
    pub enabled: bool,
    /// PEM bundle: leaf certificate followed by intermediates.
    pub cert_path: Option<PathBuf>,
    /// PEM private key (PKCS#8 or PKCS#1).
    pub key_path: Option<PathBuf>,
    /// Generate a self-signed certificate at boot when no PEM is configured.
    /// Intended for local development and CI — never for a production MX.
    pub self_signed_fallback: bool,
    /// Minimum protocol version: `"1.2"` or `"1.3"`.
    pub min_version: String,
    /// Also load certificates the OS trusts, for outbound TLS verification.
    pub use_platform_roots: bool,
    /// Refuse to start with `self_signed_fallback` outside development.
    pub allow_insecure_dev_mode: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        TlsConfig {
            enabled: false,
            cert_path: None,
            key_path: None,
            self_signed_fallback: true,
            min_version: "1.2".into(),
            use_platform_roots: true,
            allow_insecure_dev_mode: true,
        }
    }
}

/// `[dns]` — the resolver used for MX lookups and SPF/DKIM/DMARC verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DnsConfig {
    /// Explicit resolvers, e.g. `["1.1.1.1:53", "9.9.9.9:53"]`. Empty = system config.
    pub resolvers: Vec<String>,
    /// Per-query timeout, in seconds.
    pub timeout_secs: u64,
    /// Number of attempts per query.
    pub attempts: usize,
    /// Cache positive answers for this many seconds (bounded by record TTL).
    pub cache_ttl_secs: u64,
    /// Cache negative answers for this many seconds.
    pub negative_ttl_secs: u64,
    /// Also try TCP when a UDP answer is truncated.
    pub tcp_fallback: bool,
}

impl Default for DnsConfig {
    fn default() -> Self {
        DnsConfig {
            resolvers: Vec::new(),
            timeout_secs: 5,
            attempts: 3,
            cache_ttl_secs: 300,
            negative_ttl_secs: 60,
            tcp_fallback: true,
        }
    }
}

/// `[dkim]` — outbound signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DkimConfig {
    pub enabled: bool,
    /// DKIM selector, as published at `<selector>._domainkey.<domain>`.
    pub selector: String,
    /// PEM private key used to sign.
    pub private_key_path: Option<PathBuf>,
    /// Sign only mail from this domain. `None` signs every local domain.
    pub domain: Option<String>,
    /// `relaxed` or `simple`.
    pub canonicalization: String,
    /// Headers covered by the signature, in order.
    pub headers_to_sign: Vec<String>,
    /// Reject inbound mail whose DKIM signature does not verify.
    pub verify_inbound: bool,
}

impl Default for DkimConfig {
    fn default() -> Self {
        DkimConfig {
            enabled: false,
            selector: "default".into(),
            private_key_path: None,
            domain: None,
            canonicalization: "relaxed".into(),
            headers_to_sign: vec![
                "From".into(),
                "To".into(),
                "Cc".into(),
                "Subject".into(),
                "Date".into(),
                "Message-ID".into(),
                "MIME-Version".into(),
                "Content-Type".into(),
                "Content-Transfer-Encoding".into(),
                "Reply-To".into(),
                "In-Reply-To".into(),
                "References".into(),
            ],
            verify_inbound: true,
        }
    }
}

/// `[spf]` and `[dmarc]` inbound policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Evaluate SPF on inbound mail.
    pub spf_enabled: bool,
    /// Evaluate DMARC on inbound mail.
    pub dmarc_enabled: bool,
    /// What to do when DMARC says `p=reject` and the message fails.
    /// `"reject"`, `"quarantine"` or `"none"`.
    pub dmarc_failure_action: String,
    /// Add `Authentication-Results` to inbound mail.
    pub add_auth_results: bool,
    /// Maximum DNS lookups allowed while evaluating one SPF record (RFC 7208 §4.6.4).
    pub spf_max_lookups: usize,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            spf_enabled: true,
            dmarc_enabled: true,
            dmarc_failure_action: "quarantine".into(),
            add_auth_results: true,
            spf_max_lookups: 10,
        }
    }
}

/// `[api]` — the HTTP surface: REST API, Client API (FCP), Webmail and Admin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub enabled: bool,
    pub host: String,
    /// Plaintext HTTP port. Behind a reverse proxy in production.
    pub port: u16,
    /// Optional HTTPS listener. `0` disables it.
    pub tls_port: u16,
    /// Base path for the management API from the specification (`/api/v1`).
    pub base_path: String,
    /// Externally reachable URL, used in `.well-known/ferroma` and links.
    pub public_url: String,
    /// Directory holding the built Webmail assets.
    pub webmail_dir: Option<PathBuf>,
    /// Directory holding the built Admin assets.
    pub admin_dir: Option<PathBuf>,
    /// Allowed CORS origins. Empty means same-origin only.
    pub cors_origins: Vec<String>,
    /// HMAC secret for access/refresh tokens. `None` generates an ephemeral one,
    /// which invalidates every session on restart — fine for development only.
    pub jwt_secret: Option<String>,
    /// Access-token lifetime, in seconds.
    pub access_token_ttl_secs: u64,
    /// Refresh-token lifetime, in seconds.
    pub refresh_token_ttl_secs: u64,
    /// Web session lifetime, in seconds.
    pub session_ttl_secs: u64,
    /// Set the `Secure` flag on cookies. Must stay true in production.
    pub secure_cookies: bool,
    /// Largest request body accepted, in bytes.
    pub max_request_size: u64,
    /// Trust `X-Forwarded-For` / `X-Real-IP` from a reverse proxy.
    pub trust_proxy_headers: bool,
    /// Serve the Webmail and Admin single-page apps.
    pub serve_frontend: bool,
    /// Allow the first-run setup wizard to create the initial admin account.
    pub enable_setup_wizard: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            enabled: true,
            host: "0.0.0.0".into(),
            port: 8080,
            // HTTPS is served by the reverse proxy by default; set 8443 to terminate here.
            tls_port: 0,
            base_path: "/api/v1".into(),
            public_url: "http://localhost:8080".into(),
            webmail_dir: None,
            admin_dir: None,
            cors_origins: Vec::new(),
            jwt_secret: None,
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 2_592_000,
            session_ttl_secs: 86_400,
            secure_cookies: false,
            max_request_size: 26_214_400,
            trust_proxy_headers: false,
            serve_frontend: true,
            enable_setup_wizard: true,
        }
    }
}

/// `[client]` — the Ferroma Client Protocol (FCP) contract with official clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    /// Protocol version this server speaks.
    pub protocol_version: u32,
    /// Oldest client protocol still accepted; older clients are told to upgrade.
    pub min_protocol_version: u32,
    /// Changes returned per sync page.
    pub sync_page_size: usize,
    /// Seconds between WebSocket pings.
    pub ws_heartbeat_secs: u64,
    /// Bytes per attachment upload chunk.
    pub attachment_chunk_size: u64,
    /// Keep deleted-message tombstones this long so offline clients can catch up.
    pub tombstone_retention_days: u32,
    /// Sessions idle for longer than this are revoked, in days.
    pub session_idle_days: u32,
    /// Advertise push-notification capability.
    pub push_enabled: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            protocol_version: 1,
            min_protocol_version: 1,
            sync_page_size: 500,
            ws_heartbeat_secs: 30,
            attachment_chunk_size: 1_048_576,
            tombstone_retention_days: 30,
            session_idle_days: 90,
            push_enabled: false,
        }
    }
}

/// `[storage]` — how message bodies and attachments land on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Root of the Maildir tree. Defaults to `<data_dir>/mail`.
    pub maildir_root: Option<PathBuf>,
    /// Root for attachment blobs. Defaults to `<data_dir>/attachments`.
    pub attachment_root: Option<PathBuf>,
    /// `fsync` every message before acknowledging `DATA`. Costs throughput,
    /// buys durability — keep it on unless you have battery-backed storage.
    pub fsync_on_write: bool,
    /// Filesystem layout for mailboxes.
    pub layout: MailboxLayout,
    /// Compute and store the SHA-256 of every stored message and attachment.
    pub checksum: bool,
    /// Refuse to store more than this per mailbox, unless overridden per user.
    pub enforce_quota: bool,
    /// Move trashed mail to `Trash` instead of unlinking immediately.
    pub soft_delete: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            maildir_root: None,
            attachment_root: None,
            fsync_on_write: true,
            layout: MailboxLayout::Maildir,
            checksum: true,
            enforce_quota: true,
            soft_delete: true,
        }
    }
}

/// On-disk mailbox format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailboxLayout {
    /// `domain/user/Maildir/{cur,new,tmp}` with IMAP folders as `Maildir/.Folder`.
    #[default]
    Maildir,
    /// `domain/user/<folder>/{cur,new,tmp}` — one Maildir per folder.
    MaildirPerFolder,
}

/// The whole configuration tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub smtp: SmtpConfig,
    pub imap: ImapConfig,
    pub queue: QueueConfig,
    pub tls: TlsConfig,
    pub dns: DnsConfig,
    pub dkim: DkimConfig,
    pub policy: PolicyConfig,
    pub api: ApiConfig,
    pub client: ClientConfig,
    pub storage: StorageConfig,
    pub limits: Limits,
}

impl Default for Config {
    /// Every section's own default.
    ///
    /// Kept manual rather than `derive`d: with a derive, adding a section that has no
    /// `Default` would still compile, and the risk is a field that silently gets a
    /// zero value (a limit of `0`, a port of `0`) instead of a compile error here.
    #[allow(clippy::derivable_impls)]
    fn default() -> Self {
        Config {
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            smtp: SmtpConfig::default(),
            imap: ImapConfig::default(),
            queue: QueueConfig::default(),
            tls: TlsConfig::default(),
            dns: DnsConfig::default(),
            dkim: DkimConfig::default(),
            policy: PolicyConfig::default(),
            api: ApiConfig::default(),
            client: ClientConfig::default(),
            storage: StorageConfig::default(),
            limits: Limits::default(),
        }
    }
}

/// Environment variables that map onto a config path without the `FERROMA__` prefix.
const ENV_ALIASES: &[(&str, &[&str])] = &[
    ("DATABASE_URL", &["database", "url"]),
    ("FERROMA_HOSTNAME", &["server", "hostname"]),
    ("FERROMA_DATA_DIR", &["server", "data_dir"]),
    ("FERROMA_LOG_LEVEL", &["server", "log_level"]),
    ("FERROMA_LOG_FORMAT", &["server", "log_format"]),
    ("FERROMA_SMTP_HOST", &["smtp", "host"]),
    ("FERROMA_SMTP_PORT", &["smtp", "port"]),
    ("FERROMA_SMTP_SUBMISSION_PORT", &["smtp", "submission_port"]),
    ("FERROMA_IMAP_PORT", &["imap", "port"]),
    ("FERROMA_API_HOST", &["api", "host"]),
    ("FERROMA_API_PORT", &["api", "port"]),
    ("FERROMA_API_PUBLIC_URL", &["api", "public_url"]),
    ("FERROMA_JWT_SECRET", &["api", "jwt_secret"]),
    ("FERROMA_TLS_ENABLED", &["tls", "enabled"]),
    ("FERROMA_TLS_CERT", &["tls", "cert_path"]),
    ("FERROMA_TLS_KEY", &["tls", "key_path"]),
    ("FERROMA_DKIM_ENABLED", &["dkim", "enabled"]),
    ("FERROMA_DKIM_KEY", &["dkim", "private_key_path"]),
    ("FERROMA_DKIM_SELECTOR", &["dkim", "selector"]),
];

/// Prefix for generic overrides: `FERROMA__SMTP__PORT=2525`.
const ENV_PREFIX: &str = "FERROMA__";

impl Config {
    /// The TOML document compiled into the binary.
    pub const DEFAULT_TOML: &'static str = include_str!("../../../config/ferroma.toml");

    /// Parse the embedded defaults.
    pub fn defaults() -> Result<Config> {
        toml::from_str(Self::DEFAULT_TOML)
            .map_err(|e| FerromaError::Config(format!("embedded config/ferroma.toml is invalid: {e}")))
    }

    /// Load configuration: embedded defaults, then `path` if given, then environment.
    ///
    /// When `path` is `None`, `config/ferroma.toml` and `ferroma.toml` are tried and
    /// their absence is not an error.
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let mut root = serde_json::to_value(Config::defaults()?)
            .map_err(|e| FerromaError::Config(format!("cannot serialise defaults: {e}")))?;

        let explicit = path.is_some();
        let file = match path {
            Some(p) => Some(p.to_path_buf()),
            None => ["config/ferroma.toml", "ferroma.toml"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file()),
        };

        if let Some(ref p) = file {
            if !p.is_file() {
                return Err(FerromaError::Config(format!(
                    "configuration file not found: {}",
                    p.display()
                )));
            }
            let text = std::fs::read_to_string(p).map_err(|e| {
                FerromaError::Config(format!("cannot read {}: {e}", p.display()))
            })?;
            let parsed: toml::Value = toml::from_str(&text).map_err(|e| {
                FerromaError::Config(format!("{} is not valid TOML: {e}", p.display()))
            })?;
            let parsed = serde_json::to_value(parsed)
                .map_err(|e| FerromaError::Config(format!("cannot normalise config: {e}")))?;
            merge_json(&mut root, parsed);
        } else if explicit {
            return Err(FerromaError::Config("no configuration file given".into()));
        }

        apply_env(&mut root)?;

        let config: Config = serde_json::from_value(root).map_err(|e| {
            FerromaError::Config(format!("configuration does not match the schema: {e}"))
        })?;

        if config.server.strict_config {
            config.validate()?;
        }
        Ok(config)
    }

    /// Load from a TOML string. Used by tests and by `ferroma config check`.
    pub fn from_toml_str(text: &str) -> Result<Config> {
        let mut root = serde_json::to_value(Config::defaults()?)
            .map_err(|e| FerromaError::Config(format!("cannot serialise defaults: {e}")))?;
        let parsed: toml::Value =
            toml::from_str(text).map_err(|e| FerromaError::Config(format!("invalid TOML: {e}")))?;
        let parsed = serde_json::to_value(parsed)
            .map_err(|e| FerromaError::Config(format!("cannot normalise config: {e}")))?;
        merge_json(&mut root, parsed);
        let config: Config = serde_json::from_value(root)
            .map_err(|e| FerromaError::Config(format!("config schema mismatch: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Render the effective configuration as TOML, for `ferroma config show`.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| FerromaError::Config(format!("cannot render config: {e}")))
    }

    /// Fail fast on values that would produce a broken or unsafe server.
    pub fn validate(&self) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();

        if self.server.name.trim().is_empty() {
            problems.push("server.name must not be empty".into());
        }
        if self.server.hostname.trim().is_empty() {
            problems.push("server.hostname must not be empty".into());
        }
        if crate::address::validate_domain(&crate::address::normalise_domain(&self.server.hostname))
            .is_err()
        {
            problems.push(format!(
                "server.hostname {:?} is not a valid DNS name",
                self.server.hostname
            ));
        }
        if self.database.url.trim().is_empty() {
            problems.push("database.url must not be empty".into());
        } else if !self.database.url.starts_with("postgres://")
            && !self.database.url.starts_with("postgresql://")
        {
            problems.push("database.url must be a postgres:// URL".into());
        }
        if self.database.min_connections > self.database.max_connections {
            problems.push("database.min_connections must not exceed max_connections".into());
        }

        for (name, host) in [
            ("smtp.host", &self.smtp.host),
            ("imap.host", &self.imap.host),
            ("api.host", &self.api.host),
        ] {
            if host.parse::<IpAddr>().is_err() && host != "localhost" {
                problems.push(format!("{name} must be an IP address or `localhost`, got {host:?}"));
            }
        }

        if self.smtp.enabled {
            let ports = [
                ("smtp.port", self.smtp.port),
                ("smtp.submission_port", self.smtp.submission_port),
                ("smtp.smtps_port", self.smtp.smtps_port),
            ];
            for (name, port) in ports {
                if name != "smtp.smtps_port" && port == 0 {
                    problems.push(format!("{name} must not be 0"));
                }
            }
            // Only *active* listeners have to be distinct; 0 means "listener disabled".
            let active: Vec<u16> = ports.iter().map(|(_, p)| *p).filter(|p| *p != 0).collect();
            let mut seen = std::collections::HashSet::new();
            if active.iter().any(|p| !seen.insert(*p)) {
                problems.push("smtp ports must be distinct".into());
            }
            if self.smtp.smtps_port != 0 && !self.tls.enabled {
                problems.push("smtp.smtps_port is set but tls.enabled is false".into());
            }
        }
        if self.imap.enabled {
            if self.imap.port == 0 {
                problems.push("imap.port must not be 0".into());
            }
            if self.imap.imaps_port != 0 && self.imap.imaps_port == self.imap.port {
                problems.push("imap.port and imap.imaps_port must differ".into());
            }
            if self.imap.imaps_port != 0 && !self.tls.enabled {
                problems.push("imap.imaps_port is set but tls.enabled is false".into());
            }
        }
        if self.api.enabled {
            if self.api.port == 0 {
                problems.push("api.port must not be 0".into());
            }
            if self.api.tls_port != 0 {
                if !self.tls.enabled {
                    problems.push("api.tls_port is set but tls.enabled is false".into());
                }
                if self.api.tls_port == self.api.port {
                    problems.push("api.port and api.tls_port must differ".into());
                }
            }
        }

        if !self.api.base_path.starts_with('/') {
            problems.push("api.base_path must start with `/`".into());
        }
        if url::Url::parse(&self.api.public_url).is_err() {
            problems.push(format!("api.public_url {:?} is not a URL", self.api.public_url));
        }
        if self.api.access_token_ttl_secs == 0 {
            problems.push("api.access_token_ttl_secs must be greater than zero".into());
        }
        if self.api.refresh_token_ttl_secs < self.api.access_token_ttl_secs {
            problems.push("api.refresh_token_ttl_secs must be >= access_token_ttl_secs".into());
        }

        if self.tls.enabled {
            match (&self.tls.cert_path, &self.tls.key_path) {
                (Some(_), Some(_)) => {}
                (None, None) if self.tls.self_signed_fallback => {}
                _ => problems.push(
                    "tls.cert_path and tls.key_path must be set together (or enable self_signed_fallback)"
                        .into(),
                ),
            }
        }
        if !matches!(self.tls.min_version.as_str(), "1.2" | "1.3") {
            problems.push("tls.min_version must be \"1.2\" or \"1.3\"".into());
        }
        if self.tls.self_signed_fallback && !self.tls.allow_insecure_dev_mode {
            problems.push(
                "tls.self_signed_fallback requires tls.allow_insecure_dev_mode = true".into(),
            );
        }

        if self.dkim.enabled {
            if self.dkim.private_key_path.is_none() {
                problems.push("dkim.enabled requires dkim.private_key_path".into());
            }
            if self.dkim.selector.trim().is_empty() {
                problems.push("dkim.selector must not be empty".into());
            }
            if !matches!(self.dkim.canonicalization.as_str(), "relaxed" | "simple") {
                problems.push("dkim.canonicalization must be \"relaxed\" or \"simple\"".into());
            }
        }

        if !matches!(
            self.policy.dmarc_failure_action.as_str(),
            "none" | "quarantine" | "reject"
        ) {
            problems.push("policy.dmarc_failure_action must be none, quarantine or reject".into());
        }

        if self.queue.enabled {
            if self.queue.workers == 0 {
                problems.push("queue.workers must be greater than zero".into());
            }
            if self.queue.retry_schedule_secs.contains(&0) {
                problems.push("queue.retry_schedule_secs must not contain zero".into());
            }
            if self.queue.max_attempts == 0 {
                problems.push("queue.max_attempts must be greater than zero".into());
            }
        }

        if self.client.min_protocol_version > self.client.protocol_version {
            problems
                .push("client.min_protocol_version must not exceed client.protocol_version".into());
        }
        if self.client.sync_page_size == 0 {
            problems.push("client.sync_page_size must be greater than zero".into());
        }

        if let Some(d) = &self.storage.maildir_root {
            if d.as_os_str().is_empty() {
                problems.push("storage.maildir_root must not be empty when set".into());
            }
        }

        if let Err(e) = self.limits.validate() {
            problems.push(e);
        }

        if self.dns.timeout_secs == 0 {
            problems.push("dns.timeout_secs must be greater than zero".into());
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(FerromaError::Config(format!(
                "{} configuration problem(s):\n  - {}",
                problems.len(),
                problems.join("\n  - ")
            )))
        }
    }

    /// Root of all Ferroma state.
    pub fn data_dir(&self) -> &Path {
        &self.server.data_dir
    }

    /// Root of the Maildir tree.
    pub fn maildir_root(&self) -> PathBuf {
        self.storage
            .maildir_root
            .clone()
            .unwrap_or_else(|| self.server.data_dir.join("mail"))
    }

    /// Root of the attachment store.
    pub fn attachment_root(&self) -> PathBuf {
        self.storage
            .attachment_root
            .clone()
            .unwrap_or_else(|| self.server.data_dir.join("attachments"))
    }

    /// Directory holding generated TLS material (self-signed dev certificates).
    pub fn tls_dir(&self) -> PathBuf {
        self.server.data_dir.join("tls")
    }

    /// Whether SASL authentication is allowed on a plaintext connection.
    pub fn allows_plaintext_auth(&self) -> bool {
        !self.smtp.require_tls_for_auth
    }

    /// Human-readable one-line summary printed at boot.
    pub fn summary(&self) -> String {
        format!(
            "{} {} on {} | smtp={} imap={} api={} tls={} db={}",
            self.server.name,
            crate::VERSION,
            self.server.hostname,
            if self.smtp.enabled { "on" } else { "off" },
            if self.imap.enabled { "on" } else { "off" },
            if self.api.enabled { "on" } else { "off" },
            if self.tls.enabled { "on" } else { "off" },
            if self.database.is_local() { "local" } else { "remote" },
        )
    }
}

/// Recursively merge `overlay` into `base`. Objects merge key-by-key; anything
/// else (scalars, arrays) is replaced wholesale.
fn merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(over_map)) => {
            for (key, value) in over_map {
                match base_map.get_mut(&key) {
                    Some(slot) => merge_json(slot, value),
                    None => {
                        base_map.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Apply `FERROMA__…` and aliased environment variables onto the JSON tree.
fn apply_env(root: &mut serde_json::Value) -> Result<()> {
    // Aliases first, so an explicit FERROMA__ form can still override them.
    for (name, path) in ENV_ALIASES {
        if let Ok(value) = std::env::var(name) {
            set_path(root, path, coerce(existing(root, path), &value));
        }
    }

    let mut generic: Vec<(String, String)> = Vec::new();
    for (key, value) in std::env::vars() {
        if let Some(rest) = key.strip_prefix(ENV_PREFIX) {
            if rest.is_empty() {
                continue;
            }
            generic.push((rest.to_string(), value));
        }
    }
    generic.sort();

    for (rest, value) in generic {
        let path: Vec<String> = rest
            .split("__")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        if path.is_empty() {
            continue;
        }
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        set_path(root, &refs, coerce(existing(root, &refs), &value));
    }
    Ok(())
}

/// Read the current value at a path, if any.
fn existing<'a>(root: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut cur = root;
    for seg in path {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Write a value at a path, creating intermediate objects as needed.
fn set_path(root: &mut serde_json::Value, path: &[&str], value: serde_json::Value) {
    let mut cur = root;
    for (i, seg) in path.iter().enumerate() {
        let last = i == path.len() - 1;
        if !cur.is_object() {
            *cur = serde_json::Value::Object(serde_json::Map::new());
        }
        let map = cur.as_object_mut().expect("just ensured object");
        if last {
            map.insert((*seg).to_string(), value);
            return;
        }
        cur = map
            .entry((*seg).to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
}

/// Turn an environment string into JSON, using the previous value's type as a hint.
fn coerce(previous: Option<&serde_json::Value>, raw: &str) -> serde_json::Value {
    match previous {
        Some(serde_json::Value::Bool(_)) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => serde_json::Value::Bool(true),
            "0" | "false" | "no" | "off" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(raw.to_string()),
        },
        Some(serde_json::Value::Number(n)) if n.is_i64() || n.is_u64() => match raw.trim().parse::<i64>() {
            Ok(v) => serde_json::Value::Number(v.into()),
            Err(_) => serde_json::Value::String(raw.to_string()),
        },
        Some(serde_json::Value::Number(_)) => match raw.trim().parse::<f64>() {
            Ok(v) => serde_json::Number::from_f64(v)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(raw.to_string())),
            Err(_) => serde_json::Value::String(raw.to_string()),
        },
        Some(serde_json::Value::Array(_)) => serde_json::Value::Array(
            raw.split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| serde_json::Value::String(s.to_string()))
                .collect(),
        ),
        _ => match raw.trim().to_ascii_lowercase().as_str() {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(raw.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_defaults_are_valid() {
        let cfg = Config::defaults().expect("embedded config must parse");
        cfg.validate().expect("embedded config must validate");
        assert_eq!(cfg.server.name, "Ferroma");
        assert_eq!(cfg.limits.max_message_size, 26_214_400);
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let cfg = Config::default();
        let text = cfg.to_toml().unwrap();
        let back = Config::from_toml_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn toml_overlay_only_changes_named_keys() {
        let cfg = Config::from_toml_str(
            r#"
            [server]
            hostname = "mail.example.com"

            [smtp]
            port = 2525
            "#,
        )
        .unwrap();
        assert_eq!(cfg.server.hostname, "mail.example.com");
        assert_eq!(cfg.smtp.port, 2525);
        // untouched values survive the merge
        assert_eq!(cfg.smtp.submission_port, 587);
        assert_eq!(cfg.imap.port, 143);
        assert_eq!(cfg.limits.max_recipients, 100);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::from_toml_str("[smtp]\nprot = 25\n").unwrap_err();
        assert!(format!("{err}").contains("schema"), "{err}");
    }

    #[test]
    fn invalid_values_are_reported_together() {
        let err = Config::from_toml_str(
            r#"
            [database]
            url = "mysql://nope"

            [api]
            base_path = "api"
            "#,
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("database.url must be a postgres:// URL"), "{text}");
        assert!(text.contains("api.base_path must start with `/`"), "{text}");
    }

    #[test]
    fn smtps_without_tls_is_rejected() {
        let err = Config::from_toml_str("[smtp]\nsmtps_port = 465\n").unwrap_err();
        assert!(format!("{err}").contains("smtps_port is set but tls.enabled is false"), "{err}");
    }

    #[test]
    fn zero_means_listener_disabled() {
        // The shipped defaults keep 465/993/8443 off so a bare `cargo run` boots.
        let cfg = Config::defaults().unwrap();
        assert_eq!(cfg.smtp.smtps_port, 0);
        assert_eq!(cfg.imap.imaps_port, 0);
        assert_eq!(cfg.api.tls_port, 0);
        assert!(!cfg.tls.enabled);
        cfg.validate().unwrap();
    }

    #[test]
    fn duplicate_active_smtp_ports_are_rejected() {
        let err = Config::from_toml_str("[smtp]\nport = 2525\nsubmission_port = 2525\n").unwrap_err();
        assert!(format!("{err}").contains("smtp ports must be distinct"), "{err}");
    }

    #[test]
    fn sasl_over_plaintext_policy() {
        let mut cfg = Config::default();
        assert!(cfg.allows_plaintext_auth());
        cfg.smtp.require_tls_for_auth = true;
        assert!(!cfg.allows_plaintext_auth());
    }

    #[test]
    fn derived_paths_hang_off_data_dir() {
        let mut cfg = Config::default();
        cfg.server.data_dir = PathBuf::from("/srv/ferroma");
        assert_eq!(cfg.maildir_root(), PathBuf::from("/srv/ferroma/mail"));
        assert_eq!(cfg.attachment_root(), PathBuf::from("/srv/ferroma/attachments"));
        assert_eq!(cfg.tls_dir(), PathBuf::from("/srv/ferroma/tls"));

        cfg.storage.maildir_root = Some(PathBuf::from("/mnt/mail"));
        assert_eq!(cfg.maildir_root(), PathBuf::from("/mnt/mail"));
    }

    #[test]
    fn queue_backoff_follows_the_schedule_and_saturates() {
        let q = QueueConfig::default();
        assert_eq!(q.backoff_for_attempt(1), 60);
        assert_eq!(q.backoff_for_attempt(2), 300);
        assert_eq!(q.backoff_for_attempt(3), 900);
        assert_eq!(q.backoff_for_attempt(4), 3600);
        assert_eq!(q.backoff_for_attempt(5), 21_600);
        assert_eq!(q.backoff_for_attempt(6), 86_400);
        assert_eq!(q.backoff_for_attempt(99), 86_400);
        assert_eq!(q.backoff_for_attempt(0), 60);
    }

    #[test]
    fn env_coercion_respects_the_schema_type() {
        let mut root = serde_json::to_value(Config::default()).unwrap();

        let port = coerce(existing(&root, &["smtp", "port"]), "2525");
        set_path(&mut root, &["smtp", "port"], port);

        let tls = coerce(existing(&root, &["tls", "enabled"]), "true");
        set_path(&mut root, &["tls", "enabled"], tls);

        let resolvers = coerce(
            existing(&root, &["dns", "resolvers"]),
            "1.1.1.1:53, 9.9.9.9:53",
        );
        set_path(&mut root, &["dns", "resolvers"], resolvers);

        let cfg: Config = serde_json::from_value(root).unwrap();
        assert_eq!(cfg.smtp.port, 2525);
        assert!(cfg.tls.enabled);
        assert_eq!(cfg.dns.resolvers, vec!["1.1.1.1:53", "9.9.9.9:53"]);
    }

    #[test]
    fn summary_mentions_the_essentials() {
        let s = Config::default().summary();
        assert!(s.contains("Ferroma"));
        assert!(s.contains("smtp=on"));
        assert!(s.contains("db=local"));
    }
}
