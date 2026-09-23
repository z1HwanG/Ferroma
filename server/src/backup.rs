//! `ferroma storage export` / `ferroma storage import` — one archive, both halves.
//!
//! A manual backup and a move to a new server are the same operation: the database
//! and the `ferroma-data` volume must be taken at the same moment, and neither half
//! alone is a backup. [`export`] writes **one** archive holding both — a PostgreSQL
//! custom-format dump, the Maildir, the attachment blobs, the `dkim/` directory,
//! `<data_dir>/database.json` and the generated `jwt_secret`, plus a
//! `manifest.json` that names every member with its size and SHA-256. [`import`]
//! verifies that manifest, refuses a target that is not empty unless `--replace`,
//! refuses a dump whose `pg_dump` major version does not match the server, and
//! hands the caller back a store that `ferroma storage verify` is expected to
//! confirm.
//!
//! The destination (`--to` / `--from`) is a local path or an `s3://` / `webdav://`
//! URL. S3 and WebDAV are destinations for **that same archive**, not a second
//! backup system: credentials come from a private file under the data directory
//! (or the environment), never from `ferroma.toml` or the archive. Transfers use rustls.
//!
//! Neither command revives the retired backup sidecar: scheduling and off-site
//! retention stay the operator's job.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use tokio::io::AsyncWriteExt;

use anyhow::{anyhow, bail, Context, Result};
use ferroma_core::config::Config;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The archive format name, checked on import so a random file is never restored.
const ARCHIVE_FORMAT: &str = "ferroma-archive";
/// Bumped only when the archive layout stops being readable by older binaries.
const ARCHIVE_FORMAT_VERSION: u32 = 1;
/// The manifest's own archive path. Always the first entry, so a truncated archive
/// is missing it and is rejected before anything is touched.
const MANIFEST_PATH: &str = "manifest.json";
/// Where the `pg_dump` output lives inside the archive.
const DUMP_PATH: &str = "postgres/dump";

// -----------------------------------------------------------------------------
// The manifest
// -----------------------------------------------------------------------------

/// `manifest.json` — the archive's table of contents and its integrity record.
///
/// Everything an import must decide is here: what made the archive, when, whether
/// it was a live copy, and the exact bytes of every member so a corrupt or partial
/// archive is rejected before it can touch the target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`ARCHIVE_FORMAT`].
    pub format: String,
    /// Always [`ARCHIVE_FORMAT_VERSION`] for archives this binary writes.
    pub format_version: u32,
    /// The Ferroma release that wrote the archive (`ferroma_core::version::VERSION`).
    pub ferroma_version: String,
    /// When the export started, RFC 3339 UTC.
    pub created_at: String,
    /// Major version of the `pg_dump` that made the dump (e.g. `16`).
    ///
    /// An import refuses to restore into a server whose major version differs,
    /// because a dump written by a newer `pg_dump` cannot be loaded by an older
    /// server and a silently mismatched pair is how a restore "succeeds" into
    /// nothing usable.
    pub pg_dump_major: u32,
    /// Whether the export ran while `ferroma serve` was up (`--live`).
    pub live: bool,
    /// File counts per archive section.
    pub counts: Counts,
    /// Every member except `manifest.json` itself: path, size, SHA-256.
    pub members: Vec<Member>,
}

/// One file inside the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    /// Archive path, forward slashes, relative to the archive root.
    pub path: String,
    pub size: u64,
    /// Lower-case hex SHA-256 of the member's bytes.
    pub sha256: String,
}

/// How many files each archive section contributed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    pub maildir_files: u64,
    pub attachment_files: u64,
    pub dkim_files: u64,
    /// `<data_dir>/database.json` plus `<data_dir>/jwt_secret`, whichever exist.
    pub data_files: u64,
}

impl Manifest {
    fn validate(&self) -> Result<()> {
        if self.format != ARCHIVE_FORMAT {
            bail!(
                "this is not a Ferroma archive (format {:?}, expected {ARCHIVE_FORMAT:?}); refusing to restore it",
                self.format
            );
        }
        if self.format_version != ARCHIVE_FORMAT_VERSION {
            bail!(
                "this archive uses format version {}, but this binary understands only {ARCHIVE_FORMAT_VERSION}; \
                 restore it with a Ferroma release from the same era",
                self.format_version
            );
        }
        if self.pg_dump_major == 0 {
            bail!("the manifest does not say which pg_dump wrote the dump; refusing to restore it");
        }
        if self.ferroma_version.trim().is_empty() {
            bail!("the manifest does not say which Ferroma version wrote the archive");
        }
        let mut seen = std::collections::HashSet::new();
        for member in &self.members {
            if member.path.trim().is_empty() {
                bail!("the manifest lists a member with no path");
            }
            if member.path == MANIFEST_PATH {
                bail!("the manifest lists {MANIFEST_PATH} as a member of itself");
            }
            if !seen.insert(member.path.as_str()) {
                bail!("the manifest lists {} twice", member.path);
            }
        }
        Ok(())
    }
}

/// One file planned for the archive, before its checksum is known.
struct Planned {
    /// Archive path, e.g. `mail/example.com/alice/Maildir/cur/…` or `postgres/dump`.
    archive_path: String,
    /// The file on disk to read.
    source: PathBuf,
}

/// SHA-256 of a file, hex lower-case, without loading it into memory.
fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Hash and measure every planned member, in archive order.
fn members_from_planned(planned: &[Planned]) -> Result<Vec<Member>> {
    planned
        .iter()
        .map(|item| {
            let size = std::fs::metadata(&item.source)
                .with_context(|| format!("stat {}", item.source.display()))?
                .len();
            let sha256 = sha256_file(&item.source)?;
            Ok(Member {
                path: item.archive_path.clone(),
                size,
                sha256,
            })
        })
        .collect()
}

/// Add every file under `root` to the plan, as `prefix/<relative>`.
///
/// Missing roots are skipped: a fresh server has no Maildir yet, and an export
/// must still produce a valid (database-only) archive.
fn collect_dir(root: &Path, prefix: &str, planned: &mut Vec<Planned>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("path outside the walked root: {}", entry.path().display()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let archive_path = format!("{prefix}/{}", relative.to_string_lossy());
        planned.push(Planned {
            archive_path,
            source: entry.path().to_path_buf(),
        });
    }
    Ok(())
}

/// Add one file to the plan when it exists.
fn collect_optional_file(
    path: &Path,
    archive_path: &str,
    planned: &mut Vec<Planned>,
) -> Result<()> {
    if path.is_file() {
        planned.push(Planned {
            archive_path: archive_path.to_string(),
            source: path.to_path_buf(),
        });
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// PostgreSQL tooling
// -----------------------------------------------------------------------------

/// Locate `pg_dump` / `pg_restore` for the PostgreSQL major version it will talk to.
///
/// An explicit `FERROMA_PG_DUMP` / `FERROMA_PG_RESTORE` is used as given. Otherwise
/// the client whose own major version equals the server's is chosen: a client older
/// than the server aborts, which is what a PostgreSQL 16 client does against a
/// PostgreSQL 18 server. The versioned directory is checked before `PATH`, because
/// `PATH` holds whichever client the image installed and that one may not match.
fn find_tool_for_server(name: &str, env_var: &str, server_major: u32) -> Result<PathBuf> {
    if std::env::var(env_var)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return find_tool(name, env_var);
    }
    let mut candidates = Vec::new();
    let versioned = PathBuf::from(format!("/usr/lib/postgresql/{server_major}/bin/{name}"));
    if versioned.is_file() {
        candidates.push(versioned);
    }
    if let Ok(found) = find_tool(name, env_var) {
        candidates.push(found);
    }
    for candidate in &candidates {
        if tool_major_version(candidate, name).ok() == Some(server_major) {
            return Ok(candidate.clone());
        }
    }
    bail!(
        "no {name} matches PostgreSQL {server_major}. The client has to be the same major \
         version as the server; an older one aborts. Install postgresql-client-{server_major}, \
         or point {env_var} at that version's binary"
    )
}

/// The server's major version, read before a dump so the matching client is chosen.
async fn server_major_of_url(db_url: &str) -> Result<u32> {
    let mut config = ferroma_core::config::DatabaseConfig::default();
    config.url = db_url.to_string();
    config.run_migrations = false;
    config.max_connections = 1;
    let db = ferroma_storage::Database::connect(&config)
        .await
        .map_err(|error| anyhow!("{error}"))?;
    server_major_version(&db).await
}

/// Locate `pg_dump` / `pg_restore`: the `FERROMA_PG_DUMP` / `FERROMA_PG_RESTORE`
/// override first, then `PATH`.
fn find_tool(name: &str, env_var: &str) -> Result<PathBuf> {
    if let Ok(value) = std::env::var(env_var) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            let path = PathBuf::from(&value);
            if path.is_file() {
                return Ok(path);
            }
            bail!(
                "{env_var} points at {}, which is not a file",
                path.display()
            );
        }
    }
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(&executable);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "{name} was not found on PATH. Install it (the runtime image ships postgresql-client-18), \
         or point {env_var} at the binary"
    )
}

/// The leading integer of `pg_dump --version` output, e.g. `18` from
/// `pg_dump (PostgreSQL) 18.6 (Debian …)`.
fn parse_major_version(output: &str) -> Option<u32> {
    output
        .split(|c: char| !c.is_ascii_digit())
        .find(|token| !token.is_empty())
        .and_then(|token| token.parse::<u32>().ok())
}

fn tool_major_version(path: &Path, name: &str) -> Result<u32> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("running {name} --version"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_major_version(&text).ok_or_else(|| {
        anyhow!(
            "could not read the version of {name} at {}: {:?}",
            path.display(),
            text.trim()
        )
    })
}

/// Run `pg_dump` against the configured database, writing a custom-format dump.
async fn run_pg_dump(pg_dump: &Path, db_url: &str, output: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    let status = tokio::process::Command::new(pg_dump)
        .arg("--format=custom")
        .arg("--compress=6")
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg("-d")
        .arg(db_url)
        .arg("-f")
        .arg(output)
        .output()
        .await
        .with_context(|| format!("running {}", pg_dump.display()))?;
    if !status.status.success() {
        bail!(
            "pg_dump failed ({}): {}",
            status.status,
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "database dumped"
    );
    Ok(())
}

/// Run `pg_restore` to load the dump into the (verified empty) target database.
async fn run_pg_restore(pg_restore: &Path, db_url: &str, dump: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    let status = tokio::process::Command::new(pg_restore)
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg("--exit-on-error")
        .arg("-d")
        .arg(db_url)
        .arg(dump)
        .output()
        .await
        .with_context(|| format!("running {}", pg_restore.display()))?;
    if !status.status.success() {
        bail!(
            "pg_restore failed ({}): {}",
            status.status,
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "database restored"
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// Destinations: local path, S3, WebDAV
// -----------------------------------------------------------------------------

/// Where an archive goes (export) or comes from (import).
#[derive(Debug, Clone)]
enum Destination {
    Local(PathBuf),
    S3 { bucket: String, key: String },
    WebDav(url::Url),
}

impl Destination {
    /// Parse `--to` / `--from`: a plain path, an `s3://bucket/key`, or a
    /// `webdav://` / `webdavs://` URL. `https://` and `http://` are accepted as
    /// WebDAV too — many WebDAV servers are only reachable through a reverse proxy
    /// that does not speak `webdav://`.
    fn parse(raw: &str) -> Result<Destination> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("the destination must not be empty");
        }
        if let Some(rest) = raw.strip_prefix("s3://") {
            let (bucket, key) = rest
                .split_once('/')
                .ok_or_else(|| anyhow!("an s3:// URL must name a key: s3://bucket/key"))?;
            let bucket = bucket.trim();
            let key = key.trim();
            if bucket.is_empty() || key.is_empty() {
                bail!("an s3:// URL must look like s3://bucket/key");
            }
            return Ok(Destination::S3 {
                bucket: bucket.to_string(),
                key: key.to_string(),
            });
        }
        if raw.starts_with("webdav://") || raw.starts_with("webdavs://") {
            // Normalise to a scheme reqwest understands.
            let url_text = if let Some(rest) = raw.strip_prefix("webdavs://") {
                format!("https://{rest}")
            } else {
                format!("https://{}", raw.trim_start_matches("webdav://"))
            };
            let url = url::Url::parse(&url_text).context("parsing the webdav:// URL")?;
            if url.host_str().is_none() {
                bail!("a webdav:// URL must name a host");
            }
            return Ok(Destination::WebDav(url));
        }
        if raw.starts_with("http://") || raw.starts_with("https://") {
            let url = url::Url::parse(raw).context("parsing the URL")?;
            if url.host_str().is_none() {
                bail!("the URL must name a host");
            }
            return Ok(Destination::WebDav(url));
        }
        if raw.contains("://") {
            bail!(
                "unsupported destination scheme in {raw:?}: use a local path, s3://bucket/key, \
                 or a webdav:// URL"
            );
        }
        Ok(Destination::Local(PathBuf::from(raw)))
    }

    /// The destination as the operator should read it back: no embedded password.
    fn display(&self) -> String {
        match self {
            Destination::Local(path) => path.display().to_string(),
            Destination::S3 { bucket, key } => format!("s3://{bucket}/{key}"),
            Destination::WebDav(url) => {
                let mut redacted = url.clone();
                let _ = redacted.set_username("");
                let _ = redacted.set_password(None);
                redacted.to_string()
            }
        }
    }
}

/// Transfer credentials read at use time from the environment and the private
/// `<data_dir>/transfer_credentials.json` file. Never included in the archive.
struct TransferCredentials {
    access_key: Option<String>,
    secret_key: Option<String>,
    session_token: Option<String>,
    region: String,
    /// A self-hosted S3 endpoint, such as `https://s3.example.com`. Empty means AWS.
    endpoint: Option<String>,
    webdav_username: Option<String>,
    webdav_password: Option<String>,
}

impl TransferCredentials {
    /// Load optional server-side settings; an absent file falls back to the environment.
    fn from_config(config: &Config) -> Result<Self> {
        let path = config.server.data_dir.join("transfer_credentials.json");
        let stored = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid transfer settings in {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self::from_sources(&stored))
    }

    /// Environment first, then a value the console stored.
    fn from_sources(stored: &serde_json::Value) -> Self {
        let mut creds = Self::from_env();
        let text = |key: &str| {
            stored
                .get(key)
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        if let Some(value) = text("access_key") {
            creds.access_key = Some(value);
        }
        if let Some(value) = text("secret_key") {
            creds.secret_key = Some(value);
        }
        if let Some(value) = text("region") {
            creds.region = value;
        }
        if let Some(value) = text("endpoint") {
            creds.endpoint = Some(value.trim_end_matches('/').to_string());
        }
        if let Some(value) = text("webdav_username") {
            creds.webdav_username = Some(value);
        }
        if let Some(value) = text("webdav_password") {
            creds.webdav_password = Some(value);
        }
        creds
    }

    fn from_env() -> Self {
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        TransferCredentials {
            access_key: std::env::var("AWS_ACCESS_KEY_ID").ok(),
            secret_key: std::env::var("AWS_SECRET_ACCESS_KEY").ok(),
            session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
            region,
            endpoint: std::env::var("AWS_S3_ENDPOINT")
                .ok()
                .map(|value| value.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty()),
            webdav_username: std::env::var("WEBDAV_USERNAME")
                .or_else(|_| std::env::var("WEBDAV_USER"))
                .ok(),
            webdav_password: std::env::var("WEBDAV_PASSWORD").ok(),
        }
    }

    /// A copy aimed at another region.
    ///
    /// S3 answers a request to the wrong region with `301` and names the right host.
    /// The signature covers the region, so the retry has to be signed again.
    fn in_region(&self, region: &str) -> Self {
        TransferCredentials {
            access_key: self.access_key.clone(),
            secret_key: self.secret_key.clone(),
            session_token: self.session_token.clone(),
            region: region.to_string(),
            endpoint: self.endpoint.clone(),
            webdav_username: self.webdav_username.clone(),
            webdav_password: self.webdav_password.clone(),
        }
    }

    fn s3_required(&self, purpose: &str) -> Result<()> {
        if self.access_key.as_deref().is_none_or(str::is_empty) {
            bail!(
                "AWS_ACCESS_KEY_ID is not set; {purpose} needs it (credentials come from the \
                 environment, never from ferroma.toml or the archive)"
            );
        }
        if self.secret_key.as_deref().is_none_or(str::is_empty) {
            bail!("AWS_SECRET_ACCESS_KEY is not set; {purpose} needs it");
        }
        Ok(())
    }
}

/// The empty-string SHA-256, used as the payload hash of unsigned requests (GET).
fn empty_sha256() -> String {
    hex::encode(Sha256::digest([]))
}

/// Percent-encode one path segment per RFC 3986, the way AWS SigV4 canonical URIs
/// require: everything except unreserved characters is encoded, `/` is kept.
fn encode_path_segment(segment: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if UNRESERVED.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Percent-encode an S3 object key for a canonical URI: each `/`-separated segment
/// is encoded, the separators stay.
fn encode_s3_key(key: &str) -> String {
    key.split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// The AWS Signature Version 4 signing key chain.
fn s3_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;
    fn step(key: &[u8], value: &str) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(value.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
    let date = step(format!("AWS4{secret}").as_bytes(), date_stamp);
    let region = step(&date, region);
    let service = step(&region, service);
    step(&service, "aws4_request")
}

/// The hex signature for one SigV4 request.
fn s3_signature(
    secret: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
    string_to_sign: &str,
) -> String {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;
    let key = s3_signing_key(secret, date_stamp, region, service);
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(string_to_sign.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// The fields of one SigV4 request that are not the headers or the payload hash.
struct S3AuthRequest<'a> {
    method: &'a str,
    canonical_uri: &'a str,
    canonical_query: &'a str,
    amz_date: &'a str,
    region: &'a str,
    access_key: &'a str,
    secret: &'a str,
}

/// Build the canonical request, string to sign and Authorization header for one
/// SigV4 request. Exposed for the AWS test vector in the unit tests.
fn s3_authorization(
    request: &S3AuthRequest<'_>,
    headers: &[(&str, &str)],
    payload_sha256: &str,
) -> String {
    let S3AuthRequest {
        method,
        canonical_uri,
        canonical_query,
        amz_date,
        region,
        access_key,
        secret,
    } = *request;
    let date_stamp = &amz_date[..8];
    // Headers sorted by name, lower-cased, exactly as they will be sent.
    let mut sorted: Vec<(&str, &str)> = headers.to_vec();
    sorted.sort_by_key(|(name, _)| name.to_ascii_lowercase());
    let mut canonical_headers = String::new();
    let mut signed_headers = Vec::new();
    for (name, value) in &sorted {
        let name = name.to_ascii_lowercase();
        // Host and x-amz-* values are lower-cased by the protocol; everything else
        // is trimmed once (we only sign our own headers, so this is safe).
        let value = value.trim();
        canonical_headers.push_str(&format!("{name}:{value}\n"));
        signed_headers.push(name);
    }
    let signed_headers = signed_headers.join(";");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}"
    );
    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let signature = s3_signature(secret, date_stamp, region, "s3", &string_to_sign);
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope},SignedHeaders={signed_headers},Signature={signature}"
    )
}

/// The URL, the signed headers and the Authorization value of one S3 request.
type SignedS3Request = (String, Vec<(&'static str, String)>, String);

/// Build the signed S3 request for one archive transfer (PUT or GET), returning
/// the URL, the canonical headers (minus `host`) and the Authorization header.
fn s3_signed_request(
    method: &str,
    bucket: &str,
    key: &str,
    creds: &TransferCredentials,
    payload_sha256: &str,
    amz_date: &str,
) -> Result<SignedS3Request> {
    // A self-hosted endpoint is addressed as `https://host/bucket/key`. AWS keeps
    // the virtual-host form, because that is what its signature expects.
    let (host, canonical_uri, url) = if let Some(endpoint) = creds.endpoint.as_deref() {
        let endpoint = endpoint.trim_end_matches('/');
        let parsed = url::Url::parse(endpoint).context("invalid S3 endpoint URL")?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            bail!("S3 endpoint must be an HTTPS origin without path, credentials or query");
        }
        let host = parsed.host_str().unwrap_or_default();
        let host = match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        let canonical_uri = format!("/{bucket}/{}", encode_s3_key(key));
        let url = format!("{endpoint}{canonical_uri}");
        (host, canonical_uri, url)
    } else {
        let host = format!("{bucket}.s3.{}.amazonaws.com", creds.region);
        let canonical_uri = format!("/{}", encode_s3_key(key));
        let url = format!("https://{host}{canonical_uri}");
        (host, canonical_uri, url)
    };
    let mut headers: Vec<(&'static str, String)> = vec![
        ("host", host.clone()),
        ("x-amz-content-sha256", payload_sha256.to_string()),
        ("x-amz-date", amz_date.to_string()),
    ];
    if let Some(token) = creds.session_token.as_deref() {
        if !token.is_empty() {
            headers.push(("x-amz-security-token", token.to_string()));
        }
    }
    let header_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let authorization = s3_authorization(
        &S3AuthRequest {
            method,
            canonical_uri: &canonical_uri,
            canonical_query: "",
            amz_date,
            region: &creds.region,
            access_key: creds.access_key.as_deref().expect("checked by s3_required"),
            secret: creds.secret_key.as_deref().expect("checked by s3_required"),
        },
        &header_refs,
        payload_sha256,
    );
    Ok((url, headers, authorization))
}

fn amz_date_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Stream a file as a request body.
async fn body_from_file(path: &Path) -> Result<(reqwest::Body, u64)> {
    let size = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let stream = tokio_util::io::ReaderStream::new(file);
    Ok((reqwest::Body::wrap_stream(stream), size))
}

fn reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("ferroma/{}", ferroma_core::version::VERSION))
        // S3 signatures include the host. Never follow a redirect with a signature
        // for the old host; the S3 branch reads the 301 and signs the new request.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the HTTP client")
}

/// Send the local archive at `file` to its destination.
async fn upload_archive(
    destination: &Destination,
    file: &Path,
    creds: &TransferCredentials,
) -> Result<()> {
    match destination {
        Destination::S3 { bucket, key } => {
            let creds = creds.in_region(&creds.region);
            creds.s3_required("an s3:// export")?;
            let payload_sha = sha256_file(file)?;
            let mut creds = creds;
            for _attempt in 0..2 {
                let amz_date = amz_date_now();
                let (url, headers, authorization) =
                    s3_signed_request("PUT", bucket, key, &creds, &payload_sha, &amz_date)?;
                let (body, size) = body_from_file(file).await?;
                let mut request = reqwest_client()?
                    .put(&url)
                    .header(reqwest::header::CONTENT_LENGTH, size)
                    .header(reqwest::header::AUTHORIZATION, authorization)
                    .body(body);
                for (name, value) in &headers {
                    if *name != "host" {
                        request = request.header(*name, value);
                    }
                }
                let response = request.send().await.context("uploading to S3")?;
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if let Some(region) = s3_redirect_region(status, &body) {
                    if region != creds.region {
                        creds = creds.in_region(&region);
                        continue;
                    }
                }
                if !status.is_success() {
                    bail!("PUT {url} failed: {status}: {}", body.trim());
                }
                return Ok(());
            }
            bail!("S3 kept redirecting {bucket}; set AWS_REGION to the bucket's region")
        }
        Destination::WebDav(url) => {
            let (username, password) = webdav_credentials(url, creds);
            let (body, size) = body_from_file(file).await?;
            let mut request = reqwest_client()?
                .put(url.clone())
                .header(reqwest::header::CONTENT_LENGTH, size)
                .body(body);
            if let Some((user, pass)) = username.zip(password) {
                request = request.basic_auth(user, Some(pass));
            }
            let response = request.send().await.context("uploading to WebDAV")?;
            check_response("PUT", url.as_str(), response).await?;
            Ok(())
        }
        Destination::Local(_) => unreachable!("local archives are moved, not uploaded"),
    }
}

fn webdav_credentials(
    url: &url::Url,
    creds: &TransferCredentials,
) -> (Option<String>, Option<String>) {
    if creds.webdav_username.is_some() || creds.webdav_password.is_some() {
        (creds.webdav_username.clone(), creds.webdav_password.clone())
    } else if !url.username().is_empty() || url.password().is_some() {
        (
            Some(url.username().to_string()),
            url.password().map(str::to_string),
        )
    } else {
        (None, None)
    }
}

/// The region named by an S3 permanent redirect.
///
/// `backup.s3-ap-northeast-1.amazonaws.com` and
/// `backup.s3.ap-northeast-1.amazonaws.com` are the two forms S3 uses. A request
/// signed for `us-east-1` against a bucket in another region gets exactly this.
fn s3_redirect_region(status: reqwest::StatusCode, body: &str) -> Option<String> {
    if status.as_u16() != 301 && status.as_u16() != 307 {
        return None;
    }
    let host = body
        .split_once("<Endpoint>")
        .and_then(|(_, rest)| rest.split_once("</Endpoint>"))
        .map(|(host, _)| host.trim())?;
    let host = host.strip_prefix("https://").unwrap_or(host);
    let rest = host.split_once(".s3")?;
    let rest = rest.1.trim_start_matches(['.', '-']);
    let region = rest.split(".amazonaws.com").next()?.trim();
    if region.is_empty() {
        None
    } else {
        Some(region.to_string())
    }
}

/// Write a successful response body to `into`.
async fn save_body(response: reqwest::Response, into: &Path) -> Result<()> {
    let mut out = tokio::fs::File::create(into)
        .await
        .with_context(|| format!("creating {}", into.display()))?;
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the download")?;
        out.write_all(&chunk)
            .await
            .context("writing the download")?;
    }
    out.flush().await.context("flushing the download")?;
    Ok(())
}

async fn check_response(verb: &str, url: &str, response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    bail!(
        "{verb} {url} failed: {status}{}",
        if body.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", body.trim())
        }
    );
}

/// Fetch an archive from its source into `into` (a file path).
async fn download_archive(
    destination: &Destination,
    into: &Path,
    creds: &TransferCredentials,
) -> Result<()> {
    match destination {
        Destination::Local(source) => {
            if !source.is_file() {
                bail!("no such archive: {}", source.display());
            }
            tokio::fs::copy(source, into)
                .await
                .with_context(|| format!("reading {}", source.display()))?;
            Ok(())
        }
        Destination::S3 { bucket, key } => {
            creds.s3_required("an s3:// import")?;
            let mut creds = creds.in_region(&creds.region);
            for _attempt in 0..2 {
                let amz_date = amz_date_now();
                let (url, headers, authorization) =
                    s3_signed_request("GET", bucket, key, &creds, &empty_sha256(), &amz_date)?;
                let mut request = reqwest_client()?
                    .get(&url)
                    .header(reqwest::header::AUTHORIZATION, authorization);
                for (name, value) in &headers {
                    if *name != "host" {
                        request = request.header(*name, value);
                    }
                }
                let response = request.send().await.context("downloading from S3")?;
                let status = response.status();
                if status.is_success() {
                    return save_body(response, into).await;
                }
                let body = response.text().await.unwrap_or_default();
                if let Some(region) = s3_redirect_region(status, &body) {
                    if region != creds.region {
                        creds = creds.in_region(&region);
                        continue;
                    }
                }
                bail!("GET {url} failed: {status}: {}", body.trim());
            }
            bail!("S3 kept redirecting {bucket}; set AWS_REGION to the bucket's region")
        }
        Destination::WebDav(url) => {
            let (username, password) = webdav_credentials(url, creds);
            let mut request = reqwest_client()?.get(url.clone());
            if let Some((user, pass)) = username.zip(password) {
                request = request.basic_auth(user, Some(pass));
            }
            let response = request.send().await.context("downloading from WebDAV")?;
            save_response("GET", url.as_str(), response, into).await
        }
    }
}

async fn save_response(
    verb: &str,
    url: &str,
    response: reqwest::Response,
    into: &Path,
) -> Result<()> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "{verb} {url} failed: {status}{}",
            if body.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", body.trim())
            }
        );
    }
    let mut out = tokio::fs::File::create(into)
        .await
        .with_context(|| format!("creating {}", into.display()))?;
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the download")?;
        out.write_all(&chunk)
            .await
            .context("writing the download")?;
    }
    out.flush().await.context("flushing the download")?;
    Ok(())
}

// -----------------------------------------------------------------------------
// The archive
// -----------------------------------------------------------------------------

/// Write `manifest.json` and every member into `writer` as one tar archive.
fn build_archive<W: Write>(writer: &mut W, manifest: &Manifest, planned: &[Planned]) -> Result<()> {
    let mut builder = tar::Builder::new(writer);
    {
        let manifest_json = serde_json::to_vec_pretty(manifest)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_json.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, MANIFEST_PATH, manifest_json.as_slice())
            .context("writing manifest.json")?;
    }
    for item in planned {
        let mut file = std::fs::File::open(&item.source)
            .with_context(|| format!("reading {}", item.source.display()))?;
        builder
            .append_file(&item.archive_path, &mut file)
            .with_context(|| format!("archiving {}", item.archive_path))?;
    }
    builder.finish().context("finishing the archive")?;
    Ok(())
}

/// Read and parse the manifest from an archive.
fn read_manifest(archive_path: &Path) -> Result<Manifest> {
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("opening {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(file);
    let entries = archive.entries().context("reading the archive")?;
    for entry in entries {
        let mut entry = entry.context("reading an archive entry")?;
        let path = entry.path().context("reading an entry path")?.into_owned();
        if path != Path::new(MANIFEST_PATH) {
            continue;
        }
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .context("reading manifest.json")?;
        let manifest: Manifest = serde_json::from_str(&text).context("parsing manifest.json")?;
        manifest.validate()?;
        return Ok(manifest);
    }
    bail!("the archive has no {MANIFEST_PATH}; it is not a Ferroma archive (or it is truncated)")
}

/// Extract every member into `staging`, verifying size and SHA-256 against the
/// manifest as it goes. A single mismatch aborts before anything is restored.
fn extract_and_verify(archive_path: &Path, manifest: &Manifest, staging: &Path) -> Result<()> {
    let wanted: HashMap<&str, &Member> = manifest
        .members
        .iter()
        .map(|member| (member.path.as_str(), member))
        .collect();
    let mut seen = std::collections::HashSet::new();

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("opening {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(file);
    let entries = archive.entries().context("reading the archive")?;
    for entry in entries {
        let mut entry = entry.context("reading an archive entry")?;
        let path = entry.path().context("reading an entry path")?.into_owned();
        let path = path.to_string_lossy().into_owned();
        if path == MANIFEST_PATH {
            continue;
        }
        let member = wanted.get(path.as_str()).ok_or_else(|| {
            anyhow!("the archive contains {path:?}, which the manifest does not list; refusing to restore")
        })?;
        let header_size = entry.header().size().context("reading an entry size")?;
        if header_size != member.size {
            bail!(
                "{} is {} bytes in the archive but {} in the manifest; the archive is corrupt",
                path,
                header_size,
                member.size
            );
        }
        let destination = staging.join(&path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&destination)
            .with_context(|| format!("extracting {}", destination.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let read = entry.read(&mut buffer).context("reading the archive")?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            out.write_all(&buffer[..read])
                .with_context(|| format!("writing {}", destination.display()))?;
        }
        let digest = hex::encode(hasher.finalize());
        if digest != member.sha256 {
            bail!(
                "checksum mismatch for {path}: the archive holds {digest}, the manifest says {}; \
                 the archive is corrupt",
                member.sha256
            );
        }
        seen.insert(path);
    }

    for member in &manifest.members {
        if !seen.contains(member.path.as_str()) {
            bail!(
                "the archive is missing {} which the manifest lists; refusing to restore",
                member.path
            );
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// "Is the server up?"
// -----------------------------------------------------------------------------

/// Whether `ferroma serve` looks like it is running: any enabled listener port
/// that answers a TCP connect. Returns a description of the first hit.
///
/// A connect to a bound port is the honest signal — the health endpoint only
/// exists when the API is enabled, and an IMAP-only or SMTP-only process still
/// writes underneath an export.
async fn serve_is_up(config: &Config) -> Option<String> {
    let mut addresses: Vec<(&'static str, String)> = Vec::new();
    if config.smtp.enabled {
        addresses.push(("smtp", format!("{}:{}", config.smtp.host, config.smtp.port)));
        addresses.push((
            "submission",
            format!("{}:{}", config.smtp.host, config.smtp.submission_port),
        ));
        if config.smtp.smtps_port != 0 {
            addresses.push((
                "smtps",
                format!("{}:{}", config.smtp.host, config.smtp.smtps_port),
            ));
        }
    }
    if config.imap.enabled {
        addresses.push(("imap", format!("{}:{}", config.imap.host, config.imap.port)));
        if config.imap.imaps_port != 0 {
            addresses.push((
                "imaps",
                format!("{}:{}", config.imap.host, config.imap.imaps_port),
            ));
        }
    }
    if config.api.enabled {
        addresses.push(("http", format!("{}:{}", config.api.host, config.api.port)));
        if config.api.tls_port != 0 {
            addresses.push((
                "https",
                format!("{}:{}", config.api.host, config.api.tls_port),
            ));
        }
    }

    for (label, address) in addresses {
        let Ok(socket) = address.parse::<std::net::SocketAddr>() else {
            continue;
        };
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(400),
            tokio::net::TcpStream::connect(socket),
        )
        .await;
        if matches!(probe, Ok(Ok(_))) {
            return Some(format!("the {label} listener on {address}"));
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Export
// -----------------------------------------------------------------------------

/// What one export produced, for the caller to print.
#[derive(Debug)]
pub struct ExportReport {
    pub destination: String,
    pub ferroma_version: String,
    pub created_at: String,
    pub pg_dump_major: u32,
    pub live: bool,
    pub members: usize,
    pub bytes: u64,
    pub counts: Counts,
}

/// `ferroma storage export --to <path|s3://…|webdav://…> [--live]`
pub async fn export(config: &Config, to: &str, live: bool) -> Result<ExportReport> {
    let destination = Destination::parse(to)?;

    if !live {
        if let Some(what) = serve_is_up(config).await {
            bail!(
                "{what} is listening, which looks like a running `ferroma serve`. \
                 Stop it first for a consistent pair, or pass --live to export a live copy \
                 (the archive is then labelled live in its manifest, and may miss a delivery \
                 in progress)."
            );
        }
    }

    let db_url = effective_database_url(config)?;
    let server_major = server_major_of_url(&db_url).await?;
    let pg_dump = find_tool_for_server("pg_dump", "FERROMA_PG_DUMP", server_major)?;
    let pg_dump_major = tool_major_version(&pg_dump, "pg_dump")?;

    let work = tempfile::tempdir().context("creating a temporary directory")?;
    let dump = work.path().join("ferroma.dump");
    run_pg_dump(&pg_dump, &db_url, &dump).await?;

    let mut planned: Vec<Planned> = Vec::new();
    collect_dir(&config.maildir_root(), "mail", &mut planned)?;
    collect_dir(&config.attachment_root(), "attachments", &mut planned)?;
    let dkim_dir = config.server.data_dir.join("dkim");
    if dkim_dir.is_dir() {
        collect_dir(&dkim_dir, "dkim", &mut planned)?;
    }
    collect_optional_file(
        &config.server.data_dir.join("database.json"),
        "database.json",
        &mut planned,
    )?;
    collect_optional_file(
        &config.server.data_dir.join("jwt_secret"),
        "jwt_secret",
        &mut planned,
    )?;
    planned.push(Planned {
        archive_path: DUMP_PATH.to_string(),
        source: dump.clone(),
    });

    let members = members_from_planned(&planned)?;
    let mut counts = Counts::default();
    for member in &members {
        // The dump is the database half, not a data-directory file. It is listed
        // as a member (and checksummed) but not counted with the volume files.
        match member.path.split('/').next() {
            Some("mail") => counts.maildir_files += 1,
            Some("attachments") => counts.attachment_files += 1,
            Some("dkim") => counts.dkim_files += 1,
            // The dump is the database half, not a data-directory file.
            Some("postgres") => {}
            _ => counts.data_files += 1,
        }
    }

    let manifest = Manifest {
        format: ARCHIVE_FORMAT.to_string(),
        format_version: ARCHIVE_FORMAT_VERSION,
        ferroma_version: ferroma_core::version::VERSION.to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        pg_dump_major,
        live,
        counts: counts.clone(),
        members: members.clone(),
    };

    // Write the archive to a temporary file next to where it will live (or in the
    // system temp for uploads), then move or upload it.
    let archive_holder = match &destination {
        Destination::Local(path) => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            let holder = tempfile::Builder::new()
                .prefix(".ferroma-export-")
                .tempfile_in(parent)
                .context("creating a temporary archive file")?;
            Some(holder)
        }
        _ => None,
    };
    let archive_path = match &archive_holder {
        Some(holder) => holder.path().to_path_buf(),
        None => work.path().join("ferroma-archive.tar"),
    };

    {
        let file = std::fs::File::create(&archive_path)
            .with_context(|| format!("creating {}", archive_path.display()))?;
        let mut file = std::io::BufWriter::new(file);
        build_archive(&mut file, &manifest, &planned)?;
        file.flush().context("flushing the archive")?;
        file.get_ref().sync_all().context("syncing the archive")?;
    }

    let bytes = std::fs::metadata(&archive_path)?.len();
    let created_at = manifest.created_at.clone();
    let ferroma_version = manifest.ferroma_version.clone();

    match &destination {
        Destination::Local(path) => {
            if let Some(holder) = archive_holder {
                holder
                    .persist(path)
                    .map_err(|error| anyhow!("writing {}: {}", path.display(), error.error))?;
            } else {
                std::fs::copy(&archive_path, path)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
        _ => {
            upload_archive(
                &destination,
                &archive_path,
                &TransferCredentials::from_config(config)?,
            )
            .await?
        }
    }

    Ok(ExportReport {
        destination: destination.display(),
        ferroma_version,
        created_at,
        pg_dump_major,
        live,
        members: members.len(),
        bytes,
        counts,
    })
}

// -----------------------------------------------------------------------------
// Import
// -----------------------------------------------------------------------------

/// `ferroma storage import --from <path|s3://…|webdav://…> [--replace]`
///
/// Restores into an empty database and an empty data directory; refuses either
/// that is not empty unless `--replace` (restoring over an existing store is a
/// merge, which is how a week of mail gets lost). The server must be stopped.
pub async fn import(config: &Config, from: &str, replace: bool) -> Result<()> {
    if let Some(what) = serve_is_up(config).await {
        bail!(
            "{what} is listening, which looks like a running `ferroma serve`. \
             `ferroma storage import` needs the server stopped: a restore writes both halves \
             underneath it, and a server pointed at a half-restored store is worse than a \
             stopped one. Stop it first."
        );
    }

    let source = Destination::parse(from)?;
    let work = tempfile::tempdir().context("creating a temporary directory")?;
    let archive_path = work.path().join("ferroma-archive.tar");
    println!("reading {}…", source.display());
    download_archive(
        &source,
        &archive_path,
        &TransferCredentials::from_config(config)?,
    )
    .await?;

    let manifest = read_manifest(&archive_path)?;
    println!(
        "  archive    Ferroma {}, pg_dump {}, created {}",
        manifest.ferroma_version, manifest.pg_dump_major, manifest.created_at
    );
    if manifest.live {
        println!("  note       this archive was exported while the server was live; it may be missing a delivery in progress");
    }
    println!(
        "  members    {} file(s), {}",
        manifest.members.len(),
        human_bytes_total(&manifest.members)
    );

    // Every byte of the archive is proven good before the target is touched.
    let staging = work.path().join("stage");
    extract_and_verify(&archive_path, &manifest, &staging)?;

    let db_url = effective_database_url(config)?;
    let database_name = ferroma_storage::Database::database_name(&db_url)?;
    let created = create_database_if_missing(&db_url).await?;
    if created {
        println!("created database {database_name}");
    }

    // Connect with the *effective* URL — the stated one, or the one the setup page
    // remembered — so the emptiness check and the version check see the database
    // that will actually be restored. Migrations stay off: a freshly created
    // database is empty, and applying the schema here would make the emptiness
    // check refuse the only restore that does not need `--replace`.
    let mut db_config = config.database.clone();
    db_config.url = db_url.clone();
    db_config.run_migrations = false;
    let db = ferroma_storage::Database::connect(&db_config)
        .await
        .map_err(|error| anyhow!("{error}"))?;

    let server_major = server_major_version(&db).await?;
    if manifest.pg_dump_major != server_major {
        bail!(
            "the archive was made with pg_dump {0}, but this server runs PostgreSQL {1}; \
             refusing to restore it. {0} cannot be loaded by a {1} server; dump the source \
             with a client whose major version matches the target.",
            manifest.pg_dump_major,
            server_major
        );
    }

    match database_state(&db).await? {
        DatabaseState::HasMail if !replace => {
            bail!(
                "the target database {database_name} is not empty (it already has accounts). \
                 Restoring over an existing database is a merge, which is how a week of mail \
                 gets lost. Empty it, or pass --replace to replace what is there."
            );
        }
        DatabaseState::HasMail => {
            println!("replacing the existing schema in {database_name}");
            wipe_schema(&db).await?;
        }
        // The schema is there and unused — from `database init`, or because the
        // cluster's template already carries it. `pg_restore` would fail on every
        // `CREATE TABLE`, so the empty schema is dropped either way. There is no
        // mail to lose, which is why this does not need `--replace`.
        DatabaseState::SchemaOnly => wipe_schema(&db).await?,
        DatabaseState::Empty => {}
    }

    let pg_restore =
        find_tool_for_server("pg_restore", "FERROMA_PG_RESTORE", manifest.pg_dump_major)?;
    run_pg_restore(&pg_restore, &db_url, &staging.join(DUMP_PATH)).await?;
    db.close().await;
    println!("database    restored");

    // The data directory(s) are cleared only once the database is proven loaded.
    check_and_clear_data_dirs(config, replace)?;
    install_staging(&staging, config)?;
    println!("data        extracted");

    Ok(())
}

/// Total bytes across all manifest members, for the summary line.
fn human_bytes_total(members: &[Member]) -> String {
    let total: u64 = members.iter().map(|member| member.size).sum();
    human_bytes(total)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The database address to dump or restore: the stated one, or the one the setup
/// page remembered in `<data_dir>/database.json`.
fn effective_database_url(config: &Config) -> Result<String> {
    let default = ferroma_core::config::DatabaseConfig::default();
    if config.database.url.trim() != default.url.trim() {
        return Ok(config.database.url.trim().to_string());
    }
    if let Some(url) = crate::bootstrap::stored_url(&config.server.data_dir) {
        if !url.trim().is_empty() {
            return Ok(url);
        }
    }
    bail!(
        "no database is configured: set database.url (or FERROMA_DATABASE__URL), or open the \
         setup page once so the address is remembered in <data_dir>/database.json"
    )
}

/// Create the database when it does not exist, like `ferroma migrate` does — a
/// bare PostgreSQL needs no `psql` step before the first restore either.
async fn create_database_if_missing(db_url: &str) -> Result<bool> {
    match ferroma_storage::Database::ensure_database_exists(db_url).await {
        Ok(exists) => Ok(!exists),
        Err(err) => Err(anyhow!("{err}")),
    }
}

/// The target server's PostgreSQL major version.
async fn server_major_version(db: &ferroma_storage::Database) -> Result<u32> {
    let version = db
        .server_version()
        .await
        .map_err(|error| anyhow!("{error}"))?;
    parse_major_version(&version)
        .ok_or_else(|| anyhow!("cannot read the server version: {version}"))
}

/// What an import would be writing over.
enum DatabaseState {
    /// No Ferroma schema at all.
    Empty,
    /// The schema is there and `users` has no rows: nothing to merge with.
    SchemaOnly,
    /// Accounts exist. Restoring over this is a merge.
    HasMail,
}

/// "Empty" means there is nothing an import would merge with. A database that has
/// never been migrated has no `users` table, and a database that was migrated but
/// never used — `ferroma database init`, or a cluster whose `template1` already
/// carries the schema — has the table and no rows. Either is a safe target.
/// A database with accounts is not.
async fn database_state(db: &ferroma_storage::Database) -> Result<DatabaseState> {
    let exists: (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .map_err(|error| anyhow!("{error}"))?;
    if !exists.0 {
        return Ok(DatabaseState::Empty);
    }
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .map_err(|error| anyhow!("{error}"))?;
    Ok(if count > 0 {
        DatabaseState::HasMail
    } else {
        DatabaseState::SchemaOnly
    })
}

/// Empty the `public` schema so the restore starts from nothing.
async fn wipe_schema(db: &ferroma_storage::Database) -> Result<()> {
    sqlx::query("DROP SCHEMA public CASCADE")
        .execute(db.pool())
        .await
        .map_err(|error| anyhow!("{error}"))?;
    sqlx::query("CREATE SCHEMA public")
        .execute(db.pool())
        .await
        .map_err(|error| anyhow!("{error}"))?;
    Ok(())
}

/// The distinct roots an import writes to: the data directory plus whatever
/// `storage.maildir_root` / `storage.attachment_root` point at.
fn import_roots(config: &Config) -> Vec<PathBuf> {
    let mut roots = vec![config.server.data_dir.clone()];
    for extra in [config.maildir_root(), config.attachment_root()] {
        // A root that lives inside one we already clear is wiped with it.
        // `starts_with` compares whole components, so `/srv/ferroma2` is not
        // treated as inside `/srv/ferroma`.
        let covered = roots
            .iter()
            .any(|root| extra == *root || extra.starts_with(root));
        if !covered {
            roots.push(extra);
        }
    }
    roots
}

/// Refuse a non-empty data directory (or Maildir/blob root) unless `--replace`,
/// and clear it when replacing. `data_dir` is cleared last so nested roots are
/// never double-wiped by an order change.
fn check_and_clear_data_dirs(config: &Config, replace: bool) -> Result<()> {
    let roots = import_roots(config);
    let mut empty_ok = true;
    let mut description = Vec::new();
    for root in &roots {
        if dir_is_non_empty(root) {
            empty_ok = false;
            description.push(root.display().to_string());
        }
    }
    if !empty_ok {
        if !replace {
            bail!(
                "the data directory is not empty: {}. A restore must start from an empty \
                 data directory — restoring over existing files is a merge, which is how a \
                 week of mail gets lost. Empty it, or pass --replace to replace what is there.",
                description.join(", ")
            );
        }
        println!("replacing the existing data directory");
        // Attachment and Maildir roots first (they may live inside data_dir), data_dir last.
        for root in roots.iter().rev() {
            clear_dir(root)?;
        }
    }
    Ok(())
}

fn dir_is_non_empty(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

fn clear_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("clearing {}", dir.display()));
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("recreating {}", dir.display()))?;
    Ok(())
}

/// Move the verified staging tree into its final places.
fn install_staging(staging: &Path, config: &Config) -> Result<()> {
    let data_dir = config.server.data_dir.clone();
    move_tree(&staging.join("mail"), &config.maildir_root())?;
    move_tree(&staging.join("attachments"), &config.attachment_root())?;
    let dkim_from = staging.join("dkim");
    if dkim_from.is_dir() {
        move_tree(&dkim_from, &data_dir.join("dkim"))?;
    }
    for name in ["database.json", "jwt_secret"] {
        let from = staging.join(name);
        if from.is_file() {
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("creating {}", data_dir.display()))?;
            copy_tree(&from, &data_dir.join(name))?;
        }
    }
    Ok(())
}

/// Copy a file or a whole tree from `from` to `to`, creating parents.
///
/// A rename would be faster but fails across filesystems (system temp to a data
/// volume), and a restore that halves the copy is worse than a restore that is
/// merely not instant.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if to.exists() {
            std::fs::remove_dir_all(to).with_context(|| format!("clearing {}", to.display()))?;
        }
        std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
        for entry in walkdir::WalkDir::new(from).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", from.display()))?;
            let relative = entry.path().strip_prefix(from).with_context(|| {
                format!("path outside the walked root: {}", entry.path().display())
            })?;
            let destination = to.join(relative);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&destination)
                    .with_context(|| format!("creating {}", destination.display()))?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::copy(entry.path(), &destination).with_context(|| {
                    format!(
                        "copying {} to {}",
                        entry.path().display(),
                        destination.display()
                    )
                })?;
            }
        }
        Ok(())
    } else if from.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::copy(from, to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        Ok(())
    } else {
        Ok(())
    }
}

fn move_tree(from: &Path, to: &Path) -> Result<()> {
    copy_tree(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_archive_format_parses_and_validates() {
        let manifest = Manifest {
            format: ARCHIVE_FORMAT.to_string(),
            format_version: ARCHIVE_FORMAT_VERSION,
            ferroma_version: "0.1.8".to_string(),
            created_at: "2026-09-22T00:00:00Z".to_string(),
            pg_dump_major: 16,
            live: false,
            counts: Counts::default(),
            members: vec![Member {
                path: "postgres/dump".to_string(),
                size: 3,
                sha256: "abc".to_string(),
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let round: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(round.pg_dump_major, 16);
        round.validate().unwrap();

        let mut bad = manifest.clone();
        bad.format = "tar".to_string();
        assert!(bad.validate().is_err());

        let mut newer = manifest.clone();
        newer.format_version = 99;
        assert!(newer.validate().is_err());

        let mut duplicated = manifest.clone();
        duplicated.members.push(duplicated.members[0].clone());
        assert!(duplicated.validate().is_err());
    }

    #[test]
    fn destinations_parse() {
        match Destination::parse("backup.tar").unwrap() {
            Destination::Local(path) => assert_eq!(path, PathBuf::from("backup.tar")),
            _ => panic!("a bare path must be local"),
        }
        match Destination::parse("/backups/ferroma.tar").unwrap() {
            Destination::Local(_) => {}
            _ => panic!("an absolute path must be local"),
        }
        match Destination::parse("s3://my-bucket/mail/ferroma.tar").unwrap() {
            Destination::S3 { bucket, key } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "mail/ferroma.tar");
            }
            _ => panic!("s3:// must be S3"),
        }
        match Destination::parse("webdav://dav.example.com/backups/ferroma.tar").unwrap() {
            Destination::WebDav(url) => {
                assert_eq!(url.as_str(), "https://dav.example.com/backups/ferroma.tar")
            }
            _ => panic!("webdav:// must be WebDAV"),
        }
        assert!(Destination::parse("ftp://host/file").is_err());
        assert!(Destination::parse("").is_err());
        assert!(Destination::parse("s3://bucket").is_err());
    }

    #[test]
    fn destination_display_redacts_passwords() {
        let url = Destination::parse("webdav://alice:s3cret@dav.example.com/x").unwrap();
        assert_eq!(url.display(), "https://dav.example.com/x");
    }

    #[test]
    fn pg_major_versions_parse() {
        assert_eq!(parse_major_version("pg_dump (PostgreSQL) 18.6"), Some(18));
        assert_eq!(
            parse_major_version("pg_dump (PostgreSQL) 16.4 (Debian 16.4-1) on x86_64"),
            Some(16)
        );
        assert_eq!(parse_major_version("PostgreSQL 15.2"), Some(15));
        assert_eq!(parse_major_version("not a version"), None);
    }

    #[test]
    fn sha256_of_a_file_matches_the_digest_crate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, b"hello world").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn an_s3_redirect_names_its_region() {
        let body = "<Endpoint>backup.s3-ap-northeast-1.amazonaws.com</Endpoint>";
        assert_eq!(
            s3_redirect_region(reqwest::StatusCode::MOVED_PERMANENTLY, body).as_deref(),
            Some("ap-northeast-1")
        );
        let dotted = "<Endpoint>backup.s3.eu-west-1.amazonaws.com</Endpoint>";
        assert_eq!(
            s3_redirect_region(reqwest::StatusCode::MOVED_PERMANENTLY, dotted).as_deref(),
            Some("eu-west-1")
        );
        assert!(s3_redirect_region(reqwest::StatusCode::OK, body).is_none());
    }

    #[test]
    fn self_hosted_s3_uses_path_style_and_signs_its_actual_host() {
        let settings = serde_json::json!({
            "endpoint": "https://s3.example.test:9443",
            "region": "local",
            "access_key": "example-key",
            "secret_key": "example-secret"
        });
        let creds = TransferCredentials::from_sources(&settings);
        let (url, headers, authorization) = s3_signed_request(
            "PUT", "backup", "mail/my archive.tar", &creds,
            &empty_sha256(), "20260923T120000Z",
        ).expect("valid endpoint");
        assert_eq!(url, "https://s3.example.test:9443/backup/mail/my%20archive.tar");
        assert!(headers.iter().any(|(name, value)| *name == "host" && value == "s3.example.test:9443"));
        assert!(authorization.contains("/local/s3/aws4_request"));
        assert!(!authorization.contains("example-secret"));
    }

    #[test]
    fn s3_keys_are_uri_encoded_per_segment() {
        assert_eq!(encode_s3_key("a b/c+d"), "a%20b/c%2Bd");
        assert_eq!(encode_s3_key("plain/key.tar"), "plain/key.tar");
        assert_eq!(encode_s3_key("ümlaut"), "%C3%BCmlaut");
    }

    /// The canonical AWS SigV4 test vector (GET object, single chunk).
    ///
    /// From the AWS documentation's worked example, so a change to the signing
    /// code that breaks interoperability fails here even though no S3 bucket is
    /// reachable from the test environment.
    #[test]
    fn s3_signing_matches_the_aws_test_vector() {
        let authorization = s3_authorization(
            &S3AuthRequest {
                method: "GET",
                canonical_uri: "/test.txt",
                canonical_query: "",
                amz_date: "20130524T000000Z",
                region: "us-east-1",
                access_key: "AKIAIOSFODNN7EXAMPLE",
                secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            },
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", &empty_sha256()),
                ("x-amz-date", "20130524T000000Z"),
            ],
            &empty_sha256(),
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn a_built_archive_extracts_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let mail_root = dir.path().join("mail");
        let message = mail_root.join("example.com/alice/Maildir/cur/message");
        std::fs::create_dir_all(message.parent().unwrap()).unwrap();
        std::fs::write(&message, b"From: a@example.com\r\n\r\nhello").unwrap();
        let secret = dir.path().join("jwt_secret");
        std::fs::write(&secret, b"a-secret-of-sufficient-length-0123456789").unwrap();

        let mut planned = Vec::new();
        collect_dir(&mail_root, "mail", &mut planned).unwrap();
        collect_optional_file(&secret, "jwt_secret", &mut planned).unwrap();
        assert_eq!(planned.len(), 2);

        let members = members_from_planned(&planned).unwrap();
        let manifest = Manifest {
            format: ARCHIVE_FORMAT.to_string(),
            format_version: ARCHIVE_FORMAT_VERSION,
            ferroma_version: "test".to_string(),
            created_at: "2026-09-22T00:00:00Z".to_string(),
            pg_dump_major: 18,
            live: false,
            counts: Counts {
                maildir_files: 1,
                ..Counts::default()
            },
            members,
        };

        let archive = dir.path().join("archive.tar");
        let mut file = std::fs::File::create(&archive).unwrap();
        build_archive(&mut file, &manifest, &planned).unwrap();
        file.sync_all().unwrap();

        // The manifest is the first entry and is parseable.
        let read_back = read_manifest(&archive).unwrap();
        assert_eq!(read_back.pg_dump_major, 18);
        assert_eq!(read_back.members.len(), 2);

        // Extraction verifies checksums and reproduces the bytes.
        let staging = dir.path().join("stage");
        extract_and_verify(&archive, &manifest, &staging).unwrap();
        let restored =
            std::fs::read(staging.join("mail/example.com/alice/Maildir/cur/message")).unwrap();
        assert_eq!(restored, b"From: a@example.com\r\n\r\nhello");
        assert_eq!(
            std::fs::read_to_string(staging.join("jwt_secret")).unwrap(),
            "a-secret-of-sufficient-length-0123456789"
        );

        // A flipped byte inside a member is caught by the checksum. The manifest
        // itself is not checksummed (it would be self-referential), so the byte
        // has to land in a member the manifest lists — the message body.
        let mut bytes = std::fs::read(&archive).unwrap();
        let payload = bytes
            .windows(5)
            .position(|window| window == b"hello")
            .expect("the message body");
        bytes[payload] ^= 0x01;
        let corrupt = dir.path().join("corrupt.tar");
        std::fs::write(&corrupt, &bytes).unwrap();
        let err = extract_and_verify(&corrupt, &manifest, &staging).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("checksum"), "{message}");
    }

    #[test]
    fn an_archive_without_a_manifest_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("random.tar");
        let file = std::fs::File::create(&archive).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        builder
            .append_data(&mut header, "not-manifest", b"junk".as_slice())
            .unwrap();
        builder.finish().unwrap();
        assert!(read_manifest(&archive).is_err());
    }

    #[tokio::test]
    async fn serve_is_up_detects_a_bound_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut config = Config::default();
        config.api.host = "127.0.0.1".to_string();
        config.api.port = port;
        config.api.enabled = true;
        assert!(serve_is_up(&config).await.is_some());

        // Drop the listener: the same config now reads as stopped.
        drop(listener);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(serve_is_up(&config).await.is_none());
    }

    #[test]
    fn import_roots_deduplicate_nested_roots() {
        let mut config = Config::default();
        config.server.data_dir = PathBuf::from("/srv/ferroma");
        // Defaults: maildir and attachments hang off data_dir.
        let roots = import_roots(&config);
        assert_eq!(roots, vec![PathBuf::from("/srv/ferroma")]);
    }
}
