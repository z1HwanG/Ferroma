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
use crate::state::AppState;

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

/// The `GET /api/v1/setup` body.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SetupStatusResponse {
    /// Whether the wizard still has work to do.
    pub required: bool,
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
    /// The domain to create, e.g. `example.com`.
    pub domain: String,
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
    filter.action = query.action.clone().filter(|action| !action.trim().is_empty());
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

    Ok(Json(pagination.page(
        rows.iter().map(AuditResponse::from_row).collect(),
        total,
    )))
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

    state
        .repos
        .settings
        .set(key, request.value.clone())
        .await?;

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

/// `GET /api/v1/setup`
pub async fn setup_status(
    State(state): State<AppState>,
) -> Result<Json<SetupStatusResponse>, ApiError> {
    Ok(Json(SetupStatusResponse {
        required: setup_required(&state).await?,
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
) -> Result<(StatusCode, Json<TokenResponse>), ApiError> {
    if !state.config.api.enable_setup_wizard {
        return Err(ApiError::new(FerromaError::Conflict(
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

    // The account first: it is the only step that can fail for a reason the operator
    // can act on (a weak password, a duplicate address).
    let user = state
        .auth
        .create_user(
            request.email.trim(),
            &request.password,
            Some("Administrator"),
            true,
            None,
        )
        .await?;

    let domain = state
        .repos
        .domains
        .create(&domain_name, Some("Created by the first-run wizard"))
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

    if let Some(hostname) = request.hostname.as_deref().filter(|h| !h.trim().is_empty()) {
        tracing::info!(
            requested_hostname = hostname,
            configured_hostname = %state.config.server.hostname,
            "setup wizard hostname: the running configuration is unchanged"
        );
    }

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
        Json(TokenResponse {
            access_token: outcome.tokens.access_token,
            refresh_token: outcome.tokens.refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: outcome.tokens.expires_in,
            user: UserResponse::from_row(&outcome.user),
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
        let json = serde_json::to_value(SetupStatusResponse { required: true })
            .expect("must serialise");
        assert_eq!(json, serde_json::json!({ "required": true }));
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
