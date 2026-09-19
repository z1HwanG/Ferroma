//! `docs/api.md` §4.1 — accounts and the addresses they own.
//!
//! # Creating an address
//!
//! `POST /users/:id/mailboxes` is the one management operation that has to touch three
//! stores coherently:
//!
//! 1. the `mailboxes` row (which is what SMTP `RCPT TO` resolves against),
//! 2. the six standard `folders` rows (`INBOX`, `Sent`, `Drafts`, `Trash`, `Junk`,
//!    `Archive`) so an IMAP client sees a normal account, and
//! 3. the Maildir tree on disk, because that is where the bytes go.
//!
//! The order matters: the *database* row is created first so a duplicate address is
//! rejected by the unique index before anything is written to disk, then the
//! filesystem, then the folders. A filesystem failure after the row exists is reported
//! honestly rather than leaving the caller thinking the address is usable.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::{DomainId, FerromaError, MailboxId, UserId};
use ferroma_storage::repository::NewMailbox;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AdminUser, Pagination, PaginationQuery, Page};
use crate::routes::admin::domains::audit;
use crate::routes::mail::shapes::{FolderResponse, MailboxResponse, UserResponse};
use crate::state::AppState;

/// The `GET /api/v1/users` filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserListQuery {
    /// Substring of the address or the display name.
    pub query: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// The `POST /api/v1/users` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateUserRequest {
    /// The login address.
    pub email: String,
    /// The initial password.
    pub password: String,
    /// A human name.
    pub display_name: Option<String>,
    /// Whether the account may use the Admin API.
    #[serde(default)]
    pub is_admin: bool,
    /// Whether the account may log in.
    ///
    /// The Admin console's New-account dialog offers this; without the field serde
    /// dropped it and every account was created enabled however the box was set.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// The storage allowance in bytes.
    pub quota_bytes: Option<i64>,
}

/// The serde default for [`CreateUserRequest::enabled`]: an account is usable
/// unless the caller says otherwise.
fn enabled_by_default() -> bool {
    true
}

/// The `PATCH /api/v1/users/:id` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateUserRequest {
    /// A human name. An empty string clears it.
    pub display_name: Option<String>,
    /// Whether the account may log in.
    pub enabled: Option<bool>,
    /// Whether it may use the Admin API.
    pub is_admin: Option<bool>,
    /// The storage allowance in bytes.
    pub quota_bytes: Option<i64>,
    /// A replacement password.
    pub password: Option<String>,
}

/// The `POST /api/v1/users/:id/mailboxes` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateMailboxRequest {
    /// The domain the address lives in, by name.
    pub domain: String,
    /// The local part.
    pub local_part: String,
    /// Whether it becomes the account's primary address.
    #[serde(default)]
    pub is_primary: bool,
    /// A per-address quota, overriding the account's.
    pub quota_bytes: Option<i64>,
}

/// A page of accounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserListResponse {
    /// The accounts.
    pub items: Vec<UserResponse>,
    /// How many matched.
    pub total: i64,
    /// The page size used.
    pub limit: i64,
    /// The offset used.
    pub offset: i64,
}

/// A page of addresses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxListResponse {
    /// The addresses.
    pub items: Vec<MailboxResponse>,
    /// How many exist.
    pub total: i64,
    /// The page size used.
    pub limit: i64,
    /// The offset used.
    pub offset: i64,
}

/// The result of creating an address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedMailboxResponse {
    /// The new address.
    pub mailbox: MailboxResponse,
    /// The folders that were created for it.
    pub folders: Vec<FolderResponse>,
}

/// `GET /api/v1/users`
pub async fn list_users(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<UserListQuery>,
) -> Result<Json<UserListResponse>, ApiError> {
    let pagination = Pagination::clamped(query.limit, query.offset);
    let total = state.repos.users.count().await?;

    // The repository pages but does not filter; a bounded fetch plus an in-memory
    // filter keeps `total` honest for the `?query=` case without a second SQL path.
    let (rows, total) = match query.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(needle) => {
            let all = state.repos.users.list(i64::MAX, 0).await?;
            let needle = needle.to_ascii_lowercase();
            let matched: Vec<_> = all
                .into_iter()
                .filter(|user| {
                    user.email.to_ascii_lowercase().contains(&needle)
                        || user
                            .display_name
                            .as_deref()
                            .map(|name| name.to_ascii_lowercase().contains(&needle))
                            .unwrap_or(false)
                })
                .collect();
            let total = matched.len() as i64;
            let start = pagination.offset.max(0) as usize;
            let items: Vec<_> = matched
                .into_iter()
                .skip(start)
                .take(pagination.limit.max(0) as usize)
                .collect();
            (items, total)
        }
        None => (
            state
                .repos
                .users
                .list(pagination.limit, pagination.offset)
                .await?,
            total,
        ),
    };

    // The console's user table prints each account's address count, so the addresses
    // for the whole page are fetched in one query rather than one query per row. The
    // rows that come back are grouped by owner; an account with none gets an empty
    // list, which is a different answer from "this response did not say".
    let ids: Vec<UserId> = rows.iter().map(|user| UserId::new(user.id)).collect();
    let mut by_owner: HashMap<i64, Vec<MailboxResponse>> = HashMap::new();
    for row in state.repos.mailboxes.list_by_users_with_domain(&ids).await? {
        by_owner
            .entry(row.mailbox.user_id)
            .or_default()
            .push(MailboxResponse::from_row(&row.mailbox, &row.domain));
    }

    Ok(Json(UserListResponse {
        items: rows
            .iter()
            .map(|user| {
                UserResponse::from_row(user)
                    .with_mailboxes(by_owner.remove(&user.id).unwrap_or_default())
            })
            .collect(),
        total,
        limit: pagination.limit,
        offset: pagination.offset,
    }))
}

/// `POST /api/v1/users`
pub async fn create_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(request): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserResponse>), ApiError> {
    let user = state
        .auth
        .create_user(
            &request.email,
            &request.password,
            request.display_name.as_deref(),
            request.is_admin,
            request.enabled,
            request.quota_bytes,
        )
        .await?;

    audit(
        &state,
        &admin,
        "user.created",
        Some("user"),
        Some(&user.id.to_string()),
        serde_json::json!({ "email": user.email, "is_admin": user.is_admin }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(UserResponse::from_row(&user))))
}

/// `GET /api/v1/users/:id`
pub async fn get_user(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<UserResponse>, ApiError> {
    let user = state
        .repos
        .users
        .find_by_id(UserId::new(id))
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("user {id}"))))?;
    Ok(Json(UserResponse::from_row(&user)))
}

/// `PATCH /api/v1/users/:id`
pub async fn update_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Json(request): Json<UpdateUserRequest>,
) -> Result<Json<UserResponse>, ApiError> {
    let user_id = UserId::new(id);
    state
        .repos
        .users
        .find_by_id(user_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("user {id}"))))?;

    if let Some(name) = request.display_name.as_deref() {
        let value = if name.trim().is_empty() {
            None
        } else {
            Some(name.trim())
        };
        state.repos.users.set_display_name(user_id, value).await?;
    }
    if let Some(enabled) = request.enabled {
        state.repos.users.set_enabled(user_id, enabled).await?;
        // Disabling an account must take effect now, not in an hour: every session it
        // holds is revoked.
        if !enabled {
            let revoked = state.repos.sessions.revoke_all_for_user(user_id).await?;
            tracing::info!(user_id = id, revoked, "account disabled; sessions revoked");
        }
    }
    if let Some(is_admin) = request.is_admin {
        state.repos.users.set_admin(user_id, is_admin).await?;
    }
    if let Some(quota) = request.quota_bytes {
        state.repos.users.set_quota(user_id, quota).await?;
    }
    if let Some(password) = request.password.as_deref().filter(|p| !p.is_empty()) {
        let hash = state.auth.hash_password(password).await?;
        state.repos.users.update_password(user_id, &hash).await?;
        // A password reset invalidates every existing session.
        state.repos.sessions.revoke_all_for_user(user_id).await?;
    }

    audit(
        &state,
        &admin,
        "user.updated",
        Some("user"),
        Some(&id.to_string()),
        serde_json::json!({}),
    )
    .await;

    let user = state
        .repos
        .users
        .require_by_id(user_id)
        .await?;
    Ok(Json(UserResponse::from_row(&user)))
}

/// `DELETE /api/v1/users/:id`
pub async fn delete_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let user_id = UserId::new(id);
    let user = state
        .repos
        .users
        .find_by_id(user_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("user {id}"))))?;

    // Deleting the account cascades in the database, but the Maildir tree is not the
    // database's to remove, so the addresses are collected first.
    let addresses = state.repos.mailboxes.list_by_user_with_domain(user_id).await?;
    let removed = state.repos.users.delete(user_id).await?;
    if !removed {
        return Err(ApiError::new(FerromaError::NotFound(format!("user {id}"))));
    }

    for address in &addresses {
        if let Err(err) = state
            .maildir
            .delete_folder(&address.domain, &address.mailbox.local_part, "INBOX")
        {
            tracing::warn!(error = %err, "could not remove a mailbox tree");
        }
        if let Ok(mailbox_dir) = state.maildir.mailbox_dir(&address.domain, &address.mailbox.local_part)
        {
            let _ = std::fs::remove_dir_all(mailbox_dir);
        }
    }

    audit(
        &state,
        &admin,
        "user.deleted",
        Some("user"),
        Some(&id.to_string()),
        serde_json::json!({ "email": user.email, "addresses": addresses.len() }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/users/:id/mailboxes`
pub async fn list_user_mailboxes(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<MailboxListResponse>, ApiError> {
    let user_id = UserId::new(id);
    state
        .repos
        .users
        .find_by_id(user_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("user {id}"))))?;

    let rows = state.repos.mailboxes.list_by_user_with_domain(user_id).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        let used = state
            .repos
            .mailboxes
            .used_bytes(row.mailbox.mailbox_id())
            .await
            .unwrap_or(0);
        items.push(MailboxResponse::from_row(&row.mailbox, &row.domain).with_usage(used));
    }

    Ok(Json(MailboxListResponse {
        total: items.len() as i64,
        limit: crate::extract::DEFAULT_LIMIT,
        offset: 0,
        items,
    }))
}

/// `POST /api/v1/users/:id/mailboxes` — creates the Maildir and the standard folders.
pub async fn create_user_mailbox(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Json(request): Json<CreateMailboxRequest>,
) -> Result<(StatusCode, Json<CreatedMailboxResponse>), ApiError> {
    let user_id = UserId::new(id);
    state
        .repos
        .users
        .find_by_id(user_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("user {id}"))))?;

    let domain_name = ferroma_core::address::normalise_domain(&request.domain);
    if ferroma_core::address::validate_domain(&domain_name).is_err() {
        return Err(ApiError::new(FerromaError::Invalid(format!(
            "{} is not a valid domain name",
            request.domain
        ))));
    }
    let local_part = request.local_part.trim().to_ascii_lowercase();
    ferroma_core::address::validate_local_part(&local_part)
        .map_err(|err| ApiError::new(err).with_details(serde_json::json!({ "field": "local_part" })))?;

    let domain = state
        .repos
        .domains
        .find_by_name(&domain_name)
        .await?
        .ok_or_else(|| {
            ApiError::new(FerromaError::NotFound(format!(
                "no such domain {domain_name}; create it first"
            )))
        })?;

    // The row first: the unique index is what refuses a duplicate address, and it does
    // so before anything is written to disk.
    let mailbox = state
        .repos
        .mailboxes
        .create(NewMailbox {
            user_id,
            domain_id: domain.domain_id(),
            local_part: local_part.clone(),
            display_name: None,
            is_primary: request.is_primary,
            quota_bytes: request.quota_bytes,
        })
        .await?;

    // The Maildir, then the folder rows. A filesystem failure is reported, because an
    // address whose directory is missing cannot receive mail.
    state
        .maildir
        .ensure_mailbox(&domain.name, &local_part)
        .map_err(|err| {
            ApiError::new(FerromaError::storage(err)).with_details(serde_json::json!({
                "address": format!("{local_part}@{}", domain.name),
                "stage": "maildir",
            }))
        })?;

    let folders = state
        .repos
        .folders
        .ensure_standard(mailbox.mailbox_id())
        .await?;

    audit(
        &state,
        &admin,
        "mailbox.created",
        Some("mailbox"),
        Some(&mailbox.id.to_string()),
        serde_json::json!({ "address": format!("{local_part}@{}", domain.name) }),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(CreatedMailboxResponse {
            mailbox: MailboxResponse::from_row(&mailbox, &domain.name),
            folders: folders.iter().map(FolderResponse::from_row).collect(),
        }),
    ))
}

/// A page of users, for callers that build the response themselves.
pub fn paged_users(items: Vec<UserResponse>, total: i64, pagination: Pagination) -> Page<UserResponse> {
    pagination.page(items, total)
}

/// The pagination an admin list endpoint should use.
pub fn pagination_of(query: &PaginationQuery) -> Pagination {
    Pagination::clamped(query.limit, query.offset)
}

/// Whether a mailbox id belongs to a user, without loading the row.
pub async fn owns(state: &AppState, user: UserId, mailbox: MailboxId) -> Result<bool, ApiError> {
    Ok(state
        .repos
        .mailboxes
        .find_by_id(mailbox)
        .await?
        .map(|row| row.user_id == user.get())
        .unwrap_or(false))
}

/// The six standard folder names, for the Admin panel's address dialog.
pub const STANDARD_FOLDERS: [&str; 6] = ["INBOX", "Sent", "Drafts", "Trash", "Junk", "Archive"];

/// The domain a request named, resolved to its row.
pub async fn require_domain(
    state: &AppState,
    id: DomainId,
) -> Result<ferroma_storage::models::Domain, ApiError> {
    state
        .repos
        .domains
        .find_by_id(id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("domain {id}"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_list_filters_are_all_optional() {
        let query: UserListQuery =
            serde_json::from_value(serde_json::json!({})).expect("must deserialise");
        assert!(query.query.is_none());
        assert!(query.limit.is_none());
    }

    #[test]
    fn the_create_body_requires_email_and_password() {
        assert!(serde_json::from_value::<CreateUserRequest>(serde_json::json!({
            "email": "a@b.c"
        }))
        .is_err());
        let request: CreateUserRequest = serde_json::from_value(serde_json::json!({
            "email": "a@b.c",
            "password": "secret123"
        }))
        .expect("must deserialise");
        assert!(!request.is_admin, "the admin bit defaults to off");
        assert!(request.quota_bytes.is_none());
    }

    #[test]
    fn the_update_body_is_entirely_optional() {
        let request: UpdateUserRequest =
            serde_json::from_value(serde_json::json!({})).expect("must deserialise");
        assert!(request.display_name.is_none());
        assert!(request.enabled.is_none());
        assert!(request.is_admin.is_none());
        assert!(request.quota_bytes.is_none());
        assert!(request.password.is_none());

        let request: UpdateUserRequest = serde_json::from_value(serde_json::json!({
            "enabled": false,
            "quota_bytes": 1024
        }))
        .expect("must deserialise");
        assert_eq!(request.enabled, Some(false));
        assert_eq!(request.quota_bytes, Some(1024));
    }

    #[test]
    fn the_mailbox_body_requires_a_domain_and_a_local_part() {
        assert!(serde_json::from_value::<CreateMailboxRequest>(serde_json::json!({
            "domain": "example.com"
        }))
        .is_err());
        let request: CreateMailboxRequest = serde_json::from_value(serde_json::json!({
            "domain": "example.com",
            "local_part": "alice"
        }))
        .expect("must deserialise");
        assert!(!request.is_primary);
        assert!(request.quota_bytes.is_none());
    }

    #[test]
    fn the_standard_folder_set_is_the_documented_six() {
        assert_eq!(STANDARD_FOLDERS.len(), 6);
        assert_eq!(STANDARD_FOLDERS[0], "INBOX");
        assert!(STANDARD_FOLDERS.contains(&"Sent"));
        assert!(STANDARD_FOLDERS.contains(&"Drafts"));
        assert!(STANDARD_FOLDERS.contains(&"Trash"));
        assert!(STANDARD_FOLDERS.contains(&"Junk"));
        assert!(STANDARD_FOLDERS.contains(&"Archive"));
    }

    #[test]
    fn the_pagination_helpers_clamp() {
        let pagination = pagination_of(&PaginationQuery {
            limit: Some(0),
            offset: Some(-5),
        });
        assert_eq!(pagination.limit, crate::extract::DEFAULT_LIMIT);
        assert_eq!(pagination.offset, 0);
        let page = paged_users(Vec::new(), 0, pagination);
        assert_eq!(page.total, 0);
    }

    #[test]
    fn the_list_shapes_serialise_with_the_documented_keys() {
        let users = UserListResponse {
            items: Vec::new(),
            total: 0,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&users).expect("must serialise");
        assert_eq!(json["total"], 0);
        assert_eq!(json["limit"], 50);

        let mailboxes = MailboxListResponse {
            items: Vec::new(),
            total: 0,
            limit: 50,
            offset: 0,
        };
        assert_eq!(
            serde_json::to_value(&mailboxes).expect("must serialise")["items"],
            serde_json::json!([])
        );
    }

    #[test]
    fn a_created_mailbox_response_carries_the_folders() {
        let response = CreatedMailboxResponse {
            mailbox: MailboxResponse {
                id: 3,
                address: "alice@example.com".into(),
                user_id: 7,
                display_name: None,
                is_primary: true,
                enabled: true,
                quota_bytes: None,
                used_bytes: None,
                created_at: chrono::Utc::now(),
            },
            folders: Vec::new(),
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["mailbox"]["address"], "alice@example.com");
        assert_eq!(json["folders"], serde_json::json!([]));
    }
}
