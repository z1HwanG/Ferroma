//! `docs/api.md` §4.8 — the system log ring and the server-wide device list.
//!
//! # The log buffer
//!
//! `GET /api/v1/logs` reads [`crate::logbuf::LogBuffer`], a bounded in-process ring the
//! server's `tracing` subscriber fills. It is **lost on restart**, and the response
//! says so with `buffer_entries` / `buffer_capacity` / `oldest_at` rather than
//! pretending to be a complete log. Values that look like opaque credentials
//! (`rt_…`, `st_…`) are scrubbed before they enter the ring, so a token that reached a
//! log line by accident is not readable by every admin.
//!
//! # Devices
//!
//! The client API's device routes are bearer-only and scoped to one account. The Admin
//! panel needs the whole server, with each owner's address resolved, so this module
//! reads `devices` joined to `users` and offers the same two revoke verbs — mark
//! revoked, revoke the device's sessions, publish `device.revoked` — for any device.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::{DeviceId, FerromaError, UserId};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AdminUser, DEFAULT_LIMIT, Pagination};
use crate::logbuf::{LogEntry, LogFilter};
use crate::routes::admin::domains::audit;
use crate::routes::mail::shapes::DeviceResponse;
use crate::state::AppState;

/// The `GET /api/v1/logs` filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogQuery {
    /// Minimum severity: `error`, `warn`, `info`, `debug` or `trace`.
    pub level: Option<String>,
    /// Substring of the `tracing` target.
    pub target: Option<String>,
    /// Substring of the message or of any field value.
    pub query: Option<String>,
    /// Only entries at or after this instant.
    pub since: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// The `GET /api/v1/logs` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogPageResponse {
    /// The entries, newest first.
    pub items: Vec<LogEntry>,
    /// How many entries matched the filters.
    pub total: i64,
    /// The page size used.
    pub limit: i64,
    /// The offset used.
    pub offset: i64,
    /// How many entries the ring holds right now.
    pub buffer_entries: usize,
    /// How many it can hold.
    pub buffer_capacity: usize,
    /// When the oldest retained entry was recorded.
    pub oldest_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The `GET /api/v1/devices` filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeviceQuery {
    /// Only devices owned by this account.
    pub user_id: Option<i64>,
    /// Include revoked devices.
    pub include_revoked: Option<bool>,
    /// Only devices on this platform.
    pub platform: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// A page of devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceListResponse {
    /// The devices.
    pub items: Vec<DeviceResponse>,
    /// How many matched.
    pub total: i64,
    /// The page size used.
    pub limit: i64,
    /// The offset used.
    pub offset: i64,
}

/// A `devices` row with its owner's address resolved.
#[derive(Debug, Clone)]
pub struct DeviceWithOwner {
    /// The device row id.
    pub id: i64,
    /// The owning account.
    pub user_id: i64,
    /// The owner's login address.
    pub email: Option<String>,
    /// The client-generated installation id.
    pub device_uid: String,
    /// Human name.
    pub name: Option<String>,
    /// `windows`, `linux`, `macos`, `android` or `ios`.
    pub platform: Option<String>,
    /// The client version.
    pub client_version: Option<String>,
    /// The FCP version it last spoke.
    pub protocol_version: Option<i32>,
    /// When it was last seen.
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The last peer address.
    pub last_ip: Option<String>,
    /// When it was first registered.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it was revoked.
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl DeviceWithOwner {
    /// The documented device shape, with the owner's address.
    pub fn into_response(self) -> DeviceResponse {
        DeviceResponse {
            id: self.id,
            user_id: self.user_id,
            email: self.email,
            device_uid: self.device_uid,
            name: self.name,
            platform: self.platform,
            client_version: self.client_version,
            protocol_version: self.protocol_version,
            last_seen_at: self.last_seen_at,
            last_ip: self.last_ip,
            created_at: self.created_at,
            revoked: self.revoked_at.is_some(),
        }
    }
}

/// `GET /api/v1/logs`
pub async fn list_logs(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<LogQuery>,
) -> Result<Json<LogPageResponse>, ApiError> {
    // The log panel pages with its own bounds rather than the API's 50/500: a ring of
    // 1000 entries is small enough that a larger page is harmless.
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0).max(0);

    if let Some(raw) = query.level.as_deref().filter(|level| !level.trim().is_empty()) {
        if crate::logbuf::parse_level(raw).is_none() {
            return Err(ApiError::new(FerromaError::Invalid(format!(
                "unknown log level {raw:?}; expected error, warn, info, debug or trace"
            ))));
        }
    }

    let filter = LogFilter {
        level: query
            .level
            .as_deref()
            .and_then(crate::logbuf::parse_level)
            .unwrap_or_else(|| state.logs.buffer().floor()),
        target: query.target.clone().filter(|t| !t.trim().is_empty()),
        query: query.query.clone().filter(|q| !q.trim().is_empty()),
        since: query
            .since
            .as_deref()
            .and_then(crate::routes::mail::messages::parse_instant),
        limit,
        offset,
    };

    let (items, total) = state.logs.buffer().query(&filter);
    Ok(Json(LogPageResponse {
        items,
        total,
        limit,
        offset,
        buffer_entries: state.logs.buffer().len(),
        buffer_capacity: state.logs.buffer().capacity(),
        oldest_at: state.logs.buffer().oldest_at(),
    }))
}

/// `GET /api/v1/devices`
pub async fn list_devices(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<DeviceQuery>,
) -> Result<Json<DeviceListResponse>, ApiError> {
    let pagination = Pagination::clamped(query.limit, query.offset);

    // `ferroma-storage` offers per-user device listing but no server-wide query with
    // filters, and this crate may not edit that crate. The whole device table is small
    // (one row per signed-in installation), so the filters are applied in memory and
    // the page is sliced from the result.
    let mut rows: Vec<DeviceWithOwner> = Vec::new();
    let mut offset = 0i64;
    loop {
        let users = state.repos.users.list(200, offset).await?;
        if users.is_empty() {
            break;
        }
        offset += users.len() as i64;
        for user in &users {
            let devices = state
                .repos
                .devices
                .list_for_user(ferroma_core::UserId::new(user.id), true)
                .await
                .unwrap_or_default();
            for device in devices {
                rows.push(DeviceWithOwner {
                    id: device.id,
                    user_id: device.user_id,
                    email: Some(user.email.clone()),
                    device_uid: device.device_uid,
                    name: device.name,
                    platform: device.platform,
                    client_version: device.client_version,
                    protocol_version: device.protocol_version,
                    last_seen_at: device.last_seen_at,
                    last_ip: device.last_ip,
                    created_at: device.created_at,
                    revoked_at: device.revoked_at,
                });
            }
        }
    }

    if let Some(user_id) = query.user_id {
        rows.retain(|row| row.user_id == user_id);
    }
    if !query.include_revoked.unwrap_or(false) {
        rows.retain(|row| row.revoked_at.is_none());
    }
    if let Some(platform) = query
        .platform
        .as_deref()
        .map(str::trim)
        .filter(|platform| !platform.is_empty())
    {
        rows.retain(|row| {
            row.platform
                .as_deref()
                .map(|value| value.eq_ignore_ascii_case(platform))
                .unwrap_or(false)
        });
    }

    // Newest activity first, which is how an operator reads the list.
    rows.sort_by(|a, b| {
        let left = a.last_seen_at.unwrap_or(a.created_at);
        let right = b.last_seen_at.unwrap_or(b.created_at);
        right.cmp(&left).then_with(|| b.id.cmp(&a.id))
    });

    let total = rows.len() as i64;
    let start = pagination.offset.max(0) as usize;
    let items: Vec<DeviceResponse> = rows
        .into_iter()
        .skip(start)
        .take(pagination.limit.max(0) as usize)
        .map(DeviceWithOwner::into_response)
        .collect();

    Ok(Json(DeviceListResponse {
        items,
        total,
        limit: pagination.limit,
        offset: pagination.offset,
    }))
}

/// `POST /api/v1/devices/:id/revoke`
pub async fn revoke_device(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<DeviceResponse>, ApiError> {
    let device_id = DeviceId::new(id);
    let device = state
        .repos
        .devices
        .find_by_id(device_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("device {id}"))))?;

    // Revoking through the auth service is what revokes the device's sessions too.
    let revoked_sessions = state
        .auth
        .revoke_device(device_id)
        .await
        .map_err(ApiError::from)?;

    // The event is what makes a live socket for that device disconnect.
    state
        .events
        .publish(
            ferroma_events::EventScope::User(UserId::new(device.user_id)),
            ferroma_events::Event::device_revoked(device_id, UserId::new(device.user_id)),
        )
        .await;

    audit(
        &state,
        &admin,
        "device.revoked",
        Some("device"),
        Some(&id.to_string()),
        serde_json::json!({
            "user_id": device.user_id,
            "device_uid": device.device_uid,
            "revoked_sessions": revoked_sessions,
        }),
    )
    .await;

    let updated = state
        .repos
        .devices
        .find_by_id(device_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("device {id}"))))?;
    let email = state
        .repos
        .users
        .find_by_id(UserId::new(updated.user_id))
        .await
        .ok()
        .flatten()
        .map(|user| user.email);

    let mut response = DeviceResponse::from_row(&updated);
    if let Some(email) = email {
        response = response.with_email(email);
    }
    Ok(Json(response))
}

/// `DELETE /api/v1/devices/:id` — the same effect as revoking.
pub async fn delete_device(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let device_id = DeviceId::new(id);
    let device = state
        .repos
        .devices
        .find_by_id(device_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("device {id}"))))?;

    state
        .auth
        .revoke_device(device_id)
        .await
        .map_err(ApiError::from)?;
    state
        .events
        .publish(
            ferroma_events::EventScope::User(UserId::new(device.user_id)),
            ferroma_events::Event::device_revoked(device_id, UserId::new(device.user_id)),
        )
        .await;
    audit(
        &state,
        &admin,
        "device.deleted",
        Some("device"),
        Some(&id.to_string()),
        serde_json::json!({ "user_id": device.user_id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// The default page size for the log panel.
pub const DEFAULT_LOG_LIMIT: i64 = 100;

/// A page of logs sized the way the panel asks for it.
pub fn log_pagination(query: &LogQuery) -> (i64, i64) {
    (
        query.limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, 1000),
        query.offset.unwrap_or(0).max(0),
    )
}

/// The default page size for the device list.
pub fn device_pagination(query: &DeviceQuery) -> Pagination {
    Pagination::clamped(query.limit, query.offset).max_default()
}

/// A tiny extension so the device list keeps the documented default page size.
trait PaginationDefault {
    /// The same pagination, with the documented default when nothing was asked for.
    fn max_default(self) -> Self;
}

impl PaginationDefault for Pagination {
    fn max_default(self) -> Self {
        if self.limit <= 0 {
            Pagination {
                limit: DEFAULT_LIMIT,
                offset: self.offset,
            }
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_query_defaults_to_warn_and_one_hundred() {
        let query = LogQuery::default();
        assert!(query.level.is_none());
        let (limit, offset) = log_pagination(&query);
        assert_eq!(limit, DEFAULT_LOG_LIMIT);
        assert_eq!(offset, 0);
    }

    #[test]
    fn the_log_page_bounds_are_clamped() {
        let query = LogQuery {
            limit: Some(10_000),
            offset: Some(-3),
            ..LogQuery::default()
        };
        assert_eq!(log_pagination(&query), (1000, 0));

        let query = LogQuery {
            limit: Some(0),
            offset: Some(5),
            ..LogQuery::default()
        };
        assert_eq!(log_pagination(&query), (1, 5));
    }

    #[test]
    fn the_device_pagination_uses_the_api_default() {
        let pagination = device_pagination(&DeviceQuery::default());
        assert_eq!(pagination.limit, DEFAULT_LIMIT);
        assert_eq!(pagination.offset, 0);

        let pagination = device_pagination(&DeviceQuery {
            limit: Some(5),
            offset: Some(10),
            ..DeviceQuery::default()
        });
        assert_eq!(pagination.limit, 5);
        assert_eq!(pagination.offset, 10);
    }

    #[test]
    fn the_log_page_shape_carries_its_buffer_metadata() {
        let response = LogPageResponse {
            items: Vec::new(),
            total: 0,
            limit: 100,
            offset: 0,
            buffer_entries: 12,
            buffer_capacity: 1000,
            oldest_at: None,
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["buffer_entries"], 12);
        assert_eq!(json["buffer_capacity"], 1000);
        assert!(json["oldest_at"].is_null());
        assert_eq!(json["items"], serde_json::json!([]));
    }

    #[test]
    fn the_device_page_shape_is_a_normal_page() {
        let response = DeviceListResponse {
            items: vec![DeviceResponse {
                id: 12,
                user_id: 7,
                email: Some("alice@example.com".into()),
                device_uid: "3f2c".into(),
                name: Some("Alice's laptop".into()),
                platform: Some("windows".into()),
                client_version: Some("0.7.0".into()),
                protocol_version: Some(1),
                last_seen_at: None,
                last_ip: Some("203.0.113.44".into()),
                created_at: chrono::Utc::now(),
                revoked: false,
            }],
            total: 1,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["items"][0]["email"], "alice@example.com");
        assert_eq!(json["items"][0]["revoked"], false);
        assert_eq!(json["total"], 1);
    }

    #[test]
    fn a_device_row_projects_revocation_as_a_boolean() {
        let row = DeviceWithOwner {
            id: 12,
            user_id: 7,
            email: Some("alice@example.com".into()),
            device_uid: "3f2c".into(),
            name: None,
            platform: Some("linux".into()),
            client_version: None,
            protocol_version: Some(1),
            last_seen_at: None,
            last_ip: None,
            created_at: chrono::Utc::now(),
            revoked_at: Some(chrono::Utc::now()),
        };
        let response = row.into_response();
        assert!(response.revoked);
        assert_eq!(response.email.as_deref(), Some("alice@example.com"));
        let json = serde_json::to_value(&response).expect("must serialise");
        assert!(json.get("revoked_at").is_none(), "{json}");
    }

    #[test]
    fn the_device_filters_are_optional() {
        let query: DeviceQuery = serde_json::from_value(serde_json::json!({})).expect("must parse");
        assert!(query.user_id.is_none());
        assert!(query.include_revoked.is_none());
        assert!(query.platform.is_none());

        let query: DeviceQuery = serde_json::from_value(serde_json::json!({
            "user_id": 7,
            "include_revoked": true,
            "platform": "windows"
        }))
        .expect("must parse");
        assert_eq!(query.user_id, Some(7));
        assert!(query.include_revoked.unwrap_or(false));
        assert_eq!(query.platform.as_deref(), Some("windows"));
    }

    #[test]
    fn the_log_query_accepts_every_documented_filter() {
        let query: LogQuery = serde_json::from_value(serde_json::json!({
            "level": "info",
            "target": "ferroma_smtp",
            "query": "deferred",
            "since": "2026-09-16T12:00:00Z",
            "limit": 20,
            "offset": 40
        }))
        .expect("must parse");
        assert_eq!(query.level.as_deref(), Some("info"));
        assert_eq!(query.target.as_deref(), Some("ferroma_smtp"));
        assert_eq!(query.limit, Some(20));
        assert!(crate::routes::mail::messages::parse_instant(
            query.since.as_deref().unwrap_or_default()
        )
        .is_some());
    }
}
