//! Maildir storage: where the actual RFC 5322 bytes live.
//!
//! Layout, per the specification §13:
//!
//! ```text
//! <root>/example.com/alice/Maildir/
//!     ├── cur/    messages the client has already seen (flags live in the filename)
//!     ├── new/    delivered but not yet seen by a mail client
//!     ├── tmp/    half-written files; never read
//!     └── .Sent/{cur,new,tmp}
//! ```
//!
//! # Naming
//!
//! A message file is `<seconds>.<pid>_<counter>.<hostname>:2,<flags>` where the
//! suffix after `:2,` carries the Maildir flags (`S` seen, `R` answered, `F` flagged,
//! `T` trashed, `D` draft). The mapping to IMAP flags is in [`Maildir::flags_to_maildir`]
//! and [`Maildir::maildir_to_flags`] — those two functions are the single place where
//! the two vocabularies meet.
//!
//! # Durability
//!
//! A message is written into `tmp/`, flushed (and `fsync`ed when configured), then
//! `rename(2)`d into `new/` or `cur/`. A reader can therefore never observe a partial
//! message, and a crash leaves only squatting files in `tmp/`.
//!
//! # Path safety
//!
//! Domain names, local parts and folder names all reach the filesystem. Every
//! component is validated by [`sanitize_component`] before it is joined to a path, so
//! a crafted folder name such as `../../etc` cannot escape the mail root.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ferroma_core::config::MailboxLayout;
use sha2::{Digest, Sha256};

use crate::error::{Result, StorageError};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The three Maildir subdirectories.
pub const SUBDIRS: [&str; 3] = ["cur", "new", "tmp"];

/// The canonical name of the inbox folder.
pub const INBOX: &str = "INBOX";

/// What [`Maildir::store`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// Path relative to the Maildir root — this is what `messages.storage_path` holds.
    pub path: String,
    /// Size of the stored bytes.
    pub size: u64,
    /// Lower-case hex SHA-256 of the stored bytes.
    pub sha256: String,
}

/// One message file found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaildirEntry {
    /// Path relative to the Maildir root.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Maildir flag letters (no `:2,` prefix), e.g. `S`.
    pub maildir_flags: String,
    /// Modification time, seconds since the Unix epoch.
    pub modified_secs: i64,
}

/// The canonical Maildir "info" separator (RFC-less de-facto standard, Dovecot et al).
pub const INFO_SEPARATOR_UNIX: char = ':';

/// The separator used on filesystems that reserve `:` in file names.
///
/// NTFS treats `name:stream` as an alternate data stream, so a literal
/// `1234.M1P.host:2,S` cannot be created on Windows. Maildir implementations on
/// Windows have used `;` instead for decades; [`split_info`] accepts either, so a
/// mail store can be moved between platforms without rewriting a single file name.
pub const INFO_SEPARATOR_WINDOWS: char = ';';

/// The "info" separator this build writes.
pub fn info_separator() -> char {
    if cfg!(windows) {
        INFO_SEPARATOR_WINDOWS
    } else {
        INFO_SEPARATOR_UNIX
    }
}

/// Split `1234.M1P.host:2,S` into (`1234.M1P.host`, `S`).
///
/// Accepts both separators so a store written on Linux is readable on Windows and
/// vice versa. A name carrying flags but no `2` version marker (some tools emit
/// `:S`) is also handled.
pub fn split_info(file_name: &str) -> (&str, &str) {
    for separator in [INFO_SEPARATOR_UNIX, INFO_SEPARATOR_WINDOWS] {
        if let Some(idx) = file_name.rfind(separator) {
            let (base, info) = file_name.split_at(idx);
            let info = info.trim_start_matches(separator);
            // Strip the leading "2," version marker when present.
            let flags = info.strip_prefix("2,").unwrap_or(info);
            // Ignore drive-letter-looking prefixes such as `C:` on Windows paths.
            if base.len() <= 1 {
                continue;
            }
            return (base, flags);
        }
    }
    (file_name, "")
}

/// A Maildir file name for `base` carrying `maildir_flags`.
pub fn with_info(base: &str, maildir_flags: &str) -> String {
    if maildir_flags.is_empty() {
        base.to_string()
    } else {
        format!("{base}{}2,{maildir_flags}", info_separator())
    }
}

/// A Maildir-backed message store.
#[derive(Debug, Clone)]
pub struct Maildir {
    root: PathBuf,
    fsync: bool,
    layout: MailboxLayout,
    hostname: String,
}

impl Maildir {
    /// Create a store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>, fsync: bool, layout: MailboxLayout) -> Self {
        Maildir {
            root: root.into(),
            fsync,
            layout,
            hostname: sanitize_component(&default_hostname())
                .unwrap_or_else(|_| "ferroma".to_string()),
        }
    }

    /// The configured root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The mailbox root: `<root>/<domain>/<local_part>`.
    pub fn mailbox_dir(&self, domain: &str, local_part: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(sanitize_component(&domain.to_ascii_lowercase())?)
            .join(sanitize_component(&local_part.to_ascii_lowercase())?))
    }

    /// The directory holding a folder's `cur`/`new`/`tmp`.
    pub fn folder_dir(&self, domain: &str, local_part: &str, folder: &str) -> Result<PathBuf> {
        let base = self.mailbox_dir(domain, local_part)?;
        let folder = normalise_folder(folder);
        match self.layout {
            MailboxLayout::Maildir => {
                if folder.eq_ignore_ascii_case(INBOX) {
                    Ok(base.join("Maildir"))
                } else {
                    Ok(base.join("Maildir").join(maildir_folder_name(&folder)?))
                }
            }
            MailboxLayout::MaildirPerFolder => {
                if folder.eq_ignore_ascii_case(INBOX) {
                    Ok(base.join(INBOX))
                } else {
                    Ok(base.join(sanitize_component(&folder)?))
                }
            }
        }
    }

    /// Create a mailbox with the standard folder set (`INBOX`, `Sent`, `Drafts`,
    /// `Trash`, `Junk`, `Archive`). Idempotent.
    pub fn ensure_mailbox(&self, domain: &str, local_part: &str) -> Result<()> {
        for folder in [INBOX, "Sent", "Drafts", "Trash", "Junk", "Archive"] {
            self.create_folder(domain, local_part, folder)?;
        }
        Ok(())
    }

    /// Create one folder (and its `cur`/`new`/`tmp`). Idempotent.
    pub fn create_folder(&self, domain: &str, local_part: &str, folder: &str) -> Result<()> {
        let dir = self.folder_dir(domain, local_part, folder)?;
        for sub in SUBDIRS {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        Ok(())
    }

    /// Whether the folder exists on disk.
    pub fn folder_exists(&self, domain: &str, local_part: &str, folder: &str) -> Result<bool> {
        let dir = self.folder_dir(domain, local_part, folder)?;
        Ok(dir.join("cur").is_dir() && dir.join("new").is_dir() && dir.join("tmp").is_dir())
    }

    /// Remove a folder and everything in it. `INBOX` cannot be removed.
    pub fn delete_folder(&self, domain: &str, local_part: &str, folder: &str) -> Result<()> {
        if normalise_folder(folder).eq_ignore_ascii_case(INBOX) {
            return Err(StorageError::Invalid("INBOX cannot be deleted".into()));
        }
        let dir = self.folder_dir(domain, local_part, folder)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Rename a folder, moving every message with it.
    pub fn rename_folder(
        &self,
        domain: &str,
        local_part: &str,
        from: &str,
        to: &str,
    ) -> Result<()> {
        if normalise_folder(from).eq_ignore_ascii_case(INBOX) {
            return Err(StorageError::Invalid("INBOX cannot be renamed".into()));
        }
        let src = self.folder_dir(domain, local_part, from)?;
        let dst = self.folder_dir(domain, local_part, to)?;
        if !src.exists() {
            return Err(StorageError::NotFound(format!("folder {from}")));
        }
        if dst.exists() {
            return Err(StorageError::Conflict(format!("folder {to} already exists")));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&src, &dst)?;
        Ok(())
    }

    /// Every folder that exists on disk, `INBOX` first.
    pub fn list_folders(&self, domain: &str, local_part: &str) -> Result<Vec<String>> {
        let base = self.mailbox_dir(domain, local_part)?;
        let mut folders: Vec<String> = Vec::new();

        match self.layout {
            MailboxLayout::Maildir => {
                let maildir = base.join("Maildir");
                if maildir.join("cur").is_dir() {
                    folders.push(INBOX.to_string());
                }
                if let Ok(entries) = std::fs::read_dir(&maildir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if !entry.path().is_dir() || !name.starts_with('.') || name == "." {
                            continue;
                        }
                        // Maildir++: `.Archive.2026` -> `Archive/2026`.
                        let decoded = name.trim_start_matches('.').replace('.', "/");
                        if !decoded.is_empty() {
                            folders.push(decoded);
                        }
                    }
                }
            }
            MailboxLayout::MaildirPerFolder => {
                if let Ok(entries) = std::fs::read_dir(&base) {
                    for entry in entries.flatten() {
                        if entry.path().join("cur").is_dir() {
                            folders.push(entry.file_name().to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        folders.sort_by(|a, b| {
            // INBOX always sorts first, then case-insensitive alphabetical.
            let ai = a.eq_ignore_ascii_case(INBOX);
            let bi = b.eq_ignore_ascii_case(INBOX);
            bi.cmp(&ai).then_with(|| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()))
        });
        folders.dedup();
        Ok(folders)
    }

    /// Write a message. Returns the path to record in `messages.storage_path`.
    ///
    /// `flags` is a canonical flag string (see `ferroma-mail`'s `Flags`); only the
    /// flags that Maildir can express end up in the filename. Messages with an empty
    /// flag string land in `new/`, which is what an IMAP `\Recent` message looks like.
    pub fn store(
        &self,
        domain: &str,
        local_part: &str,
        folder: &str,
        bytes: &[u8],
        flags: &str,
    ) -> Result<StoredMessage> {
        let dir = self.folder_dir(domain, local_part, folder)?;
        for sub in SUBDIRS {
            std::fs::create_dir_all(dir.join(sub))?;
        }

        let maildir_flags = flags_to_maildir(flags);
        let filename = self.unique_filename(&maildir_flags);
        let tmp_path = dir.join("tmp").join(format!("{filename}.tmp"));
        let final_sub = if maildir_flags.is_empty() { "new" } else { "cur" };
        let final_path = dir.join(final_sub).join(&filename);

        std::fs::write(&tmp_path, bytes)?;
        if self.fsync {
            // Flush the file *and* the directory entry, so a power loss cannot
            // leave a rename pointing at unflushed blocks.
            if let Ok(file) = std::fs::File::open(&tmp_path) {
                let _ = file.sync_all();
            }
        }
        std::fs::rename(&tmp_path, &final_path)?;
        if self.fsync {
            if let Ok(dir_handle) = std::fs::File::open(dir.join(final_sub)) {
                let _ = dir_handle.sync_all();
            }
        }

        Ok(StoredMessage {
            path: self.relative(&final_path)?,
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(bytes)),
        })
    }

    /// Read a message by its relative path.
    pub fn read(&self, relative_path: &str) -> Result<Vec<u8>> {
        let path = self.absolute(relative_path)?;
        std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::BodyMissing(relative_path.to_string())
            } else {
                StorageError::Io(e)
            }
        })
    }

    /// Read only the first `limit` bytes — enough for header-only IMAP fetches.
    pub fn read_prefix(&self, relative_path: &str, limit: usize) -> Result<Vec<u8>> {
        use std::io::Read;
        let path = self.absolute(relative_path)?;
        let mut file = std::fs::File::open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::BodyMissing(relative_path.to_string())
            } else {
                StorageError::Io(e)
            }
        })?;
        let mut buf = vec![0u8; limit];
        let mut filled = 0;
        while filled < limit {
            match file.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    /// Delete a message file.
    pub fn delete(&self, relative_path: &str) -> Result<()> {
        let path = self.absolute(relative_path)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    /// Replace a message's flags, renaming the file accordingly.
    /// Returns the (possibly new) relative path.
    pub fn set_flags(&self, relative_path: &str, flags: &str) -> Result<String> {
        let path = self.absolute(relative_path)?;
        let maildir_flags = flags_to_maildir(flags);

        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| StorageError::Invalid("path has no file name".into()))?;
        let (base, _existing_flags) = split_info(stem);
        let new_name = with_info(base, &maildir_flags);

        // A message that gains flags moves from new/ to cur/; losing all of them
        // moves it back, which is what a client marking a message "unread" expects.
        let folder_dir = path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| StorageError::Invalid("path has no parent directory".into()))?;
        let target_sub = if maildir_flags.is_empty() { "new" } else { "cur" };
        let new_path = folder_dir.join(target_sub).join(&new_name);

        if new_path == path {
            return Ok(relative_path.to_string());
        }
        std::fs::create_dir_all(folder_dir.join(target_sub))?;
        std::fs::rename(&path, &new_path)?;
        self.relative(&new_path)
    }

    /// Move a message to another folder of the same mailbox.
    pub fn move_message(
        &self,
        relative_path: &str,
        domain: &str,
        local_part: &str,
        to_folder: &str,
        flags: &str,
    ) -> Result<String> {
        let bytes = self.read(relative_path)?;
        let stored = self.store(domain, local_part, to_folder, &bytes, flags)?;
        self.delete(relative_path)?;
        Ok(stored.path)
    }

    /// Every message in a folder, newest name last.
    pub fn iter_messages(
        &self,
        domain: &str,
        local_part: &str,
        folder: &str,
    ) -> Result<Vec<MaildirEntry>> {
        let dir = self.folder_dir(domain, local_part, folder)?;
        let mut out = Vec::new();
        for sub in ["cur", "new"] {
            let sub_dir = dir.join(sub);
            let Ok(entries) = std::fs::read_dir(&sub_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                // Squatting files from an interrupted delivery are ignored.
                if name.ends_with(".tmp") {
                    continue;
                }
                let meta = entry.metadata()?;
                let modified_secs = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                out.push(MaildirEntry {
                    path: self.relative(&path)?,
                    size: meta.len(),
                    maildir_flags: split_info(&name).1.to_string(),
                    modified_secs,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Total bytes stored for one mailbox, for quota accounting.
    pub fn usage(&self, domain: &str, local_part: &str) -> Result<u64> {
        let base = self.mailbox_dir(domain, local_part)?;
        let mut total = 0u64;
        if !base.exists() {
            return Ok(0);
        }
        for entry in walkdir::WalkDir::new(&base).into_iter().flatten() {
            if entry.file_type().is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        Ok(total)
    }

    /// Delete leftover files in every `tmp/` directory. Safe to run while the
    /// server is live: a squatting file is by definition abandoned.
    pub fn sweep_tmp(&self, older_than_secs: u64) -> Result<usize> {
        let now = SystemTime::now();
        let mut removed = 0;
        if !self.root.exists() {
            return Ok(0);
        }
        for entry in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let in_tmp = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.eq_ignore_ascii_case("tmp"))
                .unwrap_or(false);
            if !in_tmp {
                continue;
            }
            let age = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| now.duration_since(t).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if age >= older_than_secs {
                std::fs::remove_file(path)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Turn a relative path into an absolute one, refusing to escape the root.
    pub fn absolute(&self, relative_path: &str) -> Result<PathBuf> {
        let candidate = Path::new(relative_path);
        if candidate.is_absolute() {
            return Err(StorageError::Invalid(format!(
                "storage path must be relative: {relative_path}"
            )));
        }
        for component in candidate.components() {
            match component {
                std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_) => {
                    return Err(StorageError::Invalid(format!(
                        "storage path escapes the mail root: {relative_path}"
                    )));
                }
                _ => {}
            }
        }
        Ok(self.root.join(candidate))
    }

    /// Make `path` relative to the root, as stored in the database.
    fn relative(&self, path: &Path) -> Result<String> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| StorageError::Invalid(format!("path outside the mail root: {}", path.display())))?;
        Ok(rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/"))
    }

    /// `<secs>.<pid>_<counter>.<hostname>[:2,<flags>]`, guaranteed unique in-process
    /// and effectively unique across processes on the same host.
    fn unique_filename(&self, maildir_flags: &str) -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = format!("{secs}.{}_{}.{}", std::process::id(), n, self.hostname);
        with_info(&base, maildir_flags)
    }
}

/// Maildir++ folder name for an IMAP folder: `Archive/2026` -> `.Archive.2026`.
fn maildir_folder_name(folder: &str) -> Result<String> {
    for part in folder.split('/') {
        sanitize_component(part)?;
    }
    Ok(format!(".{}", folder.replace('/', ".")))
}

/// Normalise an IMAP folder name for storage.
pub fn normalise_folder(folder: &str) -> String {
    let trimmed = folder.trim().trim_matches('/');
    if trimmed.is_empty() {
        INBOX.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Reject anything that could be used to escape the mail root.
pub fn sanitize_component(component: &str) -> Result<String> {
    let trimmed = component.trim();
    if trimmed.is_empty() {
        return Err(StorageError::Invalid("empty path component".into()));
    }
    if trimmed == "." || trimmed == ".." {
        return Err(StorageError::Invalid(format!("invalid path component: {trimmed}")));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err(StorageError::Invalid(format!(
            "path component contains a separator: {trimmed}"
        )));
    }
    if trimmed.contains(':') {
        return Err(StorageError::Invalid(format!(
            "path component contains a colon: {trimmed}"
        )));
    }
    Ok(trimmed.to_string())
}

/// Map a canonical flag string (`"seen flagged $label1"`) onto Maildir letters.
///
/// Only flags Maildir can represent survive; custom keywords are stored in the
/// database and re-applied on read.
pub fn flags_to_maildir(flags: &str) -> String {
    let mut out = String::new();
    let lower = flags.to_ascii_lowercase();
    let has = |name: &str| lower.split([' ', ',']).any(|f| f == name);
    // Order follows the Maildir convention: D F P R S T.
    if has("draft") {
        out.push('D');
    }
    if has("flagged") {
        out.push('F');
    }
    if has("answered") {
        out.push('R');
    }
    if has("seen") {
        out.push('S');
    }
    if has("deleted") {
        out.push('T');
    }
    out
}

/// Map Maildir flag letters back onto canonical flag names.
pub fn maildir_to_flags(letters: &str) -> String {
    let mut flags: Vec<&str> = Vec::new();
    for ch in letters.chars() {
        match ch {
            'D' => flags.push("draft"),
            'F' => flags.push("flagged"),
            'R' => flags.push("answered"),
            'S' => flags.push("seen"),
            'T' => flags.push("deleted"),
            _ => {}
        }
    }
    flags.join(" ")
}

fn default_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "ferroma".to_string())
        .split('.')
        .next()
        .unwrap_or("ferroma")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Maildir) {
        let dir = tempfile::tempdir().unwrap();
        let maildir = Maildir::new(dir.path(), false, MailboxLayout::Maildir);
        (dir, maildir)
    }

    const SAMPLE: &[u8] = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: Hi\r\n\r\nHello\r\n";

    #[test]
    fn rejects_path_traversal_components() {
        for bad in ["..", ".", "a/b", "a\\b", "", "  ", "a:b", "x\0y"] {
            assert!(sanitize_component(bad).is_err(), "should reject {bad:?}");
        }
        assert_eq!(sanitize_component("alice").unwrap(), "alice");
        assert_eq!(sanitize_component(" example.com ").unwrap(), "example.com");
    }

    #[test]
    fn absolute_path_refuses_to_escape_the_root() {
        let (_g, m) = store();
        assert!(m.absolute("../../etc/passwd").is_err());
        assert!(m.absolute("/etc/passwd").is_err());
        assert!(m.absolute("example.com/alice/Maildir/cur/1").is_ok());
    }

    #[test]
    fn creates_the_standard_folder_set() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let folders = m.list_folders("example.com", "alice").unwrap();
        assert_eq!(folders[0], "INBOX", "INBOX must sort first: {folders:?}");
        for expected in ["Sent", "Drafts", "Trash", "Junk", "Archive"] {
            assert!(folders.contains(&expected.to_string()), "missing {expected}: {folders:?}");
        }
        assert!(m.folder_exists("example.com", "alice", "INBOX").unwrap());
        assert!(!m.folder_exists("example.com", "alice", "Nope").unwrap());
    }

    #[test]
    fn nested_folders_use_maildir_plus_plus_naming() {
        let (_g, m) = store();
        m.create_folder("example.com", "alice", "Archive/2026").unwrap();
        let dir = m.folder_dir("example.com", "alice", "Archive/2026").unwrap();
        assert!(dir.ends_with("Maildir/.Archive.2026"), "{}", dir.display());
        let folders = m.list_folders("example.com", "alice").unwrap();
        assert!(folders.contains(&"Archive/2026".to_string()), "{folders:?}");
    }

    #[test]
    fn store_then_read_round_trip() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m.store("example.com", "alice", "INBOX", SAMPLE, "").unwrap();
        assert_eq!(stored.size, SAMPLE.len() as u64);
        assert_eq!(stored.path, {
            // No flags means the message is "new" (unseen).
            let path = Path::new(&stored.path);
            assert_eq!(path.parent().unwrap().file_name().unwrap(), "new");
            stored.path.clone()
        });
        assert_eq!(m.read(&stored.path).unwrap(), SAMPLE);
        assert_eq!(
            stored.sha256,
            hex::encode(Sha256::digest(SAMPLE)),
            "checksum must match the bytes"
        );
    }

    #[test]
    fn flagged_messages_land_in_cur() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m
            .store("example.com", "alice", "INBOX", SAMPLE, "seen")
            .unwrap();
        let file = Path::new(&stored.path).file_name().unwrap().to_string_lossy().to_string();
        assert!(file.ends_with(&format!("{}2,S", info_separator())), "{file}");
        let parent = Path::new(&stored.path).parent().unwrap().file_name().unwrap();
        assert_eq!(parent, "cur");
    }

    #[test]
    fn set_flags_renames_and_moves_between_new_and_cur() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m.store("example.com", "alice", "INBOX", SAMPLE, "").unwrap();
        assert!(stored.path.contains("/new/"));

        let seen = m.set_flags(&stored.path, "seen").unwrap();
        assert!(seen.contains("/cur/"), "{seen}");
        assert!(seen.ends_with(&format!("{}2,S", info_separator())), "{seen}");
        assert_eq!(m.read(&seen).unwrap(), SAMPLE);

        let flagged = m.set_flags(&seen, "seen flagged").unwrap();
        assert!(flagged.ends_with(&format!("{}2,FS", info_separator())), "{flagged}");

        let unread = m.set_flags(&flagged, "").unwrap();
        assert!(unread.contains("/new/"), "{unread}");
        assert_eq!(split_info(Path::new(&unread).file_name().unwrap().to_str().unwrap()).1, "");
        assert_eq!(m.read(&unread).unwrap(), SAMPLE);
    }

    #[test]
    fn set_flags_is_idempotent() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m.store("example.com", "alice", "INBOX", SAMPLE, "seen").unwrap();
        let again = m.set_flags(&stored.path, "seen").unwrap();
        assert_eq!(again, stored.path);
    }

    #[test]
    fn move_message_relocates_the_bytes() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m.store("example.com", "alice", "INBOX", SAMPLE, "seen").unwrap();
        let moved = m
            .move_message(&stored.path, "example.com", "alice", "Archive", "seen")
            .unwrap();
        assert!(moved.contains("/.Archive/"), "{moved}");
        assert_eq!(m.read(&moved).unwrap(), SAMPLE);
        assert!(!m.absolute(&stored.path).unwrap().exists(), "original must be gone");
    }

    #[test]
    fn iter_messages_sees_cur_and_new_and_skips_tmp() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        m.store("example.com", "alice", "INBOX", SAMPLE, "").unwrap();
        m.store("example.com", "alice", "INBOX", SAMPLE, "seen").unwrap();
        let tmp = m.folder_dir("example.com", "alice", "INBOX").unwrap().join("tmp");
        std::fs::write(tmp.join("leftover.tmp"), b"partial").unwrap();

        let entries = m.iter_messages("example.com", "alice", "INBOX").unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries.iter().any(|e| e.maildir_flags == "S"));
        assert!(entries.iter().any(|e| e.maildir_flags.is_empty()));
    }

    #[test]
    fn read_prefix_returns_a_header_sized_slice() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m.store("example.com", "alice", "INBOX", SAMPLE, "").unwrap();
        let head = m.read_prefix(&stored.path, 20).unwrap();
        assert_eq!(head.len(), 20);
        assert!(head.starts_with(b"From: alice@example"));
        // Asking for more than exists returns everything.
        let all = m.read_prefix(&stored.path, 10_000).unwrap();
        assert_eq!(all.len(), SAMPLE.len());
    }

    #[test]
    fn usage_sums_the_whole_mailbox() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        m.store("example.com", "alice", "INBOX", SAMPLE, "").unwrap();
        m.store("example.com", "alice", "Sent", SAMPLE, "seen").unwrap();
        let used = m.usage("example.com", "alice").unwrap();
        assert_eq!(used, (SAMPLE.len() * 2) as u64);
        assert_eq!(m.usage("example.com", "nobody").unwrap(), 0);
    }

    #[test]
    fn sweep_tmp_removes_abandoned_deliveries_only() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        m.store("example.com", "alice", "INBOX", SAMPLE, "").unwrap();
        let tmp = m.folder_dir("example.com", "alice", "INBOX").unwrap().join("tmp");
        std::fs::write(tmp.join("leftover.tmp"), b"partial").unwrap();

        // Nothing is old enough yet.
        assert_eq!(m.sweep_tmp(3600).unwrap(), 0);
        // With a zero threshold everything in tmp/ goes.
        assert_eq!(m.sweep_tmp(0).unwrap(), 1);
        assert_eq!(m.iter_messages("example.com", "alice", "INBOX").unwrap().len(), 1);
    }

    #[test]
    fn delete_folder_removes_everything_and_protects_inbox() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        m.store("example.com", "alice", "Archive", SAMPLE, "seen").unwrap();
        assert!(m.delete_folder("example.com", "alice", "INBOX").is_err());
        m.delete_folder("example.com", "alice", "Archive").unwrap();
        assert!(!m.folder_exists("example.com", "alice", "Archive").unwrap());
    }

    #[test]
    fn rename_folder_moves_the_messages() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m.store("example.com", "alice", "Junk", SAMPLE, "seen").unwrap();
        m.rename_folder("example.com", "alice", "Junk", "Spam").unwrap();
        assert!(!m.folder_exists("example.com", "alice", "Junk").unwrap());
        assert!(m.folder_exists("example.com", "alice", "Spam").unwrap());
        let moved = stored.path.replace(".Junk/", ".Spam/");
        assert_eq!(m.read(&moved).unwrap(), SAMPLE);
        assert!(m.rename_folder("example.com", "alice", "INBOX", "X").is_err());
    }

    #[test]
    fn per_folder_layout_uses_plain_directories() {
        let dir = tempfile::tempdir().unwrap();
        let m = Maildir::new(dir.path(), false, MailboxLayout::MaildirPerFolder);
        m.ensure_mailbox("example.com", "alice").unwrap();
        let inbox = m.folder_dir("example.com", "alice", "INBOX").unwrap();
        assert!(inbox.ends_with("alice/INBOX"), "{}", inbox.display());
        let sent = m.folder_dir("example.com", "alice", "Sent").unwrap();
        assert!(sent.ends_with("alice/Sent"), "{}", sent.display());
        let folders = m.list_folders("example.com", "alice").unwrap();
        assert!(folders.contains(&"Sent".to_string()), "{folders:?}");
    }

    #[test]
    fn missing_body_is_reported_as_such() {
        let (_g, m) = store();
        let err = m.read("example.com/alice/Maildir/cur/nope").unwrap_err();
        assert!(matches!(err, StorageError::BodyMissing(_)), "{err:?}");
    }

    #[test]
    fn flag_translation_covers_the_maildir_letters() {
        assert_eq!(flags_to_maildir("seen"), "S");
        assert_eq!(flags_to_maildir("seen flagged answered"), "FRS");
        assert_eq!(flags_to_maildir("draft seen deleted"), "DST");
        assert_eq!(flags_to_maildir("$label1 seen"), "S", "keywords do not fit in a filename");
        assert_eq!(flags_to_maildir("recent"), "", "\\Recent is not storable in Maildir");

        assert_eq!(maildir_to_flags("FS"), "flagged seen");
        assert_eq!(maildir_to_flags(""), "");
        assert_eq!(maildir_to_flags("XYZ"), "", "unknown letters are ignored");
    }

    #[test]
    fn folder_names_are_normalised() {
        assert_eq!(normalise_folder(""), "INBOX");
        assert_eq!(normalise_folder("/"), "INBOX");
        assert_eq!(normalise_folder("/Sent/"), "Sent");
        assert_eq!(normalise_folder("Archive/2026"), "Archive/2026");
    }

    #[test]
    fn info_separator_avoids_characters_the_filesystem_reserves() {
        // `:` starts an alternate data stream on NTFS, so a Maildir file name must
        // never contain one on Windows.
        let sep = info_separator();
        if cfg!(windows) {
            assert_eq!(sep, INFO_SEPARATOR_WINDOWS);
            assert_ne!(sep, ':');
        } else {
            assert_eq!(sep, INFO_SEPARATOR_UNIX);
        }
    }

    #[test]
    fn split_info_accepts_both_separators() {
        assert_eq!(split_info("1234.M1P.host:2,S"), ("1234.M1P.host", "S"));
        assert_eq!(split_info("1234.M1P.host;2,FS"), ("1234.M1P.host", "FS"));
        assert_eq!(split_info("1234.M1P.host"), ("1234.M1P.host", ""));
        assert_eq!(split_info("1234.M1P.host:2,"), ("1234.M1P.host", ""));
        // Some tools omit the `2,` version marker.
        assert_eq!(split_info("1234.M1P.host:S"), ("1234.M1P.host", "S"));
    }

    #[test]
    fn with_info_round_trips_through_split_info() {
        for flags in ["", "S", "FS", "DRST"] {
            let name = with_info("1234.M1P.host", flags);
            assert_eq!(split_info(&name), ("1234.M1P.host", flags), "name was {name}");
        }
    }

    #[test]
    fn stored_file_names_never_contain_a_colon_on_windows() {
        if !cfg!(windows) {
            return;
        }
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let stored = m.store("example.com", "alice", "INBOX", SAMPLE, "seen flagged").unwrap();
        assert!(
            !stored.path.contains(':'),
            "NTFS rejects `:` in file names: {}",
            stored.path
        );
        // And the file really exists on disk.
        assert!(m.absolute(&stored.path).unwrap().is_file());
    }

    #[test]
    fn stored_filenames_are_unique_under_rapid_writes() {
        let (_g, m) = store();
        m.ensure_mailbox("example.com", "alice").unwrap();
        let mut paths = std::collections::HashSet::new();
        for _ in 0..200 {
            let stored = m.store("example.com", "alice", "INBOX", SAMPLE, "seen").unwrap();
            assert!(paths.insert(stored.path.clone()), "duplicate path {}", stored.path);
        }
        assert_eq!(paths.len(), 200);
    }
}
