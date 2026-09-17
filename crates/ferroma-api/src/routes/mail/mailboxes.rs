//! `docs/api.md` §5.1 — mailboxes and folders.
//!
//! Creating a folder creates it in the database *and* on disk, because the Maildir is
//! where the bytes go and a row without a directory would fail at the first delivery.
//! Deleting is the reverse order: the row first (so a failure cannot leave mail
//! invisible), then the directory.
//!
//! Renaming and deleting `INBOX` are refused — IMAP has no way to express either — and
//! the refusal is a `400 invalid_input` rather than a `500`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::{FerromaError, MailboxId};
use ferroma_sync::ChangeKind;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::routes::mail::ownership::{owned_folder, owned_mailbox};
use crate::routes::mail::shapes::{AddressBrief, FolderResponse, MailboxResponse};
use crate::state::AppState;

/// The `GET /api/v1/mailboxes` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxListResponse {
    /// The caller's addresses.
    pub mailboxes: Vec<AddressBrief>,
}

/// The `GET /api/v1/mailboxes/:id/folders` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderListResponse {
    /// The address the folders belong to.
    pub mailbox_id: i64,
    /// Its folders, `INBOX` first.
    pub folders: Vec<FolderResponse>,
}

/// The `POST /api/v1/mailboxes/:id/folders` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateFolderRequest {
    /// The folder name, possibly a `Parent/Child` path.
    pub name: String,
    /// An optional parent folder name to prefix.
    pub parent: Option<String>,
}

/// The `PATCH /api/v1/folders/:id` body.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateFolderRequest {
    /// A new name.
    pub name: Option<String>,
    /// Subscribe or unsubscribe.
    pub subscribed: Option<bool>,
}

/// `GET /api/v1/mailboxes` — the addresses the caller may send from.
pub async fn list_mailboxes(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<MailboxListResponse>, ApiError> {
    let rows = state
        .repos
        .mailboxes
        .list_by_user(user.user_id())
        .await
        .map_err(ApiError::from)?;

    let mut mailboxes = Vec::with_capacity(rows.len());
    for row in rows {
        let domain = crate::routes::mail::store::domain_name(&state.repos, row.domain_id).await?;
        mailboxes.push(AddressBrief {
            id: row.id,
            address: row.address(&domain),
            is_primary: row.is_primary,
        });
    }
    Ok(Json(MailboxListResponse { mailboxes }))
}

/// `GET /api/v1/mailboxes/:id/folders`
pub async fn list_folders(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<FolderListResponse>, ApiError> {
    let mailbox = owned_mailbox(&state.repos, MailboxId::new(id), user.user_id()).await?;
    let folders = state
        .repos
        .folders
        .list(mailbox.mailbox_id())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(FolderListResponse {
        mailbox_id: mailbox.id,
        folders: folders.iter().map(FolderResponse::from_row).collect(),
    }))
}

/// `POST /api/v1/mailboxes/:id/folders`
pub async fn create_folder(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(request): Json<CreateFolderRequest>,
) -> Result<(StatusCode, Json<FolderResponse>), ApiError> {
    let mailbox = owned_mailbox(&state.repos, MailboxId::new(id), user.user_id()).await?;

    let name = compose_folder_name(request.name.trim(), request.parent.as_deref());
    if name.is_empty() {
        return Err(ApiError::new(FerromaError::Invalid(
            "folder name must not be blank".to_string(),
        )));
    }
    if name.len() > 255 {
        return Err(ApiError::new(FerromaError::Invalid(
            "folder name is too long".to_string(),
        )));
    }

    let domain = crate::routes::mail::store::domain_name(&state.repos, mailbox.domain_id).await?;
    // The directory first: a row whose Maildir is missing would fail at delivery.
    state
        .maildir
        .create_folder(&domain, &mailbox.local_part, &name)?;

    let folder = match state
        .repos
        .folders
        .create(mailbox.mailbox_id(), &name, None)
        .await
    {
        Ok(folder) => folder,
        Err(err) => {
            // Roll the directory back so the next attempt starts clean.
            let _ = state
                .maildir
                .delete_folder(&domain, &mailbox.local_part, &name);
            return Err(ApiError::from(err));
        }
    };

    state
        .sync
        .record_folder_change(
            user.user_id(),
            mailbox.mailbox_id(),
            folder.folder_id(),
            &folder.name,
            ChangeKind::FolderCreated,
        )
        .await?;

    Ok((StatusCode::CREATED, Json(FolderResponse::from_row(&folder))))
}

/// `PATCH /api/v1/folders/:id`
pub async fn update_folder(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(request): Json<UpdateFolderRequest>,
) -> Result<Json<FolderResponse>, ApiError> {
    let (folder, mailbox) = owned_folder(&state.repos, MailboxId::new(id), user.user_id()).await?;

    let mut updated = folder.clone();
    if let Some(name) = request.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        if folder.is_inbox() {
            return Err(ApiError::new(FerromaError::Invalid(
                "INBOX cannot be renamed".to_string(),
            )));
        }
        if name.len() > 255 {
            return Err(ApiError::new(FerromaError::Invalid(
                "folder name is too long".to_string(),
            )));
        }
        let domain =
            crate::routes::mail::store::domain_name(&state.repos, mailbox.domain_id).await?;
        // Rename on disk first, so a conflict (an existing directory) fails before
        // the database is touched.
        state
            .maildir
            .rename_folder(&domain, &mailbox.local_part, &folder.name, name)?;
        updated = state
            .repos
            .folders
            .rename(folder.folder_id(), name)
            .await
            .map_err(ApiError::from)?;
    }

    if let Some(subscribed) = request.subscribed {
        state
            .repos
            .folders
            .set_subscribed(updated.folder_id(), subscribed)
            .await
            .map_err(ApiError::from)?;
        updated = state
            .repos
            .folders
            .find_by_id(updated.folder_id())
            .await
            .map_err(ApiError::from)?
            .unwrap_or(updated);
        state
            .sync
            .record_folder_change(
                user.user_id(),
                mailbox.mailbox_id(),
                updated.folder_id(),
                &updated.name,
                ChangeKind::FolderUpdated,
            )
            .await?;
    }

    Ok(Json(FolderResponse::from_row(&updated)))
}

/// `DELETE /api/v1/folders/:id` — refuses `INBOX`.
pub async fn delete_folder(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let (folder, mailbox) = owned_folder(&state.repos, MailboxId::new(id), user.user_id()).await?;

    if folder.is_inbox() {
        return Err(ApiError::new(FerromaError::Invalid(
            "INBOX cannot be deleted".to_string(),
        )));
    }

    // Row first: a failure here leaves the mail visible and the directory orphaned,
    // which is recoverable. The reverse order would lose mail.
    state
        .repos
        .folders
        .delete(folder.folder_id())
        .await
        .map_err(ApiError::from)?;

    let domain = crate::routes::mail::store::domain_name(&state.repos, mailbox.domain_id).await?;
    if let Err(err) = state
        .maildir
        .delete_folder(&domain, &mailbox.local_part, &folder.name)
    {
        // The folder is already gone from the database; a filesystem hiccup is worth
        // a warning, not a 500 that would make the client retry a completed delete.
        tracing::warn!(
            folder_id = folder.id,
            error = %err,
            "folder directory could not be removed"
        );
    }

    state
        .sync
        .record_folder_change(
            user.user_id(),
            mailbox.mailbox_id(),
            folder.folder_id(),
            &folder.name,
            ChangeKind::FolderDeleted,
        )
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Join a folder name with an optional parent, avoiding a doubled separator.
pub fn compose_folder_name(name: &str, parent: Option<&str>) -> String {
    let name = name.trim().trim_matches('/');
    match parent.map(str::trim).filter(|parent| !parent.is_empty()) {
        Some(parent) => {
            let parent = parent.trim_matches('/');
            if name.is_empty() {
                parent.to_string()
            } else if name.eq_ignore_ascii_case(parent)
                || name
                    .to_ascii_lowercase()
                    .starts_with(&format!("{}/", parent.to_ascii_lowercase()))
            {
                name.to_string()
            } else {
                format!("{parent}/{name}")
            }
        }
        None => name.to_string(),
    }
}

/// A mailbox plus its folders, used by the client mailbox list.
pub async fn mailbox_tree(
    state: &AppState,
    user: ferroma_core::UserId,
) -> Result<Vec<crate::routes::mail::shapes::MailboxTreeResponse>, ApiError> {
    let rows = state
        .repos
        .mailboxes
        .list_by_user(user)
        .await
        .map_err(ApiError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let domain = crate::routes::mail::store::domain_name(&state.repos, row.domain_id).await?;
        let folders = state
            .repos
            .folders
            .list(row.mailbox_id())
            .await
            .map_err(ApiError::from)?;
        out.push(crate::routes::mail::shapes::MailboxTreeResponse {
            id: row.id,
            address: row.address(&domain),
            display_name: row.display_name.clone(),
            is_primary: row.is_primary,
            folders: folders.iter().map(FolderResponse::from_row).collect(),
        });
    }
    Ok(out)
}

/// A mailbox response with its usage attached.
pub async fn mailbox_response(
    state: &AppState,
    row: &ferroma_storage::models::Mailbox,
    domain: &str,
) -> MailboxResponse {
    let used = state
        .repos
        .mailboxes
        .used_bytes(row.mailbox_id())
        .await
        .unwrap_or(0);
    MailboxResponse::from_row(row, domain).with_usage(used)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_join_with_a_single_separator() {
        assert_eq!(compose_folder_name("2026", Some("Archive")), "Archive/2026");
        assert_eq!(compose_folder_name("/2026/", Some("/Archive/")), "Archive/2026");
        assert_eq!(compose_folder_name("Archive/2026", Some("Archive")), "Archive/2026");
        assert_eq!(compose_folder_name("Archive", Some("Archive")), "Archive");
        assert_eq!(compose_folder_name("Sent", None), "Sent");
        assert_eq!(compose_folder_name("  Sent  ", None), "Sent");
        assert_eq!(compose_folder_name("", Some("Archive")), "Archive");
        assert_eq!(compose_folder_name("   ", None), "");
        assert_eq!(compose_folder_name("x", Some("   ")), "x");
    }

    #[test]
    fn folder_creation_rejects_a_blank_name() {
        // The handler's guard, exercised directly.
        assert!(compose_folder_name("", None).is_empty());
        assert!(compose_folder_name("  ", Some("  ")).is_empty());
    }

    #[test]
    fn folder_list_response_shape() {
        let response = FolderListResponse {
            mailbox_id: 3,
            folders: Vec::new(),
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["mailbox_id"], 3);
        assert_eq!(json["folders"], serde_json::json!([]));
    }

    #[test]
    fn mailbox_list_response_uses_the_documented_key() {
        let response = MailboxListResponse {
            mailboxes: vec![AddressBrief {
                id: 3,
                address: "alice@example.com".into(),
                is_primary: true,
            }],
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["mailboxes"][0]["address"], "alice@example.com");
    }
}
