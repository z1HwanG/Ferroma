//! The attachment blob store.
//!
//! Attachments are addressed by content: a blob lives at
//! `<root>/<first two hex>/<next two hex>/<sha256>`. Two identical attachments in
//! two different messages occupy one file, which is exactly what a mail platform
//! wants — the same PDF forwarded around an organisation is stored once.
//!
//! Writes go through a temporary file and a `rename(2)`, so a reader never observes
//! a partial blob. When [`AttachmentStore::store`] finds the blob already present it
//! does not rewrite it, which makes re-delivery cheap and idempotent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Result, StorageError};

/// What [`AttachmentStore::store`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlob {
    /// Path relative to the attachment root — this is what `attachments.storage_path` holds.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// `true` when an identical blob was already on disk.
    pub deduplicated: bool,
}

/// A content-addressed attachment store.
#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
    fsync: bool,
}

impl AttachmentStore {
    /// Create a store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>, fsync: bool) -> Self {
        AttachmentStore {
            root: root.into(),
            fsync,
        }
    }

    /// The configured root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The relative path a blob with this digest would occupy.
    pub fn path_for_digest(digest_hex: &str) -> Result<String> {
        if digest_hex.len() < 4 || !digest_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(StorageError::Invalid(format!("bad digest: {digest_hex}")));
        }
        let lower = digest_hex.to_ascii_lowercase();
        Ok(format!("{}/{}/{}", &lower[0..2], &lower[2..4], lower))
    }

    /// Store `data`, returning its content address. Idempotent.
    pub fn store(&self, data: &[u8]) -> Result<StoredBlob> {
        let digest = hex::encode(Sha256::digest(data));
        let relative = Self::path_for_digest(&digest)?;
        let final_path = self.root.join(&relative);

        if final_path.is_file() {
            return Ok(StoredBlob {
                path: relative,
                size: data.len() as u64,
                sha256: digest,
                deduplicated: true,
            });
        }

        let parent = final_path
            .parent()
            .ok_or_else(|| StorageError::Invalid("blob path has no parent".into()))?;
        std::fs::create_dir_all(parent)?;

        let tmp = parent.join(format!(".{}.{}.tmp", &digest[4..16], std::process::id()));
        std::fs::write(&tmp, data)?;
        if self.fsync {
            if let Ok(file) = std::fs::File::open(&tmp) {
                let _ = file.sync_all();
            }
        }
        // A concurrent writer may have created the same blob between the check and
        // the rename; on Unix `rename` overwrites atomically with identical content,
        // and on Windows the first writer wins. Either way the bytes match the name.
        match std::fs::rename(&tmp, &final_path) {
            Ok(()) => {}
            Err(_) if final_path.is_file() => {
                let _ = std::fs::remove_file(&tmp);
            }
            Err(e) => return Err(StorageError::Io(e)),
        }

        Ok(StoredBlob {
            path: relative,
            size: data.len() as u64,
            sha256: digest,
            deduplicated: false,
        })
    }

    /// Read a blob.
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

    /// Read a byte range, for HTTP `Range` requests and resumable downloads.
    pub fn read_range(&self, relative_path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.absolute(relative_path)?;
        let mut file = std::fs::File::open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::BodyMissing(relative_path.to_string())
            } else {
                StorageError::Io(e)
            }
        })?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; length];
        let mut filled = 0;
        while filled < length {
            match file.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    /// Whether a blob exists.
    pub fn exists(&self, relative_path: &str) -> bool {
        self.absolute(relative_path)
            .map(|p| p.is_file())
            .unwrap_or(false)
    }

    /// Size of a blob in bytes.
    pub fn size(&self, relative_path: &str) -> Result<u64> {
        let path = self.absolute(relative_path)?;
        std::fs::metadata(&path).map(|m| m.len()).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::BodyMissing(relative_path.to_string())
            } else {
                StorageError::Io(e)
            }
        })
    }

    /// Delete a blob. Returns `true` when a file was actually removed.
    ///
    /// Callers must ensure no other message references the blob — this is what
    /// [`AttachmentStore::gc`] exists to do correctly in bulk.
    pub fn delete(&self, relative_path: &str) -> Result<bool> {
        let path = self.absolute(relative_path)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    /// Total bytes stored.
    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0u64;
        if !self.root.exists() {
            return Ok(0);
        }
        for entry in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            if entry.file_type().is_file() {
                let name = entry.file_name().to_string_lossy();
                if name.ends_with(".tmp") {
                    continue;
                }
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        Ok(total)
    }

    /// Delete every blob whose digest is not in `keep`.
    ///
    /// `keep` holds the digests (or the file names) still referenced by an
    /// `attachments.storage_path`. Returns the number of blobs removed.
    pub fn gc(&self, keep: &HashSet<String>) -> Result<usize> {
        let mut removed = 0;
        if !self.root.exists() {
            return Ok(0);
        }
        for entry in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".tmp") {
                std::fs::remove_file(entry.path())?;
                removed += 1;
                continue;
            }
            if keep.contains(&name) {
                continue;
            }
            // Also accept a fully-qualified relative path in `keep`.
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .ok()
                .map(|p| {
                    p.components()
                        .map(|c| c.as_os_str().to_string_lossy().to_string())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            if keep.contains(&relative) {
                continue;
            }
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Turn a relative path into an absolute one, refusing to escape the root.
    pub fn absolute(&self, relative_path: &str) -> Result<PathBuf> {
        let candidate = Path::new(relative_path);
        if candidate.is_absolute() {
            return Err(StorageError::Invalid(format!(
                "attachment path must be relative: {relative_path}"
            )));
        }
        for component in candidate.components() {
            match component {
                std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_) => {
                    return Err(StorageError::Invalid(format!(
                        "attachment path escapes the store root: {relative_path}"
                    )));
                }
                _ => {}
            }
        }
        Ok(self.root.join(candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, AttachmentStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(dir.path(), false);
        (dir, store)
    }

    #[test]
    fn stores_and_reads_a_blob() {
        let (_g, s) = store();
        let data = b"PDF-BYTES".to_vec();
        let blob = s.store(&data).unwrap();
        assert!(!blob.deduplicated);
        assert_eq!(blob.size, data.len() as u64);
        assert_eq!(blob.sha256, hex::encode(Sha256::digest(&data)));
        assert!(s.exists(&blob.path));
        assert_eq!(s.read(&blob.path).unwrap(), data);
        assert_eq!(s.size(&blob.path).unwrap(), data.len() as u64);
    }

    #[test]
    fn identical_content_is_stored_once() {
        let (_g, s) = store();
        let data = b"same bytes".to_vec();
        let a = s.store(&data).unwrap();
        let b = s.store(&data).unwrap();
        assert!(!a.deduplicated);
        assert!(b.deduplicated);
        assert_eq!(a.path, b.path);
        assert_eq!(s.total_size().unwrap(), data.len() as u64);
    }

    #[test]
    fn digest_paths_are_sharded_two_levels() {
        let digest = "abcdef0123456789".repeat(4);
        let path = AttachmentStore::path_for_digest(&digest).unwrap();
        assert!(path.starts_with("ab/cd/"));
        assert!(path.ends_with(&digest));
        assert!(AttachmentStore::path_for_digest("zz").is_err());
        assert!(AttachmentStore::path_for_digest("ab").is_err());
    }

    #[test]
    fn range_reads_serve_http_range_requests() {
        let (_g, s) = store();
        let blob = s.store(b"0123456789").unwrap();
        assert_eq!(s.read_range(&blob.path, 0, 4).unwrap(), b"0123");
        assert_eq!(s.read_range(&blob.path, 6, 4).unwrap(), b"6789");
        // Reading past the end returns what exists, not an error.
        assert_eq!(s.read_range(&blob.path, 8, 100).unwrap(), b"89");
    }

    #[test]
    fn rejects_path_traversal() {
        let (_g, s) = store();
        assert!(s.absolute("../../secret").is_err());
        assert!(s.absolute("/etc/passwd").is_err());
        assert!(!s.exists("../../secret"));
        assert!(s.read("../../secret").is_err());
    }

    #[test]
    fn missing_blob_is_reported_as_body_missing() {
        let (_g, s) = store();
        let err = s.read("ab/cd/deadbeef").unwrap_err();
        assert!(matches!(err, StorageError::BodyMissing(_)), "{err:?}");
    }

    #[test]
    fn delete_is_idempotent() {
        let (_g, s) = store();
        let blob = s.store(b"bye").unwrap();
        assert!(s.delete(&blob.path).unwrap());
        assert!(!s.delete(&blob.path).unwrap());
    }

    #[test]
    fn gc_keeps_referenced_blobs_and_drops_the_rest() {
        let (_g, s) = store();
        let kept = s.store(b"keep me").unwrap();
        let dropped = s.store(b"drop me").unwrap();
        assert_eq!(s.total_size().unwrap(), 14);

        let mut keep = HashSet::new();
        keep.insert(kept.sha256.clone());
        let removed = s.gc(&keep).unwrap();
        assert_eq!(removed, 1);
        assert!(s.exists(&kept.path));
        assert!(!s.exists(&dropped.path));
    }

    #[test]
    fn gc_accepts_full_relative_paths_too() {
        let (_g, s) = store();
        let kept = s.store(b"keep me").unwrap();
        let mut keep = HashSet::new();
        keep.insert(kept.path.clone());
        assert_eq!(s.gc(&keep).unwrap(), 0);
        assert!(s.exists(&kept.path));
    }

    #[test]
    fn gc_cleans_up_interrupted_writes() {
        let (g, s) = store();
        let blob = s.store(b"real").unwrap();
        std::fs::write(g.path().join("ab/cd/.deadbeef.999.tmp"), b"partial").ok();
        // Create the scratch directory that the .tmp file would live in.
        let scratch = g.path().join("11/22");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join(".cafebabe.1.tmp"), b"partial").unwrap();

        let mut keep = HashSet::new();
        keep.insert(blob.sha256.clone());
        let removed = s.gc(&keep).unwrap();
        assert_eq!(removed, 1, "the stray .tmp must be swept");
        assert!(s.exists(&blob.path));
    }
}
