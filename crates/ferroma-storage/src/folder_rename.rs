//! Coordinated folder-tree rename across PostgreSQL and Maildir.
//!
//! Files are copied first, and every folder name and message path changes in a
//! single database transaction. The old bodies remain readable until commit; on
//! database failure the staged directories are removed. Filesystem and database
//! cannot commit atomically: a crash can leave duplicate/orphaned files. Callers
//! must serialize delivery, deletion and other folder renames for this mailbox
//! while this operation runs; otherwise a concurrently inserted file/row may be
//! missed or a concurrent write to a staged target may be removed on rollback.

use std::path::{Path, PathBuf};

use ferroma_core::{DomainId, MailboxId, UserId};
use crate::{Folder, Maildir, Repositories, Result, StorageError};

struct FolderMove {
    id: i64,
    from: String,
    src: PathBuf,
    dst: PathBuf,
}

/// Copy a folder tree's Maildir bodies, commit its names and storage paths, then
/// remove the old directories. `new_name` is the complete destination folder
/// name (including parent path); `new_parent` is `None` to retain the parent,
/// `Some(None)` to move to the root, or `Some(Some(id))` to reparent it. `user`
/// records durable `folder_updated` changes for the root and descendants when
/// supplied, and verifies ownership in the database transaction.
///
/// Concurrent filesystem/row mutations require serialization by the caller;
/// this is not a crash-atomic transaction across PostgreSQL and the filesystem.
 pub async fn rename_folder_tree(
    repos: &Repositories, maildir: &Maildir, id: MailboxId,
    new_name: &str, new_parent: Option<Option<i64>>, user: Option<UserId>,
) -> Result<Folder> {
    let root = repos.folders.find_by_id(id).await?
        .ok_or_else(|| StorageError::NotFound(format!("folder {id}")))?;
    if root.name.eq_ignore_ascii_case("INBOX") || new_name.trim().eq_ignore_ascii_case("INBOX") {
        return Err(StorageError::Invalid("INBOX cannot be renamed".into()));
    }
    let name = new_name.trim();
    if name.is_empty() || name.len() > 255 {
        return Err(StorageError::Invalid("invalid destination folder name".into()));
    }
    let mailbox = repos.mailboxes.find_by_id(MailboxId::new(root.mailbox_id)).await?
        .ok_or_else(|| StorageError::NotFound(format!("mailbox {}", root.mailbox_id)))?;
    if user.is_some_and(|user| mailbox.user_id != user.get()) {
        return Err(StorageError::NotFound(format!("folder {id}")));
    }
    let domain = repos.domains.find_by_id(DomainId::new(mailbox.domain_id)).await?
        .ok_or_else(|| StorageError::NotFound(format!("domain {}", mailbox.domain_id)))?;
    let children = repos.folders.descendants(id).await?;
    if let Some(Some(parent)) = new_parent {
        if parent == root.id || children.iter().any(|child| child.id == parent) {
            return Err(StorageError::Invalid("folder cannot be its own descendant".into()));
        }
        let parent_folder = repos.folders.find_by_id(MailboxId::new(parent)).await?
            .ok_or_else(|| StorageError::NotFound(format!("folder {parent}")))?;
        if parent_folder.mailbox_id != root.mailbox_id {
            return Err(StorageError::Invalid("parent belongs to another mailbox".into()));
        }
    }
    let old_prefix = format!("{}/", root.name);
    let mut names = Vec::with_capacity(children.len());
    let mut moves = Vec::with_capacity(children.len() + 1);
    let mut pairs = vec![(root.id, root.name.clone(), name.to_owned())];
    for child in &children {
        let leaf = child.name.rsplit('/').next().filter(|v| !v.is_empty()).unwrap_or(&child.name);
        let target = match child.name.strip_prefix(&old_prefix) {
            Some(tail) if !tail.is_empty() => format!("{name}/{tail}"),
            _ => format!("{name}/{leaf}"),
        };
        names.push((child.id, target.clone()));
        pairs.push((child.id, child.name.clone(), target));
    }
    let ids: Vec<i64> = pairs.iter().map(|(id, _, _)| *id).collect();
    let existing: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM folders WHERE mailbox_id = $1 AND name = ANY($2)",
    ).bind(root.mailbox_id).bind(pairs.iter().map(|(_, _, to)| to.clone()).collect::<Vec<_>>())
        .fetch_all(repos.pool()).await?;
    if existing.iter().any(|(found, _)| !ids.contains(found)) ||
        pairs.iter().enumerate().any(|(i, (_, _, to))| pairs[..i].iter().any(|(_, _, previous)| previous == to)) {
        return Err(StorageError::Conflict(format!("folder {name} already exists")));
    }
    for (folder_id, from, to) in pairs {
        let src = maildir.folder_dir(&domain.name, &mailbox.local_part, &from)?;
        let dst = maildir.folder_dir(&domain.name, &mailbox.local_part, &to)?;
        if src == dst || dst.starts_with(&src) || src.starts_with(&dst) || dst.exists() {
            return Err(StorageError::Conflict(format!("folder {to} already exists or overlaps source")));
        }
        moves.push(FolderMove { id: folder_id, from, src, dst });
    }
    // Require every database body to be inside its actual source directory. This
    // also prevents rewriting an unrelated relative path into a new folder.
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT id, folder_id, storage_path FROM messages WHERE folder_id = ANY($1)",
    ).bind(&ids).fetch_all(repos.pool()).await?;
    let mut paths = Vec::with_capacity(rows.len());
    for (message_id, folder_id, old) in rows {
        let item = moves.iter().find(|m| m.id == folder_id)
            .ok_or_else(|| StorageError::Invalid("message has unknown folder".into()))?;
        let relative = maildir.root().join(&old);
        let tail = relative.strip_prefix(&item.src)
            .map_err(|_| StorageError::Invalid(format!("message {message_id} path is outside folder")))?;
        if !matches!(tail.components().next(), Some(std::path::Component::Normal(_))) ||
            tail.components().any(|part| !matches!(part, std::path::Component::Normal(_))) ||
            !relative.is_file() {
            return Err(StorageError::BodyMissing(old));
        }
        let destination = item.dst.join(tail).strip_prefix(maildir.root())
            .map_err(|_| StorageError::Invalid("destination outside Maildir root".into()))?
            .to_string_lossy().into_owned();
        paths.push((message_id, old, destination));
    }
    let mut staged = Vec::new();
    for item in &moves {
        // Stage a fresh destination even for an empty or never-created source.
        staged.push(item.dst.clone());
        let result = if item.src.is_dir() {
            copy_tree(&item.src, &item.dst)
        } else if item.src.exists() {
            Err(StorageError::Invalid(format!("folder {} is not a directory", item.from)))
        } else {
            crate::maildir::SUBDIRS.iter().try_for_each(|sub| {
                std::fs::create_dir_all(item.dst.join(sub)).map_err(StorageError::from)
            })
        };
        if let Err(error) = result {
            cleanup(&staged);
            return Err(error);
        }
    }
    let changed = repos.folders.rename_folder_tree_with_paths(
        id, name, new_parent, &names, &paths, user,
    ).await;
    let changed = match changed {
        Ok(folder) => folder,
        Err(error) => { cleanup(&staged); return Err(error); }
    };
    for item in &moves {
        if item.src.exists() {
            if let Err(error) = std::fs::remove_dir_all(&item.src) {
                tracing::warn!(folder = %item.from, %error, "old Maildir folder remains after rename");
            }
        }
    }
    Ok(changed)
}

fn cleanup(paths: &[PathBuf]) {
    for path in paths.iter().rev() {
        if path.exists() {
            if let Err(error) = std::fs::remove_dir_all(path) {
                tracing::warn!(path = %path.display(), %error, "failed to clean staged folder");
            }
        }
    }
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            return Err(StorageError::Invalid("symlink or special file in Maildir folder".into()));
        }
    }
    Ok(())
}
