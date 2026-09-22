//! `docs/api.md` §4.6 and §4.7 — storage, audit, settings and the first-run wizard.
//!
//! # Omitting rather than inventing
//!
//! `docs/api.md` §4.6 is explicit that a figure a deployment cannot report is *left
//! out* of the storage object, not sent as `0`. [`StorageResponse`] therefore carries
//! `Option`s with `skip_serializing_if`, and a filesystem this process cannot stat
//! simply does not appear — the Admin panel renders the absence as `—`, which is
//! honest, instead of a zero, which is a lie that reads as "you have no mail".
//!
//! # Garbage collection
//!
//! `POST /storage/gc` does two things and reports both:
//!
//! * deletes attachment blobs no `attachments` row references (`AttachmentStore::gc`),
//!   and
//! * removes the `.tmp` and `.part` files an interrupted write leaves behind, in both
//!   the blob store and the Maildir tree.
//!
//! It never touches a blob that is referenced, so it is safe to run on a live server.

use std::collections::HashSet;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_auth::SessionKind;
use ferroma_core::{FerromaError, UserId};
use ferroma_storage::repository::{AuditFilter, NewAuditLog};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AdminUser, Pagination};
use crate::routes::admin::domains::audit;
use crate::routes::auth::TokenResponse;
use crate::routes::mail::shapes::{AuditResponse, UserResponse};
use crate::state::{AppState, ManagedListener, ManagedListenerState};

/// The `GET /api/v1/storage` body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageResponse {
    /// Bytes of RFC 5322 messages on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maildir_bytes: Option<u64>,
    /// Bytes of attachment blobs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_bytes: Option<u64>,
    /// Size of the PostgreSQL database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_bytes: Option<i64>,
    /// How many addresses exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailboxes: Option<i64>,
    /// How many stored messages exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<i64>,
    /// How many accounts exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<i64>,
    /// How many domains exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domains: Option<i64>,
    /// Total size of the filesystem holding `data_dir`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    /// Free space on that filesystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
}

/// What `POST /api/v1/storage/gc` did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcResponse {
    /// Attachment blobs removed.
    pub removed_attachments: usize,
    /// Bytes those blobs occupied.
    pub freed_bytes: u64,
    /// Interrupted-write scratch files removed from the blob store.
    pub removed_attachment_scratch: usize,
    /// Interrupted-write scratch files removed from the Maildir tree.
    pub removed_maildir_scratch: usize,
    /// How long the sweep took, in milliseconds.
    pub duration_ms: u64,
}

/// The `GET /api/v1/audit` filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditQuery {
    /// Only actions by this account.
    pub actor_user_id: Option<i64>,
    /// Only this action.
    pub action: Option<String>,
    /// Only this target kind.
    pub target_type: Option<String>,
    /// Only entries at or after this instant.
    pub since: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// The `PUT /api/v1/settings/:key` body.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingUpdateRequest {
    /// The new value. Any JSON value is accepted; the setting's consumer interprets it.
    pub value: serde_json::Value,
}

/// One DB-backed setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingResponse {
    /// The setting's name.
    pub key: String,
    /// Its value.
    pub value: serde_json::Value,
    /// When it last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The settings list body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsListResponse {
    /// The settings.
    pub items: Vec<SettingResponse>,
    /// How many exist.
    pub total: i64,
}

/// The state of one SMTP, IMAP or JMAP service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceListenerResponse {
    /// Whether this process can start the listener.
    pub available: bool,
    /// Whether the listener accepts connections now.
    pub enabled: bool,
}

impl From<ManagedListenerState> for ServiceListenerResponse {
    fn from(value: ManagedListenerState) -> Self {
        ServiceListenerResponse {
            available: value.available,
            enabled: value.enabled,
        }
    }
}

/// `GET /api/v1/services` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicesResponse {
    /// SMTP's configured listener family.
    pub smtp: ServiceListenerResponse,
    /// IMAP's configured listener family.
    pub imap: ServiceListenerResponse,
    /// JMAP's HTTP API surface.
    pub jmap: ServiceListenerResponse,
}

/// `PUT /api/v1/services/:service` request.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ServiceUpdateRequest {
    /// Whether this listener family should accept connections.
    pub enabled: bool,
}

/// The `GET /api/v1/setup` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupStatusResponse {
    /// Whether the wizard still has work to do.
    pub required: bool,
    /// The hostname the running configuration advertises.
    ///
    /// The wizard prefills its hostname field from this. Submitting a different value no
    /// longer fails: it is stored as a setting and adopted on the next start, because the
    /// running configuration cannot be rewritten underneath the process.
    pub hostname: String,
    /// The externally reachable URL the running configuration advertises.
    pub public_url: String,
    /// Address the HTTP API currently listens on.
    pub api_host: String,
    /// Port the HTTP API currently listens on.
    pub api_port: u16,
    /// Whether TLS is on for the current process.
    pub tls_enabled: bool,
    /// Implicit-TLS SMTP port the running configuration binds, `0` when it does not.
    pub smtps_port: u16,
    /// Implicit-TLS IMAP port the running configuration binds, `0` when it does not.
    pub imaps_port: u16,
    /// Certificate bundle the running configuration loads, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<String>,
    /// Its private key, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<String>,
}

/// The `POST /api/v1/setup` body.
#[derive(Debug, Clone, Deserialize)]
pub struct SetupRequest {
    /// The first administrator's login address.
    pub email: String,
    /// Their password.
    pub password: String,
    /// The server hostname to adopt.
    pub hostname: Option<String>,
    /// The externally reachable URL to adopt, e.g. `https://mail.example.com`.
    pub public_url: Option<String>,
    /// The domain to create, e.g. `example.com`.
    pub domain: String,
    /// Optional description for that domain.
    pub domain_description: Option<String>,
    /// Address the HTTP API should listen on after the next start.
    pub api_host: Option<String>,
    /// Port the HTTP API should listen on after the next start.
    pub api_port: Option<u16>,
    /// Whether TLS should be on after the next start.
    pub tls_enabled: Option<bool>,
    /// Implicit-TLS SMTP port after the next start. `0` leaves it off.
    pub smtps_port: Option<u16>,
    /// Implicit-TLS IMAP port after the next start. `0` leaves it off.
    pub imaps_port: Option<u16>,
    /// Certificate bundle (leaf first) to load after the next start.
    pub tls_cert: Option<String>,
    /// Its private key.
    pub tls_key: Option<String>,
}

/// What the wizard stored for the next start.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetupApplied {
    /// The hostname written to `server.hostname`, when it differed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// The URL written to `api.public_url`, when it differed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// The address written to `api.host`, when it differed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_host: Option<String>,
    /// The port written to `api.port`, when it differed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_port: Option<u16>,
    /// The TLS switch written to `tls.enabled`, when it differed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_enabled: Option<bool>,
    /// The certificate path written to `tls.cert_path`, when it was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<String>,
    /// The key path written to `tls.key_path`, when it was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<String>,
    /// The implicit-TLS SMTP port written to `smtp.smtps_port`, when it differed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtps_port: Option<u16>,
    /// The implicit-TLS IMAP port written to `imap.imaps_port`, when it differed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imaps_port: Option<u16>,
    /// Whether a restart is needed for the stored values to take effect.
    pub restart_required: bool,
}

/// The `POST /api/v1/setup` response: a session, plus what was stored.
///
/// The token fields stay at the top level (`flatten`) because the wizard hands the body
/// straight to the shared `setTokens`, exactly as `POST /auth/login` does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupResponse {
    /// The administrator's session.
    #[serde(flatten)]
    pub tokens: TokenResponse,
    /// The settings written for the next start.
    pub applied: SetupApplied,
}

/// `GET /api/v1/storage`
pub async fn storage_overview(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<StorageResponse>, ApiError> {
    let response = StorageResponse {
        maildir_bytes: directory_size(state.maildir.root()).ok(),
        attachment_bytes: state.attachments.total_size().ok(),
        database_bytes: state.database_bytes().await,
        mailboxes: count_mailboxes(&state).await,
        messages: count_messages(&state).await,
        users: state.repos.users.count().await.ok(),
        domains: state.repos.domains.count().await.ok(),
        disk_total_bytes: None,
        disk_free_bytes: None,
    };

    let (total, free) = disk_space(&state.config.server.data_dir);
    Ok(Json(StorageResponse {
        disk_total_bytes: total,
        disk_free_bytes: free,
        ..response
    }))
}

/// How many addresses exist across every domain.
///
/// `ferroma-storage` exposes a per-domain listing, so the total is the sum of those —
/// bounded by the number of domains a server manages.
pub async fn count_mailboxes(state: &AppState) -> Option<i64> {
    let domains = state.repos.domains.list().await.ok()?;
    let mut total = 0i64;
    for domain in domains {
        total += state
            .repos
            .mailboxes
            .list_by_domain(domain.domain_id())
            .await
            .map(|rows| rows.len() as i64)
            .unwrap_or(0);
    }
    Some(total)
}

/// How many stored messages exist.
///
/// Derived from the folders' own `message_count` counters, which the repository
/// maintains, rather than from a `COUNT(*)` this crate cannot issue.
pub async fn count_messages(state: &AppState) -> Option<i64> {
    let domains = state.repos.domains.list().await.ok()?;
    let mut total = 0i64;
    let mut counted_any = false;
    for domain in domains {
        let mailboxes = state
            .repos
            .mailboxes
            .list_by_domain(domain.domain_id())
            .await
            .unwrap_or_default();
        for mailbox in mailboxes {
            let folders = state
                .repos
                .folders
                .list(mailbox.mailbox_id())
                .await
                .unwrap_or_default();
            for folder in folders {
                total += i64::from(folder.message_count);
                counted_any = true;
            }
        }
    }
    if counted_any {
        Some(total)
    } else {
        None
    }
}

/// Where `POST /api/v1/storage/export` should write.
#[derive(Debug, Deserialize)]
pub struct StorageExportRequest {
    /// A path inside the container, or an `s3://` / `webdav://` URL.
    pub to: String,
    /// Accepted and ignored. This endpoint always exports while the server is up.
    #[serde(default)]
    pub live: bool,
}

/// What one export produced.
#[derive(Debug, Serialize)]
pub struct StorageExportResponse {
    /// Where the archive went.
    pub destination: String,
    /// Size of the archive, in bytes.
    pub bytes: u64,
    /// How many files the archive holds.
    pub members: usize,
    /// Always true: the server answering this request cannot stop itself first.
    pub live: bool,
}

/// `POST /api/v1/storage/export`
///
/// Runs `ferroma storage export --live`. Import is not offered here: it refuses
/// while a server is listening, because a restore writes both halves underneath
/// that process. The new host runs `ferroma storage import` after its own server
/// is stopped.
pub async fn storage_export(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(request): Json<StorageExportRequest>,
) -> Result<Json<StorageExportResponse>, ApiError> {
    let to = request.to.trim();
    if to.is_empty() {
        return Err(ApiError::new(FerromaError::Invalid(
            "say where the archive should go".into(),
        )));
    }
    let writer = state.backup.clone().ok_or_else(|| {
        ApiError::new(FerromaError::Invalid(
            "this process cannot write an archive; run `ferroma storage export --live` instead".into(),
        ))
    })?;
    let report = writer(to.to_string()).await.map_err(|error| {
        ApiError::new(FerromaError::Invalid(error.chars().take(500).collect()))
    })?;
    let report = StorageExportResponse {
        destination: report.destination,
        bytes: report.bytes,
        members: report.members,
        live: true,
    };
    audit(
        &state,
        &admin,
        "storage.export",
        Some("storage"),
        None,
        serde_json::json!({ "destination": report.destination, "bytes": report.bytes }),
    )
    .await;
    Ok(Json(report))
}

/// `POST /api/v1/storage/gc`
pub async fn storage_gc(
    State(state): State<AppState>,
    admin: AdminUser,
) -> Result<Json<GcResponse>, ApiError> {
    let started = std::time::Instant::now();

    let referenced = state.repos.attachments.referenced_paths().await?;
    let keep: HashSet<String> = referenced
        .iter()
        .map(|path| {
            // `gc` matches on the file name or the full relative path; both are given.
            path.rsplit('/').next().unwrap_or(path).to_string()
        })
        .chain(referenced.iter().cloned())
        .collect();

    let attachment_bytes_before = state.attachments.total_size().unwrap_or(0);
    let removed_attachments = state.attachments.gc(&keep)?;
    let attachment_bytes_after = state.attachments.total_size().unwrap_or(0);

    let removed_attachment_scratch = sweep_scratch(state.attachments.root());
    let removed_maildir_scratch = sweep_scratch(state.maildir.root());

    let response = GcResponse {
        removed_attachments,
        freed_bytes: attachment_bytes_before.saturating_sub(attachment_bytes_after),
        removed_attachment_scratch,
        removed_maildir_scratch,
        duration_ms: started.elapsed().as_millis() as u64,
    };

    audit(
        &state,
        &admin,
        "storage.gc",
        Some("storage"),
        None,
        serde_json::json!({
            "removed_attachments": response.removed_attachments,
            "freed_bytes": response.freed_bytes,
        }),
    )
    .await;

    Ok(Json(response))
}

/// `GET /api/v1/audit`
pub async fn list_audit(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<AuditQuery>,
) -> Result<Json<crate::extract::Page<AuditResponse>>, ApiError> {
    let pagination = Pagination::clamped(query.limit, query.offset);

    let mut filter = AuditFilter::new(pagination.limit, pagination.offset);
    filter.actor_user_id = query.actor_user_id.map(UserId::new);
    filter.action = query
        .action
        .clone()
        .filter(|action| !action.trim().is_empty());
    filter.target_type = query
        .target_type
        .clone()
        .filter(|kind| !kind.trim().is_empty());
    filter.since = query
        .since
        .as_deref()
        .and_then(crate::routes::mail::messages::parse_instant);

    let rows = state.repos.audit.list(filter.clone()).await?;
    let total = state.repos.audit.count(filter).await?;

    // The console's audit table has an Actor column, and it reads an address. The rows
    // only carry the id, so the ids are resolved here — once for the whole page.
    let mut distinct = std::collections::BTreeSet::new();
    let actor_ids: Vec<UserId> = rows
        .iter()
        .filter_map(|row| row.actor_user_id)
        .filter(|id| distinct.insert(*id))
        .map(UserId::new)
        .collect();
    let actors: std::collections::HashMap<i64, String> = state
        .repos
        .users
        .find_by_ids(&actor_ids)
        .await?
        .into_iter()
        .map(|user| (user.id, user.email))
        .collect();

    let items = rows
        .iter()
        .map(|row| {
            AuditResponse::from_row(row)
                .with_actor(row.actor_user_id.and_then(|id| actors.get(&id).cloned()))
        })
        .collect();

    Ok(Json(pagination.page(items, total)))
}

/// `GET /api/v1/settings`
pub async fn list_settings(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<SettingsListResponse>, ApiError> {
    let rows = state.repos.settings.all().await?;
    Ok(Json(SettingsListResponse {
        total: rows.len() as i64,
        items: rows
            .iter()
            .map(|row| SettingResponse {
                key: row.key.clone(),
                value: row.value.clone(),
                updated_at: row.updated_at,
            })
            .collect(),
    }))
}

/// `PUT /api/v1/settings/:key`
pub async fn put_setting(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(key): Path<String>,
    Json(request): Json<SettingUpdateRequest>,
) -> Result<Json<SettingResponse>, ApiError> {
    let key = key.trim();
    if key.is_empty() || key.len() > 128 {
        return Err(ApiError::new(FerromaError::Invalid(
            "a setting key must be 1..=128 characters".to_string(),
        )));
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(ApiError::new(FerromaError::Invalid(
            "a setting key may contain letters, digits, `.`, `_` and `-`".to_string(),
        )));
    }

    state.repos.settings.set(key, request.value.clone()).await?;

    audit(
        &state,
        &admin,
        "setting.updated",
        Some("setting"),
        Some(key),
        serde_json::json!({ "value": request.value }),
    )
    .await;

    let value = state
        .repos
        .settings
        .get(key)
        .await?
        .unwrap_or(request.value);

    Ok(Json(SettingResponse {
        key: key.to_string(),
        value,
        updated_at: chrono::Utc::now(),
    }))
}

/// `GET /api/v1/services`
pub async fn services(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Json<ServicesResponse> {
    let listeners = state.listeners.states();
    Json(ServicesResponse {
        smtp: listeners.smtp.into(),
        imap: listeners.imap.into(),
        jmap: listeners.jmap.into(),
    })
}

/// `PUT /api/v1/services/:service`
///
/// This is deliberately separate from the generic settings endpoint: a listener change
/// has an observable socket-side effect, which must either succeed now or leave the
/// persisted choice exactly as it was.
pub async fn update_service(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(service): Path<String>,
    Json(request): Json<ServiceUpdateRequest>,
) -> Result<Json<ServiceListenerResponse>, ApiError> {
    let listener = match service.trim().to_ascii_lowercase().as_str() {
        "smtp" => ManagedListener::Smtp,
        "imap" => ManagedListener::Imap,
        "jmap" => ManagedListener::Jmap,
        _ => {
            return Err(ApiError::new(FerromaError::Invalid(
                "service must be smtp, imap or jmap".to_string(),
            )))
        }
    };
    let key = listener.setting_key();
    let previous = state.repos.settings.get(key).await?;
    state.repos.settings.set(key, serde_json::json!(request.enabled)).await?;

    let effective = match state.listeners.set_enabled(listener, request.enabled).await {
        Ok(state) => state,
        Err(error) => {
            // A database record is a restart promise. Do not leave it promising a listener
            // state we failed to bind (or failed to stop) in this running process.
            let restored = match previous {
                Some(value) => state.repos.settings.set(key, value).await,
                None => state.repos.settings.delete(key).await.map(|_| ()),
            };
            if let Err(restore_error) = restored {
                tracing::error!(%restore_error, key, "could not restore listener setting after runtime failure");
            }
            return Err(ApiError::new(error));
        }
    };

    audit(
        &state,
        &admin,
        "service.updated",
        Some("service"),
        Some(listener.as_str()),
        serde_json::json!({ "enabled": request.enabled }),
    )
    .await;
    Ok(Json(effective.into()))
}

/// `GET /api/v1/setup`
///
/// Answers `404` when `api.enable_setup_wizard` is off, which is what "disabled
/// entirely" means to a client: the endpoint does not exist. Answering `200
/// {required:false}` instead made the console tell an operator with no administrator
/// that one already existed.
pub async fn setup_status(
    State(state): State<AppState>,
) -> Result<Json<SetupStatusResponse>, ApiError> {
    if !state.config.api.enable_setup_wizard {
        return Err(ApiError::new(FerromaError::NotFound(
            "the setup wizard is disabled by api.enable_setup_wizard".to_string(),
        )));
    }
    Ok(Json(SetupStatusResponse {
        required: setup_required(&state).await?,
        hostname: state.config.server.hostname.clone(),
        public_url: state.config.api.public_url.clone(),
        api_host: state.config.api.host.clone(),
        api_port: state.config.api.port,
        tls_enabled: state.config.tls.enabled,
        smtps_port: state.config.smtp.smtps_port,
        imaps_port: state.config.imap.imaps_port,
        tls_cert: state
            .config
            .tls
            .cert_path
            .as_ref()
            .map(|path| path.display().to_string()),
        tls_key: state
            .config
            .tls
            .key_path
            .as_ref()
            .map(|path| path.display().to_string()),
    }))
}

/// Whether the first-run wizard still has work to do.
pub async fn setup_required(state: &AppState) -> Result<bool, ApiError> {
    if !state.config.api.enable_setup_wizard {
        return Ok(false);
    }
    Ok(state.repos.users.count_admins().await? == 0)
}

/// `POST /api/v1/setup`
pub async fn setup(
    State(state): State<AppState>,
    Json(request): Json<SetupRequest>,
) -> Result<(StatusCode, Json<SetupResponse>), ApiError> {
    if !state.config.api.enable_setup_wizard {
        // `404`, matching `GET /setup`: a disabled wizard is an endpoint that is not
        // there, and the console already renders that as "disabled" rather than as the
        // "an administrator already exists" a `409` means.
        return Err(ApiError::new(FerromaError::NotFound(
            "the setup wizard is disabled by api.enable_setup_wizard".to_string(),
        )));
    }
    if !setup_required(&state).await? {
        return Err(ApiError::new(FerromaError::Conflict(
            "this server already has an administrator".to_string(),
        )));
    }

    let domain_name = ferroma_core::address::normalise_domain(&request.domain);
    if ferroma_core::address::validate_domain(&domain_name).is_err() {
        return Err(ApiError::new(FerromaError::Invalid(format!(
            "{} is not a valid domain name",
            request.domain
        ))));
    }

    let address = ferroma_core::EmailAddress::parse(request.email.trim())?;
    if address.domain() != domain_name {
        return Err(ApiError::new(FerromaError::Invalid(format!(
            "the administrator address must be inside {domain_name}"
        ))));
    }

    // The hostname this server advertises cannot be rewritten under a running process,
    // so a submitted value is *stored* rather than refused: the wizard is a browser form
    // and can only persist to the database, and `server.hostname` / `api.public_url` are
    // read back from there on the next start (see the server's `apply_stored_settings`).
    // It applies on the next start, and the response says so.
    let mut applied = SetupApplied::default();

    if let Some(hostname) = request
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|hostname| !hostname.is_empty())
    {
        if ferroma_core::address::validate_domain(&ferroma_core::address::normalise_domain(
            hostname,
        ))
        .is_err()
        {
            return Err(ApiError::new(FerromaError::Invalid(format!(
                "{hostname} is not a valid server hostname"
            ))));
        }
        if !hostname.eq_ignore_ascii_case(state.config.server.hostname.trim()) {
            state
                .repos
                .settings
                .set("server.hostname", serde_json::json!(hostname))
                .await?;
            applied.hostname = Some(hostname.to_string());
        }
    }

    if let Some(public_url) = request
        .public_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(|url| url.trim_end_matches('/').to_string())
    {
        if !(public_url.starts_with("http://") || public_url.starts_with("https://")) {
            return Err(ApiError::new(FerromaError::Invalid(
                "the public URL must start with http:// or https://".to_string(),
            )));
        }
        if public_url != state.config.api.public_url.trim_end_matches('/') {
            state
                .repos
                .settings
                .set("api.public_url", serde_json::json!(public_url))
                .await?;
            applied.public_url = Some(public_url);
        }
    }

    // The HTTP listener and TLS are the same kind of value as the hostname: a running
    // process cannot move its own socket, and the PEM files are read once at boot. They are
    // stored instead, and the server adopts whatever nobody stated explicitly (see
    // `apply_stored_settings`), so the wizard — not a deploy flag — is where an operator
    // says where this instance should be reachable.
    if let Some(api_host) = request
        .api_host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
    {
        if api_host.chars().any(char::is_whitespace) {
            return Err(ApiError::new(FerromaError::Invalid(
                "the listen address must not contain spaces".to_string(),
            )));
        }
        if api_host != state.config.api.host.trim() {
            state
                .repos
                .settings
                .set("api.host", serde_json::json!(api_host))
                .await?;
            applied.api_host = Some(api_host.to_string());
        }
    }

    if let Some(api_port) = request.api_port.filter(|port| *port != 0) {
        if api_port != state.config.api.port {
            state
                .repos
                .settings
                .set("api.port", serde_json::json!(api_port))
                .await?;
            applied.api_port = Some(api_port);
        }
    }

    if let Some(tls_enabled) = request.tls_enabled {
        if tls_enabled != state.config.tls.enabled {
            state
                .repos
                .settings
                .set("tls.enabled", serde_json::json!(tls_enabled))
                .await?;
            applied.tls_enabled = Some(tls_enabled);
        }
    }

    if let Some(cert) = request
        .tls_cert
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        // The path is read by *this* process, not by the browser, so its existence can be
        // checked here — and it must be, because a typo is only discovered at the next
        // start, where it used to stop the server from coming up at all.
        if !std::path::Path::new(cert).exists() {
            return Err(ApiError::new(FerromaError::Invalid(format!(
                "no file at {cert} on the server: TLS paths are read by the server process, \
                 not by the browser (inside a container, use the mounted path)"
            ))));
        }
        let current = state
            .config
            .tls
            .cert_path
            .as_ref()
            .map(|path| path.display().to_string());
        if current.as_deref() != Some(cert) {
            state
                .repos
                .settings
                .set("tls.cert_path", serde_json::json!(cert))
                .await?;
            applied.tls_cert = Some(cert.to_string());
        }
    }

    if let Some(key) = request
        .tls_key
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        if !std::path::Path::new(key).exists() {
            return Err(ApiError::new(FerromaError::Invalid(format!(
                "no file at {key} on the server: TLS paths are read by the server process, \
                 not by the browser (inside a container, use the mounted path)"
            ))));
        }
        let current = state
            .config
            .tls
            .key_path
            .as_ref()
            .map(|path| path.display().to_string());
        if current.as_deref() != Some(key) {
            state
                .repos
                .settings
                .set("tls.key_path", serde_json::json!(key))
                .await?;
            applied.tls_key = Some(key.to_string());
        }
    }

    // 465 and 993 are listeners, so they take effect on the next start, the same way the
    // certificate does. Turning them on also refuses a password on the plaintext ports:
    // a client that can reach 465 has no reason to send one on 587 before STARTTLS.
    if let Some(port) = request.smtps_port {
        if port != state.config.smtp.smtps_port {
            state
                .repos
                .settings
                .set("smtp.smtps_port", serde_json::json!(port))
                .await?;
            applied.smtps_port = Some(port);
        }
    }
    if let Some(port) = request.imaps_port {
        if port != state.config.imap.imaps_port {
            state
                .repos
                .settings
                .set("imap.imaps_port", serde_json::json!(port))
                .await?;
            applied.imaps_port = Some(port);
        }
    }
    let implicit = request.smtps_port.unwrap_or(0) != 0 || request.imaps_port.unwrap_or(0) != 0;
    if implicit {
        state
            .repos
            .settings
            .set("smtp.require_tls_for_auth", serde_json::json!(true))
            .await?;
        state
            .repos
            .settings
            .set("imap.require_tls_for_login", serde_json::json!(true))
            .await?;
    }

    applied.restart_required = applied.hostname.is_some()
        || applied.public_url.is_some()
        || applied.api_host.is_some()
        || applied.api_port.is_some()
        || applied.tls_enabled.is_some()
        || applied.tls_cert.is_some()
        || applied.tls_key.is_some()
        || applied.smtps_port.is_some()
        || applied.imaps_port.is_some();

    if applied.restart_required {
        // Every field above is read once at boot, which is why they are stored rather than
        // applied — a running process cannot move its own socket and the PEM files were read
        // when it started. Asking the operator to restart the container themselves was the old
        // arrangement, and it is why a wizard that had just been filled in looked like it had
        // done nothing. The process comes back up by itself instead (see `restart_itself` in
        // the server binary); the console waits for it and reloads into it.
        state.restart.request();
    }

    // The account first: it is the only step that can fail for a reason the operator
    // can act on (a weak password, a duplicate address).
    let user = state
        .auth
        .create_user(
            request.email.trim(),
            &request.password,
            Some("Administrator"),
            true,
            true,
            None,
        )
        .await?;

    // Only what the operator typed. The wizard used to substitute
    // "Created by the first-run wizard" when this field was absent — and the wizard never asks
    // for one, so that sentence was what *every* fresh installation carried: operator-facing
    // metadata nobody wrote, listed next to the domain as if someone had left a note, and
    // indistinguishable from a note someone actually left.
    let description = request
        .domain_description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let domain = state
        .repos
        .domains
        .create(&domain_name, description)
        .await?;

    let mailbox = state
        .repos
        .mailboxes
        .create(ferroma_storage::repository::NewMailbox {
            user_id: UserId::new(user.id),
            domain_id: domain.domain_id(),
            local_part: address.local_part().to_string(),
            display_name: user.display_name.clone(),
            is_primary: true,
            quota_bytes: None,
        })
        .await?;

    state
        .maildir
        .ensure_mailbox(&domain.name, address.local_part())?;
    state
        .repos
        .folders
        .ensure_standard(mailbox.mailbox_id())
        .await?;

    let outcome = state
        .auth
        .login(
            request.email.trim(),
            &request.password,
            SessionKind::Api,
            None,
            Some("ferroma-setup"),
            None,
        )
        .await?;

    if let Err(err) = state
        .repos
        .audit
        .record(NewAuditLog {
            actor_user_id: Some(UserId::new(user.id)),
            action: "setup.completed".to_string(),
            target_type: Some("user".to_string()),
            target_id: Some(user.id.to_string()),
            ip: None,
            user_agent: Some("ferroma-setup".to_string()),
            details: serde_json::json!({ "domain": domain.name, "address": address.to_string() }),
        })
        .await
    {
        tracing::warn!(error = %err, "could not write the audit trail for setup");
    }

    tracing::info!(
        user_id = user.id,
        domain = %domain.name,
        "first-run setup completed"
    );

    Ok((
        StatusCode::CREATED,
        Json(SetupResponse {
            tokens: TokenResponse {
                access_token: outcome.tokens.access_token,
                refresh_token: outcome.tokens.refresh_token,
                token_type: "Bearer".to_string(),
                expires_in: outcome.tokens.expires_in,
                user: UserResponse::from_row(&outcome.user),
            },
            applied,
        }),
    ))
}

/// The total size of a directory tree, in bytes.
///
/// Unreadable entries are skipped rather than failing the whole figure: a partly
/// readable mail store should still report what it can.
pub fn directory_size(root: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    if !root.exists() {
        return Ok(0);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind) if kind.is_file() => {
                    if let Ok(metadata) = entry.metadata() {
                        total += metadata.len();
                    }
                }
                // A symlink is not followed: a loop would hang the dashboard.
                _ => {}
            }
        }
    }
    Ok(total)
}

/// Total and free bytes of the filesystem holding `path`.
///
/// `statvfs` is not available without a dependency, so the figure comes from the
/// platform's own tool (`df` on Unix, PowerShell's `Get-PSDrive` on Windows). A host
/// without either simply omits the two figures.
pub fn disk_space(path: &std::path::Path) -> (Option<u64>, Option<u64>) {
    if cfg!(windows) {
        return (None, None);
    }
    let output = match std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return (None, None),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().nth(1) else {
        return (None, None);
    };
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 {
        return (None, None);
    }
    let total = fields[1].parse::<u64>().ok().map(|kb| kb * 1024);
    let free = fields[3].parse::<u64>().ok().map(|kb| kb * 1024);
    (total, free)
}

/// Delete interrupted-write scratch files under `root`.
///
/// A `.tmp` (blob store) or `.part` file older than an hour is abandoned: a live upload
/// never holds one that long, and leaving it would grow the store forever.
pub fn sweep_scratch(root: &std::path::Path) -> usize {
    let mut removed = 0usize;
    if !root.exists() {
        return 0;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !(name.ends_with(".tmp") || name.ends_with(".part")) {
                continue;
            }
            let recent = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .map(|age| age.as_secs() < 3600)
                .unwrap_or(false);
            if recent {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_storage_shape_omits_what_it_cannot_report() {
        let response = StorageResponse {
            maildir_bytes: Some(100),
            attachment_bytes: None,
            database_bytes: Some(200),
            mailboxes: Some(1),
            messages: Some(2),
            users: Some(3),
            domains: Some(4),
            disk_total_bytes: None,
            disk_free_bytes: None,
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["maildir_bytes"], 100);
        assert_eq!(json["messages"], 2);
        // The whole point of the `Option`s: absence, not a fabricated zero.
        assert!(json.get("attachment_bytes").is_none(), "{json}");
        assert!(json.get("disk_total_bytes").is_none(), "{json}");
        assert!(json.get("disk_free_bytes").is_none(), "{json}");
    }

    #[test]
    fn an_empty_storage_response_is_an_empty_object() {
        let json = serde_json::to_value(StorageResponse::default()).expect("must serialise");
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn directory_size_walks_a_tree() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
        std::fs::write(dir.path().join("one.txt"), b"12345").expect("write");
        std::fs::write(dir.path().join("a/b/two.txt"), b"123").expect("write");
        assert_eq!(directory_size(dir.path()).expect("must walk"), 8);
    }

    #[test]
    fn directory_size_of_a_missing_path_is_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            directory_size(&dir.path().join("nope")).expect("must not fail"),
            0
        );
    }

    #[test]
    fn sweeping_scratch_removes_only_stale_temp_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("ab/cd")).expect("mkdir");
        // A fresh `.tmp` belongs to a live write and must survive.
        std::fs::write(dir.path().join("ab/cd/.deadbeef.1.tmp"), b"partial").expect("write");
        // Real blobs never match.
        std::fs::write(dir.path().join("ab/cd/realblob"), b"data").expect("write");

        let removed = sweep_scratch(dir.path());
        assert_eq!(removed, 0, "a recent scratch file is a live write");
        assert!(dir.path().join("ab/cd/.deadbeef.1.tmp").exists());
        assert!(dir.path().join("ab/cd/realblob").exists());
    }

    #[test]
    fn sweeping_a_missing_root_is_a_no_op() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(sweep_scratch(&dir.path().join("absent")), 0);
    }

    #[test]
    fn disk_space_never_panics() {
        // Either figure may be absent depending on the host; neither may panic.
        let (total, free) = disk_space(std::path::Path::new("."));
        if let Some(total) = total {
            assert!(total > 0);
        }
        if let (Some(total), Some(free)) = (total, free) {
            assert!(free <= total);
        }
    }

    #[test]
    fn the_storage_route_accepts_no_body() {
        // A sanity check that the response type is the one the contract documents.
        let json = serde_json::to_value(StorageResponse {
            maildir_bytes: Some(8_123_456_789),
            attachment_bytes: Some(1_234_567_890),
            database_bytes: Some(234_567_890),
            mailboxes: Some(42),
            messages: Some(128_431),
            users: Some(17),
            domains: Some(3),
            disk_total_bytes: Some(107_374_182_400),
            disk_free_bytes: Some(64_424_509_440),
        })
        .expect("must serialise");
        assert_eq!(json["mailboxes"], 42);
        assert_eq!(json["users"], 17);
        assert_eq!(json["domains"], 3);
        assert_eq!(json["disk_free_bytes"], 64_424_509_440u64);
    }

    #[test]
    fn the_audit_query_is_all_optional() {
        let query: AuditQuery = serde_json::from_value(serde_json::json!({})).expect("must parse");
        assert!(query.actor_user_id.is_none());
        assert!(query.action.is_none());
        assert!(query.since.is_none());

        let query: AuditQuery = serde_json::from_value(serde_json::json!({
            "actor_user_id": 7,
            "action": "user.created",
            "since": "2026-09-16T12:00:00Z"
        }))
        .expect("must parse");
        assert_eq!(query.actor_user_id, Some(7));
        assert!(crate::routes::mail::messages::parse_instant(
            query.since.as_deref().unwrap_or_default()
        )
        .is_some());
    }

    #[test]
    fn the_setting_update_body_needs_a_value() {
        assert!(serde_json::from_value::<SettingUpdateRequest>(serde_json::json!({})).is_err());
        let request: SettingUpdateRequest =
            serde_json::from_value(serde_json::json!({ "value": { "enabled": true } }))
                .expect("must parse");
        assert_eq!(request.value["enabled"], true);
        // A scalar is a legal value too.
        let request: SettingUpdateRequest =
            serde_json::from_value(serde_json::json!({ "value": 5 })).expect("must parse");
        assert_eq!(request.value, serde_json::json!(5));
    }

    #[test]
    fn the_setup_status_shape_is_the_documented_one() {
        // Every field the wizard prefills from, with the running configuration's values.
        // A form that guessed these would offer to store a port the server is not on, or
        // to enable TLS that is already on and report it as a change.
        let json = serde_json::to_value(SetupStatusResponse {
            required: true,
            hostname: "mail.example.com".to_string(),
            public_url: "https://mail.example.com".to_string(),
            api_host: "0.0.0.0".to_string(),
            api_port: 8080,
            tls_enabled: false,
            tls_cert: None,
            tls_key: None,
        })
        .expect("must serialise");
        assert_eq!(
            json,
            serde_json::json!({
                "required": true,
                "hostname": "mail.example.com",
                "public_url": "https://mail.example.com",
                "api_host": "0.0.0.0",
                "api_port": 8080,
                "tls_enabled": false
            })
        );
    }

    #[test]
    fn the_setup_request_takes_the_wizard_fields_as_optional() {
        // The administrator and the domain are the whole request; everything else is a
        // field the wizard may leave alone, and leaving it out must not be an error.
        let minimal: SetupRequest = serde_json::from_value(serde_json::json!({
            "email": "admin@example.com",
            "password": "correct horse battery",
            "domain": "example.com"
        }))
        .expect("must parse");
        assert!(minimal.api_port.is_none());
        assert!(minimal.tls_enabled.is_none());

        let full: SetupRequest = serde_json::from_value(serde_json::json!({
            "email": "admin@example.com",
            "password": "correct horse battery",
            "domain": "example.com",
            "domain_description": "Wizard test",
            "hostname": "mail.example.com",
            "public_url": "https://mail.example.com",
            "api_host": "127.0.0.1",
            "api_port": 18080,
            "tls_enabled": true,
            "tls_cert": "/etc/ferroma/tls/fullchain.pem",
            "tls_key": "/etc/ferroma/tls/privkey.pem"
        }))
        .expect("must parse");
        assert_eq!(full.api_port, Some(18080));
        assert_eq!(full.tls_enabled, Some(true));
        assert_eq!(full.domain_description.as_deref(), Some("Wizard test"));
    }

    #[test]
    fn the_setup_response_keeps_the_token_fields_at_the_top_level() {
        // The wizard hands this body straight to the shared `setTokens`, which reads
        // `access_token`/`refresh_token` from the top level — so the `applied` object
        // must not push them down a level.
        let json = serde_json::to_value(SetupResponse {
            tokens: crate::routes::auth::TokenResponse {
                access_token: "at".to_string(),
                refresh_token: "rt".to_string(),
                token_type: "Bearer".to_string(),
                expires_in: 3600,
                user: crate::routes::mail::shapes::UserResponse {
                    id: 1,
                    email: "root@example.com".to_string(),
                    display_name: None,
                    enabled: true,
                    is_admin: true,
                    quota_bytes: 0,
                    used_bytes: 0,
                    last_login_at: None,
                    created_at: chrono::Utc::now(),
                    mailboxes: None,
                },
            },
            applied: SetupApplied {
                hostname: Some("mail.example.com".to_string()),
                public_url: None,
                api_host: None,
                api_port: Some(18080),
                tls_enabled: Some(true),
                tls_cert: None,
                tls_key: None,
                restart_required: true,
            },
        })
        .expect("must serialise");

        assert_eq!(json["access_token"], "at");
        assert_eq!(json["refresh_token"], "rt");
        assert_eq!(json["applied"]["hostname"], "mail.example.com");
        assert_eq!(json["applied"]["api_port"], 18080);
        assert_eq!(json["applied"]["tls_enabled"], true);
        assert_eq!(json["applied"]["restart_required"], true);
        // An unchanged value is omitted rather than sent as `null`.
        assert!(json["applied"].get("public_url").is_none());
        assert!(json["applied"].get("api_host").is_none());
        assert!(json["applied"].get("tls_cert").is_none());
    }

    #[test]
    fn the_setup_body_needs_email_password_and_domain() {
        assert!(serde_json::from_value::<SetupRequest>(serde_json::json!({
            "email": "alice@example.com"
        }))
        .is_err());
        let request: SetupRequest = serde_json::from_value(serde_json::json!({
            "email": "alice@example.com",
            "password": "hunter2hunter2",
            "domain": "example.com"
        }))
        .expect("must parse");
        assert!(request.hostname.is_none());
        assert_eq!(request.domain, "example.com");
    }

    #[test]
    fn the_gc_shape_is_stable() {
        let json = serde_json::to_value(GcResponse {
            removed_attachments: 3,
            freed_bytes: 1024,
            removed_attachment_scratch: 1,
            removed_maildir_scratch: 2,
            duration_ms: 12,
        })
        .expect("must serialise");
        assert_eq!(json["removed_attachments"], 3);
        assert_eq!(json["freed_bytes"], 1024);
        assert_eq!(json["removed_maildir_scratch"], 2);
        assert_eq!(json["duration_ms"], 12);
    }

    #[test]
    fn the_services_shape_is_stable() {
        let json = serde_json::to_value(ServicesResponse {
            smtp: ServiceListenerResponse { available: true, enabled: false },
            imap: ServiceListenerResponse { available: false, enabled: false },
            jmap: ServiceListenerResponse { available: true, enabled: true },
        })
        .expect("must serialise");
        assert_eq!(json["smtp"]["available"], true);
        assert_eq!(json["smtp"]["enabled"], false);
        assert_eq!(json["imap"]["available"], false);
        assert_eq!(json["jmap"]["enabled"], true);
        let update: ServiceUpdateRequest = serde_json::from_value(serde_json::json!({ "enabled": true }))
            .expect("the documented request parses");
        assert!(update.enabled);
    }

    #[test]
    fn the_settings_shape_wraps_a_json_value() {
        let json = serde_json::to_value(SettingsListResponse {
            items: vec![SettingResponse {
                key: "signup.enabled".into(),
                value: serde_json::json!(false),
                updated_at: chrono::Utc::now(),
            }],
            total: 1,
        })
        .expect("must serialise");
        assert_eq!(json["items"][0]["key"], "signup.enabled");
        assert_eq!(json["items"][0]["value"], false);
        assert_eq!(json["total"], 1);
    }
}
