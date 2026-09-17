//! Account autodiscovery: `https://<domain>/.well-known/ferroma` (specification
//! §32; `docs/client.md` §12, `docs/api.md` §2).
//!
//! The user types an address and nothing else; discovery turns its domain into a
//! server configuration. The flow, in order:
//!
//! 1. `GET https://<domain>/.well-known/ferroma`. A `200` carrying the documented
//!    JSON wins and every field of it is recorded in a [`Discovery`].
//! 2. A **`404`** — and only a `404` — means the domain does not publish a record.
//!    [`Discovery::guessed`] then assumes the conventional hostnames
//!    (`mail.<domain>`, `imap.<domain>`) and marks the result
//!    [`DiscoverySource::Guessed`], so the UI can show it and ask the user to
//!    confirm it instead of pretending it was discovered.
//! 3. **Anything else is an error, and never guesses**: a `5xx`, a transport
//!    failure, a body that is not JSON, or a document without a usable `api` URL.
//!    Guessing there would silently point an account at somebody else's server, so
//!    the error is handed back and the caller decides — [`Discovery::candidate_hosts`]
//!    and the manual configuration pane are the documented fallback
//!    (`docs/client.md` §12, steps 2 and 3).
//!
//! The document is versioned and will grow, so unknown fields are ignored and
//! missing ones fall back: `protocol_version` defaults to 1 and a `imap`/`smtp`
//! section that is absent — or present but unusable — is simply `None` rather than
//! a hard failure. Only `api` is load-bearing, because it is the one field this
//! client actually uses; the `imap`/`smtp`/`web` entries exist so the same record
//! can serve third-party clients and future import flows.
//!
//! Discovery is unauthenticated and read-only: nothing here writes to the cache
//! and nothing here touches a stored token.
//!
//! ```text
//!   domain ──► Autodiscover::discover ──► 200 ──► Discovery { source: WellKnown }
//!                        │
//!                        ├── 404 ──────────► Discovery::guessed { source: Guessed }
//!                        │
//!                        └── 5xx / transport / bad body ──► Err(ClientError)
//! ```

use std::time::Duration;

use ferroma_core::EmailAddress;
use reqwest::header::{HeaderValue, ACCEPT, USER_AGENT};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::api::ApiError;
use crate::error::{ClientError, ClientResult};

/// The path a Ferroma deployment publishes its discovery document at (§32).
pub const WELL_KNOWN_PATH: &str = "/.well-known/ferroma";

/// The protocol version assumed when the document does not name one.
pub const DEFAULT_PROTOCOL_VERSION: u32 = 1;

/// How long the TCP and TLS setup of one discovery fetch may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a whole discovery fetch may take. Discovery is on the critical path of
/// "add an account", so it is bounded rather than left to the OS default.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a [`Discovery`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// The domain published `/.well-known/ferroma` and it was used as it was.
    WellKnown,
    /// No document was published (the well-known path answered `404`), so the
    /// conventional hostnames were assumed. The UI shows this as a guess and asks
    /// the user to confirm the server before the account is created.
    Guessed,
}

impl DiscoverySource {
    /// A short, stable name for logs and the UI (`well_known` / `guessed`).
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoverySource::WellKnown => "well_known",
            DiscoverySource::Guessed => "guessed",
        }
    }
}

/// One mail service endpoint the document advertises (`imap`, `smtp`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceEndpoint {
    /// The hostname, e.g. `mail.example.com`.
    pub host: String,
    /// The TCP port, e.g. `993`.
    pub port: u16,
    /// Whether the connection is wrapped in TLS from the first byte (implicit
    /// TLS), which is what both `imaps` and `smtps`/`submission` use here — never
    /// `STARTTLS`, which is negotiated later and cannot be expressed by a record
    /// that has to stay readable by a human.
    pub tls: bool,
}

/// A resolved server configuration for one address domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovery {
    /// The FCP API root, e.g. `https://mail.example.com/api/v1`. Append `/client`
    /// — or call [`Discovery::fcp_base_url`] — before talking to it.
    pub api: String,
    /// The IMAP endpoint, when the document names one.
    #[serde(default)]
    pub imap: Option<ServiceEndpoint>,
    /// The SMTP submission endpoint, when the document names one.
    #[serde(default)]
    pub smtp: Option<ServiceEndpoint>,
    /// The webmail URL, when the document names one.
    #[serde(default)]
    pub web: Option<String>,
    /// The protocol version the document advertises; `1` when it is silent.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
    /// Whether this configuration was published or assumed.
    pub source: DiscoverySource,
    /// The lower-cased domain the record was looked up for.
    pub domain: String,
}

/// The protocol version a silent document is assumed to speak.
fn default_protocol_version() -> u32 {
    DEFAULT_PROTOCOL_VERSION
}

impl Discovery {
    /// The FCP base URL: [`api`](Discovery::api) with `/client` appended when it
    /// does not already end in it.
    ///
    /// `https://mail.example.com/api/v1` becomes
    /// `https://mail.example.com/api/v1/client`, which is what
    /// [`crate::FcpClient::new`] expects; a document that already includes the
    /// suffix is left alone rather than getting a second one.
    pub fn fcp_base_url(&self) -> String {
        let api = self.api.trim_end_matches('/');
        if api.ends_with("/client") {
            api.to_string()
        } else {
            format!("{api}/client")
        }
    }

    /// The conventional-hostname fallback used when `/.well-known/ferroma` answers
    /// `404` (§32).
    ///
    /// * `api` — `https://mail.<domain>/api/v1`
    /// * `imap` — `imap.<domain>:993`, implicit TLS
    /// * `smtp` — `mail.<domain>:587`, implicit TLS
    /// * `web` — `https://mail.<domain>`
    ///
    /// The result is marked [`DiscoverySource::Guessed`]: it is a guess, and the
    /// UI must say so.
    pub fn guessed(domain: &str) -> Discovery {
        let domain = normalise_domain(domain);
        let mail = format!("mail.{domain}");
        Discovery {
            api: format!("https://{mail}/api/v1"),
            imap: Some(ServiceEndpoint {
                host: format!("imap.{domain}"),
                port: 993,
                tls: true,
            }),
            smtp: Some(ServiceEndpoint {
                host: mail.clone(),
                port: 587,
                tls: true,
            }),
            web: Some(format!("https://{mail}")),
            protocol_version: DEFAULT_PROTOCOL_VERSION,
            source: DiscoverySource::Guessed,
            domain,
        }
    }

    /// The candidate hostnames to try, in order, for the UI's "advanced" pane:
    /// `["mail.<domain>", "imap.<domain>", "<domain>"]`.
    ///
    /// The order is deliberate — the names a Ferroma deployment actually uses come
    /// first, and the bare domain last, because that is the one that most often
    /// turns out to be a web host rather than a mail host.
    pub fn candidate_hosts(domain: &str) -> Vec<String> {
        let domain = normalise_domain(domain);
        vec![
            format!("mail.{domain}"),
            format!("imap.{domain}"),
            domain,
        ]
    }
}

/// The autodiscovery client: one HTTP client plus the path it fetches.
///
/// Cheap to clone — the clone shares the connection pool — and safe to keep for
/// the lifetime of the "add an account" dialog.
#[derive(Debug, Clone)]
pub struct Autodiscover {
    http: reqwest::Client,
    well_known_path: String,
}

impl Autodiscover {
    /// Build a client that fetches [`WELL_KNOWN_PATH`].
    pub fn new() -> ClientResult<Self> {
        Autodiscover::with_path(WELL_KNOWN_PATH)
    }

    /// Build a client that fetches `path` instead of `/.well-known/ferroma`.
    ///
    /// This exists for tests, and for a deployment that has to publish the record
    /// somewhere else than the well-known root.
    pub fn with_path(path: impl Into<String>) -> ClientResult<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| ClientError::Network(format!("building the http client failed: {err}")))?;
        Ok(Autodiscover {
            http,
            well_known_path: path.into(),
        })
    }

    /// The document path this client fetches.
    pub fn well_known_path(&self) -> &str {
        &self.well_known_path
    }

    /// Fetch `<base><well_known_path>` for `domain`.
    ///
    /// `base` is a scheme-and-host root such as `https://example.com`; a trailing
    /// `/` is ignored. The `domain` is only used to fill in
    /// [`Discovery::domain`] and to build a guess — the request itself goes to
    /// `base`, which is what lets the tests point this at a loopback mock server.
    ///
    /// A `404` is not an error here: it is the documented "this domain does not
    /// publish a record" answer, and it produces [`Discovery::guessed`]. A `5xx`
    /// or a transport failure *is* an error
    /// ([`ClientError::Api`](crate::ClientError::Api) /
    /// [`ClientError::Network`](crate::ClientError::Network)) and never guesses,
    /// because "the server is down" and "there is no server" are different facts.
    pub async fn discover_at(&self, domain: &str, base: &str) -> ClientResult<Discovery> {
        let url = format!("{}{}", base.trim_end_matches('/'), self.well_known_path);
        tracing::debug!(domain, %url, "fetching the autodiscovery document");

        let mut request = self.http.get(url.as_str()).header(ACCEPT, "application/json");
        if let Ok(agent) = HeaderValue::from_str(&crate::api::client_version_string()) {
            request = request.header(USER_AGENT, agent);
        }

        let response = request
            .send()
            .await
            .map_err(|err| map_reqwest_error(&url, err))?;
        let status = response.status();

        if status == StatusCode::NOT_FOUND {
            tracing::debug!(
                domain,
                %url,
                "no discovery document; assuming the conventional hostnames"
            );
            return Ok(Discovery::guessed(domain));
        }

        let body = response
            .bytes()
            .await
            .map_err(|err| map_reqwest_error(&url, err))?;

        if !status.is_success() {
            return Err(ClientError::Api(ApiError::parse(
                status.as_u16(),
                &body,
                None,
            )));
        }

        parse_document(domain, &body, DiscoverySource::WellKnown)
    }

    /// Fetch `https://<domain>/.well-known/ferroma` and fall back to
    /// [`Discovery::guessed`] when — and only when — that path answers `404`.
    ///
    /// Every other failure is returned to the caller: a `5xx`, a transport
    /// failure, and a body that is not the documented JSON
    /// ([`ClientError::Parse`](crate::ClientError::Parse)) all mean "we could not
    /// ask", not "there is nothing to ask", and guessing there would hide a
    /// misconfigured or intercepted server. The caller decides whether to fall
    /// back to the manual pane.
    ///
    /// The fetch is HTTPS-only: the hostname comes from the domain the user typed
    /// and a plain-HTTP answer is never trusted (`docs/client.md` §12).
    pub async fn discover(&self, domain: &str) -> ClientResult<Discovery> {
        let domain = normalise_domain(domain);
        if domain.is_empty() {
            return Err(ClientError::invalid("autodiscovery needs a domain"));
        }
        self.discover_at(&domain, &format!("https://{domain}")).await
    }
}

/// The domain part of an address, for autodiscovery.
///
/// `alice@Example.COM` yields `example.com`: the domain is the lookup key, and DNS
/// is case-insensitive, so it is normalised here rather than at every call site.
/// An address that is not an address — no `@`, an empty local part, a malformed
/// domain — is [`ClientError::Invalid`](crate::ClientError::Invalid); nothing is
/// guessed, because guessing here means discovering the wrong server.
pub fn domain_of(address: &str) -> ClientResult<String> {
    match EmailAddress::parse(address.trim()) {
        Ok(parsed) => Ok(parsed.domain().to_string()),
        Err(err) => Err(ClientError::invalid(format!(
            "{address:?} is not an address: {err}"
        ))),
    }
}

/// Lower-case a domain and drop the root label's trailing dot, so
/// `Mail.Example.COM.` and `mail.example.com` are one host.
fn normalise_domain(domain: &str) -> String {
    domain
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// The document as it arrives, before any of it is trusted.
///
/// Every field is optional and every field is read through a
/// [`serde_json::Value`], so a document that is *almost* right still yields a
/// usable [`Discovery`] instead of failing the whole account setup.
#[derive(Debug, Default, Deserialize)]
struct WellKnownDocument {
    /// The FCP API root. The only field this client requires.
    #[serde(default)]
    api: Option<Value>,
    /// The IMAP endpoint.
    #[serde(default)]
    imap: Option<Value>,
    /// The SMTP submission endpoint.
    #[serde(default)]
    smtp: Option<Value>,
    /// The webmail URL.
    #[serde(default)]
    web: Option<Value>,
    /// The advertised protocol version.
    #[serde(default)]
    protocol_version: Option<Value>,
}

/// Parse a `200` body into a [`Discovery`].
///
/// A body that is not JSON, or JSON without a usable `api` URL, is
/// [`ClientError::Parse`](crate::ClientError::Parse) — the caller decides whether
/// that is worth guessing around.
fn parse_document(domain: &str, body: &[u8], source: DiscoverySource) -> ClientResult<Discovery> {
    let document: WellKnownDocument = serde_json::from_slice(body).map_err(|err| {
        ClientError::parse(format!(
            "the discovery document for {domain} was not the expected JSON: {err}"
        ))
    })?;

    let api = document
        .api
        .as_ref()
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|api| !api.is_empty())
        .ok_or_else(|| {
            ClientError::parse(format!(
                "the discovery document for {domain} has no usable `api` URL"
            ))
        })?;
    if !is_usable_api_url(api) {
        return Err(ClientError::parse(format!(
            "the discovery document for {domain} has an unusable `api` URL: {api:?}"
        )));
    }

    Ok(Discovery {
        // A trailing `/` would produce `…/api/v1//client` once `/client` is
        // appended, so it is dropped here, exactly like `FcpClient::new` does.
        api: api.trim_end_matches('/').to_string(),
        imap: document.imap.as_ref().and_then(service_endpoint),
        smtp: document.smtp.as_ref().and_then(service_endpoint),
        web: document
            .web
            .as_ref()
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|web| !web.is_empty())
            .map(str::to_string),
        protocol_version: document
            .protocol_version
            .as_ref()
            .and_then(Value::as_u64)
            .map(|version| version as u32)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION),
        source,
        domain: normalise_domain(domain),
    })
}

/// Read an `imap`/`smtp` section, or `None` when it is not usable.
///
/// A section without a host, or with a port that is not a `u16`, is treated as
/// absent: a guessed port is worse than no port, and the endpoint is optional
/// anyway.
fn service_endpoint(value: &Value) -> Option<ServiceEndpoint> {
    let host = value.get("host")?.as_str()?.trim();
    if host.is_empty() {
        return None;
    }
    let port = value.get("port")?.as_u64()?;
    if port == 0 || port > u64::from(u16::MAX) {
        return None;
    }
    Some(ServiceEndpoint {
        host: host.to_string(),
        port: port as u16,
        tls: value
            .get("tls")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Whether `api` is a URL this client could actually talk to: an absolute
/// `http`/`https` URL with a host. `mail.example.com/api/v1` is not one, and
/// neither is `""`.
fn is_usable_api_url(api: &str) -> bool {
    match Url::parse(api) {
        Ok(url) => matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        Err(_) => false,
    }
}

/// Map a `reqwest` failure onto the client's error vocabulary.
///
/// The distinction matters: a timeout and a refused connection are temporary and
/// the caller may retry, while a URL we could not even build is a bug in the
/// caller's input and never will be.
fn map_reqwest_error(url: &str, err: reqwest::Error) -> ClientError {
    if err.is_timeout() {
        ClientError::Timeout(format!("fetching {url} timed out"))
    } else if err.is_builder() {
        ClientError::invalid(format!("could not build the request for {url}: {err}"))
    } else {
        ClientError::Network(format!("fetching {url} failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockServer;

    /// The document frozen by `docs/api.md` §2 and specification §32.
    const DOCUMENT: &str = r#"{
        "api": "https://mail.example.com/api/v1",
        "imap": { "host": "mail.example.com", "port": 993, "tls": true },
        "smtp": { "host": "mail.example.com", "port": 587, "tls": true },
        "web": "https://mail.example.com",
        "protocol_version": 1
    }"#;

    async fn client() -> Autodiscover {
        Autodiscover::new().expect("autodiscover client")
    }

    #[tokio::test]
    async fn a_published_document_is_used_field_for_field() {
        let server = MockServer::start().await;
        server.json_route("GET", WELL_KNOWN_PATH, 200, DOCUMENT);

        let discovery = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect("discovery");

        assert_eq!(discovery.api, "https://mail.example.com/api/v1");
        assert_eq!(
            discovery.imap,
            Some(ServiceEndpoint {
                host: "mail.example.com".to_string(),
                port: 993,
                tls: true,
            })
        );
        assert_eq!(
            discovery.smtp,
            Some(ServiceEndpoint {
                host: "mail.example.com".to_string(),
                port: 587,
                tls: true,
            })
        );
        assert_eq!(discovery.web.as_deref(), Some("https://mail.example.com"));
        assert_eq!(discovery.protocol_version, 1);
        assert_eq!(discovery.source, DiscoverySource::WellKnown);
        assert_eq!(discovery.domain, "example.com");
        assert_eq!(discovery.source.as_str(), "well_known");
    }

    #[tokio::test]
    async fn the_document_is_fetched_from_the_well_known_path() {
        let server = MockServer::start().await;
        server.json_route("GET", WELL_KNOWN_PATH, 200, DOCUMENT);

        client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect("discovery");

        let requests = server.requests_for(WELL_KNOWN_PATH);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].header("accept"),
            Some("application/json")
        );
        // A client with a custom path asks for exactly that path.
        let custom = Autodiscover::with_path("/.well-known/ferroma.json").expect("client");
        assert_eq!(custom.well_known_path(), "/.well-known/ferroma.json");
    }

    #[tokio::test]
    async fn a_404_falls_back_to_the_conventional_hostnames() {
        // No route is registered at all, so the mock server answers 404.
        let server = MockServer::start().await;

        let discovery = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect("the 404 is the documented fallback, not an error");

        assert_eq!(discovery.source, DiscoverySource::Guessed);
        assert_eq!(discovery.domain, "example.com");
        assert_eq!(discovery.api, "https://mail.example.com/api/v1");
        assert_eq!(
            discovery.imap,
            Some(ServiceEndpoint {
                host: "imap.example.com".to_string(),
                port: 993,
                tls: true,
            })
        );
        assert_eq!(
            discovery.smtp,
            Some(ServiceEndpoint {
                host: "mail.example.com".to_string(),
                port: 587,
                tls: true,
            })
        );
        assert_eq!(discovery.web.as_deref(), Some("https://mail.example.com"));
        assert_eq!(discovery.protocol_version, 1);
    }

    #[tokio::test]
    async fn a_404_envelope_also_falls_back() {
        let server = MockServer::start().await;
        server.error_route("GET", WELL_KNOWN_PATH, 404, "not_found", "no record");

        let discovery = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect("discovery");
        assert_eq!(discovery, Discovery::guessed("example.com"));
    }

    #[tokio::test]
    async fn a_malformed_body_is_a_parse_error_and_does_not_guess() {
        let server = MockServer::start().await;
        server.json_route("GET", WELL_KNOWN_PATH, 200, "{ this is not json");

        let err = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect_err("a malformed body must not be guessed around");
        assert!(matches!(err, ClientError::Parse(_)), "{err:?}");
        assert_eq!(err.api_status(), Some(400));
    }

    #[tokio::test]
    async fn json_without_an_api_url_is_a_parse_error() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            WELL_KNOWN_PATH,
            200,
            r#"{"imap":{"host":"mail.example.com","port":993,"tls":true}}"#,
        );

        let err = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect_err("no `api`, no discovery");
        assert!(matches!(err, ClientError::Parse(_)), "{err:?}");
    }

    #[tokio::test]
    async fn an_api_url_without_a_scheme_is_a_parse_error() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            WELL_KNOWN_PATH,
            200,
            r#"{"api":"mail.example.com/api/v1"}"#,
        );

        let err = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect_err("a relative api URL is not usable");
        assert!(matches!(err, ClientError::Parse(_)), "{err:?}");

        assert!(!is_usable_api_url("mail.example.com/api/v1"));
        assert!(!is_usable_api_url(""));
        assert!(!is_usable_api_url("ftp://mail.example.com"));
        assert!(is_usable_api_url("https://mail.example.com/api/v1"));
        assert!(is_usable_api_url("http://127.0.0.1:8080/api/v1"));
    }

    #[tokio::test]
    async fn a_server_error_is_an_error_and_does_not_guess() {
        let server = MockServer::start().await;
        server.error_route("GET", WELL_KNOWN_PATH, 500, "internal_error", "boom");

        let err = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect_err("a 5xx means we could not ask, not that nothing is published");
        assert!(
            matches!(&err, ClientError::Api(api) if api.status == 500),
            "{err:?}"
        );
        // A 5xx is temporary; the caller may retry the very same request.
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn a_body_without_the_optional_sections_still_parses() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            WELL_KNOWN_PATH,
            200,
            r#"{"api":"https://mail.example.com/api/v1"}"#,
        );

        let discovery = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect("discovery");

        assert_eq!(discovery.api, "https://mail.example.com/api/v1");
        assert_eq!(discovery.imap, None);
        assert_eq!(discovery.smtp, None);
        assert_eq!(discovery.web, None);
        assert_eq!(discovery.protocol_version, DEFAULT_PROTOCOL_VERSION);
        assert_eq!(discovery.source, DiscoverySource::WellKnown);
    }

    #[tokio::test]
    async fn null_and_unusable_optional_sections_become_none() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            WELL_KNOWN_PATH,
            200,
            r#"{"api":"https://mail.example.com/api/v1","imap":null,
                "smtp":{"host":"mail.example.com","port":99999,"tls":true},
                "web":123,"protocol_version":"one"}"#,
        );

        let discovery = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect("a document that is almost right is still usable");

        assert_eq!(discovery.imap, None);
        assert_eq!(discovery.smtp, None);
        assert_eq!(discovery.web, None);
        assert_eq!(discovery.protocol_version, DEFAULT_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn a_trailing_slash_on_the_api_url_is_normalised() {
        let server = MockServer::start().await;
        server.json_route(
            "GET",
            WELL_KNOWN_PATH,
            200,
            r#"{"api":"https://mail.example.com/api/v1/"}"#,
        );

        let discovery = client()
            .await
            .discover_at("example.com", &server.base_url())
            .await
            .expect("discovery");
        assert_eq!(discovery.api, "https://mail.example.com/api/v1");
        assert_eq!(
            discovery.fcp_base_url(),
            "https://mail.example.com/api/v1/client"
        );
    }

    #[tokio::test]
    async fn fcp_base_url_appends_client_exactly_once() {
        let mut discovery = Discovery::guessed("example.com");
        assert_eq!(
            discovery.fcp_base_url(),
            "https://mail.example.com/api/v1/client"
        );

        discovery.api = "https://mail.example.com/api/v1/client".to_string();
        assert_eq!(
            discovery.fcp_base_url(),
            "https://mail.example.com/api/v1/client"
        );

        discovery.api = "https://mail.example.com/api/v1/client/".to_string();
        assert_eq!(
            discovery.fcp_base_url(),
            "https://mail.example.com/api/v1/client"
        );
    }

    #[tokio::test]
    async fn guessed_uses_the_documented_hostnames() {
        let discovery = Discovery::guessed("example.com");
        assert_eq!(discovery.api, "https://mail.example.com/api/v1");
        assert_eq!(discovery.web.as_deref(), Some("https://mail.example.com"));
        assert_eq!(discovery.protocol_version, 1);
        assert_eq!(discovery.source, DiscoverySource::Guessed);
        assert_eq!(discovery.domain, "example.com");
        assert_eq!(discovery.source.as_str(), "guessed");

        // `Mail.Example.COM.` and `mail.example.com` are one host.
        assert_eq!(
            Discovery::guessed(" Mail.Example.COM. "),
            Discovery::guessed("mail.example.com")
        );
    }

    #[tokio::test]
    async fn candidate_hosts_are_ordered_mail_then_imap_then_the_domain() {
        assert_eq!(
            Discovery::candidate_hosts("example.com"),
            vec![
                "mail.example.com".to_string(),
                "imap.example.com".to_string(),
                "example.com".to_string(),
            ]
        );
        assert_eq!(
            Discovery::candidate_hosts(" Example.COM. "),
            Discovery::candidate_hosts("example.com")
        );
    }

    #[tokio::test]
    async fn domain_of_extracts_the_lower_cased_domain() {
        assert_eq!(domain_of("alice@example.com").expect("domain"), "example.com");
        assert_eq!(
            domain_of("  Bob@Mail.Example.COM  ").expect("domain"),
            "mail.example.com"
        );
        assert_eq!(
            domain_of("<alice@example.com>").expect("domain"),
            "example.com"
        );
    }

    #[tokio::test]
    async fn domain_of_rejects_a_malformed_address() {
        for bad in ["alice", "@example.com", "", "alice@", "a b@example.com"] {
            let err = domain_of(bad).expect_err("not an address");
            assert!(matches!(err, ClientError::Invalid(_)), "{bad}: {err:?}");
        }
    }

    #[tokio::test]
    async fn discover_rejects_an_empty_domain_before_it_dials() {
        let err = client()
            .await
            .discover("   ")
            .await
            .expect_err("an empty domain has nothing to look up");
        assert!(matches!(err, ClientError::Invalid(_)), "{err:?}");
    }
}
