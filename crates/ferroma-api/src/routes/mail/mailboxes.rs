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
    /// The folder to move this one inside.
    ///
    /// `null` moves it to the top level, and leaving the field out leaves the parent alone —
    /// which is why this is a double option: `Option<i64>` cannot tell "no opinion" from
    /// "no parent", and a move to the top level is exactly the second one.
    #[serde(default, deserialize_with = "double_option")]
    pub parent_id: Option<Option<i64>>,
    /// Subscribe or unsubscribe.
    pub subscribed: Option<bool>,
}

/// Tell `"parent_id": null` (top level) apart from an absent `parent_id` (no opinion).
fn double_option<'de, D>(deserializer: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// The path a folder takes when it is moved under `parent`.
///
/// A folder's IMAP name *is* its path, so a move is a rename whose prefix changes: the
/// folder keeps its own leaf, and its new name is the parent's path plus that leaf. Moving
/// to the top level leaves the bare leaf.
pub fn moved_folder_name(
    parent: Option<&ferroma_storage::models::Folder>,
    current_name: &str,
) -> String {
    let leaf = current_name
        .rsplit('/')
        .next()
        .filter(|leaf| !leaf.is_empty())
        .unwrap_or(current_name);
    match parent {
        Some(parent) => compose_folder_name(leaf, Some(&parent.name)),
        None => leaf.to_string(),
    }
}

/// Whether moving `folder` under `candidate` would put a folder inside itself.
///
/// The tree is a `parent_id` chain, so the test is whether the candidate is the folder or
/// one of its descendants — a cycle the database would happily store and no client could
/// render.
pub fn would_cycle(
    folder: &ferroma_storage::models::Folder,
    candidate: &ferroma_storage::models::Folder,
    descendants: &[ferroma_storage::models::Folder],
) -> bool {
    candidate.id == folder.id || descendants.iter().any(|child| child.id == candidate.id)
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
    let stored = state
        .repos
        .folders
        .list(mailbox.mailbox_id())
        .await
        .map_err(ApiError::from)?;
    // The sidebar renders these counters, and they are a cache. Move, copy and a hard
    // delete used to leave them behind, so a folder that no longer holds the mail still
    // wore its old badge. Recomputing here is what makes the next folder refresh true
    // for mail that was already miscounted; a failed recount keeps the stored row
    // rather than failing the whole list.
    let mut folders = Vec::with_capacity(stored.len());
    for folder in stored {
        match state.repos.folders.recount(folder.folder_id()).await {
            Ok(fresh) => folders.push(fresh),
            Err(error) => {
                tracing::warn!(
                    folder_id = folder.id,
                    error = %error,
                    "folder counters could not be recomputed while listing folders"
                );
                folders.push(folder);
            }
        }
    }
    Ok(Json(FolderListResponse {
        mailbox_id: mailbox.id,
        folders: folders.iter().map(FolderResponse::from_row).collect(),
    }))
}

/// `POST /api/v1/mailboxes/:id/folders`
///
/// A `Parent/Child` name creates `Parent` too, when it is missing. That is what makes
/// the Webmail's "Nested folders use “Parent/Child”" hint true: typing the path is the
/// whole interaction, and the row it lands in is a real folder the tree can walk,
/// rather than a flat name that only looks hierarchical.
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

    // A name that already exists is a conflict, exactly as it was before paths were
    // supported and as IMAP's `CREATE` answers `ALREADYEXISTS`. Checked before anything
    // is written, so a rejected creation leaves no half-built parent behind.
    if state
        .repos
        .folders
        .find_by_name(mailbox.mailbox_id(), &name)
        .await
        .map_err(ApiError::from)?
        .is_some()
    {
        return Err(ApiError::new(FerromaError::Conflict(format!(
            "folder {name} already exists"
        ))));
    }

    // The directory first: a row whose Maildir is missing would fail at delivery.
    state
        .maildir
        .create_folder(&domain, &mailbox.local_part, &name)?;

    let folder = match ensure_folder_path(&state, mailbox.mailbox_id(), &name).await {
        Ok(folder) => folder,
        Err(err) => {
            // Roll the directory back so the next attempt starts clean.
            let _ = state
                .maildir
                .delete_folder(&domain, &mailbox.local_part, &name);
            return Err(err);
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

/// Find or create every folder on the path `name`, and return the leaf.
///
/// `Archive/2026/January` walks three levels: an existing `Archive` is reused, a
/// missing one is created at the top level, then `Archive/2026`, then the leaf. Each
/// created row records the row above it as `parent_id`, so the tree the console and
/// the Webmail draw needs no string parsing.
pub async fn ensure_folder_path(
    state: &AppState,
    mailbox_id: MailboxId,
    name: &str,
) -> Result<ferroma_storage::models::Folder, ApiError> {
    let path = name.trim().trim_matches('/');
    if path.is_empty() {
        return Err(ApiError::new(FerromaError::Invalid(
            "folder name must not be blank".to_string(),
        )));
    }

    let mut parent: Option<MailboxId> = None;
    let mut prefix = String::new();
    let mut leaf: Option<ferroma_storage::models::Folder> = None;

    for segment in path
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);

        let existing = state
            .repos
            .folders
            .find_by_name(mailbox_id, &prefix)
            .await
            .map_err(ApiError::from)?;

        let folder = match existing {
            Some(folder) => {
                // Adopt a folder that predates `parent_id`, so a path created before
                // this version still nests the first time it is touched.
                if folder.parent_id != parent.map(|id| id.get()) {
                    if let Some(parent) = parent {
                        let _ = state
                            .repos
                            .folders
                            .set_parent(folder.folder_id(), Some(parent))
                            .await;
                    }
                }
                folder
            }
            None => state
                .repos
                .folders
                .create_in(mailbox_id, &prefix, parent, None)
                .await
                .map_err(ApiError::from)?,
        };

        parent = Some(folder.folder_id());
        leaf = Some(folder);
    }

    leaf.ok_or_else(|| {
        ApiError::new(FerromaError::Invalid(
            "folder name must not be blank".to_string(),
        ))
    })
}

/// `PATCH /api/v1/folders/:id`
///
/// Renaming a folder renames the path of everything beneath it, the way IMAP `RENAME`
/// does. The shared storage coordinator stages destination Maildir bodies before
/// committing folder names, message paths and cursor changes together.
pub async fn update_folder(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(request): Json<UpdateFolderRequest>,
) -> Result<Json<FolderResponse>, ApiError> {
    let (folder, mailbox) = owned_folder(&state.repos, MailboxId::new(id), user.user_id()).await?;

    let mut updated = folder.clone();

    // A move and a rename are the same operation underneath: a folder's IMAP name is its
    // path, so changing its parent means changing that path (and every descendant's).
    let asked_name = request
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let new_parent: Option<Option<i64>> = match request.parent_id {
        // No opinion about the parent: leave it where it is.
        None => None,
        Some(parent_id) => {
            if folder.is_inbox() {
                return Err(ApiError::new(FerromaError::Invalid(
                    "INBOX cannot be moved".to_string(),
                )));
            }
            let target = match parent_id {
                // A folder id is positive. Zero arrives from a client that turned "no
                // parent" into a number, and answering `no such folder` for it reads like
                // the folder vanished rather than like the bug it is.
                Some(id) if id <= 0 => {
                    return Err(ApiError::new(FerromaError::Invalid(
                        "parent_id must be a folder id, or null for the top level".to_string(),
                    )));
                }
                Some(id) => {
                    let (parent, parent_mailbox) =
                        owned_folder(&state.repos, MailboxId::new(id), user.user_id()).await?;
                    if parent_mailbox.mailbox_id() != mailbox.mailbox_id() {
                        return Err(ApiError::new(FerromaError::Invalid(
                            "a folder cannot be moved into another address".to_string(),
                        )));
                    }
                    Some(parent)
                }
                None => None,
            };
            // The children are needed twice: to reject a cycle, and to re-path them.
            let children = state
                .repos
                .folders
                .descendants(folder.folder_id())
                .await
                .map_err(ApiError::from)?;
            if let Some(target) = &target {
                if would_cycle(&folder, target, &children) {
                    return Err(ApiError::new(FerromaError::Invalid(
                        "a folder cannot be moved inside itself".to_string(),
                    )));
                }
            }
            Some(target.map(|parent| parent.id))
        }
    };

    if new_parent.is_some() || asked_name.is_some() {
        if folder.is_inbox() {
            return Err(ApiError::new(FerromaError::Invalid(
                "INBOX cannot be renamed".to_string(),
            )));
        }
        // The name the folder ends up with: an explicit one wins, otherwise the move
        // decides it (parent's path + own leaf, or the bare leaf at the top level).
        let name = match (asked_name, new_parent) {
            (Some(name), None) => name.to_string(),
            (Some(name), Some(_)) => {
                let parent = match new_parent.flatten() {
                    Some(parent_id) => Some(
                        state
                            .repos
                            .folders
                            .find_by_id(MailboxId::new(parent_id))
                            .await
                            .map_err(ApiError::from)?
                            .ok_or_else(|| {
                                ApiError::new(FerromaError::NotFound(format!("folder {parent_id}")))
                            })?,
                    ),
                    None => None,
                };
                compose_folder_name(name, parent.as_ref().map(|parent| parent.name.as_str()))
            }
            (None, Some(parent)) => {
                let parent = match parent {
                    Some(parent_id) => Some(
                        state
                            .repos
                            .folders
                            .find_by_id(MailboxId::new(parent_id))
                            .await
                            .map_err(ApiError::from)?
                            .ok_or_else(|| {
                                ApiError::new(FerromaError::NotFound(format!("folder {parent_id}")))
                            })?,
                    ),
                    None => None,
                };
                moved_folder_name(parent.as_ref(), &folder.name)
            }
            (None, None) => unreachable!("guarded by the condition above"),
        };
        let name = name.as_str();

        if name.len() > 255 {
            return Err(ApiError::new(FerromaError::Invalid(
                "folder name is too long".to_string(),
            )));
        }
        if name == folder.name && new_parent.flatten() == folder.parent_id {
            // Nothing to do: a drag that landed where it started.
            return Ok(Json(FolderResponse::from_row(&folder)));
        }
        // Stage the new Maildir directories while old paths remain readable;
        // names, message paths and the FCP cursor commit in one SQL transaction.
        updated = ferroma_storage::rename_folder_tree(
            &state.repos, &state.maildir, folder.folder_id(), name,
            new_parent, Some(user.user_id()),
        ).await.map_err(ApiError::from)?;
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

/// The names a rename produces: the folder itself, then each descendant.
///
/// A descendant keeps the part of its path below the folder being renamed, so
/// `Projects/2026/Q1` becomes `Work/2026/Q1` when `Projects` becomes `Work`. A child
/// whose stored name does not spell the old path out — the console used to allow
/// renaming a child on its own — is re-anchored under the new parent by its last
/// segment, which is the only reading of its position that is still true.
///
/// The order matches the order `descendants()` returns, which is what lets the caller
/// zip the two together.
pub fn rename_plan(
    folder: &ferroma_storage::models::Folder,
    descendants: &[ferroma_storage::models::Folder],
    new_name: &str,
) -> Vec<(String, String)> {
    let mut plan = vec![(folder.name.clone(), new_name.to_string())];
    let old_prefix = format!("{}/", folder.name);
    for child in descendants {
        let leaf = child
            .name
            .rsplit('/')
            .next()
            .filter(|leaf| !leaf.is_empty())
            .unwrap_or(&child.name);
        let renamed = match child.name.strip_prefix(&old_prefix) {
            Some(tail) if !tail.is_empty() => format!("{new_name}/{tail}"),
            _ => format!("{new_name}/{leaf}"),
        };
        plan.push((child.name.clone(), renamed));
    }
    plan
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

    /// A folder row with the fields `rename_plan` reads.
    fn folder(id: i64, name: &str, parent_id: Option<i64>) -> ferroma_storage::models::Folder {
        ferroma_storage::models::Folder {
            id,
            mailbox_id: 1,
            name: name.into(),
            parent_id,
            special_use: None,
            subscribed: true,
            uid_validity: 1,
            uid_next: 1,
            highest_modseq: 1,
            message_count: 0,
            unseen_count: 0,
            total_bytes: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_rename_moves_every_descendant_path() {
        let root = folder(1, "Projects", None);
        let year = folder(2, "Projects/2026", Some(1));
        let quarter = folder(3, "Projects/2026/Q1", Some(2));
        // A child the console used to be able to rename on its own: its name no longer
        // spells its ancestry out, so it is re-anchored under the new parent by its leaf.
        let loose = folder(4, "Loose", Some(2));

        let plan = rename_plan(&root, &[year, quarter, loose], "Work");
        assert_eq!(
            plan,
            vec![
                ("Projects".to_string(), "Work".to_string()),
                ("Projects/2026".to_string(), "Work/2026".to_string()),
                ("Projects/2026/Q1".to_string(), "Work/2026/Q1".to_string()),
                ("Loose".to_string(), "Work/Loose".to_string()),
            ]
        );
    }

    #[test]
    fn a_move_takes_the_parent_path_and_keeps_its_own_leaf() {
        let projects = folder(1, "项目", None);
        let child = folder(2, "项目/2026", Some(1));
        let finance = folder(3, "财务", None);
        let fixture = ferroma_storage::models::Folder {
            id: 9,
            mailbox_id: 1,
            name: "待办".into(),
            parent_id: None,
            special_use: None,
            subscribed: true,
            uid_validity: 1,
            uid_next: 1,
            highest_modseq: 1,
            message_count: 0,
            unseen_count: 0,
            total_bytes: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        // Into a folder: the leaf is kept and the parent's path is prefixed.
        assert_eq!(
            moved_folder_name(Some(&finance), fixture.name.as_str()),
            "财务/待办"
        );
        // A nested folder keeps only its own leaf, not the whole old path.
        assert_eq!(
            moved_folder_name(Some(&finance), child.name.as_str()),
            "财务/2026"
        );
        // To the top level: the bare leaf.
        assert_eq!(moved_folder_name(None, child.name.as_str()), "2026");
        // And the parent is untouched by the name helper.
        assert_eq!(projects.name, "项目");
    }

    #[test]
    fn a_folder_cannot_be_moved_inside_itself() {
        let projects = folder(1, "项目", None);
        let year = folder(2, "项目/2026", Some(1));
        let quarter = folder(3, "项目/2026/Q1", Some(2));
        let other = folder(4, "财务", None);

        assert!(would_cycle(
            &projects,
            &projects,
            &[year.clone(), quarter.clone()]
        ));
        assert!(would_cycle(
            &projects,
            &year,
            &[year.clone(), quarter.clone()]
        ));
        assert!(would_cycle(
            &projects,
            &quarter,
            &[year.clone(), quarter.clone()]
        ));
        assert!(!would_cycle(&projects, &other, &[year, quarter]));
    }

    #[test]
    fn a_rename_with_no_children_is_just_the_folder() {
        let root = folder(1, "Archive", None);
        assert_eq!(
            rename_plan(&root, &[], "Old"),
            vec![("Archive".to_string(), "Old".to_string())]
        );
    }

    #[test]
    fn folder_names_join_with_a_single_separator() {
        assert_eq!(compose_folder_name("2026", Some("Archive")), "Archive/2026");
        assert_eq!(
            compose_folder_name("/2026/", Some("/Archive/")),
            "Archive/2026"
        );
        assert_eq!(
            compose_folder_name("Archive/2026", Some("Archive")),
            "Archive/2026"
        );
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
