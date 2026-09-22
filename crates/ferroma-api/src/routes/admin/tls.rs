//! `docs/api.md` §4.9 — the TLS posture of this deployment.
//!
//! TLS material lives in files (`[tls] cert_path` / `key_path`) and on listener
//! ports (`smtp.smtps_port`, `imap.imaps_port`, `api.tls_port`), so "is TLS on?",
//! "which certificate is loaded?" and "can this process read it?" are questions about
//! *this host*, not about a row in the database. This module answers them from the
//! running configuration plus a `stat` of the two files, which is what the Admin
//! panel's TLS screen (specification §4.3) needs and what nothing else exposed.
//!
//! # What is deliberately not reported
//!
//! The private key's **contents** and **fingerprint** are never returned. A hash of a
//! secret is still a durable fact about that secret, and an operator who needs to
//! confirm which key is loaded can compare file size and mtime instead. The
//! certificate is public data, so its SHA-256 fingerprint *is* reported — that is the
//! value to compare against `openssl x509 -fingerprint -noout` on the host.
//!
//! The certificate's validity window (`notBefore` / `notAfter`) is **not** parsed:
//! this build links no X.509 parser, and deriving an expiry from a file mtime would be
//! a worse answer than saying nothing.

use std::path::Path;

use axum::extract::State;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::AdminUser;
use crate::state::AppState;

/// The largest file whose bytes this endpoint will read to fingerprint it.
///
/// A certificate bundle is a few kilobytes. The cap exists so a misconfigured
/// `cert_path` pointing at something enormous cannot turn an admin page load into a
/// multi-gigabyte read.
pub const MAX_FINGERPRINT_BYTES: u64 = 4 * 1024 * 1024;

/// One TLS file, as the host sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TlsFileStatus {
    /// The configured path, when one is configured.
    pub path: Option<String>,
    /// Whether something exists at that path.
    pub present: bool,
    /// Whether this process can open it. A key the server cannot read is the classic
    /// silent deployment failure, so it is reported rather than assumed.
    pub readable: bool,
    /// Size in bytes, when it exists.
    pub size_bytes: Option<u64>,
    /// When it was last modified.
    pub modified_at: Option<DateTime<Utc>>,
    /// The SHA-256 of the file's bytes, lower-case hex. `None` for a private key, or
    /// for a file that is missing, unreadable or over [`MAX_FINGERPRINT_BYTES`].
    pub sha256: Option<String>,
    /// Why the file could not be used, when it could not.
    pub error: Option<String>,
}

/// Every port that can speak TLS, and what the server publishes about itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TlsListeners {
    /// Implicit-TLS SMTP port (`0` when not listening).
    pub smtps_port: u16,
    /// Implicit-TLS IMAP port (`0` when not listening).
    pub imaps_port: u16,
    /// HTTPS port (`0` when the reverse proxy terminates TLS).
    pub https_port: u16,
    /// The externally reachable URL.
    pub public_url: String,
    /// Whether that URL is already `https://`.
    pub public_url_is_tls: bool,
}

/// The `GET /api/v1/tls` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TlsStatusResponse {
    /// `[tls] enabled`.
    pub enabled: bool,
    /// `"1.2"` or `"1.3"`.
    pub min_version: String,
    /// Whether a self-signed certificate is generated when none is configured.
    pub self_signed_fallback: bool,
    /// Whether OS roots are also trusted for outbound verification.
    pub use_platform_roots: bool,
    /// Whether the self-signed fallback is tolerated outside development.
    pub allow_insecure_dev_mode: bool,
    /// The PEM bundle: leaf certificate followed by intermediates.
    pub certificate: TlsFileStatus,
    /// The PEM private key.
    pub private_key: TlsFileStatus,
    /// Where TLS is offered.
    pub listeners: TlsListeners,
}

/// `GET /api/v1/tls`
pub async fn tls_status(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<TlsStatusResponse>, ApiError> {
    let tls = &state.config.tls;
    let public_url = state.config.api.public_url.clone();

    Ok(Json(TlsStatusResponse {
        enabled: tls.enabled,
        min_version: tls.min_version.clone(),
        self_signed_fallback: tls.self_signed_fallback,
        use_platform_roots: tls.use_platform_roots,
        allow_insecure_dev_mode: tls.allow_insecure_dev_mode,
        // The certificate is public, so it is fingerprinted.
        certificate: inspect(tls.cert_path.as_deref(), true),
        // The key is not: presence, size and readability are enough to spot a
        // permissions mistake, which is the failure this panel exists to catch.
        private_key: inspect(tls.key_path.as_deref(), false),
        listeners: TlsListeners {
            smtps_port: state.config.smtp.smtps_port,
            imaps_port: state.config.imap.imaps_port,
            https_port: state.config.api.tls_port,
            public_url_is_tls: public_url
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("https://"),
            public_url,
        },
    }))
}

/// Stat one configured TLS file, optionally hashing its bytes.
///
/// Never returns an error: a missing or unreadable certificate is exactly what the
/// caller wants to *see*, not a reason to fail the request.
pub fn inspect(path: Option<&Path>, hash_contents: bool) -> TlsFileStatus {
    let Some(path) = path else {
        return TlsFileStatus {
            path: None,
            present: false,
            readable: false,
            size_bytes: None,
            modified_at: None,
            sha256: None,
            error: None,
        };
    };

    let shown = Some(path.display().to_string());
    let mut status = TlsFileStatus {
        path: shown,
        present: false,
        readable: false,
        size_bytes: None,
        modified_at: None,
        sha256: None,
        error: None,
    };

    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) => {
            status.error = Some(e.to_string());
            return status;
        }
    };
    status.present = true;
    status.size_bytes = Some(metadata.len());
    status.modified_at = metadata.modified().ok().map(DateTime::<Utc>::from);

    if !metadata.is_file() {
        status.error = Some("not a regular file".to_string());
        return status;
    }

    // Opening, not reading: this is the permission check that catches a key the
    // server's own user cannot read, without touching the bytes.
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            status.error = Some(e.to_string());
            return status;
        }
    };
    status.readable = true;
    drop(file);

    if !hash_contents {
        return status;
    }
    if metadata.len() > MAX_FINGERPRINT_BYTES {
        status.error = Some(format!(
            "not fingerprinted: {0} bytes exceeds the {MAX_FINGERPRINT_BYTES}-byte limit",
            metadata.len()
        ));
        return status;
    }
    match std::fs::read(path) {
        Ok(bytes) => status.sha256 = Some(sha256_hex(&bytes)),
        Err(e) => status.error = Some(e.to_string()),
    }
    status
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_is_reported_rather_than_failing() {
        let status = inspect(Some(Path::new("definitely/not/here.pem")), true);
        assert!(!status.present);
        assert!(!status.readable);
        assert!(status.sha256.is_none());
        assert!(status.error.is_some(), "the reason must be visible");
    }

    #[test]
    fn no_configured_path_is_an_empty_status() {
        let status = inspect(None, true);
        assert_eq!(status.path, None);
        assert!(!status.present);
        assert!(status.error.is_none(), "nothing configured is not an error");
    }

    #[test]
    fn a_certificate_is_fingerprinted_from_its_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("fullchain.pem");
        let pem = b"-----BEGIN CERTIFICATE-----\nnot really\n".to_vec();
        std::fs::write(&path, &pem).expect("write");

        let status = inspect(Some(&path), true);
        assert!(status.present && status.readable);
        assert_eq!(status.size_bytes, Some(pem.len() as u64));
        assert!(status.modified_at.is_some());
        assert_eq!(status.sha256.as_deref(), Some(sha256_hex(&pem).as_str()));
    }

    #[test]
    fn a_private_key_is_never_fingerprinted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("privkey.pem");
        std::fs::write(&path, b"-----BEGIN PRIVATE KEY-----\nsecret\n").expect("write");

        // `false` is what the handler passes for `key_path`.
        let status = inspect(Some(&path), false);
        assert!(status.present && status.readable);
        assert!(
            status.size_bytes.is_some(),
            "size and mtime are still reported"
        );
        assert!(
            status.sha256.is_none(),
            "a hash of the key must not be published"
        );
    }

    #[test]
    fn a_directory_is_not_a_certificate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let status = inspect(Some(dir.path()), true);
        assert!(status.present);
        assert_eq!(status.error.as_deref(), Some("not a regular file"));
    }

    #[test]
    fn an_oversized_bundle_is_reported_as_unfingerprinted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("huge.pem");
        std::fs::write(&path, vec![b'x'; (MAX_FINGERPRINT_BYTES + 1) as usize]).expect("write");

        let status = inspect(Some(&path), true);
        assert!(status.present && status.readable);
        assert!(status.sha256.is_none());
        assert!(
            status
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("exceeds"),
            "error: {:?}",
            status.error
        );
    }

    #[test]
    fn the_fingerprint_is_lower_case_hex_of_the_expected_length() {
        let value = sha256_hex(b"abc");
        assert_eq!(value.len(), 64);
        assert_eq!(
            value,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_response_serialises_with_the_documented_keys() {
        let response = TlsStatusResponse {
            enabled: true,
            min_version: "1.2".into(),
            self_signed_fallback: false,
            use_platform_roots: true,
            allow_insecure_dev_mode: false,
            certificate: inspect(None, true),
            private_key: inspect(None, false),
            listeners: TlsListeners {
                smtps_port: 465,
                imaps_port: 993,
                https_port: 0,
                public_url: "https://mail.example.com".into(),
                public_url_is_tls: true,
            },
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["min_version"], "1.2");
        assert_eq!(json["listeners"]["smtps_port"], 465);
        assert_eq!(json["listeners"]["public_url_is_tls"], true);
        assert_eq!(json["certificate"]["present"], false);
        assert!(json["private_key"]["sha256"].is_null());
    }

    #[test]
    fn a_plain_http_public_url_is_not_tls() {
        // The rule the handler applies, asserted directly so a proxy-terminated
        // deployment is classified by the URL rather than by `tls.enabled`.
        for (url, expected) in [
            ("https://mail.example.com", true),
            ("  HTTPS://MAIL.EXAMPLE.COM", true),
            ("http://mail.example.com", false),
            ("mail.example.com", false),
        ] {
            assert_eq!(
                url.trim_start()
                    .to_ascii_lowercase()
                    .starts_with("https://"),
                expected,
                "{url}"
            );
        }
    }
}
