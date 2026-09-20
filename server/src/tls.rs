//! TLS material for the SMTP, IMAP and HTTPS listeners.
//!
//! Three listeners terminate TLS in this process, all through the same
//! [`rustls::ServerConfig`]:
//!
//! | Listener | Port | Mode |
//! |---|---|---|
//! | SMTPS | 465 | implicit TLS |
//! | submission `STARTTLS` | 587 | upgrade in place |
//! | IMAPS | 993 | implicit TLS |
//! | IMAP `STARTTLS` | 143 | upgrade in place |
//! | HTTPS | `api.tls_port` | implicit TLS |
//!
//! # Where the certificate comes from
//!
//! * `[tls] cert_path` + `key_path` — an operator's PEM bundle (Let's Encrypt, or a
//!   corporate CA). This is the production path.
//! * `tls.self_signed_fallback` — when no PEM is configured, a self-signed
//!   certificate is generated at boot so `cargo run` can exercise the TLS code paths
//!   without a certificate authority. It is logged loudly, because a self-signed
//!   certificate on a public MX is worse than no TLS at all: clients cannot verify
//!   it and mail is rejected.
//!
//! Everything is rustls. This host's schannel is broken, but that is incidental —
//! one TLS implementation, in Rust, is the right call for a server that terminates
//! SMTP and IMAP itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use ferroma_core::config::{Config, TlsConfig};
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// The TLS a set of listeners will use, or `None` when `[tls] enabled = false`.
#[derive(Clone)]
pub struct TlsMaterial {
    acceptor: TlsAcceptor,
    /// Where the certificate came from, for the boot log.
    source: CertificateSource,
    /// The names the certificate covers, for diagnostics.
    subject_alt_names: Vec<String>,
}

impl std::fmt::Debug for TlsMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsMaterial")
            .field("source", &self.source)
            .field("subject_alt_names", &self.subject_alt_names)
            .finish()
    }
}

/// Where a certificate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateSource {
    /// A PEM bundle the operator configured.
    PemFile {
        /// The certificate chain.
        cert: PathBuf,
        /// The private key.
        key: PathBuf,
    },
    /// Generated at boot because none was configured.
    ///
    /// Only ever correct for local development and CI.
    SelfSignedGenerated,
}

impl CertificateSource {
    /// Whether this is safe on a public-facing listener.
    pub fn is_trusted(&self) -> bool {
        matches!(self, CertificateSource::PemFile { .. })
    }
}

impl TlsMaterial {
    /// A `TlsAcceptor` for `tokio-rustls`, cloned into every listener.
    pub fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    /// Where the certificate came from.
    pub fn source(&self) -> &CertificateSource {
        &self.source
    }

    /// The names the certificate is valid for.
    pub fn subject_alt_names(&self) -> &[String] {
        &self.subject_alt_names
    }
}

/// Install the process-wide crypto provider, once.
///
/// rustls 0.23 requires a process default; calling this repeatedly is harmless.
pub fn install_crypto_provider() {
    // `ring` is the provider the workspace pins, so there is exactly one TLS
    // implementation in the binary and no chance of two features fighting.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Treat an empty configured path as "not configured".
///
/// Docker Compose renders an unset variable as an empty string
/// (`FERROMA_TLS_CERT: ${FERROMA_TLS_CERT:-}`), which reaches us as `Some("")`. Left
/// alone that is not "no certificate", it is "a certificate at the empty path", and
/// the operator's reward for enabling TLS is `opening the certificate : No such file
/// or directory`. An empty string means unset everywhere else in the configuration,
/// so it means unset here too.
fn path(configured: &Option<PathBuf>) -> Option<&Path> {
    configured
        .as_deref()
        .filter(|candidate| !candidate.as_os_str().is_empty())
}

/// Build the TLS material for this configuration.
///
/// Returns `None` when TLS is disabled, which is the default for a `cargo run`
/// deployment: the plaintext listeners still work, with `STARTTLS` unadvertised.
pub fn build(config: &Config) -> Result<Option<TlsMaterial>> {
    if !config.tls.enabled {
        return Ok(None);
    }
    install_crypto_provider();

    let configured = match (path(&config.tls.cert_path), path(&config.tls.key_path)) {
        (Some(cert), Some(key)) => Some((cert.to_path_buf(), key.to_path_buf())),
        (None, None) => None,
        _ => bail!("tls.cert_path and tls.key_path must be set together"),
    };

    // A configured PEM that will not open must not cost the operator the console that
    // would let them fix it. With the self-signed fallback on — the shipped default — a
    // missing, unreadable or mismatched file is a loud error and a generated certificate
    // rather than a refusal to start; this is also what a path typo saved from the setup
    // wizard used to do to the next boot.
    let mut failure: Option<String> = None;
    let pair = match &configured {
        Some((cert, key)) => match load_pem(cert, key) {
            Ok(pair) => Some(pair),
            Err(err) => {
                failure = Some(format!("{err:#}"));
                None
            }
        },
        None => None,
    };

    let (certs, key, source) = match (pair, configured) {
        (Some((certs, key)), Some((cert, key_path))) => (
            certs,
            key,
            CertificateSource::PemFile {
                cert,
                key: key_path,
            },
        ),
        _ => {
            if let Some(problem) = &failure {
                if !config.tls.self_signed_fallback {
                    bail!("{problem}");
                }
                tracing::error!(
                    error = %problem,
                    "the configured TLS certificate could not be loaded; falling back to a \
                     self-signed certificate. Fix tls.cert_path / tls.key_path before \
                     exposing this instance to the internet."
                );
            } else if !config.tls.self_signed_fallback {
                bail!("tls.enabled is true but no tls.cert_path/tls.key_path is set");
            }
            if !config.tls.allow_insecure_dev_mode {
                bail!(
                    "no usable TLS certificate is configured and tls.allow_insecure_dev_mode is false; \
                     set tls.cert_path and tls.key_path"
                );
            }
            let (certs, key) = generate_self_signed(&config.server.hostname)?;
            (certs, key, CertificateSource::SelfSignedGenerated)
        }
    };

    let server_config = server_config(config, certs.clone(), key)?;
    let subject_alt_names = describe(&certs);

    let material = TlsMaterial {
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        source: source.clone(),
        subject_alt_names: subject_alt_names.clone(),
    };

    match &source {
        CertificateSource::PemFile { cert, .. } => {
            tracing::info!(
                cert = %cert.display(),
                names = ?subject_alt_names,
                "TLS enabled with the configured certificate"
            );
        }
        CertificateSource::SelfSignedGenerated => {
            tracing::warn!(
                names = ?subject_alt_names,
                "TLS enabled with a SELF-SIGNED certificate generated at boot. Mail clients \
                 cannot verify it and remote servers will refuse delivery. Configure \
                 tls.cert_path and tls.key_path before exposing this instance to the internet."
            );
        }
    }

    Ok(Some(material))
}

fn server_config(
    config: &Config,
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig> {
    if certs.is_empty() {
        bail!("the certificate file contains no certificates");
    }

    let builder = ServerConfig::builder();
    // `min_version` is the one protocol knob the configuration exposes; anything
    // below TLS 1.2 is refused outright rather than negotiated down. `builder()`
    // already carries the default provider's versions, so pinning 1.3 means asking
    // for a builder-with-provider first — `with_protocol_versions` only exists on
    // the `WantsVersions` stage.
    let server_config = if config.tls.min_version == "1.3" {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| anyhow!("configuring TLS protocol versions: {e}"))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| anyhow!("the certificate and private key do not match: {e}"))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| anyhow!("the certificate and private key do not match: {e}"))?
    };

    Ok(server_config)
}

/// Read a certificate bundle and its key together.
fn load_pem(
    cert: &Path,
    key: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    Ok((load_certificates(cert)?, load_private_key(key)?))
}

/// Read a PEM certificate bundle (leaf first, then intermediates).
pub fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening the certificate {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parsing certificates from {}", path.display()))?;

    if certs.is_empty() {
        bail!("{} contains no CERTIFICATE blocks", path.display());
    }
    Ok(certs)
}

/// Read a PEM private key, accepting PKCS#8, PKCS#1 (RSA) or SEC1 (EC).
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening the private key {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);

    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("parsing the private key {}", path.display()))?
        .ok_or_else(|| {
            anyhow!(
                "{} contains no PRIVATE KEY block (looked for PKCS#8, PKCS#1 and SEC1)",
                path.display()
            )
        })
}

/// Generate a self-signed certificate for `hostname`.
fn generate_self_signed(
    hostname: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    if hostname.trim().is_empty() {
        bail!("server.hostname must be set before a self-signed certificate can be generated");
    }

    let names = vec![
        hostname.to_string(),
        // A mail server is routinely reached as `localhost` during setup, and a
        // certificate that does not cover it makes every local client complain.
        "localhost".to_string(),
    ];

    let certified = rcgen::generate_simple_self_signed(names)
        .map_err(|e| anyhow!("generating a self-signed certificate: {e}"))?;

    let cert_der = CertificateDer::from(certified.cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(certified.key_pair.serialize_der())
        .map_err(|e| anyhow!("encoding the generated private key: {e}"))?;

    Ok((vec![cert_der], key_der))
}

/// The DNS names and IP addresses a certificate covers, best-effort.
///
/// This is for the boot log and the Admin panel; it does not validate anything.
fn describe(certs: &[CertificateDer<'static>]) -> Vec<String> {
    // `rustls` deliberately exposes no certificate parser, and adding one just to
    // pretty-print a log line is not worth a dependency. Parse the SAN extension by
    // hand: find the `subjectAltName` OID (2.5.29.17) in the DER and pull the
    // dNSName entries out of it.
    certs
        .first()
        .map(|cert| extract_dns_names(cert.as_ref()))
        .unwrap_or_default()
}

/// Extract `dNSName` entries from a DER certificate's SAN extension.
///
/// Deliberately forgiving: a certificate whose DER cannot be walked yields an empty
/// list, never an error. This only feeds a log line.
fn extract_dns_names(der: &[u8]) -> Vec<String> {
    // OID 2.5.29.17 = 06 03 55 1D 11
    const SAN_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x1D, 0x11];

    let Some(at) = der.windows(SAN_OID.len()).position(|w| w == SAN_OID) else {
        return Vec::new();
    };
    // ...followed by an OCTET STRING wrapping a SEQUENCE of GeneralNames.
    let rest = &der[at + SAN_OID.len()..];
    let mut names = Vec::new();
    // GeneralName ::= CHOICE { …, dNSName [2] IA5String, … } — tag 0x82.
    let mut index = 0usize;
    while index + 2 <= rest.len() {
        if rest[index] == 0x82 {
            let len = rest[index + 1] as usize;
            let start = index + 2;
            if len > 0 && start + len <= rest.len() {
                if let Ok(name) = std::str::from_utf8(&rest[start..start + len]) {
                    // Printable ASCII only, minus anything that would forge a log line.
                    if name.chars().all(|c| c.is_ascii_graphic() || c == '.' || c == '-') {
                        names.push(name.to_string());
                    }
                }
            }
            index = start + len;
        } else {
            index += 1;
        }
    }
    names.truncate(16);
    names
}

/// A one-line summary for the boot log and `/health`.
pub fn summary(material: Option<&TlsMaterial>) -> String {
    match material {
        None => "tls=off".to_string(),
        Some(m) => match m.source() {
            CertificateSource::PemFile { cert, .. } => {
                format!("tls=on (pem {}, {:?})", cert.display(), m.subject_alt_names())
            }
            CertificateSource::SelfSignedGenerated => {
                format!("tls=on (SELF-SIGNED, {:?})", m.subject_alt_names())
            }
        },
    }
}

/// Whether the configuration asks for something that must not reach production.
pub fn insecure_warnings(config: &TlsConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    if config.enabled && config.self_signed_fallback && config.cert_path.is_none() {
        warnings.push(
            "tls.self_signed_fallback will generate a certificate at boot; set \
             tls.cert_path and tls.key_path before exposing this server"
                .to_string(),
        );
    }
    if !config.enabled {
        warnings.push(
            "tls.enabled is false: STARTTLS, SMTPS, IMAPS and HTTPS are all unavailable"
                .to_string(),
        );
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_is_off_when_disabled() {
        let mut config = Config::default();
        config.tls.enabled = false;
        assert!(build(&config).unwrap().is_none());
        assert_eq!(summary(None), "tls=off");
    }

    #[test]
    fn a_missing_certificate_falls_back_to_self_signed() {
        let mut config = Config::default();
        config.tls.enabled = true;
        config.tls.self_signed_fallback = true;
        config.server.hostname = "mail.example.com".into();

        let material = build(&config).unwrap().expect("TLS material");
        assert_eq!(material.source(), &CertificateSource::SelfSignedGenerated);
        assert!(!material.source().is_trusted());
        assert!(
            material.subject_alt_names().contains(&"mail.example.com".to_string()),
            "{:?}",
            material.subject_alt_names()
        );
        // `localhost` is covered too, so a local client does not trip over the name.
        assert!(material.subject_alt_names().contains(&"localhost".to_string()));
        assert!(summary(Some(&material)).contains("SELF-SIGNED"));
    }

    #[test]
    fn the_self_signed_fallback_can_be_refused() {
        let mut config = Config::default();
        config.tls.enabled = true;
        config.tls.self_signed_fallback = true;
        config.tls.allow_insecure_dev_mode = false;

        // `Config::validate` rejects this combination at load time, and `build`
        // refuses it too, so neither path can produce an insecure production server.
        let err = build(&config).unwrap_err();
        assert!(format!("{err}").contains("allow_insecure_dev_mode"), "{err}");
    }

    #[test]
    fn cert_and_key_must_be_configured_together() {
        let mut config = Config::default();
        config.tls.enabled = true;
        config.tls.cert_path = Some(PathBuf::from("/etc/ferroma/tls/fullchain.pem"));

        let err = build(&config).unwrap_err();
        assert!(format!("{err}").contains("must be set together"), "{err}");
    }

    #[test]
    fn tls_enabled_without_any_option_is_an_error() {
        let mut config = Config::default();
        config.tls.enabled = true;
        config.tls.self_signed_fallback = false;

        let err = build(&config).unwrap_err();
        assert!(format!("{err}").contains("no tls.cert_path"), "{err}");
    }

    #[test]
    fn a_missing_certificate_file_names_the_path() {
        let err = load_certificates(Path::new("/nonexistent/fullchain.pem")).unwrap_err();
        assert!(format!("{err}").contains("fullchain.pem"), "{err}");

        let err = load_private_key(Path::new("/nonexistent/privkey.pem")).unwrap_err();
        assert!(format!("{err}").contains("privkey.pem"), "{err}");
    }

    #[test]
    fn a_pem_round_trip_through_the_loader_works() {
        // Generate, write as PEM, read back: this is exactly what an operator does,
        // so the loader has to accept what the generator produces.
        let certified = rcgen::generate_simple_self_signed(vec!["mail.example.com".into()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, certified.cert.pem()).unwrap();
        std::fs::write(&key_path, certified.key_pair.serialize_pem()).unwrap();

        let certs = load_certificates(&cert_path).unwrap();
        assert_eq!(certs.len(), 1);
        let key = load_private_key(&key_path).unwrap();
        assert!(!key.secret_der().is_empty());

        let config = Config::default();
        install_crypto_provider();
        server_config(&config, certs, key).expect("the generated pair must be usable");
    }

    #[test]
    fn dns_names_are_extracted_from_a_real_certificate() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["mail.example.com".into(), "example.com".into()])
                .unwrap();
        let der = certified.cert.der();
        let names = extract_dns_names(der);
        assert!(names.contains(&"mail.example.com".to_string()), "{names:?}");
        assert!(names.contains(&"example.com".to_string()), "{names:?}");
    }

    #[test]
    fn a_certificate_without_a_san_yields_no_names_rather_than_panicking() {
        assert!(extract_dns_names(&[]).is_empty());
        assert!(extract_dns_names(&[0u8; 64]).is_empty());
        assert!(extract_dns_names(b"not a certificate at all").is_empty());
    }

    #[test]
    fn an_empty_configured_path_counts_as_unset() {
        // Docker Compose renders an unset variable as `""`, so `Some("")` is what a
        // `${FERROMA_TLS_CERT:-}` actually delivers. Treating it as a real path makes
        // enabling TLS fail with "opening the certificate : No such file or directory".
        let config = Config {
            tls: TlsConfig {
                enabled: true,
                cert_path: Some(PathBuf::from("")),
                key_path: Some(PathBuf::from("")),
                self_signed_fallback: true,
                ..TlsConfig::default()
            },
            ..Config::default()
        };

        let material = build(&config).unwrap().expect("TLS material");
        assert_eq!(material.source(), &CertificateSource::SelfSignedGenerated);

        // ...and with the fallback off it is a clear configuration error, not a
        // confusing file-not-found.
        let strict = Config {
            tls: TlsConfig {
                self_signed_fallback: false,
                ..config.tls.clone()
            },
            ..Config::default()
        };
        let err = build(&strict).unwrap_err();
        assert!(format!("{err}").contains("no tls.cert_path"), "{err}");
    }

    #[test]
    fn insecure_configurations_are_reported() {
        let self_signed_in_production = TlsConfig {
            enabled: true,
            cert_path: None,
            self_signed_fallback: true,
            ..TlsConfig::default()
        };
        assert!(!insecure_warnings(&self_signed_in_production).is_empty());

        let disabled = TlsConfig {
            enabled: false,
            ..TlsConfig::default()
        };
        let warnings = insecure_warnings(&disabled);
        assert!(warnings.iter().any(|w| w.contains("STARTTLS")), "{warnings:?}");

        // A real certificate with TLS on produces no warnings at all.
        let configured = TlsConfig {
            enabled: true,
            cert_path: Some(PathBuf::from("/etc/ferroma/tls/fullchain.pem")),
            self_signed_fallback: false,
            ..TlsConfig::default()
        };
        assert!(insecure_warnings(&configured).is_empty());
    }
}
