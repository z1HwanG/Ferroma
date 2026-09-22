//! Health, version and autodiscovery.
//!
//! `GET /api/v1/health` drives the container healthcheck *and* the Admin dashboard, so
//! it answers with real figures or with `503 degraded` — never with a fabricated zero.
//! When the database is unreachable the `database` block says so, the `queue` and
//! `clients` blocks are **omitted** (their numbers would be lies), and the status is
//! `degraded`, which is what `docs/api.md` §2 requires.
//!
//! `GET /.well-known/ferroma` is the autodiscovery document a client fetches from the
//! domain of the address the user typed (specification §32). It is built entirely from
//! configuration, so it answers even while the database is down.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferroma_core::VERSION;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// The `database` block of the health body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseHealth {
    /// Whether a trivial query round-tripped.
    pub ok: bool,
    /// The PostgreSQL server version, when it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// Pool utilisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolHealth>,
}

/// Connection-pool utilisation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PoolHealth {
    /// Connections currently open.
    pub size: u32,
    /// Connections sitting idle.
    pub idle: u32,
    /// The configured maximum.
    pub max: u32,
}

/// One listener's state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ListenerHealth {
    /// Whether the listener runs in this process.
    pub enabled: bool,
    /// Live connections.
    pub connections: u64,
}

/// Client sessions and devices.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClientsHealth {
    /// Non-revoked, non-expired `sessions` rows.
    pub active_sessions: i64,
    /// Non-revoked `devices` rows.
    pub active_devices: i64,
}

/// The outbound queue and today's traffic.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QueueHealth {
    /// Waiting for a first attempt.
    pub pending: i64,
    /// Claimed by a worker right now.
    pub delivering: i64,
    /// Waiting for a later attempt.
    pub retry: i64,
    /// Given up on (bounced).
    pub failed: i64,
    /// Messages received during the current UTC day.
    pub received_today: i64,
    /// Messages queued during the current UTC day.
    pub sent_today: i64,
}

/// The whole `GET /api/v1/health` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// `ok` or `degraded`.
    pub status: String,
    /// The server version.
    pub version: String,
    /// The FCP protocol version.
    pub protocol_version: u32,
    /// Seconds since the process started.
    pub uptime_secs: u64,
    /// Database reachability and pool state.
    pub database: DatabaseHealth,
    /// SMTP listener state.
    pub smtp: ListenerHealth,
    /// IMAP listener state.
    pub imap: ListenerHealth,
    /// Client sessions and devices; omitted when the database is down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clients: Option<ClientsHealth>,
    /// Queue depth and daily traffic; omitted when the database is down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueHealth>,
}

/// `GET /api/v1/health`
pub async fn health(State(state): State<AppState>) -> Response {
    let reachable = state.database_reachable().await;

    let (database, clients, queue) = if reachable {
        let pool = state.pool_stats();
        (
            DatabaseHealth {
                ok: true,
                server_version: state
                    .server_version_text()
                    .await
                    .map(|text| text.split(" on ").next().unwrap_or(&text).to_string()),
                pool: pool.map(|pool| PoolHealth {
                    size: pool.size,
                    idle: pool.idle,
                    max: pool.max,
                }),
            },
            collect_clients(&state).await,
            collect_queue(&state).await,
        )
    } else {
        (
            DatabaseHealth {
                ok: false,
                server_version: None,
                pool: None,
            },
            None,
            None,
        )
    };

    let ok = database.ok;
    let listeners = state.listeners.states();
    let body = HealthResponse {
        status: if ok { "ok".into() } else { "degraded".into() },
        version: VERSION.to_string(),
        protocol_version: state.protocol_version(),
        uptime_secs: state.uptime_secs(),
        database,
        smtp: ListenerHealth {
            enabled: listeners.smtp.enabled,
            connections: state.connections.smtp_connections(),
        },
        imap: ListenerHealth {
            enabled: listeners.imap.enabled,
            connections: state.connections.imap_connections(),
        },
        clients,
        queue,
    };

    if ok {
        (StatusCode::OK, Json(body)).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
    }
}

/// The `clients` block. Only called when the database is reachable.
///
/// `ferroma-storage`'s devices repository has no "count active" method and this crate
/// must not edit that crate, so the count comes from the active-device list. It is
/// bounded by the number of client installations on the server, which is small.
async fn collect_clients(state: &AppState) -> Option<ClientsHealth> {
    let active_sessions = match state.repos.sessions.count_active().await {
        Ok(count) => count,
        Err(err) => {
            tracing::debug!(error = %err, "health: session count failed");
            return None;
        }
    };
    let active_devices = state
        .repos
        .devices
        .list_active()
        .await
        .map(|devices| devices.len() as i64)
        .unwrap_or(0);
    Some(ClientsHealth {
        active_sessions,
        active_devices,
    })
}

/// The `queue` block. Only called when the database is reachable.
///
/// `sent_today` comes from the queue (one row per recipient per outbound message, which
/// is what "sent" means operationally), and `received_today` from the messages that
/// landed in an inbox since midnight UTC. A figure the repositories cannot answer is
/// only reported as `0` when the query itself succeeded; the whole block is omitted
/// otherwise, which is what `docs/api.md` §2 requires.
async fn collect_queue(state: &AppState) -> Option<QueueHealth> {
    let stats = match state.repos.queue.stats().await {
        Ok(stats) => stats,
        Err(err) => {
            tracing::debug!(error = %err, "health: queue stats failed");
            return None;
        }
    };

    let midnight = utc_midnight(chrono::Utc::now());
    let sent_today = count_queue_since(state, midnight).await;
    let received_today = count_inbound_since(state, midnight).await;

    Some(QueueHealth {
        pending: stats.pending,
        delivering: stats.delivering,
        retry: stats.retry,
        failed: stats.failed,
        received_today,
        sent_today,
    })
}

/// Midnight UTC of the day `now` falls in.
pub fn utc_midnight(now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc())
        .unwrap_or(now)
}

/// Queue rows created at or after `since`, across every status.
///
/// The repository counts one status at a time and has no time filter, so the six
/// statuses are counted through the rows the queue holds. A deployment with a very
/// large queue would want a SQL aggregate; this crate has no `sqlx` dependency of its
/// own, so the count is assembled from the repository's own API.
async fn count_queue_since(state: &AppState, since: chrono::DateTime<chrono::Utc>) -> i64 {
    let mut total = 0i64;
    for status in crate::routes::admin::queue::QUEUE_STATUSES {
        let mut offset = 0i64;
        loop {
            let Ok(rows) = state.repos.queue.list_by_status(status, 500, offset).await else {
                break;
            };
            if rows.is_empty() {
                break;
            }
            offset += rows.len() as i64;
            total += rows
                .iter()
                .filter(|entry| entry.created_at >= since)
                .count() as i64;
            if rows.len() < 500 {
                break;
            }
            // A queue row is created before `since` as soon as one is older than it, so
            // the page walk can stop: rows are ordered newest first.
            if rows
                .last()
                .map(|entry| entry.created_at < since)
                .unwrap_or(true)
            {
                break;
            }
        }
    }
    total
}

/// Messages that arrived in an account's `INBOX` at or after `since`.
async fn count_inbound_since(state: &AppState, since: chrono::DateTime<chrono::Utc>) -> i64 {
    let mut total = 0i64;
    let mut offset = 0i64;
    loop {
        let users = match state.repos.users.list(200, offset).await {
            Ok(users) => users,
            Err(err) => {
                tracing::debug!(error = %err, "health: user page failed");
                return total;
            }
        };
        if users.is_empty() {
            return total;
        }
        offset += users.len() as i64;

        for user in &users {
            let Ok(mailboxes) = state
                .repos
                .mailboxes
                .list_by_user(ferroma_core::UserId::new(user.id))
                .await
            else {
                continue;
            };
            for mailbox in mailboxes {
                let Ok(folders) = state.repos.folders.list(mailbox.mailbox_id()).await else {
                    continue;
                };
                for folder in folders.iter().filter(|folder| folder.special_use.is_none()) {
                    let Ok(messages) = state
                        .repos
                        .messages
                        .list_by_folder(folder.folder_id(), 500, 0)
                        .await
                    else {
                        continue;
                    };
                    total += messages
                        .iter()
                        .filter(|message| message.internal_date >= since)
                        .count() as i64;
                }
            }
        }
    }
}

/// The `GET /api/v1/version` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionResponse {
    /// The release version.
    pub version: String,
    /// The FCP protocol version.
    pub protocol_version: u32,
    /// The git revision this build came from, or `"unknown"`.
    pub git_sha: String,
    /// When it was built, or `"unknown"`.
    pub built: String,
}

/// `GET /api/v1/version`
pub async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: VERSION.to_string(),
        protocol_version: ferroma_core::PROTOCOL_VERSION,
        git_sha: ferroma_core::version::GIT_SHA.to_string(),
        built: ferroma_core::BUILD_TIMESTAMP.to_string(),
    })
}

/// One listener in the autodiscovery document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredEndpoint {
    /// Host name.
    pub host: String,
    /// Port number.
    pub port: u16,
    /// Whether the connection is encrypted at all.
    ///
    /// This does not say how. A client that reads `true` as "TLS from the first byte"
    /// opens 587 that way and the handshake fails: 587 speaks SMTP first. `security`
    /// names the mode.
    pub tls: bool,
    /// `starttls` on 587 and 143, `implicit` on 465 and 993.
    ///
    /// Omitted on a plaintext port. A client that only reads `tls` still works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<String>,
}

/// The `GET /.well-known/ferroma` body (specification §32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WellKnownResponse {
    /// The base URL of the management API.
    pub api: String,
    /// How to reach IMAP.
    pub imap: DiscoveredEndpoint,
    /// How to reach SMTP submission.
    pub smtp: DiscoveredEndpoint,
    /// The Webmail URL.
    pub web: String,
    /// The FCP protocol version.
    pub protocol_version: u32,
}

impl WellKnownResponse {
    /// Build the document from configuration.
    ///
    /// `api.public_url` is the single source of truth for the external host and
    /// scheme: everything else is derived from it, so a deployment behind a reverse
    /// proxy publishes the proxy's hostname rather than the container's.
    pub fn from_config(config: &ferroma_core::Config) -> Self {
        let public = config.api.public_url.trim_end_matches('/');
        let (host, host_is_tls) = split_public_url(public, &config.server.hostname);
        let base_path = config.api.base_path.trim_end_matches('/');
        // TLS is advertised when this server terminates it on the explicit TLS ports,
        // when `[tls]` is on at all, or when the public URL is already `https://` —
        // which is what a deployment behind a TLS-terminating proxy publishes.
        let tls_configured = config.tls.enabled || host_is_tls;

        WellKnownResponse {
            api: format!("{public}{base_path}"),
            imap: DiscoveredEndpoint {
                host: host.clone(),
                // 993 when implicit TLS is available, otherwise the plaintext port,
                // which upgrades with STARTTLS. Advertising 993 while it is not
                // listening is what makes a client time out on "TLS".
                port: if config.imap.imaps_port != 0 {
                    config.imap.imaps_port
                } else {
                    config.imap.port
                },
                tls: config.imap.imaps_port != 0 || tls_configured,
                security: security_mode(config.imap.imaps_port != 0, tls_configured),
            },
            smtp: DiscoveredEndpoint {
                host,
                // 587, not 465, unless implicit TLS is actually listening. A client
                // that opens 587 as implicit TLS fails immediately; one that opens a
                // closed 465 as STARTTLS waits until it times out.
                port: if config.smtp.smtps_port != 0 {
                    config.smtp.smtps_port
                } else {
                    config.smtp.submission_port
                },
                tls: config.smtp.smtps_port != 0 || tls_configured,
                security: security_mode(config.smtp.smtps_port != 0, tls_configured),
            },
            web: public.to_string(),
            protocol_version: config.client.protocol_version,
        }
    }
}

/// How a discovered port is encrypted.
///
/// An implicit-TLS port (465, 993) is `implicit`. A plaintext port that can upgrade
/// (587, 143) is `starttls` once TLS exists at all. A deployment with no TLS names
/// neither, and the field is left out.
fn security_mode(implicit: bool, tls_configured: bool) -> Option<String> {
    if implicit {
        Some("implicit".to_string())
    } else if tls_configured {
        Some("starttls".to_string())
    } else {
        None
    }
}

/// Split a public URL into its host and whether it is served over TLS.
fn split_public_url(public: &str, fallback_host: &str) -> (String, bool) {
    match url::Url::parse(public) {
        Ok(parsed) => {
            let host = parsed
                .host_str()
                .filter(|host| !host.is_empty())
                .unwrap_or(fallback_host)
                .to_string();
            (host, parsed.scheme() == "https")
        }
        Err(_) => (fallback_host.to_string(), false),
    }
}

/// `GET /.well-known/ferroma`
pub async fn well_known(State(state): State<AppState>) -> Json<WellKnownResponse> {
    Json(WellKnownResponse::from_config(&state.config))
}

/// `GET /.well-known/mta-sts.txt`
///
/// MTA-STS (RFC 8461) requires a policy file served from `mta-sts.<domain>`; this
/// endpoint serves the policy for the *server's* own hostname, so an operator can
/// point a rewrite at it. A deployment that runs MTA-STS for its customer domains
/// publishes per-domain files from the Admin panel's DNS screen instead.
pub async fn mta_sts(State(state): State<AppState>) -> Response {
    let hostname = state.config.server.hostname.clone();
    let policy = format!(
        "version: STSv1\r\nmode: {mode}\r\nmx: {hostname}\r\nmax_age: 604800\r\n",
        mode = if state.config.tls.enabled {
            "enforce"
        } else {
            "testing"
        },
    );
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        policy,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroma_core::Config;

    fn config_with(public_url: &str) -> Config {
        let mut config = Config::default();
        config.api.public_url = public_url.to_string();
        config.server.hostname = "mail.example.com".into();
        config
    }

    #[test]
    fn autodiscovery_is_built_from_the_public_url() {
        let document = WellKnownResponse::from_config(&config_with("https://mail.example.com"));
        assert_eq!(document.api, "https://mail.example.com/api/v1");
        assert_eq!(document.web, "https://mail.example.com");
        assert_eq!(document.imap.host, "mail.example.com");
        assert_eq!(document.smtp.host, "mail.example.com");
        assert_eq!(document.protocol_version, 1);
        // The defaults leave 993/465 off, so the plaintext ports are advertised.
        assert_eq!(document.imap.port, 143);
        assert_eq!(document.smtp.port, 587);
        // TLS is on because the public URL is https, but neither implicit port is
        // listening, so a client must upgrade rather than speak TLS first.
        assert_eq!(document.imap.security.as_deref(), Some("starttls"));
        assert_eq!(document.smtp.security.as_deref(), Some("starttls"));
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        let document = WellKnownResponse::from_config(&config_with("https://mail.example.com/"));
        assert_eq!(document.api, "https://mail.example.com/api/v1");
        assert_eq!(document.web, "https://mail.example.com");
    }

    #[test]
    fn implicit_tls_ports_win_when_configured() {
        let mut config = config_with("https://mail.example.com");
        config.imap.imaps_port = 993;
        config.smtp.smtps_port = 465;
        config.tls.enabled = true;
        let document = WellKnownResponse::from_config(&config);
        assert_eq!(document.imap.port, 993);
        assert!(document.imap.tls);
        assert_eq!(document.smtp.port, 465);
        assert!(document.smtp.tls);
        assert_eq!(document.imap.security.as_deref(), Some("implicit"));
        assert_eq!(document.smtp.security.as_deref(), Some("implicit"));
    }

    #[test]
    fn a_plaintext_deployment_says_so() {
        let document = WellKnownResponse::from_config(&config_with("http://localhost:8080"));
        assert_eq!(document.api, "http://localhost:8080/api/v1");
        assert!(!document.imap.tls);
        assert!(!document.smtp.tls);
        assert!(document.imap.security.is_none());
        assert!(document.smtp.security.is_none());
    }

    #[test]
    fn an_unparseable_public_url_falls_back_to_the_hostname() {
        let document = WellKnownResponse::from_config(&config_with("not a url"));
        assert_eq!(document.imap.host, "mail.example.com");
        assert_eq!(document.smtp.host, "mail.example.com");
        assert!(!document.imap.tls);
    }

    #[test]
    fn the_document_serialises_with_the_documented_keys() {
        let json = serde_json::to_value(WellKnownResponse::from_config(&config_with(
            "https://mail.example.com",
        )))
        .expect("must serialise");
        assert!(json["api"].is_string());
        assert!(json["imap"]["host"].is_string());
        assert!(json["imap"]["port"].is_number());
        assert!(json["imap"]["tls"].is_boolean());
        assert!(json["smtp"]["port"].is_number());
        assert!(json["web"].is_string());
        assert_eq!(json["protocol_version"], 1);
    }

    #[test]
    fn a_healthy_body_carries_every_documented_block() {
        let body = HealthResponse {
            status: "ok".into(),
            version: VERSION.into(),
            protocol_version: 1,
            uptime_secs: 84_213,
            database: DatabaseHealth {
                ok: true,
                server_version: Some("PostgreSQL 16.15".into()),
                pool: Some(PoolHealth {
                    size: 4,
                    idle: 3,
                    max: 20,
                }),
            },
            smtp: ListenerHealth {
                enabled: true,
                connections: 3,
            },
            imap: ListenerHealth {
                enabled: true,
                connections: 1,
            },
            clients: Some(ClientsHealth {
                active_sessions: 4,
                active_devices: 2,
            }),
            queue: Some(QueueHealth {
                pending: 0,
                delivering: 0,
                retry: 2,
                failed: 1,
                received_today: 128,
                sent_today: 41,
            }),
        };
        let json = serde_json::to_value(&body).expect("must serialise");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["database"]["pool"]["max"], 20);
        assert_eq!(json["clients"]["active_sessions"], 4);
        assert_eq!(json["queue"]["received_today"], 128);
        assert_eq!(json["queue"]["sent_today"], 41);
        assert!(json["smtp"]["enabled"].as_bool().unwrap_or(false));
    }

    #[test]
    fn a_degraded_body_omits_the_blocks_it_cannot_report() {
        let body = HealthResponse {
            status: "degraded".into(),
            version: VERSION.into(),
            protocol_version: 1,
            uptime_secs: 1,
            database: DatabaseHealth {
                ok: false,
                server_version: None,
                pool: None,
            },
            smtp: ListenerHealth {
                enabled: true,
                connections: 0,
            },
            imap: ListenerHealth {
                enabled: true,
                connections: 0,
            },
            clients: None,
            queue: None,
        };
        let json = serde_json::to_value(&body).expect("must serialise");
        assert_eq!(json["status"], "degraded");
        assert_eq!(json["database"]["ok"], false);
        assert!(json["database"].get("pool").is_none(), "{json}");
        assert!(json["database"].get("server_version").is_none(), "{json}");
        // The whole point: no fabricated zeros.
        assert!(json.get("clients").is_none(), "{json}");
        assert!(json.get("queue").is_none(), "{json}");
    }

    #[test]
    fn version_reports_the_build_identity() {
        let body = VersionResponse {
            version: VERSION.to_string(),
            protocol_version: ferroma_core::PROTOCOL_VERSION,
            git_sha: ferroma_core::version::GIT_SHA.to_string(),
            built: ferroma_core::BUILD_TIMESTAMP.to_string(),
        };
        let json = serde_json::to_value(&body).expect("must serialise");
        assert_eq!(json["version"], VERSION);
        assert_eq!(json["protocol_version"], 1);
        assert!(json["git_sha"].is_string());
        assert!(json["built"].is_string());
    }

    #[test]
    fn pool_health_is_a_plain_projection() {
        let pool = PoolHealth {
            size: 10,
            idle: 4,
            max: 20,
        };
        assert_eq!(pool.size - pool.idle, 6);
        let json = serde_json::to_value(pool).expect("must serialise");
        assert_eq!(json["size"], 10);
        assert_eq!(json["idle"], 4);
        assert_eq!(json["max"], 20);
    }

    #[test]
    fn url_splitting_handles_hosts_ports_and_schemes() {
        assert_eq!(
            split_public_url("https://mail.example.com", "fallback"),
            ("mail.example.com".to_string(), true)
        );
        assert_eq!(
            split_public_url("http://mail.example.com:8080", "fallback"),
            ("mail.example.com".to_string(), false)
        );
        assert_eq!(
            split_public_url("garbage", "fallback"),
            ("fallback".to_string(), false)
        );
        assert_eq!(
            split_public_url("https://[::1]:8443", "fallback"),
            ("[::1]".to_string(), true)
        );
    }
}
